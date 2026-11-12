//! Review assistant sessions: which review a Bcode session works on.
//!
//! A review app binds a session to the review it is showing, so the review tools the session
//! calls act on that review without the model naming it. The binding records the review's
//! workspace (a durable workspace is read again by its id; a transient one is used as recorded)
//! and the revisions its sources resolved to when the app last bound it, so tools read exactly
//! what the app shows, including sources only a review-source provider can resolve.

use crate::{ResolvedReviewStack, ResolvedSource, ReviewWorkspace};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Current schema version of [`ReviewAssistantBinding`].
pub const REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION: u16 = 1;

/// The review a Bcode session works on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAssistantBinding {
    /// Persisted schema version.
    pub schema_version: u16,
    /// Bound Bcode session id.
    pub session_id: String,
    /// The review's workspace when it was bound. Durable workspaces are read again by id;
    /// transient (unsaved) ones are used as recorded.
    pub workspace: ReviewWorkspace,
    /// Resolved sources of the review when it was bound, in review order.
    #[serde(default)]
    pub resolved: Vec<ResolvedSource>,
    /// Stacks the sources resolved into.
    #[serde(default)]
    pub stacks: Vec<ResolvedReviewStack>,
    /// When the binding was first made, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// When the binding was last refreshed, in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
}

/// Request to bind a session to a review (or refresh its binding).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindReviewAssistantRequest {
    /// Repository path where the review's state is stored.
    pub repo_path: PathBuf,
    /// The binding to record; its timestamps are set by the store.
    pub binding: ReviewAssistantBinding,
}

/// Response to [`BindReviewAssistantRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindReviewAssistantResponse {
    /// The recorded binding.
    pub binding: ReviewAssistantBinding,
}

/// Request for the binding of one session, or the newest binding of one review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReviewAssistantRequest {
    /// Repository path where the review's state is stored.
    pub repo_path: PathBuf,
    /// Which binding to read.
    pub lookup: ReviewAssistantLookup,
}

/// Which assistant binding to read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewAssistantLookup {
    /// The binding of one session.
    Session {
        /// Bcode session id.
        session_id: String,
    },
    /// The most recently refreshed binding of a review, by its annotation scope id.
    Review {
        /// Annotation scope id of the review's workspace.
        workspace_id: String,
    },
}

/// Response to [`GetReviewAssistantRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReviewAssistantResponse {
    /// The binding, when one exists.
    pub binding: Option<ReviewAssistantBinding>,
}
