//! Exact source/receipt reads exercise storage, never implementation text.
use super::*;

#[tokio::test]
async fn exact_facts_reject_wrong_sequences_missing_source_and_mismatched_identity() {
    let temp = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let (db, initial) = prepare_terminal_fixture(temp.path(), session).await;
    let source = db.durable_interaction_facts(1, 2).await.unwrap();
    let digest = source.source_digest().unwrap();
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert!(db.durable_interaction_facts(2, 1).await.is_err());
    assert!(db.durable_interaction_facts(0, 2).await.is_err());
    assert!(db.durable_interaction_facts(1, 999).await.is_err());
    assert!(db.durable_interaction_facts(u64::MAX, 2).await.is_err());
    let mut changed = source.clone();
    if let SessionEventKind::PositionedToolCallRequested {
        producer_plugin_id, ..
    } = &mut changed.invocation.kind
    {
        *producer_plugin_id = Some("foreign".into());
    }
    assert!(changed.source_digest().is_err());
    let mut oversized = source.clone();
    if let SessionEventKind::PositionedToolCallRequested { arguments_json, .. } =
        &mut oversized.invocation.kind
    {
        *arguments_json = "x".repeat(MAX_SOURCE_BYTES + 1);
    }
    assert!(oversized.source_digest().is_err());
    assert!(!format!("{source:?}").contains("arguments_json"));
    let mut changed = source.clone();
    changed.generation += 1;
    assert_eq!(changed.source_digest().unwrap(), digest);
    if let SessionEventKind::PositionedToolCallRequested { arguments_json, .. } =
        &mut changed.invocation.kind
    {
        *arguments_json = "{\"input\":2}".into();
    }
    assert_ne!(changed.source_digest().unwrap(), digest);
    if let SessionEventKind::ToolExchangeRequested { request } = &mut changed.exchange.kind {
        request.producer_id = "foreign".into();
    }
    assert!(changed.source_digest().is_err());
    let mut other = source.exchange.clone();
    other.sequence = 4;
    if let SessionEventKind::ToolExchangeRequested { request } = &mut other.kind {
        request.invocation_id = "other".into();
    }
    db.append_event(&other).await.unwrap();
    assert!(db.durable_interaction_facts(1, 4).await.is_err());
    // Publication is off: ordinary event append does not advance this staged projection.
    assert!(
        db.durable_interaction_operation(&initial.key, "initial")
            .await
            .is_err()
    );
    db.close().await.unwrap();
    let db = crate::db::SessionDb::open_turso_in_root(session, temp.path())
        .await
        .unwrap();
    assert_eq!(
        db.durable_interaction_facts(1, 2)
            .await
            .unwrap()
            .source_digest()
            .unwrap(),
        digest
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn exact_operation_survives_restart_and_conflicting_receipt_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let (db, initial) = prepare_terminal_fixture(temp.path(), session).await;
    let stored = db
        .durable_interaction_operation(&initial.key, "initial")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.operation, initial);
    assert_eq!(stored.operation.request_digest, "a".repeat(64));
    assert!(
        db.durable_interaction_operation(&initial.key, "missing")
            .await
            .unwrap()
            .is_none()
    );
    let mut wrong_key = initial.key.clone();
    wrong_key.interaction_id = "other".into();
    assert!(
        db.durable_interaction_operation(&wrong_key, "initial")
            .await
            .unwrap()
            .is_none()
    );
    db.close().await.unwrap();
    let db = crate::db::SessionDb::open_turso_in_root(session, temp.path())
        .await
        .unwrap();
    assert_eq!(
        db.durable_interaction_operation(&initial.key, "initial")
            .await
            .unwrap(),
        Some(stored.clone())
    );
    let mut bad = stored.receipt;
    bad.revision += 1;
    db.database()
        .update("durable_interaction_operations")
        .value("receipt", serde_json::to_string(&bad).unwrap())
        .where_eq("operation_key", operation_key(&initial).unwrap())
        .execute(db.database())
        .await
        .unwrap();
    assert!(
        db.durable_interaction_operation(&initial.key, "initial")
            .await
            .is_err()
    );
    db.close().await.unwrap();
}
