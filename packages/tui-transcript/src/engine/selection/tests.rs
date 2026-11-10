//! Transcript selection through the engine façade: fragments registered from the painted
//! retained layouts, logical endpoints resolved from them, the copied source text, and the
//! selection surviving scrolling (C34, BMUX 13).

use std::time::Instant;

use bcode_session_models::{
    ClientId, SessionEvent, SessionEventKind, SessionId, TurnAdmissionMetadata,
};
use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::paint::PaintCx;
use bmux_tui::selection::{SelectionAffinity, SelectionGranularity, SelectionScene};
use bmux_tui::style::{Modifier, Style};

use crate::engine::{HostRequest, TranscriptEngine};
use crate::frame::{FrameClock, FrameSnapshot};
use crate::navigation::{InputSource, MotionConfig, NavIntent};
use crate::selection::{SELECTION_SCOPE, TranscriptSelection};
use crate::viewport::ViewportGeometry;
use crate::window::HistoryWindowResult;

const WIDTH: u16 = 40;
const HEIGHT: u16 = 12;

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

fn user(sequence: u64, text: &str) -> SessionEvent {
    event(
        sequence,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: text.to_owned(),
            admission: TurnAdmissionMetadata::default(),
        },
    )
}

fn assistant(sequence: u64, text: &str) -> SessionEvent {
    event(
        sequence,
        SessionEventKind::AssistantMessage {
            text: text.to_owned(),
        },
    )
}

fn attach(events: &[SessionEvent], at: Instant) -> TranscriptEngine {
    let mut engine = TranscriptEngine::new(
        ViewportGeometry::new(u64::from(WIDTH), u64::from(HEIGHT)),
        MotionConfig::INSTANT,
    );
    let update = engine.attach(at);
    let HostRequest::Window(issued) = update.requests[0] else {
        panic!("attach requests a window");
    };
    let result = HistoryWindowResult {
        generation: issued.generation,
        target: issued.request,
        events: events.to_vec(),
        has_older: false,
        has_newer: false,
        first_available_sequence: events.first().map(|event| event.sequence),
        last_available_sequence: events.last().map(|event| event.sequence),
    };
    let _ = engine.apply_window(&result, at).expect("attach");
    engine
}

/// One painted frame: its rows, its selection scene, the highlighted cells and the snapshot
/// it painted.
struct Painted {
    rows: Vec<String>,
    scene: SelectionScene,
    highlighted: Vec<Point>,
    snapshot: FrameSnapshot,
}

impl Painted {
    fn find(&self, needle: &str) -> Point {
        self.rows
            .iter()
            .enumerate()
            .find_map(|(y, row)| {
                let x = row.find(needle)?;
                Some(Point::new(
                    u16::try_from(row[..x].chars().count()).ok()?,
                    u16::try_from(y).ok()?,
                ))
            })
            .unwrap_or_else(|| panic!("{needle:?} is not painted: {:#?}", self.rows))
    }
}

fn highlight() -> Style {
    Style::new().add_modifier(Modifier::UNDERLINE)
}

fn paint(engine: &mut TranscriptEngine, at: Instant) -> Painted {
    let clock = FrameClock { now: at };
    let snapshot = engine.prepare_frame(clock);
    let area = Rect::new(0, 0, WIDTH, HEIGHT);
    let mut buffer = Buffer::empty(area);
    let scene = {
        let mut frame = Frame::new(&mut buffer);
        frame.set_clock(Some(clock.presentation()));
        let mut cx = PaintCx::new(&mut frame);
        engine.paint_frame(&snapshot, &mut cx);
        engine.paint_selection(&snapshot, &mut cx, highlight());
        frame.selection().clone()
    };
    let rows = (0..HEIGHT)
        .map(|row| buffer.row_symbols(row).unwrap_or_default())
        .collect();
    let highlighted = (0..HEIGHT)
        .flat_map(|y| (0..WIDTH).map(move |x| Point::new(x, y)))
        .filter(|point| {
            buffer
                .get(*point)
                .is_some_and(|cell| cell.style.modifiers.contains(Modifier::UNDERLINE))
        })
        .collect();
    let _ = engine.commit(snapshot.id);
    Painted {
        rows,
        scene,
        highlighted,
        snapshot,
    }
}

/// Select from before the grapheme at `from` to after the grapheme at `to` in the scope a
/// press at `from` locks to.
fn select(engine: &mut TranscriptEngine, painted: &Painted, from: Point, to: Point) {
    let scope = painted
        .scene
        .initiation_scope(from)
        .expect("a selection scope at the press")
        .id
        .clone();
    let endpoint = |point, affinity| {
        let endpoint = painted
            .scene
            .endpoint_at_granularity(&scope, point, affinity, SelectionGranularity::Grapheme)
            .expect("an endpoint");
        engine
            .selection_endpoint(&painted.snapshot, &endpoint)
            .expect("a transcript endpoint")
    };
    let anchor = endpoint(from, SelectionAffinity::Before);
    let focus = endpoint(to, SelectionAffinity::After);
    let _ = engine.set_selection(Some(TranscriptSelection::new(anchor, focus)), false);
}

fn session() -> Vec<SessionEvent> {
    vec![
        user(1, "first question"),
        assistant(2, "alpha **bold** beta\n\nsecond paragraph"),
        user(3, "next question"),
        assistant(4, "gamma delta"),
    ]
}

#[test]
fn frames_register_unit_scopes_and_grapheme_fragments_for_visible_text() {
    let at = Instant::now();
    let mut engine = attach(&session(), at);
    let painted = paint(&mut engine, at);
    assert!(
        painted
            .scene
            .scopes()
            .iter()
            .any(|scope| scope.id.as_str() == SELECTION_SCOPE)
    );
    let alpha = painted.find("alpha");
    let scope = painted.scene.initiation_scope(alpha).expect("unit scope");
    assert_ne!(scope.id.as_str(), SELECTION_SCOPE, "text locks to its unit");
    assert!(
        painted
            .scene
            .fragments()
            .iter()
            .any(|fragment| fragment.area == Rect::new(alpha.x, alpha.y, 1, 1)),
        "one fragment per grapheme"
    );
    assert!(painted.highlighted.is_empty(), "nothing selected");
}

#[test]
fn selection_copies_markdown_source_and_highlights_the_painted_cells() {
    let at = Instant::now();
    let mut engine = attach(&session(), at);
    let painted = paint(&mut engine, at);
    let from = painted.find("alpha");
    let mut to = painted.find("beta");
    to.x += 3;
    select(&mut engine, &painted, from, to);
    assert_eq!(
        engine.selected_text().as_deref(),
        Some("alpha **bold** beta")
    );
    let painted = paint(&mut engine, at);
    assert!(painted.highlighted.contains(&from));
    assert!(painted.highlighted.contains(&to));
    assert!(!painted.highlighted.contains(&painted.find("second")));
}

#[test]
fn selection_from_the_space_between_units_spans_them_in_document_order() {
    let at = Instant::now();
    let mut engine = attach(&session(), at);
    let painted = paint(&mut engine, at);
    // The blank row below the first response, outside every unit's text.
    let mut from = painted.find("second");
    from.y += 1;
    let mut to = painted.find("gamma");
    to.x += 4;
    select(&mut engine, &painted, from, to);
    let text = engine.selected_text().expect("selected");
    let (question, answer) = (text.find("next question"), text.find("gamma"));
    assert!(
        matches!((question, answer), (Some(question), Some(answer)) if question < answer),
        "{text:?}"
    );
    assert!(!text.contains("alpha"), "{text:?}");
}

#[test]
fn selection_survives_scrolling_its_rows_out_of_view_and_back() {
    let at = Instant::now();
    let mut engine = attach(&session(), at);
    let painted = paint(&mut engine, at);
    let from = painted.find("gamma");
    let mut to = from;
    to.x += 10;
    select(&mut engine, &painted, from, to);
    let selected = engine.selected_text();
    assert_eq!(selected.as_deref(), Some("gamma delta"));
    for rows in [-20, 20] {
        let _ = engine.intent(
            NavIntent::ScrollBy {
                rows,
                source: InputSource::Wheel,
            },
            at,
        );
        let _ = paint(&mut engine, at);
        assert_eq!(engine.selected_text(), selected);
    }
}

#[test]
fn scroll_only_frames_report_their_shift_and_content_changes_do_not() {
    let at = Instant::now();
    let events = (1..=20)
        .map(|sequence| assistant(sequence, &format!("line {sequence}")))
        .collect::<Vec<_>>();
    let mut engine = attach(&events, at);
    let first = paint(&mut engine, at).snapshot;
    let _ = engine.intent(
        NavIntent::ScrollBy {
            rows: -2,
            source: InputSource::Wheel,
        },
        at,
    );
    let scrolled = paint(&mut engine, at).snapshot;
    assert_eq!(scrolled.scrolled_from(&first), Some(-2));
    assert_eq!(first.scrolled_from(&scrolled), Some(2));
    assert_eq!(scrolled.scrolled_from(&scrolled), None, "no movement");
    let painted = paint(&mut engine, at);
    let from = painted.find("line 17");
    let mut to = from;
    to.x += 6;
    select(&mut engine, &painted, from, to);
    let selected = paint(&mut engine, at).snapshot;
    assert_eq!(
        selected.scrolled_from(&scrolled),
        None,
        "the selection changed"
    );
}

/// A shell tool unit whose output is `output`.
fn shell_session(output: &str) -> Vec<SessionEvent> {
    vec![
        user(1, "run it"),
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
                    model_output: output.to_owned(),
                    is_error: false,
                    presentation: None,
                    result: None,
                    content: Vec::new(),
                },
            },
        ),
    ]
}

/// B3/C34: fragments of tool output follow the cells the buffer paints, so zero-width
/// graphemes and control characters never shift hits, highlights or the copied range.
#[test]
fn tool_output_fragments_follow_the_painted_cells_around_zero_width_and_control_text() {
    let at = Instant::now();
    let output = "\u{FEFF}abc\tdef\u{200B}ghi 界x";
    let mut engine = attach(&shell_session(output), at);
    let painted = paint(&mut engine, at);
    let from = painted.find("ghi");
    let mut to = from;
    to.x += 2;
    select(&mut engine, &painted, from, to);
    assert_eq!(engine.selected_text().as_deref(), Some("ghi"));
    let highlighted = paint(&mut engine, at).highlighted;
    assert_eq!(
        highlighted,
        (from.x..=to.x)
            .map(|x| Point::new(x, from.y))
            .collect::<Vec<_>>(),
        "the highlight covers exactly the painted cells"
    );
}

/// C34/C36: a copy of tool output is its logical source text: tabs and other control
/// characters painted as spaces are copied as they are.
#[test]
fn tool_output_copies_its_source_text_with_tabs() {
    let at = Instant::now();
    let output = "\u{FEFF}abc\tdef\u{200B}ghi 界x";
    let mut engine = attach(&shell_session(output), at);
    let painted = paint(&mut engine, at);
    let from = painted.find("abc");
    let mut to = painted.find("界");
    to.x += 2;
    select(&mut engine, &painted, from, to);
    assert_eq!(engine.selected_text().as_deref(), Some(output));
}

/// L3/L8: installing an accepted Markdown visual re-measures only the unit that owns it; an
/// unchanged visual state does no work and invalidates nothing.
#[test]
fn accepting_a_markdown_visual_remeasures_only_its_owning_unit() {
    use crate::layout::producers::markdown::{
        AcceptedMarkdownVisual, MarkdownVisual, MarkdownVisualState,
    };

    let at = Instant::now();
    let events = vec![
        user(1, "show me"),
        assistant(
            2,
            "before\n\n![diagram](https://example.invalid/a.png)\n\nafter",
        ),
        user(3, "and text"),
        assistant(4, "plain answer"),
    ];
    let mut engine = attach(&events, at);
    let _ = paint(&mut engine, at);
    let (owner, slot) = engine
        .units()
        .keys()
        .iter()
        .find_map(|key| {
            let layout = engine.layouts().current(key)?;
            let slot = layout.geometry.visual_slots.first()?.clone();
            Some((key.clone(), slot))
        })
        .expect("an image slot");
    let _ = engine.take_layout_work();
    let mut visuals = MarkdownVisualState::new();
    visuals.accept(
        slot.contribution_id.clone(),
        AcceptedMarkdownVisual {
            source_key: slot.request.source_key(),
            revision: 1,
            visual: MarkdownVisual::Notice(std::sync::Arc::from("image unavailable")),
        },
    );
    let revision = engine
        .layouts()
        .current(&owner)
        .map(|layout| layout.key.clone());
    let update = engine.set_markdown_visuals(visuals.clone(), at);
    assert!(update.invalidated);
    assert_eq!(engine.take_layout_work().measurements, 1, "only the owner");
    assert_ne!(
        engine
            .layouts()
            .current(&owner)
            .map(|layout| layout.key.clone()),
        revision
    );
    assert!(
        paint(&mut engine, at)
            .rows
            .iter()
            .any(|row| row.contains("image unavailable"))
    );
    let update = engine.set_markdown_visuals(visuals, at);
    assert!(!update.invalidated, "nothing changed");
    assert!(engine.take_layout_work().is_zero());
}
