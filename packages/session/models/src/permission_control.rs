//! Bounded session-scoped permission observations and conditional decisions.

use crate::{PermissionSummary, SessionId};
use serde::{Deserialize, Serialize};

/// Maximum checkpoints in one scoped page or complete conditional batch.
pub const MAX_SCOPED_PERMISSION_CHECKPOINTS: usize = 128;
/// Maximum serialized summary bytes retained in one scoped observation.
pub const MAX_SCOPED_PERMISSION_BYTES: usize = 256 * 1024;

/// A pending checkpoint identity, invalidated by removal/re-registration or payload changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionCheckpointIdentity {
    /// Exact pending permission ID.
    pub permission_id: String,
    /// Opaque server-issued digest; clients must copy it unchanged.
    pub checkpoint_digest: String,
}

/// One bounded, inspectable checkpoint with its conditional-decision identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedPermissionCheckpoint {
    /// Exact conditional identity.
    pub identity: PermissionCheckpointIdentity,
    /// Current source-owned summary, addressed to the requested session.
    pub permission: PermissionSummary,
}

/// Session-scoped indexed lookup. A batch lookup is always complete or rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedPermissionQuery {
    /// Source session or authenticated notification destination.
    pub session_id: SessionId,
    /// Inspect exactly this batch; requires no cursor and the maximum page limit.
    pub batch_id: Option<String>,
    /// Exclusive permission-ID cursor for session paging, not a durable resume token.
    pub after_permission_id: Option<String>,
    /// Positive page bound, at most `MAX_SCOPED_PERMISSION_CHECKPOINTS`.
    pub limit: usize,
}

/// A bounded lookup result. An incomplete batch never supplies decision authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScopedPermissionLookup {
    /// Current page or complete batch, ordered by permission ID.
    Ready {
        /// Current exact checkpoints.
        checkpoints: Vec<ScopedPermissionCheckpoint>,
        /// Exclusive cursor when another checkpoint was observed.
        next_after: Option<String>,
    },
    /// Invalid bounds or malformed identity.
    Invalid,
    /// The batch is missing, already decided, foreign, or missing members.
    Unavailable,
    /// A complete batch or individual summary cannot fit the bounded response.
    LimitExceeded,
}

/// One conditional permission decision. This grants no generic execution capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionalPermissionDecision {
    /// Session that observed these checkpoints.
    pub session_id: SessionId,
    /// Complete batch identity, or `None` for one non-batched checkpoint.
    pub batch_id: Option<String>,
    /// Exact, distinct permission IDs and digests from a scoped observation.
    pub expected: Vec<PermissionCheckpointIdentity>,
    /// One-time decision applied to the entire validated set.
    pub approved: bool,
    /// Remember only a supported individual decision; never a batch.
    pub remember: bool,
}

/// Conditional admission result, not a durable exactly-once receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConditionalPermissionResult {
    /// Exact set atomically claimed; normal canonical completion follows.
    Resolved {
        /// Number successfully completed by the permission owner.
        resolved: usize,
    },
    /// Atomic durable batch approval is unavailable. No checkpoint was claimed
    /// or decision latched; callers must not fall back to sequential approval.
    BatchApprovalUnavailable,
    /// Exact set was claimed, but canonical completion failed. No batch member
    /// is released with approval; inspect fresh state before any further action.
    CompletionFailed,
    /// Identity, payload, destination, membership or pending state changed. No decision latched.
    Stale,
    /// Invalid bounds, duplicate identities, unsupported remembering or partial batch.
    Invalid,
}
