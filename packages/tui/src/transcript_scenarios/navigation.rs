//! Kept-behavior navigation scenarios, generic over the engine driver.
//!
//! These scenarios pin the kept navigation behavior (rows
//! C1–C17, C22–C33, and the focus model's Home/End rules FO4/FO5). Each one asserts what the
//! specification requires, observed through committed frames: marker rows, content shift, the
//! latest bar, following state, motion and focus. Names start with the requirement they cover;
//! `docs/tui-v2-scenarios.md` maps each to its IDs and records per-engine status.
//!
//! Shift sign convention: positive means content moved down (toward older content).

use std::ops::{Range, RangeInclusive};
use std::time::Duration;

use bcode_session_models::{SessionEvent, SessionEventKind};

use super::Scenario;
use super::driver::{
    HistoryRequestKind, ResidentSeed, ScenarioInput, ScenarioIntent, ScenarioSetting,
    ScenarioSetup, TranscriptScenarioDriver, WheelDirection,
};
use super::frame::{FocusObservation, ScenarioFrame};
use super::session::{Marker, ScenarioSession};

const WIDTH: u16 = 80;
const HEIGHT: u16 = 20;
/// Narrow width used to reflow scenario text (marker lines are about 26 cells wide).
const NARROW: u16 = 24;
/// Long enough for any motion and the manual-scroll grace to settle.
const SETTLE: Duration = Duration::from_secs(2);
/// One presentation tick: shorter than any motion and than the manual-scroll grace.
const TICK: Duration = Duration::from_millis(40);
/// Structural rules that collapse every tool unit.
const COLLAPSE_TOOLS: &str =
    "[[rules]]\nmatch = { kind = \"tool\" }\nset = { disclosure = \"collapsed\" }";
/// Distinctive composer text, observable in frames without marker tokens.
const PROMPT: &str = "zqx prompt";

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Start `session` fully resident at `width`×[`HEIGHT`], then present a settled frame.
fn attach_everything<D: TranscriptScenarioDriver>(
    session: ScenarioSession,
    width: u16,
) -> Scenario<D> {
    let mut scenario = Scenario::<D>::start(ScenarioSetup {
        session,
        resident: ResidentSeed::Everything,
        width,
        height: HEIGHT,
    });
    scenario.present();
    scenario.advance(SETTLE);
    scenario.present();
    scenario
}

/// Present, let motion and grace settle, and present again; returns the settled frame.
fn settle<D: TranscriptScenarioDriver>(scenario: &mut Scenario<D>) -> ScenarioFrame {
    scenario.present();
    scenario.advance(SETTLE);
    scenario.present();
    scenario.advance(SETTLE);
    scenario.present()
}

/// Scroll up by `ticks` wheel ticks as one gesture and return the settled reading frame.
fn read_older<D: TranscriptScenarioDriver>(
    scenario: &mut Scenario<D>,
    ticks: usize,
) -> ScenarioFrame {
    scenario.wheel(WheelDirection::Up, ticks);
    let reading = settle(scenario);
    scenario.check(!reading.following_bottom, || {
        "wheel up did not detach from the latest bottom".to_owned()
    });
    reading
}

/// Blank transcript rows below the bottom-most visible marker.
fn blank_rows_below(frame: &ScenarioFrame) -> Option<i32> {
    frame
        .bottom_marker()
        .map(|at| i32::from(frame.transcript.height) - 1 - i32::from(at.row))
}

/// Transcript-area row of the first transcript row containing `needle`.
fn text_row(frame: &ScenarioFrame, needle: &str) -> Option<u16> {
    frame
        .transcript_rows()
        .iter()
        .position(|row| row.contains(needle))
        .and_then(|row| u16::try_from(row).ok())
}

/// Bottom marker of `frame`, as a marker only.
fn bottom(frame: &ScenarioFrame) -> Option<Marker> {
    frame.bottom_marker().map(|at| at.marker)
}

/// Fail unless `after` moved `before`'s top-most surviving marker by exactly `expected` rows.
#[track_caller]
fn check_shift_eq<D: TranscriptScenarioDriver>(
    scenario: &Scenario<D>,
    before: &ScenarioFrame,
    after: &ScenarioFrame,
    expected: i32,
    what: &str,
) {
    check_shift_in(scenario, before, after, &(expected..=expected), what);
}

/// Fail unless `after` moved `before`'s top-most surviving marker by a row count in `expected`.
#[track_caller]
fn check_shift_in<D: TranscriptScenarioDriver>(
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

/// Type `text` into whichever surface owns keyboard input.
fn type_text<D: TranscriptScenarioDriver>(scenario: &mut Scenario<D>, text: &str) {
    for character in text.chars() {
        let key = if character == ' ' {
            "space".to_owned()
        } else {
            character.to_string()
        };
        scenario.key(&key);
    }
}

/// Marker line text of one scenario line.
fn marker_line(sequence: u64, line: u32) -> String {
    format!(
        "{} item {sequence} line {line}",
        Marker::new(sequence, line).token()
    )
}

/// Markdown body (hard breaks) of marker lines `lines` of item `sequence`.
fn markdown_lines(sequence: u64, lines: Range<u32>) -> String {
    lines
        .map(|line| marker_line(sequence, line))
        .collect::<Vec<_>>()
        .join("\\\n")
}

/// Append a system message with `lines` plain marker lines; returns the event and its last
/// marker.
fn push_system_lines(session: &mut ScenarioSession, lines: u32) -> (SessionEvent, Marker) {
    let sequence = session.next_sequence();
    let lines = lines.max(1);
    let text = (0..lines)
        .map(|line| marker_line(sequence, line))
        .collect::<Vec<_>>()
        .join("\n");
    let event = session.push_kind(SessionEventKind::SystemMessage { text });
    (event, Marker::new(sequence, lines - 1))
}

/// Append a finished `bash` tool call whose output is `output_lines` marker lines keyed by the
/// call's sequence.
fn push_tool(session: &mut ScenarioSession, output_lines: u32) {
    let sequence = session.next_sequence();
    let id = format!("call-{sequence}");
    session.push_kind(SessionEventKind::ToolCallRequested {
        tool_call_id: id.clone(),
        producer_plugin_id: None,
        tool_name: "bash".to_owned(),
        arguments_json: serde_json::json!({ "command": "ls" }).to_string(),
        working_directory: None,
    });
    let output = (1..=output_lines)
        .map(|line| marker_line(sequence, line))
        .collect::<Vec<_>>()
        .join("\n");
    session.push_kind(SessionEventKind::ToolInvocationResultRecorded {
        record: bcode_session_models::ToolInvocationResultRecord {
            invocation_id: id,
            model_output: output,
            is_error: false,
            presentation: None,
            result: None,
            content: Vec::new(),
        },
    });
}

/// Append a requested (still running) tool call.
fn push_tool_request(session: &mut ScenarioSession) -> SessionEvent {
    let sequence = session.next_sequence();
    session.push_kind(SessionEventKind::ToolCallRequested {
        tool_call_id: format!("call-{sequence}"),
        producer_plugin_id: None,
        tool_name: "bash".to_owned(),
        arguments_json: serde_json::json!({ "command": "ls" }).to_string(),
        working_directory: None,
    })
}

/// Append an assistant item of one long marker line that wraps at narrow widths.
fn push_long_assistant(session: &mut ScenarioSession) {
    let sequence = session.next_sequence();
    let text = format!(
        "{} {}",
        marker_line(sequence, 0),
        "padding words that wrap ".repeat(6)
    );
    session.push_kind(SessionEventKind::AssistantMessage { text });
}

/// The accepted canonical user message for a submission of `text`.
fn accepted_user_message(session: &mut ScenarioSession, text: &str) -> SessionEvent {
    session.push_kind(SessionEventKind::UserMessage {
        client_id: bcode_session_models::ClientId::new(),
        text: text.to_owned(),
        admission: bcode_session_models::TurnAdmissionMetadata::default(),
    })
}

/// Completion message of a streamed assistant item keyed `item` with `lines` marker lines.
fn assistant_completion(session: &mut ScenarioSession, item: u64, lines: u32) -> SessionEvent {
    session.push_kind(SessionEventKind::AssistantMessage {
        text: markdown_lines(item, 0..lines),
    })
}

/// Rows the focused transcript target occupies, relative to the transcript viewport top.
fn focused_extent<D: TranscriptScenarioDriver>(scenario: &Scenario<D>) -> Range<i64> {
    scenario
        .driver
        .focused_extent()
        .unwrap_or_else(|| scenario.fail("no focused transcript target"))
}

/// Whether `extent` lies entirely inside a viewport of `height` rows.
fn fully_visible(extent: &Range<i64>, height: u16) -> bool {
    extent.start >= 0 && extent.end <= i64::from(height)
}

/// Session of finished tool calls between short assistant items (focus-target rich).
fn tool_session(calls: usize) -> ScenarioSession {
    let mut session = ScenarioSession::new();
    for _ in 0..calls {
        session.push_user(1);
        push_tool(&mut session, 3);
        session.push_assistant(2);
    }
    session
}

/// Wheel down at the latest bottom by `ticks` (one gesture) and return the settled frame.
fn enter_tail_space<D: TranscriptScenarioDriver>(
    scenario: &mut Scenario<D>,
    ticks: usize,
) -> ScenarioFrame {
    scenario.wheel(WheelDirection::Down, ticks);
    settle(scenario)
}

/// Following session whose newest item is a user prompt, ready for a streamed response.
fn prompt_last<D: TranscriptScenarioDriver>(turns: usize) -> Scenario<D> {
    let mut session = ScenarioSession::with_turns(turns, 2);
    session.push_user(1);
    attach_everything(session, WIDTH)
}

/// Stream assistant item lines `lines` as one delta; returns the item key.
fn stream<D: TranscriptScenarioDriver>(
    scenario: &mut Scenario<D>,
    item: Option<u64>,
    lines: Range<u32>,
) -> u64 {
    let item = item.unwrap_or_else(|| scenario.session().next_sequence());
    scenario.append_live_with(|session| session.push_assistant_delta(item, lines));
    item
}

/// Deliver a system message of `lines` marker lines live; returns its last marker.
fn append_system<D: TranscriptScenarioDriver>(scenario: &mut Scenario<D>, lines: u32) -> Marker {
    let mut last = None;
    scenario.append_live_with(|session| {
        let (event, marker) = push_system_lines(session, lines);
        last = Some(marker);
        event
    });
    last.expect("system message marker")
}

/// Submit [`PROMPT`] from the composer and deliver its acceptance; returns the settled frame.
fn submit_and_accept<D: TranscriptScenarioDriver>(scenario: &mut Scenario<D>) -> ScenarioFrame {
    type_text(scenario, PROMPT);
    scenario.key("enter");
    scenario.present();
    scenario.append_live_with(|session| accepted_user_message(session, PROMPT));
    settle(scenario)
}

/// Read older content, activate the latest bar and present the first motion frame.
fn latest_motion_started<D: TranscriptScenarioDriver>() -> Scenario<D> {
    let session = ScenarioSession::with_turns(30, 3);
    let mut scenario = attach_everything::<D>(session, WIDTH);
    let reading = read_older(&mut scenario, 10);
    let Some(bar) = reading.latest_bar else {
        scenario.fail("no latest bar while reading older content");
    };
    scenario.input(ScenarioInput::Click { at: bar.center() });
    scenario.advance(TICK);
    let moving = scenario.present();
    scenario.check(moving.motion_running, || {
        "latest-bar activation started no motion".to_owned()
    });
    scenario
}

/// Newest marker of the canonical session (last line of its newest scenario item).
fn newest_marker<D: TranscriptScenarioDriver>(scenario: &Scenario<D>) -> Marker {
    let newest = scenario
        .session()
        .last_sequence()
        .expect("session has events");
    scenario
        .session()
        .last_marker(newest)
        .unwrap_or_else(|| Marker::new(newest, 0))
}

/// Whether `item`'s first line is the top-most marker, on the viewport's first or second row
/// (the second row leaves room for an item header).
fn item_top_at_viewport_top(frame: &ScenarioFrame, item: u64) -> bool {
    frame.top_marker().map(|at| at.marker) == Some(Marker::new(item, 0))
        && frame
            .row_of(Marker::new(item, 0))
            .is_some_and(|row| row <= 1)
}

/// Whether the frame follows the latest bottom with `last` as its bottom marker.
fn at_latest_bottom(frame: &ScenarioFrame, last: Marker) -> bool {
    frame.following_bottom && bottom(frame) == Some(last)
}

/// Long session attached by its latest window, with the transcript focused by a click.
fn transcript_focused_latest_window<D: TranscriptScenarioDriver>() -> Scenario<D> {
    let session = ScenarioSession::with_turns(400, 1);
    let mut scenario = Scenario::<D>::attach_latest(session, 40, WIDTH, HEIGHT);
    settle(&mut scenario);
    // FO2: alt+u is the single key that moves keyboard focus into the transcript.
    scenario.key("alt+u");
    let focused = settle(&mut scenario);
    scenario.check(
        matches!(focused.focus, FocusObservation::Transcript { .. }),
        || "alt+u did not focus the transcript".to_owned(),
    );
    scenario
}

/// Wheel up a 12-event latest window until an older page is requested; returns the boundary
/// frame with the request still pending.
fn older_request_pending<D: TranscriptScenarioDriver>() -> (Scenario<D>, ScenarioFrame) {
    let session = ScenarioSession::with_turns(40, 3);
    let mut scenario = Scenario::<D>::attach_latest(session, 12, WIDTH, HEIGHT);
    scenario.present();
    if scenario
        .wheel_until_request(WheelDirection::Up, HistoryRequestKind::Older, 200)
        .is_none()
    {
        scenario.fail("no older request at the window top");
    }
    let boundary = scenario.last_frame().clone();
    (scenario, boundary)
}

// ---------------------------------------------------------------------------------------------
// C1: viewport modes
// ---------------------------------------------------------------------------------------------

/// C1: attaching follows the latest bottom: the newest line sits on the last content row.
pub fn c1_attach_follows_latest_bottom<D: TranscriptScenarioDriver>() {
    let scenario = attach_everything::<D>(ScenarioSession::with_turns(20, 3), WIDTH);
    let frame = scenario.last_frame().clone();
    let last = newest_marker(&scenario);
    scenario.check(
        at_latest_bottom(&frame, last) && blank_rows_below(&frame).is_some_and(|rows| rows <= 1),
        || "attach does not follow the latest bottom".to_owned(),
    );
}

/// C1: wheel up enters reading by exactly the wheel distance; wheeling back down to the content
/// bottom resumes following (T8, T9).
pub fn c1_wheel_up_reads_and_wheel_down_resumes_following<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(ScenarioSession::with_turns(20, 3), WIDTH);
    let following = scenario.last_frame().clone();
    let tick = scenario.wheel_rows();
    let reading = read_older(&mut scenario, 2);
    check_shift_eq(&scenario, &following, &reading, 2 * tick, "wheel up");
    scenario.wheel(WheelDirection::Down, 2);
    settle(&mut scenario);
    scenario.append_live_assistant(3);
    let live = settle(&mut scenario);
    let last = newest_marker(&scenario);
    scenario.check(at_latest_bottom(&live, last), || {
        "wheeling back to the content bottom did not resume following".to_owned()
    });
}

// ---------------------------------------------------------------------------------------------
// C2–C7: virtual (tail) space
// ---------------------------------------------------------------------------------------------

/// C2/T10: wheel down at the latest bottom moves content up into virtual space below the latest
/// content, by the wheel distance.
pub fn c2_wheel_down_at_bottom_enters_tail_space<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(ScenarioSession::with_turns(20, 3), WIDTH);
    let following = scenario.last_frame().clone();
    let tick = scenario.wheel_rows();
    let tail = enter_tail_space(&mut scenario, 1);
    check_shift_eq(
        &scenario,
        &following,
        &tail,
        -tick,
        "wheel down at the bottom",
    );
    let before = blank_rows_below(&following).unwrap_or_default();
    scenario.check(blank_rows_below(&tail) == Some(before + tick), || {
        format!(
            "tail space shows {:?} blank rows, expected {}",
            blank_rows_below(&tail),
            before + tick
        )
    });
}

/// C2: tail space is bounded by `viewport_height - 1` rows: further wheel input changes nothing,
/// and wheeling back up by `n` rows brings the newest line back to within `n` rows of the top.
///
/// The bound is measured from the content bottom, which may include one trailing separator row
/// after the newest line, so the newest line itself may sit just above the viewport at the bound.
pub fn c2_tail_space_is_bounded_by_the_viewport<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(ScenarioSession::with_turns(20, 3), WIDTH);
    let tick = scenario.wheel_rows();
    // Wheel until the view stops moving, so the check does not depend on how many ticks an
    // engine needs to reach the bound.
    let mut bounded = enter_tail_space(&mut scenario, 40);
    for _ in 0..8 {
        let next = enter_tail_space(&mut scenario, 10);
        if next.rows == bounded.rows {
            break;
        }
        bounded = next;
    }
    let further = enter_tail_space(&mut scenario, 20);
    scenario.check(further.rows == bounded.rows, || {
        "wheel input past the tail-space bound still moved the view".to_owned()
    });
    scenario.wheel(WheelDirection::Up, 2);
    let back = settle(&mut scenario);
    let last = newest_marker(&scenario);
    let row = back.row_of(last).map(i32::from);
    scenario.check(
        row.is_some_and(|row| (2 * tick - 2..=2 * tick).contains(&row)),
        || {
            format!(
                "newest line at row {row:?} after wheeling {} rows back up",
                2 * tick
            )
        },
    );
}

/// C3: tail space is a top-origin reading position: frames and time without input never move it.
pub fn c3_tail_space_position_is_stable_across_frames<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(ScenarioSession::with_turns(20, 3), WIDTH);
    let tail = enter_tail_space(&mut scenario, 2);
    for _ in 0..3 {
        scenario.advance(SETTLE);
        let frame = scenario.present();
        check_shift_eq(&scenario, &tail, &frame, 0, "tail space across frames");
    }
}

/// C4/T11: new content fills virtual space without moving existing content, even inside the
/// manual-scroll grace; once the content reaches the viewport bottom, following resumes.
pub fn c4_new_content_fills_tail_space_then_follows<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(ScenarioSession::with_turns(20, 3), WIDTH);
    scenario.wheel(WheelDirection::Down, 3);
    let tail = scenario.present();
    // Still inside the grace: no clock advance since the wheel.
    let sequence = scenario.append_live_assistant(1);
    let filled = scenario.present();
    check_shift_eq(&scenario, &tail, &filled, 0, "content filling tail space");
    scenario.check(filled.shows(Marker::new(sequence, 0)), || {
        "new content is not shown in the tail space".to_owned()
    });
    for _ in 0..4 {
        scenario.append_live_assistant(3);
    }
    let caught_up = settle(&mut scenario);
    let last = newest_marker(&scenario);
    scenario.check(
        at_latest_bottom(&caught_up, last) && !caught_up.motion_running,
        || "filling the tail space did not resume following".to_owned(),
    );
    scenario.append_live_assistant(2);
    let followed = settle(&mut scenario);
    let last = newest_marker(&scenario);
    scenario.check(at_latest_bottom(&followed, last), || {
        "content after the tail space filled is not followed".to_owned()
    });
}

/// C5: growth and shrink never drift: reflowing narrower and back restores the same reading
/// position, and repeated presentation is idempotent.
pub fn c5_reflow_round_trip_does_not_drift<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(ScenarioSession::with_turns(20, 3), WIDTH);
    let reading = read_older(&mut scenario, 4);
    scenario.input(ScenarioInput::Resize {
        width: NARROW,
        height: HEIGHT,
    });
    settle(&mut scenario);
    scenario.input(ScenarioInput::Resize {
        width: WIDTH,
        height: HEIGHT,
    });
    let restored = settle(&mut scenario);
    check_shift_eq(&scenario, &reading, &restored, 0, "reflow round trip");
    let again = scenario.present();
    check_shift_eq(&scenario, &restored, &again, 0, "repeated presentation");
}

/// C6: rows inserted above the viewport (reflow of wrapping items above) do not consume
/// virtual space: the blank rows below the newest line are unchanged.
pub fn c6_rows_inserted_above_do_not_consume_tail_space<D: TranscriptScenarioDriver>() {
    let mut session = ScenarioSession::new();
    for _ in 0..12 {
        session.push_user(1);
        push_long_assistant(&mut session);
    }
    // Everything inside the viewport is short lines that fit at both widths; only items above
    // the viewport reflow. Ten short turns fill the viewport under any engine's item spacing.
    for _ in 0..10 {
        session.push_user(1);
        session.push_assistant(1);
    }
    let mut scenario = attach_everything::<D>(session, WIDTH);
    let tail = enter_tail_space(&mut scenario, 2);
    let blank = blank_rows_below(&tail);
    scenario.input(ScenarioInput::Resize {
        width: 40,
        height: HEIGHT,
    });
    let reflowed = settle(&mut scenario);
    scenario.check(blank_rows_below(&reflowed) == blank, || {
        format!(
            "rows above changed the tail space from {blank:?} to {:?} blank rows",
            blank_rows_below(&reflowed)
        )
    });
}

/// C7/T12: scrolling up while in tail space keeps tail space (it does not snap to following).
pub fn c7_wheel_up_inside_tail_space_keeps_tail_space<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(ScenarioSession::with_turns(20, 3), WIDTH);
    let tick = scenario.wheel_rows();
    let tail = enter_tail_space(&mut scenario, 3);
    scenario.wheel(WheelDirection::Up, 1);
    let detached = settle(&mut scenario);
    check_shift_eq(&scenario, &tail, &detached, tick, "wheel up in tail space");
    let blank = blank_rows_below(&tail).unwrap_or_default();
    scenario.check(
        blank_rows_below(&detached) == Some(blank - tick) && !detached.following_bottom,
        || "detaching inside tail space did not keep the remaining tail space".to_owned(),
    );
}

// ---------------------------------------------------------------------------------------------
// C8–C9: submission reveal
// ---------------------------------------------------------------------------------------------

/// C8/N9: a staged ("sending") submission never moves the view before semantic acceptance.
///
/// The reader is reading older content: the staged submission's pending row (LR1) is tail
/// content below the viewport and moves nothing. While following, the pending row is shown
/// on the latest bottom like any tail content
/// (`local_rows::lr7_staged_submission_while_following_stays_on_latest_bottom`).
pub fn c8_staged_submission_does_not_move_view<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(ScenarioSession::with_turns(10, 3), WIDTH);
    let before = read_older(&mut scenario, 4);
    type_text(&mut scenario, PROMPT);
    scenario.key("enter");
    let staged = scenario.present();
    check_shift_eq(&scenario, &before, &staged, 0, "staged submission");
    scenario.advance(Duration::from_millis(250));
    let later = scenario.present();
    check_shift_eq(
        &scenario,
        &before,
        &later,
        0,
        "staged submission after 250 ms",
    );
}

/// C8/T2: on acceptance, the user message moves to the top row and the space below it stays
/// open; the motion settles.
pub fn c8_accepted_submission_reveals_message_at_top<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(ScenarioSession::with_turns(10, 3), WIDTH);
    let settled = submit_and_accept(&mut scenario);
    let row = text_row(&settled, PROMPT);
    scenario.check(
        row.is_some_and(|row| row <= 1) && !settled.motion_running,
        || format!("accepted submission is on row {row:?}, not at the top"),
    );
}

/// C8/N9: a submission that is cleared without a semantic message (a local slash command) never
/// anchors or moves the view.
pub fn c8_cleared_local_command_submission_does_not_anchor<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(ScenarioSession::with_turns(10, 3), WIDTH);
    let before = scenario.last_frame().clone();
    type_text(&mut scenario, "/help");
    scenario.key("enter");
    scenario.key("escape");
    let after = settle(&mut scenario);
    check_shift_eq(&scenario, &before, &after, 0, "cleared local submission");
}

/// C9/T4: tool activity after an accepted submission keeps the message on top while it fits,
/// then resumes following the latest rows once it overflows.
pub fn c9_tool_activity_after_submission_resumes_following<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(ScenarioSession::with_turns(10, 3), WIDTH);
    submit_and_accept(&mut scenario);
    scenario.append_live_with(push_tool_request);
    let fitting = settle(&mut scenario);
    scenario.check(
        text_row(&fitting, PROMPT).is_some_and(|row| row <= 1),
        || "fitting tool activity moved the submitted message off the top".to_owned(),
    );
    let last = append_system(&mut scenario, 40);
    let overflowed = settle(&mut scenario);
    scenario.check(
        overflowed.shows(last) && overflowed.following_bottom,
        || "overflowing activity after the submission is not followed".to_owned(),
    );
}

// ---------------------------------------------------------------------------------------------
// C10–C13: assistant reveal and hold
// ---------------------------------------------------------------------------------------------

/// C10/T3: the first visible content of a new assistant segment is revealed; later updates to
/// the same segment never move it.
pub fn c10_later_stream_updates_do_not_move_revealed_response<D: TranscriptScenarioDriver>() {
    let mut scenario = prompt_last::<D>(10);
    let item = stream(&mut scenario, None, 0..1);
    let revealed = settle(&mut scenario);
    let row = revealed.row_of(Marker::new(item, 0));
    scenario.check(item_top_at_viewport_top(&revealed, item), || {
        "the new assistant segment was not revealed at the top".to_owned()
    });
    stream(&mut scenario, Some(item), 1..4);
    let grown = settle(&mut scenario);
    scenario.check(grown.row_of(Marker::new(item, 0)) == row, || {
        format!(
            "a later update moved the response from row {row:?} to {:?}",
            grown.row_of(Marker::new(item, 0))
        )
    });
}

/// C11/T7: the assistant hold survives the segment's completion and later presentations.
pub fn c11_assistant_hold_survives_completion<D: TranscriptScenarioDriver>() {
    let mut scenario = prompt_last::<D>(10);
    let item = stream(&mut scenario, None, 0..1);
    let held = settle(&mut scenario);
    let row = held.row_of(Marker::new(item, 0));
    scenario.check(item_top_at_viewport_top(&held, item), || {
        "the new assistant segment was not revealed and held at the top".to_owned()
    });
    scenario.append_live_with(|session| assistant_completion(session, item, 1));
    for _ in 0..4 {
        scenario.advance(SETTLE);
        let frame = scenario.present();
        scenario.check(
            row.is_some() && frame.row_of(Marker::new(item, 0)) == row,
            || "completion moved the held assistant response".to_owned(),
        );
    }
}

/// C11/T6: a new non-assistant item after the held response does not move content that fits,
/// and overflowing content is then followed.
pub fn c11_non_assistant_item_releases_hold_into_overflow_follow<D: TranscriptScenarioDriver>() {
    let mut scenario = prompt_last::<D>(10);
    let item = stream(&mut scenario, None, 0..1);
    let held = settle(&mut scenario);
    scenario.check(item_top_at_viewport_top(&held, item), || {
        "the new assistant segment was not revealed and held at the top".to_owned()
    });
    scenario.append_live_with(push_tool_request);
    let fitting = settle(&mut scenario);
    check_shift_eq(
        &scenario,
        &held,
        &fitting,
        0,
        "fitting tool item after the hold",
    );
    scenario.check(fitting.shows(Marker::new(item, 0)), || {
        "held response left the view".to_owned()
    });
    let last = append_system(&mut scenario, 40);
    let overflowed = settle(&mut scenario);
    scenario.check(
        overflowed.shows(last) && overflowed.following_bottom,
        || "overflow after the released hold is not followed".to_owned(),
    );
}

/// C12/N10: a revealed item is admitted once: after the reader scrolls away, further updates and
/// the completion of the same item never reveal it again.
pub fn c12_revealed_item_is_not_readmitted<D: TranscriptScenarioDriver>() {
    let mut scenario = prompt_last::<D>(10);
    let item = stream(&mut scenario, None, 0..2);
    settle(&mut scenario);
    let reading = read_older(&mut scenario, 2);
    stream(&mut scenario, Some(item), 2..3);
    scenario.append_live_with(|session| assistant_completion(session, item, 3));
    let after = settle(&mut scenario);
    check_shift_eq(&scenario, &reading, &after, 0, "update of an admitted item");
}

/// C13/T5–T6: a sticky assistant reveal holds through overflow until the segment ends; the next
/// non-assistant item hands off to following the overflow.
pub fn c13_sticky_reveal_hands_off_after_segment_end<D: TranscriptScenarioDriver>() {
    let mut scenario = prompt_last::<D>(10);
    let item = stream(&mut scenario, None, 0..3);
    scenario.present();
    stream(&mut scenario, Some(item), 3..30);
    let held = settle(&mut scenario);
    scenario.check(item_top_at_viewport_top(&held, item), || {
        "overflowing assistant response does not hold its top".to_owned()
    });
    scenario.append_live_with(|session| assistant_completion(session, item, 30));
    scenario.append_live_with(push_tool_request);
    let last = append_system(&mut scenario, 6);
    let handed_off = settle(&mut scenario);
    scenario.check(
        handed_off.shows(last) && handed_off.following_bottom,
        || "the next item after the held segment is not followed".to_owned(),
    );
}

// ---------------------------------------------------------------------------------------------
// C14–C16: manual input and grace
// ---------------------------------------------------------------------------------------------

/// C14/N6: manual input during a motion stops it at its visual position and applies only the
/// input's distance; the motion does not resume.
pub fn c14_manual_scroll_materializes_running_motion<D: TranscriptScenarioDriver>() {
    let mut scenario = latest_motion_started::<D>();
    let moving = scenario.last_frame().clone();
    let tick = scenario.wheel_rows();
    scenario.wheel(WheelDirection::Up, 1);
    let stopped = scenario.present();
    check_shift_eq(&scenario, &moving, &stopped, tick, "wheel during motion");
    let settled = settle(&mut scenario);
    check_shift_eq(
        &scenario,
        &stopped,
        &settled,
        0,
        "after the cancelled motion",
    );
    scenario.check(!settled.motion_running && !settled.following_bottom, || {
        "the cancelled motion resumed".to_owned()
    });
}

/// C14/T22: manual input while a non-resident timeline jump is pending cancels the jump; its
/// result never moves the reader.
pub fn c14_manual_scroll_cancels_pending_jump<D: TranscriptScenarioDriver>() {
    let session = ScenarioSession::with_turns(400, 1);
    let mut scenario = Scenario::<D>::attach_latest(session, 40, WIDTH, HEIGHT);
    scenario.present();
    let reading = read_older(&mut scenario, 2);
    scenario.navigate(ScenarioIntent::JumpToSequence(101));
    let tick = scenario.wheel_rows();
    scenario.wheel(WheelDirection::Up, 1);
    let scrolled = scenario.present();
    check_shift_eq(
        &scenario,
        &reading,
        &scrolled,
        tick,
        "wheel with a pending jump",
    );
    if let Some(pending) = scenario.pending_of(HistoryRequestKind::Around) {
        scenario.complete(&pending);
    }
    let after = settle(&mut scenario);
    check_shift_eq(
        &scenario,
        &scrolled,
        &after,
        0,
        "result of the cancelled jump",
    );
}

/// C15/N5: during the manual-scroll grace, a new assistant stream does not top-anchor; it only
/// fills the tail space below unmoved content.
pub fn c15_grace_suppresses_stream_top_anchor<D: TranscriptScenarioDriver>() {
    let mut scenario = prompt_last::<D>(10);
    scenario.wheel(WheelDirection::Down, 1);
    let tail = scenario.present();
    // No clock advance: the grace is still running.
    stream(&mut scenario, None, 0..1);
    let frame = scenario.present();
    check_shift_eq(&scenario, &tail, &frame, 0, "stream during grace");
    scenario.check(!frame.motion_running, || {
        "a reveal motion started during the manual-scroll grace".to_owned()
    });
}

/// C16/T23: a new stream never anchors while the reader is scrolled up.
pub fn c16_stream_does_not_anchor_when_scrolled_up<D: TranscriptScenarioDriver>() {
    let mut scenario = prompt_last::<D>(20);
    let reading = read_older(&mut scenario, 3);
    let item = stream(&mut scenario, None, 0..2);
    let after = settle(&mut scenario);
    check_shift_eq(&scenario, &reading, &after, 0, "stream while reading");
    scenario.check(!after.shows(Marker::new(item, 0)), || {
        "the stream pulled the reader to the response".to_owned()
    });
}

// ---------------------------------------------------------------------------------------------
// C17: animation targets
// ---------------------------------------------------------------------------------------------

/// C17/T13: activating the latest bar settles following the latest bottom.
pub fn c17_latest_bar_motion_lands_on_latest_bottom<D: TranscriptScenarioDriver>() {
    let mut scenario = latest_motion_started::<D>();
    let settled = settle(&mut scenario);
    let last = newest_marker(&scenario);
    scenario.check(
        !settled.motion_running && at_latest_bottom(&settled, last),
        || "latest-bar motion did not settle on the latest bottom".to_owned(),
    );
}

/// C17/T18: a pending non-resident jump never falls back to the tail: frames before its result
/// keep the reader, and the result lands the target at the top.
pub fn c17_pending_jump_never_falls_back_to_tail<D: TranscriptScenarioDriver>() {
    let session = ScenarioSession::with_turns(400, 1);
    let mut scenario = Scenario::<D>::attach_latest(session, 40, WIDTH, HEIGHT);
    scenario.present();
    let reading = read_older(&mut scenario, 2);
    scenario.navigate(ScenarioIntent::JumpToSequence(101));
    for _ in 0..2 {
        scenario.advance(TICK);
        let waiting = scenario.present();
        check_shift_eq(
            &scenario,
            &reading,
            &waiting,
            0,
            "frame before the jump result",
        );
        scenario.check(!waiting.following_bottom, || {
            "a pending jump fell back to following".to_owned()
        });
    }
    if scenario.complete_kind(HistoryRequestKind::Around).is_none() {
        scenario.fail("a non-resident jump requested no window");
    }
    let landed = settle(&mut scenario);
    scenario.check(
        landed.top_marker().map(|at| at.marker) == Some(Marker::new(101, 0)),
        || "the jump target is not at the top".to_owned(),
    );
}

// ---------------------------------------------------------------------------------------------
// C22: reduced motion
// ---------------------------------------------------------------------------------------------

/// C22/N7: with `reduce_motion`, a latest-bar jump lands in the first frame without a motion.
pub fn c22_reduced_motion_jump_is_instant<D: TranscriptScenarioDriver>() {
    let session = ScenarioSession::with_turns(30, 3);
    let mut scenario = attach_everything::<D>(session, WIDTH);
    scenario
        .driver
        .configure(&ScenarioSetting::ReducedMotion(true));
    let reading = read_older(&mut scenario, 10);
    let Some(bar) = reading.latest_bar else {
        scenario.fail("no latest bar while reading older content");
    };
    scenario.input(ScenarioInput::Click { at: bar.center() });
    let landed = scenario.present();
    let last = newest_marker(&scenario);
    scenario.check(
        !landed.motion_running && at_latest_bottom(&landed, last),
        || "reduced-motion jump did not land immediately".to_owned(),
    );
}

/// C22/N7: enabling `reduce_motion` while a motion runs settles it at its target immediately.
pub fn c22_enabling_reduced_motion_mid_motion_settles_at_target<D: TranscriptScenarioDriver>() {
    let mut scenario = latest_motion_started::<D>();
    scenario
        .driver
        .configure(&ScenarioSetting::ReducedMotion(true));
    scenario.advance(TICK);
    let landed = scenario.present();
    let last = newest_marker(&scenario);
    scenario.check(
        !landed.motion_running && at_latest_bottom(&landed, last),
        || "toggling reduced motion mid-motion did not settle at the target".to_owned(),
    );
}

// ---------------------------------------------------------------------------------------------
// C23–C26: anchoring across change
// ---------------------------------------------------------------------------------------------

/// C23/N11: the reading position is kept by item identity and row: an older page and a live
/// append incorporated in the same update before the next frame leave visible content in place.
pub fn c23_reader_survives_page_and_live_append_before_frame<D: TranscriptScenarioDriver>() {
    let (mut scenario, _) = older_request_pending::<D>();
    scenario.advance(SETTLE);
    let boundary = scenario.present();
    scenario.complete_kind(HistoryRequestKind::Older);
    scenario.append_live_assistant(3);
    let after = scenario.present();
    // Only movement left over from the boundary tick may be spent against the page (C28).
    let tick = scenario.wheel_rows();
    check_shift_in(
        &scenario,
        &boundary,
        &after,
        &(0..=tick),
        "page plus live append",
    );
}

/// C24: a width change keeps the source line at the top of the viewport (source-backed anchor),
/// and changing back restores the original frame.
pub fn c24_width_change_keeps_source_line_at_top<D: TranscriptScenarioDriver>() {
    let mut session = ScenarioSession::new();
    for _ in 0..8 {
        session.push_user(1);
        session.push_assistant(8);
    }
    let mut scenario = attach_everything::<D>(session, WIDTH);
    // Sequence 8 is the fourth turn's assistant item; land a few lines into it.
    let item = 8;
    scenario.navigate(ScenarioIntent::JumpToSequence(item));
    settle(&mut scenario);
    scenario.wheel(WheelDirection::Down, 1);
    let reading = settle(&mut scenario);
    let Some(top) = reading.top_marker() else {
        scenario.fail("nothing visible");
    };
    scenario.check(top.marker.sequence == item && top.marker.line > 0, || {
        format!("setup did not read inside item {item}")
    });
    scenario.input(ScenarioInput::Resize {
        width: NARROW,
        height: HEIGHT,
    });
    let narrow = settle(&mut scenario);
    scenario.check(
        narrow.top_marker().map(|at| at.marker) == Some(top.marker)
            && narrow.row_of(top.marker).is_some_and(|row| row <= top.row),
        || {
            format!(
                "width change moved source line {:?} off the top",
                top.marker
            )
        },
    );
    scenario.input(ScenarioInput::Resize {
        width: WIDTH,
        height: HEIGHT,
    });
    let wide = settle(&mut scenario);
    scenario.check(wide.row_of(top.marker) == Some(top.row), || {
        "changing the width back did not restore the source line".to_owned()
    });
}

/// C25: structural insertion (a new tool unit while reading) keeps the reading position.
pub fn c25_structural_insertion_keeps_reading_position<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(tool_session(10), WIDTH);
    let reading = read_older(&mut scenario, 6);
    scenario.append_live_with(push_tool_request);
    let after = settle(&mut scenario);
    check_shift_eq(&scenario, &reading, &after, 0, "structural insertion");
}

/// C25: collapsing every tool unit (including units above the viewport) by configuration keeps
/// the reading line at its row.
pub fn c25_collapse_above_keeps_reading_position<D: TranscriptScenarioDriver>() {
    let mut session = ScenarioSession::new();
    for _ in 0..8 {
        session.push_user(1);
        push_tool(&mut session, 4);
        session.push_assistant(8);
    }
    let mut scenario = attach_everything::<D>(session, WIDTH);
    // Sequence 16 is the fourth turn's assistant item (user, call, result, assistant).
    let item = 16;
    scenario.navigate(ScenarioIntent::JumpToSequence(item));
    settle(&mut scenario);
    scenario.wheel(WheelDirection::Down, 1);
    let reading = settle(&mut scenario);
    let Some(top) = reading.top_marker() else {
        scenario.fail("nothing visible");
    };
    scenario.check(top.marker.sequence == item, || {
        format!("setup did not read inside item {item}")
    });
    scenario
        .driver
        .configure(&ScenarioSetting::PresentationRules(
            COLLAPSE_TOOLS.to_owned(),
        ));
    let collapsed = settle(&mut scenario);
    scenario.check(collapsed.row_of(top.marker) == Some(top.row), || {
        format!("collapsing units moved the reading line {:?}", top.marker)
    });
}

/// C26/T23: runtime-work and usage events after a final response never pull it to the bottom.
pub fn c26_runtime_work_events_do_not_move_final_response<D: TranscriptScenarioDriver>() {
    let mut scenario = prompt_last::<D>(10);
    let item = stream(&mut scenario, None, 0..4);
    settle(&mut scenario);
    scenario.append_live_with(|session| assistant_completion(session, item, 4));
    let before = settle(&mut scenario);
    scenario.append_live_with(|session| {
        session.push_kind(SessionEventKind::RuntimeWorkStarted {
            work_id: bcode_session_models::WorkId::new("work-1"),
            kind: bcode_session_models::RuntimeWorkKind::ModelTurn,
            label: "model turn".to_owned(),
            tool_call_id: None,
            plugin_id: None,
            service_interface: None,
            operation: None,
            parent_work_id: None,
            started_at_ms: None,
            cancellable: false,
        })
    });
    scenario.append_live_with(|session| {
        session.push_kind(SessionEventKind::ModelUsage {
            turn_id: "turn-1".to_owned(),
            usage: bcode_session_models::SessionTokenUsage::default(),
        })
    });
    let after = settle(&mut scenario);
    check_shift_eq(&scenario, &before, &after, 0, "runtime-work events");
}

// ---------------------------------------------------------------------------------------------
// C27: presentation does not change navigation
// ---------------------------------------------------------------------------------------------

/// C27/CK1: presenting repeatedly without input or time is idempotent, while a motion runs and
/// while reading.
pub fn c27_repeated_presentation_is_idempotent<D: TranscriptScenarioDriver>() {
    let mut scenario = latest_motion_started::<D>();
    let moving = scenario.last_frame().clone();
    let again = scenario.present();
    scenario.check(again.rows == moving.rows, || {
        "re-presenting a motion frame without time moved it".to_owned()
    });
    let reading = read_older(&mut scenario, 2);
    let again = scenario.present();
    scenario.check(again.rows == reading.rows, || {
        "re-presenting a reading frame changed it".to_owned()
    });
}

/// C27/CK2–CK3: a presentation that fails does not commit navigation; events delivered after
/// it apply to the last committed navigation state.
///
/// A reader scrolls up one tick and the frame showing it fails to present; a live append then
/// arrives. The next presented frame shows exactly the one tick of movement from the committed
/// reading frame (nothing of the failed frame was spent or replayed) and the live append left
/// the reader in place (T23); a further tick whose first frame also fails moves by one tick.
pub fn c27_failed_presentation_does_not_commit_navigation<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(ScenarioSession::with_turns(10, 3), WIDTH);
    let reading = read_older(&mut scenario, 2);
    let tick = scenario.wheel_rows();
    // The tick's motion settles before the frame that would show it, so that frame differs
    // from the committed one and its output is written (and fails).
    scenario.wheel(WheelDirection::Up, 1);
    scenario.advance(SETTLE);
    scenario.driver.abandon_presentation();
    scenario.append_live_assistant(2);
    let after = settle(&mut scenario);
    check_shift_eq(
        &scenario,
        &reading,
        &after,
        tick,
        "a tick whose frame failed to present, then a live append",
    );
    scenario.check(!after.following_bottom, || {
        "the reader resumed following after a failed presentation".to_owned()
    });
    scenario.wheel(WheelDirection::Up, 1);
    scenario.advance(SETTLE);
    scenario.driver.abandon_presentation();
    let later = settle(&mut scenario);
    check_shift_eq(
        &scenario,
        &after,
        &later,
        tick,
        "the next tick, whose first frame also failed",
    );
}

// ---------------------------------------------------------------------------------------------
// C28–C31: history paging rules
// ---------------------------------------------------------------------------------------------

/// C28/N8: movement requested while an older page loads is kept and spent once against the
/// page's rows; it is not applied again afterwards.
pub fn c28_movement_kept_while_page_loads_is_spent_once<D: TranscriptScenarioDriver>() {
    let (mut scenario, boundary) = older_request_pending::<D>();
    let tick = scenario.wheel_rows();
    scenario.wheel(WheelDirection::Up, 2);
    scenario.present();
    scenario.complete_kind(HistoryRequestKind::Older);
    let spent = scenario.present();
    check_shift_in(
        &scenario,
        &boundary,
        &spent,
        &(1..=3 * tick),
        "kept movement spent against the page",
    );
    let later = settle(&mut scenario);
    check_shift_eq(&scenario, &spent, &later, 0, "kept movement applied again");
}

/// C29/N8: reversing direction while a page loads cancels the kept movement: the reversal moves
/// only by its own distance, the page is still incorporated, and neither its arrival nor later
/// frames spend the cancelled movement.
pub fn c29_direction_reversal_cancels_pending_movement<D: TranscriptScenarioDriver>() {
    let (mut scenario, _) = older_request_pending::<D>();
    let tick = scenario.wheel_rows();
    let first_before = scenario.resident().map(|(first, _)| first);
    scenario.wheel(WheelDirection::Up, 2);
    let held = scenario.present();
    scenario.wheel(WheelDirection::Down, 1);
    let reversed = scenario.present();
    // The reversal applies its own tick only; kept upward movement is not spent here.
    check_shift_in(
        &scenario,
        &held,
        &reversed,
        &(-tick..=-1),
        "reversal while the page loads",
    );
    if scenario.complete_kind(HistoryRequestKind::Older).is_none() {
        scenario.fail("the older page request disappeared after the reversal");
    }
    let after = scenario.present();
    let first_after = scenario.resident().map(|(first, _)| first);
    scenario.check(
        matches!((first_before, first_after), (Some(before), Some(after)) if after < before),
        || format!("the older page was not incorporated: first resident {first_before:?} -> {first_after:?}"),
    );
    check_shift_eq(&scenario, &reversed, &after, 0, "page after reversal");
    let later = settle(&mut scenario);
    check_shift_eq(
        &scenario,
        &after,
        &later,
        0,
        "cancelled movement spent later",
    );
}

/// C30/N12: content growth while following (reflow) is never interpreted as a prepend: the view
/// stays on the latest bottom.
pub fn c30_growth_while_following_is_not_a_prepend<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(ScenarioSession::with_turns(20, 3), WIDTH);
    scenario.input(ScenarioInput::Resize {
        width: NARROW,
        height: HEIGHT,
    });
    let reflowed = settle(&mut scenario);
    let last = newest_marker(&scenario);
    scenario.check(at_latest_bottom(&reflowed, last), || {
        "growth while following moved the view off the latest bottom".to_owned()
    });
}

/// C31/W10: the bottom of a historical (timeline) window is not the session tail: it does not
/// follow, it requests newer history, and live events stay out of view.
pub fn c31_window_bottom_is_not_session_tail<D: TranscriptScenarioDriver>() {
    let session = ScenarioSession::with_turns(1_000, 1);
    let mut scenario = Scenario::<D>::attach_latest(session, 20, WIDTH, HEIGHT);
    scenario.present();
    scenario.navigate(ScenarioIntent::JumpToSequence(1_001));
    if scenario.complete_kind(HistoryRequestKind::Around).is_none() {
        scenario.fail("timeline jump requested no window");
    }
    settle(&mut scenario);
    let mut reached = false;
    for _ in 0..1_000 {
        scenario.key("pagedown");
        scenario.present();
        if scenario.pending_of(HistoryRequestKind::Newer).is_some() {
            reached = true;
            break;
        }
    }
    scenario.check(reached, || {
        "paging down a historical window requested no newer page".to_owned()
    });
    let boundary = settle(&mut scenario);
    let tail = newest_marker(&scenario);
    scenario.check(!boundary.following_bottom && !boundary.shows(tail), || {
        "the historical window bottom was treated as the session tail".to_owned()
    });
    let live = scenario.append_live_assistant(1);
    let after = scenario.present();
    check_shift_eq(
        &scenario,
        &boundary,
        &after,
        0,
        "live event at a historical bottom",
    );
    scenario.check(!after.shows(Marker::new(live, 0)), || {
        "a live event appeared below a historical window".to_owned()
    });
}

// ---------------------------------------------------------------------------------------------
// C32–C33: focus navigation edges
// ---------------------------------------------------------------------------------------------

/// C32/T19: moving focus up scrolls so the newly focused target's first row is on the top edge;
/// every frame shows the focused target.
pub fn c32_focus_up_puts_target_top_on_top_edge<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(tool_session(12), WIDTH);
    scenario.key("alt+u");
    scenario.present();
    let mut moved = false;
    for step in 0..30 {
        let before = scenario.last_frame().clone();
        scenario.key("k");
        let frame = settle(&mut scenario);
        let extent = focused_extent(&scenario);
        let height = i64::from(frame.transcript.height);
        scenario.check(extent.start < height && extent.end > 0, || {
            format!("step {step}: focused target {extent:?} is not visible")
        });
        if frame.shift_from(&before) != Some(0) {
            moved = true;
            scenario.check(extent.start == 0, || {
                format!("step {step}: focus moving up scrolled to {extent:?}, not the top edge")
            });
        }
    }
    scenario.check(moved, || "walking focus up never scrolled".to_owned());
}

/// C32/T19: moving focus down scrolls so the target's last row is on the bottom edge (a taller
/// target keeps its top row on the top edge).
pub fn c32_focus_down_puts_target_bottom_on_bottom_edge<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(tool_session(12), WIDTH);
    scenario.key("alt+u");
    scenario.present();
    for _ in 0..40 {
        scenario.key("k");
    }
    settle(&mut scenario);
    let mut moved = false;
    for step in 0..30 {
        let before = scenario.last_frame().clone();
        scenario.key("j");
        let frame = settle(&mut scenario);
        let extent = focused_extent(&scenario);
        let height = i64::from(frame.transcript.height);
        scenario.check(extent.start < height && extent.end > 0, || {
            format!("step {step}: focused target {extent:?} is not visible")
        });
        if frame.shift_from(&before) != Some(0) {
            moved = true;
            scenario.check(extent.end == height || extent.start == 0, || {
                format!("step {step}: focus moving down scrolled to {extent:?}, not an edge")
            });
        }
    }
    scenario.check(moved, || "walking focus down never scrolled".to_owned());
}

/// C32: moving focus to a target that is already fully visible does not scroll.
pub fn c32_focus_on_visible_target_does_not_scroll<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(tool_session(12), WIDTH);
    scenario.key("alt+u");
    let entered = settle(&mut scenario);
    scenario.key("k");
    let moved = settle(&mut scenario);
    let extent = focused_extent(&scenario);
    scenario.check(fully_visible(&extent, moved.transcript.height), || {
        "setup: the previous target is not fully visible".to_owned()
    });
    check_shift_eq(&scenario, &entered, &moved, 0, "focus to a visible target");
}

/// C33: entering unit focus targets the last visible target without scrolling.
pub fn c33_initial_focus_is_last_visible_target<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(tool_session(12), WIDTH);
    let before = scenario.last_frame().clone();
    scenario.key("alt+u");
    let frame = settle(&mut scenario);
    scenario.check(
        frame.focus == FocusObservation::Transcript { units: true },
        || "alt+u did not enter unit focus".to_owned(),
    );
    check_shift_eq(&scenario, &before, &frame, 0, "entering focus");
    let extent = focused_extent(&scenario);
    let bottom_row = frame.bottom_marker().map(|at| i64::from(at.row));
    scenario.check(
        bottom_row.is_some_and(|row| extent.start <= row && row < extent.end),
        || format!("initial focus {extent:?} is not the last visible target"),
    );
}

/// C33: with no target starting inside the viewport (reading the middle of a tall item), the
/// initial focus is the nearest target above.
pub fn c33_initial_focus_falls_back_to_nearest_target_above<D: TranscriptScenarioDriver>() {
    let mut session = ScenarioSession::new();
    session.push_user(1);
    session.push_assistant(4);
    session.push_user(1);
    let tall = session.push_assistant(80).sequence;
    let mut scenario = attach_everything::<D>(session, WIDTH);
    scenario.navigate(ScenarioIntent::JumpToSequence(tall));
    settle(&mut scenario);
    scenario.wheel(WheelDirection::Down, 6);
    let reading = settle(&mut scenario);
    scenario.check(
        reading.visible_items().len() == 1
            && reading.top_marker().is_some_and(|at| at.marker.line > 0),
        || "setup: the viewport is not inside the tall item".to_owned(),
    );
    scenario.key("alt+u");
    let frame = settle(&mut scenario);
    // Entering focus may scroll the partly visible target into view (T19); either way the
    // focused target must be the tall item, so it spans every visible row of that item.
    let extent = focused_extent(&scenario);
    let rows = frame
        .visible_items()
        .into_iter()
        .find(|visible| visible.sequence == tall)
        .map(|visible| visible.rows);
    scenario.check(
        rows.is_some_and(|rows| {
            extent.start <= i64::from(*rows.start()) && i64::from(*rows.end()) < extent.end
        }),
        || format!("initial focus {extent:?} is not the tall item above"),
    );
}

// ---------------------------------------------------------------------------------------------
// FO4/FO5: Home/End and page keys per focus
// ---------------------------------------------------------------------------------------------

/// FO4: with the composer focused, Home/End edit the text input and never move the transcript.
pub fn fo4_composer_home_end_do_not_move_transcript<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(ScenarioSession::with_turns(20, 3), WIDTH);
    let reading = read_older(&mut scenario, 4);
    type_text(&mut scenario, "abc");
    scenario.key("home");
    type_text(&mut scenario, "x");
    scenario.key("end");
    type_text(&mut scenario, "y");
    let after = settle(&mut scenario);
    check_shift_eq(&scenario, &reading, &after, 0, "Home/End in the composer");
    scenario.check(
        after.contains("xabcy") && after.focus == FocusObservation::Composer,
        || "Home/End did not move the composer cursor".to_owned(),
    );
}

/// FO4/T16: with the transcript focused, Home loads the session start directly (one bounded
/// head window, not paging) and shows the first item at the top.
pub fn fo4_transcript_home_loads_session_start_directly<D: TranscriptScenarioDriver>() {
    let mut scenario = transcript_focused_latest_window::<D>();
    scenario.key("home");
    let pending = scenario.pending();
    scenario.check(
        pending.len() == 1 && pending[0].request.kind() == HistoryRequestKind::Head,
        || format!("Home issued {pending:?}, not one head window request"),
    );
    scenario.complete(&pending[0]);
    let start = settle(&mut scenario);
    scenario.check(
        start.top_marker().map(|at| at.marker) == Some(Marker::new(1, 0)),
        || "Home did not show the session start".to_owned(),
    );
}

/// FO4/T13: with the transcript focused, End returns to the latest bottom and follows.
pub fn fo4_transcript_end_returns_to_latest<D: TranscriptScenarioDriver>() {
    let mut scenario = transcript_focused_latest_window::<D>();
    read_older(&mut scenario, 4);
    scenario.key("end");
    let latest = settle(&mut scenario);
    let last = newest_marker(&scenario);
    scenario.check(at_latest_bottom(&latest, last), || {
        "End did not return to the latest bottom".to_owned()
    });
}

/// FO5: Page Up scrolls the transcript from composer focus without changing focus.
pub fn fo5_page_up_scrolls_from_composer_focus<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_everything::<D>(ScenarioSession::with_turns(20, 3), WIDTH);
    let following = scenario.last_frame().clone();
    scenario.key("pageup");
    let paged = settle(&mut scenario);
    check_shift_in(
        &scenario,
        &following,
        &paged,
        &(1..=i32::from(HEIGHT)),
        "Page Up",
    );
    scenario.check(paged.focus == FocusObservation::Composer, || {
        "Page Up moved focus out of the composer".to_owned()
    });
}
