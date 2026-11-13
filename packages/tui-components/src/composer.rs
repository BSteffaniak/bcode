//! Bcode composer presentation recipe.

use bmux_tui::chrome::{Border, BorderSet, BorderSides};
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutNode,
};
use bmux_tui::composition::Surface;
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::Insets;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Style};

/// Semantic styles for Bcode's message composer shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposerStyle {
    /// Focused composer border.
    pub border: Style,
    /// Raised composer surface.
    pub surface: Style,
}

/// Border treatment of the composer shell.
///
/// Every variant keeps the same child geometry (one row above and below the
/// editor, two cells on each side), so switching frames never changes the
/// editor's size or the composer's height; only the painted chrome differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerFrame {
    /// A border on all sides with these glyphs, titled `Message`.
    Framed(BorderSet),
    /// A single leading bar drawn with this set's vertical glyph; untitled.
    LeftBar(BorderSet),
    /// Horizontal rules above and below drawn with this set's horizontal glyph; untitled.
    Rules(BorderSet),
    /// No painted border; untitled.
    None,
}

impl Default for ComposerFrame {
    fn default() -> Self {
        Self::Framed(BorderSet::SINGLE)
    }
}

/// Child-owning composer shell. Geometry and event routing are owned by its surface.
pub struct ComposerPanel<'a> {
    surface: Surface<'a>,
    style: ComposerStyle,
    titled: bool,
    prompt: Option<Line>,
}

/// Columns between the composer's left edge and its editor in every frame; a prompt glyph is
/// drawn there.
pub const COMPOSER_LEADING_CELLS: u16 = 2;

/// Compose the message editor inside Bcode's bordered shell.
#[must_use]
pub fn composer_panel<'a>(style: ComposerStyle, child: impl Component + 'a) -> ComposerPanel<'a> {
    ComposerPanel {
        surface: Surface::new(child)
            .id("bcode.composer")
            .background(style.surface)
            .content_style(style.surface),
        style,
        titled: false,
        prompt: None,
    }
    .frame(ComposerFrame::default())
}

impl ComposerPanel<'_> {
    /// Select the border treatment; the default is a single-line frame.
    #[must_use]
    pub fn frame(mut self, frame: ComposerFrame) -> Self {
        let (border, padding, paint, titled) = match frame {
            ComposerFrame::Framed(set) => (
                Border::new(set, self.style.border),
                Insets::new(0, 1, 0, 1),
                true,
                true,
            ),
            ComposerFrame::LeftBar(set) => (
                Border::new(set, self.style.border).sides(BorderSides::LEFT),
                Insets::new(1, 2, 1, 1),
                true,
                false,
            ),
            ComposerFrame::Rules(set) => (
                Border::new(set, self.style.border)
                    .sides(BorderSides::new(true, false, true, false)),
                Insets::new(0, 2, 0, 2),
                true,
                false,
            ),
            // The border keeps its layout insets but is not painted.
            ComposerFrame::None => (
                Border::new(BorderSet::SINGLE, self.style.border),
                Insets::new(0, 1, 0, 1),
                false,
                false,
            ),
        };
        self.surface = self
            .surface
            .border(border)
            .paint_border(paint)
            .padding(padding);
        self.titled = titled;
        self
    }

    /// Hide the `Message` title that full frames draw.
    #[must_use]
    pub const fn untitled(mut self) -> Self {
        self.titled = false;
        self
    }

    /// Draw `prompt` before the editor's first row, in the leading cells every frame keeps.
    /// A prompt wider than [`COMPOSER_LEADING_CELLS`] is not drawn.
    #[must_use]
    pub fn prompt(mut self, prompt: Line) -> Self {
        self.prompt = Some(prompt);
        self
    }

    /// Set caller-owned identity, allowing multiple composers in one retained tree.
    #[must_use]
    pub fn id(mut self, id: impl Into<bmux_tui::component::LayoutId>) -> Self {
        self.surface = self.surface.id(id);
        self
    }
}

impl Component for ComposerPanel<'_> {
    fn revision(&self) -> ComponentRevision {
        self.surface.revision()
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.surface.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.surface.paint(layout, cx);
        if let Some(prompt) = &self.prompt
            && prompt.width() <= usize::from(COMPOSER_LEADING_CELLS)
            && layout.size.height > 2
        {
            cx.write_line(LocalRect::new(0, 1, COMPOSER_LEADING_CELLS, 1), prompt);
        }
        if !self.titled {
            return;
        }
        let title = Line::raw(" Message ");
        let title_width = u16::try_from(title.width()).unwrap_or(u16::MAX);
        cx.write_line_with_fallback_style(
            LocalRect::new(
                1,
                0,
                title_width
                    .min(u16::try_from(layout.size.width.saturating_sub(2)).unwrap_or(u16::MAX)),
                1,
            ),
            &title,
            self.style.surface.patch(self.style.border),
        );
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.surface.event(event, layout, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::composition::TextBlock;
    use bmux_tui::geometry::Size;

    #[test]
    fn composer_title_preserves_remaining_top_border() {
        use bmux_tui::buffer::Buffer;
        use bmux_tui::frame::Frame;
        use bmux_tui::geometry::Rect;
        for width in [1, 2, 5, 20, 80] {
            let panel = composer_panel(
                ComposerStyle {
                    border: Style::new(),
                    surface: Style::new(),
                },
                TextBlock::new("editor"),
            );
            let layout = panel.layout(
                Constraints::tight(Size::new(width, 3)),
                &mut LayoutCx::new(),
            );
            let mut buffer = Buffer::empty(Rect::new(0, 0, width, 3));
            panel.paint(&layout, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
            if width >= 2 {
                let title = " Message ";
                let interior = usize::from(width - 2);
                let title_len = title.len().min(interior);
                let expected = format!(
                    "┌{}{}┐",
                    &title[..title_len],
                    "─".repeat(interior - title_len)
                );
                assert_eq!(buffer.row_symbols(0).expect("top row"), expected);
            }
        }
    }

    #[test]
    fn composer_surface_inherits_background_into_editor_and_title() {
        use bmux_tui::buffer::Buffer;
        use bmux_tui::frame::Frame;
        use bmux_tui::geometry::{Point, Rect};
        use bmux_tui::style::Color;
        let surface = Style::new().bg(Color::Blue);
        let editor = TextBlock::new("editor").style(Style::new().fg(Color::White));
        let panel = composer_panel(
            ComposerStyle {
                border: Style::new().fg(Color::Cyan),
                surface,
            },
            editor,
        );
        let area = Rect::new(0, 0, 24, 5);
        let layout = panel.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(area);
        panel.paint(&layout, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
        for y in 0..area.height {
            for x in 0..area.width {
                assert_eq!(buffer.get(Point::new(x, y)).unwrap().style.bg, surface.bg);
            }
        }
    }

    #[test]
    fn composer_panel_preserves_one_cell_horizontal_padding() {
        let panel = composer_panel(
            ComposerStyle {
                border: Style::new(),
                surface: Style::new(),
            },
            TextBlock::new("editor"),
        );
        let layout = panel.layout(Constraints::tight(Size::new(20, 5)), &mut LayoutCx::new());
        assert_eq!(layout.children[0].x, 2);
        assert_eq!(layout.children[0].y, 1);
        assert_eq!(layout.children[0].node.size.width, 16);
        assert_eq!(layout.children[0].node.size.height, 3);
    }

    fn painted(frame: ComposerFrame, width: u16, height: u16) -> Vec<String> {
        use bmux_tui::buffer::Buffer;
        use bmux_tui::frame::Frame;
        use bmux_tui::geometry::Rect;
        let panel = composer_panel(
            ComposerStyle {
                border: Style::new(),
                surface: Style::new(),
            },
            TextBlock::new(""),
        )
        .frame(frame);
        let layout = panel.layout(
            Constraints::tight(Size::new(width, height)),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        panel.paint(&layout, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
        (0..height)
            .map(|y| buffer.row_symbols(y).expect("row"))
            .collect()
    }

    #[test]
    fn every_composer_frame_keeps_the_editor_geometry() {
        let frames = [
            ComposerFrame::Framed(BorderSet::SINGLE),
            ComposerFrame::Framed(BorderSet::ROUNDED),
            ComposerFrame::LeftBar(BorderSet::SINGLE),
            ComposerFrame::Rules(BorderSet::SINGLE),
            ComposerFrame::None,
        ];
        for width in [0, 1, 3, 4, 5, 20] {
            let geometry = |frame| {
                let panel = composer_panel(
                    ComposerStyle {
                        border: Style::new(),
                        surface: Style::new(),
                    },
                    TextBlock::new("editor"),
                )
                .frame(frame);
                let layout = panel.layout(
                    Constraints::tight(Size::new(width, 5)),
                    &mut LayoutCx::new(),
                );
                let child = &layout.children[0];
                (child.x, child.y, child.node.size)
            };
            let expected = geometry(ComposerFrame::default());
            for frame in frames {
                assert_eq!(geometry(frame), expected, "{frame:?} at width {width}");
            }
        }
    }

    #[test]
    fn rounded_composer_frame_paints_rounded_corners_and_title() {
        let rows = painted(ComposerFrame::Framed(BorderSet::ROUNDED), 14, 3);
        assert_eq!(rows[0], "╭ Message ───╮");
        assert_eq!(rows[2], "╰────────────╯");
    }

    #[test]
    fn left_bar_composer_frame_paints_only_the_leading_bar() {
        for width in [1, 3, 12] {
            let rows = painted(ComposerFrame::LeftBar(BorderSet::SINGLE), width, 3);
            for row in &rows {
                let mut cells = row.chars();
                assert_eq!(cells.next(), Some('│'), "width {width}: {rows:?}");
                assert!(cells.all(|cell| cell == ' '), "width {width}: {rows:?}");
            }
        }
    }

    #[test]
    fn borderless_composer_frame_paints_no_border_or_title() {
        for width in [1, 3, 12] {
            let rows = painted(ComposerFrame::None, width, 3);
            assert!(
                rows.iter().all(|row| row.chars().all(|cell| cell == ' ')),
                "width {width}: {rows:?}"
            );
        }
    }

    #[test]
    fn rules_composer_frame_paints_full_width_rules_only() {
        let rows = painted(ComposerFrame::Rules(BorderSet::SINGLE), 12, 3);
        assert_eq!(rows[0], "─".repeat(12));
        assert_eq!(rows[1], " ".repeat(12));
        assert_eq!(rows[2], "─".repeat(12));
    }

    fn prompted(prompt: &str, width: u16) -> Vec<String> {
        use bmux_tui::buffer::Buffer;
        use bmux_tui::frame::Frame;
        use bmux_tui::geometry::Rect;
        let panel = composer_panel(
            ComposerStyle {
                border: Style::new(),
                surface: Style::new(),
            },
            TextBlock::new("hi"),
        )
        .frame(ComposerFrame::Rules(BorderSet::SINGLE))
        .prompt(Line::raw(prompt));
        let layout = panel.layout(
            Constraints::tight(Size::new(width, 3)),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, 3));
        panel.paint(&layout, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
        (0..3)
            .map(|y| buffer.row_symbols(y).expect("row"))
            .collect()
    }

    #[test]
    fn prompt_occupies_the_leading_cells_of_the_first_editor_row() {
        assert!(prompted("❯ ", 12)[1].starts_with("❯ hi"));
        // A wide prompt is dropped instead of overlapping the editor.
        assert!(prompted(">>> ", 12)[1].starts_with("  hi"));
        // CJK and emoji measure two cells each: one fits, two do not.
        assert!(prompted("中", 12)[1].starts_with("中hi"));
        assert!(prompted("🦀🦀", 12)[1].starts_with("  hi"));
    }
}
