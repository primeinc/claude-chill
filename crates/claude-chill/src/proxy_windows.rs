//! Windows PTY proxy: ConPTY-based event loop, lookback mode, and coordination
//! between [`VtRenderer`](crate::vt_renderer::VtRenderer),
//! [`HistoryManager`](crate::history_manager::HistoryManager),
//! and terminal I/O.

use crate::alt_screen::AltScreenTracker;
#[cfg(test)]
use crate::escape_sequences::ALT_SCREEN_ENTER;
#[cfg(test)]
use crate::escape_sequences::SYNC_START;
use crate::escape_sequences::{CLEAR_SCREEN, CURSOR_HOME, INPUT_BUFFER_CAPACITY};
use crate::history_manager::HistoryManager;
use crate::kitty_tracker_windows as kitty_tracker;
pub use crate::proxy_common::ProxyConfig;
use crate::proxy_common::should_auto_lookback;
use crate::sequence_match::{self, SequenceMatch};
use crate::sync_block::SyncBlockParser;
use crate::terminal_windows::{
    self, ConsoleMode, SIGTERM_RECEIVED, SIGWINCH_RECEIVED, TerminalSize, get_terminal_size,
    setup_raw_mode, setup_signal_handlers,
};
use crate::vt_renderer::VtRenderer;
use anyhow::{Context, Result};
use log::debug;
use std::io::{self, Write};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, INVALID_HANDLE_VALUE, S_OK, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, GetStdHandle, HPCON, ResizePseudoConsole,
    STD_INPUT_HANDLE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    STARTUPINFOW, UpdateProcThreadAttribute, WaitForSingleObject,
};

/// PTY proxy using Windows ConPTY for pseudo-console support.
pub struct Proxy {
    // Process & terminal
    config: ProxyConfig,
    conpty: HPCON,
    child_process: HANDLE,
    child_thread: HANDLE,
    pipe_input_write: HANDLE, // Write end: our input → child's stdin
    pipe_output_read: HANDLE, // Read end: child's stdout → our output
    original_console_mode: Option<ConsoleMode>,

    // VT rendering
    renderer: VtRenderer,
    sync_parser: SyncBlockParser,

    // History & filtering
    history: HistoryManager,

    // Mode tracking
    in_lookback_mode: bool,
    alt_screen: AltScreenTracker,
    kitty_tracker: kitty_tracker::KittyTracker,

    // Lookback input matching
    lookback_input_buffer: Vec<u8>,
    lookback_cache: Vec<u8>,

    // Timing
    last_stdin_time: Option<Instant>,
    last_auto_lookback_time: Option<Instant>,
    auto_lookback_timeout: Duration,

    // Reusable write buffer for history dump / lookback
    output_buffer: Vec<u8>,

    // Track last known terminal size for resize detection
    last_terminal_size: TerminalSize,
}

impl Proxy {
    /// Spawn a child process in a ConPTY and return a proxy ready to run.
    pub fn spawn(command: &str, args: &[&str], config: ProxyConfig) -> Result<Self> {
        anyhow::ensure!(
            !config.lookback_sequence_legacy.is_empty(),
            "lookback_sequence_legacy must not be empty"
        );
        anyhow::ensure!(
            !config.lookback_sequence_kitty.is_empty(),
            "lookback_sequence_kitty must not be empty"
        );
        anyhow::ensure!(
            config.lookback_sequence_legacy.len() <= 16
                && config.lookback_sequence_kitty.len() <= 16,
            "lookback sequences must be at most 16 bytes"
        );

        let winsize = get_terminal_size()?;

        let original_console_mode = setup_raw_mode()?;
        setup_signal_handlers()?;

        // Kitty detection: Windows terminals generally don't support Kitty protocol
        let kitty_tracker = kitty_tracker::KittyTracker::new(false, 0);

        // Create pipes for ConPTY communication
        // Per MS docs: "read" end of input and "write" end of output go to ConPTY
        let mut pipe_input_read: HANDLE = INVALID_HANDLE_VALUE;
        let mut pipe_input_write: HANDLE = INVALID_HANDLE_VALUE;
        let mut pipe_output_read: HANDLE = INVALID_HANDLE_VALUE;
        let mut pipe_output_write: HANDLE = INVALID_HANDLE_VALUE;

        // SAFETY: CreatePipe writes valid handles into the out-pointers.
        // Null security attrs and zero buffer size use system defaults.
        // Handles are cleaned up via CloseHandle on error or in Drop.
        unsafe {
            if CreatePipe(
                &mut pipe_input_read,
                &mut pipe_input_write,
                std::ptr::null(),
                0,
            ) == 0
            {
                anyhow::bail!("CreatePipe (input) failed: {}", io::Error::last_os_error());
            }
            if CreatePipe(
                &mut pipe_output_read,
                &mut pipe_output_write,
                std::ptr::null(),
                0,
            ) == 0
            {
                CloseHandle(pipe_input_read);
                CloseHandle(pipe_input_write);
                anyhow::bail!("CreatePipe (output) failed: {}", io::Error::last_os_error());
            }
        }

        // Create the pseudo console
        let size = COORD {
            X: winsize.ws_col as i16,
            Y: winsize.ws_row as i16,
        };
        let mut conpty: HPCON = 0;
        // SAFETY: pipe_input_read and pipe_output_write are valid handles from
        // CreatePipe above. conpty receives the pseudo-console handle.
        let hr = unsafe {
            CreatePseudoConsole(size, pipe_input_read, pipe_output_write, 0, &mut conpty)
        };
        if hr != S_OK {
            // SAFETY: All four handles are valid from CreatePipe; closing on error path.
            unsafe {
                CloseHandle(pipe_input_read);
                CloseHandle(pipe_input_write);
                CloseHandle(pipe_output_read);
                CloseHandle(pipe_output_write);
            }
            anyhow::bail!("CreatePseudoConsole failed: HRESULT 0x{hr:08x}");
        }

        // Per MS docs: close the pipe ends given to ConPTY after CreateProcess
        // We'll close them after spawning the child.

        // Prepare startup info with ConPTY attribute
        let (child_process, child_thread) = create_child_process(conpty, command, args)?;

        // Per MS docs: "Upon completion of the CreateProcess call, the handles given
        // during creation should be freed from this process."
        // SAFETY: pipe_input_read and pipe_output_write are valid handles that
        // have been given to ConPTY and are no longer needed by this process.
        unsafe {
            CloseHandle(pipe_input_read);
            CloseHandle(pipe_output_write);
        }

        let renderer = VtRenderer::new(winsize.ws_row, winsize.ws_col);
        let history = HistoryManager::new(config.max_history_lines);
        let auto_lookback_timeout = Duration::from_millis(config.auto_lookback_timeout_ms);

        debug!("Proxy::spawn: command={command} args={args:?}");

        Ok(Self {
            history,
            config,
            conpty,
            child_process,
            child_thread,
            pipe_input_write,
            pipe_output_read,
            original_console_mode,
            renderer,
            last_stdin_time: None,
            last_auto_lookback_time: None,
            auto_lookback_timeout,
            sync_parser: SyncBlockParser::new(),
            in_lookback_mode: false,
            alt_screen: AltScreenTracker::new(),
            kitty_tracker,
            lookback_cache: Vec::new(),
            lookback_input_buffer: Vec::with_capacity(INPUT_BUFFER_CAPACITY),
            output_buffer: Vec::with_capacity(crate::escape_sequences::OUTPUT_BUFFER_CAPACITY),
            last_terminal_size: winsize,
        })
    }

    /// Run the proxy event loop until the child process exits.
    /// Returns the child's exit code.
    ///
    /// Per Microsoft ConPTY docs: "To prevent race conditions and deadlocks,
    /// we highly recommend that each of the communication channels is serviced
    /// on a separate thread." The output pipe is read on a background thread
    /// and sent to the main loop via a channel.
    pub fn run(&mut self) -> Result<i32> {
        let mut stdout = io::stdout();

        // Spawn a background thread to read from the ConPTY output pipe.
        // ReadFile on an anonymous pipe is blocking, so it MUST be on its own thread.
        // SAFETY: The pipe handle is valid for the lifetime of the proxy and is only
        // used for reading on this thread. HANDLE is a raw pointer (*mut c_void) which
        // isn't Send, but pipe handles are safe to use from any thread.
        // Convert HANDLE to usize for thread transfer (HANDLE is *mut c_void,
        // which isn't Send). usize round-trips safely on Windows.
        let pipe_handle_raw = self.pipe_output_read as usize;
        let (output_tx, output_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let reader_thread = std::thread::spawn(move || {
            let pipe_output_read = pipe_handle_raw as HANDLE;
            let mut read_buf = vec![0u8; 65536];
            loop {
                let mut bytes_read: u32 = 0;
                // SAFETY: pipe_output_read is a valid pipe handle transferred
                // via usize; read_buf is a valid buffer; bytes_read receives count.
                let ok = unsafe {
                    ReadFile(
                        pipe_output_read,
                        read_buf.as_mut_ptr(),
                        read_buf.len() as u32,
                        &mut bytes_read,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || bytes_read == 0 {
                    break; // Pipe closed (child exited or ConPTY shut down)
                }
                if output_tx
                    .send(read_buf[..bytes_read as usize].to_vec())
                    .is_err()
                {
                    break; // Main thread dropped the receiver
                }
            }
        });

        // stdin gets its own blocking reader thread too. A console ReadFile
        // waits for key input (resize and focus records do not complete it),
        // so on the event loop it stalls output. A redirected stdin (pipe,
        // file, NUL) is not a console, and only ReadFile drains it and sees
        // its EOF. The thread ends at EOF or on a read error; it is never
        // joined, because a console read cannot be cancelled and process exit
        // ends it.
        // SAFETY: GetStdHandle returns this process's stdin handle, valid for
        // the life of the process. Transferred as usize like the output pipe.
        let stdin_handle_raw = unsafe { GetStdHandle(STD_INPUT_HANDLE) } as usize;
        let (input_tx, input_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(64);
        std::thread::spawn(move || {
            let stdin_handle = stdin_handle_raw as HANDLE;
            let mut read_buf = vec![0u8; 4096];
            loop {
                let mut bytes_read: u32 = 0;
                // SAFETY: stdin_handle is the process stdin handle; read_buf is
                // valid for its length; bytes_read receives the count.
                let ok = unsafe {
                    ReadFile(
                        stdin_handle,
                        read_buf.as_mut_ptr(),
                        read_buf.len() as u32,
                        &mut bytes_read,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || bytes_read == 0 {
                    break; // EOF, closed pipe, or no usable stdin
                }
                if input_tx
                    .send(read_buf[..bytes_read as usize].to_vec())
                    .is_err()
                {
                    break; // Main thread dropped the receiver
                }
            }
        });

        loop {
            // Check for signals
            if SIGTERM_RECEIVED.swap(false, Ordering::SeqCst) {
                // SAFETY: child_process is a valid process handle from CreateProcessW.
                unsafe {
                    windows_sys::Win32::System::Threading::TerminateProcess(self.child_process, 1);
                }
            }

            // Poll for resize (Windows doesn't have SIGWINCH)
            self.check_resize()?;

            // Flush pending renders
            if let Some(bytes) = self.renderer.flush_if_ready(
                self.in_lookback_mode,
                self.alt_screen.in_alternate_screen(),
                self.sync_parser.in_sync_block(),
            ) {
                self.kitty_tracker.process(bytes);
                stdout.write_all(bytes).context("write to stdout failed")?;
                stdout.flush()?;
            }

            // Drain all available output from the reader thread (non-blocking)
            let mut got_output = false;
            while let Ok(data) = output_rx.try_recv() {
                self.process_output(&data, &mut stdout)?;
                got_output = true;
            }

            // Forward stdin read by the input thread (non-blocking)
            while let Ok(data) = input_rx.try_recv() {
                self.process_input(&data, &mut stdout)?;
            }

            // Check auto-lookback
            if !got_output {
                self.check_auto_lookback(&mut stdout)?;
            }

            // Check if child has exited AND output pipe is drained
            // SAFETY: child_process is a valid handle; timeout 0 = non-blocking poll.
            let child_wait = unsafe { WaitForSingleObject(self.child_process, 0) };
            if child_wait == WAIT_OBJECT_0 {
                // Child exited. Drain any remaining output from the channel.
                // Give the reader thread a moment to flush.
                std::thread::sleep(Duration::from_millis(50));
                while let Ok(data) = output_rx.try_recv() {
                    self.process_output(&data, &mut stdout)?;
                }
                break;
            }

            // Small sleep to avoid busy-looping when no data is available
            if !got_output {
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        // Close the ConPTY BEFORE joining the reader thread.
        // Per MS docs: "closing the pseudoconsole session may emit a final frame
        // update to hOutput which should be drained from the communications channel
        // buffer." The reader thread does this draining. ClosePseudoConsole will
        // break the pipe, causing the reader thread's ReadFile to return 0/error.
        // SAFETY: conpty is a valid HPCON from CreatePseudoConsole. Set to 0 after
        // to prevent double-close in Drop.
        unsafe { ClosePseudoConsole(self.conpty) };
        self.conpty = 0; // Mark as closed so Drop doesn't double-close

        // Now the reader thread can finish (pipe is broken)
        let _ = reader_thread.join();

        // Drain any final output the reader thread sent before exiting
        while let Ok(data) = output_rx.try_recv() {
            self.process_output(&data, &mut stdout)?;
        }

        // Final render
        if self.renderer.is_pending()
            && let Some(bytes) = self.renderer.render()
        {
            self.kitty_tracker.process(bytes);
            stdout.write_all(bytes)?;
            stdout.flush()?;
        }

        self.wait_child()
    }

    fn process_output(&mut self, data: &[u8], stdout: &mut io::Stdout) -> Result<()> {
        debug!(
            "process_output: len={} in_alt={} in_lookback={}",
            data.len(),
            self.alt_screen.in_alternate_screen(),
            self.in_lookback_mode,
        );

        if self.alt_screen.in_alternate_screen() {
            self.renderer.process(data);
            return self.process_output_alt_screen(data, stdout);
        }

        if self.in_lookback_mode {
            debug!("process_output: caching {} bytes for lookback", data.len());
            self.lookback_cache.extend_from_slice(data);
            return Ok(());
        }

        self.renderer.process(data);
        self.renderer.mark_pending();

        if let Some(alt_pos) = self.alt_screen.find_enter(data) {
            debug!("process_output: ALT_SCREEN_ENTER detected at pos={alt_pos}");
            let before_alt = &data[..alt_pos];
            if !before_alt.is_empty() {
                let mut segments = Vec::with_capacity(4);
                self.sync_parser.parse(before_alt, &mut segments);
                for segment in segments {
                    self.history.apply_segment(segment);
                }
            }
            let remaining = &data[alt_pos..];
            if self.sync_parser.in_sync_block() {
                let segment = self.sync_parser.append_and_flush(remaining);
                self.history.apply_segment(segment);
            } else {
                self.history.push(remaining);
            }
            self.alt_screen.set_alternate_screen(true);
            let seq_len = self.alt_screen.enter_len(&data[alt_pos..]);
            self.write_stdout(stdout, &data[alt_pos..alt_pos + seq_len])?;
            return self.process_output_alt_screen(&data[alt_pos + seq_len..], stdout);
        }

        let mut segments = Vec::with_capacity(4);
        self.sync_parser.parse(data, &mut segments);
        for segment in segments {
            self.history.apply_segment(segment);
        }

        Ok(())
    }

    fn process_output_alt_screen(&mut self, data: &[u8], stdout: &mut io::Stdout) -> Result<()> {
        if let Some(exit_pos) = self.alt_screen.find_exit(data) {
            debug!("process_output_alt_screen: ALT_SCREEN_EXIT detected at pos={exit_pos}");
            self.write_stdout(stdout, &data[..exit_pos])?;
            let seq_len = self.alt_screen.exit_len(&data[exit_pos..]);
            self.write_stdout(stdout, &data[exit_pos..exit_pos + seq_len])?;
            self.alt_screen.set_alternate_screen(false);

            let bytes = self.renderer.render_full();
            self.kitty_tracker.process(bytes);
            stdout.write_all(bytes).context("write to stdout")?;
            stdout.flush()?;

            let remaining = &data[exit_pos + seq_len..];
            if !remaining.is_empty() && self.alt_screen.find_enter(remaining).is_some() {
                return self.process_output_check_alt_only(remaining, stdout);
            }
            return Ok(());
        }
        self.write_stdout(stdout, data)
    }

    fn process_output_check_alt_only(
        &mut self,
        data: &[u8],
        stdout: &mut io::Stdout,
    ) -> Result<()> {
        if let Some(alt_pos) = self.alt_screen.find_enter(data) {
            self.alt_screen.set_alternate_screen(true);
            let seq_len = self.alt_screen.enter_len(&data[alt_pos..]);
            self.write_stdout(stdout, &data[alt_pos..alt_pos + seq_len])?;
            return self.process_output_alt_screen(&data[alt_pos + seq_len..], stdout);
        }
        Ok(())
    }

    fn write_stdout(&mut self, stdout: &mut io::Stdout, data: &[u8]) -> Result<()> {
        self.kitty_tracker.process(data);
        stdout.write_all(data).context("write to stdout failed")?;
        stdout.flush()?;
        Ok(())
    }

    fn write_to_pty(&self, data: &[u8]) -> Result<()> {
        let mut written: u32 = 0;
        let mut offset = 0;
        while offset < data.len() {
            // SAFETY: pipe_input_write is a valid write handle from CreatePipe,
            // data slice is valid for the given length, written receives byte count.
            let ok = unsafe {
                WriteFile(
                    self.pipe_input_write,
                    data[offset..].as_ptr(),
                    (data.len() - offset) as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                anyhow::bail!("WriteFile to PTY failed: {}", io::Error::last_os_error());
            }
            offset += written as usize;
        }
        Ok(())
    }

    fn process_input(&mut self, data: &[u8], stdout: &mut io::Stdout) -> Result<()> {
        self.last_stdin_time = Some(Instant::now());

        if self.alt_screen.in_alternate_screen() {
            return self.write_to_pty(data);
        }

        let mut seq_buf = [0u8; 16];
        let seq = if self.kitty_tracker.mode_enabled() {
            let len = self.config.lookback_sequence_kitty.len().min(seq_buf.len());
            seq_buf[..len].copy_from_slice(&self.config.lookback_sequence_kitty[..len]);
            &seq_buf[..len]
        } else {
            let len = self
                .config
                .lookback_sequence_legacy
                .len()
                .min(seq_buf.len());
            seq_buf[..len].copy_from_slice(&self.config.lookback_sequence_legacy[..len]);
            &seq_buf[..len]
        };

        for &byte in data {
            if self.in_lookback_mode && byte == 0x03 {
                self.lookback_input_buffer.clear();
                self.exit_lookback_mode(stdout)?;
                continue;
            }

            let lookback_action = sequence_match::check(&self.lookback_input_buffer, byte, seq);

            self.lookback_input_buffer.push(byte);
            if self.lookback_input_buffer.len() > seq.len() {
                let excess = self.lookback_input_buffer.len() - seq.len();
                self.lookback_input_buffer.drain(..excess);
            }

            match lookback_action {
                SequenceMatch::Complete => {
                    self.lookback_input_buffer.clear();
                    if self.in_lookback_mode {
                        self.exit_lookback_mode(stdout)?;
                    } else {
                        self.enter_lookback_mode(stdout)?;
                    }
                    continue;
                }
                SequenceMatch::Partial => continue,
                SequenceMatch::None => {
                    if !self.in_lookback_mode {
                        self.write_to_pty(&self.lookback_input_buffer.clone())?;
                    }
                    self.lookback_input_buffer.clear();
                }
            }
        }
        Ok(())
    }

    fn enter_lookback_mode(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        self.in_lookback_mode = true;
        self.lookback_cache.clear();
        self.renderer.cancel_pending();

        self.output_buffer.clear();
        self.history.append_all(&mut self.output_buffer);

        self.write_stdout(stdout, CLEAR_SCREEN)?;
        self.write_stdout(stdout, CURSOR_HOME)?;
        let buf = std::mem::take(&mut self.output_buffer);
        self.write_stdout(stdout, &buf)?;
        self.output_buffer = buf;

        let exit_msg = format!(
            "\r\n\x1b[7m--- LOOKBACK MODE: press {} or Ctrl+C to exit ---\x1b[0m\r\n",
            self.config.lookback_key
        );
        stdout
            .write_all(exit_msg.as_bytes())
            .context("write exit msg")?;
        stdout.flush()?;

        Ok(())
    }

    fn exit_lookback_mode(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        self.in_lookback_mode = false;

        let cached = std::mem::take(&mut self.lookback_cache);
        if !cached.is_empty() {
            self.process_output(&cached, stdout)?;
        }

        self.sync_parser.reset();
        self.check_resize()?;

        let bytes = self.renderer.render_full();
        self.kitty_tracker.process(bytes);
        stdout.write_all(bytes).context("write to stdout")?;
        stdout.flush()?;

        Ok(())
    }

    fn check_resize(&mut self) -> Result<()> {
        if let Ok(winsize) = get_terminal_size()
            && (winsize.ws_row != self.last_terminal_size.ws_row
                || winsize.ws_col != self.last_terminal_size.ws_col)
        {
            debug!(
                "check_resize: rows={} cols={}",
                winsize.ws_row, winsize.ws_col
            );
            self.last_terminal_size = winsize;
            self.renderer.resize(winsize.ws_row, winsize.ws_col);
            let size = COORD {
                X: winsize.ws_col as i16,
                Y: winsize.ws_row as i16,
            };
            // SAFETY: conpty is a valid HPCON, size is a valid COORD.
            let hr = unsafe { ResizePseudoConsole(self.conpty, size) };
            if hr != S_OK {
                debug!("ResizePseudoConsole failed: HRESULT 0x{hr:08x}");
            }
            SIGWINCH_RECEIVED.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    fn check_auto_lookback(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        let now = Instant::now();
        if !should_auto_lookback(
            self.auto_lookback_timeout,
            self.in_lookback_mode,
            self.alt_screen.in_alternate_screen(),
            self.last_stdin_time.map(|t| now.duration_since(t)),
            self.renderer.last_render_time(),
            self.last_auto_lookback_time,
            now,
        ) {
            return Ok(());
        }

        self.dump_history(stdout)?;
        self.last_auto_lookback_time = Some(Instant::now());
        Ok(())
    }

    fn dump_history(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        self.output_buffer.clear();
        self.history.append_all(&mut self.output_buffer);

        self.write_stdout(stdout, CLEAR_SCREEN)?;
        self.write_stdout(stdout, CURSOR_HOME)?;
        let buf = std::mem::take(&mut self.output_buffer);
        self.write_stdout(stdout, &buf)?;
        self.output_buffer = buf;

        self.renderer.force_full_render();
        Ok(())
    }

    fn wait_child(&mut self) -> Result<i32> {
        // SAFETY: child_process is a valid handle. 0xFFFFFFFF = INFINITE timeout.
        unsafe { WaitForSingleObject(self.child_process, 0xFFFFFFFF) }; // INFINITE
        let mut exit_code: u32 = 1;
        // SAFETY: child_process has terminated (WaitForSingleObject returned),
        // exit_code is a valid out-pointer.
        unsafe {
            windows_sys::Win32::System::Threading::GetExitCodeProcess(
                self.child_process,
                &mut exit_code,
            );
        }
        Ok(exit_code as i32)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        // SAFETY: All handles were obtained from Win32 API calls during spawn().
        // conpty may have been set to 0 in run() to prevent double-close.
        // CloseHandle is idempotent-safe for valid handles.
        unsafe {
            // conpty may have been closed already in run() — check for null
            if self.conpty != 0 {
                ClosePseudoConsole(self.conpty);
            }
            CloseHandle(self.pipe_input_write);
            CloseHandle(self.pipe_output_read);
            CloseHandle(self.child_process);
            CloseHandle(self.child_thread);
        }
        if let Some(ref mode) = self.original_console_mode {
            terminal_windows::restore_console_mode(mode);
        }
    }
}

/// Quote a single argument for the Windows command line using the
/// `CommandLineToArgvW` escaping convention.
///
/// Rules (per Microsoft docs):
/// - Arguments containing spaces, tabs, or double quotes are wrapped in quotes.
/// - Backslashes are literal unless immediately preceding a double quote.
/// - A run of N backslashes before a `"` becomes 2N+1 backslashes plus `"`.
/// - A run of N backslashes at the end of the argument (before closing `"`)
///   becomes 2N backslashes.
/// - Empty arguments are quoted as `""`.
fn quote_arg_windows(arg: &str) -> String {
    // If the arg doesn't need quoting, return it as-is.
    // This preserves cmd.exe /c semantics where unconditional quoting
    // changes how the command tail is interpreted.
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }

    let mut quoted = String::with_capacity(arg.len() + 4);
    quoted.push('"');

    let mut backslash_count: usize = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => {
                backslash_count += 1;
            }
            '"' => {
                // Double the backslashes before a quote, then escape the quote
                for _ in 0..(backslash_count * 2) {
                    quoted.push('\\');
                }
                backslash_count = 0;
                quoted.push('\\');
                quoted.push('"');
            }
            _ => {
                // Backslashes not before a quote are literal
                for _ in 0..backslash_count {
                    quoted.push('\\');
                }
                backslash_count = 0;
                quoted.push(ch);
            }
        }
    }

    // Double any trailing backslashes (they precede the closing quote)
    for _ in 0..(backslash_count * 2) {
        quoted.push('\\');
    }

    quoted.push('"');
    quoted
}

/// Create a child process attached to the given ConPTY.
///
/// Per MS docs: uses InitializeProcThreadAttributeList + UpdateProcThreadAttribute
/// with PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, then CreateProcessW with
/// EXTENDED_STARTUPINFO_PRESENT.
fn create_child_process(conpty: HPCON, command: &str, args: &[&str]) -> Result<(HANDLE, HANDLE)> {
    // Build command line string with proper escaping.
    // Uses the CommandLineToArgvW-compatible quoting algorithm from Microsoft docs:
    // https://learn.microsoft.com/en-us/cpp/c-language/parsing-c-command-line-arguments
    let mut cmd_line = quote_arg_windows(command);
    for arg in args {
        cmd_line.push(' ');
        cmd_line.push_str(&quote_arg_windows(arg));
    }
    let mut cmd_wide: Vec<u16> = cmd_line.encode_utf16().chain(std::iter::once(0)).collect();

    // Initialize proc thread attribute list (double-call pattern per MS docs)
    let mut attr_list_size: usize = 0;
    // SAFETY: First call with NULL determines required size. This is the documented
    // double-call pattern for InitializeProcThreadAttributeList.
    unsafe {
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_list_size);
    }

    let attr_list_buf = vec![0u8; attr_list_size];
    let attr_list = attr_list_buf.as_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;

    // SAFETY: attr_list points to a buffer of the size requested by the first call.
    if unsafe { InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_list_size) } == 0 {
        anyhow::bail!(
            "InitializeProcThreadAttributeList failed: {}",
            io::Error::last_os_error()
        );
    }

    // SAFETY: attr_list was initialized above. conpty is a valid HPCON from
    // CreatePseudoConsole. PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE is a documented flag.
    if unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            conpty as *const core::ffi::c_void,
            std::mem::size_of::<HPCON>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    } == 0
    {
        // SAFETY: attr_list was initialized above; cleaning up on error path.
        unsafe { DeleteProcThreadAttributeList(attr_list) };
        anyhow::bail!(
            "UpdateProcThreadAttribute failed: {}",
            io::Error::last_os_error()
        );
    }

    // SAFETY: zeroed memory is valid for STARTUPINFOEXW and PROCESS_INFORMATION.
    let mut startup_info: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup_info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    // Invalid std handles, so the child attaches to the pseudo-console. Without
    // STARTF_USESTDHANDLES a child inherits this process's redirected stdio:
    // it writes straight to our stdout pipe and reads our stdin pipe, bypassing
    // ConPTY. Same as wezterm pty/src/win/pseudocon.rs spawn_command.
    startup_info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup_info.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    startup_info.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    startup_info.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    startup_info.lpAttributeList = attr_list;

    // SAFETY: PROCESS_INFORMATION is a plain C struct; zeroed is valid.
    let mut proc_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    // SAFETY: startup_info and proc_info are properly initialized. cmd_wide is a
    // null-terminated UTF-16 string. EXTENDED_STARTUPINFO_PRESENT tells CreateProcessW
    // to interpret startup_info as STARTUPINFOEXW containing the ConPTY attribute.
    if unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmd_wide.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0, // FALSE - don't inherit handles
            EXTENDED_STARTUPINFO_PRESENT,
            std::ptr::null(),
            std::ptr::null(),
            &startup_info.StartupInfo as *const STARTUPINFOW,
            &mut proc_info,
        )
    } == 0
    {
        // SAFETY: attr_list was initialized; cleaning up on error path.
        unsafe { DeleteProcThreadAttributeList(attr_list) };
        anyhow::bail!("CreateProcessW failed: {}", io::Error::last_os_error());
    }

    // SAFETY: attr_list is no longer needed after CreateProcessW succeeds.
    unsafe { DeleteProcThreadAttributeList(attr_list) };

    Ok((proc_info.hProcess, proc_info.hThread))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_manager::HistoryManager;
    use crate::sync_block::SyncBlockParser;

    // Alt screen + sync block integration test
    fn history_text(hm: &HistoryManager) -> String {
        let mut output = Vec::new();
        hm.append_all(&mut output);
        String::from_utf8_lossy(&output).into_owned()
    }

    #[test]
    fn test_alt_screen_enter_during_sync_block() {
        let mut parser = SyncBlockParser::new();
        let mut hm = HistoryManager::new(10000);
        let alt_tracker = AltScreenTracker::new();

        let mut segments = Vec::new();
        parser.parse(b"initial output\r\n", &mut segments);
        for seg in segments.drain(..) {
            hm.apply_segment(seg);
        }

        let mut chunk = Vec::new();
        chunk.extend_from_slice(SYNC_START);
        chunk.extend_from_slice(b"sync content before alt ");
        chunk.extend_from_slice(ALT_SCREEN_ENTER);
        chunk.extend_from_slice(b"alt screen content");

        let alt_pos = alt_tracker.find_enter(&chunk).unwrap();
        let before_alt = &chunk[..alt_pos];
        parser.parse(before_alt, &mut segments);
        for seg in segments.drain(..) {
            hm.apply_segment(seg);
        }

        assert!(parser.in_sync_block());
        let remaining = &chunk[alt_pos..];
        let segment = parser.append_and_flush(remaining);
        hm.apply_segment(segment);

        assert!(!parser.in_sync_block());
        let text = history_text(&hm);
        assert!(text.contains("initial output"));
        assert!(text.contains("sync content before alt"));
    }

    // ================================================================
    // Windows argument quoting tests
    // ================================================================

    #[test]
    fn test_quote_simple_arg_no_quoting_needed() {
        // No spaces, quotes, or tabs — returned as-is
        assert_eq!(quote_arg_windows("hello"), "hello");
    }

    #[test]
    fn test_quote_arg_with_spaces() {
        assert_eq!(quote_arg_windows("hello world"), r#""hello world""#);
    }

    #[test]
    fn test_quote_arg_with_tab() {
        assert_eq!(quote_arg_windows("a\tb"), "\"a\tb\"");
    }

    #[test]
    fn test_quote_arg_with_embedded_quote() {
        assert_eq!(quote_arg_windows(r#"say "hi""#), r#""say \"hi\"""#);
    }

    #[test]
    fn test_quote_arg_with_backslash_before_quote() {
        // Contains a quote, so it gets quoted. Trailing backslash before
        // the embedded quote must be doubled.
        assert_eq!(quote_arg_windows(r#"path\"#), r"path\");
    }

    #[test]
    fn test_quote_arg_trailing_backslash_with_space() {
        // Has a space so it must be quoted, trailing backslash doubled
        assert_eq!(quote_arg_windows(r"path with\"), r#""path with\\""#);
    }

    #[test]
    fn test_quote_arg_with_backslashes_before_quote() {
        // Contains no spaces/quotes/tabs — returned as-is
        assert_eq!(quote_arg_windows(r"a\\"), r"a\\");
    }

    #[test]
    fn test_quote_arg_backslash_not_before_quote() {
        // No special chars — returned as-is
        assert_eq!(quote_arg_windows(r"a\b"), r"a\b");
    }

    #[test]
    fn test_quote_arg_empty() {
        assert_eq!(quote_arg_windows(""), r#""""#);
    }

    #[test]
    fn test_quote_arg_injection_attempt() {
        // This is the injection payload from the audit — contains quotes
        let malicious = r#"foo" & del /q C:\* & ""#;
        let quoted = quote_arg_windows(malicious);
        // The embedded quotes must be escaped so they can't break out
        assert_eq!(quoted, r#""foo\" & del /q C:\* & \"""#);
    }

    #[test]
    fn test_quote_arg_backslashes_before_embedded_quote() {
        // Backslashes immediately before an embedded quote must be doubled
        assert_eq!(quote_arg_windows(r#"a\\"b"#), r#""a\\\\\"b""#);
    }
}
