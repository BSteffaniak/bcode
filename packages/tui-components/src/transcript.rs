//! Reusable Bcode transcript container composition.

use bmux_tui::chrome::{Border, BorderSet, BorderSides};
use bmux_tui::geometry::Insets;
use bmux_tui::line_frame::{LineFrame, LineFrameGeometry, LineFrameOverflow, LineFrameWidth};
use bmux_tui::prelude::{Line, Style};

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
    /// Horizontal rules above and below.
    Rules,
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

/// One resolved transcript container recipe and semantic style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptContainerStyle {
    /// Bounded layout recipe.
    pub recipe: TranscriptContainerRecipe,
    /// Container background and border style.
    pub style: Style,
    /// Border glyphs for bordered recipes.
    pub border_set: BorderSet,
}

impl TranscriptContainerStyle {
    /// Return the BMUX line frame for this style, or `None` for plain recipes.
    #[must_use]
    pub const fn line_frame(self) -> Option<LineFrame> {
        if matches!(self.recipe.layout, TranscriptContainerLayout::Plain) {
            return None;
        }
        let padding_x = self.recipe.padding_x;
        let padding_y = self.recipe.padding_y;
        let frame = LineFrame::new()
            .padding(Insets::new(padding_y, padding_x, padding_y, padding_x))
            .fill(self.style)
            .width(match self.recipe.width {
                TranscriptContainerWidth::Content => LineFrameWidth::Content,
                TranscriptContainerWidth::Full => LineFrameWidth::Full,
            })
            .overflow(LineFrameOverflow::Ellipsis);
        let sides = match self.recipe.border {
            TranscriptContainerBorder::None => return Some(frame),
            TranscriptContainerBorder::Left => BorderSides::LEFT,
            TranscriptContainerBorder::All => BorderSides::ALL,
            TranscriptContainerBorder::Rules => BorderSides::new(true, false, true, false),
        };
        Some(frame.border(Border::new(self.border_set, self.style).sides(sides)))
    }

    /// Resolve the full-width geometry producers lay out against.
    ///
    /// Content width, content column, and border/padding rows all come from
    /// the same frame that [`apply_transcript_container`] paints with.
    #[must_use]
    pub fn geometry(self, width: u16) -> Option<LineFrameGeometry> {
        let available = usize::from(width.max(1));
        self.line_frame()
            .map(|frame| frame.geometry(available, available))
    }
}

/// Compose one transcript entry through its bounded Bcode container recipe.
///
/// The caller retains ownership of transcript semantics and supplies only the
/// already-rendered entry rows plus resolved presentation.
///
/// Decoration preserves the producer's row count. Residual overflow is
/// truncated, never reflowed: producers must lay out text at the resolved
/// content width.
pub fn apply_transcript_container(
    rows: &mut Vec<Line>,
    start: usize,
    presentation: TranscriptContainerStyle,
    width: u16,
) {
    let Some(frame) = presentation.line_frame() else {
        return;
    };
    if start >= rows.len() {
        return;
    }
    let framed = frame.frame(&rows[start..], usize::from(width.max(1)));
    rows.truncate(start);
    rows.extend(framed.lines);
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::prelude::Span;
    use bmux_tui::style::Color;

    #[test]
    fn panel_recipe_clips_overflow_without_changing_content_row_count() {
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
                border_set: BorderSet::SINGLE,
            },
            8,
        );

        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|line| line.width() <= 8));
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
                border_set: BorderSet::SINGLE,
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
                border_set: BorderSet::SINGLE,
            },
            20,
        );
        assert_eq!(rows, original);
    }
}
