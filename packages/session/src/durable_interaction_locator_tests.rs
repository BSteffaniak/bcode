//! Checkpointed identity locator behavior; no canonical history scans.
use super::*;

#[tokio::test]
async fn invocation_id_source_uses_exact_request_locator_and_rejects_corrupt_binding() {
    let root = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let (db, _) = prepare_terminal_fixture(root.path(), session).await;
    let facts = db
        .durable_interaction_source("call", "exchange")
        .await
        .unwrap();
    assert_eq!(facts.invocation.sequence, 1);
    assert_eq!(facts.exchange.sequence, 2);
    assert_eq!(facts, db.durable_interaction_facts(1, 2).await.unwrap());
    for (invocation, exchange) in [
        ("", "exchange"),
        ("call", ""),
        ("missing", "exchange"),
        ("call", "missing"),
    ] {
        assert!(
            db.durable_interaction_source(invocation, exchange)
                .await
                .is_err()
        );
    }
    let oversized = "x".repeat(bcode_session_models::MAX_DURABLE_INTERACTION_ID_BYTES + 1);
    assert!(
        db.durable_interaction_source(&oversized, "exchange")
            .await
            .is_err()
    );
    assert!(
        db.durable_interaction_source("call", &oversized)
            .await
            .is_err()
    );
    db.close().await.unwrap();
    let db = crate::db::SessionDb::open_turso_in_root(session, root.path())
        .await
        .unwrap();
    assert_eq!(
        db.durable_interaction_source("call", "exchange")
            .await
            .unwrap(),
        facts
    );
    db.database()
        .update("tool_runs")
        .value("event_seq_start", 0_i64)
        .where_eq("tool_call_id", "call")
        .execute(db.database())
        .await
        .unwrap();
    assert!(
        db.durable_interaction_source("call", "exchange")
            .await
            .is_err()
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn exchange_locator_fresh_append_restart_and_duplicate_rejection() {
    let root = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let (db, _) = prepare_terminal_fixture(root.path(), session).await;
    let source = db
        .durable_interaction_exchange_facts(1, "exchange")
        .await
        .unwrap();
    assert_eq!(source.exchange.sequence, 2);
    assert_eq!(source, db.durable_interaction_facts(1, 2).await.unwrap());
    assert!(
        db.durable_interaction_exchange_facts(1, "missing")
            .await
            .is_err()
    );
    assert!(
        db.durable_interaction_exchange_facts(0, "exchange")
            .await
            .is_err()
    );
    let mut duplicate = source.exchange.clone();
    duplicate.sequence = 4;
    assert!(db.append_event(&duplicate).await.is_err());
    assert_eq!(db.last_event_sequence().await.unwrap(), Some(3));
    db.close().await.unwrap();
    let db = crate::db::SessionDb::open_turso_in_root(session, root.path())
        .await
        .unwrap();
    assert_eq!(
        db.durable_interaction_exchange_facts(1, "exchange")
            .await
            .unwrap(),
        source
    );
    let mut resolution = source.exchange.clone();
    resolution.sequence = 4;
    resolution.kind = SessionEventKind::ToolExchangeResolved {
        event: bcode_session_models::ToolExchangeResolutionEvent {
            invocation_id: "call".into(),
            exchange_id: "exchange".into(),
            resolution: bcode_session_models::ToolExchangeResolution::Cancelled,
        },
    };
    db.append_event(&resolution).await.unwrap();
    assert_eq!(
        db.durable_interaction_exchange_facts(1, "exchange")
            .await
            .unwrap()
            .exchange,
        source.exchange
    );
    duplicate.sequence = 5;
    assert!(db.append_event(&duplicate).await.is_err());
    db.close().await.unwrap();
}

#[tokio::test]
async fn exchange_locator_rejects_corrupt_missing_or_stale_projection_without_repair() {
    let root = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let (db, _) = prepare_terminal_fixture(root.path(), session).await;
    let key = exchange_key("call", "exchange").unwrap();
    db.database()
        .update("interaction_exchange_locators")
        .value("request_seq", 0_i64)
        .where_eq("exchange_key", key.clone())
        .execute(db.database())
        .await
        .unwrap();
    assert!(
        db.durable_interaction_exchange_facts(1, "exchange")
            .await
            .is_err()
    );
    db.database()
        .update("interaction_exchange_locators")
        .value("request_seq", 2_i64)
        .where_eq("exchange_key", key)
        .execute(db.database())
        .await
        .unwrap();
    db.database()
        .update("projection_checkpoints")
        .value("last_event_seq", 2_i64)
        .where_eq("projection_name", "interaction_exchange_locators")
        .execute(db.database())
        .await
        .unwrap();
    assert!(
        db.durable_interaction_exchange_facts(1, "exchange")
            .await
            .is_err()
    );
    db.database()
        .delete("projection_checkpoints")
        .where_eq("projection_name", "interaction_exchange_locators")
        .execute(db.database())
        .await
        .unwrap();
    assert!(
        db.durable_interaction_exchange_facts(1, "exchange")
            .await
            .is_err()
    );
    let mut append = event(session, 4, operation());
    append.kind = SessionEventKind::SessionRenamed {
        name: Some("not committed".into()),
    };
    assert!(db.append_event(&append).await.is_err());
    assert_eq!(db.last_event_sequence().await.unwrap(), Some(3));
    db.close().await.unwrap();
}

#[tokio::test]
async fn exchange_locator_missing_index_or_table_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let (db, _) = prepare_terminal_fixture(root.path(), session).await;
    switchy::database::schema::drop_index(
        "idx_interaction_exchange_identity",
        "interaction_exchange_locators",
    )
    .execute(db.database())
    .await
    .unwrap();
    assert!(
        db.durable_interaction_exchange_facts(1, "exchange")
            .await
            .is_err()
    );
    switchy::database::schema::drop_table("interaction_exchange_locators")
        .execute(db.database())
        .await
        .unwrap();
    assert!(
        db.durable_interaction_exchange_facts(1, "exchange")
            .await
            .is_err()
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn exchange_locator_rejects_competing_corrupt_identity_rows() {
    let root = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let (db, _) = prepare_terminal_fixture(root.path(), session).await;
    db.database()
        .insert("interaction_exchange_locators")
        .value("exchange_key", "corrupt-duplicate")
        .value("invocation_id", "call")
        .value("exchange_id", "exchange")
        .value("request_seq", 2_i64)
        .execute(db.database())
        .await
        .unwrap();
    assert!(
        db.durable_interaction_exchange_facts(1, "exchange")
            .await
            .is_err()
    );
    db.close().await.unwrap();
}
