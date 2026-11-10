//! Phase-5 engine tests for the resident window (spec section 5): prefetch in both directions
//! (W5, W6, W7a), eviction and the `Live → Detached` transition (W7, W9), aligned timeline
//! windows (W8, BUG-B1, BUG-B2, BUG-B5), direct `Head`/`Tail` windows and fast return to live
//! (W10, X11, BUG-B3), continuous reading across pages (X12, BUG-A*) and the whole-session
//! estimate (W12). A scripted host answers requests like the production executor: pages of 512
//! events, `Around` windows of 256 events on each side, `Head`/`Tail` windows of 512 events.
//! Observations are painted rows and engine state; [`TranscriptEngine::window_work`] proves
//! paging never rebuilds the window.

use std::time::Duration;
use web_time::Instant;

use bcode_session_models::{
    ClientId, SessionEvent, SessionEventKind, SessionId, TurnAdmissionMetadata,
};
use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;

use super::*;
use crate::focus::FocusEdge;
use crate::navigation::{InputSource, Placement};
use crate::window::{
    HistoryWindowResult, PAGE_EVENTS, RESIDENT_CEILING_EVENTS, RESIDENT_MAX_EVENTS, WindowMode,
};

const WIDTH: u64 = 40;
const HEIGHT: u64 = 10;
/// Rows of one turn with the plain producer: a one-line user message, a three-line answer.
const TURN_ROWS: u64 = 4;

fn turn_event(id: SessionId, sequence: u64) -> SessionEvent {
    SessionEvent {
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
    }
}

/// Global content row of a painted line (`u{s}` or `a{s}.{l}`), from the fixture's fixed turn
/// geometry; `None` for boundary or blank rows.
fn content_row(line: &str) -> Option<u64> {
    let token = line.trim_end().trim_end_matches('\\');
    if let Some(sequence) = token.strip_prefix('u') {
        let sequence = sequence.parse::<u64>().ok()?;
        return Some((sequence - 1) / 2 * TURN_ROWS);
    }
    let (sequence, line) = token.strip_prefix('a')?.split_once('.')?;
    let sequence = sequence.parse::<u64>().ok()?;
    let line = line.parse::<u64>().ok()?;
    Some((sequence / 2 - 1) * TURN_ROWS + 1 + line)
}

fn row_of(rows: &[String], needle: &str) -> Option<usize> {
    rows.iter()
        .position(|row| row.trim_end().trim_end_matches('\\') == needle)
}

/// Host double: owns the whole canonical session and answers awaited requests, optionally
/// after a latency measured in steps.
struct Host {
    engine: TranscriptEngine,
    id: SessionId,
    events: Vec<SessionEvent>,
    origin: Instant,
    elapsed: Duration,
    step: u64,
    /// Issued requests not yet answered, with the step they were issued at.
    pending: Vec<(u64, HostRequest)>,
    /// Every request the engine issued, in order.
    issued: Vec<HostRequest>,
}

impl Host {
    /// A `turns`-turn session attached at its tail (W10), with the plain fixture rows.
    fn attach(turns: u64) -> Self {
        Self::attach_with(plain_engine(HEIGHT), turns)
    }

    /// A `turns`-turn session attached at its tail by `engine`.
    fn attach_with(engine: TranscriptEngine, turns: u64) -> Self {
        let id = SessionId::new();
        let mut host = Self {
            engine,
            id,
            events: (1..=turns * 2).map(|s| turn_event(id, s)).collect(),
            origin: Instant::now(),
            elapsed: Duration::ZERO,
            step: 0,
            pending: Vec::new(),
            issued: Vec::new(),
        };
        let update = host.engine.attach(host.at());
        host.absorb(update);
        host.pump(0);
        host
    }

    fn at(&self) -> Instant {
        self.origin + self.elapsed
    }

    fn absorb(&mut self, update: EngineUpdate) {
        for request in update.requests {
            self.issued.push(request);
            self.pending.push((self.step, request));
        }
    }

    /// Issued requests since `mark` (an index into [`Self::issued`]).
    fn issued_since(&self, mark: usize) -> Vec<HostRequest> {
        self.issued[mark..].to_vec()
    }

    fn intent(&mut self, intent: NavIntent) {
        let update = self.engine.intent(intent, self.at());
        self.absorb(update);
    }

    fn wheel(&mut self, rows: i64) {
        self.intent(NavIntent::ScrollBy {
            rows,
            source: InputSource::Wheel,
        });
    }

    /// Answer every awaited request issued at least `latency` steps ago, repeatedly, so
    /// follow-up requests with no latency are answered too. Requests no longer awaited are
    /// dropped, like the host's effect queue.
    fn pump(&mut self, latency: u64) {
        for _ in 0..64 {
            let engine = &self.engine;
            self.pending
                .retain(|(_, request)| engine.is_awaiting(request));
            let step = self.step;
            let Some(index) = self
                .pending
                .iter()
                .position(|(issued, _)| step.saturating_sub(*issued) >= latency)
            else {
                return;
            };
            let (_, request) = self.pending.remove(index);
            self.answer(request);
        }
        panic!("requests kept coming without settling");
    }

    /// Answer one request like `transcript_v2_host::effects::load_history`.
    fn answer(&mut self, request: HostRequest) {
        let page = usize::try_from(PAGE_EVENTS).expect("page fits usize");
        let len = self.events.len();
        let index = |sequence: u64| usize::try_from(sequence).expect("index fits usize");
        let at = self.at();
        let update = match request {
            HostRequest::Page(issued) => {
                let (direction, start, end, has_more) = match issued.request {
                    PageRequest::Older { before_sequence } => {
                        let end = index(before_sequence - 1).min(len);
                        let start = end.saturating_sub(page);
                        (PageDirection::Older, start, end, start > 0)
                    }
                    PageRequest::Newer { after_sequence } => {
                        let start = index(after_sequence).min(len);
                        let end = (start + page).min(len);
                        (PageDirection::Newer, start, end, end < len)
                    }
                };
                let result = HistoryPageResult {
                    generation: issued.generation,
                    direction,
                    events: self.events[start..end].to_vec(),
                    has_more,
                    unaligned: false,
                };
                self.engine.apply_page(&result, at).expect("awaited page")
            }
            HostRequest::Window(issued) => {
                let (start, end) = match issued.request {
                    WindowRequest::Tail => (len.saturating_sub(page), len),
                    WindowRequest::Head => (0, page.min(len)),
                    WindowRequest::Around { sequence } => {
                        let target = index(sequence).min(len);
                        (
                            target.saturating_sub(page / 2),
                            (target + page / 2).min(len),
                        )
                    }
                    WindowRequest::LiveState => {
                        let last = self.engine.window().last_sequence.unwrap_or(0);
                        (index(last).min(len), len)
                    }
                };
                let result = HistoryWindowResult {
                    generation: issued.generation,
                    target: issued.request,
                    events: self.events[start..end].to_vec(),
                    has_older: start > 0 && issued.request != WindowRequest::LiveState,
                    has_newer: end < len,
                    first_available_sequence: Some(1),
                    last_available_sequence: self.events.last().map(|event| event.sequence),
                };
                self.engine
                    .apply_window(&result, at)
                    .expect("awaited window")
            }
        };
        self.absorb(update);
    }

    /// Append one turn to the session and deliver it live.
    fn live_turn(&mut self) {
        for _ in 0..2 {
            let sequence = self.events.len() as u64 + 1;
            let event = turn_event(self.id, sequence);
            self.events.push(event.clone());
            let update = self
                .engine
                .apply_live(LiveInput::Durable(Box::new(event)), self.at());
            self.absorb(update);
        }
    }

    /// Prepare, paint and commit one frame; returns the painted rows.
    fn present(&mut self) -> Vec<String> {
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
            .collect::<Vec<_>>();
        let update = self.engine.commit(snapshot.id);
        self.absorb(update);
        rows
    }

    /// Global content row on the viewport top row of a painted frame.
    fn top_row(rows: &[String]) -> u64 {
        rows.iter()
            .enumerate()
            .find_map(|(offset, row)| {
                content_row(row).map(|global| global.saturating_sub(offset as u64))
            })
            .expect("a content row is painted")
    }

    /// Timeline jump to `sequence` with `placement`; answers only the `Around` window and
    /// returns the requests the jump issued.
    fn jump(&mut self, sequence: u64, placement: Placement) -> Vec<HostRequest> {
        let mark = self.issued.len();
        self.intent(NavIntent::JumpToSequence {
            sequence,
            placement,
        });
        let requested = self.issued_since(mark);
        self.answer_window();
        requested
    }

    /// Answer the awaited window request only.
    fn answer_window(&mut self) {
        let engine = &self.engine;
        let index = self
            .pending
            .iter()
            .position(|(_, request)| {
                matches!(request, HostRequest::Window(_)) && engine.is_awaiting(request)
            })
            .expect("a window is awaited");
        let (_, request) = self.pending.remove(index);
        self.answer(request);
    }

    /// The awaited page in `direction`, if any.
    fn awaited_page(&self, direction: PageDirection) -> Option<HostRequest> {
        self.pending
            .iter()
            .map(|(_, request)| *request)
            .find(|request| {
                self.engine.is_awaiting(request)
                    && matches!(request, HostRequest::Page(issued)
                        if issued.request.direction() == direction)
            })
    }

    /// Wheel `rows` per step (without answering anything) until a page in `direction` is
    /// awaited; returns the rows scrolled.
    fn wheel_until_page(&mut self, rows: i64, direction: PageDirection) -> u64 {
        let mut scrolled = 0;
        for _ in 0..1_000 {
            if self.awaited_page(direction).is_some() {
                return scrolled;
            }
            self.wheel(rows);
            scrolled += rows.unsigned_abs();
        }
        panic!("no {direction:?} page was requested");
    }
}

fn plain_engine(height: u64) -> TranscriptEngine {
    TranscriptEngine::with_producer(
        ViewportGeometry::new(WIDTH, height),
        MotionConfig::INSTANT,
        Box::new(crate::layout::producers::testing::PlainTextProducer),
    )
}

fn windows(requests: &[HostRequest]) -> Vec<WindowRequest> {
    requests
        .iter()
        .filter_map(|request| match request {
            HostRequest::Window(issued) => Some(issued.request),
            HostRequest::Page(_) => None,
        })
        .collect()
}

fn pages(requests: &[HostRequest]) -> Vec<PageRequest> {
    requests
        .iter()
        .filter_map(|request| match request {
            HostRequest::Page(issued) => Some(issued.request),
            HostRequest::Window(_) => None,
        })
        .collect()
}

#[test]
fn w8_t18_timeline_jump_lands_on_the_target_with_its_placement() {
    let mut host = Host::attach(2_000);
    let requested = host.jump(1_001, Placement::ItemTopAtTop);
    assert_eq!(
        windows(&requested),
        vec![WindowRequest::Around { sequence: 1_001 }],
        "one Around window, no paging toward the target"
    );
    assert_eq!(pages(&requested), Vec::<PageRequest>::new());
    let rows = host.present();
    assert_eq!(row_of(&rows, "u1001"), Some(0), "target top on the top row");
    assert_eq!(host.engine.window().mode, WindowMode::Detached);

    let mut host = Host::attach(2_000);
    let _ = host.jump(1_001, Placement::ItemBottomAtBottom);
    let rows = host.present();
    assert_eq!(
        row_of(&rows, "u1001"),
        Some(usize::try_from(HEIGHT - 1).expect("row")),
        "target bottom on the bottom row"
    );
}

#[test]
fn bug_b5_w8_timeline_window_edges_are_whole_turns_and_bounded() {
    let mut host = Host::attach(2_000);
    let _ = host.jump(1_000, Placement::ItemTopAtTop);
    let window = host.engine.window().clone();
    let first = window.first_sequence.expect("resident");
    let last = window.last_sequence.expect("resident");
    assert_eq!(
        first % 2,
        1,
        "the window starts at a user message (turn start)"
    );
    assert_eq!(last % 2, 0, "the window ends with a complete turn");
    assert!(window.event_count <= PAGE_EVENTS, "W8: at most 512 events");
    assert!(window.contains_sequence(1_000));
    let edge_height = |host: &Host, sequence: u64| {
        let unit = host.engine.units().unit_for_sequence(sequence)?;
        host.engine
            .viewport()
            .index()
            .item_by_key(unit)
            .map(|item| item.height)
    };
    let (first_height, last_height) = (edge_height(&host, first), edge_height(&host, last));
    // Pages on both sides never complete an edge unit in place (BUG-B5).
    let _ = host.wheel_until_page(-5, PageDirection::Older);
    host.pump(0);
    assert!(host.engine.window().first_sequence < Some(first));
    assert_eq!(edge_height(&host, first), first_height);
    // Reading down far enough evicts the old leading edge; the trailing edge is checked once
    // its own page lands.
    let _ = host.wheel_until_page(5, PageDirection::Newer);
    host.pump(0);
    assert!(host.engine.window().last_sequence > Some(last));
    assert_eq!(edge_height(&host, last), last_height);
    assert_eq!(
        host.engine.window_work().fallbacks,
        0,
        "aligned edges splice"
    );
}

#[test]
fn w5_detached_window_prefetches_one_page_per_side() {
    // A viewport of 300 rows prefetches 600 rows, more than an `Around` window holds on either
    // side of its target, so both sides are requested as soon as it lands.
    let mut host = Host::attach_with(plain_engine(300), 2_000);
    let mark = host.issued.len();
    host.intent(NavIntent::JumpToSequence {
        sequence: 1_001,
        placement: Placement::Center,
    });
    // The replacement is in flight: no page is requested beside it (W5).
    assert_eq!(pages(&host.issued_since(mark)), Vec::<PageRequest>::new());
    let mark = host.issued.len();
    host.answer_window();
    let prefetch = pages(&host.issued_since(mark));
    assert_eq!(prefetch.len(), 2, "one page per side: {prefetch:?}");
    assert!(
        prefetch
            .iter()
            .any(|request| matches!(request, PageRequest::Older { .. }))
    );
    assert!(
        prefetch
            .iter()
            .any(|request| matches!(request, PageRequest::Newer { .. }))
    );
    // Further updates while both are in flight request nothing more.
    let mark = host.issued.len();
    host.wheel(1);
    host.wheel(-1);
    assert_eq!(pages(&host.issued_since(mark)), Vec::<PageRequest>::new());
    // W6: answering continues until the prefetch distance is covered or the side is exhausted.
    host.pump(0);
    assert!(host.engine.window().event_count > PAGE_EVENTS);
    assert!(host.engine.window().event_count <= RESIDENT_CEILING_EVENTS);
}

#[test]
fn bug_b1_first_newer_page_after_a_timeline_jump_does_not_evict() {
    let mut host = Host::attach(2_000);
    let _ = host.jump(1_001, Placement::ItemTopAtTop);
    let first = host.engine.window().first_sequence;
    let scrolled = host.wheel_until_page(5, PageDirection::Newer);
    let request = host
        .awaited_page(PageDirection::Newer)
        .expect("newer page awaited");
    host.answer(request);
    assert_eq!(host.engine.window_work().evictions, 0, "W8: no eviction");
    assert_eq!(host.engine.window().first_sequence, first);
    assert!(host.engine.window().event_count <= RESIDENT_MAX_EVENTS);
    let rows = host.present();
    assert_eq!(
        Host::top_row(&rows),
        content_row("u1001").expect("row") + scrolled,
        "the page did not move the reader"
    );
}

#[test]
fn bug_b1_b2_x12_reading_down_from_a_timeline_jump_is_continuous_and_keeps_the_anchor() {
    let mut host = Host::attach(2_500);
    let _ = host.jump(1_001, Placement::ItemTopAtTop);
    let mut top = Host::top_row(&host.present());
    let end = 2_500 * TURN_ROWS - HEIGHT;
    let mut steps = 0;
    while top + 3 <= end && host.engine.window().mode == WindowMode::Detached {
        host.step += 1;
        host.wheel(3);
        // Page latency of 8 steps (24 rows) is below the time to scroll 48 prefetch rows.
        host.pump(8);
        let rows = host.present();
        let next = Host::top_row(&rows);
        assert_eq!(next, top + 3, "step {steps}: no stall, leap or blank frame");
        top = next;
        steps += 1;
        assert!(host.engine.window().event_count <= RESIDENT_CEILING_EVENTS);
    }
    let work = host.engine.window_work();
    assert!(
        work.evictions > 0,
        "older content was evicted on the way down"
    );
    assert_eq!(work.fallbacks, 0);
    assert_eq!(
        work.replacements, 2,
        "attach and the jump only: paging never rebuilds"
    );
    assert!(work.appends > 0);
}

#[test]
fn x12_w9_reading_up_from_live_is_continuous_and_detaches_on_tail_eviction() {
    // 5,000 events of history.
    let mut host = Host::attach(2_500);
    let mut top = Host::top_row(&host.present());
    let mut detached_at = None;
    while top >= 3 {
        host.step += 1;
        host.wheel(-3);
        host.pump(8);
        let rows = host.present();
        let next = Host::top_row(&rows);
        assert_eq!(
            next + 3,
            top,
            "step {}: monotonic, no stall or leap",
            host.step
        );
        top = next;
        assert!(host.engine.window().event_count <= RESIDENT_CEILING_EVENTS);
        if detached_at.is_none() && host.engine.window().mode == WindowMode::Detached {
            detached_at = Some(host.step);
        }
    }
    assert!(
        detached_at.is_some(),
        "W9: tail eviction switched to Detached"
    );
    assert_eq!(host.engine.window().first_sequence, Some(1));
    let work = host.engine.window_work();
    assert_eq!(
        work.replacements, 1,
        "the attach only: paging never rebuilds"
    );
    assert_eq!(work.fallbacks, 0);
    assert!(work.prepends >= 9 && work.evictions > 0);
}

/// Whether a painted row shows `needle` (standard producers add chrome around the text).
fn shows(rows: &[String], needle: &str) -> bool {
    rows.iter().any(|row| {
        row.split_whitespace()
            .any(|word| word.trim_end_matches('\\') == needle)
    })
}

/// Detach far from the tail with a timeline jump, then return with `return_to_latest`.
fn return_to_live(engine: TranscriptEngine, return_to_latest: impl FnOnce(&mut Host)) {
    let mut host = Host::attach_with(engine, 2_000);
    let _ = host.jump(1_001, Placement::ItemTopAtTop);
    let before = host.present();
    assert!(shows(&before, "u1001"));
    assert_eq!(host.engine.window().mode, WindowMode::Detached);

    let mark = host.issued.len();
    return_to_latest(&mut host);
    let requested = host.issued_since(mark);
    assert_eq!(
        windows(&requested),
        vec![WindowRequest::Tail],
        "X11: exactly one tail window"
    );
    assert_eq!(
        pages(&requested),
        Vec::<PageRequest>::new(),
        "W10: no paging toward the tail"
    );
    // While the tail window loads, frames keep showing the reader's content (no wrong items).
    assert_eq!(host.present(), before);

    let mark = host.issued.len();
    host.pump(0);
    assert_eq!(
        windows(&host.issued_since(mark)),
        Vec::<WindowRequest>::new(),
        "no further window requests"
    );
    assert_eq!(host.engine.window().mode, WindowMode::Live);
    assert_eq!(host.engine.nav().mode, NavMode::Following);
    let rows = host.present();
    assert!(shows(&rows, "a4000.2"), "latest content shown: {rows:#?}");
    assert!(!shows(&rows, "u1001"));
    // Live events apply and are followed.
    host.live_turn();
    let rows = host.present();
    assert!(shows(&rows, "a4002.2"), "live content followed: {rows:#?}");
    assert_eq!(host.engine.nav().mode, NavMode::Following);
}

#[test]
fn bug_b3_x11_jump_to_latest_returns_live_with_one_tail_window() {
    return_to_live(plain_engine(HEIGHT), |host| {
        host.intent(NavIntent::JumpToLatest);
    });
}

#[test]
fn bug_b3_end_in_transcript_focus_returns_live_with_one_tail_window() {
    // Standard producers: messages are focus targets (FO4: `End` in transcript focus).
    let engine = TranscriptEngine::new(ViewportGeometry::new(WIDTH, HEIGHT), MotionConfig::INSTANT);
    return_to_live(engine, |host| {
        let (update, entered) = host.engine.enter_focus(host.at());
        host.absorb(update);
        assert!(entered, "transcript focus");
        let update = host.engine.jump_to_edge(FocusEdge::Last, host.at());
        host.absorb(update);
    });
}

#[test]
fn w10_t16_jump_to_session_start_loads_the_head_window_directly() {
    let mut host = Host::attach(2_000);
    let mark = host.issued.len();
    host.intent(NavIntent::JumpToSessionStart);
    let requested = host.issued_since(mark);
    assert_eq!(windows(&requested), vec![WindowRequest::Head]);
    assert_eq!(
        pages(&requested),
        Vec::<PageRequest>::new(),
        "never pages through history"
    );
    host.pump(0);
    let rows = host.present();
    assert_eq!(Host::top_row(&rows), 0, "session start at the top");
    assert!(row_of(&rows, "u1").is_some());
    assert!(host.engine.window().contains_head());
    assert_eq!(host.engine.window().mode, WindowMode::Detached);
    assert!(matches!(host.engine.nav().mode, NavMode::Reading { .. }));
    // From the head, jump-to-latest is one tail window again.
    let mark = host.issued.len();
    host.intent(NavIntent::JumpToLatest);
    assert_eq!(windows(&host.issued_since(mark)), vec![WindowRequest::Tail]);
}

#[test]
fn w12_position_estimate_covers_the_whole_session() {
    let mut host = Host::attach(2_000);
    let estimate = host.engine.position_estimate();
    let total = host.engine.viewport().index().total_height();
    assert_eq!(estimate.resident, total);
    assert_eq!(estimate.after, 0, "the tail is resident");
    assert!(estimate.before > 0);

    let _ = host.jump(2_001, Placement::ItemTopAtTop);
    let middle = host.engine.position_estimate();
    assert!(middle.before > 0 && middle.after > 0);
    let session_rows = 2_000 * TURN_ROWS;
    let sum = middle.before + middle.resident + middle.after;
    assert!(
        sum > session_rows / 2 && sum < session_rows * 2,
        "estimate {sum} spans the session ({session_rows} rows)"
    );
    // A detached window counts newer events it observed live (W2), and the estimate never
    // moves the reader.
    let nav = host.engine.nav().clone();
    let after = middle.after;
    host.live_turn();
    assert!(host.engine.position_estimate().after > after);
    assert_eq!(host.engine.nav(), &nav);
}

#[test]
fn w7_w7a_eviction_waits_for_a_running_motion_and_the_ceiling_holds() {
    let mut host = Host::attach(2_000);
    // Read up until the window holds 1,024 events and the next older page is awaited.
    for _ in 0..2_000 {
        if host.engine.window().event_count >= 1_024
            && host.awaited_page(PageDirection::Older).is_some()
        {
            break;
        }
        host.pump(0);
        host.wheel(-5);
    }
    assert_eq!(host.engine.window().event_count, 1_024);
    let page = host
        .awaited_page(PageDirection::Older)
        .expect("older page awaited");

    // Start a motion toward a user message lower in the viewport: eviction is deferred.
    let motion = MotionConfig {
        duration: Duration::from_secs(1),
        ..MotionConfig::INSTANT
    };
    let update = host.engine.set_motion(motion, host.at());
    host.absorb(update);
    let rows = host.present();
    let target = rows
        .iter()
        .skip(1)
        .find_map(|row| row.trim_end().strip_prefix('u')?.parse::<u64>().ok())
        .expect("a user message below the top row");
    let item = host
        .engine
        .units()
        .unit_for_sequence(target)
        .cloned()
        .expect("resident");
    host.intent(NavIntent::JumpToItem {
        item,
        placement: Placement::ItemTopAtTop,
    });
    assert!(host.engine.nav().motion.is_some(), "motion running");

    let evictions = host.engine.window_work().evictions;
    host.answer(page);
    assert_eq!(host.engine.window().event_count, RESIDENT_CEILING_EVENTS);
    assert_eq!(
        host.engine.window_work().evictions,
        evictions,
        "W7: deferred while the motion runs"
    );
    assert!(
        host.awaited_page(PageDirection::Older).is_none(),
        "W7a: no page beyond the ceiling"
    );

    // The motion completes: the next update evicts down toward the target.
    host.elapsed += Duration::from_secs(2);
    let update = host.engine.advance(host.at());
    host.absorb(update);
    assert!(host.engine.nav().motion.is_none());
    assert_eq!(host.engine.window_work().evictions, evictions + 1);
    assert!(host.engine.window().event_count <= RESIDENT_MAX_EVENTS);
    let rows = host.present();
    assert_eq!(
        row_of(&rows, &format!("u{target}")),
        Some(0),
        "eviction kept the motion target in place"
    );
}

#[test]
fn w7_eviction_waits_for_an_active_selection() {
    use crate::selection::{SelectionKey, TranscriptSelection, TranscriptSelectionEndpoint};

    let mut host = Host::attach(2_000);
    for _ in 0..2_000 {
        if host.engine.window().event_count >= 1_024
            && host.awaited_page(PageDirection::Older).is_some()
        {
            break;
        }
        host.pump(0);
        host.wheel(-5);
    }
    let page = host
        .awaited_page(PageDirection::Older)
        .expect("older page awaited");
    // A selection between two resident units (a finished drag): eviction is deferred.
    let keys = host.engine.units().keys();
    let (first, last) = (keys[1].clone(), keys[keys.len() - 2].clone());
    let selection = TranscriptSelection::new(
        TranscriptSelectionEndpoint::new(SelectionKey::new(first, 0), 0),
        TranscriptSelectionEndpoint::new(SelectionKey::new(last, 0), 1),
    );
    let _ = host.engine.set_selection(Some(selection.clone()), false);
    let evictions = host.engine.window_work().evictions;
    host.answer(page);
    assert_eq!(
        host.engine.window_work().evictions,
        evictions,
        "W7: deferred while a selection exists"
    );
    assert_eq!(
        host.engine.selection(),
        Some(&selection),
        "the selection is kept"
    );
    // Clearing the selection lets the next update evict.
    let _ = host.engine.set_selection(None, false);
    host.wheel(1);
    assert!(host.engine.window_work().evictions > evictions);
}

/// W7: the W7a ceiling trim that still runs while a selection defers eviction never evicts a
/// unit holding a selection endpoint; the selection survives the trim.
#[test]
fn w7a_ceiling_trim_keeps_selection_endpoint_units() {
    use crate::selection::{SelectionKey, TranscriptSelection, TranscriptSelectionEndpoint};

    let mut host = Host::attach(2_000);
    let _ = host.jump(2_900, Placement::ItemTopAtTop);
    // The selection's anchor is in the oldest resident unit, on the side the ceiling trims
    // after newer pages; its focus is in a later one.
    let items = host
        .engine
        .units()
        .keys()
        .iter()
        .filter(|key| matches!(key, UnitKey::Item(_)))
        .cloned()
        .collect::<Vec<_>>();
    let (oldest, later) = (items[0].clone(), items[4].clone());
    let selection = TranscriptSelection::new(
        TranscriptSelectionEndpoint::new(SelectionKey::new(oldest.clone(), 0), 0),
        TranscriptSelectionEndpoint::new(SelectionKey::new(later, 0), 1),
    );
    let _ = host.engine.set_selection(Some(selection.clone()), false);
    // Read down to the live tail, answering newer pages only.
    for _ in 0..1_000 {
        if host.engine.window().mode == WindowMode::Live {
            break;
        }
        let engine = &host.engine;
        host.pending
            .retain(|(_, request)| engine.is_awaiting(request));
        let next = host.pending.iter().position(|(_, request)| {
            !matches!(request, HostRequest::Page(issued) if matches!(issued.request, PageRequest::Older { .. }))
        });
        if let Some(index) = next {
            let (_, request) = host.pending.remove(index);
            host.answer(request);
        }
        host.wheel(50);
    }
    assert_eq!(host.engine.window().mode, WindowMode::Live);
    assert_eq!(host.engine.selection(), Some(&selection));
    // Durable live events take the window past the ceiling. The trim would remove the head,
    // where the selection's anchor unit is pinned, so the window may pass the ceiling.
    for _ in 0..120 {
        host.live_turn();
    }
    assert!(host.engine.window().event_count > RESIDENT_CEILING_EVENTS);
    assert!(
        host.engine.units().contains(&oldest),
        "W7: the selection endpoint unit stays resident"
    );
    assert_eq!(
        host.engine.selection(),
        Some(&selection),
        "the selection survives the ceiling trim"
    );
    // One page above the ceiling the pins give way: the selection is dropped and the window
    // is trimmed back to the ceiling, so residency stays bounded.
    let evictions = host.engine.window_work().evictions;
    for _ in 0..PAGE_EVENTS {
        if host.engine.window_work().evictions > evictions {
            break;
        }
        host.live_turn();
    }
    assert!(host.engine.window_work().evictions > evictions);
    assert!(host.engine.window().event_count <= RESIDENT_CEILING_EVENTS);
    assert_eq!(host.engine.selection(), None);
}
