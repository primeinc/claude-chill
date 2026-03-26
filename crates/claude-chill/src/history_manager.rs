//! History management: combines a line buffer with an escape-sequence filter.
//!
//! Owns the [`LineBuffer`] and [`HistoryFilter`], providing a single entry point
//! for applying output segments to the scrollback history.

use crate::escape_sequences::{CLEAR_SCREEN, CURSOR_HOME};
use crate::history_filter::HistoryFilter;
use crate::line_buffer::LineBuffer;
use crate::sync_block::OutputSegment;
use log::debug;

/// Manages scrollback history by filtering escape sequences and storing lines.
pub struct HistoryManager {
    buffer: LineBuffer,
    filter: HistoryFilter,
}

impl HistoryManager {
    /// Create a new history manager with the given line capacity.
    ///
    /// Seeds the buffer with a clear-screen + cursor-home so that replaying
    /// history always starts from a clean slate.
    pub fn new(max_lines: usize) -> Self {
        let mut buffer = LineBuffer::new(max_lines);
        buffer.push_bytes(CLEAR_SCREEN);
        buffer.push_bytes(CURSOR_HOME);
        Self {
            buffer,
            filter: HistoryFilter::new(),
        }
    }

    /// Apply an output segment to the history buffer.
    ///
    /// For passthrough data, filters and appends directly.
    /// For sync blocks, clears history first if the block is a full redraw.
    pub fn apply_segment(&mut self, segment: OutputSegment<'_>) {
        match segment {
            OutputSegment::PassThrough(data) => {
                self.push(data);
            }
            OutputSegment::SyncBlock {
                data,
                is_full_redraw,
            } => {
                if is_full_redraw {
                    debug!("CLEARING HISTORY");
                    self.buffer.clear();
                    self.buffer.push_bytes(CLEAR_SCREEN);
                    self.buffer.push_bytes(CURSOR_HOME);
                }
                self.push(&data);
            }
        }
    }

    /// Push data to history, filtering out unsafe escape sequences.
    pub fn push(&mut self, data: &[u8]) {
        let filtered = self.filter.filter(data);
        self.buffer.push_bytes(filtered.as_ref());
    }

    /// Discard all buffered content and re-seed with clear-screen.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.buffer.push_bytes(CLEAR_SCREEN);
        self.buffer.push_bytes(CURSOR_HOME);
    }

    /// Total bytes across all buffered lines.
    pub fn total_bytes(&self) -> usize {
        self.buffer.total_bytes()
    }

    /// Number of lines currently buffered.
    pub fn line_count(&self) -> usize {
        self.buffer.line_count()
    }

    /// Append all buffered content to `output`.
    pub fn append_all(&self, output: &mut Vec<u8>) {
        self.buffer.append_all(output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_block::SyncBlockParser;

    fn history_text(hm: &HistoryManager) -> String {
        let mut output = Vec::new();
        hm.append_all(&mut output);
        String::from_utf8_lossy(&output).into_owned()
    }

    #[test]
    fn test_push_plain_text() {
        let mut hm = HistoryManager::new(1000);
        hm.push(b"hello world\n");

        let text = history_text(&hm);
        assert!(text.contains("hello world"));
    }

    #[test]
    fn test_push_filters_mode_sequences() {
        let mut hm = HistoryManager::new(1000);
        hm.push(b"\x1b[?1004h\x1b[?1000hvisible text\x1b[?2004h");

        let text = history_text(&hm);
        assert!(text.contains("visible text"));
        assert!(!text.contains("1004"));
        assert!(!text.contains("1000"));
        assert!(!text.contains("2004"));
    }

    #[test]
    fn test_apply_passthrough_segment() {
        let mut hm = HistoryManager::new(1000);

        let segment = OutputSegment::PassThrough(b"hello world\n");
        hm.apply_segment(segment);

        let text = history_text(&hm);
        assert!(text.contains("hello world"));
    }

    #[test]
    fn test_full_redraw_clears_history() {
        let mut hm = HistoryManager::new(1000);
        hm.push(b"old content\n");

        let mut data = Vec::new();
        data.extend_from_slice(CLEAR_SCREEN);
        data.extend_from_slice(CURSOR_HOME);
        data.extend_from_slice(b"new content\n");

        let segment = OutputSegment::SyncBlock {
            data,
            is_full_redraw: true,
        };
        hm.apply_segment(segment);

        let text = history_text(&hm);
        assert!(!text.contains("old content"));
        assert!(text.contains("new content"));
    }

    #[test]
    fn test_non_redraw_sync_preserves_history() {
        let mut hm = HistoryManager::new(1000);
        hm.push(b"existing\n");

        let segment = OutputSegment::SyncBlock {
            data: b"appended\n".to_vec(),
            is_full_redraw: false,
        };
        hm.apply_segment(segment);

        let text = history_text(&hm);
        assert!(text.contains("existing"));
        assert!(text.contains("appended"));
    }

    #[test]
    fn test_clear_reseeds() {
        let mut hm = HistoryManager::new(1000);
        hm.push(b"data\n");
        hm.clear();

        let text = history_text(&hm);
        assert!(!text.contains("data"));
        // Should have clear screen + cursor home
        let mut raw = Vec::new();
        hm.append_all(&mut raw);
        assert!(raw.starts_with(CLEAR_SCREEN));
    }

    #[test]
    fn test_realistic_output_flow() {
        let mut parser = SyncBlockParser::new();
        let mut hm = HistoryManager::new(10000);

        // Step 1: passthrough text
        let mut segments = Vec::new();
        parser.parse(b"$ claude\r\nStarting...\r\n", &mut segments);
        for seg in segments.drain(..) {
            hm.apply_segment(seg);
        }
        let text = history_text(&hm);
        assert!(text.contains("Starting..."), "passthrough text in history");

        // Step 2: partial sync block (no full redraw)
        let mut partial = Vec::new();
        partial.extend_from_slice(crate::escape_sequences::SYNC_START);
        partial.extend_from_slice(b"\x1b[10;1HThinking...");
        partial.extend_from_slice(crate::escape_sequences::SYNC_END);
        parser.parse(&partial, &mut segments);
        for seg in segments.drain(..) {
            hm.apply_segment(seg);
        }
        let text = history_text(&hm);
        assert!(text.contains("Starting..."), "old text preserved");
        assert!(text.contains("Thinking..."), "new text added");

        // Step 3: full redraw
        let mut full = Vec::new();
        full.extend_from_slice(crate::escape_sequences::SYNC_START);
        full.extend_from_slice(CLEAR_SCREEN);
        full.extend_from_slice(CURSOR_HOME);
        full.extend_from_slice(b"Fresh screen\r\n");
        full.extend_from_slice(crate::escape_sequences::SYNC_END);
        parser.parse(&full, &mut segments);
        for seg in segments.drain(..) {
            hm.apply_segment(seg);
        }
        let text = history_text(&hm);
        assert!(!text.contains("Starting..."), "old text cleared");
        assert!(text.contains("Fresh screen"), "new content present");
    }

    #[test]
    fn test_split_sync_block_across_chunks() {
        let mut parser = SyncBlockParser::new();
        let mut hm = HistoryManager::new(10000);

        // Chunk 1
        let mut chunk1 = Vec::new();
        chunk1.extend_from_slice(b"preamble\r\n");
        chunk1.extend_from_slice(crate::escape_sequences::SYNC_START);
        chunk1.extend_from_slice(b"partial con");

        let mut segments = Vec::new();
        parser.parse(&chunk1, &mut segments);
        for seg in segments.drain(..) {
            hm.apply_segment(seg);
        }
        assert!(history_text(&hm).contains("preamble"));
        assert!(parser.in_sync_block());

        // Chunk 2
        let mut chunk2 = Vec::new();
        chunk2.extend_from_slice(b"tent here");
        chunk2.extend_from_slice(crate::escape_sequences::SYNC_END);
        chunk2.extend_from_slice(b"\r\nafter sync\r\n");

        parser.parse(&chunk2, &mut segments);
        for seg in segments.drain(..) {
            hm.apply_segment(seg);
        }
        assert!(history_text(&hm).contains("after sync"));
        assert!(!parser.in_sync_block());
    }

    #[test]
    fn test_mode_sequences_filtered_inside_sync_blocks() {
        let mut parser = SyncBlockParser::new();
        let mut hm = HistoryManager::new(10000);

        // A sync block containing mode-setting sequences that should be filtered
        let mut input = Vec::new();
        input.extend_from_slice(crate::escape_sequences::SYNC_START);
        // Focus tracking + mouse mode + visible text + bracketed paste
        input.extend_from_slice(b"\x1b[?1004h\x1b[?1000hvisible text\x1b[?2004h");
        input.extend_from_slice(crate::escape_sequences::SYNC_END);

        let mut segments = Vec::new();
        parser.parse(&input, &mut segments);
        for seg in segments.drain(..) {
            hm.apply_segment(seg);
        }

        let text = history_text(&hm);
        assert!(
            text.contains("visible text"),
            "visible text should be in history"
        );
        assert!(
            !text.contains("1004"),
            "focus tracking should be filtered out"
        );
        assert!(!text.contains("1000"), "mouse mode should be filtered out");
        assert!(
            !text.contains("2004"),
            "bracketed paste should be filtered out"
        );
    }

    #[test]
    fn test_full_redraw_then_partial_then_full_redraw() {
        let mut parser = SyncBlockParser::new();
        let mut hm = HistoryManager::new(10000);

        // Full redraw 1
        let mut full1 = Vec::new();
        full1.extend_from_slice(crate::escape_sequences::SYNC_START);
        full1.extend_from_slice(crate::escape_sequences::CLEAR_SCREEN);
        full1.extend_from_slice(crate::escape_sequences::CURSOR_HOME);
        full1.extend_from_slice(b"Screen A\r\n");
        full1.extend_from_slice(crate::escape_sequences::SYNC_END);

        let mut segments = Vec::new();
        parser.parse(&full1, &mut segments);
        for seg in segments.drain(..) {
            hm.apply_segment(seg);
        }
        assert!(history_text(&hm).contains("Screen A"));

        // Partial update
        let mut partial = Vec::new();
        partial.extend_from_slice(crate::escape_sequences::SYNC_START);
        partial.extend_from_slice(b"Added line\r\n");
        partial.extend_from_slice(crate::escape_sequences::SYNC_END);

        parser.parse(&partial, &mut segments);
        for seg in segments.drain(..) {
            hm.apply_segment(seg);
        }
        assert!(history_text(&hm).contains("Screen A"));
        assert!(history_text(&hm).contains("Added line"));

        // Full redraw 2 — should clear both Screen A and Added line
        let mut full2 = Vec::new();
        full2.extend_from_slice(crate::escape_sequences::SYNC_START);
        full2.extend_from_slice(crate::escape_sequences::CLEAR_SCREEN);
        full2.extend_from_slice(crate::escape_sequences::CURSOR_HOME);
        full2.extend_from_slice(b"Screen B\r\n");
        full2.extend_from_slice(crate::escape_sequences::SYNC_END);

        parser.parse(&full2, &mut segments);
        for seg in segments.drain(..) {
            hm.apply_segment(seg);
        }
        let text = history_text(&hm);
        assert!(!text.contains("Screen A"), "Screen A should be cleared");
        assert!(!text.contains("Added line"), "Added line should be cleared");
        assert!(text.contains("Screen B"), "Screen B should be present");
    }

    #[test]
    fn test_stress_many_sync_blocks() {
        // Simulate a realistic Claude Code session: hundreds of sync blocks
        // interleaved with passthrough data.
        let mut parser = SyncBlockParser::new();
        let mut hm = HistoryManager::new(10000);
        let mut segments = Vec::new();

        for i in 0..200 {
            // Passthrough text
            let text = format!("output line {i}\r\n");
            parser.parse(text.as_bytes(), &mut segments);
            for seg in segments.drain(..) {
                hm.apply_segment(seg);
            }

            // Sync block (every 10th is a full redraw)
            let mut block = Vec::new();
            block.extend_from_slice(crate::escape_sequences::SYNC_START);
            if i % 10 == 0 {
                block.extend_from_slice(crate::escape_sequences::CLEAR_SCREEN);
                block.extend_from_slice(crate::escape_sequences::CURSOR_HOME);
            }
            block.extend_from_slice(format!("sync {i}\r\n").as_bytes());
            block.extend_from_slice(crate::escape_sequences::SYNC_END);
            parser.parse(&block, &mut segments);
            for seg in segments.drain(..) {
                hm.apply_segment(seg);
            }
        }

        // After 200 iterations with redraws every 10, last redraw was at i=190
        let text = history_text(&hm);
        // Should contain content from after last full redraw (i >= 190)
        assert!(text.contains("sync 199"), "latest sync should be present");
        // Content from before last full redraw should be gone
        assert!(
            !text.contains("output line 189"),
            "pre-redraw content should be cleared"
        );
        // History line count should be bounded
        assert!(
            hm.line_count() < 10000,
            "history should not grow unboundedly"
        );
    }

    #[test]
    fn test_empty_input_handling() {
        let mut hm = HistoryManager::new(1000);

        // Push empty data
        hm.push(b"");
        // Apply empty passthrough
        hm.apply_segment(OutputSegment::PassThrough(b""));
        // Apply empty sync block
        hm.apply_segment(OutputSegment::SyncBlock {
            data: Vec::new(),
            is_full_redraw: false,
        });

        // Should still have the initial clear screen seed
        let mut output = Vec::new();
        hm.append_all(&mut output);
        assert!(!output.is_empty(), "should have initial seed data");
    }
}
