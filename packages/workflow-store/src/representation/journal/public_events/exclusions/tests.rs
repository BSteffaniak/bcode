use super::*;
use crate::representation as rep;

type Fixture = (
    tempfile::TempDir,
    Box<dyn Database>,
    bcode_workflow::WorkflowDefinition,
);

async fn database() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let mut store = crate::WorkflowStore::create_coexistence_baseline(temp.path()).unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "exclusion",
        bcode_workflow::Step::task("first", |n: u32, _| async move { Ok(n) }).then(
            bcode_workflow::Step::task("last", |n: u32, _| async move { Ok(n) }),
        ),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("test", &definition).unwrap();
    store
        .persist_definition(
            &identity.definition_id,
            identity.definition_version,
            &definition,
        )
        .unwrap();
    let mut run = rep::tests::run("legacy", false);
    run.definition_id = identity.definition_id;
    run.definition_version = identity.definition_version;
    run.limits.concurrency_cap = Some(8);
    store.create_run(&run).unwrap();
    store
        .create_activation(&crate::NewActivation {
            run_id: "legacy".into(),
            node_id: "first".into(),
            activation_id: "legacy-activation".into(),
            dependency_generation: 1,
            input: Some(serde_json::json!(1)),
            created_at_ms: 1,
        })
        .unwrap();
    let path = store.path().to_path_buf();
    drop(store);
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    rep::install(&*db).await.unwrap();
    rep::journal::install_in(&*db).await.unwrap();
    let tx = db
        .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
        .await
        .unwrap();
    super::super::install(&*tx).await.unwrap();
    tx.commit().await.unwrap();
    (temp, db, definition)
}
async fn fixture() -> Fixture {
    let (temp, db, definition) = database().await;
    let tx = db
        .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
        .await
        .unwrap();
    install_in(&*tx).await.unwrap();
    super::super::enable(&*tx, "covered").await.unwrap();
    enable_in(&*tx, "covered").await.unwrap();
    for name in ["covered", "historical"] {
        let mut run = rep::tests::run(name, false);
        let identity =
            bcode_workflow::WorkflowDefinitionIdentity::for_definition("test", &definition)
                .unwrap();
        run.definition_id = identity.definition_id;
        run.definition_version = identity.definition_version;
        rep::admit_in(&*tx, &run, &definition).await.unwrap();
        rep::hierarchy::register_root(&*tx, &run).await.unwrap();
    }
    tx.commit().await.unwrap();
    (temp, db, definition)
}
async fn insert(
    db: &dyn Database,
    table: &str,
    run: &str,
    id: &str,
) -> Result<(), switchy_database::DatabaseError> {
    let mut row = db
        .insert(table)
        .value("run_id", run)
        .value("node_id", "first");
    row = match table {
        "workflow_decisions" => row
            .value("decision_id", id)
            .value("decision_type", "fixture")
            .value("value_json", "{}")
            .value("created_at_ms", 1_i64),
        "workflow_grants" => row
            .value("grant_id", id)
            .value("scope_json", "{}")
            .value("granted_at_ms", 1_i64)
            .value("uses_consumed", 0_i64),
        "workflow_mutation_approvals" => row
            .value("approval_id", id)
            .value("activation_id", "legacy-activation")
            .value("scope_json", "{}")
            .value("status", "pending")
            .value("requested_at_ms", 1_i64),
        _ => unreachable!(),
    };
    row.execute(db).await?;
    Ok(())
}
fn key(table: &str) -> &str {
    match table {
        "workflow_decisions" => "decision_id",
        "workflow_grants" => "grant_id",
        "workflow_mutation_approvals" => "approval_id",
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn excluded_inserts_and_moved_rows_reject_while_legacy_writes_remain_legal() {
    let (_temp, db, _) = fixture().await;
    assert_eq!(
        coverage(&*db, "covered").await.unwrap(),
        Coverage::VerifiedExcludedV1
    );
    for table in SOURCES {
        insert(&*db, table, "legacy", "legal").await.unwrap();
        let error = insert(&*db, table, "covered", "forbidden")
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("workflow_permission_legacy_collection_excluded_v1")
        );
        let error = db
            .update(table)
            .value("run_id", "covered")
            .where_eq(key(table), "legal")
            .execute(&*db)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("workflow_permission_legacy_collection_excluded_v1")
        );
        // Updates and deletes remain legal for unmarked legacy rows.
        db.update(table)
            .value("node_id", "first")
            .where_eq(key(table), "legal")
            .execute(&*db)
            .await
            .unwrap();
        db.delete(table)
            .where_eq(key(table), "legal")
            .execute(&*db)
            .await
            .unwrap();
    }
    assert_eq!(
        coverage(&*db, "covered").await.unwrap(),
        Coverage::VerifiedExcludedV1
    );
}

#[tokio::test]
async fn historical_prior_rows_and_origin_mutation_are_not_adopted() {
    let (_temp, db, _) = fixture().await;
    assert_eq!(
        coverage(&*db, "historical").await.unwrap(),
        Coverage::HistoricalGap
    );
    let tx = db.begin_transaction().await.unwrap();
    assert!(enable_in(&*tx, "historical").await.is_err());
    assert!(enable_in(&*tx, "legacy").await.is_err());
    tx.rollback().await.unwrap();
    // A valid legacy mutation approval provides a pre-existing source row without disabling guards.
    insert(&*db, "workflow_mutation_approvals", "legacy", "prior")
        .await
        .unwrap();
    let tx = db.begin_transaction().await.unwrap();
    assert!(enable_in(&*tx, "legacy").await.is_err());
    tx.rollback().await.unwrap();
    assert!(
        db.update(ORIGINS)
            .value("version", 2_i64)
            .where_eq("run_id", "covered")
            .execute(&*db)
            .await
            .is_err()
    );
    assert!(
        db.delete(ORIGINS)
            .where_eq("run_id", "covered")
            .execute(&*db)
            .await
            .is_err()
    );
    assert!(
        db.insert(ORIGINS)
            .value("run_id", "future")
            .value("version", 2_i64)
            .execute(&*db)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn legal_dynamic_agent_replacement_keeps_verified_exclusion() {
    let (_temp, db, definition) = fixture().await;
    let owner = crate::WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "fence".into(),
    };
    rep::attempts::claim_authority(&*db, "covered", &owner)
        .await
        .unwrap();
    let mut node = definition.nodes["last"].clone();
    node.kind = bcode_workflow::NodeKind::Agent;
    node.configuration = serde_json::to_value(
        bcode_workflow::WorkflowPromptConfiguration::preserve_input("build", "observe"),
    )
    .unwrap();
    let edit = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "covered".into(),
        expected_revision: 1,
        mutation_id: "replace".into(),
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::ReplaceNode {
            node,
            entry: false,
            exit: true,
        }],
        reconciliation: vec![],
    };
    assert_eq!(
        rep::graph_revision::publish(&*db, &owner, &edit)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        rep::graph_revision::current(&*db, "covered")
            .await
            .unwrap()
            .1
            .nodes["last"]
            .kind,
        bcode_workflow::NodeKind::Agent
    );
    assert_eq!(
        coverage(&*db, "covered").await.unwrap(),
        Coverage::VerifiedExcludedV1
    );
}

#[tokio::test]
async fn future_persisted_origin_and_partial_schema_fail_closed_without_repair() {
    let (_temp, db, _) = database().await;
    assert_eq!(coverage(&*db, "run").await.unwrap(), Coverage::Unsupported);
    // Construct unsupported persisted state before contract installation, not by bypassing guards.
    let mut table = create_table(ORIGINS);
    for column in columns() {
        table = table.column(column);
    }
    table.primary_key("run_id").execute(&*db).await.unwrap();
    db.insert(ORIGINS)
        .value("run_id", "future")
        .value("version", 99_i64)
        .execute(&*db)
        .await
        .unwrap();
    assert!(coverage(&*db, "future").await.is_err());
    let tx = db.begin_transaction().await.unwrap();
    assert!(install_in(&*tx).await.is_err());
    tx.rollback().await.unwrap();
    assert_eq!(
        db.select(ORIGINS)
            .where_eq("run_id", "future")
            .limit(1)
            .execute(&*db)
            .await
            .unwrap()[0]
            .get("version"),
        Some(DatabaseValue::Int64(99))
    );
}

#[tokio::test]
async fn missing_index_is_damage_not_empty_proof() {
    let (_temp, db, _) = fixture().await;
    // Explicit typed damage fixture; coverage must not rebuild an index on reads.
    db.drop_index(
        "permission_exclusion_workflow_decisions_run_v1",
        "workflow_decisions",
    )
    .execute(&*db)
    .await
    .unwrap();
    assert!(coverage(&*db, "covered").await.is_err());
}
