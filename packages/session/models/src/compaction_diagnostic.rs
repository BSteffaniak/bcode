//! Portable body-free compaction diagnostic evidence, not execution authority.

use serde::{Deserialize, Serialize};

/// Size and digest of a complete response; absent when transport did not return complete text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionResponseDiagnostic {
    pub bytes: usize,
    pub sha256: String,
}

/// Versioned observation of one activation within an operation. Private checkpoint keys,
/// response bodies and arbitrary provider/error messages are never included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionDiagnostic {
    pub version: u32,
    /// SHA-256 of the exact operation namespace; absent before operation resolution.
    pub operation_id: Option<String>,
    pub activation_id: String,
    pub request_id: Option<String>,
    pub contract: Option<String>,
    /// Closed vocabulary validated before publication and after decoding.
    pub stage: String,
    pub outcome: String,
    pub reason: Option<String>,
    pub chunk: Option<usize>,
    pub attempt: Option<usize>,
    pub response: Option<CompactionResponseDiagnostic>,
    /// Present only on a transactionally published commit observation.
    pub marker_sequence: Option<u64>,
}

fn digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl CompactionDiagnostic {
    /// Reject unsupported versions, unbounded identifiers and open-ended diagnostic text.
    #[must_use]
    pub fn valid(&self) -> bool {
        self.version == 1
            && self.contract.as_deref().is_none_or(|contract| {
                matches!(
                    contract,
                    "update" | "replace" | "recollection" | "consolidation"
                )
            })
            && self.operation_id.as_deref().is_none_or(digest)
            && uuid::Uuid::parse_str(&self.activation_id).is_ok()
            && self.request_id.as_ref().is_none_or(|id| {
                !id.is_empty()
                    && id.len() <= 256
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
            && self.chunk.is_none_or(|chunk| chunk > 0)
            && self.attempt.is_none_or(|attempt| attempt > 0)
            && self
                .response
                .as_ref()
                .is_none_or(|response| digest(&response.sha256))
            && self.reason.as_deref().is_none_or(|reason| {
                matches!(
                    reason,
                    "wire_budget"
                        | "invalid_json"
                        | "schema"
                        | "invalid_shape"
                        | "unsupported_format"
                        | "empty_continuation"
                        | "carry_budget"
                        | "invalid_source"
                        | "invalid_supersession"
                        | "ambiguous_group_supersession"
                        | "unauthorized_scope_change"
                        | "missing_scope"
                        | "invalid_reconciliation"
                        | "incomplete_output"
                        | "previous_attempt_unavailable"
                )
            })
            && match (self.stage.as_str(), self.outcome.as_str()) {
                ("operation", "started" | "failed" | "blocked" | "cancelled") => {
                    self.response.is_none()
                        && self.reason.is_none()
                        && self.marker_sequence.is_none()
                }
                (
                    "request",
                    "dispatch_requested" | "returned" | "failed" | "cancelled" | "blocked",
                ) => {
                    self.request_id.is_some()
                        && self.marker_sequence.is_none()
                        && self.reason.is_none()
                }
                ("contract" | "capacity" | "merge" | "consolidation", "rejected") => {
                    self.reason.is_some() && self.marker_sequence.is_none()
                }
                (
                    "publication",
                    "validating" | "committing" | "failed" | "blocked" | "cancelled",
                ) => {
                    self.operation_id.is_some()
                        && self.marker_sequence.is_none()
                        && self.reason.is_none()
                        && self.response.is_none()
                }
                ("summary", "ready") => {
                    self.response.is_some()
                        && self.reason.is_none()
                        && self.marker_sequence.is_none()
                }
                ("publication", "committed") => {
                    self.operation_id.is_some()
                        && self.marker_sequence.is_some()
                        && self.reason.is_none()
                        && self.response.is_none()
                }
                _ => false,
            }
    }

    /// Encode validated evidence using the normal trace contract.
    ///
    /// # Errors
    /// Rejects invalid diagnostic values or serialization failures.
    pub fn into_trace(
        self,
        timestamp_ms: u64,
        turn_id: Option<String>,
    ) -> Result<crate::SessionTraceEvent, String> {
        if !self.valid() {
            return Err("invalid compaction diagnostic".into());
        }
        Ok(crate::SessionTraceEvent {
            timestamp_ms,
            turn_id,
            phase: crate::SessionTracePhase::ModelProviderEvent,
            payload: crate::SessionTracePayload::ProviderEvent {
                event_type: "compaction_diagnostic_v1".into(),
                detail: Some(
                    serde_json::to_string(&self)
                        .map_err(|_| "cannot encode compaction diagnostic")?,
                ),
            },
        })
    }
}
