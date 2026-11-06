//! Recovery diagnostics must not masquerade as synthesis activation terminals.
use super::*;
use bcode_session_models::SessionEvent;

fn event(sequence: u64, phase: SessionTracePhase, reason: &str) -> SessionEvent {
    SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence,
        timestamp_ms: sequence,
        session_id: SessionId::new(),
        provenance: None,
        kind: SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: sequence,
                turn_id: Some("compaction-owner".into()),
                phase,
                payload: SessionTracePayload::ContextCompaction {
                    reason: reason.into(),
                    projected_context_chars: 0,
                    compacted: false,
                    message: Some("context compaction failed: invalid or incompatible compaction checkpoint; boundary unchanged".into()),
                },
            }),
        },
    }
}

#[test]
fn recovery_report_requires_a_closed_activation_and_matching_owner() {
    let started = event(
        1,
        SessionTracePhase::ContextCompactionStarted,
        "compaction_started",
    );
    let finished = event(
        2,
        SessionTracePhase::ContextCompactionFinished,
        "compaction_finished",
    );
    let report = event(
        3,
        SessionTracePhase::ContextCompactionFinished,
        "compaction_recovery_failed",
    );
    for abandon in [false, true] {
        validate_settled_attempts(
            &[started.clone(), finished.clone(), report.clone()],
            abandon,
        )
        .unwrap();
        assert!(validate_settled_attempts(std::slice::from_ref(&report), abandon).is_err());
        assert!(validate_settled_attempts(&[started.clone(), report.clone()], abandon).is_err());
        assert!(
            validate_settled_attempts(
                &[started.clone(), finished.clone(), finished.clone()],
                abandon
            )
            .is_err()
        );
        let mut foreign = report.clone();
        if let SessionEventKind::TraceEvent { trace } = &mut foreign.kind {
            trace.turn_id = Some("another-owner".into());
        }
        assert!(
            validate_settled_attempts(&[started.clone(), finished.clone(), foreign], abandon)
                .is_err()
        );
        let mut published = report.clone();
        if let SessionEventKind::TraceEvent { trace } = &mut published.kind
            && let SessionTracePayload::ContextCompaction { compacted, .. } = &mut trace.payload
        {
            *compacted = true;
        }
        assert!(
            validate_settled_attempts(&[started.clone(), finished.clone(), published], abandon)
                .is_err()
        );
    }
}
