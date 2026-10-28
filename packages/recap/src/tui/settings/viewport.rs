//! A settings viewport sized from its allocated BMUX constraints.
use bmux_tui::{
    component::{Component, Constraints, Element, EventCx, LayoutCx, LayoutNode, LogicalSize},
    event::{Event, EventOutcome},
    paint::PaintCx,
};
use bmux_tui_components::scroll_view::{ScrollViewComponent, ScrollViewState};
use std::cell::Cell;

pub(super) struct SettingsViewport<'a> {
    pub content: Element<'a>,
    pub state: &'a Cell<ScrollViewState>,
    pub focus: &'a str,
}

// Borrow the composed form rather than reimplementing measurement, paint or hit geometry.
struct Content<'a>(&'a Element<'a>);
impl Component for Content<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.0.layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.0.paint(layout, cx);
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.0.event(event, layout, cx)
    }
}
impl SettingsViewport<'_> {
    fn control(&self, size: LogicalSize) -> ScrollViewComponent<'_> {
        ScrollViewComponent::new(
            "settings.viewport",
            size,
            self.state.get(),
            Content(&self.content),
        )
        .retain_state(self.state)
        .reveal(self.focus.to_owned())
    }
}
impl Component for SettingsViewport<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.control(LogicalSize::new(
            constraints.max_width(),
            constraints.max_height().unwrap_or(0),
        ))
        .layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.control(layout.size).paint(layout, cx);
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.control(layout.size).event(event, layout, cx)
    }
}
