use super::super::super::{attempts, graph_revision, runs, tests::run};
use super::*;

async fn parallel_fixture(
    policy: bcode_workflow::ParallelFailurePolicy,
) -> (Box<dyn Database>, crate::WorkflowExecutionAuthority) {
    let mut definition = bcode_workflow::WorkflowBuilder::new(
        "parallel",
        bcode_workflow::parallel_named(
            "join",
            super::super::tests::task("left"),
            super::super::tests::task("right"),
        ),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    definition.nodes.get_mut("join").unwrap().configuration["failure_policy"] =
        serde_json::to_value(policy).unwrap();
    if policy == bcode_workflow::ParallelFailurePolicy::CollectOutcomes {
        let members = ["left", "right"]
            .into_iter()
            .map(|id| {
                (
                    id.to_string(),
                    bcode_workflow::workflow_member_outcome_schema(&definition.nodes[id].output)
                        .unwrap(),
                )
            })
            .collect();
        let schema = bcode_workflow::named_result_schema("outcomes".into(), &members).unwrap();
        let join = definition.nodes.get_mut("join").unwrap();
        join.input = schema.clone();
        join.output = schema.clone();
        definition.output = schema;
    }
    super::super::tests::admitted(&definition).await
}
async fn receipt_attempt(
    db: &dyn Database,
    owner: &crate::WorkflowExecutionAuthority,
    node: &str,
) -> String {
    let activation = super::super::activation_id(db, "graph", node)
        .await
        .unwrap()
        .unwrap();
    let prepared = crate::PreparedAttempt {
        run_id: "graph".into(),
        node_id: node.into(),
        activation_id: activation.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let id = attempts::prepare(db, owner, &prepared).await.unwrap();
    attempts::attach_receipt(
        db,
        owner,
        &crate::DispatchReceipt {
            run_id: "graph".into(),
            node_id: node.into(),
            activation_id: activation,
            attempt: 1,
            dispatch_identity: id.clone(),
            receipt: serde_json::json!({}),
            admitted_at_ms: 2,
        },
    )
    .await
    .unwrap();
    id
}
#[tokio::test]
async fn collected_failure_cannot_terminalize_before_controller_consumes_it() {
    let (db, owner) =
        parallel_fixture(bcode_workflow::ParallelFailurePolicy::CollectOutcomes).await;
    let failed = receipt_attempt(&*db, &owner, "left").await;
    // Simulate a previously completed cached proof; canonical new outcome must invalidate it.
    db.insert(TABLE)
        .value("run_id", "graph")
        .value("revision", 1_i64)
        .value("after_node", DatabaseValue::Null)
        .value("complete", true)
        .value("member_index", 0_i64)
        .execute(&*db)
        .await
        .unwrap();
    attempts::settle(
        &*db,
        &owner,
        "graph",
        &failed,
        attempts::TerminalOutcome::Failed,
        3,
    )
    .await
    .unwrap();
    assert_eq!(
        runs::terminalize(&*db, &owner, "graph").await.unwrap(),
        None
    );
    super::super::tests::success(&*db, &owner, "right").await;
    super::super::settle_output(&*db, &owner, "graph", "right", 1, &serde_json::json!(9))
        .await
        .unwrap();
    assert_eq!(
        runs::terminalize(&*db, &owner, "graph").await.unwrap(),
        None
    );
    super::super::parallel::settle(&*db, &owner, "graph", "join", 1)
        .await
        .unwrap();
    assert_eq!(
        runs::terminalize(&*db, &owner, "graph").await.unwrap(),
        Some(runs::RunState::Completed)
    );
}
#[tokio::test]
async fn failfast_waits_for_receipt_backed_sibling_cancellation() {
    let (db, owner) = parallel_fixture(bcode_workflow::ParallelFailurePolicy::FailFast).await;
    let left = receipt_attempt(&*db, &owner, "left").await;
    let right = receipt_attempt(&*db, &owner, "right").await;
    attempts::settle(
        &*db,
        &owner,
        "graph",
        &left,
        attempts::TerminalOutcome::Failed,
        3,
    )
    .await
    .unwrap();
    super::super::parallel::settle(&*db, &owner, "graph", "join", 1)
        .await
        .unwrap();
    assert_eq!(
        runs::terminalize(&*db, &owner, "graph").await.unwrap(),
        None
    );
    let targets =
        attempts::owner_boundary::parallel_cancellation_receipts(&*db, "graph", &owner, None, 128)
            .await
            .unwrap();
    assert_eq!(targets.0.len(), 1);
    assert_eq!(targets.0[0].dispatch_identity, right);
    assert!(
        attempts::owner_boundary::cancellation_intended(&*db, "graph", &right, &owner)
            .await
            .unwrap()
    );
    assert!(
        !attempts::owner_boundary::cancellation_intended(&*db, "graph", &left, &owner)
            .await
            .unwrap()
    );
    assert!(
        super::super::super::safety::cancellation_requested_at(&*db, "graph")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        attempts::pending(&*db, "graph", None, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    attempts::settle(
        &*db,
        &owner,
        "graph",
        &right,
        attempts::TerminalOutcome::Cancelled,
        4,
    )
    .await
    .unwrap();
    assert_eq!(
        runs::terminalize(&*db, &owner, "graph").await.unwrap(),
        None
    );
    super::super::parallel::settle(&*db, &owner, "graph", "join", 1)
        .await
        .unwrap();
    assert!(
        runs::terminalize(&*db, &owner, "graph")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn incomplete_proof_is_not_local_work_while_receipts_are_outstanding() {
    let (db, owner) = parallel_fixture(bcode_workflow::ParallelFailurePolicy::FailFast).await;
    let receipt = receipt_attempt(&*db, &owner, "left").await;
    db.insert(TABLE)
        .value("run_id", "graph")
        .value("revision", 1_i64)
        .value("after_node", DatabaseValue::Null)
        .value("complete", false)
        .value("member_index", 0_i64)
        .execute(&*db)
        .await
        .unwrap();
    assert!(!pending(&*db, "graph").await.unwrap());
    assert_eq!(
        db.select(TABLE)
            .where_eq("run_id", "graph")
            .limit(1)
            .execute(&*db)
            .await
            .unwrap()
            .len(),
        1,
        "observation preserves proof state"
    );
    attempts::settle(
        &*db,
        &owner,
        "graph",
        &receipt,
        attempts::TerminalOutcome::Failed,
        3,
    )
    .await
    .unwrap();
    // Simulate the next bounded proof page after the canonical settlement invalidation.
    db.insert(TABLE)
        .value("run_id", "graph")
        .value("revision", 1_i64)
        .value("after_node", DatabaseValue::Null)
        .value("complete", false)
        .value("member_index", 0_i64)
        .execute(&*db)
        .await
        .unwrap();
    assert!(pending(&*db, "graph").await.unwrap());
}

#[tokio::test]
async fn terminal_scan_reopens_and_new_revision_invalidates_prior_proof() {
    let temp = tempfile::tempdir().unwrap();
    let _legacy = crate::WorkflowStore::create_coexistence_baseline(temp.path()).unwrap();
    let path = crate::workflow_database_path(temp.path());
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    super::super::super::install(&*db).await.unwrap();
    let mut definition = bcode_workflow::WorkflowBuilder::new(
        "scan",
        bcode_workflow::Step::task("exit", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let node = definition.nodes["exit"].clone();
    for index in 0..130 {
        let mut node = node.clone();
        node.id = format!("n{index:03}");
        definition.entries.push(node.id.clone());
        definition.nodes.insert(node.id.clone(), node);
    }
    let mut request = run("scan", false);
    let id =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("scan", &definition).unwrap();
    request.definition_id = id.definition_id;
    request.definition_version = id.definition_version;
    super::super::super::admit(&*db, &request, &definition)
        .await
        .unwrap();
    let owner = crate::WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    attempts::claim_authority(&*db, "scan", &owner)
        .await
        .unwrap();
    assert_eq!(runs::terminalize(&*db, &owner, "scan").await.unwrap(), None);
    assert!(pending(&*db, "scan").await.unwrap());
    drop(db);
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    assert!(pending(&*db, "scan").await.unwrap());
    let mut changed = node;
    changed.name = "new pending executable".into();
    let batch = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "scan".into(),
        expected_revision: 1,
        mutation_id: "edit".into(),
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::ReplaceNode {
            node: changed,
            entry: true,
            exit: true,
        }],
        reconciliation: vec![],
    };
    graph_revision::publish(&*db, &owner, &batch).await.unwrap();
    assert!(!pending(&*db, "scan").await.unwrap());
    assert!(
        db.select(TABLE)
            .where_eq("run_id", "scan")
            .execute(&*db)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(runs::terminalize(&*db, &owner, "scan").await.unwrap(), None);
    assert!(pending(&*db, "scan").await.unwrap());
    let mut calls = 1;
    while pending(&*db, "scan").await.unwrap() {
        assert_eq!(runs::terminalize(&*db, &owner, "scan").await.unwrap(), None);
        calls += 1;
        assert!(calls < 10);
    }
    assert!(calls >= 5);
    // Creating a new generation invalidates a completed proof in the SAME transaction.
    let tx = db
        .begin_transaction_with_mode(super::super::TransactionMode::Immediate)
        .await
        .unwrap();
    tx.update(super::super::ACTIVATIONS)
        .value("current", false)
        .where_eq("run_id", "scan")
        .where_eq("node_id", "exit")
        .execute(&*tx)
        .await
        .unwrap();
    super::super::insert_activation(&*tx, "scan", "exit", &serde_json::json!(1))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        db.select(TABLE)
            .where_eq("run_id", "scan")
            .execute(&*db)
            .await
            .unwrap()
            .is_empty()
    );
}
