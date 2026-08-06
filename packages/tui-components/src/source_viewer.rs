//! Shared source-code card and gutter rendering for TUI presentations.

use bmux_tui::prelude::{Color, Line, Span, Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[cfg(feature = "syntax")]
use bcode_syntax_render::{SyntaxHighlighter, SyntaxPalette, SyntaxStyle};

/// Input used to render a source viewer card.
#[derive(Debug, Clone, Copy)]
pub struct SourceViewerInput<'a> {
    /// Path or language hint used for syntax highlighting.
    pub label: &'a str,
    /// Optional semantic syntax palette.
    #[cfg(feature = "syntax")]
    pub syntax_palette: Option<SyntaxPalette>,
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

const SOURCE_CARD_MIN_WIDTH: usize = 16;
const SOURCE_CARD_UNNUMBERED_CHROME_WIDTH: usize = 4;
const SOURCE_CARD_NUMBERED_CHROME_WIDTH: usize = 7;

/// Semantic styles used by source viewer cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceViewerStyle {
    /// Base style patched beneath source and syntax token styles.
    pub source: Style,
    /// Card border style.
    pub border: Style,
    /// Line-number gutter style.
    pub gutter: Style,
    /// Omission and truncation message style.
    pub truncated: Style,
}

impl Default for SourceViewerStyle {
    fn default() -> Self {
        let muted = Style::new().fg(Color::BrightBlack);
        Self {
            source: Style::new(),
            border: muted,
            gutter: muted,
            truncated: muted,
        }
    }
}

/// Render source text using the same card and gutter language as the diff viewer.
#[must_use]
pub fn source_viewer_rows(input: SourceViewerInput<'_>, width: u16) -> Vec<Line> {
    source_viewer_rows_with_style(input, width, SourceViewerStyle::default())
}

/// Render source text with caller-supplied semantic card styles.
#[must_use]
pub fn source_viewer_rows_with_style(
    input: SourceViewerInput<'_>,
    width: u16,
    style: SourceViewerStyle,
) -> Vec<Line> {
    let lines = input.contents.lines().collect::<Vec<_>>();
    let displayed = lines.len().min(input.max_lines);
    let last_line = input.start_line.saturating_add(displayed.saturating_sub(1));
    let number_width = if input.line_numbers {
        last_line.to_string().len().max(1)
    } else {
        0
    };
    let available_width = width.saturating_sub(2);
    let card_width = source_card_width(
        &lines[..displayed],
        (lines.len() > displayed).then_some(input.truncated_message),
        number_width,
        available_width,
    );
    let body_width = usize::from(card_width)
        .saturating_sub(source_card_chrome_width(number_width))
        .max(1);
    let highlighted = {
        #[cfg(feature = "syntax")]
        {
            highlight_lines(input.label, &lines[..displayed], input.syntax_palette)
        }
        #[cfg(not(feature = "syntax"))]
        {
            highlight_lines(input.label, &lines[..displayed])
        }
    };
    let mut rows = Vec::new();
    rows.push(card_border(card_width, "┌", "┐", style.border));
    for (index, spans) in highlighted.into_iter().enumerate() {
        let spans = spans
            .into_iter()
            .map(|span| Span::styled(span.content, style.source.patch(span.style)))
            .collect();
        let chunks = wrap_spans(spans, body_width);
        for (chunk_index, chunk) in chunks.into_iter().enumerate() {
            let number = (chunk_index == 0 && input.line_numbers)
                .then(|| input.start_line.saturating_add(index));
            rows.push(source_card_row(
                chunk,
                number,
                number_width,
                card_width,
                style,
            ));
        }
    }
    if lines.len() > displayed {
        rows.push(source_card_row(
            vec![Span::styled(input.truncated_message, style.truncated)],
            None,
            number_width,
            card_width,
            style,
        ));
    }
    rows.push(card_border(card_width, "└", "┘", style.border));
    rows
}

const fn source_card_chrome_width(number_width: usize) -> usize {
    if number_width == 0 {
        SOURCE_CARD_UNNUMBERED_CHROME_WIDTH
    } else {
        number_width.saturating_add(SOURCE_CARD_NUMBERED_CHROME_WIDTH)
    }
}

fn source_card_width(
    lines: &[&str],
    truncated_message: Option<&str>,
    number_width: usize,
    available_width: u16,
) -> u16 {
    let available = usize::from(available_width.max(1));
    let content_width = lines
        .iter()
        .map(|line| UnicodeWidthStr::width(*line))
        .chain(truncated_message.map(UnicodeWidthStr::width))
        .max()
        .unwrap_or(0);
    let desired = content_width.saturating_add(source_card_chrome_width(number_width));
    u16::try_from(desired.clamp(SOURCE_CARD_MIN_WIDTH.min(available), available))
        .unwrap_or(u16::MAX)
}

fn source_card_row(
    content: Vec<Span>,
    line_number: Option<usize>,
    number_width: usize,
    width: u16,
    style: SourceViewerStyle,
) -> Line {
    let gutter = style.gutter;
    let mut card = vec![Span::styled("│ ", style.border)];
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
        style.source,
    );
    card.push(Span::styled(" │", style.border));
    Line::from_spans(
        std::iter::once(Span::styled("  ", style.border))
            .chain(card)
            .collect::<Vec<_>>(),
    )
}

fn card_border(width: u16, left: &str, right: &str, style: Style) -> Line {
    let inner = usize::from(width.saturating_sub(2));
    Line::from_spans(vec![
        Span::styled("  ", style),
        Span::styled(left, style),
        Span::styled("─".repeat(inner), style),
        Span::styled(right, style),
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
fn highlight_lines(hint: &str, lines: &[&str], palette: Option<SyntaxPalette>) -> Vec<Vec<Span>> {
    let highlighter = palette.map_or_else(SyntaxHighlighter::new, SyntaxHighlighter::with_palette);
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
    let mut output = Style::new().fg(style.foreground.to_tui());
    if style.bold {
        output = output.add_modifier(bmux_tui::prelude::Modifier::BOLD);
    }
    output
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
                #[cfg(feature = "syntax")]
                syntax_palette: None,
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
    fn rows_fit_available_width_and_keep_right_border() {
        let width = 24;
        let rows = source_viewer_rows(
            SourceViewerInput {
                #[cfg(feature = "syntax")]
                syntax_palette: None,
                label: "file.rs",
                contents: "a source line long enough to wrap",
                start_line: 42,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: true,
            },
            width,
        );

        for row in &rows {
            let text = row
                .spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert!(UnicodeWidthStr::width(text.as_str()) <= usize::from(width));
            assert!(text.ends_with('│') || text.ends_with('┐') || text.ends_with('┘'));
        }
    }

    #[test]
    fn short_source_uses_content_sized_card() {
        let rows = source_viewer_rows(
            SourceViewerInput {
                #[cfg(feature = "syntax")]
                syntax_palette: None,
                label: "file.rs",
                contents: "let x = 1;",
                start_line: 1,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: true,
            },
            100,
        );

        assert!(line_width(&rows[0]) < 100, "{rows:?}");
        assert!(
            rows.iter()
                .all(|row| line_width(row) == line_width(&rows[0]))
        );
    }

    #[test]
    fn omitted_long_lines_do_not_expand_source_card() {
        let rows = source_viewer_rows(
            SourceViewerInput {
                #[cfg(feature = "syntax")]
                syntax_palette: None,
                label: "file.rs",
                contents: "short\nthis omitted line is intentionally extremely long and should not size the card",
                start_line: 1,
                max_lines: 1,
                truncated_message: "truncated",
                line_numbers: true,
            },
            100,
        );

        assert!(line_width(&rows[0]) < 40, "{rows:?}");
    }

    #[test]
    fn unicode_source_width_uses_terminal_cells() {
        let rows = source_viewer_rows(
            SourceViewerInput {
                #[cfg(feature = "syntax")]
                syntax_palette: None,
                label: "file.txt",
                contents: "界界",
                start_line: 1,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: false,
            },
            100,
        );

        assert_eq!(line_width(&rows[0]), SOURCE_CARD_MIN_WIDTH + 2);
    }

    fn line_width(line: &Line) -> usize {
        line.spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_str()))
            .sum()
    }

    #[test]
    fn applies_caller_supplied_source_card_styles() {
        let custom = Style::new().fg(Color::Magenta).bg(Color::Blue);
        let rows = source_viewer_rows_with_style(
            SourceViewerInput {
                #[cfg(feature = "syntax")]
                syntax_palette: None,
                label: "file.txt",
                contents: "content",
                start_line: 1,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: true,
            },
            40,
            SourceViewerStyle {
                source: custom,
                border: custom,
                gutter: custom,
                truncated: custom,
            },
        );

        assert!(
            rows.iter()
                .flat_map(|row| &row.spans)
                .all(|span| { span.style.bg == Some(Color::Blue) })
        );
        assert!(
            rows.first()
                .into_iter()
                .flat_map(|row| &row.spans)
                .all(|span| span.style.fg == Some(Color::Magenta))
        );
    }

    #[test]
    fn supports_unnumbered_source_cards() {
        let output = rendered(&source_viewer_rows(
            SourceViewerInput {
                #[cfg(feature = "syntax")]
                syntax_palette: None,
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
