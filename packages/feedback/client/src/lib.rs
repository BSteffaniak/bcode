#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Feedback over a Bcode daemon client: typed `bcode.feedback/v1` calls and intake turns.
//!
//! Every call acts for the person. The intake session is the single long-lived session that
//! triages new items as follow-up turns, so it keeps what it learns across unrelated items.
//! [`ensure_intake_session`], [`submit_to_intake`] and [`wait_for_reply`] are separate so a
//! caller can claim the session for its own tools before the turn starts.

mod intake;

use bcode_client::BcodeClient;
use bcode_feedback_models::{
    AttachFeedbackRequest, FEEDBACK_PLUGIN_ID, FEEDBACK_SERVICE_INTERFACE_ID, FeedbackItem,
    FeedbackPage, FeedbackQuery, GetFeedbackRequest, LinkFeedbackRequest, OP_ATTACH, OP_GET,
    OP_LINK, OP_LIST, OP_RECORD, OP_SET_STATUS, RecordFeedbackRequest, SetFeedbackStatusRequest,
};

pub use intake::{
    IntakeReceipt, MAX_APP_INSTRUCTIONS_BYTES, ensure_intake_session, final_reply, intake_context,
    submit_to_intake, wait_for_reply,
};

/// Failure of a feedback call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeedbackClientError {
    /// The daemon or its feedback service could not be reached.
    #[error("feedback service unavailable: {0}")]
    Unavailable(String),
    /// The feedback service rejected the call; `code` is stable (`not_found`, `forbidden`,
    /// `invalid_request`, `attachment_rejected`, ...).
    #[error("{message} ({code})")]
    Service {
        /// Stable service error code.
        code: String,
        /// Explanation.
        message: String,
    },
    /// The request is malformed and was not sent.
    #[error("{0}")]
    Invalid(String),
    /// The intake session could not be created, reached, or used.
    #[error("{0}")]
    Intake(String),
}

impl FeedbackClientError {
    /// A stable code for callers that relay the failure, such as the session bridge.
    #[must_use]
    pub fn code(&self) -> &str {
        match self {
            Self::Unavailable(_) => "feedback_unavailable",
            Self::Service { code, .. } => code,
            Self::Invalid(_) => "invalid_request",
            Self::Intake(_) => "feedback_intake_failed",
        }
    }
}

/// Record a new item.
///
/// # Errors
/// [`FeedbackClientError::Invalid`] for an out-of-bounds request, or [`call`]'s errors.
pub async fn record(
    client: &BcodeClient,
    request: &RecordFeedbackRequest,
) -> Result<FeedbackItem, FeedbackClientError> {
    request
        .validate()
        .map_err(|error| FeedbackClientError::Invalid(error.to_string()))?;
    call(client, OP_RECORD, request).await
}

/// Read one item.
///
/// # Errors
/// [`call`]'s errors, including `not_found`.
pub async fn get(
    client: &BcodeClient,
    request: &GetFeedbackRequest,
) -> Result<FeedbackItem, FeedbackClientError> {
    call(client, OP_GET, request).await
}

/// List matching items, newest first.
///
/// # Errors
/// [`call`]'s errors.
pub async fn list(
    client: &BcodeClient,
    query: &FeedbackQuery,
) -> Result<FeedbackPage, FeedbackClientError> {
    call(client, OP_LIST, query).await
}

/// Change an item's status, as the person.
///
/// # Errors
/// [`call`]'s errors.
pub async fn set_status(
    client: &BcodeClient,
    request: &SetFeedbackStatusRequest,
) -> Result<FeedbackItem, FeedbackClientError> {
    call(client, OP_SET_STATUS, request).await
}

/// Link one item to another.
///
/// # Errors
/// [`call`]'s errors.
pub async fn link(
    client: &BcodeClient,
    request: &LinkFeedbackRequest,
) -> Result<FeedbackItem, FeedbackClientError> {
    call(client, OP_LINK, request).await
}

/// Copy a file or directory into an item.
///
/// # Errors
/// [`FeedbackClientError::Invalid`] for a malformed request, or [`call`]'s errors, including
/// `attachment_rejected`.
pub async fn attach(
    client: &BcodeClient,
    request: &AttachFeedbackRequest,
) -> Result<FeedbackItem, FeedbackClientError> {
    request
        .validate()
        .map_err(|error| FeedbackClientError::Invalid(error.to_string()))?;
    call(client, OP_ATTACH, request).await
}

/// Call one `bcode.feedback/v1` operation.
///
/// # Errors
/// * [`FeedbackClientError::Unavailable`] when the daemon or service cannot be reached or
///   answers with something undecodable.
/// * [`FeedbackClientError::Service`] when the service rejects the call.
pub async fn call<R: serde::de::DeserializeOwned>(
    client: &BcodeClient,
    operation: &str,
    payload: &(impl serde::Serialize + Sync),
) -> Result<R, FeedbackClientError> {
    let payload = serde_json::to_vec(payload)
        .map_err(|error| FeedbackClientError::Invalid(error.to_string()))?;
    let response = client
        .invoke_plugin_service(
            FEEDBACK_PLUGIN_ID.into(),
            FEEDBACK_SERVICE_INTERFACE_ID.into(),
            operation.into(),
            payload,
        )
        .await
        .map_err(|error| FeedbackClientError::Unavailable(error.to_string()))?;
    if let Some(error) = response.error {
        return Err(FeedbackClientError::Service {
            code: error.code,
            message: error.message,
        });
    }
    serde_json::from_slice(&response.payload).map_err(|error| {
        FeedbackClientError::Unavailable(format!("unexpected feedback service response: {error}"))
    })
}

#[cfg(test)]
mod tests;
