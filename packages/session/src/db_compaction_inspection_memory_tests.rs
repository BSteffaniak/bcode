use super::*;

#[tokio::test]
async fn memory_compaction_inspection_is_bounded_safe_and_non_mutating() {
    let manager = crate::SessionManager::default();
    let session = manager
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    let observation = observation();
    let recorded = manager
        .append_event(session.id, observation.clone().into_event_kind(1).unwrap())
        .await
        .unwrap();
    for _ in 0..300 {
        manager
            .append_event(
                session.id,
                SessionEventKind::SystemMessage {
                    text: "PRIVATE unrelated".into(),
                },
            )
            .await
            .unwrap();
    }
    let query = SessionCompactionInspectionQuery {
        before: None,
        limit: 200,
    };
    let before = manager.session_history(session.id).await.unwrap();
    let page = manager
        .session_compaction_inspection_page(session.id, query)
        .await
        .unwrap();
    assert_eq!(page.scanned_events, 200);
    assert!(page.entries.is_empty());
    assert!(page.has_more);
    assert!(!page.degraded);
    let page = manager
        .session_compaction_inspection_page(
            session.id,
            SessionCompactionInspectionQuery {
                before: page.next_cursor,
                limit: 200,
            },
        )
        .await
        .unwrap();
    assert!(!page.has_more);
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].sequence, recorded.sequence);
    assert_eq!(page.entries[0].progress.as_ref(), Some(&observation));
    assert_eq!(manager.session_history(session.id).await.unwrap(), before);
    manager
        .append_compaction_checkpoint(session.id, "PRIVATE key", Some("PRIVATE payload".into()))
        .await
        .unwrap();
    let page = manager
        .session_compaction_inspection_page(session.id, query)
        .await
        .unwrap();
    assert!(!serde_json::to_string(&page).unwrap().contains("PRIVATE"));
    assert_eq!(
        page.entries.last().unwrap().blocker,
        Some(CompactionInspectionBlocker::CheckpointNotValidated)
    );
    manager
        .append_event(
            session.id,
            SessionEventKind::TraceEvent {
                trace: Box::new(bcode_session_models::SessionTraceEvent {
                    timestamp_ms: 1,
                    turn_id: None,
                    phase: SessionTracePhase::ContextCompactionDiagnostic,
                    payload: SessionTracePayload::ContextCompaction {
                        reason: "PRIVATE".into(),
                        projected_context_chars: 0,
                        compacted: false,
                        message: Some("x".repeat(3 * 1024 * 1024)),
                    },
                }),
            },
        )
        .await
        .unwrap();
    let degraded = manager
        .session_compaction_inspection_page(session.id, query)
        .await
        .unwrap();
    assert!(degraded.degraded);
    assert_eq!(
        degraded.entries.last().unwrap().blocker,
        Some(CompactionInspectionBlocker::InspectionBudgetExceeded)
    );
}
