//! Syntax-highlighted terminal rendering for Bcode text surfaces.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::path::Path;
use std::sync::OnceLock;

use bmux_tui::prelude::{Color, Modifier, Span, Style};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<Theme> = OnceLock::new();

/// Renderer-neutral syntax-highlighted text span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxSpan {
    /// Span text.
    pub content: String,
    /// Renderer-neutral syntax style.
    pub style: SyntaxStyle,
}

impl SyntaxSpan {
    /// Create a syntax span.
    #[must_use]
    pub const fn new(content: String, style: SyntaxStyle) -> Self {
        Self { content, style }
    }
}

/// Renderer-neutral syntax style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxStyle {
    /// Foreground red channel.
    pub foreground_r: u8,
    /// Foreground green channel.
    pub foreground_g: u8,
    /// Foreground blue channel.
    pub foreground_b: u8,
    /// Whether text should be bold.
    pub bold: bool,
    /// Whether text should be italic.
    pub italic: bool,
    /// Whether text should be underlined.
    pub underline: bool,
}

/// Terminal syntax highlighter backed by syntect's bundled syntaxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyntaxHighlighter;

impl SyntaxHighlighter {
    /// Create a syntax highlighter.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Return whether a syntax can be detected for a path or language hint.
    #[must_use]
    pub fn can_highlight(&self, path_or_language: &str) -> bool {
        syntax_for(path_or_language).is_some()
    }

    /// Highlight one line using a path or language hint.
    #[must_use]
    pub fn highlight_line(&self, path_or_language: &str, line: &str) -> Vec<Span> {
        self.highlight_line_tokens(path_or_language, line)
            .into_iter()
            .map(syntax_span_to_tui)
            .collect()
    }

    /// Highlight one line into renderer-neutral syntax spans.
    #[must_use]
    pub fn highlight_line_tokens(&self, path_or_language: &str, line: &str) -> Vec<SyntaxSpan> {
        let Some(syntax) = syntax_for(path_or_language) else {
            return plain_syntax_spans(line);
        };
        let mut highlighter = HighlightLines::new(syntax, theme());
        highlight_line_tokens_with(&mut highlighter, line)
            .unwrap_or_else(|| plain_syntax_spans(line))
    }

    /// Highlight multiple lines using a path or language hint.
    #[must_use]
    pub fn highlight_lines(&self, path_or_language: &str, lines: &[&str]) -> Vec<Vec<Span>> {
        self.highlight_lines_tokens(path_or_language, lines)
            .into_iter()
            .map(|line| line.into_iter().map(syntax_span_to_tui).collect())
            .collect()
    }

    /// Highlight multiple lines into renderer-neutral syntax spans.
    #[must_use]
    pub fn highlight_lines_tokens(
        &self,
        path_or_language: &str,
        lines: &[&str],
    ) -> Vec<Vec<SyntaxSpan>> {
        let Some(syntax) = syntax_for(path_or_language) else {
            return lines.iter().map(|line| plain_syntax_spans(line)).collect();
        };
        let mut highlighter = HighlightLines::new(syntax, theme());
        lines
            .iter()
            .map(|line| {
                highlight_line_tokens_with(&mut highlighter, line)
                    .unwrap_or_else(|| plain_syntax_spans(line))
            })
            .collect()
    }
}

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme() -> &'static Theme {
    THEME.get_or_init(|| {
        let themes = ThemeSet::load_defaults();
        themes
            .themes
            .get("base16-ocean.dark")
            .or_else(|| themes.themes.values().next())
            .cloned()
            .unwrap_or_default()
    })
}

fn syntax_for(path_or_language: &str) -> Option<&'static SyntaxReference> {
    let syntaxes = syntax_set();
    let lower_language = path_or_language.to_ascii_lowercase();
    let normalized_language = match lower_language.as_str() {
        "toml" | "cargo.toml" => "conf",
        other => other,
    };
    syntaxes
        .find_syntax_by_token(normalized_language)
        .or_else(|| syntaxes.find_syntax_by_extension(normalized_language))
        .or_else(|| {
            let path = Path::new(path_or_language);
            path.file_name()
                .and_then(std::ffi::OsStr::to_str)
                .and_then(|file_name| syntaxes.find_syntax_by_extension(file_name))
        })
        .or_else(|| {
            Path::new(path_or_language)
                .extension()
                .and_then(std::ffi::OsStr::to_str)
                .and_then(|extension| syntaxes.find_syntax_by_extension(extension))
        })
}

fn highlight_line_tokens_with(
    highlighter: &mut HighlightLines<'_>,
    line: &str,
) -> Option<Vec<SyntaxSpan>> {
    let ranges = highlighter.highlight_line(line, syntax_set()).ok()?;
    let spans = ranges
        .into_iter()
        .flat_map(|(style, content)| {
            LinesWithEndings::from(content).filter_map(move |line| {
                let content = line.trim_end_matches(['\r', '\n']);
                if content.is_empty() {
                    None
                } else {
                    Some(SyntaxSpan::new(
                        content.to_owned(),
                        syntect_style_to_syntax(style),
                    ))
                }
            })
        })
        .collect::<Vec<_>>();
    Some(if spans.is_empty() {
        plain_syntax_spans(line)
    } else {
        spans
    })
}

fn plain_syntax_spans(line: &str) -> Vec<SyntaxSpan> {
    vec![SyntaxSpan::new(line.to_owned(), default_syntax_style())]
}

const fn default_syntax_style() -> SyntaxStyle {
    SyntaxStyle {
        foreground_r: 255,
        foreground_g: 255,
        foreground_b: 255,
        bold: false,
        italic: false,
        underline: false,
    }
}

fn syntax_span_to_tui(span: SyntaxSpan) -> Span {
    Span::styled(span.content, syntax_style_to_tui(span.style))
}

const fn syntect_style_to_syntax(style: syntect::highlighting::Style) -> SyntaxStyle {
    SyntaxStyle {
        foreground_r: style.foreground.r,
        foreground_g: style.foreground.g,
        foreground_b: style.foreground.b,
        bold: style.font_style.contains(FontStyle::BOLD),
        italic: style.font_style.contains(FontStyle::ITALIC),
        underline: style.font_style.contains(FontStyle::UNDERLINE),
    }
}

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
    use super::SyntaxHighlighter;

    #[test]
    fn detects_common_file_extensions() {
        let highlighter = SyntaxHighlighter::new();

        assert!(highlighter.can_highlight("src/lib.rs"));
        assert!(highlighter.can_highlight("data.json"));
        assert!(highlighter.can_highlight("json"));
    }

    #[test]
    fn falls_back_for_unknown_extensions() {
        let spans = SyntaxHighlighter::new().highlight_line("file.unknown-bcode", "plain text");

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "plain text");
    }

    #[test]
    fn highlights_known_syntax() {
        let spans = SyntaxHighlighter::new().highlight_line("rust", "pub fn main() {}");

        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>(),
            "pub fn main() {}"
        );
        assert!(spans.iter().any(|span| span.style.fg.is_some()));
    }
}
