//! The intake session and the turns that hand it new items.

use std::time::Duration;

use bcode_client::{BcodeClient, ClientError};
use bcode_feedback_models::{
    ClaimIntakeSessionRequest, FEEDBACK_INTAKE_APP_ID, FeedbackItem, IntakeSession,
    OP_CLAIM_INTAKE_SESSION, OP_INTAKE_SESSION, triage_idempotency_key, validate_instructions,
};
use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionHistoryDirection, SessionHistoryQuery, SessionId,
    SessionOrigin, TurnAdmission, TurnAdmissionMetadata, TurnContext, TurnContextBlock, TurnId,
    TurnOrigin, TurnRejectionReason,
};

use crate::{FeedbackClientError, call};

/// Standing instructions carried with every item. Durable so later turns, and summaries made
/// by compaction, still see what each turn was.
const INSTRUCTIONS: &str = "\
A person just gave the feedback in this message. Triage it:
1. Search earlier feedback (feedback.search) for duplicates or related items and link them \
(feedback.link).
2. Record triage with feedback.triage: a one-line title, a summary of the problem and what they \
expected, category, severity and tags. Put questions only the person can answer in \
open_questions instead of guessing.
3. Reply in a few sentences: what you recorded, what it relates to, and any question.
Use what you have learned about this person from earlier feedback in this conversation.";

/// A submitted intake turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeReceipt {
    /// The intake session.
    pub session_id: SessionId,
    /// The turn triaging the item.
    pub turn_id: TurnId,
    /// Sequence of the accepted user message.
    pub accepted_event_sequence: u64,
}

/// Return the intake session, creating and claiming one when none exists or it was deleted.
///
/// Concurrent callers each create a candidate; exactly one claim wins, and the losers delete
/// their candidate and use the winner.
///
/// # Errors
/// [`FeedbackClientError::Intake`] when the session cannot be checked, created, or claimed.
pub async fn ensure_intake_session(client: &BcodeClient) -> Result<SessionId, FeedbackClientError> {
    let current: IntakeSession = call(client, OP_INTAKE_SESSION, &()).await?;
    if let Some(id) = &current.session_id {
        let id: SessionId = id.parse().map_err(|_| {
            FeedbackClientError::Intake(format!("stored intake session {id} is not a session id"))
        })?;
        match client.session_summary(id).await {
            Ok(_) => return Ok(id),
            Err(ClientError::Server { code, .. }) if code == "session_not_found" => {}
            Err(error) => {
                return Err(FeedbackClientError::Intake(format!(
                    "intake session unavailable: {error}"
                )));
            }
        }
    }
    let directory = bcode_config::default_state_dir().join("feedback");
    std::fs::create_dir_all(&directory).map_err(|error| {
        FeedbackClientError::Intake(format!("{}: {error}", directory.display()))
    })?;
    let mut origin = SessionOrigin::new(FEEDBACK_INTAKE_APP_ID);
    origin.app_display_name = Some("Feedback".into());
    let created = client
        .create_session_with_origin(Some("Feedback intake".into()), directory, Some(origin))
        .await
        .map_err(|error| {
            FeedbackClientError::Intake(format!("creating the intake session: {error}"))
        })?;
    let claimed: IntakeSession = call(
        client,
        OP_CLAIM_INTAKE_SESSION,
        &ClaimIntakeSessionRequest {
            expected: current.session_id,
            session_id: created.id.to_string(),
        },
    )
    .await?;
    let winner = claimed.session_id.ok_or_else(|| {
        FeedbackClientError::Intake("intake session claim returned nothing".into())
    })?;
    if winner != created.id.to_string() {
        // Another submitter won the race; this candidate never received a turn.
        let _ = client.delete_session(created.id).await;
    }
    winner.parse().map_err(|_| {
        FeedbackClientError::Intake(format!("intake session {winner} is not a session id"))
    })
}

/// Submit a recorded item to the intake session as a follow-up turn, as the first delivery
/// generation. Submission is idempotent per item; see [`submit_generation`].
///
/// # Errors
/// See [`submit_generation`].
pub async fn submit_to_intake(
    client: &BcodeClient,
    session_id: SessionId,
    item: &FeedbackItem,
    app_instructions: Option<&str>,
) -> Result<IntakeReceipt, FeedbackClientError> {
    submit_generation(client, session_id, item, app_instructions, 1)
        .await
        .map_err(|failure| match failure {
            SubmitFailure::Invalid(message) => FeedbackClientError::Invalid(message),
            SubmitFailure::Transient(message) | SubmitFailure::Refused(message) => {
                FeedbackClientError::Intake(message)
            }
        })
}

/// Why an intake turn was not admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitFailure {
    /// The request is malformed; resending it cannot succeed.
    Invalid(String),
    /// The daemon or session could not take the turn now; the same generation may be resent.
    Transient(String),
    /// The session refused or cancelled this generation's turn; resending it cannot succeed.
    Refused(String),
}

/// Submit `generation` of an item's delivery to the intake session as a follow-up turn.
///
/// `app_instructions` tells the agent what the reporting application offers, such as its own
/// tools for capturing context around the feedback. Every submission of one generation uses
/// the same idempotency key, so the session admits at most one turn for it: resending after a
/// lost acknowledgement returns the turn already admitted.
///
/// # Errors
/// * [`SubmitFailure::Invalid`] when `app_instructions` is empty or longer than
///   [`bcode_feedback_models::MAX_APP_INSTRUCTIONS_BYTES`].
/// * [`SubmitFailure::Transient`] when the daemon cannot be reached or the session is
///   unavailable.
/// * [`SubmitFailure::Refused`] when the session's execution policy rejects the turn or the
///   generation's turn was cancelled before it started.
pub async fn submit_generation(
    client: &BcodeClient,
    session_id: SessionId,
    item: &FeedbackItem,
    app_instructions: Option<&str>,
    generation: u32,
) -> Result<IntakeReceipt, SubmitFailure> {
    let context = intake_context(item, app_instructions)
        .map_err(|error| SubmitFailure::Invalid(error.to_string()))?;
    let admission = TurnAdmissionMetadata {
        origin: Some(TurnOrigin {
            producer: FEEDBACK_INTAKE_APP_ID.into(),
            correlation_id: Some(item.id.to_string()),
            display_label: Some(format!("Feedback {} · {}", item.id, item.app)),
        }),
        idempotency_key: Some(triage_idempotency_key(item.id, generation)),
        context,
        ..TurnAdmissionMetadata::default()
    };
    let receipt = match client
        .submit_turn(session_id, item.text.clone(), admission)
        .await
        .map_err(|error| {
            SubmitFailure::Transient(format!("submitting to the intake session: {error}"))
        })? {
        TurnAdmission::Accepted(receipt)
        | TurnAdmission::Existing(receipt)
        | TurnAdmission::Deferred(receipt) => receipt,
        TurnAdmission::Rejected(TurnRejectionReason::SessionUnavailable) => {
            return Err(SubmitFailure::Transient(
                "the intake session is unavailable".into(),
            ));
        }
        TurnAdmission::Rejected(TurnRejectionReason::ExecutionPolicy) => {
            return Err(SubmitFailure::Refused(
                "the intake session's execution policy rejected the turn".into(),
            ));
        }
        TurnAdmission::CancelledBeforeStart(_) => {
            return Err(SubmitFailure::Refused(
                "the intake turn was cancelled before it started".into(),
            ));
        }
    };
    Ok(IntakeReceipt {
        session_id,
        turn_id: receipt.turn_id,
        accepted_event_sequence: receipt.accepted_event_sequence,
    })
}

/// Wait up to `timeout` for an intake turn to end and return the agent's reply.
///
/// `watcher` must have been created with [`BcodeClient::watch_session`] before the turn was
/// submitted, so its outcome cannot be missed.
///
/// # Errors
/// [`FeedbackClientError::Intake`] when observation fails or times out; the turn keeps running.
pub async fn wait_for_reply(
    client: &BcodeClient,
    watcher: &mut bcode_client::SessionWatcher,
    receipt: &IntakeReceipt,
    timeout: Duration,
) -> Result<String, FeedbackClientError> {
    let outcome = watcher
        .wait_for_turn_outcome(&receipt.turn_id.to_string(), timeout)
        .await
        .map_err(|error| {
            FeedbackClientError::Intake(format!("waiting for the intake agent: {error}"))
        })?;
    let page = client
        .session_history_page(
            receipt.session_id,
            SessionHistoryQuery {
                cursor: None,
                limit: 64,
                direction: SessionHistoryDirection::Backward,
            },
        )
        .await
        .map_err(|error| {
            FeedbackClientError::Intake(format!("reading the intake reply: {error}"))
        })?;
    Ok(final_reply(&page.events, &receipt.turn_id)
        .unwrap_or_else(|| format!("The intake turn ended without a reply ({outcome:?}).")))
}

/// The text of `turn`'s last output position: the reply after its final tool round.
#[must_use]
pub fn final_reply(events: &[SessionEvent], turn: &TurnId) -> Option<String> {
    let mut segments: Vec<(u64, u32, &str)> = Vec::new();
    for event in events {
        match &event.kind {
            SessionEventKind::PositionedAssistantResponseSegment {
                turn_id,
                output_position,
                segment_order,
                text,
                ..
            } if *turn_id == turn.0 => {
                segments.push((output_position.get(), *segment_order, text));
            }
            SessionEventKind::AssistantResponseSegment {
                turn_id,
                segment_order,
                text,
                ..
            } if *turn_id == turn.0 => segments.push((0, *segment_order, text)),
            _ => {}
        }
    }
    let last = segments.iter().map(|(position, ..)| *position).max()?;
    segments.retain(|(position, ..)| *position == last);
    segments.sort_by_key(|(_, order, _)| *order);
    let reply: String = segments.iter().map(|(.., text)| *text).collect();
    (!reply.trim().is_empty()).then_some(reply)
}

/// Durable context blocks describing the item; sensitive facet values are withheld.
///
/// # Errors
/// [`FeedbackClientError::Invalid`] when `app_instructions` is empty or longer than
/// [`bcode_feedback_models::MAX_APP_INSTRUCTIONS_BYTES`].
pub fn intake_context(
    item: &FeedbackItem,
    app_instructions: Option<&str>,
) -> Result<TurnContext, FeedbackClientError> {
    let mut blocks = vec![
        TurnContextBlock::new("feedback-item", format!("{}\n\n{INSTRUCTIONS}", item.id)),
        TurnContextBlock::new("app", item.app.clone()),
    ];
    if let Some(instructions) = app_instructions {
        validate_instructions(instructions)
            .map_err(|error| FeedbackClientError::Invalid(error.to_string()))?;
        blocks.push(TurnContextBlock::new("app-instructions", instructions));
    }
    if !item.facets.is_empty() {
        let facets = item
            .facets
            .iter()
            .map(|facet| format!("{}={}", facet.key, facet.model_value()))
            .collect::<Vec<_>>()
            .join("\n");
        blocks.push(TurnContextBlock::new("facets", facets));
    }
    Ok(TurnContext {
        blocks,
        ..TurnContext::default()
    })
}
