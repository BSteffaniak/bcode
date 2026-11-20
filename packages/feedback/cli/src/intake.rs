//! The intake session: one long-lived session that triages every new item as a follow-up
//! turn, so it keeps what it learns about the person across unrelated items.

use std::time::Duration;

use bcode_client::{BcodeClient, ClientError};
use bcode_feedback_models::{
    ClaimIntakeSessionRequest, FEEDBACK_INTAKE_APP_ID, FeedbackItem, IntakeSession,
    OP_CLAIM_INTAKE_SESSION,
};
use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionHistoryDirection, SessionHistoryQuery, SessionId,
    SessionOrigin, TurnAdmission, TurnAdmissionMetadata, TurnContext, TurnContextBlock, TurnId,
    TurnOrigin,
};

use crate::{call, intake_session};

/// How the item reached the intake session.
pub struct SendOutcome {
    /// The intake session.
    pub session_id: SessionId,
    /// The intake agent's reply, when the caller waited for it.
    pub reply: Option<String>,
}

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

/// Return the intake session, creating and claiming one when none exists or it was deleted.
///
/// # Errors
/// Returns a message when the daemon cannot list, create, or claim the session.
pub async fn ensure_session(client: &BcodeClient) -> Result<SessionId, String> {
    let current = intake_session(client).await?;
    if let Some(id) = &current.session_id {
        let id: SessionId = id
            .parse()
            .map_err(|_| format!("stored intake session {id} is not a session id"))?;
        match client.session_summary(id).await {
            Ok(_) => return Ok(id),
            Err(ClientError::Server { code, .. }) if code == "session_not_found" => {}
            Err(error) => return Err(format!("intake session unavailable: {error}")),
        }
    }
    let directory = bcode_config::default_state_dir().join("feedback");
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    let mut origin = SessionOrigin::new(FEEDBACK_INTAKE_APP_ID);
    origin.app_display_name = Some("Feedback".into());
    let created = client
        .create_session_with_origin(Some("Feedback intake".into()), directory, Some(origin))
        .await
        .map_err(|error| format!("creating the intake session: {error}"))?;
    let claimed: IntakeSession = call(
        client,
        OP_CLAIM_INTAKE_SESSION,
        &ClaimIntakeSessionRequest {
            expected: current.session_id,
            session_id: created.id.to_string(),
        },
    )
    .await?;
    let winner = claimed
        .session_id
        .ok_or("intake session claim returned nothing")?;
    if winner != created.id.to_string() {
        // Another submitter won the race; this candidate never received a turn.
        let _ = client.delete_session(created.id).await;
    }
    winner
        .parse()
        .map_err(|_| format!("intake session {winner} is not a session id"))
}

/// Send a recorded item to the intake session, optionally waiting for the reply.
///
/// Submission is idempotent per item, so retrying a submit never triages an item twice.
///
/// # Errors
/// Returns a message when the session cannot be reached, the turn is rejected, or waiting
/// fails or times out. The item itself stays recorded.
pub async fn send(
    client: &BcodeClient,
    item: &FeedbackItem,
    wait: Option<Duration>,
) -> Result<SendOutcome, String> {
    let session_id = ensure_session(client).await?;
    let mut watcher = match wait {
        Some(_) => Some(
            client
                .watch_session(session_id, 0)
                .await
                .map_err(|error| format!("watching the intake session: {error}"))?,
        ),
        None => None,
    };
    let admission = TurnAdmissionMetadata {
        origin: Some(TurnOrigin {
            producer: FEEDBACK_INTAKE_APP_ID.into(),
            correlation_id: Some(item.id.to_string()),
            display_label: Some(format!("Feedback {} · {}", item.id, item.app)),
        }),
        idempotency_key: Some(format!("feedback:{}", item.id)),
        context: context(item),
        ..TurnAdmissionMetadata::default()
    };
    let receipt = match client
        .submit_turn(session_id, item.text.clone(), admission)
        .await
        .map_err(|error| format!("submitting to the intake session: {error}"))?
    {
        TurnAdmission::Accepted(receipt)
        | TurnAdmission::Existing(receipt)
        | TurnAdmission::Deferred(receipt) => receipt,
        TurnAdmission::Rejected(reason) => {
            return Err(format!("the intake session rejected the item: {reason:?}"));
        }
        TurnAdmission::CancelledBeforeStart(_) => {
            return Err("the intake turn was cancelled before it started".into());
        }
    };
    let (Some(timeout), Some(watcher)) = (wait, watcher.as_mut()) else {
        return Ok(SendOutcome {
            session_id,
            reply: None,
        });
    };
    let outcome = watcher
        .wait_for_turn_outcome(&receipt.turn_id.to_string(), timeout)
        .await
        .map_err(|error| format!("waiting for the intake agent: {error}"))?;
    let page = client
        .session_history_page(
            session_id,
            SessionHistoryQuery {
                cursor: None,
                limit: 64,
                direction: SessionHistoryDirection::Backward,
            },
        )
        .await
        .map_err(|error| format!("reading the intake reply: {error}"))?;
    let reply = final_reply(&page.events, &receipt.turn_id)
        .unwrap_or_else(|| format!("The intake turn ended without a reply ({outcome:?})."));
    Ok(SendOutcome {
        session_id,
        reply: Some(reply),
    })
}

/// The text of `turn`'s last output position: the reply after its final tool round.
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
pub fn context(item: &FeedbackItem) -> TurnContext {
    let mut blocks = vec![
        TurnContextBlock::new("feedback-item", format!("{}\n\n{INSTRUCTIONS}", item.id)),
        TurnContextBlock::new("app", item.app.clone()),
    ];
    if !item.facets.is_empty() {
        let facets = item
            .facets
            .iter()
            .map(|facet| format!("{}={}", facet.key, facet.model_value()))
            .collect::<Vec<_>>()
            .join("\n");
        blocks.push(TurnContextBlock::new("facets", facets));
    }
    TurnContext {
        blocks,
        ..TurnContext::default()
    }
}
