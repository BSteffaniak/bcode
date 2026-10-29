//! One retained BMUX reader layout: navigation and resize replace, never accumulate.
use std::cell::RefCell;

use bmux_tui::{
    component::{Component, Constraints, EventCx, LayoutCache, LayoutCx, LayoutId, LayoutNode},
    event::{Event, EventOutcome},
    paint::PaintCx,
};
use bmux_tui_components::text_view::TextViewComponent;

#[derive(Default)]
pub(super) struct ReaderLayout {
    cache: LayoutCache,
    key: Option<(u64, Constraints)>,
}

impl ReaderLayout {
    pub(super) fn resolve(
        &mut self,
        view: &TextViewComponent<'_, '_>,
        revision: u64,
        constraints: Constraints,
        cx: &mut LayoutCx,
    ) -> LayoutNode {
        let key = (revision, constraints);
        if self.key != Some(key) {
            self.cache.clear();
            self.key = Some(key);
        }
        self.cache.layout_with_revision(
            LayoutId::new("reader-layout"),
            view,
            revision,
            constraints,
            cx,
        )
    }

    #[cfg(test)]
    pub(super) fn entries(&self) -> usize {
        self.cache.len()
    }
}

pub(super) struct RetainedReader<'a> {
    pub view: TextViewComponent<'a, 'a>,
    pub revision: u64,
    pub layout: &'a RefCell<ReaderLayout>,
}

impl Component for RetainedReader<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.layout
            .borrow_mut()
            .resolve(&self.view, self.revision, constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.view.paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.view.event(event, layout, cx)
    }
}
