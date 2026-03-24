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
}
