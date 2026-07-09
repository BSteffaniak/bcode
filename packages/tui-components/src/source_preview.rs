//! Source-code preview rendering helpers for Bcode TUI surfaces.

use bmux_tui::prelude::{Color, Line, Span, Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[cfg(feature = "syntax")]
use bmux_tui::prelude::Modifier;

#[cfg(feature = "syntax")]
use bcode_syntax_render::{SyntaxHighlighter, SyntaxStyle};

/// Maximum number of source lines rendered by [`source_preview_lines`].
pub const DEFAULT_SOURCE_PREVIEW_MAX_LINES: usize = 30;

/// Options for rendering a source-code preview block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePreviewOptions<'a> {
    /// Path or language hint used to select a syntax highlighter.
    pub syntax_hint: &'a str,
    /// Available terminal width in cells.
    pub width: u16,
    /// Maximum number of source lines to render.
    pub max_lines: usize,
    /// Prefix shown before every source line.
    pub line_prefix: &'a str,
    /// Style applied to every source-line prefix.
    pub prefix_style: Style,
    /// Base style patched beneath syntax-token styles.
    pub source_style: Style,
    /// Text shown when hidden lines remain after the preview.
    pub truncated_message: &'a str,
    /// Style applied to the truncation message.
    pub truncated_style: Style,
}

impl<'a> SourcePreviewOptions<'a> {
    /// Create source preview options with Bcode's standard source block chrome.
    #[must_use]
    pub const fn new(syntax_hint: &'a str, width: u16) -> Self {
        Self {
            syntax_hint,
            width,
            max_lines: DEFAULT_SOURCE_PREVIEW_MAX_LINES,
            line_prefix: "  │ ",
            prefix_style: Style::new().fg(Color::BrightBlack),
            source_style: Style::new(),
            truncated_message: "  … preview truncated",
            truncated_style: Style::new().fg(Color::BrightBlack),
        }
    }

    /// Set the maximum rendered source lines.
    #[must_use]
    pub const fn max_lines(mut self, max_lines: usize) -> Self {
        self.max_lines = max_lines;
        self
    }

    /// Set the source line prefix and style.
    #[must_use]
    pub const fn line_prefix(mut self, prefix: &'a str, style: Style) -> Self {
        self.line_prefix = prefix;
        self.prefix_style = style;
        self
    }

    /// Set the base source text style.
    #[must_use]
    pub const fn source_style(mut self, style: Style) -> Self {
        self.source_style = style;
        self
    }

    /// Set the truncation message and style.
    #[must_use]
    pub const fn truncated_message(mut self, message: &'a str, style: Style) -> Self {
        self.truncated_message = message;
        self.truncated_style = style;
        self
    }
}

/// Render a bounded source-code preview.
///
/// The renderer highlights only the source lines that will be displayed,
/// preserves syntax state across those lines, and clips styled spans after
/// highlighting so horizontal truncation does not affect tokenization.
/// It never reads beyond `options.max_lines + 1` lines from `contents`.
#[must_use]
pub fn source_preview_lines(contents: &str, options: &SourcePreviewOptions<'_>) -> Vec<Line> {
    let max_width = preview_width(options.width, options.line_prefix);
    let mut display_lines = Vec::new();
    let mut truncated = false;

    for (index, line) in contents.lines().enumerate() {
        if index >= options.max_lines {
            truncated = true;
            break;
        }
        display_lines.push(line.to_owned());
    }

    let highlighted_lines = highlight_lines(options.syntax_hint, &display_lines);
    let mut rows = highlighted_lines
        .into_iter()
        .map(|spans| preview_line(spans, options, max_width))
        .collect::<Vec<_>>();

    if truncated {
        rows.push(Line::from_spans(vec![Span::styled(
            options.truncated_message.to_owned(),
            options.truncated_style,
        )]));
    }

    rows
}

fn preview_width(width: u16, prefix: &str) -> usize {
    usize::from(width)
        .saturating_sub(UnicodeWidthStr::width(prefix))
        .max(20)
}

fn preview_line(
    spans: Vec<SourceSpan>,
    options: &SourcePreviewOptions<'_>,
    max_width: usize,
) -> Line {
    let spans = truncate_spans(spans, max_width);
    let mut output = Vec::with_capacity(spans.len().saturating_add(1));
    output.push(Span::styled(
        options.line_prefix.to_owned(),
        options.prefix_style,
    ));
    output.extend(spans.into_iter().map(|span| {
        Span::styled(
            span.content,
            options
                .source_style
                .patch(span.style.unwrap_or_else(Style::new)),
        )
    }));
    Line::from_spans(output)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceSpan {
    content: String,
    style: Option<Style>,
}

impl SourceSpan {
    const fn plain(content: String) -> Self {
        Self {
            content,
            style: None,
        }
    }

    #[cfg(feature = "syntax")]
    const fn styled(content: String, style: Style) -> Self {
        Self {
            content,
            style: Some(style),
        }
    }
}

#[cfg(feature = "syntax")]
fn highlight_lines(syntax_hint: &str, lines: &[String]) -> Vec<Vec<SourceSpan>> {
    let highlighter = SyntaxHighlighter::new();
    if !highlighter.can_highlight(syntax_hint) {
        return plain_lines(lines);
    }
    let borrowed_lines = lines.iter().map(String::as_str).collect::<Vec<_>>();
    highlighter
        .highlight_lines_tokens(syntax_hint, &borrowed_lines)
        .into_iter()
        .map(|line| {
            line.into_iter()
                .map(|span| SourceSpan::styled(span.content, syntax_style_to_tui(span.style)))
                .collect()
        })
        .collect()
}

#[cfg(not(feature = "syntax"))]
fn highlight_lines(_syntax_hint: &str, lines: &[String]) -> Vec<Vec<SourceSpan>> {
    plain_lines(lines)
}

fn plain_lines(lines: &[String]) -> Vec<Vec<SourceSpan>> {
    lines
        .iter()
        .map(|line| vec![SourceSpan::plain(line.clone())])
        .collect()
}

fn truncate_spans(spans: Vec<SourceSpan>, max_width: usize) -> Vec<SourceSpan> {
    let total_width = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_str()))
        .sum::<usize>();
    if total_width <= max_width {
        return spans;
    }

    let content_width = max_width.saturating_sub(UnicodeWidthStr::width("…"));
    let mut output = Vec::new();
    let mut used_width = 0usize;

    for span in spans {
        let mut content = String::new();
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if used_width.saturating_add(grapheme_width) > content_width {
                if !content.is_empty() {
                    output.push(SourceSpan {
                        content,
                        style: span.style,
                    });
                }
                push_truncation_marker(&mut output, span.style);
                return output;
            }
            content.push_str(grapheme);
            used_width = used_width.saturating_add(grapheme_width);
        }
        if !content.is_empty() {
            output.push(SourceSpan {
                content,
                style: span.style,
            });
        }
    }

    push_truncation_marker(&mut output, None);
    output
}

fn push_truncation_marker(output: &mut Vec<SourceSpan>, style: Option<Style>) {
    output.push(SourceSpan {
        content: "…".to_owned(),
        style,
    });
}

#[cfg(feature = "syntax")]
const fn syntax_style_to_tui(style: SyntaxStyle) -> Style {
    let mut output = Style::new().fg(Color::Rgb(
        style.foreground_r,
        style.foreground_g,
        style.foreground_b,
    ));
    if style.bold {
        output = output.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        output = output.add_modifier(Modifier::ITALIC);
    }
    if style.underline {
        output = output.add_modifier(Modifier::UNDERLINE);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{SourcePreviewOptions, source_preview_lines};

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref() as &str)
            .collect::<String>()
    }

    use bmux_tui::prelude::Line;

    #[test]
    fn limits_preview_lines() {
        let rows = source_preview_lines(
            "one\ntwo\nthree",
            &SourcePreviewOptions::new("txt", 80).max_lines(2),
        );

        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("one"), "{rendered}");
        assert!(rendered.contains("two"), "{rendered}");
        assert!(!rendered.contains("three"), "{rendered}");
        assert!(rendered.contains("preview truncated"), "{rendered}");
    }

    #[test]
    fn truncates_long_lines() {
        let rows = source_preview_lines(
            "abcdefghijklmnopqrstuvwxyz",
            &SourcePreviewOptions::new("txt", 10),
        );

        assert!(line_text(&rows[0]).contains('…'));
    }

    #[test]
    fn truncates_after_preserving_grapheme_boundaries() {
        let rows = source_preview_lines(
            "abcdefghijklmnopqr🙂b",
            &SourcePreviewOptions::new("txt", 6),
        );

        assert_eq!(line_text(&rows[0]), "  │ abcdefghijklmnopqr…");
    }

    #[cfg(feature = "syntax")]
    #[test]
    fn highlights_known_source() {
        let rows = source_preview_lines("pub fn main() {}", &SourcePreviewOptions::new("rust", 80));

        assert!(
            rows[0]
                .spans
                .iter()
                .skip(1)
                .any(|span| span.style.fg.is_some()),
            "expected at least one highlighted span: {:?}",
            rows[0]
        );
    }

    #[cfg(feature = "syntax")]
    #[test]
    fn highlights_toml_from_nested_path() {
        let rows = source_preview_lines(
            "[package]\nname = \"bcode\"",
            &SourcePreviewOptions::new("packages/example/Cargo.toml", 80),
        );

        assert!(
            rows.iter()
                .flat_map(|row| row.spans.iter().skip(1))
                .any(|span| span.style.fg.is_some()),
            "expected TOML highlighting: {rows:?}"
        );
    }
}
