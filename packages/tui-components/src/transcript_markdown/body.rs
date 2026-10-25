//! Retained BMUX component for an accepted Markdown body, without parsing in paint.

use std::hash::{Hash, Hasher};

use bmux_tui::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::selection::{SelectionFragment, SelectionScope};

use super::TranscriptMarkdown;

/// A visible window into a complete accepted Markdown projection.
///
/// Selection fragments are prepared once together with the component, not once
/// per painted row. The host owns selection gestures and canonical copy actions.
pub struct MarkdownBody<'a> {
    id: LayoutId,
    accepted: &'a TranscriptMarkdown,
    top: usize,
    selection_scope: String,
    fragments: Vec<SelectionFragment>,
    order_base: u64,
}

impl<'a> MarkdownBody<'a> {
    /// Bind caller-owned layout and selection identities to accepted source.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        selection_scope: String,
        accepted: &'a TranscriptMarkdown,
        order_base: u64,
    ) -> Self {
        let fragments = crate::markdown_selection::markdown_selection_fragments(
            &selection_scope,
            accepted.content_id(),
            accepted.projection(),
            Point::new(0, 0),
            order_base,
            accepted.revision(),
            None,
        );
        Self {
            id: id.into(),
            accepted,
            top: 0,
            selection_scope,
            fragments,
            order_base,
        }
    }

    /// Select the first body row visible in this component's viewport.
    #[must_use]
    pub const fn top_row(mut self, top: usize) -> Self {
        self.top = top;
        self
    }
}

impl Component for MarkdownBody<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        self.accepted.content_id().hash(&mut hash);
        self.accepted.revision().hash(&mut hash);
        self.accepted
            .projection()
            .layout_signature()
            .hash(&mut hash);
        self.top.hash(&mut hash);
        self.selection_scope.hash(&mut hash);
        self.order_base.hash(&mut hash);
        ComponentRevision::new(hash.finish(), hash.finish())
    }

    fn layout(&self, constraints: Constraints, _: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(
                constraints.max_width(),
                constraints.max_height().unwrap_or_else(|| {
                    u64::try_from(self.accepted.projection().lines.len()).unwrap_or(u64::MAX)
                }),
            )),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let area = Rect::new(0, 0, width, height);
        if area.is_empty() {
            return;
        }
        cx.push_selection_scope(SelectionScope::new(self.selection_scope.clone(), area));
        for (y, line) in (0..height).zip(self.accepted.projection().lines.iter().skip(self.top)) {
            cx.write_line(LocalRect::new(0, i64::from(y), width, 1), line);
        }
        // BMUX translates/clips source fragments and hits through the same context.
        cx.with_child(
            0,
            -i64::try_from(self.top).unwrap_or(i64::MAX),
            LocalRect::new(
                0,
                i64::try_from(self.top).unwrap_or(i64::MAX),
                width,
                height,
            ),
            |cx| {
                for fragment in &self.fragments {
                    cx.push_selection_fragment(fragment.clone());
                }
                for geometry in &self.accepted.projection().geometry {
                    let Some(contribution) = self
                        .accepted
                        .projection()
                        .contributions
                        .iter()
                        .find(|contribution| contribution.id == geometry.contribution_id)
                    else {
                        continue;
                    };
                    if !super::contribution_actionable(&contribution.kind) {
                        continue;
                    }
                    for (index, rect) in geometry.rects.iter().enumerate() {
                        cx.push_hit(
                            HitRegion::new(
                                format!("markdown:{}:{index}", contribution.id),
                                Rect::new(rect.x, rect.y, rect.width, rect.height),
                            )
                            .role(HitRole::Action)
                            .layer(1),
                        );
                    }
                }
            },
        );
    }
}
