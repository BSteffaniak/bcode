//! The identity-keyed item layout cache (L1, L6, L9, VP6) and layout work counters (9.2).
//!
//! Each resident unit keeps one retained [`ItemLayout`] keyed by `(UnitKey, revision,
//! LayoutFingerprint, width)`: the producer's component, the BMUX `LayoutNode` it measured to,
//! and the geometry derived from that node. The virtual list uses the retained node for
//! measurement, painting and events (BMUX 6 `retained_layout`), so no item is laid out twice
//! and scrolling performs no producer or layout work (D4). Inserting, removing or reordering
//! units never invalidates other units (L1).

use std::collections::BTreeMap;

use bcode_session_view_models::TranscriptViewItem;
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutNode, LogicalRect,
};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::Line;
use bmux_tui_components::virtual_list::SourceMap;

use super::SourcePosition;
use super::context::{ActivityPhase, LayoutFingerprint, RenderConfig};
use super::geometry::{ItemGeometry, PaintSlotKind};
use super::producers::{ItemContent, ProducerWork, UnitProducer, UnitView, common};
use crate::units::structure::StructureResolver;
use crate::units::{UnitKey, Units};

/// Cache key of one retained layout (L1).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemLayoutKey {
    /// Unit identity.
    pub unit: UnitKey,
    /// Unit layout revision.
    pub revision: u64,
    /// Layout-affecting inputs other than width.
    pub fingerprint: LayoutFingerprint,
    /// Layout width in cells.
    pub width: u64,
}

/// Retained exact layout of one unit at one width: one layout definition for height, paint,
/// hits, focus, selection and source mapping (B3, L6).
pub struct ItemLayout {
    /// Exact cache key.
    pub key: ItemLayoutKey,
    /// Exact height in rows.
    pub height: u64,
    /// Retained BMUX layout node.
    pub node: LayoutNode,
    /// Geometry derived from `node`.
    pub geometry: ItemGeometry,
    content: Box<dyn ItemContent>,
}

impl std::fmt::Debug for ItemLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ItemLayout")
            .field("key", &self.key)
            .field("height", &self.height)
            .field("geometry", &self.geometry)
            .finish_non_exhaustive()
    }
}

impl ItemLayout {
    /// The retained component.
    #[must_use]
    pub fn content(&self) -> &dyn ItemContent {
        self.content.as_ref()
    }
}

/// Layout work observed since the counters were last taken (spec section 9.2, D4).
///
/// Scroll-only frames keep every counter at zero; a prepend counts only the new units.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LayoutWorkCounters {
    /// Markdown source parses.
    pub markdown_parses: u64,
    /// Syntax highlighting runs.
    pub highlights: u64,
    /// Markdown projections built.
    pub projections: u64,
    /// Unit measurements (retained layouts computed).
    pub measurements: u64,
    /// Producer component builds.
    pub producer_builds: u64,
}

impl LayoutWorkCounters {
    const fn add_work(&mut self, work: ProducerWork) {
        self.markdown_parses = self.markdown_parses.saturating_add(work.markdown_parses);
        self.highlights = self.highlights.saturating_add(work.highlights);
        self.projections = self.projections.saturating_add(work.projections);
    }

    /// Whether every counter is zero.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.markdown_parses == 0
            && self.highlights == 0
            && self.projections == 0
            && self.measurements == 0
            && self.producer_builds == 0
    }
}

/// Identity-keyed cache of retained unit layouts; the only source of unit heights (VP6).
#[derive(Debug, Default)]
pub struct ItemLayoutCache {
    entries: BTreeMap<UnitKey, ItemLayout>,
    counters: LayoutWorkCounters,
}

impl ItemLayoutCache {
    /// Create an empty cache.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            counters: LayoutWorkCounters {
                markdown_parses: 0,
                highlights: 0,
                projections: 0,
                measurements: 0,
                producer_builds: 0,
            },
        }
    }

    /// Retained layout for an exact key, if cached.
    #[must_use]
    pub fn get(&self, key: &ItemLayoutKey) -> Option<&ItemLayout> {
        self.entries
            .get(&key.unit)
            .filter(|layout| &layout.key == key)
    }

    /// The unit's current retained layout, whatever its key.
    #[must_use]
    pub fn current(&self, unit: &UnitKey) -> Option<&ItemLayout> {
        self.entries.get(unit)
    }

    /// Exact height of `view` under `cx` at revision `revision`, laying it out only when no
    /// retained layout matches `(unit, revision, cx.fingerprint(), cx.width)` (L1). A miss
    /// builds the producer's component once, lays it out once with
    /// `Constraints::for_width(cx.width)`, derives its geometry from that node, and replaces
    /// the unit's previous layout.
    pub fn measure(
        &mut self,
        view: UnitView<'_>,
        revision: u64,
        cx: &super::ItemRenderContext<'_>,
        producer: &dyn UnitProducer,
    ) -> u64 {
        let key = ItemLayoutKey {
            unit: view.unit.key.clone(),
            revision,
            fingerprint: cx.fingerprint(),
            width: cx.width,
        };
        if let Some(layout) = self.get(&key) {
            return layout.height;
        }
        let output = producer.produce(view, cx);
        self.counters.producer_builds = self.counters.producer_builds.saturating_add(1);
        self.counters.add_work(output.work);
        let node = output
            .content
            .layout(Constraints::for_width(cx.width), &mut LayoutCx::new());
        self.counters.measurements = self.counters.measurements.saturating_add(1);
        let geometry = output.content.geometry(&node);
        let height = node.size.height;
        self.entries.insert(
            key.unit.clone(),
            ItemLayout {
                key,
                height,
                node,
                geometry,
                content: output.content,
            },
        );
        height
    }

    /// Drop the retained layout of a unit (it left the resident window).
    pub fn remove_unit(&mut self, unit: &UnitKey) {
        self.entries.remove(unit);
    }

    /// Keep only layouts of units `keep` accepts.
    pub fn retain(&mut self, keep: impl Fn(&UnitKey) -> bool) {
        self.entries.retain(|unit, _| keep(unit));
    }

    /// Number of retained layouts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Layout work since the counters were last taken.
    #[must_use]
    pub const fn counters(&self) -> LayoutWorkCounters {
        self.counters
    }

    /// Return the layout work since the counters were last taken and reset them.
    pub const fn take_counters(&mut self) -> LayoutWorkCounters {
        std::mem::replace(
            &mut self.counters,
            LayoutWorkCounters {
                markdown_parses: 0,
                highlights: 0,
                projections: 0,
                measurements: 0,
                producer_builds: 0,
            },
        )
    }
}

/// Source mapping for source-backed anchors (P3, BMUX 10), read from retained geometry.
impl SourceMap<UnitKey, SourcePosition> for ItemLayoutCache {
    fn row_for_source(&self, key: &UnitKey, position: &SourcePosition) -> Option<u64> {
        self.entries
            .get(key)?
            .geometry
            .source_map
            .row_for_source(position)
    }

    fn source_for_row(&self, key: &UnitKey, row: u64) -> Option<(SourcePosition, u64)> {
        self.entries
            .get(key)?
            .geometry
            .source_map
            .source_for_row(row)
    }
}

/// Everything unit measurement reads besides the cache itself.
#[derive(Clone, Copy)]
pub struct LayoutInputs<'a> {
    /// Current units.
    pub units: &'a Units,
    /// The document the units were derived from.
    pub items: &'a [TranscriptViewItem],
    /// Resolved structure.
    pub structure: &'a StructureResolver,
    /// Render configuration.
    pub render: &'a RenderConfig,
    /// Producer (normally the dispatching registry).
    pub producer: &'a dyn UnitProducer,
}

impl std::fmt::Debug for LayoutInputs<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayoutInputs")
            .field("units", &self.units.len())
            .field("items", &self.items.len())
            .finish_non_exhaustive()
    }
}

impl LayoutInputs<'_> {
    /// Exact height of unit `key` at `width` through `cache` (0 for an unknown unit).
    pub fn measure(&self, cache: &mut ItemLayoutCache, key: &UnitKey, width: u64) -> u64 {
        let Some(unit) = self.units.entry(key) else {
            return 0;
        };
        let item = key.item_id().and_then(|id| self.units.item(self.items, id));
        let structure = self.structure.unit_presentation(key);
        let cx = self.render.context(key, width, structure);
        cache.measure(UnitView { unit, item }, unit.revision, &cx, self.producer)
    }
}

/// Paint-time view of one retained layout: paints the retained component through its retained
/// node, then the paint slots from the paint inputs (L5). Never lays anything out.
pub struct RetainedItem<'a> {
    layout: &'a ItemLayout,
    activity: ActivityPhase,
}

impl std::fmt::Debug for RetainedItem<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedItem")
            .field("key", &self.layout.key)
            .finish_non_exhaustive()
    }
}

impl<'a> RetainedItem<'a> {
    /// Paint `layout` with `activity` filling its paint slots.
    #[must_use]
    pub const fn new(layout: &'a ItemLayout, activity: ActivityPhase) -> Self {
        Self { layout, activity }
    }

    fn paint_slots(&self, cx: &mut PaintCx<'_, '_>) {
        for slot in &self.layout.geometry.paint_slots {
            let text = match slot.kind {
                PaintSlotKind::Spinner => {
                    common::spinner_glyph(self.activity.spinner_frame).to_owned()
                }
                PaintSlotKind::Elapsed { started_at_ms } => self
                    .activity
                    .now_ms
                    .map(|now| common::elapsed_label(now.saturating_sub(started_at_ms)))
                    .unwrap_or_default(),
            };
            cx.write_line(local_rect(slot.rect), &Line::raw(text));
        }
    }
}

fn local_rect(rect: LogicalRect) -> LocalRect {
    LocalRect::new(
        i32::try_from(rect.x).unwrap_or(i32::MAX),
        i64::try_from(rect.y).unwrap_or(i64::MAX),
        u16::try_from(rect.width).unwrap_or(u16::MAX),
        u16::try_from(rect.height).unwrap_or(u16::MAX),
    )
}

impl Component for RetainedItem<'_> {
    fn layout(&self, _constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        // The list uses the retained node; a caller asking anyway gets that node unchanged.
        self.layout.node.clone()
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.layout.content().paint(layout, cx);
        self.paint_slots(cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.layout.content().event(event, layout, cx)
    }

    fn revision(&self) -> ComponentRevision {
        self.layout.content().revision()
    }
}

#[cfg(test)]
mod tests {
    use bcode_session_view_models::{
        ChatMessageView, TranscriptChanges, TranscriptViewItemId, TranscriptViewItemKind,
    };

    use super::*;
    use crate::layout::producers::testing::PlainTextProducer;
    use crate::units::structure::FlatStructure;
    use crate::window::WindowMode;

    fn items(texts: &[&str]) -> Vec<TranscriptViewItem> {
        texts
            .iter()
            .enumerate()
            .map(|(n, text)| TranscriptViewItem {
                id: TranscriptViewItemId::new(format!("i{n}")),
                revision: 1,
                sequence: Some(n as u64),
                timestamp_ms: None,
                output_location: None,
                streaming: false,
                kind: TranscriptViewItemKind::AssistantMessage {
                    message: ChatMessageView::markdown(*text),
                },
            })
            .collect()
    }

    struct Fixture {
        items: Vec<TranscriptViewItem>,
        units: Units,
        structure: StructureResolver,
        render: RenderConfig,
        cache: ItemLayoutCache,
    }

    impl Fixture {
        fn new(texts: &[&str]) -> Self {
            let items = items(texts);
            let mut units = Units::new();
            let _ = units.apply(
                &TranscriptChanges::default(),
                &items,
                WindowMode::Live,
                &FlatStructure,
            );
            Self {
                items,
                units,
                structure: StructureResolver::default(),
                render: RenderConfig::default(),
                cache: ItemLayoutCache::new(),
            }
        }

        fn measure(&mut self, key: &UnitKey, width: u64) -> u64 {
            let inputs = LayoutInputs {
                units: &self.units,
                items: &self.items,
                structure: &self.structure,
                render: &self.render,
                producer: &PlainTextProducer,
            };
            inputs.measure(&mut self.cache, key, width)
        }
    }

    fn key(n: u64) -> UnitKey {
        UnitKey::Item(TranscriptViewItemId::new(format!("i{n}")))
    }

    #[test]
    fn l1_exact_keys_hit_and_any_key_component_change_misses() {
        let mut fixture = Fixture::new(&["one two three four", "x"]);
        assert_eq!(fixture.measure(&key(0), 40), 1);
        assert_eq!(fixture.measure(&key(0), 40), 1);
        assert_eq!(fixture.cache.take_counters().measurements, 1, "exact hit");

        assert_eq!(
            fixture.measure(&key(0), 8),
            3,
            "width is part of the key (L9)"
        );
        let mut theme = (*fixture.render.theme).clone();
        theme.fingerprint = 7;
        fixture.render.theme = std::sync::Arc::new(theme);
        let _ = fixture.measure(&key(0), 8);
        assert_eq!(
            fixture.cache.take_counters().measurements,
            2,
            "fingerprint is part of the key (L3)"
        );

        // Measuring another unit never invalidates this one (L1).
        let _ = fixture.measure(&key(1), 8);
        let _ = fixture.measure(&key(0), 8);
        assert_eq!(fixture.cache.take_counters().measurements, 1);
        assert_eq!(fixture.cache.len(), 2);
        fixture.cache.remove_unit(&key(1));
        assert!(fixture.cache.current(&key(1)).is_none());
    }

    #[test]
    fn b6_retained_node_height_and_geometry_come_from_one_layout() {
        let mut fixture = Fixture::new(&["alpha beta gamma"]);
        let height = fixture.measure(&key(0), 6);
        let layout = fixture.cache.current(&key(0)).expect("retained");
        assert_eq!(layout.height, height);
        assert_eq!(layout.node.size.height, height);
        assert_eq!(layout.key.width, 6);
        assert_eq!(
            layout.key.revision,
            fixture.units.revision(&key(0)).expect("unit")
        );
    }
}
