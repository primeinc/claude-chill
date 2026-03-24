use crate::escape_sequences::{
    ALT_SCREEN_ENTER, ALT_SCREEN_ENTER_LEGACY, ALT_SCREEN_EXIT, ALT_SCREEN_EXIT_LEGACY,
};
use memchr::memmem;

/// Tracks alternate screen state and detects alt screen enter/exit sequences.
/// Platform-independent.
pub struct AltScreenTracker {
    in_alternate_screen: bool,
    enter_finder: memmem::Finder<'static>,
    exit_finder: memmem::Finder<'static>,
    enter_legacy_finder: memmem::Finder<'static>,
    exit_legacy_finder: memmem::Finder<'static>,
}

impl AltScreenTracker {
    pub fn new() -> Self {
        Self {
            in_alternate_screen: false,
            enter_finder: memmem::Finder::new(ALT_SCREEN_ENTER),
            exit_finder: memmem::Finder::new(ALT_SCREEN_EXIT),
            enter_legacy_finder: memmem::Finder::new(ALT_SCREEN_ENTER_LEGACY),
            exit_legacy_finder: memmem::Finder::new(ALT_SCREEN_EXIT_LEGACY),
        }
    }

    pub fn in_alternate_screen(&self) -> bool {
        self.in_alternate_screen
    }

    pub fn set_alternate_screen(&mut self, active: bool) {
        self.in_alternate_screen = active;
    }

    /// Find the position of an alt screen enter sequence in the data.
    /// Returns the earliest match position (modern or legacy).
    pub fn find_enter(&self, data: &[u8]) -> Option<usize> {
        let pos1 = self.enter_finder.find(data);
        let pos2 = self.enter_legacy_finder.find(data);
        match (pos1, pos2) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    /// Find the position of an alt screen exit sequence in the data.
    /// Returns the earliest match position (modern or legacy).
    pub fn find_exit(&self, data: &[u8]) -> Option<usize> {
        let pos1 = self.exit_finder.find(data);
        let pos2 = self.exit_legacy_finder.find(data);
        match (pos1, pos2) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    /// Get the byte length of the enter sequence at the given position.
    pub fn enter_len(&self, data: &[u8]) -> usize {
        if data.starts_with(ALT_SCREEN_ENTER) {
            ALT_SCREEN_ENTER.len()
        } else {
            ALT_SCREEN_ENTER_LEGACY.len()
        }
    }

    /// Get the byte length of the exit sequence at the given position.
    pub fn exit_len(&self, data: &[u8]) -> usize {
        if data.starts_with(ALT_SCREEN_EXIT) {
            ALT_SCREEN_EXIT.len()
        } else {
            ALT_SCREEN_EXIT_LEGACY.len()
        }
    }
}

impl Default for AltScreenTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initially_not_in_alt_screen() {
        let tracker = AltScreenTracker::new();
        assert!(!tracker.in_alternate_screen());
    }

    #[test]
    fn test_find_enter_modern() {
        let tracker = AltScreenTracker::new();
        let mut data = Vec::new();
        data.extend_from_slice(b"before");
        data.extend_from_slice(ALT_SCREEN_ENTER);
        data.extend_from_slice(b"after");
        assert_eq!(tracker.find_enter(&data), Some(6));
    }

    #[test]
    fn test_find_enter_legacy() {
        let tracker = AltScreenTracker::new();
        let mut data = Vec::new();
        data.extend_from_slice(b"before");
        data.extend_from_slice(ALT_SCREEN_ENTER_LEGACY);
        data.extend_from_slice(b"after");
        assert_eq!(tracker.find_enter(&data), Some(6));
    }

    #[test]
    fn test_find_exit_modern() {
        let tracker = AltScreenTracker::new();
        let mut data = Vec::new();
        data.extend_from_slice(ALT_SCREEN_EXIT);
        assert_eq!(tracker.find_exit(&data), Some(0));
    }

    #[test]
    fn test_find_none() {
        let tracker = AltScreenTracker::new();
        assert_eq!(tracker.find_enter(b"no escape here"), None);
        assert_eq!(tracker.find_exit(b"no escape here"), None);
    }

    #[test]
    fn test_enter_len_modern() {
        let tracker = AltScreenTracker::new();
        let mut data = Vec::new();
        data.extend_from_slice(ALT_SCREEN_ENTER);
        assert_eq!(tracker.enter_len(&data), ALT_SCREEN_ENTER.len());
    }

    #[test]
    fn test_enter_len_legacy() {
        let tracker = AltScreenTracker::new();
        let mut data = Vec::new();
        data.extend_from_slice(ALT_SCREEN_ENTER_LEGACY);
        assert_eq!(tracker.enter_len(&data), ALT_SCREEN_ENTER_LEGACY.len());
    }

    #[test]
    fn test_set_alternate_screen() {
        let mut tracker = AltScreenTracker::new();
        tracker.set_alternate_screen(true);
        assert!(tracker.in_alternate_screen());
        tracker.set_alternate_screen(false);
        assert!(!tracker.in_alternate_screen());
    }

    #[test]
    fn test_find_enter_both_modern_and_legacy_picks_earliest() {
        let tracker = AltScreenTracker::new();
        // Legacy comes first
        let mut data = Vec::new();
        data.extend_from_slice(ALT_SCREEN_ENTER_LEGACY);
        data.extend_from_slice(b"gap");
        data.extend_from_slice(ALT_SCREEN_ENTER);
        assert_eq!(tracker.find_enter(&data), Some(0));

        // Modern comes first
        let mut data2 = Vec::new();
        data2.extend_from_slice(ALT_SCREEN_ENTER);
        data2.extend_from_slice(b"gap");
        data2.extend_from_slice(ALT_SCREEN_ENTER_LEGACY);
        assert_eq!(tracker.find_enter(&data2), Some(0));
    }

    #[test]
    fn test_find_exit_both_modern_and_legacy_picks_earliest() {
        let tracker = AltScreenTracker::new();
        let mut data = Vec::new();
        data.extend_from_slice(b"prefix");
        data.extend_from_slice(ALT_SCREEN_EXIT_LEGACY);
        data.extend_from_slice(b"gap");
        data.extend_from_slice(ALT_SCREEN_EXIT);
        assert_eq!(tracker.find_exit(&data), Some(6));
    }

    #[test]
    fn test_find_on_empty_data() {
        let tracker = AltScreenTracker::new();
        assert_eq!(tracker.find_enter(b""), None);
        assert_eq!(tracker.find_exit(b""), None);
    }

    #[test]
    fn test_exit_len_modern() {
        let tracker = AltScreenTracker::new();
        assert_eq!(tracker.exit_len(ALT_SCREEN_EXIT), ALT_SCREEN_EXIT.len());
    }

    #[test]
    fn test_exit_len_legacy() {
        let tracker = AltScreenTracker::new();
        assert_eq!(
            tracker.exit_len(ALT_SCREEN_EXIT_LEGACY),
            ALT_SCREEN_EXIT_LEGACY.len()
        );
    }
}
