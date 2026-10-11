use super::*;
use bcode_workflow::{
    WorkflowSemanticHistoryCoverage as Coverage, WorkflowSemanticHistoryPayload as Payload,
};

async fn fixture() -> (
    Box<dyn Database>,
    crate::WorkflowExecutionAuthority,
    crate::PreparedAttempt,
) {
    let db = super::super::tests::database().await;
    super::super::install(&*db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "history",
        bcode_workflow::Step::task("worker", |value: u32, _| async move { Ok(value) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let run = super::super::tests::run("history", false);
    super::super::admit(&*db, &run, &definition).await.unwrap();
    let owner = crate::WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "fence".into(),
    };
    super::super::attempts::claim_authority(&*db, "history", &owner)
        .await
        .unwrap();
    let prepared = crate::PreparedAttempt {
        run_id: "history".into(),
        node_id: definition.entries[0].clone(),
        activation_id: super::super::graph::activation_id(&*db, "history", &definition.entries[0])
            .await
            .unwrap()
            .unwrap(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    (db, owner, prepared)
}
fn receipt(prepared: &crate::PreparedAttempt) -> crate::DispatchReceipt {
    crate::DispatchReceipt {
        run_id: prepared.run_id.clone(),
        node_id: prepared.node_id.clone(),
        activation_id: prepared.activation_id.clone(),
        attempt: 1,
        dispatch_identity: prepared.dispatch_identity(),
        receipt: serde_json::json!({"owner":"test"}),
        admitted_at_ms: 2,
    }
}
#[tokio::test]
async fn exact_attempt_events_duplicates_and_pinned_pages() {
    let (db, owner, prepared) = fixture().await;
    let id = super::super::attempts::prepare(&*db, &owner, &prepared)
        .await
        .unwrap();
    super::super::attempts::prepare(&*db, &owner, &prepared)
        .await
        .unwrap();
    let first = page(&*db, "history", None, 1).await.unwrap();
    assert_eq!(first.cursor.coverage, Coverage::CompleteFromAdmission);
    assert_eq!(first.events.len(), 1);
    let receipt = receipt(&prepared);
    super::super::attempts::attach_receipt(&*db, &owner, &receipt)
        .await
        .unwrap();
    super::super::attempts::attach_receipt(&*db, &owner, &receipt)
        .await
        .unwrap();
    super::super::attempts::settle(
        &*db,
        &owner,
        "history",
        &id,
        super::super::attempts::TerminalOutcome::Succeeded,
        3,
    )
    .await
    .unwrap();
    let all = page(&*db, "history", None, 100).await.unwrap();
    assert_eq!(all.events.len(), 3);
    assert!(matches!(
        all.events[1].payload,
        Payload::ReceiptCommitted {}
    ));
    let mut pinned = first.cursor;
    pinned.after_sequence = 1;
    assert!(
        page(&*db, "history", Some(&pinned), 100)
            .await
            .unwrap()
            .events
            .is_empty()
    );
}
#[tokio::test]
async fn older_unacknowledged_write_survives_later_valid_settlement() {
    let (db, owner, prepared) = fixture().await;
    let id = super::super::attempts::prepare(&*db, &owner, &prepared)
        .await
        .unwrap();
    let receipt = receipt(&prepared);
    db.update(SOURCE)
        .value("receipt_json", serde_json::to_string(&receipt).unwrap())
        .where_eq("dispatch_identity", id.as_str())
        .execute(&*db)
        .await
        .unwrap();
    assert_eq!(
        page(&*db, "history", None, 100)
            .await
            .unwrap()
            .cursor
            .coverage,
        Coverage::HistoricalGap
    );
    super::super::attempts::settle(
        &*db,
        &owner,
        "history",
        &id,
        super::super::attempts::TerminalOutcome::Cancelled,
        3,
    )
    .await
    .unwrap();
    let history = page(&*db, "history", None, 100).await.unwrap();
    assert_eq!(history.cursor.coverage, Coverage::HistoricalGap);
    assert_eq!(history.events.len(), 2);
}
#[tokio::test]
async fn truncated_facts_fail_closed_and_rollback() {
    let (db, owner, prepared) = fixture().await;
    let id = super::super::attempts::prepare(&*db, &owner, &prepared)
        .await
        .unwrap();
    let tx = db
        .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
        .await
        .unwrap();
    let transition = begin(&*tx, &id, &prepared.run_id).await.unwrap();
    let oversized = "x".repeat(1_048_577);
    tx.update(SOURCE)
        .value("receipt_json", oversized.as_str())
        .where_eq("dispatch_identity", id.as_str())
        .execute(&*tx)
        .await
        .unwrap();
    assert!(
        finish(
            &*tx,
            transition,
            source_row(&prepared, Some(oversized), None, None).unwrap(),
            Payload::ReceiptCommitted {},
            2
        )
        .await
        .is_err()
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        page(&*db, "history", None, 100)
            .await
            .unwrap()
            .cursor
            .coverage,
        Coverage::CompleteFromAdmission
    );
}

#[tokio::test]
async fn installation_retries_verify_without_resetting_contract() {
    let db = super::super::tests::database().await;
    super::super::install(&*db).await.unwrap();
    // Reinstallation verifies existing exact structure and never resets origin/gaps.
    super::super::install(&*db).await.unwrap();
    let versions = db.select(CONTRACT).limit(2).execute(&*db).await.unwrap();
    assert_eq!(versions.len(), 1);
}

#[tokio::test]
async fn multiple_connections_preserve_old_writer_gap_and_independent_admission() {
    let root = tempfile::tempdir().unwrap();
    let store = crate::WorkflowStore::create_coexistence_baseline(root.path()).unwrap();
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(store.path())).unwrap();
    super::super::install(&*db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "history",
        bcode_workflow::Step::task("worker", |value: u32, _| async move { Ok(value) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let run = super::super::tests::run("modern", false);
    super::super::admit(&*db, &run, &definition).await.unwrap();
    let other = switchy_database_connection::init_sqlite_rusqlite(Some(store.path())).unwrap();
    // Independent legacy-family mutation is unaffected by the attempt observers.
    other
        .update("workflow_store_contract")
        .value("schema_version", 42_i64)
        .where_eq("contract_id", 1_i64)
        .execute(&*other)
        .await
        .unwrap();
    let owner = crate::WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "fence".into(),
    };
    super::super::attempts::claim_authority(&*db, "modern", &owner)
        .await
        .unwrap();
    let prepared = crate::PreparedAttempt {
        run_id: "modern".into(),
        node_id: definition.entries[0].clone(),
        activation_id: super::super::graph::activation_id(&*db, "modern", &definition.entries[0])
            .await
            .unwrap()
            .unwrap(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let id = super::super::attempts::prepare(&*db, &owner, &prepared)
        .await
        .unwrap();
    other
        .update(SOURCE)
        .value(
            "receipt_json",
            serde_json::to_string(&receipt(&prepared)).unwrap(),
        )
        .where_eq("dispatch_identity", id.as_str())
        .execute(&*other)
        .await
        .unwrap();
    super::super::attempts::settle(
        &*db,
        &owner,
        "modern",
        &id,
        super::super::attempts::TerminalOutcome::Cancelled,
        3,
    )
    .await
    .unwrap();
    assert_eq!(
        page(&*other, "modern", None, 100)
            .await
            .unwrap()
            .cursor
            .coverage,
        Coverage::HistoricalGap
    );
    let independent = super::super::tests::run("independent", false);
    super::super::admit(&*other, &independent, &definition)
        .await
        .unwrap();
    assert_eq!(
        page(&*db, "independent", None, 100)
            .await
            .unwrap()
            .cursor
            .coverage,
        Coverage::CompleteFromAdmission
    );
    drop(other);
    drop(db);
    drop(store);
}

#[tokio::test]
async fn rolled_back_event_write_and_missing_origin_are_explicit() {
    let db = super::super::tests::database().await;
    super::super::install(&*db).await.unwrap();
    let tx = db
        .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
        .await
        .unwrap();
    tx.insert(EVENTS)
        .value("identity", "rollback")
        .value("run_id", "never-committed")
        .value("sequence", 1_i64)
        .value("event_json", "{}")
        .execute(&*tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(
        db.select(EVENTS)
            .where_eq("run_id", "never-committed")
            .limit(1)
            .execute(&*db)
            .await
            .unwrap()
            .is_empty()
    );
    // No origin is manufactured by inspection for an older or absent run.
    assert_eq!(
        page(&*db, "older", None, 100)
            .await
            .unwrap()
            .cursor
            .coverage,
        Coverage::HistoricalGap
    );
}

#[tokio::test]
async fn additive_installer_rolls_back_all_schema_and_observers() {
    use switchy_database::schema::{Column, DataType, create_table};
    let db = super::super::tests::database().await;
    create_table("workflow_runs_v2")
        .column(Column {
            name: "run_id".into(),
            data_type: DataType::Text,
            nullable: false,
            auto_increment: false,
            default: None,
        })
        .primary_key("run_id")
        .execute(&*db)
        .await
        .unwrap();
    let mut statement = create_table(SOURCE);
    for field in FIELDS {
        statement = statement.column(Column {
            name: (*field).into(),
            data_type: if matches!(*field, "attempt_number" | "terminal_at_ms") {
                DataType::Int
            } else {
                DataType::Text
            },
            nullable: true,
            auto_increment: false,
            default: None,
        });
    }
    statement.execute(&*db).await.unwrap();
    let tx = db
        .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
        .await
        .unwrap();
    install_in(&*tx).await.unwrap();
    tx.rollback().await.unwrap();
    for table in [CONTRACT, ORIGINS, OBLIGATIONS, EVENTS, ACKS] {
        assert!(db.get_table_info(table).await.unwrap().is_none());
    }
    let tx = db
        .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
        .await
        .unwrap();
    install_in(&*tx).await.unwrap();
    tx.commit().await.unwrap();
    installation::verify(&*db).await.unwrap();
}

#[tokio::test]
async fn unexpected_transition_rolls_back_without_acknowledging_any_obligation() {
    let (db, owner, prepared) = fixture().await;
    let id = super::super::attempts::prepare(&*db, &owner, &prepared)
        .await
        .unwrap();
    let tx = db
        .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
        .await
        .unwrap();
    let transition = begin(&*tx, &id, &prepared.run_id).await.unwrap();
    for _ in 0..2 {
        tx.update(SOURCE)
            .value("receipt_json", "unexpected")
            .where_eq("dispatch_identity", id.as_str())
            .execute(&*tx)
            .await
            .unwrap();
    }
    let expected = source_row(&prepared, Some("unexpected".into()), None, None).unwrap();
    assert!(
        finish(&*tx, transition, expected, Payload::ReceiptCommitted {}, 2)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        page(&*db, "history", None, 100)
            .await
            .unwrap()
            .cursor
            .coverage,
        Coverage::CompleteFromAdmission
    );
}
