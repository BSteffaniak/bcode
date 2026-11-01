//! `ConPTY` cursor negotiation that must complete before a hosted process starts.
//!
//! portable-pty creates pseudoconsoles with `PSEUDOCONSOLE_INHERIT_CURSOR`, so
//! conhost writes a leading `CSI 6 n` and blocks the child until the terminal
//! reports a cursor position. A newly allocated terminal is empty, so the reply
//! is the origin. Only that exact leading query is transport bootstrap: any other
//! leading byte releases the retained bytes unchanged and disables recognition
//! for the rest of the execution. The recognized query is omitted from capture so
//! replays and clean output contain only application output.
use std::io::{self, Read, Write};

const QUERY: &[u8] = b"\x1b[6n";
const REPLY: &[u8] = b"\x1b[1;1R";

/// Reader that answers the leading `ConPTY` cursor query through `writer`.
pub(super) struct CursorQueryResponder<R, W> {
    reader: R,
    /// Retained for the reader's lifetime so the pseudoconsole input stays open.
    writer: W,
    prefix: Vec<u8>,
    pending: Vec<u8>,
    finished: bool,
}

impl<R, W> CursorQueryResponder<R, W> {
    pub(super) const fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            prefix: Vec::new(),
            pending: Vec::new(),
            finished: false,
        }
    }
}

impl<R: Read, W: Write> CursorQueryResponder<R, W> {
    /// Feeds bytes through the recognizer, retaining at most `QUERY.len() - 1`
    /// bytes between reads. Returns whether the query was recognized.
    fn project(&mut self, bytes: &[u8]) -> bool {
        for (index, byte) in bytes.iter().copied().enumerate() {
            self.prefix.push(byte);
            if byte != QUERY[self.prefix.len() - 1] {
                self.finished = true;
                self.pending = std::mem::take(&mut self.prefix);
                self.pending.extend_from_slice(&bytes[index + 1..]);
                return false;
            }
            if self.prefix.len() == QUERY.len() {
                self.finished = true;
                self.prefix.clear();
                self.pending = bytes[index + 1..].to_vec();
                return true;
            }
        }
        false
    }
}

impl<R: Read, W: Write> Read for CursorQueryResponder<R, W> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            if !self.pending.is_empty() {
                let count = self.pending.len().min(buffer.len());
                buffer[..count].copy_from_slice(&self.pending[..count]);
                self.pending.drain(..count);
                return Ok(count);
            }
            if self.finished {
                return self.reader.read(buffer);
            }
            let count = self.reader.read(buffer)?;
            if count == 0 {
                // A partial query at EOF is ordinary output.
                self.finished = true;
                self.pending = std::mem::take(&mut self.prefix);
                if self.pending.is_empty() {
                    return Ok(0);
                }
                continue;
            }
            let chunk = buffer[..count].to_vec();
            if self.project(&chunk) {
                self.writer.write_all(REPLY)?;
                self.writer.flush()?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CursorQueryResponder;
    use std::io::Read as _;

    /// Yields each configured chunk from a separate read call.
    struct Chunks(std::collections::VecDeque<Vec<u8>>);

    impl std::io::Read for Chunks {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let Some(mut chunk) = self.0.pop_front() else {
                return Ok(0);
            };
            let count = chunk.len().min(buffer.len());
            buffer[..count].copy_from_slice(&chunk[..count]);
            if count < chunk.len() {
                self.0.push_front(chunk.split_off(count));
            }
            Ok(count)
        }
    }

    fn run(chunks: &[&[u8]], buffer_len: usize) -> (Vec<u8>, Vec<u8>) {
        let reader = Chunks(chunks.iter().map(|chunk| chunk.to_vec()).collect());
        let mut responder = CursorQueryResponder::new(reader, Vec::new());
        let mut output = Vec::new();
        let mut buffer = vec![0; buffer_len];
        loop {
            let count = responder.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..count]);
        }
        (output, responder.writer)
    }

    #[test]
    fn leading_query_is_answered_and_omitted() {
        let (output, reply) = run(&[b"\x1b[6nhello"], 64);
        assert_eq!(output, b"hello");
        assert_eq!(reply, b"\x1b[1;1R");
    }

    #[test]
    fn query_split_across_reads_is_answered_once() {
        let (output, reply) = run(&[b"\x1b", b"[", b"6", b"nhi\x1b[6n"], 64);
        assert_eq!(output, b"hi\x1b[6n");
        assert_eq!(reply, b"\x1b[1;1R");
    }

    #[test]
    fn other_leading_output_passes_through_unanswered() {
        let (output, reply) = run(&[b"\x1b[", b"2Jtext\x1b[6n"], 64);
        assert_eq!(output, b"\x1b[2Jtext\x1b[6n");
        assert!(reply.is_empty());
    }

    #[test]
    fn partial_query_at_eof_is_released() {
        let (output, reply) = run(&[b"\x1b[6"], 64);
        assert_eq!(output, b"\x1b[6");
        assert!(reply.is_empty());
    }

    #[test]
    fn released_bytes_respect_small_buffers() {
        let (output, reply) = run(&[b"\x1b[6", b"xyz"], 2);
        assert_eq!(output, b"\x1b[6xyz");
        assert!(reply.is_empty());
        let (output, reply) = run(&[b"\x1b[6nabcdef"], 2);
        assert_eq!(output, b"abcdef");
        assert_eq!(reply, b"\x1b[1;1R");
    }

    #[test]
    fn empty_stream_reports_eof() {
        let (output, reply) = run(&[], 8);
        assert!(output.is_empty());
        assert!(reply.is_empty());
    }
}
