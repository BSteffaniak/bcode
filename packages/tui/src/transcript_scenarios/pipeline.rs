//! Frame pipeline, damage and transcript tooling scenarios.
//!
//! The pipeline requirements that the kept-behavior suites do not already
//! cover: the single-snapshot frame (F1, F4), paint purity and the frame clock (F2, F3),
//! regional and scroll damage (F5, D6, BMUX 11 and 15b), activity labels on accepted
//! invalidations (F7, D7), no layout work for focus and selection (D4), the structure inspector,
//! make-default, detail cycling and bulk disclosure (FO6, X2), Markdown images and Mermaid
//! diagrams replacing their placeholder rows, inline interaction placement (C37, D10) and inline
//! shell expansion (C39).
//!
//! Like the other suites, every scenario observes committed frames plus the driver's semantic
//! facts (damage, layout work, selection, clipboard). They are instantiated for the V2 engine in
//! the application only (`v2_pipeline` in `mod.rs`): they need the host's chrome, focus model,
//! command routes and the terminal's committed damage.

use std::time::Duration;

use bcode_session_models::SessionEventKind;

use super::Scenario;
use super::driver::{DamageObservation, ScenarioSetting, TranscriptScenarioDriver, WheelDirection};
use super::frame::{CellRect, FocusObservation, ScenarioFrame};
use super::interaction::{
    InteractionScenario, attach_all, find, locate, right_of, tool_header, tool_session,
    transcript_shows,
};
use super::session::{Marker, ScenarioSession};

const WIDTH: u16 = 80;
const HEIGHT: u16 = 20;
/// Tall enough to show a whole tool fixture without scrolling.
const TALL: u16 = 40;
/// Long enough for any motion (scroll animation, reveal, glow) to settle.
const SETTLE: Duration = Duration::from_secs(2);
/// Columns past a located token that reach the end of its line.
const LINE_END: u16 = 40;
/// Spinner glyphs of running units (`bcode_tui_transcript` paint slots).
const SPINNER: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

// ---------------------------------------------------------------------------------------------
// Observation helpers
// ---------------------------------------------------------------------------------------------

/// Damage of the last presented frame; fails when the driver cannot observe it.
#[track_caller]
fn damage<D: TranscriptScenarioDriver>(scenario: &Scenario<D>) -> DamageObservation {
    scenario
        .driver
        .last_damage()
        .unwrap_or_else(|| scenario.fail("driver: needs committed damage observation (15b)"))
}

/// The transcript region of `frame`: the viewport plus the latest-bar row below it.
const fn transcript_region(frame: &ScenarioFrame) -> CellRect {
    let mut region = frame.transcript;
    if let Some(bar) = frame.latest_bar
        && bar.y == region.y.saturating_add(region.height)
    {
        region.height = region.height.saturating_add(bar.height);
    }
    region
}

/// Layout work since the previous observation; fails when the driver cannot observe it.
#[track_caller]
fn layout_work<D: TranscriptScenarioDriver>(
    scenario: &mut Scenario<D>,
) -> super::driver::LayoutWork {
    scenario
        .driver
        .layout_work()
        .unwrap_or_else(|| scenario.fail("driver: needs per-frame layout work counters"))
}

/// Rows of `frame` (whole terminal) containing `needle`.
fn rows_with(frame: &ScenarioFrame, needle: &str) -> Vec<u16> {
    frame
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains(needle))
        .filter_map(|(y, _)| u16::try_from(y).ok())
        .collect()
}

/// The row of the transcript that shows a spinner glyph, with the glyph.
fn spinner_in(frame: &ScenarioFrame) -> Option<(usize, char)> {
    frame
        .transcript_rows()
        .iter()
        .enumerate()
        .find_map(|(y, row)| {
            row.chars()
                .find(|character| SPINNER.contains(character))
                .map(|glyph| (y, glyph))
        })
}

/// Append a `read` call that is still running (no result recorded).
fn push_running_tool(session: &mut ScenarioSession, call: &str, target: &str) {
    session.push_kind(SessionEventKind::ToolCallRequested {
        tool_call_id: call.to_owned(),
        producer_plugin_id: None,
        tool_name: "read".to_owned(),
        arguments_json: serde_json::json!({ "path": target }).to_string(),
        working_directory: None,
    });
    session.push_kind(SessionEventKind::ToolInvocationLifecycle {
        event: bcode_session_models::ToolInvocationLifecycleEvent {
            invocation_id: call.to_owned(),
            sequence: 1,
            stage: bcode_session_models::ToolInvocationLifecycleStage::Started,
            message: None,
            metadata: serde_json::Value::Null,
        },
    });
}

/// Append a finished `bash` call (a shell screen, C39) whose output has `lines` lines
/// `SHELLnnn`.
fn push_shell(session: &mut ScenarioSession, call: &str, lines: u32) {
    session.push_kind(SessionEventKind::ToolCallRequested {
        tool_call_id: call.to_owned(),
        producer_plugin_id: None,
        tool_name: "bash".to_owned(),
        arguments_json: serde_json::json!({ "command": "seq SHELLCMD" }).to_string(),
        working_directory: None,
    });
    let output = (0..lines)
        .map(|line| format!("SHELL{line:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    session.push_kind(SessionEventKind::ToolInvocationResultRecorded {
        record: bcode_session_models::ToolInvocationResultRecord {
            invocation_id: call.to_owned(),
            model_output: output,
            is_error: false,
            presentation: None,
            result: None,
            content: Vec::new(),
        },
    });
}

/// Append a running `ask` call with an unresolved, required question exchange
/// ([`EXCHANGE_ID`]).
fn push_question(session: &mut ScenarioSession) {
    session.push_kind(SessionEventKind::ToolCallRequested {
        tool_call_id: "call-ask".to_owned(),
        producer_plugin_id: None,
        tool_name: "ask".to_owned(),
        arguments_json: "{}".to_owned(),
        working_directory: None,
    });
    session.push_kind(SessionEventKind::ToolExchangeRequested {
        request: bcode_session_models::ToolExchangeRequest {
            invocation_id: "call-ask".to_owned(),
            exchange_id: EXCHANGE_ID.to_owned(),
            producer_id: "scenario.producer".to_owned(),
            schema: "scenario.question/schema".to_owned(),
            schema_version: 1,
            payload: serde_json::json!({ "question": "QUESTIONTEXT" }),
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        },
    });
}

/// Append an assistant message with `text` (Markdown).
fn push_markdown(session: &mut ScenarioSession, text: &str) {
    session.push_kind(SessionEventKind::AssistantMessage {
        text: text.to_owned(),
    });
}

// ---------------------------------------------------------------------------------------------
// Single snapshot, pure paint and the frame clock (F1-F4, F7, D7)
// ---------------------------------------------------------------------------------------------

/// F1/F4/B3/B12: one snapshot drives cells, hit regions and the focus highlight. The focused
/// unit's highlighted rows are the rows its header is painted on, and a click on that painted
/// header toggles exactly that unit, before and after the view scrolls.
pub fn f1_cells_hits_and_focus_come_from_one_snapshot<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(tool_session(), WIDTH, TALL);
    focus_tool(&mut scenario, "call-gamma");
    let frame = scenario.last_frame().clone();
    let header = tool_header(&scenario, "GAMMAOUT1");
    let extent = scenario
        .driver
        .focused_extent()
        .unwrap_or_else(|| scenario.fail("no focused target"));
    let header_row = i64::from(header.y) - i64::from(frame.transcript.y);
    scenario.check(extent.start == header_row, || {
        format!("the focus highlight starts at {extent:?}, the header is painted at {header_row}")
    });
    scenario.click_at(header);
    let collapsed = scenario.present();
    scenario.check(
        !transcript_shows(&collapsed, "GAMMAOUT1") && transcript_shows(&collapsed, "ALPHAOUT1"),
        || "the click on the painted header did not toggle exactly that unit".to_owned(),
    );
    // Scroll by one wheel tick: the painted rows, the hit regions and the highlight move
    // together.
    scenario.key("escape");
    scenario.present();
    scenario.wheel(WheelDirection::Up, 1);
    scenario.present();
    scenario.advance(SETTLE);
    scenario.present();
    let header = find(&scenario, "▸ Tool result");
    scenario.click_at(header);
    let expanded = scenario.present();
    let row = &expanded.rows[usize::from(header.y)];
    scenario.check(row.contains("▾ Tool result"), || {
        format!("after scrolling, the click on the painted header did not reach its unit: {row:?}")
    });
}

/// F2/F3/CK1: drawing reads no clock and mutates nothing. Presenting the same state again,
/// also after the host clock moved on without an accepted activity frame, repaints identical
/// cells (spinner and elapsed labels included), does no layout work and moves nothing.
pub fn f2_f3_presentation_is_pure_and_reads_no_clock<D: TranscriptScenarioDriver>() {
    let mut session = tool_session();
    push_running_tool(&mut session, "call-running", "delta/four.rs");
    let mut scenario = attach_all::<D>(session, WIDTH, TALL);
    let first = scenario.present();
    scenario.check(spinner_in(&first).is_some(), || {
        "the running tool shows no spinner".to_owned()
    });
    let _attached = layout_work(&mut scenario);
    for _ in 0..3 {
        scenario.advance(Duration::from_millis(700));
        let again = scenario.present();
        scenario.check(again.rows == first.rows, || {
            "a presentation without an accepted change painted different cells".to_owned()
        });
        let work = layout_work(&mut scenario);
        scenario.check(work.is_zero(), || {
            format!("a repeated presentation did layout work: {work:?}")
        });
        let repaint = scenario.repaint();
        scenario.check(
            repaint.is_some_and(|repaint| repaint.changed_cells == 0),
            || format!("a repeated presentation changed terminal cells: {repaint:?}"),
        );
    }
}

/// F7/D7: spinner and elapsed labels advance only when the host shell accepts an activity
/// frame. The accepted frame repaints the transcript alone, without layout work.
pub fn d7_activity_labels_advance_on_accepted_frames<D: TranscriptScenarioDriver>() {
    let mut session = tool_session();
    push_running_tool(&mut session, "call-running", "delta/four.rs");
    let mut scenario = attach_all::<D>(session, WIDTH, TALL);
    let before = scenario.present();
    let (row, glyph) =
        spinner_in(&before).unwrap_or_else(|| scenario.fail("the running tool shows no spinner"));
    scenario.advance(SETTLE);
    let waited = scenario.present();
    scenario.check(spinner_in(&waited) == Some((row, glyph)), || {
        "the spinner advanced without an accepted activity frame".to_owned()
    });
    let _settled = layout_work(&mut scenario);
    scenario.driver.accept_activity_frame();
    let advanced = scenario.present();
    let next = spinner_in(&advanced);
    scenario.check(
        next.is_some_and(|(y, next)| y == row && next != glyph),
        || format!("an accepted activity frame did not advance the spinner: {glyph:?} -> {next:?}"),
    );
    let work = layout_work(&mut scenario);
    scenario.check(work.is_zero(), || {
        format!("advancing the spinner did layout work: {work:?}")
    });
    let repaint = damage(&scenario);
    let region = transcript_region(&advanced);
    scenario.check(repaint.within(region), || {
        format!("advancing the spinner repainted outside the transcript {region:?}: {repaint:?}")
    });
}

// ---------------------------------------------------------------------------------------------
// Regional and scroll damage (F5, D6, BMUX 11, 15b)
// ---------------------------------------------------------------------------------------------

/// F5/D6/BMUX 11: a wheel tick is scroll damage of the transcript viewport by the wheel rows,
/// presented with a terminal scroll region, and repaints nothing outside the transcript.
pub fn d6_wheel_tick_is_scroll_damage_of_the_viewport<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(ScenarioSession::with_turns(20, 2), WIDTH, HEIGHT);
    scenario.check(damage(&scenario).is_full(), || {
        "the first frame is not full damage".to_owned()
    });
    scenario.present();
    scenario.configure(&ScenarioSetting::ReducedMotion(true));
    scenario.present();
    let rows = i64::from(scenario.driver.wheel_rows());
    for (direction, shift) in [(WheelDirection::Up, -rows), (WheelDirection::Down, rows)] {
        let before = scenario.last_frame().clone();
        scenario.wheel(direction, 1);
        let frame = scenario.present();
        let observed = damage(&scenario);
        let viewport = frame.transcript;
        scenario.check(
            matches!(&observed, DamageObservation::Scroll { region, rows, accelerated, .. }
                if *region == viewport && *rows == shift && *accelerated),
            || format!("a {direction:?} wheel tick is not an accelerated scroll of {viewport:?} by {shift}: {observed:?}"),
        );
        // Besides the transcript, only the status row below it repaints: it reports the
        // reading position, which the tick changed (C2), shown before or after it.
        let mut region = transcript_region(&frame);
        if [&frame, &before].iter().any(|shown| {
            shown
                .rows
                .get(usize::from(region.bottom()))
                .is_some_and(|row| row.contains("rows from bottom"))
        }) {
            region.height += 1;
        }
        scenario.check(observed.within(region), || {
            format!("a wheel tick repainted outside the transcript and status row {region:?}: {observed:?}")
        });
    }
}

/// F5/D6: focus traversal, a disclosure toggle and a drag selection repaint the transcript
/// region only, composer typing repaints outside it, and entering focus mode (status hints)
/// and a resize repaint fully.
pub fn d6_transcript_changes_repaint_the_transcript_region<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(tool_session(), WIDTH, TALL);
    scenario.key("alt+u");
    scenario.present();
    scenario.check(damage(&scenario).is_full(), || {
        "entering focus mode (status hints) did not repaint fully".to_owned()
    });
    for (step, key) in [("focus traversal", "k"), ("disclosure toggle", "enter")] {
        scenario.key(key);
        let frame = scenario.present();
        let observed = damage(&scenario);
        let region = transcript_region(&frame);
        scenario.check(!observed.is_full() && observed.within(region), || {
            format!("{step} repainted outside the transcript {region:?}: {observed:?}")
        });
    }
    scenario.key("escape");
    scenario.present();
    let from = find(&scenario, "ALPHAOUT1");
    let to = find(&scenario, "ALPHAOUT2");
    scenario.input(super::driver::ScenarioInput::Press { at: from });
    scenario.input(super::driver::ScenarioInput::DragTo {
        at: right_of(to, LINE_END),
    });
    let frame = scenario.present();
    let observed = damage(&scenario);
    let region = transcript_region(&frame);
    scenario.check(!observed.is_full() && observed.within(region), || {
        format!("a selection drag repainted outside the transcript {region:?}: {observed:?}")
    });
    scenario.input(super::driver::ScenarioInput::Release {
        at: right_of(to, LINE_END),
    });
    scenario.present();
    scenario.key("x");
    let typed = scenario.present();
    let observed = damage(&scenario);
    let region = transcript_region(&typed);
    scenario.check(!observed.is_full() && !observed.touches(region), || {
        format!("composer typing repainted the transcript {region:?}: {observed:?}")
    });
    scenario.input(super::driver::ScenarioInput::Resize {
        width: WIDTH - 10,
        height: TALL,
    });
    scenario.present();
    scenario.check(damage(&scenario).is_full(), || {
        "a resize did not repaint fully".to_owned()
    });
}

/// D4: focus traversal and drag selection over resident content are paint-only
/// (L5): no parse, highlight, projection, build or measurement work.
pub fn d4_focus_and_selection_do_no_layout_work<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(tool_session(), WIDTH, TALL);
    let _attached = layout_work(&mut scenario);
    scenario.key("alt+u");
    scenario.present();
    for key in ["k", "k", "j", "k"] {
        scenario.key(key);
        scenario.present();
        let work = layout_work(&mut scenario);
        scenario.check(work.is_zero(), || {
            format!("focus traversal ({key:?}) did layout work: {work:?}")
        });
    }
    scenario.key("escape");
    scenario.present();
    let _left = layout_work(&mut scenario);
    let from = find(&scenario, "ALPHAOUT1");
    let to = find(&scenario, "GAMMAOUT1");
    scenario.drag_select(from, right_of(to, LINE_END));
    scenario.present();
    let work = layout_work(&mut scenario);
    scenario.check(work.is_zero(), || {
        format!("a drag selection did layout work: {work:?}")
    });
    scenario.check(scenario.selected().is_some(), || {
        "the drag selected nothing".to_owned()
    });
}

// ---------------------------------------------------------------------------------------------
// Transcript tooling (FO6, X2)
// ---------------------------------------------------------------------------------------------

/// Enter unit focus and walk up (`k`) until the header of tool call `call` is focused.
#[track_caller]
fn focus_tool<D: TranscriptScenarioDriver>(scenario: &mut Scenario<D>, call: &str) {
    if scenario.last_frame().focus == FocusObservation::Composer {
        scenario.key("alt+u");
        scenario.present();
    }
    let header = |frame: &ScenarioFrame| {
        frame.focused_target.as_deref().is_some_and(|target| {
            target.contains(&format!("\"tool:{call}\"")) && target.contains("local: 0 ")
        })
    };
    for _ in 0..16 {
        if header(scenario.last_frame()) {
            return;
        }
        scenario.key("k");
        scenario.present();
    }
    scenario.fail(&format!("k never focused the header of {call}"));
}

/// FO6: `i` opens the structure inspector for the focused unit, the inspector follows focus,
/// shift+Down/Up scroll it, and Escape closes it before it leaves focus mode (FO3).
pub fn fo6_inspector_follows_focus_scrolls_and_closes_first<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(tool_session(), WIDTH, HEIGHT);
    focus_tool(&mut scenario, "call-gamma");
    scenario.key("i");
    let opened = scenario.present();
    scenario.check(opened.contains("Inspector · unit:tool:call-gamma"), || {
        "i did not open the inspector for the focused unit".to_owned()
    });
    focus_tool(&mut scenario, "call-beta");
    let followed = scenario.last_frame().clone();
    scenario.check(
        followed.contains("Inspector · unit:tool:call-beta")
            && !followed.contains("unit:tool:call-gamma"),
        || "the inspector did not follow focus to the previous unit".to_owned(),
    );
    scenario.key("shift+down");
    let scrolled = scenario.present();
    scenario.check(scrolled.rows != followed.rows, || {
        "shift+Down did not scroll the inspector".to_owned()
    });
    scenario.check(scrolled.focused_target == followed.focused_target, || {
        "shift+Down moved transcript focus".to_owned()
    });
    scenario.key("shift+up");
    let back = scenario.present();
    scenario.check(back.rows == followed.rows, || {
        "shift+Up did not scroll the inspector back".to_owned()
    });
    scenario.key("escape");
    let closed = scenario.present();
    scenario.check(!closed.contains("Inspector · unit:"), || {
        "Escape did not close the inspector".to_owned()
    });
    scenario.check(
        closed.focus == FocusObservation::Transcript { units: true },
        || "the Escape that closed the inspector also left focus mode".to_owned(),
    );
    scenario.key("escape");
    let left = scenario.present();
    scenario.check(left.focus == FocusObservation::Composer, || {
        "the second Escape did not leave focus mode".to_owned()
    });
}

/// FO6/A3: `m` makes the focused tool unit's disclosure the default for its tool. The rule is
/// persisted to user state and applies to every unit of that tool, also to later ones, while
/// other tools keep theirs.
pub fn fo6_make_default_applies_the_focused_disclosure_to_its_tool<D: TranscriptScenarioDriver>() {
    let mut session = tool_session();
    session.push_kind(SessionEventKind::ToolCallRequested {
        tool_call_id: "call-list".to_owned(),
        producer_plugin_id: None,
        tool_name: "list".to_owned(),
        arguments_json: serde_json::json!({ "path": "list/dir" }).to_string(),
        working_directory: None,
    });
    session.push_kind(SessionEventKind::ToolInvocationResultRecorded {
        record: bcode_session_models::ToolInvocationResultRecord {
            invocation_id: "call-list".to_owned(),
            model_output: "LISTOUT1".to_owned(),
            is_error: false,
            presentation: None,
            result: None,
            content: Vec::new(),
        },
    });
    let mut scenario = attach_all::<D>(session, WIDTH, TALL);
    focus_tool(&mut scenario, "call-gamma");
    scenario.key("h");
    let collapsed = scenario.present();
    scenario.check(
        !transcript_shows(&collapsed, "GAMMAOUT1") && transcript_shows(&collapsed, "ALPHAOUT1"),
        || "h did not collapse only the focused read unit".to_owned(),
    );
    scenario.key("m");
    let saved = scenario.present();
    scenario.check(
        !transcript_shows(&saved, "ALPHAOUT1") && !transcript_shows(&saved, "GAMMAOUT1"),
        || "the read default did not collapse every read unit".to_owned(),
    );
    scenario.check(transcript_shows(&saved, "LISTOUT1"), || {
        "the read default changed another tool".to_owned()
    });
    scenario.key("escape");
    scenario.append_live_with(|session| {
        session.push_kind(SessionEventKind::ToolCallRequested {
            tool_call_id: "call-delta".to_owned(),
            producer_plugin_id: None,
            tool_name: "read".to_owned(),
            arguments_json: serde_json::json!({ "path": "delta/four.rs" }).to_string(),
            working_directory: None,
        })
    });
    scenario.append_live_with(|session| {
        session.push_kind(SessionEventKind::ToolInvocationResultRecorded {
            record: bcode_session_models::ToolInvocationResultRecord {
                invocation_id: "call-delta".to_owned(),
                model_output: "DELTAOUT1".to_owned(),
                is_error: false,
                presentation: None,
                result: None,
                content: Vec::new(),
            },
        })
    });
    scenario.present();
    scenario.advance(SETTLE);
    let later = scenario.present();
    let collapsed_reads = rows_with(&later, "▸ Tool result · read · finished").len();
    scenario.check(
        collapsed_reads == 3 && !transcript_shows(&later, "DELTAOUT1"),
        || {
            format!(
                "a later read unit did not take the saved default ({collapsed_reads} collapsed)"
            )
        },
    );
}

/// FO6: `alt+d` outside focus mode cycles every unit collapsed, summary, expanded; the
/// visibility floor keeps the failed unit at least at summary (A16).
pub fn fo6_detail_cycle_applies_to_every_unit<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(tool_session(), WIDTH, TALL);
    let mut seen = Vec::new();
    for _ in 0..3 {
        scenario.key("alt+d");
        let frame = scenario.present();
        seen.push((
            transcript_shows(&frame, "ALPHAOUT1"),
            transcript_shows(&frame, "GAMMAOUT1"),
            transcript_shows(&frame, "BETAOUT1"),
        ));
    }
    scenario.check(
        seen[0].0 == seen[0].1 && seen[1].0 == seen[1].1 && seen[2].0 == seen[2].1,
        || format!("a detail cycle did not apply to every unit alike: {seen:?}"),
    );
    scenario.check(!seen[0].0 && seen[2].0, || {
        format!("three detail cycles did not go collapsed .. expanded: {seen:?}")
    });
    scenario.check(seen.iter().all(|step| step.2), || {
        format!("the failed unit fell below summary: {seen:?}")
    });
}

/// FO6: the bulk disclosure routes collapse and expand every unit from the composer, apply to
/// the focused subtree in focus mode, and reset restores the configured disclosure.
pub fn fo6_bulk_disclosure_routes_apply_to_units_or_the_focused_subtree<
    D: TranscriptScenarioDriver,
>() {
    let mut scenario = attach_all::<D>(tool_session(), WIDTH, TALL);
    scenario.driver.run_command("tools.collapseAll");
    let collapsed = scenario.present();
    scenario.check(
        !transcript_shows(&collapsed, "ALPHAOUT1") && !transcript_shows(&collapsed, "GAMMAOUT1"),
        || "tools.collapseAll did not collapse every unit".to_owned(),
    );
    scenario.check(collapsed.contains("All units collapsed"), || {
        "tools.collapseAll reported no status".to_owned()
    });
    scenario.driver.run_command("tools.expandAll");
    let expanded = scenario.present();
    scenario.check(
        transcript_shows(&expanded, "ALPHAOUT1") && transcript_shows(&expanded, "GAMMAOUT1"),
        || "tools.expandAll did not expand every unit".to_owned(),
    );
    focus_tool(&mut scenario, "call-gamma");
    scenario.driver.run_command("tools.collapseAll");
    let focused = scenario.present();
    scenario.check(
        !transcript_shows(&focused, "GAMMAOUT1") && transcript_shows(&focused, "ALPHAOUT1"),
        || "tools.collapseAll in focus mode did not collapse only the focused subtree".to_owned(),
    );
    scenario.key("escape");
    scenario.present();
    scenario.driver.run_command("tools.resetDisclosure");
    let reset = scenario.present();
    scenario.check(
        transcript_shows(&reset, "ALPHAOUT1") && transcript_shows(&reset, "GAMMAOUT1"),
        || "tools.resetDisclosure did not restore the configured disclosure".to_owned(),
    );
}

// ---------------------------------------------------------------------------------------------
// Markdown visuals
// ---------------------------------------------------------------------------------------------

/// Markdown images replace their placeholder rows with the terminal's fallback notice when the
/// terminal cannot show images, keeping the reserved row count, so text below stays put.
pub fn visuals_image_notice_replaces_the_placeholder<D: TranscriptScenarioDriver>() {
    let mut session = ScenarioSession::with_turns(1, 1);
    let sequence = session.next_sequence();
    push_markdown(
        &mut session,
        &format!(
            "{}\n\n![PICALT](https://example.com/pic.png)\n\n{}",
            Marker::new(sequence, 0).token(),
            Marker::new(sequence, 1).token()
        ),
    );
    let mut scenario = attach_all::<D>(session, WIDTH, TALL);
    scenario.advance(SETTLE);
    let frame = scenario.present();
    scenario.check(frame.contains("PICALT") && frame.contains("image"), || {
        "the image placeholder was not replaced by its notice".to_owned()
    });
    let top = frame.row_of(Marker::new(sequence, 0));
    let bottom = frame.row_of(Marker::new(sequence, 1));
    scenario.check(
        top.zip(bottom)
            .is_some_and(|(top, bottom)| bottom > top + 1),
        || format!("the image rows collapsed: {top:?} .. {bottom:?}"),
    );
}

/// Mermaid diagrams on a terminal without image support keep their source-backed code block
/// instead of blank placeholder rows.
pub fn visuals_mermaid_without_images_keeps_its_source<D: TranscriptScenarioDriver>() {
    let mut session = ScenarioSession::with_turns(1, 1);
    let sequence = session.next_sequence();
    push_markdown(
        &mut session,
        &format!(
            "{}\n\n```mermaid\ngraph TD\n  MERMAIDA --> MERMAIDB\n```\n\n{}",
            Marker::new(sequence, 0).token(),
            Marker::new(sequence, 1).token()
        ),
    );
    let mut scenario = attach_all::<D>(session, WIDTH, TALL);
    scenario.advance(SETTLE);
    let frame = scenario.present();
    scenario.check(
        frame.contains("MERMAIDA") && frame.contains("MERMAIDB"),
        || "the Mermaid source is not shown".to_owned(),
    );
}

// ---------------------------------------------------------------------------------------------
// Inline interactions (C37, D10) and inline shell expansion (C39)
// ---------------------------------------------------------------------------------------------

/// Exchange (interaction) id of the fixture's unresolved question.
const EXCHANGE_ID: &str = "exchange-question";
/// Text the interaction surface double paints on each of its rows.
const SURFACE_TEXT: &str = "SURFACEROW";

/// C37/D10: with `interactions.placement = transcript` the active interaction's surface is
/// placed inside the transcript over rows its unit reserves; it scrolls with the transcript,
/// leaves nothing behind when scrolled away, and the interaction-focus key restores it into
/// view.
pub fn c37_inline_interaction_scrolls_with_the_transcript_and_restores<
    D: TranscriptScenarioDriver,
>() {
    let mut session = ScenarioSession::with_turns(12, 2);
    push_question(&mut session);
    let mut scenario = attach_all::<D>(session, WIDTH, HEIGHT);
    scenario
        .driver
        .open_interaction(EXCHANGE_ID, 3, SURFACE_TEXT);
    scenario.present();
    scenario.advance(SETTLE);
    let placed = scenario.present();
    let rows = rows_with(&placed, SURFACE_TEXT);
    scenario.check(rows.len() == 3, || {
        format!("the surface does not show its three rows: {rows:?}")
    });
    scenario.check(
        rows.iter().all(|row| placed.transcript.contains_row(*row)),
        || {
            format!(
                "the surface is not inside the transcript {:?}: {rows:?}",
                placed.transcript
            )
        },
    );
    let header = locate(&placed, "ask · requested")
        .unwrap_or_else(|| scenario.fail("the interaction unit is not visible"));
    scenario.check(rows.iter().all(|row| *row > header.y), || {
        "the surface is not below its interaction unit's header".to_owned()
    });
    scenario.wheel(WheelDirection::Up, 1);
    scenario.present();
    scenario.advance(SETTLE);
    let moved = scenario.present();
    let shifted = rows_with(&moved, SURFACE_TEXT);
    let delta = moved.shift_from(&placed);
    scenario.check(
        delta.is_some_and(|delta| {
            shifted
                .iter()
                .zip(&rows)
                .all(|(after, before)| i32::from(*after) - i32::from(*before) == delta)
        }),
        || format!("the surface did not scroll with the transcript ({delta:?}): {rows:?} -> {shifted:?}"),
    );
    scenario.wheel(WheelDirection::Up, 20);
    scenario.present();
    scenario.advance(SETTLE);
    let away = scenario.present();
    scenario.check(rows_with(&away, SURFACE_TEXT).is_empty(), || {
        "the surface stayed on screen after its unit scrolled away".to_owned()
    });
    scenario.key("ctrl+i");
    scenario.present();
    scenario.advance(SETTLE);
    let restored = scenario.present();
    let back = rows_with(&restored, SURFACE_TEXT);
    scenario.check(
        !back.is_empty()
            && back
                .iter()
                .all(|row| restored.transcript.contains_row(*row)),
        || format!("the interaction-focus key did not restore the surface into view: {back:?}"),
    );
}

/// Shell output lines (`SHELLnnn`) shown anywhere in the transcript, found by scrolling from
/// the top of the resident content to its bottom one wheel tick per frame.
fn shell_lines_seen<D: TranscriptScenarioDriver>(
    scenario: &mut Scenario<D>,
    lines: u32,
) -> std::collections::BTreeSet<u32> {
    scenario.wheel(WheelDirection::Up, 100);
    scenario.present();
    scenario.advance(SETTLE);
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..80 {
        let frame = scenario.present();
        seen.extend((0..lines).filter(|line| frame.contains(&format!("SHELL{line:03}"))));
        scenario.wheel(WheelDirection::Down, 1);
        scenario.advance(SETTLE);
    }
    scenario.present();
    seen
}

/// C39: `e` on a focused shell unit opens its screen inline with up to 128 rows (its newest
/// output, as a terminal shows it), and `e` again collapses it back to the preview.
pub fn c39_shell_expansion_opens_up_to_128_rows<D: TranscriptScenarioDriver>() {
    const LINES: u32 = 200;
    let mut session = ScenarioSession::with_turns(1, 1);
    push_shell(&mut session, "call-shell", LINES);
    let mut scenario = attach_all::<D>(session, WIDTH, HEIGHT);
    let preview = shell_lines_seen(&mut scenario, LINES);
    scenario.check(preview.len() < 128, || {
        format!("the shell preview already shows {} lines", preview.len())
    });
    focus_tool(&mut scenario, "call-shell");
    scenario.key("e");
    let expanded_frame = scenario.present();
    scenario.check(
        expanded_frame.contains("Shell screen expanded inline"),
        || "e did not report the inline shell expansion".to_owned(),
    );
    scenario.key("escape");
    let expanded = shell_lines_seen(&mut scenario, LINES);
    let contiguous = expanded
        .first()
        .zip(expanded.last())
        .is_some_and(|(first, last)| {
            usize::try_from(last - first + 1).ok() == Some(expanded.len())
        });
    scenario.check(
        expanded.len() == 128 && contiguous && expanded.contains(&(LINES - 1)),
        || {
            format!(
                "the expanded shell screen does not show its newest 128 lines ({} lines): {expanded:?}",
                expanded.len()
            )
        },
    );
    focus_tool(&mut scenario, "call-shell");
    scenario.key("e");
    scenario.present();
    scenario.key("escape");
    let collapsed = shell_lines_seen(&mut scenario, LINES);
    scenario.check(collapsed == preview, || {
        format!(
            "e again did not collapse the shell screen to its preview: {} lines",
            collapsed.len()
        )
    });
}

/// C39: the inline shell screen scrolls horizontally on its own (local horizontal scroll), so
/// the end of a line wider than the transcript becomes visible without wrapping it.
pub fn c39_shell_screen_scrolls_horizontally<D: TranscriptScenarioDriver>() {
    let mut session = ScenarioSession::with_turns(1, 1);
    session.push_kind(SessionEventKind::ToolCallRequested {
        tool_call_id: "call-wide".to_owned(),
        producer_plugin_id: None,
        tool_name: "bash".to_owned(),
        arguments_json: serde_json::json!({ "command": "echo wide" }).to_string(),
        working_directory: None,
    });
    let wide = format!("WIDEHEAD{}WIDETAIL", "-".repeat(160));
    session.push_kind(SessionEventKind::ToolInvocationResultRecorded {
        record: bcode_session_models::ToolInvocationResultRecord {
            invocation_id: "call-wide".to_owned(),
            model_output: wide,
            is_error: false,
            presentation: None,
            result: None,
            content: Vec::new(),
        },
    });
    let mut scenario = attach_all::<D>(session, WIDTH, HEIGHT);
    focus_tool(&mut scenario, "call-wide");
    scenario.key("e");
    scenario.key("escape");
    let expanded = scenario.present();
    scenario.check(
        expanded.contains("WIDEHEAD") && !expanded.contains("WIDETAIL"),
        || "the expanded shell screen wraps or shows the end of its wide line".to_owned(),
    );
    let at = find(&scenario, "WIDEHEAD");
    for _ in 0..40 {
        scenario.driver.horizontal_wheel(true, at);
    }
    let scrolled = scenario.present();
    scenario.check(scrolled.contains("WIDETAIL"), || {
        "horizontal wheel input did not scroll the shell screen to the end of its line".to_owned()
    });
}
