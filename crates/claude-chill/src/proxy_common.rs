//! Shared proxy types and pure decision functions used by both Unix and Windows
//! proxy implementations.

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

/// Pure decision: should auto-lookback trigger?
/// Returns true if all conditions for auto-lookback are met.
#[must_use]
pub fn should_auto_lookback(
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

#[cfg(test)]
mod tests {
    use super::*;

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
            Some(Duration::from_secs(5)),
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
            None,
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
            Some(Duration::from_secs(20)),
            Some(now - Duration::from_secs(10)),
            None,
            now,
        ));
    }

    #[test]
    fn test_auto_lookback_suppressed_no_new_output() {
        let now = Instant::now();
        let last_auto = now - Duration::from_secs(20);
        let render_before_auto = now - Duration::from_secs(25);
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
        let last_auto = now - Duration::from_secs(5);
        let render_after_auto = now - Duration::from_secs(3);
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
        let last_auto = now - Duration::from_secs(20);
        let render_after_auto = now - Duration::from_secs(10);
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

    #[test]
    fn test_auto_lookback_exact_timeout_boundary() {
        let now = Instant::now();
        let timeout = Duration::from_secs(15);
        assert!(!should_auto_lookback(
            timeout,
            false,
            false,
            Some(Duration::from_secs(14)),
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
            Some(Duration::from_secs(16)),
            Some(now - Duration::from_secs(10)),
            None,
            now,
        ));
    }
}
