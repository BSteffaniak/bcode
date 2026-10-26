//! Presentation-only copies of operational compaction evidence.
//!
//! Canonical history/around/projection source pages and export APIs retain exact evidence.
//! Semantic observation must not publish the host's opaque carry.
//! Redaction never interprets a payload, changes source identity, or retires a checkpoint.

use std::borrow::Cow;

use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionTraceEvent, SessionTracePayload,
};

fn is_checkpoint(payload: &SessionTracePayload) -> bool {
    matches!(payload, SessionTracePayload::ProviderEvent { event_type, .. }
        if event_type.starts_with("compaction_synthesis_checkpoint_"))
}

/// Borrow ordinary events and construct a payload-free copy only for checkpoints.
/// The private payload is not cloned into the presentation copy or parsed.
#[must_use]
pub fn presentation_event(event: &SessionEvent) -> Cow<'_, SessionEvent> {
    let SessionEventKind::TraceEvent { trace } = &event.kind else {
        return Cow::Borrowed(event);
    };
    if !is_checkpoint(&trace.payload) {
        return Cow::Borrowed(event);
    }
    let SessionTracePayload::ProviderEvent { event_type, .. } = &trace.payload else {
        return Cow::Borrowed(event);
    };
    Cow::Owned(SessionEvent {
        schema_version: event.schema_version,
        sequence: event.sequence,
        timestamp_ms: event.timestamp_ms,
        session_id: event.session_id,
        provenance: event.provenance.clone(),
        kind: SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: trace.timestamp_ms,
                turn_id: trace.turn_id.clone(),
                phase: trace.phase,
                payload: SessionTracePayload::ProviderEvent {
                    event_type: event_type.clone(),
                    detail: None,
                },
            }),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, CompactionSynthesisCheckpoint, SessionId,
    };

    #[test]
    fn presentation_copy_preserves_identity_without_mutating_canonical_evidence() {
        let canonical = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 42,
            timestamp_ms: 123,
            session_id: SessionId::new(),
            provenance: None,
            kind: CompactionSynthesisCheckpoint {
                version: 1,
                operation_key: "private-namespace-canary".into(),
                host_payload: Some("private-carry-canary".into()),
            }
            .into_event_kind(456)
            .unwrap(),
        };
        let original = canonical.clone();
        for event_type in [
            "compaction_synthesis_checkpoint_v1",
            "compaction_synthesis_checkpoint_v999",
        ] {
            let mut source = canonical.clone();
            let SessionEventKind::TraceEvent { trace } = &mut source.kind else {
                unreachable!()
            };
            trace.payload = SessionTracePayload::ProviderEvent {
                event_type: event_type.into(),
                detail: Some("malformed private-carry-canary private-namespace-canary".into()),
            };
            let projected = presentation_event(&source);
            assert_eq!(projected.sequence, source.sequence);
            assert_eq!(projected.schema_version, source.schema_version);
            assert_eq!(projected.timestamp_ms, source.timestamp_ms);
            assert_eq!(projected.session_id, source.session_id);
            assert_eq!(projected.provenance, source.provenance);
            let json = serde_json::to_string(&projected).unwrap();
            assert!(!json.contains("private-carry-canary"));
            assert!(!json.contains("private-namespace-canary"));
            assert!(json.contains(event_type));
            assert!(
                serde_json::to_string(&source)
                    .unwrap()
                    .contains("private-carry-canary")
            );
        }
        assert_eq!(canonical, original);
        assert!(
            CompactionSynthesisCheckpoint::from_event(&canonical)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn unrelated_provider_evidence_is_unchanged() {
        let mut event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 0,
            session_id: SessionId::new(),
            provenance: None,
            kind: CompactionSynthesisCheckpoint {
                version: 1,
                operation_key: "namespace".into(),
                host_payload: None,
            }
            .into_event_kind(0)
            .unwrap(),
        };
        let SessionEventKind::TraceEvent { trace } = &mut event.kind else {
            unreachable!()
        };
        trace.payload = SessionTracePayload::ProviderEvent {
            event_type: "ordinary-provider-event".into(),
            detail: Some("ordinary diagnostic".into()),
        };
        let original = event.clone();
        assert!(matches!(presentation_event(&event), Cow::Borrowed(_)));
        assert_eq!(event, original);
    }
}
