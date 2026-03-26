//! PTY proxy: event loop, VT rendering, history management, and lookback mode.

use crate::alt_screen::AltScreenTracker;
#[cfg(test)]
use crate::escape_sequences::ALT_SCREEN_ENTER;
use crate::escape_sequences::{
    CLEAR_SCREEN, CURSOR_HOME, INPUT_BUFFER_CAPACITY, OUTPUT_BUFFER_CAPACITY, SYNC_END, SYNC_START,
};
use crate::history_filter::HistoryFilter;
use crate::kitty_tracker::KittyTracker;
use crate::line_buffer::LineBuffer;
use crate::sequence_match::{self, SequenceMatch};
use crate::sync_block::{OutputSegment, SyncBlockParser};
use crate::terminal::{
    self, SIGINT_RECEIVED, SIGTERM_RECEIVED, SIGWINCH_RECEIVED, exit_code_from_status,
    get_terminal_size, nix_read, set_nonblocking, setup_raw_mode, setup_signal_handlers, write_all,
};
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

const RENDER_DELAY_MS: u64 = 5;
const SYNC_BLOCK_DELAY_MS: u64 = 50;

/// PTY proxy that sits between a terminal and a child process, providing
/// VT-based differential rendering and scrollback history.
pub struct Proxy {
    // Process & terminal
    config: ProxyConfig,
    pty_master: OwnedFd,
    child: Child,
    original_termios: Option<Termios>,

    // VT emulation
    vt_parser: vt100::Parser,
    vt_prev_screen: Option<vt100::Screen>,
    vt_render_pending: bool,
    sync_parser: SyncBlockParser,

    // History & filtering
    history: LineBuffer,
    history_filter: HistoryFilter,

    // Mode tracking
    in_lookback_mode: bool,
    alt_screen: AltScreenTracker,
    kitty_tracker: KittyTracker,

    // Lookback input matching
    lookback_input_buffer: Vec<u8>,
    lookback_cache: Vec<u8>,

    // Timing
    last_output_time: Option<Instant>,
    last_render_time: Option<Instant>,
    last_stdin_time: Option<Instant>,
    last_auto_lookback_time: Option<Instant>,
    auto_lookback_timeout: Duration,

    // Reusable write buffer (avoids per-render allocation)
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

        let vt_parser = vt100::Parser::new(winsize.ws_row, winsize.ws_col, 0);

        // Seed history with clear screen so replay starts fresh
        let mut history = LineBuffer::new(config.max_history_lines);
        history.push_bytes(CLEAR_SCREEN);
        history.push_bytes(CURSOR_HOME);

        let auto_lookback_timeout = Duration::from_millis(config.auto_lookback_timeout_ms);

        debug!("Proxy::spawn: command={command} args={args:?}");

        Ok(Self {
            history,
            history_filter: HistoryFilter::new(),
            config,
            pty_master: pty.master,
            child,
            original_termios,
            vt_parser,
            vt_prev_screen: None,
            last_output_time: None,
            last_render_time: None,
            last_stdin_time: None,
            last_auto_lookback_time: None,
            auto_lookback_timeout,
            sync_parser: SyncBlockParser::new(),
            in_lookback_mode: false,
            alt_screen: AltScreenTracker::new(),
            kitty_tracker,
            vt_render_pending: false,
            lookback_cache: Vec::new(),
            lookback_input_buffer: Vec::with_capacity(INPUT_BUFFER_CAPACITY),
            output_buffer: Vec::with_capacity(OUTPUT_BUFFER_CAPACITY),
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
                .time_until_render()
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
        if self.vt_render_pending {
            self.render_vt_screen(&stdout_fd)?;
        }

        self.wait_child()
    }

    fn process_output<F: AsFd>(&mut self, data: &[u8], stdout_fd: &F) -> Result<()> {
        self.process_output_inner(data, stdout_fd, true)
    }

    fn process_output_inner<F: AsFd>(
        &mut self,
        data: &[u8],
        stdout_fd: &F,
        feed_vt: bool,
    ) -> Result<()> {
        debug!(
            "process_output: len={} in_alt={} in_lookback={} feed_vt={}",
            data.len(),
            self.alt_screen.in_alternate_screen(),
            self.in_lookback_mode,
            feed_vt
        );

        if self.alt_screen.in_alternate_screen() {
            // Feed VT but NOT history while in alt screen
            // Alt screen content (TUI editors, etc.) shouldn't be in lookback history
            if feed_vt {
                self.vt_parser.process(data);
            }
            return self.process_output_alt_screen(data, stdout_fd);
        }

        if self.in_lookback_mode {
            debug!("process_output: caching {} bytes for lookback", data.len());
            self.lookback_cache.extend_from_slice(data);
            return Ok(());
        }

        // Feed data to VT emulator (unless already fed by caller)
        if feed_vt {
            self.vt_parser.process(data);
        }
        self.vt_render_pending = true;
        self.last_output_time = Some(Instant::now());

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

    fn process_output_alt_screen<F: AsFd>(&mut self, data: &[u8], stdout_fd: &F) -> Result<()> {
        if let Some(exit_pos) = self.alt_screen.find_exit(data) {
            debug!("process_output_alt_screen: ALT_SCREEN_EXIT detected at pos={exit_pos}");
            self.write_to_terminal(stdout_fd, &data[..exit_pos])?;
            let seq_len = self.alt_screen.exit_len(&data[exit_pos..]);
            self.write_to_terminal(stdout_fd, &data[exit_pos..exit_pos + seq_len])?;
            self.alt_screen.set_alternate_screen(false);

            // Force full VT render to restore main screen content
            debug!("process_output_alt_screen: rendering VT screen after alt exit");
            self.vt_prev_screen = None;
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
        self.kitty_tracker.process(data);
        write_all(stdout_fd, data)
    }

    fn apply_segment_to_history(&mut self, segment: OutputSegment<'_>) {
        match segment {
            OutputSegment::PassThrough(data) => {
                self.push_to_history(data);
            }
            OutputSegment::SyncBlock {
                data,
                is_full_redraw,
            } => {
                if is_full_redraw {
                    debug!("CLEARING HISTORY");
                    self.history.clear();
                    self.history.push_bytes(CLEAR_SCREEN);
                    self.history.push_bytes(CURSOR_HOME);
                }
                self.push_to_history(&data);
            }
        }
    }

    /// Push data to history, filtering out terminal query sequences that would
    /// cause the terminal to respond when replayed.
    fn push_to_history(&mut self, data: &[u8]) {
        let filtered = self.history_filter.filter(data);
        self.history.push_bytes(filtered.as_ref());
    }

    fn flush_pending_vt_render<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        if let Some(Duration::ZERO) = self.time_until_render() {
            self.render_vt_screen(stdout_fd)?;
        }
        Ok(())
    }

    fn time_until_render(&self) -> Option<Duration> {
        compute_render_delay(
            self.vt_render_pending,
            self.in_lookback_mode,
            self.alt_screen.in_alternate_screen(),
            self.sync_parser.in_sync_block(),
            self.last_output_time.map(|t| t.elapsed()),
        )
    }

    fn render_vt_screen<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        let is_diff = self.vt_prev_screen.is_some();
        self.output_buffer.clear();
        self.output_buffer.extend_from_slice(SYNC_START);

        match &self.vt_prev_screen {
            Some(prev) => {
                // Diff-based render: only send changes
                self.output_buffer
                    .extend_from_slice(&self.vt_parser.screen().contents_diff(prev));
            }
            None => {
                // First render: full screen
                self.output_buffer
                    .extend_from_slice(&self.vt_parser.screen().contents_formatted());
            }
        }

        self.output_buffer
            .extend_from_slice(&self.vt_parser.screen().cursor_state_formatted());
        self.output_buffer.extend_from_slice(SYNC_END);

        debug!(
            "render_vt_screen: diff={} output_len={}\n",
            is_diff,
            self.output_buffer.len()
        );
        // Take the buffer temporarily to avoid borrow conflict with write_to_terminal
        let buf = std::mem::take(&mut self.output_buffer);
        self.write_to_terminal(stdout_fd, &buf)?;
        self.output_buffer = buf;

        // Store current screen for next diff
        self.vt_prev_screen = Some(self.vt_parser.screen().clone());
        self.vt_render_pending = false;
        self.last_render_time = Some(Instant::now());
        Ok(())
    }

    fn check_auto_lookback<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        let now = Instant::now();
        if !should_auto_lookback(
            self.auto_lookback_timeout,
            self.in_lookback_mode,
            self.alt_screen.in_alternate_screen(),
            self.last_stdin_time.map(|t| now.duration_since(t)),
            self.last_render_time,
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
            self.last_render_time
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

        // Debug: write history to file if CLAUDE_CHILL_HISTORY_FILE is set
        if let Ok(path) = std::env::var("CLAUDE_CHILL_HISTORY_FILE")
            && let Err(e) = std::fs::write(&path, &self.output_buffer)
        {
            debug!("Failed to write history file: {e}");
        }

        self.write_to_terminal(stdout_fd, CLEAR_SCREEN)?;
        self.write_to_terminal(stdout_fd, CURSOR_HOME)?;
        // Take the buffer temporarily to avoid borrow conflict with write_to_terminal
        let buf = std::mem::take(&mut self.output_buffer);
        self.write_to_terminal(stdout_fd, &buf)?;
        self.output_buffer = buf;

        // Force full VT render on next output since terminal now shows history
        self.vt_prev_screen = None;
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
        self.vt_render_pending = false;

        self.output_buffer.clear();
        self.history.append_all(&mut self.output_buffer);
        debug!(
            "enter_lookback_mode: output_buffer_len={}",
            self.output_buffer.len()
        );

        self.write_to_terminal(stdout_fd, CLEAR_SCREEN)?;
        self.write_to_terminal(stdout_fd, CURSOR_HOME)?;
        // Take the buffer temporarily to avoid borrow conflict with write_to_terminal
        let buf = std::mem::take(&mut self.output_buffer);
        self.write_to_terminal(stdout_fd, &buf)?;
        self.output_buffer = buf;

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
        self.vt_prev_screen = None;
        self.render_vt_screen(stdout_fd)?;

        Ok(())
    }

    fn forward_winsize(&mut self) -> Result<()> {
        if let Ok(winsize) = get_terminal_size() {
            debug!(
                "forward_winsize: rows={} cols={}",
                winsize.ws_row, winsize.ws_col
            );
            // Resize VT emulator
            self.vt_parser
                .screen_mut()
                .set_size(winsize.ws_row, winsize.ws_col);
            // Force full render on next frame since size changed
            self.vt_prev_screen = None;
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

/// Pure decision: should auto-lookback trigger?
/// Returns true if all conditions for auto-lookback are met.
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

/// Pure decision: compute the delay before the next VT render.
/// Returns None if no render is pending or rendering is suppressed.
fn compute_render_delay(
    vt_render_pending: bool,
    in_lookback_mode: bool,
    in_alt_screen: bool,
    in_sync_block: bool,
    output_elapsed: Option<Duration>,
) -> Option<Duration> {
    if !vt_render_pending || in_lookback_mode || in_alt_screen {
        return None;
    }

    let elapsed = output_elapsed.unwrap_or(Duration::MAX);

    let delay = if in_sync_block {
        Duration::from_millis(SYNC_BLOCK_DELAY_MS)
    } else {
        Duration::from_millis(RENDER_DELAY_MS)
    };

    if elapsed >= delay {
        Some(Duration::ZERO)
    } else {
        Some(delay - elapsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ================================================================
    // Auto-lookback decision tests
    // ================================================================

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
    fn test_auto_lookback_disabled_in_lookback_mode() {
        let now = Instant::now();
        assert!(!should_auto_lookback(
            Duration::from_secs(15),
            true,
            false,
            Some(Duration::from_secs(100)),
            Some(now - Duration::from_secs(50)),
            None,
            now,
        ));
    }

    #[test]
    fn test_auto_lookback_disabled_in_alt_screen() {
        let now = Instant::now();
        assert!(!should_auto_lookback(
            Duration::from_secs(15),
            false,
            true,
            Some(Duration::from_secs(100)),
            Some(now - Duration::from_secs(50)),
            None,
            now,
        ));
    }

    #[test]
    fn test_auto_lookback_disabled_no_stdin() {
        let now = Instant::now();
        assert!(!should_auto_lookback(
            Duration::from_secs(15),
            false,
            false,
            None,
            Some(now - Duration::from_secs(50)),
            None,
            now,
        ));
    }

    #[test]
    fn test_auto_lookback_disabled_stdin_too_recent() {
        let now = Instant::now();
        assert!(!should_auto_lookback(
            Duration::from_secs(15),
            false,
            false,
            Some(Duration::from_secs(5)), // only 5s idle, need 15s
            Some(now - Duration::from_secs(50)),
            None,
            now,
        ));
    }

    #[test]
    fn test_auto_lookback_disabled_no_render() {
        let now = Instant::now();
        assert!(!should_auto_lookback(
            Duration::from_secs(15),
            false,
            false,
            Some(Duration::from_secs(20)),
            None, // no render has happened
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
            Some(Duration::from_secs(20)), // idle 20s > 15s timeout
            Some(now - Duration::from_secs(10)), // rendered 10s ago
            None,                          // never auto-lookbacked before
            now,
        ));
    }

    #[test]
    fn test_auto_lookback_suppressed_no_new_output() {
        let now = Instant::now();
        let last_auto = now - Duration::from_secs(20);
        let render_before_auto = now - Duration::from_secs(25);
        // render_time <= last_auto_time means no new output since last auto
        assert!(!should_auto_lookback(
            Duration::from_secs(15),
            false,
            false,
            Some(Duration::from_secs(30)),
            Some(render_before_auto),
            Some(last_auto),
            now,
        ));
    }

    #[test]
    fn test_auto_lookback_suppressed_too_soon() {
        let now = Instant::now();
        let last_auto = now - Duration::from_secs(5); // only 5s ago
        let render_after_auto = now - Duration::from_secs(3); // new output exists
        assert!(!should_auto_lookback(
            Duration::from_secs(15),
            false,
            false,
            Some(Duration::from_secs(30)),
            Some(render_after_auto),
            Some(last_auto),
            now,
        ));
    }

    #[test]
    fn test_auto_lookback_triggers_with_new_output_after_cooldown() {
        let now = Instant::now();
        let last_auto = now - Duration::from_secs(20); // 20s ago, past 15s cooldown
        let render_after_auto = now - Duration::from_secs(10); // new output since last auto
        assert!(should_auto_lookback(
            Duration::from_secs(15),
            false,
            false,
            Some(Duration::from_secs(30)),
            Some(render_after_auto),
            Some(last_auto),
            now,
        ));
    }

    // ================================================================
    // Render delay decision tests
    // ================================================================

    #[test]
    fn test_render_delay_none_when_not_pending() {
        assert_eq!(
            compute_render_delay(false, false, false, false, Some(Duration::from_millis(100))),
            None,
        );
    }

    #[test]
    fn test_render_delay_none_in_lookback() {
        assert_eq!(
            compute_render_delay(true, true, false, false, Some(Duration::from_millis(100))),
            None,
        );
    }

    #[test]
    fn test_render_delay_none_in_alt_screen() {
        assert_eq!(
            compute_render_delay(true, false, true, false, Some(Duration::from_millis(100))),
            None,
        );
    }

    #[test]
    fn test_render_delay_immediate_when_enough_time_passed() {
        assert_eq!(
            compute_render_delay(true, false, false, false, Some(Duration::from_millis(100))),
            Some(Duration::ZERO),
        );
    }

    #[test]
    fn test_render_delay_short_outside_sync_block() {
        // 2ms elapsed, 5ms delay → 3ms remaining
        let result =
            compute_render_delay(true, false, false, false, Some(Duration::from_millis(2)));
        assert!(result.is_some());
        let remaining = result.unwrap();
        assert!(remaining > Duration::ZERO);
        assert!(remaining <= Duration::from_millis(RENDER_DELAY_MS));
    }

    #[test]
    fn test_render_delay_longer_in_sync_block() {
        // 10ms elapsed, 50ms sync delay → 40ms remaining
        let result =
            compute_render_delay(true, false, false, true, Some(Duration::from_millis(10)));
        assert!(result.is_some());
        let remaining = result.unwrap();
        assert!(remaining > Duration::from_millis(30));
        assert!(remaining <= Duration::from_millis(SYNC_BLOCK_DELAY_MS));
    }

    #[test]
    fn test_render_delay_immediate_when_sync_delay_exceeded() {
        assert_eq!(
            compute_render_delay(true, false, false, true, Some(Duration::from_millis(100))),
            Some(Duration::ZERO),
        );
    }

    // ================================================================
    // apply_segment_to_history tests (via LineBuffer directly)
    // ================================================================

    #[test]
    fn test_apply_passthrough_to_history() {
        let mut history = LineBuffer::new(1000);
        let mut history_filter = HistoryFilter::new();

        let data = b"hello world\n";
        let filtered = history_filter.filter(data);
        history.push_bytes(filtered.as_ref());

        let mut output = Vec::new();
        history.append_all(&mut output);
        assert!(!output.is_empty());
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("hello world"));
    }

    #[test]
    fn test_history_cleared_on_full_redraw() {
        let mut history = LineBuffer::new(1000);
        history.push_bytes(b"old content\n");

        // Simulate full redraw: clear history and re-seed
        history.clear();
        history.push_bytes(CLEAR_SCREEN);
        history.push_bytes(CURSOR_HOME);
        history.push_bytes(b"new content\n");

        let mut output = Vec::new();
        history.append_all(&mut output);
        let text = String::from_utf8_lossy(&output);
        assert!(!text.contains("old content"));
        assert!(text.contains("new content"));
    }

    #[test]
    fn test_sync_block_full_redraw_detection() {
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();

        let mut input = Vec::new();
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(CLEAR_SCREEN);
        input.extend_from_slice(CURSOR_HOME);
        input.extend_from_slice(b"screen content");
        input.extend_from_slice(SYNC_END);

        parser.parse(&input, &mut segments);
        assert_eq!(segments.len(), 1);

        // Simulate apply_segment_to_history behavior
        match &segments[0] {
            OutputSegment::SyncBlock { is_full_redraw, .. } => {
                assert!(is_full_redraw, "should detect full redraw");
            }
            _ => panic!("expected SyncBlock"),
        }
    }

    // ================================================================
    // Additional edge-case tests
    // ================================================================

    #[test]
    fn test_render_delay_no_output_yet() {
        // No output has happened (elapsed = None → Duration::MAX → immediate)
        assert_eq!(
            compute_render_delay(true, false, false, false, None),
            Some(Duration::ZERO),
        );
    }

    #[test]
    fn test_render_delay_exact_boundary_normal() {
        // Exactly at the 5ms boundary
        assert_eq!(
            compute_render_delay(
                true,
                false,
                false,
                false,
                Some(Duration::from_millis(RENDER_DELAY_MS))
            ),
            Some(Duration::ZERO),
        );
    }

    #[test]
    fn test_render_delay_exact_boundary_sync() {
        // Exactly at the 50ms sync block boundary
        assert_eq!(
            compute_render_delay(
                true,
                false,
                false,
                true,
                Some(Duration::from_millis(SYNC_BLOCK_DELAY_MS))
            ),
            Some(Duration::ZERO),
        );
    }

    #[test]
    fn test_render_delay_just_under_boundary() {
        // 1ms under the 5ms boundary → should return 1ms
        let result = compute_render_delay(
            true,
            false,
            false,
            false,
            Some(Duration::from_millis(RENDER_DELAY_MS - 1)),
        );
        assert_eq!(result, Some(Duration::from_millis(1)));
    }

    #[test]
    fn test_auto_lookback_exact_timeout_boundary() {
        // stdin_idle == timeout exactly → should NOT trigger (< check is strict)
        let now = Instant::now();
        let timeout = Duration::from_secs(15);
        assert!(!should_auto_lookback(
            timeout,
            false,
            false,
            Some(Duration::from_secs(14)), // just under threshold
            Some(now - Duration::from_secs(10)),
            None,
            now,
        ));
    }

    #[test]
    fn test_auto_lookback_just_over_timeout() {
        let now = Instant::now();
        let timeout = Duration::from_secs(15);
        assert!(should_auto_lookback(
            timeout,
            false,
            false,
            Some(Duration::from_secs(16)), // just over threshold
            Some(now - Duration::from_secs(10)),
            None,
            now,
        ));
    }

    #[test]
    fn test_sync_block_non_redraw_preserves_history() {
        // A sync block without clear screen should NOT clear history
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();

        let mut input = Vec::new();
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(b"just some content");
        input.extend_from_slice(SYNC_END);

        parser.parse(&input, &mut segments);
        assert_eq!(segments.len(), 1);
        match &segments[0] {
            OutputSegment::SyncBlock { is_full_redraw, .. } => {
                assert!(
                    !is_full_redraw,
                    "non-redraw sync block should not clear history"
                );
            }
            _ => panic!("expected SyncBlock"),
        }
    }

    #[test]
    fn test_history_filter_strips_mode_sequences() {
        let mut filter = HistoryFilter::new();
        // Focus tracking, mouse mode, bracketed paste should all be stripped
        let input = b"\x1b[?1004h\x1b[?1000hvisible text\x1b[?2004h";
        let output = filter.filter(input);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("visible text"));
        assert!(!text.contains("1004"));
        assert!(!text.contains("1000"));
        assert!(!text.contains("2004"));
    }

    // ================================================================
    // Simulated output flow: sync parser + history integration
    // ================================================================

    /// Helper: simulate apply_segment_to_history logic from Proxy
    fn apply_segment(
        history: &mut LineBuffer,
        filter: &mut HistoryFilter,
        segment: OutputSegment<'_>,
    ) {
        match segment {
            OutputSegment::PassThrough(data) => {
                let filtered = filter.filter(data);
                history.push_bytes(filtered.as_ref());
            }
            OutputSegment::SyncBlock {
                data,
                is_full_redraw,
            } => {
                if is_full_redraw {
                    history.clear();
                    history.push_bytes(CLEAR_SCREEN);
                    history.push_bytes(CURSOR_HOME);
                }
                let filtered = filter.filter(&data);
                history.push_bytes(filtered.as_ref());
            }
        }
    }

    fn history_text(history: &LineBuffer) -> String {
        let mut output = Vec::new();
        history.append_all(&mut output);
        String::from_utf8_lossy(&output).into_owned()
    }

    #[test]
    fn test_realistic_output_flow() {
        let mut parser = SyncBlockParser::new();
        let mut history = LineBuffer::new(10000);
        let mut filter = HistoryFilter::new();

        // Seed history like Proxy::spawn does
        history.push_bytes(CLEAR_SCREEN);
        history.push_bytes(CURSOR_HOME);

        // Step 1: passthrough text (initial shell output)
        let mut segments = Vec::new();
        parser.parse(b"$ claude\r\nStarting...\r\n", &mut segments);
        for seg in segments.drain(..) {
            apply_segment(&mut history, &mut filter, seg);
        }
        let text = history_text(&history);
        assert!(text.contains("Starting..."), "passthrough text in history");

        // Step 2: partial sync block (update without full redraw)
        let mut partial_sync = Vec::new();
        partial_sync.extend_from_slice(SYNC_START);
        partial_sync.extend_from_slice(b"\x1b[10;1HThinking...");
        partial_sync.extend_from_slice(SYNC_END);
        parser.parse(&partial_sync, &mut segments);
        for seg in segments.drain(..) {
            apply_segment(&mut history, &mut filter, seg);
        }
        let text = history_text(&history);
        assert!(
            text.contains("Starting..."),
            "old text preserved after partial sync"
        );
        assert!(
            text.contains("Thinking..."),
            "new text added after partial sync"
        );

        // Step 3: full redraw sync block (clears history)
        let mut full_redraw = Vec::new();
        full_redraw.extend_from_slice(SYNC_START);
        full_redraw.extend_from_slice(CLEAR_SCREEN);
        full_redraw.extend_from_slice(CURSOR_HOME);
        full_redraw.extend_from_slice(b"Fresh screen content\r\n");
        full_redraw.extend_from_slice(SYNC_END);
        parser.parse(&full_redraw, &mut segments);
        for seg in segments.drain(..) {
            apply_segment(&mut history, &mut filter, seg);
        }
        let text = history_text(&history);
        assert!(
            !text.contains("Starting..."),
            "old text cleared after full redraw"
        );
        assert!(
            !text.contains("Thinking..."),
            "partial sync text cleared after full redraw"
        );
        assert!(
            text.contains("Fresh screen content"),
            "new content present after full redraw"
        );
    }

    #[test]
    fn test_split_sync_block_across_chunks_with_history() {
        let mut parser = SyncBlockParser::new();
        let mut history = LineBuffer::new(10000);
        let mut filter = HistoryFilter::new();

        // Chunk 1: start of sync block
        let mut chunk1 = Vec::new();
        chunk1.extend_from_slice(b"preamble\r\n");
        chunk1.extend_from_slice(SYNC_START);
        chunk1.extend_from_slice(b"partial con");

        let mut segments = Vec::new();
        parser.parse(&chunk1, &mut segments);
        for seg in segments.drain(..) {
            apply_segment(&mut history, &mut filter, seg);
        }
        let text = history_text(&history);
        assert!(
            text.contains("preamble"),
            "passthrough before sync should be in history"
        );
        assert!(parser.in_sync_block(), "should be mid-sync-block");

        // Chunk 2: end of sync block
        let mut chunk2 = Vec::new();
        chunk2.extend_from_slice(b"tent here");
        chunk2.extend_from_slice(SYNC_END);
        chunk2.extend_from_slice(b"\r\nafter sync\r\n");

        parser.parse(&chunk2, &mut segments);
        for seg in segments.drain(..) {
            apply_segment(&mut history, &mut filter, seg);
        }
        let text = history_text(&history);
        assert!(
            text.contains("after sync"),
            "text after sync block should be in history"
        );
        assert!(!parser.in_sync_block(), "should be outside sync block");
    }

    #[test]
    fn test_alt_screen_enter_during_sync_block() {
        // Simulates the scenario from proxy.rs:375-398 where alt screen
        // enter occurs in the middle of a sync block.
        let mut parser = SyncBlockParser::new();
        let mut history = LineBuffer::new(10000);
        let mut filter = HistoryFilter::new();
        let alt_tracker = AltScreenTracker::new();

        // Seed history
        history.push_bytes(CLEAR_SCREEN);
        history.push_bytes(CURSOR_HOME);

        // Preamble text
        let mut segments = Vec::new();
        parser.parse(b"initial output\r\n", &mut segments);
        for seg in segments.drain(..) {
            apply_segment(&mut history, &mut filter, seg);
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
            apply_segment(&mut history, &mut filter, seg);
        }

        // Flush the open sync block with remaining data (simulating what proxy does)
        assert!(
            parser.in_sync_block(),
            "should be in sync block when alt screen hits"
        );
        let remaining = &chunk[alt_pos..];
        let segment = parser.append_and_flush(remaining);
        apply_segment(&mut history, &mut filter, segment);

        assert!(
            !parser.in_sync_block(),
            "sync block should be flushed after alt screen"
        );

        let text = history_text(&history);
        assert!(
            text.contains("initial output"),
            "preamble should be in history"
        );
        // The sync block content (before alt) should have been flushed to history
        assert!(
            text.contains("sync content before alt"),
            "pre-alt sync content should be in history"
        );
    }
}
