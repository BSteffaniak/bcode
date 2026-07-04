//! Streaming terminal-output normalization for shell model artifacts.

use std::collections::VecDeque;
use std::io::{self, Write};

const TAB_WIDTH: usize = 8;

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
    parser: vte::Parser,
    performer: TerminalCleanPerformer<W>,
}

impl<W: Write> TerminalCleanWriter<W> {
    /// Create a streaming terminal normalizer.
    pub fn new(writer: W, columns: u16, rows: u16, max_tail_bytes: usize) -> Self {
        Self {
            parser: vte::Parser::new(),
            performer: TerminalCleanPerformer::new(writer, columns, rows, max_tail_bytes),
        }
    }

    /// Consume a raw PTY chunk and append newly stable cleaned text to the writer.
    pub fn write_chunk(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.parser.advance(&mut self.performer, bytes);
        self.performer.take_error()?;
        Ok(())
    }

    /// Flush all remaining visible terminal state and return the normalized summary.
    pub fn finish(mut self) -> io::Result<TerminalCleanSummary> {
        self.performer.flush_all()?;
        self.performer.writer.flush()?;
        Ok(self.performer.summary())
    }
}

struct TerminalCleanPerformer<W> {
    writer: W,
    columns: usize,
    rows: usize,
    lines: VecDeque<TerminalLine>,
    cursor_row: usize,
    cursor_col: usize,
    saved_cursor: Option<(usize, usize)>,
    tail: ByteTail,
    bytes_written: u64,
    error: Option<io::Error>,
}

impl<W: Write> TerminalCleanPerformer<W> {
    fn new(writer: W, columns: u16, rows: u16, max_tail_bytes: usize) -> Self {
        let mut lines = VecDeque::new();
        lines.push_back(TerminalLine::default());
        Self {
            writer,
            columns: usize::from(columns).max(1),
            rows: usize::from(rows).max(1),
            lines,
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: None,
            tail: ByteTail::new(max_tail_bytes),
            bytes_written: 0,
            error: None,
        }
    }

    fn take_error(&mut self) -> io::Result<()> {
        self.error.take().map_or(Ok(()), Err)
    }

    fn record_result(&mut self, result: io::Result<()>) {
        if self.error.is_none()
            && let Err(error) = result
        {
            self.error = Some(error);
        }
    }

    fn print_char(&mut self, ch: char) {
        if ch == '\n' {
            self.newline();
            return;
        }
        if ch == '\r' {
            self.carriage_return();
            return;
        }
        if ch == '\t' {
            self.tab();
            return;
        }
        if ch.is_control() {
            return;
        }
        self.ensure_cursor_line();
        if let Some(line) = self.lines.get_mut(self.cursor_row) {
            line.put(self.cursor_col, ch);
        }
        self.cursor_col = self.cursor_col.saturating_add(1);
        if self.cursor_col >= self.columns {
            self.cursor_col = 0;
            self.advance_row_with_scroll();
        }
    }

    fn execute_byte(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.newline(),
            b'\r' => self.carriage_return(),
            b'\t' => self.tab(),
            0x08 => self.backspace(),
            _ => {}
        }
    }

    fn newline(&mut self) {
        self.cursor_col = 0;
        self.advance_row_with_scroll();
    }

    fn advance_row_with_scroll(&mut self) {
        if self.cursor_row.saturating_add(1) >= self.rows {
            self.scroll_up_one();
        } else {
            self.cursor_row = self.cursor_row.saturating_add(1);
            self.ensure_cursor_line();
        }
    }

    const fn carriage_return(&mut self) {
        self.cursor_col = 0;
    }

    fn tab(&mut self) {
        let next_stop = (self.cursor_col / TAB_WIDTH).saturating_add(1) * TAB_WIDTH;
        self.cursor_col = next_stop.min(self.columns.saturating_sub(1));
    }

    const fn backspace(&mut self) {
        self.cursor_col = self.cursor_col.saturating_sub(1);
    }

    fn cursor_up(&mut self, count: usize) {
        self.cursor_row = self.cursor_row.saturating_sub(count.max(1));
    }

    fn cursor_down(&mut self, count: usize) {
        self.cursor_row = self
            .cursor_row
            .saturating_add(count.max(1))
            .min(self.rows.saturating_sub(1));
        self.ensure_cursor_line();
    }

    fn cursor_forward(&mut self, count: usize) {
        self.cursor_col = self
            .cursor_col
            .saturating_add(count.max(1))
            .min(self.columns.saturating_sub(1));
    }

    fn cursor_back(&mut self, count: usize) {
        self.cursor_col = self.cursor_col.saturating_sub(count.max(1));
    }

    fn cursor_column(&mut self, column: usize) {
        self.cursor_col = column.saturating_sub(1).min(self.columns.saturating_sub(1));
    }

    fn cursor_position(&mut self, row: usize, column: usize) {
        self.cursor_row = row.saturating_sub(1).min(self.rows.saturating_sub(1));
        self.cursor_col = column.saturating_sub(1).min(self.columns.saturating_sub(1));
        self.ensure_cursor_line();
    }

    const fn save_cursor(&mut self) {
        self.saved_cursor = Some((self.cursor_row, self.cursor_col));
    }

    fn restore_cursor(&mut self) {
        if let Some((row, column)) = self.saved_cursor {
            self.cursor_row = row.min(self.rows.saturating_sub(1));
            self.cursor_col = column.min(self.columns.saturating_sub(1));
            self.ensure_cursor_line();
        }
    }

    fn erase_line(&mut self, mode: usize) {
        self.ensure_cursor_line();
        if let Some(line) = self.lines.get_mut(self.cursor_row) {
            match mode {
                1 => line.clear_to_left(self.cursor_col),
                2 => line.clear(),
                _ => line.clear_to_right(self.cursor_col),
            }
        }
    }

    fn erase_display(&mut self, mode: usize) {
        match mode {
            1 => {
                for row in 0..self.cursor_row {
                    if let Some(line) = self.lines.get_mut(row) {
                        line.clear();
                    }
                }
                self.erase_line(1);
            }
            2 | 3 => {
                for line in &mut self.lines {
                    line.clear();
                }
                self.cursor_row = 0;
                self.cursor_col = 0;
            }
            _ => {
                self.erase_line(0);
                for row in self.cursor_row.saturating_add(1)..self.lines.len() {
                    if let Some(line) = self.lines.get_mut(row) {
                        line.clear();
                    }
                }
            }
        }
    }

    fn ensure_cursor_line(&mut self) {
        while self.lines.len() < self.rows {
            self.lines.push_back(TerminalLine::default());
        }
        while self.cursor_row >= self.lines.len() {
            self.lines.push_back(TerminalLine::default());
        }
    }

    fn scroll_up_one(&mut self) {
        if let Some(line) = self.lines.pop_front() {
            let result = self.write_line(&line);
            self.record_result(result);
        }
        self.lines.push_back(TerminalLine::default());
        self.cursor_row = self.rows.saturating_sub(1);
    }

    fn flush_all(&mut self) -> io::Result<()> {
        let last_content_row = self.lines.iter().rposition(|line| !line.is_empty());
        let lines = self
            .lines
            .drain(..)
            .take(last_content_row.map_or(0, |row| row.saturating_add(1)))
            .collect::<Vec<_>>();
        for line in lines {
            self.write_line(&line)?;
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
        Ok(())
    }

    fn write_line(&mut self, line: &TerminalLine) -> io::Result<()> {
        let text = line.text();
        if !text.is_empty() {
            self.write_bytes(text.as_bytes())?;
        }
        self.write_bytes(b"\n")
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.tail.push(bytes);
        self.bytes_written = self
            .bytes_written
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        Ok(())
    }

    fn summary(&self) -> TerminalCleanSummary {
        TerminalCleanSummary {
            bytes_written: self.bytes_written,
            tail: self.tail.text(),
            tail_truncated: self.tail.truncated,
        }
    }
}

impl<W: Write> vte::Perform for TerminalCleanPerformer<W> {
    fn print(&mut self, ch: char) {
        self.print_char(ch);
    }

    fn execute(&mut self, byte: u8) {
        self.execute_byte(byte);
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
    }

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let first = first_param(params);
        let second = second_param(params);
        match action {
            'A' => self.cursor_up(first),
            'B' => self.cursor_down(first),
            'C' => self.cursor_forward(first),
            'D' => self.cursor_back(first),
            'E' => {
                self.cursor_down(first);
                self.carriage_return();
            }
            'F' => {
                self.cursor_up(first);
                self.carriage_return();
            }
            'G' | '`' => self.cursor_column(first),
            'H' | 'f' => self.cursor_position(first, second),
            'J' => self.erase_display(first_param_or_zero(params)),
            'K' => self.erase_line(first_param_or_zero(params)),
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'D' => self.newline(),
            b'E' => {
                self.newline();
                self.carriage_return();
            }
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'M' => self.cursor_up(1),
            b'c' => self.erase_display(2),
            _ => {}
        }
    }
}

fn first_param(params: &vte::Params) -> usize {
    nth_param(params, 0).unwrap_or(1).max(1)
}

fn first_param_or_zero(params: &vte::Params) -> usize {
    nth_param(params, 0).unwrap_or(0)
}

fn second_param(params: &vte::Params) -> usize {
    nth_param(params, 1).unwrap_or(1).max(1)
}

fn nth_param(params: &vte::Params, index: usize) -> Option<usize> {
    params
        .iter()
        .nth(index)
        .and_then(|param| param.first().copied())
        .map(usize::from)
}

#[derive(Debug, Clone, Default)]
struct TerminalLine {
    cells: Vec<char>,
}

impl TerminalLine {
    fn put(&mut self, column: usize, ch: char) {
        if self.cells.len() <= column {
            self.cells.resize(column.saturating_add(1), ' ');
        }
        self.cells[column] = ch;
    }

    fn clear(&mut self) {
        self.cells.clear();
    }

    fn clear_to_right(&mut self, column: usize) {
        if column < self.cells.len() {
            self.cells.truncate(column);
        }
    }

    fn clear_to_left(&mut self, column: usize) {
        let end = column.saturating_add(1).min(self.cells.len());
        for cell in self.cells.iter_mut().take(end) {
            *cell = ' ';
        }
    }

    fn text(&self) -> String {
        let mut end = self.cells.len();
        while end > 0 && self.cells[end - 1] == ' ' {
            end = end.saturating_sub(1);
        }
        self.cells.iter().take(end).collect()
    }

    fn is_empty(&self) -> bool {
        self.cells.iter().all(|cell| *cell == ' ')
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
    fn wrapping_at_last_column_scrolls_viewport() {
        let summary = clean_with_size(&[b"abcdef"], 3, 2);

        assert_eq!(summary.tail, "abc\ndef\n");
    }
}
