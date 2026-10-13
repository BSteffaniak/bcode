use super::{EVENTS, Fact, WorkflowLifecycleHistoryPayload, append, enable, install, page};

#[tokio::test]
async fn actual_admission_preparation_receipt_and_settlement_produce_timed_public_facts() {
    let (_temp, db) = super::super::diagnostics::tests::admitted(true).await;
    let owner = crate::WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "SECRET_FENCE".into(),
    };
    crate::representation::attempts::claim_authority(&*db, "run", &owner)
        .await
        .unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "diagnostics",
        bcode_workflow::Step::task("work", |value: u32, _| async move { Ok(value) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let node = definition.entries[0].clone();
    let activation = crate::representation::graph::activation_id(&*db, "run", &node)
        .await
        .unwrap()
        .unwrap();
    let prepared = crate::PreparedAttempt {
        run_id: "run".into(),
        node_id: node,
        activation_id: activation,
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({"secret":"SECRET_INTENT"}),
        prepared_at_ms: 11,
    };
    let identity = crate::representation::attempts::prepare(&*db, &owner, &prepared)
        .await
        .unwrap();
    let receipt = crate::DispatchReceipt {
        run_id: "run".into(),
        node_id: prepared.node_id.clone(),
        activation_id: prepared.activation_id.clone(),
        attempt: 1,
        dispatch_identity: identity.clone(),
        receipt: serde_json::json!({"secret":"SECRET_RECEIPT"}),
        admitted_at_ms: 12,
    };
    crate::representation::attempts::attach_receipt(&*db, &owner, &receipt)
        .await
        .unwrap();
    crate::representation::attempts::settle(
        &*db,
        &owner,
        "run",
        &identity,
        crate::representation::attempts::TerminalOutcome::Succeeded,
        13,
    )
    .await
    .unwrap();
    let page = page(&*db, 1, "run", 0, None, 100).await.unwrap();
    assert!(page.from_admission);
    assert_eq!(
        page.attempt_coverage,
        bcode_workflow::WorkflowSemanticHistoryCoverage::CompleteFromAdmission
    );
    assert_eq!(
        page.lifecycle_coverage,
        bcode_workflow::WorkflowSemanticHistoryCoverage::CompleteFromAdmission
    );
    assert_eq!(page.events.len(), 4);
    assert_eq!(
        page.events
            .iter()
            .skip(1)
            .map(|e| e.created_at_ms)
            .collect::<Vec<_>>(),
        vec![11, 12, 13]
    );
    let json = serde_json::to_string(&page).unwrap();
    for secret in ["SECRET_FENCE", "SECRET_INTENT", "SECRET_RECEIPT"] {
        assert!(!json.contains(secret));
    }
}
use switchy_database::query::FilterableQuery;

#[tokio::test]
async fn future_event_kind_is_preserved_but_private_payload_is_not_interpreted() {
    let (_temp, db) = super::super::diagnostics::tests::admitted(true).await;
    db.insert(EVENTS)
        .value("run_id", "run")
        .value("identity", format!("{:064x}", 991))
        .value("payload", "SECRET_FUTURE_OWNER_PAYLOAD")
        .value("event_type", "future_kind_v99")
        .value("recorded_at_ms", 991_i64)
        .execute(&*db)
        .await
        .unwrap();
    let page = page(&*db, 1, "run", 0, None, 100).await.unwrap();
    let future = page.events.last().unwrap();
    assert_eq!(future.event_type, "future_kind_v99");
    assert_eq!(future.created_at_ms, 991);
    assert_eq!(future.payload["unavailable"], "unreviewed_event_payload");
    assert!(
        !serde_json::to_string(&page)
            .unwrap()
            .contains("SECRET_FUTURE_OWNER_PAYLOAD")
    );
    assert!(
        enable(&*db.begin_transaction().await.unwrap(), "run")
            .await
            .is_err()
    );
    assert!(
        db.insert("workflow_attempt_history_events_v1")
            .value("run_id", "run")
            .value("identity", format!("{:064x}", 992))
            .value("sequence", 1_i64)
            .value("event_json", "private")
            .execute(&*db)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn timed_events_are_transactional_pinned_bounded_and_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let store = crate::WorkflowStore::create_coexistence_baseline(temp.path()).unwrap();
    let path = store.path().to_path_buf();
    drop(store);
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    crate::representation::install(&*db).await.unwrap();
    let tx = db.begin_transaction().await.unwrap();
    install(&*tx).await.unwrap();
    enable(&*tx, "run").await.unwrap();
    let fact = Fact::Lifecycle {
        payload: WorkflowLifecycleHistoryPayload::RunAdmitted {
            status: bcode_workflow::RunStatus::Paused,
        },
    };
    for index in 0..205_u64 {
        let id = format!("{index:064x}");
        append(&*tx, "run", &id, index, fact.clone()).await.unwrap();
        append(&*tx, "run", &id, index, fact.clone()).await.unwrap();
    }
    tx.commit().await.unwrap();
    let first = page(&*db, 1, "run", 0, None, 100).await.unwrap();
    assert!(first.from_admission);
    assert_eq!(first.events.len(), 100);
    let through = first.through_sequence;
    let tx = db.begin_transaction().await.unwrap();
    append(&*tx, "run", &format!("{:064x}", 999), 999, fact.clone())
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(
        page(&*db, 1, "run", 0, None, 100)
            .await
            .unwrap()
            .through_sequence,
        through
    );
    let tx = db.begin_transaction().await.unwrap();
    append(&*tx, "run", &format!("{:064x}", 1000), 1000, fact)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut events = first.events;
    let mut next = first.next_after_sequence;
    while let Some(after) = next {
        let part = page(&*db, 1, "run", after, Some(through), 100)
            .await
            .unwrap();
        assert_eq!(part.through_sequence, through);
        events.extend(part.events);
        next = part.next_after_sequence;
    }
    assert_eq!(events.len(), 205);
    assert_eq!(events.last().unwrap().created_at_ms, 204);
    assert_eq!(events[0].event_type, "committed_fact_v1");
    assert!(
        !page(&*db, 1, "historical", 0, None, 100)
            .await
            .unwrap()
            .from_admission
    );
    assert!(page(&*db, 2, "run", 0, None, 100).await.is_err());
    assert!(
        page(&*db, 1, "run", 0, Some(through + 100), 100)
            .await
            .is_err()
    );
    assert!(
        db.update(EVENTS)
            .value("payload", "secret")
            .where_eq("run_id", "run")
            .execute(&*db)
            .await
            .is_err()
    );
}
