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

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<Theme> = OnceLock::new();

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
        let Some(syntax) = syntax_for(path_or_language) else {
            return plain_spans(line);
        };
        let mut highlighter = HighlightLines::new(syntax, theme());
        highlight_line_with(&mut highlighter, line).unwrap_or_else(|| plain_spans(line))
    }

    /// Highlight multiple lines using a path or language hint.
    #[must_use]
    pub fn highlight_lines(&self, path_or_language: &str, lines: &[&str]) -> Vec<Vec<Span>> {
        let Some(syntax) = syntax_for(path_or_language) else {
            return lines.iter().map(|line| plain_spans(line)).collect();
        };
        let mut highlighter = HighlightLines::new(syntax, theme());
        lines
            .iter()
            .map(|line| {
                highlight_line_with(&mut highlighter, line).unwrap_or_else(|| plain_spans(line))
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

fn highlight_line_with(highlighter: &mut HighlightLines<'_>, line: &str) -> Option<Vec<Span>> {
    let ranges = highlighter.highlight_line(line, syntax_set()).ok()?;
    let spans = ranges
        .into_iter()
        .filter(|(_, content)| !content.is_empty())
        .map(|(style, content)| Span::styled(content.to_owned(), syntect_style_to_tui(style)))
        .collect::<Vec<_>>();
    Some(if spans.is_empty() {
        plain_spans(line)
    } else {
        spans
    })
}

fn plain_spans(line: &str) -> Vec<Span> {
    vec![Span::raw(line.to_owned())]
}

const fn syntect_style_to_tui(style: syntect::highlighting::Style) -> Style {
    let mut output = Style::new().fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));
    if style.font_style.contains(FontStyle::BOLD) {
        output = output.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        output = output.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
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
