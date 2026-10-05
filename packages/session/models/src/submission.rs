//! Durable prompt acceptance, distinct from independent turn execution.

use crate::{ClientId, TurnAdmissionMetadata, TurnId};
use serde::{Deserialize, Serialize};

/// Caller-chosen stable identity for idempotent prompt submission.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubmissionId(pub String);

impl SubmissionId {
    /// Submission identity for an independently admitted queued turn.
    #[must_use]
    pub fn for_queued_turn(turn_id: &TurnId) -> Self {
        Self(format!("queued-turn:{turn_id}"))
    }
}

/// Accepted prompt payload. Acceptance alone does not add model context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSubmission {
    pub id: SubmissionId,
    pub client_id: ClientId,
    pub text: String,
    pub admission: TurnAdmissionMetadata,
    pub requested_placement: SubmissionPlacement,
    pub target: SubmissionTarget,
    #[serde(default)]
    pub execution_descriptor: Option<SubmissionExecutionDescriptor>,
}

/// Requested scheduling policy, retained independently of its resolved target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionPlacement {
    FollowUp,
    Steer,
}

/// Durable resolved scheduling decision; recovery must not choose another target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionTarget {
    OwnTurn { turn_id: TurnId },
    Steering { turn_id: TurnId },
}

impl SubmissionTarget {
    /// Exact turn authorized at acceptance.
    #[must_use]
    pub const fn turn_id(&self) -> &TurnId {
        match self {
            Self::OwnTurn { turn_id } | Self::Steering { turn_id } => turn_id,
        }
    }
}

/// Durable acceptance receipt; deliberately contains no execution/work identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionReceipt {
    pub submission_id: SubmissionId,
    pub accepted_event_sequence: u64,
}

/// A submission's terminal lifecycle is independent of its bound turn's outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionStatus {
    Pending,
    Activated {
        turn_id: TurnId,
        conversation_sequence: u64,
    },
    Cancelled {
        event_sequence: u64,
    },
}

/// Exact activated context and canonical generation captured together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivatedConversationSnapshot {
    /// Canonical tail captured in the same actor operation as `events`.
    pub through_sequence: u64,
    /// Activated model context in conversation order; accepted pending input is excluded.
    pub events: Vec<crate::SessionEvent>,
}

/// Bounded-query result containing the payload even after context compaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionRecord {
    pub receipt: SubmissionReceipt,
    pub submission: PromptSubmission,
    pub status: SubmissionStatus,
    #[serde(default)]
    pub context_observation: Option<SubmissionContextObservation>,
}

/// Secret-free recovery identity. Only version 1 is writable; runtime resolves and compares it.
/// Profile fields are names, never credentials, paths, environment or provider-native payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionExecutionDescriptor {
    pub version: u32,
    pub provider: String,
    pub model: String,
    pub reasoning: Option<crate::TurnReasoningOptions>,
    pub model_profile: Option<String>,
    pub auth_profile: Option<String>,
    /// Runtime-owned digest of normalized, secret-free execution configuration.
    pub config_identity: String,
}

/// Actionable nonterminal reason why pending input cannot currently execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionContextIssue {
    /// Restore the named execution context before retrying recovery.
    Unavailable,
    /// Restore compatible execution configuration before retrying recovery.
    Incompatible,
}

/// Latest durable context recovery observation, independent of submission lifecycle status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionContextObservation {
    pub issue: SubmissionContextIssue,
    pub event_sequence: u64,
}
