//! The daemon's triage dispatcher: delivers queued triage requests that no submitting client
//! finished, and notices intake turns that ended without triaging their item.
//!
//! It runs beside the daemon and talks to it like any client, so it needs nothing from the
//! application that reported the feedback: items queued while the intake session or its
//! model was unavailable, or whose application closed, are delivered once the daemon (or a
//! restarted one) can deliver them. Every pass is bounded.
//!
//! An intake turn queued behind another keeps its submitting client's execution context. When
//! that client is gone before the turn runs (its application closed, or the daemon restarted),
//! the session holds the turn, and every turn queued after it, until someone restores or
//! cancels it. The dispatcher cancels such a held intake turn, which it owns, and delivers its
//! item again as a new turn.

use std::time::Duration;

use bcode_client::{BcodeClient, ClientError};
use bcode_feedback_models::FEEDBACK_INTAKE_APP_ID;
use bcode_feedback_models::{
    IntakeTurnEnding, LeaseDueTriageRequest, OP_LEASE_DUE_TRIAGE, OP_STARTED_TRIAGES,
    OP_TRIAGE_TURN_ENDED, StartedTriage, StartedTriageQuery, TriageLease, TriageTurnEndedRequest,
};
use bcode_session_models::{
    ModelTurnOutcome, SessionId, SubmissionQueueBlocker, SubmissionQueueStatus, SubmissionStatus,
};

use crate::dispatch::{DELIVERY_LEASE, deliver_leased};
use crate::{FeedbackClientError, call};

/// Queued items delivered per pass.
const DELIVERIES_PER_PASS: usize = 8;
/// Started items checked per pass.
const CHECKS_PER_PASS: usize = 16;
/// How long a check waits to learn whether a started turn has ended.
const OUTCOME_WAIT: Duration = Duration::from_millis(500);

/// How often the dispatcher looks for work.
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    /// This dispatcher's lease holder name.
    pub holder: String,
    /// Time between passes that deliver queued items.
    pub poll: Duration,
    /// Check started turns every this many passes.
    pub check_started_every: u32,
}

impl WorkerOptions {
    /// Two-second passes; started turns are checked every five.
    #[must_use]
    pub const fn new(holder: String) -> Self {
        Self {
            holder,
            poll: Duration::from_secs(2),
            check_started_every: 5,
        }
    }
}

/// Run passes until `stop` resolves.
///
/// The first pass delivers every queued item at once, even those waiting out a retry delay:
/// a dispatcher starts with its daemon, and the outage that delayed them may be over.
pub async fn run(
    client: BcodeClient,
    options: WorkerOptions,
    stop: impl std::future::Future<Output = ()>,
) {
    let mut stop = std::pin::pin!(stop);
    let mut pass: u32 = 0;
    let mut fresh = true;
    loop {
        let check_started = pass.is_multiple_of(options.check_started_every.max(1));
        tokio::select! {
            () = &mut stop => return,
            delivered = run_pass(&client, &options.holder, fresh, check_started) => {
                if delivered {
                    fresh = false;
                }
            }
        }
        pass = pass.wrapping_add(1);
        tokio::select! {
            () = &mut stop => return,
            () = tokio::time::sleep(options.poll) => {}
        }
    }
}

/// One pass; returns whether the daemon was reachable.
///
/// # Errors
/// None: failures are logged and retried next pass.
pub async fn run_pass(
    client: &BcodeClient,
    holder: &str,
    ignore_retry_delay: bool,
    check_started: bool,
) -> bool {
    if client.ping().await.is_err() {
        return false;
    }
    if let Err(error) = deliver_due(client, holder, ignore_retry_delay).await {
        tracing::warn!(%error, "feedback: queued triage not delivered");
    }
    if check_started && let Err(error) = check_started_turns(client).await {
        tracing::warn!(%error, "feedback: started triage not checked");
    }
    true
}

async fn deliver_due(
    client: &BcodeClient,
    holder: &str,
    ignore_retry_delay: bool,
) -> Result<(), FeedbackClientError> {
    let leases: Vec<TriageLease> = call(
        client,
        OP_LEASE_DUE_TRIAGE,
        &LeaseDueTriageRequest {
            holder: holder.to_owned(),
            lease_ms: u64::try_from(DELIVERY_LEASE.as_millis()).unwrap_or(u64::MAX),
            limit: DELIVERIES_PER_PASS,
            ignore_retry_delay,
        },
    )
    .await?;
    for lease in &leases {
        let delivery = deliver_leased(client, lease, None).await?;
        match (&delivery.intake, &delivery.failure) {
            (Some(intake), _) => tracing::info!(
                id = %lease.item.id,
                turn = %intake.turn_id,
                "feedback: triage started"
            ),
            (None, Some(failure)) => tracing::warn!(
                id = %lease.item.id,
                %failure,
                "feedback: triage delivery failed"
            ),
            (None, None) => {}
        }
    }
    Ok(())
}

async fn check_started_turns(client: &BcodeClient) -> Result<(), FeedbackClientError> {
    let started: Vec<StartedTriage> = call(
        client,
        OP_STARTED_TRIAGES,
        &StartedTriageQuery {
            limit: CHECKS_PER_PASS,
        },
    )
    .await?;
    for started in started {
        let ending = match turn_ending(client, &started).await {
            Some(ending) => Some(ending),
            None => release_held_turn(client, &started).await,
        };
        let Some((ending, detail)) = ending else {
            continue;
        };
        let _: bcode_feedback_models::FeedbackItem = call(
            client,
            OP_TRIAGE_TURN_ENDED,
            &TriageTurnEndedRequest {
                id: started.id,
                generation: started.generation,
                turn_id: started.turn_id.clone(),
                ending,
                detail,
            },
        )
        .await?;
    }
    Ok(())
}

/// How a started turn ended, or `None` while it runs or its end cannot be observed yet.
async fn turn_ending(
    client: &BcodeClient,
    started: &StartedTriage,
) -> Option<(IntakeTurnEnding, String)> {
    let Ok(session_id) = started.session_id.parse::<SessionId>() else {
        return Some((
            IntakeTurnEnding::Failed,
            format!("intake session {} is not a session id", started.session_id),
        ));
    };
    let mut watcher = match client.watch_session(session_id, 0).await {
        Ok(watcher) => watcher,
        Err(ClientError::Server { code, .. }) if code == "session_not_found" => {
            return Some((
                IntakeTurnEnding::Failed,
                "the intake session no longer exists".to_owned(),
            ));
        }
        Err(_) => return None,
    };
    let outcome = watcher
        .recover_and_wait_for_turn_outcome(&started.turn_id, OUTCOME_WAIT)
        .await
        .ok()?;
    Some(ending(outcome))
}

/// Cancel `started`'s turn when it heads the intake queue but cannot run because its
/// submitting client's context is gone, so the queue moves again; the item is then delivered
/// as a new turn. Returns how the turn ended, or `None` when it is not held.
async fn release_held_turn(
    client: &BcodeClient,
    started: &StartedTriage,
) -> Option<(IntakeTurnEnding, String)> {
    let session_id = started.session_id.parse::<SessionId>().ok()?;
    let status: SubmissionQueueStatus = client.queue_status(session_id).await.ok()?;
    let blocker = status.blocker?;
    if !matches!(
        blocker,
        SubmissionQueueBlocker::MissingContext | SubmissionQueueBlocker::IncompatibleContext
    ) {
        return None;
    }
    let held = status.oldest?;
    let ours = held
        .submission
        .admission
        .origin
        .as_ref()
        .is_some_and(|origin| origin.producer == FEEDBACK_INTAKE_APP_ID);
    if !ours
        || held.status != SubmissionStatus::Pending
        || held.submission.target.turn_id().0 != started.turn_id
    {
        return None;
    }
    let submission_id = held.submission.id.clone();
    let cancelled = client
        .cancel_pending_submission(
            session_id,
            submission_id.clone(),
            held.receipt.accepted_event_sequence,
            format!("feedback-redeliver:{}", submission_id.0),
        )
        .await;
    if let Err(error) = cancelled {
        tracing::warn!(%error, id = %started.id, "feedback: held intake turn not released");
        return None;
    }
    tracing::info!(id = %started.id, ?blocker, "feedback: released a held intake turn");
    Some((
        IntakeTurnEnding::Failed,
        format!("the queued intake turn could not run ({blocker:?}); delivering it again"),
    ))
}

/// How a terminal turn outcome counts for delivery.
#[must_use]
pub fn ending(outcome: ModelTurnOutcome) -> (IntakeTurnEnding, String) {
    let ending = match outcome {
        ModelTurnOutcome::Completed | ModelTurnOutcome::ToolRoundLimitReached => {
            IntakeTurnEnding::Untriaged
        }
        ModelTurnOutcome::Cancelled => IntakeTurnEnding::Cancelled,
        ModelTurnOutcome::Error
        | ModelTurnOutcome::IdleTimeout
        | ModelTurnOutcome::ProviderUnavailable => IntakeTurnEnding::Failed,
    };
    (ending, format!("{outcome:?}"))
}
