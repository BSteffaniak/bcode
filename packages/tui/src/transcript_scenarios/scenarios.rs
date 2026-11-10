//! Scenario definitions, generic over the engine driver.
//!
//! Every scenario asserts the behavior the V2 specification requires
//! (`docs/tui-v2-architecture.md`), not what V1 happens to do. Scenario names start with the
//! requirement or bug they cover; `docs/tui-v2-scenarios.md` maps each one to its IDs and records
//! per-engine status.
//!
//! Row shifts are measured with [`ScenarioFrame::shift_from`]: positive means content moved
//! down (the reader moved toward older content), negative means content moved up.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::time::Duration;

use super::Scenario;
use super::driver::{
    HistoryRequestKind, RequestId, ScenarioInput, ScenarioIntent, TranscriptScenarioDriver,
    WheelDirection,
};
use super::frame::ScenarioFrame;
use super::session::{Marker, ScenarioSession};

const WIDTH: u16 = 80;
/// Resident window event cap shared by both engines (D1, W7).
const RESIDENT_MAX_EVENTS: u64 = 1_024;
const HEIGHT: u16 = 20;
/// Long enough for any motion (scroll animation, reveal) to settle.
const SETTLE: Duration = Duration::from_secs(2);

/// Fail unless `after` moved `before`'s top-most surviving marker by a row count in `expected`.
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
            "{what}: content shift {shift:?} outside {expected:?} (shifts {:?})\n--- reference frame ---\n{}",
            after.shifts_from(before),
            before.text()
        )
    });
}

/// Deliver `key` and present until a request of `kind` is pending.
fn key_until_request<D: TranscriptScenarioDriver>(
    scenario: &mut Scenario<D>,
    key: &str,
    kind: HistoryRequestKind,
    max_presses: usize,
) -> usize {
    for press in 1..=max_presses {
        scenario.key(key);
        scenario.present();
        if scenario.pending_of(kind).is_some() {
            return press;
        }
    }
    scenario.fail(&format!(
        "no {kind:?} request after {max_presses} presses of {key}"
    ));
}

/// Reach the older boundary of a 12-item latest window by wheel, leaving the request pending.
fn at_older_boundary<D: TranscriptScenarioDriver>(
    session: ScenarioSession,
) -> (Scenario<D>, ScenarioFrame) {
    let mut scenario = Scenario::<D>::attach_latest(session, 12, WIDTH, HEIGHT);
    scenario.present();
    if scenario
        .wheel_until_request(WheelDirection::Up, HistoryRequestKind::Older, 200)
        .is_none()
    {
        scenario.fail("scrolling up never requested an older page");
    }
    let boundary = scenario.last_frame().clone();
    (scenario, boundary)
}

/// Timeline-jump into a long session, then page down to the window's newer boundary.
fn timeline_window_at_newer_boundary<D: TranscriptScenarioDriver>() -> (Scenario<D>, ScenarioFrame)
{
    let session = ScenarioSession::with_turns(1_000, 1);
    let mut scenario = Scenario::<D>::attach_latest(session, 20, WIDTH, HEIGHT);
    scenario.present();
    scenario.navigate(ScenarioIntent::JumpToSequence(1_000));
    if scenario.complete_kind(HistoryRequestKind::Around).is_none() {
        scenario.fail("timeline jump to a non-resident sequence requested no window");
    }
    let landed = scenario.present();
    scenario.check(landed.shows(Marker::new(1_000, 0)), || {
        "timeline jump target is not visible".to_owned()
    });
    key_until_request(&mut scenario, "pagedown", HistoryRequestKind::Newer, 1_000);
    let boundary = scenario.last_frame().clone();
    (scenario, boundary)
}

// ---------------------------------------------------------------------------------------------
// Baselines (engine-neutral sanity; expected to pass on every engine)
// ---------------------------------------------------------------------------------------------

/// T1/C10: live content while following stays at the latest bottom.
pub fn t1_live_append_while_following_shows_latest<D: TranscriptScenarioDriver>() {
    let session = ScenarioSession::with_turns(15, 3);
    let mut scenario = Scenario::<D>::attach_latest(session, 30, WIDTH, HEIGHT);
    let first = scenario.present();
    scenario.check(first.following_bottom, || {
        "attach does not follow".to_owned()
    });
    let sequence = scenario.append_live_assistant(3);
    scenario.advance(SETTLE);
    scenario.present();
    let frame = scenario.present();
    let last = scenario.session().last_marker(sequence).expect("item");
    scenario.check(frame.shows(last) && frame.following_bottom, || {
        "live item's last line is not visible while following".to_owned()
    });
}

/// T23/C16: live content while reading never moves the reader.
pub fn t23_live_append_while_reading_keeps_position<D: TranscriptScenarioDriver>() {
    let session = ScenarioSession::with_turns(15, 3);
    let mut scenario = Scenario::<D>::attach_latest(session, 30, WIDTH, HEIGHT);
    scenario.present();
    scenario.wheel(WheelDirection::Up, 6);
    scenario.present();
    scenario.advance(SETTLE);
    let reading = scenario.present();
    scenario.check(!reading.following_bottom, || {
        "wheel up did not detach".to_owned()
    });
    scenario.append_live_assistant(3);
    scenario.append_live_assistant(3);
    let after = scenario.present();
    check_shift(
        &scenario,
        &reading,
        &after,
        &(0..=0),
        "live append while reading",
    );
}

/// C29: an older page incorporated before the next frame preserves the reader.
pub fn c29_older_page_then_frame_preserves_reader<D: TranscriptScenarioDriver>() {
    let (mut scenario, boundary) = at_older_boundary::<D>(ScenarioSession::with_turns(30, 3));
    scenario.complete_kind(HistoryRequestKind::Older);
    let after = scenario.present();
    // Only movement left over from the boundary tick may be spent against the page (C28).
    let tick = scenario.wheel_rows();
    check_shift(
        &scenario,
        &boundary,
        &after,
        &(0..=tick),
        "older page incorporation",
    );
}

// ---------------------------------------------------------------------------------------------
// Bug A: scrolling up into history jumps
// ---------------------------------------------------------------------------------------------

/// BUG-A1: an older page arrives and a wheel tick is processed before the next frame. The
/// committed frame shows the same content shifted only by the tick (plus leftover boundary
/// movement), never by the page height.
pub fn bug_a1_wheel_between_older_page_and_frame<D: TranscriptScenarioDriver>() {
    let (mut scenario, boundary) = at_older_boundary::<D>(ScenarioSession::with_turns(30, 3));
    scenario.complete_kind(HistoryRequestKind::Older);
    scenario.wheel(WheelDirection::Up, 1);
    let after = scenario.present();
    let tick = scenario.wheel_rows();
    check_shift(
        &scenario,
        &boundary,
        &after,
        &(tick..=tick * 2),
        "wheel tick between page arrival and frame",
    );
}

/// BUG-A2: continuous upward wheel input across many pages (with page latency shorter than
/// scrolling the prefetch distance) moves every frame by at least one row and at most the tick:
/// no stall frames while a page loads and no leap when it lands.
pub fn bug_a2_continuous_wheel_moves_without_stall_or_leap<D: TranscriptScenarioDriver>() {
    const LATENCY_TICKS: usize = 2;
    let mut session = ScenarioSession::new();
    for turn in 0..120 {
        // Runs of item-less events produce pages that contribute no rows.
        if turn % 20 == 0 {
            for _ in 0..6 {
                session.push_silent();
            }
        }
        session.push_user(1);
        session.push_assistant(3);
    }
    let first_item = session
        .events()
        .iter()
        .find_map(|event| session.item_lines(event.sequence).map(|_| event.sequence))
        .expect("item");
    let mut scenario = Scenario::<D>::attach_latest(session, 12, WIDTH, HEIGHT);
    let tick = scenario.wheel_rows();
    let mut previous = scenario.present();
    let mut requested_at = BTreeMap::<RequestId, usize>::new();
    for step in 0..1_200 {
        scenario.wheel(WheelDirection::Up, 1);
        for pending in scenario.pending() {
            let since = *requested_at.entry(pending.id).or_insert(step);
            if step - since >= LATENCY_TICKS {
                scenario.complete(&pending);
            }
        }
        let frame = scenario.present();
        if frame.shows(Marker::new(first_item, 0)) {
            return;
        }
        check_shift(
            &scenario,
            &previous,
            &frame,
            &(1..=tick),
            &format!("continuous wheel step {step}"),
        );
        previous = frame;
    }
    scenario.fail("continuous scrolling never reached the session start");
}

/// BUG-A3: incorporating an older page into a window at its event cap evicts only content away
/// from the reader; every marker visible before the page stays visible at a consistent shift.
pub fn bug_a3_eviction_after_prepend_keeps_visible_content<D: TranscriptScenarioDriver>() {
    let session = ScenarioSession::with_turns(700, 1);
    let mut scenario = Scenario::<D>::attach_latest(session, 1_020, WIDTH, HEIGHT);
    scenario.present();
    let (oldest, _) = scenario.resident().expect("resident window");
    scenario.navigate(ScenarioIntent::JumpToSequence(oldest));
    scenario.present();
    scenario.advance(SETTLE);
    scenario.present();
    if scenario
        .wheel_until_request(WheelDirection::Up, HistoryRequestKind::Older, 50)
        .is_none()
    {
        scenario.fail("no older request at the top of a capped window");
    }
    let boundary = scenario.last_frame().clone();
    scenario.complete_kind(HistoryRequestKind::Older);
    let after = scenario.present();
    let tick = scenario.wheel_rows();
    check_shift(
        &scenario,
        &boundary,
        &after,
        &(0..=tick),
        "prepend at the cap",
    );
    let shifts = after.shifts_from(&boundary);
    let visible_before = boundary.markers().len();
    scenario.check(
        shifts.len() + usize::try_from(tick).unwrap_or(0) >= visible_before
            && shifts
                .values()
                .all(|shift| Some(*shift) == shifts.values().next().copied()),
        || format!("visible content changed across eviction: {shifts:?}"),
    );
    // W7: nothing near the reader blocks eviction here, so the window returns under its cap.
    let (first, last) = scenario.resident().expect("resident window");
    scenario.check(last - first < RESIDENT_MAX_EVENTS, || {
        format!("window {first}..={last} exceeds the resident cap after the prepend")
    });
}

/// BUG-A4: reaching the session start (an exhausting page that adds no rows) does not shift
/// content; the history boundary keeps a constant height.
pub fn bug_a4_session_start_does_not_shift_content<D: TranscriptScenarioDriver>() {
    let mut session = ScenarioSession::new();
    for _ in 0..3 {
        session.push_silent();
    }
    for _ in 0..6 {
        session.push_user(1);
        session.push_assistant(3);
    }
    let (mut scenario, boundary) = at_older_boundary::<D>(session);
    let response = scenario
        .complete_kind(HistoryRequestKind::Older)
        .expect("older request");
    scenario.check(!response.has_older, || {
        "page did not exhaust history".to_owned()
    });
    let after = scenario.present();
    check_shift(
        &scenario,
        &boundary,
        &after,
        &(0..=0),
        "session start reached",
    );
}

// ---------------------------------------------------------------------------------------------
// Bug B: timeline jump then scrolling back down flings
// ---------------------------------------------------------------------------------------------

/// BUG-B1: after a timeline jump, the first newer page is incorporated without evicting older
/// resident content and without moving the reader.
pub fn bug_b1_timeline_window_first_newer_page_keeps_window<D: TranscriptScenarioDriver>() {
    let (mut scenario, boundary) = timeline_window_at_newer_boundary::<D>();
    let (first_before, _) = scenario.resident().expect("resident window");
    scenario.complete_kind(HistoryRequestKind::Newer);
    let after = scenario.present();
    // Page Down leaves at most one step of movement for the page (C28).
    check_shift(&scenario, &boundary, &after, &(-10..=0), "first newer page");
    let (first_after, _) = scenario.resident().expect("resident window");
    scenario.check(first_after == first_before, || {
        format!(
            "first newer page evicted older content: first resident {first_before} -> {first_after}"
        )
    });
}

/// BUG-B2: as BUG-A1 in the newer direction: a newer page that also evicts hundreds of items
/// above, followed by a wheel tick before the next frame, moves content only by the tick.
pub fn bug_b2_wheel_between_newer_page_with_eviction_and_frame<D: TranscriptScenarioDriver>() {
    let (mut scenario, boundary) = timeline_window_at_newer_boundary::<D>();
    scenario.complete_kind(HistoryRequestKind::Newer);
    scenario.wheel(WheelDirection::Down, 1);
    let after = scenario.present();
    let tick = scenario.wheel_rows();
    check_shift(
        &scenario,
        &boundary,
        &after,
        &(-(tick + 10)..=-tick),
        "wheel tick between newer page and frame",
    );
}

/// BUG-B3: after a deep timeline jump (tail not resident), jump-to-latest shows live content
/// with exactly one request (a tail window, W10), issues no further navigation requests, follows
/// the bottom, and live events then apply.
pub fn bug_b3_jump_to_latest_after_deep_jump_returns_live<D: TranscriptScenarioDriver>() {
    let session = ScenarioSession::with_turns(1_000, 1);
    let mut scenario = Scenario::<D>::attach_latest(session, 20, WIDTH, HEIGHT);
    scenario.present();
    scenario.navigate(ScenarioIntent::JumpToSequence(500));
    scenario.complete_kind(HistoryRequestKind::Around);
    scenario.present();
    scenario.navigate(ScenarioIntent::JumpToLatest);
    let requests = scenario.pending();
    scenario.check(
        requests.len() == 1 && requests[0].request.kind() == HistoryRequestKind::Tail,
        || format!("jump-to-latest must issue exactly one tail-window request, got {requests:?}"),
    );
    scenario.complete(&requests[0]);
    scenario.present();
    scenario.advance(SETTLE);
    let latest = scenario.present();
    // Prefetch of older history may follow (W5); reaching the tail must not need more requests.
    let extra = scenario
        .pending()
        .into_iter()
        .filter(|pending| pending.request.kind() != HistoryRequestKind::Older)
        .collect::<Vec<_>>();
    scenario.check(extra.is_empty(), || {
        format!("returning to latest issued further requests: {extra:?}")
    });
    let last = scenario.session().last_marker(2_000).expect("item");
    scenario.check(latest.shows(last) && latest.following_bottom, || {
        "jump-to-latest did not return to the live tail with one request".to_owned()
    });
    let sequence = scenario.append_live_assistant(1);
    scenario.advance(SETTLE);
    let live = scenario.present();
    let last = scenario.session().last_marker(sequence).expect("item");
    scenario.check(live.shows(last), || {
        "live event after returning to latest is not shown".to_owned()
    });
}

/// BUG-B4: a resident timeline jump while detached (inside a timeline window, reader scrolled
/// away, a live event arriving before the next frame) lands on the target in the next committed
/// frame and stays there.
pub fn bug_b4_resident_jump_while_detached_lands_and_stays<D: TranscriptScenarioDriver>() {
    const TARGET: u64 = 1_100;
    let session = ScenarioSession::with_turns(1_000, 1);
    let mut scenario = Scenario::<D>::attach_latest(session, 20, WIDTH, HEIGHT);
    scenario.present();
    scenario.navigate(ScenarioIntent::JumpToSequence(1_000));
    scenario.complete_kind(HistoryRequestKind::Around);
    scenario.present();
    scenario.wheel(WheelDirection::Down, 5);
    scenario.present();
    // The reader pauses before opening the timeline; the jump is a separate gesture.
    scenario.advance(SETTLE);
    let reading = scenario.present();
    scenario.check(!reading.following_bottom, || {
        "reader is not detached".to_owned()
    });
    scenario.navigate(ScenarioIntent::JumpToSequence(TARGET));
    scenario.check(scenario.pending().is_empty(), || {
        "resident jump requested history".to_owned()
    });
    scenario.append_live_assistant(2);
    let landed = scenario.present();
    scenario.check(item_top_at_viewport_top(&landed, TARGET), || {
        "resident jump did not put the target at the top".to_owned()
    });
    scenario.present();
    scenario.advance(SETTLE);
    let stayed = scenario.present();
    scenario.check(item_top_at_viewport_top(&stayed, TARGET), || {
        "resident jump target did not stay at the top".to_owned()
    });
}

// ---------------------------------------------------------------------------------------------
// Height change during motion
// ---------------------------------------------------------------------------------------------

/// Start a streaming assistant response after a user prompt while following the bottom.
fn start_streaming_response<D: TranscriptScenarioDriver>(width: u16) -> (Scenario<D>, u64) {
    let mut session = ScenarioSession::with_turns(10, 2);
    session.push_user(1);
    let mut scenario = Scenario::<D>::start(super::driver::ScenarioSetup {
        session,
        resident: super::driver::ResidentSeed::Everything,
        width,
        height: HEIGHT,
    });
    scenario.present();
    scenario.advance(SETTLE);
    scenario.present();
    let item = scenario.session().next_sequence();
    scenario.append_live_with(|session| session.push_assistant_delta(item, 0..3));
    scenario.advance(Duration::from_millis(40));
    scenario.present();
    scenario.append_live_with(|session| session.push_assistant_delta(item, 3..30));
    scenario.advance(Duration::from_millis(40));
    scenario.present();
    (scenario, item)
}

/// Whether `item`'s first line is the top-most marker, on the viewport's first or second row
/// (the second row leaves room for an item header).
fn item_top_at_viewport_top(frame: &ScenarioFrame, item: u64) -> bool {
    frame.top_marker().map(|at| at.marker) == Some(Marker::new(item, 0))
        && frame
            .row_of(Marker::new(item, 0))
            .is_some_and(|row| row <= 1)
}

/// C11: the first visible content of a new assistant segment is revealed at the top and held
/// while the response grows past the viewport.
pub fn c11_streaming_response_reveals_top_and_holds<D: TranscriptScenarioDriver>() {
    let (mut scenario, item) = start_streaming_response::<D>(WIDTH);
    scenario.advance(SETTLE);
    scenario.present();
    let settled = scenario.present();
    scenario.check(item_top_at_viewport_top(&settled, item), || {
        "streaming response top is not held at the viewport top".to_owned()
    });
}

/// C11/C17/C23: content changes height while the reveal motion runs; the motion still settles
/// with the revealed item's top at the viewport top.
///
/// The height change is a reflow (narrower terminal) rather than an asynchronous visual, because
/// the fake session has no asynchronous visual producer; both change rows above the target while
/// the motion's target must be resolved by key.
pub fn c17_reflow_during_reveal_motion_keeps_target<D: TranscriptScenarioDriver>() {
    let (mut scenario, item) = start_streaming_response::<D>(WIDTH);
    scenario.input(ScenarioInput::Resize {
        width: 24,
        height: HEIGHT,
    });
    scenario.advance(Duration::from_millis(40));
    scenario.present();
    scenario.advance(SETTLE);
    scenario.present();
    let settled = scenario.present();
    scenario.check(item_top_at_viewport_top(&settled, item), || {
        "reveal motion did not settle on the revealed item's top after reflow".to_owned()
    });
}

/// Read older content, then activate the latest bar and present the first motion frame.
fn jump_to_latest_motion_started<D: TranscriptScenarioDriver>() -> Scenario<D> {
    let session = ScenarioSession::with_turns(30, 3);
    let mut scenario = Scenario::<D>::attach_latest(session, 60, WIDTH, HEIGHT);
    scenario.present();
    scenario.wheel(WheelDirection::Up, 10);
    scenario.present();
    scenario.advance(SETTLE);
    let reading = scenario.present();
    let Some(bar) = reading.latest_bar else {
        scenario.fail("no latest bar while reading older content");
    };
    scenario.input(ScenarioInput::Click { at: bar.center() });
    scenario.advance(Duration::from_millis(40));
    let moving = scenario.present();
    scenario.check(moving.motion_running, || {
        "latest-bar activation started no motion".to_owned()
    });
    scenario
}

/// C17/C5/T13: content above the reader changes height (reflow) while the jump-to-latest motion
/// started from the latest bar runs; the motion re-resolves its key target and settles following
/// the latest bottom.
///
/// The height change is a reflow rather than an asynchronous visual (no asynchronous visual
/// producer exists in the fake session); both change rows above the reader during the motion.
pub fn c17_height_change_during_latest_motion_lands_on_latest<D: TranscriptScenarioDriver>() {
    let mut scenario = jump_to_latest_motion_started::<D>();
    scenario.input(ScenarioInput::Resize {
        width: 24,
        height: HEIGHT,
    });
    scenario.advance(Duration::from_millis(40));
    scenario.present();
    scenario.advance(SETTLE);
    scenario.present();
    scenario.advance(SETTLE);
    let settled = scenario.present();
    let newest = scenario.session().last_sequence().expect("item");
    let last = scenario.session().last_marker(newest).expect("item");
    scenario.check(
        !settled.motion_running
            && settled.following_bottom
            && settled.bottom_marker().map(|at| at.marker) == Some(last),
        || "jump-to-latest motion did not settle following the latest bottom".to_owned(),
    );
}

/// C17/N7/T3: a live assistant item arriving during the jump-to-latest motion is shown at the
/// bottom and every motion settles (the item's own reveal may run once, C12).
pub fn c17_live_item_during_latest_motion_settles<D: TranscriptScenarioDriver>() {
    let mut scenario = jump_to_latest_motion_started::<D>();
    let sequence = scenario.append_live_assistant(3);
    for _ in 0..3 {
        scenario.advance(SETTLE);
        scenario.present();
    }
    let settled = scenario.last_frame().clone();
    let last = scenario.session().last_marker(sequence).expect("item");
    scenario.check(
        !settled.motion_running && settled.bottom_marker().map(|at| at.marker) == Some(last),
        || "motion did not settle after a live item arrived during jump-to-latest".to_owned(),
    );
}
