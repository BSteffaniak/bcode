//! Bcode syntax adapter for the generic BMUX source viewer.

#[cfg(feature = "syntax")]
use bcode_syntax_render::{SyntaxHighlighter, SyntaxPalette, SyntaxStyle};
use bmux_tui::prelude::{Line, Span};
#[cfg(feature = "syntax")]
use bmux_tui::prelude::{Modifier, Style};

pub use bmux_tui_components::source_viewer::SourceViewerStyle;

/// Derive generic source-viewer styles from a component theme.
#[must_use]
pub fn source_viewer_style(theme: bmux_tui_components::theme::ComponentTheme) -> SourceViewerStyle {
    theme.into()
}

/// Input used to render a Bcode source viewer card.
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

/// Render source text using the generic BMUX source viewer.
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
    let styled_lines = highlighted_lines(input);
    bmux_tui_components::source_viewer::source_viewer_rows_with_style(
        bmux_tui_components::source_viewer::SourceViewerInput {
            label: input.label,
            styled_lines: Some(&styled_lines),
            contents: input.contents,
            start_line: input.start_line,
            max_lines: input.max_lines,
            truncated_message: input.truncated_message,
            line_numbers: input.line_numbers,
        },
        width,
        style,
    )
}

fn highlighted_lines(input: SourceViewerInput<'_>) -> Vec<Line> {
    #[cfg(feature = "syntax")]
    {
        let lines = input
            .contents
            .lines()
            .take(input.max_lines)
            .collect::<Vec<_>>();
        let highlighter = input
            .syntax_palette
            .map_or_else(SyntaxHighlighter::new, SyntaxHighlighter::with_palette);
        if highlighter.can_highlight(input.label) {
            return highlighter
                .highlight_lines_tokens(input.label, &lines)
                .into_iter()
                .map(|line| {
                    Line::from_spans(
                        line.into_iter()
                            .map(|span| Span::styled(span.content, syntax_style(span.style)))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
        }
        return lines
            .into_iter()
            .map(|line| Line::from_spans(vec![Span::raw(line.to_owned())]))
            .collect();
    }
    #[cfg(not(feature = "syntax"))]
    input
        .contents
        .lines()
        .take(input.max_lines)
        .map(|line| Line::from_spans(vec![Span::raw(line.to_owned())]))
        .collect()
}

#[cfg(feature = "syntax")]
const fn syntax_style(style: SyntaxStyle) -> Style {
    let mut output = Style::new().fg(style.foreground.to_tui());
    if style.bold {
        output = output.add_modifier(Modifier::BOLD);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "syntax")]
    #[test]
    fn semantic_palette_reaches_generic_source_rows() {
        use bcode_syntax_render::SyntaxColor;
        use bmux_tui::prelude::Color;

        let color = SyntaxColor::rgb(7, 8, 9);
        let palette = SyntaxPalette {
            text: color,
            comment: color,
            keyword: color,
            function: color,
            variable: color,
            string: color,
            number: color,
            type_name: color,
            operator: color,
            punctuation: color,
        };
        let rows = source_viewer_rows(
            SourceViewerInput {
                label: "file.rs",
                syntax_palette: Some(palette),
                contents: "fn main() {}",
                start_line: 1,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: true,
            },
            80,
        );

        assert!(
            rows.iter()
                .flat_map(|row| &row.spans)
                .any(|span| { span.style.fg == Some(Color::Rgb(7, 8, 9)) })
        );
    }

    #[test]
    fn theme_less_unknown_source_remains_visible() {
        let rows = source_viewer_rows(
            SourceViewerInput {
                label: "file.unknown",
                #[cfg(feature = "syntax")]
                syntax_palette: None,
                contents: "plain text",
                start_line: 1,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: false,
            },
            40,
        );
        let rendered = rows
            .iter()
            .flat_map(|row| &row.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(rendered.contains("plain text"), "{rendered}");
    }
}
