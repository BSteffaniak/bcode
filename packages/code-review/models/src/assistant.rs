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
    /// Where the session's conversation started. The store keeps the first value ever saved
    /// for a session, so rebinding never changes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<ReviewAssistantTag>,
    /// Set while the session is attached to no review (its app shows no review), in
    /// milliseconds since the Unix epoch. Review tools treat a detached session as unbound;
    /// `workspace` keeps the review it was last attached to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached_at_ms: Option<u64>,
}

impl ReviewAssistantBinding {
    /// Whether the session is attached to no review.
    #[must_use]
    pub const fn is_detached(&self) -> bool {
        self.detached_at_ms.is_some()
    }
}

/// Where a review assistant conversation started: a review (and the stack layer that was
/// current), or no review (the reviews home or the new-review wizard).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAssistantTag {
    /// Annotation scope id of the review; `None` when no review was on screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The review's title then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Number of the stack layer (pull request) that was current, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<u64>,
}

impl ReviewAssistantTag {
    /// The tag of a conversation started where no review was on screen.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            workspace_id: None,
            title: None,
            layer: None,
        }
    }
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

/// Operation listing the assistant bindings of a repository.
pub const OP_REVIEW_ASSISTANT_LIST: &str = "review.assistant.list";
/// Operation removing one session's assistant binding.
pub const OP_REVIEW_ASSISTANT_UNBIND: &str = "review.assistant.unbind";
/// Bindings listed at most by one [`ListReviewAssistantsRequest`].
pub const MAX_REVIEW_ASSISTANT_LIST: u32 = 200;

/// Request for the assistant bindings of a repository, most recently refreshed first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReviewAssistantsRequest {
    /// Repository path where the review state is stored.
    pub repo_path: PathBuf,
    /// Bindings returned at most (at most [`MAX_REVIEW_ASSISTANT_LIST`], which is also the
    /// default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Response to [`ListReviewAssistantsRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReviewAssistantsResponse {
    /// Readable bindings, most recently refreshed first.
    pub bindings: Vec<ReviewAssistantBinding>,
    /// Whether more bindings exist than were returned.
    #[serde(default)]
    pub truncated: bool,
}

/// Request to remove one session's assistant binding (its conversation was deleted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnbindReviewAssistantRequest {
    /// Repository path where the review state is stored.
    pub repo_path: PathBuf,
    /// Bcode session id.
    pub session_id: String,
}

/// Response to [`UnbindReviewAssistantRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnbindReviewAssistantResponse {
    /// Whether a binding was removed.
    pub removed: bool,
}

/// Response to [`GetReviewAssistantRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReviewAssistantResponse {
    /// The binding, when one exists.
    pub binding: Option<ReviewAssistantBinding>,
}

/// Operation reading the review app's assistant settings.
pub const OP_REVIEW_ASSISTANT_SETTINGS_GET: &str = "review.assistant.settings.get";
/// Operation saving the review app's assistant settings.
pub const OP_REVIEW_ASSISTANT_SETTINGS_SAVE: &str = "review.assistant.settings.save";

/// The model and reasoning effort the review app gives new assistant conversations and Ask AI
/// card sessions: the latest choice made anywhere in the app. One record per review store.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAssistantSettings {
    /// Model-provider plugin of the model; Bcode's default provider when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_plugin_id: Option<String>,
    /// Model id; Bcode's default model when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Reasoning effort; the model's default when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// When the record was last saved, in milliseconds since the Unix epoch (set by the store).
    #[serde(default)]
    pub updated_at_ms: u64,
}

impl ReviewAssistantSettings {
    /// Whether the record chooses nothing (Bcode's default model and its default effort).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.model_id.is_none() && self.effort.is_none()
    }
}

/// Request for the review app's assistant settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReviewAssistantSettingsRequest {
    /// Repository path whose review store holds the record.
    pub repo_path: PathBuf,
}

/// Response to [`GetReviewAssistantSettingsRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReviewAssistantSettingsResponse {
    /// The record, once one was saved.
    pub settings: Option<ReviewAssistantSettings>,
}

/// Request to replace the review app's assistant settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveReviewAssistantSettingsRequest {
    /// Repository path whose review store holds the record.
    pub repo_path: PathBuf,
    /// The record; its timestamp is set by the store.
    pub settings: ReviewAssistantSettings,
}

/// Response to [`SaveReviewAssistantSettingsRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveReviewAssistantSettingsResponse {
    /// The recorded settings.
    pub settings: ReviewAssistantSettings,
}
