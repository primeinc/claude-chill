//! **claude-chill** — a PTY proxy that tames Claude Code's massive terminal updates.
//!
//! Intercepts synchronized output blocks, renders only screen deltas via a VT100
//! emulator, and maintains a scrollback history buffer accessible through a
//! configurable hotkey.
//!
//! Supports Unix (PTY, termios, POSIX signals) and Windows (ConPTY, Console API).

pub mod alt_screen;
pub mod config;
pub mod escape_sequences;
pub mod history_filter;
pub mod history_manager;
pub mod key_parser;
pub mod line_buffer;
pub mod sequence_match;
pub mod sync_block;
pub mod vt_renderer;

// Platform-specific modules
#[cfg(unix)]
pub mod kitty_tracker;
#[cfg(unix)]
pub mod proxy;
#[cfg(unix)]
pub mod terminal;

#[cfg(windows)]
pub mod kitty_tracker_windows;
#[cfg(windows)]
pub mod proxy_windows;
#[cfg(windows)]
pub mod terminal_windows;

// Re-export platform-specific modules under canonical names
#[cfg(windows)]
pub use kitty_tracker_windows as kitty_tracker;
#[cfg(windows)]
pub use proxy_windows as proxy;
#[cfg(windows)]
pub use terminal_windows as terminal;
