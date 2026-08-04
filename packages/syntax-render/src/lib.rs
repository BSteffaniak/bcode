//! Syntax-highlighted terminal rendering for Bcode text surfaces.
//!
//! Syntax definitions come from `two-face`'s `bat`-curated Syntect bundle.
//! Callers may identify syntaxes with language names, common Markdown fence
//! aliases, exact filenames, or file extensions. Unknown hints safely render as
//! plain text. Prefer updating the bundled definitions for future language
//! coverage; add aliases here only when common user-facing hints differ from
//! the syntax metadata.

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
static DEFAULT_THEME: OnceLock<Theme> = OnceLock::new();

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
///
/// The optional palette remaps syntax scope colors into application-supplied
/// semantic colors while preserving Syntect's syntax parsing and font-style
/// modifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxHighlighter {
    palette: Option<SyntaxPalette>,
}

/// Semantic syntax color palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyntaxPalette {
    /// Plain source text.
    pub text: SyntaxColor,
    /// Comments and documentation.
    pub comment: SyntaxColor,
    /// Keywords and control flow.
    pub keyword: SyntaxColor,
    /// Function and method names.
    pub function: SyntaxColor,
    /// Variables, fields, and parameters.
    pub variable: SyntaxColor,
    /// String and character literals.
    pub string: SyntaxColor,
    /// Numeric and boolean literals.
    pub number: SyntaxColor,
    /// Type and namespace names.
    pub type_name: SyntaxColor,
    /// Operators.
    pub operator: SyntaxColor,
    /// Punctuation and delimiters.
    pub punctuation: SyntaxColor,
}

/// Portable RGB syntax color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyntaxColor {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl SyntaxColor {
    /// Create an RGB syntax color.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

impl Default for SyntaxHighlighter {
    fn default() -> Self {
        Self::new()
    }
}

impl SyntaxHighlighter {
    /// Create a syntax highlighter using the bundled default syntax theme.
    #[must_use]
    pub const fn new() -> Self {
        Self { palette: None }
    }

    /// Create a syntax highlighter using an application-supplied semantic palette.
    #[must_use]
    pub const fn with_palette(palette: SyntaxPalette) -> Self {
        Self {
            palette: Some(palette),
        }
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
        let mut highlighter = HighlightLines::new(syntax, default_theme());
        highlight_line_tokens_with(&mut highlighter, line).map_or_else(
            || plain_syntax_spans_with_palette(line, self.palette),
            |spans| remap_spans(spans, self.palette),
        )
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
            return lines
                .iter()
                .map(|line| plain_syntax_spans_with_palette(line, self.palette))
                .collect();
        };
        let mut highlighter = HighlightLines::new(syntax, default_theme());
        lines
            .iter()
            .map(|line| {
                highlight_line_tokens_with(&mut highlighter, line).map_or_else(
                    || plain_syntax_spans_with_palette(line, self.palette),
                    |spans| remap_spans(spans, self.palette),
                )
            })
            .collect()
    }
}

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

fn default_theme() -> &'static Theme {
    DEFAULT_THEME.get_or_init(|| {
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
    let normalized_hint = path_or_language.trim().to_ascii_lowercase();
    let language = language_alias(&normalized_hint);

    syntaxes
        .find_syntax_by_token(language)
        .or_else(|| {
            Path::new(path_or_language)
                .file_name()
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

fn language_alias(language: &str) -> &str {
    match language {
        "c++" => "cpp",
        "js" => "javascript",
        "py" => "python",
        "shell" => "bash",
        "ts" => "typescript",
        other => other,
    }
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

fn plain_syntax_spans_with_palette(line: &str, palette: Option<SyntaxPalette>) -> Vec<SyntaxSpan> {
    vec![SyntaxSpan::new(
        line.to_owned(),
        palette.map_or_else(default_syntax_style, |palette| syntax_style(palette.text)),
    )]
}

fn plain_syntax_spans(line: &str) -> Vec<SyntaxSpan> {
    plain_syntax_spans_with_palette(line, None)
}

fn remap_spans(mut spans: Vec<SyntaxSpan>, palette: Option<SyntaxPalette>) -> Vec<SyntaxSpan> {
    let Some(palette) = palette else {
        return spans;
    };
    for span in &mut spans {
        let color = classify_scope_color(span.style, default_syntax_style(), palette);
        span.style.foreground_r = color.r;
        span.style.foreground_g = color.g;
        span.style.foreground_b = color.b;
    }
    spans
}

fn classify_scope_color(
    style: SyntaxStyle,
    default: SyntaxStyle,
    palette: SyntaxPalette,
) -> SyntaxColor {
    // Syntect's bundled theme supplies stable source scope colors. Map those
    // known colors to semantic categories; unknown scope colors remain plain
    // text rather than leaking the bundled dark palette into a caller theme.
    match (style.foreground_r, style.foreground_g, style.foreground_b) {
        (101, 115, 126) | (92, 99, 112) => palette.comment,
        (180, 142, 173) | (198, 120, 221) => palette.keyword,
        (143, 161, 179) | (220, 220, 170) => palette.function,
        (192, 197, 206) | (156, 220, 254) => palette.variable,
        (163, 190, 140) | (206, 145, 120) => palette.string,
        (208, 135, 112) | (181, 206, 168) => palette.number,
        (235, 203, 139) | (78, 201, 176) => palette.type_name,
        (197, 200, 198) | (212, 212, 212) => palette.operator,
        channels
            if channels
                == (
                    default.foreground_r,
                    default.foreground_g,
                    default.foreground_b,
                ) =>
        {
            palette.text
        }
        _ => palette.punctuation,
    }
}

const fn syntax_style(color: SyntaxColor) -> SyntaxStyle {
    SyntaxStyle {
        foreground_r: color.r,
        foreground_g: color.g,
        foreground_b: color.b,
        bold: false,
        italic: false,
        underline: false,
    }
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
    use super::{SyntaxColor, SyntaxHighlighter, SyntaxPalette, syntax_for};

    #[test]
    fn semantic_palette_replaces_bundled_dark_colors() {
        let palette = SyntaxPalette {
            text: SyntaxColor::rgb(1, 1, 1),
            comment: SyntaxColor::rgb(2, 2, 2),
            keyword: SyntaxColor::rgb(3, 3, 3),
            function: SyntaxColor::rgb(4, 4, 4),
            variable: SyntaxColor::rgb(5, 5, 5),
            string: SyntaxColor::rgb(6, 6, 6),
            number: SyntaxColor::rgb(7, 7, 7),
            type_name: SyntaxColor::rgb(8, 8, 8),
            operator: SyntaxColor::rgb(9, 9, 9),
            punctuation: SyntaxColor::rgb(10, 10, 10),
        };
        let spans = SyntaxHighlighter::with_palette(palette)
            .highlight_line_tokens("rust", "// comment\nfn main() { let value = 42; }");

        assert!(spans.iter().all(|span| {
            let red = span.style.foreground_r;
            (1..=10).contains(&red)
                && span.style.foreground_g == red
                && span.style.foreground_b == red
        }));
    }

    #[test]
    fn detects_curated_syntaxes_from_languages_and_paths() {
        let cases = [
            ("toml", "TOML"),
            ("Cargo.toml", "TOML"),
            ("packages/example/Cargo.toml", "TOML"),
            ("nix", "Nix"),
            ("default.nix", "Nix"),
            ("flake.nix", "Nix"),
            ("Dockerfile", "Dockerfile"),
            ("typescript", "TypeScript"),
            ("file.ts", "TypeScript"),
            ("file.tsx", "TypeScriptReact"),
            ("main.tf", "Terraform"),
            ("build.zig", "Zig"),
            ("src/lib.rs", "Rust"),
        ];

        for (hint, expected_name) in cases {
            let syntax = syntax_for(hint).unwrap_or_else(|| panic!("missing syntax for {hint}"));
            assert_eq!(syntax.name, expected_name, "wrong syntax for {hint}");
        }
    }

    #[test]
    fn detects_common_language_aliases() {
        let cases = [
            ("shell", "Bourne Again Shell (bash)"),
            ("js", "JavaScript"),
            ("ts", "TypeScript"),
            ("c++", "C++"),
            ("py", "Python"),
        ];

        for (hint, expected_name) in cases {
            let syntax = syntax_for(hint).unwrap_or_else(|| panic!("missing syntax for {hint}"));
            assert_eq!(syntax.name, expected_name, "wrong syntax for {hint}");
        }
    }

    #[test]
    fn detection_is_case_insensitive() {
        assert_eq!(
            syntax_for("TOML").map(|syntax| syntax.name.as_str()),
            Some("TOML")
        );
        assert_eq!(
            syntax_for("config.NIX").map(|syntax| syntax.name.as_str()),
            Some("Nix")
        );
    }

    #[test]
    fn falls_back_for_unknown_extensions() {
        let highlighter = SyntaxHighlighter::new();
        assert!(!highlighter.can_highlight("file.unknown-bcode"));

        let spans = highlighter.highlight_line_tokens("file.unknown-bcode", "plain text");

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "plain text");
    }

    #[test]
    fn highlights_curated_syntaxes_without_changing_text() {
        let highlighter = SyntaxHighlighter::new();
        let cases = [
            ("toml", "[package]\nname = \"bcode\""),
            ("nix", "{ pkgs, ... }: pkgs.mkShell { }"),
        ];

        for (hint, source) in cases {
            let lines = source.lines().collect::<Vec<_>>();
            let token_lines = highlighter.highlight_lines_tokens(hint, &lines);
            let reconstructed = token_lines
                .iter()
                .map(|line| {
                    line.iter()
                        .map(|span| span.content.as_str())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");

            assert_eq!(reconstructed, source);
            assert!(
                token_lines
                    .iter()
                    .flatten()
                    .any(|span| { span.style != super::default_syntax_style() }),
                "expected syntax styles for {hint}"
            );
        }
    }
}
