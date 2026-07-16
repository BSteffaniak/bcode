//! Generic terminal transcript viewer component for Bcode TUI presentations.

use bmux_terminal_grid::{
    Color as GridColor, GridLimits, PhysicalRow, Style as GridStyle, TerminalGrid,
    TerminalGridStream,
};
use bmux_tui::ansi::ansi_to_lines;
use bmux_tui::prelude::{Color, Line, Span, Style};
use bmux_tui::style::Modifier;

/// Default maximum number of terminal rows rendered inline.
pub const MAX_INLINE_TERMINAL_ROWS: usize = 28;

/// Stateful sizing policy for live terminal previews.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalViewerLiveState {
    visible_rows: usize,
}

impl TerminalViewerLiveState {
    /// Return the currently reserved live terminal rows.
    #[must_use]
    pub const fn visible_rows(self) -> usize {
        self.visible_rows
    }

    /// Grow the reserved live terminal rows from an already-decoded row count.
    pub fn update_rows(&mut self, content_rows: usize, max_rows: usize) {
        self.visible_rows = self.visible_rows.max(content_rows).min(max_rows);
    }

    /// Grow the reserved live terminal rows to fit `input`, capped by `max_rows`.
    pub fn update(&mut self, input: TerminalViewerInput<'_>, max_rows: usize) {
        let content_rows = terminal_viewer_content_row_count(input, max_rows);
        self.update_rows(content_rows, max_rows);
    }
}

/// Terminal row sizing policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalViewerSizing {
    /// Render only current compact transcript content.
    Compact,
    /// Render a live preview with a stable, caller-managed row reservation.
    Live {
        visible_rows: usize,
        max_rows: usize,
    },
}

/// Input used to render terminal transcript rows.
#[derive(Debug, Clone, Copy)]
pub struct TerminalViewerInput<'a> {
    /// Raw terminal stream output.
    pub output: &'a str,
    /// Terminal columns used when capturing the stream.
    pub columns: u16,
    /// Terminal rows used when capturing the stream.
    pub rows: u16,
    /// Process exit code, when known.
    pub exit_code: Option<i32>,
    /// Whether execution timed out, when known.
    pub timed_out: Option<bool>,
    /// Human-readable elapsed duration, when known.
    pub elapsed: Option<&'a str>,
    /// Whether to render a status summary before terminal rows.
    pub show_status: bool,
    /// Whether earlier output was omitted.
    pub output_truncated: bool,
    /// Original output byte length, when known.
    pub output_bytes: Option<u64>,
    /// Retained output byte length, when known.
    pub retained_output_bytes: Option<u64>,
    /// Terminal row sizing policy.
    pub sizing: TerminalViewerSizing,
}

/// Render terminal transcript rows using terminal-grid semantics.
#[must_use]
pub fn terminal_viewer_rows(input: TerminalViewerInput<'_>, width: u16) -> Vec<Line> {
    let mut rows = Vec::new();
    if input.show_status {
        push_wrapped_styled_text(
            &mut rows,
            vec![Span::styled("  ", muted_style())],
            &terminal_status(&input),
            width,
            terminal_status_style(&input),
            muted_style(),
        );
    }
    if input.output_truncated {
        push_wrapped_styled_text(
            &mut rows,
            vec![Span::styled("  ", muted_style())],
            &terminal_truncation_status(&input),
            width,
            muted_style(),
            muted_style(),
        );
    }
    for line in terminal_output_lines(&input) {
        rows.push(prefix_line(line, "    ", muted_style()));
    }
    rows
}

fn terminal_status(input: &TerminalViewerInput<'_>) -> String {
    let status = if input.timed_out.unwrap_or(false) {
        "timed out".to_owned()
    } else if let Some(exit_code) = input.exit_code {
        if exit_code == 0 {
            "completed".to_owned()
        } else {
            format!("failed · exit {exit_code}")
        }
    } else {
        "running".to_owned()
    };
    input.elapsed.map_or_else(
        || status.clone(),
        |elapsed| format!("{status} · duration {elapsed}"),
    )
}

fn terminal_status_style(input: &TerminalViewerInput<'_>) -> Style {
    if input.timed_out.unwrap_or(false) || input.exit_code.is_some_and(|code| code != 0) {
        Style::new().fg(Color::Red)
    } else {
        muted_style()
    }
}

fn terminal_truncation_status(input: &TerminalViewerInput<'_>) -> String {
    match (input.retained_output_bytes, input.output_bytes) {
        (Some(retained), Some(original)) => {
            format!("output truncated · showing {retained} of {original} bytes")
        }
        _ => "output truncated".to_owned(),
    }
}

fn terminal_output_lines(input: &TerminalViewerInput<'_>) -> Vec<Line> {
    let Ok(mut stream) = TerminalGridStream::new(
        input.columns.max(1),
        input.rows.max(1),
        GridLimits {
            scrollback_rows: MAX_INLINE_TERMINAL_ROWS.saturating_mul(8),
        },
    ) else {
        return ansi_to_lines(input.output);
    };
    stream.process(input.output.as_bytes());
    let grid = stream.grid();
    let max_rows = match input.sizing {
        TerminalViewerSizing::Compact => MAX_INLINE_TERMINAL_ROWS,
        TerminalViewerSizing::Live { max_rows, .. } => max_rows,
    };
    let rows = grid.main_content_tail_rows(max_rows);
    let mut lines = rows
        .iter()
        .map(|row| terminal_grid_row_to_line(grid, row))
        .collect::<Vec<_>>();
    match input.sizing {
        TerminalViewerSizing::Compact => preview_lines(&lines, max_rows)
            .into_iter()
            .cloned()
            .collect(),
        TerminalViewerSizing::Live {
            visible_rows,
            max_rows,
        } => {
            let target_rows = visible_rows.max(1).min(max_rows);
            if lines.len() > target_rows {
                lines = lines[lines.len().saturating_sub(target_rows)..].to_vec();
            }
            while lines.len() < target_rows {
                lines.push(Line::default());
            }
            lines
        }
    }
}

fn terminal_viewer_content_row_count(input: TerminalViewerInput<'_>, max_rows: usize) -> usize {
    let Ok(mut stream) = TerminalGridStream::new(
        input.columns.max(1),
        input.rows.max(1),
        GridLimits {
            scrollback_rows: max_rows.saturating_mul(8),
        },
    ) else {
        return ansi_to_lines(input.output).len().max(1).min(max_rows);
    };
    stream.process(input.output.as_bytes());
    stream.grid().main_content_tail_rows(max_rows).len().max(1)
}

fn terminal_grid_row_to_line(grid: &TerminalGrid, row: &PhysicalRow) -> Line {
    let mut spans = Vec::new();
    let mut current_style = None;
    let mut current_text = String::new();
    for cell in row.cells() {
        if cell.is_wide_continuation() {
            continue;
        }
        let style = terminal_grid_style(grid.palette().get(cell.style()));
        if current_style == Some(style) {
            current_text.push_str(cell.text());
            continue;
        }
        if !current_text.is_empty() {
            spans.push(Span::styled(
                current_text,
                current_style.unwrap_or_default(),
            ));
            current_text = String::new();
        }
        current_style = Some(style);
        current_text.push_str(cell.text());
    }
    if !current_text.is_empty() {
        spans.push(Span::styled(
            current_text,
            current_style.unwrap_or_default(),
        ));
    }
    Line::from_spans(spans)
}

const fn terminal_grid_style(style: GridStyle) -> Style {
    let mut output = Style::new();
    if let Some(fg) = style.fg {
        output = output.fg(terminal_grid_color(fg));
    }
    if let Some(bg) = style.bg {
        output = output.bg(terminal_grid_color(bg));
    }
    if style.bold {
        output = output.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        output = output.add_modifier(Modifier::ITALIC);
    }
    if style.underline {
        output = output.add_modifier(Modifier::UNDERLINE);
    }
    if style.dim {
        output = output.add_modifier(Modifier::DIM);
    }
    if style.inverse {
        output = output.add_modifier(Modifier::REVERSED);
    }
    if style.strike {
        output = output.add_modifier(Modifier::CROSSED_OUT);
    }
    output
}

const fn terminal_grid_color(color: GridColor) -> Color {
    match color {
        GridColor::Indexed(index) => ansi_indexed_color(index),
        GridColor::Rgb { r, g, b } => Color::Rgb(r, g, b),
    }
}

const fn ansi_indexed_color(index: u8) -> Color {
    match index {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::White,
        8 => Color::BrightBlack,
        9 => Color::BrightRed,
        10 => Color::BrightGreen,
        11 => Color::BrightYellow,
        12 => Color::BrightBlue,
        13 => Color::BrightMagenta,
        14 => Color::BrightCyan,
        15 => Color::BrightWhite,
        other => Color::Indexed(other),
    }
}

fn preview_lines<T>(lines: &[T], max_rows: usize) -> Vec<&T> {
    lines.iter().take(max_rows).collect()
}

fn prefix_line(mut line: Line, prefix: &str, style: Style) -> Line {
    line.spans.insert(0, Span::styled(prefix.to_owned(), style));
    line
}

const fn muted_style() -> Style {
    Style::new().fg(Color::BrightBlack)
}

fn push_wrapped_styled_text(
    rows: &mut Vec<Line>,
    prefix: Vec<Span>,
    text: &str,
    width: u16,
    first_style: Style,
    rest_style: Style,
) {
    let available = usize::from(width)
        .saturating_sub(line_display_width(&prefix))
        .max(1);
    let mut first = true;
    for source_line in text.lines() {
        let wrapped = wrap_text(source_line, available);
        for segment in wrapped {
            let mut spans = if first {
                prefix.clone()
            } else {
                vec![Span::styled(
                    " ".repeat(line_display_width(&prefix)),
                    rest_style,
                )]
            };
            spans.push(Span::styled(
                segment,
                if first { first_style } else { rest_style },
            ));
            rows.push(Line::from_spans(spans));
            first = false;
        }
    }
    if text.is_empty() {
        rows.push(Line::from_spans(prefix));
    }
}

fn line_display_width(spans: &[Span]) -> usize {
    spans.iter().map(|span| span.content.chars().count()).sum()
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in text.chars() {
        if current_width >= width {
            rows.push(current);
            current = String::new();
            current_width = 0;
        }
        current.push(ch);
        current_width = current_width.saturating_add(1);
    }
    if !current.is_empty() {
        rows.push(current);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered_text(rows: &[Line]) -> String {
        rows.iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn terminal_viewer_interprets_carriage_return() {
        let rows = terminal_viewer_rows(
            TerminalViewerInput {
                output: "first\rsecond\n",
                columns: 80,
                rows: 24,
                exit_code: Some(0),
                timed_out: Some(false),
                elapsed: None,
                output_truncated: false,
                output_bytes: Some(13),
                retained_output_bytes: Some(13),
                show_status: true,
                sizing: TerminalViewerSizing::Compact,
            },
            100,
        );
        let rendered = rendered_text(&rows);

        assert!(rendered.contains("second"), "{rendered}");
        assert!(!rendered.contains("first"), "{rendered}");
    }

    #[test]
    fn live_terminal_state_grows_but_does_not_shrink() {
        let mut state = TerminalViewerLiveState::default();
        let one_line = TerminalViewerInput {
            output: "one\n",
            columns: 80,
            rows: 24,
            exit_code: None,
            timed_out: None,
            elapsed: None,
            output_truncated: false,
            output_bytes: None,
            retained_output_bytes: None,
            show_status: false,
            sizing: TerminalViewerSizing::Compact,
        };
        state.update(one_line, 28);
        assert_eq!(state.visible_rows(), 1);

        let three_lines = TerminalViewerInput {
            output: "one\ntwo\nthree\n",
            ..one_line
        };
        state.update(three_lines, 28);
        assert_eq!(state.visible_rows(), 3);

        state.update(one_line, 28);
        assert_eq!(state.visible_rows(), 3);
    }

    #[test]
    fn live_terminal_rows_pad_to_reserved_height() {
        let rows = terminal_viewer_rows(
            TerminalViewerInput {
                output: "one\n",
                columns: 80,
                rows: 24,
                exit_code: None,
                timed_out: None,
                elapsed: None,
                output_truncated: false,
                output_bytes: None,
                retained_output_bytes: None,
                show_status: false,
                sizing: TerminalViewerSizing::Live {
                    visible_rows: 3,
                    max_rows: 28,
                },
            },
            100,
        );

        assert_eq!(rows.len(), 3);
        assert!(rendered_text(&rows).contains("one"));
    }
}
