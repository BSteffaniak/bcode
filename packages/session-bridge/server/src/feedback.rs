//! Feedback requests: recording and triaging feedback for the application.

use bcode_feedback_client::FeedbackClientError;
use bcode_feedback_models::RecordFeedbackRequest;
use bcode_session_bridge_models::{BridgeError, BridgeResponse, FeedbackIntake};

use crate::{Bridge, Handled};

/// A feedback failure with its stable code.
pub fn error(failure: &FeedbackClientError) -> BridgeError {
    BridgeError {
        code: failure.code().to_owned(),
        message: failure.to_string(),
    }
}

impl Bridge {
    /// Record `item`, then hand it to the intake session.
    ///
    /// With `claim`, the intake session is claimed for this bridge's registered tools before the
    /// turn starts, so the intake agent can call them while it triages. A triage failure is
    /// reported beside the recorded item rather than failing the request: the item is kept.
    pub(crate) async fn feedback_submit(
        &self,
        item: &RecordFeedbackRequest,
        instructions: Option<&str>,
        claim: bool,
        skip_triage: bool,
    ) -> Handled {
        let item = bcode_feedback_client::record(&self.client, item)
            .await
            .map_err(|failure| error(&failure))?;
        if skip_triage {
            return Ok(BridgeResponse::FeedbackSubmitted {
                item,
                intake: None,
                triage_error: None,
            });
        }
        match self.triage(&item, instructions, claim).await {
            Ok(intake) => Ok(BridgeResponse::FeedbackSubmitted {
                item,
                intake: Some(intake),
                triage_error: None,
            }),
            Err(triage_error) => Ok(BridgeResponse::FeedbackSubmitted {
                item,
                intake: None,
                triage_error: Some(triage_error),
            }),
        }
    }

    async fn triage(
        &self,
        item: &bcode_feedback_models::FeedbackItem,
        instructions: Option<&str>,
        claim: bool,
    ) -> Result<FeedbackIntake, BridgeError> {
        let session_id = bcode_feedback_client::ensure_intake_session(&self.client)
            .await
            .map_err(|failure| error(&failure))?;
        if claim {
            self.with_tools(|handle| async move { handle.claim_session(session_id).await })
                .await?;
        }
        let receipt =
            bcode_feedback_client::submit_to_intake(&self.client, session_id, item, instructions)
                .await
                .map_err(|failure| error(&failure))?;
        Ok(FeedbackIntake {
            session_id: receipt.session_id.to_string(),
            turn_id: receipt.turn_id.to_string(),
        })
    }
}
