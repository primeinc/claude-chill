//! PTY proxy: event loop, lookback mode, and coordination between
//! [`VtRenderer`](crate::vt_renderer::VtRenderer),
//! [`HistoryManager`](crate::history_manager::HistoryManager),
//! and terminal I/O.

use crate::alt_screen::AltScreenTracker;
#[cfg(test)]
use crate::escape_sequences::ALT_SCREEN_ENTER;
#[cfg(test)]
use crate::escape_sequences::SYNC_START;
use crate::escape_sequences::{CLEAR_SCREEN, CURSOR_HOME, INPUT_BUFFER_CAPACITY};
use crate::history_manager::HistoryManager;
use crate::kitty_tracker::KittyTracker;
pub use crate::proxy_common::ProxyConfig;
use crate::proxy_common::should_auto_lookback;
use crate::sequence_match::{self, SequenceMatch};
use crate::sync_block::{OutputSegment, SyncBlockParser};
use crate::terminal::{
    self, SIGINT_RECEIVED, SIGTERM_RECEIVED, SIGWINCH_RECEIVED, exit_code_from_status,
    get_terminal_size, nix_read, set_nonblocking, setup_raw_mode, setup_signal_handlers, write_all,
};
use crate::vt_renderer::VtRenderer;
use anyhow::{Context, Result};
use log::debug;
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::openpty;
use nix::sys::signal::{Signal, kill};
use nix::sys::termios::Termios;
use nix::unistd::Pid;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// PTY proxy that sits between a terminal and a child process, providing
/// VT-based differential rendering and scrollback history.
pub struct Proxy {
    // Process & terminal
    config: ProxyConfig,
    pty_master: OwnedFd,
    child: Child,
    original_termios: Option<Termios>,

    // VT rendering
    renderer: VtRenderer,
    sync_parser: SyncBlockParser,

    // History & filtering
    history: HistoryManager,

    // Mode tracking
    in_lookback_mode: bool,
    alt_screen: AltScreenTracker,
    kitty_tracker: KittyTracker,

    // Lookback input matching
    lookback_input_buffer: Vec<u8>,
    lookback_cache: Vec<u8>,

    // Timing
    last_stdin_time: Option<Instant>,
    last_auto_lookback_time: Option<Instant>,
    auto_lookback_timeout: Duration,

    // Reusable write buffer for history dump / lookback
    output_buffer: Vec<u8>,
}

impl Proxy {
    /// Spawn a child process in a PTY and return a proxy ready to run.
    ///
    /// Sets up raw mode, signal handlers, and Kitty protocol detection before
    /// spawning the child. The caller should call [`run`](Self::run) to enter
    /// the event loop.
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
        let pty = openpty(&winsize, None).context("openpty failed")?;

        let original_termios = setup_raw_mode()?;
        setup_signal_handlers()?;

        // Detect Kitty support before spawning child
        let kitty_tracker = crate::kitty_tracker::detect();

        let slave_fd = pty.slave.as_raw_fd();

        // SAFETY: pre_exec runs between fork and exec in the child process.
        // slave_fd is a valid file descriptor obtained from openpty above.
        // We call setsid/TIOCSCTTY to establish a new session with the PTY as
        // controlling terminal, dup2 to wire stdin/stdout/stderr to the slave,
        // and close the original fd if it's not one of 0/1/2.
        let child = unsafe {
            Command::new(command)
                .args(args)
                .pre_exec(move || {
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::dup2(slave_fd, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::dup2(slave_fd, 1) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::dup2(slave_fd, 2) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if slave_fd > 2 {
                        libc::close(slave_fd);
                    }
                    Ok(())
                })
                .spawn()
                .context("spawn failed")?
        };

        drop(pty.slave);
        set_nonblocking(&pty.master)?;

        let renderer = VtRenderer::new(winsize.ws_row, winsize.ws_col);

        let history = HistoryManager::new(config.max_history_lines);

        let auto_lookback_timeout = Duration::from_millis(config.auto_lookback_timeout_ms);

        debug!("Proxy::spawn: command={command} args={args:?}");

        Ok(Self {
            history,
            config,
            pty_master: pty.master,
            child,
            original_termios,
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
        })
    }

    /// Run the proxy event loop until the child process exits.
    /// Returns the child's exit code.
    pub fn run(&mut self) -> Result<i32> {
        let stdin_fd = io::stdin();
        let stdout_fd = io::stdout();

        let mut buf = [0u8; 65536];

        loop {
            if SIGWINCH_RECEIVED.swap(false, Ordering::SeqCst) {
                self.forward_winsize()?;
            }
            if SIGINT_RECEIVED.swap(false, Ordering::SeqCst) {
                self.forward_signal(Signal::SIGINT);
            }
            if SIGTERM_RECEIVED.swap(false, Ordering::SeqCst) {
                self.forward_signal(Signal::SIGTERM);
            }

            // SAFETY: pty_master is an OwnedFd that outlives the poll call.
            // stdin_fd is io::stdin() which lives for the duration of the loop.
            // The BorrowedFd references are only used within this loop iteration.
            let master_fd = unsafe { BorrowedFd::borrow_raw(self.pty_master.as_raw_fd()) };
            let stdin_borrowed = unsafe { BorrowedFd::borrow_raw(stdin_fd.as_raw_fd()) };

            let mut poll_fds = [
                PollFd::new(master_fd, PollFlags::POLLIN),
                PollFd::new(stdin_borrowed, PollFlags::POLLIN),
            ];

            let poll_timeout_ms = self
                .renderer
                .time_until_render(
                    self.in_lookback_mode,
                    self.alt_screen.in_alternate_screen(),
                    self.sync_parser.in_sync_block(),
                )
                .map(|d| d.as_millis().min(100) as u16)
                .unwrap_or(100);

            match poll(&mut poll_fds, PollTimeout::from(poll_timeout_ms)) {
                Ok(0) => {
                    self.flush_pending_vt_render(&stdout_fd)?;
                    self.check_auto_lookback(&stdout_fd)?;
                    continue;
                }
                Ok(_) => {}
                Err(Errno::EINTR) => continue,
                Err(e) => anyhow::bail!("poll failed: {e}"),
            }

            self.flush_pending_vt_render(&stdout_fd)?;

            if let Some(revents) = poll_fds[0].revents() {
                if revents.contains(PollFlags::POLLIN) {
                    match nix_read(&self.pty_master, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => self.process_output(&buf[..n], &stdout_fd)?,
                        Err(Errno::EAGAIN) => {}
                        Err(Errno::EIO) => break,
                        Err(e) => anyhow::bail!("read from pty failed: {e}"),
                    }
                }
                if revents.contains(PollFlags::POLLHUP) {
                    break;
                }
            }

            if let Some(revents) = poll_fds[1].revents()
                && revents.contains(PollFlags::POLLIN)
            {
                match nix_read(&stdin_fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => self.process_input(&buf[..n], &stdout_fd)?,
                    Err(Errno::EAGAIN) => {}
                    Err(e) => anyhow::bail!("read from stdin failed: {e}"),
                }
            }
        }

        // Final render before exit
        if self.renderer.is_pending() {
            self.render_vt_screen(&stdout_fd)?;
        }

        self.wait_child()
    }

    fn process_output<F: AsFd>(&mut self, data: &[u8], stdout_fd: &F) -> Result<()> {
        debug!(
            "process_output: len={} in_alt={} in_lookback={}",
            data.len(),
            self.alt_screen.in_alternate_screen(),
            self.in_lookback_mode,
        );

        if self.alt_screen.in_alternate_screen() {
            // Feed VT for screen tracking but skip history (alt screen content
            // like TUI editors shouldn't appear in lookback)
            self.renderer.process(data);
            return self.process_output_alt_screen(data, stdout_fd);
        }

        if self.in_lookback_mode {
            debug!("process_output: caching {} bytes for lookback", data.len());
            self.lookback_cache.extend_from_slice(data);
            return Ok(());
        }

        self.renderer.process(data);
        self.renderer.mark_pending();

        // Process sync blocks for history management, watching for alt screen enter
        if let Some(alt_pos) = self.alt_screen.find_enter(data) {
            debug!("process_output: ALT_SCREEN_ENTER detected at pos={alt_pos}");
            // Parse sync blocks in data up to the alt screen enter point,
            // then flush any remaining sync block state with the rest
            let before_alt = &data[..alt_pos];
            if !before_alt.is_empty() {
                let mut segments = Vec::with_capacity(4);
                self.sync_parser.parse(before_alt, &mut segments);
                for segment in segments {
                    self.apply_segment_to_history(segment);
                }
            }
            // Flush any open sync block with remaining data
            let remaining = &data[alt_pos..];
            if self.sync_parser.in_sync_block() {
                let segment = self.sync_parser.append_and_flush(remaining);
                self.apply_segment_to_history(segment);
            } else {
                self.push_to_history(remaining);
            }
            self.alt_screen.set_alternate_screen(true);
            let seq_len = self.alt_screen.enter_len(&data[alt_pos..]);
            self.write_to_terminal(stdout_fd, &data[alt_pos..alt_pos + seq_len])?;
            return self.process_output_alt_screen(&data[alt_pos + seq_len..], stdout_fd);
        }

        let mut segments = Vec::with_capacity(4);
        self.sync_parser.parse(data, &mut segments);
        for segment in segments {
            self.apply_segment_to_history(segment);
        }

        Ok(())
    }

    /// Handle output while in alternate screen mode (TUI editors, etc.).
    ///
    /// Passes data directly to the terminal. When an alt-screen exit is found,
    /// restores the main screen via a full VT render and checks remaining data
    /// for another alt-screen enter (handles rapid enter/exit in a single chunk).
    fn process_output_alt_screen<F: AsFd>(&mut self, data: &[u8], stdout_fd: &F) -> Result<()> {
        if let Some(exit_pos) = self.alt_screen.find_exit(data) {
            debug!("process_output_alt_screen: ALT_SCREEN_EXIT detected at pos={exit_pos}");
            self.write_to_terminal(stdout_fd, &data[..exit_pos])?;
            let seq_len = self.alt_screen.exit_len(&data[exit_pos..]);
            self.write_to_terminal(stdout_fd, &data[exit_pos..exit_pos + seq_len])?;
            self.alt_screen.set_alternate_screen(false);

            // Force full VT render to restore main screen content
            debug!("process_output_alt_screen: rendering VT screen after alt exit");
            self.renderer.force_full_render();
            self.render_vt_screen(stdout_fd)?;

            // Data after ALT_EXIT was already fed to VT and history when we processed
            // the alt screen chunk, so we just need to check for more alt screen transitions
            let remaining = &data[exit_pos + seq_len..];
            if !remaining.is_empty() {
                // Check if there's another alt screen enter in the remaining data
                if self.alt_screen.find_enter(remaining).is_some() {
                    // Need to process for alt screen detection, but skip VT/history feed
                    return self.process_output_check_alt_only(remaining, stdout_fd);
                }
            }
            return Ok(());
        }
        self.write_to_terminal(stdout_fd, data)
    }

    /// Check for alt screen transitions without re-feeding VT/history
    fn process_output_check_alt_only<F: AsFd>(&mut self, data: &[u8], stdout_fd: &F) -> Result<()> {
        if let Some(alt_pos) = self.alt_screen.find_enter(data) {
            debug!("process_output_check_alt_only: ALT_SCREEN_ENTER at pos={alt_pos}");
            self.alt_screen.set_alternate_screen(true);
            let seq_len = self.alt_screen.enter_len(&data[alt_pos..]);
            self.write_to_terminal(stdout_fd, &data[alt_pos..alt_pos + seq_len])?;
            return self.process_output_alt_screen(&data[alt_pos + seq_len..], stdout_fd);
        }
        Ok(())
    }

    /// Write data to the terminal and track Kitty keyboard protocol state
    fn write_to_terminal<F: AsFd>(&mut self, stdout_fd: &F, data: &[u8]) -> Result<()> {
        write_to_terminal(&mut self.kitty_tracker, stdout_fd, data)
    }

    fn apply_segment_to_history(&mut self, segment: OutputSegment<'_>) {
        self.history.apply_segment(segment);
    }

    fn push_to_history(&mut self, data: &[u8]) {
        self.history.push(data);
    }

    fn flush_pending_vt_render<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        if let Some(bytes) = self.renderer.flush_if_ready(
            self.in_lookback_mode,
            self.alt_screen.in_alternate_screen(),
            self.sync_parser.in_sync_block(),
        ) {
            write_to_terminal(&mut self.kitty_tracker, stdout_fd, bytes)?;
        }
        Ok(())
    }

    fn render_vt_screen<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        if let Some(bytes) = self.renderer.render() {
            write_to_terminal(&mut self.kitty_tracker, stdout_fd, bytes)?;
        }
        Ok(())
    }

    fn check_auto_lookback<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
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

        debug!(
            "auto_lookback triggered: stdin_idle={}ms render_age={}ms last_auto_age={}ms",
            self.last_stdin_time
                .map(|t| t.elapsed().as_millis())
                .unwrap_or(0),
            self.renderer
                .last_render_time()
                .map(|t| t.elapsed().as_millis())
                .unwrap_or(0),
            self.last_auto_lookback_time
                .map(|t| t.elapsed().as_millis())
                .unwrap_or(0)
        );
        self.dump_history(stdout_fd)?;
        self.last_auto_lookback_time = Some(Instant::now());
        Ok(())
    }

    fn dump_history<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        debug_assert!(
            !self.in_lookback_mode,
            "dump_history called while in lookback mode"
        );
        debug!(
            "dump_history: history_bytes={} lines={}",
            self.history.total_bytes(),
            self.history.line_count()
        );
        self.output_buffer.clear();
        self.history.append_all(&mut self.output_buffer);

        write_to_terminal(&mut self.kitty_tracker, stdout_fd, CLEAR_SCREEN)?;
        write_to_terminal(&mut self.kitty_tracker, stdout_fd, CURSOR_HOME)?;
        write_to_terminal(&mut self.kitty_tracker, stdout_fd, &self.output_buffer)?;

        // Force full VT render on next output since terminal now shows history
        self.renderer.force_full_render();
        Ok(())
    }

    fn process_input<F: AsFd>(&mut self, data: &[u8], stdout_fd: &F) -> Result<()> {
        self.last_stdin_time = Some(Instant::now());

        debug!("process_input: stdin={data:?}");

        if self.alt_screen.in_alternate_screen() {
            return write_all(&self.pty_master, data);
        }

        // Stack-copy the active lookback sequence to avoid borrowing self.config
        // across mutable calls. Lookback sequences are always small (≤16 bytes).
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
                self.exit_lookback_mode(stdout_fd)?;
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
                        self.exit_lookback_mode(stdout_fd)?;
                    } else {
                        self.enter_lookback_mode(stdout_fd)?;
                    }
                    continue;
                }
                SequenceMatch::Partial => {
                    // Still might be lookback sequence, don't forward yet
                    continue;
                }
                SequenceMatch::None => {
                    // Not a lookback sequence - forward all buffered bytes
                    if !self.in_lookback_mode {
                        write_all(&self.pty_master, &self.lookback_input_buffer)?;
                    }
                    self.lookback_input_buffer.clear();
                }
            }
        }
        Ok(())
    }

    fn enter_lookback_mode<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        debug_assert!(
            !self.in_lookback_mode,
            "enter_lookback_mode called while already in lookback mode"
        );
        debug_assert!(
            !self.alt_screen.in_alternate_screen(),
            "enter_lookback_mode called while in alt screen"
        );
        debug!(
            "enter_lookback_mode: history_bytes={} lines={}",
            self.history.total_bytes(),
            self.history.line_count()
        );
        self.in_lookback_mode = true;
        self.lookback_cache.clear();
        self.renderer.cancel_pending();

        self.output_buffer.clear();
        self.history.append_all(&mut self.output_buffer);
        debug!(
            "enter_lookback_mode: output_buffer_len={}",
            self.output_buffer.len()
        );

        write_to_terminal(&mut self.kitty_tracker, stdout_fd, CLEAR_SCREEN)?;
        write_to_terminal(&mut self.kitty_tracker, stdout_fd, CURSOR_HOME)?;
        write_to_terminal(&mut self.kitty_tracker, stdout_fd, &self.output_buffer)?;

        let exit_msg = format!(
            "\r\n\x1b[7m--- LOOKBACK MODE: press {} or Ctrl+C to exit ---\x1b[0m\r\n",
            self.config.lookback_key
        );
        write_all(stdout_fd, exit_msg.as_bytes())?;

        Ok(())
    }

    fn exit_lookback_mode<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        debug_assert!(
            self.in_lookback_mode,
            "exit_lookback_mode called while not in lookback mode"
        );
        debug!(
            "exit_lookback_mode: cached_len={}",
            self.lookback_cache.len()
        );
        self.in_lookback_mode = false;

        // Process cached output through VT to update screen state
        let cached = std::mem::take(&mut self.lookback_cache);
        if !cached.is_empty() {
            debug!(
                "exit_lookback_mode: processing {} cached bytes",
                cached.len()
            );
            self.process_output(&cached, stdout_fd)?;
        }

        // Reset sync block state
        self.sync_parser.reset();

        self.forward_winsize()?;

        // Force full render since terminal was showing history
        debug!("exit_lookback_mode: rendering VT screen");
        self.renderer.force_full_render();
        self.render_vt_screen(stdout_fd)?;

        Ok(())
    }

    fn forward_winsize(&mut self) -> Result<()> {
        if let Ok(winsize) = get_terminal_size() {
            debug!(
                "forward_winsize: rows={} cols={}",
                winsize.ws_row, winsize.ws_col
            );
            // Resize VT emulator and force full render on next frame
            self.renderer.resize(winsize.ws_row, winsize.ws_col);
            // SAFETY: pty_master is a valid fd (OwnedFd), winsize is a valid
            // Winsize struct just obtained from get_terminal_size. TIOCSWINSZ
            // reads the struct and applies the size to the PTY.
            let ret = unsafe {
                libc::ioctl(
                    self.pty_master.as_raw_fd(),
                    libc::TIOCSWINSZ as libc::c_ulong,
                    &winsize,
                )
            };
            if ret == -1 {
                debug!(
                    "forward_winsize: TIOCSWINSZ ioctl failed: {}",
                    io::Error::last_os_error()
                );
            }
        }
        Ok(())
    }

    fn forward_signal(&self, signal: Signal) {
        let pid = Pid::from_raw(self.child.id() as i32);
        let _ = kill(pid, signal);
    }

    fn wait_child(&mut self) -> Result<i32> {
        match self.child.wait() {
            Ok(status) => Ok(exit_code_from_status(status)),
            Err(e) => anyhow::bail!("wait failed: {e}"),
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        if let Some(ref termios) = self.original_termios {
            terminal::restore_termios(termios);
        }
    }
}

/// Write data to the terminal while tracking Kitty keyboard protocol state.
fn write_to_terminal<F: AsFd>(kitty: &mut KittyTracker, stdout_fd: &F, data: &[u8]) -> Result<()> {
    kitty.process(data);
    write_all(stdout_fd, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history_text(hm: &HistoryManager) -> String {
        let mut output = Vec::new();
        hm.append_all(&mut output);
        String::from_utf8_lossy(&output).into_owned()
    }

    #[test]
    fn test_alt_screen_enter_during_sync_block() {
        // Simulates the scenario where alt screen enter occurs in the
        // middle of a sync block.
        let mut parser = SyncBlockParser::new();
        let mut hm = HistoryManager::new(10000);
        let alt_tracker = AltScreenTracker::new();

        // Preamble text
        let mut segments = Vec::new();
        parser.parse(b"initial output\r\n", &mut segments);
        for seg in segments.drain(..) {
            hm.apply_segment(seg);
        }

        // Build a chunk that starts a sync block, then has alt screen enter mid-block
        let mut chunk = Vec::new();
        chunk.extend_from_slice(SYNC_START);
        chunk.extend_from_slice(b"sync content before alt ");
        chunk.extend_from_slice(ALT_SCREEN_ENTER);
        chunk.extend_from_slice(b"alt screen content");

        // Find where alt screen enter is
        let alt_pos = alt_tracker.find_enter(&chunk);
        assert!(
            alt_pos.is_some(),
            "should find alt screen enter in the chunk"
        );
        let alt_pos = alt_pos.unwrap();

        // Parse data up to alt screen enter point
        let before_alt = &chunk[..alt_pos];
        parser.parse(before_alt, &mut segments);
        for seg in segments.drain(..) {
            hm.apply_segment(seg);
        }

        // Flush the open sync block with remaining data (simulating what proxy does)
        assert!(
            parser.in_sync_block(),
            "should be in sync block when alt screen hits"
        );
        let remaining = &chunk[alt_pos..];
        let segment = parser.append_and_flush(remaining);
        hm.apply_segment(segment);

        assert!(
            !parser.in_sync_block(),
            "sync block should be flushed after alt screen"
        );

        let text = history_text(&hm);
        assert!(
            text.contains("initial output"),
            "preamble should be in history"
        );
        assert!(
            text.contains("sync content before alt"),
            "pre-alt sync content should be in history"
        );
    }
}
