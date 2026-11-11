//! Kept-behavior interaction scenarios: latest bar, selection and copy, disclosure clicks,
//! keyboard focus mode, the visibility floor, streaming Markdown and repaint scope.
//!
//! Like [`super::scenarios`], every scenario asserts what `docs/tui-v2-architecture.md`
//! requires and observes only committed frames plus the driver's semantic facts (selection
//! text, clipboard writes, repaint scope). Targets are located by unique text in the frame, so
//! no scenario reads engine hit regions or anchor fields.

use std::time::Duration;

use bcode_session_models::SessionEventKind;

use super::Scenario;
use super::driver::{
    CellPoint, HistoryRequestKind, LayoutWork, RepaintObservation, ScenarioInput, ScenarioIntent,
    ScenarioSetting, TranscriptScenarioDriver, WheelDirection,
};
use super::frame::{FocusObservation, ScenarioFrame};
use super::session::{Marker, ScenarioSession};

const WIDTH: u16 = 80;
const HEIGHT: u16 = 20;
/// Tall enough to show a whole tool fixture without scrolling.
const TALL: u16 = 40;
/// Long enough for any motion (scroll animation, reveal, glow) to settle.
const SETTLE: Duration = Duration::from_secs(2);
/// Shortly before the default post-copy clear delay (`tui.selection.clear_delay_ms` = 1000).
const BEFORE_CLEAR: Duration = Duration::from_millis(900);
/// Longest latest-bar glow window (full motion).
const GLOW_WINDOW: Duration = Duration::from_millis(630);
/// Text of the latest-content bar at every width.
const LATEST_BAR_TEXT: &str = "messages below";
/// Rules that group consecutive tool calls into one collapsed group.
const GROUP_TOOLS: &str = "[[rules]]\nmatch = { kind = \"tool\" }\nset = { group = { by = \"consecutive\" } }\n[[rules]]\nmatch = { kind = \"group\" }\nset = { disclosure = \"collapsed\" }";
/// Rules that hide every tool unit.
const HIDE_TOOLS: &str =
    "[[rules]]\nmatch = { kind = \"tool\" }\nset = { disclosure = \"hidden\" }";
/// Rules that collapse every tool unit.
const COLLAPSE_TOOLS: &str =
    "[[rules]]\nmatch = { kind = \"tool\" }\nset = { disclosure = \"collapsed\" }";
/// Columns past a located token that reach the end of its line (selection clamps there).
const LINE_END: u16 = 40;
/// Single-cell glyphs that may precede located text in a row (disclosure markers, chrome).
const NARROW_GLYPHS: &[char] = &['▾', '▸', '│', '·', '╭', '╰', '─', '┌', '└', '⠋'];

/// Driver capabilities used only by these scenarios.
pub(super) trait InteractionScenario {
    /// Apply one engine-neutral setting.
    fn apply_setting(&mut self, setting: &ScenarioSetting);
    /// Clipboard writes so far.
    fn copied(&self) -> Vec<String>;
    /// Repaint scope of the last frame.
    fn repaint(&self) -> Option<RepaintObservation>;
    /// Nonempty selection text.
    fn selected(&self) -> Option<String>;
    /// Press, drag and release the primary button from `from` to `to` without presenting.
    fn drag_select(&mut self, from: CellPoint, to: CellPoint);
    /// Click at `at` without presenting.
    fn click_at(&mut self, at: CellPoint);
}

impl<D: TranscriptScenarioDriver> InteractionScenario for Scenario<D> {
    fn apply_setting(&mut self, setting: &ScenarioSetting) {
        self.driver.configure(setting);
    }

    fn copied(&self) -> Vec<String> {
        self.driver.clipboard_writes()
    }

    fn repaint(&self) -> Option<RepaintObservation> {
        self.driver.last_repaint()
    }

    fn selected(&self) -> Option<String> {
        self.driver.selection_text().filter(|text| !text.is_empty())
    }

    fn drag_select(&mut self, from: CellPoint, to: CellPoint) {
        self.input(ScenarioInput::Press { at: from });
        self.input(ScenarioInput::DragTo { at: to });
        self.input(ScenarioInput::Release { at: to });
    }

    fn click_at(&mut self, at: CellPoint) {
        self.input(ScenarioInput::Click { at });
    }
}

// ---------------------------------------------------------------------------------------------
// Fixtures and frame helpers
// ---------------------------------------------------------------------------------------------

/// Append one finished `read` call whose result carries `output`.
pub(super) fn push_tool(
    session: &mut ScenarioSession,
    call: &str,
    target: &str,
    output: &str,
    failed: bool,
) {
    session.push_kind(SessionEventKind::ToolCallRequested {
        tool_call_id: call.to_owned(),
        producer_plugin_id: None,
        tool_name: "read".to_owned(),
        arguments_json: serde_json::json!({ "path": target }).to_string(),
        working_directory: None,
    });
    session.push_kind(SessionEventKind::ToolInvocationResultRecorded {
        record: bcode_session_models::ToolInvocationResultRecord {
            invocation_id: call.to_owned(),
            model_output: output.to_owned(),
            is_error: failed,
            presentation: None,
            result: None,
            content: Vec::new(),
        },
    });
}

/// Append a permission request awaiting the user's decision.
pub(super) fn push_permission(session: &mut ScenarioSession, command: &str) {
    session.push_kind(SessionEventKind::PermissionRequested {
        permission_id: "permission-1".to_owned(),
        tool_call_id: "call-awaiting".to_owned(),
        producer_plugin_id: None,
        tool_name: "bash".to_owned(),
        arguments_json: serde_json::json!({ "command": command }).to_string(),
        batch: None,
        policy_source: None,
        policy_reason: None,
    });
}

/// Two turns of history followed by three read calls (the middle one failed).
///
/// Output tokens: `ALPHAOUT1`/`ALPHAOUT2`, `BETAOUT1`/`BETAOUT2` (failed), `GAMMAOUT1`.
pub(super) fn tool_session() -> ScenarioSession {
    let mut session = ScenarioSession::with_turns(2, 2);
    session.push_user(1);
    push_tool(
        &mut session,
        "call-alpha",
        "alpha/one.rs",
        "ALPHAOUT1\nALPHAOUT2",
        false,
    );
    push_tool(
        &mut session,
        "call-beta",
        "beta/two.rs",
        "BETAOUT1\nBETAOUT2",
        true,
    );
    push_tool(
        &mut session,
        "call-gamma",
        "gamma/three.rs",
        "GAMMAOUT1",
        false,
    );
    session
}

/// Attach the whole of `session` at `width`×`height` and present the first frame.
pub(super) fn attach_all<D: TranscriptScenarioDriver>(
    session: ScenarioSession,
    width: u16,
    height: u16,
) -> Scenario<D> {
    let resident = session.events().len();
    let mut scenario = Scenario::<D>::attach_latest(session, resident, width, height);
    scenario.present();
    scenario
}

/// Cell where `needle` starts in row `y` of `row`.
///
/// Columns are counted in cells. The text before the needle must be ASCII or one of
/// [`NARROW_GLYPHS`], so a fixture never relies on measuring arbitrary text.
pub(super) fn locate_in_row(row: &str, needle: &str, y: usize) -> Option<CellPoint> {
    let byte = row.find(needle)?;
    let prefix = &row[..byte];
    assert!(
        prefix
            .chars()
            .all(|character| character.is_ascii() || NARROW_GLYPHS.contains(&character)),
        "text before {needle:?} has cells of unknown width: {prefix:?}"
    );
    Some(CellPoint {
        x: u16::try_from(prefix.chars().count()).ok()?,
        y: u16::try_from(y).ok()?,
    })
}

/// Cell where `needle` first appears in `frame`, top to bottom.
pub(super) fn locate(frame: &ScenarioFrame, needle: &str) -> Option<CellPoint> {
    frame
        .rows
        .iter()
        .enumerate()
        .find_map(|(y, row)| locate_in_row(row, needle, y))
}

/// Cell where `needle` starts in the last frame; fails the scenario when it is not visible.
#[track_caller]
pub(super) fn find<D: TranscriptScenarioDriver>(scenario: &Scenario<D>, needle: &str) -> CellPoint {
    locate(scenario.last_frame(), needle)
        .unwrap_or_else(|| scenario.fail(&format!("{needle:?} is not visible")))
}

/// Cell `columns` to the right of `at`.
pub(super) const fn right_of(at: CellPoint, columns: u16) -> CellPoint {
    CellPoint {
        x: at.x.saturating_add(columns),
        y: at.y,
    }
}

/// Header cell of the tool unit whose output contains `needle`.
#[track_caller]
pub(super) fn tool_header<D: TranscriptScenarioDriver>(
    scenario: &Scenario<D>,
    needle: &str,
) -> CellPoint {
    let output = find(scenario, needle);
    let rows = &scenario.last_frame().rows;
    (0..=usize::from(output.y))
        .rev()
        .find_map(|y| locate_in_row(&rows[y], "Tool result", y))
        .unwrap_or_else(|| scenario.fail(&format!("no tool header above {needle:?}")))
}

/// Whether `needle` is visible inside the transcript area.
pub(super) fn transcript_shows(frame: &ScenarioFrame, needle: &str) -> bool {
    frame
        .transcript_rows()
        .iter()
        .any(|row| row.contains(needle))
}

/// Whether the latest bar is shown, by area and by its text.
fn latest_bar_shown(frame: &ScenarioFrame) -> bool {
    frame.latest_bar.is_some() && frame.contains(LATEST_BAR_TEXT)
}

/// Whether the shown latest bar animates its glow (`None` when no bar is shown).
///
/// A still bar is its centered message plus at most a trailing `▾`; a glowing bar draws rail
/// glyphs on both sides of the message.
fn latest_bar_glowing(frame: &ScenarioFrame) -> Option<bool> {
    let bar = frame.latest_bar?;
    let row = frame.rows.get(usize::from(bar.y))?;
    let start = row.find(LATEST_BAR_TEXT)?;
    let (left, rest) = row.split_at(start);
    let right = rest
        .find("jump")
        .map_or("", |end| &rest[end + "jump".len()..]);
    let left = left.trim_end().trim_end_matches("New").trim();
    let right = right.trim();
    Some(!left.is_empty() || !(right.is_empty() || right == "▾"))
}

/// One-line turns scrolled up so newer content is below the reader; returns settled.
fn reading_above_latest<D: TranscriptScenarioDriver>() -> Scenario<D> {
    let session = ScenarioSession::with_turns(20, 1);
    let mut scenario = Scenario::<D>::attach_latest(session, 40, WIDTH, HEIGHT);
    scenario.present();
    scenario.wheel(WheelDirection::Up, 4);
    scenario.present();
    scenario.advance(SETTLE);
    let reading = scenario.present();
    scenario.check(!reading.following_bottom, || {
        "wheel up did not detach from the latest content".to_owned()
    });
    scenario
}

/// Drag across the two marker lines of the assistant item at `sequence`.
fn select_item<D: TranscriptScenarioDriver>(scenario: &mut Scenario<D>, sequence: u64) {
    let from = find(scenario, &Marker::new(sequence, 0).token());
    let to = find(scenario, &Marker::new(sequence, 1).token());
    scenario.drag_select(from, right_of(to, LINE_END));
}

// ---------------------------------------------------------------------------------------------
// Latest bar (C18-C21)
// ---------------------------------------------------------------------------------------------

/// C18: the hidden continuation of the entry being read never shows the latest bar.
pub fn c18_latest_bar_ignores_hidden_continuation_of_visible_entry<D: TranscriptScenarioDriver>() {
    let mut session = ScenarioSession::new();
    let item = session.push_assistant(30).sequence;
    let mut scenario = attach_all::<D>(session, WIDTH, 12);
    scenario.wheel(WheelDirection::Up, 40);
    scenario.present();
    scenario.advance(SETTLE);
    let frame = scenario.present();
    scenario.check(frame.shows(Marker::new(item, 0)), || {
        "the long entry's first line is not visible after scrolling to its top".to_owned()
    });
    scenario.check(!latest_bar_shown(&frame), || {
        "the latest bar shows for the hidden continuation of the visible entry".to_owned()
    });
}

/// C18: a distinct newer entry entirely below the viewport shows the latest bar.
pub fn c18_latest_bar_shows_for_distinct_entry_fully_below<D: TranscriptScenarioDriver>() {
    let session = ScenarioSession::with_assistant_items(20, 1);
    let last = session.last_sequence().expect("items");
    let mut scenario = attach_all::<D>(session, WIDTH, 12);
    scenario.wheel(WheelDirection::Up, 4);
    scenario.present();
    scenario.advance(SETTLE);
    let frame = scenario.present();
    scenario.check(!frame.shows(Marker::new(last, 0)), || {
        "the newest entry is still visible".to_owned()
    });
    scenario.check(latest_bar_shown(&frame), || {
        "no latest bar for a distinct newer entry fully below".to_owned()
    });
}

/// C18/T23: a live entry arriving below the reader shows the bar without moving the reader.
pub fn c18_live_entry_below_reader_shows_bar<D: TranscriptScenarioDriver>() {
    let mut scenario = reading_above_latest::<D>();
    let reading = scenario.last_frame().clone();
    scenario.append_live_assistant(2);
    let after = scenario.present();
    scenario.check(after.shift_from(&reading) == Some(0), || {
        format!(
            "a live entry below moved the reader by {:?}",
            after.shift_from(&reading)
        )
    });
    scenario.check(latest_bar_shown(&after), || {
        "no latest bar after a live entry arrived below the reader".to_owned()
    });
}

/// C18/X11: after a live entry arrived below the reader, jumping to latest settles following
/// the live entry and removes the bar.
pub fn c18_jump_to_latest_after_live_entry_follows_and_clears_bar<D: TranscriptScenarioDriver>() {
    let mut scenario = reading_above_latest::<D>();
    let sequence = scenario.append_live_assistant(2);
    scenario.present();
    scenario.navigate(ScenarioIntent::JumpToLatest);
    scenario.present();
    scenario.advance(SETTLE);
    let latest = scenario.present();
    let last = scenario.session().last_marker(sequence).expect("live item");
    scenario.check(
        latest.following_bottom && latest.shows(last) && !latest_bar_shown(&latest),
        || "jump to latest did not follow the live entry and clear the bar".to_owned(),
    );
}

/// C19: bar visibility is decided against the full transcript body. Walking up from the
/// latest content one wheel tick at a time, the bar shows exactly when the newest entry is out
/// of view, never while following, and repeating a frame keeps the same decision.
pub fn c19_latest_bar_never_causes_its_own_visibility<D: TranscriptScenarioDriver>() {
    let session = ScenarioSession::with_assistant_items(30, 1);
    let newest = Marker::new(session.last_sequence().expect("items"), 0);
    let mut scenario = attach_all::<D>(session, WIDTH, 12);
    for tick in 0..6 {
        if tick > 0 {
            scenario.wheel(WheelDirection::Up, 1);
            scenario.present();
            scenario.advance(SETTLE);
        }
        let first = scenario.present();
        let repeated = scenario.present();
        scenario.check(
            latest_bar_shown(&first) == latest_bar_shown(&repeated)
                && first.transcript_rows() == repeated.transcript_rows(),
            || format!("tick {tick}: repeating the frame changed the latest bar decision"),
        );
        if first.following_bottom {
            scenario.check(!latest_bar_shown(&first), || {
                format!("tick {tick}: the latest bar shows while following")
            });
        } else {
            scenario.check(latest_bar_shown(&first) != first.shows(newest), || {
                format!(
                    "tick {tick}: bar shown={} but newest entry visible={}",
                    latest_bar_shown(&first),
                    first.shows(newest)
                )
            });
        }
    }
}

/// C20: scrolling, timer-only refreshes and width reflow never generate latest activity; only
/// a regenerated entry below the reader does.
pub fn c20_scroll_timer_and_reflow_generate_no_activity<D: TranscriptScenarioDriver>() {
    let mut scenario = reading_above_latest::<D>();
    scenario.check(
        latest_bar_glowing(scenario.last_frame()) == Some(false),
        || "a reader above existing content does not see a still latest bar".to_owned(),
    );
    scenario.wheel(WheelDirection::Down, 1);
    let scrolled = scenario.present();
    scenario.advance(Duration::from_millis(50));
    let timer = scenario.present();
    scenario.input(ScenarioInput::Resize {
        width: WIDTH - 10,
        height: HEIGHT,
    });
    let reflowed = scenario.present();
    for (what, frame) in [
        ("scroll", &scrolled),
        ("timer", &timer),
        ("reflow", &reflowed),
    ] {
        scenario.check(latest_bar_glowing(frame) != Some(true), || {
            format!("{what} produced latest-content activity (the bar glows)")
        });
    }
    scenario.advance(SETTLE);
    scenario.present();
    scenario.append_live_assistant(2);
    let live = scenario.present();
    scenario.check(latest_bar_glowing(&live) == Some(true), || {
        "a regenerated entry below the reader produced no activity".to_owned()
    });
}

/// C21: activity glows, then settles to a still bar after the glow window and stays still.
pub fn c21_latest_bar_glow_settles_still_after_window<D: TranscriptScenarioDriver>() {
    let mut scenario = reading_above_latest::<D>();
    scenario.append_live_assistant(2);
    let glowing = scenario.present();
    scenario.check(latest_bar_glowing(&glowing) == Some(true), || {
        "live activity below the reader does not glow".to_owned()
    });
    scenario.advance(GLOW_WINDOW + Duration::from_millis(20));
    let settled = scenario.present();
    scenario.advance(Duration::from_millis(500));
    let later = scenario.present();
    scenario.check(
        latest_bar_glowing(&settled) == Some(false) && latest_bar_glowing(&later) == Some(false),
        || "the latest bar still glows after the glow window".to_owned(),
    );
}

/// C21/C22: under reduced motion the latest bar is still from its first frame.
pub fn c21_reduced_motion_latest_bar_is_still_immediately<D: TranscriptScenarioDriver>() {
    let mut scenario = reading_above_latest::<D>();
    scenario.apply_setting(&ScenarioSetting::ReducedMotion(true));
    scenario.present();
    scenario.append_live_assistant(2);
    let frame = scenario.present();
    scenario.check(latest_bar_glowing(&frame) == Some(false), || {
        "the latest bar glows under reduced motion".to_owned()
    });
}

// ---------------------------------------------------------------------------------------------
// Selection and copy (C34-C36)
// ---------------------------------------------------------------------------------------------

/// C34: a drag that starts inside a response and ends in a later item selects only that
/// response's source text.
pub fn c34_drag_inside_response_stays_within_it<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(ScenarioSession::with_turns(3, 2), WIDTH, TALL);
    // Start a few cells into the response's first line, well inside its text.
    let from = right_of(find(&scenario, &Marker::new(2, 0).token()), 3);
    let to = find(&scenario, &Marker::new(4, 1).token());
    scenario.drag_select(from, right_of(to, LINE_END));
    scenario.present();
    let selected = scenario.selected().unwrap_or_default();
    scenario.check(selected.contains(&Marker::new(2, 1).token()), || {
        format!("the response's own text is not selected: {selected:?}")
    });
    scenario.check(
        !selected.contains(&Marker::new(3, 0).token())
            && !selected.contains(&Marker::new(4, 0).token()),
        || format!("a drag inside a response selected later items: {selected:?}"),
    );
}

/// C34: a drag that starts in the space between items selects across items in document order.
pub fn c34_drag_from_surrounding_space_spans_items<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(ScenarioSession::with_turns(3, 2), WIDTH, TALL);
    let first = find(&scenario, &Marker::new(2, 1).token());
    let to = find(&scenario, &Marker::new(4, 0).token());
    // The blank separator row below the first response.
    let from = CellPoint {
        x: 0,
        y: first.y + 1,
    };
    scenario.drag_select(from, right_of(to, LINE_END));
    scenario.present();
    let selected = scenario.selected().unwrap_or_default();
    let user = selected.find(&Marker::new(3, 0).token());
    let response = selected.find(&Marker::new(4, 0).token());
    scenario.check(
        matches!((user, response), (Some(user), Some(response)) if user < response),
        || format!("a drag from surrounding space did not span items in order: {selected:?}"),
    );
}

/// C34/C36: selection copies source text; soft wraps of one source line add no newline.
pub fn c34_soft_wrapped_line_selects_without_newlines<D: TranscriptScenarioDriver>() {
    let mut session = ScenarioSession::with_turns(1, 1);
    let words = (0..14)
        .map(|word| format!("w{word:02}"))
        .collect::<Vec<_>>();
    let text = format!("WRAPSTART {} WRAPEND", words.join(" "));
    session.push_kind(SessionEventKind::AssistantMessage { text });
    let mut scenario = attach_all::<D>(session, 30, TALL);
    let from = find(&scenario, "WRAPSTART");
    let to = find(&scenario, "WRAPEND");
    scenario.check(to.y > from.y, || "the long line did not wrap".to_owned());
    // `WRAPEND` is seven cells; ending on its last cell keeps the drag inside the terminal.
    scenario.drag_select(from, right_of(to, 7));
    scenario.present();
    let selected = scenario.selected().unwrap_or_default();
    scenario.check(
        selected.contains("WRAPSTART w00") && selected.contains("w13 WRAPEND"),
        || format!("the wrapped line is not selected as source text: {selected:?}"),
    );
    scenario.check(!selected.trim_end().contains('\n'), || {
        format!("soft wraps added newlines to the selection: {selected:?}")
    });
}

/// C36: copying a code block keeps indentation and blank lines exactly.
pub fn c36_code_selection_preserves_indentation_and_blank_lines<D: TranscriptScenarioDriver>() {
    let mut session = ScenarioSession::with_turns(1, 1);
    session.push_kind(SessionEventKind::AssistantMessage {
        text: "Code:\n\n```rust\nfn codestart() {\n    let x = 1;\n\n        codeend(x);\n}\n```\n"
            .to_owned(),
    });
    let mut scenario = attach_all::<D>(session, WIDTH, TALL);
    let from = find(&scenario, "fn codestart");
    let to = find(&scenario, "codeend(x);");
    scenario.drag_select(from, right_of(to, LINE_END));
    scenario.present();
    let selected = scenario.selected().unwrap_or_default();
    scenario.check(
        selected.contains("fn codestart() {\n    let x = 1;\n\n        codeend(x);"),
        || format!("code selection lost indentation or blank lines: {selected:?}"),
    );
}

/// Session of two-line responses with copy enabled and the post-copy clear disabled, so the
/// selection only changes through the behavior under test.
fn retained_selection_session<D: TranscriptScenarioDriver>() -> Scenario<D> {
    let mut scenario = attach_all::<D>(ScenarioSession::with_turns(12, 2), WIDTH, HEIGHT);
    scenario.apply_setting(&ScenarioSetting::Selection {
        auto_copy_on_release: true,
        clear_after_copy: false,
        clear_delay_ms: 1_000,
    });
    scenario.present();
    scenario
}

/// Scroll by `ticks` wheel ticks and let the motion settle.
fn wheel_and_settle<D: TranscriptScenarioDriver>(
    scenario: &mut Scenario<D>,
    direction: WheelDirection,
    ticks: usize,
) -> ScenarioFrame {
    scenario.wheel(direction, ticks);
    scenario.present();
    scenario.advance(SETTLE);
    scenario.present()
}

/// C34: the selection keeps its source text when the view scrolls while it stays visible.
pub fn c34_selection_survives_scroll<D: TranscriptScenarioDriver>() {
    let mut scenario = retained_selection_session::<D>();
    // A response well above the bottom stays visible after one wheel tick up.
    let item = scenario.session().last_sequence().expect("items") - 2;
    select_item(&mut scenario, item);
    scenario.present();
    let selected = scenario.selected();
    scenario.check(selected.is_some(), || {
        "the drag selected nothing".to_owned()
    });
    let frame = wheel_and_settle(&mut scenario, WheelDirection::Up, 1);
    scenario.check(frame.shows(Marker::new(item, 1)), || {
        "the selected response scrolled out of view".to_owned()
    });
    scenario.check(scenario.selected() == selected, || {
        format!(
            "scrolling changed the selection from {selected:?} to {:?}",
            scenario.selected()
        )
    });
}

/// C34/BMUX 13: a selection keeps its logical endpoints while its rows scroll out of view and
/// back.
pub fn c34_selection_survives_scrolling_out_of_view<D: TranscriptScenarioDriver>() {
    let mut scenario = retained_selection_session::<D>();
    let item = scenario.session().last_sequence().expect("items");
    select_item(&mut scenario, item);
    scenario.present();
    let selected = scenario.selected();
    scenario.check(selected.is_some(), || {
        "the drag selected nothing".to_owned()
    });
    let away = wheel_and_settle(&mut scenario, WheelDirection::Up, 3);
    scenario.check(!away.shows(Marker::new(item, 0)), || {
        "the selected response is still visible".to_owned()
    });
    wheel_and_settle(&mut scenario, WheelDirection::Down, 3);
    scenario.check(scenario.selected() == selected, || {
        format!(
            "scrolling the selection out of view changed it from {selected:?} to {:?}",
            scenario.selected()
        )
    });
}

/// C34: a hard line break in the source copies as a line break; lines are never glued.
pub fn c34_hard_line_break_copies_as_line_break<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(ScenarioSession::with_turns(3, 2), WIDTH, TALL);
    select_item(&mut scenario, 4);
    scenario.present();
    let selected = scenario.selected().unwrap_or_default();
    let first_end = format!("item 4 line 0{}", Marker::new(4, 1).token());
    scenario.check(
        selected.contains(&Marker::new(4, 1).token()) && !selected.contains(&first_end),
        || format!("the hard line break was not copied as a line break: {selected:?}"),
    );
}

/// C35: releasing a drag copies the selection; the highlight clears 1000 ms after the copy.
pub fn c35_release_copies_and_clears_after_delay<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(ScenarioSession::with_turns(3, 2), WIDTH, TALL);
    select_item(&mut scenario, 4);
    scenario.present();
    let selected = scenario.selected();
    scenario.check(
        selected
            .as_ref()
            .is_some_and(|selected| scenario.copied() == [selected.clone()]),
        || {
            format!(
                "release did not copy the selection {selected:?}: {:?}",
                scenario.copied()
            )
        },
    );
    scenario.advance(BEFORE_CLEAR);
    scenario.present();
    scenario.check(scenario.selected().is_some(), || {
        "the selection cleared before the clear delay".to_owned()
    });
    scenario.advance(Duration::from_millis(200));
    scenario.present();
    scenario.check(scenario.selected().is_none(), || {
        "the selection did not clear after the clear delay".to_owned()
    });
}

/// C35: a failed copy keeps the selection and schedules no clear.
pub fn c35_failed_copy_keeps_selection<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(ScenarioSession::with_turns(3, 2), WIDTH, TALL);
    scenario.apply_setting(&ScenarioSetting::ClipboardFails(true));
    select_item(&mut scenario, 4);
    scenario.present();
    scenario.advance(SETTLE);
    scenario.present();
    scenario.check(
        scenario.selected().is_some() && scenario.copied().is_empty(),
        || "a failed copy did not keep the selection".to_owned(),
    );
}

/// C35: a new selection gesture cancels the previous copy's pending clear.
pub fn c35_new_gesture_cancels_pending_clear<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(ScenarioSession::with_turns(3, 2), WIDTH, TALL);
    select_item(&mut scenario, 2);
    scenario.present();
    scenario.advance(Duration::from_millis(600));
    scenario.present();
    select_item(&mut scenario, 4);
    scenario.present();
    let second = scenario.selected();
    // The first copy's deadline passes; the second selection must survive it.
    scenario.advance(Duration::from_millis(600));
    scenario.present();
    scenario.check(second.is_some() && scenario.selected() == second, || {
        format!(
            "the first copy's clear erased the new selection {second:?}: now {:?}",
            scenario.selected()
        )
    });
    scenario.check(scenario.copied().len() == 2, || {
        format!("expected two copies, got {:?}", scenario.copied())
    });
}

/// C35/X1: a click without drag on plain text neither selects nor copies.
pub fn c35_click_without_drag_does_not_copy<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(ScenarioSession::with_turns(3, 2), WIDTH, TALL);
    let at = find(&scenario, &Marker::new(4, 0).token());
    scenario.click_at(at);
    scenario.present();
    scenario.check(
        scenario.selected().is_none() && scenario.copied().is_empty(),
        || "a click without drag selected or copied text".to_owned(),
    );
}

// ---------------------------------------------------------------------------------------------
// Disclosure clicks (X1)
// ---------------------------------------------------------------------------------------------

/// X1: each click on a tool header toggles it exactly once (collapse, then expand again), and
/// a click never selects or copies.
pub fn x1_click_toggles_tool_header_once_per_click<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(tool_session(), WIDTH, TALL);
    scenario.check(transcript_shows(scenario.last_frame(), "ALPHAOUT1"), || {
        "the tool output is not expanded by default".to_owned()
    });
    let header = tool_header(&scenario, "ALPHAOUT1");
    scenario.click_at(header);
    let collapsed = scenario.present();
    scenario.check(!transcript_shows(&collapsed, "ALPHAOUT1"), || {
        "one click did not collapse the tool".to_owned()
    });
    scenario.check(transcript_shows(&collapsed, "GAMMAOUT1"), || {
        "the click toggled a different tool".to_owned()
    });
    let header = find(&scenario, "▸ Tool result");
    scenario.click_at(header);
    let expanded = scenario.present();
    scenario.check(transcript_shows(&expanded, "ALPHAOUT1"), || {
        "a second click did not expand the tool again".to_owned()
    });
    scenario.check(
        scenario.selected().is_none() && scenario.copied().is_empty(),
        || "clicking a header selected or copied text".to_owned(),
    );
}

/// X1: clicking a section label toggles only that section, not its unit.
pub fn x1_click_toggles_section_label<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(tool_session(), WIDTH, TALL);
    let output = find(&scenario, "ALPHAOUT1");
    let label_row = usize::from(output.y.saturating_sub(1));
    let label = locate_in_row(&scenario.last_frame().rows[label_row], "output", label_row)
        .unwrap_or_else(|| scenario.fail("no output section label above the tool output"));
    scenario.click_at(label);
    let collapsed = scenario.present();
    scenario.check(!transcript_shows(&collapsed, "ALPHAOUT1"), || {
        "clicking the section label did not collapse the section".to_owned()
    });
    let label = find(&scenario, "▸ output");
    let unit_header = (0..usize::from(label.y))
        .rev()
        .find(|y| collapsed.rows[*y].contains("Tool result"));
    scenario.check(
        unit_header.is_some_and(|y| collapsed.rows[y].contains('▾')),
        || "collapsing the section collapsed its unit".to_owned(),
    );
    scenario.click_at(label);
    let expanded = scenario.present();
    scenario.check(transcript_shows(&expanded, "ALPHAOUT1"), || {
        "a second click on the section label did not expand it".to_owned()
    });
}

/// X1/X3: clicking a collapsed group header expands the group; clicking again collapses it.
pub fn x1_click_toggles_group_header<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(tool_session(), WIDTH, TALL);
    scenario.apply_setting(&ScenarioSetting::PresentationRules(GROUP_TOOLS.to_owned()));
    scenario.present();
    let header = find(&scenario, "3 tool calls");
    scenario.click_at(header);
    let expanded = scenario.present();
    scenario.check(
        transcript_shows(&expanded, "ALPHAOUT1") && transcript_shows(&expanded, "GAMMAOUT1"),
        || "clicking the collapsed group header did not expand its members".to_owned(),
    );
    let header = find(&scenario, "3 tool calls");
    scenario.click_at(header);
    let collapsed = scenario.present();
    scenario.check(!transcript_shows(&collapsed, "ALPHAOUT1"), || {
        "a second click did not collapse the group".to_owned()
    });
}

/// X1/C35: a drag that starts on a header selects text and does not toggle the header.
pub fn x1_drag_from_header_selects_without_toggling<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(tool_session(), WIDTH, TALL);
    let header = tool_header(&scenario, "ALPHAOUT1");
    let to = find(&scenario, "ALPHAOUT2");
    scenario.drag_select(header, right_of(to, LINE_END));
    let frame = scenario.present();
    scenario.check(transcript_shows(&frame, "ALPHAOUT1"), || {
        "a drag from the header toggled it".to_owned()
    });
    let selected = scenario.selected().unwrap_or_default();
    scenario.check(selected.contains("ALPHAOUT1"), || {
        format!("a drag from the header did not select the output: {selected:?}")
    });
}

// ---------------------------------------------------------------------------------------------
// Keyboard focus mode (X2)
// ---------------------------------------------------------------------------------------------

/// Deliver `key` in focus mode and check the focused unit's output visibility and focus.
fn focus_key<D: TranscriptScenarioDriver>(
    scenario: &mut Scenario<D>,
    key: &str,
    output: &str,
    visible: bool,
    focused: Option<&String>,
) {
    scenario.key(key);
    let frame = scenario.present();
    scenario.check(transcript_shows(&frame, output) == visible, || {
        format!("after {key:?} the focused unit's output should be visible={visible}")
    });
    scenario.check(frame.focused_target.as_ref() == focused, || {
        format!(
            "{key:?} moved focus from {focused:?} to {:?}",
            frame.focused_target
        )
    });
}

/// X2: `alt+u` enters unit focus on the last visible target; `k`/`j` walk targets in
/// transcript order (unit headers and section labels); `h`, `l`, Enter, Space, `d` and `e`
/// change the focused unit's disclosure; `i` opens the inspector; Escape closes the inspector,
/// then leaves focus mode.
pub fn x2_focus_mode_keys_walk_and_toggle_targets<D: TranscriptScenarioDriver>() {
    let mut session = tool_session();
    push_permission(&mut session, "echo PERMTARGET");
    let mut scenario = attach_all::<D>(session, WIDTH, TALL);
    scenario.key("alt+u");
    let entered = scenario.present();
    scenario.check(
        entered.focus == FocusObservation::Transcript { units: true },
        || "alt+u did not enter unit focus mode".to_owned(),
    );
    let last = entered.focused_target;
    scenario.check(last.is_some(), || {
        "focus mode has no focused target".to_owned()
    });
    // Up: the last tool's output section label, then that tool's header.
    scenario.key("k");
    let section = scenario.present().focused_target;
    scenario.key("k");
    let unit = scenario.present().focused_target;
    scenario.check(section != last && unit != section && unit != last, || {
        format!("k did not walk targets: {last:?} -> {section:?} -> {unit:?}")
    });
    for (key, visible) in [
        ("h", false),
        ("l", true),
        ("enter", false),
        ("space", true),
        ("d", false),
        ("e", true),
    ] {
        focus_key(&mut scenario, key, "GAMMAOUT1", visible, unit.as_ref());
    }
    scenario.key("j");
    let next = scenario.present().focused_target;
    scenario.check(next == section, || {
        format!("j did not move to the next target {section:?}: {next:?}")
    });
    scenario.key("k");
    scenario.key("i");
    let inspector = scenario.present();
    scenario.check(
        transcript_shows(&inspector, "Inspector") && transcript_shows(&inspector, "gamma/three.rs"),
        || "i did not open the inspector for the focused unit".to_owned(),
    );
    scenario.key("escape");
    let closed = scenario.present();
    scenario.check(
        !transcript_shows(&closed, "Inspector")
            && closed.focus == FocusObservation::Transcript { units: true },
        || "the first Escape did not close only the inspector".to_owned(),
    );
    scenario.key("escape");
    let left = scenario.present();
    scenario.check(left.focus == FocusObservation::Composer, || {
        "the second Escape did not leave focus mode".to_owned()
    });
}

/// X2/C32: walking focus up past the top of the view scrolls each focused target into view.
pub fn x2_focus_walk_scrolls_targets_into_view<D: TranscriptScenarioDriver>() {
    let mut session = ScenarioSession::with_turns(1, 1);
    for call in 0..12 {
        push_tool(
            &mut session,
            &format!("call-{call}"),
            &format!("file{call:02}.rs"),
            &format!("WALK{call:02}A\nWALK{call:02}B\nWALK{call:02}C"),
            false,
        );
    }
    let mut scenario = attach_all::<D>(session, WIDTH, 14);
    scenario.check(!transcript_shows(scenario.last_frame(), "WALK00A"), || {
        "the fixture fits the view; nothing to scroll".to_owned()
    });
    scenario.key("alt+u");
    scenario.present();
    let mut saw_first = false;
    for _ in 0..40 {
        scenario.key("k");
        // Time passes between key presses, so the scroll toward each focused target (an
        // animated motion unless motion is reduced) completes before the next press.
        scenario.advance(SETTLE);
        let frame = scenario.present();
        scenario.check(frame.focused_target.is_some(), || {
            "focus mode lost its target while walking".to_owned()
        });
        saw_first |= transcript_shows(&frame, "WALK00A");
    }
    scenario.check(saw_first, || {
        "walking focus up never scrolled the first tool into view".to_owned()
    });
}

// ---------------------------------------------------------------------------------------------
// Structure v2 and the visibility floor (A16, X3)
// ---------------------------------------------------------------------------------------------

/// A16/X3: a collapsed group reveals its failed member in place; succeeded members stay hidden.
pub fn x3_collapsed_group_reveals_failed_member<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(tool_session(), WIDTH, TALL);
    scenario.apply_setting(&ScenarioSetting::PresentationRules(GROUP_TOOLS.to_owned()));
    let frame = scenario.present();
    scenario.check(frame.contains("3 tool calls"), || {
        "the three calls did not form one group (failed members stay members)".to_owned()
    });
    scenario.check(transcript_shows(&frame, "BETAOUT1"), || {
        "the collapsed group hides its failed member".to_owned()
    });
    scenario.check(
        !transcript_shows(&frame, "ALPHAOUT1") && !transcript_shows(&frame, "GAMMAOUT1"),
        || "the collapsed group shows succeeded members".to_owned(),
    );
}

/// A16: hidden units leave a count marker, while a failed unit is never below summary.
pub fn a16_hidden_units_leave_marker_and_failed_stays_visible<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(tool_session(), WIDTH, TALL);
    scenario.apply_setting(&ScenarioSetting::PresentationRules(HIDE_TOOLS.to_owned()));
    let frame = scenario.present();
    scenario.check(transcript_shows(&frame, "BETAOUT1"), || {
        "a rule hid the failed unit below summary".to_owned()
    });
    scenario.check(
        !transcript_shows(&frame, "ALPHAOUT1") && !transcript_shows(&frame, "GAMMAOUT1"),
        || "hidden succeeded units still show their output".to_owned(),
    );
    scenario.check(transcript_shows(&frame, "hidden"), || {
        "hidden units left no count marker".to_owned()
    });
}

/// A16: the floor applies after manual toggles: collapsing a failed unit by click stops at its
/// summary, and a click cannot collapse a unit awaiting a decision.
pub fn a16_floor_applies_after_manual_toggles<D: TranscriptScenarioDriver>() {
    let mut session = tool_session();
    push_permission(&mut session, "rm -rf PERMTARGET");
    let mut scenario = attach_all::<D>(session, WIDTH, TALL);
    let header = tool_header(&scenario, "BETAOUT1");
    scenario.click_at(header);
    let frame = scenario.present();
    scenario.check(
        transcript_shows(&frame, "BETAOUT1") && !transcript_shows(&frame, "BETAOUT2"),
        || "collapsing a failed unit did not stop at its summary".to_owned(),
    );
    let permission = find(&scenario, "Permission required");
    scenario.click_at(permission);
    let frame = scenario.present();
    scenario.check(transcript_shows(&frame, "PERMTARGET"), || {
        "a click collapsed a unit awaiting a decision".to_owned()
    });
}

/// A16: rules cannot collapse a unit awaiting a decision; its operation facts stay visible.
pub fn a16_awaiting_decision_stays_expanded_under_rules<D: TranscriptScenarioDriver>() {
    let mut session = tool_session();
    push_permission(&mut session, "rm -rf PERMTARGET");
    let mut scenario = attach_all::<D>(session, WIDTH, TALL);
    scenario.apply_setting(&ScenarioSetting::PresentationRules(
        COLLAPSE_TOOLS.to_owned(),
    ));
    let frame = scenario.present();
    scenario.check(!transcript_shows(&frame, "GAMMAOUT1"), || {
        "the collapse rule did not apply to succeeded tools".to_owned()
    });
    scenario.check(transcript_shows(&frame, "PERMTARGET"), || {
        "a rule collapsed the unit awaiting a decision".to_owned()
    });
}

// ---------------------------------------------------------------------------------------------
// Streaming Markdown (C38)
// ---------------------------------------------------------------------------------------------

/// C38: incomplete streaming Markdown keeps accepted text visible, never shows an unsafe
/// unfinished destination, and converges to the final projection.
pub fn c38_streaming_markdown_converges_to_final<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(ScenarioSession::with_turns(2, 1), WIDTH, HEIGHT);
    scenario.append_live_with(|session| {
        session.push_kind(SessionEventKind::AssistantDelta {
            text: "STREAMHEAD **boldpart".to_owned(),
        })
    });
    scenario.append_live_with(|session| {
        session.push_kind(SessionEventKind::AssistantDelta {
            text: " done** see [linktext](javascript:alert".to_owned(),
        })
    });
    scenario.advance(SETTLE);
    let streaming = scenario.present();
    scenario.check(
        transcript_shows(&streaming, "STREAMHEAD") && transcript_shows(&streaming, "linktext"),
        || "accepted streaming text is not visible".to_owned(),
    );
    scenario.check(!streaming.contains("javascript"), || {
        "an unsafe unfinished link destination is visible".to_owned()
    });
    scenario.append_live_with(|session| {
        session.push_kind(SessionEventKind::AssistantMessage {
            text: "STREAMHEAD **boldpart done** see [linktext](javascript:alert(1)) STREAMEND"
                .to_owned(),
        })
    });
    scenario.advance(SETTLE);
    let finished = scenario.present();
    scenario.check(
        transcript_shows(&finished, "boldpart done") && transcript_shows(&finished, "STREAMEND"),
        || "the finished response does not show its final text".to_owned(),
    );
    scenario.check(
        !finished.contains("**boldpart") && !finished.contains("javascript"),
        || "the finished response did not converge to the normal projection".to_owned(),
    );
}

// ---------------------------------------------------------------------------------------------
// Repaint scope and boundedness (D4, D6)
// ---------------------------------------------------------------------------------------------

/// D6/F5: a wheel scroll repaints the transcript viewport, not the whole surface.
pub fn d6_wheel_scroll_repaints_regionally<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(ScenarioSession::with_turns(20, 2), WIDTH, HEIGHT);
    let first = scenario.repaint();
    scenario.check(first.is_some_and(|repaint| repaint.full), || {
        format!("the first frame is not reported as a full repaint: {first:?}")
    });
    scenario.present();
    scenario.wheel(WheelDirection::Up, 1);
    scenario.present();
    let repaint = scenario.repaint();
    scenario.check(repaint.is_some_and(|repaint| !repaint.full), || {
        format!("a wheel scroll repainted the whole surface: {repaint:?}")
    });
}

/// D6/F5: typing in the composer repaints the composer, not the whole surface.
pub fn d6_composer_typing_repaints_regionally<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(ScenarioSession::with_turns(20, 2), WIDTH, HEIGHT);
    scenario.present();
    scenario.key("x");
    let frame = scenario.present();
    let repaint = scenario.repaint();
    scenario.check(locate(&frame, "x").is_some(), || {
        "the typed key is not visible".to_owned()
    });
    scenario.check(repaint.is_some_and(|repaint| !repaint.full), || {
        format!("composer typing repainted the whole surface: {repaint:?}")
    });
}

/// Layout work since the previous observation; fails when the driver cannot observe it.
#[track_caller]
fn layout_work<D: TranscriptScenarioDriver>(scenario: &mut Scenario<D>) -> LayoutWork {
    scenario
        .driver
        .layout_work()
        .unwrap_or_else(|| scenario.fail("driver: needs per-frame layout work counters"))
}

/// D4 (spec 9.2): steady scrolling over resident content does no parse, highlight,
/// projection, build or measurement work, in updates or frames.
pub fn d4_scrolling_does_no_layout_work<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(ScenarioSession::with_turns(20, 2), WIDTH, HEIGHT);
    let attached = layout_work(&mut scenario);
    scenario.check(attached.measurements > 0, || {
        format!("attaching measured nothing: {attached:?}")
    });
    for direction in [WheelDirection::Up, WheelDirection::Down] {
        for _ in 0..4 {
            scenario.wheel(direction, 1);
            scenario.present();
            scenario.advance(SETTLE);
            scenario.present();
            let work = layout_work(&mut scenario);
            scenario.check(work.is_zero(), || {
                format!("a scroll-only update or frame did layout work: {work:?}")
            });
        }
    }
}

/// D4 (spec 9.2): incorporating an older page measures only the page's new units, and the
/// frames around it do no work.
pub fn d4_prepend_measures_only_new_units<D: TranscriptScenarioDriver>() {
    let session = ScenarioSession::with_turns(60, 2);
    let mut scenario = Scenario::<D>::attach_latest(session, 20, WIDTH, HEIGHT);
    scenario.present();
    let _attached = layout_work(&mut scenario);
    if scenario.pending_of(HistoryRequestKind::Older).is_none()
        && scenario
            .wheel_until_request(WheelDirection::Up, HistoryRequestKind::Older, 200)
            .is_none()
    {
        scenario.fail("scrolling up never requested an older page");
    }
    let scrolled = layout_work(&mut scenario);
    scenario.check(scrolled.is_zero(), || {
        format!("scrolling up to the boundary did layout work: {scrolled:?}")
    });
    let response = scenario
        .complete_kind(HistoryRequestKind::Older)
        .unwrap_or_else(|| scenario.fail("no older page pending"));
    let new_items = u64::try_from(response.events.len()).unwrap_or(u64::MAX);
    let page = layout_work(&mut scenario);
    scenario.check(
        page.measurements > 0
            && page.measurements <= new_items
            && page.producer_builds == page.measurements,
        || format!("the page of {new_items} items did not measure only new units: {page:?}"),
    );
    scenario.present();
    let frame = layout_work(&mut scenario);
    scenario.check(frame.is_zero(), || {
        format!("the frame after the page did layout work: {frame:?}")
    });
}
