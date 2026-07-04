//! Streaming terminal-output normalization for shell model artifacts.

use std::collections::VecDeque;
use std::io::{self, Write};

const DEFAULT_RETAINED_LINES: usize = 64;
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
    retained_rows: usize,
    lines: VecDeque<TerminalLine>,
    cursor_row: usize,
    cursor_col: usize,
    tail: ByteTail,
    bytes_written: u64,
    error: Option<io::Error>,
}

impl<W: Write> TerminalCleanPerformer<W> {
    fn new(writer: W, columns: u16, rows: u16, max_tail_bytes: usize) -> Self {
        let retained_rows = usize::from(rows)
            .saturating_add(DEFAULT_RETAINED_LINES)
            .max(1);
        let mut lines = VecDeque::new();
        lines.push_back(TerminalLine::default());
        Self {
            writer,
            columns: usize::from(columns).max(1),
            retained_rows,
            lines,
            cursor_row: 0,
            cursor_col: 0,
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
            self.cursor_row = self.cursor_row.saturating_add(1);
            self.ensure_cursor_line();
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
        self.cursor_row = self.cursor_row.saturating_add(1);
        self.cursor_col = 0;
        self.ensure_cursor_line();
        self.flush_stable_lines();
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
        self.cursor_row = self.cursor_row.saturating_add(count.max(1));
        self.ensure_cursor_line();
        self.flush_stable_lines();
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
        self.cursor_row = row.saturating_sub(1);
        self.cursor_col = column.saturating_sub(1).min(self.columns.saturating_sub(1));
        self.ensure_cursor_line();
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
        while self.cursor_row >= self.lines.len() {
            self.lines.push_back(TerminalLine::default());
        }
    }

    fn flush_stable_lines(&mut self) {
        while self.lines.len() > self.retained_rows {
            if let Some(line) = self.lines.pop_front() {
                let result = self.write_line(&line);
                self.record_result(result);
                self.cursor_row = self.cursor_row.saturating_sub(1);
            }
        }
    }

    fn flush_all(&mut self) -> io::Result<()> {
        while self.lines.len() > 1 {
            if let Some(line) = self.lines.pop_front() {
                self.write_line(&line)?;
            }
        }
        if let Some(line) = self.lines.pop_front()
            && !line.is_empty()
        {
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
            'J' => self.erase_display(first.saturating_sub(1)),
            'K' => self.erase_line(first.saturating_sub(1)),
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
            b'M' => self.cursor_up(1),
            b'c' => self.erase_display(2),
            _ => {}
        }
    }
}

fn first_param(params: &vte::Params) -> usize {
    nth_param(params, 0).unwrap_or(1).max(1)
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

    #[test]
    fn tail_is_bounded() {
        let mut output = Vec::new();
        let mut cleaner = TerminalCleanWriter::new(&mut output, 80, 24, 6);
        cleaner.write_chunk(b"alpha\nbeta\n").expect("chunk cleans");
        let summary = cleaner.finish().expect("cleaner finishes");

        assert!(summary.tail_truncated);
        assert_eq!(summary.tail, "\nbeta\n");
    }
}
