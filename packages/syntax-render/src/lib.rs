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
use syntect::easy::ScopeRegionIterator;
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();

/// Renderer-neutral semantic syntax role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyntaxRole {
    Text,
    Comment,
    Keyword,
    Function,
    Variable,
    String,
    Number,
    Type,
    Operator,
    Punctuation,
}

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
    /// Semantic token role determined from syntax scopes.
    pub role: SyntaxRole,
    /// Foreground terminal color selected by the active palette.
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
    palette: SyntaxPalette,
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

impl Default for SyntaxPalette {
    fn default() -> Self {
        Self {
            text: SyntaxColor::rgb(212, 212, 212),
            comment: SyntaxColor::rgb(106, 153, 85),
            keyword: SyntaxColor::rgb(86, 156, 214),
            function: SyntaxColor::rgb(220, 220, 170),
            variable: SyntaxColor::rgb(156, 220, 254),
            string: SyntaxColor::rgb(206, 145, 120),
            number: SyntaxColor::rgb(181, 206, 168),
            type_name: SyntaxColor::rgb(78, 201, 176),
            operator: SyntaxColor::rgb(212, 212, 212),
            punctuation: SyntaxColor::rgb(212, 212, 212),
        }
    }
}

impl SyntaxPalette {
    const fn color(self, role: SyntaxRole) -> SyntaxColor {
        match role {
            SyntaxRole::Text => self.text,
            SyntaxRole::Comment => self.comment,
            SyntaxRole::Keyword => self.keyword,
            SyntaxRole::Function => self.function,
            SyntaxRole::Variable => self.variable,
            SyntaxRole::String => self.string,
            SyntaxRole::Number => self.number,
            SyntaxRole::Type => self.type_name,
            SyntaxRole::Operator => self.operator,
            SyntaxRole::Punctuation => self.punctuation,
        }
    }
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
    /// Create a syntax highlighter using Bcode's compatibility palette.
    #[must_use]
    pub fn new() -> Self {
        Self {
            palette: SyntaxPalette::default(),
        }
    }

    /// Create a syntax highlighter using an application-supplied semantic palette.
    #[must_use]
    pub const fn with_palette(palette: SyntaxPalette) -> Self {
        Self { palette }
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
            return plain_syntax_spans(line, self.palette);
        };
        let mut classifier = ScopeClassifier::new(syntax, self.palette);
        classifier
            .classify_line(line)
            .unwrap_or_else(|| plain_syntax_spans(line, self.palette))
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
                .map(|line| plain_syntax_spans(line, self.palette))
                .collect();
        };
        let mut classifier = ScopeClassifier::new(syntax, self.palette);
        lines
            .iter()
            .map(|line| {
                classifier
                    .classify_line(line)
                    .unwrap_or_else(|| plain_syntax_spans(line, self.palette))
            })
            .collect()
    }
}

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
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

struct ScopeClassifier {
    parse_state: ParseState,
    scope_stack: ScopeStack,
    palette: SyntaxPalette,
}

impl ScopeClassifier {
    fn new(syntax: &SyntaxReference, palette: SyntaxPalette) -> Self {
        Self {
            parse_state: ParseState::new(syntax),
            scope_stack: ScopeStack::new(),
            palette,
        }
    }

    fn classify_line(&mut self, line: &str) -> Option<Vec<SyntaxSpan>> {
        let operations = self.parse_state.parse_line(line, syntax_set()).ok()?;
        let mut spans = Vec::new();
        for (content, operation) in ScopeRegionIterator::new(&operations, line) {
            self.scope_stack.apply(operation).ok()?;
            let content = content.trim_end_matches(['\r', '\n']);
            if content.is_empty() {
                continue;
            }
            let (role, font) = classify_scope_stack(&self.scope_stack);
            spans.push(SyntaxSpan::new(
                content.to_owned(),
                syntax_style(role, self.palette.color(role), font),
            ));
        }
        Some(if spans.is_empty() {
            plain_syntax_spans(line, self.palette)
        } else {
            spans
        })
    }
}

/// Font emphasis derived from syntax scopes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FontEmphasis {
    bold: bool,
    italic: bool,
    underline: bool,
}

impl FontEmphasis {
    const fn bold() -> Self {
        Self {
            bold: true,
            italic: false,
            underline: false,
        }
    }

    const fn italic() -> Self {
        Self {
            bold: false,
            italic: true,
            underline: false,
        }
    }

    const fn underline() -> Self {
        Self {
            bold: false,
            italic: false,
            underline: true,
        }
    }
}

fn classify_scope_stack(stack: &ScopeStack) -> (SyntaxRole, FontEmphasis) {
    let scopes = stack
        .scopes
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let has = |needle: &str| scopes.iter().any(|scope| scope_contains(scope, needle));

    // Code-token scopes win over markup scopes so languages embedded in Markdown
    // fences keep their own semantic colors.
    let role = if has("comment") {
        SyntaxRole::Comment
    } else if has("string") || has("character") || has("regexp") {
        SyntaxRole::String
    } else if has("constant.numeric")
        || has("constant.language.boolean")
        || has("constant.language.null")
    {
        SyntaxRole::Number
    } else if has("entity.name.function") || has("support.function") || has("meta.function-call") {
        SyntaxRole::Function
    } else if has("entity.name.type")
        || has("entity.name.class")
        || has("entity.name.struct")
        || has("entity.name.enum")
        || has("entity.name.namespace")
        || has("support.type")
    {
        SyntaxRole::Type
    } else if has("keyword.operator") {
        SyntaxRole::Operator
    } else if has("keyword")
        || has("storage.modifier")
        || has("storage.control")
        || has("storage.type")
    {
        SyntaxRole::Keyword
    } else if has("variable")
        || has("entity.name.field")
        || has("entity.name.property")
        || has("support.variable")
    {
        SyntaxRole::Variable
    } else {
        return markup_classification(&has);
    };
    (role, markup_emphasis(&has))
}

/// Classify prose scopes once no code-token scope matched.
fn markup_classification(has: &impl Fn(&str) -> bool) -> (SyntaxRole, FontEmphasis) {
    if has("markup.heading") || has("entity.name.section") {
        (SyntaxRole::Keyword, FontEmphasis::bold())
    } else if has("markup.underline.link") || has("markup.link") {
        (SyntaxRole::Function, FontEmphasis::underline())
    } else if has("markup.raw") && !has("source") {
        // Fenced blocks embed a `source.*` scope; only unembedded raw spans such as
        // inline code should adopt the raw-literal color.
        (SyntaxRole::String, markup_emphasis(has))
    } else if has("markup.quote") {
        (SyntaxRole::Comment, markup_emphasis(has))
    } else if has("markup.list") || has("meta.separator") {
        (SyntaxRole::Punctuation, markup_emphasis(has))
    } else if has("markup.italic") && !has("markup.bold") {
        (SyntaxRole::Text, FontEmphasis::italic())
    } else if has("punctuation") {
        (SyntaxRole::Punctuation, markup_emphasis(has))
    } else {
        (SyntaxRole::Text, markup_emphasis(has))
    }
}

/// Derive emphasis that applies regardless of the resolved semantic role.
fn markup_emphasis(has: &impl Fn(&str) -> bool) -> FontEmphasis {
    FontEmphasis {
        bold: has("markup.bold") || has("markup.heading"),
        italic: has("markup.italic"),
        underline: has("markup.underline"),
    }
}

fn scope_contains(scope: &str, needle: &str) -> bool {
    scope == needle
        || scope
            .strip_prefix(needle)
            .is_some_and(|suffix| suffix.starts_with('.'))
}

fn plain_syntax_spans(line: &str, palette: SyntaxPalette) -> Vec<SyntaxSpan> {
    vec![SyntaxSpan::new(
        line.to_owned(),
        syntax_style(SyntaxRole::Text, palette.text, FontEmphasis::default()),
    )]
}

const fn syntax_style(role: SyntaxRole, color: SyntaxColor, font: FontEmphasis) -> SyntaxStyle {
    SyntaxStyle {
        role,
        foreground: color,
        bold: font.bold,
        italic: font.italic,
        underline: font.underline,
    }
}

fn syntax_span_to_tui(span: SyntaxSpan) -> Span {
    Span::styled(span.content, syntax_style_to_tui(span.style))
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
    use super::{AnsiColor, SyntaxColor, SyntaxHighlighter, SyntaxPalette, SyntaxRole, syntax_for};
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
        let lines = ["// comment", "pub fn main() { let value = 42; }"];
        let spans = SyntaxHighlighter::with_palette(palette)
            .highlight_lines_tokens("rust", &lines)
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();

        for (role, color) in [
            (SyntaxRole::Comment, palette.comment),
            (SyntaxRole::Keyword, palette.keyword),
            (SyntaxRole::Function, palette.function),
            (SyntaxRole::Number, palette.number),
        ] {
            assert!(
                spans
                    .iter()
                    .any(|span| span.style.role == role && span.style.foreground == color),
                "missing {role:?} with {color:?}: {spans:?}"
            );
        }
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
        let roles = spans.iter().map(|span| span.style.role).collect::<Vec<_>>();

        assert!(roles.contains(&SyntaxRole::Comment), "{spans:?}");
        assert!(roles.contains(&SyntaxRole::Keyword), "{spans:?}");
        assert!(roles.contains(&SyntaxRole::Function), "{spans:?}");
        assert!(roles.contains(&SyntaxRole::Number), "{spans:?}");
        assert!(
            spans
                .iter()
                .all(|span| { span.style.foreground == palette.color(span.style.role) })
        );
    }

    #[test]
    fn semantic_roles_cover_representative_languages() {
        let cases = [
            (
                "rust",
                "// note\npub fn main() { let value = 42; }",
                SyntaxRole::Comment,
            ),
            (
                "Cargo.toml",
                "[package]\nname = \"bcode\"",
                SyntaxRole::String,
            ),
            (
                "data.json",
                "{\"enabled\": true, \"count\": 3}",
                SyntaxRole::Number,
            ),
            (
                "script.sh",
                "# note\nif true; then echo ok; fi",
                SyntaxRole::Comment,
            ),
            (
                "file.ts",
                "export function main(): number { return 3; }",
                SyntaxRole::Keyword,
            ),
            (
                "default.nix",
                "{ pkgs }: \"${pkgs.hello}\"",
                SyntaxRole::String,
            ),
            (
                "README.md",
                "# Heading\n\nprose text\n",
                SyntaxRole::Keyword,
            ),
        ];

        for (hint, source, expected) in cases {
            let lines = source.lines().collect::<Vec<_>>();
            let spans = SyntaxHighlighter::new()
                .highlight_lines_tokens(hint, &lines)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            assert!(
                spans.iter().any(|span| span.style.role == expected),
                "expected {expected:?} for {hint}: {spans:?}"
            );
        }
    }

    #[test]
    fn multiline_scope_state_is_preserved() {
        let lines = ["/* first", "second */", "let value = 1;"];
        let rendered = SyntaxHighlighter::new().highlight_lines_tokens("rust", &lines);

        assert!(
            rendered[0]
                .iter()
                .all(|span| span.style.role == SyntaxRole::Comment),
            "{:?}",
            rendered[0]
        );
        assert!(
            rendered[1]
                .iter()
                .all(|span| span.style.role == SyntaxRole::Comment),
            "{:?}",
            rendered[1]
        );
        assert!(
            rendered[2]
                .iter()
                .any(|span| span.style.role == SyntaxRole::Keyword),
            "{:?}",
            rendered[2]
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
                    .any(|span| span.style.role != SyntaxRole::Text),
                "expected syntax styles for {hint}"
            );
        }
    }

    #[test]
    fn markdown_prose_roles_and_emphasis_are_semantic() {
        let source = "# Heading\n\n**bold** and *italic* with `code` and [link](https://example.com)\n\n> quoted\n\n- item one\n";
        let lines = source.lines().collect::<Vec<_>>();
        let spans = SyntaxHighlighter::new()
            .highlight_lines_tokens("README.md", &lines)
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();

        let find = |text: &str| {
            spans
                .iter()
                .find(|span| span.content == text)
                .unwrap_or_else(|| panic!("missing {text:?} in {spans:?}"))
                .style
        };

        let heading = find("Heading");
        assert_eq!(heading.role, SyntaxRole::Keyword);
        assert!(heading.bold, "headings render bold: {heading:?}");

        let bold = find("bold");
        assert!(bold.bold, "bold spans render bold: {bold:?}");

        let italic = find("italic");
        assert!(italic.italic, "italic spans render italic: {italic:?}");

        let link = find("https://example.com");
        assert_eq!(link.role, SyntaxRole::Function);
        assert!(link.underline, "links render underlined: {link:?}");

        assert_eq!(find("code").role, SyntaxRole::String);
        assert_eq!(find(" quoted").role, SyntaxRole::Comment);
        assert_eq!(find("-").role, SyntaxRole::Punctuation);
    }

    #[test]
    fn markdown_fenced_code_keeps_embedded_language_roles() {
        let source = "```rust\n// note\npub fn main() { let value = 42; }\n```";
        let lines = source.lines().collect::<Vec<_>>();
        let spans = SyntaxHighlighter::new()
            .highlight_lines_tokens("README.md", &lines)
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();

        for (text, expected) in [
            ("//", SyntaxRole::Comment),
            ("fn", SyntaxRole::Keyword),
            ("main", SyntaxRole::Function),
            ("42", SyntaxRole::Number),
        ] {
            assert!(
                spans
                    .iter()
                    .any(|span| span.content.trim() == text && span.style.role == expected),
                "expected {expected:?} for {text:?}: {spans:?}"
            );
        }
        assert!(
            !spans
                .iter()
                .any(|span| span.content.trim() == "42" && span.style.role == SyntaxRole::String),
            "embedded code must not fall back to the raw-literal role: {spans:?}"
        );
    }

    #[test]
    fn markdown_highlighting_preserves_source_text() {
        let source = "# Heading\n\ntext with `code`\n\n- item\n";
        let lines = source.lines().collect::<Vec<_>>();
        let reconstructed = SyntaxHighlighter::new()
            .highlight_lines_tokens("README.md", &lines)
            .iter()
            .map(|line| {
                line.iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(reconstructed, source.trim_end_matches('\n'));
    }
}
