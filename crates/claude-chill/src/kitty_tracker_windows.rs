//! Kitty keyboard protocol state tracking (Windows stub).
//!
//! Windows terminals generally do not support the Kitty keyboard protocol.
//! This module provides the same `KittyTracker` struct and `detect()` function
//! as the Unix version, but detection always returns unsupported.

use log::debug;
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

    /// Current push/pop stack depth (0 = protocol inactive).
    pub fn stack_depth(&self) -> u32 {
        self.stack
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
/// On Windows, always returns true (skip detection).
#[must_use]
pub fn should_skip_kitty_query(_term_program: Option<&str>, _term: Option<&str>) -> bool {
    true
}

/// Detect Kitty keyboard protocol support.
/// On Windows, always returns unsupported.
#[must_use]
pub fn detect() -> KittyTracker {
    debug!("Kitty detection skipped: Windows platform");
    KittyTracker::new(false, 0)
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
    fn test_detect_returns_unsupported() {
        let tracker = detect();
        assert!(!tracker.supported());
        assert!(!tracker.mode_enabled());
    }

    #[test]
    fn test_skip_kitty_always_true() {
        assert!(should_skip_kitty_query(None, None));
        assert!(should_skip_kitty_query(
            Some("WindowsTerminal"),
            Some("xterm-256color")
        ));
    }

    #[test]
    fn test_kitty_push_ignored_when_unsupported() {
        let mut tracker = KittyTracker::new(false, 0);
        tracker.process(b"\x1b[>1u");
        assert_eq!(tracker.stack_depth(), 0);
    }

    #[test]
    fn test_kitty_push_works_when_supported() {
        let mut tracker = KittyTracker::new(true, 0);
        tracker.process(b"\x1b[>1u");
        assert_eq!(tracker.stack_depth(), 1);
        assert!(tracker.mode_enabled());
    }
}
