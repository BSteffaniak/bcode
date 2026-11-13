//! Engine façade tests: attach, incorporation order (W3/N11), timers, frames and stale
//! requests, observed through prepared frames and painted rows.

use std::time::Duration;
use web_time::Instant;

use bcode_session_models::{
    ClientId, SessionEvent, SessionEventKind, SessionId, TurnAdmissionMetadata,
};
use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;

use super::*;
use crate::layout::geometry::ItemAction;
use crate::navigation::InputSource;
use crate::window::HistoryWindowResult;

const WIDTH: u64 = 40;
const HEIGHT: u64 = 10;

/// Canonical session of `turns` turns: a one-line user message and a three-line assistant
/// message each. Every line carries its sequence.
fn session(turns: u64) -> Vec<SessionEvent> {
    let id = SessionId::new();
    (1..=turns * 2)
        .map(|sequence| SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id: id,
            provenance: None,
            kind: if sequence % 2 == 1 {
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: format!("u{sequence}"),
                    admission: TurnAdmissionMetadata::default(),
                }
            } else {
                SessionEventKind::AssistantMessage {
                    text: (0..3)
                        .map(|line| format!("a{sequence}.{line}"))
                        .collect::<Vec<_>>()
                        .join("\\\n"),
                }
            },
        })
        .collect()
}

struct Harness {
    engine: TranscriptEngine,
    events: Vec<SessionEvent>,
    origin: Instant,
    elapsed: Duration,
    requests: Vec<HostRequest>,
}

impl Harness {
    /// Attach with the newest `resident` events of a `turns`-turn session. Navigation tests
    /// use the plain fixture rows (one row per message line, no chrome) so their geometry does
    /// not depend on content producers.
    fn attach(turns: u64, resident: usize, motion: MotionConfig) -> Self {
        Self::attach_with(
            TranscriptEngine::with_producer(
                ViewportGeometry::new(WIDTH, HEIGHT),
                motion,
                Box::new(crate::layout::producers::testing::PlainTextProducer),
            ),
            session(turns),
            resident,
        )
    }

    /// Attach `engine` with the newest `resident` of `events`.
    fn attach_with(engine: TranscriptEngine, events: Vec<SessionEvent>, resident: usize) -> Self {
        let mut harness = Self {
            engine,
            events,
            origin: Instant::now(),
            elapsed: Duration::ZERO,
            requests: Vec::new(),
        };
        let at = harness.at();
        let update = harness.engine.attach(at);
        let HostRequest::Window(issued) = update.requests[0] else {
            panic!("attach requests a window");
        };
        let start = harness.events.len() - resident;
        let result = HistoryWindowResult {
            generation: issued.generation,
            target: issued.request,
            events: harness.events[start..].to_vec(),
            has_older: start > 0,
            has_newer: false,
            first_available_sequence: Some(1),
            last_available_sequence: harness.events.last().map(|event| event.sequence),
        };
        let update = harness.engine.apply_window(&result, at).expect("attach");
        harness.requests.extend(update.requests);
        harness
    }

    fn at(&self) -> Instant {
        self.origin + self.elapsed
    }

    fn advance(&mut self, by: Duration) {
        self.elapsed += by;
        let at = self.at();
        let update = self.engine.advance(at);
        self.requests.extend(update.requests);
    }

    fn scroll(&mut self, rows: i64) {
        let at = self.at();
        let update = self.engine.intent(
            NavIntent::ScrollBy {
                rows,
                source: InputSource::Wheel,
            },
            at,
        );
        self.requests.extend(update.requests);
    }

    fn awaited(&self) -> Vec<HostRequest> {
        self.requests
            .iter()
            .copied()
            .filter(|request| self.engine.is_awaiting(request))
            .collect()
    }

    /// Answer the awaited older page with up to `limit` events before its bound.
    fn complete_older(&mut self, limit: usize) {
        let Some(HostRequest::Page(issued)) = self
            .awaited()
            .into_iter()
            .find(|request| matches!(request, HostRequest::Page(_)))
        else {
            panic!("no older page awaited");
        };
        let PageRequest::Older { before_sequence } = issued.request else {
            panic!("expected an older page");
        };
        let end = self
            .events
            .partition_point(|event| event.sequence < before_sequence);
        let start = end.saturating_sub(limit);
        let result = HistoryPageResult {
            generation: issued.generation,
            direction: PageDirection::Older,
            events: self.events[start..end].to_vec(),
            has_more: start > 0,
            unaligned: false,
        };
        let at = self.at();
        let update = self.engine.apply_page(&result, at).expect("page applies");
        self.requests.extend(update.requests);
    }

    /// Prepare, paint and commit one frame; returns the painted rows.
    fn present(&mut self) -> Vec<String> {
        self.present_snapshot().0
    }

    /// Prepare, paint (with the frame's presentation clock, BMUX 8) and commit one frame;
    /// returns the painted rows and the committed snapshot.
    fn present_snapshot(&mut self) -> (Vec<String>, FrameSnapshot) {
        let clock = FrameClock { now: self.at() };
        let snapshot = self.engine.prepare_frame(clock);
        let width = u16::try_from(WIDTH).expect("width");
        let height = u16::try_from(HEIGHT).expect("height");
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        let mut frame = Frame::new(&mut buffer);
        frame.set_clock(Some(clock.presentation()));
        self.engine
            .paint_frame(&snapshot, &mut PaintCx::new(&mut frame));
        let rows = (0..height)
            .map(|row| frame.buffer().row_symbols(row).unwrap_or_default())
            .collect();
        let _ = self.engine.commit(snapshot.id);
        (rows, snapshot)
    }
}

fn row_of(rows: &[String], needle: &str) -> Option<usize> {
    rows.iter().position(|row| row.trim_end() == needle)
}

#[test]
fn attach_follows_the_latest_bottom_and_prefetches_older_history() {
    let mut harness = Harness::attach(30, 8, MotionConfig::INSTANT);
    let rows = harness.present();
    assert_eq!(
        row_of(&rows, "a60.2"),
        Some(9),
        "newest line on the last row"
    );
    assert_eq!(harness.engine.nav().mode, NavMode::Following);
    // W5: fewer than prefetch_rows rows above the viewport, so an older page is awaited.
    assert!(harness.awaited().iter().any(|request| matches!(
        request,
        HostRequest::Page(IssuedRequest {
            request: PageRequest::Older {
                before_sequence: 53
            },
            ..
        })
    )));
}

#[test]
fn bug_a1_page_then_wheel_before_the_frame_moves_only_by_the_tick() {
    let mut harness = Harness::attach(30, 8, MotionConfig::INSTANT);
    harness.scroll(-3);
    let before = harness.present();
    let reference = before[0].trim_end().to_owned();
    let row = row_of(&before, &reference).expect("visible");
    harness.complete_older(8);
    harness.scroll(-3);
    let after = harness.present();
    assert_eq!(row_of(&after, &reference), Some(row + 3));
}

#[test]
fn frames_are_idempotent_and_never_change_navigation() {
    let mut harness = Harness::attach(30, 8, MotionConfig::default());
    harness.scroll(-2);
    let nav = harness.engine.nav().clone();
    let first = harness.present();
    let second = harness.present();
    assert_eq!(first, second);
    assert_eq!(harness.engine.nav(), &nav);
}

#[test]
fn motion_frames_are_driven_by_timers_until_the_motion_settles() {
    let mut harness = Harness::attach(30, 8, MotionConfig::default());
    harness.scroll(-6);
    harness.advance(Duration::from_secs(1));
    let at = harness.at();
    let _ = harness.engine.intent(NavIntent::JumpToLatest, at);
    assert!(harness.engine.nav().motion.is_some());
    assert!(harness.engine.next_deadline().is_some());
    harness.advance(Duration::from_millis(40));
    let moving = harness.present();
    assert_ne!(row_of(&moving, "a60.2"), Some(9), "still moving");
    harness.advance(Duration::from_secs(1));
    let settled = harness.present();
    assert!(harness.engine.nav().motion.is_none());
    assert_eq!(row_of(&settled, "a60.2"), Some(9));
}

#[test]
fn t22_manual_input_cancels_a_pending_jump_and_its_window_request() {
    let mut harness = Harness::attach(30, 8, MotionConfig::INSTANT);
    harness.scroll(-2);
    let at = harness.at();
    let update = harness.engine.intent(
        NavIntent::JumpToSequence {
            sequence: 3,
            placement: crate::navigation::Placement::ItemTopAtTop,
        },
        at,
    );
    let jump = update
        .requests
        .iter()
        .copied()
        .find(|request| matches!(request, HostRequest::Window(_)))
        .expect("non-resident jump requests a window");
    assert!(harness.engine.is_awaiting(&jump));
    harness.requests.extend(update.requests);
    harness.scroll(-1);
    assert!(
        !harness.engine.is_awaiting(&jump),
        "the cancelled jump is not awaited"
    );
    assert_eq!(harness.engine.window().in_flight.replace, None);
}

#[test]
fn live_events_while_detached_are_not_applied() {
    let mut harness = Harness::attach(30, 8, MotionConfig::INSTANT);
    let at = harness.at();
    let update = harness.engine.intent(
        NavIntent::JumpToSequence {
            sequence: 11,
            placement: crate::navigation::Placement::ItemTopAtTop,
        },
        at,
    );
    let Some(HostRequest::Window(issued)) = update.requests.first().copied() else {
        panic!("window request");
    };
    let result = HistoryWindowResult {
        generation: issued.generation,
        target: issued.request,
        events: harness.events[4..20].to_vec(),
        has_older: true,
        has_newer: true,
        first_available_sequence: Some(1),
        last_available_sequence: Some(60),
    };
    let _ = harness.engine.apply_window(&result, at).expect("window");
    assert_eq!(
        harness.engine.window().mode,
        crate::window::WindowMode::Detached
    );
    let rows = harness.present();
    assert_eq!(row_of(&rows, "u11"), Some(0), "jump target at the top");
    let mut live = harness.events[0].clone();
    live.sequence = 61;
    let update = harness
        .engine
        .apply_live(LiveInput::Durable(Box::new(live)), at);
    assert_eq!(update.requests, Vec::new());
    assert_eq!(harness.present(), rows);
}

#[test]
fn superseded_attach_does_not_disable_abandoned_jump_cancellation() {
    let events = session(30);
    let origin = Instant::now();
    let mut engine =
        TranscriptEngine::new(ViewportGeometry::new(WIDTH, HEIGHT), MotionConfig::INSTANT);
    let attach = engine.attach(origin);
    let HostRequest::Window(tail) = attach.requests[0] else {
        panic!("attach requests a window");
    };
    // Session start before the tail window lands supersedes the attach request.
    let update = engine.intent(NavIntent::JumpToSessionStart, origin);
    let Some(HostRequest::Window(head)) = update.requests.first().copied() else {
        panic!("session start requests the head window");
    };
    assert_eq!(head.request, WindowRequest::Head);
    let stale = HistoryWindowResult {
        generation: tail.generation,
        target: tail.request,
        events: events[44..].to_vec(),
        has_older: true,
        has_newer: false,
        first_available_sequence: Some(1),
        last_available_sequence: Some(60),
    };
    assert!(engine.apply_window(&stale, origin).is_err());
    let result = HistoryWindowResult {
        generation: head.generation,
        target: head.request,
        events: events[..16].to_vec(),
        has_older: false,
        has_newer: true,
        first_available_sequence: Some(1),
        last_available_sequence: Some(60),
    };
    let _ = engine.apply_window(&result, origin).expect("head window");
    // A later non-resident jump abandoned by manual input must not replace the window.
    let update = engine.intent(
        NavIntent::JumpToSequence {
            sequence: 59,
            placement: crate::navigation::Placement::ItemTopAtTop,
        },
        origin,
    );
    let jump = update
        .requests
        .iter()
        .copied()
        .find(|request| matches!(request, HostRequest::Window(_)))
        .expect("non-resident jump requests a window");
    let _ = engine.intent(
        NavIntent::ScrollBy {
            rows: 1,
            source: InputSource::Wheel,
        },
        origin,
    );
    assert!(
        !engine.is_awaiting(&jump),
        "the abandoned jump is cancelled"
    );
    assert_eq!(engine.window().in_flight.replace, None);
}

#[test]
fn d4_scroll_frames_measure_nothing_and_a_page_measures_only_its_units() {
    let mut harness = Harness::attach(30, 8, MotionConfig::INSTANT);
    let _ = harness.present();
    let attached = harness.engine.take_layout_work();
    assert!(attached.measurements > 0);
    assert_eq!(attached.measurements, attached.producer_builds);

    harness.scroll(-2);
    let _ = harness.present();
    harness.scroll(-1);
    let _ = harness.present();
    assert!(
        harness.engine.take_layout_work().is_zero(),
        "scroll-only updates and frames do no layout work"
    );

    let units_before = harness.engine.units().len();
    harness.complete_older(4);
    let added = harness.engine.units().len() - units_before;
    assert!(added > 0);
    let work = harness.engine.take_layout_work();
    assert_eq!(
        work.measurements,
        u64::try_from(added).expect("count"),
        "only the page's units are measured"
    );
    assert_eq!(work.producer_builds, work.measurements);
}

// ---- item layout, structure and render configuration ----------------------------------------

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

/// One turn per entry: a user message, `tools` tool calls, then a three-line answer.
fn tool_session(turns: &[usize]) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    let mut sequence = 0;
    let mut next = || {
        sequence += 1;
        sequence
    };
    for (turn, &tools) in turns.iter().enumerate() {
        let seq = next();
        events.push(event(
            seq,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: format!("u{turn}"),
                admission: TurnAdmissionMetadata::default(),
            },
        ));
        for tool in 0..tools {
            let seq = next();
            events.push(event(
                seq,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: format!("call-{turn}-{tool}"),
                    producer_plugin_id: None,
                    tool_name: "read".to_owned(),
                    arguments_json: "{}".to_owned(),
                    working_directory: None,
                },
            ));
        }
        let seq = next();
        events.push(event(
            seq,
            SessionEventKind::AssistantMessage {
                text: format!("a{turn}.0\\\na{turn}.1\\\na{turn}.2"),
            },
        ));
    }
    events
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

fn group_headers(engine: &TranscriptEngine) -> Vec<UnitKey> {
    engine
        .units()
        .keys()
        .iter()
        .filter(|key| matches!(key, UnitKey::GroupHeader(_)))
        .cloned()
        .collect()
}

#[test]
fn x3_u2_groups_render_as_headers_and_toggle_inserts_and_removes_members() {
    let engine = TranscriptEngine::new(ViewportGeometry::new(WIDTH, HEIGHT), MotionConfig::INSTANT)
        .with_structure_config(grouping_config());
    let events = tool_session(&[3]);
    let resident = events.len();
    let mut harness = Harness::attach_with(engine, events, resident);
    let headers = group_headers(&harness.engine);
    assert_eq!(headers.len(), 1, "three consecutive calls form one group");
    let rows = harness.present();
    assert!(
        rows.iter()
            .any(|row| row.trim_end() == "▸ 3 tool calls · read ×3 · requested"),
        "collapsed header painted: {rows:?}"
    );
    assert!(!rows.iter().any(|row| row.contains("Tool · read")));

    let target = harness
        .engine
        .structure()
        .unit_presentation(&headers[0])
        .target()
        .cloned()
        .expect("group target");
    let at = harness.at();
    let (_, disclosure) = harness.engine.toggle_disclosure(&target, at);
    assert_eq!(disclosure, Some(Disclosure::Expanded));
    let _ = harness.present();
    // Expanded members are units directly after the header (taller than the viewport now
    // that tools render their anatomy, so this reads the unit list rather than painted rows).
    let keys = harness.engine.units().keys();
    let header = keys
        .iter()
        .position(|key| key == &headers[0])
        .expect("header unit");
    let members = harness
        .engine
        .structure()
        .unit_presentation(&headers[0])
        .clone();
    let crate::units::structure::ResolvedUnitPresentation::Group(group) = members else {
        panic!("group structure");
    };
    assert_eq!(
        keys[header + 1..header + 4],
        group
            .members
            .iter()
            .cloned()
            .map(UnitKey::Item)
            .collect::<Vec<_>>(),
        "expanded members are units"
    );

    // The header's retained geometry is the disclosure focus target and hit region (B3, X1).
    let layout = harness
        .engine
        .layouts()
        .current(&headers[0])
        .expect("header layout");
    assert_eq!(layout.geometry.hit_regions.len(), 1);
    assert_eq!(layout.geometry.hit_regions[0].rect.width, WIDTH);
    assert_eq!(
        layout.geometry.focus_targets[0].disclosure.as_ref(),
        Some(&target)
    );

    let at = harness.at();
    let (_, disclosure) = harness.engine.toggle_disclosure(&target, at);
    assert_eq!(disclosure, Some(Disclosure::Collapsed));
    let rows = harness.present();
    assert!(
        !rows.iter().any(|row| row.contains("Tool · read")),
        "{rows:?}"
    );
}

#[test]
fn c25_collapsing_a_group_above_the_viewport_keeps_the_reading_position() {
    let engine = TranscriptEngine::new(ViewportGeometry::new(WIDTH, HEIGHT), MotionConfig::INSTANT)
        .with_structure_config(grouping_config());
    let events = tool_session(&[4, 0, 0, 0, 0]);
    let resident = events.len();
    let mut harness = Harness::attach_with(engine, events, resident);
    let header = group_headers(&harness.engine)[0].clone();
    let target = harness
        .engine
        .structure()
        .unit_presentation(&header)
        .target()
        .cloned()
        .expect("group target");
    let at = harness.at();
    let _ = harness.engine.toggle_disclosure(&target, at);
    let _ = harness.present();
    // Read with a later answer at the top, the expanded group above the viewport.
    harness.scroll(-3);
    let before = harness.present();
    let top = before[0].clone();
    assert!(!top.trim().is_empty(), "{before:?}");

    let at = harness.at();
    let _ = harness.engine.toggle_disclosure(&target, at);
    let after = harness.present();
    assert_eq!(
        after[0], top,
        "collapse above the viewport does not move content"
    );
}

#[test]
fn l9_width_change_remeasures_resident_units_and_height_change_measures_nothing() {
    let mut harness = Harness::attach(10, 8, MotionConfig::INSTANT);
    let _ = harness.present();
    let _ = harness.engine.take_layout_work();
    let at = harness.at();
    let _ = harness
        .engine
        .resize(ViewportGeometry::new(WIDTH, HEIGHT + 4), at);
    let _ = harness.present();
    assert!(harness.engine.take_layout_work().is_zero());

    let at = harness.at();
    let _ = harness
        .engine
        .resize(ViewportGeometry::new(WIDTH / 2, HEIGHT + 4), at);
    let work = harness.engine.take_layout_work();
    assert_eq!(
        work.measurements,
        u64::try_from(harness.engine.units().len()).expect("count")
    );
}

#[test]
fn x4_theme_change_remeasures_every_unit_and_unchanged_config_measures_nothing() {
    let mut harness = Harness::attach(6, 8, MotionConfig::INSTANT);
    let _ = harness.present();
    let _ = harness.engine.take_layout_work();
    let at = harness.at();
    let same = harness.engine.render_config().clone();
    let _ = harness.engine.set_render_config(same, at);
    assert!(
        harness.engine.take_layout_work().is_zero(),
        "identical fingerprints keep every retained layout (L1)"
    );

    let mut themed = harness.engine.render_config().clone();
    let mut theme = (*themed.theme).clone();
    theme.fingerprint = theme.fingerprint.wrapping_add(1);
    themed.theme = std::sync::Arc::new(theme);
    let at = harness.at();
    let _ = harness.engine.set_render_config(themed, at);
    assert_eq!(
        harness.engine.take_layout_work().measurements,
        u64::try_from(harness.engine.units().len()).expect("count")
    );
}

#[test]
fn c24_source_backed_anchors_survive_a_width_change() {
    use crate::layout::producers::common::{SourceText, TextSource};
    use crate::layout::producers::{ProducedItem, UnitView};
    use bmux_tui::composition::TextBlock;
    use bmux_tui::text::{Line, Text, TextWrap};

    // Messages render as source-mapped, word-wrapped text; every other unit as plain rows.
    let producer = |unit: UnitView<'_>, cx: &crate::layout::ItemRenderContext<'_>| match unit
        .item
        .map(|item| &item.kind)
    {
        Some(
            bcode_session_view_models::TranscriptViewItemKind::AssistantMessage { message }
            | bcode_session_view_models::TranscriptViewItemKind::UserMessage { message },
        ) => ProducedItem::new(SourceText::new(
            TextBlock::new(Text::from_lines(vec![Line::raw(message.text.clone())]))
                .wrap(TextWrap::Word),
            TextSource::Markdown { base: 0 },
        )),
        _ => crate::layout::producers::FallbackProducer.produce(unit, cx),
    };
    let long = (0..200)
        .map(|word| format!("w{word:03}"))
        .collect::<Vec<_>>()
        .join(" ");
    let events = vec![
        event(
            1,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "question".to_owned(),
                admission: TurnAdmissionMetadata::default(),
            },
        ),
        event(2, SessionEventKind::AssistantMessage { text: long }),
        event(
            3,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "next".to_owned(),
                admission: TurnAdmissionMetadata::default(),
            },
        ),
    ];
    let engine = TranscriptEngine::with_producer(
        ViewportGeometry::new(WIDTH, HEIGHT),
        MotionConfig::INSTANT,
        Box::new(producer),
    );
    let mut harness = Harness::attach_with(engine, events, 3);
    let _ = harness.present();
    harness.scroll(-4);
    let before = harness.present();
    let first_word = |rows: &[String]| {
        rows.iter()
            .find_map(|row| {
                row.split_whitespace()
                    .next()
                    .filter(|word| word.starts_with('w'))
            })
            .map(ToOwned::to_owned)
    };
    let word = first_word(&before).expect("reading inside the answer");
    assert!(
        matches!(
            &harness.engine.nav().mode,
            NavMode::Reading { anchor }
                if matches!(anchor.offset, crate::navigation::AnchorOffset::Source { .. })
        ),
        "manual reading inside mapped content anchors to a source position: {:?}",
        harness.engine.nav().mode
    );

    let at = harness.at();
    let _ = harness
        .engine
        .resize(ViewportGeometry::new(WIDTH / 2, HEIGHT), at);
    let after = harness.present();
    // The source position at the top survives reflow: the first visible word of the answer
    // is the word that started the top row before (or the row containing it).
    assert!(
        after[0].split_whitespace().any(|top| top == word),
        "top row {:?} keeps {word:?} (before {before:?})",
        after[0]
    );
}

#[test]
fn d4_standard_producers_do_markdown_work_only_for_new_units() {
    let engine = TranscriptEngine::new(ViewportGeometry::new(WIDTH, HEIGHT), MotionConfig::INSTANT);
    let mut harness = Harness::attach_with(engine, session(30), 8);
    let _ = harness.present();
    let attached = harness.engine.take_layout_work();
    assert!(
        attached.markdown_parses > 0 && attached.projections > 0,
        "message producers parse and project Markdown: {attached:?}"
    );

    harness.scroll(-2);
    let _ = harness.present();
    harness.scroll(-1);
    let _ = harness.present();
    let scrolled = harness.engine.take_layout_work();
    assert!(
        scrolled.is_zero(),
        "scrolling does no layout work: {scrolled:?}"
    );

    let units_before = harness.engine.units().len();
    harness.complete_older(4);
    let added = u64::try_from(harness.engine.units().len() - units_before).expect("count");
    let page = harness.engine.take_layout_work();
    assert!(added > 0);
    assert_eq!(
        page.measurements, added,
        "only the page's units are measured"
    );
    assert!(
        page.markdown_parses > 0 && page.markdown_parses <= added,
        "only the page's messages are parsed: {page:?}"
    );
    let _ = harness.present();
    assert!(harness.engine.take_layout_work().is_zero());
}

#[test]
fn x1_b7_clicks_hit_retained_disclosure_regions_inside_the_viewport_only() {
    let engine = TranscriptEngine::new(ViewportGeometry::new(WIDTH, HEIGHT), MotionConfig::INSTANT)
        .with_structure_config(grouping_config());
    let events = tool_session(&[3]);
    let resident = events.len();
    let mut harness = Harness::attach_with(engine, events, resident);
    let (rows, snapshot) = harness.present_snapshot();
    let row = rows
        .iter()
        .position(|row| row.starts_with("▸ 3 tool calls"))
        .map(|row| u64::try_from(row).expect("row"))
        .expect("collapsed group header painted");

    // B7: nothing outside the viewport is hit.
    assert!(harness.engine.hit_test(&snapshot, 0, HEIGHT).is_none());
    assert!(harness.engine.hit_test(&snapshot, WIDTH, row).is_none());
    let hit = harness
        .engine
        .hit_test(&snapshot, 2, row)
        .expect("the header row is its group's toggle region (B3)");
    assert!(matches!(&hit.unit, UnitKey::GroupHeader(_)));
    assert!(matches!(
        &hit.action,
        ItemAction::ToggleDisclosure(DisclosureTarget::Group(_))
    ));

    let at = harness.at();
    let (update, clicked) = harness.engine.click(&snapshot, 2, row, at);
    assert!(update.invalidated);
    assert_eq!(clicked, Some(hit));
    // The header was laid out again, so the old frame's region no longer hits.
    assert!(harness.engine.hit_test(&snapshot, 2, row).is_none());
    assert_eq!(
        harness
            .engine
            .structure()
            .unit_presentation(&clicked.expect("hit").unit)
            .disclosure(),
        Disclosure::Expanded,
        "the click expanded the group"
    );
}

fn rules_config(rules: serde_json::Value) -> StructureConfig {
    let rules: Vec<bcode_presentation_policy::models::PresentationRule> =
        serde_json::from_value(rules).expect("rules");
    let (policy, diagnostics) = bcode_presentation_policy::CompiledPolicy::compile(
        &[bcode_presentation_policy::models::RuleLayer {
            source: bcode_presentation_policy::models::LayerSource::Config,
            rules,
        }],
        &bcode_presentation_policy::PolicyRegistry::with_host_parts(),
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    StructureConfig {
        policy: std::sync::Arc::new(policy),
        ..StructureConfig::default()
    }
}

/// A user message and one `read` call whose only argument is `file` (not a host target field).
fn declared_target_session() -> Vec<SessionEvent> {
    vec![
        event(
            1,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "u".to_owned(),
                admission: TurnAdmissionMetadata::default(),
            },
        ),
        event(
            2,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: None,
                tool_name: "read".to_owned(),
                arguments_json: r#"{"file":"lib.rs"}"#.to_owned(),
                working_directory: None,
            },
        ),
    ]
}

#[test]
fn declared_target_fields_re_render_retained_tool_headers() {
    let rules = serde_json::json!([{
        "match": { "kind": "tool" },
        "set": { "disclosure": "collapsed", "header": { "fields": ["label", "tool", "target"] } }
    }]);
    let engine = TranscriptEngine::new(ViewportGeometry::new(WIDTH, HEIGHT), MotionConfig::INSTANT)
        .with_structure_config(rules_config(rules.clone()));
    let mut harness = Harness::attach_with(engine, declared_target_session(), 2);
    let before = harness.present();
    assert!(
        !before.iter().any(|row| row.contains("lib.rs")),
        "no target without a declaration: {before:?}"
    );

    // A plugin declares `file` as the target field of `read`; the rules and their placements
    // do not change, but the rendered target fact does.
    let mut declared = rules_config(rules);
    let mut plugins = std::collections::BTreeMap::new();
    plugins.insert("plugin".to_owned(), vec!["file".to_owned()]);
    let mut fields = crate::units::structure::DeclaredTargetFields::new();
    fields.insert("read".to_owned(), plugins);
    declared.target_fields = std::sync::Arc::new(fields);
    let at = harness.at();
    let _ = harness.engine.set_structure_config(declared, at);
    let after = harness.present();
    assert!(
        after.iter().any(|row| row.contains("lib.rs")),
        "the retained header re-renders with the declared target: {after:?}"
    );
}

#[test]
fn b3_hits_use_only_the_layout_the_snapshot_painted() {
    let engine = TranscriptEngine::new(ViewportGeometry::new(WIDTH, HEIGHT), MotionConfig::INSTANT)
        .with_structure_config(grouping_config());
    let events = tool_session(&[3]);
    let resident = events.len();
    let mut harness = Harness::attach_with(engine, events, resident);
    let (rows, snapshot) = harness.present_snapshot();
    let row = rows
        .iter()
        .position(|row| row.starts_with("▸ 3 tool calls"))
        .map(|row| u64::try_from(row).expect("row"))
        .expect("collapsed group header painted");
    assert!(harness.engine.hit_test(&snapshot, 2, row).is_some());

    // New layout inputs with unchanged revisions and heights (an equivalent theme under a new
    // fingerprint): the snapshot never painted the new layouts, so it no longer hits them.
    let mut themed = harness.engine.render_config().clone();
    let mut theme = (*themed.theme).clone();
    theme.fingerprint = theme.fingerprint.wrapping_add(1);
    themed.theme = std::sync::Arc::new(theme);
    let at = harness.at();
    let _ = harness.engine.set_render_config(themed, at);
    assert!(
        harness.engine.hit_test(&snapshot, 2, row).is_none(),
        "stale snapshot geometry is not hit (B3, B7)"
    );
    let (fresh_rows, fresh) = harness.present_snapshot();
    assert_eq!(fresh_rows, rows, "same painted rows");
    assert!(harness.engine.hit_test(&fresh, 2, row).is_some());
}

#[test]
fn l1_toggling_one_details_block_remeasures_only_its_unit() {
    let engine = TranscriptEngine::new(ViewportGeometry::new(WIDTH, HEIGHT), MotionConfig::INSTANT);
    let mut harness = Harness::attach_with(engine, session(6), 8);
    let _ = harness.present();
    let _ = harness.engine.take_layout_work();

    let unit = harness
        .engine
        .units()
        .keys()
        .iter()
        .rev()
        .find(|key| matches!(key, UnitKey::Item(_)))
        .cloned()
        .expect("a message unit");
    let mut config = harness.engine.render_config().clone();
    config.markdown.details_open.insert(
        format!(
            "{}:details-0",
            crate::layout::context::markdown_document_id(&unit)
        ),
        true,
    );
    let at = harness.at();
    let _ = harness.engine.set_render_config(config, at);
    let work = harness.engine.take_layout_work();
    assert_eq!(work.measurements, 1, "only the owning unit: {work:?}");
    assert_eq!(work.markdown_parses, 1, "{work:?}");
}
