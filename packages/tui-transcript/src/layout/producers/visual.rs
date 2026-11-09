//! Plugin visual producer (L8, A8, E1–E6) and rendered-row components.
//!
//! Plugin visual adapters run host-side, outside drawing (D9): the host resolves the adapter
//! (E1), renders it with the unit's resolved presentation (E6), validates and bounds the rows,
//! and accepts them into [`crate::layout::PluginVisualState`] for an exact item revision. This
//! producer only consumes accepted rows:
//!
//! * With an accepted visual, the unit renders through the tool anatomy
//!   ([`super::tool::compose`]): the host header (lifecycle, timing, disclosure marker), then a
//!   `visual` section holding the adapter rows, bounded by the section preview and clipped at
//!   the body width (adapters render at that width; rows are never reflowed), then the host
//!   `error` section when the call failed. The rows are one rendered selection fragment and
//!   one producer-namespaced content key region (`<adapter>:visual`, E3) that never
//!   authorizes anything. Adapter rows carry no source correspondence here, so anchors in
//!   them fall back to item rows (E3: older contracts fall back to item level).
//! * Without one (adapter missing, disabled, failed or not yet accepted), the unit renders the
//!   renderer-neutral canonical tool fallback of [`super::tool::ToolProducer`] (A8, E1 "canonical
//!   fallback last", E5).

use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutNode,
};
use bmux_tui::composition::TextBlock;
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::paint::PaintCx;

use super::tool::{self, Section, SectionBody};
use super::{ItemContent, ItemRenderContext, ProducedItem, UnitProducer, UnitView, common};
use crate::layout::geometry::{ContentKeyRegion, ItemGeometry, ItemSelectionFragment};

/// Rows copied as rendered text (no source mapping: adapter rows, argument fields, metadata):
/// one rendered selection fragment, optionally one content key region over the same rows.
#[derive(Debug, Clone)]
pub struct VisualRows {
    block: TextBlock,
    key: Option<String>,
}

impl VisualRows {
    /// Rendered rows of `block`.
    #[must_use]
    pub const fn new(block: TextBlock) -> Self {
        Self { block, key: None }
    }

    /// Return the rows covered by the producer-namespaced content `key` (E3).
    #[must_use]
    pub fn keyed(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }
}

impl Component for VisualRows {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.block.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.block.paint(layout, cx);
    }

    fn revision(&self) -> ComponentRevision {
        self.block.revision()
    }
}

impl ItemContent for VisualRows {
    fn geometry(&self, layout: &LayoutNode) -> ItemGeometry {
        let height = layout.size.height;
        if height == 0 {
            return ItemGeometry::default();
        }
        ItemGeometry {
            selection_fragments: vec![ItemSelectionFragment {
                rows: 0..height,
                source: None,
                rendered: true,
            }],
            content_keys: self
                .key
                .iter()
                .map(|key| ContentKeyRegion {
                    key: key.clone(),
                    rows: 0..height,
                })
                .collect(),
            ..ItemGeometry::default()
        }
    }
}

/// Any item content additionally covered by one producer-namespaced content key region (E3).
pub struct ContentKeyed {
    inner: Box<dyn ItemContent>,
    key: String,
}

impl std::fmt::Debug for ContentKeyed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentKeyed")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl ContentKeyed {
    /// `inner` covered by `key`.
    #[must_use]
    pub fn new(inner: Box<dyn ItemContent>, key: impl Into<String>) -> Self {
        Self {
            inner,
            key: key.into(),
        }
    }
}

impl Component for ContentKeyed {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.inner.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.inner.paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.inner.event(event, layout, cx)
    }

    fn revision(&self) -> ComponentRevision {
        self.inner.revision()
    }
}

impl ItemContent for ContentKeyed {
    fn geometry(&self, layout: &LayoutNode) -> ItemGeometry {
        let mut geometry = self.inner.geometry(layout);
        if layout.size.height > 0 {
            geometry.content_keys.push(ContentKeyRegion {
                key: self.key.clone(),
                rows: 0..layout.size.height,
            });
        }
        geometry
    }
}

/// Content key of an adapter's visual rows (namespaced by the adapter, E3).
#[must_use]
pub fn visual_key(adapter: &str) -> String {
    format!("{adapter}:visual")
}

/// Tool units with an accepted plugin visual; the canonical tool fallback otherwise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PluginVisualProducer;

impl UnitProducer for PluginVisualProducer {
    fn produce(&self, unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
        let Some(item) = unit.item else {
            return ProducedItem::new(common::plain_unit(unit));
        };
        let Some(visual) = cx.plugin_visuals.accepted(&item.id) else {
            return tool::ToolProducer.produce(unit, cx);
        };
        let mut anatomy = tool::tool_anatomy(item, tool::unit_facts(cx));
        // The adapter renders the call's arguments and result; the host keeps lifecycle chrome
        // and the failure notice.
        anatomy.sections.retain(|section| section.name == "error");
        anatomy.sections.insert(
            0,
            Section {
                name: "visual".to_owned(),
                label: None,
                body: SectionBody::Rows {
                    rows: visual.rows.clone(),
                    key: visual_key(&visual.adapter),
                },
            },
        );
        tool::compose(anatomy, &item.id, cx)
    }
}

#[cfg(test)]
mod tests;
