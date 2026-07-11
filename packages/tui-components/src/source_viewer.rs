//! Shared source-code card and gutter rendering for TUI presentations.

use bmux_tui::prelude::{Color, Line, Span, Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[cfg(feature = "syntax")]
use bcode_syntax_render::{SyntaxHighlighter, SyntaxStyle};

/// Input used to render a source viewer card.
#[derive(Debug, Clone, Copy)]
pub struct SourceViewerInput<'a> {
    /// Path or language hint used for syntax highlighting.
    pub label: &'a str,
    /// Source text to display.
    pub contents: &'a str,
    /// Absolute, one-based number of the first source line.
    pub start_line: usize,
    /// Maximum number of logical source lines to display.
    pub max_lines: usize,
    /// Message displayed when logical lines are omitted.
    pub truncated_message: &'a str,
    /// Whether to display line numbers.
    pub line_numbers: bool,
}

/// Render source text using the same card and gutter language as the diff viewer.
#[must_use]
pub fn source_viewer_rows(input: SourceViewerInput<'_>, width: u16) -> Vec<Line> {
    let lines = input.contents.lines().collect::<Vec<_>>();
    let displayed = lines.len().min(input.max_lines);
    let last_line = input.start_line.saturating_add(displayed.saturating_sub(1));
    let number_width = if input.line_numbers {
        last_line.to_string().len().max(1)
    } else {
        0
    };
    let body_width = usize::from(width)
        .saturating_sub(number_width)
        .saturating_sub(10)
        .max(1);
    let highlighted = highlight_lines(input.label, &lines[..displayed]);
    let mut rows = Vec::new();
    rows.push(card_border(width, "┌", "┐"));
    for (index, spans) in highlighted.into_iter().enumerate() {
        let chunks = wrap_spans(spans, body_width);
        for (chunk_index, chunk) in chunks.into_iter().enumerate() {
            let number = (chunk_index == 0 && input.line_numbers)
                .then(|| input.start_line.saturating_add(index));
            rows.push(source_card_row(chunk, number, number_width, width));
        }
    }
    if lines.len() > displayed {
        rows.push(source_card_row(
            vec![Span::styled(input.truncated_message, muted_style())],
            None,
            number_width,
            width,
        ));
    }
    rows.push(card_border(width, "└", "┘"));
    rows
}

fn source_card_row(
    content: Vec<Span>,
    line_number: Option<usize>,
    number_width: usize,
    width: u16,
) -> Line {
    let gutter = gutter_style();
    let mut card = vec![Span::styled("│ ", muted_style())];
    if number_width > 0 {
        card.push(Span::styled(
            line_number.map_or_else(
                || " ".repeat(number_width),
                |number| format!("{number:>number_width$}"),
            ),
            gutter,
        ));
        card.push(Span::styled(" │ ", gutter));
    }
    card.extend(content);
    pad_card_spans(
        &mut card,
        usize::from(width).saturating_sub(2),
        Style::new(),
    );
    card.push(Span::styled(" │", muted_style()));
    Line::from_spans(
        std::iter::once(Span::styled("  ", muted_style()))
            .chain(card)
            .collect::<Vec<_>>(),
    )
}

fn card_border(width: u16, left: &str, right: &str) -> Line {
    let inner = usize::from(width).saturating_sub(4);
    Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled(left, muted_style()),
        Span::styled("─".repeat(inner), muted_style()),
        Span::styled(right, muted_style()),
    ])
}

fn wrap_spans(spans: Vec<Span>, width: usize) -> Vec<Vec<Span>> {
    let mut rows = vec![Vec::new()];
    let mut used = 0usize;
    for span in spans {
        for grapheme in span.content.graphemes(true) {
            let cell_width = UnicodeWidthStr::width(grapheme);
            if used > 0 && used.saturating_add(cell_width) > width {
                rows.push(Vec::new());
                used = 0;
            }
            rows.last_mut()
                .expect("source row")
                .push(Span::styled(grapheme, span.style));
            used = used.saturating_add(cell_width);
        }
    }
    rows
}

#[cfg(feature = "syntax")]
fn highlight_lines(hint: &str, lines: &[&str]) -> Vec<Vec<Span>> {
    let highlighter = SyntaxHighlighter::new();
    if !highlighter.can_highlight(hint) {
        return lines
            .iter()
            .map(|line| vec![Span::raw((*line).to_owned())])
            .collect();
    }
    highlighter
        .highlight_lines_tokens(hint, lines)
        .into_iter()
        .map(|line| {
            line.into_iter()
                .map(|span| Span::styled(span.content, syntax_style(span.style)))
                .collect()
        })
        .collect()
}

#[cfg(not(feature = "syntax"))]
fn highlight_lines(_hint: &str, lines: &[&str]) -> Vec<Vec<Span>> {
    lines
        .iter()
        .map(|line| vec![Span::raw((*line).to_owned())])
        .collect()
}

#[cfg(feature = "syntax")]
const fn syntax_style(style: SyntaxStyle) -> Style {
    let mut output = Style::new().fg(Color::Rgb(
        style.foreground_r,
        style.foreground_g,
        style.foreground_b,
    ));
    if style.bold {
        output = output.add_modifier(bmux_tui::prelude::Modifier::BOLD);
    }
    output
}

pub(crate) const fn muted_style() -> Style {
    Style::new().fg(Color::BrightBlack)
}

pub(crate) const fn gutter_style() -> Style {
    Style::new()
        .fg(Color::BrightBlack)
        .bg(Color::Rgb(30, 30, 30))
}

pub(crate) fn pad_card_spans(spans: &mut Vec<Span>, target_width: usize, style: Style) {
    let current_width = spans_width(spans);
    if current_width < target_width {
        spans.push(Span::styled(
            " ".repeat(target_width - current_width),
            style,
        ));
    }
}

fn spans_width(spans: &[Span]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_str()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(rows: &[Line]) -> String {
        rows.iter()
            .map(|row| {
                row.spans
                    .iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_absolute_aligned_line_numbers() {
        let rows = source_viewer_rows(
            SourceViewerInput {
                label: "file.rs",
                contents: "nine\nten",
                start_line: 9,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: true,
            },
            40,
        );
        let output = rendered(&rows);
        assert!(output.contains(" 9 │ nine"), "{output}");
        assert!(output.contains("10 │ ten"), "{output}");
    }

    #[test]
    fn supports_unnumbered_source_cards() {
        let output = rendered(&source_viewer_rows(
            SourceViewerInput {
                label: "artifact",
                contents: "content",
                start_line: 1,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: false,
            },
            40,
        ));
        assert!(!output.contains("1 │"), "{output}");
    }
}
