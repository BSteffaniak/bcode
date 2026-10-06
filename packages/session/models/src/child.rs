//! Portable recursive session relationship and activity contracts.

use crate::{SessionId, SessionSummary};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Idempotent creation of a fresh ordinary child in the parent's verified state location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionChildCreateRequest {
    /// Caller-retained identity; retries must use the same immutable request.
    pub child_id: SessionId,
    /// Immutable direct parent; unrelated to workflow provenance or derivation.
    pub parent_id: SessionId,
    /// Initial explicit title, not the subsequently editable title.
    pub name: Option<String>,
    /// Authorized working directory; no parent context is implicitly copied.
    pub working_directory: PathBuf,
}

/// Bounded ID-ordered candidate query for direct children.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionChildQuery {
    /// Direct parent to select.
    pub parent_id: SessionId,
    /// Exclusive candidate cursor, including candidates that were not children.
    pub after: Option<SessionId>,
    /// Candidate work budget, between one and 128.
    pub limit: usize,
}

/// Direct children observed within one bounded candidate window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionChildPage {
    /// Verified children; a sparse window may contain none.
    pub children: Vec<SessionSummary>,
    /// Last examined candidate when additional candidates remain.
    pub next_after: Option<SessionId>,
    /// Whether catalog discovery is trustworthy, independently of pagination.
    /// False does not prove that an unobserved child does not exist.
    pub complete: bool,
    /// Reasons discovery or individual candidates could not be verified.
    pub issues: Vec<String>,
}

/// Relationship scope requested for activity observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionActivityScope {
    /// Only the queried session.
    Own,
    /// Only its immediate children, in addition to its own activity.
    DirectChildren,
}

/// Bounded non-owning activity query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivityQuery {
    /// Requested relationship scope.
    pub scope: SessionActivityScope,
    /// Exclusive continuation identity.
    pub after: Option<SessionId>,
    /// Observation work budget.
    pub limit: usize,
}

/// Observed activity facts; multiple concurrent states may be present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionActivityState {
    /// Work is executing.
    Running,
    /// Work is awaiting admission.
    Queued,
    /// A permission decision is pending.
    PermissionWait,
    /// User input is pending.
    InputWait,
    /// No active work is observed.
    Idle,
    /// A failure is observed.
    Failed,
    /// Work was interrupted.
    Interrupted,
    /// State cannot be verified.
    Unknown,
}

/// Non-owning portable activity for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivity {
    /// Session observed.
    pub session_id: SessionId,
    /// Immutable direct parent, when verified.
    pub parent_id: Option<SessionId>,
    /// Concurrent activity facts, not a scalar winner chosen by presentation.
    pub states: Vec<SessionActivityState>,
    /// Optional own-session evidence; never an aggregate subtree count.
    #[serde(default)]
    pub evidence: Option<SessionActivityEvidence>,
}

/// Bounded normalized activity evidence for one session only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivityEvidence {
    /// Observed executing operations.
    pub running: u32,
    /// Observed queued operations.
    pub queued: u32,
    /// Observed pending permission decisions.
    pub permission_waits: u32,
    /// Observed pending user-input requests.
    pub input_waits: u32,
    /// Whether these own-session counts cover the complete observation.
    pub counts_complete: bool,
    /// Canonical event checkpoint of this observation, when available.
    pub observed_event_sequence: Option<u64>,
    /// Optional bounded normalized runtime label, not raw tool arguments.
    pub current_operation: Option<String>,
}

/// Observation coverage, separate from the presence of a pagination cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionActivityCoverage {
    /// Requested scope was completely observed.
    Complete,
    /// Valid bounded observations omit part of the requested scope.
    Partial,
    /// Missing, stale, damaged, or unverifiable state prevents trustworthy coverage.
    Degraded,
}

/// Portable activity observation; not a durable stream or reconnect checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivityPage {
    /// Queried session's own activity.
    pub own: SessionActivity,
    /// Bounded descendant observations.
    pub descendants: Vec<SessionActivity>,
    /// Exclusive continuation identity, if further observations are available.
    pub next_after: Option<SessionId>,
    /// Truthful requested-scope coverage.
    pub coverage: SessionActivityCoverage,
    /// Observation wall-clock timestamp, not an execution generation.
    pub observed_at_ms: u64,
    /// Reasons for partial or degraded coverage.
    pub issues: Vec<String>,
}

/// Result of explicit relationship maintenance; canonical parent checkpoint is authoritative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionChildReconciliation {
    /// Parent whose canonical relationship set was reconciled.
    pub parent_id: SessionId,
    /// Verified direct children observed by maintenance.
    pub verified_children: usize,
}
