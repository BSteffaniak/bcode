use super::*;
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, CompactionInspectionBudgets, CompactionInspectionCount,
    CompactionInspectionResume, SessionId, SessionTraceEvent,
};

#[path = "db_compaction_inspection_restart_tests.rs"]
mod restart;

#[path = "db_compaction_inspection_memory_tests.rs"]
mod memory;

fn event(id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
    SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence,
        timestamp_ms: 1,
        session_id: id,
        provenance: None,
        kind,
    }
}

fn trace(
    id: SessionId,
    sequence: u64,
    phase: SessionTracePhase,
    payload: SessionTracePayload,
) -> SessionEvent {
    event(
        id,
        sequence,
        SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: 1,
                turn_id: None,
                phase,
                payload,
            }),
        },
    )
}

fn observation() -> CompactionProgressObservation {
    CompactionProgressObservation {
        version: 1,
        phase: CompactionInspectionPhase::Synthesizing,
        chunks: Some(CompactionInspectionCount { done: 2, total: 5 }),
        resume: CompactionInspectionResume::ValidatedCheckpoint,
        budgets: Some(CompactionInspectionBudgets {
            provider_requests: Some(CompactionInspectionCount { done: 3, total: 12 }),
            source_events: None,
            decoded_bytes: None,
        }),
        blocker: None,
    }
}

#[test]
fn normalization_never_exposes_checkpoint_or_message_or_guesses_counters() {
    let id = SessionId::new();
    let checkpoint = event(
        id,
        0,
        CompactionSynthesisCheckpoint {
            version: 1,
            operation_key: "PRIVATE-operation-key".into(),
            host_payload: Some("PRIVATE-provider-secret-summary".into()),
        }
        .into_event_kind(0)
        .unwrap(),
    );
    let retained = normalize(&checkpoint).unwrap();
    assert_eq!(
        retained.blocker,
        Some(CompactionInspectionBlocker::CheckpointNotValidated)
    );
    assert!(retained.progress.is_none());
    let diagnostic = trace(
        id,
        1,
        SessionTracePhase::ContextCompactionDiagnostic,
        SessionTracePayload::ContextCompaction {
            reason: "PRIVATE-reason".into(),
            projected_context_chars: 400,
            compacted: false,
            message: Some("PRIVATE chunk 99/100 resumed".into()),
        },
    );
    let diagnostic = normalize(&diagnostic).unwrap();
    assert!(diagnostic.phase.is_none());
    assert!(diagnostic.progress.is_none());
    let retired = event(
        id,
        2,
        CompactionSynthesisCheckpoint {
            version: 1,
            operation_key: "PRIVATE-operation-key".into(),
            host_payload: None,
        }
        .into_event_kind(0)
        .unwrap(),
    );
    assert_eq!(
        normalize(&retired).unwrap().phase,
        Some(CompactionInspectionPhase::CheckpointRetired)
    );
    let json = serde_json::to_string(&[retained, diagnostic]).unwrap();
    assert!(!json.contains("PRIVATE"));
    assert!(!json.contains("99"));
}

#[test]
fn structured_progress_versions_counts_and_restart_are_explicit() {
    let mut progress = observation();
    let encoded_event = event(
        SessionId::new(),
        0,
        progress.clone().into_event_kind(7).unwrap(),
    );
    assert_eq!(
        normalize(&encoded_event).unwrap().progress,
        Some(progress.clone())
    );
    assert_eq!(
        decode_observation(Some(&progress.encode().unwrap())),
        Ok(progress.clone())
    );
    progress.resume = CompactionInspectionResume::Restarted;
    assert_eq!(
        decode_observation(Some(&progress.encode().unwrap()))
            .unwrap()
            .resume,
        CompactionInspectionResume::Restarted
    );
    progress.version = 2;
    progress.phase = CompactionInspectionPhase::Validating;
    assert!(progress.validate().is_ok());
    progress.phase = CompactionInspectionPhase::Publishing;
    assert!(progress.validate().is_ok());
    progress.version = 3;
    assert_eq!(
        progress.validate(),
        Err(CompactionInspectionBlocker::UnsupportedFormat)
    );
    progress.version = 1;
    progress.phase = CompactionInspectionPhase::Synthesizing;
    progress.chunks = Some(CompactionInspectionCount { done: 6, total: 5 });
    assert_eq!(
        progress.validate(),
        Err(CompactionInspectionBlocker::InvalidEvidence)
    );
    assert!(decode_observation(Some("{\"version\":99,\"PRIVATE\":\"secret\"}")).is_err());
    assert!(
        decode_observation(Some(
            &"x".repeat(MAX_COMPACTION_PROGRESS_OBSERVATION_BYTES + 1)
        ))
        .is_err()
    );
    let future = trace(
        SessionId::new(),
        9,
        SessionTracePhase::ModelProviderEvent,
        SessionTracePayload::ProviderEvent {
            event_type: "compaction_progress_v99".into(),
            detail: Some("PRIVATE".into()),
        },
    );
    assert_eq!(
        normalize(&future).unwrap().blocker,
        Some(CompactionInspectionBlocker::UnsupportedFormat)
    );
}

#[tokio::test]
async fn inspection_is_bounded_reopens_without_mutation_and_keeps_terminal_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let db = SessionDb::open_turso_in_root(id, temp.path())
        .await
        .unwrap();
    let started = trace(
        id,
        0,
        SessionTracePhase::ContextCompactionStarted,
        SessionTracePayload::ContextCompaction {
            reason: "PRIVATE".into(),
            projected_context_chars: 1,
            compacted: false,
            message: None,
        },
    );
    db.append_event(&started).await.unwrap();
    append_unrelated_history(&db, id).await;
    let finished = trace(
        id,
        10_001,
        SessionTracePhase::ContextCompactionFinished,
        SessionTracePayload::ContextCompaction {
            reason: "PRIVATE".into(),
            projected_context_chars: 1,
            compacted: true,
            message: Some("PRIVATE-summary".into()),
        },
    );
    db.append_event(&finished).await.unwrap();
    // A stale diagnostic cannot rewrite the previously terminal evidence.
    let stale = trace(
        id,
        10_002,
        SessionTracePhase::ContextCompactionDiagnostic,
        SessionTracePayload::ContextCompaction {
            reason: "PRIVATE".into(),
            projected_context_chars: 1,
            compacted: false,
            message: Some("started again 7/10".into()),
        },
    );
    db.append_event(&stale).await.unwrap();
    let query = SessionCompactionInspectionQuery {
        before: None,
        limit: 2,
    };
    let before = db.compaction_inspection_page(query).await.unwrap();
    assert_eq!(before.scanned_events, 2);
    assert!(before.has_more);
    assert_eq!(
        before.entries[0].phase,
        Some(CompactionInspectionPhase::Finished)
    );
    assert!(before.entries[1].phase.is_none());
    assert!(!serde_json::to_string(&before).unwrap().contains("PRIVATE"));
    let previous = db
        .compaction_inspection_page(SessionCompactionInspectionQuery {
            before: before.next_cursor,
            limit: 2,
        })
        .await
        .unwrap();
    assert_eq!(previous.scanned_events, 1);
    assert!(!previous.has_more);
    assert_eq!(previous.entries[0].sequence, 0);
    // Query-only calls leave canonical rows byte-identical.
    let rows_before = db
        .db
        .select("events")
        .columns(&["event_seq", "payload"])
        .sort("event_seq", SortDirection::Asc)
        .execute(&**db.db)
        .await
        .unwrap();
    let again = db.compaction_inspection_page(query).await.unwrap();
    let rows_after = db
        .db
        .select("events")
        .columns(&["event_seq", "payload"])
        .sort("event_seq", SortDirection::Asc)
        .execute(&**db.db)
        .await
        .unwrap();
    assert_eq!(format!("{rows_before:?}"), format!("{rows_after:?}"));
    assert_eq!(before, again);
    drop(db);
    let db = SessionDb::open_existing_turso_in_root(id, temp.path())
        .await
        .unwrap();
    assert_eq!(before, db.compaction_inspection_page(query).await.unwrap());
    assert!(
        db.compaction_inspection_page(SessionCompactionInspectionQuery {
            before: None,
            limit: 0
        })
        .await
        .is_err()
    );
}

#[tokio::test]
async fn unrelated_traces_are_incomplete_not_missing_and_empty_history_is_distinct() {
    let temp = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let db = SessionDb::open_turso_in_root(id, temp.path())
        .await
        .unwrap();
    let query = SessionCompactionInspectionQuery {
        before: None,
        limit: 1,
    };
    let empty = db.compaction_inspection_page(query).await.unwrap();
    assert!(empty.entries.is_empty());
    assert!(!empty.has_more);
    assert!(!empty.degraded);
    for sequence in 0..3 {
        db.append_event(&trace(
            id,
            sequence,
            SessionTracePhase::ModelProviderEvent,
            SessionTracePayload::ProviderEvent {
                event_type: "unrelated".into(),
                detail: Some("PRIVATE".into()),
            },
        ))
        .await
        .unwrap();
    }
    let incomplete = db.compaction_inspection_page(query).await.unwrap();
    assert!(incomplete.entries.is_empty());
    assert!(incomplete.has_more);
    assert_eq!(incomplete.scanned_events, 1);
    db.append_event(&trace(
        id,
        3,
        SessionTracePhase::ModelProviderEvent,
        SessionTracePayload::ProviderEvent {
            event_type: "compaction_progress_v99".into(),
            detail: Some("PRIVATE".into()),
        },
    ))
    .await
    .unwrap();
    let degraded = db.compaction_inspection_page(query).await.unwrap();
    assert!(degraded.degraded);
    assert_eq!(
        degraded.entries[0].blocker,
        Some(CompactionInspectionBlocker::UnsupportedFormat)
    );
    assert!(
        !serde_json::to_string(&degraded)
            .unwrap()
            .contains("PRIVATE")
    );
    db.append_event(&trace(
        id,
        4,
        SessionTracePhase::ContextCompactionDiagnostic,
        SessionTracePayload::ContextCompaction {
            reason: "PRIVATE".into(),
            projected_context_chars: 0,
            compacted: false,
            message: Some("x".repeat(3 * 1024 * 1024)),
        },
    ))
    .await
    .unwrap();
    let oversized = db.compaction_inspection_page(query).await.unwrap();
    assert!(oversized.degraded);
    assert_eq!(
        oversized.entries[0].blocker,
        Some(CompactionInspectionBlocker::InspectionBudgetExceeded)
    );
}

async fn append_unrelated_history(db: &SessionDb, id: SessionId) {
    // Large unrelated canonical history must not consume the compaction candidate allowance.
    for sequence in 1..=10_000 {
        db.append_event(&event(
            id,
            sequence,
            SessionEventKind::ModelTurnFinished {
                turn_id: format!("unrelated-{sequence}"),
                outcome: bcode_session_models::ModelTurnOutcome::Cancelled,
                message: None,
            },
        ))
        .await
        .unwrap();
    }
}
