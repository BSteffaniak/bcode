//! Portable, versioned live execution-watch application contracts.
//!
//! Watch IDs are public diagnostic identities, not credentials. The transport must
//! authenticate every operation and bind it to the principal that opened the watch.
//! Nothing here grants execution control, canonical history access or durable replay.

use crate::{XrayEnvelope, XrayFinalization, XrayReadPayloadRequest};
use serde::{Deserialize, Serialize};

/// Execution-watch compatibility boundary. Unknown versions must be rejected.
pub const EXECUTION_WATCH_SCHEMA_VERSION: u16 = 1;

/// Explicit requested scope. Session/parent access never implies this authorization.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionWatchScope {
    /// Exact root of this observation (not necessarily the workflow's global root).
    pub root_run_id: String,
    /// Explicit opt-in to future canonical recursive descendants.
    pub recursive: bool,
    /// Separately authorized sensitive capture; never includes capture credentials.
    pub deep_capture: bool,
}

/// Explicit observation options for atomic fresh-run admission. The start request must
/// carry a stable caller-selected run ID. These booleans are requests, not grants.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionWatchStartOptions {
    /// Must equal one.
    pub schema_version: u16,
    /// Explicit recursive descendant capture opt-in.
    pub recursive: bool,
    /// Separate sensitive capture request.
    pub deep_capture: bool,
}

/// Open only after the application authorizes this exact scope for its principal.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OpenExecutionWatchRequest {
    /// Must equal one.
    pub schema_version: u16,
    /// Run, recursion and sensitive scope requiring explicit approval.
    pub scope: ExecutionWatchScope,
}

/// Readiness covers future pre-dispatch hooks, not past execution or durable resume.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionWatchReady {
    /// Compatibility boundary.
    pub schema_version: u16,
    /// Ephemeral identity, not a bearer credential.
    pub watch_id: String,
    /// Exact approved scope.
    pub scope: ExecutionWatchScope,
    /// Verified session associated with the granted root run. Not a descendant grant
    /// or a claim that future execution sessions have already been materialized.
    pub root_session_id: Option<String>,
    /// Initial live cursor (zero).
    pub cursor: u64,
    /// Whether prior execution was unobserved. False only for a fresh prepublication
    /// admission reservation, not ordinary attachment or restart.
    pub prior_execution_gap: bool,
    /// Maximum requested page item count.
    pub max_page_items: u32,
    /// Maximum returned serialized envelope bytes per page.
    pub max_page_bytes: u64,
}

/// Bounded destructive event paging. Retrying a consumed cursor is rejected, not replayed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReadExecutionWatchRequest {
    /// Compatibility boundary.
    pub schema_version: u16,
    /// Exact live segment.
    pub watch_id: String,
    /// Last returned cursor; zero initially.
    pub cursor: u64,
    /// Positive item bound, no larger than readiness policy.
    pub limit: u32,
}

/// A run/session-labelled engine envelope. Engine sequences are observation-local.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionWatchEvent {
    /// Canonically linked run.
    pub run_id: String,
    /// Exact canonical session ID, never a storage path.
    pub session_id: String,
    /// Bounded engine diagnostic envelope.
    pub envelope: XrayEnvelope,
}

/// Diagnostic lifecycle only; never an execution outcome.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionWatchStatus {
    /// Future pre-dispatch attachments enabled.
    Ready,
    /// Future attachments disabled; captures retained for drain/export.
    Finalized,
    /// Concurrent diagnostic capacity exceeded.
    Overloaded,
    /// Ownership changed; no continuity claim across owners.
    OwnershipChanged,
    /// Observation explicitly closed.
    Closed,
    /// Authorization withdrawn.
    Revoked,
}

/// Live diagnostic page. Empty does not imply completion; polling remains caller-owned.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionWatchPage {
    /// Compatibility boundary.
    pub schema_version: u16,
    /// Exact segment.
    pub watch_id: String,
    /// Required next cursor; not durable/reconnect-safe.
    pub next_cursor: u64,
    /// Diagnostic lifecycle.
    pub status: ExecutionWatchStatus,
    /// True for ordinary attachments/restarts; false only for a fresh admission barrier.
    pub prior_execution_gap: bool,
    /// Conservative daemon-wide skipped hooks since admission.
    pub contention_gaps: u64,
    /// Unsupported/unavailable lineage or capture links omitted.
    pub omitted_links: u64,
    /// Consumed oversized/foreign envelopes omitted in this page.
    pub omitted_events: u64,
    /// Bounded live events.
    pub events: Vec<ExecutionWatchEvent>,
}

/// Stop attachment/capture without stopping work; read cutoffs in bounded pages.
/// Repeated requests are safe at the same offset while this segment is retained.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinalizeExecutionWatchRequest {
    /// Compatibility boundary.
    pub schema_version: u16,
    /// Exact segment.
    pub watch_id: String,
    /// Cutoff page offset; zero starts finalization.
    pub offset: u64,
    /// Positive bounded page count.
    pub limit: u32,
}

/// Engine cutoff evidence, not canonical settlement, disk flush or task success.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionWatchCutoff {
    /// Exact run.
    pub run_id: String,
    /// Exact session.
    pub session_id: String,
    /// Engine cutoff; absent means finalization failed and evidence is incomplete.
    pub finalization: Option<XrayFinalization>,
}

/// Final capture evidence can be paged independently of destructive event delivery.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionWatchFinalEvidence {
    /// Compatibility boundary.
    pub schema_version: u16,
    /// Exact segment.
    pub watch_id: String,
    /// Diagnostic lifecycle before cutoff (overload/transfer remains visible).
    pub status: ExecutionWatchStatus,
    /// Prior execution gap status; false requires a fresh prepublication reservation.
    pub prior_execution_gap: bool,
    /// Conservative skipped hooks.
    pub contention_gaps: u64,
    /// Omitted canonical links/capture admissions.
    pub omitted_links: u64,
    /// Envelopes omitted across consumed pages (not an engine loss counter).
    pub omitted_events: u64,
    /// Next offset, absent at end of retained cutoffs.
    pub next_offset: Option<u64>,
    /// Bounded engine cutoff evidence. Drain corresponding final envelopes separately.
    pub cutoffs: Vec<ExecutionWatchCutoff>,
}

/// Export bounded retained payload bytes under the exact run/session/deep grant.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExportExecutionWatchPayloadRequest {
    /// Compatibility boundary.
    pub schema_version: u16,
    /// Exact segment.
    pub watch_id: String,
    /// Exact linked run; no inference from payload IDs.
    pub run_id: String,
    /// Engine-owned range request; observation/session must match retained capture.
    pub payload: XrayReadPayloadRequest,
}

/// Revoke and delete captures/pending data; no execution cancellation or mutation.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RevokeExecutionWatchRequest {
    /// Compatibility boundary.
    pub schema_version: u16,
    /// Exact segment owned by the authenticated principal.
    pub watch_id: String,
}

/// Acknowledges capture removal, not execution settlement.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionWatchRevoked {
    /// Compatibility boundary.
    pub schema_version: u16,
    /// Removed diagnostic identity.
    pub watch_id: String,
}

/// Fixed secret-safe application failure codes; transports map these without private errors.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionWatchError {
    /// Malformed/unsupported request or stale destructive cursor.
    InvalidRequest,
    /// No matching authenticated scope; does not disclose watch existence.
    Forbidden,
    /// Concurrent diagnostic capacity exhausted.
    Capacity,
    /// Capture or registry unavailable.
    Unavailable,
}
