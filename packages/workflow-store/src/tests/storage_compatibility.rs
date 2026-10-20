//! Existing storage compatibility behavioral acceptance.
use super::*;
use crate::database::block_on;
use switchy_database::schema::{
    Column, DataType, alter_table, create_table, drop_index, drop_table,
};

fn fixture_open(
    path: &std::path::Path,
    foreign_keys: bool,
) -> switchy_database::rusqlite::RusqliteDatabase {
    database::open(
        path,
        switchy_database_connection::SqliteAccess::ReadWriteCreate,
        std::time::Duration::from_secs(5),
        foreign_keys,
    )
    .expect("fixture database")
}

#[test]
fn inherited_cancellation_refuses_incompatible_contract_without_mutation() {
    let (temp, mut store) = initialized_store();
    let authority = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "child-owner".into(),
        generation: 1,
        fencing_token: "child-fence".into(),
    };
    store
        .create_run(&NewWorkflowRun {
            run_id: "child".into(),
            execution_authority: Some(authority.clone()),
            ..new_run()
        })
        .expect("child");
    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        "example",
        &definition("example"),
    )
    .expect("identity");
    let target =
        serde_json::to_string(&bcode_workflow::WorkflowCallTarget::Definition { identity })
            .expect("target");
    block_on(
        insert("workflow_run_links")
            .value("root_run_id", "run-1")
            .value("parent_run_id", "run-1")
            .value("parent_node_id", "review")
            .value("parent_activation_id", "activation")
            .value("parent_attempt", 1)
            .value("child_run_id", "child")
            .value("version", WORKFLOW_RUN_LINK_VERSION)
            .value("target_json", target)
            .value("depth", 2)
            .value("created_at_ms", 0)
            .execute(&store.connection),
    )
    .expect("link");
    store
        .request_cancellation("run-1", 21)
        .expect("parent cancellation");
    let foreign = fixture_open(store.path(), false);
    block_on(
        update("workflow_storage_compatibility")
            .value("compatibility", WORKFLOW_STORE_COMPATIBILITY + 1)
            .where_eq("contract_id", 1)
            .execute(&foreign),
    )
    .expect("incompatible contract");
    let error = store
        .inherit_parent_cancellation_owned("child", &authority, 22)
        .expect_err("child must not inherit across incompatible contract");
    assert!(matches!(error, WorkflowStoreError::UnsupportedStore { .. }));
    let requested: Option<u64> = database::optional_unsigned(
        &fixture_rows(
            &foreign,
            &select("workflow_runs")
                .columns(&["cancellation_requested_at_ms"])
                .where_eq("run_id", "child"),
        )[0],
        "cancellation_requested_at_ms",
    )
    .expect("cancellation state");
    assert_eq!(requested, None);
    drop(store);
    assert!(matches!(
        WorkflowStore::open_in_state_dir(temp.path()),
        Err(WorkflowStoreError::UnsupportedStore { .. })
    ));
}
#[test]
fn genuine_schema_42_upgrade_preserves_cap_and_defers_to_old_owner() {
    let temp = tempfile::tempdir().expect("temp");
    let root = temp.path().join("workflows");
    std::fs::create_dir_all(&root).expect("root");
    let path = root.join(DATABASE_FILE);
    let connection = fixture_open(&path, false);
    schema::initialize(&connection, 42).expect("actual old schema");
    let mut old = WorkflowStore {
        path,
        connection,
        ownership: None,
        read_identity: std::sync::Arc::new(()),
    };
    old.persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let mut run = new_run();
    run.limits.concurrency_cap = Some(7);
    old.create_run(&run).expect("numeric cap");
    let before = fixture_changes(&old.connection);
    let ownership = open_ownership_file(&root).expect("ownership");
    ownership.lock_shared().expect("old reader");
    assert!(WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 20).is_err());
    assert_eq!(detected_store_schema(&old.connection), Some(42));
    assert_eq!(fixture_changes(&old.connection), before);
    assert_eq!(
        old.run_limits(&run.run_id)
            .unwrap()
            .unwrap()
            .concurrency_cap,
        Some(7)
    );
    drop(ownership);
    drop(old);
    WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 20).expect("exclusive upgrade");
    let upgraded = WorkflowStore::open_in_state_dir(temp.path()).expect("new reader");
    assert_eq!(
        upgraded
            .run_limits(&run.run_id)
            .unwrap()
            .unwrap()
            .concurrency_cap,
        Some(7)
    );
    assert_eq!(detected_store_schema(&upgraded.connection), Some(43));
    let contracts = database::rows(
        &upgraded.connection,
        &switchy_database::query::select("workflow_storage_compatibility")
            .columns(&["compatibility"])
            .limit(1),
    )
    .unwrap();
    assert_eq!(
        contracts[0].get("compatibility"),
        Some(switchy_database::DatabaseValue::Int64(3))
    );
}
#[test]
fn retained_handle_refuses_pending_activation_after_incompatible_contract_change() {
    let (temp, mut store) = initialized_store();
    let foreign = fixture_open(store.path(), false);
    block_on(
        update("workflow_storage_compatibility")
            .value("compatibility", WORKFLOW_STORE_COMPATIBILITY + 1)
            .where_eq("contract_id", 1)
            .execute(&foreign),
    )
    .expect("incompatible contract");
    let error = store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect_err("retained handle must refuse pending activation");
    assert!(matches!(error, WorkflowStoreError::UnsupportedStore { .. }));
    let count: u64 = fixture_unsigned(
        &fixture_rows(
            &foreign,
            &select("workflow_attempts")
                .project_as(switchy_database::query::count_all(), "count")
                .where_eq("run_id", "run-1"),
        )[0],
        "count",
    );
    assert_eq!(count, 0);
    drop(store);
    assert!(matches!(
        WorkflowStore::open_in_state_dir(temp.path()),
        Err(WorkflowStoreError::UnsupportedStore { .. })
    ));
}
#[test]
fn retained_handle_refuses_prepared_attempt_after_incompatible_contract_change() {
    let (temp, mut store) = initialized_store();
    let foreign = fixture_open(store.path(), false);
    block_on(
        update("workflow_storage_compatibility")
            .value("compatibility", WORKFLOW_STORE_COMPATIBILITY + 1)
            .where_eq("contract_id", 1)
            .execute(&foreign),
    )
    .expect("incompatible contract");
    let attempt = PreparedAttempt {
        run_id: "run-1".into(),
        node_id: "review".into(),
        activation_id: activation_id(),
        attempt: 1,
        side_effect: DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({"operation": "review"}),
        prepared_at_ms: 12,
    };
    let error = store
        .prepare_attempt(&attempt)
        .expect_err("retained handle must refuse dispatch preparation");
    assert!(matches!(error, WorkflowStoreError::UnsupportedStore { .. }));
    let count: u64 = fixture_unsigned(
        &fixture_rows(
            &foreign,
            &select("workflow_attempts")
                .project_as(switchy_database::query::count_all(), "count")
                .where_eq("run_id", "run-1"),
        )[0],
        "count",
    );
    assert_eq!(count, 0);
    drop(store);
    assert!(matches!(
        WorkflowStore::open_in_state_dir(temp.path()),
        Err(WorkflowStoreError::UnsupportedStore { .. })
    ));
}
#[test]
fn schema_fourteen_migration_preserves_runs_and_adds_authority_columns() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let definition = definition("example");
    store
        .persist_definition("example", 1, &definition)
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    let path = store.path().to_path_buf();
    drop(store);
    let connection = fixture_open(&path, false);
    block_on(drop_table("workflow_run_graph_edges").execute(&connection))
        .expect("schema 14 fixture");
    block_on(drop_table("workflow_run_graph_nodes").execute(&connection))
        .expect("schema 14 fixture");
    block_on(drop_table("workflow_run_graphs").execute(&connection)).expect("schema 14 fixture");
    block_on(
        alter_table("workflow_runs")
            .drop_column("target_artifact_id".into())
            .execute(&connection),
    )
    .expect("schema 14 fixture");
    block_on(
        alter_table("workflow_runs")
            .drop_column("coordinator_daemon_instance_id".into())
            .execute(&connection),
    )
    .expect("schema 14 fixture");
    block_on(
        alter_table("workflow_runs")
            .drop_column("coordinator_generation".into())
            .execute(&connection),
    )
    .expect("schema 14 fixture");
    block_on(
        alter_table("workflow_runs")
            .drop_column("coordinator_fencing_token".into())
            .execute(&connection),
    )
    .expect("schema 14 fixture");
    fixture_historical_activations(&connection, false).expect("schema 14 fixture");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 14)
            .where_eq("contract_id", 1)
            .execute(&connection),
    )
    .expect("schema 14 fixture");
    drop(connection);

    let receipt = WorkflowStore::migrate_schema_14_to_current_in_state_dir(temp.path(), 30)
        .expect("migration");

    assert_eq!(receipt.previous_schema_version, 14);
    assert_eq!(receipt.new_schema_version, WORKFLOW_STORE_SCHEMA_VERSION);
    assert!(receipt.backup_path.is_file());
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("current store");
    assert!(reopened.run_summary("run-1").expect("run").is_some());
    assert_eq!(
        reopened.execution_authority("run-1").expect("authority"),
        None
    );
}
#[test]
fn startup_upgrade_preserves_runs_and_ordinary_reads_do_not_migrate() {
    let temp = historical_startup_fixture();
    assert!(matches!(
        WorkflowStore::open_in_state_dir(temp.path()),
        Err(WorkflowStoreError::UnsupportedStore {
            actual: Some(15),
            ..
        })
    ));
    let store = WorkflowStore::initialize_in_state_dir(temp.path(), 901).expect("upgrade");
    assert!(store.run_summary("run-1").expect("run").is_some());
    assert!(store.run_graph_revision("run-1").expect("graph").is_some());
    assert_eq!(
        detected_store_schema(&store.connection),
        Some(WORKFLOW_STORE_SCHEMA_VERSION)
    );
    let reopened = WorkflowStore::initialize_in_state_dir(temp.path(), 901).expect("current store");
    assert!(reopened.run_summary("run-1").expect("run").is_some());
    let mut next = new_run();
    next.run_id = "after-upgrade".to_string();
    let mut reopened = reopened;
    reopened
        .create_run(&next)
        .expect("admit work after upgrade");
}
#[test]
fn retirement_preserves_history_and_current_page_continuations_after_upgrade() {
    let (temp, store) = initialized_store();
    remove_retirement_fixture(&store);
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 20)
            .execute(&store.connection),
    )
    .expect("old contract");
    drop(store);
    let store = WorkflowStore::initialize_in_state_dir(temp.path(), 901).expect("upgrade");
    publish_retirement_fixture(&store);
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .current_run_graph_node("run-1", "review")
            .expect("retired")
            .is_none()
    );
    assert!(
        store
            .current_run_graph_edge("run-1", 0)
            .expect("retired edge")
            .is_none()
    );
    assert!(
        store
            .run_graph_node_revision("run-1", "review", 1)
            .expect("history")
            .is_some()
    );
    let page = store
        .current_run_graph_page("run-1", Some(2), None, None, 1)
        .expect("page");
    assert_eq!(page.nodes.len(), 1);
    assert_eq!(page.nodes[0].node.id, "after");
    assert!(page.nodes_complete && page.edges_complete);
    assert!(page.edges.is_empty());
    assert!(
        store
            .current_run_graph_nodes("run-1", 2, Some("after"), 1)
            .expect("continuation")
            .is_empty()
    );
    assert_eq!(before, fixture_changes(&store.connection));
    block_on(
        update("workflow_run_graph_nodes")
            .value("retired_at_revision", 3)
            .where_eq("node_id", "review")
            .execute(&store.connection),
    )
    .expect("future retirement fixture");
    assert!(
        store
            .current_run_graph_page("run-1", Some(2), None, None, 1)
            .is_err()
    );
}
#[test]
fn schema_38_upgrade_adds_quiescence_without_changing_runs() {
    let (temp, store) = initialized_store();
    block_on(drop_table("workflow_quiescence").execute(&store.connection)).expect("old fixture");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 38)
            .execute(&store.connection),
    )
    .expect("old fixture");
    drop(store);
    let store = WorkflowStore::initialize_in_state_dir(temp.path(), 2000).expect("upgrade");
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Running
    );
    recovery::verify(&store.connection).expect("guards");
    block_on(
        store
            .connection
            .drop_trigger("quiescence_blocks_reopen", false),
    )
    .expect("damage");
    drop(store);
    assert!(WorkflowStore::open_in_state_dir(temp.path()).is_err());
}
#[test]
fn schema_37_upgrade_preserves_discovery_and_adds_replacement_intents() {
    let (temp, mut store) = initialized_store();
    store
        .advance_continuation_discovery("artifact", 1)
        .expect("cursor");
    block_on(drop_table("workflow_replacement_intents").execute(&store.connection))
        .expect("old schema");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 37)
            .execute(&store.connection),
    )
    .expect("old schema");
    drop(store);
    let mut store = WorkflowStore::initialize_in_state_dir(temp.path(), 1001).expect("upgrade");
    assert!(
        store
            .advance_continuation_discovery("artifact", 1)
            .expect("retained cursor")
            .is_empty()
    );
    recovery::verify(&store.connection).expect("current schema");
    block_on(drop_table("workflow_replacement_intents").execute(&store.connection))
        .expect("damage");
    drop(store);
    assert!(WorkflowStore::initialize_in_state_dir(temp.path(), 1002).is_err());
}
#[test]
fn schema_36_upgrade_adds_durable_discovery_without_changing_authority() {
    let (temp, mut store, run, authority, _) = connected_publication_fixture();
    store
        .enter_recovery_only(&run.run_id, &authority, 20)
        .expect("barrier");
    block_on(drop_table("workflow_discovery_cursors").execute(&store.connection))
        .expect("previous schema");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 36)
            .execute(&store.connection),
    )
    .expect("previous schema");
    drop(store);
    let mut store = WorkflowStore::initialize_in_state_dir(temp.path(), 999).expect("upgrade");
    assert_eq!(
        store.execution_authority(&run.run_id).expect("authority"),
        Some(authority)
    );
    assert!(store.is_recovery_only(&run.run_id).expect("barrier"));
    assert_eq!(
        store
            .advance_continuation_discovery("artifact", 1)
            .expect("page"),
        [run.run_id]
    );
    block_on(drop_table("workflow_discovery_cursors").execute(&store.connection)).expect("damage");
    drop(store);
    assert!(WorkflowStore::initialize_in_state_dir(temp.path(), 1000).is_err());
}
#[test]
fn schema_35_index_upgrade_preserves_recovery_cursor_and_barrier() {
    let (temp, mut store, run, authority, _) = connected_publication_fixture();
    store
        .enter_recovery_only(&run.run_id, &authority, 20)
        .expect("barrier");
    let cursor = "a".repeat(64);
    block_on(
        insert("workflow_receipt_cursors")
            .value("run_id", run.run_id.as_str())
            .value("after_dispatch_identity", cursor.as_str())
            .execute(&store.connection),
    )
    .expect("cursor");
    block_on(
        drop_index("workflow_receipt_recovery_page", "workflow_attempts")
            .execute(&store.connection),
    )
    .expect("previous schema");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 35)
            .execute(&store.connection),
    )
    .expect("previous schema");
    drop(store);
    let store = WorkflowStore::initialize_in_state_dir(temp.path(), 997).expect("upgrade");
    assert!(store.is_recovery_only(&run.run_id).expect("barrier"));
    let retained: String = fixture_text(
        &fixture_rows(
            &store.connection,
            &select("workflow_receipt_cursors")
                .columns(&["after_dispatch_identity"])
                .where_eq("run_id", run.run_id.as_str()),
        )[0],
        "after_dispatch_identity",
    );
    assert_eq!(retained, cursor);
    recovery::verify(&store.connection).expect("indexed recovery available");
    block_on(
        drop_index("workflow_receipt_recovery_page", "workflow_attempts")
            .execute(&store.connection),
    )
    .expect("damage current index");
    drop(store);
    assert!(WorkflowStore::open_in_state_dir(temp.path()).is_err());
    assert!(WorkflowStore::initialize_in_state_dir(temp.path(), 998).is_err());
}
#[test]
fn damaged_schema_35_barrier_is_not_repaired_by_upgrade() {
    let (temp, store) = initialized_store();
    block_on(
        store
            .connection
            .drop_trigger("recovery_blocks_resume", false),
    )
    .expect("damaged previous schema");
    block_on(
        drop_index("workflow_receipt_recovery_page", "workflow_attempts")
            .execute(&store.connection),
    )
    .expect("damaged previous schema");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 35)
            .execute(&store.connection),
    )
    .expect("damaged previous schema");
    drop(store);
    assert!(WorkflowStore::initialize_in_state_dir(temp.path(), 996).is_err());
    let connection = fixture_open(&workflow_database_path(temp.path()), false);
    assert_eq!(detected_store_schema(&connection), Some(35));
    assert!(
        !block_on(connection.trigger_exists("recovery_blocks_resume")).expect("preserved damage")
    );
}
#[test]
fn schema_34_upgrade_adds_receipt_cursor_without_changing_recovery_barrier() {
    let (temp, mut store, run, authority, _) = connected_publication_fixture();
    store
        .enter_recovery_only(&run.run_id, &authority, 20)
        .expect("barrier");
    block_on(drop_table("workflow_receipt_cursors").execute(&store.connection)).expect("schema 34");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 34)
            .execute(&store.connection),
    )
    .expect("schema 34");
    drop(store);
    let mut store = WorkflowStore::initialize_in_state_dir(temp.path(), 995).expect("upgrade");
    assert!(
        store
            .is_recovery_only(&run.run_id)
            .expect("preserved barrier")
    );
    assert!(store.resume_run_owned(&run.run_id, 21, &authority).is_err());
    assert_eq!(
        fixture_unsigned(
            &fixture_rows(
                &store.connection,
                &select("workflow_receipt_cursors")
                    .project_as(switchy_database::query::count_all(), "count")
            )[0],
            "count"
        ),
        0
    );
}
#[test]
fn schema_33_upgrade_installs_recovery_barrier_without_changing_run() {
    let (temp, store) = initialized_store();
    let before = store.run_summary("run-1").expect("summary");
    block_on(
        store
            .connection
            .drop_trigger("recovery_blocks_attempt_insert", false),
    )
    .expect("schema 33 fixture");
    block_on(
        store
            .connection
            .drop_trigger("recovery_blocks_resume", false),
    )
    .expect("schema 33 fixture");
    block_on(
        store
            .connection
            .drop_trigger("recovery_blocks_handoff", false),
    )
    .expect("schema 33 fixture");
    block_on(drop_table("workflow_recovery_barriers").execute(&store.connection))
        .expect("schema 33 fixture");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 33)
            .execute(&store.connection),
    )
    .expect("schema 33 fixture");
    drop(store);
    assert!(matches!(
        WorkflowStore::open_in_state_dir(temp.path()),
        Err(WorkflowStoreError::UnsupportedStore { .. })
    ));
    let store = WorkflowStore::initialize_in_state_dir(temp.path(), 993).expect("upgrade");
    assert_eq!(store.run_summary("run-1").expect("preserved"), before);
    recovery::verify(&store.connection).expect("barriers installed");
    assert_eq!(
        fixture_unsigned(
            &fixture_rows(
                &store.connection,
                &select("workflow_recovery_barriers")
                    .project_as(switchy_database::query::count_all(), "count")
            )[0],
            "count"
        ),
        0
    );
}
#[test]
fn schema_32_missing_package_bindings_upgrade_preserves_runs_and_allows_admission() {
    let (temp, store) = initialized_store();
    block_on(drop_table("workflow_run_packages").execute(&store.connection))
        .expect("historical upgrade gap");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 32)
            .execute(&store.connection),
    )
    .expect("historical upgrade gap");
    drop(store);
    assert!(matches!(
        WorkflowStore::open_in_state_dir(temp.path()),
        Err(WorkflowStoreError::UnsupportedStore { .. })
    ));
    let mut store = WorkflowStore::initialize_in_state_dir(temp.path(), 990).expect("safe upgrade");
    assert!(store.run_summary("run-1").expect("preserved run").is_some());
    let mut run = new_run();
    run.run_id = "after-upgrade".into();
    assert!(
        store
            .create_run_with_package(&run, None)
            .expect("admission")
    );
    let backup = fixture_open(
        &temp
            .path()
            .join("workflows")
            .join(MIGRATION_BACKUP_DIRECTORY)
            .join("workflow-990.db"),
        false,
    );
    assert_eq!(detected_store_schema(&backup), Some(32));
    assert!(block_on(backup.validate_select(&select("workflow_run_packages"))).is_err());
}
#[test]
fn schema_32_missing_bindings_with_publications_preserves_ambiguous_state() {
    let (temp, store) = initialized_store();
    block_on(drop_table("workflow_run_packages").execute(&store.connection))
        .expect("ambiguous fixture");
    block_on(
        insert("workflow_package_publications")
            .value("package_id", "package")
            .value("package_lock_digest_sha256", "digest")
            .value("lock_json", "{}")
            .value("receipt_json", "{}")
            .value("published_at_ms", 1)
            .execute(&store.connection),
    )
    .expect("ambiguous fixture");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 32)
            .execute(&store.connection),
    )
    .expect("ambiguous fixture");
    drop(store);
    assert!(WorkflowStore::initialize_in_state_dir(temp.path(), 992).is_err());
    let connection = fixture_open(&workflow_database_path(temp.path()), false);
    assert_eq!(detected_store_schema(&connection), Some(32));
    assert!(block_on(connection.validate_select(&select("workflow_run_packages"))).is_err());
    assert_eq!(
        fixture_unsigned(
            &fixture_rows(
                &connection,
                &select("workflow_package_publications")
                    .project_as(switchy_database::query::count_all(), "count")
            )[0],
            "count"
        ),
        1
    );
}
#[test]
fn retained_handle_refuses_authority_transfer_after_incompatible_contract_change() {
    let (temp, mut store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("fixture authority");
    let expected = store
        .execution_authority("run-1")
        .expect("authority read")
        .expect("owner");
    let replacement = WorkflowExecutionAuthority {
        target_artifact_id: expected.target_artifact_id.clone(),
        daemon_instance_id: "daemon-b".into(),
        generation: expected.generation + 1,
        fencing_token: "token-b".into(),
    };
    let foreign = fixture_open(store.path(), false);
    block_on(
        update("workflow_storage_compatibility")
            .value("compatibility", WORKFLOW_STORE_COMPATIBILITY + 1)
            .where_eq("contract_id", 1)
            .execute(&foreign),
    )
    .expect("incompatible contract");
    let error = store
        .transfer_execution_authority("run-1", &expected, &replacement, 901)
        .expect_err("retained handle must refuse handoff");
    assert!(matches!(error, WorkflowStoreError::UnsupportedStore { .. }));
    assert_eq!(
        store
            .execution_authority("run-1")
            .expect("authority unchanged"),
        Some(expected)
    );
    drop(store);
    assert!(matches!(
        WorkflowStore::open_in_state_dir(temp.path()),
        Err(WorkflowStoreError::UnsupportedStore { .. })
    ));
}
#[test]
fn retained_handle_refuses_owned_cancellation_after_incompatible_contract_change() {
    let (temp, mut store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("fixture authority");
    let authority = store
        .execution_authority("run-1")
        .expect("authority read")
        .expect("run authority");
    let foreign = fixture_open(store.path(), false);
    block_on(
        update("workflow_storage_compatibility")
            .value("compatibility", WORKFLOW_STORE_COMPATIBILITY + 1)
            .where_eq("contract_id", 1)
            .execute(&foreign),
    )
    .expect("incompatible contract");
    let error = store
        .request_cancellation_owned("run-1", 901, &authority)
        .expect_err("retained handle must refuse cancellation");
    assert!(matches!(error, WorkflowStoreError::UnsupportedStore { .. }));
    let (status, requested): (String, Option<u64>) = {
        let rows = fixture_rows(
            &foreign,
            &select("workflow_runs")
                .columns(&["status", "cancellation_requested_at_ms"])
                .where_eq("run_id", "run-1"),
        );
        (
            fixture_text(&rows[0], "status"),
            database::optional_unsigned(&rows[0], "cancellation_requested_at_ms")
                .expect("run state"),
        )
    };
    assert_eq!(status, "running");
    assert_eq!(requested, None);
    drop(store);
    assert!(matches!(
        WorkflowStore::open_in_state_dir(temp.path()),
        Err(WorkflowStoreError::UnsupportedStore { .. })
    ));
}
#[test]
fn retained_handle_refuses_owned_control_after_incompatible_contract_change() {
    let (temp, mut store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("fixture authority");
    let authority = store
        .execution_authority("run-1")
        .expect("authority read")
        .expect("run authority");
    let foreign = fixture_open(store.path(), false);
    block_on(
        update("workflow_storage_compatibility")
            .value("compatibility", WORKFLOW_STORE_COMPATIBILITY + 1)
            .where_eq("contract_id", 1)
            .execute(&foreign),
    )
    .expect("incompatible contract");
    let error = store
        .pause_run_owned("run-1", 901, &authority)
        .expect_err("retained handle must refuse active control");
    assert!(matches!(error, WorkflowStoreError::UnsupportedStore { .. }));
    let status: String = fixture_text(
        &fixture_rows(
            &foreign,
            &select("workflow_runs")
                .columns(&["status"])
                .where_eq("run_id", "run-1"),
        )[0],
        "status",
    );
    assert_eq!(status, "running");
    drop(store);
    assert!(matches!(
        WorkflowStore::open_in_state_dir(temp.path()),
        Err(WorkflowStoreError::UnsupportedStore { .. })
    ));
}
#[test]
fn retained_handle_refuses_run_admission_after_incompatible_contract_change() {
    let (temp, mut store) = initialized_store();
    let foreign = fixture_open(store.path(), false);
    block_on(
        update("workflow_storage_compatibility")
            .value("compatibility", WORKFLOW_STORE_COMPATIBILITY + 1)
            .where_eq("contract_id", 1)
            .execute(&foreign),
    )
    .expect("incompatible contract");
    let mut run = new_run();
    run.run_id = "incompatible-admission".into();
    let error = store
        .create_run(&run)
        .expect_err("retained handle must refuse run admission");
    assert!(matches!(error, WorkflowStoreError::UnsupportedStore { .. }));
    let count: u64 = fixture_unsigned(
        &fixture_rows(
            &foreign,
            &select("workflow_runs")
                .project_as(switchy_database::query::count_all(), "count")
                .where_eq("run_id", run.run_id.as_str()),
        )[0],
        "count",
    );
    assert_eq!(count, 0);
    drop(store);
    assert!(matches!(
        WorkflowStore::open_in_state_dir(temp.path()),
        Err(WorkflowStoreError::UnsupportedStore { .. })
    ));
}
#[test]
fn retained_handle_refuses_definition_write_after_incompatible_contract_change() {
    let (temp, mut store) = initialized_store();
    let foreign = fixture_open(store.path(), false);
    block_on(
        update("workflow_storage_compatibility")
            .value("compatibility", WORKFLOW_STORE_COMPATIBILITY + 1)
            .where_eq("contract_id", 1)
            .execute(&foreign),
    )
    .expect("incompatible contract");
    let error = store
        .persist_definition("incompatible-write", 1, &definition("example"))
        .expect_err("retained handle must not write across compatibility boundary");
    assert!(matches!(error, WorkflowStoreError::UnsupportedStore { .. }));
    let count: u64 = fixture_unsigned(
        &fixture_rows(
            &foreign,
            &select("workflow_definitions")
                .project_as(switchy_database::query::count_all(), "count")
                .where_eq("definition_id", "incompatible-write"),
        )[0],
        "count",
    );
    assert_eq!(count, 0);
    drop(store);
    assert!(matches!(
        WorkflowStore::open_in_state_dir(temp.path()),
        Err(WorkflowStoreError::UnsupportedStore { .. })
    ));
}
#[test]
fn current_schema_missing_package_bindings_is_not_repaired_on_open() {
    let (temp, store) = initialized_store();
    block_on(drop_table("workflow_run_packages").execute(&store.connection)).expect("damage");
    drop(store);
    assert!(WorkflowStore::open_in_state_dir(temp.path()).is_err());
    assert!(WorkflowStore::initialize_in_state_dir(temp.path(), 991).is_err());
}
#[test]
fn startup_upgrade_supports_schema_14_and_16() {
    for schema in [14, 16] {
        let (temp, store) = initialized_store();
        remove_retirement_fixture(&store);
        if schema == 14 {
            block_on(drop_table("workflow_run_graph_edges").execute(&store.connection))
                .expect("schema 14");
            block_on(drop_table("workflow_run_graph_nodes").execute(&store.connection))
                .expect("schema 14");
            block_on(drop_table("workflow_run_graphs").execute(&store.connection))
                .expect("schema 14");
            block_on(
                alter_table("workflow_runs")
                    .drop_column("target_artifact_id".into())
                    .execute(&store.connection),
            )
            .expect("schema 14");
            block_on(
                alter_table("workflow_runs")
                    .drop_column("coordinator_daemon_instance_id".into())
                    .execute(&store.connection),
            )
            .expect("schema 14");
            block_on(
                alter_table("workflow_runs")
                    .drop_column("coordinator_generation".into())
                    .execute(&store.connection),
            )
            .expect("schema 14");
            block_on(
                alter_table("workflow_runs")
                    .drop_column("coordinator_fencing_token".into())
                    .execute(&store.connection),
            )
            .expect("schema 14");
        } else {
            block_on(
                drop_index(
                    "workflow_run_graph_edges_source",
                    "workflow_run_graph_edges",
                )
                .execute(&store.connection),
            )
            .expect("schema 16");
        }
        fixture_historical_activations(&store.connection, true).expect("historical activations");
        block_on(
            update("workflow_store_contract")
                .value("schema_version", schema)
                .execute(&store.connection),
        )
        .expect("historical contract");
        drop(store);
        let mut store =
            WorkflowStore::initialize_in_state_dir(temp.path(), 900).expect("supported upgrade");
        assert!(store.run_graph_revision("run-1").expect("graph").is_some());
        let mut run = new_run();
        run.run_id = "post-upgrade-run".to_string();
        assert!(
            store
                .create_run_with_package(&run, None)
                .expect("post-upgrade admission")
        );
    }
}
#[test]
fn startup_upgrade_concurrent_initializers_share_completed_store() {
    let temp = historical_startup_fixture();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let barrier = barrier.clone();
            let path = temp.path();
            scope.spawn(move || {
                barrier.wait();
                let result = WorkflowStore::initialize_in_state_dir(path, 902);
                barrier.wait();
                match result {
                    Ok(store) => assert!(store.run_summary("run-1").expect("run").is_some()),
                    Err(WorkflowStoreError::UpgradeOwnershipUnavailable) => {}
                    Err(error) => panic!("unexpected initialization failure: {error}"),
                }
            });
        }
    });
}
#[test]
fn startup_upgrade_retries_after_rollback_with_retained_backup() {
    let temp = historical_startup_fixture();
    let path = workflow_database_path(temp.path());
    let connection = fixture_open(&path, false);
    let checksum: String = fixture_text(
        &fixture_rows(
            &connection,
            &select("workflow_definitions")
                .columns(&["checksum_sha256"])
                .limit(1),
        )[0],
        "checksum_sha256",
    );
    block_on(
        update("workflow_definitions")
            .value("checksum_sha256", "damaged")
            .execute(&connection),
    )
    .expect("damage");
    assert!(WorkflowStore::initialize_in_state_dir(temp.path(), 903).is_err());
    assert_eq!(detected_store_schema(&connection), Some(15));
    block_on(
        update("workflow_definitions")
            .value("checksum_sha256", checksum.as_str())
            .execute(&connection),
    )
    .expect("restore fixture");
    drop(connection);
    let store =
        WorkflowStore::initialize_in_state_dir(temp.path(), 903).expect("retry same timestamp");
    assert!(store.run_summary("run-1").expect("run").is_some());
}
#[test]
fn startup_upgrade_preserves_future_schema_without_backup() {
    let temp = historical_startup_fixture();
    let path = workflow_database_path(temp.path());
    let connection = fixture_open(&path, false);
    block_on(drop_table("workflow_storage_compatibility").execute(&connection))
        .expect("unknown compatibility");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 999)
            .execute(&connection),
    )
    .expect("future");
    drop(connection);
    let before = std::fs::read(&path).expect("bytes");
    assert!(matches!(
        WorkflowStore::initialize_in_state_dir(temp.path(), 904),
        Err(WorkflowStoreError::UnsupportedStore {
            actual: Some(999),
            ..
        })
    ));
    assert_eq!(std::fs::read(&path).expect("bytes"), before);
    assert!(
        !path
            .parent()
            .expect("root")
            .join(MIGRATION_BACKUP_DIRECTORY)
            .exists()
    );
}
#[test]
fn startup_upgrade_crash_child() {
    let Ok(root) = std::env::var("BCODE_TEST_WORKFLOW_UPGRADE_CRASH_ROOT") else {
        return;
    };
    let path = workflow_database_path(Path::new(&root));
    let owner = open_ownership_file(path.parent().expect("root")).expect("owner");
    owner.lock().expect("exclusive");
    let connection = fixture_open(&path, false);
    let transaction =
        database::begin(&connection, TransactionMode::Immediate).expect("transaction");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", WORKFLOW_STORE_SCHEMA_VERSION)
            .execute(&*transaction),
    )
    .expect("uncommitted migration");
    std::process::exit(73);
}
#[test]
fn startup_upgrade_recovers_after_process_interruption() {
    let temp = historical_startup_fixture();
    let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "tests::storage_compatibility::startup_upgrade_crash_child",
        ])
        .env("BCODE_TEST_WORKFLOW_UPGRADE_CRASH_ROOT", temp.path())
        .status()
        .expect("child");
    assert_eq!(status.code(), Some(73));
    let store = WorkflowStore::initialize_in_state_dir(temp.path(), 906).expect("recover upgrade");
    assert!(store.run_graph_revision("run-1").expect("graph").is_some());
}
#[test]
fn continuation_upgrade_preserves_schema_39_while_legacy_owner_is_present() {
    let (temp, store) = initialized_store();
    block_on(drop_table("workflow_continuations").execute(&store.connection))
        .expect("schema 39 fixture");
    block_on(
        drop_index("workflow_events_kind_sequence", "workflow_events").execute(&store.connection),
    )
    .expect("schema 39 fixture");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 39)
            .execute(&store.connection),
    )
    .expect("schema 39 fixture");
    let path = store.path().to_path_buf();
    drop(store);
    // Historical schema-39 binaries retain this shared lock for the store lifetime.
    // This is legacy refusal coverage, not a substitute for mixed-version acceptance.
    let owner = open_ownership_file(path.parent().expect("root")).expect("legacy owner");
    owner.lock_shared().expect("legacy shared ownership");
    assert!(matches!(
        WorkflowStore::initialize_in_state_dir(temp.path(), 907),
        Err(WorkflowStoreError::UpgradeOwnershipUnavailable)
    ));
    let connection = fixture_open(&path, false);
    assert_eq!(detected_store_schema(&connection), Some(39));
    let status: String = fixture_text(
        &fixture_rows(
            &connection,
            &select("workflow_runs")
                .columns(&["status"])
                .where_eq("run_id", "run-1"),
        )[0],
        "status",
    );
    assert_eq!(status, "running");
    drop(connection);
    drop(owner);
    let upgraded =
        WorkflowStore::initialize_in_state_dir(temp.path(), 908).expect("upgrade after release");
    assert_eq!(
        upgraded
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Running
    );
    assert!(
        upgraded
            .continuation_lineage("run-1")
            .expect("lineage")
            .is_none()
    );
}
#[test]
fn compatible_additive_revision_remains_writable_with_retained_handles() {
    let (temp, mut original) = initialized_store();
    let mut upgraded = WorkflowStore::open_in_state_dir(temp.path()).expect("second handle");
    let tx = database::begin(&upgraded.connection, TransactionMode::Immediate)
        .expect("migration transaction");
    block_on(
        create_table("additive_extension")
            .column(Column {
                name: "id".into(),
                data_type: DataType::Int,
                nullable: true,
                auto_increment: false,
                default: None,
            })
            .primary_key("id")
            .execute(&*tx),
    )
    .expect("additive migration");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", WORKFLOW_STORE_SCHEMA_VERSION + 1)
            .execute(&*tx),
    )
    .expect("revision");
    database::commit(tx).expect("commit");
    assert!(original.pause_run("run-1", 20).expect("retained writer"));
    assert!(upgraded.resume_run("run-1", 21).expect("second writer"));
    drop(original);
    drop(upgraded);
    let reopened =
        WorkflowStore::initialize_in_state_dir(temp.path(), 910).expect("compatible reopen");
    assert_eq!(
        reopened
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Running
    );
    assert_eq!(
        detected_store_schema(&reopened.connection),
        Some(WORKFLOW_STORE_SCHEMA_VERSION + 1)
    );
    drop(reopened);
    assert!(WorkflowStore::reset_incompatible_store_in_state_dir(temp.path(), 911).is_err());
}
#[test]
fn schema_probe_retains_transient_lock_failure() {
    let (temp, store) = initialized_store();
    let path = store.path().to_path_buf();
    drop(store);
    let owner = fixture_open(&path, false);
    block_on(owner.sqlite_journal_mode(switchy_database::SqliteJournalMode::Delete))
        .expect("rollback journal");
    let observer = fixture_open(&path, false);
    block_on(observer.sqlite_set_busy_timeout(std::time::Duration::ZERO)).expect("no wait");
    let transaction =
        database::begin(&owner, TransactionMode::Exclusive).expect("exclusive transaction");
    let error = verify_store_schema(&observer).expect_err("locked schema");
    assert!(error.is_transient_initialization_failure(), "{error}");
    block_on(transaction.rollback()).expect("release");
    verify_store_schema(&observer).expect("same observer recovers");
    drop(observer);
    WorkflowStore::initialize_in_state_dir(temp.path(), 913).expect("canonical reopen");
}
#[test]
fn unknown_compatibility_preserves_current_and_future_revisions() {
    for revision in [
        41,
        WORKFLOW_STORE_SCHEMA_VERSION,
        WORKFLOW_STORE_SCHEMA_VERSION + 1,
    ] {
        let (temp, store) = initialized_store();
        block_on(
            update("workflow_store_contract")
                .value("schema_version", revision)
                .execute(&store.connection),
        )
        .expect("revision");
        block_on(
            update("workflow_storage_compatibility")
                .value("compatibility", 999)
                .execute(&store.connection),
        )
        .expect("unknown contract");
        let path = store.path().to_path_buf();
        drop(store);
        let before = std::fs::read(&path).expect("before");
        assert!(matches!(
            WorkflowStore::initialize_in_state_dir(temp.path(), 912),
            Err(WorkflowStoreError::UnsupportedStore { .. })
        ));
        assert_eq!(std::fs::read(path).expect("after"), before);
    }
}
#[test]
fn clean_break_transition_preserves_schema_40_runs_and_authority() {
    let (temp, store) = initialized_store();
    let before = store.run_summary("run-1").expect("summary");
    let authority = store.execution_authority("run-1").expect("authority");
    block_on(drop_table("workflow_storage_compatibility").execute(&store.connection))
        .expect("pre-contract fixture");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 40)
            .execute(&store.connection),
    )
    .expect("pre-contract fixture");
    drop(store);
    assert!(matches!(
        WorkflowStore::open_in_state_dir(temp.path()),
        Err(WorkflowStoreError::UnsupportedStore {
            actual: Some(40),
            ..
        })
    ));
    let upgraded = WorkflowStore::initialize_in_state_dir(temp.path(), 920).expect("transition");
    assert_eq!(upgraded.run_summary("run-1").expect("summary"), before);
    assert_eq!(
        upgraded.execution_authority("run-1").expect("authority"),
        authority
    );
    assert_eq!(
        detected_store_schema(&upgraded.connection),
        Some(WORKFLOW_STORE_SCHEMA_VERSION)
    );
    assert!(
        upgraded
            .path()
            .parent()
            .expect("root")
            .join(MIGRATION_BACKUP_DIRECTORY)
            .is_dir()
    );
    drop(upgraded);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(reopened.run_summary("run-1").expect("summary"), before);
}
#[test]
fn startup_upgrade_does_not_revoke_old_store_ownership() {
    let temp = historical_startup_fixture();
    let path = workflow_database_path(temp.path());
    let owner = open_ownership_file(path.parent().expect("root")).expect("owner");
    owner.lock_shared().expect("old owner");
    let started = std::time::Instant::now();
    assert!(matches!(
        WorkflowStore::initialize_in_state_dir(temp.path(), 905),
        Err(WorkflowStoreError::UpgradeOwnershipUnavailable)
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    let connection = fixture_open(&path, false);
    assert_eq!(detected_store_schema(&connection), Some(15));
    drop(connection);
    drop(owner);
    let upgraded = WorkflowStore::initialize_in_state_dir(temp.path(), 905)
        .expect("upgrade after historical store owner releases ownership");
    assert_eq!(
        detected_store_schema(&upgraded.connection),
        Some(WORKFLOW_STORE_SCHEMA_VERSION)
    );
    assert!(
        upgraded
            .run_summary("run-1")
            .expect("preserved run")
            .is_some()
    );
    assert!(
        upgraded
            .run_graph_revision("run-1")
            .expect("migrated graph")
            .is_some()
    );
}
#[test]
fn schema_migration_rolls_back_when_canonical_relationships_are_damaged() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    let path = store.path().to_path_buf();
    drop(store);
    let connection = fixture_open(&path, false);
    block_on(connection.sqlite_foreign_keys(false)).expect("damaged historical fixture");
    block_on(drop_table("workflow_run_graph_edges").execute(&connection))
        .expect("damaged historical fixture");
    block_on(drop_table("workflow_run_graph_nodes").execute(&connection))
        .expect("damaged historical fixture");
    block_on(drop_table("workflow_run_graphs").execute(&connection))
        .expect("damaged historical fixture");
    block_on(delete("workflow_definitions").execute(&connection))
        .expect("damaged historical fixture");
    block_on(
        alter_table("workflow_runs")
            .drop_column("target_artifact_id".into())
            .execute(&connection),
    )
    .expect("damaged historical fixture");
    block_on(
        alter_table("workflow_runs")
            .drop_column("coordinator_daemon_instance_id".into())
            .execute(&connection),
    )
    .expect("damaged historical fixture");
    block_on(
        alter_table("workflow_runs")
            .drop_column("coordinator_generation".into())
            .execute(&connection),
    )
    .expect("damaged historical fixture");
    block_on(
        alter_table("workflow_runs")
            .drop_column("coordinator_fencing_token".into())
            .execute(&connection),
    )
    .expect("damaged historical fixture");
    fixture_historical_activations(&connection, false).expect("damaged historical fixture");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 14)
            .where_eq("contract_id", 1)
            .execute(&connection),
    )
    .expect("damaged historical fixture");
    let before = damaged_migration_snapshot(&connection);
    assert_historical_owner_columns_absent(&connection);
    drop(connection);

    let error = WorkflowStore::migrate_schema_14_to_current_in_state_dir(temp.path(), 31)
        .expect_err("damaged relationships must not be committed");
    assert!(
        error
            .to_string()
            .contains("foreign-key verification failed")
    );
    let connection = fixture_open(&path, false);
    assert_eq!(detected_store_schema(&connection), Some(14));
    assert_historical_owner_columns_absent(&connection);
    assert_eq!(
        damaged_migration_snapshot(&connection),
        before,
        "schema and durable contracts must roll back together"
    );
    assert_eq!(
        fixture_unsigned(
            &fixture_rows(
                &connection,
                &select("workflow_runs").project_as(switchy_database::query::count_all(), "count")
            )[0],
            "count"
        ),
        1
    );
    let root = path.parent().expect("workflow root");
    assert!(
        root.join(MIGRATION_BACKUP_DIRECTORY)
            .join("workflow-31.db")
            .is_file()
    );
    assert!(!root.join(MIGRATION_RECEIPT_FILE).exists());
    assert!(WorkflowStore::open_in_state_dir(temp.path()).is_err());
    assert_eq!(detected_store_schema(&connection), Some(14));
}
type DamagedMigrationSnapshot = (
    Vec<Option<switchy_database::schema::TableInfo>>,
    Vec<Vec<switchy_database::Row>>,
);

fn damaged_migration_snapshot(db: &dyn Database) -> DamagedMigrationSnapshot {
    let schemas = [
        "workflow_runs",
        "workflow_activations",
        "workflow_run_graphs",
        "workflow_run_graph_nodes",
        "workflow_run_graph_edges",
    ]
    .into_iter()
    .map(|table| block_on(db.get_table_info(table)).expect("fixture schema snapshot"))
    .collect();
    let contracts = ["workflow_store_contract", "workflow_storage_compatibility"]
        .into_iter()
        .map(|table| {
            fixture_rows(
                db,
                &select(table).sort("contract_id", switchy_database::query::SortDirection::Asc),
            )
        })
        .collect();
    (schemas, contracts)
}

fn assert_historical_owner_columns_absent(db: &dyn Database) {
    let columns = block_on(db.get_table_columns("workflow_runs")).expect("historical run columns");
    for column in [
        "target_artifact_id",
        "coordinator_daemon_instance_id",
        "coordinator_generation",
        "coordinator_fencing_token",
    ] {
        assert!(
            columns.iter().all(|stored| stored.name != column),
            "historical fixture must not contain {column}"
        );
    }
}

#[test]
fn schema_17_upgrade_preserves_activation_executable_binding() {
    let (temp, store) = initialized_store();
    let original = store
        .activation_graph_node("run-1", "review", &activation_id())
        .expect("binding")
        .expect("executable");
    fixture_historical_activations(&store.connection, true).expect("schema 17 fixture");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 17)
            .execute(&store.connection),
    )
    .expect("schema 17 fixture");
    remove_retirement_fixture(&store);
    drop(store);
    let store = WorkflowStore::initialize_in_state_dir(temp.path(), 900).expect("upgrade");
    assert_eq!(
        store
            .activation_graph_node("run-1", "review", &activation_id())
            .expect("migrated binding"),
        Some(original)
    );
    assert!(
        store
            .activation_graph_node("run-1", "review", "absent")
            .expect("missing activation")
            .is_none()
    );
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        store
            .activation_graph_node("run-1", "review", &activation_id())
            .expect("durable binding")
            .expect("node")
            .revision,
        1
    );
}
#[test]
fn recursion_policy_survives_schema_41_upgrade_and_reopen() {
    let (temp, mut store) = initialized_store();
    let mut run = new_run();
    run.run_id = "policy-run".to_string();
    run.limits.recursion_depth_cap = 24;
    run.limits.descendant_cap = 256;
    store.create_run(&run).expect("run");
    assert_eq!(
        store.run_limits(&run.run_id).expect("limits"),
        Some(run.limits.clone())
    );
    block_on(
        alter_table("workflow_runs")
            .drop_column("recursion_depth_cap".into())
            .execute(&store.connection),
    )
    .expect("historical schema");
    block_on(
        alter_table("workflow_runs")
            .drop_column("descendant_cap".into())
            .execute(&store.connection),
    )
    .expect("historical schema");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 41)
            .execute(&store.connection),
    )
    .expect("historical schema");
    block_on(
        update("workflow_storage_compatibility")
            .value("compatibility", 1)
            .execute(&store.connection),
    )
    .expect("historical schema");
    drop(store);
    WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 42).expect("upgrade");
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let limits = reopened
        .run_limits(&run.run_id)
        .expect("limits")
        .expect("run");
    assert_eq!(limits.recursion_depth_cap, 8);
    assert_eq!(limits.descendant_cap, 64);
    assert_eq!(limits.node_execution_cap, run.limits.node_execution_cap);
}
#[test]
fn repeated_cancellation_finalizes_legacy_settled_run() {
    let (_temp, mut store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("cancellation_requested_at_ms", 20)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("legacy cancellation intent");

    assert!(
        !store
            .request_cancellation("run-1", 21)
            .expect("finalize existing intent")
    );
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Cancelled
    );
}
#[test]
fn schema_30_upgrade_preserves_runs_without_inventing_publication_intents() {
    let (temp, store, run, _, _) = connected_publication_fixture();
    block_on(drop_table("workflow_publication_cancellations").execute(&store.connection))
        .expect("old schema");
    block_on(drop_table("workflow_pending_publications").execute(&store.connection))
        .expect("old schema");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 30)
            .execute(&store.connection),
    )
    .expect("old schema");
    drop(store);
    let store = WorkflowStore::initialize_in_state_dir(temp.path(), 40).expect("upgrade");
    assert!(store.run_summary(&run.run_id).expect("summary").is_some());
    let count: u64 = fixture_unsigned(
        &fixture_rows(
            &store.connection,
            &select("workflow_publication_cancellations")
                .project_as(switchy_database::query::count_all(), "count"),
        )[0],
        "count",
    );
    assert_eq!(count, 0);
}
#[test]
fn run_creation_validates_each_entry_schema_and_rolls_back_atomically() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let mut definition = definition("entry-validation");
    definition.input.schema = serde_json::json!({});
    let mut first = definition.nodes["review"].clone();
    first.id = "first".to_string();
    first.input.schema = serde_json::json!({});
    definition.nodes.insert(first.id.clone(), first);
    definition.entries = vec!["first".to_string(), "review".to_string()];
    definition.exits.push("first".to_string());
    definition
        .nodes
        .get_mut("review")
        .expect("entry")
        .input
        .schema = serde_json::json!({"type": "string"});
    store
        .persist_definition("entry-validation", 1, &definition)
        .expect("definition");
    let error = store
        .create_run(&NewWorkflowRun {
            run_id: "entry-validation-run".to_string(),
            definition_id: "entry-validation".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: None,
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        })
        .expect_err("entry schema mismatch");
    assert!(error.to_string().contains("workflow entry input"));
    assert!(
        store
            .run_summary("entry-validation-run")
            .expect("summary")
            .is_none()
    );
    assert!(
        store
            .pending_activations(10)
            .expect("activations")
            .is_empty()
    );
    for table in [
        "workflow_run_graphs",
        "workflow_run_graph_nodes",
        "workflow_run_graph_edges",
    ] {
        let count = fixture_unsigned(
            &fixture_rows(
                &store.connection,
                &select(table)
                    .project_as(switchy_database::query::count_all(), "count")
                    .where_eq("run_id", "entry-validation-run"),
            )[0],
            "count",
        );
        assert_eq!(count, 0, "entry validation left rows in {table}");
    }
    let mut retry = new_run();
    retry.run_id = "entry-validation-run".to_string();
    retry.definition_id = "entry-validation".to_string();
    retry.input = Some(serde_json::json!("valid"));
    store.create_run(&retry).expect("retry after rollback");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen retry");
    let activations = store.pending_activations(10).expect("retry activations");
    assert_eq!(activations.len(), 2);
    for activation in &activations {
        assert_eq!(activation.input, retry.input);
        assert_eq!(activation.run_id, retry.run_id);
        assert_eq!(activation.dependency_generation, 0);
    }
    assert_eq!(
        activations
            .iter()
            .map(|activation| activation.node_id.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["first", "review"])
    );
    assert_eq!(
        store
            .run_graph_nodes(&retry.run_id, None, 10)
            .expect("retry graph")
            .len(),
        2
    );
}
#[test]
fn create_activation_validates_target_schema() {
    let (_temp, mut store) = initialized_store();
    let error = store
        .create_activation(&NewActivation {
            input: Some(serde_json::json!("wrong")),
            ..new_activation()
        })
        .expect_err("invalid activation input");
    assert!(error.to_string().contains("workflow activation input"));
}
#[test]
fn schema_29_upgrade_does_not_invent_dispatch_handoff_evidence() {
    let (temp, mut store, run, _, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            25,
        )
        .expect("prepare")
        .expect("pending");
    block_on(drop_table("workflow_dispatch_handoffs").execute(&store.connection))
        .expect("historical schema");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 29)
            .execute(&store.connection),
    )
    .expect("historical schema");
    drop(store);
    let mut store = WorkflowStore::initialize_in_state_dir(temp.path(), 26).expect("upgrade");
    let evidence: bool = !fixture_rows(
        &store.connection,
        &select("workflow_dispatch_handoffs")
            .where_eq("dispatch_identity", prepared.dispatch_identity.as_str()),
    )
    .is_empty();
    assert!(!evidence);
    assert_eq!(
        store
            .reconcile_prepared_attempts(10, 26)
            .expect("conservative recovery")
            .repair_required,
        vec![prepared.dispatch_identity]
    );
}
#[test]
fn validated_output_must_match_exact_compiled_node_schema() {
    let (_temp, mut store) = initialized_store();
    let activation_id = activation_identity("run-1", "review", 0);
    let output_count = |store: &WorkflowStore| {
        fixture_unsigned(
            &fixture_rows(
                &store.connection,
                &select("workflow_outputs")
                    .project_as(switchy_database::query::count_all(), "count"),
            )[0],
            "count",
        )
    };
    let activation_status = |store: &WorkflowStore| {
        fixture_text(
            &fixture_rows(
                &store.connection,
                &select("workflow_activations")
                    .columns(&["status"])
                    .where_eq("run_id", "run-1")
                    .where_eq("node_id", "review"),
            )[0],
            "status",
        )
    };

    let mut output = ValidatedOutput {
        output_id: "output-1".to_string(),
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id,
        schema_id: "wrong-schema".to_string(),
        schema_version: 1,
        value: serde_json::json!(1),
        artifact_reference: None,
        created_at_ms: 20,
    };
    assert!(store.persist_validated_output(&output).is_err());
    assert_eq!(output_count(&store), 0);
    assert_eq!(activation_status(&store), "pending");

    output.schema_id = "u32".to_string();
    output.value = serde_json::json!({"not": "an integer"});
    assert!(store.persist_validated_output(&output).is_err());
    assert_eq!(output_count(&store), 0);
    assert_eq!(activation_status(&store), "pending");
}
#[test]
fn incompatible_store_reset_is_exclusive_backed_up_verified_and_receipted() {
    let temp = tempfile::tempdir().expect("temp");
    let workflows = temp.path().join("workflows");
    std::fs::create_dir_all(workflows.join(ARTIFACT_DIRECTORY)).expect("artifact root");
    std::fs::write(
        workflows.join(ARTIFACT_DIRECTORY).join("output.bin"),
        b"artifact",
    )
    .expect("artifact");
    let path = workflows.join(DATABASE_FILE);
    let connection = fixture_open(&path, false);
    install_reset_legacy_payload_fixture(&connection);
    drop(connection);

    let receipt = WorkflowStore::reset_incompatible_store_in_state_dir(temp.path(), 42)
        .expect("explicit reset");
    assert_eq!(receipt.version, WORKFLOW_STORE_RESET_RECEIPT_VERSION);
    assert_eq!(receipt.previous_schema_version, Some(9));
    assert_eq!(receipt.new_schema_version, WORKFLOW_STORE_SCHEMA_VERSION);
    assert_eq!(receipt.removed_artifact_entries, 1);
    assert!(
        receipt
            .backup_path
            .join(ARTIFACT_DIRECTORY)
            .join("output.bin")
            .is_file()
    );
    assert_eq!(
        std::fs::read(
            receipt
                .backup_path
                .join(ARTIFACT_DIRECTORY)
                .join("output.bin")
        )
        .expect("backed-up artifact"),
        b"artifact"
    );
    assert_eq!(receipt.backup_sha256.len(), 64);
    assert!(receipt.backup_path.is_dir());
    assert!(!workflows.join(ARTIFACT_DIRECTORY).exists());
    let retained = database::open(
        &receipt.backup_path.join(DATABASE_FILE),
        switchy_database_connection::SqliteAccess::ReadOnly,
        std::time::Duration::from_secs(5),
        false,
    )
    .expect("backup");
    assert_eq!(
        fixture_text(
            &fixture_rows(&retained, &select("legacy_payload").columns(&["value"]))[0],
            "value"
        ),
        "retain me"
    );
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("current store");
    assert_eq!(
        detected_store_schema(&reopened.connection),
        Some(WORKFLOW_STORE_SCHEMA_VERSION)
    );
    drop(reopened);
    let persisted: WorkflowStoreResetReceipt = serde_json::from_slice(
        &std::fs::read(workflows.join(RESET_RECEIPT_FILE)).expect("receipt"),
    )
    .expect("receipt JSON");
    assert_eq!(persisted, receipt);
}
#[test]
fn incompatible_store_reset_refuses_active_owner_and_current_store() {
    let temp = tempfile::tempdir().expect("temp");
    let workflows = temp.path().join("workflows");
    std::fs::create_dir_all(&workflows).expect("workflow root");
    let path = workflows.join(DATABASE_FILE);
    let connection = fixture_open(&path, false);
    block_on(
        create_table("workflow_store_contract")
            .column(Column {
                name: "contract_id".into(),
                data_type: DataType::Int,
                nullable: true,
                auto_increment: false,
                default: None,
            })
            .primary_key("contract_id")
            .column(Column {
                name: "schema_version".into(),
                data_type: DataType::Int,
                nullable: false,
                auto_increment: false,
                default: None,
            })
            .execute(&connection),
    )
    .expect("legacy state");
    block_on(
        insert("workflow_store_contract")
            .value("contract_id", 1)
            .value("schema_version", 9)
            .execute(&connection),
    )
    .expect("legacy state");
    drop(connection);
    let owner = open_ownership_file(&workflows).expect("owner");
    owner.lock_shared().expect("shared owner");
    assert!(
        WorkflowStore::reset_incompatible_store_in_state_dir(temp.path(), 43).is_err(),
        "reset must not run under an active store owner"
    );
    drop(owner);
    WorkflowStore::reset_incompatible_store_in_state_dir(temp.path(), 43)
        .expect("reset after owner exits");
    assert!(
        WorkflowStore::reset_incompatible_store_in_state_dir(temp.path(), 44).is_err(),
        "current stores must not be destructively reset"
    );
}
#[test]
fn schema_nine_is_rejected_without_mutation() {
    let temp = tempfile::tempdir().expect("temp");
    let workflows = temp.path().join("workflows");
    std::fs::create_dir_all(&workflows).expect("workflows directory");
    let path = workflows.join(DATABASE_FILE);
    let connection = fixture_open(&path, false);
    install_schema_nine_fixture(&connection);
    drop(connection);

    let before = std::fs::read(&path).expect("legacy bytes");
    let error = WorkflowStore::open_in_state_dir(temp.path()).expect_err("unsupported store");
    assert!(matches!(
        error,
        WorkflowStoreError::UnsupportedStore {
            actual: Some(9),
            expected: WORKFLOW_STORE_SCHEMA_VERSION
        }
    ));
    assert_eq!(
        std::fs::read(&path).expect("unchanged legacy bytes"),
        before
    );
}
#[test]
fn schema_six_is_rejected_without_mutation() {
    let temp = tempfile::tempdir().expect("temp");
    let workflows = temp.path().join("workflows");
    std::fs::create_dir_all(&workflows).expect("workflows directory");
    let path = workflows.join(DATABASE_FILE);
    let connection = fixture_open(&path, false);
    block_on(
        create_table("workflow_store_contract")
            .column(Column {
                name: "contract_id".into(),
                data_type: DataType::Int,
                nullable: true,
                auto_increment: false,
                default: None,
            })
            .primary_key("contract_id")
            .check(E::column("contract_id").compare(C::Equal, E::Integer(1)))
            .column(Column {
                name: "schema_version".into(),
                data_type: DataType::Int,
                nullable: false,
                auto_increment: false,
                default: None,
            })
            .execute(&connection),
    )
    .expect("legacy contract");
    block_on(
        insert("workflow_store_contract")
            .value("contract_id", 1)
            .value("schema_version", 6)
            .execute(&connection),
    )
    .expect("legacy contract");
    drop(connection);

    let before = std::fs::read(&path).expect("legacy bytes");
    let error = WorkflowStore::open_in_state_dir(temp.path()).expect_err("unsupported store");
    assert!(matches!(
        error,
        WorkflowStoreError::UnsupportedStore {
            actual: Some(6),
            expected: WORKFLOW_STORE_SCHEMA_VERSION
        }
    ));
    assert_eq!(
        std::fs::read(&path).expect("unchanged legacy bytes"),
        before
    );
}
#[test]
fn schema_seven_is_rejected_without_mutation() {
    let temp = tempfile::tempdir().expect("temp");
    let workflows = temp.path().join("workflows");
    std::fs::create_dir_all(&workflows).expect("workflows directory");
    let path = workflows.join(DATABASE_FILE);
    let connection = fixture_open(&path, false);
    block_on(
        create_table("workflow_store_contract")
            .column(Column {
                name: "contract_id".into(),
                data_type: DataType::Int,
                nullable: true,
                auto_increment: false,
                default: None,
            })
            .primary_key("contract_id")
            .check(E::column("contract_id").compare(C::Equal, E::Integer(1)))
            .column(Column {
                name: "schema_version".into(),
                data_type: DataType::Int,
                nullable: false,
                auto_increment: false,
                default: None,
            })
            .execute(&connection),
    )
    .expect("legacy contract");
    block_on(
        insert("workflow_store_contract")
            .value("contract_id", 1)
            .value("schema_version", 7)
            .execute(&connection),
    )
    .expect("legacy contract");
    block_on(
        create_table("workflow_runs")
            .column(Column {
                name: "run_id".into(),
                data_type: DataType::Text,
                nullable: false,
                auto_increment: false,
                default: None,
            })
            .primary_key("run_id")
            .column(Column {
                name: "owner_plugin_id".into(),
                data_type: DataType::Text,
                nullable: true,
                auto_increment: false,
                default: None,
            })
            .column(Column {
                name: "workflow_kind".into(),
                data_type: DataType::Text,
                nullable: true,
                auto_increment: false,
                default: None,
            })
            .column(Column {
                name: "scope_key".into(),
                data_type: DataType::Text,
                nullable: true,
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
            .execute(&connection),
    )
    .expect("legacy contract");
    drop(connection);

    let before = std::fs::read(&path).expect("legacy bytes");
    let error = WorkflowStore::open_in_state_dir(temp.path()).expect_err("unsupported store");
    assert!(matches!(
        error,
        WorkflowStoreError::UnsupportedStore {
            actual: Some(7),
            expected: WORKFLOW_STORE_SCHEMA_VERSION
        }
    ));
    assert_eq!(
        std::fs::read(&path).expect("unchanged legacy bytes"),
        before
    );
}
#[test]
fn explicit_authored_repair_requires_confined_backup_and_repairs_only_safe_state() {
    let temp = tempfile::tempdir().expect("temp");
    let (workflow, draft, catalog) = authored_store_fixture();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store.create_authored_workflow(&workflow).expect("workflow");
    store.create_workflow_draft(&draft).expect("draft");
    let preview = draft.document.compilation_preview(&catalog, None);
    store
        .publish_workflow_draft(
            &workflow.workflow_id,
            &draft.draft_id,
            1,
            &preview,
            true,
            None,
            3,
        )
        .expect("publish");
    block_on(store.connection.sqlite_foreign_keys(false)).expect("disable foreign keys");
    block_on(
        update("authored_workflows")
            .value("active_revision", 99)
            .where_eq("workflow_id", workflow.workflow_id.as_str())
            .execute(&store.connection),
    )
    .expect("corrupt pointer");
    let outside = temp.path().join("outside").join("backup.db");
    assert!(
        store
            .repair_authored_workflow(&workflow.workflow_id, &outside, 4)
            .is_err()
    );
    let backup = store
        .path()
        .parent()
        .expect("root")
        .join("repair-backup.db");
    let report = store
        .repair_authored_workflow(&workflow.workflow_id, &backup, 4)
        .expect("repair");
    assert!(report.active_pointer_cleared);
    assert_eq!(report.indexes_recreated, 5);
    assert!(report.remaining_issues.is_empty());
    assert!(backup.is_file());
    assert_eq!(
        store
            .authored_workflow(&workflow.workflow_id)
            .expect("workflow")
            .expect("workflow")
            .active_revision,
        None
    );
    let backed_up = WorkflowStore::open_at_path(&backup).expect("backup store");
    assert_eq!(
        backed_up
            .diagnose_authored_workflow(&workflow.workflow_id, 10)
            .expect("backup diagnosis"),
        vec![WorkflowAuthoringIssue::InvalidActiveRevision { revision: 99 }]
    );
    assert!(
        store
            .repair_authored_workflow(&workflow.workflow_id, &backup, 5)
            .is_err(),
        "maintenance must not overwrite its retained backup evidence"
    );
}

fn publish_retirement_fixture(store: &WorkflowStore) {
    let source_nodes = fixture_rows(
        &store.connection,
        &select("workflow_run_graph_nodes")
            .columns(&["run_id", "revision", "node_json"])
            .where_eq("node_id", "review"),
    );
    for row in source_nodes {
        block_on(
            insert("workflow_run_graph_nodes")
                .value("run_id", fixture_text(&row, "run_id"))
                .value("node_id", "after")
                .value(
                    "revision",
                    database::integer_value(fixture_unsigned(&row, "revision")).expect("revision"),
                )
                .value(
                    "node_json",
                    fixture_text(&row, "node_json").replace("review", "after"),
                )
                .value("is_entry", 0)
                .value("is_exit", 0)
                .execute(&store.connection),
        )
        .expect("published retirement fixture");
    }
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0)
            .value("revision", 1)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value("edge_json", "{\"from\":\"review\",\"to\":\"review\"}")
            .execute(&store.connection),
    )
    .expect("published retirement fixture");
    block_on(
        update("workflow_run_graph_nodes")
            .value("retired_at_revision", 2)
            .where_eq("node_id", "review")
            .execute(&store.connection),
    )
    .expect("published retirement fixture");
    block_on(
        update("workflow_run_graph_edges")
            .value("retired_at_revision", 2)
            .execute(&store.connection),
    )
    .expect("published retirement fixture");
    block_on(
        update("workflow_run_graphs")
            .value("revision", 2)
            .execute(&store.connection),
    )
    .expect("published retirement fixture");
}

fn install_reset_legacy_payload_fixture(connection: &dyn Database) {
    block_on(
        create_table("workflow_store_contract")
            .column(Column {
                name: "contract_id".into(),
                data_type: DataType::Int,
                nullable: true,
                auto_increment: false,
                default: None,
            })
            .primary_key("contract_id")
            .check(E::column("contract_id").compare(C::Equal, E::Integer(1)))
            .column(Column {
                name: "schema_version".into(),
                data_type: DataType::Int,
                nullable: false,
                auto_increment: false,
                default: None,
            })
            .execute(connection),
    )
    .expect("legacy state");
    block_on(
        insert("workflow_store_contract")
            .value("contract_id", 1)
            .value("schema_version", 9)
            .execute(connection),
    )
    .expect("legacy state");
    block_on(
        create_table("legacy_payload")
            .column(Column {
                name: "value".into(),
                data_type: DataType::Text,
                nullable: false,
                auto_increment: false,
                default: None,
            })
            .execute(connection),
    )
    .expect("legacy state");
    block_on(
        insert("legacy_payload")
            .value("value", "retain me")
            .execute(connection),
    )
    .expect("legacy state");
}

fn install_schema_nine_fixture(connection: &dyn Database) {
    block_on(
        create_table("workflow_store_contract")
            .column(Column {
                name: "contract_id".into(),
                data_type: DataType::Int,
                nullable: true,
                auto_increment: false,
                default: None,
            })
            .primary_key("contract_id")
            .check(E::column("contract_id").compare(C::Equal, E::Integer(1)))
            .column(Column {
                name: "schema_version".into(),
                data_type: DataType::Int,
                nullable: false,
                auto_increment: false,
                default: None,
            })
            .execute(connection),
    )
    .expect("legacy contract");
    block_on(
        insert("workflow_store_contract")
            .value("contract_id", 1)
            .value("schema_version", 9)
            .execute(connection),
    )
    .expect("legacy contract");
    block_on(
        create_table("workflow_runs")
            .column(Column {
                name: "run_id".into(),
                data_type: DataType::Text,
                nullable: false,
                auto_increment: false,
                default: None,
            })
            .primary_key("run_id")
            .column(Column {
                name: "updated_at_ms".into(),
                data_type: DataType::Int,
                nullable: false,
                auto_increment: false,
                default: None,
            })
            .execute(connection),
    )
    .expect("legacy contract");
    block_on(
        create_table("workflow_attempts")
            .column(Column {
                name: "run_id".into(),
                data_type: DataType::Text,
                nullable: false,
                auto_increment: false,
                default: None,
            })
            .column(Column {
                name: "node_id".into(),
                data_type: DataType::Text,
                nullable: false,
                auto_increment: false,
                default: None,
            })
            .column(Column {
                name: "activation_id".into(),
                data_type: DataType::Text,
                nullable: false,
                auto_increment: false,
                default: None,
            })
            .column(Column {
                name: "attempt".into(),
                data_type: DataType::Int,
                nullable: false,
                auto_increment: false,
                default: None,
            })
            .primary_key_columns(["run_id", "node_id", "activation_id", "attempt"])
            .execute(connection),
    )
    .expect("legacy contract");
}
