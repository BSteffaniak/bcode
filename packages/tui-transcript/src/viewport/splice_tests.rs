//! Viewport tests over real units and the measured index: splice correspondence (W3, L1,
//! VP6), tail-space overscroll (VP2), controlled mode (VP5) and visible rows (P1, VP3).

use bcode_session_view_models::{
    ChatMessageView, TranscriptChanges, TranscriptViewItem, TranscriptViewItemId,
    TranscriptViewItemKind,
};
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::geometry::Size;
use bmux_tui_components::virtual_list::{ActionDispatch, NoSourceMap};

use super::source::TranscriptItemSource;
use super::*;
use crate::layout::producers::UnitProducer;
use crate::layout::producers::testing::{FixtureProducer, PlainTextProducer};
use crate::layout::{ActivityPhase, ItemLayoutCache, LayoutInputs, RenderConfig};
use crate::navigation::NoUnitFacts;
use crate::units::structure::{FlatStructure, StructureResolver};
use crate::units::{BoundaryEdge, UnitKey, Units};
use crate::window::{ResidentWindow, WindowMode};

fn item(n: u64) -> TranscriptViewItem {
    let message = ChatMessageView::markdown(format!("item {n}"));
    TranscriptViewItem {
        id: TranscriptViewItemId::new(format!("i{n}")),
        revision: 1,
        sequence: Some(n),
        timestamp_ms: None,
        output_location: None,
        streaming: false,
        kind: TranscriptViewItemKind::AssistantMessage { message },
    }
}

fn key(n: u64) -> UnitKey {
    UnitKey::Item(TranscriptViewItemId::new(format!("i{n}")))
}

struct Fixture {
    items: Vec<TranscriptViewItem>,
    units: Units,
    viewport: TranscriptViewport,
    window: ResidentWindow,
    producer: FixtureProducer,
    layouts: ItemLayoutCache,
    structure: StructureResolver,
    render: RenderConfig,
}

/// Measure `changes` into `viewport` through `layouts` (the engine's VP6 path).
#[allow(clippy::too_many_arguments)]
fn apply(
    viewport: &mut TranscriptViewport,
    layouts: &mut ItemLayoutCache,
    changes: &crate::units::UnitChanges,
    units: &Units,
    items: &[TranscriptViewItem],
    structure: &StructureResolver,
    render: &RenderConfig,
    producer: &dyn UnitProducer,
) {
    let inputs = LayoutInputs {
        units,
        items,
        structure,
        render,
        producer,
    };
    viewport
        .apply_unit_changes(changes, units, 40, &mut |unit| {
            inputs.measure(layouts, unit, 40)
        })
        .expect("splices match the index");
}

impl Fixture {
    /// Items `range`, each 3 rows tall, synced into the index.
    fn new(range: std::ops::Range<u64>) -> Self {
        let mut fixture = Self {
            items: Vec::new(),
            units: Units::new(),
            viewport: TranscriptViewport::new(),
            window: ResidentWindow::new(),
            producer: FixtureProducer::uniform(3),
            layouts: ItemLayoutCache::new(),
            structure: StructureResolver::default(),
            render: RenderConfig::default(),
        };
        fixture.set_items(range.map(item).collect());
        fixture
    }

    /// Replace the document and apply the resulting unit changes incrementally.
    fn set_items(&mut self, items: Vec<TranscriptViewItem>) -> crate::units::UnitChanges {
        self.items = items;
        let changes = self.units.apply(
            &TranscriptChanges::default(),
            &self.items,
            self.window.mode,
            &FlatStructure,
        );
        apply(
            &mut self.viewport,
            &mut self.layouts,
            &changes,
            &self.units,
            &self.items,
            &self.structure,
            &self.render,
            &self.producer,
        );
        changes
    }

    /// Measurements since the last call.
    fn measured(&mut self) -> u64 {
        self.layouts.take_counters().measurements
    }

    fn index_keys(&self) -> Vec<UnitKey> {
        self.viewport.index().keys().cloned().collect()
    }

    fn frame(&self, position: &ListPosition) -> ViewportFrame {
        self.viewport
            .frame(position, 10, &NoSourceMap)
            .expect("position resolves")
    }
}

fn anchored(unit: UnitKey, row: u64) -> ListPosition {
    ListPosition::Anchored {
        anchor: ReadingAnchor::row(unit, row),
    }
}

#[test]
fn w3_vp6_index_follows_unit_splices_and_measures_only_new_units() {
    let mut fixture = Fixture::new(10..20);
    assert_eq!(fixture.index_keys(), fixture.units.keys());
    assert_eq!(fixture.measured(), 11, "boundary plus ten items");

    // Prepend five items and drop the last two in one update.
    let _ = fixture.measured();
    let next = (5..18).map(item).collect();
    let changes = fixture.set_items(next);
    assert_eq!(changes.splices.len(), 2);
    assert_eq!(fixture.index_keys(), fixture.units.keys());
    assert_eq!(
        fixture.measured(),
        5,
        "only inserted units are measured (L1)"
    );
    let index = fixture.viewport.index();
    assert_eq!(index.total_height(), 1 + 13 * 3);
    assert_eq!(index.key_offset(&key(10)), Some(1 + 5 * 3));
}

#[test]
fn d3_revised_unit_is_remeasured_in_place() {
    let mut fixture = Fixture::new(0..5);
    fixture.producer = FixtureProducer::uniform(3).with_height(key(2), 7);
    let _ = fixture.measured();
    let mut items = fixture.items.clone();
    items[2].revision = 2;
    let changes = fixture.set_items(items);
    assert_eq!(changes.revised, vec![key(2)]);
    assert_eq!(fixture.measured(), 1);
    let index = fixture.viewport.index();
    assert_eq!(index.item_by_key(&key(2)).map(|item| item.height), Some(7));
    assert_eq!(index.key_offset(&key(3)), Some(1 + 3 + 3 + 7));
}

#[test]
fn p1_vp5_prepend_above_a_keyed_anchor_keeps_its_viewport_row() {
    let mut fixture = Fixture::new(10..30);
    let position = anchored(key(20), 1);
    fixture.viewport.apply_position(&position);
    let before = fixture.frame(&position);
    let anchor_row = |frame: &ViewportFrame| {
        frame
            .units
            .iter()
            .find(|unit| unit.key == key(20))
            .map(|unit| unit.top)
    };
    assert_eq!(anchor_row(&before), Some(-1));

    let next = (0..30).map(item).collect();
    let _ = fixture.set_items(next);
    // The controlled list never moved: its stored position is still the keyed anchor.
    assert_eq!(
        fixture.viewport.state().borrow().position(),
        bmux_position(&position)
    );
    let after = fixture.frame(&position);
    assert_eq!(anchor_row(&after), Some(-1));
    assert_eq!(
        after.resolved.top_offset,
        before.resolved.top_offset + 10 * 3
    );
}

#[test]
fn vp5_controlled_list_queues_input_and_never_moves_itself() {
    let mut fixture = Fixture::new(0..20);
    let position = anchored(key(5), 0);
    fixture.viewport.apply_position(&position);
    let dispatch = fixture.viewport.state_mut().dispatch_action(
        VirtualListAction::ScrollRequested {
            rows: 3,
            source: ScrollInputSource::Wheel,
        },
        &NoSourceMap,
    );
    assert_eq!(dispatch, ActionDispatch::Queued);
    assert_eq!(
        fixture.viewport.state().borrow().position(),
        bmux_position(&position)
    );
    assert_eq!(
        fixture.viewport.drain_outputs(),
        vec![ViewportOutput::Navigate(NavIntent::ScrollBy {
            rows: 3,
            source: InputSource::Wheel
        })]
    );
}

#[test]
fn vp2_tail_space_leaves_at_least_one_content_row() {
    let fixture = Fixture::new(0..4); // 1 + 12 rows
    let last = key(3);
    let tail = ListPosition::TailSpace {
        anchor: ReadingAnchor::row(last.clone(), 2),
    };
    let frame = fixture.frame(&tail);
    assert_eq!(
        frame.resolved.top_offset, 12,
        "last content row on the top row"
    );
    assert_eq!(frame.blank_rows_below(), 9);
    // Anchored positions never show blank rows below content.
    let anchored = fixture.frame(&anchored(last, 2));
    assert_eq!(anchored.blank_rows_below(), 0);
    assert_eq!(anchored.resolved.top_offset, 3);
}

#[test]
fn vp3_frame_lists_only_intersecting_units_with_viewport_rows() {
    let fixture = Fixture::new(0..20);
    // Unit 4 spans rows 13..16; with row 1 on top the viewport shows rows 14..24.
    let frame = fixture.frame(&anchored(key(4), 1));
    let keys = frame
        .units
        .iter()
        .map(|unit| unit.key.clone())
        .collect::<Vec<_>>();
    assert_eq!(keys, vec![key(4), key(5), key(6), key(7)]);
    assert_eq!(frame.units[0].top, -1);
    assert!(frame.fully_visible(&key(5)));
    assert!(!frame.fully_visible(&key(4)));
    assert!(!frame.fully_visible(&key(7)));
    let follow = fixture.frame(&ListPosition::FollowBottom);
    assert_eq!(
        follow.units.last().map(|unit| unit.key.clone()),
        Some(key(19))
    );
}

#[test]
fn w5_edge_rows_count_resident_rows_beyond_the_viewport() {
    let fixture = Fixture::new(0..20); // 61 rows
    let rows = fixture
        .viewport
        .edge_rows(&anchored(key(10), 0), 10, &NoSourceMap);
    assert_eq!(rows.before, 31);
    assert_eq!(rows.after, 61 - 41);
    let tail = ListPosition::TailSpace {
        anchor: ReadingAnchor::top(key(19)),
    };
    assert_eq!(fixture.viewport.edge_rows(&tail, 10, &NoSourceMap).after, 0);
}

#[test]
fn nav_view_reads_the_measured_index() {
    let fixture = Fixture::new(0..5);
    let context = NavViewContext {
        sources: &NoSourceMap,
        facts: &NoUnitFacts,
        geometry: ViewportGeometry::new(40, 10),
        window: &fixture.window,
        motion: MotionConfig::INSTANT,
    };
    let (height, total) = fixture.viewport.with_nav_view(context, |view| {
        (view.unit_height(&key(1)), view.total_height())
    });
    assert_eq!(height, Some(3));
    assert_eq!(total, 16);
}

#[test]
fn u3_boundary_state_change_is_paint_only() {
    use bmux_tui_components::virtual_list::VirtualItemSource;

    let mut fixture = Fixture::new(0..3);
    let boundary = UnitKey::Boundary(BoundaryEdge::Older);
    let source = TranscriptItemSource::new(
        &fixture.units,
        &fixture.window,
        &fixture.layouts,
        ActivityPhase::default(),
    );
    let layout_before = source.layout_revision(0);
    let paint_before = source.paint_revision(0);
    fixture.window.in_flight.older = true;
    let source = TranscriptItemSource::new(
        &fixture.units,
        &fixture.window,
        &fixture.layouts,
        ActivityPhase::default(),
    );
    assert_eq!(source.key(0), boundary);
    assert_eq!(source.layout_revision(0), layout_before);
    assert_ne!(source.paint_revision(0), paint_before);
    assert_eq!(
        fixture
            .layouts
            .current(&boundary)
            .map(|layout| layout.height),
        Some(1)
    );
}

#[test]
fn component_layout_resolves_the_written_position_without_moving_it() {
    let items = (0..30).map(item).collect::<Vec<_>>();
    let mut units = Units::new();
    let window = ResidentWindow {
        mode: WindowMode::Live,
        ..ResidentWindow::new()
    };
    let changes = units.apply(
        &TranscriptChanges::default(),
        &items,
        WindowMode::Live,
        &FlatStructure,
    );
    let mut viewport = TranscriptViewport::new();
    let mut layouts = ItemLayoutCache::new();
    apply(
        &mut viewport,
        &mut layouts,
        &changes,
        &units,
        &items,
        &StructureResolver::default(),
        &RenderConfig::default(),
        &PlainTextProducer,
    );
    let source = TranscriptItemSource::new(&units, &window, &layouts, ActivityPhase::default());
    let position = anchored(key(12), 0);
    viewport.apply_position(&position);
    let expected = viewport
        .resolve(&position, 10, &NoSourceMap)
        .expect("resolves");
    let component = viewport.component("transcript", &source, &layouts, ScrollbarMode::Off);
    let _layout = component.layout(Constraints::tight(Size::new(40, 10)), &mut LayoutCx::new());
    let state = viewport.state().borrow();
    assert_eq!(
        state.resolved().map(|r| r.top_offset),
        Some(expected.top_offset)
    );
    assert_eq!(state.position(), bmux_position(&position));
    assert!(!state.anchor_missing());
}

#[test]
fn source_resolves_unit_content_to_its_item_after_prepends_and_revisions() {
    let mut fixture = Fixture::new(10..20);
    let next = (0..20).map(item).collect();
    let _ = fixture.set_items(next);
    let mut items = fixture.items.clone();
    items[12].revision = 2;
    let changes = TranscriptChanges {
        changes: vec![bcode_session_view_models::TranscriptChange::Revised {
            id: items[12].id.clone(),
            revision: 2,
        }],
        ..TranscriptChanges::default()
    };
    fixture.items = items;
    let revised = fixture
        .units
        .apply(&changes, &fixture.items, WindowMode::Live, &FlatStructure);
    assert_eq!(revised.revised, vec![key(12)]);
    for n in [0, 9, 10, 12, 19] {
        let id = TranscriptViewItemId::new(format!("i{n}"));
        let item = fixture
            .units
            .item(&fixture.items, &id)
            .expect("resident unit has its item");
        assert_eq!(item.sequence, Some(n));
    }
    assert_eq!(
        fixture
            .units
            .item(&fixture.items, &TranscriptViewItemId::new("i12"))
            .map(|item| item.revision),
        Some(2)
    );
    assert!(
        fixture
            .units
            .item(&fixture.items, &TranscriptViewItemId::new("i40"))
            .is_none()
    );
}
