//! Feedback requests: recording and triaging feedback for the application.

use std::future::Future;
use std::pin::Pin;

use bcode_feedback_client::FeedbackClientError;
use bcode_feedback_models::{
    FeedbackItem, RecordFeedbackRequest, RecordForTriageRequest, TriageDispatchState, TriageIntent,
};
use bcode_session_bridge_models::{BridgeError, BridgeResponse, FeedbackIntake};
use bcode_session_models::SessionId;

use crate::{Bridge, Handled};

/// `triage_error` code: delivery failed and will be retried; the item is queued.
pub const TRIAGE_QUEUED: &str = "feedback_triage_queued";
/// `triage_error` code: delivery stopped until a person asks again.
pub const TRIAGE_BLOCKED: &str = "feedback_triage_blocked";

/// A feedback failure with its stable code.
pub fn error(failure: &FeedbackClientError) -> BridgeError {
    BridgeError {
        code: failure.code().to_owned(),
        message: failure.to_string(),
    }
}

impl Bridge {
    /// Record `item` together with the decision to triage it, then try to deliver it to the
    /// intake session at once.
    ///
    /// The item and its triage request are written in one transaction, so triage cannot be
    /// lost once the item is saved. When delivery cannot finish now, the item stays queued and
    /// the daemon's dispatcher delivers it later, even after this application is gone; the
    /// response says so through the item's `triage_dispatch` and `triage_error`. With `claim`,
    /// the intake session is claimed for this bridge's registered tools before the turn
    /// starts, so the intake agent can call them; a failed claim does not stop triage.
    pub(crate) async fn feedback_submit(
        &self,
        item: &RecordFeedbackRequest,
        instructions: Option<&str>,
        claim: bool,
        skip_triage: bool,
    ) -> Handled {
        let triage = if skip_triage {
            TriageIntent::Skipped
        } else {
            TriageIntent::Requested {
                instructions: instructions.map(str::to_owned),
            }
        };
        let item = bcode_feedback_client::record_for_triage(
            &self.client,
            &RecordForTriageRequest {
                item: item.clone(),
                triage,
            },
        )
        .await
        .map_err(|failure| error(&failure))?;
        if skip_triage {
            return Ok(BridgeResponse::FeedbackSubmitted {
                item,
                intake: None,
                triage_error: None,
            });
        }
        let holder = bcode_feedback_client::holder("bridge");
        let claim_tools = |session_id: SessionId| -> Pin<
            Box<dyn Future<Output = Result<(), String>> + Send + '_>,
        > {
            Box::pin(async move {
                self.with_tools(|handle| async move { handle.claim_session(session_id).await })
                    .await
                    .map(|_| ())
                    .map_err(|error| error.message)
            })
        };
        let delivered = bcode_feedback_client::deliver(
            &self.client,
            item.id,
            &holder,
            claim.then_some(&claim_tools as bcode_feedback_client::ClaimIntake<'_>),
        )
        .await;
        Ok(match delivered {
            Ok(delivery) => {
                let triage_error = delivery.failure.map(|message| BridgeError {
                    code: failure_code(&delivery.item).to_owned(),
                    message,
                });
                BridgeResponse::FeedbackSubmitted {
                    intake: delivery
                        .intake
                        .map(|receipt| FeedbackIntake {
                            session_id: receipt.session_id.to_string(),
                            turn_id: receipt.turn_id.to_string(),
                        })
                        .or_else(|| started_intake(&delivery.item)),
                    item: delivery.item,
                    triage_error,
                }
            }
            // The store could not record the attempt; the request stays queued for the
            // daemon's dispatcher.
            Err(failure) => BridgeResponse::FeedbackSubmitted {
                item,
                intake: None,
                triage_error: Some(BridgeError {
                    code: TRIAGE_QUEUED.to_owned(),
                    message: failure.to_string(),
                }),
            },
        })
    }
}

/// The stable code of a failed delivery, from the state it left the item in.
fn failure_code(item: &FeedbackItem) -> &'static str {
    match item.triage_dispatch.as_ref().map(|dispatch| dispatch.state) {
        Some(TriageDispatchState::Blocked) => TRIAGE_BLOCKED,
        _ => TRIAGE_QUEUED,
    }
}

/// The intake turn an item's request already started, such as one another dispatcher
/// delivered first.
fn started_intake(item: &FeedbackItem) -> Option<FeedbackIntake> {
    let dispatch = item.triage_dispatch.as_ref()?;
    if dispatch.state != TriageDispatchState::Started {
        return None;
    }
    Some(FeedbackIntake {
        session_id: dispatch.session_id.clone()?,
        turn_id: dispatch.turn_id.clone()?,
    })
}
