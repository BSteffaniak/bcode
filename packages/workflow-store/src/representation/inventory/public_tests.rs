use super::*;
use bcode_workflow::{
    WorkflowInventoryCoverage as Coverage, WorkflowInventoryEntry as Entry,
    WorkflowInventoryQuery as Query, WorkflowInventorySource as Source,
};
#[tokio::test]
async fn output_payload_proof_is_verified_not_generated_by_inventory() {
    let db = super::tests::fixture().await;
    let output = crate::ValidatedOutput {
        output_id: "proven".into(),
        run_id: "run".into(),
        node_id: "node".into(),
        activation_id: "activation".into(),
        schema_id: "json".into(),
        schema_version: 1,
        value: serde_json::json!({"value":7}),
        artifact_reference: None,
        created_at_ms: 1,
    };
    let expected = bcode_workflow::workflow_canonical_value_sha256(&output.value).unwrap();
    let mut encoded = serde_json::to_value(output).unwrap();
    db.insert("workflow_outputs_v2")
        .value("output_id", "proven")
        .value("run_id", "run")
        .value("activation_id", "activation")
        .value("output_json", serde_json::to_string(&encoded).unwrap())
        .execute(&*db)
        .await
        .unwrap();
    let query = Query {
        version: 1,
        run_id: "run".into(),
        source: Source::Outputs,
        cursor: None,
        limit: 100,
    };
    let missing = page(&*db, &query).await.unwrap();
    assert!(matches!(
        missing.entries[0],
        Entry::OutputUnavailable { .. }
    ));
    for (version, checksum, accepted) in [
        (1, expected.clone(), true),
        (2, expected.clone(), false),
        (1, "0".repeat(64), false),
    ] {
        encoded["capture_proof"] =
            serde_json::json!({"version":version,"checksum_sha256":checksum});
        db.update("workflow_outputs_v2")
            .value("output_json", serde_json::to_string(&encoded).unwrap())
            .where_eq("output_id", "proven")
            .execute(&*db)
            .await
            .unwrap();
        let result = page(&*db, &query).await;
        if accepted {
            let result = result.unwrap();
            let Entry::Output { output } = &result.entries[0] else {
                panic!("proof unavailable");
            };
            assert_eq!(output.checksum_sha256, expected);
        } else {
            assert!(result.is_err());
        }
    }
}

#[tokio::test]
async fn public_outputs_pin_source_and_report_missing_stored_proof() {
    let db = super::tests::fixture().await;
    for n in 0..105 {
        super::tests::output(&*db, "run", &format!("out-{n:03}")).await;
    }
    let mut query = Query {
        version: 1,
        run_id: "run".into(),
        source: Source::Outputs,
        cursor: None,
        limit: 100,
    };
    let first = page(&*db, &query).await.unwrap();
    query.validate_page(&first).unwrap();
    assert_eq!(first.entries.len(), 100);
    assert!(matches!(first.coverage, Coverage::CurrentInventoryComplete));
    assert!(matches!(&first.entries[0], Entry::OutputUnavailable { .. }));
    query.cursor = first.next_cursor;
    let second = page(&*db, &query).await.unwrap();
    assert_eq!(second.entries.len(), 5);
    assert!(second.next_cursor.is_none());
    // A different source changes the global ledger but not the output source pin.
    db.update("workflow_graphs_v2")
        .value("definition_json", "{}")
        .where_eq("run_id", "run")
        .execute(&*db)
        .await
        .unwrap();
    assert!(page(&*db, &query).await.is_ok());
    super::tests::output(&*db, "run", "aaa").await;
    assert!(matches!(
        page(&*db, &query).await,
        Err(RepresentationError::InventoryStalePage)
    ));
}
#[tokio::test]
async fn canonical_control_kind_is_explicit_non_session_but_missing_task_link_is_unknown() {
    let (db, identity) = contributor_fixture().await;
    let query = Query {
        version: 1,
        run_id: "run".into(),
        source: Source::AttemptContributors,
        cursor: None,
        limit: 100,
    };
    let before = page(&*db, &query).await.unwrap();
    assert!(matches!(
        &before.entries[0],
        Entry::AttemptContributor {
            session: bcode_workflow::WorkflowInventorySessionLink::Unavailable { .. },
            ..
        }
    ));
    let mut node = crate::representation::graph_pages::node(&*db, "run", 1, "work")
        .await
        .unwrap()
        .unwrap();
    node.kind = bcode_workflow::NodeKind::Parallel;
    db.update("workflow_definition_nodes_v2")
        .value("node_json", serde_json::to_string(&node).unwrap())
        .where_eq("run_id", "run")
        .where_eq("node_id", "work")
        .execute(&*db)
        .await
        .unwrap();
    let control = page(&*db, &query).await.unwrap();
    assert!(matches!(&control.entries[0], Entry::AttemptContributor { session:bcode_workflow::WorkflowInventorySessionLink::NotApplicable { reason:bcode_workflow::WorkflowInventorySessionNotApplicableReason::CanonicalNonSessionNode }, .. }));
    // Even terminal failure without receipt does not imply no session for a task.
    node.kind = bcode_workflow::NodeKind::Task;
    db.update("workflow_definition_nodes_v2")
        .value("node_json", serde_json::to_string(&node).unwrap())
        .where_eq("run_id", "run")
        .where_eq("node_id", "work")
        .execute(&*db)
        .await
        .unwrap();
    let again = page(&*db, &query).await.unwrap();
    assert!(
        matches!(&again.entries[0], Entry::AttemptContributor { attempt, session:bcode_workflow::WorkflowInventorySessionLink::Unavailable { .. }, .. } if attempt.dispatch_identity == identity)
    );
}

#[tokio::test]
async fn public_attempt_facts_keep_private_payloads_out_and_fence_link_updates() {
    let (db, identity) = contributor_fixture().await;
    let mut query = Query {
        version: 1,
        run_id: "run".into(),
        source: Source::AttemptContributors,
        cursor: None,
        limit: 100,
    };
    let before = page(&*db, &query).await.unwrap();
    let Entry::AttemptContributor {
        attempt, session, ..
    } = &before.entries[0]
    else {
        panic!("expected attempt")
    };
    assert_eq!(attempt.status, "admitted");
    assert_eq!(attempt.admitted_at_ms, Some(2));
    assert!(matches!(
        session,
        bcode_workflow::WorkflowInventorySessionLink::Unavailable { .. }
    ));
    let link = crate::WorkflowExecutionSessionLink::new(
        "run".into(),
        "work".into(),
        "activation".into(),
        1,
        "session".into(),
        "workspace".into(),
        3,
    );
    db.insert("workflow_execution_sessions_v2")
        .value("dispatch_identity", identity.as_str())
        .value("link_json", serde_json::to_string(&link).unwrap())
        .execute(&*db)
        .await
        .unwrap();
    query.cursor = Some(before.cursor);
    assert!(matches!(
        page(&*db, &query).await,
        Err(RepresentationError::InventoryStalePage)
    ));
    query.cursor = None;
    let linked = page(&*db, &query).await.unwrap();
    assert!(
        matches!(&linked.entries[0], Entry::AttemptContributor {session: bcode_workflow::WorkflowInventorySessionLink::Verified {session_id, ..}, ..} if session_id == "session")
    );
    let public = serde_json::to_value(&linked).unwrap();
    let attempt = &public["entries"][0]["attempt"];
    assert!(attempt.get("receipt").is_none());
    assert!(attempt.get("intent").is_none());
    query.cursor = Some(linked.cursor);
    let mut changed = link;
    changed.session_id = "replacement".into();
    db.update("workflow_execution_sessions_v2")
        .value("link_json", serde_json::to_string(&changed).unwrap())
        .where_eq("dispatch_identity", identity.as_str())
        .execute(&*db)
        .await
        .unwrap();
    assert!(matches!(
        page(&*db, &query).await,
        Err(RepresentationError::InventoryStalePage)
    ));
}

#[tokio::test]
async fn public_unmapped_capture_is_unavailable_not_empty_complete() {
    let db = super::tests::fixture().await;
    db.insert("workflow_execution_sessions_v2")
        .value("dispatch_identity", "unknown")
        .value("link_json", "{}")
        .execute(&*db)
        .await
        .unwrap();
    let query = Query {
        version: 1,
        run_id: "run".into(),
        source: Source::Outputs,
        cursor: None,
        limit: 100,
    };
    let result = page(&*db, &query).await.unwrap();
    query.validate_page(&result).unwrap();
    assert!(matches!(result.coverage, Coverage::Unavailable { .. }));
    assert!(result.entries.is_empty());
    assert!(result.next_cursor.is_none());
}

async fn registered_inventory_fixture() -> (
    tempfile::TempDir,
    Box<dyn Database>,
    crate::NewWorkflowRun,
    bcode_workflow::WorkflowDefinition,
) {
    let root = tempfile::tempdir().unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "registered-inventory",
        bcode_workflow::Step::task("work", |value: u32, _| async move { Ok(value) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let mut run = crate::representation::tests::run("registered", false);
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("test", &definition).unwrap();
    run.definition_id = identity.definition_id;
    run.definition_version = identity.definition_version;
    let mut store = crate::WorkflowStore::create_coexistence_baseline(root.path()).unwrap();
    store
        .persist_definition(&run.definition_id, run.definition_version, &definition)
        .unwrap();
    let path = store.path().to_path_buf();
    drop(store);
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    crate::representation::install(&*db).await.unwrap();
    // Deliberately no manual inventory setup: registered admission owns installation.
    (root, db, run, definition)
}

fn registered_query(run: &crate::NewWorkflowRun, source: Source) -> Query {
    Query {
        version: 1,
        run_id: run.run_id.clone(),
        source,
        cursor: None,
        limit: 100,
    }
}

#[tokio::test]
async fn registered_admission_installs_current_inventory_without_history_backfill() {
    let (_root, db, run, definition) = registered_inventory_fixture().await;
    Box::pin(crate::representation::admit_registered(
        &*db,
        &run,
        &definition,
    ))
    .await
    .unwrap();
    for source in [
        Source::Outputs,
        Source::AttemptContributors,
        Source::ChildRuns,
    ] {
        let query = registered_query(&run, source);
        let current = page(&*db, &query).await.unwrap();
        query.validate_page(&current).unwrap();
        assert_eq!(current.coverage, Coverage::CurrentInventoryComplete);
        assert!(current.entries.is_empty());
        assert!(current.next_cursor.is_none());
    }
    assert_eq!(
        crate::representation::journal::page(&*db, &run.run_id, 0, None, 100)
            .await
            .unwrap()
            .origin,
        crate::representation::journal::CoverageOrigin::NotEnabled
    );
    assert!(
        Box::pin(crate::representation::admit_registered(
            &*db,
            &run,
            &definition
        ))
        .await
        .unwrap()
        .already_present
    );
    let mut query = registered_query(&run, Source::Outputs);
    query.cursor = Some(page(&*db, &query).await.unwrap().cursor);
    let output = crate::ValidatedOutput {
        output_id: "registered-output".into(),
        run_id: run.run_id.clone(),
        node_id: "work".into(),
        activation_id: "activation".into(),
        schema_id: "json".into(),
        schema_version: 1,
        value: serde_json::json!(7),
        artifact_reference: None,
        created_at_ms: 2,
    };
    let checksum = bcode_workflow::workflow_canonical_value_sha256(&output.value).unwrap();
    let mut encoded = serde_json::to_value(&output).unwrap();
    encoded["capture_proof"] = serde_json::json!({"version":1,"checksum_sha256":checksum});
    db.insert("workflow_outputs_v2")
        .value("output_id", output.output_id.as_str())
        .value("run_id", run.run_id.as_str())
        .value("activation_id", "activation")
        .value("output_json", serde_json::to_string(&encoded).unwrap())
        .execute(&*db)
        .await
        .unwrap();
    assert!(matches!(
        page(&*db, &query).await,
        Err(RepresentationError::InventoryStalePage)
    ));
    query.cursor = None;
    let current = page(&*db, &query).await.unwrap();
    assert_eq!(current.coverage, Coverage::CurrentInventoryComplete);
    assert!(
        matches!(&current.entries[0], Entry::Output { output: fact } if fact.value == output.value)
    );
}

#[tokio::test]
async fn registered_admission_observes_real_contributor_and_rejects_stale_generation() {
    let (_root, db, run, definition) = registered_inventory_fixture().await;
    Box::pin(crate::representation::admit_registered(
        &*db,
        &run,
        &definition,
    ))
    .await
    .unwrap();
    let mut query = registered_query(&run, Source::AttemptContributors);
    query.cursor = Some(page(&*db, &query).await.unwrap().cursor);
    let prepared = crate::PreparedAttempt {
        run_id: run.run_id.clone(),
        node_id: "work".into(),
        activation_id: "activation".into(),
        attempt: 1,
        side_effect: crate::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 2,
    };
    let identity = prepared.dispatch_identity();
    db.insert("workflow_attempts_v2")
        .value("dispatch_identity", identity.as_str())
        .value("run_id", run.run_id.as_str())
        .value("node_id", "work")
        .value("activation_id", "activation")
        .value("attempt_number", 1_i64)
        .value("prepared_json", serde_json::to_string(&prepared).unwrap())
        .execute(&*db)
        .await
        .unwrap();
    assert!(matches!(
        page(&*db, &query).await,
        Err(RepresentationError::InventoryStalePage)
    ));
    query.cursor = None;
    let current = page(&*db, &query).await.unwrap();
    assert_eq!(current.coverage, Coverage::CurrentInventoryComplete);
    assert_eq!(current.entries.len(), 1);
    assert!(
        matches!(&current.entries[0], Entry::AttemptContributor { attempt, .. }
        if attempt.dispatch_identity == identity)
    );
}

#[tokio::test]
async fn registered_admission_preserves_existing_unattributed_inventory_gap() {
    let (_root, db, run, definition) = registered_inventory_fixture().await;
    // Existing session attribution cannot be retroactively proven by enabling observers.
    db.insert("workflow_execution_sessions_v2")
        .value("dispatch_identity", "older-unattributed")
        .value("link_json", "{}")
        .execute(&*db)
        .await
        .unwrap();
    Box::pin(crate::representation::admit_registered(
        &*db,
        &run,
        &definition,
    ))
    .await
    .unwrap();
    for _ in 0..2 {
        let current = page(&*db, &registered_query(&run, Source::Outputs))
            .await
            .unwrap();
        assert!(matches!(current.coverage, Coverage::Unavailable { .. }));
        assert!(current.entries.is_empty());
        assert!(
            Box::pin(crate::representation::admit_registered(
                &*db,
                &run,
                &definition
            ))
            .await
            .unwrap()
            .already_present
        );
    }
}

#[tokio::test]
async fn failed_registered_admission_rolls_back_observer_installation() {
    let (_root, db, mut run, definition) = registered_inventory_fixture().await;
    run.input = Some(serde_json::json!("invalid u32"));
    assert!(
        Box::pin(crate::representation::admit_registered(
            &*db,
            &run,
            &definition
        ))
        .await
        .is_err()
    );
    assert!(db.get_table_info(CONTRACT).await.unwrap().is_none());
}

async fn contributor_fixture() -> (Box<dyn Database>, String) {
    let db = crate::representation::tests::database().await;
    crate::representation::install(&*db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "inventory-test",
        bcode_workflow::Step::task("work", |value: u32, _| async move { Ok(value) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let mut request = crate::representation::tests::run("run", false);
    let exact =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("test", &definition).unwrap();
    request.definition_id = exact.definition_id;
    request.definition_version = exact.definition_version;
    crate::representation::admit(&*db, &request, &definition)
        .await
        .unwrap();
    install(&*db).await.unwrap();
    let prepared = crate::PreparedAttempt {
        run_id: "run".into(),
        node_id: "work".into(),
        activation_id: "activation".into(),
        attempt: 1,
        side_effect: crate::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({"private_intent": "not-public"}),
        prepared_at_ms: 1,
    };
    let identity = prepared.dispatch_identity();
    let receipt = crate::DispatchReceipt {
        run_id: prepared.run_id.clone(),
        node_id: prepared.node_id.clone(),
        activation_id: prepared.activation_id.clone(),
        attempt: 1,
        dispatch_identity: identity.clone(),
        receipt: serde_json::json!({"private_receipt": "not-public"}),
        admitted_at_ms: 2,
    };
    db.insert("workflow_attempts_v2")
        .value("dispatch_identity", identity.as_str())
        .value("run_id", "run")
        .value("node_id", "work")
        .value("activation_id", "activation")
        .value("attempt_number", 1_i64)
        .value("prepared_json", serde_json::to_string(&prepared).unwrap())
        .value("receipt_json", serde_json::to_string(&receipt).unwrap())
        .execute(&*db)
        .await
        .unwrap();
    (db, identity)
}
