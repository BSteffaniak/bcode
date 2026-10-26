use super::*;

#[tokio::test]
async fn validated_progress_survives_storage_restart_without_restore() {
    let temp = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let db = SessionDb::open_turso_in_root(id, temp.path())
        .await
        .unwrap();
    let progress = observation();
    db.append_event(&trace(
        id,
        0,
        SessionTracePhase::ModelProviderEvent,
        SessionTracePayload::ProviderEvent {
            event_type: COMPACTION_PROGRESS_OBSERVATION_EVENT.into(),
            detail: Some(progress.encode().unwrap()),
        },
    ))
    .await
    .unwrap();
    let query = SessionCompactionInspectionQuery {
        before: None,
        limit: 1,
    };
    let page = db.compaction_inspection_page(query).await.unwrap();
    assert_eq!(page.entries[0].progress.as_ref(), Some(&progress));
    assert!(!page.degraded);
    assert!(!page.has_more);
    drop(db);
    let db = SessionDb::open_existing_turso_in_root(id, temp.path())
        .await
        .unwrap();
    assert_eq!(page, db.compaction_inspection_page(query).await.unwrap());
}
