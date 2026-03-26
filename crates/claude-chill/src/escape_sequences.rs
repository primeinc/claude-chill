//! Terminal escape sequence constants and buffer size defaults.

/// Synchronized output start (DEC private mode 2026 set).
pub const SYNC_START: &[u8] = b"\x1b[?2026h";
/// Synchronized output end (DEC private mode 2026 reset).
pub const SYNC_END: &[u8] = b"\x1b[?2026l";
/// Erase entire display (ED 2).
pub const CLEAR_SCREEN: &[u8] = b"\x1b[2J";
/// Erase scrollback buffer (ED 3).
pub const CLEAR_SCROLLBACK: &[u8] = b"\x1b[3J";
/// Move cursor to row 1, column 1.
pub const CURSOR_HOME: &[u8] = b"\x1b[H";

/// Enter alternate screen buffer (DEC private mode 1049).
pub const ALT_SCREEN_ENTER: &[u8] = b"\x1b[?1049h";
/// Exit alternate screen buffer (DEC private mode 1049).
pub const ALT_SCREEN_EXIT: &[u8] = b"\x1b[?1049l";
/// Enter alternate screen buffer (legacy DEC private mode 47).
pub const ALT_SCREEN_ENTER_LEGACY: &[u8] = b"\x1b[?47h";
/// Exit alternate screen buffer (legacy DEC private mode 47).
pub const ALT_SCREEN_EXIT_LEGACY: &[u8] = b"\x1b[?47l";

/// Maximum bytes buffered while inside a sync block (1 MiB).
pub const SYNC_BUFFER_CAPACITY: usize = 1024 * 1024;
/// Pre-allocated capacity for the reusable output buffer.
pub const OUTPUT_BUFFER_CAPACITY: usize = 32768;
/// Pre-allocated capacity for the lookback input buffer.
pub const INPUT_BUFFER_CAPACITY: usize = 64;
