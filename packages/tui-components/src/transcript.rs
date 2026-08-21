//! Reusable Bcode transcript container composition.

use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::text_width::display_width;

/// Supported Bcode transcript container layout families.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum TranscriptContainerLayout {
    /// No dedicated container chrome.
    #[default]
    Plain,
    /// One status-colored leading bar.
    LeftBar,
    /// Bordered/background panel.
    Panel,
}

/// Supported Bcode transcript container width behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum TranscriptContainerWidth {
    /// Paint only the content width.
    #[default]
    Content,
    /// Paint the full available width.
    Full,
}

/// Supported Bcode transcript border placement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum TranscriptContainerBorder {
    /// No border.
    #[default]
    None,
    /// Left border only.
    Left,
    /// Single-line border on all sides.
    All,
}

/// Bounded Bcode transcript container recipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TranscriptContainerRecipe {
    /// Container layout family.
    pub layout: TranscriptContainerLayout,
    /// Width/fill behavior.
    pub width: TranscriptContainerWidth,
    /// Border placement.
    pub border: TranscriptContainerBorder,
    /// Horizontal padding in terminal cells.
    pub padding_x: u16,
    /// Vertical padding in terminal cells.
    pub padding_y: u16,
}

/// How overflowing container content must be re-wrapped.
///
/// This re-wrap is a defensive clamp: entry producers already wrap to the
/// resolved content width, so it only applies to rows that still overflow.
/// `ColumnExact` is the safe default because reflowing already-wrapped rows at
/// word boundaries would discard the producer's chosen line breaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum TranscriptContainerContent {
    /// Column-significant or already-wrapped rows; clamp at grapheme boundaries.
    #[default]
    ColumnExact,
    /// Unwrapped prose supplied directly to the container; wrap at word
    /// boundaries.
    Prose,
}

impl TranscriptContainerContent {
    /// Return the wrapping policy for this content kind.
    #[must_use]
    pub const fn wrap(self) -> bmux_tui::text::TextWrap {
        match self {
            Self::Prose => bmux_tui::text::TextWrap::Word,
            Self::ColumnExact => bmux_tui::text::TextWrap::Character,
        }
    }
}

/// One resolved transcript container recipe and semantic style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptContainerStyle {
    /// Bounded layout recipe.
    pub recipe: TranscriptContainerRecipe,
    /// Container background and border style.
    pub style: Style,
}

/// Compose one transcript entry through its bounded Bcode container recipe.
///
/// The caller retains ownership of transcript semantics and supplies only the
/// already-rendered entry rows plus resolved presentation.
///
/// Rows that still overflow the resolved interior are clamped at grapheme
/// boundaries, preserving the line breaks the entry producer already chose. Use
/// [`apply_transcript_container_with_content`] to word-wrap unwrapped prose.
pub fn apply_transcript_container(
    rows: &mut Vec<Line>,
    start: usize,
    presentation: TranscriptContainerStyle,
    width: u16,
) {
    apply_transcript_container_with_content(
        rows,
        start,
        presentation,
        width,
        TranscriptContainerContent::ColumnExact,
    );
}

/// Compose one transcript entry, choosing how overflow is re-wrapped.
pub fn apply_transcript_container_with_content(
    rows: &mut Vec<Line>,
    start: usize,
    presentation: TranscriptContainerStyle,
    width: u16,
    content: TranscriptContainerContent,
) {
    if matches!(presentation.recipe.layout, TranscriptContainerLayout::Plain) || start >= rows.len()
    {
        return;
    }

    let available_width = usize::from(width.max(1));
    let left_border_width = usize::from(!matches!(
        presentation.recipe.border,
        TranscriptContainerBorder::None
    ));
    let right_border_width = usize::from(matches!(
        presentation.recipe.border,
        TranscriptContainerBorder::All
    ));
    let horizontal_padding = usize::from(presentation.recipe.padding_x);
    let natural_width = rows[start..]
        .iter()
        .map(line_width)
        .max()
        .unwrap_or_default()
        .saturating_add(horizontal_padding.saturating_mul(2))
        .saturating_add(left_border_width)
        .saturating_add(right_border_width)
        .max(1);
    let container_width = match presentation.recipe.width {
        TranscriptContainerWidth::Full => available_width,
        TranscriptContainerWidth::Content => natural_width.min(available_width),
    };
    let interior_width = container_width
        .saturating_sub(left_border_width)
        .saturating_sub(right_border_width);
    let left_padding_width = horizontal_padding.min(interior_width);
    let content_width = interior_width
        .saturating_sub(left_padding_width)
        .saturating_sub(horizontal_padding.min(interior_width - left_padding_width));

    let mut container_rows = Vec::new();
    if matches!(presentation.recipe.border, TranscriptContainerBorder::All) {
        container_rows.push(container_border_line(
            container_width,
            '┌',
            '─',
            '┐',
            presentation.style,
        ));
    }
    let vertical_padding = usize::from(presentation.recipe.padding_y);
    for _ in 0..vertical_padding {
        container_rows.push(container_content_line(
            &Line::new(),
            content_width,
            left_padding_width,
            interior_width,
            presentation,
        ));
    }
    for line in rows.drain(start..) {
        let wrapped = if content_width == 0 {
            vec![Line::new()]
        } else {
            line.wrap(
                bmux_tui::text::TextWrapGeometry::uniform(content_width),
                content.wrap(),
            )
        };
        for line in wrapped {
            container_rows.push(container_content_line(
                &line,
                content_width,
                left_padding_width,
                interior_width,
                presentation,
            ));
        }
    }
    for _ in 0..vertical_padding {
        container_rows.push(container_content_line(
            &Line::new(),
            content_width,
            left_padding_width,
            interior_width,
            presentation,
        ));
    }
    if matches!(presentation.recipe.border, TranscriptContainerBorder::All) {
        container_rows.push(container_border_line(
            container_width,
            '└',
            '─',
            '┘',
            presentation.style,
        ));
    }
    rows.extend(container_rows);
}

fn container_content_line(
    line: &Line,
    content_width: usize,
    left_padding_width: usize,
    interior_width: usize,
    presentation: TranscriptContainerStyle,
) -> Line {
    let mut spans = Vec::new();
    if !matches!(presentation.recipe.border, TranscriptContainerBorder::None) {
        spans.push(Span::styled("│", presentation.style));
    }
    spans.push(Span::styled(
        " ".repeat(left_padding_width),
        presentation.style,
    ));
    let line = line
        .with_fallback_style(presentation.style)
        .truncate(content_width);
    let used_width = line_width(&line);
    spans.extend(line.spans);
    spans.push(Span::styled(
        " ".repeat(
            interior_width
                .saturating_sub(left_padding_width)
                .saturating_sub(used_width),
        ),
        presentation.style,
    ));
    if matches!(presentation.recipe.border, TranscriptContainerBorder::All) {
        spans.push(Span::styled("│", presentation.style));
    }
    Line::from_spans(spans)
}

fn container_border_line(width: usize, left: char, fill: char, right: char, style: Style) -> Line {
    let text = match width {
        0 => String::new(),
        1 => left.to_string(),
        _ => format!("{left}{}{right}", fill.to_string().repeat(width - 2)),
    };
    Line::from_spans(vec![Span::styled(text, style)])
}

fn line_width(line: &Line) -> usize {
    line.spans
        .iter()
        .map(|span| display_width(&span.content))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::style::Color;

    #[test]
    fn panel_recipe_wraps_and_stays_bounded() {
        let mut rows = vec![Line::from("abcdefgh")];
        apply_transcript_container(
            &mut rows,
            0,
            TranscriptContainerStyle {
                recipe: TranscriptContainerRecipe {
                    layout: TranscriptContainerLayout::Panel,
                    width: TranscriptContainerWidth::Full,
                    border: TranscriptContainerBorder::All,
                    padding_x: 1,
                    padding_y: 0,
                },
                style: Style::new().bg(Color::Blue),
            },
            8,
        );

        assert!(rows.len() > 3);
        assert!(rows.iter().all(|line| line_width(line) <= 8));
        assert_eq!(
            rows.first().map(Line::plain_text).as_deref(),
            Some("┌──────┐")
        );
        assert_eq!(
            rows.last().map(Line::plain_text).as_deref(),
            Some("└──────┘")
        );
    }

    #[test]
    fn left_bar_recipe_preserves_semantic_content_styles() {
        let content_style = Style::new().fg(Color::Green);
        let container_style = Style::new().bg(Color::Blue);
        let mut rows = vec![Line::from_spans(vec![Span::styled("body", content_style)])];
        apply_transcript_container(
            &mut rows,
            0,
            TranscriptContainerStyle {
                recipe: TranscriptContainerRecipe {
                    layout: TranscriptContainerLayout::LeftBar,
                    width: TranscriptContainerWidth::Content,
                    border: TranscriptContainerBorder::Left,
                    padding_x: 1,
                    padding_y: 0,
                },
                style: container_style,
            },
            20,
        );

        assert_eq!(rows[0].plain_text(), "│ body ");
        assert_eq!(rows[0].spans[2].style.fg, content_style.fg);
        assert_eq!(rows[0].spans[2].style.bg, container_style.bg);
    }

    #[test]
    fn plain_recipe_does_not_modify_rows() {
        let mut rows = vec![Line::from("body")];
        let original = rows.clone();
        apply_transcript_container(
            &mut rows,
            0,
            TranscriptContainerStyle {
                recipe: TranscriptContainerRecipe {
                    layout: TranscriptContainerLayout::Plain,
                    width: TranscriptContainerWidth::Content,
                    border: TranscriptContainerBorder::None,
                    padding_x: 0,
                    padding_y: 0,
                },
                style: Style::new(),
            },
            20,
        );
        assert_eq!(rows, original);
    }
}
