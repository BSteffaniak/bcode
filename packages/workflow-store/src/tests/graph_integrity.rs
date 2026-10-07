//! Existing graph integrity behavioral acceptance.
use super::*;
use crate::database::block_on;
use crate::receipts::schedule_retry_for_observation_transaction;

#[test]
fn preparation_rejects_inconsistent_admission_graph_binding() {
    let (_temp, mut store) = initialized_store();
    block_on(
        store.connection.exec_update_count(
            &update("workflow_activation_graph_bindings")
                .value("graph_revision", 2_i64)
                .where_eq("run_id", "run-1"),
        ),
    )
    .expect("damaged binding");
    let before = fixture_changes(&store.connection);
    let error = store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect_err("inconsistent admission");
    assert!(error.to_string().contains("admission binding"));
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(
        store
            .attempt_history("run-1", None, 10)
            .expect("history")
            .is_empty()
    );
    block_on(store.connection.exec_delete_count(
        &delete("workflow_activation_graph_bindings").where_eq("run_id", "run-1"),
    ))
    .expect("historical unrecorded binding");
    assert!(
        store
            .prepare_pending_activation(
                "run-1",
                "review",
                &activation_id(),
                DispatchSideEffect::ReadOnly,
                serde_json::json!({"operation": "review"}),
                12,
            )
            .expect("historical initial graph")
            .is_some()
    );
}
#[test]
fn preparation_rejects_graph_advance_until_revised_scheduling_is_supported() {
    let (_temp, mut store) = initialized_store();
    let pending = store.pending_activations(1).expect("pending").remove(0);
    block_on(async {
        let fixture_source = select("workflow_run_graph_nodes")
            .execute(&store.connection)
            .await?;
        let fixture_inserted = fixture_source.len();
        for fixture_row in fixture_source {
            insert("workflow_run_graph_nodes")
                .value("run_id", fixture_row.get("run_id").expect("source column"))
                .value(
                    "node_id",
                    fixture_row.get("node_id").expect("source column"),
                )
                .value("revision", 2_i64)
                .value(
                    "node_json",
                    fixture_row.get("node_json").expect("source column"),
                )
                .value("is_entry", 0_i64)
                .value("is_exit", 0_i64)
                .execute(&store.connection)
                .await?;
        }
        store
            .connection
            .exec_update_count(&update("workflow_run_graphs").value("revision", 2_i64))
            .await?;
        Ok::<_, switchy_database::DatabaseError>(fixture_inserted)
    })
    .expect("revised graph fixture");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .prepare_pending_activation(
                &pending.run_id,
                &pending.node_id,
                &pending.activation_id,
                DispatchSideEffect::ReadOnly,
                serde_json::json!({}),
                20,
            )
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(store.pending_activations(1).is_err());
    assert_eq!(
        store
            .activation_graph_node(&pending.run_id, &pending.node_id, &pending.activation_id)
            .expect("binding")
            .expect("node")
            .revision,
        1
    );
}
#[test]
fn prepared_recovery_requires_admitted_graph_without_writes() {
    let (temp, mut store) = initialized_store();
    let prepared = store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect("prepare")
        .expect("activation");
    let mut source = definition("example");
    source.nodes.clear();
    let json = serde_json::to_string(&source).expect("source");
    block_on(
        store.connection.exec_update_count(
            &update("workflow_definitions")
                .value("definition_json", DatabaseValue::from((json).clone()))
                .value(
                    "checksum_sha256",
                    DatabaseValue::from(sha256_hex(json.as_bytes())),
                ),
        ),
    )
    .expect("source fixture");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let before = fixture_changes(&store.connection);
    assert_eq!(
        prepared_read_only_dispatches(&store.connection, 10).expect("admitted recovery"),
        vec![prepared]
    );
    assert_eq!(fixture_changes(&store.connection), before);
    for damage in 0..4 {
        damage_prepared_recovery_graph(&store.connection, damage).expect("damage fixture");
        let before = fixture_changes(&store.connection);
        assert!(prepared_read_only_dispatches_for_run(&store.connection, "run-1", 10).is_err());
        assert!(prepared_read_only_dispatches(&store.connection, 10).is_err());
        assert_eq!(fixture_changes(&store.connection), before);
        restore_prepared_recovery_header(&store.connection).expect("restore revision");
        restore_prepared_recovery_node(&store.connection).expect("restore node fixture");
    }
}
#[test]
fn prepared_recovery_preserves_missing_graph_header_after_reopen() {
    let (temp, mut store) = initialized_store();
    store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect("prepare")
        .expect("activation");
    block_on(async {
        store.connection.sqlite_foreign_keys(false).await?;
        store
            .connection
            .exec_delete_count(&delete("workflow_run_graphs").where_eq("run_id", "run-1"))
            .await?;
        store.connection.sqlite_foreign_keys(true).await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("missing header fixture");
    drop(store);
    let store =
        WorkflowStore::open_in_state_dir(temp.path()).expect("reopen damaged current store");
    let before = fixture_changes(&store.connection);
    assert!(prepared_read_only_dispatches(&store.connection, 10).is_err());
    assert!(prepared_read_only_dispatches_for_run(&store.connection, "run-1", 10).is_err());
    let headers: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_run_graphs")
            .project_as(switchy_database::query::count_all(), "fixture_count")
            .where_eq("run_id", "run-1"),
        "fixture_count",
    )
    .expect("header count");
    assert_eq!(headers, 0);
    let attempt_status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_attempts")
            .columns(&["status"])
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review"),
        "status",
    )
    .expect("preserved attempt");
    let activation_status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review"),
        "status",
    )
    .expect("preserved activation");
    assert_eq!(attempt_status, "prepared");
    assert_eq!(activation_status, "running");
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn failed_node_retry_checks_revised_graph_not_authored_edges() {
    struct Failed;
    impl AttemptStatusObserver for Failed {
        fn observe(
            &self,
            _: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            Ok(AttemptObservation::Failed {
                message: "worker failed".into(),
            })
        }
    }
    let (_temp, mut store) = initialized_store();
    let definition = sequential_definition();
    store
        .persist_definition("sequential", 1, &definition)
        .unwrap();
    let mut run = new_run();
    run.definition_id = "sequential".into();
    run.run_id = "revised-retry".into();
    store.create_run(&run).unwrap();
    let activation = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &activation,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation":"review"}),
            12,
        )
        .unwrap()
        .unwrap();
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: run.run_id.clone(),
            node_id: "first".into(),
            activation_id: activation.clone(),
            attempt: prepared.attempt,
            dispatch_identity: prepared.dispatch_identity,
            receipt: serde_json::json!({"turn_id":"failed-worker"}),
            admitted_at_ms: 13,
        })
        .unwrap();
    store
        .create_activation(&NewActivation {
            run_id: run.run_id.clone(),
            node_id: "second".into(),
            activation_id: activation_identity(&run.run_id, "second", 0),
            dependency_generation: 0,
            input: Some(serde_json::json!(2)),
            created_at_ms: 19,
        })
        .unwrap();
    store
        .reconcile_receipt_backed_attempts(&Failed, 10, 20)
        .unwrap();
    // The current graph includes a dependency absent from the authored snapshot.
    // Recovery must use canonical execution topology, not reinterpret that snapshot.
    let mut authored = definition;
    authored.edges.clear();
    block_on(
        store.connection.exec_update_count(
            &update("workflow_definitions")
                .value(
                    "definition_json",
                    DatabaseValue::from(serde_json::to_string(&authored).unwrap()),
                )
                .where_eq("definition_id", "sequential"),
        ),
    )
    .unwrap();
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graphs")
                .value("revision", 2_i64)
                .where_eq("run_id", DatabaseValue::from((run.run_id).clone())),
        ),
    )
    .unwrap();
    let error = store
        .retry_failed_node(&run.run_id, "first", &activation, 1, 21)
        .unwrap_err();
    assert!(error.to_string().contains("downstream activation"));
    assert_eq!(
        store.run_summary(&run.run_id).unwrap().unwrap().status,
        RunStatus::Failed
    );
    assert_eq!(
        store.attempt_history(&run.run_id, None, 10).unwrap().len(),
        1
    );
    assert_retry_with_historical_target(&store, &run, &activation);
}
#[test]
fn fan_out_failure_requires_run_graph_and_releases_wait_all_capacity() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("fan-out", 1, &fan_out_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "fan-out".to_string();
    run.input = Some(serde_json::json!([3, 1, 2]));
    store.create_run(&run).expect("run");
    store
        .settle_pending_control_nodes("run-1", 10, 20)
        .expect("materialize");
    let member = store
        .pending_activations_for_run("run-1", 1)
        .expect("pending")
        .remove(0);
    store
        .prepare_pending_activation(
            "run-1",
            &member.node_id,
            &member.activation_id,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "test"}),
            30,
        )
        .expect("prepare")
        .expect("admitted");
    let request = AttemptReconciliationRequest {
        run_id: member.run_id,
        node_id: member.node_id,
        activation_id: member.activation_id,
        attempt: 1,
        dispatch_identity: "test".to_string(),
        side_effect: DispatchSideEffect::ReadOnly,
        receipt: serde_json::json!({}),
    };
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    block_on(
        transaction
            .as_ref()
            .exec_delete_count(&delete("workflow_run_graph_nodes").where_eq("run_id", "run-1")),
    )
    .expect("damage graph");
    let before = fixture_changes(transaction.as_ref());
    assert!(
        settle_fan_out_member_failure(transaction.as_ref(), &request, "failure", 40)
            .expect_err("missing graph")
            .to_string()
            .contains("run-graph node is missing")
    );
    assert_eq!(fixture_changes(transaction.as_ref()), before);
    block_on(transaction.rollback()).expect("restore fixture");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    assert!(
        settle_fan_out_member_failure(transaction.as_ref(), &request, "failure", 40)
            .expect("settle failure")
            .is_empty()
    );
    let after_settlement = fixture_changes(transaction.as_ref());
    assert!(
        settle_fan_out_member_failure(transaction.as_ref(), &request, "duplicate failure", 41)
            .expect_err("already settled")
            .to_string()
            .contains("already settled")
    );
    assert_eq!(fixture_changes(transaction.as_ref()), after_settlement);
    database::commit(transaction).expect("commit");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    verify_failed_member_and_finish(store, &request);
}
#[test]
fn automatic_retry_observation_rejects_graph_damage_before_writes() {
    for damage in 0..4 {
        let (_temp, store) = initialized_store();
        let request = AttemptReconciliationRequest {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            dispatch_identity: "test".to_string(),
            side_effect: DispatchSideEffect::ReadOnly,
            receipt: serde_json::json!({}),
        };
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
        block_on(async {
            let fixture_database: &dyn Database = transaction.as_ref();
            match damage {
                0 => {
                    fixture_database
                        .exec_update_count(
                            &update("workflow_activations")
                                .value("node_revision", 2_i64)
                                .where_eq("node_id", "review"),
                        )
                        .await?;
                }
                1 => {
                    fixture_database
                        .exec_delete_count(
                            &delete("workflow_run_graph_nodes").where_eq("node_id", "review"),
                        )
                        .await?;
                }
                2 => {
                    fixture_database
                        .exec_update_count(
                            &update("workflow_run_graph_nodes")
                                .value("node_json", "{}")
                                .where_eq("node_id", "review"),
                        )
                        .await?;
                }
                3 => {
                    fixture_database
                        .exec_update_count(&update("workflow_run_graphs").value("revision", 2_i64))
                        .await?;
                }
                _ => unreachable!("fixture damage case"),
            }
            Ok::<_, switchy_database::DatabaseError>(())
        })
        .expect("damage fixture");
        let before = fixture_changes(transaction.as_ref());
        assert!(
            schedule_retry_for_observation_transaction(
                transaction.as_ref(),
                &request,
                bcode_workflow::AutomaticRetryFailureKind::OwnerReportedRetryable,
                20,
            )
            .is_err()
        );
        assert_eq!(fixture_changes(transaction.as_ref()), before);
    }
}
#[test]
fn graph_migration_preserves_ambiguous_prepared_mutations() {
    let (temp, mut store) = initialized_store();
    let identity = store
        .prepare_attempt(&PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::Mutating,
            intent: serde_json::json!({"operation": "mutate"}),
            prepared_at_ms: 12,
        })
        .expect("prepare");
    let attempts = store.attempt_history("run-1", None, 10).expect("attempts");
    let path = store.path().to_path_buf();
    drop(store);
    let connection = fixture_open(&path, false);
    make_ambiguous_execution_schema_15(&connection).expect("historical fixture");
    let checksum: String = fixture_scalar_text(
        &connection,
        &select("workflow_definitions")
            .columns(&["checksum_sha256"])
            .where_eq("definition_id", "example")
            .where_eq("version", 1_i64),
        "checksum_sha256",
    )
    .expect("checksum");
    block_on(
        connection
            .exec_update_count(&update("workflow_definitions").value("checksum_sha256", "damaged")),
    )
    .expect("damage checksum");
    drop(connection);
    assert!(
        WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 46)
            .expect_err("checksum mismatch")
            .to_string()
            .contains("workflow definition checksum mismatch during graph migration")
    );
    let connection = fixture_open(&path, false);
    assert_eq!(detected_store_schema(&connection), Some(15));
    let status: String = fixture_scalar_text(
        &connection,
        &select("workflow_attempts")
            .columns(&["status"])
            .where_eq("run_id", "run-1"),
        "status",
    )
    .expect("preserved prepared attempt");
    assert_eq!(status, "prepared");
    block_on(connection.exec_update_count(
        &update("workflow_definitions").value("checksum_sha256", DatabaseValue::from(checksum)),
    ))
    .expect("restore fixture checksum");
    drop(connection);
    WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 47).expect("migration");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        store.attempt_history("run-1", None, 10).expect("attempts"),
        attempts
    );
    assert!(
        !store
            .dispatch_receipt_committed(&identity)
            .expect("receipt")
    );
    let summary = store
        .reconcile_prepared_attempts_for_run("run-1", 10, 50)
        .expect("reconcile");
    assert_eq!(summary.repair_required, [identity]);
    assert!(summary.safe_prepared.is_empty());
    let attempts = store.attempt_history("run-1", None, 10).expect("attempts");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].status, "repair_required");
}
#[test]
fn graph_migration_preserves_definition_with_distinct_storage_identity() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let original = definition("display-name");
    store
        .persist_definition("example", 1, &original)
        .expect("distinct storage identity");
    store.create_run(&new_run()).expect("run");
    let mut newer = definition("newer-display-name");
    let mut newer_node = newer.nodes.remove("review").expect("review node");
    newer_node.id = "replacement".to_string();
    newer.nodes.insert(newer_node.id.clone(), newer_node);
    newer.entries = vec!["replacement".to_string()];
    newer.exits = vec!["replacement".to_string()];
    newer.validate().expect("valid newer definition");
    store
        .persist_definition("example", 2, &newer)
        .expect("newer version");
    let mut second_run = new_run();
    second_run.run_id = "run-2".to_string();
    second_run.definition_version = 2;
    store.create_run(&second_run).expect("version 2 run");
    let second_nodes = store
        .run_graph_nodes("run-2", None, 100)
        .expect("version 2 nodes");
    let nodes = store.run_graph_nodes("run-1", None, 100).expect("nodes");
    assert_ne!(nodes, second_nodes);
    let path = store.path().to_path_buf();
    drop(store);
    let connection = fixture_open(&path, false);
    block_on(async {
        connection
            .drop_table("workflow_run_graph_edges")
            .execute(&connection)
            .await?;
        connection
            .drop_table("workflow_run_graph_nodes")
            .execute(&connection)
            .await?;
        connection
            .drop_table("workflow_run_graphs")
            .execute(&connection)
            .await?;
        Ok::<_, WorkflowStoreError>(())
    })
    .expect("historical graph setup");
    fixture_historical_activations(&connection, false).expect("historical activations");
    block_on(async {
        connection
            .exec_update_count(&update("workflow_store_contract").value("schema_version", 15_i64))
            .await?;
        Ok::<_, WorkflowStoreError>(())
    })
    .expect("schema 15");
    drop(connection);
    WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 48).expect("migration");
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        store
            .run_graph_nodes("run-1", None, 100)
            .expect("preserved graph"),
        nodes
    );
    assert_eq!(
        store
            .run_graph_nodes("run-2", None, 100)
            .expect("preserved version 2 graph"),
        second_nodes
    );
    let payload: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_definitions")
            .columns(&["definition_json"])
            .where_eq("definition_id", "example")
            .where_eq("version", 1_i64),
        "definition_json",
    )
    .expect("preserved definition");
    assert_eq!(
        serde_json::from_str::<WorkflowDefinition>(&payload).expect("definition"),
        original
    );
}
#[test]
fn graph_migration_preserves_completed_execution_state() {
    let (temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
    let output = ValidatedOutput {
        output_id: "preserved-output".to_string(),
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        schema_id: "u32".to_string(),
        schema_version: 1,
        value: serde_json::json!(7),
        artifact_reference: None,
        created_at_ms: 20,
    };
    store
        .apply_attempt_observation(&identity, AttemptObservation::Succeeded { output }, 20)
        .expect("complete");
    let output = store
        .canonical_terminal_output("run-1")
        .expect("output")
        .expect("terminal output");
    let attempts = store.attempt_history("run-1", None, 10).expect("attempts");
    let summary = store.run_summary("run-1").expect("summary");
    let path = store.path().to_path_buf();
    drop(store);
    let connection = fixture_open(&path, false);
    make_completed_execution_schema_15(&connection).expect("schema 15");
    let checksum: String = fixture_scalar_text(
        &connection,
        &select("workflow_definitions")
            .columns(&["checksum_sha256"])
            .where_eq("definition_id", "example")
            .where_eq("version", 1_i64),
        "checksum_sha256",
    )
    .expect("checksum");
    block_on(
        connection
            .exec_update_count(&update("workflow_definitions").value("checksum_sha256", "damaged")),
    )
    .expect("damage fixture checksum");
    drop(connection);
    assert!(
        WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 45)
            .expect_err("checksum mismatch")
            .to_string()
            .contains("workflow definition checksum mismatch during graph migration")
    );
    let connection = fixture_open(&path, false);
    assert_eq!(detected_store_schema(&connection), Some(15));
    let status: String = fixture_scalar_text(
        &connection,
        &select("workflow_attempts")
            .columns(&["status"])
            .where_eq("run_id", "run-1"),
        "status",
    )
    .expect("terminal attempt");
    assert_eq!(status, "succeeded");
    block_on(connection.exec_update_count(
        &update("workflow_definitions").value("checksum_sha256", DatabaseValue::from(checksum)),
    ))
    .expect("restore fixture checksum");
    drop(connection);
    WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 46).expect("migration");
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        store.canonical_terminal_output("run-1").expect("output"),
        Some(output)
    );
    assert_eq!(
        store.attempt_history("run-1", None, 10).expect("attempts"),
        attempts
    );
    assert_eq!(store.run_summary("run-1").expect("summary"), summary);
    assert_eq!(
        store.run_graph_revision("run-1").expect("revision"),
        Some(1)
    );
    assert!(
        store
            .pending_activations_for_run("run-1", 10)
            .expect("pending")
            .is_empty()
    );
}
#[test]
fn graph_migration_rejects_missing_definition_without_partial_upgrade() {
    let (temp, store) = initialized_store();
    let path = store.path().to_path_buf();
    drop(store);
    let connection = fixture_open(&path, false);
    block_on(async {
        connection.sqlite_foreign_keys(false).await?;
        connection
            .drop_table("workflow_run_graph_edges")
            .execute(&connection)
            .await?;
        connection
            .drop_table("workflow_run_graph_nodes")
            .execute(&connection)
            .await?;
        connection
            .drop_table("workflow_run_graphs")
            .execute(&connection)
            .await?;
        Ok::<_, WorkflowStoreError>(())
    })
    .expect("historical graph setup");
    fixture_historical_activations(&connection, false).expect("historical activations");
    block_on(async {
        connection
            .exec_update_count(&update("workflow_store_contract").value("schema_version", 15_i64))
            .await?;
        connection
            .exec_delete_count(&delete("workflow_definitions"))
            .await?;
        Ok::<_, WorkflowStoreError>(())
    })
    .expect("historical damaged fixture");
    drop(connection);
    assert!(WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 44).is_err());
    let connection = fixture_open(&path, false);
    assert_eq!(detected_store_schema(&connection), Some(15));
    assert_eq!(
        fixture_scalar_unsigned(
            &connection,
            &select("workflow_runs")
                .project_as(switchy_database::query::count_all(), "fixture_count"),
            "fixture_count"
        )
        .expect("runs"),
        1
    );
    assert!(
        block_on(connection.validate_select(&select("workflow_run_graphs").columns(&["run_id"])))
            .is_err()
    );
}
#[test]
fn graph_migration_rolls_back_previously_materialized_runs() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    store
        .persist_definition("second", 1, &definition("second"))
        .expect("second definition");
    store.create_run(&new_run()).expect("first run");
    let mut second = new_run();
    second.run_id = "run-2".to_string();
    second.definition_id = "second".to_string();
    store.create_run(&second).expect("second run");
    let path = store.path().to_path_buf();
    drop(store);
    let connection = fixture_open(&path, false);
    make_two_run_schema_15_fixture(&connection).expect("historical fixture");
    drop(connection);
    assert!(
        WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 44)
            .expect_err("second definition damaged")
            .to_string()
            .contains("checksum mismatch")
    );
    let connection = fixture_open(&path, false);
    assert_eq!(detected_store_schema(&connection), Some(15));
    assert_eq!(
        fixture_scalar_unsigned(
            &connection,
            &select("workflow_runs")
                .project_as(switchy_database::query::count_all(), "fixture_count"),
            "fixture_count"
        )
        .expect("runs"),
        2
    );
    assert!(
        block_on(connection.validate_select(&select("workflow_run_graphs").columns(&["run_id"])))
            .is_err()
    );
    // Repair only the deliberately corrupted fixture, then retry explicit migration.
    block_on(
        connection.exec_update_count(
            &update("workflow_definitions")
                .value(
                    "checksum_sha256",
                    DatabaseValue::from(sha256_hex(
                        serde_json::to_string(&definition("second"))
                            .expect("payload")
                            .as_bytes(),
                    )),
                )
                .where_eq("definition_id", "second"),
        ),
    )
    .expect("restore fixture checksum");
    drop(connection);
    WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 45).expect("retry migration");
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    for run_id in ["run-1", "run-2"] {
        assert_eq!(store.run_graph_revision(run_id).expect("revision"), Some(1));
        assert_eq!(
            store
                .run_graph_nodes(run_id, None, 100)
                .expect("nodes")
                .len(),
            1
        );
    }
}
#[test]
fn graph_migration_preserves_unsupported_definition_versions() {
    let mut invalid = definition("example");
    invalid.schema_version = bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION + 1;
    verify_graph_migration_preserves_invalid_definition(&invalid);
}
#[test]
fn graph_migration_preserves_malformed_current_definitions() {
    let mut invalid = definition("example");
    invalid.entries.clear();
    assert!(invalid.validate().is_err());
    verify_graph_migration_preserves_invalid_definition(&invalid);
}
#[test]
fn graph_migration_preserves_checksum_mismatched_definitions() {
    let mut changed = definition("example");
    changed.name = "modified".to_string();
    changed.validate().expect("valid changed definition");
    verify_graph_migration_rejects_definition(&changed, true);
}
#[test]
fn graph_migration_preserves_oversized_definitions() {
    let mut oversized = definition("example");
    oversized.name = "x".repeat(MAX_INLINE_JSON_BYTES + 1);
    verify_graph_migration_rejects_definition(&oversized, false);
}
#[test]
fn graph_migration_preserves_malformed_checksums() {
    let plan = definition("example");
    let payload = serde_json::to_string(&plan).expect("definition payload");
    let uppercase = sha256_hex(payload.as_bytes()).to_ascii_uppercase();
    for checksum in ["", "abc", &"g".repeat(64), &uppercase] {
        verify_graph_migration_rejects_definition_checksum(&plan, true, Some(checksum));
    }
}
#[test]
fn graph_migration_preserves_oversized_checksums() {
    verify_graph_migration_rejects_definition_checksum(
        &definition("example"),
        true,
        Some(&"x".repeat(MAX_INLINE_JSON_BYTES + 1)),
    );
}
#[test]
fn quiescent_graph_publication_is_atomic_and_idempotent() {
    let (temp, mut store) = initialized_store();
    block_on(
        store.connection.exec_update_count(
            &update("workflow_runs")
                .value("target_artifact_id", "artifact-a")
                .value("coordinator_daemon_instance_id", "daemon-a")
                .value("coordinator_generation", 1_i64)
                .value("coordinator_fencing_token", "token-a")
                .where_eq("run_id", "run-1"),
        ),
    )
    .expect("owner");
    let authority = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owner");
    let node = store
        .current_run_graph_node("run-1", "review")
        .expect("node")
        .expect("review");
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "run-1".to_string(),
        mutation_id: "publish-leaf".to_string(),
        expected_revision: 1,
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::ReplaceNode {
            node: node.node,
            entry: true,
            exit: true,
        }],
        reconciliation: vec![],
    };
    store
        .stage_run_graph_edit(&request, &authority, 20)
        .expect("stage");
    assert!(
        store
            .publish_quiescent_run_graph_edit("run-1", "publish-leaf", &authority, 21)
            .is_err()
    );
    assert_eq!(
        store.run_graph_revision("run-1").expect("revision"),
        Some(1)
    );
    block_on(
        store.connection.exec_update_count(
            &update("workflow_activations")
                .value("status", "cancelled")
                .where_eq("run_id", "run-1"),
        ),
    )
    .expect("quiescent fixture");
    let mut stale = authority.clone();
    stale.generation += 1;
    assert!(
        store
            .publish_quiescent_run_graph_edit("run-1", "publish-leaf", &stale, 22)
            .is_err()
    );
    assert_eq!(
        store
            .publish_quiescent_run_graph_edit("run-1", "publish-leaf", &authority, 23)
            .expect("publish"),
        2
    );
    assert_eq!(
        store
            .current_run_graph_node("run-1", "review")
            .expect("node")
            .expect("review")
            .revision,
        2
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let before = fixture_changes(&store.connection);
    assert_eq!(
        store
            .publish_quiescent_run_graph_edit("run-1", "publish-leaf", &authority, 24)
            .expect("duplicate"),
        2
    );
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn graph_edit_candidate_survives_upgrade_and_duplicate_delivery() {
    let (temp, store) = initialized_store();
    remove_retirement_fixture(&store);
    block_on(async {
        store
            .connection
            .drop_table("workflow_graph_edit_candidates")
            .execute(&store.connection)
            .await?;
        store
            .connection
            .exec_update_count(&update("workflow_store_contract").value("schema_version", 18_i64))
            .await?;
        store
            .connection
            .exec_update_count(
                &update("workflow_runs")
                    .value("target_artifact_id", "artifact-a")
                    .value("coordinator_daemon_instance_id", "daemon-a")
                    .value("coordinator_generation", 1_i64)
                    .value("coordinator_fencing_token", "token-a")
                    .where_eq("run_id", "run-1"),
            )
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("schema eighteen fixture");
    drop(store);
    let mut store =
        WorkflowStore::initialize_in_state_dir(temp.path(), 20).expect("preservation upgrade");
    let authority = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owner");
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "run-1".to_string(),
        expected_revision: 1,
        mutation_id: "mutation-1".to_string(),
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::RemoveNode {
            node_id: "review".to_string(),
        }],
        reconciliation: vec![bcode_workflow::WorkflowRunGraphReconciliation::Retain {
            activation_id: activation_id(),
        }],
    };
    let original = store
        .activation_graph_node("run-1", "review", &activation_id())
        .expect("binding");
    let mut stale = authority.clone();
    stale.generation += 1;
    assert!(store.stage_run_graph_edit(&request, &stale, 21).is_err());
    assert!(
        store
            .stage_run_graph_edit(&request, &authority, 21)
            .expect("stage")
    );
    assert_eq!(
        store.run_graph_revision("run-1").expect("revision"),
        Some(1)
    );
    assert_eq!(
        store
            .activation_graph_node("run-1", "review", &activation_id())
            .expect("binding unchanged"),
        original
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert!(
        !store
            .stage_run_graph_edit(&request, &authority, 22)
            .expect("duplicate")
    );
    let mut conflict = request.clone();
    conflict.expected_revision = 2;
    assert!(
        store
            .stage_run_graph_edit(&conflict, &authority, 23)
            .is_err()
    );
    conflict.mutation_id = "mutation-2".to_string();
    assert!(
        store
            .stage_run_graph_edit(&conflict, &authority, 23)
            .is_err()
    );
    let count: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_graph_edit_candidates")
            .project_as(switchy_database::query::count_all(), "fixture_count"),
        "fixture_count",
    )
    .expect("candidate count");
    assert_eq!(count, 1);
    verify_candidate_read_and_damage(&mut store, &request, &authority);
}
#[test]
fn admission_records_graph_revision_independently_of_unchanged_node() {
    let (temp, mut store) = initialized_store();
    block_on(async {
        store
            .connection
            .drop_table("workflow_activation_graph_bindings")
            .execute(&store.connection)
            .await?;
        store
            .connection
            .exec_update_count(&update("workflow_store_contract").value("schema_version", 21_i64))
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("schema 21 fixture");
    drop(store);
    store = WorkflowStore::initialize_in_state_dir(temp.path(), 901).expect("upgrade");
    assert!(
        store
            .activation_admitted_graph_revision("run-1", "review", &activation_id())
            .expect("unknown historical admission")
            .is_none()
    );
    block_on(async {
        store
            .connection
            .exec_update_count(
                &update("workflow_runs")
                    .value("target_artifact_id", "artifact-a")
                    .value("coordinator_daemon_instance_id", "daemon-a")
                    .value("coordinator_generation", 1_i64)
                    .value("coordinator_fencing_token", "token-a")
                    .where_eq("run_id", "run-1"),
            )
            .await?;
        store
            .connection
            .exec_update_count(
                &update("workflow_run_graphs")
                    .value("revision", 2_i64)
                    .where_eq("run_id", "run-1"),
            )
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("graph with unchanged executable");
    let authority = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owner");
    let activation = NewActivation {
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_identity("run-1", "review", 1),
        dependency_generation: 1,
        input: Some(serde_json::json!(42)),
        created_at_ms: 20,
    };
    store
        .create_activation_at_graph_revision(&activation, 2, &authority)
        .expect("admit");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        store
            .activation_admitted_graph_revision("run-1", "review", &activation.activation_id)
            .expect("admitted graph"),
        Some(2)
    );
    assert_eq!(
        store
            .activation_graph_node("run-1", "review", &activation.activation_id)
            .expect("node")
            .expect("binding")
            .revision,
        1
    );
    block_on(store.connection.exec_update_count(
        &update("workflow_activation_graph_bindings").value("graph_revision", 3_i64),
    ))
    .expect("future binding");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .activation_admitted_graph_revision("run-1", "review", &activation.activation_id)
            .is_err()
    );
    assert_eq!(before, fixture_changes(&store.connection));
}
#[test]
fn current_node_pages_skip_history_and_reject_changed_revision() {
    let (_temp, store) = initialized_store();
    let mut node = store
        .current_run_graph_node("run-1", "review")
        .expect("read")
        .expect("node")
        .node;
    node.id = "second".to_string();
    block_on(
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", "second")
            .value("revision", 1_i64)
            .value(
                "node_json",
                DatabaseValue::from(serde_json::to_string(&node).expect("json")),
            )
            .value("is_entry", 0_i64)
            .value("is_exit", 0_i64)
            .execute(&store.connection),
    )
    .expect("second node");
    block_on(async {
        let fixture_source = select("workflow_run_graph_nodes")
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .execute(&store.connection)
            .await?;
        let fixture_inserted = fixture_source.len();
        for fixture_row in fixture_source {
            insert("workflow_run_graph_nodes")
                .value("run_id", fixture_row.get("run_id").expect("source column"))
                .value(
                    "node_id",
                    fixture_row.get("node_id").expect("source column"),
                )
                .value("revision", 2_i64)
                .value(
                    "node_json",
                    fixture_row.get("node_json").expect("source column"),
                )
                .value(
                    "is_entry",
                    fixture_row.get("is_entry").expect("source column"),
                )
                .value(
                    "is_exit",
                    fixture_row.get("is_exit").expect("source column"),
                )
                .execute(&store.connection)
                .await?;
        }
        store
            .connection
            .exec_update_count(
                &update("workflow_run_graphs")
                    .value("revision", 2_i64)
                    .where_eq("run_id", "run-1"),
            )
            .await?;
        Ok::<_, switchy_database::DatabaseError>(fixture_inserted)
    })
    .expect("revision fixture");
    let before = fixture_changes(&store.connection);
    let first = store
        .current_run_graph_nodes("run-1", 2, None, 1)
        .expect("first");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].revision, 2);
    let second = store
        .current_run_graph_nodes("run-1", 2, Some(&first[0].node.id), 1)
        .expect("second");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].node.id, "second");
    assert_eq!(second[0].revision, 1);
    assert_eq!(fixture_changes(&store.connection), before);
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graphs")
                .value("revision", 3_i64)
                .where_eq("run_id", "run-1"),
        ),
    )
    .expect("changed graph");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .current_run_graph_nodes("run-1", 2, Some(&first[0].node.id), 1)
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn combined_graph_page_failure_releases_snapshot_without_writes() {
    let (_temp, store) = initialized_store();
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0_i64)
            .value("revision", 1_i64)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value("edge_json", "{}")
            .execute(&store.connection),
    )
    .expect("damaged edge");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .current_run_graph_page("run-1", Some(1), None, None, 1)
            .is_err()
    );
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
    assert_eq!(fixture_changes(&store.connection), before);
    let page = store
        .current_run_graph_page("run-1", Some(1), None, Some(0), 1)
        .expect("page beyond damage");
    assert_eq!(page.nodes.len(), 1);
    assert!(page.edges.is_empty());
    assert!(page.nodes_complete && page.edges_complete);
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn edge_pages_seek_past_revision_history() {
    let (_temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::default(),
        transform: None,
    };
    let json = serde_json::to_string(&edge).expect("json");
    for revision in 1..=200 {
        block_on(
            insert("workflow_run_graph_edges")
                .value("run_id", "run-1")
                .value("edge_id", 0_i64)
                .value("revision", DatabaseValue::from(revision))
                .value("source_node_id", "review")
                .value("target_node_id", "review")
                .value("edge_json", DatabaseValue::from((json).clone()))
                .execute(&store.connection),
        )
        .expect("history");
    }
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 1_i64)
            .value("revision", 1_i64)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value("edge_json", DatabaseValue::from(json))
            .execute(&store.connection),
    )
    .expect("next edge");
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graphs")
                .value("revision", 200_i64)
                .where_eq("run_id", "run-1"),
        ),
    )
    .expect("revision");
    let first = store
        .current_run_graph_edges("run-1", 200, None, 1)
        .expect("first");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].edge_id, 0);
    let next = store
        .current_run_graph_edges("run-1", 200, Some(0), 1)
        .expect("next");
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].edge_id, 1);
    assert!(
        store
            .current_run_graph_edges("run-1", 200, Some(1), 1)
            .expect("end")
            .is_empty()
    );
}
#[test]
fn initial_run_graph_is_atomic_paged_and_preserved_by_migration() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let plan = definition("example");
    store
        .persist_definition("example", 1, &plan)
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    assert_eq!(
        store.run_graph_revision("run-1").expect("revision"),
        Some(1)
    );
    let mut after = None;
    let mut actual = Vec::new();
    loop {
        let page = store
            .run_graph_nodes("run-1", after.as_deref(), 1)
            .expect("page");
        let Some(node) = page.first() else { break };
        assert_eq!(node.revision, 1);
        assert_eq!(node.entry, plan.entries.contains(&node.node.id));
        assert_eq!(node.exit, plan.exits.contains(&node.node.id));
        after = Some(node.node.id.clone());
        actual.push(node.node.clone());
    }
    assert_eq!(actual, plan.nodes.values().cloned().collect::<Vec<_>>());
    let initial_edges = store.run_graph_edges("run-1", None, 100).expect("edges");
    assert_eq!(
        initial_edges
            .iter()
            .map(|row| row.edge.clone())
            .collect::<Vec<_>>(),
        plan.edges
    );
    for pair in initial_edges.windows(2) {
        let page = store
            .run_graph_edges("run-1", Some(pair[0].edge_id), 1)
            .expect("edge page");
        assert_eq!(page, vec![pair[1].clone()]);
    }
    let before = store.run_summary("run-1").expect("summary");
    let path = store.path().to_path_buf();
    drop(store);
    let connection = fixture_open(&path, false);
    make_initial_graph_schema_15(&connection).expect("schema 15");
    drop(connection);
    assert!(WorkflowStore::open_in_state_dir(temp.path()).is_err());
    let receipt =
        WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 42).expect("migration");
    assert_eq!(receipt.previous_schema_version, 15);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(store.run_summary("run-1").expect("summary"), before);
    assert_eq!(
        store
            .run_graph_nodes("run-1", None, 100)
            .expect("nodes")
            .into_iter()
            .map(|node| node.node)
            .collect::<Vec<_>>(),
        actual
    );
    assert_eq!(store.run_graph_revision("missing").expect("missing"), None);
    block_on(
        store
            .connection
            .exec_delete_count(&delete("workflow_run_graph_nodes").where_eq("run_id", "run-1")),
    )
    .expect("damage nodes");
    block_on(
        store
            .connection
            .exec_delete_count(&delete("workflow_run_graph_edges").where_eq("run_id", "run-1")),
    )
    .expect("damage edges");
    block_on(
        store
            .connection
            .exec_delete_count(&delete("workflow_run_graphs").where_eq("run_id", "run-1")),
    )
    .expect("damage graph");
    assert!(store.run_graph_revision("run-1").is_err());
}
#[test]
fn graph_node_lookup_rejects_invalid_boundary_flags_without_repair() {
    let (_temp, store) = initialized_store();
    block_on(async {
        store
            .connection
            .sqlite_ignore_check_constraints(true)
            .await?;
        store
            .connection
            .exec_update_count(
                &update("workflow_run_graph_nodes")
                    .value("is_entry", 2_i64)
                    .where_eq("run_id", "run-1"),
            )
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("damage boundary");
    let before = fixture_changes(&store.connection);
    assert!(store.run_graph_node("run-1", "review").is_err());
    assert!(store.run_graph_nodes("run-1", None, 10).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn node_pages_reject_unusable_stored_cursor_ids_without_repair() {
    let (_temp, store) = initialized_store();
    let template = store
        .run_graph_nodes("run-1", None, 1)
        .expect("node")
        .remove(0)
        .node;
    for id in [String::new(), " ".to_string(), "x".repeat(MAX_ID_BYTES + 1)] {
        let mut node = template.clone();
        node.id.clone_from(&id);
        block_on(
            store.connection.exec_update_count(
                &update("workflow_run_graph_nodes")
                    .value("node_id", DatabaseValue::from((id).clone()))
                    .value(
                        "node_json",
                        DatabaseValue::from(
                            (serde_json::to_string(&node).expect("payload")).clone(),
                        ),
                    )
                    .where_eq("run_id", "run-1"),
            ),
        )
        .expect("damage stored identity");
        let before = fixture_changes(&store.connection);
        assert!(
            store
                .run_graph_nodes("run-1", None, 10)
                .expect_err("unusable cursor")
                .to_string()
                .contains("node_id must contain")
        );
        assert_eq!(fixture_changes(&store.connection), before);
    }
}
#[test]
fn graph_materialization_rejects_unreadable_node_ids_before_writes() {
    let (_temp, store) = initialized_store();
    for id in [String::new(), " ".to_string(), "x".repeat(MAX_ID_BYTES + 1)] {
        let mut plan = definition("example");
        let mut node = plan.nodes.values().next().expect("node").clone();
        node.id.clone_from(&id);
        plan.nodes.clear();
        plan.nodes.insert(id.clone(), node);
        plan.entries = vec![id.clone()];
        plan.exits = vec![id];
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
        let before = fixture_changes(transaction.as_ref());
        assert!(
            run_graph::materialize(transaction.as_ref(), "run-1", &plan)
                .expect_err("invalid id")
                .to_string()
                .contains("node_id must contain")
        );
        assert_eq!(fixture_changes(transaction.as_ref()), before);
    }
    assert_eq!(
        store
            .run_graph_nodes("run-1", None, 10)
            .expect("preserved graph")
            .len(),
        1
    );
}
#[test]
fn graph_materialization_rejects_unsupported_versions_before_writes() {
    let (_temp, store) = initialized_store();
    for version in [0, bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION + 1] {
        let mut plan = definition("example");
        plan.schema_version = version;
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
        let before = fixture_changes(transaction.as_ref());
        assert!(
            run_graph::materialize(transaction.as_ref(), "run-1", &plan)
                .expect_err("unsupported version")
                .to_string()
                .contains("unsupported workflow definition schema version")
        );
        assert_eq!(fixture_changes(transaction.as_ref()), before);
        block_on(transaction.rollback()).expect("rollback");
    }
    assert_eq!(
        store
            .run_graph_revision("run-1")
            .expect("preserved revision"),
        Some(1)
    );
}
#[test]
fn graph_materialization_rejects_mismatched_node_identity_before_writes() {
    let (_temp, store) = initialized_store();
    let mut plan = definition("example");
    plan.nodes.values_mut().next().expect("node").id = "different-node".to_string();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let before = fixture_changes(transaction.as_ref());
    assert!(
        run_graph::materialize(transaction.as_ref(), "run-1", &plan)
            .expect_err("mismatched identity")
            .to_string()
            .contains("node identity mismatch")
    );
    assert_eq!(fixture_changes(transaction.as_ref()), before);
    block_on(transaction.rollback()).expect("rollback");
}
#[test]
fn graph_materialization_rejects_missing_boundaries_before_writes() {
    let (_temp, store) = initialized_store();
    for missing_entry in [true, false] {
        let mut plan = definition("example");
        if missing_entry {
            plan.entries.push("missing".to_string());
        } else {
            plan.exits.push("missing".to_string());
        }
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
        let before = fixture_changes(transaction.as_ref());
        assert!(
            run_graph::materialize(transaction.as_ref(), "run-1", &plan)
                .expect_err("missing boundary")
                .to_string()
                .contains("boundary references a missing node")
        );
        assert_eq!(fixture_changes(transaction.as_ref()), before);
        block_on(transaction.rollback()).expect("rollback");
    }
}
#[test]
fn graph_materialization_rejects_dangling_edges_atomically() {
    let (_temp, store) = initialized_store();
    for missing_source in [true, false] {
        let mut plan = definition("broken");
        let existing = plan.nodes.keys().next().expect("node").clone();
        let (from, to) = if missing_source {
            ("missing".to_string(), existing)
        } else {
            (existing, "missing".to_string())
        };
        plan.edges.push(bcode_workflow::EdgeDefinition {
            from,
            to,
            kind: bcode_workflow::EdgeKind::Direct,
            transform: None,
        });
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
        let before = fixture_changes(transaction.as_ref());
        assert!(
            run_graph::materialize(transaction.as_ref(), "run-1", &plan)
                .expect_err("dangling edge")
                .to_string()
                .contains("edge references a missing node")
        );
        assert_eq!(fixture_changes(transaction.as_ref()), before);
        block_on(transaction.rollback()).expect("rollback");
    }
}
#[test]
fn graph_inspection_rejects_mismatched_payload_identity_without_mutation() {
    let (temp, store) = initialized_store();
    let record = store
        .run_graph_nodes("run-1", None, 1)
        .expect("initial node")
        .remove(0);
    let node_id = record.node.id.clone();
    let mut node = record.node;
    node.id = "different-node".to_string();
    let payload = serde_json::to_string(&node).expect("encode node");
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graph_nodes")
                .value("node_json", DatabaseValue::from((payload).clone()))
                .where_eq("run_id", "run-1")
                .where_eq("node_id", DatabaseValue::from((node_id).clone())),
        ),
    )
    .expect("corrupt identity fixture");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen damaged graph");
    let before = fixture_changes(&store.connection);
    let paged = store
        .run_graph_nodes("run-1", None, 1)
        .expect_err("paged mismatch");
    let exact = store
        .run_graph_node_record("run-1", &node_id)
        .expect_err("exact mismatch");
    for error in [paged, exact] {
        assert!(error.to_string().contains("node identity mismatch"));
    }
    assert_eq!(fixture_changes(&store.connection), before);
    let preserved: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_run_graph_nodes")
            .columns(&["node_json"])
            .where_eq("run_id", "run-1")
            .where_eq("node_id", DatabaseValue::from(node_id)),
        "node_json",
    )
    .expect("preserved payload");
    assert_eq!(preserved, payload);
}
#[test]
fn graph_inspection_rejects_oversized_payloads_and_invalid_flags_without_mutation() {
    let (_temp, store) = initialized_store();
    let original: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_run_graph_nodes")
            .columns(&["node_json"])
            .where_eq("run_id", "run-1")
            .limit(1),
        "node_json",
    )
    .expect("node");
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graph_nodes")
                .value(
                    "node_json",
                    DatabaseValue::from("x".repeat(MAX_INLINE_JSON_BYTES + 1)),
                )
                .where_eq("run_id", "run-1"),
        ),
    )
    .expect("oversized fixture");
    let before = fixture_changes(&store.connection);
    let error = store
        .run_graph_nodes("run-1", None, 1)
        .expect_err("oversized");
    assert!(
        error
            .to_string()
            .contains("payload is invalid or oversized")
    );
    assert_eq!(fixture_changes(&store.connection), before);
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graph_nodes")
                .value("node_json", DatabaseValue::from(original))
                .where_eq("run_id", "run-1"),
        ),
    )
    .expect("restore payload");
    block_on(async {
        store
            .connection
            .sqlite_ignore_check_constraints(true)
            .await?;
        store
            .connection
            .exec_update_count(
                &update("workflow_run_graph_nodes")
                    .value("is_entry", 2_i64)
                    .where_eq("run_id", "run-1"),
            )
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("invalid flag");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .run_graph_nodes("run-1", None, 1)
            .expect_err("invalid flag")
            .to_string()
            .contains("boundary flags")
    );
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn graph_materialization_preserves_multiple_boundary_memberships() {
    let (_temp, store) = initialized_store();
    let mut plan = definition("example");
    let template = plan.nodes.values().next().expect("template").clone();
    plan.nodes.clear();
    plan.entries.clear();
    plan.exits.clear();
    for index in 0..40 {
        let mut node = template.clone();
        node.id = format!("boundary-{index:02}");
        if index % 2 == 0 {
            plan.entries.push(node.id.clone());
        }
        if index % 3 == 0 {
            plan.exits.push(node.id.clone());
        }
        plan.nodes.insert(node.id.clone(), node);
    }
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    block_on(async {
        transaction
            .as_ref()
            .exec_delete_count(&delete("workflow_run_graph_edges").where_eq("run_id", "run-1"))
            .await?;
        transaction
            .as_ref()
            .exec_delete_count(&delete("workflow_run_graph_nodes").where_eq("run_id", "run-1"))
            .await?;
        transaction
            .as_ref()
            .exec_delete_count(&delete("workflow_run_graphs").where_eq("run_id", "run-1"))
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("replace fixture graph");
    run_graph::materialize(transaction.as_ref(), "run-1", &plan).expect("materialize");
    database::commit(transaction).expect("commit");
    let nodes = store.run_graph_nodes("run-1", None, 100).expect("nodes");
    assert_eq!(nodes.len(), 40);
    for node in nodes {
        assert_eq!(node.entry, plan.entries.contains(&node.node.id));
        assert_eq!(node.exit, plan.exits.contains(&node.node.id));
        assert_eq!(node.node, plan.nodes[&node.node.id]);
    }
}
#[test]
fn initial_node_pages_resume_without_duplicates_or_mutation() {
    let (_temp, store) = initialized_store();
    let template = store
        .run_graph_nodes("run-1", None, 1)
        .expect("template")
        .remove(0)
        .node;
    for index in 0..205 {
        let mut node = template.clone();
        node.id = format!("page-{index:03}");
        block_on(
            insert("workflow_run_graph_nodes")
                .value("run_id", "run-1")
                .value("node_id", DatabaseValue::from((node.id).clone()))
                .value("revision", 1_i64)
                .value(
                    "node_json",
                    DatabaseValue::from((serde_json::to_string(&node).expect("serialize")).clone()),
                )
                .value("is_entry", 0_i64)
                .value("is_exit", 0_i64)
                .execute(&store.connection),
        )
        .expect("insert node");
    }
    let before = fixture_changes(&store.connection);
    let mut cursor = None;
    let mut ids = Vec::new();
    loop {
        let page = store
            .run_graph_nodes("run-1", cursor.as_deref(), usize::MAX)
            .expect("page");
        assert!(page.len() <= 100);
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(|item| item.node.id.clone());
        ids.extend(page.into_iter().map(|item| item.node.id));
    }
    assert_eq!(ids.len(), 206);
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    for index in 0..205 {
        assert!(ids.binary_search(&format!("page-{index:03}")).is_ok());
    }
    assert!(store.run_graph_nodes("missing", Some(""), 10).is_err());
    assert!(
        store
            .run_graph_nodes("missing", None, 10)
            .expect("missing")
            .is_empty()
    );
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn schema_16_index_migration_rolls_back_on_orphaned_graph() {
    let (temp, store) = initialized_store();
    let original_nodes = store
        .run_graph_nodes("run-1", None, 100)
        .expect("original nodes");
    make_orphaned_schema_16_graph(&store.connection).expect("orphan graph fixture");
    remove_retirement_fixture(&store);
    drop(store);
    let error = WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 78)
        .expect_err("orphaned graph must reject migration");
    assert!(matches!(error, WorkflowStoreError::InvalidData(message)
            if message == "workflow migration foreign-key verification failed; canonical relationships are inconsistent"));
    let database_path = workflow_database_path(temp.path());
    let backup_path = schema_16_failed_backup_path(&database_path);
    let backup = database::open(
        &backup_path,
        switchy_database_connection::SqliteAccess::ReadOnly,
        std::time::Duration::from_secs(5),
        false,
    )
    .expect("read-only backup");
    assert_eq!(detected_store_schema(&backup), Some(16));
    let backup_nodes: u64 = fixture_scalar_unsigned(
        &backup,
        &select("workflow_run_graph_nodes")
            .project_as(switchy_database::query::count_all(), "fixture_count")
            .where_eq("run_id", "run-1"),
        "fixture_count",
    )
    .expect("backed up orphan");
    assert_eq!(backup_nodes, 1);
    let backup_headers: u64 = fixture_scalar_unsigned(
        &backup,
        &select("workflow_run_graphs")
            .project_as(switchy_database::query::count_all(), "fixture_count")
            .where_eq("run_id", "run-1"),
        "fixture_count",
    )
    .expect("backup preserves absent header");
    assert_eq!(backup_headers, 0);
    drop(backup);
    let failed_backup_bytes = std::fs::read(&backup_path).expect("retain backup bytes");
    let database_path = workflow_database_path(temp.path());
    let connection = fixture_open(&database_path, false);
    assert_eq!(detected_store_schema(&connection), Some(16));
    let indexes: u64 = fixture_scalar_unsigned(
        &connection,
        &select("sqlite_master")
            .project_as(switchy_database::query::count_all(), "fixture_count")
            .where_eq("name", "workflow_run_graph_edges_source"),
        "fixture_count",
    )
    .expect("index rollback");
    assert_eq!(indexes, 0);
    let nodes: u64 = fixture_scalar_unsigned(
        &connection,
        &select("workflow_run_graph_nodes")
            .project_as(switchy_database::query::count_all(), "fixture_count")
            .where_eq("run_id", "run-1"),
        "fixture_count",
    )
    .expect("orphan preserved");
    assert_eq!(nodes, 1);
    let headers: u64 = fixture_scalar_unsigned(
        &connection,
        &select("workflow_run_graphs")
            .project_as(switchy_database::query::count_all(), "fixture_count")
            .where_eq("run_id", "run-1"),
        "fixture_count",
    )
    .expect("no reconstruction");
    assert_eq!(headers, 0);
    block_on(
        insert("workflow_run_graphs")
            .value("run_id", "run-1")
            .value("revision", 1_i64)
            .execute(&connection),
    )
    .expect("explicit fixture restoration");
    drop(connection);
    let receipt = WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 79)
        .expect("retry after restoration");
    assert_eq!(
        std::fs::read(&backup_path).expect("failed backup after retry"),
        failed_backup_bytes
    );
    assert_ne!(receipt.backup_path, backup_path);
    assert_eq!(receipt.previous_schema_version, 16);
    assert_eq!(receipt.new_schema_version, WORKFLOW_STORE_SCHEMA_VERSION);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen migrated store");
    let nodes = reopened.run_graph_nodes("run-1", None, 100).expect("nodes");
    assert_eq!(nodes, original_nodes);
    assert!(
        reopened
            .run_graph_outgoing_edges("run-1", &original_nodes[0].node.id, None, 1)
            .expect("source index available")
            .is_empty()
    );
}
#[test]
fn graph_reads_reject_invalid_header_revisions_without_mutation() {
    let (_temp, store) = initialized_store();
    block_on(store.connection.sqlite_ignore_check_constraints(true))
        .expect("allow damaged fixture");
    for revision in [
        DatabaseValue::Int64(0),
        DatabaseValue::Int64(-1),
        DatabaseValue::Real64(1.5),
        DatabaseValue::String("future".to_string()),
    ] {
        block_on(
            store.connection.exec_update_count(
                &update("workflow_run_graphs")
                    .value("revision", revision.clone())
                    .where_eq("run_id", "run-1"),
            ),
        )
        .expect("damage revision");
        let before = fixture_changes(&store.connection);
        assert!(store.run_graph_revision("run-1").is_err());
        assert!(store.run_graph_nodes("run-1", None, 1).is_err());
        assert!(store.run_graph_edges("run-1", None, 1).is_err());
        assert_eq!(fixture_changes(&store.connection), before);
        let preserved: DatabaseValue = block_on(async {
            let fixture_row = select("workflow_run_graphs")
                .columns(&["revision"])
                .where_eq("run_id", "run-1")
                .execute_first(&store.connection)
                .await?
                .ok_or_else(|| WorkflowStoreError::InvalidData("missing fixture row".into()))?;
            Ok::<_, WorkflowStoreError>(fixture_row.get("revision").expect("column"))
        })
        .expect("preserved revision");
        assert_eq!(preserved, revision);
    }
}
#[test]
fn exact_graph_node_record_rejects_missing_or_future_graph_without_mutation() {
    for missing in [true, false] {
        let (_temp, store) = initialized_store();
        let node = store
            .run_graph_nodes("run-1", None, 1)
            .expect("node")
            .remove(0);
        block_on(async {
            if missing {
                store.connection.sqlite_foreign_keys(false).await?;
                store
                    .connection
                    .exec_delete_count(&delete("workflow_run_graphs").where_eq("run_id", "run-1"))
                    .await?;
            } else {
                store
                    .connection
                    .exec_update_count(
                        &update("workflow_run_graphs")
                            .value("revision", 2_i64)
                            .where_eq("run_id", "run-1"),
                    )
                    .await?;
            }
            Ok::<_, switchy_database::DatabaseError>(())
        })
        .expect("damage graph header");
        let changes = fixture_changes(&store.connection);
        assert!(store.run_graph_node_record("run-1", &node.node.id).is_err());
        assert!(store.run_graph_nodes("run-1", None, 1).is_err());
        assert!(store.run_graph_edges("run-1", None, 1).is_err());
        assert!(store.run_graph_edge("run-1", 0).is_err());
        assert!(
            store
                .run_graph_incoming_edges("run-1", &node.node.id, None, 1)
                .is_err()
        );
        assert!(
            store
                .run_graph_outgoing_edges("run-1", &node.node.id, None, 1)
                .is_err()
        );
        assert_eq!(fixture_changes(&store.connection), changes);
        let revision: Option<u64> = block_on(async {
            select("workflow_run_graphs")
                .columns(&["revision"])
                .where_eq("run_id", "run-1")
                .execute_first(&store.connection)
                .await?
                .map(|row| database::unsigned(&row, "revision"))
                .transpose()
        })
        .expect("preserved header");
        assert_eq!(revision, if missing { None } else { Some(2) });
    }
}
#[test]
fn exact_graph_node_record_rejects_invalid_boundary_flags_without_mutation() {
    for column in ["is_entry", "is_exit"] {
        let (_temp, store) = initialized_store();
        let node = store
            .run_graph_nodes("run-1", None, 1)
            .expect("node")
            .remove(0);
        block_on(store.connection.sqlite_ignore_check_constraints(true))
            .expect("damage fixture constraints");
        block_on(
            store.connection.exec_update_count(
                &update("workflow_run_graph_nodes")
                    .value(column, 2_i64)
                    .where_eq("run_id", "run-1")
                    .where_eq("node_id", node.node.id.clone()),
            ),
        )
        .expect("damage flag");
        let changes = fixture_changes(&store.connection);
        assert!(store.run_graph_node_record("run-1", &node.node.id).is_err());
        assert!(store.run_graph_node("run-1", &node.node.id).is_err());
        assert_eq!(fixture_changes(&store.connection), changes);
        let value: i64 = block_on(async {
            let row = select("workflow_run_graph_nodes")
                .columns(&[column])
                .where_eq("run_id", "run-1")
                .where_eq("node_id", node.node.id.clone())
                .execute_first(&store.connection)
                .await?
                .expect("flag row");
            Ok::<_, switchy_database::DatabaseError>(
                i64::try_from(row.get(column).expect("flag column")).expect("integer flag"),
            )
        })
        .expect("preserved flag");
        assert_eq!(value, 2);
    }
}
#[test]
fn exact_graph_node_record_preserves_roles_and_rejects_damage() {
    let (_temp, store) = initialized_store();
    let source = store
        .run_graph_nodes("run-1", None, 1)
        .expect("nodes")
        .remove(0);
    add_migration_exit_node(&store, &source.node);
    for expected in store.run_graph_nodes("run-1", None, 100).expect("nodes") {
        assert_eq!(
            store
                .run_graph_node_record("run-1", &expected.node.id)
                .expect("record"),
            Some(expected)
        );
    }
    assert_eq!(
        store
            .run_graph_node_record("run-1", "missing")
            .expect("missing node"),
        None
    );
    assert_eq!(
        store
            .run_graph_node_record("missing", &source.node.id)
            .expect("missing run"),
        None
    );
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graph_nodes")
                .value("node_json", "{}")
                .where_eq("run_id", "run-1")
                .where_eq("node_id", DatabaseValue::from((source.node.id).clone())),
        ),
    )
    .expect("damage payload");
    let changes = fixture_changes(&store.connection);
    assert!(
        store
            .run_graph_node_record("run-1", &source.node.id)
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), changes);
}
#[test]
fn schema_16_migration_adds_source_index_without_rebuilding_graph() {
    let (temp, store) = initialized_store();
    let nodes = store.run_graph_nodes("run-1", None, 100).expect("nodes");
    let node_id = &nodes[0].node.id;
    let target = add_migration_exit_node(&store, &nodes[0].node);
    let expected_nodes = store
        .run_graph_nodes("run-1", None, 100)
        .expect("all nodes");
    let edge = bcode_workflow::EdgeDefinition {
        from: node_id.clone(),
        to: target.id.clone(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 42_i64)
            .value("revision", 1_i64)
            .value("source_node_id", DatabaseValue::from((node_id).clone()))
            .value("target_node_id", DatabaseValue::from((target.id).clone()))
            .value(
                "edge_json",
                DatabaseValue::from(serde_json::to_string(&edge).expect("serialize")),
            )
            .execute(&store.connection),
    )
    .expect("persist edge");
    let edges = store.run_graph_edges("run-1", None, 100).expect("edges");
    make_schema_16_graph_without_source_index(&store.connection).expect("schema 16 fixture");
    remove_retirement_fixture(&store);
    drop(store);
    assert!(WorkflowStore::open_in_state_dir(temp.path()).is_err());
    let receipt = WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 71)
        .expect("explicit migration");
    assert_eq!(receipt.previous_schema_version, 16);
    assert_eq!(receipt.new_schema_version, WORKFLOW_STORE_SCHEMA_VERSION);
    assert_eq!(receipt.migrated_at_ms, 71);
    let backup_bytes = std::fs::read(&receipt.backup_path).expect("receipt backup");
    assert_eq!(receipt.backup_sha256, sha256_hex(&backup_bytes));
    let backup = database::open(
        &receipt.backup_path,
        switchy_database_connection::SqliteAccess::ReadOnly,
        std::time::Duration::from_secs(5),
        false,
    )
    .expect("read-only backup");
    assert_schema_16_backup_edge(&backup, &edge);
    assert_backup_nodes(&backup, &expected_nodes);
    drop(backup);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        store.run_graph_revision("run-1").expect("graph revision"),
        Some(1)
    );
    assert_eq!(
        store.run_graph_nodes("run-1", None, 100).expect("nodes"),
        expected_nodes
    );
    assert_eq!(
        store
            .run_graph_incoming_edges("run-1", &target.id, None, 100)
            .expect("incoming"),
        edges
    );
    assert!(
        store
            .run_graph_outgoing_edges("run-1", &target.id, None, 100)
            .expect("target outgoing")
            .is_empty()
    );
    assert!(
        store
            .run_graph_incoming_edges("run-1", node_id, None, 100)
            .expect("source incoming")
            .is_empty()
    );
    assert_eq!(
        store
            .run_graph_edges("run-1", None, 100)
            .expect("preserved edges"),
        edges
    );
    assert_eq!(
        store
            .run_graph_outgoing_edges("run-1", node_id, None, 100)
            .expect("indexed edges"),
        edges
    );
    let index_count: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("sqlite_master")
            .project_as(switchy_database::query::count_all(), "fixture_count")
            .where_eq("type", "index")
            .where_eq("name", "workflow_run_graph_edges_source"),
        "fixture_count",
    )
    .expect("source index");
    assert_eq!(index_count, 1);
}
#[test]
fn outgoing_edge_pages_filter_sources_and_preserve_storage() {
    let (temp, store) = initialized_store();
    let node = store
        .run_graph_nodes("run-1", None, 1)
        .expect("node")
        .remove(0)
        .node;
    let edge = bcode_workflow::EdgeDefinition {
        from: node.id.clone(),
        to: node.id.clone(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    let json = serde_json::to_string(&edge).expect("serialize");
    for id in 0..205 {
        insert_outgoing_page_edge(&store.connection, &node, &json, id).expect("edge");
    }
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 205_i64)
            .value("revision", 1_i64)
            .value("source_node_id", "unrelated")
            .value("target_node_id", DatabaseValue::from((node.id).clone()))
            .value("edge_json", "invalid")
            .execute(&store.connection),
    )
    .expect("unrelated source damage");
    let first = store
        .run_graph_outgoing_edges("run-1", &node.id, None, 100)
        .expect("first page before reopen");
    assert_eq!(first.len(), 100);
    let mut cursor = first.last().map(|edge| edge.edge_id);
    let mut ids = first
        .into_iter()
        .map(|record| {
            assert_eq!(record.edge, edge);
            record.edge_id
        })
        .collect::<Vec<_>>();
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen paged graph");
    let before = fixture_changes(&store.connection);
    let resumed = store
        .run_graph_outgoing_edges("run-1", &node.id, cursor, 100)
        .expect("resumed page");
    assert_eq!(resumed.len(), 100);
    assert_eq!(resumed.first().expect("first resumed edge").edge_id, 100);
    assert_eq!(resumed.last().expect("last resumed edge").edge_id, 199);
    assert_eq!(
        store
            .run_graph_outgoing_edges("run-1", &node.id, cursor, 100)
            .expect("retried page"),
        resumed
    );
    assert_eq!(fixture_changes(&store.connection), before);
    for expected_len in [100, 5, 0] {
        let page = store
            .run_graph_outgoing_edges("run-1", &node.id, cursor, usize::MAX)
            .expect("page");
        assert_eq!(page.len(), expected_len);
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(|edge| edge.edge_id);
        for item in page {
            assert_eq!(item.edge, edge);
            ids.push(item.edge_id);
        }
    }
    assert_eq!(ids, (0..205).collect::<Vec<u64>>());
    assert!(
        store
            .run_graph_outgoing_edges("run-1", "unrelated", None, 1)
            .is_err()
    );
    assert!(
        store
            .run_graph_outgoing_edges("run-1", "", None, 1)
            .is_err()
    );
    assert!(
        store
            .run_graph_outgoing_edges("missing", &node.id, Some(u64::MAX), 1)
            .is_err()
    );
    assert!(
        store
            .run_graph_outgoing_edges("missing", &node.id, None, 1)
            .expect("missing run")
            .is_empty()
    );
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn edge_pages_validate_inputs_even_for_missing_runs() {
    let (_temp, store) = initialized_store();
    let before = fixture_changes(&store.connection);
    for query in [
        WorkflowStore::run_graph_incoming_edges,
        WorkflowStore::run_graph_outgoing_edges,
    ] {
        for run_id in ["run-1", "missing"] {
            assert!(query(&store, run_id, "", None, 10).is_err());
            assert!(query(&store, run_id, "review", Some(u64::MAX), 10).is_err());
            assert!(
                query(&store, run_id, "missing", None, usize::MAX)
                    .expect("absent endpoint")
                    .is_empty()
            );
            assert!(
                query(&store, run_id, "review", Some(i64::MAX as u64), 10)
                    .expect("valid final cursor")
                    .is_empty()
            );
        }
        assert!(query(&store, "", "review", None, 10).is_err());
    }
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn incoming_edge_pages_are_bounded_and_isolate_unrelated_damage() {
    let (_temp, store) = initialized_store();
    let node = store
        .run_graph_nodes("run-1", None, 1)
        .expect("node")
        .remove(0)
        .node;
    let edge = bcode_workflow::EdgeDefinition {
        from: node.id.clone(),
        to: node.id.clone(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    let json = serde_json::to_string(&edge).expect("serialize");
    for index in 0..205 {
        insert_incoming_page_edge(&store.connection, &node, &json, index).expect("edge");
    }
    insert_unrelated_incoming_edge(&store.connection).expect("unrelated damage");
    let before = fixture_changes(&store.connection);
    let mut cursor = None;
    let mut ids = Vec::new();
    loop {
        let page = store
            .run_graph_incoming_edges("run-1", &node.id, cursor, usize::MAX)
            .expect("page");
        assert!(page.len() <= 100);
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(|item| item.edge_id);
        for item in page {
            assert_eq!(item.edge, edge);
            ids.push(item.edge_id);
        }
    }
    assert_eq!(ids, (0..205).collect::<Vec<u64>>());
    let first = store
        .run_graph_incoming_edges("run-1", &node.id, None, 1)
        .expect("single edge page");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].edge_id, 0);
    let second = store
        .run_graph_incoming_edges("run-1", &node.id, Some(0), 1)
        .expect("exclusive cursor");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].edge_id, 1);
    assert!(
        store
            .run_graph_incoming_edges("run-1", "unrelated", None, 10)
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graph_edges")
                .value("edge_json", "invalid")
                .where_eq("run_id", "run-1")
                .where_eq("edge_id", 0_i64),
        ),
    )
    .expect("damage first edge");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .run_graph_incoming_edges("run-1", &node.id, None, 1)
            .is_err()
    );
    let page = store
        .run_graph_incoming_edges("run-1", &node.id, Some(0), 1)
        .expect("earlier damage lies outside page");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].edge_id, 1);
    assert_eq!(page[0].edge, edge);
    assert_eq!(fixture_changes(&store.connection), before);
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graph_edges")
                .value("edge_json", DatabaseValue::from(json))
                .where_eq("run_id", "run-1")
                .where_eq("edge_id", 0_i64),
        ),
    )
    .expect("restore edge before testing missing endpoints");
    let restored = store
        .run_graph_incoming_edges("run-1", &node.id, None, 10)
        .expect("valid edges before endpoint damage");
    assert_eq!(restored.len(), 10);
    assert_eq!(restored[0].edge, edge);
    remove_incoming_source_nodes(&store.connection).expect("damage source");
    let before = fixture_changes(&store.connection);
    let error = store
        .run_graph_incoming_edges("run-1", &node.id, None, 10)
        .expect_err("missing endpoints must fail");
    assert!(matches!(
        error,
        WorkflowStoreError::InvalidData(message)
            if message == "workflow graph edge relationships are inconsistent"
    ));
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn graph_readers_reject_missing_header_without_reconstruction() {
    let (temp, store) = initialized_store();
    let node = store
        .run_graph_nodes("run-1", None, 1)
        .expect("node")
        .remove(0)
        .node;
    block_on(async {
        store.connection.sqlite_foreign_keys(false).await?;
        store
            .connection
            .exec_delete_count(&delete("workflow_run_graphs").where_eq("run_id", "run-1"))
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("damage header");
    let before = fixture_changes(&store.connection);
    assert!(store.run_graph_revision("run-1").is_err());
    assert!(
        store
            .run_graph_outgoing_edges("run-1", &node.id, None, 10)
            .is_err()
    );
    assert!(store.run_graph_node("run-1", &node.id).is_err());
    assert!(store.run_graph_nodes("run-1", None, 10).is_err());
    assert!(store.run_graph_edge("run-1", 0).is_err());
    assert!(store.run_graph_edges("run-1", None, 10).is_err());
    assert!(
        store
            .run_graph_incoming_edges("run-1", &node.id, None, 10)
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert_eq!(
        fixture_scalar_unsigned(
            &store.connection,
            &select("workflow_run_graphs")
                .project_as(switchy_database::query::count_all(), "fixture_count"),
            "fixture_count"
        )
        .expect("headers"),
        0
    );
    assert_eq!(
        fixture_scalar_unsigned(
            &store.connection,
            &select("workflow_run_graph_nodes")
                .project_as(switchy_database::query::count_all(), "fixture_count")
                .where_eq("run_id", "run-1"),
            "fixture_count"
        )
        .expect("surviving nodes"),
        1
    );
    drop(store);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen missing header");
    let before = fixture_changes(&reopened.connection);
    assert!(reopened.run_graph_revision("run-1").is_err());
    assert!(reopened.run_graph_node("run-1", &node.id).is_err());
    assert!(reopened.run_graph_nodes("run-1", None, 1).is_err());
    assert!(reopened.run_graph_edge("run-1", 0).is_err());
    assert!(reopened.run_graph_edges("run-1", None, 1).is_err());
    assert!(
        reopened
            .run_graph_incoming_edges("run-1", &node.id, None, 1)
            .is_err()
    );
    assert_eq!(fixture_changes(&reopened.connection), before);
    assert_eq!(
        fixture_scalar_unsigned(
            &reopened.connection,
            &select("workflow_run_graphs")
                .project_as(switchy_database::query::count_all(), "fixture_count"),
            "fixture_count"
        )
        .expect("header remains absent"),
        0
    );
}
#[test]
fn graph_readers_preserve_corrupt_and_unsupported_revisions() {
    let (_temp, store) = initialized_store();
    let node = store
        .run_graph_nodes("run-1", None, 1)
        .expect("node")
        .remove(0)
        .node;
    block_on(store.connection.sqlite_ignore_check_constraints(true)).expect("fixture constraints");
    for revision in [-1_i64, 0, 2] {
        block_on(
            store.connection.exec_update_count(
                &update("workflow_run_graphs")
                    .value("revision", DatabaseValue::from(revision))
                    .where_eq("run_id", "run-1"),
            ),
        )
        .expect("fixture revision");
        let before = fixture_changes(&store.connection);
        if revision > 0 {
            assert_eq!(
                store
                    .run_graph_revision("run-1")
                    .expect("revision inspection"),
                Some(2)
            );
        } else {
            assert!(store.run_graph_revision("run-1").is_err());
        }
        assert!(
            store
                .run_graph_outgoing_edges("run-1", &node.id, None, 10)
                .is_err()
        );
        assert!(store.run_graph_node("run-1", &node.id).is_err());
        assert!(store.run_graph_nodes("run-1", None, 10).is_err());
        assert!(store.run_graph_edge("run-1", 0).is_err());
        assert!(store.run_graph_edges("run-1", None, 10).is_err());
        assert!(
            store
                .run_graph_incoming_edges("run-1", &node.id, None, 10)
                .is_err()
        );
        assert_eq!(fixture_changes(&store.connection), before);
        assert_eq!(
            block_on(async {
                let fixture_row = select("workflow_run_graphs")
                    .columns(&["revision"])
                    .where_eq("run_id", "run-1")
                    .execute_first(&store.connection)
                    .await?
                    .ok_or_else(|| WorkflowStoreError::InvalidData("missing fixture row".into()))?;
                match fixture_row.get("revision") {
                    Some(DatabaseValue::Int64(value)) => Ok::<_, WorkflowStoreError>(value),
                    _ => Err(WorkflowStoreError::InvalidData(
                        "expected signed fixture revision".into(),
                    )),
                }
            })
            .expect("preserved revision"),
            revision
        );
    }
}
#[test]
fn current_graph_pages_preserve_caller_transaction() {
    let (_temp, store) = initialized_store();
    let expected = store
        .current_run_graph_page("run-1", Some(1), None, None, 100)
        .expect("baseline");
    let before = fixture_changes(&store.connection);
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("snapshot");
    let page = WorkflowStore::current_run_graph_page_in(
        transaction.as_ref(),
        "run-1",
        Some(1),
        None,
        None,
        100,
    )
    .expect("caller snapshot page");
    assert_eq!(page.nodes, expected.nodes);
    assert_eq!(page.edges, expected.edges);
    assert_eq!(page.nodes_complete, expected.nodes_complete);
    assert_eq!(page.edges_complete, expected.edges_complete);
    assert_eq!(
        WorkflowStore::current_run_graph_nodes_in(transaction.as_ref(), "run-1", 1, None, 100)
            .expect("nodes"),
        expected.nodes
    );
    assert_eq!(
        WorkflowStore::current_run_graph_edges_in(transaction.as_ref(), "run-1", 1, None, 100)
            .expect("edges"),
        expected.edges
    );
    assert!(
        WorkflowStore::current_run_graph_page_in(
            transaction.as_ref(),
            "run-1",
            Some(2),
            None,
            None,
            100
        )
        .is_err()
    );
    assert!(
        WorkflowStore::current_run_graph_nodes_in(transaction.as_ref(), "run-1", 2, None, 100)
            .is_err()
    );
    assert!(
        WorkflowStore::current_run_graph_edges_in(transaction.as_ref(), "run-1", 2, None, 100)
            .is_err()
    );
    assert!(!block_on(transaction.sqlite_is_autocommit()).expect("autocommit state"));
    assert_eq!(fixture_changes(transaction.as_ref()), before);
    block_on(transaction.rollback()).expect("caller retains ownership");
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
}
#[test]
fn initial_graph_reads_preserve_caller_revision_transaction() {
    let (_temp, store) = initialized_store();
    let expected_nodes = store
        .run_graph_nodes("run-1", None, 10)
        .expect("baseline nodes");
    let expected_node = store
        .run_graph_node_record("run-1", "review")
        .expect("baseline record");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("caller transaction");
    assert!(
        WorkflowStore::run_graph_incoming_edges_in(
            transaction.as_ref(),
            "run-1",
            "review",
            None,
            10
        )
        .expect("initial edges")
        .is_empty()
    );
    assert_eq!(
        WorkflowStore::run_graph_nodes_in(transaction.as_ref(), "run-1", None, 10)
            .expect("caller snapshot nodes"),
        expected_nodes
    );
    assert_eq!(
        WorkflowStore::run_graph_node_record_in(transaction.as_ref(), "run-1", "review")
            .expect("caller record"),
        expected_node
    );
    block_on(
        transaction.as_ref().exec_update_count(
            &update("workflow_run_graphs")
                .value("revision", 2_i64)
                .where_eq("run_id", "run-1"),
        ),
    )
    .expect("caller revision fixture");
    let before = fixture_changes(transaction.as_ref());
    assert!(
        WorkflowStore::run_graph_incoming_edges_in(
            transaction.as_ref(),
            "run-1",
            "review",
            None,
            10
        )
        .is_err()
    );
    assert!(
        WorkflowStore::run_graph_outgoing_edges_in(
            transaction.as_ref(),
            "run-1",
            "review",
            None,
            10
        )
        .is_err()
    );
    assert!(
        WorkflowStore::run_graph_node_record_in(transaction.as_ref(), "run-1", "review").is_err()
    );
    assert!(WorkflowStore::run_graph_nodes_in(transaction.as_ref(), "run-1", None, 10).is_err());
    assert!(!block_on(transaction.sqlite_is_autocommit()).expect("autocommit state"));
    assert_eq!(fixture_changes(transaction.as_ref()), before);
    assert_eq!(
        run_graph::graph_revision(transaction.as_ref(), "run-1").expect("caller revision"),
        Some(2)
    );
    block_on(transaction.rollback()).expect("caller rollback");
    assert_eq!(
        store
            .run_graph_revision("run-1")
            .expect("restored revision"),
        Some(1)
    );
    assert!(
        store
            .run_graph_incoming_edges("run-1", "review", None, 10)
            .expect("restored edges")
            .is_empty()
    );
    assert_eq!(
        store
            .run_graph_nodes("run-1", None, 10)
            .expect("restored nodes"),
        expected_nodes
    );
    assert_eq!(
        store
            .run_graph_node_record("run-1", "review")
            .expect("restored record"),
        expected_node
    );
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
}
#[test]
fn graph_page_failure_preserves_uncommitted_caller_writes() {
    let (_temp, store) = initialized_store();
    let expected = store
        .current_run_graph_page("run-1", Some(1), None, None, 100)
        .expect("baseline");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("caller transaction");
    block_on(
        transaction.as_ref().exec_update_count(
            &update("workflow_run_graph_nodes")
                .value("node_json", "invalid-json")
                .where_eq("run_id", "run-1")
                .where_eq("node_id", "review"),
        ),
    )
    .expect("uncommitted damage fixture");
    let before = fixture_changes(transaction.as_ref());
    assert!(
        WorkflowStore::current_run_graph_page_in(
            transaction.as_ref(),
            "run-1",
            Some(1),
            None,
            None,
            100
        )
        .is_err()
    );
    assert!(
        WorkflowStore::current_run_graph_nodes_in(transaction.as_ref(), "run-1", 1, None, 100)
            .is_err()
    );
    assert!(!block_on(transaction.sqlite_is_autocommit()).expect("autocommit state"));
    assert_eq!(fixture_changes(transaction.as_ref()), before);
    let payload: String = fixture_scalar_text(
        transaction.as_ref(),
        &select("workflow_run_graph_nodes")
            .columns(&["node_json"])
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .where_eq("revision", 1_i64),
        "node_json",
    )
    .expect("caller write remains visible");
    assert_eq!(payload, "invalid-json");
    block_on(transaction.rollback()).expect("caller rolls back own write");
    let restored = store
        .current_run_graph_page("run-1", Some(1), None, None, 100)
        .expect("original graph restored");
    assert_eq!(restored.nodes, expected.nodes);
    assert_eq!(restored.edges, expected.edges);
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
}
#[test]
fn exact_current_edge_lookup_preserves_snapshot_and_rejects_future_revision() {
    let (_temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0_i64)
            .value("revision", 1_i64)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value(
                "edge_json",
                DatabaseValue::from(serde_json::to_string(&edge).expect("edge")),
            )
            .execute(&store.connection),
    )
    .expect("edge fixture");
    assert_eq!(
        store
            .current_run_graph_edge("run-1", 0)
            .expect("current")
            .expect("edge")
            .edge,
        edge
    );
    assert!(
        store
            .current_run_graph_edge("run-1", 1)
            .expect("missing edge")
            .is_none()
    );
    assert!(
        store
            .current_run_graph_edge("missing", 0)
            .expect("missing run")
            .is_none()
    );
    assert!(store.current_run_graph_edge("run-1", u64::MAX).is_err());
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("caller snapshot");
    assert!(
        WorkflowStore::current_run_graph_edge_in(transaction.as_ref(), "run-1", 0)
            .expect("caller read")
            .is_some()
    );
    block_on(async {
        let fixture_source = select("workflow_run_graph_edges")
            .where_eq("run_id", "run-1")
            .where_eq("edge_id", 0_i64)
            .execute(transaction.as_ref())
            .await?;
        let fixture_inserted = fixture_source.len();
        for fixture_row in fixture_source {
            insert("workflow_run_graph_edges")
                .value("run_id", fixture_row.get("run_id").expect("source column"))
                .value(
                    "edge_id",
                    fixture_row.get("edge_id").expect("source column"),
                )
                .value("revision", 2_i64)
                .value(
                    "source_node_id",
                    fixture_row.get("source_node_id").expect("source column"),
                )
                .value(
                    "target_node_id",
                    fixture_row.get("target_node_id").expect("source column"),
                )
                .value(
                    "edge_json",
                    fixture_row.get("edge_json").expect("source column"),
                )
                .execute(transaction.as_ref())
                .await?;
        }
        Ok::<_, switchy_database::DatabaseError>(fixture_inserted)
    })
    .expect("future edge fixture");
    let before = fixture_changes(transaction.as_ref());
    assert!(WorkflowStore::current_run_graph_edge_in(transaction.as_ref(), "run-1", 0).is_err());
    assert_eq!(fixture_changes(transaction.as_ref()), before);
    assert!(!block_on(transaction.sqlite_is_autocommit()).expect("autocommit state"));
    block_on(transaction.rollback()).expect("caller rollback");
    assert!(
        store
            .current_run_graph_edge("run-1", 0)
            .expect("restored")
            .is_some()
    );
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
}
#[test]
fn edge_reads_report_selected_revision_independently_of_graph_revision() {
    let (_temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0_i64)
            .value("revision", 1_i64)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value(
                "edge_json",
                DatabaseValue::from(serde_json::to_string(&edge).expect("edge")),
            )
            .execute(&store.connection),
    )
    .expect("initial edge fixture");
    assert_eq!(
        store
            .run_graph_edge("run-1", 0)
            .expect("initial")
            .expect("edge")
            .revision,
        1
    );
    assert_eq!(
        store
            .run_graph_edges("run-1", None, 10)
            .expect("initial page")[0]
            .revision,
        1
    );
    block_on(async {
        let fixture_source = select("workflow_run_graph_edges")
            .where_eq("run_id", "run-1")
            .where_eq("edge_id", 0_i64)
            .execute(&store.connection)
            .await?;
        let fixture_inserted = fixture_source.len();
        for fixture_row in fixture_source {
            insert("workflow_run_graph_edges")
                .value("run_id", fixture_row.get("run_id").expect("source column"))
                .value(
                    "edge_id",
                    fixture_row.get("edge_id").expect("source column"),
                )
                .value("revision", 2_i64)
                .value(
                    "source_node_id",
                    fixture_row.get("source_node_id").expect("source column"),
                )
                .value(
                    "target_node_id",
                    fixture_row.get("target_node_id").expect("source column"),
                )
                .value(
                    "edge_json",
                    fixture_row.get("edge_json").expect("source column"),
                )
                .execute(&store.connection)
                .await?;
        }
        store
            .connection
            .exec_update_count(
                &update("workflow_run_graphs")
                    .value("revision", 3_i64)
                    .where_eq("run_id", "run-1"),
            )
            .await?;
        Ok::<_, switchy_database::DatabaseError>(fixture_inserted)
    })
    .expect("later revision fixture");
    let before = fixture_changes(&store.connection);
    let historical = store
        .run_graph_edge_revision("run-1", 0, 1)
        .expect("history")
        .expect("edge");
    let current = store
        .current_run_graph_edge("run-1", 0)
        .expect("current")
        .expect("edge");
    assert_eq!(historical.revision, 1);
    assert_eq!(current.revision, 2);
    assert_eq!(historical.edge, current.edge);
    assert_ne!(historical, current);
    let page = store
        .current_run_graph_page("run-1", Some(3), None, None, 10)
        .expect("page");
    assert_eq!(page.revision, 3);
    assert_eq!(page.edges, vec![current]);
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn current_edge_pages_revalidate_shared_endpoints_after_changes() {
    let (_temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    for id in 0..3 {
        block_on(
            insert("workflow_run_graph_edges")
                .value("run_id", "run-1")
                .value("edge_id", DatabaseValue::from(id))
                .value("revision", 1_i64)
                .value("source_node_id", "review")
                .value("target_node_id", "review")
                .value(
                    "edge_json",
                    DatabaseValue::from((serde_json::to_string(&edge).expect("edge")).clone()),
                )
                .execute(&store.connection),
        )
        .expect("edge fixture");
    }
    assert_eq!(
        store
            .current_run_graph_edges("run-1", 1, None, 2)
            .expect("page")
            .len(),
        2
    );
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("snapshot");
    block_on(async {
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("revision", 2_i64)
            .value("node_json", "{")
            .value("is_entry", 1_i64)
            .value("is_exit", 1_i64)
            .execute(transaction.as_ref())
            .await?;
        transaction
            .as_ref()
            .exec_update_count(
                &update("workflow_run_graphs")
                    .value("revision", 2_i64)
                    .where_eq("run_id", "run-1"),
            )
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("damaged current endpoint");
    let before = fixture_changes(transaction.as_ref());
    assert!(
        WorkflowStore::run_graph_edge_revision_in(transaction.as_ref(), "run-1", 2, 1)
            .expect("historical edge")
            .is_some()
    );
    assert!(
        WorkflowStore::current_run_graph_edges_in(transaction.as_ref(), "run-1", 2, Some(1), 2)
            .is_err()
    );
    assert!(WorkflowStore::current_run_graph_edge_in(transaction.as_ref(), "run-1", 0).is_err());
    assert_eq!(fixture_changes(transaction.as_ref()), before);
    assert!(!block_on(transaction.sqlite_is_autocommit()).expect("autocommit state"));
    block_on(transaction.rollback()).expect("rollback");
    let page = store
        .current_run_graph_edges("run-1", 1, Some(1), 2)
        .expect("restored");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].edge_id, 2);
}
#[test]
fn current_edge_page_reuse_does_not_skip_historical_endpoint_validation() {
    let (_temp, store) = initialized_store();
    let node = store
        .run_graph_node("run-1", "review")
        .expect("node")
        .expect("review");
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("revision", 3_i64)
            .value(
                "node_json",
                DatabaseValue::from(serde_json::to_string(&node).expect("current node")),
            )
            .value("is_entry", 1_i64)
            .value("is_exit", 1_i64)
            .execute(&store.connection),
    )
    .expect("current node fixture");
    block_on(async {
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("revision", 2_i64)
            .value("node_json", "{")
            .value("is_entry", 1_i64)
            .value("is_exit", 1_i64)
            .execute(&store.connection)
            .await?;
        store
            .connection
            .exec_update_count(
                &update("workflow_run_graphs")
                    .value("revision", 3_i64)
                    .where_eq("run_id", "run-1"),
            )
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("damaged historical node fixture");
    for (id, revision) in [(0, 1), (1, 2)] {
        block_on(
            insert("workflow_run_graph_edges")
                .value("run_id", "run-1")
                .value("edge_id", DatabaseValue::from(id))
                .value("revision", DatabaseValue::from(revision))
                .value("source_node_id", "review")
                .value("target_node_id", "review")
                .value(
                    "edge_json",
                    DatabaseValue::from((serde_json::to_string(&edge).expect("edge")).clone()),
                )
                .execute(&store.connection),
        )
        .expect("edge fixture");
    }
    let before = fixture_changes(&store.connection);
    let first = store
        .current_run_graph_edges("run-1", 3, None, 1)
        .expect("first edge");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].edge_id, 0);
    assert_eq!(
        store
            .current_run_graph_node("run-1", "review")
            .expect("current")
            .expect("node")
            .revision,
        3
    );
    assert!(store.current_run_graph_edges("run-1", 3, None, 2).is_err());
    assert!(store.current_run_graph_edge("run-1", 1).is_err());
    assert!(store.run_graph_edge_revision("run-1", 1, 2).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
}
#[test]
fn initial_graph_pages_reject_zero_limits_without_mutation() {
    let (_temp, store) = initialized_store();
    let before = fixture_changes(&store.connection);
    for run_id in ["run-1", "missing"] {
        assert!(store.run_graph_nodes(run_id, None, 0).is_err());
        assert!(store.run_graph_edges(run_id, None, 0).is_err());
        assert!(
            store
                .run_graph_incoming_edges(run_id, "review", None, 0)
                .is_err()
        );
        assert!(
            store
                .run_graph_outgoing_edges(run_id, "review", None, 0)
                .is_err()
        );
        assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("caller snapshot");
        assert!(WorkflowStore::run_graph_nodes_in(transaction.as_ref(), run_id, None, 0).is_err());
        assert!(WorkflowStore::run_graph_edges_in(transaction.as_ref(), run_id, None, 0).is_err());
        assert!(
            WorkflowStore::run_graph_incoming_edges_in(
                transaction.as_ref(),
                run_id,
                "review",
                None,
                0
            )
            .is_err()
        );
        assert!(
            WorkflowStore::run_graph_outgoing_edges_in(
                transaction.as_ref(),
                run_id,
                "review",
                None,
                0
            )
            .is_err()
        );
        assert!(!block_on(transaction.sqlite_is_autocommit()).expect("autocommit state"));
        block_on(transaction.rollback()).expect("caller rollback");
    }
    assert_eq!(fixture_changes(&store.connection), before);
    assert_eq!(
        store
            .run_graph_nodes("run-1", None, 1)
            .expect("valid page")
            .len(),
        1
    );
}
#[test]
fn current_edge_pages_surface_minimum_storage_identity_damage() {
    let (_temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(store.connection.sqlite_ignore_check_constraints(true)).expect("allow damage fixture");
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", DatabaseValue::from(i64::MIN))
            .value("revision", 1_i64)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value(
                "edge_json",
                DatabaseValue::from(serde_json::to_string(&edge).expect("edge")),
            )
            .execute(&store.connection),
    )
    .expect("invalid edge identity fixture");
    block_on(store.connection.sqlite_ignore_check_constraints(false)).expect("restore checks");
    let before = fixture_changes(&store.connection);
    assert!(store.current_run_graph_edges("run-1", 1, None, 1).is_err());
    assert!(
        store
            .current_run_graph_page("run-1", Some(1), None, None, 1)
            .is_err()
    );
    assert!(store.run_graph_edges("run-1", None, 1).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
}
#[test]
fn graph_pages_include_maximum_edge_identity_and_finish_without_overflow() {
    let (_temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    let maximum = u64::try_from(i64::MAX).expect("maximum identity");
    for id in [maximum - 1, maximum] {
        block_on(
            insert("workflow_run_graph_edges")
                .value("run_id", "run-1")
                .value("edge_id", DatabaseValue::from(id))
                .value("revision", 1_i64)
                .value("source_node_id", "review")
                .value("target_node_id", "review")
                .value(
                    "edge_json",
                    DatabaseValue::from((serde_json::to_string(&edge).expect("edge")).clone()),
                )
                .execute(&store.connection),
        )
        .expect("edge fixture");
    }
    let before = fixture_changes(&store.connection);
    let first = store
        .current_run_graph_page("run-1", Some(1), None, None, 1)
        .expect("first page");
    assert_eq!(first.edges[0].edge_id, maximum - 1);
    assert_eq!(first.next_edge_id, None);
    assert!(!first.edges_complete);
    let last = store
        .current_run_graph_page("run-1", Some(1), None, Some(maximum - 1), 1)
        .expect("last page");
    assert_eq!(last.edges.len(), 1);
    assert_eq!(last.edges[0].edge_id, maximum);
    assert!(last.edges_complete);
    let exhausted = store
        .current_run_graph_page("run-1", Some(1), None, Some(maximum), 1)
        .expect("exhausted page");
    assert!(exhausted.edges.is_empty());
    assert!(exhausted.edges_complete);
    assert_eq!(
        store
            .current_run_graph_edge("run-1", maximum)
            .expect("exact current")
            .expect("edge"),
        last.edges[0]
    );
    assert_eq!(
        store
            .run_graph_edge_revision("run-1", maximum, 1)
            .expect("exact historical")
            .expect("edge"),
        last.edges[0]
    );
    assert_eq!(
        store
            .run_graph_edges("run-1", Some(maximum - 1), 1)
            .expect("initial last"),
        last.edges
    );
    assert!(
        store
            .run_graph_edges("run-1", Some(maximum), 1)
            .expect("initial exhausted")
            .is_empty()
    );
    assert!(
        store
            .run_graph_incoming_edges("run-1", "review", Some(maximum), 1)
            .expect("incoming exhausted")
            .is_empty()
    );
    assert!(
        store
            .run_graph_outgoing_edges("run-1", "review", Some(maximum), 1)
            .expect("outgoing exhausted")
            .is_empty()
    );
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn graph_edge_allocation_observation_includes_retired_edges_outside_page() {
    let (_temp, store) = initialized_store();
    let empty = store
        .current_run_graph_page("run-1", Some(1), None, None, 1)
        .expect("empty graph");
    assert_eq!(empty.next_edge_id, Some(0));
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".into(),
        to: "review".into(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    for id in [2, 900] {
        block_on(
            insert("workflow_run_graph_edges")
                .value("run_id", "run-1")
                .value("edge_id", DatabaseValue::from(id))
                .value("revision", 1_i64)
                .value("source_node_id", "review")
                .value("target_node_id", "review")
                .value(
                    "edge_json",
                    DatabaseValue::from((serde_json::to_string(&edge).expect("edge")).clone()),
                )
                .execute(&store.connection),
        )
        .expect("fixture");
    }
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graph_edges")
                .value("retired_at_revision", 2_i64)
                .where_eq("edge_id", 900_i64),
        ),
    )
    .expect("retire");
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graphs")
                .value("revision", 2_i64)
                .where_eq("run_id", "run-1"),
        ),
    )
    .expect("advance revision");
    let before = fixture_changes(&store.connection);
    let page = store
        .current_run_graph_page("run-1", Some(2), None, None, 1)
        .expect("bounded page");
    assert_eq!(page.edges.len(), 1);
    assert_eq!(page.edges[0].edge_id, 2);
    assert_eq!(page.next_edge_id, Some(901));
    let exhausted = store
        .current_run_graph_page("run-1", Some(2), None, Some(2), 1)
        .expect("empty page");
    assert!(exhausted.edges.is_empty());
    assert_eq!(exhausted.next_edge_id, Some(901));
    assert!(
        store
            .current_run_graph_page("run-1", Some(1), None, None, 1)
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn combined_graph_continuation_requires_revision_without_mutation() {
    let (_temp, store) = initialized_store();
    let first = store
        .current_run_graph_page("run-1", None, None, None, 1)
        .expect("discover revision");
    let cursor = first.nodes[0].node.id.as_str();
    let before = fixture_changes(&store.connection);
    for (node, edge) in [
        (Some(cursor), None),
        (None, Some(0)),
        (Some(cursor), Some(0)),
    ] {
        assert!(
            store
                .current_run_graph_page("run-1", None, node, edge, 1)
                .is_err()
        );
        assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("caller snapshot");
        assert!(
            WorkflowStore::current_run_graph_page_in(
                transaction.as_ref(),
                "run-1",
                None,
                node,
                edge,
                1
            )
            .is_err()
        );
        assert!(!block_on(transaction.sqlite_is_autocommit()).expect("autocommit state"));
        assert!(
            WorkflowStore::current_run_graph_page_in(
                transaction.as_ref(),
                "run-1",
                Some(first.revision),
                node,
                edge,
                1
            )
            .is_ok()
        );
        block_on(transaction.rollback()).expect("caller rollback");
    }
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn graph_continuation_rejects_old_revision_after_plan_changes() {
    let (_temp, store) = initialized_store();
    let first = store
        .current_run_graph_page("run-1", None, None, None, 1)
        .expect("first page");
    let cursor = first.nodes[0].node.id.as_str();
    let mut revised = first.nodes[0].node.clone();
    revised.name = "revised plan node".to_string();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("revision fixture");
    block_on(
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", DatabaseValue::from(cursor))
            .value("revision", 2_i64)
            .value(
                "node_json",
                DatabaseValue::from(serde_json::to_string(&revised).expect("node")),
            )
            .value("is_entry", DatabaseValue::from(first.nodes[0].entry))
            .value("is_exit", DatabaseValue::from(first.nodes[0].exit))
            .execute(transaction.as_ref()),
    )
    .expect("revised node");
    block_on(
        transaction.as_ref().exec_update_count(
            &update("workflow_run_graphs")
                .value("revision", 2_i64)
                .where_eq("run_id", "run-1"),
        ),
    )
    .expect("advance revision");
    database::commit(transaction).expect("commit revision fixture");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .current_run_graph_page("run-1", Some(first.revision), Some(cursor), None, 1)
            .is_err()
    );
    assert!(
        store
            .current_run_graph_page("run-1", None, Some(cursor), None, 1)
            .is_err()
    );
    let refreshed = store
        .current_run_graph_page("run-1", None, None, None, 1)
        .expect("rediscover");
    assert_eq!(refreshed.revision, 2);
    assert_eq!(refreshed.nodes[0].node, revised);
    let continued = store
        .current_run_graph_page("run-1", Some(refreshed.revision), Some(cursor), None, 1)
        .expect("new continuation");
    assert_eq!(continued.revision, 2);
    assert!(
        continued
            .nodes
            .iter()
            .all(|record| record.node.id.as_str() > cursor)
    );
    assert_eq!(
        store
            .run_graph_node_revision("run-1", cursor, 1)
            .expect("history")
            .expect("node"),
        first.nodes[0]
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
}
#[test]
fn combined_graph_page_validates_requests_before_reading_storage() {
    let (_temp, store) = initialized_store();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("caller transaction");
    block_on(async {
        transaction.as_ref().sqlite_defer_foreign_keys(true).await?;
        transaction
            .as_ref()
            .drop_table("workflow_run_graphs")
            .execute(transaction.as_ref())
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("unavailable graph fixture");
    let before = fixture_changes(transaction.as_ref());
    for (revision, node, edge, limit) in [
        (Some(1), None, None, 0),
        (Some(0), None, None, 1),
        (Some(u64::MAX), None, None, 1),
        (Some(1), Some(""), None, 1),
        (Some(1), None, Some(u64::MAX), 1),
        (None, Some("review"), None, 1),
    ] {
        assert!(matches!(
            WorkflowStore::current_run_graph_page_in(
                transaction.as_ref(),
                "run-1",
                revision,
                node,
                edge,
                limit
            ),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(!block_on(transaction.sqlite_is_autocommit()).expect("autocommit state"));
    }
    assert_eq!(fixture_changes(transaction.as_ref()), before);
    block_on(transaction.rollback()).expect("restore graph");
    assert!(
        store
            .current_run_graph_page("run-1", None, None, None, 1)
            .is_ok()
    );
}
#[test]
fn standalone_graph_pages_validate_before_storage_reads() {
    let (_temp, store) = initialized_store();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("snapshot");
    block_on(async {
        transaction.as_ref().sqlite_defer_foreign_keys(true).await?;
        transaction
            .as_ref()
            .drop_table("workflow_run_graphs")
            .execute(transaction.as_ref())
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("unavailable graph fixture");
    let before = fixture_changes(transaction.as_ref());
    for (run_id, revision, limit) in [
        ("", 1, 1),
        ("run-1", 0, 1),
        ("run-1", u64::MAX, 1),
        ("run-1", 1, 0),
    ] {
        assert!(matches!(
            WorkflowStore::current_run_graph_nodes_in(
                transaction.as_ref(),
                run_id,
                revision,
                None,
                limit
            ),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            WorkflowStore::current_run_graph_edges_in(
                transaction.as_ref(),
                run_id,
                revision,
                None,
                limit
            ),
            Err(WorkflowStoreError::InvalidData(_))
        ));
    }
    assert!(matches!(
        WorkflowStore::current_run_graph_nodes_in(transaction.as_ref(), "run-1", 1, Some(""), 1),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert!(matches!(
        WorkflowStore::current_run_graph_edges_in(
            transaction.as_ref(),
            "run-1",
            1,
            Some(u64::MAX),
            1
        ),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert!(!block_on(transaction.sqlite_is_autocommit()).expect("autocommit state"));
    assert_eq!(fixture_changes(transaction.as_ref()), before);
    block_on(transaction.rollback()).expect("restore graph");
    assert!(store.current_run_graph_nodes("run-1", 1, None, 1).is_ok());
    assert!(store.current_run_graph_edges("run-1", 1, None, 1).is_ok());
}
#[test]
fn initial_graph_pages_validate_before_storage_reads() {
    let (_temp, store) = initialized_store();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("snapshot");
    block_on(async {
        transaction.as_ref().sqlite_defer_foreign_keys(true).await?;
        transaction
            .as_ref()
            .drop_table("workflow_run_graphs")
            .execute(transaction.as_ref())
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("unavailable graph fixture");
    let before = fixture_changes(transaction.as_ref());
    for (run_id, limit) in [("", 1), ("run-1", 0)] {
        assert!(matches!(
            WorkflowStore::run_graph_nodes_in(transaction.as_ref(), run_id, None, limit),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            WorkflowStore::run_graph_edges_in(transaction.as_ref(), run_id, None, limit),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            WorkflowStore::run_graph_incoming_edges_in(
                transaction.as_ref(),
                run_id,
                "review",
                None,
                limit
            ),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            WorkflowStore::run_graph_outgoing_edges_in(
                transaction.as_ref(),
                run_id,
                "review",
                None,
                limit
            ),
            Err(WorkflowStoreError::InvalidData(_))
        ));
    }
    assert!(matches!(
        WorkflowStore::run_graph_nodes_in(transaction.as_ref(), "run-1", Some(""), 1),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert!(matches!(
        WorkflowStore::run_graph_edges_in(transaction.as_ref(), "run-1", Some(u64::MAX), 1),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert!(!block_on(transaction.sqlite_is_autocommit()).expect("autocommit state"));
    assert_eq!(fixture_changes(transaction.as_ref()), before);
    block_on(transaction.rollback()).expect("restore graph");
    assert!(store.run_graph_nodes("run-1", None, 1).is_ok());
    assert!(store.run_graph_edges("run-1", None, 1).is_ok());
}
#[test]
fn exact_graph_revisions_validate_before_storage_reads() {
    let (_temp, store) = initialized_store();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("snapshot");
    block_on(async {
        transaction.as_ref().sqlite_defer_foreign_keys(true).await?;
        transaction
            .as_ref()
            .drop_table("workflow_run_graphs")
            .execute(transaction.as_ref())
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("unavailable graph fixture");
    let before = fixture_changes(transaction.as_ref());
    for (run_id, node_id, revision) in [
        ("", "review", 1),
        ("run-1", "", 1),
        ("run-1", "review", 0),
        ("run-1", "review", u64::MAX),
    ] {
        assert!(matches!(
            WorkflowStore::run_graph_node_revision_in(
                transaction.as_ref(),
                run_id,
                node_id,
                revision
            ),
            Err(WorkflowStoreError::InvalidData(_))
        ));
    }
    for (run_id, edge_id, revision) in [
        ("", 0, 1),
        ("run-1", u64::MAX, 1),
        ("run-1", 0, 0),
        ("run-1", 0, u64::MAX),
    ] {
        assert!(matches!(
            WorkflowStore::run_graph_edge_revision_in(
                transaction.as_ref(),
                run_id,
                edge_id,
                revision
            ),
            Err(WorkflowStoreError::InvalidData(_))
        ));
    }
    assert!(matches!(
        WorkflowStore::run_graph_node_revision_in(transaction.as_ref(), "run-1", "review", 1),
        Err(WorkflowStoreError::StructuredDatabase(_))
    ));
    assert!(matches!(
        WorkflowStore::run_graph_edge_revision_in(transaction.as_ref(), "run-1", 0, 1),
        Err(WorkflowStoreError::StructuredDatabase(_))
    ));
    assert!(!block_on(transaction.sqlite_is_autocommit()).expect("autocommit state"));
    assert_eq!(fixture_changes(transaction.as_ref()), before);
    block_on(transaction.rollback()).expect("restore graph");
    assert!(store.run_graph_node_revision("run-1", "review", 1).is_ok());
    assert!(store.run_graph_edge_revision("run-1", 0, 1).is_ok());
}
#[test]
fn current_graph_entities_validate_before_storage_reads() {
    let (_temp, store) = initialized_store();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("snapshot");
    block_on(async {
        transaction.as_ref().sqlite_defer_foreign_keys(true).await?;
        transaction
            .as_ref()
            .drop_table("workflow_run_graphs")
            .execute(transaction.as_ref())
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("unavailable graph fixture");
    let before = fixture_changes(transaction.as_ref());
    for (run_id, node_id) in [("", "review"), ("run-1", "")] {
        assert!(matches!(
            WorkflowStore::current_run_graph_node_in(transaction.as_ref(), run_id, node_id),
            Err(WorkflowStoreError::InvalidData(_))
        ));
    }
    for (run_id, edge_id) in [("", 0), ("run-1", u64::MAX)] {
        assert!(matches!(
            WorkflowStore::current_run_graph_edge_in(transaction.as_ref(), run_id, edge_id),
            Err(WorkflowStoreError::InvalidData(_))
        ));
    }
    assert!(matches!(
        WorkflowStore::current_run_graph_node_in(transaction.as_ref(), "run-1", "review"),
        Err(WorkflowStoreError::StructuredDatabase(_))
    ));
    assert!(matches!(
        WorkflowStore::current_run_graph_edge_in(transaction.as_ref(), "run-1", 0),
        Err(WorkflowStoreError::StructuredDatabase(_))
    ));
    assert!(!block_on(transaction.sqlite_is_autocommit()).expect("autocommit state"));
    assert_eq!(fixture_changes(transaction.as_ref()), before);
    block_on(transaction.rollback()).expect("restore graph");
    assert!(store.current_run_graph_node("run-1", "review").is_ok());
    assert!(store.current_run_graph_edge("run-1", 0).is_ok());
}
#[test]
fn activation_graph_reads_validate_before_storage_reads() {
    let (_temp, store) = initialized_store();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("snapshot");
    block_on(async {
        transaction.as_ref().sqlite_defer_foreign_keys(true).await?;
        transaction
            .as_ref()
            .drop_table("workflow_activations")
            .execute(transaction.as_ref())
            .await?;
        transaction
            .as_ref()
            .drop_table("workflow_run_graphs")
            .execute(transaction.as_ref())
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("unavailable graph fixture");
    let before = fixture_changes(transaction.as_ref());
    for (run_id, node_id, activation_id) in [
        ("", "review", "activation-1"),
        ("run-1", "", "activation-1"),
        ("run-1", "review", ""),
    ] {
        assert!(matches!(
            WorkflowStore::activation_graph_node_in(
                transaction.as_ref(),
                run_id,
                node_id,
                activation_id
            ),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            super::run_graph::initial_activation_node(
                transaction.as_ref(),
                run_id,
                node_id,
                activation_id
            ),
            Err(WorkflowStoreError::InvalidData(_))
        ));
    }
    assert!(matches!(
        WorkflowStore::activation_graph_node_in(
            transaction.as_ref(),
            "run-1",
            "review",
            "activation-1"
        ),
        Err(WorkflowStoreError::StructuredDatabase(_))
    ));
    assert!(matches!(
        super::run_graph::initial_activation_node(
            transaction.as_ref(),
            "run-1",
            "review",
            "activation-1"
        ),
        Err(WorkflowStoreError::StructuredDatabase(_))
    ));
    assert!(!block_on(transaction.sqlite_is_autocommit()).expect("autocommit state"));
    assert_eq!(fixture_changes(transaction.as_ref()), before);
    block_on(transaction.rollback()).expect("restore graph");
    assert!(
        store
            .activation_graph_node("run-1", "review", "absent-activation")
            .expect("restored activation storage")
            .is_none()
    );
}
#[test]
fn rolled_back_graph_revision_is_never_published() {
    let (_temp, store) = initialized_store();
    block_on(
        store
            .connection
            .sqlite_journal_mode(switchy_database::SqliteJournalMode::Wal),
    )
    .expect("concurrent fixture");
    let original = store
        .current_run_graph_node("run-1", "review")
        .expect("read")
        .expect("node");
    let writer = fixture_open(store.path(), false);
    let edit = database::begin(&writer, TransactionMode::Deferred).expect("candidate");
    let mut candidate = original.node.clone();
    candidate.name = "Rolled back candidate".to_string();
    insert_rolled_back_candidate_node(edit.as_ref(), &candidate).expect("candidate node");
    let candidate_edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    insert_candidate_graph_edge(edit.as_ref(), &candidate_edge).expect("candidate edge");
    block_on(
        edit.as_ref().exec_update_count(
            &update("workflow_run_graphs")
                .value("revision", 2_i64)
                .where_eq("run_id", "run-1"),
        ),
    )
    .expect("candidate revision");
    assert_single_node_graph_page(&store, &original);
    assert_activation_keeps_node(&store, &original);
    assert_unpublished_candidate_is_invisible(&store);
    block_on(edit.rollback()).expect("abort candidate");
    assert_unpublished_candidate_is_invisible(&store);
    assert!(
        store
            .current_run_graph_edge("run-1", 0)
            .expect("edge rolled back")
            .is_none()
    );
    assert_eq!(
        fixture_scalar_unsigned(
            &writer,
            &select("workflow_run_graph_edges")
                .project_as(switchy_database::query::count_all(), "fixture_count")
                .where_eq("run_id", "run-1")
                .where_eq("revision", 2_i64),
            "fixture_count"
        )
        .expect("candidate edge removed"),
        0
    );
    assert_single_node_graph_page(&store, &original);
    assert_activation_keeps_node(&store, &original);
    assert_eq!(
        fixture_scalar_unsigned(
            &writer,
            &select("workflow_run_graph_nodes")
                .project_as(switchy_database::query::count_all(), "fixture_count")
                .where_eq("run_id", "run-1")
                .where_eq("revision", 2_i64),
            "fixture_count"
        )
        .expect("candidate removed"),
        0
    );
    assert_eq!(
        store
            .current_run_graph_node("run-1", "review")
            .expect("unchanged node"),
        Some(original.clone())
    );
    let retry = database::begin(&writer, TransactionMode::Deferred).expect("retry revision");
    insert_retry_candidate_node(retry.as_ref(), &candidate).expect("retry node");
    publish_retry_graph_header(retry.as_ref()).expect("publish retry");
    database::commit(retry).expect("commit retry");
    let current = store
        .current_run_graph_node("run-1", "review")
        .expect("retry read")
        .expect("retry node");
    assert_eq!(current.revision, 2);
    assert_eq!(current.node, candidate);
    assert_activation_keeps_node(&store, &original);
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
}
fn assert_unpublished_candidate_is_invisible(store: &WorkflowStore) {
    assert!(matches!(
        store.run_graph_node_revision("run-1", "review", 2),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert!(
        store
            .current_run_graph_edge("run-1", 0)
            .expect("candidate edge invisible")
            .is_none()
    );
    assert!(matches!(
        store.run_graph_edge_revision("run-1", 0, 2),
        Err(WorkflowStoreError::InvalidData(_))
    ));
}

#[test]
fn combined_graph_page_surfaces_damaged_lookahead_edge() {
    let (_temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0_i64)
            .value("revision", 1_i64)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value(
                "edge_json",
                DatabaseValue::from(serde_json::to_string(&edge).expect("edge payload")),
            )
            .execute(&store.connection),
    )
    .expect("valid edge fixture");
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 1_i64)
            .value("revision", 1_i64)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value("edge_json", "not-json")
            .execute(&store.connection),
    )
    .expect("damaged lookahead fixture");
    let before = fixture_changes(&store.connection);
    let edges = store
        .current_run_graph_edges("run-1", 1, None, 1)
        .expect("bounded first edge");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].edge, edge);
    assert_eq!(edges[0].edge_id, 0);
    for expected in [None, Some(1)] {
        assert!(
            store
                .current_run_graph_page("run-1", expected, None, None, 1)
                .is_err()
        );
        assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
        assert_eq!(fixture_changes(&store.connection), before);
    }
}
#[test]
fn combined_graph_page_surfaces_damaged_lookahead_node() {
    let (_temp, store) = initialized_store();
    let oversized = "z".repeat(MAX_ID_BYTES + 1);
    block_on(
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", DatabaseValue::from(oversized))
            .value("revision", 1_i64)
            .value("node_json", "not-json")
            .value("is_entry", 0_i64)
            .value("is_exit", 0_i64)
            .execute(&store.connection),
    )
    .expect("damaged lookahead fixture");
    let before = fixture_changes(&store.connection);
    let nodes = store
        .current_run_graph_nodes("run-1", 1, None, 1)
        .expect("bounded first node");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].node.id, "review");
    let error = store
        .current_run_graph_page("run-1", Some(1), None, None, 1)
        .expect_err("lookahead damage must be surfaced");
    assert!(error.to_string().contains("node_id"), "{error}");
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn initial_node_page_rejects_invalid_identity_before_payload_without_writes() {
    let (_temp, store) = initialized_store();
    for id in [String::new(), " ".to_string(), "x".repeat(MAX_ID_BYTES + 1)] {
        block_on(
            store.connection.exec_update_count(
                &update("workflow_run_graph_nodes")
                    .value("node_id", DatabaseValue::from((id).clone()))
                    .value("node_json", "not-json")
                    .where_eq("run_id", "run-1"),
            ),
        )
        .expect("damaged node fixture");
        let before = fixture_changes(&store.connection);
        let error = store
            .run_graph_nodes("run-1", None, 1)
            .expect_err("invalid identity");
        assert!(
            matches!(error, WorkflowStoreError::InvalidData(ref message) if message.contains("node_id")),
            "{error}"
        );
        let current_error = store
            .current_run_graph_nodes("run-1", 1, None, 1)
            .expect_err("current page must surface invalid identity");
        assert!(
            current_error.to_string().contains("node_id"),
            "{current_error}"
        );
        for expected_revision in [None, Some(1)] {
            let page_error = store
                .current_run_graph_page("run-1", expected_revision, None, None, 1)
                .expect_err("combined page must surface invalid identity");
            assert!(page_error.to_string().contains("node_id"), "{page_error}");
            assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
        }
        assert!(block_on(store.connection.sqlite_is_autocommit()).expect("autocommit state"));
        assert_eq!(fixture_changes(&store.connection), before);
    }
}
#[test]
fn exact_edge_lookup_isolates_damage_without_repair() {
    let (_temp, store) = initialized_store();
    let node = store
        .run_graph_nodes("run-1", None, 1)
        .expect("node")
        .remove(0)
        .node;
    let edge = bcode_workflow::EdgeDefinition {
        from: node.id.clone(),
        to: node.id.clone(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 10_i64)
            .value("revision", 1_i64)
            .value("source_node_id", DatabaseValue::from((node.id).clone()))
            .value("target_node_id", DatabaseValue::from((node.id).clone()))
            .value(
                "edge_json",
                DatabaseValue::from(serde_json::to_string(&edge).expect("serialize")),
            )
            .execute(&store.connection),
    )
    .expect("edge");
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0_i64)
            .value("revision", 1_i64)
            .value("source_node_id", "missing")
            .value("target_node_id", "missing")
            .value("edge_json", "invalid")
            .execute(&store.connection),
    )
    .expect("unrelated corruption");
    let before = fixture_changes(&store.connection);
    assert_eq!(
        store
            .run_graph_edge("run-1", 10)
            .expect("exact edge")
            .expect("present")
            .edge,
        edge
    );
    assert!(store.run_graph_edge("run-1", 5).expect("gap").is_none());
    assert!(store.run_graph_edge("run-1", 0).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
    for missing_source in [true, false] {
        let (source, target) = if missing_source {
            ("missing", node.id.as_str())
        } else {
            (node.id.as_str(), "missing")
        };
        block_on(
            store.connection.exec_update_count(
                &update("workflow_run_graph_edges")
                    .value("source_node_id", DatabaseValue::from(source))
                    .value("target_node_id", DatabaseValue::from(target))
                    .where_eq("run_id", "run-1")
                    .where_eq("edge_id", 10_i64),
            ),
        )
        .expect("damage requested relationship");
        let before = fixture_changes(&store.connection);
        assert!(
            store
                .run_graph_edge("run-1", 10)
                .expect_err("inconsistent relationship")
                .to_string()
                .contains("edge relationships are inconsistent")
        );
        assert_eq!(fixture_changes(&store.connection), before);
    }
}
#[test]
fn exact_edge_lookup_handles_absence_and_overflow() {
    let (_temp, store) = initialized_store();
    assert!(
        store
            .run_graph_edge("run-1", 99)
            .expect("missing edge")
            .is_none()
    );
    assert!(
        store
            .run_graph_edge("missing", 0)
            .expect("missing run")
            .is_none()
    );
    assert!(store.run_graph_edge("run-1", u64::MAX).is_err());
}
#[test]
fn exact_node_lookup_rejects_additional_future_revision_without_mutation() {
    let (_temp, store) = initialized_store();
    block_on(async {
        let fixture_source = select("workflow_run_graph_nodes")
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .execute(&store.connection)
            .await?;
        let fixture_inserted = fixture_source.len();
        for fixture_row in fixture_source {
            insert("workflow_run_graph_nodes")
                .value("run_id", fixture_row.get("run_id").expect("source column"))
                .value(
                    "node_id",
                    fixture_row.get("node_id").expect("source column"),
                )
                .value("revision", 2_i64)
                .value(
                    "node_json",
                    fixture_row.get("node_json").expect("source column"),
                )
                .value(
                    "is_entry",
                    fixture_row.get("is_entry").expect("source column"),
                )
                .value(
                    "is_exit",
                    fixture_row.get("is_exit").expect("source column"),
                )
                .execute(&store.connection)
                .await?;
        }
        Ok::<_, switchy_database::DatabaseError>(fixture_inserted)
    })
    .expect("additional future revision");
    let before = fixture_changes(&store.connection);
    assert!(matches!(
        store.run_graph_node("run-1", "review"),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert!(matches!(
        store.run_graph_nodes("run-1", Some("reviev"), 1),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn exact_node_lookup_rejects_future_node_revision_without_mutation() {
    let (_temp, store) = initialized_store();
    assert!(
        store
            .run_graph_node("run-1", "review")
            .expect("node")
            .is_some()
    );
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graph_nodes")
                .value("revision", 2_i64)
                .where_eq("run_id", "run-1")
                .where_eq("node_id", "review"),
        ),
    )
    .expect("future node revision");
    let before = fixture_changes(&store.connection);
    assert!(matches!(
        store.run_graph_node("run-1", "review"),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert!(matches!(
        store.run_graph_node_record("run-1", "review"),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert!(matches!(
        store.run_graph_nodes("run-1", None, 100),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn edge_between_distinguishes_missing_graph_from_unknown_run() {
    let (_temp, store) = initialized_store();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    block_on(async {
        transaction
            .as_ref()
            .exec_delete_count(&delete("workflow_run_graph_edges").where_eq("run_id", "run-1"))
            .await?;
        transaction
            .as_ref()
            .exec_delete_count(&delete("workflow_run_graph_nodes").where_eq("run_id", "run-1"))
            .await?;
        transaction
            .as_ref()
            .exec_delete_count(&delete("workflow_run_graphs").where_eq("run_id", "run-1"))
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("remove graph");
    let before = fixture_changes(transaction.as_ref());
    assert!(matches!(
        run_graph::initial_edge_between(transaction.as_ref(), "run-1", "review", "absent"),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert_eq!(
        run_graph::initial_edge_between(transaction.as_ref(), "unknown-run", "review", "absent")
            .expect("unknown run"),
        None
    );
    assert_eq!(fixture_changes(transaction.as_ref()), before);
}
#[test]
fn edge_lookup_rejects_additional_endpoint_revision_without_mutation() {
    let (_temp, store) = initialized_store();
    let mut plan = definition("example");
    let mut target = plan.nodes["review"].clone();
    target.id = "target".to_string();
    plan.nodes.insert(target.id.clone(), target);
    plan.edges = vec![bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "target".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    }];
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    block_on(async {
        transaction
            .as_ref()
            .exec_delete_count(&delete("workflow_run_graph_edges").where_eq("run_id", "run-1"))
            .await?;
        transaction
            .as_ref()
            .exec_delete_count(&delete("workflow_run_graph_nodes").where_eq("run_id", "run-1"))
            .await?;
        transaction
            .as_ref()
            .exec_delete_count(&delete("workflow_run_graphs").where_eq("run_id", "run-1"))
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("replace graph");
    run_graph::materialize(transaction.as_ref(), "run-1", &plan).expect("materialize");
    database::commit(transaction).expect("commit");
    for endpoint in ["review", "target"] {
        assert!(
            store
                .run_graph_edge("run-1", 0)
                .expect("intact edge")
                .is_some()
        );
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
        let inserted = block_on(async {
            let fixture_source = select("workflow_run_graph_nodes")
                .where_eq("run_id", "run-1")
                .where_eq("node_id", DatabaseValue::from(endpoint))
                .execute(transaction.as_ref())
                .await?;
            let fixture_inserted = fixture_source.len();
            for fixture_row in fixture_source {
                insert("workflow_run_graph_nodes")
                    .value("run_id", fixture_row.get("run_id").expect("source column"))
                    .value(
                        "node_id",
                        fixture_row.get("node_id").expect("source column"),
                    )
                    .value("revision", 2_i64)
                    .value(
                        "node_json",
                        fixture_row.get("node_json").expect("source column"),
                    )
                    .value(
                        "is_entry",
                        fixture_row.get("is_entry").expect("source column"),
                    )
                    .value(
                        "is_exit",
                        fixture_row.get("is_exit").expect("source column"),
                    )
                    .execute(transaction.as_ref())
                    .await?;
            }
            Ok::<_, switchy_database::DatabaseError>(fixture_inserted)
        })
        .expect("additional endpoint revision");
        assert_eq!(inserted, 1);
        let before = fixture_changes(transaction.as_ref());
        assert!(matches!(
            run_graph::initial_edge_between(transaction.as_ref(), "run-1", "review", "target"),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            run_graph::initial_incoming_edges(transaction.as_ref(), "run-1", "target", None),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            run_graph::initial_outgoing_edges(transaction.as_ref(), "run-1", "review", None),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert_eq!(fixture_changes(transaction.as_ref()), before);
        block_on(transaction.rollback()).expect("restore endpoint");
    }
}
#[test]
fn edge_lookup_rejects_additional_future_revision_at_page_boundary() {
    let (_temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0_i64)
            .value("revision", 1_i64)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value(
                "edge_json",
                DatabaseValue::from(serde_json::to_string(&edge).expect("edge payload")),
            )
            .execute(&store.connection),
    )
    .expect("fixture edge");
    let first = store.run_graph_edges("run-1", None, 1).expect("edge page");
    let edge = first.first().expect("fixture edge");
    let inserted = block_on(async {
        let fixture_source = select("workflow_run_graph_edges")
            .where_eq("run_id", "run-1")
            .where_eq("edge_id", DatabaseValue::from(edge.edge_id))
            .execute(&store.connection)
            .await?;
        let fixture_inserted = fixture_source.len();
        for fixture_row in fixture_source {
            insert("workflow_run_graph_edges")
                .value("run_id", fixture_row.get("run_id").expect("source column"))
                .value(
                    "edge_id",
                    fixture_row.get("edge_id").expect("source column"),
                )
                .value("revision", 2_i64)
                .value(
                    "source_node_id",
                    fixture_row.get("source_node_id").expect("source column"),
                )
                .value(
                    "target_node_id",
                    fixture_row.get("target_node_id").expect("source column"),
                )
                .value(
                    "edge_json",
                    fixture_row.get("edge_json").expect("source column"),
                )
                .execute(&store.connection)
                .await?;
        }
        Ok::<_, switchy_database::DatabaseError>(fixture_inserted)
    })
    .expect("additional revision");
    assert_eq!(inserted, 1);
    let before = fixture_changes(&store.connection);
    assert!(matches!(
        store.run_graph_edges("run-1", None, 1),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert!(matches!(
        store.run_graph_edge("run-1", edge.edge_id),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert_eq!(fixture_changes(&store.connection), before);
}
#[test]
fn initial_edge_pages_preserve_duplicate_edges_and_detect_damage() {
    let (_temp, store) = initialized_store();
    let mut plan = definition("example");
    let node_id = plan.nodes.keys().next().expect("node").clone();
    let edge = bcode_workflow::EdgeDefinition {
        from: node_id.clone(),
        to: node_id,
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    plan.edges = vec![edge.clone(), edge.clone(), edge.clone()];
    // Exercise storage independently of scheduler cycle admission.
    replace_duplicate_edge_graph(&store.connection, &plan);
    assert!(store.run_graph_edges("run-1", Some(u64::MAX), 2).is_err());
    assert!(
        store
            .run_graph_edges("run-1", Some(i64::MAX as u64), 2)
            .expect("last representable cursor")
            .is_empty()
    );
    let first = store.run_graph_edges("run-1", None, 2).expect("first page");
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].edge, edge);
    assert_eq!(first[0].edge_id, 0);
    assert_eq!(first[1].edge_id, 1);
    assert_eq!(
        store.run_graph_edge("run-1", 0).expect("exact first"),
        Some(first[0].clone())
    );
    assert_eq!(
        store.run_graph_edge("run-1", 1).expect("exact duplicate"),
        Some(first[1].clone())
    );
    assert_eq!(
        store
            .run_graph_incoming_edges("run-1", &edge.to, None, 2)
            .expect("incoming"),
        first
    );
    assert!(
        store
            .run_graph_incoming_edges("run-1", "absent", None, 2)
            .expect("other target")
            .is_empty()
    );
    let incoming = store
        .run_graph_incoming_edges("run-1", &edge.to, Some(1), 2)
        .expect("incoming continuation");
    assert_eq!(incoming.len(), 1);
    assert_eq!(incoming[0].edge_id, 2);
    let second = store
        .run_graph_edges("run-1", Some(1), 2)
        .expect("second page");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].edge_id, 2);
    assert!(
        store
            .run_graph_edges("run-1", Some(2), 2)
            .expect("end")
            .is_empty()
    );
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graph_edges")
                .value("target_node_id", "missing")
                .where_eq("edge_id", 2_i64),
        ),
    )
    .expect("damage endpoint");
    let before = fixture_changes(&store.connection);
    assert!(store.run_graph_edges("run-1", Some(1), 2).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graph_edges")
                .value(
                    "edge_json",
                    DatabaseValue::from("x".repeat(MAX_INLINE_JSON_BYTES + 1)),
                )
                .where_eq("edge_id", 0_i64),
        ),
    )
    .expect("oversized edge");
    assert!(
        store
            .run_graph_edges("run-1", None, 1)
            .expect_err("oversized")
            .to_string()
            .contains("payload is invalid or oversized")
    );
}
#[test]
fn pending_standard_work_requires_exact_run_graph_node_without_repair() {
    let (_temp, mut store) = initialized_store();
    let pending = store.pending_activations(10).expect("pending");
    let activation = pending.first().expect("activation");
    assert_eq!(
        store
            .run_graph_node("run-1", &activation.node_id)
            .expect("node"),
        Some(activation.node.clone())
    );
    assert!(
        store
            .run_graph_node("run-1", "absent")
            .expect("absent node")
            .is_none()
    );
    block_on(
        store.connection.exec_delete_count(
            &delete("workflow_run_graph_nodes")
                .where_eq("run_id", DatabaseValue::from((activation.run_id).clone()))
                .where_eq("node_id", DatabaseValue::from((activation.node_id).clone())),
        ),
    )
    .expect("damage graph");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .pending_activations(10)
            .expect_err("must not fall back to definition")
            .to_string()
            .contains("missing run-graph node")
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(
        store
            .prepare_pending_activation(
                &activation.run_id,
                &activation.node_id,
                &activation.activation_id,
                DispatchSideEffect::ReadOnly,
                serde_json::json!({}),
                30,
            )
            .expect_err("admission must not fall back")
            .to_string()
            .contains("missing run-graph node")
    );
    assert_eq!(
        fixture_scalar_unsigned(
            &store.connection,
            &select("workflow_attempts")
                .project_as(switchy_database::query::count_all(), "fixture_count")
                .where_eq("run_id", "run-1"),
            "fixture_count"
        )
        .expect("attempt count"),
        0
    );
}
#[test]
fn output_validation_requires_run_graph_node_before_persistence() {
    let (_temp, mut store) = initialized_store();
    let activation = store.pending_activations(1).expect("pending").remove(0);
    let output = ValidatedOutput {
        output_id: "missing-graph-output".to_string(),
        run_id: activation.run_id.clone(),
        node_id: activation.node_id.clone(),
        activation_id: activation.activation_id.clone(),
        schema_id: activation.node.output.type_name.clone(),
        schema_version: 1,
        value: serde_json::json!(1),
        artifact_reference: None,
        created_at_ms: 20,
    };
    block_on(
        store.connection.exec_update_count(
            &update("workflow_activations")
                .value("node_revision", 2_i64)
                .where_eq("run_id", DatabaseValue::from((activation.run_id).clone()))
                .where_eq(
                    "activation_id",
                    DatabaseValue::from((activation.activation_id).clone()),
                ),
        ),
    )
    .expect("invalid binding");
    let before = fixture_changes(&store.connection);
    assert!(store.persist_validated_output(&output).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
    block_on(
        store.connection.exec_update_count(
            &update("workflow_activations")
                .value("node_revision", 1_i64)
                .where_eq("run_id", DatabaseValue::from((activation.run_id).clone()))
                .where_eq(
                    "activation_id",
                    DatabaseValue::from((activation.activation_id).clone()),
                ),
        ),
    )
    .expect("restore binding");
    block_on(
        store.connection.exec_delete_count(
            &delete("workflow_run_graph_nodes")
                .where_eq("run_id", DatabaseValue::from((activation.run_id).clone()))
                .where_eq("node_id", DatabaseValue::from((activation.node_id).clone())),
        ),
    )
    .expect("damage");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .persist_validated_output(&output)
            .expect_err("missing graph")
            .to_string()
            .contains("activation executable revision is missing")
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert_eq!(
        fixture_scalar_unsigned(
            &store.connection,
            &select("workflow_outputs")
                .project_as(switchy_database::query::count_all(), "fixture_count")
                .where_eq("output_id", DatabaseValue::from((output.output_id).clone())),
            "fixture_count"
        )
        .expect("outputs"),
        0
    );
    assert_eq!(
        fixture_scalar_text(
            &store.connection,
            &select("workflow_activations")
                .columns(&["status"])
                .where_eq("run_id", DatabaseValue::from((activation.run_id).clone()))
                .where_eq(
                    "activation_id",
                    DatabaseValue::from(activation.activation_id),
                ),
            "status"
        )
        .expect("status"),
        "pending"
    );
}
#[test]
fn repeat_back_edges_use_admitted_graph_after_reopen() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let mut definition = repeat_definition();
    store
        .persist_definition("example", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.input = Some(serde_json::json!({"condition_met": false, "iteration": 1}));
    store.create_run(&run).expect("run");
    let body = store
        .pending_activations(10)
        .expect("pending")
        .pop()
        .expect("body");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "body-output".to_string(),
            run_id: run.run_id.clone(),
            node_id: body.node_id,
            activation_id: body.activation_id,
            schema_id: body.node.output.type_name,
            schema_version: 1,
            value: run.input.clone().expect("input"),
            artifact_reference: None,
            created_at_ms: 2,
        })
        .expect("output");
    // Change only the source fixture: the admitted graph must still own execution.
    definition.edges.clear();
    definition.nodes.get_mut("body").expect("body").input.schema = serde_json::json!(false);
    let json = serde_json::to_string(&definition).expect("json");
    block_on(
        store.connection.exec_update_count(
            &update("workflow_definitions")
                .value("definition_json", DatabaseValue::from((json).clone()))
                .value(
                    "checksum_sha256",
                    DatabaseValue::from(sha256_hex(json.as_bytes())),
                ),
        ),
    )
    .expect("source fixture");
    // Reconcile the pending control activation against a newer committed graph.
    let control_id = activation_identity(&run.run_id, "repeat-control", 0);
    revise_repeat_retention_graph(&store.connection).expect("revised fixture");
    block_on(
        insert("workflow_leaf_retentions")
            .value("run_id", "run-1")
            .value("node_id", "repeat-control")
            .value("activation_id", DatabaseValue::from((control_id).clone()))
            .value("revision", 2_i64)
            .execute(&store.connection),
    )
    .expect("retain control");
    retain_repeat_control_edges(&store.connection, &control_id).expect("retain outgoing edges");
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let settled = store
        .settle_pending_control_nodes(&run.run_id, 10, 3)
        .expect("settle");
    assert_eq!(
        store
            .activation_admitted_graph_revision(
                &run.run_id,
                "body",
                &activation_identity(&run.run_id, "body", 1),
            )
            .expect("successor admission"),
        Some(2)
    );
    assert_eq!(
        store
            .activation_admitted_graph_revision(&run.run_id, "repeat-control", &control_id,)
            .expect("historical admission"),
        Some(1)
    );
    assert_eq!(settled.activated.len(), 1);
    assert_eq!(settled.activated[0].node_id, "body");
    assert_eq!(settled.activated[0].dependency_generation, 1);
    assert_eq!(
        settled.activated[0].input,
        Some(serde_json::json!({"condition_met": false, "iteration": 2}))
    );
    assert!(
        store
            .settle_pending_control_nodes(&run.run_id, 10, 4)
            .expect("settled already")
            .activated
            .is_empty()
    );
}
#[test]
fn rejected_run_preserves_existing_run_and_graph_across_reopen() {
    let (temp, mut store) = initialized_store();
    let summary = store.run_summary("run-1").expect("existing run");
    let nodes = store.run_graph_nodes("run-1", None, 100).expect("nodes");
    let activations = store.pending_activations(100).expect("activations");
    let mut plan = definition("invalid-entry");
    plan.input.schema = serde_json::json!({});
    let mut first = plan.nodes["review"].clone();
    first.id = "first".to_string();
    first.input.schema = serde_json::json!({});
    plan.nodes.insert(first.id.clone(), first);
    plan.entries = vec!["first".to_string(), "review".to_string()];
    plan.exits.push("first".to_string());
    plan.nodes.get_mut("review").expect("review").input.schema =
        serde_json::json!({"type": "string"});
    store
        .persist_definition("invalid-entry", 1, &plan)
        .expect("definition");
    let mut rejected = new_run();
    rejected.run_id = "rejected-run".to_string();
    rejected.definition_id = "invalid-entry".to_string();
    rejected.input = Some(serde_json::json!(42));
    let error = store
        .create_run(&rejected)
        .expect_err("entry input must fail schema validation");
    assert!(
        matches!(&error, WorkflowStoreError::InvalidData(_)),
        "unexpected rejection: {error}"
    );
    assert!(
        error.to_string().contains("workflow entry input"),
        "unexpected rejection: {error}"
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(store.run_summary("run-1").expect("existing run"), summary);
    assert_eq!(
        store.run_graph_nodes("run-1", None, 100).expect("nodes"),
        nodes
    );
    assert_eq!(
        store.pending_activations(100).expect("activations"),
        activations
    );
    assert!(
        store
            .run_summary(&rejected.run_id)
            .expect("rejected run")
            .is_none()
    );
    assert!(
        store
            .run_graph_nodes(&rejected.run_id, None, 100)
            .expect("rejected graph")
            .is_empty()
    );
    rejected.input = Some(serde_json::json!("valid"));
    store.create_run(&rejected).expect("retry after reopen");
    let pending = store.pending_activations(100).expect("pending after retry");
    assert_eq!(pending.len(), activations.len() + 2);
    assert_eq!(
        pending
            .iter()
            .filter(|activation| activation.run_id == "run-1")
            .cloned()
            .collect::<Vec<_>>(),
        activations
    );
    let retried = pending
        .iter()
        .filter(|activation| activation.run_id == rejected.run_id)
        .collect::<Vec<_>>();
    assert_eq!(retried.len(), 2);
    assert_eq!(
        retried
            .iter()
            .map(|activation| activation.node_id.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["first", "review"])
    );
    for activation in retried {
        assert_eq!(activation.input, rejected.input);
        assert_eq!(activation.dependency_generation, 0);
    }
}
#[test]
fn idempotent_start_rejects_future_graph_revision_without_mutation() {
    let (_temp, mut store) = initialized_store();
    let summary = store.run_summary("run-1").expect("summary");
    let pending = store.pending_activations(100).expect("pending");
    for revision in [2_u64, i64::MAX.unsigned_abs()] {
        block_on(
            store.connection.exec_update_count(
                &update("workflow_run_graphs")
                    .value("revision", DatabaseValue::from(revision))
                    .where_eq("run_id", "run-1"),
            ),
        )
        .expect("future revision");
        let before = fixture_changes(&store.connection);
        let error = store
            .create_run_idempotent(&new_run())
            .expect_err("future graph");
        assert!(matches!(error, WorkflowStoreError::InvalidData(_)));
        assert_eq!(fixture_changes(&store.connection), before);
        assert_eq!(
            run_graph::graph_revision(&store.connection, "run-1").expect("preserved revision"),
            Some(revision)
        );
        assert_eq!(store.run_summary("run-1").expect("summary"), summary);
        assert!(store.pending_activations(100).is_err());
    }
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graphs")
                .value("revision", 1_i64)
                .where_eq("run_id", "run-1"),
        ),
    )
    .expect("restore fixture revision");
    assert_eq!(store.pending_activations(100).expect("pending"), pending);
}
#[test]
fn idempotent_start_rejects_malformed_graph_revision_without_mutation() {
    let (_temp, mut store) = initialized_store();
    let summary = store.run_summary("run-1").expect("summary");
    // Simulate externally damaged storage, including values excluded by current constraints.
    block_on(store.connection.sqlite_ignore_check_constraints(true))
        .expect("allow damaged fixture");
    for revision in [
        Some(DatabaseValue::Int64(0)),
        Some(DatabaseValue::Int64(-1)),
        Some(DatabaseValue::Real64(1.5)),
        Some(DatabaseValue::String("unknown".to_string())),
        None,
    ] {
        block_on(
            store.connection.exec_update_count(
                &update("workflow_run_graphs")
                    .value(
                        "revision",
                        revision.as_ref().map_or_else(
                            || {
                                switchy_database::query::cast_blob(DatabaseValue::String(
                                    "\u{0001}".into(),
                                ))
                                .into()
                            },
                            |value| {
                                Box::<dyn switchy_database::query::Expression>::from(value.clone())
                            },
                        ),
                    )
                    .where_eq("run_id", "run-1"),
            ),
        )
        .expect("damage header");
        let before = fixture_changes(&store.connection);
        assert!(matches!(
            store.create_run_idempotent(&new_run()),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            store.run_graph_revision("run-1"),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            store.run_graph_nodes("run-1", None, 10),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            store.run_graph_node("run-1", "review"),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            store.run_graph_edges("run-1", None, 10),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            store.run_graph_edge("run-1", 0),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            store.run_graph_incoming_edges("run-1", "review", None, 10),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            store.run_graph_outgoing_edges("run-1", "review", None, 10),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert_eq!(fixture_changes(&store.connection), before);
        if let Some(revision) = revision {
            let row = fixture_rows(
                &store.connection,
                &select("workflow_run_graphs")
                    .columns(&["revision"])
                    .where_eq("run_id", "run-1"),
            )
            .remove(0);
            assert_eq!(row.get("revision").expect("revision"), revision);
        } else {
            let row = fixture_rows(
                &store.connection,
                &select("workflow_run_graphs")
                    .project_as(
                        switchy_database::query::cast_text(switchy_database::query::identifier(
                            "revision",
                        )),
                        "revision_bytes",
                    )
                    .project_as(
                        switchy_database::query::storage_type(switchy_database::query::identifier(
                            "revision",
                        )),
                        "revision_type",
                    )
                    .where_eq("run_id", "run-1"),
            )
            .remove(0);
            assert_eq!(fixture_text(&row, "revision_bytes").as_bytes(), &[1]);
            assert_eq!(fixture_text(&row, "revision_type"), "blob");
        }
        assert_eq!(store.run_summary("run-1").expect("summary"), summary);
    }
}
#[test]
fn idempotent_start_rejects_missing_graph_without_repair() {
    let (_temp, mut store) = initialized_store();
    let run = new_run();
    assert!(!store.create_run_idempotent(&run).expect("duplicate"));
    block_on(async {
        store
            .connection
            .exec_delete_count(&delete("workflow_run_graph_edges").where_eq("run_id", "run-1"))
            .await?;
        store
            .connection
            .exec_delete_count(&delete("workflow_run_graph_nodes").where_eq("run_id", "run-1"))
            .await?;
        store
            .connection
            .exec_delete_count(&delete("workflow_run_graphs").where_eq("run_id", "run-1"))
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
    .expect("damage graph");
    let before = fixture_changes(&store.connection);
    let error = store
        .create_run_idempotent(&run)
        .expect_err("missing graph");
    assert!(matches!(error, WorkflowStoreError::InvalidData(_)));
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(
        store
            .run_summary(&run.run_id)
            .expect("run preserved")
            .is_some()
    );
}
#[test]
fn create_activation_rejects_graph_damage_without_persisting() {
    for damage in 0..4 {
        let (_temp, mut store) = initialized_store();
        let events = store.event_history("run-1", None, 100).expect("events");
        let before = store
            .activations_for_run("run-1", 100)
            .expect("activations");
        block_on(store.connection.sqlite_ignore_check_constraints(true)).expect("fixture");
        block_on(async {
            let fixture_database: &dyn Database = &store.connection;
            match damage {
                0 => {
                    fixture_database
                        .exec_delete_count(
                            &delete("workflow_run_graph_nodes").where_eq("node_id", "review"),
                        )
                        .await?;
                }
                1 => {
                    fixture_database
                        .exec_update_count(
                            &update("workflow_run_graph_nodes")
                                .value("node_json", "{}")
                                .where_eq("node_id", "review"),
                        )
                        .await?;
                }
                2 => {
                    fixture_database
                        .exec_update_count(
                            &update("workflow_run_graph_nodes")
                                .value("is_entry", 2_i64)
                                .where_eq("node_id", "review"),
                        )
                        .await?;
                }
                3 => {
                    fixture_database
                        .exec_update_count(&update("workflow_run_graphs").value("revision", 2_i64))
                        .await?;
                }
                _ => unreachable!("fixture damage case"),
            }
            Ok::<_, switchy_database::DatabaseError>(())
        })
        .expect("damage fixture");
        assert!(
            store
                .create_activation(&NewActivation {
                    activation_id: "new-activation".to_string(),
                    ..new_activation()
                })
                .is_err()
        );
        assert_eq!(
            store.event_history("run-1", None, 100).expect("events"),
            events
        );
        assert_eq!(
            store
                .activations_for_run("run-1", 100)
                .expect("activations"),
            before
        );
    }
}
#[test]
fn successor_reuse_requires_current_graph_reconciliation() {
    let (_temp, mut store) = initialized_store();
    store
        .persist_definition("sequential", 1, &sequential_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "sequential".into();
    run.run_id = "sequential-run".into();
    store.create_run(&run).expect("run");
    let successor_id = activation_identity(&run.run_id, "second", 0);
    store
        .create_activation(&NewActivation {
            run_id: run.run_id.clone(),
            node_id: "second".into(),
            activation_id: successor_id.clone(),
            dependency_generation: 0,
            input: Some(serde_json::json!(2)),
            created_at_ms: 10,
        })
        .expect("historical successor");
    block_on(
        store.connection.exec_update_count(
            &update("workflow_run_graphs")
                .value("revision", 2_i64)
                .where_eq("run_id", DatabaseValue::from((run.run_id).clone())),
        ),
    )
    .expect("revised fixture");
    let output = ValidatedOutput {
        output_id: "source".into(),
        run_id: run.run_id.clone(),
        node_id: "first".into(),
        activation_id: activation_identity(&run.run_id, "first", 0),
        schema_id: "u32".into(),
        schema_version: 1,
        value: serde_json::json!(2),
        artifact_reference: None,
        created_at_ms: 20,
    };
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let error = materialize_selected_successors(
        transaction.as_ref(),
        &output,
        0,
        2,
        vec!["second".into()],
        &BTreeMap::new(),
    )
    .expect_err("historical identity is not current authority");
    assert!(error.to_string().contains("reconciliation"));
    block_on(
        insert("workflow_leaf_retentions")
            .value("run_id", DatabaseValue::from((run.run_id).clone()))
            .value("node_id", "second")
            .value("activation_id", DatabaseValue::from((successor_id).clone()))
            .value("revision", 2_i64)
            .execute(transaction.as_ref()),
    )
    .expect("explicit retention");
    assert!(
        materialize_selected_successors(
            transaction.as_ref(),
            &output,
            0,
            2,
            vec!["second".into()],
            &BTreeMap::new(),
        )
        .expect("reconciled reuse")
        .is_empty()
    );
    let admitted: u64 = fixture_scalar_unsigned(
        transaction.as_ref(),
        &select("workflow_activation_graph_bindings")
            .columns(&["graph_revision"])
            .where_eq("run_id", DatabaseValue::from((run.run_id).clone()))
            .where_eq("activation_id", DatabaseValue::from(successor_id)),
        "graph_revision",
    )
    .expect("historical admission");
    assert_eq!(admitted, 1);
}
#[test]
fn graph_publication_cancels_workflow_owned_gates_and_fences_late_answers() {
    for kind in [
        bcode_workflow::NodeKind::Input,
        bcode_workflow::NodeKind::Approval,
    ] {
        assert_graph_gate_cancellation(kind, false);
        assert_graph_gate_cancellation(kind, true);
    }
}
#[test]
fn parallel_join_uses_run_graph_despite_authored_definition_damage() {
    for damage in 0..5 {
        assert_parallel_definition_independence(|database: &dyn Database| {
            block_on(async {
                match damage {
                    0 => {
                        database
                            .exec_update_count(
                                &update("workflow_definitions").value("definition_json", "{}"),
                            )
                            .await?;
                    }
                    1 => {
                        database
                            .exec_update_count(
                                &update("workflow_definitions").value(
                                    "definition_json",
                                    switchy_database::query::cast_blob("{}"),
                                ),
                            )
                            .await?;
                    }
                    2 => {
                        database
                            .exec_update_count(&update("workflow_definitions").value(
                                "definition_json",
                                switchy_database::query::cast_text(
                                    switchy_database::query::zero_blob(1_048_577),
                                ),
                            ))
                            .await?;
                    }
                    3 => {
                        database
                            .exec_update_count(
                                &update("workflow_definitions").value("checksum_sha256", "invalid"),
                            )
                            .await?;
                    }
                    4 => {
                        database
                            .exec_update_count(
                                &update("workflow_definitions")
                                    .value("checksum_sha256", "0".repeat(64)),
                            )
                            .await?;
                    }
                    _ => unreachable!("definition damage case"),
                }
                Ok::<_, switchy_database::DatabaseError>(())
            })
            .expect("damage authored definition");
        });
    }
}
#[test]
fn output_schema_validation_retains_activation_revision_after_graph_edit() {
    let (_temp, store) = initialized_store();
    let mut replacement = store
        .current_run_graph_node("run-1", "review")
        .expect("current node")
        .expect("node")
        .node;
    replacement.output.type_name = "replacement-string".to_string();
    replacement.output.schema = serde_json::json!({"type": "string"});
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    block_on(async {
        let fixture_source = select("workflow_run_graph_nodes")
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .where_eq("revision", 1_i64)
            .execute(transaction.as_ref())
            .await?;
        let fixture_inserted = fixture_source.len();
        for fixture_row in fixture_source {
            insert("workflow_run_graph_nodes")
                .value("run_id", fixture_row.get("run_id").expect("source column"))
                .value(
                    "node_id",
                    fixture_row.get("node_id").expect("source column"),
                )
                .value("revision", 2_i64)
                .value(
                    "node_json",
                    DatabaseValue::from(
                        (serde_json::to_string(&replacement).expect("serialize")).clone(),
                    ),
                )
                .value(
                    "is_entry",
                    fixture_row.get("is_entry").expect("source column"),
                )
                .value(
                    "is_exit",
                    fixture_row.get("is_exit").expect("source column"),
                )
                .execute(transaction.as_ref())
                .await?;
        }
        Ok::<_, switchy_database::DatabaseError>(fixture_inserted)
    })
    .expect("new revision fixture");
    block_on(
        transaction.as_ref().exec_update_count(
            &update("workflow_run_graphs")
                .value("revision", 2_i64)
                .where_eq("run_id", "run-1"),
        ),
    )
    .expect("advance graph fixture");
    let mut output = ValidatedOutput {
        output_id: "output-1".to_string(),
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_identity("run-1", "review", 0),
        schema_id: "u32".to_string(),
        schema_version: 1,
        value: serde_json::json!(1),
        artifact_reference: None,
        created_at_ms: 20,
    };
    validate_output_against_node_schema(transaction.as_ref(), &output).expect("original binding");
    output.schema_id = "replacement-string".to_string();
    output.value = serde_json::json!("replacement");
    assert!(validate_output_against_node_schema(transaction.as_ref(), &output).is_err());
    block_on(
        transaction.as_ref().exec_update_count(
            &update("workflow_activations")
                .value("node_revision", 2_i64)
                .where_eq("run_id", "run-1")
                .where_eq("node_id", "review"),
        ),
    )
    .expect("new binding fixture");
    validate_output_against_node_schema(transaction.as_ref(), &output).expect("new binding");
}

fn damage_prepared_recovery_graph(
    db: &dyn Database,
    damage: usize,
) -> Result<(), switchy_database::DatabaseError> {
    block_on(async {
        let fixture_database: &dyn Database = db;
        match damage {
            0 => {
                fixture_database
                    .exec_update_count(
                        &update("workflow_run_graph_nodes")
                            .value("node_json", "{}")
                            .where_eq("node_id", "review"),
                    )
                    .await?;
            }
            1 => {
                fixture_database
                    .exec_update_count(&update("workflow_run_graphs").value("revision", 2_i64))
                    .await?;
            }
            2 => {
                fixture_database
                    .exec_delete_count(
                        &delete("workflow_run_graph_nodes").where_eq("node_id", "review"),
                    )
                    .await?;
            }
            3 => {
                fixture_database.sqlite_foreign_keys(false).await?;
                fixture_database
                    .exec_delete_count(&delete("workflow_run_graphs").where_eq("run_id", "run-1"))
                    .await?;
                fixture_database.sqlite_foreign_keys(true).await?;
            }
            _ => unreachable!("fixture damage case"),
        }
        Ok::<_, switchy_database::DatabaseError>(())
    })
}

fn restore_prepared_recovery_header(
    db: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    block_on(async {
        if select("workflow_run_graphs")
            .where_eq("run_id", "run-1")
            .execute_first(db)
            .await?
            .is_some()
        {
            db.exec_update_count(
                &update("workflow_run_graphs")
                    .value("revision", 1_i64)
                    .where_eq("run_id", "run-1"),
            )
            .await?;
        } else {
            insert("workflow_run_graphs")
                .value("run_id", "run-1")
                .value("revision", 1_i64)
                .execute(db)
                .await?;
        }
        Ok::<_, switchy_database::DatabaseError>(())
    })
}

fn restore_prepared_recovery_node(
    db: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    block_on(async {
        db.exec_delete_count(
            &delete("workflow_run_graph_nodes")
                .where_eq("run_id", "run-1")
                .where_eq("node_id", "review")
                .where_eq("revision", 1_i64),
        )
        .await?;
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("revision", 1_i64)
            .value(
                "node_json",
                DatabaseValue::from(
                    (serde_json::to_string(&definition("example").nodes["review"]).expect("node"))
                        .clone(),
                ),
            )
            .value("is_entry", 1_i64)
            .value("is_exit", 1_i64)
            .execute(db)
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
}

fn make_completed_execution_schema_15(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    block_on(async {
        db.drop_table("workflow_run_graph_edges")
            .execute(db)
            .await?;
        db.drop_table("workflow_run_graph_nodes")
            .execute(db)
            .await?;
        db.drop_table("workflow_run_graphs").execute(db).await?;
        Ok::<_, WorkflowStoreError>(())
    })?;
    fixture_historical_activations(db, false)?;
    block_on(async {
        db.exec_update_count(&update("workflow_store_contract").value("schema_version", 15_i64))
            .await?;
        Ok::<_, WorkflowStoreError>(())
    })
}

fn make_initial_graph_schema_15(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    block_on(async {
        db.drop_table("workflow_run_graph_edges")
            .execute(db)
            .await?;
        db.drop_table("workflow_run_graph_nodes")
            .execute(db)
            .await?;
        db.drop_table("workflow_run_graphs").execute(db).await?;
        Ok::<_, WorkflowStoreError>(())
    })?;
    fixture_historical_activations(db, false)?;
    block_on(async {
        db.exec_update_count(&update("workflow_store_contract").value("schema_version", 15_i64))
            .await?;
        Ok::<_, WorkflowStoreError>(())
    })
}

fn make_orphaned_schema_16_graph(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    block_on(async {
        db.drop_index(
            "workflow_run_graph_edges_source",
            "workflow_run_graph_edges",
        )
        .execute(db)
        .await?;
        Ok::<_, WorkflowStoreError>(())
    })?;
    fixture_historical_activations(db, true)?;
    block_on(async {
        db.exec_update_count(
            &update("workflow_store_contract")
                .value("schema_version", 16_i64)
                .where_eq("contract_id", 1_i64),
        )
        .await?;
        db.sqlite_foreign_keys(false).await?;
        db.exec_delete_count(&delete("workflow_run_graphs").where_eq("run_id", "run-1"))
            .await?;
        Ok::<_, WorkflowStoreError>(())
    })
}

fn make_schema_16_graph_without_source_index(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    block_on(async {
        db.drop_index(
            "workflow_run_graph_edges_source",
            "workflow_run_graph_edges",
        )
        .execute(db)
        .await?;
        Ok::<_, WorkflowStoreError>(())
    })?;
    fixture_historical_activations(db, true)?;
    block_on(async {
        db.exec_update_count(
            &update("workflow_store_contract")
                .value("schema_version", 16_i64)
                .where_eq("contract_id", 1_i64),
        )
        .await?;
        Ok::<_, WorkflowStoreError>(())
    })
}

fn revise_repeat_retention_graph(db: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    block_on(async {
        db.exec_update_count(&update("workflow_run_graphs").value("revision", 2_i64))
            .await?;
        insert("workflow_graph_edit_candidates")
            .value("run_id", "run-1")
            .value("mutation_id", "repeat-retention")
            .value("expected_revision", 1_i64)
            .value("request_json", "{}")
            .value("authority_json", "{}")
            .value("created_at_ms", 1_i64)
            .execute(db)
            .await?;
        insert("workflow_graph_edit_publications")
            .value("run_id", "run-1")
            .value("mutation_id", "repeat-retention")
            .value("revision", 2_i64)
            .execute(db)
            .await?;
        Ok::<_, switchy_database::DatabaseError>(())
    })
}

fn retain_repeat_control_edges(
    db: &dyn Database,
    control_id: &str,
) -> Result<(), switchy_database::DatabaseError> {
    block_on(async {
        let fixture_source = select("workflow_run_graph_edges")
            .where_eq("source_node_id", "repeat-control")
            .execute(db)
            .await?;
        for fixture_row in fixture_source {
            insert("workflow_retained_edge_bindings")
                .value("run_id", fixture_row.get("run_id").expect("source column"))
                .value("mutation_id", "repeat-retention")
                .value("graph_revision", 2_i64)
                .value(
                    "edge_id",
                    fixture_row.get("edge_id").expect("source column"),
                )
                .value(
                    "edge_revision",
                    fixture_row.get("revision").expect("source column"),
                )
                .value(
                    "source_node_id",
                    fixture_row.get("source_node_id").expect("source column"),
                )
                .value("source_activation_id", DatabaseValue::from(control_id))
                .execute(db)
                .await?;
        }
        Ok::<_, switchy_database::DatabaseError>(())
    })
}

fn insert_outgoing_page_edge(
    db: &dyn Database,
    node: &bcode_workflow::NodeDefinition,
    json: &str,
    id: i64,
) -> Result<switchy_database::Row, switchy_database::DatabaseError> {
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", DatabaseValue::from(id))
            .value("revision", 1_i64)
            .value("source_node_id", DatabaseValue::from((node.id).clone()))
            .value("target_node_id", DatabaseValue::from((node.id).clone()))
            .value("edge_json", DatabaseValue::from(json))
            .execute(db),
    )
}

fn insert_incoming_page_edge(
    db: &dyn Database,
    node: &bcode_workflow::NodeDefinition,
    json: &str,
    index: i64,
) -> Result<switchy_database::Row, switchy_database::DatabaseError> {
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", DatabaseValue::from(index))
            .value("revision", 1_i64)
            .value("source_node_id", DatabaseValue::from((node.id).clone()))
            .value("target_node_id", DatabaseValue::from((node.id).clone()))
            .value("edge_json", DatabaseValue::from(json))
            .execute(db),
    )
}

fn insert_rolled_back_candidate_node(
    db: &dyn Database,
    candidate: &bcode_workflow::NodeDefinition,
) -> Result<switchy_database::Row, switchy_database::DatabaseError> {
    block_on(
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("revision", 2_i64)
            .value(
                "node_json",
                DatabaseValue::from(serde_json::to_string(candidate).expect("payload")),
            )
            .value("is_entry", 0_i64)
            .value("is_exit", 0_i64)
            .execute(db),
    )
}

fn replace_duplicate_edge_graph(db: &dyn Database, plan: &bcode_workflow::WorkflowDefinition) {
    let transaction = database::begin(db, TransactionMode::Deferred).expect("transaction");
    block_on(
        transaction
            .as_ref()
            .exec_delete_count(&delete("workflow_run_graph_edges").where_eq("run_id", "run-1")),
    )
    .expect("edges");
    block_on(
        transaction
            .as_ref()
            .exec_delete_count(&delete("workflow_run_graph_nodes").where_eq("run_id", "run-1")),
    )
    .expect("nodes");
    block_on(
        transaction
            .as_ref()
            .exec_delete_count(&delete("workflow_run_graphs").where_eq("run_id", "run-1")),
    )
    .expect("graph");
    run_graph::materialize(transaction.as_ref(), "run-1", plan).expect("materialize");
    database::commit(transaction).expect("commit");
}

fn insert_unrelated_incoming_edge(
    db: &dyn Database,
) -> Result<switchy_database::Row, switchy_database::DatabaseError> {
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 205_i64)
            .value("revision", 1_i64)
            .value("source_node_id", "missing")
            .value("target_node_id", "unrelated")
            .value("edge_json", "invalid")
            .execute(db),
    )
}

fn insert_candidate_graph_edge(
    db: &dyn Database,
    candidate_edge: &bcode_workflow::EdgeDefinition,
) -> Result<switchy_database::Row, switchy_database::DatabaseError> {
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0_i64)
            .value("revision", 2_i64)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value(
                "edge_json",
                DatabaseValue::from(serde_json::to_string(candidate_edge).expect("edge payload")),
            )
            .execute(db),
    )
}

fn insert_retry_candidate_node(
    db: &dyn Database,
    candidate: &bcode_workflow::NodeDefinition,
) -> Result<switchy_database::Row, switchy_database::DatabaseError> {
    block_on(
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("revision", 2_i64)
            .value(
                "node_json",
                DatabaseValue::from(serde_json::to_string(candidate).expect("retry payload")),
            )
            .value("is_entry", 0_i64)
            .value("is_exit", 0_i64)
            .execute(db),
    )
}

fn make_two_run_schema_15_fixture(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    block_on(async {
        db.drop_table("workflow_run_graph_edges")
            .execute(db)
            .await?;
        db.drop_table("workflow_run_graph_nodes")
            .execute(db)
            .await?;
        db.drop_table("workflow_run_graphs").execute(db).await?;
        Ok::<_, WorkflowStoreError>(())
    })?;
    fixture_historical_activations(db, false)?;
    block_on(async {
        db.exec_update_count(&update("workflow_store_contract").value("schema_version", 15_i64))
            .await?;
        db.exec_update_count(
            &update("workflow_definitions")
                .value("checksum_sha256", "damaged")
                .where_eq("definition_id", "second"),
        )
        .await?;
        Ok::<_, WorkflowStoreError>(())
    })
}

fn schema_16_failed_backup_path(database_path: &std::path::Path) -> std::path::PathBuf {
    database_path
        .parent()
        .expect("store directory")
        .join(MIGRATION_BACKUP_DIRECTORY)
        .join("workflow-78.db")
}

fn remove_incoming_source_nodes(db: &dyn Database) -> Result<u64, switchy_database::DatabaseError> {
    block_on(db.exec_delete_count(&delete("workflow_run_graph_nodes").where_eq("run_id", "run-1")))
}

fn make_ambiguous_execution_schema_15(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    block_on(async {
        db.drop_table("workflow_run_graph_edges")
            .execute(db)
            .await?;
        db.drop_table("workflow_run_graph_nodes")
            .execute(db)
            .await?;
        db.drop_table("workflow_run_graphs").execute(db).await?;
        Ok::<_, WorkflowStoreError>(())
    })?;
    fixture_historical_activations(db, false)?;
    block_on(async {
        db.exec_update_count(&update("workflow_store_contract").value("schema_version", 15_i64))
            .await?;
        Ok::<_, WorkflowStoreError>(())
    })
}

fn publish_retry_graph_header(db: &dyn Database) -> Result<u64, switchy_database::DatabaseError> {
    block_on(
        db.exec_update_count(
            &update("workflow_run_graphs")
                .value("revision", 2_i64)
                .where_eq("run_id", "run-1"),
        ),
    )
}
