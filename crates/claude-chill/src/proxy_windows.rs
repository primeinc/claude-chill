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
use crate::sequence_match::{self, SequenceMatch};
use crate::sync_block::SyncBlockParser;
use crate::terminal_windows::{
    self, ConsoleMode, SIGINT_RECEIVED, SIGTERM_RECEIVED, SIGWINCH_RECEIVED, TerminalSize,
    get_terminal_size, setup_raw_mode, setup_signal_handlers,
};
use crate::vt_renderer::VtRenderer;
use anyhow::{Context, Result};
use log::debug;
use std::io::{self, Write};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, INVALID_HANDLE_VALUE, S_OK, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTUPINFOEXW, STARTUPINFOW,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

/// Configuration for the PTY proxy.
pub struct ProxyConfig {
    /// Maximum number of lines retained in the lookback history buffer.
    pub max_history_lines: usize,
    /// Human-readable key name for display in the lookback mode banner.
    pub lookback_key: String,
    /// Byte sequence that triggers lookback mode in legacy terminal mode.
    pub lookback_sequence_legacy: Vec<u8>,
    /// Byte sequence that triggers lookback mode in Kitty keyboard protocol mode.
    pub lookback_sequence_kitty: Vec<u8>,
    /// Idle timeout in ms before auto-lookback triggers (0 to disable).
    pub auto_lookback_timeout_ms: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            max_history_lines: 100_000,
            lookback_key: "[ctrl][6]".to_string(),
            lookback_sequence_legacy: vec![0x1E],
            lookback_sequence_kitty: b"\x1b[54;5u".to_vec(),
            auto_lookback_timeout_ms: 15000,
        }
    }
}

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
        let hr = unsafe {
            CreatePseudoConsole(size, pipe_input_read, pipe_output_write, 0, &mut conpty)
        };
        if hr != S_OK {
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
    pub fn run(&mut self) -> Result<i32> {
        let mut buf = [0u8; 65536];
        let mut stdout = io::stdout();

        loop {
            // Check for signals
            if SIGINT_RECEIVED.swap(false, Ordering::SeqCst) {
                // On Windows, Ctrl+C is handled via the console ctrl handler.
                // The child process gets its own Ctrl+C event from the console.
                // We just need to note we received it.
            }
            if SIGTERM_RECEIVED.swap(false, Ordering::SeqCst) {
                // Terminate the child
                unsafe {
                    windows_sys::Win32::System::Threading::TerminateProcess(self.child_process, 1);
                }
            }

            // Poll for resize (Windows doesn't have SIGWINCH)
            self.check_resize()?;

            // Wait for either child output or a short timeout (for stdin polling)
            let wait_result = unsafe {
                WaitForSingleObject(self.pipe_output_read, 10) // 10ms timeout
            };

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

            if wait_result == WAIT_OBJECT_0 {
                // Data available from child
                let mut bytes_read: u32 = 0;
                let ok = unsafe {
                    ReadFile(
                        self.pipe_output_read,
                        buf.as_mut_ptr(),
                        buf.len() as u32,
                        &mut bytes_read,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || bytes_read == 0 {
                    break; // Pipe closed or error
                }
                self.process_output(&buf[..bytes_read as usize], &mut stdout)?;
            } else if wait_result == WAIT_TIMEOUT {
                // Check for stdin input (non-blocking)
                self.poll_stdin(&mut buf, &mut stdout)?;
                self.check_auto_lookback(&mut stdout)?;
            } else if wait_result == WAIT_FAILED {
                // Check if child exited
                let child_wait = unsafe { WaitForSingleObject(self.child_process, 0) };
                if child_wait == WAIT_OBJECT_0 {
                    break;
                }
                anyhow::bail!("WaitForSingleObject failed: {}", io::Error::last_os_error());
            }

            // Check if child has exited
            let child_wait = unsafe { WaitForSingleObject(self.child_process, 0) };
            if child_wait == WAIT_OBJECT_0 {
                // Drain remaining output
                loop {
                    let mut bytes_read: u32 = 0;
                    let ok = unsafe {
                        ReadFile(
                            self.pipe_output_read,
                            buf.as_mut_ptr(),
                            buf.len() as u32,
                            &mut bytes_read,
                            std::ptr::null_mut(),
                        )
                    };
                    if ok == 0 || bytes_read == 0 {
                        break;
                    }
                    self.process_output(&buf[..bytes_read as usize], &mut stdout)?;
                }
                break;
            }
        }

        // Final render before exit
        if self.renderer.is_pending() {
            if let Some(bytes) = self.renderer.render() {
                self.kitty_tracker.process(bytes);
                stdout.write_all(bytes)?;
                stdout.flush()?;
            }
        }

        self.wait_child()
    }

    fn poll_stdin(&mut self, buf: &mut [u8], stdout: &mut io::Stdout) -> Result<()> {
        // Use non-blocking stdin read via Windows console input
        let stdin = io::stdin();
        let handle = stdin.lock();

        // Try to read available input (this may block briefly on Windows)
        // We rely on the 10ms timeout in the main loop to keep things responsive
        use windows_sys::Win32::System::Console::{
            GetNumberOfConsoleInputEvents, GetStdHandle, STD_INPUT_HANDLE,
        };
        let stdin_handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut num_events: u32 = 0;
        if unsafe { GetNumberOfConsoleInputEvents(stdin_handle, &mut num_events) } != 0
            && num_events > 0
        {
            // There's input available, read it
            let mut bytes_read: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    stdin_handle,
                    buf.as_mut_ptr(),
                    buf.len().min(4096) as u32,
                    &mut bytes_read,
                    std::ptr::null_mut(),
                )
            };
            if ok != 0 && bytes_read > 0 {
                self.process_input(&buf[..bytes_read as usize], stdout)?;
            }
        }
        drop(handle);
        Ok(())
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

            self.renderer.force_full_render();
            if let Some(bytes) = self.renderer.render() {
                self.kitty_tracker.process(bytes);
                stdout.write_all(bytes).context("write to stdout")?;
                stdout.flush()?;
            }

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

        self.renderer.force_full_render();
        if let Some(bytes) = self.renderer.render() {
            self.kitty_tracker.process(bytes);
            stdout.write_all(bytes).context("write to stdout")?;
            stdout.flush()?;
        }

        Ok(())
    }

    fn check_resize(&mut self) -> Result<()> {
        if let Ok(winsize) = get_terminal_size() {
            if winsize.ws_row != self.last_terminal_size.ws_row
                || winsize.ws_col != self.last_terminal_size.ws_col
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
                let hr = unsafe { ResizePseudoConsole(self.conpty, size) };
                if hr != S_OK {
                    debug!("ResizePseudoConsole failed: HRESULT 0x{hr:08x}");
                }
                SIGWINCH_RECEIVED.store(true, Ordering::SeqCst);
            }
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
        unsafe { WaitForSingleObject(self.child_process, 0xFFFFFFFF) }; // INFINITE
        let mut exit_code: u32 = 1;
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
        unsafe {
            ClosePseudoConsole(self.conpty);
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

/// Create a child process attached to the given ConPTY.
///
/// Per MS docs: uses InitializeProcThreadAttributeList + UpdateProcThreadAttribute
/// with PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, then CreateProcessW with
/// EXTENDED_STARTUPINFO_PRESENT.
fn create_child_process(conpty: HPCON, command: &str, args: &[&str]) -> Result<(HANDLE, HANDLE)> {
    // Build command line string
    let mut cmd_line = command.to_string();
    for arg in args {
        cmd_line.push(' ');
        // Simple quoting for args with spaces
        if arg.contains(' ') {
            cmd_line.push('"');
            cmd_line.push_str(arg);
            cmd_line.push('"');
        } else {
            cmd_line.push_str(arg);
        }
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
        unsafe { DeleteProcThreadAttributeList(attr_list) };
        anyhow::bail!(
            "UpdateProcThreadAttribute failed: {}",
            io::Error::last_os_error()
        );
    }

    // SAFETY: zeroed memory is valid for STARTUPINFOEXW and PROCESS_INFORMATION.
    let mut startup_info: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup_info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup_info.lpAttributeList = attr_list;

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
        unsafe { DeleteProcThreadAttributeList(attr_list) };
        anyhow::bail!("CreateProcessW failed: {}", io::Error::last_os_error());
    }

    unsafe { DeleteProcThreadAttributeList(attr_list) };

    Ok((proc_info.hProcess, proc_info.hThread))
}

/// Pure decision: should auto-lookback trigger?
#[must_use]
fn should_auto_lookback(
    timeout: Duration,
    in_lookback_mode: bool,
    in_alt_screen: bool,
    stdin_elapsed: Option<Duration>,
    render_time: Option<Instant>,
    last_auto_time: Option<Instant>,
    now: Instant,
) -> bool {
    if timeout.is_zero() || in_lookback_mode || in_alt_screen {
        return false;
    }

    let Some(stdin_idle) = stdin_elapsed else {
        return false;
    };
    if stdin_idle < timeout {
        return false;
    }

    let Some(render_t) = render_time else {
        return false;
    };

    if let Some(last_auto) = last_auto_time {
        let no_new_output = render_t <= last_auto;
        let too_soon = now.duration_since(last_auto) < timeout;
        if no_new_output || too_soon {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_manager::HistoryManager;
    use crate::sync_block::SyncBlockParser;

    // Auto-lookback tests (same as unix proxy)
    #[test]
    fn test_auto_lookback_disabled_when_timeout_zero() {
        let now = Instant::now();
        assert!(!should_auto_lookback(
            Duration::ZERO,
            false,
            false,
            Some(Duration::from_secs(100)),
            Some(now - Duration::from_secs(50)),
            None,
            now,
        ));
    }

    #[test]
    fn test_auto_lookback_triggers_first_time() {
        let now = Instant::now();
        assert!(should_auto_lookback(
            Duration::from_secs(15),
            false,
            false,
            Some(Duration::from_secs(20)),
            Some(now - Duration::from_secs(10)),
            None,
            now,
        ));
    }

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
}
