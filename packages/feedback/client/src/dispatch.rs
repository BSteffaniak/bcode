//! Delivering durable triage requests to the intake session.
//!
//! A submitting client records the item and its request together
//! ([`record_for_triage`]), then tries to deliver it at once ([`deliver`]). Whatever that
//! attempt cannot finish stays queued in the store, and the daemon's dispatcher
//! ([`crate::worker`]) delivers it later, with backoff, across restarts and after the
//! submitting application is gone.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use bcode_client::BcodeClient;
use bcode_feedback_models::{
    FeedbackId, FeedbackItem, GetFeedbackRequest, LeaseTriageRequest, OP_GET, OP_LEASE_TRIAGE,
    OP_RECORD_FOR_TRIAGE, OP_REQUEST_TRIAGE, OP_TRIAGE_FAILED, OP_TRIAGE_STARTED,
    RecordForTriageRequest, RequestTriageRequest, TriageFailedRequest, TriageLease,
    TriageStartedRequest, bounded_note,
};
use bcode_session_models::SessionId;

use crate::intake::{SubmitFailure, ensure_intake_session, submit_generation};
use crate::{FeedbackClientError, IntakeReceipt, call};

/// How long a dispatcher holds an item while delivering it. Long enough to create the intake
/// session and submit one turn; a dispatcher that dies mid-delivery frees the item after it.
pub const DELIVERY_LEASE: Duration = Duration::from_mins(2);

/// Claims the intake session for the caller's own tools before the turn starts, so the
/// intake agent can call them. A failed claim does not stop delivery: the item is triaged
/// without those tools.
pub type ClaimIntake<'a> = &'a (
        dyn Fn(SessionId) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>
            + Send
            + Sync
    );

/// One delivery attempt and the item as it stands after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// The item, with its triage request's state.
    pub item: FeedbackItem,
    /// The admitted intake turn, when this attempt delivered it.
    pub intake: Option<IntakeReceipt>,
    /// Why this attempt failed, when it did. The item's request says whether it will be
    /// retried or is blocked.
    pub failure: Option<String>,
}

/// Record a new item and its triage decision in one write.
///
/// # Errors
/// [`FeedbackClientError::Invalid`] for an out-of-bounds request, or [`call`]'s errors.
pub async fn record_for_triage(
    client: &BcodeClient,
    request: &RecordForTriageRequest,
) -> Result<FeedbackItem, FeedbackClientError> {
    request
        .validate()
        .map_err(|error| FeedbackClientError::Invalid(error.to_string()))?;
    call(client, OP_RECORD_FOR_TRIAGE, request).await
}

/// Queue an untriaged item for triage, again or for the first time.
///
/// # Errors
/// [`call`]'s errors, including `forbidden` for an item that is already triaged.
pub async fn request_triage(
    client: &BcodeClient,
    id: FeedbackId,
) -> Result<FeedbackItem, FeedbackClientError> {
    call(client, OP_REQUEST_TRIAGE, &RequestTriageRequest { id }).await
}

/// Deliver `id` to the intake session now, if it is queued and no other dispatcher holds it.
///
/// `holder` names this dispatcher. A failed attempt is recorded on the item: retried later
/// when transient, otherwise blocked. When the item is not deliverable now (another
/// dispatcher holds it, or it was started, triaged, skipped or blocked), the item is returned
/// as it stands.
///
/// # Errors
/// [`call`]'s errors when the feedback store cannot be reached. Delivery failures are not
/// errors: they are recorded and reported in [`Delivery::failure`].
pub async fn deliver(
    client: &BcodeClient,
    id: FeedbackId,
    holder: &str,
    claim: Option<ClaimIntake<'_>>,
) -> Result<Delivery, FeedbackClientError> {
    let lease: Option<TriageLease> = call(
        client,
        OP_LEASE_TRIAGE,
        &LeaseTriageRequest {
            id,
            holder: holder.to_owned(),
            lease_ms: lease_ms(),
        },
    )
    .await?;
    match lease {
        Some(lease) => deliver_leased(client, &lease, claim).await,
        None => Ok(Delivery {
            item: call(client, OP_GET, &GetFeedbackRequest { id }).await?,
            intake: None,
            failure: None,
        }),
    }
}

/// Deliver a leased item: find or create the intake session, claim it when asked, and submit
/// the leased generation's idempotent turn, then record how that went.
///
/// # Errors
/// [`call`]'s errors when the outcome cannot be recorded. The lease then expires and another
/// attempt resends the same generation, which the intake session deduplicates.
pub async fn deliver_leased(
    client: &BcodeClient,
    lease: &TriageLease,
    claim: Option<ClaimIntake<'_>>,
) -> Result<Delivery, FeedbackClientError> {
    match attempt(client, lease, claim).await {
        Ok(receipt) => {
            let item = call(
                client,
                OP_TRIAGE_STARTED,
                &TriageStartedRequest {
                    id: lease.item.id,
                    holder: lease.holder.clone(),
                    generation: lease.generation,
                    session_id: receipt.session_id.to_string(),
                    turn_id: receipt.turn_id.to_string(),
                },
            )
            .await?;
            Ok(Delivery {
                item,
                intake: Some(receipt),
                failure: None,
            })
        }
        Err(failure) => {
            let reason = bounded_note(&failure.reason);
            let item = call(
                client,
                OP_TRIAGE_FAILED,
                &TriageFailedRequest {
                    id: lease.item.id,
                    holder: lease.holder.clone(),
                    generation: lease.generation,
                    reason: reason.clone(),
                    retry: failure.retry,
                },
            )
            .await?;
            Ok(Delivery {
                item,
                intake: None,
                failure: Some(reason),
            })
        }
    }
}

/// A failed delivery attempt.
struct AttemptFailure {
    reason: String,
    retry: bool,
}

async fn attempt(
    client: &BcodeClient,
    lease: &TriageLease,
    claim: Option<ClaimIntake<'_>>,
) -> Result<IntakeReceipt, AttemptFailure> {
    let session_id = ensure_intake_session(client)
        .await
        .map_err(|error| AttemptFailure {
            reason: format!("intake session unavailable: {error}"),
            retry: true,
        })?;
    if let Some(claim) = claim
        && let Err(error) = claim(session_id).await
    {
        tracing::warn!(
            id = %lease.item.id,
            %error,
            "feedback: triaging without the application's tools"
        );
    }
    submit_generation(
        client,
        session_id,
        &lease.item,
        lease.instructions.as_deref(),
        lease.generation,
    )
    .await
    .map_err(|failure| match failure {
        SubmitFailure::Transient(reason) => AttemptFailure {
            reason,
            retry: true,
        },
        SubmitFailure::Invalid(reason) | SubmitFailure::Refused(reason) => AttemptFailure {
            reason,
            retry: false,
        },
    })
}

/// A dispatcher name unique to this process and call site.
#[must_use]
pub fn holder(kind: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    format!(
        "{kind}-{}-{started}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn lease_ms() -> u64 {
    u64::try_from(DELIVERY_LEASE.as_millis()).unwrap_or(u64::MAX)
}
