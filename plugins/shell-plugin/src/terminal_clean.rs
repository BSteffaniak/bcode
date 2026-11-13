//! Streaming terminal-output normalization for shell model artifacts.
//!
//! Output is interpreted by a real terminal emulator (`bmux_terminal_grid`), so the text the
//! model reads is what the terminal showed: progress bars that redraw in place leave their
//! final state, full-screen programs leave what they returned to, wide characters and tab
//! stops line up, and lines the terminal soft-wrapped read as the logical lines the program
//! printed. Lines are written as they scroll off the screen, where nothing can change them;
//! what the screen still shows is written when the output ends.
//!
//! One departure from a terminal: a line feed without a carriage return (output a program
//! wrote with newline translation off) starts a new line rather than only moving down, so the
//! text reads as lines instead of a staircase.

use std::io::{self, Write};

use bmux_terminal_grid::{GridLimits, TerminalGridStream, content_text};

/// History rows the emulator keeps. None: a program cannot move its cursor above the screen,
/// so a line that scrolls off is final and is written at once.
const RETAINED_HISTORY_ROWS: usize = 0;

/// Summary of a completed normalized terminal transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCleanSummary {
    /// Number of bytes written to the normalized transcript.
    pub bytes_written: u64,
    /// Bounded tail of the normalized transcript.
    pub tail: String,
    /// Whether the bounded tail omitted earlier normalized transcript bytes.
    pub tail_truncated: bool,
}

/// Streaming terminal normalizer that writes only stable, cleaned text to `writer`.
pub struct TerminalCleanWriter<W> {
    writer: W,
    stream: TerminalGridStream,
    tail: ByteTail,
    bytes_written: u64,
    /// The previous byte was a carriage return.
    after_return: bool,
}

impl<W: Write> TerminalCleanWriter<W> {
    /// Create a streaming terminal normalizer.
    pub fn new(writer: W, columns: u16, rows: u16, max_tail_bytes: usize) -> Self {
        let mut stream = TerminalGridStream::new(
            columns.max(1),
            rows.max(1),
            GridLimits {
                scrollback_rows: RETAINED_HISTORY_ROWS,
            },
        )
        .unwrap_or_else(|_| unreachable!("non-zero terminal dimensions are valid"));
        stream.grid_mut().collect_evicted_text(true);
        Self {
            writer,
            stream,
            tail: ByteTail::new(max_tail_bytes),
            bytes_written: 0,
            after_return: false,
        }
    }

    /// Consume a raw PTY chunk and append newly stable cleaned text to the writer.
    pub fn write_chunk(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut start = 0;
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == b'\n' && !self.after_return {
                self.stream.process(&bytes[start..index]);
                self.stream.process(b"\r");
                start = index;
            }
            self.after_return = *byte == b'\r';
        }
        self.stream.process(&bytes[start..]);
        let evicted = self.stream.grid_mut().take_evicted_text();
        self.emit(evicted.as_bytes())
    }

    /// Capture a bounded semantic preview without consuming or flushing live state.
    /// Terminal parsing remains producer-native; consumers receive plain text only.
    pub fn snapshot(&self) -> TerminalCleanSummary {
        let mut tail = self.tail.clone();
        let remaining = self.remaining_text();
        tail.push(remaining.as_bytes());
        TerminalCleanSummary {
            bytes_written: self
                .bytes_written
                .saturating_add(u64::try_from(remaining.len()).unwrap_or(u64::MAX)),
            tail: tail.text(),
            tail_truncated: tail.truncated,
        }
    }

    /// Flush all remaining visible terminal state and return the normalized summary.
    pub fn finish(mut self) -> io::Result<TerminalCleanSummary> {
        let remaining = self.remaining_text();
        self.emit(remaining.as_bytes())?;
        self.writer.flush()?;
        Ok(TerminalCleanSummary {
            bytes_written: self.bytes_written,
            tail: self.tail.text(),
            tail_truncated: self.tail.truncated,
        })
    }

    /// What the terminal still shows, as lines each ending in a newline.
    fn remaining_text(&self) -> String {
        let mut text = content_text(self.stream.grid());
        if !text.is_empty() {
            text.push('\n');
        }
        text
    }

    fn emit(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.writer.write_all(bytes)?;
        self.tail.push(bytes);
        self.bytes_written = self
            .bytes_written
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ByteTail {
    bytes: Vec<u8>,
    max_bytes: usize,
    truncated: bool,
}

impl ByteTail {
    const fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes,
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.max_bytes == 0 {
            self.truncated = self.truncated || !bytes.is_empty();
            return;
        }
        self.bytes.extend_from_slice(bytes);
        if self.bytes.len() > self.max_bytes {
            let remove = self.bytes.len().saturating_sub(self.max_bytes);
            self.bytes.drain(..remove);
            self.truncated = true;
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn semantic_snapshot_is_non_consuming_plain_and_bounded() {
        let mut output = Vec::new();
        let mut cleaner = super::TerminalCleanWriter::new(&mut output, 80, 24, 128);
        cleaner
            .write_chunk(b"loading 1\rloading 2\x1b[31m red\x1b[0m")
            .unwrap();
        let first = cleaner.snapshot();
        assert!(first.tail.contains("loading 2 red"));
        assert!(!first.tail.contains('\u{1b}'));
        assert_eq!(first, cleaner.snapshot());
        cleaner.write_chunk(b"\nfinished\n").unwrap();
        let final_snapshot = cleaner.snapshot();
        let final_output = cleaner.finish().unwrap();
        assert_eq!(final_snapshot, final_output);
        assert!(final_output.tail.len() <= 128);
    }

    use super::*;

    fn clean(chunks: &[&[u8]]) -> TerminalCleanSummary {
        let mut output = Vec::new();
        let mut cleaner = TerminalCleanWriter::new(&mut output, 80, 24, 1024);
        for chunk in chunks {
            cleaner.write_chunk(chunk).expect("chunk cleans");
        }
        cleaner.finish().expect("cleaner finishes")
    }

    #[test]
    fn strips_sgr_sequences_split_across_chunks() {
        let summary = clean(&[b"hel\x1b[3", b"1mlo\x1b[0m\n"]);

        assert_eq!(summary.tail, "hello\n");
    }

    #[test]
    fn ignores_osc_sequences_split_across_chunks() {
        let summary = clean(&[b"before\n\x1b]0;tit", b"le\x07after\n"]);

        assert_eq!(summary.tail, "before\nafter\n");
    }

    #[test]
    fn carriage_return_keeps_final_visible_line() {
        let summary = clean(&[b"download 10%\rdownload 20%\rdownload 100%\n"]);

        assert_eq!(summary.tail, "download 100%\n");
    }

    #[test]
    fn backspace_mutates_buffered_line() {
        let summary = clean(&[b"abc\x08d\n"]);

        assert_eq!(summary.tail, "abd\n");
    }

    #[test]
    fn erase_line_clears_current_line() {
        let summary = clean(&[b"abcdef\r\x1b[Kxy\n"]);

        assert_eq!(summary.tail, "xy\n");
    }

    #[test]
    fn cursor_back_allows_overwrite() {
        let summary = clean(&[b"abc\x1b[2DX\n"]);

        assert_eq!(summary.tail, "aXc\n");
    }

    #[test]
    fn cursor_up_can_rewrite_active_line() {
        let summary = clean(&[b"one\ntwo\x1b[A\rONE\n"]);

        assert_eq!(summary.tail, "ONE\ntwo\n");
    }

    fn clean_with_size(chunks: &[&[u8]], columns: u16, rows: u16) -> TerminalCleanSummary {
        let mut output = Vec::new();
        let mut cleaner = TerminalCleanWriter::new(&mut output, columns, rows, 4096);
        for chunk in chunks {
            cleaner.write_chunk(chunk).expect("chunk cleans");
        }
        cleaner.finish().expect("cleaner finishes")
    }

    #[test]
    fn cursor_up_cannot_rewrite_flushed_scrollback() {
        let summary = clean_with_size(&[b"one\ntwo\nthree\x1b[A\rTWO\n"], 80, 2);

        assert_eq!(summary.tail, "one\nTWO\nthree\n");
    }

    #[test]
    fn absolute_cursor_position_is_viewport_relative() {
        let summary = clean_with_size(&[b"one\ntwo\nthree\x1b[1;1HTOP\n"], 80, 2);

        assert_eq!(summary.tail, "one\nTOP\nthree\n");
    }

    #[test]
    fn cursor_down_clamps_without_scrolling() {
        let summary = clean_with_size(&[b"top\x1b[99B\rbottom\n"], 80, 2);

        assert_eq!(summary.tail, "top\nbottom\n");
    }

    #[test]
    fn large_output_flushes_progressively() {
        let summary = clean_with_size(&[b"a\nb\nc\nd\ne\n"], 80, 2);

        assert_eq!(summary.tail, "a\nb\nc\nd\ne\n");
    }

    #[test]
    fn carriage_return_without_erase_preserves_remaining_cells() {
        let summary = clean(&[b"abcdef\rxy\n"]);

        assert_eq!(summary.tail, "xycdef\n");
    }

    #[test]
    fn erase_entire_line_removes_all_cells() {
        let summary = clean(&[b"abcdef\r\x1b[2Kxy\n"]);

        assert_eq!(summary.tail, "xy\n");
    }

    #[test]
    fn erase_to_left_preserves_right_cells() {
        let summary = clean(&[b"abcdef\x1b[3D\x1b[1KX\n"]);

        assert_eq!(summary.tail, "   Xef\n");
    }

    #[test]
    fn erase_display_below_clears_lower_viewport_lines() {
        let summary = clean_with_size(&[b"one\ntwo\x1b[1;1H\x1b[Jtop\n"], 80, 3);

        assert_eq!(summary.tail, "top\n");
    }

    #[test]
    fn utf8_codepoint_can_be_split_across_chunks() {
        let summary = clean(&[b"caf", &[0xC3], &[0xA9], b"\n"]);

        assert_eq!(summary.tail, "café\n");
    }

    #[test]
    fn osc_st_sequences_are_ignored() {
        let summary = clean(&[b"a\x1b]8;;https://example.test\x1b\\b\x1b]8;;\x1b\\\n"]);

        assert_eq!(summary.tail, "ab\n");
    }

    #[test]
    fn dcs_payload_is_ignored() {
        let summary = clean(&[b"a\x1bPignored\x1b\\b\n"]);

        assert_eq!(summary.tail, "ab\n");
    }

    #[test]
    fn csi_save_and_restore_cursor_rewrites_saved_position() {
        let summary = clean(&[b"abc\x1b[sdef\x1b[uX\n"]);

        assert_eq!(summary.tail, "abcXef\n");
    }

    #[test]
    fn esc_save_and_restore_cursor_rewrites_saved_position() {
        let summary = clean(&[b"abc\x1b7def\x1b8X\n"]);

        assert_eq!(summary.tail, "abcXef\n");
    }

    #[test]
    fn lines_the_terminal_wrapped_read_as_printed() {
        // Wrapped at three columns, scrolled off a two-row screen, and still one line.
        let summary = clean_with_size(&[b"abcdef\r\nabc def ghi\r\nend"], 3, 2);

        assert_eq!(summary.tail, "abcdef\nabc def ghi\nend\n");
    }

    #[test]
    fn spaces_where_the_terminal_wrapped_are_kept() {
        // "word word" wraps after "word " at five columns, on the screen and once scrolled off.
        for rows in [5, 1] {
            let summary = clean_with_size(&[b"word word word\r\nend"], 5, rows);

            assert_eq!(summary.tail, "word word word\nend\n", "{rows} rows");
        }
    }

    #[test]
    fn line_feeds_without_returns_start_lines() {
        let summary = clean(&[b"a\nb\n", b"\nc\r\nd"]);

        assert_eq!(summary.tail, "a\nb\n\nc\nd\n");
    }

    #[test]
    fn full_screen_programs_leave_what_the_terminal_returns_to() {
        let summary = clean(&[
            b"before\r\n\x1b[?1049h\x1b[H\x1b[2Jpager page\r\n~",
            b"\x1b[?1049lafter\r\n",
        ]);

        assert_eq!(summary.tail, "before\nafter\n");
    }

    #[test]
    fn wide_characters_and_tabs_line_up() {
        let summary = clean(&["日本\tx\r\nab\tx\r\n".as_bytes()]);

        assert_eq!(summary.tail, "日本    x\nab      x\n");
    }

    #[test]
    fn output_beyond_the_screen_is_complete_however_chunked() {
        use std::fmt::Write as _;
        let bytes: Vec<u8> = (1..=500)
            .flat_map(|n| format!("line {n}\r\n").into_bytes())
            .collect();
        let expected = (1..=500).fold(String::new(), |mut text, n| {
            let _ = writeln!(text, "line {n}");
            text
        });
        for chunk in [1, 13, 4096] {
            let mut log = Vec::new();
            let mut cleaner = TerminalCleanWriter::new(&mut log, 20, 5, 64);
            for piece in bytes.chunks(chunk) {
                cleaner.write_chunk(piece).expect("chunk cleans");
            }
            let summary = cleaner.finish().expect("cleaner finishes");
            assert_eq!(String::from_utf8(log).unwrap(), expected, "{chunk}");
            assert_eq!(summary.bytes_written, expected.len() as u64);
            assert!(summary.tail_truncated && expected.ends_with(&summary.tail));
        }
    }
}
