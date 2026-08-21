//! Bcode syntax adapter for the generic BMUX diff viewer.

#[cfg(feature = "syntax")]
use bcode_syntax_render::{SyntaxHighlighter, SyntaxPalette, SyntaxStyle};
use bmux_tui::prelude::Line;
#[cfg(feature = "syntax")]
use bmux_tui::prelude::{Modifier, Style};

pub use bmux_tui_components::diff_viewer::{
    ChangedRange, DiffDocument, DiffLine, DiffLineKind, DiffSyntaxSpan, DiffViewerLayout,
    DiffViewerStyle,
};

/// Derive generic diff-viewer styles from a component theme.
#[must_use]
pub fn diff_viewer_style(theme: bmux_tui_components::theme::ComponentTheme) -> DiffViewerStyle {
    theme.into()
}

/// Input used to render Bcode diff viewer rows.
#[derive(Debug, Clone, Copy)]
pub struct DiffViewerInput<'a> {
    pub label: &'a str,
    pub old_text: &'a str,
    pub new_text: &'a str,
    pub old_start_line: u32,
    pub new_start_line: u32,
    pub line_numbers_known: bool,
    pub title: &'a str,
    pub subtitle: Option<&'a str>,
    pub argument_bytes: Option<usize>,
    pub truncated: bool,
    #[cfg(feature = "syntax")]
    pub syntax_palette: Option<SyntaxPalette>,
    pub layout: DiffViewerLayout,
}

/// Build a Bcode diff document from old and new text.
#[must_use]
pub fn diff_from_text(label: &str, old_text: &str, new_text: &str) -> DiffDocument {
    diff_from_text_at_lines(label, old_text, new_text, 1, 1)
}

/// Build a Bcode diff document whose fragments begin at the given file lines.
#[must_use]
pub fn diff_from_text_at_lines(
    label: &str,
    old_text: &str,
    new_text: &str,
    old_start_line: u32,
    new_start_line: u32,
) -> DiffDocument {
    diff_from_text_at_lines_with_palette(
        label,
        old_text,
        new_text,
        old_start_line,
        new_start_line,
        #[cfg(feature = "syntax")]
        None,
    )
}

/// Build a line-offset Bcode diff document with an optional syntax palette.
#[must_use]
pub fn diff_from_text_at_lines_with_palette(
    label: &str,
    old_text: &str,
    new_text: &str,
    old_start_line: u32,
    new_start_line: u32,
    #[cfg(feature = "syntax")] syntax_palette: Option<SyntaxPalette>,
) -> DiffDocument {
    let document = bmux_tui_components::diff_viewer::diff_from_text_at_lines(
        label,
        old_text,
        new_text,
        old_start_line,
        new_start_line,
    );
    #[cfg(feature = "syntax")]
    let mut document = document;
    #[cfg(feature = "syntax")]
    apply_syntax(label, &mut document, syntax_palette);
    document
}

/// Render Bcode diff viewer rows.
#[must_use]
pub fn diff_viewer_rows(input: DiffViewerInput<'_>, width: u16) -> Vec<Line> {
    diff_viewer_rows_with_style(input, width, DiffViewerStyle::default())
}

/// Render Bcode diff rows with caller-supplied semantic styles.
#[must_use]
pub fn diff_viewer_rows_with_style(
    input: DiffViewerInput<'_>,
    width: u16,
    style: DiffViewerStyle,
) -> Vec<Line> {
    let document = diff_from_text_at_lines_with_palette(
        input.label,
        input.old_text,
        input.new_text,
        input.old_start_line,
        input.new_start_line,
        #[cfg(feature = "syntax")]
        input.syntax_palette,
    );
    bmux_tui_components::diff_viewer::diff_viewer_document_rows_with_style(
        generic_input(input),
        document,
        width,
        style,
    )
}

const fn generic_input(
    input: DiffViewerInput<'_>,
) -> bmux_tui_components::diff_viewer::DiffViewerInput<'_> {
    bmux_tui_components::diff_viewer::DiffViewerInput {
        label: input.label,
        old_text: input.old_text,
        new_text: input.new_text,
        old_start_line: input.old_start_line,
        new_start_line: input.new_start_line,
        line_numbers_known: input.line_numbers_known,
        title: input.title,
        subtitle: input.subtitle,
        argument_bytes: input.argument_bytes,
        truncated: input.truncated,
        layout: input.layout,
    }
}

#[cfg(feature = "syntax")]
fn apply_syntax(label: &str, document: &mut DiffDocument, palette: Option<SyntaxPalette>) {
    let highlighter = palette.map_or_else(SyntaxHighlighter::new, SyntaxHighlighter::with_palette);
    if !highlighter.can_highlight(label) {
        return;
    }
    for line in document.lines.iter_mut().filter(|line| {
        matches!(
            line.kind,
            DiffLineKind::Context | DiffLineKind::Added | DiffLineKind::Removed
        )
    }) {
        line.syntax_spans = highlighter
            .highlight_line_tokens(label, &line.content)
            .into_iter()
            .map(|span| DiffSyntaxSpan {
                content: span.content,
                style: syntax_style(span.style),
            })
            .collect();
    }
}

#[cfg(feature = "syntax")]
const fn syntax_style(style: SyntaxStyle) -> Style {
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
    use super::*;

    #[cfg(feature = "syntax")]
    #[test]
    fn semantic_palette_styles_generic_diff_spans() {
        use bcode_syntax_render::SyntaxColor;
        use bmux_tui::prelude::Color;

        let color = SyntaxColor::rgb(11, 12, 13);
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
            heading: color,
            link: color,
            raw: color,
        };
        let document = diff_from_text_at_lines_with_palette(
            "file.rs",
            "fn old() {}",
            "fn new() {}",
            1,
            1,
            Some(palette),
        );

        assert!(
            document
                .lines
                .iter()
                .flat_map(|line| &line.syntax_spans)
                .any(|span| { span.style.fg == Some(Color::Rgb(11, 12, 13)) })
        );
    }

    #[test]
    fn generic_diff_mechanics_preserve_change_counts() {
        let document = diff_from_text("file.txt", "old", "new");
        assert_eq!(document.added, 1);
        assert_eq!(document.removed, 1);
        assert!(
            document
                .lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Added)
        );
        assert!(
            document
                .lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Removed)
        );
    }
}
