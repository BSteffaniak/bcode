//! Resident window and paging scenarios (TUI V2 phase 5): prefetch, eviction and the resident
//! ceiling, aligned timeline windows, direct head and tail windows, return to live, deep history
//! paging in both directions and the optional scrollbar.
//!
//! The scenarios cover W1–W13 (with W7a), X6, X11, X12, BUG-B3 through `End` in transcript focus
//! and BUG-B5. Paging work is checked through the driver's [`WindowWork`] counters (phase 5 exit:
//! paging performs no full rebuild) and the resident event count (W7a ceiling). Those are engine
//! facts a frame cannot show, so the counter checks need a driver that reports them.
//!
//! Page latency is scripted in wheel ticks: a pending request is answered once it has waited
//! [`LATENCY_TICKS`] input steps, shorter than scrolling the prefetch distance (X12).

use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::time::Duration;

use bmux_tui_components::scrollbar::ScrollbarPolicy;

use super::Scenario;
use super::driver::{
    HistoryRequest, HistoryRequestKind, HistoryResponse, PendingHistoryRequest, RequestId,
    ScenarioInput, ScenarioIntent, ScenarioScrollbar, ScenarioSetting, TranscriptScenarioDriver,
    WheelDirection, WindowWork,
};
use super::frame::{FocusObservation, ScenarioFrame};
use super::session::{Marker, ScenarioSession};

const WIDTH: u16 = 80;
const HEIGHT: u16 = 20;
/// Long enough for any motion (scroll animation, reveal) to settle.
const SETTLE: Duration = Duration::from_secs(2);
/// Events of one bounded window (`Head`, `Tail`, `Around`) and of one page (W6, W8, W10).
const WINDOW_EVENTS: usize = 512;
/// The resident ceiling: max + one page (W7a).
const CEILING_EVENTS: u64 = 1_536;
/// Turns of the long session.
const LONG_TURNS: usize = 850;
/// Item-less events inside every turn of the long session (runtime work, client attachments).
const SILENT_PER_TURN: usize = 4;
/// Input steps a page waits before it is answered. Scrolling the prefetch distance
/// (`max(2 × viewport, 48)` rows, W5) takes at least 16 wheel ticks of 3 rows, so pages arrive
/// in time (X12's latency condition).
const LATENCY_TICKS: usize = 8;
/// Upper bound on input steps for one pass through the long session.
const MAX_STEPS: usize = 20_000;

/// A session of more than 5,000 canonical events: [`LONG_TURNS`] turns of a one-line user
/// message, [`SILENT_PER_TURN`] item-less events and a one-line assistant answer.
fn long_session() -> ScenarioSession {
    let mut session = ScenarioSession::new();
    for _ in 0..LONG_TURNS {
        session.push_user(1);
        for _ in 0..SILENT_PER_TURN {
            session.push_silent();
        }
        session.push_assistant(1);
    }
    assert!(session.events().len() >= 5_000, "X12 needs 5,000 events");
    session
}

/// The newest item marker of the canonical session.
fn newest_marker<D: TranscriptScenarioDriver>(scenario: &Scenario<D>) -> Marker {
    let session = scenario.session();
    session
        .events()
        .iter()
        .rev()
        .find_map(|event| session.last_marker(event.sequence))
        .expect("the session has an item")
}

/// The window work counters; scenarios that check paging work need them.
fn work<D: TranscriptScenarioDriver>(scenario: &Scenario<D>) -> WindowWork {
    scenario
        .window_work()
        .unwrap_or_else(|| scenario.fail("the driver reports no window work counters"))
}

/// Fail unless the resident window holds at most the ceiling (W7a).
#[track_caller]
fn check_ceiling<D: TranscriptScenarioDriver>(scenario: &Scenario<D>, what: &str) {
    let count = scenario
        .resident_event_count()
        .unwrap_or_else(|| scenario.fail("the driver reports no resident event count"));
    scenario.check(count <= CEILING_EVENTS, || {
        format!("{what}: {count} resident events exceed the {CEILING_EVENTS}-event ceiling")
    });
}

/// Fail unless `after` moved `before`'s content by a row count in `expected`, with markers in
/// common (a frame without common markers is a leap or a blank frame).
#[track_caller]
fn check_shift<D: TranscriptScenarioDriver>(
    scenario: &Scenario<D>,
    before: &ScenarioFrame,
    after: &ScenarioFrame,
    expected: &RangeInclusive<i32>,
    what: &str,
) {
    let shift = after.shift_from(before);
    scenario.check(shift.is_some_and(|shift| expected.contains(&shift)), || {
        format!(
            "{what}: content shift {shift:?} outside {expected:?}\n--- reference frame ---\n{}",
            before.text()
        )
    });
}

/// Fail unless every marker common to both frames moved by the same row count: no unit between
/// them changed height in place (BUG-B5), and the frame kept at least `min_common` markers.
#[track_caller]
fn check_uniform_shift<D: TranscriptScenarioDriver>(
    scenario: &Scenario<D>,
    before: &ScenarioFrame,
    after: &ScenarioFrame,
    min_common: usize,
    what: &str,
) {
    let shifts = after.shifts_from(before);
    let first = shifts.values().next().copied();
    scenario.check(
        shifts.len() >= min_common && shifts.values().all(|shift| Some(*shift) == first),
        || {
            format!(
                "{what}: content changed height in place (shifts {shifts:?})\n--- reference frame ---\n{}",
                before.text()
            )
        },
    );
}

fn settle<D: TranscriptScenarioDriver>(scenario: &mut Scenario<D>) -> ScenarioFrame {
    scenario.present();
    scenario.advance(SETTLE);
    scenario.present();
    scenario.advance(SETTLE);
    scenario.present()
}

/// Scripted page latency: answers each pending request once it has waited `latency` steps.
#[derive(Default)]
struct Latency {
    issued_at: BTreeMap<RequestId, usize>,
    /// Events delivered in page results (each is reduced at most once, SD1).
    page_events: u64,
    /// Requests answered, by kind.
    answered: BTreeMap<HistoryRequestKind, u64>,
}

impl Latency {
    /// Answer every pending request that has waited `latency` steps by `step`.
    fn answer_due<D: TranscriptScenarioDriver>(
        &mut self,
        scenario: &mut Scenario<D>,
        step: usize,
        latency: usize,
    ) {
        for pending in scenario.pending() {
            let since = *self.issued_at.entry(pending.id).or_insert(step);
            if step - since >= latency {
                let response = scenario.complete(&pending);
                *self.answered.entry(pending.request.kind()).or_default() += 1;
                if matches!(
                    pending.request.kind(),
                    HistoryRequestKind::Older | HistoryRequestKind::Newer
                ) {
                    self.page_events += u64::try_from(response.events.len()).unwrap_or(u64::MAX);
                }
                check_ceiling(scenario, &format!("step {step}: after a page result"));
            }
        }
    }

    fn answered(&self, kind: HistoryRequestKind) -> u64 {
        self.answered.get(&kind).copied().unwrap_or(0)
    }
}

/// Fail unless paging since `before` replaced the window `windows` times and otherwise applied
/// every page incrementally: no SD2 fallback, and no event reduced more than once per page
/// result (no full rebuild, phase 5 exit).
#[track_caller]
fn check_incremental<D: TranscriptScenarioDriver>(
    scenario: &Scenario<D>,
    before: WindowWork,
    windows: u64,
    page_events: u64,
    what: &str,
) -> WindowWork {
    let after = work(scenario);
    let window_events = windows * u64::try_from(WINDOW_EVENTS).unwrap_or(u64::MAX);
    scenario.check(
        after.replacements - before.replacements == windows
            && after.fallbacks == before.fallbacks
            && after.events_reduced - before.events_reduced <= page_events + window_events,
        || {
            format!(
                "{what}: paging was not incremental: {before:?} -> {after:?} \
                 ({page_events} page events, {windows} windows)"
            )
        },
    );
    after
}

/// Fail if any pending request is of `kind`.
#[track_caller]
fn check_no_request<D: TranscriptScenarioDriver>(
    scenario: &Scenario<D>,
    kinds: &[HistoryRequestKind],
    what: &str,
) {
    let found = scenario
        .pending()
        .into_iter()
        .filter(|pending| kinds.contains(&pending.request.kind()))
        .collect::<Vec<_>>();
    scenario.check(found.is_empty(), || format!("{what}: unexpected {found:?}"));
}

/// Jump to the latest content of a live window: a motion, never a history request (T13).
fn jump_to_latest_without_requests<D: TranscriptScenarioDriver>(
    scenario: &mut Scenario<D>,
    what: &str,
) -> ScenarioFrame {
    scenario.navigate(ScenarioIntent::JumpToLatest);
    check_no_request(
        scenario,
        &[
            HistoryRequestKind::Tail,
            HistoryRequestKind::Head,
            HistoryRequestKind::Around,
            HistoryRequestKind::LiveState,
        ],
        &format!("{what}: jump-to-latest on a live window"),
    );
    let latest = settle(scenario);
    let newest = newest_marker(scenario);
    scenario.check(
        latest.following_bottom && latest.bottom_marker().map(|at| at.marker) == Some(newest),
        || format!("{what}: jump-to-latest did not follow the latest bottom"),
    );
    latest
}

/// Deliver one live assistant item and fail unless the following view shows it.
fn live_item_is_shown<D: TranscriptScenarioDriver>(scenario: &mut Scenario<D>, what: &str) {
    let sequence = scenario.append_live_assistant(1);
    let frame = settle(scenario);
    let last = scenario.session().last_marker(sequence).expect("item");
    scenario.check(frame.shows(last) && frame.following_bottom, || {
        format!("{what}: the live item is not shown")
    });
}

// ---------------------------------------------------------------------------------------------
// X12: deep history paging in both directions
// ---------------------------------------------------------------------------------------------

/// X12, W5–W7a, W9, BUG-A2: steady wheel scrolling up through more than 5,000 events, with page
/// latency shorter than scrolling the prefetch distance, moves every frame by one tick: no stall
/// frame at a loaded boundary, no leap and no blank frame, while the resident window stays
/// under the ceiling and paging never rebuilds the window. Evicting the tail detaches the window
/// (W9), and jump-to-latest then returns to live with one tail request (X11).
pub fn x12_wheel_up_through_long_history_is_continuous<D: TranscriptScenarioDriver>() {
    let mut scenario = Scenario::<D>::attach_latest(long_session(), WINDOW_EVENTS, WIDTH, HEIGHT);
    let tick = scenario.wheel_rows();
    let attached = work(&scenario);
    let mut previous = scenario.present();
    let mut latency = Latency::default();
    let start = Marker::new(1, 0);
    let mut reached = false;
    for step in 0..MAX_STEPS {
        scenario.wheel(WheelDirection::Up, 1);
        latency.answer_due(&mut scenario, step, LATENCY_TICKS);
        let frame = scenario.present();
        check_ceiling(&scenario, &format!("step {step}"));
        if frame.shows(start) {
            reached = true;
            break;
        }
        check_shift(
            &scenario,
            &previous,
            &frame,
            &(1..=tick),
            &format!("continuous wheel up, step {step}"),
        );
        previous = frame;
    }
    scenario.check(reached, || {
        "continuous scrolling never reached the session start".to_owned()
    });
    let paged = check_incremental(&scenario, attached, 0, latency.page_events, "wheel up");
    scenario.check(
        paged.prepends > 0
            && paged.evictions > 0
            && latency.answered(HistoryRequestKind::Newer) == 0,
        || format!("expected incremental prepends and evictions only: {paged:?}"),
    );
    // W9: the tail was evicted, so the window is detached and live events are not applied.
    let live = scenario.append_live_assistant(1);
    let reading = settle(&mut scenario);
    scenario.check(!reading.shows(Marker::new(live, 0)), || {
        "a live event appeared in a detached window".to_owned()
    });
    // X11: one tail request returns to live.
    scenario.navigate(ScenarioIntent::JumpToLatest);
    let requests = scenario.pending();
    scenario.check(
        requests.len() == 1 && requests[0].request.kind() == HistoryRequestKind::Tail,
        || format!("jump-to-latest must issue exactly one tail-window request, got {requests:?}"),
    );
    scenario.complete(&requests[0]);
    let latest = settle(&mut scenario);
    let newest = newest_marker(&scenario);
    scenario.check(
        latest.following_bottom && latest.bottom_marker().map(|at| at.marker) == Some(newest),
        || "jump-to-latest did not return to the live tail".to_owned(),
    );
    live_item_is_shown(&mut scenario, "after returning to latest");
}

/// X12, W9, T23, multi-event frames: bursts of three wheel ticks per frame, with live events
/// arriving while scrolling up and pages answered in the same update as the next ticks, still
/// move every frame by at most the burst: live content never moves the reader (T23), and once the
/// tail is evicted the window detaches (W9) without moving the reader either.
pub fn x12_wheel_bursts_with_live_events_stay_continuous<D: TranscriptScenarioDriver>() {
    const TICKS_PER_FRAME: usize = 3;
    const FRAMES: usize = 600;
    let mut scenario = Scenario::<D>::attach_latest(long_session(), WINDOW_EVENTS, WIDTH, HEIGHT);
    let tick = scenario.wheel_rows();
    let attached = work(&scenario);
    let mut previous = scenario.present();
    let mut latency = Latency::default();
    let burst = tick * i32::try_from(TICKS_PER_FRAME).unwrap_or(i32::MAX);
    let mut live = Vec::new();
    for step in 0..FRAMES {
        scenario.wheel(WheelDirection::Up, TICKS_PER_FRAME);
        if step % 10 == 5 {
            live.push(scenario.append_live_assistant(1));
        }
        latency.answer_due(&mut scenario, step, 2);
        let frame = scenario.present();
        check_ceiling(&scenario, &format!("step {step}"));
        if frame.shows(Marker::new(1, 0)) {
            break;
        }
        check_shift(
            &scenario,
            &previous,
            &frame,
            &(1..=burst),
            &format!("wheel burst with live events, step {step}"),
        );
        previous = frame;
    }
    // Live events applied while the window was live are reduced once each too.
    let live_events = u64::try_from(live.len()).unwrap_or(u64::MAX);
    let paged = check_incremental(
        &scenario,
        attached,
        0,
        latency.page_events + live_events,
        "wheel bursts",
    );
    scenario.check(paged.evictions > 0, || {
        format!("the bursts never evicted: {paged:?}")
    });
    scenario.check(
        scenario
            .resident()
            .is_some_and(|(_, last)| live.last().is_some_and(|newest| last < *newest)),
        || "the tail was never evicted while scrolling up".to_owned(),
    );
}

/// X12, W10, W11, multi-event frames: from the session start (one direct `Head` window), steady
/// wheel scrolling down two ticks per frame through more than 5,000 events moves every frame,
/// keeps the window under the ceiling and appends pages incrementally. The newer page that
/// reaches the tail returns the window to live (W11) without a tail request, so jump-to-latest
/// is a motion and live events apply.
pub fn x12_wheel_down_from_session_start_returns_live<D: TranscriptScenarioDriver>() {
    const TICKS_PER_FRAME: usize = 2;
    let mut scenario = Scenario::<D>::attach_latest(long_session(), WINDOW_EVENTS, WIDTH, HEIGHT);
    let tick = scenario.wheel_rows();
    settle(&mut scenario);
    let attached = work(&scenario);
    scenario.key("ctrl+home");
    let requests = scenario.pending();
    scenario.check(
        requests.len() == 1 && requests[0].request.kind() == HistoryRequestKind::Head,
        || format!("ctrl+home must issue exactly one head-window request, got {requests:?}"),
    );
    scenario.complete(&requests[0]);
    let mut previous = settle(&mut scenario);
    scenario.check(
        previous.top_marker().map(|at| at.marker) == Some(Marker::new(1, 0)),
        || "the head window does not show the session start".to_owned(),
    );
    let mut latency = Latency::default();
    let newest = newest_marker(&scenario);
    let mut reached = false;
    let ticks = i32::try_from(TICKS_PER_FRAME).unwrap_or(i32::MAX);
    for step in 0..MAX_STEPS {
        scenario.wheel(WheelDirection::Down, TICKS_PER_FRAME);
        latency.answer_due(&mut scenario, step, LATENCY_TICKS / TICKS_PER_FRAME);
        let frame = scenario.present();
        check_ceiling(&scenario, &format!("step {step}"));
        check_no_request(
            &scenario,
            &[HistoryRequestKind::Tail, HistoryRequestKind::Around],
            &format!("step {step}: paging down"),
        );
        check_shift(
            &scenario,
            &previous,
            &frame,
            &(-(tick * ticks)..=-1),
            &format!("continuous wheel down, step {step}"),
        );
        if frame.shows(newest) {
            reached = true;
            break;
        }
        previous = frame;
    }
    scenario.check(reached, || {
        "continuous scrolling never reached the session tail".to_owned()
    });
    let paged = check_incremental(&scenario, attached, 1, latency.page_events, "wheel down");
    scenario.check(
        paged.appends > 0
            && paged.evictions > 0
            && latency.answered(HistoryRequestKind::Older) == 0,
        || format!("expected incremental appends and evictions only: {paged:?}"),
    );
    jump_to_latest_without_requests(&mut scenario, "after paging to the tail");
    live_item_is_shown(&mut scenario, "after paging to the tail");
}

/// Phase 5 exit, W7, W7a: paging up and back down through the long session with Page Up/Down,
/// every request answered before the next frame (several results per frame), keeps the resident
/// window under the ceiling after every result and performs no full rebuild: no window
/// replacement after attach, no fallback, and no event reduced more than once per result.
pub fn phase5_paging_performs_no_full_rebuild<D: TranscriptScenarioDriver>() {
    let mut scenario = Scenario::<D>::attach_latest(long_session(), WINDOW_EVENTS, WIDTH, HEIGHT);
    settle(&mut scenario);
    let attached = work(&scenario);
    let mut latency = Latency::default();
    let mut up = 0_usize;
    for step in 0..MAX_STEPS {
        scenario.key("pageup");
        latency.answer_due(&mut scenario, step, 0);
        let frame = scenario.present();
        check_ceiling(&scenario, &format!("page up {step}"));
        up = step;
        if frame.shows(Marker::new(1, 0)) {
            break;
        }
    }
    let paged_up = check_incremental(&scenario, attached, 0, latency.page_events, "page up");
    scenario.check(paged_up.prepends > 0 && paged_up.evictions > 0, || {
        format!("paging up did not prepend and evict: {paged_up:?}")
    });
    let newest = newest_marker(&scenario);
    for step in up..MAX_STEPS {
        scenario.key("pagedown");
        latency.answer_due(&mut scenario, step, 0);
        let frame = scenario.present();
        check_ceiling(&scenario, &format!("page down {step}"));
        if frame.shows(newest) {
            break;
        }
    }
    let paged = check_incremental(&scenario, attached, 0, latency.page_events, "page down");
    scenario.check(
        paged.appends > 0 && paged.evictions > paged_up.evictions,
        || format!("paging down did not append and evict: {paged:?}"),
    );
    jump_to_latest_without_requests(&mut scenario, "after paging back down");
}

/// Turns of the tool-heavy session.
const TOOL_TURNS: usize = 320;

/// A tool-heavy session: every turn is a one-line user message, two tool calls that each
/// resolve an interaction (exchange) and record a result, and a one-line assistant answer.
fn tool_session() -> ScenarioSession {
    use bcode_session_models::{
        SessionEventKind, ToolExchangeRequest, ToolExchangeResolution, ToolExchangeResolutionEvent,
        ToolExchangeResponsePolicy, ToolInvocationResultRecord,
    };
    let mut session = ScenarioSession::new();
    for turn in 0..TOOL_TURNS {
        session.push_user(1);
        for call in 0..2 {
            let invocation_id = format!("call-{turn}-{call}");
            let exchange_id = format!("exchange-{turn}-{call}");
            session.push_kind(SessionEventKind::ToolCallRequested {
                tool_call_id: invocation_id.clone(),
                producer_plugin_id: None,
                tool_name: "read".to_owned(),
                arguments_json: "{}".to_owned(),
                working_directory: None,
            });
            session.push_kind(SessionEventKind::ToolExchangeRequested {
                request: ToolExchangeRequest {
                    invocation_id: invocation_id.clone(),
                    exchange_id: exchange_id.clone(),
                    producer_id: "scenario.producer".to_owned(),
                    schema: "scenario.question/schema".to_owned(),
                    schema_version: 1,
                    payload: serde_json::json!({ "turn": turn }),
                    response_policy: ToolExchangeResponsePolicy::Required,
                },
            });
            session.push_kind(SessionEventKind::ToolExchangeResolved {
                event: ToolExchangeResolutionEvent {
                    invocation_id: invocation_id.clone(),
                    exchange_id,
                    resolution: ToolExchangeResolution::Responded {
                        payload: serde_json::json!({ "answer": turn }),
                    },
                },
            });
            session.push_kind(SessionEventKind::ToolInvocationResultRecorded {
                record: ToolInvocationResultRecord {
                    invocation_id,
                    model_output: "done".to_owned(),
                    is_error: false,
                    presentation: None,
                    result: None,
                    content: Vec::new(),
                },
            });
        }
        session.push_assistant(1);
    }
    session
}

/// Phase 5 exit over tool-heavy content (tool calls, resolved interactions, results): after a
/// direct `Head` window and a jump back to the tail, paging up through the region the head
/// window held stays incremental. The earlier window leaves no tool or interaction state that
/// would make those pages shared keyed state (no SD2 fallback, no full rebuild).
pub fn phase5_tool_heavy_paging_after_a_jump_is_incremental<D: TranscriptScenarioDriver>() {
    let mut scenario = Scenario::<D>::attach_latest(tool_session(), WINDOW_EVENTS, WIDTH, HEIGHT);
    settle(&mut scenario);
    let attached = work(&scenario);
    scenario.key("ctrl+home");
    let head = scenario
        .pending_of(HistoryRequestKind::Head)
        .unwrap_or_else(|| scenario.fail("ctrl+home requested no head window"));
    scenario.complete(&head);
    settle(&mut scenario);
    scenario.navigate(ScenarioIntent::JumpToLatest);
    let tail = scenario
        .pending_of(HistoryRequestKind::Tail)
        .unwrap_or_else(|| scenario.fail("jump-to-latest requested no tail window"));
    scenario.complete(&tail);
    settle(&mut scenario);
    let jumped = work(&scenario);
    scenario.check(jumped.replacements - attached.replacements == 2, || {
        format!("expected the head and tail windows only: {attached:?} -> {jumped:?}")
    });
    let mut latency = Latency::default();
    let mut reached = false;
    for step in 0..MAX_STEPS {
        scenario.key("pageup");
        latency.answer_due(&mut scenario, step, 0);
        let frame = scenario.present();
        check_ceiling(&scenario, &format!("page up {step}"));
        if frame.shows(Marker::new(1, 0)) {
            reached = true;
            break;
        }
    }
    scenario.check(reached, || {
        "paging up never reached the session start".to_owned()
    });
    let paged = check_incremental(&scenario, jumped, 0, latency.page_events, "page up");
    scenario.check(paged.prepends > 0 && paged.evictions > 0, || {
        format!("paging up did not prepend and evict: {paged:?}")
    });
}

// ---------------------------------------------------------------------------------------------
// W5, W8, BUG-B5: prefetch and aligned timeline windows
// ---------------------------------------------------------------------------------------------

/// W5: an older page is requested while rows remain between the viewport top and the window's
/// older boundary (prefetch), and at most one page per direction is in flight.
pub fn w5_prefetch_requests_one_page_before_the_boundary<D: TranscriptScenarioDriver>() {
    let mut scenario = Scenario::<D>::attach_latest(long_session(), WINDOW_EVENTS, WIDTH, HEIGHT);
    scenario.present();
    if scenario
        .wheel_until_request(WheelDirection::Up, HistoryRequestKind::Older, MAX_STEPS)
        .is_none()
    {
        scenario.fail("scrolling up never requested an older page");
    }
    let (first, _) = scenario.resident().expect("resident window");
    let boundary = scenario.last_frame().clone();
    scenario.check(!boundary.shows(Marker::new(first, 0)), || {
        "the older page was requested only once the window start was visible".to_owned()
    });
    let requested = scenario
        .pending_of(HistoryRequestKind::Older)
        .expect("older");
    for _ in 0..4 {
        scenario.wheel(WheelDirection::Up, 1);
        scenario.present();
        let older = scenario
            .pending()
            .into_iter()
            .filter(|pending| pending.request.kind() == HistoryRequestKind::Older)
            .collect::<Vec<_>>();
        scenario.check(older.len() == 1 && older[0].id == requested.id, || {
            format!("more than one older page in flight: {older:?}")
        });
    }
}

/// W8, BUG-B5, BUG-B1: a timeline window is at most one window of events and starts and ends
/// on whole turns; the first older and newer pages evict nothing and never change the height
/// of a unit at the window edge in place.
pub fn bug_b5_timeline_window_is_bounded_and_turn_aligned<D: TranscriptScenarioDriver>() {
    let mut scenario = Scenario::<D>::attach_latest(long_session(), WINDOW_EVENTS, WIDTH, HEIGHT);
    scenario.present();
    let target = scenario
        .session()
        .events()
        .iter()
        .map(|event| event.sequence)
        .filter(|sequence| scenario.session().is_turn_start(*sequence))
        .nth(LONG_TURNS / 2)
        .expect("middle turn");
    scenario.navigate(ScenarioIntent::JumpToSequence(target));
    if scenario.complete_kind(HistoryRequestKind::Around).is_none() {
        scenario.fail("timeline jump to a non-resident sequence requested no window");
    }
    let landed = settle(&mut scenario);
    scenario.check(landed.shows(Marker::new(target, 0)), || {
        "timeline jump target is not visible".to_owned()
    });
    let count = scenario.resident_event_count().unwrap_or(0);
    let (first, last) = scenario.resident().expect("resident window");
    let session = scenario.session();
    scenario.check(
        count <= u64::try_from(WINDOW_EVENTS).unwrap_or(u64::MAX)
            && session.is_turn_start(first)
            && session.is_turn_start(last + 1),
        || format!("timeline window {first}..={last} ({count} events) is not one aligned window"),
    );
    // Older edge: the page lands above the reader at the window start.
    let before = scenario.resident();
    for _ in 0..MAX_STEPS {
        if scenario.pending_of(HistoryRequestKind::Older).is_some() {
            break;
        }
        scenario.key("pageup");
        scenario.present();
    }
    // Answer only the older page; a newer prefetch stays pending.
    let boundary = settle(&mut scenario);
    if scenario.complete_kind(HistoryRequestKind::Older).is_none() {
        scenario.fail("paging up a timeline window requested no older page");
    }
    let after = scenario.present();
    check_uniform_shift(&scenario, &boundary, &after, 1, "first older page");
    let (first_after, last_after) = scenario.resident().expect("resident window");
    scenario.check(
        before.is_some_and(|(first, last)| first_after < first && last_after == last),
        || format!("first older page evicted: {before:?} -> {first_after}..={last_after}"),
    );
    // Newer edge.
    for _ in 0..MAX_STEPS {
        if scenario.pending_of(HistoryRequestKind::Newer).is_some() {
            break;
        }
        scenario.key("pagedown");
        scenario.present();
    }
    let boundary = settle(&mut scenario);
    let before = scenario.resident();
    if scenario.complete_kind(HistoryRequestKind::Newer).is_none() {
        scenario.fail("paging down a timeline window requested no newer page");
    }
    let after = scenario.present();
    check_uniform_shift(&scenario, &boundary, &after, 1, "first newer page");
    let (first_after, last_after) = scenario.resident().expect("resident window");
    scenario.check(
        before.is_some_and(|(first, last)| first_after == first && last_after > last),
        || format!("first newer page evicted: {before:?} -> {first_after}..={last_after}"),
    );
}

// ---------------------------------------------------------------------------------------------
// W11: return to live through newer pages
// ---------------------------------------------------------------------------------------------

/// A newer page whose daemon read reached the session tail, still in flight.
struct TailPageInFlight {
    pending: PendingHistoryRequest,
    /// The page as the daemon read it (`has_newer == false`), before any later live event.
    response: HistoryResponse,
    /// The committed frame before the page is answered.
    boundary: ScenarioFrame,
}

/// Jump into the newer part of a 2,000-event session and page down until the newer request
/// that reaches the session tail is pending, and read that page from the daemon.
fn newer_page_reaching_tail_pending<D: TranscriptScenarioDriver>() -> (Scenario<D>, TailPageInFlight)
{
    let session = ScenarioSession::with_turns(1_000, 1);
    let mut scenario = Scenario::<D>::attach_latest(session, 20, WIDTH, HEIGHT);
    scenario.present();
    scenario.navigate(ScenarioIntent::JumpToSequence(1_501));
    if scenario.complete_kind(HistoryRequestKind::Around).is_none() {
        scenario.fail("timeline jump requested no window");
    }
    settle(&mut scenario);
    for _ in 0..MAX_STEPS {
        if let Some(pending) = scenario.pending_of(HistoryRequestKind::Newer) {
            let response = scenario.session().answer(&pending.request);
            if !response.has_newer {
                let boundary = scenario.present();
                return (
                    scenario,
                    TailPageInFlight {
                        pending,
                        response,
                        boundary,
                    },
                );
            }
            scenario.complete(&pending);
        }
        scenario.key("pagedown");
        scenario.present();
    }
    scenario.fail("paging down never requested the page that reaches the tail");
}

/// W11, C23, multi-event frames: live events arriving while the newer page that reaches the tail
/// is in flight are buffered; the page, the buffered live events and a wheel tick are all
/// incorporated before the next frame, which moves only by the tick (plus kept page movement).
/// The window is live again without any tail request: jump-to-latest is a motion and shows the
/// buffered live items.
pub fn w11_newer_page_reaching_tail_applies_buffered_live_events<D: TranscriptScenarioDriver>() {
    let (mut scenario, page) = newer_page_reaching_tail_pending::<D>();
    let tick = scenario.wheel_rows();
    let boundary = page.boundary.clone();
    scenario.append_live_assistant(1);
    scenario.append_live_assistant(1);
    let work_before = work(&scenario);
    scenario.complete_with(&page.pending, page.response);
    scenario.wheel(WheelDirection::Down, 1);
    let after = scenario.present();
    check_shift(
        &scenario,
        &boundary,
        &after,
        &(-(tick + 10)..=-1),
        "page reaching the tail, live events and a wheel tick before one frame",
    );
    check_no_request(
        &scenario,
        &[HistoryRequestKind::Tail, HistoryRequestKind::LiveState],
        "return to live from buffered events",
    );
    check_incremental(
        &scenario,
        work_before,
        0,
        u64::try_from(WINDOW_EVENTS).unwrap_or(u64::MAX),
        "page reaching the tail",
    );
    jump_to_latest_without_requests(&mut scenario, "after the buffered return to live");
    live_item_is_shown(&mut scenario, "after the buffered return to live");
}

/// W11, W7a: when more live events arrive than the buffer holds while the newer page that
/// reaches the tail is in flight, the engine rehydrates live state with one `LiveState` request
/// instead of replacing the window. More events arrived than one bounded read returns, so the
/// read is incorporated like a newer page and newer paging continues until the tail; the
/// window then follows live content, never needing a tail window and staying under the ceiling.
pub fn w11_live_buffer_overflow_rehydrates_live_state<D: TranscriptScenarioDriver>() {
    const LIVE_EVENTS: usize = 600;
    let (mut scenario, page) = newer_page_reaching_tail_pending::<D>();
    for _ in 0..LIVE_EVENTS {
        scenario.append_live_assistant(1);
    }
    let attached = work(&scenario);
    let tail_page_events = u64::try_from(page.response.events.len()).unwrap_or(u64::MAX);
    scenario.complete_with(&page.pending, page.response);
    scenario.present();
    let Some(live_state) = scenario.pending_of(HistoryRequestKind::LiveState) else {
        scenario.fail("an overflowed live buffer issued no live-state request");
    };
    check_no_request(
        &scenario,
        &[HistoryRequestKind::Tail, HistoryRequestKind::Around],
        "buffer overflow",
    );
    let response = scenario.complete(&live_state);
    let mut page_events =
        tail_page_events + u64::try_from(response.events.len()).unwrap_or(u64::MAX);
    check_ceiling(&scenario, "live state incorporated");
    let newest = newest_marker(&scenario);
    let mut reached = false;
    for step in 0..MAX_STEPS {
        check_no_request(
            &scenario,
            &[HistoryRequestKind::Tail, HistoryRequestKind::Around],
            &format!("step {step}: paging to the tail after the live-state read"),
        );
        for pending in scenario.pending() {
            let response = scenario.complete(&pending);
            page_events += u64::try_from(response.events.len()).unwrap_or(u64::MAX);
            check_ceiling(&scenario, &format!("step {step}: after a result"));
        }
        let frame = scenario.present();
        if frame.shows(newest) {
            reached = true;
            break;
        }
        scenario.key("pagedown");
    }
    scenario.check(reached, || {
        "paging down after the live-state read never reached the tail".to_owned()
    });
    check_incremental(
        &scenario,
        attached,
        0,
        page_events,
        "live-state rehydration",
    );
    jump_to_latest_without_requests(&mut scenario, "after the live-state rehydration");
    live_item_is_shown(&mut scenario, "after the live-state rehydration");
}

// ---------------------------------------------------------------------------------------------
// BUG-B3 through End in transcript focus (host keymap)
// ---------------------------------------------------------------------------------------------

/// BUG-B3, X5, X11, FO4: after a deep timeline jump (tail not resident), `End` in transcript
/// focus shows live content with exactly one tail-window request, follows the bottom, and live
/// events then apply.
pub fn bug_b3_end_in_transcript_focus_after_deep_jump_returns_live<D: TranscriptScenarioDriver>() {
    let session = ScenarioSession::with_turns(1_000, 1);
    let mut scenario = Scenario::<D>::attach_latest(session, 20, WIDTH, HEIGHT);
    scenario.present();
    scenario.navigate(ScenarioIntent::JumpToSequence(500));
    scenario.complete_kind(HistoryRequestKind::Around);
    settle(&mut scenario);
    scenario.key("alt+u");
    let focused = settle(&mut scenario);
    scenario.check(
        matches!(focused.focus, FocusObservation::Transcript { .. }),
        || "alt+u did not focus the transcript".to_owned(),
    );
    scenario.key("end");
    let requests = scenario.pending();
    scenario.check(
        requests.len() == 1 && requests[0].request.kind() == HistoryRequestKind::Tail,
        || format!("End must issue exactly one tail-window request, got {requests:?}"),
    );
    scenario.complete(&requests[0]);
    let latest = settle(&mut scenario);
    check_no_request(
        &scenario,
        &[
            HistoryRequestKind::Newer,
            HistoryRequestKind::Tail,
            HistoryRequestKind::Around,
            HistoryRequestKind::Head,
        ],
        "returning to latest with End",
    );
    let newest = newest_marker(&scenario);
    scenario.check(
        latest.following_bottom && latest.bottom_marker().map(|at| at.marker) == Some(newest),
        || "End did not return to the live tail".to_owned(),
    );
    let sequence = scenario.append_live_assistant(1);
    let live = settle(&mut scenario);
    let last = scenario.session().last_marker(sequence).expect("item");
    scenario.check(live.shows(last), || {
        "live event after End is not shown".to_owned()
    });
}

// ---------------------------------------------------------------------------------------------
// X6: optional scrollbar within the whole session
// ---------------------------------------------------------------------------------------------

/// Thumb rows of `frame`'s scrollbar, failing when it shows none.
fn thumb<D: TranscriptScenarioDriver>(scenario: &Scenario<D>, frame: &ScenarioFrame) -> (u16, u16) {
    let rows = frame.scrollbar_thumb_rows(ScrollbarPolicy::vertical().thumb);
    match (rows.first(), rows.last()) {
        (Some(first), Some(last)) => (*first, *last),
        _ => scenario.fail("the scrollbar paints no thumb"),
    }
}

/// X6, W12, T18: the scrollbar is off by default; enabled, its thumb is placed within the whole
/// session (bottom at the tail window, top at the session start) rather than within the
/// resident rows, and a press on the track loads one `Around` window near that fraction.
pub fn x6_scrollbar_thumb_is_placed_within_the_whole_session<D: TranscriptScenarioDriver>() {
    let mut scenario = Scenario::<D>::attach_latest(long_session(), WINDOW_EVENTS, WIDTH, HEIGHT);
    let off = settle(&mut scenario);
    scenario.check(off.scrollbar.is_none(), || {
        "the scrollbar is shown by default".to_owned()
    });
    scenario.configure(&ScenarioSetting::TranscriptScrollbar(
        ScenarioScrollbar::Overlay,
    ));
    let tail = settle(&mut scenario);
    let Some(track) = tail.scrollbar else {
        scenario.fail("the enabled scrollbar is not presented");
    };
    let (top, bottom) = thumb(&scenario, &tail);
    scenario.check(
        bottom + 1 == track.height && bottom - top < track.height / 4,
        || {
            format!(
                "tail-window thumb rows {top}..={bottom} of {} are not a small thumb at the bottom",
                track.height
            )
        },
    );
    scenario.key("ctrl+home");
    if scenario.complete_kind(HistoryRequestKind::Head).is_none() {
        scenario.fail("ctrl+home requested no head window");
    }
    let head = settle(&mut scenario);
    let (top, _) = thumb(&scenario, &head);
    scenario.check(top == 0, || {
        format!("session-start thumb starts at row {top}, not the track top")
    });
    // A press in the middle of the track jumps to about the middle of the session.
    scenario.input(ScenarioInput::Click {
        at: super::driver::CellPoint {
            x: track.x,
            y: track.y + track.height / 2,
        },
    });
    let Some(around) = scenario.pending_of(HistoryRequestKind::Around) else {
        scenario.fail("a track press far from the resident rows requested no window");
    };
    let HistoryRequest::Around { sequence, .. } = around.request else {
        unreachable!("filtered by kind");
    };
    let total = scenario.session().last_sequence().unwrap_or(1);
    scenario.check((total / 4..=total * 3 / 4).contains(&sequence), || {
        format!("a press at the track middle requested sequence {sequence} of {total}")
    });
    scenario.complete(&around);
    let middle = settle(&mut scenario);
    let (top, bottom) = thumb(&scenario, &middle);
    scenario.check(
        top >= track.height / 4 && bottom < track.height * 3 / 4 + 1,
        || format!("thumb rows {top}..={bottom} after the middle jump are not in the middle"),
    );
    let shown = middle.top_marker().map_or(0, |at| at.marker.sequence);
    scenario.check(shown.abs_diff(sequence) <= 512, || {
        format!("the middle jump shows sequence {shown}, far from {sequence}")
    });
    // A gutter scrollbar keeps the reading position; turning the scrollbar off removes it.
    scenario.configure(&ScenarioSetting::TranscriptScrollbar(
        ScenarioScrollbar::Gutter,
    ));
    let gutter = settle(&mut scenario);
    scenario.check(gutter.scrollbar.is_some(), || {
        "the gutter scrollbar is not presented".to_owned()
    });
    check_uniform_shift(
        &scenario,
        &middle,
        &gutter,
        1,
        "switching to a gutter scrollbar",
    );
    scenario.configure(&ScenarioSetting::TranscriptScrollbar(
        ScenarioScrollbar::Off,
    ));
    let off = settle(&mut scenario);
    scenario.check(off.scrollbar.is_none(), || {
        "the scrollbar is still shown after turning it off".to_owned()
    });
}
