//! Storage foundation tests; production publication remains compatibility-gated.
#[path = "durable_interaction_batch_tests.rs"]
mod batch_tests;
#[path = "durable_interaction_codec_tests.rs"]
mod codec_tests;
#[path = "durable_interaction_guard_tests.rs"]
mod guard_tests;
#[path = "durable_interaction_locator_tests.rs"]
mod locator_tests;
#[path = "durable_interaction_read_tests.rs"]
mod read_tests;
use super::*;
use bcode_session_models::{DurableInteractionPayload, ToolInvocationResultRecord};
use switchy::{
    database::schema::{Column, DataType, create_table},
    schema::runner::MigrationRunner,
};

fn operation() -> DurableInteractionOperation {
    DurableInteractionOperation {
        version: DURABLE_INTERACTION_VERSION,
        key: DurableInteractionKey {
            turn_id: "turn".into(),
            invocation_id: "call".into(),
            interaction_id: "question".into(),
        },
        operation_id: "initial".into(),
        request_digest: "a".repeat(64),
        exchange_request_event_sequence: 0,
        producer_plugin_id: "producer".into(),
        request_event_sequence: 0,
        expected_revision: None,
        transition: DurableInteractionTransition::Checkpoint {
            payload: DurableInteractionPayload {
                contract_id: "question".into(),
                contract_version: 1,
                json: "{\"selected\":[]}".into(),
            },
        },
    }
}

fn complete(previous: &DurableInteractionRecord) -> DurableInteractionOperation {
    let mut operation = operation();
    operation.operation_id = "complete".into();
    operation.expected_revision = Some(previous.receipt.revision);
    operation.transition = DurableInteractionTransition::Complete {
        outcome: DurableInteractionPayload {
            contract_id: "answer".into(),
            contract_version: 1,
            json: "{\"answer\":1}".into(),
        },
        exchange_resolution: bcode_session_models::ToolExchangeResolutionEvent {
            invocation_id: "call".into(),
            exchange_id: "exchange".into(),
            resolution: bcode_session_models::ToolExchangeResolution::Responded {
                payload: serde_json::json!({"answer":1}),
            },
        },
        result: Box::new(ToolInvocationResultRecord {
            invocation_id: "call".into(),
            model_output: "one".into(),
            is_error: false,
            content: vec![],
            presentation: None,
            result: None,
        }),
    };
    operation
}

fn event(
    session_id: SessionId,
    sequence: u64,
    operation: DurableInteractionOperation,
) -> SessionEvent {
    SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        session_id,
        sequence,
        timestamp_ms: 1,
        provenance: None,
        kind: SessionEventKind::DurableInteractionCommitted { operation },
    }
}

async fn database(path: &std::path::Path) -> Box<dyn Database> {
    let db = crate::db_connection::init_turso_local_with_retry(path)
        .await
        .unwrap();
    create_table("projection_checkpoints")
        .if_not_exists(true)
        .column(Column {
            name: "projection_name".into(),
            data_type: DataType::Text,
            nullable: false,
            auto_increment: false,
            default: None,
        })
        .primary_key("projection_name")
        .column(Column {
            name: "last_event_seq".into(),
            data_type: DataType::Int,
            nullable: false,
            auto_increment: false,
            default: None,
        })
        .column(Column {
            name: "projection_version".into(),
            data_type: DataType::Int,
            nullable: false,
            auto_increment: false,
            default: None,
        })
        .column(Column {
            name: "updated_at_ms".into(),
            data_type: DataType::Int,
            nullable: false,
            auto_increment: false,
            default: None,
        })
        .execute(&*db)
        .await
        .unwrap();
    create_table("events")
        .if_not_exists(true)
        .column(Column {
            name: "event_seq".into(),
            data_type: DataType::Int,
            nullable: false,
            auto_increment: false,
            default: None,
        })
        .primary_key("event_seq")
        .execute(&*db)
        .await
        .unwrap();
    let mut source = crate::current_schema::CurrentMigrationSource::default();
    crate::current_schema::add_durable_interaction_migrations(&mut source);
    MigrationRunner::new(Box::new(source))
        .with_table_name("durable_test_migrations")
        .run(&*db)
        .await
        .unwrap();
    db
}

#[test]
fn initial_checkpoint_is_create_only_and_revisions_are_exact() {
    let initial = operation();
    let first = advance(None, &initial, 1).unwrap();
    assert!(matches!(
        first.recovery_state(),
        bcode_session_models::DurableInteractionRecoveryState::DurableWait
    ));
    assert_eq!(first.receipt.revision, 1);
    assert!(advance(Some(first.clone()), &initial, 2).is_err());
    let mut edit = initial;
    edit.expected_revision = Some(1);
    edit.operation_id = "edit".into();
    let second = advance(Some(first.clone()), &edit, 2).unwrap();
    assert_eq!(second.receipt.revision, 2);
    assert!(advance(Some(second), &edit, 3).is_err());
    edit.producer_plugin_id = "other".into();
    assert!(advance(Some(first), &edit, 2).is_err());
    edit.expected_revision = Some(0);
    assert!(advance(None, &edit, 1).is_err());
}

#[test]
fn terminal_state_cannot_be_reopened_and_publication_is_single_assignment() {
    let first = advance(None, &operation(), 1).unwrap();
    let completion = complete(&first);
    let terminal = advance(Some(first), &completion, 2).unwrap();
    assert_eq!(terminal.tool_result_event_sequence, Some(4));
    let mut edit = operation();
    edit.expected_revision = Some(2);
    assert!(advance(Some(terminal.clone()), &edit, 4).is_err());
    let mut publication = completion;
    publication.operation_id = "publish".into();
    publication.expected_revision = Some(2);
    publication.transition = DurableInteractionTransition::PublishContinuation {
        publication_id: "continuation".into(),
    };
    let published = advance(Some(terminal), &publication, 5).unwrap();
    assert_eq!(published.tool_result_event_sequence, Some(4));
    publication.expected_revision = Some(3);
    assert!(advance(Some(published), &publication, 6).is_err());
}

#[test]
fn resource_caps_reject_without_truncation() {
    let mut op = operation();
    op.version += 1;
    assert!(op.validate().is_err());
    op = operation();
    if let DurableInteractionTransition::Checkpoint { payload } = &mut op.transition {
        payload.json = format!(
            "\"{}\"",
            "x".repeat(bcode_session_models::MAX_DURABLE_INTERACTION_PAYLOAD_BYTES)
        );
    }
    assert!(op.validate().is_err());
    let mut record = advance(None, &operation(), 1).unwrap();
    record.retained_operation_bytes = MAX_DURABLE_INTERACTION_RETAINED_BYTES;
    op = operation();
    op.expected_revision = Some(1);
    assert!(advance(Some(record.clone()), &op, 2).is_err());
    record.retained_operation_bytes = 0;
    record.receipt.revision = MAX_DURABLE_INTERACTION_OPERATIONS;
    op.expected_revision = Some(MAX_DURABLE_INTERACTION_OPERATIONS);
    assert!(advance(Some(record), &op, 2).is_err());
}

#[tokio::test]
async fn generic_append_rejects_durable_events_in_memory_and_sql_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    for manager in [
        SessionManager::default(),
        SessionManager::persistent(root.path()).unwrap(),
    ] {
        let session = manager
            .create_session(None, root.path().to_path_buf())
            .await
            .unwrap();
        let before = manager
            .current_session_generation(session.id)
            .await
            .unwrap();
        assert!(
            manager
                .append_event(
                    session.id,
                    SessionEventKind::DurableInteractionCommitted {
                        operation: operation()
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(
            manager
                .current_session_generation(session.id)
                .await
                .unwrap(),
            before
        );
    }
    let id = SessionId::new();
    let db = crate::db::SessionDb::open_turso_in_root(id, root.path())
        .await
        .unwrap();
    let mut created = event(id, 0, operation());
    created.kind = SessionEventKind::SessionCreated {
        name: None,
        working_directory: root.path().to_path_buf(),
    };
    db.append_event(&created).await.unwrap();
    let rejected = event(id, 1, operation());
    assert!(db.append_event(&rejected).await.is_err());
    assert!(
        db.append_event_batch(&[(rejected.clone(), None)])
            .await
            .is_err()
    );
    assert!(
        db.append_durable_interaction_batch(&[(rejected, None)], guard_tests::guard(0).as_ref())
            .await
            .is_err()
    );
    assert_eq!(db.last_event_sequence().await.unwrap(), Some(0));
    db.close().await.unwrap();
}

#[test]
fn publication_fails_closed_until_coordinated_upgrade() {
    assert!(ensure_publication_enabled().is_err());
}

#[tokio::test]
async fn lost_ack_reopens_original_receipt_and_conflicting_reuse_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("checkpoint.db");
    let session = SessionId::new();
    let op = operation();
    let db = database(&path).await;
    let tx = db.begin_transaction().await.unwrap();
    project(&*tx, &event(session, 1, op.clone())).await.unwrap();
    tx.commit().await.unwrap();
    db.close().await.unwrap();
    let db = database(&path).await;
    let receipt = retry(&*db, &op).await.unwrap().unwrap();
    assert_eq!(receipt.event_sequence, 1);
    let mut conflict = op.clone();
    if let DurableInteractionTransition::Checkpoint { payload } = &mut conflict.transition {
        payload.json = "{\"selected\":[1]}".into();
    }
    assert!(retry(&*db, &conflict).await.is_err());
    let mut digest_conflict = op.clone();
    digest_conflict.request_digest = "b".repeat(64);
    assert!(retry(&*db, &digest_conflict).await.is_err());
    let mut conflict = op.clone();
    conflict.expected_revision = Some(1);
    assert!(retry(&*db, &conflict).await.is_err());
    let initial = lookup(&*db, &op.key).await.unwrap().unwrap();
    let completion = complete(&initial);
    let tx = db.begin_transaction().await.unwrap();
    project(&*tx, &event(session, 2, completion.clone()))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(retry(&*db, &completion).await.unwrap().is_none());
    assert_eq!(lookup(&*db, &op.key).await.unwrap().unwrap(), initial);
    db.close().await.unwrap();
}

#[tokio::test]
async fn competing_cas_writers_cannot_both_advance_and_original_receipts_survive() {
    let temp = tempfile::tempdir().unwrap();
    let db = database(&temp.path().join("cas.db")).await;
    let session = SessionId::new();
    let initial = operation();
    project(&*db, &event(session, 1, initial.clone()))
        .await
        .unwrap();
    let mut first = initial.clone();
    first.operation_id = "writer-a".into();
    first.expected_revision = Some(1);
    let mut second = first.clone();
    second.operation_id = "writer-b".into();
    let first_tx = db.begin_transaction().await.unwrap();
    project(&*first_tx, &event(session, 2, first))
        .await
        .unwrap();
    first_tx.commit().await.unwrap();
    let second_tx = db.begin_transaction().await.unwrap();
    assert!(
        project(&*second_tx, &event(session, 3, second))
            .await
            .is_err()
    );
    second_tx.rollback().await.unwrap();
    assert_eq!(retry(&*db, &initial).await.unwrap().unwrap().revision, 1);
    db.close().await.unwrap();
}

async fn prepare_terminal_fixture(
    root: &std::path::Path,
    session: SessionId,
) -> (crate::db::SessionDb, DurableInteractionOperation) {
    let db = crate::db::SessionDb::open_turso_in_root(session, root)
        .await
        .unwrap();
    let mut source = crate::current_schema::CurrentMigrationSource::default();
    crate::current_schema::add_durable_interaction_migrations(&mut source);
    MigrationRunner::new(Box::new(source))
        .with_table_name("durable_fixture_migrations")
        .run(db.database())
        .await
        .unwrap();
    let mut created = event(session, 0, operation());
    created.kind = SessionEventKind::SessionCreated {
        name: None,
        working_directory: root.to_path_buf(),
    };
    db.append_event(&created).await.unwrap();
    let mut request = created.clone();
    request.sequence = 1;
    request.kind = SessionEventKind::PositionedToolCallRequested {
        turn_id: "turn".into(),
        output_position: bcode_session_models::TurnOutputPosition::new(0),
        tool_call_id: "call".into(),
        tool_name: "question".into(),
        arguments_json: "{}".into(),
        producer_plugin_id: Some("producer".into()),
        working_directory: Some(root.to_path_buf()),
    };
    db.append_event(&request).await.unwrap();
    let mut initial = operation();
    initial.request_event_sequence = 1;
    initial.exchange_request_event_sequence = 2;
    let mut exchange = request.clone();
    exchange.sequence = 2;
    exchange.kind = SessionEventKind::ToolExchangeRequested {
        request: bcode_session_models::ToolExchangeRequest {
            invocation_id: "call".into(),
            exchange_id: "exchange".into(),
            producer_id: "producer".into(),
            schema: "question".into(),
            schema_version: 1,
            payload: serde_json::json!({}),
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        },
    };
    db.append_event(&exchange).await.unwrap();
    validate_exchange_binding(db.database(), session, &initial, 3)
        .await
        .unwrap();
    let mut wrong_binding = initial.clone();
    wrong_binding.producer_plugin_id = "other".into();
    assert!(
        validate_exchange_binding(db.database(), session, &wrong_binding, 3)
            .await
            .is_err()
    );
    wrong_binding = initial.clone();
    wrong_binding.key.invocation_id = "other".into();
    assert!(
        validate_exchange_binding(db.database(), session, &wrong_binding, 3)
            .await
            .is_err()
    );
    let initial_event = event(session, 3, initial.clone());
    let tx = db.database().begin_transaction().await.unwrap();
    crate::db::append_event_projections(&*tx, &initial_event, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (db, initial)
}

fn terminal_events(
    session: SessionId,
    completion: &DurableInteractionOperation,
) -> (SessionEvent, SessionEvent, SessionEvent) {
    let interaction = event(session, 4, completion.clone());
    let DurableInteractionTransition::Complete {
        result,
        exchange_resolution,
        ..
    } = &completion.transition
    else {
        unreachable!()
    };
    let terminal = SessionEvent {
        sequence: 6,
        kind: SessionEventKind::ToolInvocationResultRecorded {
            record: result.as_ref().clone(),
        },
        ..interaction.clone()
    };
    let resolution = SessionEvent {
        sequence: 5,
        kind: SessionEventKind::ToolExchangeResolved {
            event: exchange_resolution.clone(),
        },
        ..interaction.clone()
    };
    (interaction, resolution, terminal)
}

#[tokio::test]
async fn terminal_batch_rollback_and_reopen_preserve_one_canonical_outcome() {
    let temp = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let (db, initial) = prepare_terminal_fixture(temp.path(), session).await;
    let first = lookup(db.database(), &initial.key).await.unwrap().unwrap();
    let mut completion = complete(&first);
    completion.request_event_sequence = 1;
    completion.exchange_request_event_sequence = 2;
    validate_exchange_binding(db.database(), session, &completion, 4)
        .await
        .unwrap();
    let mut wrong_exchange = completion.clone();
    if let DurableInteractionTransition::Complete {
        exchange_resolution,
        ..
    } = &mut wrong_exchange.transition
    {
        exchange_resolution.exchange_id = "other".into();
    }
    assert!(
        validate_exchange_binding(db.database(), session, &wrong_exchange, 4)
            .await
            .is_err()
    );
    let (interaction, resolution, terminal) = terminal_events(session, &completion);
    let tx = db.database().begin_transaction().await.unwrap();
    crate::db::append_event_projections(&*tx, &interaction, None)
        .await
        .unwrap();
    crate::db::append_event_projections(&*tx, &resolution, None)
        .await
        .unwrap();
    // Fault boundary between the outcome and its result: roll back the entire batch.
    tx.rollback().await.unwrap();
    assert!(retry(db.database(), &completion).await.unwrap().is_none());
    assert_eq!(db.last_event_sequence().await.unwrap(), Some(3));
    assert_eq!(
        db.tool_run("call").await.unwrap().unwrap().status,
        "running"
    );
    let tx = db.database().begin_transaction().await.unwrap();
    crate::db::append_event_projections(&*tx, &interaction, None)
        .await
        .unwrap();
    crate::db::append_event_projections(&*tx, &resolution, None)
        .await
        .unwrap();
    crate::db::append_event_projections(&*tx, &terminal, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // The production feature gate remains off; finish the staged fixture checkpoint explicitly.
    db.database()
        .update("projection_checkpoints")
        .value("last_event_seq", 6_i64)
        .where_eq("projection_name", "durable_interactions")
        .execute(db.database())
        .await
        .unwrap();
    db.close().await.unwrap();
    let db = crate::db::SessionDb::open_turso_in_root(session, temp.path())
        .await
        .unwrap();
    assert_eq!(db.last_event_sequence().await.unwrap(), Some(6));
    let exact = db
        .durable_interaction_operation(&completion.key, &completion.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exact.operation, completion);
    assert_eq!(exact.receipt.tool_result_event_sequence, Some(6));
    assert_eq!(
        db.tool_run("call").await.unwrap().unwrap().status,
        "complete"
    );
    let receipt = retry(db.database(), &completion).await.unwrap().unwrap();
    assert_eq!(receipt.tool_result_event_sequence, Some(6));
    let mut conflict = completion.clone();
    conflict.request_digest = "b".repeat(64);
    assert!(retry(db.database(), &conflict).await.is_err());
    assert_eq!(
        retry(db.database(), &completion).await.unwrap().unwrap(),
        receipt
    );
    assert_eq!(db.last_event_sequence().await.unwrap(), Some(6));
    assert!(
        validate_exchange_binding(db.database(), session, &completion, 7)
            .await
            .is_err()
    );

    assert_eq!(
        retry(db.database(), &initial)
            .await
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn bounded_metadata_pages_reject_stale_generation_and_invalid_limits() {
    let temp = tempfile::tempdir().unwrap();
    let db = database(&temp.path().join("page.db")).await;
    let session = SessionId::new();
    let mut second = operation();
    second.key.interaction_id = "second".into();
    project(&*db, &event(session, 1, operation()))
        .await
        .unwrap();
    project(&*db, &event(session, 2, second)).await.unwrap();
    let mut query = DurableInteractionQuery {
        limit: 1,
        after_event_sequence: None,
        expected_generation: Some(2),
    };
    let first = page(&*db, &query, 2).await.unwrap();
    assert_eq!(first.entries.len(), 1);
    assert_eq!(first.next_after_event_sequence, Some(1));
    query.after_event_sequence = first.next_after_event_sequence;
    let second = page(&*db, &query, 2).await.unwrap();
    assert_eq!(second.entries.len(), 1);
    assert_eq!(second.next_after_event_sequence, None);
    query.expected_generation = Some(1);
    assert!(page(&*db, &query, 2).await.is_err());
    query.expected_generation = Some(2);
    query.limit = MAX_DURABLE_INTERACTION_PAGE + 1;
    assert!(page(&*db, &query, 2).await.is_err());
    assert!(validate_checkpoint(&*db, 3).await.is_err());
    db.close().await.unwrap();
}
