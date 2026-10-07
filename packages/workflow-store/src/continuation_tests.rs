use super::*;
use crate::database::block_on;
use crate::{ValidatedOutput, WorkflowRunBinding, append_event_in};
use switchy_database::query::{FilterableQuery, update};
use switchy_database::schema::{
    SchemaComparison as C, SchemaExpression as E, create_trigger, drop_index, drop_table,
};
use switchy_database::{Database, DatabaseValue, TransactionMode};

#[test]
fn admission_allowance_evidence_is_bounded_and_fails_closed() {
    let (_temp, store, run, _) = fixture();
    assert_eq!(
        admission_execution_cap(&store.connection, &run.run_id, 10).unwrap(),
        10
    );
    for payload in [
        r#"{"version":2,"previous_cap":1,"target_cap":2}"#.to_owned(),
        r#"{"version":1,"previous_cap":0,"target_cap":2}"#.to_owned(),
        r#"{"version":1,"previous_cap":2,"target_cap":1}"#.to_owned(),
        r#"{"version":1,"previous_cap":1,"target_cap":11}"#.to_owned(),
        "invalid".into(),
        "x".repeat(1025),
    ] {
        let tx = database::begin(&store.connection, TransactionMode::Deferred).unwrap();
        let db: &dyn Database = tx.as_ref();
        append_event_in(
            db,
            &run.run_id,
            "execution_allowance_increased",
            &payload,
            20,
        )
        .unwrap();
        assert!(admission_execution_cap(db, &run.run_id, 10).is_err());
        block_on(tx.rollback()).unwrap();
    }
}

fn fixture() -> (
    tempfile::TempDir,
    WorkflowStore,
    NewWorkflowRun,
    WorkflowDefinition,
) {
    let temp = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "repeat",
        bcode_workflow::Step::map("body", |value: serde_json::Value| Ok(value)).repeat_while(
            "repeat",
            bcode_workflow::field::<serde_json::Value>("condition_met").eq(false),
            2,
        ),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    store.persist_definition("example", 1, &definition).unwrap();
    let run = NewWorkflowRun {
        run_id: "original".into(),
        definition_id: "example".into(),
        definition_version: 1,
        workspace_snapshot: "workspace".into(),
        parent_session_id: Some("00000000-0000-4000-8000-000000000001".into()),
        parent_session_generation: None,
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "repeat".into(),
            scope_key: "session".into(),
            display_label: None,
            single_active: true,
        }),
        authored_provenance: None,
        input: Some(serde_json::json!({"condition_met": false,"iteration":1})),
        execution_authority: Some(WorkflowExecutionAuthority {
            target_artifact_id: "artifact".into(),
            daemon_instance_id: "daemon".into(),
            generation: 1,
            fencing_token: "fence".into(),
        }),
        created_at_ms: 1,
        authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
            version: 1,
            provider_id: "policy".into(),
            profile_id: "build".into(),
            policy_digest_sha256: "a".repeat(64),
        },
        authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
        limits: WorkflowRunLimits {
            cycle_cap: 2,
            ..WorkflowRunLimits::default()
        },
    };
    store.create_run(&run).unwrap();
    (temp, store, run, definition)
}

fn execute(
    store: &mut WorkflowStore,
    run: &NewWorkflowRun,
    definition: &WorkflowDefinition,
    count: u32,
    success: bool,
) {
    for index in 0..count {
        let activation = store
            .pending_activations(100)
            .unwrap()
            .into_iter()
            .find(|activation| activation.run_id == run.run_id && activation.node_id == "body")
            .unwrap();
        let mut value = activation.input.unwrap();
        value["condition_met"] = serde_json::json!(success && index + 1 == count);
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: format!("{}-{index}", run.run_id),
                run_id: run.run_id.clone(),
                node_id: "body".into(),
                activation_id: activation.activation_id,
                schema_id: definition.nodes["body"].output.type_name.clone(),
                schema_version: 1,
                value,
                artifact_reference: None,
                created_at_ms: 10 + u64::from(index),
            })
            .unwrap();
        store
            .settle_pending_control_nodes(&run.run_id, 10, 10 + u64::from(index))
            .unwrap();
    }
}

fn successor(
    store: &mut WorkflowStore,
    original: &NewWorkflowRun,
) -> (
    WorkflowContinuationRequest,
    NewWorkflowRun,
    WorkflowDefinition,
) {
    let source = store.continuation_source(&original.run_id).unwrap();
    let mut definition = source.definition;
    definition.nodes.get_mut("repeat").unwrap().configuration["max_iterations"] =
        serde_json::json!(3);
    for edge in &mut definition.edges {
        if let bcode_workflow::EdgeKind::Back { max_iterations, .. } = &mut edge.kind {
            *max_iterations = 3;
        }
    }
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("repeat", &definition).unwrap();
    store
        .persist_definition(
            &identity.definition_id,
            identity.definition_version,
            &definition,
        )
        .unwrap();
    let mut run = original.clone();
    run.run_id = "successor".into();
    run.definition_id.clone_from(&identity.definition_id);
    run.definition_version = identity.definition_version;
    run.created_at_ms = 100;
    run.limits.cycle_cap = 3;
    run.input = Some(source.input);
    let request = WorkflowContinuationRequest {
        source_run_id: original.run_id.clone(),
        expected_graph_revision: source.graph_revision,
        expected_output_checksum: source.output_checksum,
        additional_iterations: 3,
        successor: bcode_workflow::WorkflowStartRequest {
            identity,
            definition: definition.clone(),
            run_id: Some(run.run_id.clone()),
            workspace_snapshot: Some(run.workspace_snapshot.clone()),
            parent_session_id: run.parent_session_id.as_ref().unwrap().parse().unwrap(),
            input: run.input.clone().unwrap(),
            binding: run.binding.clone().unwrap(),
            limits: run.limits.clone(),
        },
    };
    (request, run, definition)
}

#[test]
fn continuation_preserves_history_is_idempotent_and_survives_restart() {
    let (temp, mut store, original, definition) = fixture();
    assert!(matches!(
        store.continuation_source(&original.run_id),
        Err(WorkflowStoreError::ContinuationIneligible)
    ));
    execute(&mut store, &original, &definition, 2, false);
    let before = store.run_summary(&original.run_id).unwrap();
    let events = store.event_history(&original.run_id, None, 100).unwrap();
    let (request, run, definition) = successor(&mut store, &original);
    let authority = original.execution_authority.as_ref().unwrap();
    let mut stale = authority.clone();
    stale.generation += 1;
    assert!(store.continue_run_owned(&request, &run, &stale).is_err());
    assert!(store.run_summary(&run.run_id).unwrap().is_none());
    assert!(store.continue_run_owned(&request, &run, authority).unwrap());
    assert!(!store.continue_run_owned(&request, &run, authority).unwrap());
    assert_eq!(store.run_summary(&original.run_id).unwrap(), before);
    assert_eq!(
        store.event_history(&original.run_id, None, 100).unwrap(),
        events
    );
    let mut conflict = request.clone();
    conflict.additional_iterations = 4;
    assert!(store.continuation_retry(&conflict).is_err());
    conflict = request.clone();
    conflict.successor.run_id = Some("competitor".into());
    assert!(store.continuation_retry(&conflict).is_err());
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).unwrap();
    assert!(store.continuation_retry(&request).unwrap().is_some());
    let lineage = store.continuation_lineage(&run.run_id).unwrap().unwrap();
    assert_eq!(lineage.prior_iterations, 2);
    assert_eq!(lineage.document_scope_id, original.run_id);
    execute(&mut store, &run, &definition, 3, false);
    let source = store.continuation_source(&run.run_id).unwrap();
    assert_eq!(source.iterations_completed, 3);
    assert_eq!(source.total_iterations_completed, 5);
    assert_eq!(source.document_scope_id, original.run_id);
}

#[test]
fn short_continuation_bounds_concurrency_without_expanding_authority() {
    let (_temp, mut store, original, definition) = fixture();
    execute(&mut store, &original, &definition, 2, false);
    let (mut request, mut run, _) = successor(&mut store, &original);
    assert_eq!(original.limits.concurrency_cap, None);
    run.limits.node_execution_cap = 3;
    run.limits.concurrency_cap = None;
    request.successor.limits = run.limits.clone();
    let authority = original.execution_authority.as_ref().unwrap();
    assert!(store.continue_run_owned(&request, &run, authority).unwrap());
    assert_eq!(store.run_limits(&run.run_id).unwrap(), Some(run.limits));
}

#[test]
fn continuation_rejects_increased_concurrency() {
    let (_temp, mut store, original, definition) = fixture();
    execute(&mut store, &original, &definition, 2, false);
    let (mut request, mut run, _) = successor(&mut store, &original);
    run.limits.concurrency_cap = Some(1);
    request.successor.limits = run.limits.clone();
    let before = store.run_summary(&original.run_id).unwrap();
    assert!(
        store
            .continue_run_owned(
                &request,
                &run,
                original.execution_authority.as_ref().unwrap()
            )
            .is_err()
    );
    assert!(store.run_summary(&run.run_id).unwrap().is_none());
    assert_eq!(store.run_summary(&original.run_id).unwrap(), before);
}

#[test]
fn continuation_stops_early_and_rejects_success() {
    for count in [1, 3] {
        let (_temp, mut store, original, definition) = fixture();
        execute(&mut store, &original, &definition, 2, false);
        let (request, run, definition) = successor(&mut store, &original);
        store
            .continue_run_owned(
                &request,
                &run,
                original.execution_authority.as_ref().unwrap(),
            )
            .unwrap();
        execute(&mut store, &run, &definition, count, true);
        assert_eq!(
            store.run_summary(&run.run_id).unwrap().unwrap().status,
            RunStatus::Completed
        );
        assert!(store.continuation_source(&run.run_id).is_err());
        assert!(store.pending_activations(100).unwrap().is_empty());
    }
}

#[test]
fn continuation_failed_admission_rolls_back_and_duplicate_ignores_later_state() {
    let (_temp, mut store, original, definition) = fixture();
    execute(&mut store, &original, &definition, 2, false);
    let (request, run, definition) = successor(&mut store, &original);
    // Fail after all source validation, while inserting the successor. The enclosing
    // transaction must publish neither a run nor lineage, so the exact request is retryable.
    block_on(
        create_trigger("reject_test_successor", "workflow_runs")
            .before_insert()
            .when(E::new_column("run_id").compare(C::Equal, E::Text("successor".into())))
            .raise_abort("test admission failure")
            .execute(&store.connection),
    )
    .unwrap();
    assert!(
        store
            .continue_run_owned(
                &request,
                &run,
                original.execution_authority.as_ref().unwrap()
            )
            .is_err()
    );
    assert!(store.continuation_retry(&request).unwrap().is_none());
    assert!(store.run_summary(&run.run_id).unwrap().is_none());
    block_on(
        store
            .connection
            .drop_trigger("reject_test_successor", false),
    )
    .unwrap();
    store
        .continue_run_owned(
            &request,
            &run,
            original.execution_authority.as_ref().unwrap(),
        )
        .unwrap();
    execute(&mut store, &run, &definition, 1, true);
    let before = store.run_summary(&run.run_id).unwrap();
    assert!(
        !store
            .continue_run_owned(
                &request,
                &run,
                original.execution_authority.as_ref().unwrap()
            )
            .unwrap()
    );
    assert_eq!(store.run_summary(&run.run_id).unwrap(), before);
    assert!(store.pending_activations(100).unwrap().is_empty());
}

#[test]
fn continuation_schema_upgrade_preserves_history_and_rejects_damage() {
    let (temp, mut store, original, definition) = fixture();
    execute(&mut store, &original, &definition, 2, false);
    let before = store.run_summary(&original.run_id).unwrap();
    block_on(drop_table("workflow_continuations").execute(&store.connection)).unwrap();
    block_on(
        drop_index("workflow_events_kind_sequence", "workflow_events").execute(&store.connection),
    )
    .unwrap();
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 39)
            .execute(&store.connection),
    )
    .unwrap();
    drop(store);
    assert!(WorkflowStore::open_in_state_dir(temp.path()).is_err());
    let store = WorkflowStore::initialize_in_state_dir(temp.path(), 100).unwrap();
    assert_eq!(store.run_summary(&original.run_id).unwrap(), before);
    assert_eq!(
        store
            .continuation_source(&original.run_id)
            .unwrap()
            .iterations_completed,
        2
    );
    block_on(
        update("workflow_outputs")
            .value("checksum_sha256", "damaged")
            .where_eq("run_id", original.run_id.as_str())
            .where_eq("node_id", "repeat")
            .execute(&store.connection),
    )
    .unwrap();
    assert!(store.continuation_source(&original.run_id).is_err());
}

#[test]
fn continuation_rejects_cancellation_and_unsettled_work() {
    let (_temp, mut store, original, definition) = fixture();
    execute(&mut store, &original, &definition, 2, false);
    block_on(
        update("workflow_runs")
            .value("cancellation_requested_at_ms", 20)
            .where_eq("run_id", original.run_id.as_str())
            .execute(&store.connection),
    )
    .unwrap();
    assert!(store.continuation_source(&original.run_id).is_err());
    block_on(
        update("workflow_runs")
            .value("cancellation_requested_at_ms", DatabaseValue::Null)
            .where_eq("run_id", original.run_id.as_str())
            .execute(&store.connection),
    )
    .unwrap();
    block_on(
        update("workflow_activations")
            .value("status", "pending")
            .where_eq("run_id", original.run_id.as_str())
            .where_eq("node_id", "body")
            .execute(&store.connection),
    )
    .unwrap();
    assert!(store.continuation_source(&original.run_id).is_err());
}

#[test]
fn continuation_rejects_stale_checkpoint_and_changed_association() {
    let (_temp, mut store, original, definition) = fixture();
    execute(&mut store, &original, &definition, 2, false);
    let (mut request, run, _) = successor(&mut store, &original);
    request.expected_graph_revision += 1;
    assert!(
        store
            .continue_run_owned(
                &request,
                &run,
                original.execution_authority.as_ref().unwrap()
            )
            .is_err()
    );
    request.expected_graph_revision -= 1;
    let mut unrelated = original.clone();
    unrelated.run_id = "new-loop".into();
    unrelated.created_at_ms = 99;
    store.create_run(&unrelated).unwrap();
    assert!(
        store
            .continue_run_owned(
                &request,
                &run,
                original.execution_authority.as_ref().unwrap()
            )
            .is_err()
    );
    assert!(store.run_summary(&run.run_id).unwrap().is_none());
}
