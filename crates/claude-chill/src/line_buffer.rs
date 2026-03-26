use memchr::memchr;
use std::collections::VecDeque;

/// Line-oriented ring buffer that retains the last N lines of byte data.
pub struct LineBuffer {
    lines: VecDeque<Vec<u8>>,
    current_line: Vec<u8>,
    max_lines: usize,
    cached_bytes: usize,
}

impl LineBuffer {
    /// Create a buffer that retains at most `max_lines` completed lines.
    pub fn new(max_lines: usize) -> Self {
        // Pre-allocate for typical initial growth; caps at max_lines.
        let initial_capacity = max_lines.min(1024);
        Self {
            lines: VecDeque::with_capacity(initial_capacity),
            current_line: Vec::new(),
            max_lines,
            cached_bytes: 0,
        }
    }

    /// Append a single byte, committing the current line on `\n`.
    pub fn push_byte(&mut self, byte: u8) {
        if byte == b'\n' {
            self.commit_line();
        } else {
            self.current_line.push(byte);
        }
    }

    /// Append a slice of bytes, splitting on `\n` and evicting old lines.
    pub fn push_bytes(&mut self, bytes: &[u8]) {
        let mut pos = 0;
        while pos < bytes.len() {
            match memchr(b'\n', &bytes[pos..]) {
                Some(idx) => {
                    self.current_line.extend_from_slice(&bytes[pos..pos + idx]);
                    self.commit_line();
                    pos += idx + 1;
                }
                None => {
                    self.current_line.extend_from_slice(&bytes[pos..]);
                    break;
                }
            }
        }
    }

    fn commit_line(&mut self) {
        let line = std::mem::take(&mut self.current_line);
        self.cached_bytes += line.len() + 1;
        self.lines.push_back(line);
        if self.lines.len() > self.max_lines
            && let Some(removed) = self.lines.pop_front()
        {
            self.cached_bytes -= removed.len() + 1;
        }
    }

    /// Discard all buffered content.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.current_line.clear();
        self.cached_bytes = 0;
    }

    /// Number of lines currently buffered (including any partial line).
    pub fn line_count(&self) -> usize {
        self.lines.len() + if self.current_line.is_empty() { 0 } else { 1 }
    }

    /// Total bytes across all buffered lines and the current partial line.
    pub fn total_bytes(&self) -> usize {
        self.cached_bytes + self.current_line.len()
    }

    /// Append the last `n` lines (including any partial line) to `output`.
    pub fn append_last_n_lines(&self, n: usize, output: &mut Vec<u8>) {
        let total_lines = self.line_count();
        let lines_to_skip = total_lines.saturating_sub(n);
        let completed_lines_to_skip = lines_to_skip.min(self.lines.len());

        for line in self.lines.iter().skip(completed_lines_to_skip) {
            output.extend_from_slice(line);
            output.push(b'\n');
        }
        if !self.current_line.is_empty() && lines_to_skip < total_lines {
            output.extend_from_slice(&self.current_line);
        }
    }

    /// Append all buffered content to `output`.
    pub fn append_all(&self, output: &mut Vec<u8>) {
        for line in &self.lines {
            output.extend_from_slice(line);
            output.push(b'\n');
        }
        if !self.current_line.is_empty() {
            output.extend_from_slice(&self.current_line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_all(buf: &LineBuffer) -> Vec<u8> {
        let mut result = Vec::new();
        buf.append_all(&mut result);
        result
    }

    fn get_last_n(buf: &LineBuffer, n: usize) -> Vec<u8> {
        let mut result = Vec::new();
        buf.append_last_n_lines(n, &mut result);
        result
    }

    #[test]
    fn test_empty_buffer() {
        let buf = LineBuffer::new(10);
        assert_eq!(buf.line_count(), 0);
        assert_eq!(buf.total_bytes(), 0);
        assert_eq!(get_all(&buf), Vec::<u8>::new());
    }

    #[test]
    fn test_push_single_line() {
        let mut buf = LineBuffer::new(10);
        buf.push_bytes(b"hello\n");
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.total_bytes(), 6);
        assert_eq!(get_all(&buf), b"hello\n");
    }

    #[test]
    fn test_push_partial_line() {
        let mut buf = LineBuffer::new(10);
        buf.push_bytes(b"hello");
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.total_bytes(), 5);
        assert_eq!(get_all(&buf), b"hello");
    }

    #[test]
    fn test_push_multiple_lines() {
        let mut buf = LineBuffer::new(10);
        buf.push_bytes(b"line1\nline2\nline3\n");
        assert_eq!(buf.line_count(), 3);
        assert_eq!(get_all(&buf), b"line1\nline2\nline3\n");
    }

    #[test]
    fn test_max_lines_eviction() {
        let mut buf = LineBuffer::new(3);
        buf.push_bytes(b"a\nb\nc\nd\ne\n");
        assert_eq!(buf.line_count(), 3);
        assert_eq!(get_all(&buf), b"c\nd\ne\n");
    }

    #[test]
    fn test_total_bytes_after_eviction() {
        let mut buf = LineBuffer::new(2);
        buf.push_bytes(b"long_line_1\nshort\nmedium_line\n");
        assert_eq!(buf.line_count(), 2);
        assert_eq!(buf.total_bytes(), 6 + 12);
        assert_eq!(get_all(&buf), b"short\nmedium_line\n");
    }

    #[test]
    fn test_clear() {
        let mut buf = LineBuffer::new(10);
        buf.push_bytes(b"line1\nline2\npartial");
        buf.clear();
        assert_eq!(buf.line_count(), 0);
        assert_eq!(buf.total_bytes(), 0);
        assert_eq!(get_all(&buf), Vec::<u8>::new());
    }

    #[test]
    fn test_get_last_n_lines_all() {
        let mut buf = LineBuffer::new(10);
        buf.push_bytes(b"a\nb\nc\n");
        assert_eq!(get_last_n(&buf, 10), b"a\nb\nc\n");
    }

    #[test]
    fn test_get_last_n_lines_subset() {
        let mut buf = LineBuffer::new(10);
        buf.push_bytes(b"a\nb\nc\nd\ne\n");
        assert_eq!(get_last_n(&buf, 2), b"d\ne\n");
    }

    #[test]
    fn test_get_last_n_lines_with_partial() {
        let mut buf = LineBuffer::new(10);
        buf.push_bytes(b"a\nb\nc\npartial");
        assert_eq!(buf.line_count(), 4);
        assert_eq!(get_last_n(&buf, 2), b"c\npartial");
    }

    #[test]
    fn test_get_last_n_lines_only_partial() {
        let mut buf = LineBuffer::new(10);
        buf.push_bytes(b"a\nb\nc\npartial");
        assert_eq!(get_last_n(&buf, 1), b"partial");
    }

    #[test]
    fn test_get_last_n_lines_zero() {
        let mut buf = LineBuffer::new(10);
        buf.push_bytes(b"a\nb\nc\n");
        assert_eq!(get_last_n(&buf, 0), Vec::<u8>::new());
    }

    #[test]
    fn test_crlf_preserved() {
        let mut buf = LineBuffer::new(10);
        buf.push_bytes(b"line1\r\nline2\r\n");
        assert_eq!(
            get_all(&buf),
            b"line1\r\nline2\r\n",
            "CRLF must be preserved"
        );
    }

    #[test]
    fn test_crlf_preserved_in_last_n() {
        let mut buf = LineBuffer::new(10);
        buf.push_bytes(b"line1\r\nline2\r\nline3\r\n");
        assert_eq!(
            get_last_n(&buf, 2),
            b"line2\r\nline3\r\n",
            "CRLF must be preserved in last_n"
        );
    }

    #[test]
    fn test_large_input_correctness() {
        // Simulate a large sync block (100K lines) to verify memchr-based
        // push_bytes produces identical results to byte-at-a-time push_byte.
        let max_lines = 1000;
        let mut buf_bulk = LineBuffer::new(max_lines);
        let mut buf_byte = LineBuffer::new(max_lines);

        // Build a large input: 5000 lines of varying length
        let mut input = Vec::new();
        for i in 0..5000 {
            let line = format!("line {i}: {}\n", "x".repeat(i % 80));
            input.extend_from_slice(line.as_bytes());
        }

        // Push via bulk method
        buf_bulk.push_bytes(&input);

        // Push via byte-at-a-time method
        for &byte in &input {
            buf_byte.push_byte(byte);
        }

        assert_eq!(buf_bulk.line_count(), buf_byte.line_count());
        assert_eq!(buf_bulk.total_bytes(), buf_byte.total_bytes());

        let mut out_bulk = Vec::new();
        let mut out_byte = Vec::new();
        buf_bulk.append_all(&mut out_bulk);
        buf_byte.append_all(&mut out_byte);
        assert_eq!(
            out_bulk, out_byte,
            "bulk and byte-at-a-time must produce identical output"
        );
    }

    #[test]
    fn test_push_bytes_empty_input() {
        let mut buf = LineBuffer::new(10);
        buf.push_bytes(b"existing\n");
        buf.push_bytes(b"");
        assert_eq!(buf.line_count(), 1);
        assert_eq!(get_all(&buf), b"existing\n");
    }

    #[test]
    fn test_push_bytes_only_newlines() {
        let mut buf = LineBuffer::new(10);
        buf.push_bytes(b"\n\n\n");
        assert_eq!(buf.line_count(), 3);
        assert_eq!(get_all(&buf), b"\n\n\n");
    }

    #[test]
    fn test_push_bytes_consecutive_calls() {
        let mut buf = LineBuffer::new(10);
        buf.push_bytes(b"hel");
        buf.push_bytes(b"lo\nwor");
        buf.push_bytes(b"ld\n");
        assert_eq!(buf.line_count(), 2);
        assert_eq!(get_all(&buf), b"hello\nworld\n");
    }
}
