//! Focus (section 6) and latest-bar (F6, C18–C21) behavior through the engine façade.

use std::time::Duration;
use web_time::Instant;

use bcode_presentation_policy::models::Disclosure;
use bcode_session_models::{
    ClientId, SessionEvent, SessionEventKind, SessionId, TurnAdmissionMetadata,
};

use crate::engine::{HostRequest, TranscriptEngine};
use crate::focus::{FocusEdge, FocusStep, FocusTargetId};
use crate::frame::FrameClock;
use crate::layout::producers::testing::PlainTextProducer;
use crate::navigation::{InputSource, MotionConfig, NavIntent, NavMode};
use crate::units::UnitKey;
use crate::units::structure::StructureConfig;
use crate::viewport::ViewportGeometry;
use crate::window::{HistoryWindowResult, LiveInput};

const WIDTH: u64 = 40;
const HEIGHT: u64 = 10;

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

fn user(sequence: u64) -> SessionEvent {
    event(
        sequence,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: format!("u{sequence}"),
            admission: TurnAdmissionMetadata::default(),
        },
    )
}

fn assistant(sequence: u64, lines: u64) -> SessionEvent {
    event(
        sequence,
        SessionEventKind::AssistantMessage {
            text: (0..lines)
                .map(|line| format!("a{sequence}.{line}"))
                .collect::<Vec<_>>()
                .join("\\\n"),
        },
    )
}

fn tool(sequence: u64) -> SessionEvent {
    event(
        sequence,
        SessionEventKind::ToolCallRequested {
            tool_call_id: format!("call-{sequence}"),
            producer_plugin_id: None,
            tool_name: "read".to_owned(),
            arguments_json: "{}".to_owned(),
            working_directory: None,
        },
    )
}

fn grouping_config() -> StructureConfig {
    let rules: Vec<bcode_presentation_policy::models::PresentationRule> =
        serde_json::from_value(serde_json::json!([
            { "match": { "kind": "tool" }, "set": { "group": { "by": "consecutive" } } },
            { "match": { "kind": "group" }, "set": { "disclosure": "collapsed" } }
        ]))
        .expect("rules");
    let (policy, _) = bcode_presentation_policy::CompiledPolicy::compile(
        &[bcode_presentation_policy::models::RuleLayer {
            source: bcode_presentation_policy::models::LayerSource::Config,
            rules,
        }],
        &bcode_presentation_policy::PolicyRegistry::with_host_parts(),
    );
    StructureConfig {
        policy: std::sync::Arc::new(policy),
        ..StructureConfig::default()
    }
}

/// Attach `engine` with all of `events` resident.
fn attach(mut engine: TranscriptEngine, events: &[SessionEvent], at: Instant) -> TranscriptEngine {
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

fn plain_engine() -> TranscriptEngine {
    TranscriptEngine::with_producer(
        ViewportGeometry::new(WIDTH, HEIGHT),
        MotionConfig::INSTANT,
        Box::new(PlainTextProducer),
    )
}

/// `turns` turns of a one-line user message and a three-line answer (four rows a turn).
fn chat(turns: u64) -> Vec<SessionEvent> {
    (1..=turns)
        .flat_map(|turn| [user(turn * 2 - 1), assistant(turn * 2, 3)])
        .collect()
}

fn scroll(engine: &mut TranscriptEngine, rows: i64, at: Instant) {
    let _ = engine.intent(
        NavIntent::ScrollBy {
            rows,
            source: InputSource::Wheel,
        },
        at,
    );
}

fn tool_engine(at: Instant) -> TranscriptEngine {
    // Three tool groups separated by answers; every group header is a disclosure target.
    let mut events = Vec::new();
    let mut sequence = 0;
    let mut next = || {
        sequence += 1;
        sequence
    };
    for _ in 0..3 {
        events.push(user(next()));
        for _ in 0..2 {
            events.push(tool(next()));
        }
        events.push(assistant(next(), 1));
    }
    let engine = TranscriptEngine::new(ViewportGeometry::new(WIDTH, 40), MotionConfig::INSTANT)
        .with_structure_config(grouping_config());
    attach(engine, &events, at)
}

fn group_headers(engine: &TranscriptEngine) -> Vec<UnitKey> {
    engine
        .units()
        .keys()
        .iter()
        .filter(|key| matches!(key, UnitKey::GroupHeader(_)))
        .cloned()
        .collect()
}

/// Every focus target in document order.
fn all_targets(engine: &TranscriptEngine) -> Vec<FocusTargetId> {
    let view = engine.focus_view();
    engine
        .units()
        .keys()
        .iter()
        .flat_map(|unit| {
            view.targets(unit).iter().map(|target| FocusTargetId {
                unit: unit.clone(),
                local: target.local,
            })
        })
        .collect()
}

#[test]
fn x9_c33_focus_enters_on_the_last_visible_target_and_releases_to_the_composer() {
    let at = Instant::now();
    let mut engine = tool_engine(at);
    assert_eq!(group_headers(&engine).len(), 3);
    let targets = all_targets(&engine);
    assert!(targets.len() > 3, "{targets:?}");
    assert!(engine.focus().transcript_target().is_none(), "FO1 default");

    let (update, entered) = engine.enter_focus(at);
    assert!(entered);
    assert!(update.invalidated);
    let target = engine.focus().transcript_target().cloned().expect("focus");
    assert_eq!(Some(&target), targets.last(), "C33: last visible target");

    let (_, again) = engine.enter_focus(at);
    assert!(!again, "already focused");
    assert!(engine.release_focus());
    assert!(engine.focus().transcript_target().is_none());
    assert!(!engine.release_focus());
}

#[test]
fn c33_initial_focus_falls_back_to_the_nearest_target_above_the_viewport() {
    let at = Instant::now();
    let mut engine = tool_engine(at);
    let targets = all_targets(&engine);
    let _ = engine.resize(ViewportGeometry::new(WIDTH, 1), at);
    let _ = engine.enter_focus(at);
    let focused = engine.focus().transcript_target().cloned().expect("focus");
    assert!(targets.contains(&focused));
}

#[test]
fn fo6_focus_steps_through_targets_in_document_order_and_stops_at_the_window_ends() {
    let at = Instant::now();
    let mut engine = tool_engine(at);
    let targets = all_targets(&engine);
    let _ = engine.enter_focus(at);
    let _ = engine.focus_step(FocusStep::Previous, at);
    assert_eq!(
        engine.focus().transcript_target(),
        targets.get(targets.len() - 2)
    );
    for _ in 0..targets.len() + 2 {
        let _ = engine.focus_step(FocusStep::Previous, at);
    }
    assert_eq!(
        engine.focus().transcript_target(),
        targets.first(),
        "focus stays on the first target (FO8: no paging)"
    );
    let _ = engine.focus_step(FocusStep::Next, at);
    assert_eq!(engine.focus().transcript_target(), targets.get(1));
    for _ in 0..targets.len() + 2 {
        let _ = engine.focus_step(FocusStep::Next, at);
    }
    assert_eq!(engine.focus().transcript_target(), targets.last());
}

#[test]
fn fo8_collapsing_the_focused_group_keeps_focus_on_its_header() {
    let at = Instant::now();
    let mut engine = tool_engine(at);
    let header = group_headers(&engine)[2].clone();
    let _ = engine.focus_target(FocusTargetId::unit(header.clone()), at);
    let disclosure = engine
        .focus_state()
        .disclosure()
        .cloned()
        .expect("group disclosure");
    let (_, changed) = engine.set_disclosure(&disclosure, Disclosure::Expanded, at);
    assert!(changed);
    // Focus a member tool of the expanded group, then collapse the group from it.
    let _ = engine.focus_step(FocusStep::Next, at);
    let member = engine.focus().transcript_target().cloned().expect("member");
    assert!(matches!(member.unit, UnitKey::Item(_)), "{member:?}");
    let (_, changed) = engine.set_disclosure(&disclosure, Disclosure::Collapsed, at);
    assert!(changed);
    assert_eq!(
        engine.focus().transcript_target(),
        Some(&FocusTargetId::unit(header)),
        "P4/U2: focus follows the folded member to its group header"
    );
}

#[test]
fn fo4_edge_jumps_move_focus_to_the_first_and_last_target() {
    let at = Instant::now();
    let mut engine = tool_engine(at);
    let targets = all_targets(&engine);
    let _ = engine.enter_focus(at);
    let _ = engine.jump_to_edge(FocusEdge::First, at);
    assert_eq!(engine.focus().transcript_target(), targets.first());
    let _ = engine.jump_to_edge(FocusEdge::Last, at);
    assert_eq!(engine.focus().transcript_target(), targets.last());
    assert_eq!(engine.nav().mode, NavMode::Following);
}

#[test]
fn c18_c19_latest_bar_shows_only_for_a_distinct_newer_entry_below_the_full_region() {
    let at = Instant::now();
    let mut engine = attach(plain_engine(), &chat(10), at);
    let _ = engine.set_latest_bar_enabled(true, at);
    assert!(!engine.latest_bar().visible, "following: hidden");
    assert_eq!(engine.geometry().height, HEIGHT);

    // One row up: the last answer continues below, but no distinct entry is hidden (C18).
    scroll(&mut engine, -1, at);
    assert!(!engine.latest_bar().visible);
    assert_eq!(engine.geometry().height, HEIGHT);

    // Far enough that the last user message and answer are fully below.
    scroll(&mut engine, -8, at);
    let bar = engine.latest_bar();
    assert!(bar.visible);
    assert_eq!(
        engine.geometry().height,
        HEIGHT - 1,
        "F6: the bar row is reserved"
    );
    assert_eq!(engine.region().height, HEIGHT);
    let snapshot = engine.prepare_frame(FrameClock { now: at });
    assert_eq!(snapshot.latest_bar, bar, "build_frame copies the decision");
    assert_eq!(snapshot.geometry.height, HEIGHT - 1);

    let _ = engine.intent(NavIntent::JumpToLatest, at);
    assert!(!engine.latest_bar().visible);
    assert_eq!(engine.geometry().height, HEIGHT);
}

#[test]
fn c20_hidden_regeneration_is_activity_and_following_content_is_not() {
    let at = Instant::now();
    let mut engine = attach(plain_engine(), &chat(10), at);
    let _ = engine.set_latest_bar_enabled(true, at);
    let _ = engine.apply_live(LiveInput::Durable(Box::new(user(21))), at);
    assert_eq!(engine.latest_bar().activity_at, None, "following");

    scroll(&mut engine, -12, at);
    assert!(engine.latest_bar().visible);
    assert_eq!(engine.latest_bar().activity_at, None);
    let later = at + Duration::from_millis(100);
    let _ = engine.apply_live(LiveInput::Durable(Box::new(assistant(22, 2))), later);
    let bar = engine.latest_bar();
    assert_eq!(bar.activity_at, Some(later));
    assert_eq!(bar.burst, 1);
    let _ = engine.apply_live(
        LiveInput::Durable(Box::new(user(23))),
        later + Duration::from_millis(50),
    );
    assert_eq!(engine.latest_bar().burst, 2);
}

#[test]
fn disabled_latest_bar_never_reserves_a_row() {
    let at = Instant::now();
    let mut engine = attach(plain_engine(), &chat(10), at);
    scroll(&mut engine, -12, at);
    assert!(!engine.latest_bar().visible);
    assert_eq!(engine.geometry().height, HEIGHT);
}
