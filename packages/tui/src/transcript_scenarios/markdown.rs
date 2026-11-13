//! Markdown contribution activation scenarios: `<details>` clicks,
//! the `tui.markdown.*` keys with footnote jumps, `#fragment` navigation, link opening and
//! destination copy, all observed in committed frames (status row included).
//!
//! Scenarios that reach the platform opener or the system clipboard rely on the host's opener,
//! which records destinations in test builds, and on the driver's scripted clipboard.

use std::time::Duration;

use bcode_session_models::SessionEventKind;

use super::Scenario;
use super::driver::CellPoint;
use super::driver::{ScenarioInput, TranscriptScenarioDriver};
use super::interaction::attach_all;
use super::session::ScenarioSession;

const WIDTH: u16 = 100;
/// Wide enough for the status row to show the long focus notice with its key hints.
const WIDE: u16 = 180;
const HEIGHT: u16 = 20;
/// Long enough for any motion to settle.
const SETTLE: Duration = Duration::from_secs(2);

/// A session of one question and one assistant message per entry of `answers`.
fn session(answers: &[String]) -> ScenarioSession {
    let mut session = ScenarioSession::new();
    for answer in answers {
        session.push_user(1);
        session.push_kind(SessionEventKind::AssistantMessage {
            text: answer.clone(),
        });
    }
    session
}

fn filler(lines: usize) -> String {
    (0..lines)
        .map(|line| format!("filler line {line}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Cell where `needle` starts in the last frame (cells measured with BMUX text width).
fn find<D: TranscriptScenarioDriver>(scenario: &Scenario<D>, needle: &str) -> CellPoint {
    scenario
        .last_frame()
        .rows
        .iter()
        .enumerate()
        .find_map(|(y, row)| {
            let byte = row.find(needle)?;
            Some(CellPoint {
                x: u16::try_from(bmux_tui::text_width::display_width(&row[..byte])).ok()?,
                y: u16::try_from(y).ok()?,
            })
        })
        .unwrap_or_else(|| scenario.fail(&format!("{needle:?} is not visible")))
}

fn click<D: TranscriptScenarioDriver>(scenario: &mut Scenario<D>, needle: &str) {
    let at = find(scenario, needle);
    scenario.input(ScenarioInput::Click { at });
    scenario.advance(SETTLE);
    scenario.present();
}

fn key<D: TranscriptScenarioDriver>(scenario: &mut Scenario<D>, name: &str) {
    scenario.key(name);
    scenario.advance(SETTLE);
    scenario.present();
}

#[track_caller]
fn expect_shown<D: TranscriptScenarioDriver>(scenario: &Scenario<D>, needle: &str) {
    let frame = scenario.last_frame();
    scenario.check(frame.contains(needle), || {
        format!("{needle:?} is not shown:\n{}", frame.text())
    });
}

#[track_caller]
fn expect_hidden<D: TranscriptScenarioDriver>(scenario: &Scenario<D>, needle: &str) {
    let frame = scenario.last_frame();
    scenario.check(!frame.contains(needle), || {
        format!("{needle:?} is shown:\n{}", frame.text())
    });
}

/// Clicking a `<details>` summary expands the block, clicking it again collapses it, with the
/// host's status messages.
pub fn md_details_click_toggles_the_block<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(
        session(&[
            "<details>\n<summary>MORESUMMARY</summary>\n\nHIDDENBODY\n\n</details>".to_owned(),
        ]),
        WIDTH,
        HEIGHT,
    );
    scenario.advance(SETTLE);
    scenario.present();
    expect_hidden(&scenario, "HIDDENBODY");
    click(&mut scenario, "MORESUMMARY");
    expect_shown(&scenario, "HIDDENBODY");
    expect_shown(&scenario, "Details expanded");
    click(&mut scenario, "MORESUMMARY");
    expect_hidden(&scenario, "HIDDENBODY");
    expect_shown(&scenario, "Details collapsed");
}

/// `tui.markdown.focusNext` focuses the first contribution in view and names the keys;
/// `tui.markdown.activate` on a footnote reference reveals its definition, and on the
/// definition returns to the reference.
pub fn md_keys_jump_between_footnote_reference_and_definition<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(
        session(&[format!(
            "CLAIMTEXT[^n] here.\n\n{}\n\n[^n]: NOTEBODY",
            filler(24)
        )]),
        WIDE,
        HEIGHT,
    );
    key(&mut scenario, "ctrl+home");
    expect_shown(&scenario, "CLAIMTEXT");
    expect_hidden(&scenario, "NOTEBODY");
    key(&mut scenario, "ctrl+tab");
    expect_shown(&scenario, "Markdown contribution focused;");
    key(&mut scenario, "alt+enter");
    expect_shown(&scenario, "NOTEBODY");
    expect_shown(&scenario, "Footnote definition focused; activate to return");
    key(&mut scenario, "alt+enter");
    expect_shown(&scenario, "CLAIMTEXT");
    expect_shown(&scenario, "Returned to footnote reference");
}

/// Clicking a `#fragment` link brings its section into view.
pub fn md_fragment_link_navigates_to_its_section<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(
        session(&[format!(
            "[JUMPLINK](#target-section)\n\n{}\n\n## Target section\n\nSECTIONBODY",
            filler(24)
        )]),
        WIDTH,
        HEIGHT,
    );
    key(&mut scenario, "ctrl+home");
    expect_hidden(&scenario, "SECTIONBODY");
    click(&mut scenario, "JUMPLINK");
    expect_shown(&scenario, "SECTIONBODY");
    expect_shown(&scenario, "Navigated to Markdown section");
}

/// Clicking a web link opens it through the safe activation boundary, and the copy key copies
/// the focused link's destination through the transcript clipboard path, with the host's
/// messages.
pub fn md_link_opens_and_its_destination_copies<D: TranscriptScenarioDriver>() {
    let mut scenario = attach_all::<D>(
        session(&["read [WEBLINK](https://example.com/docs) now".to_owned()]),
        WIDTH,
        HEIGHT,
    );
    scenario.advance(SETTLE);
    scenario.present();
    click(&mut scenario, "WEBLINK");
    expect_shown(&scenario, "Opened Markdown destination");
    key(&mut scenario, "ctrl+alt+c");
    expect_shown(&scenario, "Copied Markdown destination");
    let copied = scenario.driver.clipboard_writes();
    scenario.check(copied == ["https://example.com/docs"], || {
        format!("clipboard writes: {copied:?}")
    });
}
