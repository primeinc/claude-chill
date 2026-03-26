//! Synchronized output block parser for detecting and buffering DEC mode 2026 blocks.

use crate::escape_sequences::{
    CLEAR_SCREEN, CURSOR_HOME, SYNC_BUFFER_CAPACITY, SYNC_END, SYNC_START,
};
use log::debug;
use memchr::memmem;

/// Tracks synchronized output block state and buffers sync block content.
/// Platform-independent.
pub struct SyncBlockParser {
    sync_buffer: Vec<u8>,
    in_sync_block: bool,
    sync_start_finder: memmem::Finder<'static>,
    sync_end_finder: memmem::Finder<'static>,
    clear_screen_finder: memmem::Finder<'static>,
    cursor_home_finder: memmem::Finder<'static>,
}

/// A segment of output data classified by the sync block parser.
#[derive(Debug)]
pub enum OutputSegment<'a> {
    /// Data outside any sync block — should go directly to history.
    PassThrough(&'a [u8]),
    /// A completed sync block. `is_full_redraw` indicates whether it contains
    /// a clear-screen + cursor-home (meaning history should be cleared first).
    SyncBlock { data: Vec<u8>, is_full_redraw: bool },
}

impl SyncBlockParser {
    /// Create a new parser with pre-compiled byte finders for sync markers.
    pub fn new() -> Self {
        Self {
            sync_buffer: Vec::with_capacity(SYNC_BUFFER_CAPACITY),
            in_sync_block: false,
            sync_start_finder: memmem::Finder::new(SYNC_START),
            sync_end_finder: memmem::Finder::new(SYNC_END),
            clear_screen_finder: memmem::Finder::new(CLEAR_SCREEN),
            cursor_home_finder: memmem::Finder::new(CURSOR_HOME),
        }
    }

    /// Whether we are currently inside a sync block
    pub fn in_sync_block(&self) -> bool {
        self.in_sync_block
    }

    /// Reset sync block state (e.g. when exiting lookback mode)
    pub fn reset(&mut self) {
        self.in_sync_block = false;
        self.sync_buffer.clear();
    }

    /// If currently in a sync block, flush it as a completed sync block.
    /// Returns the segment if there was buffered data.
    #[must_use]
    pub fn flush_if_in_sync(&mut self) -> Option<OutputSegment<'static>> {
        if !self.in_sync_block || self.sync_buffer.is_empty() {
            return None;
        }
        let segment = self.make_sync_segment();
        self.in_sync_block = false;
        Some(segment)
    }

    /// Append remaining data to the sync buffer (used when alt screen is
    /// entered mid-sync-block) and flush.
    pub fn append_and_flush(&mut self, data: &[u8]) -> OutputSegment<'static> {
        self.sync_buffer.extend_from_slice(data);
        let segment = self.make_sync_segment();
        self.in_sync_block = false;
        segment
    }

    /// Parse a chunk of output data into segments. Calls `emit` for each
    /// segment found. Returns the position up to which data was consumed.
    /// Any remaining data in an open sync block is buffered internally.
    pub fn parse<'a>(&mut self, data: &'a [u8], segments: &mut Vec<OutputSegment<'a>>) {
        let mut pos = 0;
        while pos < data.len() {
            if self.in_sync_block {
                if let Some(idx) = self.sync_end_finder.find(&data[pos..]) {
                    debug!("SyncBlockParser: SYNC_END at pos={}", pos + idx);
                    self.sync_buffer.extend_from_slice(&data[pos..pos + idx]);
                    self.sync_buffer.extend_from_slice(SYNC_END);
                    segments.push(self.make_sync_segment());
                    self.in_sync_block = false;
                    pos += idx + SYNC_END.len();
                } else {
                    self.sync_buffer.extend_from_slice(&data[pos..]);
                    break;
                }
            } else if let Some(idx) = self.sync_start_finder.find(&data[pos..]) {
                debug!("SyncBlockParser: SYNC_START at pos={}", pos + idx);
                if idx > 0 {
                    segments.push(OutputSegment::PassThrough(&data[pos..pos + idx]));
                }
                self.in_sync_block = true;
                self.sync_buffer.clear();
                self.sync_buffer.extend_from_slice(SYNC_START);
                pos += idx + SYNC_START.len();
            } else {
                segments.push(OutputSegment::PassThrough(&data[pos..]));
                break;
            }
        }
    }

    fn make_sync_segment(&mut self) -> OutputSegment<'static> {
        let has_clear_screen = self.clear_screen_finder.find(&self.sync_buffer).is_some();
        let has_cursor_home = self.cursor_home_finder.find(&self.sync_buffer).is_some();
        let is_full_redraw = has_clear_screen && has_cursor_home;

        debug!(
            "SyncBlockParser: flush len={} full_redraw={}",
            self.sync_buffer.len(),
            is_full_redraw
        );

        let data = std::mem::take(&mut self.sync_buffer);

        OutputSegment::SyncBlock {
            data,
            is_full_redraw,
        }
    }
}

impl Default for SyncBlockParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_passthrough_no_sync() {
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();
        parser.parse(b"hello world", &mut segments);
        assert_eq!(segments.len(), 1);
        match &segments[0] {
            OutputSegment::PassThrough(data) => assert_eq!(*data, b"hello world"),
            _ => panic!("expected PassThrough"),
        }
    }

    #[test]
    fn test_complete_sync_block() {
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();
        let mut input = Vec::new();
        input.extend_from_slice(b"before");
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(b"content");
        input.extend_from_slice(SYNC_END);
        input.extend_from_slice(b"after");

        parser.parse(&input, &mut segments);
        assert_eq!(segments.len(), 3);

        match &segments[0] {
            OutputSegment::PassThrough(data) => assert_eq!(*data, b"before"),
            _ => panic!("expected PassThrough"),
        }
        match &segments[1] {
            OutputSegment::SyncBlock { is_full_redraw, .. } => assert!(!is_full_redraw),
            _ => panic!("expected SyncBlock"),
        }
        match &segments[2] {
            OutputSegment::PassThrough(data) => assert_eq!(*data, b"after"),
            _ => panic!("expected PassThrough"),
        }
    }

    #[test]
    fn test_split_sync_block() {
        let mut parser = SyncBlockParser::new();

        // First chunk: start of sync block
        let mut chunk1 = Vec::new();
        chunk1.extend_from_slice(SYNC_START);
        chunk1.extend_from_slice(b"partial");
        let mut segments = Vec::new();
        parser.parse(&chunk1, &mut segments);
        assert_eq!(segments.len(), 0);
        assert!(parser.in_sync_block());

        // Second chunk: end of sync block
        let mut chunk2 = Vec::new();
        chunk2.extend_from_slice(b"content");
        chunk2.extend_from_slice(SYNC_END);
        parser.parse(&chunk2, &mut segments);
        assert_eq!(segments.len(), 1);
        assert!(!parser.in_sync_block());
    }

    #[test]
    fn test_full_redraw_detection() {
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();
        let mut input = Vec::new();
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(CLEAR_SCREEN);
        input.extend_from_slice(CURSOR_HOME);
        input.extend_from_slice(b"screen content");
        input.extend_from_slice(SYNC_END);

        parser.parse(&input, &mut segments);
        assert_eq!(segments.len(), 1);
        match &segments[0] {
            OutputSegment::SyncBlock { is_full_redraw, .. } => assert!(is_full_redraw),
            _ => panic!("expected SyncBlock"),
        }
    }

    #[test]
    fn test_reset() {
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();
        let mut input = Vec::new();
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(b"partial");
        parser.parse(&input, &mut segments);
        assert!(parser.in_sync_block());

        parser.reset();
        assert!(!parser.in_sync_block());
    }

    #[test]
    fn test_multiple_sync_blocks_in_one_chunk() {
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();
        let mut input = Vec::new();
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(b"block1");
        input.extend_from_slice(SYNC_END);
        input.extend_from_slice(b"gap");
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(b"block2");
        input.extend_from_slice(SYNC_END);

        parser.parse(&input, &mut segments);
        assert_eq!(segments.len(), 3);

        match &segments[0] {
            OutputSegment::SyncBlock { is_full_redraw, .. } => assert!(!is_full_redraw),
            _ => panic!("expected SyncBlock"),
        }
        match &segments[1] {
            OutputSegment::PassThrough(data) => assert_eq!(*data, b"gap"),
            _ => panic!("expected PassThrough"),
        }
        match &segments[2] {
            OutputSegment::SyncBlock { is_full_redraw, .. } => assert!(!is_full_redraw),
            _ => panic!("expected SyncBlock"),
        }
    }

    #[test]
    fn test_sync_block_data_preserved() {
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();
        let mut input = Vec::new();
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(b"inner content");
        input.extend_from_slice(SYNC_END);

        parser.parse(&input, &mut segments);
        assert_eq!(segments.len(), 1);
        match &segments[0] {
            OutputSegment::SyncBlock { data, .. } => {
                // Data should contain SYNC_START + content + SYNC_END
                assert!(data.starts_with(SYNC_START));
                assert!(data.ends_with(SYNC_END));
                // Inner content should be there
                let inner = &data[SYNC_START.len()..data.len() - SYNC_END.len()];
                assert_eq!(inner, b"inner content");
            }
            _ => panic!("expected SyncBlock"),
        }
    }

    #[test]
    fn test_append_and_flush() {
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();

        // Start a sync block
        let mut input = Vec::new();
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(b"partial");
        parser.parse(&input, &mut segments);
        assert!(parser.in_sync_block());

        // Append more data and flush (simulating alt screen enter mid-sync)
        let segment = parser.append_and_flush(b" remaining data");
        assert!(!parser.in_sync_block());
        match segment {
            OutputSegment::SyncBlock { data, .. } => {
                assert!(data.starts_with(SYNC_START));
                // Should contain all buffered data
                let s = String::from_utf8_lossy(&data);
                assert!(s.contains("partial"));
                assert!(s.contains(" remaining data"));
            }
            _ => panic!("expected SyncBlock"),
        }
    }

    #[test]
    fn test_only_clear_screen_not_full_redraw() {
        // Clear screen without cursor home should NOT be full redraw
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();
        let mut input = Vec::new();
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(CLEAR_SCREEN);
        input.extend_from_slice(b"content");
        input.extend_from_slice(SYNC_END);

        parser.parse(&input, &mut segments);
        assert_eq!(segments.len(), 1);
        match &segments[0] {
            OutputSegment::SyncBlock { is_full_redraw, .. } => {
                assert!(!is_full_redraw, "clear screen alone is not full redraw");
            }
            _ => panic!("expected SyncBlock"),
        }
    }

    #[test]
    fn test_empty_chunk() {
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();
        parser.parse(b"", &mut segments);
        assert_eq!(segments.len(), 0);
    }

    #[test]
    fn test_empty_sync_block() {
        // Sync start immediately followed by sync end
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();
        let mut input = Vec::new();
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(SYNC_END);

        parser.parse(&input, &mut segments);
        assert_eq!(segments.len(), 1);
        match &segments[0] {
            OutputSegment::SyncBlock {
                data,
                is_full_redraw,
            } => {
                assert!(!is_full_redraw);
                // Data should contain only the markers
                assert!(data.starts_with(SYNC_START));
                assert!(data.ends_with(SYNC_END));
            }
            _ => panic!("expected SyncBlock"),
        }
    }

    #[test]
    fn test_sync_start_split_at_boundary() {
        // Sync start marker split across two chunks
        let mut parser = SyncBlockParser::new();
        let split_point = SYNC_START.len() / 2;

        let mut segments = Vec::new();
        parser.parse(&SYNC_START[..split_point], &mut segments);
        // First half of sync start looks like passthrough
        assert!(!parser.in_sync_block());

        // Second half completes the marker... but since memmem works on
        // full patterns, a split marker won't be detected as a sync start.
        // This is a known limitation: sync markers split across reads
        // will be treated as passthrough. In practice, terminals write
        // escape sequences atomically.
        let mut chunk2 = Vec::new();
        chunk2.extend_from_slice(&SYNC_START[split_point..]);
        chunk2.extend_from_slice(b"content");
        chunk2.extend_from_slice(SYNC_END);
        parser.parse(&chunk2, &mut segments);
        // The split marker won't be recognized — this documents the limitation
    }

    #[test]
    fn test_only_cursor_home_not_full_redraw() {
        // Cursor home without clear screen should NOT be full redraw
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();
        let mut input = Vec::new();
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(CURSOR_HOME);
        input.extend_from_slice(b"content");
        input.extend_from_slice(SYNC_END);

        parser.parse(&input, &mut segments);
        assert_eq!(segments.len(), 1);
        match &segments[0] {
            OutputSegment::SyncBlock { is_full_redraw, .. } => {
                assert!(!is_full_redraw, "cursor home alone is not full redraw");
            }
            _ => panic!("expected SyncBlock"),
        }
    }

    #[test]
    fn test_flush_if_in_sync_with_data() {
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();

        let mut input = Vec::new();
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(b"buffered data");
        parser.parse(&input, &mut segments);
        assert!(parser.in_sync_block());

        let segment = parser.flush_if_in_sync();
        assert!(segment.is_some());
        assert!(!parser.in_sync_block());
    }

    #[test]
    fn test_flush_if_not_in_sync() {
        let mut parser = SyncBlockParser::new();
        let segment = parser.flush_if_in_sync();
        assert!(segment.is_none());
    }

    #[test]
    fn test_consecutive_sync_blocks_no_gap() {
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();

        let mut input = Vec::new();
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(b"block1");
        input.extend_from_slice(SYNC_END);
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(b"block2");
        input.extend_from_slice(SYNC_END);

        parser.parse(&input, &mut segments);
        assert_eq!(segments.len(), 2);
        // Both should be sync blocks with no passthrough between them
        for seg in &segments {
            match seg {
                OutputSegment::SyncBlock { .. } => {}
                OutputSegment::PassThrough(_) => panic!("unexpected passthrough between blocks"),
            }
        }
    }

    #[test]
    fn test_reset_clears_buffer() {
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();

        let mut input = Vec::new();
        input.extend_from_slice(SYNC_START);
        input.extend_from_slice(b"some data in buffer");
        parser.parse(&input, &mut segments);
        assert!(parser.in_sync_block());

        parser.reset();
        assert!(!parser.in_sync_block());

        // After reset, new sync block should start fresh
        let mut input2 = Vec::new();
        input2.extend_from_slice(SYNC_START);
        input2.extend_from_slice(b"new data");
        input2.extend_from_slice(SYNC_END);
        parser.parse(&input2, &mut segments);
        assert_eq!(segments.len(), 1);
        match &segments[0] {
            OutputSegment::SyncBlock { data, .. } => {
                let s = String::from_utf8_lossy(data);
                assert!(s.contains("new data"));
                assert!(!s.contains("some data"), "old buffer should not leak");
            }
            _ => panic!("expected SyncBlock"),
        }
    }

    #[test]
    fn test_deterministic_varied_patterns() {
        // Feed many different input patterns to verify no panics.
        let mut parser = SyncBlockParser::new();
        let mut segments = Vec::new();

        // Pattern 1: Many small chunks interleaved with sync blocks
        for i in 0..100 {
            let text = format!("line {i}\r\n");
            parser.parse(text.as_bytes(), &mut segments);
            segments.clear();

            let mut block = Vec::new();
            block.extend_from_slice(SYNC_START);
            block.extend_from_slice(format!("block {i}").as_bytes());
            block.extend_from_slice(SYNC_END);
            parser.parse(&block, &mut segments);
            segments.clear();
        }
        assert!(!parser.in_sync_block());

        // Pattern 2: Large single chunk with many sync blocks
        let mut big_chunk = Vec::new();
        for i in 0..50 {
            big_chunk.extend_from_slice(format!("text {i} ").as_bytes());
            big_chunk.extend_from_slice(SYNC_START);
            if i % 5 == 0 {
                big_chunk.extend_from_slice(CLEAR_SCREEN);
                big_chunk.extend_from_slice(CURSOR_HOME);
            }
            big_chunk.extend_from_slice(format!("sync {i}").as_bytes());
            big_chunk.extend_from_slice(SYNC_END);
        }
        parser.parse(&big_chunk, &mut segments);
        // Should have 50 passthrough + 50 sync blocks = 100 segments
        assert_eq!(segments.len(), 100);
        segments.clear();

        // Pattern 3: Sync block that spans many small chunks
        let mut start_chunk = Vec::new();
        start_chunk.extend_from_slice(SYNC_START);
        start_chunk.extend_from_slice(b"begin ");
        parser.parse(&start_chunk, &mut segments);
        assert!(parser.in_sync_block());
        assert!(segments.is_empty());

        for i in 0..20 {
            let mid = format!("middle {i} ");
            parser.parse(mid.as_bytes(), &mut segments);
            assert!(segments.is_empty());
        }

        let mut end_chunk = Vec::new();
        end_chunk.extend_from_slice(b"end");
        end_chunk.extend_from_slice(SYNC_END);
        parser.parse(&end_chunk, &mut segments);
        assert_eq!(segments.len(), 1);
        assert!(!parser.in_sync_block());
    }
}
