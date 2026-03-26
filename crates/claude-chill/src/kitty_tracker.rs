//! Kitty keyboard protocol state tracking and terminal detection.

use log::debug;
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::unistd::{isatty, read};
use std::os::fd::AsFd;
use termwiz::escape::Action;
use termwiz::escape::csi::{CSI, Keyboard};
use termwiz::escape::parser::Parser as TermwizParser;

/// Tracks Kitty keyboard protocol state by monitoring escape sequences
/// passing through the proxy. Platform-independent.
pub struct KittyTracker {
    parser: TermwizParser,
    supported: bool,
    stack: u32,
}

impl KittyTracker {
    /// Create a new tracker with known support status and initial stack depth.
    pub fn new(supported: bool, initial_stack: u32) -> Self {
        Self {
            parser: TermwizParser::new(),
            supported,
            stack: initial_stack,
        }
    }

    /// Whether the Kitty keyboard protocol is currently active
    pub fn mode_enabled(&self) -> bool {
        self.stack > 0
    }

    /// Whether the terminal supports the Kitty keyboard protocol
    pub fn supported(&self) -> bool {
        self.supported
    }

    /// Process output data to track Kitty push/pop/set state changes
    pub fn process(&mut self, data: &[u8]) {
        let actions = self.parser.parse_as_vec(data);
        for action in actions {
            if let Action::CSI(csi) = action {
                match csi {
                    CSI::Keyboard(Keyboard::PushKittyState { flags, .. }) => {
                        if self.supported {
                            self.stack = self.stack.saturating_add(1);
                            debug!(
                                "Kitty keyboard protocol push (flags={:?}, stack={})",
                                flags, self.stack
                            );
                        }
                    }
                    CSI::Keyboard(Keyboard::SetKittyState { flags, .. }) => {
                        if self.supported && !flags.is_empty() && self.stack == 0 {
                            self.stack = 1;
                            debug!(
                                "Kitty keyboard protocol set (flags={:?}, stack={})",
                                flags, self.stack
                            );
                        } else if flags.is_empty() && self.stack > 0 {
                            debug!(
                                "Kitty keyboard protocol set empty flags (stack={})",
                                self.stack
                            );
                        }
                    }
                    CSI::Keyboard(Keyboard::PopKittyState(n)) => {
                        let prev = self.stack;
                        self.stack = self.stack.saturating_sub(n);
                        debug!(
                            "Kitty keyboard protocol pop {} (stack {} -> {})",
                            n, prev, self.stack
                        );
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Pure decision: should we skip the Kitty keyboard protocol DA query?
/// Returns true if the terminal is known not to support Kitty protocol.
#[must_use]
pub fn should_skip_kitty_query(term_program: Option<&str>, term: Option<&str>) -> bool {
    if let Some(tp) = term_program {
        let tp_lower = tp.to_ascii_lowercase();
        if matches!(
            tp_lower.as_str(),
            "apple_terminal" | "terminal.app" | "iterm.app" | "iterm2" | "hyper" | "terminus"
        ) {
            return true;
        }
    }

    if let Some(t) = term {
        if matches!(t, "dumb" | "linux" | "vt100" | "vt220") {
            return true;
        }
    }

    false
}

/// Detect Kitty keyboard protocol support and return a configured KittyTracker.
///
/// Checks environment variables first to avoid a 500ms DA query on terminals
/// known not to support Kitty protocol. Skips the query entirely if stdin
/// is not a TTY.
#[must_use]
pub fn detect() -> KittyTracker {
    // Skip the terminal query entirely if stdin isn't a TTY (e.g. piped input)
    if !isatty(std::io::stdin()).unwrap_or(false) {
        debug!("Kitty detection skipped: stdin is not a TTY");
        return KittyTracker::new(false, 0);
    }

    let term_program = std::env::var("TERM_PROGRAM").ok();
    let term = std::env::var("TERM").ok();

    if should_skip_kitty_query(term_program.as_deref(), term.as_deref()) {
        debug!(
            "Kitty detection skipped: TERM_PROGRAM={} TERM={} (known non-Kitty)",
            term_program.as_deref().unwrap_or("(unset)"),
            term.as_deref().unwrap_or("(unset)")
        );
        return KittyTracker::new(false, 0);
    }

    let (supported, initial_flags) = query_kitty_support();
    let initial_stack = if initial_flags > 0 { 1 } else { 0 };
    KittyTracker::new(supported, initial_stack)
}

/// Query the terminal for Kitty keyboard protocol support.
/// Returns (supported, initial_flags).
fn query_kitty_support() -> (bool, u32) {
    use std::io::Write;
    use termwiz::escape::csi::Device;

    const KITTY_QUERY: &[u8] = b"\x1b[?u";
    const DA_QUERY: &[u8] = b"\x1b[c";

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
            Ok(_) => match read(stdin.as_fd(), &mut buf) {
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
                                CSI::Device(dev) if matches!(*dev, Device::DeviceAttributes(_)) => {
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
            },
            Err(_) => continue,
        }
    }

    debug!("Kitty detection timed out, supported={kitty_supported} flags={kitty_flags}");
    (kitty_supported, kitty_flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kitty_initially_disabled() {
        let tracker = KittyTracker::new(false, 0);
        assert!(!tracker.mode_enabled());
        assert!(!tracker.supported());
    }

    #[test]
    fn test_kitty_push_increments_stack() {
        let mut tracker = KittyTracker::new(true, 0);
        // CSI > 1 u = push with flags
        tracker.process(b"\x1b[>1u");
        assert_eq!(tracker.stack, 1);
        assert!(tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_push_requires_support() {
        let mut tracker = KittyTracker::new(false, 0);
        // Push without support detection - should be ignored
        tracker.process(b"\x1b[>1u");
        assert_eq!(tracker.stack, 0);
        assert!(!tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_pop_decrements_stack() {
        let mut tracker = KittyTracker::new(true, 0);
        tracker.process(b"\x1b[>1u"); // push
        tracker.process(b"\x1b[<u"); // pop 1
        assert_eq!(tracker.stack, 0);
        assert!(!tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_pop_with_count() {
        let mut tracker = KittyTracker::new(true, 0);
        tracker.process(b"\x1b[>1u"); // push
        tracker.process(b"\x1b[>1u"); // push
        tracker.process(b"\x1b[>1u"); // push
        assert_eq!(tracker.stack, 3);
        tracker.process(b"\x1b[<2u"); // pop 2
        assert_eq!(tracker.stack, 1);
        assert!(tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_pop_saturates_at_zero() {
        let mut tracker = KittyTracker::new(true, 0);
        tracker.process(b"\x1b[>1u"); // push
        tracker.process(b"\x1b[<5u"); // pop 5 (more than we have)
        assert_eq!(tracker.stack, 0);
        assert!(!tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_split_sequence_across_buffers() {
        let mut tracker = KittyTracker::new(true, 0);
        // Feed the sequence in parts
        tracker.process(b"\x1b[>");
        tracker.process(b"1u");
        assert_eq!(tracker.stack, 1);
    }

    #[test]
    fn test_kitty_multiple_sequences_in_one_buffer() {
        let mut tracker = KittyTracker::new(true, 0);
        // Push twice, pop once, all in one buffer
        tracker.process(b"\x1b[>1u\x1b[>1u\x1b[<u");
        assert_eq!(tracker.stack, 1);
    }

    #[test]
    fn test_kitty_mixed_with_other_sequences() {
        let mut tracker = KittyTracker::new(true, 0);
        // Kitty push mixed with cursor moves and SGR
        tracker.process(b"\x1b[H\x1b[>1u\x1b[31m\x1b[2J");
        assert_eq!(tracker.stack, 1);
    }

    #[test]
    fn test_kitty_typical_session_flow() {
        let mut tracker = KittyTracker::new(true, 0);
        // App pushes keyboard mode
        tracker.process(b"\x1b[>1u");
        assert!(tracker.mode_enabled());
        // App pops keyboard mode on exit
        tracker.process(b"\x1b[<u");
        assert!(!tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_set_enables_when_stack_empty() {
        let mut tracker = KittyTracker::new(true, 0);
        // CSI = 1 u = set with flags (non-empty)
        tracker.process(b"\x1b[=1u");
        assert_eq!(tracker.stack, 1);
        assert!(tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_set_noop_when_already_pushed() {
        let mut tracker = KittyTracker::new(true, 0);
        tracker.process(b"\x1b[>1u"); // push
        assert_eq!(tracker.stack, 1);
        // Set with flags when already in mode — should not change stack
        tracker.process(b"\x1b[=1u");
        assert_eq!(tracker.stack, 1);
    }

    #[test]
    fn test_kitty_initial_stack() {
        let tracker = KittyTracker::new(true, 2);
        assert!(tracker.mode_enabled());
        assert_eq!(tracker.stack, 2);
    }

    #[test]
    fn test_kitty_unsupported_ignores_everything() {
        let mut tracker = KittyTracker::new(false, 0);
        tracker.process(b"\x1b[>1u"); // push
        tracker.process(b"\x1b[=1u"); // set
        assert_eq!(tracker.stack, 0);
        assert!(!tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_pop_without_push() {
        let mut tracker = KittyTracker::new(true, 0);
        // Pop with nothing on stack — should stay at 0
        tracker.process(b"\x1b[<u");
        assert_eq!(tracker.stack, 0);
        assert!(!tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_process_plain_text() {
        let mut tracker = KittyTracker::new(true, 0);
        tracker.process(b"Hello, World! This is plain text with no escape sequences.");
        assert_eq!(tracker.stack, 0);
        assert!(!tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_process_sgr_sequences() {
        let mut tracker = KittyTracker::new(true, 0);
        // SGR (color) sequences should not affect Kitty state
        tracker.process(b"\x1b[1;31mBold Red\x1b[0m Normal");
        assert_eq!(tracker.stack, 0);
    }

    #[test]
    fn test_kitty_process_cursor_sequences() {
        let mut tracker = KittyTracker::new(true, 0);
        // Cursor movement should not affect Kitty state
        tracker.process(b"\x1b[H\x1b[2J\x1b[10;20H");
        assert_eq!(tracker.stack, 0);
    }

    #[test]
    fn test_kitty_process_empty_data() {
        let mut tracker = KittyTracker::new(true, 1);
        tracker.process(b"");
        assert_eq!(tracker.stack, 1);
        assert!(tracker.mode_enabled());
    }

    #[test]
    fn test_kitty_supported_getter() {
        let tracker = KittyTracker::new(true, 0);
        assert!(tracker.supported());
        let tracker2 = KittyTracker::new(false, 0);
        assert!(!tracker2.supported());
    }

    #[test]
    fn test_kitty_deep_stack() {
        let mut tracker = KittyTracker::new(true, 0);
        // Push 10 times
        for _ in 0..10 {
            tracker.process(b"\x1b[>1u");
        }
        assert_eq!(tracker.stack, 10);
        assert!(tracker.mode_enabled());

        // Pop all at once
        tracker.process(b"\x1b[<10u");
        assert_eq!(tracker.stack, 0);
        assert!(!tracker.mode_enabled());
    }

    // ================================================================
    // Kitty detection skip logic tests
    // ================================================================

    #[test]
    fn test_skip_kitty_apple_terminal() {
        assert!(should_skip_kitty_query(Some("Apple_Terminal"), None));
    }

    #[test]
    fn test_skip_kitty_terminal_app() {
        assert!(should_skip_kitty_query(Some("Terminal.app"), None));
    }

    #[test]
    fn test_skip_kitty_iterm2() {
        assert!(should_skip_kitty_query(Some("iTerm2"), None));
        assert!(should_skip_kitty_query(Some("iTerm.app"), None));
    }

    #[test]
    fn test_skip_kitty_hyper() {
        assert!(should_skip_kitty_query(Some("Hyper"), None));
    }

    #[test]
    fn test_skip_kitty_terminus() {
        assert!(should_skip_kitty_query(Some("Terminus"), None));
    }

    #[test]
    fn test_skip_kitty_dumb_term() {
        assert!(should_skip_kitty_query(None, Some("dumb")));
    }

    #[test]
    fn test_skip_kitty_linux_console() {
        assert!(should_skip_kitty_query(None, Some("linux")));
    }

    #[test]
    fn test_skip_kitty_vt100() {
        assert!(should_skip_kitty_query(None, Some("vt100")));
        assert!(should_skip_kitty_query(None, Some("vt220")));
    }

    #[test]
    fn test_no_skip_kitty_known_supporters() {
        assert!(!should_skip_kitty_query(Some("kitty"), None));
        assert!(!should_skip_kitty_query(Some("ghostty"), None));
        assert!(!should_skip_kitty_query(Some("WezTerm"), None));
    }

    #[test]
    fn test_no_skip_kitty_xterm256() {
        assert!(!should_skip_kitty_query(None, Some("xterm-256color")));
    }

    #[test]
    fn test_no_skip_kitty_unset() {
        assert!(!should_skip_kitty_query(None, None));
    }

    #[test]
    fn test_skip_kitty_case_insensitive_term_program() {
        assert!(should_skip_kitty_query(Some("APPLE_TERMINAL"), None));
        assert!(should_skip_kitty_query(Some("apple_terminal"), None));
        assert!(should_skip_kitty_query(Some("ITERM2"), None));
    }

    #[test]
    fn test_skip_kitty_term_program_takes_priority() {
        assert!(should_skip_kitty_query(
            Some("Apple_Terminal"),
            Some("xterm-256color")
        ));
    }

    #[test]
    fn test_no_skip_kitty_unknown_term_program() {
        assert!(should_skip_kitty_query(Some("SomeUnknown"), Some("dumb")));
        assert!(!should_skip_kitty_query(
            Some("SomeUnknown"),
            Some("xterm-256color")
        ));
    }
}
