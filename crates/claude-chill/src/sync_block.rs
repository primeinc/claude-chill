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
        // Re-allocate for next sync block
        self.sync_buffer = Vec::with_capacity(SYNC_BUFFER_CAPACITY);

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
}
