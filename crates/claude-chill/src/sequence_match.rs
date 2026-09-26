//! Streaming byte-at-a-time sequence matcher for detecting lookback key input.

/// Result of checking whether a byte sequence is being matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceMatch {
    /// The full sequence has been matched.
    Complete,
    /// The buffer forms a valid prefix of the sequence.
    Partial,
    /// The buffer does not match any prefix of the sequence.
    None,
}

/// Check if appending `byte` to `buffer` would match or partially match `sequence`.
/// Does not mutate `buffer` — the caller is responsible for maintaining the rolling buffer.
///
/// The buffer is windowed to at most `sequence.len() - 1` bytes (the last N-1 bytes),
/// so this works correctly even if the buffer contains earlier unrelated bytes.
#[must_use]
pub fn check(buffer: &[u8], byte: u8, sequence: &[u8]) -> SequenceMatch {
    if sequence.is_empty() {
        return SequenceMatch::None;
    }

    // Window the buffer to the last (sequence.len() - 1) bytes
    let buf_start = if buffer.len() + 1 > sequence.len() {
        buffer.len() + 1 - sequence.len()
    } else {
        0
    };
    let prefix = &buffer[buf_start..];

    let candidate_len = prefix.len() + 1;
    if candidate_len <= sequence.len()
        && sequence[..prefix.len()] == *prefix
        && sequence[prefix.len()] == byte
    {
        if candidate_len == sequence.len() {
            SequenceMatch::Complete
        } else {
            SequenceMatch::Partial
        }
    } else {
        SequenceMatch::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_complete_single_byte() {
        assert_eq!(check(&[], 0x1E, &[0x1E]), SequenceMatch::Complete);
    }

    #[test]
    fn test_complete_multi_byte() {
        let sequence = b"\x1b[54;5u";
        let mut buffer = Vec::new();
        for &byte in &sequence[..sequence.len() - 1] {
            let result = check(&buffer, byte, sequence);
            assert_eq!(result, SequenceMatch::Partial);
            buffer.push(byte);
            if buffer.len() > sequence.len() {
                buffer.drain(..buffer.len() - sequence.len());
            }
        }
        assert_eq!(
            check(&buffer, sequence[sequence.len() - 1], sequence),
            SequenceMatch::Complete
        );
    }

    #[test]
    fn test_partial() {
        let sequence = b"\x1b[54;5u";
        assert_eq!(check(&[], 0x1b, sequence), SequenceMatch::Partial);
        assert_eq!(check(&[0x1b], b'[', sequence), SequenceMatch::Partial);
        assert_eq!(check(&[0x1b, b'['], b'5', sequence), SequenceMatch::Partial);
    }

    #[test]
    fn test_none_wrong_byte() {
        let sequence = b"\x1b[54;5u";
        assert_eq!(check(&[], b'a', sequence), SequenceMatch::None);
        assert_eq!(check(&[0x1b], b'O', sequence), SequenceMatch::None);
    }

    #[test]
    fn test_buffer_rolling() {
        let sequence = b"\x1b[54;5u";
        assert_eq!(check(&[], b'a', sequence), SequenceMatch::None);
        assert_eq!(check(b"a", b'b', sequence), SequenceMatch::None);
        assert_eq!(check(b"ab", 0x1b, sequence), SequenceMatch::None);
        assert_eq!(check(&[], 0x1b, sequence), SequenceMatch::Partial);
    }

    #[test]
    fn test_interleaved_typing() {
        let sequence = &[0x1E];
        assert_eq!(check(&[], b'a', sequence), SequenceMatch::None);
        assert_eq!(check(b"a", b'b', sequence), SequenceMatch::None);
        assert_eq!(check(b"ab", 0x1E, sequence), SequenceMatch::Complete);
    }

    #[test]
    fn test_long_buffer_overflow() {
        assert_eq!(check(b"0123456789", 0x1E, &[0x1E]), SequenceMatch::Complete);
    }

    #[test]
    fn test_multi_byte_with_correct_prefix() {
        let sequence = b"\x1b[54;5u";
        assert_eq!(check(b"\x1b[54;5", b'u', sequence), SequenceMatch::Complete);
    }

    #[test]
    fn test_almost_match() {
        let sequence = b"\x1b[54;5u";
        assert_eq!(check(b"\x1b[54;5", b'x', sequence), SequenceMatch::None);
    }

    #[test]
    fn test_empty_buffer_empty_sequence() {
        // Edge case: empty sequence should never match
        assert_eq!(check(&[], b'a', &[]), SequenceMatch::None);
    }

    #[test]
    fn test_two_byte_sequence() {
        let sequence = &[0x1b, b'A'];
        assert_eq!(check(&[], 0x1b, sequence), SequenceMatch::Partial);
        assert_eq!(check(&[0x1b], b'A', sequence), SequenceMatch::Complete);
        assert_eq!(check(&[0x1b], b'B', sequence), SequenceMatch::None);
    }

    #[test]
    fn test_buffer_longer_than_sequence() {
        // Buffer has more bytes than the sequence length — should still work
        let sequence = &[0x1E];
        let long_buffer = b"lots of random garbage here";
        assert_eq!(check(long_buffer, 0x1E, sequence), SequenceMatch::Complete);
        assert_eq!(check(long_buffer, b'x', sequence), SequenceMatch::None);
    }
}
