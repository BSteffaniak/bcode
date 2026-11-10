//! Accepted plugin visuals through the engine façade (L8, E2, E3, C36): plugin cells copy
//! their adapter source across wide, emoji and combining graphemes; action regions hit only
//! their visible cells after clipping and resize and resolve only at the accepted revision;
//! images are placed over the rows that show them.

use std::sync::Arc;

use web_time::Instant;

use bcode_session_models::{
    ClientId, SessionEvent, SessionEventKind, SessionId, TurnAdmissionMetadata,
};
use bcode_session_view_models::{TranscriptViewItemId, TranscriptViewItemKind};
use bcode_tui_components::plugin_visual::action::{VisualAction, VisualActionRegion};
use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::image::{ImageContribution, ImageFit, ImageLifecycle, ImagePayload, ImagePlacement};
use bmux_tui::paint::PaintCx;
use bmux_tui::selection::{SelectionAffinity, SelectionGranularity, SelectionScene};
use bmux_tui::style::Style;
use bmux_tui::text::Line;

use crate::engine::{HostRequest, ItemHit, TranscriptEngine};
use crate::frame::{FrameClock, FrameSnapshot};
use crate::layout::context::AcceptedPluginVisual;
use crate::layout::geometry::ItemAction;
use crate::layout::producers::visual::{
    VisualCorrespondence, VisualSelectionCell, VisualSelectionRow,
};
use crate::navigation::MotionConfig;
use crate::selection::TranscriptSelection;
use crate::units::UnitKey;
use crate::viewport::ViewportGeometry;
use crate::window::HistoryWindowResult;

const HEIGHT: u16 = 16;
const ADAPTER: &str = "example.card:card";

fn event(sequence: u64, kind: SessionEventKind) -> SessionEvent {
    SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence,
        timestamp_ms: sequence,
        session_id: SessionId::new(),
        provenance: None,
        kind,
    }
}

/// A finished tool call, rendered through an accepted visual once one is accepted.
fn session() -> Vec<SessionEvent> {
    vec![
        event(
            1,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "run it".to_owned(),
                admission: TurnAdmissionMetadata::default(),
            },
        ),
        event(
            2,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-2".to_owned(),
                producer_plugin_id: None,
                tool_name: "bash".to_owned(),
                arguments_json: r#"{"command":"printf"}"#.to_owned(),
                working_directory: None,
            },
        ),
        event(
            3,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-2".to_owned(),
                    model_output: "fallback output".to_owned(),
                    is_error: false,
                    presentation: None,
                    result: None,
                    content: Vec::new(),
                },
            },
        ),
    ]
}

fn attach(width: u16, at: Instant) -> TranscriptEngine {
    let events = session();
    let mut engine = TranscriptEngine::new(
        ViewportGeometry::new(u64::from(width), u64::from(HEIGHT)),
        MotionConfig::INSTANT,
    );
    let update = engine.attach(at);
    let HostRequest::Window(issued) = update.requests[0] else {
        panic!("attach requests a window");
    };
    let result = HistoryWindowResult {
        generation: issued.generation,
        target: issued.request,
        events: events.clone(),
        has_older: false,
        has_newer: false,
        first_available_sequence: events.first().map(|event| event.sequence),
        last_available_sequence: events.last().map(|event| event.sequence),
    };
    let _ = engine.apply_window(&result, at).expect("attach");
    engine
}

fn tool_item(engine: &TranscriptEngine) -> TranscriptViewItemId {
    engine
        .session_view()
        .snapshot()
        .transcript
        .items
        .iter()
        .find(|item| matches!(item.kind, TranscriptViewItemKind::ToolInvocation { .. }))
        .expect("a tool item")
        .id
        .clone()
}

fn accept(engine: &mut TranscriptEngine, visual: AcceptedPluginVisual, at: Instant) {
    let item = tool_item(engine);
    let mut config = engine.render_config().clone();
    config.plugin_visuals.accept(item, visual);
    let _ = engine.set_render_config(config, at);
}

fn lines(rows: &[&str]) -> Arc<[Line]> {
    rows.iter()
        .map(|row| Line::raw((*row).to_owned()))
        .collect::<Vec<_>>()
        .into()
}

fn copy(id: &str, area: Rect) -> VisualActionRegion {
    VisualActionRegion {
        id: id.to_owned(),
        area,
        action: VisualAction::Copy(format!("{id} text")),
    }
}

struct Painted {
    width: u16,
    rows: Vec<String>,
    scene: SelectionScene,
    images: Vec<ImagePlacement>,
    snapshot: FrameSnapshot,
}

impl Painted {
    /// Cell of the first painted `needle` (rows hold one symbol per cell; wide graphemes
    /// leave an empty continuation symbol).
    fn find(&self, needle: &str) -> Option<Point> {
        self.rows.iter().enumerate().find_map(|(y, row)| {
            let x = row.find(needle)?;
            Some(Point::new(
                u16::try_from(row[..x].chars().count()).ok()?,
                u16::try_from(y).ok()?,
            ))
        })
    }

    /// Every cell hit as a plugin visual action, with the hit.
    fn visual_hits(&self, engine: &TranscriptEngine) -> Vec<(Point, ItemHit)> {
        (0..HEIGHT)
            .flat_map(|y| (0..self.width).map(move |x| Point::new(x, y)))
            .filter_map(|point| {
                let hit =
                    engine.hit_test(&self.snapshot, u64::from(point.x), u64::from(point.y))?;
                matches!(hit.action, ItemAction::Visual { .. }).then_some((point, hit))
            })
            .collect()
    }
}

fn paint(engine: &mut TranscriptEngine, width: u16, at: Instant) -> Painted {
    let clock = FrameClock { now: at };
    let snapshot = engine.prepare_frame(clock);
    let area = Rect::new(0, 0, width, HEIGHT);
    let mut buffer = Buffer::empty(area);
    let (scene, images) = {
        let mut frame = Frame::new(&mut buffer);
        frame.set_clock(Some(clock.presentation()));
        let mut cx = PaintCx::new(&mut frame);
        engine.paint_frame(&snapshot, &mut cx);
        engine.paint_selection(&snapshot, &mut cx, Style::new());
        let images = frame
            .images()
            .iter()
            .filter_map(|image| match image {
                ImageContribution::Present(placement) => Some(placement.clone()),
                ImageContribution::Remove(_) => None,
            })
            .collect();
        (frame.selection().clone(), images)
    };
    let rows = (0..HEIGHT)
        .map(|row| buffer.row_symbols(row).unwrap_or_default())
        .collect();
    let _ = engine.commit(snapshot.id);
    Painted {
        width,
        rows,
        scene,
        images,
        snapshot,
    }
}

fn ids(hits: &[(Point, ItemHit)]) -> Vec<String> {
    let mut ids: Vec<String> = hits
        .iter()
        .filter_map(|(_, hit)| match &hit.action {
            ItemAction::Visual { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    ids.dedup();
    ids
}

/// C36: a selection over plugin cells copies the adapter's source bytes, one indivisible
/// fragment per adapter cell, across CJK, emoji and combining-mark graphemes.
#[test]
fn selection_over_plugin_cells_copies_wide_emoji_and_combining_source() {
    let at = Instant::now();
    let mut engine = attach(40, at);
    // Painted with a prompt the source does not contain; cells map to the source bytes.
    let source = "界🙂e\u{301}x";
    let painted_row = format!("> {source}");
    let cells = vec![
        VisualSelectionCell {
            column: 2,
            width: 2,
            bytes: 0..3,
        },
        VisualSelectionCell {
            column: 4,
            width: 2,
            bytes: 3..7,
        },
        VisualSelectionCell {
            column: 6,
            width: 1,
            bytes: 7..10,
        },
        VisualSelectionCell {
            column: 7,
            width: 1,
            bytes: 10..11,
        },
    ];
    let rows = lines(&[&painted_row]);
    let selection = VisualSelectionRow {
        identity: "example.card:src".to_owned(),
        byte_start: 0,
        text: source.to_owned(),
        cells,
    };
    accept(
        &mut engine,
        AcceptedPluginVisual {
            correspondence: Arc::new(VisualCorrespondence::new(
                Vec::new(),
                [(0, selection)],
                rows.len(),
            )),
            ..AcceptedPluginVisual::rows(ADAPTER, 1, rows)
        },
        at,
    );
    let painted = paint(&mut engine, 40, at);
    let start = painted.find("界").expect("plugin row painted");
    let scope = painted
        .scene
        .initiation_scope(start)
        .expect("a selection scope")
        .id
        .clone();
    let endpoint = |point: Point, affinity| {
        let endpoint = painted
            .scene
            .endpoint_at_granularity(&scope, point, affinity, SelectionGranularity::Grapheme)
            .expect("an endpoint");
        engine
            .selection_endpoint(&painted.snapshot, &endpoint)
            .expect("a transcript endpoint")
    };
    // From the CJK cell through the combining grapheme (cells 0..2, 2..4, 4), and the
    // continuation cell of the emoji alone, which selects the whole emoji, never half of it.
    let combining = Point::new(start.x + 4, start.y);
    let emoji_tail = Point::new(start.x + 3, start.y);
    let across = TranscriptSelection::new(
        endpoint(start, SelectionAffinity::Before),
        endpoint(combining, SelectionAffinity::After),
    );
    let emoji = TranscriptSelection::new(
        endpoint(emoji_tail, SelectionAffinity::Before),
        endpoint(emoji_tail, SelectionAffinity::After),
    );
    let _ = engine.set_selection(Some(across), false);
    assert_eq!(engine.selected_text().as_deref(), Some("界🙂e\u{301}"));
    let _ = engine.set_selection(Some(emoji), false);
    assert_eq!(engine.selected_text().as_deref(), Some("🙂"));
}

/// E2: action regions hit exactly their visible cells, clipped at the body width; after a
/// resize the re-accepted regions are measured again and a region clipped away leaves no hit.
#[test]
fn action_hits_follow_visible_cells_after_clipping_and_resize() {
    let at = Instant::now();
    let mut engine = attach(60, at);
    let rows = lines(&["[copy] some longer adapter row", "second row"]);
    let visual = |revision| AcceptedPluginVisual {
        actions: Arc::from(vec![
            copy("near", Rect::new(0, 0, 6, 1)),
            copy("far", Rect::new(40, 1, 8, 1)),
        ]),
        ..AcceptedPluginVisual::rows(ADAPTER, revision, Arc::clone(&rows))
    };
    accept(&mut engine, visual(1), at);
    let wide = paint(&mut engine, 60, at);
    let hits = wide.visual_hits(&engine);
    assert_eq!(ids(&hits), vec!["near".to_owned(), "far".to_owned()]);
    let near = wide.find("[copy]").expect("adapter row painted");
    let near_cells: Vec<Point> = hits
        .iter()
        .filter(|(_, hit)| matches!(&hit.action, ItemAction::Visual { id, .. } if id == "near"))
        .map(|(point, _)| *point)
        .collect();
    assert_eq!(
        near_cells,
        (near.x..near.x + 6)
            .map(|x| Point::new(x, near.y))
            .collect::<Vec<_>>(),
        "hits cover exactly the painted region cells"
    );
    let (_, hit) = &hits[0];
    assert_eq!(
        engine.visual_action(hit),
        Some(&VisualAction::Copy("near text".to_owned()))
    );

    // Narrower than the far region's columns: it is clipped away entirely, the near region
    // still hits its cells, and no hit lies outside the viewport.
    let _ = engine.resize(ViewportGeometry::new(30, u64::from(HEIGHT)), at);
    accept(&mut engine, visual(2), at);
    let narrow = paint(&mut engine, 30, at);
    let hits = narrow.visual_hits(&engine);
    assert_eq!(ids(&hits), vec!["near".to_owned()]);
    assert!(hits.iter().all(|(point, _)| point.x < 30));
    let (_, hit) = &hits[0];
    assert!(
        matches!(&hit.action, ItemAction::Visual { revision: 2, .. }),
        "{hit:?}"
    );
    assert!(engine.visual_action(hit).is_some());
}

/// E2/E3: a hit resolves only against the visual accepted now. A hit measured from a
/// replaced revision or another adapter resolves to nothing, and content keys or other hits
/// never authorize an action.
#[test]
fn a_hit_from_a_replaced_revision_is_rejected() {
    let at = Instant::now();
    let mut engine = attach(40, at);
    let rows = lines(&["[copy] row"]);
    let visual = |adapter: &str, revision| AcceptedPluginVisual {
        actions: Arc::from(vec![copy("near", Rect::new(0, 0, 6, 1))]),
        ..AcceptedPluginVisual::rows(adapter, revision, Arc::clone(&rows))
    };
    accept(&mut engine, visual(ADAPTER, 1), at);
    let first = paint(&mut engine, 40, at);
    let (_, stale) = first.visual_hits(&engine).remove(0);
    assert!(engine.visual_action(&stale).is_some());

    accept(&mut engine, visual(ADAPTER, 2), at);
    assert_eq!(engine.visual_action(&stale), None, "replaced revision");
    let (_, fresh) = paint(&mut engine, 40, at).visual_hits(&engine).remove(0);
    assert_eq!(
        engine.visual_action(&fresh),
        Some(&VisualAction::Copy("near text".to_owned()))
    );

    accept(&mut engine, visual("other.card:card", 2), at);
    assert_eq!(engine.visual_action(&fresh), None, "replaced adapter");

    let unit = UnitKey::Item(tool_item(&engine));
    for action in [
        ItemAction::Activate(format!("{ADAPTER}:visual")),
        ItemAction::Visual {
            adapter: "other.card:card".to_owned(),
            revision: 2,
            id: "missing".to_owned(),
        },
    ] {
        let hit = ItemHit {
            unit: unit.clone(),
            action,
        };
        assert_eq!(engine.visual_action(&hit), None, "{hit:?}");
    }
}

/// L8: an image is placed over the cells of the adapter rows it covers (the rows paint where
/// the placement lands), keyed by its unit, and nothing is placed once its rows are hidden.
#[test]
fn image_placement_agrees_with_the_rows_that_show_it() {
    let at = Instant::now();
    let mut engine = attach(40, at);
    let rows = lines(&["title row", "img a", "img b", "after"]);
    let destination = Rect::new(0, 1, 6, 2);
    let image = ImagePlacement {
        key: "preview".into(),
        payload: ImagePayload::Png {
            bytes: vec![1, 2, 3],
            width: 2,
            height: 2,
        },
        destination,
        fit: ImageFit::Contain,
        allocation: None,
        clip: destination,
        lifecycle: ImageLifecycle::Frame,
    };
    accept(
        &mut engine,
        AcceptedPluginVisual {
            images: Arc::from(vec![image]),
            ..AcceptedPluginVisual::rows(ADAPTER, 1, rows)
        },
        at,
    );
    let painted = paint(&mut engine, 40, at);
    let first = painted.find("img a").expect("first image row");
    let second = painted.find("img b").expect("second image row");
    assert_eq!(second, Point::new(first.x, first.y + 1));
    assert_eq!(painted.images.len(), 1, "{:?}", painted.images);
    let placed = &painted.images[0];
    assert_eq!(placed.destination, Rect::new(first.x, first.y, 6, 2));
    assert_eq!(placed.clip, placed.destination);
    assert!(
        placed.key.as_str().ends_with(":preview") && placed.key.as_str() != "preview",
        "scoped by the owning unit: {:?}",
        placed.key
    );

    // Collapsing the unit hides the rows; their image is no longer placed.
    let item = tool_item(&engine);
    let target = engine
        .structure()
        .unit_presentation(&UnitKey::Item(item))
        .target()
        .cloned()
        .expect("the tool unit toggles");
    let _ = engine.toggle_disclosure(&target, at);
    let collapsed = paint(&mut engine, 40, at);
    assert!(collapsed.find("img a").is_none());
    assert!(collapsed.images.is_empty(), "{:?}", collapsed.images);
}
