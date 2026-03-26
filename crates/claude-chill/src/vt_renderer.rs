//! VT100-based differential renderer.
//!
//! Feeds output through a VT100 emulator and produces screen diffs wrapped
//! in synchronised-output markers. The caller is responsible for writing
//! the rendered bytes to the terminal.

use crate::escape_sequences::{OUTPUT_BUFFER_CAPACITY, SYNC_END, SYNC_START};
use log::debug;
use std::time::{Duration, Instant};

const RENDER_DELAY_MS: u64 = 5;
const SYNC_BLOCK_DELAY_MS: u64 = 50;

/// VT100 screen-diff renderer that tracks parser state and produces
/// synchronised output frames.
pub struct VtRenderer {
    vt_parser: vt100::Parser,
    vt_prev_screen: Option<vt100::Screen>,
    vt_render_pending: bool,
    output_buffer: Vec<u8>,
    last_output_time: Option<Instant>,
    last_render_time: Option<Instant>,
}

impl VtRenderer {
    /// Create a renderer for the given terminal dimensions.
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            vt_parser: vt100::Parser::new(rows, cols, 0),
            vt_prev_screen: None,
            vt_render_pending: false,
            output_buffer: Vec::with_capacity(OUTPUT_BUFFER_CAPACITY),
            last_output_time: None,
            last_render_time: None,
        }
    }

    /// Feed raw output data through the VT100 parser.
    pub fn process(&mut self, data: &[u8]) {
        self.vt_parser.process(data);
    }

    /// Mark that new output has arrived and a render is pending.
    pub fn mark_pending(&mut self) {
        self.vt_render_pending = true;
        self.last_output_time = Some(Instant::now());
    }

    /// Whether a render is currently pending.
    pub fn is_pending(&self) -> bool {
        self.vt_render_pending
    }

    /// Time of the most recent render, if any.
    pub fn last_render_time(&self) -> Option<Instant> {
        self.last_render_time
    }

    /// Force the next render to be a full screen write (not a diff).
    pub fn force_full_render(&mut self) {
        self.vt_prev_screen = None;
    }

    /// Cancel any pending render (e.g. when entering lookback mode).
    pub fn cancel_pending(&mut self) {
        self.vt_render_pending = false;
    }

    /// Resize the VT emulator to new dimensions.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.vt_parser.screen_mut().set_size(rows, cols);
        self.force_full_render();
    }

    /// Compute the time until the next render should happen.
    /// Returns `None` if no render is pending or rendering is suppressed.
    pub fn time_until_render(
        &self,
        in_lookback_mode: bool,
        in_alt_screen: bool,
        in_sync_block: bool,
    ) -> Option<Duration> {
        compute_render_delay(
            self.vt_render_pending,
            in_lookback_mode,
            in_alt_screen,
            in_sync_block,
            self.last_output_time.map(|t| t.elapsed()),
        )
    }

    /// Render the current screen state and return the bytes to write.
    ///
    /// Returns `None` if no render is pending. The caller must write the
    /// returned bytes to the terminal.
    #[must_use]
    pub fn render(&mut self) -> Option<&[u8]> {
        if !self.vt_render_pending {
            return None;
        }

        let is_diff = self.vt_prev_screen.is_some();
        self.output_buffer.clear();
        self.output_buffer.extend_from_slice(SYNC_START);

        match &self.vt_prev_screen {
            Some(prev) => {
                self.output_buffer
                    .extend_from_slice(&self.vt_parser.screen().contents_diff(prev));
            }
            None => {
                self.output_buffer
                    .extend_from_slice(&self.vt_parser.screen().contents_formatted());
            }
        }

        self.output_buffer
            .extend_from_slice(&self.vt_parser.screen().cursor_state_formatted());
        self.output_buffer.extend_from_slice(SYNC_END);

        debug!(
            "VtRenderer::render: diff={} output_len={}",
            is_diff,
            self.output_buffer.len()
        );

        self.vt_prev_screen = Some(self.vt_parser.screen().clone());
        self.vt_render_pending = false;
        self.last_render_time = Some(Instant::now());

        Some(&self.output_buffer)
    }

    /// Render only if the delay has elapsed. Returns bytes to write, or `None`.
    #[must_use]
    pub fn flush_if_ready(
        &mut self,
        in_lookback_mode: bool,
        in_alt_screen: bool,
        in_sync_block: bool,
    ) -> Option<&[u8]> {
        if let Some(Duration::ZERO) =
            self.time_until_render(in_lookback_mode, in_alt_screen, in_sync_block)
        {
            return self.render();
        }
        None
    }
}

/// Pure decision: compute the delay before the next VT render.
/// Returns None if no render is pending or rendering is suppressed.
#[must_use]
pub fn compute_render_delay(
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
        let result =
            compute_render_delay(true, false, false, false, Some(Duration::from_millis(2)));
        assert!(result.is_some());
        let remaining = result.unwrap();
        assert!(remaining > Duration::ZERO);
        assert!(remaining <= Duration::from_millis(RENDER_DELAY_MS));
    }

    #[test]
    fn test_render_delay_longer_in_sync_block() {
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

    #[test]
    fn test_render_delay_no_output_yet() {
        assert_eq!(
            compute_render_delay(true, false, false, false, None),
            Some(Duration::ZERO),
        );
    }

    #[test]
    fn test_render_delay_exact_boundary_normal() {
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
    fn test_new_renderer_not_pending() {
        let renderer = VtRenderer::new(24, 80);
        assert!(!renderer.is_pending());
        assert!(renderer.last_render_time().is_none());
    }

    #[test]
    fn test_mark_pending() {
        let mut renderer = VtRenderer::new(24, 80);
        renderer.mark_pending();
        assert!(renderer.is_pending());
    }

    #[test]
    fn test_cancel_pending() {
        let mut renderer = VtRenderer::new(24, 80);
        renderer.mark_pending();
        renderer.cancel_pending();
        assert!(!renderer.is_pending());
    }

    #[test]
    fn test_render_returns_none_when_not_pending() {
        let mut renderer = VtRenderer::new(24, 80);
        assert!(renderer.render().is_none());
    }

    #[test]
    fn test_render_returns_bytes_when_pending() {
        let mut renderer = VtRenderer::new(24, 80);
        renderer.process(b"Hello, World!");
        renderer.mark_pending();
        let output = renderer.render();
        assert!(output.is_some());
        let bytes = output.unwrap();
        assert!(bytes.starts_with(SYNC_START));
        assert!(bytes.ends_with(SYNC_END));
        assert!(!renderer.is_pending());
        assert!(renderer.last_render_time().is_some());
    }

    #[test]
    fn test_force_full_render_clears_prev_screen() {
        let mut renderer = VtRenderer::new(24, 80);
        renderer.process(b"Hello");
        renderer.mark_pending();
        let _ = renderer.render(); // first render: full

        renderer.process(b" World");
        renderer.mark_pending();
        renderer.force_full_render();
        let output = renderer.render();
        assert!(output.is_some());
        // After force_full_render, should produce a full render, not just a diff
        // (we can't easily distinguish, but we verify it doesn't crash)
    }
}
