//! Bcode composer presentation recipe.

use bmux_tui::chrome::Border;
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

/// Child-owning composer shell. Geometry and event routing are owned by its surface.
pub struct ComposerPanel<'a> {
    surface: Surface<'a>,
    title_style: Style,
}

/// Compose the message editor inside Bcode's bordered shell.
#[must_use]
pub fn composer_panel<'a>(style: ComposerStyle, child: impl Component + 'a) -> ComposerPanel<'a> {
    ComposerPanel {
        surface: Surface::new(child)
            .id("bcode.composer")
            .border(Border::single().style(style.border))
            .padding(Insets::new(0, 1, 0, 1))
            .background(style.surface)
            .content_style(style.surface),
        title_style: style.surface.patch(style.border),
    }
}

impl ComposerPanel<'_> {
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
            self.title_style,
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
}
