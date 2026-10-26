//! Versioned operational compaction evidence, never model-context content.

use crate::{
    SessionEvent, SessionEventKind, SessionTraceEvent, SessionTracePayload, SessionTracePhase,
};
use serde::{Deserialize, Serialize};

/// Reserved canonical provider-event marker. Its detail is a checkpoint envelope.
pub const COMPACTION_SYNTHESIS_CHECKPOINT_EVENT: &str = "compaction_synthesis_checkpoint_v1";
/// Maximum UTF-8 bytes in an exact operation/source namespace key.
pub const MAX_COMPACTION_CHECKPOINT_KEY_BYTES: usize = 1024;
/// Maximum UTF-8 bytes in the opaque, host-versioned source/carry payload.
pub const MAX_COMPACTION_CHECKPOINT_PAYLOAD_BYTES: usize = 256 * 1024;

/// Durable operational evidence. A retired record remains the latest evidence.
///
/// The host owns source identity, consumed allowances and payload compatibility.
/// Session storage does not interpret, reset, or publish the payload as model context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionSynthesisCheckpoint {
    /// Envelope compatibility version; only 1 is supported.
    pub version: u32,
    /// Stable host-selected source/policy identity, independent of model turns.
    pub operation_key: String,
    /// Versioned host payload, or `None` for explicit retirement.
    pub host_payload: Option<String>,
}

impl CompactionSynthesisCheckpoint {
    /// Validate the bounded exact lookup key without exposing it in diagnostics.
    ///
    /// # Errors
    /// Returns a content-free error for empty or oversized keys.
    pub fn validate_key(key: &str) -> Result<(), String> {
        if key.is_empty() || key.len() > MAX_COMPACTION_CHECKPOINT_KEY_BYTES {
            return Err("invalid compaction checkpoint key size".into());
        }
        Ok(())
    }

    /// Validate envelope compatibility and byte bounds, not host payload semantics.
    ///
    /// # Errors
    /// Returns a content-free error for unsupported versions or invalid sizes.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported compaction checkpoint version".into());
        }
        Self::validate_key(&self.operation_key)?;
        if self
            .host_payload
            .as_ref()
            .is_some_and(|payload| payload.len() > MAX_COMPACTION_CHECKPOINT_PAYLOAD_BYTES)
        {
            return Err("compaction checkpoint payload exceeds byte limit".into());
        }
        Ok(())
    }

    /// Encode operational evidence without assigning model-turn authority.
    ///
    /// # Errors
    /// Returns a content-free error for unsupported or oversized envelopes.
    pub fn into_event_kind(self, timestamp_ms: u64) -> Result<SessionEventKind, String> {
        self.validate()?;
        let detail = serde_json::to_string(&self)
            .map_err(|_| "cannot encode compaction checkpoint".to_owned())?;
        Ok(SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms,
                turn_id: None,
                phase: SessionTracePhase::ModelProviderEvent,
                payload: SessionTracePayload::ProviderEvent {
                    event_type: COMPACTION_SYNTHESIS_CHECKPOINT_EVENT.into(),
                    detail: Some(detail),
                },
            }),
        })
    }

    /// Decode and validate reserved canonical evidence, including retirement.
    ///
    /// # Errors
    /// Returns a content-free error for malformed, oversized, unsupported evidence
    /// or evidence incorrectly claiming model-turn authority. Unrelated events return `None`.
    pub fn from_event(event: &SessionEvent) -> Result<Option<Self>, String> {
        Self::from_event_kind(&event.kind)
    }

    /// Validate an event kind before a canonical append.
    ///
    /// # Errors
    /// Returns the same content-free validation errors as [`Self::from_event`].
    pub fn from_event_kind(kind: &SessionEventKind) -> Result<Option<Self>, String> {
        let SessionEventKind::TraceEvent { trace } = kind else {
            return Ok(None);
        };
        let SessionTracePayload::ProviderEvent { event_type, detail } = &trace.payload else {
            return Ok(None);
        };
        if event_type != COMPACTION_SYNTHESIS_CHECKPOINT_EVENT {
            return Ok(None);
        }
        if trace.turn_id.is_some() || trace.phase != SessionTracePhase::ModelProviderEvent {
            return Err("invalid compaction checkpoint authority".into());
        }
        let detail = detail
            .as_ref()
            .ok_or_else(|| "missing compaction checkpoint envelope".to_owned())?;
        // JSON escaping can expand every UTF-8 input byte to six ASCII bytes.
        if detail.len()
            > 6 * (MAX_COMPACTION_CHECKPOINT_KEY_BYTES + MAX_COMPACTION_CHECKPOINT_PAYLOAD_BYTES)
                + 256
        {
            return Err("compaction checkpoint envelope exceeds byte limit".into());
        }
        let checkpoint: Self = serde_json::from_str(detail)
            .map_err(|_| "invalid compaction checkpoint envelope".to_owned())?;
        checkpoint.validate()?;
        Ok(Some(checkpoint))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_checkpoint_bounds_versions_and_authority() {
        let mut checkpoint = CompactionSynthesisCheckpoint {
            version: 1,
            operation_key: "source/policy".into(),
            host_payload: Some("carry".into()),
        };
        let kind = checkpoint.clone().into_event_kind(0).expect("encode");
        assert_eq!(
            CompactionSynthesisCheckpoint::from_event_kind(&kind).expect("decode"),
            Some(checkpoint.clone())
        );
        let SessionEventKind::TraceEvent { mut trace } = kind else {
            panic!("trace")
        };
        trace.turn_id = Some("not-model-authority".into());
        assert!(
            CompactionSynthesisCheckpoint::from_event_kind(&SessionEventKind::TraceEvent { trace })
                .is_err()
        );
        checkpoint.version = 2;
        assert!(checkpoint.validate().is_err());
        checkpoint.version = 1;
        checkpoint.host_payload = Some("x".repeat(MAX_COMPACTION_CHECKPOINT_PAYLOAD_BYTES + 1));
        assert!(checkpoint.validate().is_err());
        checkpoint.host_payload = None;
        checkpoint.operation_key = "x".repeat(MAX_COMPACTION_CHECKPOINT_KEY_BYTES + 1);
        assert!(checkpoint.validate().is_err());
        assert!(CompactionSynthesisCheckpoint::validate_key("").is_err());
    }
}
