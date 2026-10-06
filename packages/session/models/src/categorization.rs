//! Portable contract for transient, plugin-owned advisory session categorization.

use crate::SessionId;
use serde::{Deserialize, Serialize};

/// Plugin service; not a session persistence or execution operation.
pub const CATEGORIZATION_INTERFACE_ID: &str = "bcode.session-categorization/v1";
/// Categorize one newly activated request without reading session history.
pub const OP_CATEGORIZE: &str = "categorize";
/// Maximum UTF-8 source bytes supplied transiently to the plugin.
pub const MAX_CATEGORIZATION_SOURCE_BYTES: usize = 8192;
/// Maximum encoded service request, allowing JSON escaping of the bounded source.
pub const MAX_CATEGORIZATION_REQUEST_BYTES: usize = 6 * MAX_CATEGORIZATION_SOURCE_BYTES + 1024;

/// One transient trigger. Do not log, persist, or echo its source text.
///
/// Hosts supply only the newest activated normal request, truncated on a UTF-8 boundary to
/// `MAX_CATEGORIZATION_SOURCE_BYTES`. Interpretation and sanitization belong to the plugin;
/// provider requests must contain only plugin-owned allowlisted signals, not this source text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategorizationRequest {
    /// Session owning the disposable advisory result.
    pub session_id: SessionId,
    /// Canonical activation sequence. Hosts reject stale results.
    pub source_sequence: u64,
    /// Bounded request-only input. Never provider state or durable session content.
    pub newest_request: String,
}

/// Advisory model labels, not task, permission, authority, or completion state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCategorization {
    /// Source activation sequence.
    pub source_sequence: u64,
    /// Fixed-vocabulary task label supplied by the plugin.
    pub task: String,
    /// Fixed-vocabulary domain label supplied by the plugin.
    pub domain: String,
}

/// Absence retains baseline behavior and any existing advisory state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategorizationResponse {
    /// Optional labels only; never source text, extracted facts, or provider metadata.
    pub categorization: Option<SessionCategorization>,
}
