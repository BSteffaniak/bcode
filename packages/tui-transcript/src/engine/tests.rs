//! Engine façade tests: attach, incorporation order (W3/N11), timers, frames and stale
//! requests, observed through prepared frames and painted rows.

use std::time::{Duration, Instant};

use bcode_session_models::{
    ClientId, SessionEvent, SessionEventKind, SessionId, TurnAdmissionMetadata,
};
use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;

use super::*;
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
    /// Attach with the newest `resident` events of a `turns`-turn session.
    fn attach(turns: u64, resident: usize, motion: MotionConfig) -> Self {
        let events = session(turns);
        let mut harness = Self {
            engine: TranscriptEngine::new(ViewportGeometry::new(WIDTH, HEIGHT), motion),
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
        let snapshot = self.engine.prepare_frame(FrameClock { now: self.at() });
        let width = u16::try_from(WIDTH).expect("width");
        let height = u16::try_from(HEIGHT).expect("height");
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        let mut frame = Frame::new(&mut buffer);
        self.engine
            .paint_frame(&snapshot, &mut PaintCx::new(&mut frame));
        let rows = (0..height)
            .map(|row| frame.buffer().row_symbols(row).unwrap_or_default())
            .collect();
        let _ = self.engine.commit(snapshot.id);
        rows
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
    use crate::scenario::{CountingProducer, LayoutWorkProbe};
    use crate::units::producers::PlainTextProducer;

    let probe = LayoutWorkProbe::new();
    let mut harness = Harness::attach(30, 8, MotionConfig::INSTANT);
    // Re-attach through a counting engine with the same session.
    harness.engine = TranscriptEngine::with_producer(
        ViewportGeometry::new(WIDTH, HEIGHT),
        MotionConfig::INSTANT,
        Box::new(CountingProducer::new(PlainTextProducer, probe.clone())),
    );
    harness.requests.clear();
    let at = harness.at();
    let update = harness.engine.attach(at);
    let HostRequest::Window(issued) = update.requests[0] else {
        panic!("attach requests a window");
    };
    let start = harness.events.len() - 8;
    let result = HistoryWindowResult {
        generation: issued.generation,
        target: issued.request,
        events: harness.events[start..].to_vec(),
        has_older: true,
        has_newer: false,
        first_available_sequence: Some(1),
        last_available_sequence: harness.events.last().map(|event| event.sequence),
    };
    let update = harness.engine.apply_window(&result, at).expect("attach");
    harness.requests.extend(update.requests);
    let _ = harness.present();
    let attached = probe.take();
    assert!(attached.measurements > 0);

    harness.scroll(-2);
    let _ = harness.present();
    harness.scroll(-1);
    let _ = harness.present();
    assert_eq!(
        probe.take().measurements,
        0,
        "scroll-only frames never measure"
    );

    let units_before = harness.engine.units().len();
    harness.complete_older(4);
    let added = harness.engine.units().len() - units_before;
    assert!(added > 0);
    let measured = probe.take().measurements;
    assert_eq!(
        measured,
        u64::try_from(added).expect("count"),
        "only the page's units are measured"
    );
}
