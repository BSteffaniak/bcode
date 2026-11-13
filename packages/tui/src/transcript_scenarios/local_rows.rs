//! Process-local transcript rows: pending submissions and
//! process-local notices of the host shell are visible in committed frames, in presentation
//! order, and leave when the shell forgets them.
//!
//! Observations are committed frames only: rows are found by their visible text (the pending
//! heading `You · sending`, the submitted prompt, notice text) and by scenario markers.

use std::time::Duration;

use super::Scenario;
use super::driver::{ResidentSeed, ScenarioSetup, SubmissionOutcome, TranscriptScenarioDriver};
use super::frame::ScenarioFrame;
use super::session::{Marker, ScenarioSession};

const WIDTH: u16 = 80;
const HEIGHT: u16 = 24;
/// Long enough for any motion and the manual-scroll grace to settle.
const SETTLE: Duration = Duration::from_secs(2);
/// Distinctive composer text, observable in frames without marker tokens.
const PROMPT: &str = "zqx local prompt";
/// Heading of a staged submission.
const SENDING: &str = "You · sending";
/// Server error code of the rejected submission the user reported.
const REJECTION: &str = "managed_configuration_unavailable";

/// Start `session` with `resident` and present a settled frame.
fn start<D: TranscriptScenarioDriver>(
    session: ScenarioSession,
    resident: ResidentSeed,
) -> Scenario<D> {
    let mut scenario = Scenario::<D>::start(ScenarioSetup {
        session,
        resident,
        width: WIDTH,
        height: HEIGHT,
    });
    settle(&mut scenario);
    scenario
}

/// A short session that fits the viewport, fully resident.
fn short<D: TranscriptScenarioDriver>() -> Scenario<D> {
    start(ScenarioSession::with_turns(1, 2), ResidentSeed::Everything)
}

/// Present, let motion and grace settle, and present again.
fn settle<D: TranscriptScenarioDriver>(scenario: &mut Scenario<D>) -> ScenarioFrame {
    scenario.present();
    scenario.advance(SETTLE);
    scenario.present()
}

/// Type `text` into the composer.
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

/// Type [`PROMPT`] and submit it.
fn submit<D: TranscriptScenarioDriver>(scenario: &mut Scenario<D>) {
    type_text(scenario, PROMPT);
    scenario.key("enter");
}

/// Transcript row of the first row containing `needle`.
fn row_with(frame: &ScenarioFrame, needle: &str) -> Option<usize> {
    frame
        .transcript_rows()
        .iter()
        .position(|row| row.contains(needle))
}

/// Newest marker of the canonical session.
fn newest_marker<D: TranscriptScenarioDriver>(scenario: &Scenario<D>) -> Marker {
    let newest = scenario
        .session()
        .last_sequence()
        .expect("session has events");
    scenario
        .session()
        .last_marker(newest)
        .expect("newest event is a scenario item")
}

/// Fail unless `frame` shows the staged submission below the newest canonical content.
fn check_pending_row<D: TranscriptScenarioDriver>(
    scenario: &Scenario<D>,
    frame: &ScenarioFrame,
    what: &str,
) {
    let heading = row_with(frame, SENDING);
    let prompt = row_with(frame, PROMPT);
    let newest = frame.row_of(newest_marker(scenario)).map(usize::from);
    scenario.check(
        newest.is_some()
            && heading.is_some()
            && prompt.is_some()
            && newest < heading
            && heading < prompt,
        || {
            format!(
                "{what}: the staged submission is not shown below the newest content \
                 (newest {newest:?}, heading {heading:?}, prompt {prompt:?})"
            )
        },
    );
}

/// LR1/LR2: a staged submission shows its pending row (`You · sending` and the text) in the
/// first committed frame after Enter, after the newest canonical content.
pub fn lr1_staged_submission_shows_pending_row<D: TranscriptScenarioDriver>() {
    let mut scenario = short::<D>();
    submit(&mut scenario);
    let staged = scenario.present();
    check_pending_row(&scenario, &staged, "staged submission");
}

/// LR7/C8: while following an overflowing transcript, the staged submission's row is tail
/// content: the view stays on the latest bottom, showing the row below the newest content,
/// and nothing else moves (no reveal, no jump).
pub fn lr7_staged_submission_while_following_stays_on_latest_bottom<D: TranscriptScenarioDriver>() {
    let mut scenario = start::<D>(ScenarioSession::with_turns(10, 3), ResidentSeed::Everything);
    let before = scenario.present();
    submit(&mut scenario);
    let staged = scenario.present();
    check_pending_row(&scenario, &staged, "staged submission while following");
    scenario.check(staged.following_bottom, || {
        "the staged submission left the latest bottom".to_owned()
    });
    let pending_rows = staged.transcript_rows().len() - row_with(&staged, SENDING).unwrap_or(0);
    let shift = staged.shift_from(&before);
    scenario.check(
        shift.is_some_and(|shift| {
            usize::try_from(shift.unsigned_abs()).is_ok_and(|moved| moved <= pending_rows)
        }),
        || format!("content moved {shift:?} rows, more than the pending row's {pending_rows}"),
    );
}

/// LR4: a rejected submission reports the daemon's error as a visible System notice, before
/// the retained pending row (notices at their chronology boundary, pending rows last).
pub fn lr4_rejected_submission_notice_is_visible<D: TranscriptScenarioDriver>() {
    let mut scenario = short::<D>();
    submit(&mut scenario);
    scenario.present();
    scenario.resolve_submission(&SubmissionOutcome::Rejected {
        code: REJECTION.to_owned(),
    });
    let rejected = settle(&mut scenario);
    let notice = row_with(&rejected, REJECTION);
    let heading = row_with(&rejected, SENDING);
    scenario.check(notice.is_some(), || {
        "the rejection notice is not visible".to_owned()
    });
    scenario.check(row_with(&rejected, "unresolved").is_some(), || {
        "the rejection notice does not say the submission is unresolved".to_owned()
    });
    scenario.check(heading.is_some() && notice < heading, || {
        format!("the retained pending row ({heading:?}) does not follow the notice ({notice:?})")
    });
}

/// LR1: a slash command whose output is a process-local notice (a draft has no session to
/// record it in) shows that output.
pub fn lr1_slash_command_output_is_visible<D: TranscriptScenarioDriver>() {
    let mut scenario = start::<D>(ScenarioSession::new(), ResidentSeed::Draft);
    type_text(&mut scenario, "/version");
    scenario.key("enter");
    scenario.complete_slash_command();
    let shown = settle(&mut scenario);
    scenario.check(row_with(&shown, "Crate version").is_some(), || {
        "the /version output is not visible".to_owned()
    });
}

/// LR4/T2: a draft submission shows its pending row; once the daemon accepts it (creating and
/// attaching the session, with the production result: the submission record and the model
/// receipt), the submission stays visible in every frame while the created session attaches,
/// the canonical message replaces the pending row and the streamed reply is visible.
pub fn lr4_draft_submission_attaches_and_streams_reply<D: TranscriptScenarioDriver>() {
    let mut scenario = start::<D>(ScenarioSession::new(), ResidentSeed::Draft);
    submit(&mut scenario);
    let staged = scenario.present();
    scenario.check(
        row_with(&staged, SENDING).is_some() && row_with(&staged, PROMPT).is_some(),
        || "the draft submission shows no pending row".to_owned(),
    );
    // The result forgets the pending submission (its record arrives with it) before the
    // created session's events do.
    scenario.resolve_submission(&SubmissionOutcome::Accepted);
    let accepted = scenario.present();
    scenario.check(row_with(&accepted, PROMPT).is_some(), || {
        "the submission vanished when the daemon accepted it".to_owned()
    });
    let attaching = settle(&mut scenario);
    scenario.check(row_with(&attaching, PROMPT).is_some(), || {
        "the submission vanished while the created session attached".to_owned()
    });
    // V2 attaches the created session with its bounded tail window.
    let _ = scenario.complete_kind(super::driver::HistoryRequestKind::Tail);
    let attached = scenario.present();
    scenario.check(
        row_with(&attached, PROMPT).is_some() && row_with(&attached, SENDING).is_none(),
        || "the canonical message did not replace the pending row on attach".to_owned(),
    );
    scenario.append_live_assistant(2);
    let replied = settle(&mut scenario);
    let reply = newest_marker(&scenario);
    scenario.check(row_with(&replied, PROMPT).is_some(), || {
        "the accepted user message is not visible".to_owned()
    });
    scenario.check(row_with(&replied, SENDING).is_none(), || {
        "the pending row stayed after acceptance".to_owned()
    });
    scenario.check(replied.shows(reply), || {
        "the streamed reply is not visible".to_owned()
    });
}
