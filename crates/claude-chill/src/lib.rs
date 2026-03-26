//! **claude-chill** — a PTY proxy that tames Claude Code's massive terminal updates.
//!
//! Intercepts synchronized output blocks, renders only screen deltas via a VT100
//! emulator, and maintains a scrollback history buffer accessible through a
//! configurable hotkey.
//!
//! This crate is Unix-only (requires PTY, termios, and POSIX signals).

#[cfg(not(unix))]
compile_error!("claude-chill requires a Unix platform (Linux or macOS)");

pub mod alt_screen;
pub mod config;
pub mod escape_sequences;
pub mod history_filter;
pub mod history_manager;
pub mod key_parser;
pub mod kitty_tracker;
pub mod line_buffer;
pub mod proxy;
pub mod sequence_match;
pub mod sync_block;
pub mod terminal;
pub mod vt_renderer;
