use crate::alt_screen::AltScreenTracker;
use crate::escape_sequences::{
    CLEAR_SCREEN, CURSOR_HOME, INPUT_BUFFER_CAPACITY, OUTPUT_BUFFER_CAPACITY, SYNC_END, SYNC_START,
};
use crate::history_filter::HistoryFilter;
use crate::kitty_tracker::KittyTracker;
use crate::line_buffer::LineBuffer;
use crate::sync_block::{OutputSegment, SyncBlockParser};
use anyhow::{Context, Result};
use log::debug;
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, kill, sigaction};
use nix::sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr};
use nix::unistd::{Pid, isatty, read, write};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use termwiz::escape::Action;
use termwiz::escape::csi::{CSI, Keyboard};
use termwiz::escape::parser::Parser as TermwizParser;

static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);
static SIGINT_RECEIVED: AtomicBool = AtomicBool::new(false);
static SIGTERM_RECEIVED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SequenceMatch {
    Complete,
    Partial,
    None,
}

extern "C" fn handle_sigwinch(_: libc::c_int) {
    SIGWINCH_RECEIVED.store(true, Ordering::SeqCst);
}

extern "C" fn handle_sigint(_: libc::c_int) {
    SIGINT_RECEIVED.store(true, Ordering::SeqCst);
}

extern "C" fn handle_sigterm(_: libc::c_int) {
    SIGTERM_RECEIVED.store(true, Ordering::SeqCst);
}

pub struct ProxyConfig {
    pub max_history_lines: usize,
    pub lookback_key: String,
    pub lookback_sequence_legacy: Vec<u8>,
    pub lookback_sequence_kitty: Vec<u8>,
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

pub struct Proxy {
    config: ProxyConfig,
    pty_master: OwnedFd,
    child: Child,
    original_termios: Option<Termios>,
    history: LineBuffer,
    history_filter: HistoryFilter,
    vt_parser: vt100::Parser,
    vt_prev_screen: Option<vt100::Screen>,
    last_output_time: Option<Instant>,
    last_render_time: Option<Instant>,
    last_stdin_time: Option<Instant>,
    last_auto_lookback_time: Option<Instant>,
    auto_lookback_timeout: Duration,
    sync_parser: SyncBlockParser,
    in_lookback_mode: bool,
    alt_screen: AltScreenTracker,
    kitty_tracker: KittyTracker,
    vt_render_pending: bool,
    lookback_cache: Vec<u8>,
    lookback_input_buffer: Vec<u8>,
    output_buffer: Vec<u8>,
}

/// Returns (supported, initial_flags) - if flags > 0, terminal is already in Kitty mode
fn detect_kitty_support() -> (bool, u32) {
    use std::io::Write;
    use termwiz::escape::csi::Device;

    // Query sequences:
    // CSI ? u       - Kitty keyboard protocol query
    // CSI c         - Primary Device Attributes (all terminals respond)
    const KITTY_QUERY: &[u8] = b"\x1b[?u";
    const DA_QUERY: &[u8] = b"\x1b[c";

    // Send both queries
    let stdout = std::io::stdout();
    let mut stdout_lock = stdout.lock();
    if stdout_lock.write_all(KITTY_QUERY).is_err() {
        return (false, 0);
    }
    if stdout_lock.write_all(DA_QUERY).is_err() {
        return (false, 0);
    }
    if stdout_lock.flush().is_err() {
        return (false, 0);
    }
    drop(stdout_lock);

    // Read responses with timeout using termwiz parser
    let stdin = std::io::stdin();
    let mut parser = TermwizParser::new();
    let mut buf = [0u8; 256];
    let mut kitty_supported = false;
    let mut kitty_flags: u32 = 0;
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_millis(500);
    let poll_interval = PollTimeout::from(50u16);

    while start.elapsed() < timeout {
        let mut poll_fd = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];

        match poll(&mut poll_fd, poll_interval) {
            Ok(0) => continue,
            Ok(_) => {
                match read(stdin.as_fd(), &mut buf) {
                    Ok(0) => continue,
                    Ok(n) => {
                        let actions = parser.parse_as_vec(&buf[..n]);
                        for action in actions {
                            if let Action::CSI(csi) = action {
                                match csi {
                                    CSI::Keyboard(Keyboard::ReportKittyState(flags)) => {
                                        kitty_supported = true;
                                        kitty_flags = u32::from(flags.bits());
                                    }
                                    CSI::Device(dev)
                                        if matches!(*dev, Device::DeviceAttributes(_)) =>
                                    {
                                        // DA response means all responses received
                                        debug!(
                                            "Kitty detection complete: supported={kitty_supported} flags={kitty_flags}"
                                        );
                                        return (kitty_supported, kitty_flags);
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    Err(e) if e == Errno::EAGAIN || e == Errno::EWOULDBLOCK => continue,
                    Err(_) => break,
                }
            }
            Err(_) => continue,
        }
    }

    debug!("Kitty detection timed out, supported={kitty_supported} flags={kitty_flags}");
    (kitty_supported, kitty_flags)
}

impl Proxy {
    pub fn spawn(command: &str, args: &[&str], config: ProxyConfig) -> Result<Self> {
        let winsize = get_terminal_size()?;
        let pty = openpty(&winsize, None).context("openpty failed")?;

        let original_termios = setup_raw_mode()?;
        setup_signal_handlers()?;

        // Detect Kitty support before spawning child
        // If flags > 0, terminal is already in Kitty mode (inherited from parent)
        let (kitty_supported, kitty_initial_flags) = detect_kitty_support();
        let kitty_initial_stack = if kitty_initial_flags > 0 { 1 } else { 0 };

        let slave_fd = pty.slave.as_raw_fd();

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
            kitty_tracker: KittyTracker::new(kitty_supported, kitty_initial_stack),
            vt_render_pending: false,
            lookback_cache: Vec::new(),
            lookback_input_buffer: Vec::with_capacity(INPUT_BUFFER_CAPACITY),
            output_buffer: Vec::with_capacity(OUTPUT_BUFFER_CAPACITY),
        })
    }

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

        // Check for alt screen enter before sync parsing
        if let Some(alt_pos) = self.alt_screen.find_enter(data) {
            debug!("process_output: ALT_SCREEN_ENTER detected at pos={alt_pos}");
            // Add ALL remaining data to history (including alt screen enter and content)
            // This ensures history matches VT exactly
            if self.sync_parser.in_sync_block() {
                let segment = self.sync_parser.append_and_flush(data);
                self.apply_segment_to_history(segment);
            } else {
                self.push_to_history(data);
            }
            self.alt_screen.set_alternate_screen(true);
            let seq_len = self.alt_screen.enter_len(&data[alt_pos..]);
            // Write alt screen enter directly
            self.write_to_terminal(stdout_fd, &data[alt_pos..alt_pos + seq_len])?;
            return self.process_output_alt_screen(&data[alt_pos + seq_len..], stdout_fd);
        }

        // Process sync blocks for history management
        let mut segments = Vec::new();
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
        self.history.push_bytes(&filtered);
    }

    fn flush_pending_vt_render<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        if !self.vt_render_pending || self.in_lookback_mode || self.alt_screen.in_alternate_screen()
        {
            return Ok(());
        }

        let elapsed = self
            .last_output_time
            .map(|t| t.elapsed())
            .unwrap_or(Duration::MAX);

        // Wait longer if in sync block (more data likely coming)
        let delay = if self.sync_parser.in_sync_block() {
            Duration::from_millis(SYNC_BLOCK_DELAY_MS)
        } else {
            Duration::from_millis(RENDER_DELAY_MS)
        };

        if elapsed >= delay {
            self.render_vt_screen(stdout_fd)?;
        }

        Ok(())
    }

    fn time_until_render(&self) -> Option<Duration> {
        if !self.vt_render_pending || self.in_lookback_mode || self.alt_screen.in_alternate_screen()
        {
            return None;
        }

        let elapsed = self
            .last_output_time
            .map(|t| t.elapsed())
            .unwrap_or(Duration::MAX);

        let delay = if self.sync_parser.in_sync_block() {
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
        self.kitty_tracker.process(&self.output_buffer);
        write_all(stdout_fd, &self.output_buffer)?;

        // Store current screen for next diff
        self.vt_prev_screen = Some(self.vt_parser.screen().clone());
        self.vt_render_pending = false;
        self.last_render_time = Some(Instant::now());
        Ok(())
    }

    fn check_auto_lookback<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
        if self.auto_lookback_timeout.is_zero() {
            return Ok(());
        }
        if self.in_lookback_mode || self.alt_screen.in_alternate_screen() {
            return Ok(());
        }

        // Check if enough time has passed since last stdin activity
        let Some(stdin_time) = self.last_stdin_time else {
            return Ok(());
        };
        if stdin_time.elapsed() < self.auto_lookback_timeout {
            return Ok(());
        }

        // Check if there's been new output since last auto-lookback
        // AND enough time has passed since we last dumped
        let Some(render_time) = self.last_render_time else {
            return Ok(());
        };
        if let Some(last_auto) = self.last_auto_lookback_time {
            let no_new_output = render_time <= last_auto;
            let too_soon = last_auto.elapsed() < self.auto_lookback_timeout;
            if no_new_output || too_soon {
                return Ok(());
            }
        }

        debug!(
            "auto_lookback triggered: stdin_idle={}ms render_age={}ms last_auto_age={}ms",
            stdin_time.elapsed().as_millis(),
            render_time.elapsed().as_millis(),
            self.last_auto_lookback_time
                .map(|t| t.elapsed().as_millis())
                .unwrap_or(0)
        );
        self.dump_history(stdout_fd)?;
        self.last_auto_lookback_time = Some(Instant::now());
        Ok(())
    }

    fn dump_history<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
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
        self.kitty_tracker.process(&self.output_buffer);
        write_all(stdout_fd, &self.output_buffer)?;

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

        // Copy to local to avoid holding an immutable borrow on self.config across the loop
        let lookback_sequence: Vec<u8> = if self.kitty_tracker.mode_enabled() {
            self.config.lookback_sequence_kitty.clone()
        } else {
            self.config.lookback_sequence_legacy.clone()
        };

        for &byte in data {
            if self.in_lookback_mode && byte == 0x03 {
                self.lookback_input_buffer.clear();
                self.exit_lookback_mode(stdout_fd)?;
                continue;
            }

            let lookback_action =
                check_sequence_match(&self.lookback_input_buffer, byte, &lookback_sequence);

            self.lookback_input_buffer.push(byte);
            if self.lookback_input_buffer.len() > lookback_sequence.len() {
                let excess = self.lookback_input_buffer.len() - lookback_sequence.len();
                self.lookback_input_buffer.drain(..excess);
            }

            match lookback_action {
                SequenceMatch::Complete => {
                    self.lookback_input_buffer.clear();
                    if self.in_lookback_mode {
                        self.exit_lookback_mode(stdout_fd)?;
                    } else {
                        self.enter_lookback_mode()?;
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

    fn enter_lookback_mode(&mut self) -> Result<()> {
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

        let stdout_fd = io::stdout();
        write_all(&stdout_fd, CLEAR_SCREEN)?;
        write_all(&stdout_fd, CURSOR_HOME)?;
        write_all(&stdout_fd, &self.output_buffer)?;

        let exit_msg = format!(
            "\r\n\x1b[7m--- LOOKBACK MODE: press {} or Ctrl+C to exit ---\x1b[0m\r\n",
            self.config.lookback_key
        );
        write_all(&stdout_fd, exit_msg.as_bytes())?;

        Ok(())
    }

    fn exit_lookback_mode<F: AsFd>(&mut self, stdout_fd: &F) -> Result<()> {
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
            // Forward to child process
            unsafe {
                libc::ioctl(
                    self.pty_master.as_raw_fd(),
                    libc::TIOCSWINSZ as libc::c_ulong,
                    &winsize,
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
            let _ = tcsetattr(io::stdin(), SetArg::TCSANOW, termios);
        }
    }
}

fn get_terminal_size() -> Result<Winsize> {
    let mut ws: Winsize = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        libc::ioctl(
            io::stdout().as_raw_fd(),
            libc::TIOCGWINSZ as libc::c_ulong,
            &mut ws,
        )
    };
    if ret == -1 || ws.ws_row == 0 || ws.ws_col == 0 {
        ws.ws_row = 24;
        ws.ws_col = 80;
    }
    Ok(ws)
}

fn exit_code_from_status(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(signal) = status.signal() {
        128 + signal
    } else {
        1
    }
}

/// Check if appending `byte` to `buffer` would match `sequence`.
/// Does not mutate `buffer` — the caller is responsible for updating it.
fn check_sequence_match(buffer: &[u8], byte: u8, sequence: &[u8]) -> SequenceMatch {
    // Build a temporary view: the tail of buffer + new byte, windowed to sequence length
    let buf_start = if buffer.len() + 1 > sequence.len() {
        buffer.len() + 1 - sequence.len()
    } else {
        0
    };
    let prefix = &buffer[buf_start..];

    // Check if prefix + byte matches the full sequence
    if prefix.len() + 1 == sequence.len()
        && sequence[..prefix.len()] == *prefix
        && sequence[prefix.len()] == byte
    {
        SequenceMatch::Complete
    } else {
        // Check partial: does prefix + byte form a prefix of the sequence?
        let candidate_len = prefix.len() + 1;
        if candidate_len <= sequence.len()
            && sequence[..prefix.len()] == *prefix
            && sequence[prefix.len()] == byte
        {
            SequenceMatch::Partial
        } else {
            SequenceMatch::None
        }
    }
}

fn setup_raw_mode() -> Result<Option<Termios>> {
    let stdin = io::stdin();
    if !isatty(&stdin).unwrap_or(false) {
        return Ok(None);
    }

    let original = tcgetattr(&stdin).context("tcgetattr failed")?;
    let mut raw = original.clone();
    cfmakeraw(&mut raw);
    tcsetattr(&stdin, SetArg::TCSANOW, &raw).context("tcsetattr failed")?;
    Ok(Some(original))
}

fn setup_signal_handler(signal: Signal, handler: extern "C" fn(libc::c_int)) -> Result<()> {
    let action = SigAction::new(
        SigHandler::Handler(handler),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    unsafe { sigaction(signal, &action) }.context(format!("sigaction {signal:?} failed"))?;
    Ok(())
}

fn setup_signal_handlers() -> Result<()> {
    setup_signal_handler(Signal::SIGWINCH, handle_sigwinch)?;
    setup_signal_handler(Signal::SIGINT, handle_sigint)?;
    setup_signal_handler(Signal::SIGTERM, handle_sigterm)?;
    Ok(())
}

fn set_nonblocking<Fd: AsFd>(fd: &Fd) -> Result<()> {
    let flags = fcntl(fd.as_fd(), FcntlArg::F_GETFL).context("fcntl F_GETFL failed")?;
    let flags = OFlag::from_bits_truncate(flags);
    fcntl(fd.as_fd(), FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
        .context("fcntl F_SETFL failed")?;
    Ok(())
}

fn write_all<F: AsFd>(fd: &F, data: &[u8]) -> Result<()> {
    let mut written = 0;
    while written < data.len() {
        match write(fd, &data[written..]) {
            Ok(n) => written += n,
            Err(Errno::EAGAIN) | Err(Errno::EINTR) => continue,
            Err(e) => anyhow::bail!("write failed: {e}"),
        }
    }
    Ok(())
}

fn nix_read<F: AsFd>(fd: &F, buf: &mut [u8]) -> Result<usize, Errno> {
    read(fd.as_fd(), buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sequence_match_complete_single_byte() {
        let sequence = &[0x1E];
        assert_eq!(
            check_sequence_match(&[], 0x1E, sequence),
            SequenceMatch::Complete
        );
    }

    #[test]
    fn test_sequence_match_complete_multi_byte() {
        let sequence = b"\x1b[54;5u";
        let mut buffer = Vec::new();
        for &byte in &sequence[..sequence.len() - 1] {
            let result = check_sequence_match(&buffer, byte, sequence);
            assert_eq!(result, SequenceMatch::Partial);
            buffer.push(byte);
            if buffer.len() > sequence.len() {
                buffer.drain(..buffer.len() - sequence.len());
            }
        }
        assert_eq!(
            check_sequence_match(&buffer, sequence[sequence.len() - 1], sequence),
            SequenceMatch::Complete
        );
    }

    #[test]
    fn test_sequence_match_partial() {
        let sequence = b"\x1b[54;5u";
        assert_eq!(
            check_sequence_match(&[], 0x1b, sequence),
            SequenceMatch::Partial
        );
        assert_eq!(
            check_sequence_match(&[0x1b], b'[', sequence),
            SequenceMatch::Partial
        );
        assert_eq!(
            check_sequence_match(&[0x1b, b'['], b'5', sequence),
            SequenceMatch::Partial
        );
    }

    #[test]
    fn test_sequence_match_none_wrong_byte() {
        let sequence = b"\x1b[54;5u";
        assert_eq!(
            check_sequence_match(&[], b'a', sequence),
            SequenceMatch::None
        );
        assert_eq!(
            check_sequence_match(&[0x1b], b'O', sequence),
            SequenceMatch::None
        );
    }

    #[test]
    fn test_sequence_match_buffer_rolling() {
        let sequence = b"\x1b[54;5u";
        assert_eq!(
            check_sequence_match(&[], b'a', sequence),
            SequenceMatch::None
        );
        assert_eq!(
            check_sequence_match(b"a", b'b', sequence),
            SequenceMatch::None
        );
        assert_eq!(
            check_sequence_match(b"ab", 0x1b, sequence),
            SequenceMatch::None
        );
        assert_eq!(
            check_sequence_match(&[], 0x1b, sequence),
            SequenceMatch::Partial
        );
    }

    #[test]
    fn test_sequence_match_interleaved_typing() {
        let sequence = &[0x1E];
        assert_eq!(
            check_sequence_match(&[], b'a', sequence),
            SequenceMatch::None
        );
        assert_eq!(
            check_sequence_match(b"a", b'b', sequence),
            SequenceMatch::None
        );
        assert_eq!(
            check_sequence_match(b"ab", 0x1E, sequence),
            SequenceMatch::Complete
        );
    }

    #[test]
    fn test_sequence_match_long_buffer_overflow() {
        // Buffer longer than sequence — should window correctly
        let sequence = &[0x1E]; // 1 byte
        // Buffer has 10 bytes of junk
        assert_eq!(
            check_sequence_match(b"0123456789", 0x1E, sequence),
            SequenceMatch::Complete
        );
    }

    #[test]
    fn test_sequence_match_multi_byte_with_noise_prefix() {
        // Buffer full of noise, then correct sequence bytes arrive
        let sequence = b"\x1b[54;5u"; // 7 bytes
        // Buffer has the first 6 bytes correctly after windowing
        let buffer = b"\x1b[54;5";
        assert_eq!(
            check_sequence_match(buffer, b'u', sequence),
            SequenceMatch::Complete
        );
    }

    #[test]
    fn test_sequence_match_almost_match() {
        // Buffer has almost the right prefix but wrong final byte
        let sequence = b"\x1b[54;5u";
        let buffer = b"\x1b[54;5";
        assert_eq!(
            check_sequence_match(buffer, b'x', sequence),
            SequenceMatch::None
        );
    }
}
