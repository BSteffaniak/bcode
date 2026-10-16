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
    /// Latest canonical authorization; the original accepted payload is never rewritten.
    #[serde(default)]
    pub recovery_authorization: Option<SubmissionRecoveryAuthorization>,
}

/// Canonical authorization overlay for a pending submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionRecoveryAuthorization {
    pub operation_id: String,
    pub event_sequence: u64,
    pub descriptor: SubmissionExecutionDescriptor,
}

impl SubmissionRecord {
    /// Latest authorized execution context, or the immutable acceptance context.
    #[must_use]
    pub fn effective_execution_descriptor(&self) -> Option<&SubmissionExecutionDescriptor> {
        self.recovery_authorization
            .as_ref()
            .map(|a| &a.descriptor)
            .or(self.submission.execution_descriptor.as_ref())
    }
}

/// One acceptance-ordered page of currently pending input.
///
/// This is a bounded observation of a mutable queue, not a snapshot or resumable
/// event stream. Rescan from the beginning to observe earlier pending items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSubmissionsPage {
    /// Canonical submission records, including actual target and recovery blockers.
    pub records: Vec<SubmissionRecord>,
    /// Exclusive acceptance-sequence cursor when another page was observed.
    pub next_after: Option<u64>,
    /// Whether the bounded read observed another pending record.
    pub has_more: bool,
}

/// Secret-free recovery identity.
///
/// Version 2 carries a verified durable recovery recipe; version 1 identities remain
/// recognizable but cannot reconstruct lost execution context.
/// Profile fields are names, never credentials or provider-native payloads.
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
    /// Versioned config-owned source references and original account-selection policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_recipe: Option<Box<SubmissionRecoveryRecipe>>,
}

/// Portable source provenance. Paths are references, never configuration or credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmissionConfigSources {
    pub cwd: std::path::PathBuf,
    pub discovered_paths: Vec<std::path::PathBuf>,
    pub exclusive_config_path: Option<std::path::PathBuf>,
    pub base_config_path: Option<std::path::PathBuf>,
    pub env_config_path: Option<std::path::PathBuf>,
    pub cli_config_path: Option<std::path::PathBuf>,
    pub active_context: Option<String>,
    pub model_profile: Option<String>,
    pub inline_overrides: bool,
}

/// Versioned, bounded, secret-free configuration reference; config owns replay validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmissionConfigSource {
    pub schema_version: u32,
    pub sources: SubmissionConfigSources,
    pub execution_digest: String,
    pub source_presence: Vec<bool>,
}

impl SubmissionConfigSource {
    /// Validate the portable envelope without opening files or interpreting config.
    /// # Errors
    /// Rejects unknown versions, dynamic overrides, malformed digests and excessive paths.
    pub fn validate(&self) -> Result<(), &'static str> {
        let s = &self.sources;
        if self.schema_version != 1
            || s.inline_overrides
            || self.execution_digest.len() != 64
            || !self.execution_digest.bytes().all(|b| b.is_ascii_hexdigit())
            || s.discovered_paths.len() > 64
            || self.source_presence.len() > 64
            || [&s.active_context, &s.model_profile]
                .iter()
                .any(|v| v.as_ref().is_some_and(|v| v.is_empty() || v.len() > 256))
        {
            return Err("invalid configuration recovery reference");
        }
        let paths = std::iter::once(&s.cwd)
            .chain(s.discovered_paths.iter())
            .chain(s.exclusive_config_path.iter())
            .chain(s.base_config_path.iter())
            .chain(s.env_config_path.iter())
            .chain(s.cli_config_path.iter());
        for path in paths {
            if !path.is_absolute()
                || path.as_os_str().len() > 4096
                || path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err("invalid configuration recovery reference");
            }
        }
        Ok(())
    }
}

/// Secret-free configuration source references. `config_source` is validated by the config owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmissionRecoveryRecipe {
    pub version: u32,
    pub config_source: SubmissionConfigSource,
    pub model_profile: Option<String>,
    pub auth_profile: Option<String>,
    pub auth_pool: Option<String>,
}

/// Exact recipe shown to a caller before authorizing a pending item's recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmissionResumeConfirmation {
    pub version: u32,
    pub submission_id: SubmissionId,
    pub expected_accepted_sequence: u64,
    pub descriptor: SubmissionExecutionDescriptor,
}

/// Durable result of one targeted operation, including an identical operation retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionRecoveryResult {
    pub record: SubmissionRecord,
    pub duplicate: bool,
}

/// Bounded oldest-item queue observation. This is not an event stream or durable snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionQueueStatus {
    pub oldest: Option<SubmissionRecord>,
    pub blocker: Option<SubmissionQueueBlocker>,
}

/// Application-owned blocker; clients must not independently infer execution safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionQueueBlocker {
    MissingContext,
    IncompatibleContext,
    InterruptedTurn,
    /// Neither targeted resume nor targeted cancel is authorized by this session API.
    WorkflowAuthorityRequired,
    SteeringTargetUnavailable,
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
