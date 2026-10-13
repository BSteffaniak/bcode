use super::*;
use bcode_workflow::{
    WorkflowInventoryCoverage as Coverage, WorkflowInventoryEntry as Entry,
    WorkflowInventoryQuery as Query, WorkflowInventorySource as Source,
};
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
