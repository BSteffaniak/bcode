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
    /// Foreground terminal color, preserved without palette conversion.
    pub foreground: SyntaxColor,
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

/// Portable terminal syntax color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyntaxColor {
    /// Terminal-defined default foreground.
    Default,
    /// One of the terminal's sixteen named ANSI colors.
    Ansi(AnsiColor),
    /// One entry in the terminal's indexed color palette.
    Indexed(u8),
    /// Explicit true-color value.
    Rgb(u8, u8, u8),
}

/// Named ANSI terminal color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnsiColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
}

impl SyntaxColor {
    /// Create an RGB syntax color.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::Rgb(r, g, b)
    }

    /// Create a syntax color from the terminal backend representation without conversion.
    #[must_use]
    pub const fn from_tui(color: Color) -> Self {
        match color {
            Color::Default => Self::Default,
            Color::Indexed(index) => Self::Indexed(index),
            Color::Rgb(r, g, b) => Self::Rgb(r, g, b),
            Color::Black => Self::Ansi(AnsiColor::Black),
            Color::Red => Self::Ansi(AnsiColor::Red),
            Color::Green => Self::Ansi(AnsiColor::Green),
            Color::Yellow => Self::Ansi(AnsiColor::Yellow),
            Color::Blue => Self::Ansi(AnsiColor::Blue),
            Color::Magenta => Self::Ansi(AnsiColor::Magenta),
            Color::Cyan => Self::Ansi(AnsiColor::Cyan),
            Color::White => Self::Ansi(AnsiColor::White),
            Color::BrightBlack => Self::Ansi(AnsiColor::BrightBlack),
            Color::BrightRed => Self::Ansi(AnsiColor::BrightRed),
            Color::BrightGreen => Self::Ansi(AnsiColor::BrightGreen),
            Color::BrightYellow => Self::Ansi(AnsiColor::BrightYellow),
            Color::BrightBlue => Self::Ansi(AnsiColor::BrightBlue),
            Color::BrightMagenta => Self::Ansi(AnsiColor::BrightMagenta),
            Color::BrightCyan => Self::Ansi(AnsiColor::BrightCyan),
            Color::BrightWhite => Self::Ansi(AnsiColor::BrightWhite),
        }
    }

    /// Convert this renderer-neutral color to the terminal backend color.
    #[must_use]
    pub const fn to_tui(self) -> Color {
        match self {
            Self::Default => Color::Default,
            Self::Indexed(index) => Color::Indexed(index),
            Self::Rgb(r, g, b) => Color::Rgb(r, g, b),
            Self::Ansi(color) => match color {
                AnsiColor::Black => Color::Black,
                AnsiColor::Red => Color::Red,
                AnsiColor::Green => Color::Green,
                AnsiColor::Yellow => Color::Yellow,
                AnsiColor::Blue => Color::Blue,
                AnsiColor::Magenta => Color::Magenta,
                AnsiColor::Cyan => Color::Cyan,
                AnsiColor::White => Color::White,
                AnsiColor::BrightBlack => Color::BrightBlack,
                AnsiColor::BrightRed => Color::BrightRed,
                AnsiColor::BrightGreen => Color::BrightGreen,
                AnsiColor::BrightYellow => Color::BrightYellow,
                AnsiColor::BrightBlue => Color::BrightBlue,
                AnsiColor::BrightMagenta => Color::BrightMagenta,
                AnsiColor::BrightCyan => Color::BrightCyan,
                AnsiColor::BrightWhite => Color::BrightWhite,
            },
        }
    }
}

impl Default for SyntaxHighlighter {
    fn default() -> Self {
        Self::new()
    }
}

impl SyntaxHighlighter {
    /// Create a syntax highlighter using a neutral bundled classification theme.
    ///
    /// Application presentation should use [`Self::with_palette`]. This
    /// compatibility constructor is retained for standalone callers that have
    /// not supplied semantic syntax colors.
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
        let mut highlighter = HighlightLines::new(syntax, classification_theme());
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
        let mut highlighter = HighlightLines::new(syntax, classification_theme());
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

fn classification_theme() -> &'static Theme {
    DEFAULT_THEME.get_or_init(|| {
        let themes = ThemeSet::load_defaults();
        themes
            .themes
            .get("base16-ocean.dark")
            .cloned()
            .unwrap_or_else(|| panic!("Syntect default themes must include base16-ocean.dark"))
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
    let default = default_syntax_style();
    for span in &mut spans {
        span.style.foreground =
            classify_scope_color(span.style.foreground, default.foreground, palette);
    }
    spans
}

fn classify_scope_color(
    foreground: SyntaxColor,
    default: SyntaxColor,
    palette: SyntaxPalette,
) -> SyntaxColor {
    // Syntect's bundled theme supplies stable source scope colors. Map those
    // known colors to semantic categories; unknown scope colors remain plain
    // text rather than leaking the bundled dark palette into a caller theme.
    match foreground {
        SyntaxColor::Rgb(101, 115, 126) | SyntaxColor::Rgb(92, 99, 112) => palette.comment,
        SyntaxColor::Rgb(180, 142, 173) | SyntaxColor::Rgb(198, 120, 221) => palette.keyword,
        SyntaxColor::Rgb(143, 161, 179) | SyntaxColor::Rgb(220, 220, 170) => palette.function,
        SyntaxColor::Rgb(192, 197, 206) | SyntaxColor::Rgb(156, 220, 254) => palette.variable,
        SyntaxColor::Rgb(163, 190, 140) | SyntaxColor::Rgb(206, 145, 120) => palette.string,
        SyntaxColor::Rgb(208, 135, 112) | SyntaxColor::Rgb(181, 206, 168) => palette.number,
        SyntaxColor::Rgb(235, 203, 139) | SyntaxColor::Rgb(78, 201, 176) => palette.type_name,
        SyntaxColor::Rgb(197, 200, 198) | SyntaxColor::Rgb(212, 212, 212) => palette.operator,
        color if color == default => palette.text,
        _ => palette.punctuation,
    }
}

const fn syntax_style(color: SyntaxColor) -> SyntaxStyle {
    SyntaxStyle {
        foreground: color,
        bold: false,
        italic: false,
        underline: false,
    }
}

const fn default_syntax_style() -> SyntaxStyle {
    SyntaxStyle {
        foreground: SyntaxColor::rgb(255, 255, 255),
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
        foreground: SyntaxColor::rgb(style.foreground.r, style.foreground.g, style.foreground.b),
        bold: style.font_style.contains(FontStyle::BOLD),
        italic: style.font_style.contains(FontStyle::ITALIC),
        underline: style.font_style.contains(FontStyle::UNDERLINE),
    }
}

const fn syntax_style_to_tui(style: SyntaxStyle) -> Style {
    let mut output = Style::new().fg(style.foreground.to_tui());
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
    use super::{
        AnsiColor, SyntaxColor, SyntaxHighlighter, SyntaxPalette, classify_scope_color, syntax_for,
    };
    use bmux_tui::prelude::Color;

    #[test]
    fn syntax_colors_preserve_terminal_representations() {
        for color in [
            Color::Default,
            Color::Blue,
            Color::BrightCyan,
            Color::Indexed(173),
            Color::Rgb(12, 34, 56),
        ] {
            assert_eq!(SyntaxColor::from_tui(color).to_tui(), color);
        }
        assert_eq!(
            SyntaxColor::from_tui(Color::Blue),
            SyntaxColor::Ansi(AnsiColor::Blue)
        );
    }

    #[test]
    fn semantic_palette_preserves_non_rgb_colors() {
        let palette = SyntaxPalette {
            text: SyntaxColor::Default,
            comment: SyntaxColor::Ansi(AnsiColor::BrightBlack),
            keyword: SyntaxColor::Ansi(AnsiColor::Blue),
            function: SyntaxColor::Indexed(173),
            variable: SyntaxColor::Ansi(AnsiColor::BrightCyan),
            string: SyntaxColor::Ansi(AnsiColor::Green),
            number: SyntaxColor::Indexed(214),
            type_name: SyntaxColor::Ansi(AnsiColor::Cyan),
            operator: SyntaxColor::Default,
            punctuation: SyntaxColor::Rgb(12, 34, 56),
        };
        assert_eq!(
            classify_scope_color(
                SyntaxColor::rgb(101, 115, 126),
                SyntaxColor::rgb(255, 255, 255),
                palette,
            ),
            SyntaxColor::Ansi(AnsiColor::BrightBlack)
        );
        assert_eq!(
            classify_scope_color(
                SyntaxColor::rgb(143, 161, 179),
                SyntaxColor::rgb(255, 255, 255),
                palette,
            ),
            SyntaxColor::Indexed(173)
        );
    }

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
        let lines = ["// comment", "pub fn main() { let value = 42; }"];
        let spans = SyntaxHighlighter::with_palette(palette)
            .highlight_lines_tokens("rust", &lines)
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let colors = spans
            .iter()
            .map(|span| span.style.foreground)
            .collect::<Vec<_>>();

        assert!(colors.contains(&palette.comment), "{spans:?}");
        assert!(colors.contains(&palette.keyword), "{spans:?}");
        assert!(colors.contains(&palette.function), "{spans:?}");
        assert!(colors.contains(&palette.number), "{spans:?}");
        assert!(
            spans
                .iter()
                .any(|span| span.style.foreground != palette.text),
            "semantic highlighting collapsed to plain text: {spans:?}"
        );
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
