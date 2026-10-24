use super::*;

#[test]
fn workflow_operation_failures_distinguish_busy_and_unknown_without_private_payloads() {
    for (error, expected_code, expected_message) in [
        (
            ServerError::WorkflowStorageBusy,
            "workflow_storage_busy",
            "workflow edit storage is busy; durable work may be unchanged or pending; inspect retained state before further action",
        ),
        (
            ServerError::WorkflowStorageUnavailable(
                "SECRET_FIXTURE_MARKER /private/storage".into(),
            ),
            "workflow_storage_unavailable",
            "workflow edit storage is unavailable; operation outcome unknown",
        ),
        (
            ServerError::WorkflowCapabilityUnavailable(
                "SECRET_FIXTURE_MARKER workflow_storage_busy".into(),
            ),
            "workflow_operation_outcome_unknown",
            "workflow edit failed with an unclassified application error; operation outcome unknown",
        ),
    ] {
        let bcode_tool::ToolInvocationServiceResolution::Failed { code, message } =
            crate::workflow_operation_failure(&error)
        else {
            panic!("failure must not become success");
        };
        assert_eq!(code, expected_code);
        assert_eq!(message, expected_message);
        assert!(!message.contains("SECRET_FIXTURE_MARKER"));
        assert!(!message.contains("/private/storage"));
    }
}

#[tokio::test]
async fn normal_tool_cancel_acceptance_waits_for_receipt_then_commits_exact_candidate() {
    let (_root, _sessions, state, _) = typed_provider_fixture_with_delegation(true).await;
    let mut cleanup = FixtureCleanup::new(&state);
    let _gate = super::fanout::ReleaseGate(
        state.selected_provider_context.settings["delegation_fixture_gate"].clone(),
    );
    let mut definition = parallel_provider_definition();
    definition.nodes.remove("join");
    definition.edges.clear();
    definition.exits = vec!["left".into()];
    definition.output = definition.nodes["left"].output.clone();
    super::fanout::start_public(
        &state,
        "cancel-edit",
        definition,
        serde_json::json!(1),
        None,
    )
    .await;
    let sessions = state.sessions.all_session_summaries().await;
    let source = sessions
        .iter()
        .find(|s| {
            s.execution.as_ref().is_some_and(|e| {
                e.provenance.run_id == "cancel-edit" && e.provenance.node_id == "left"
            })
        })
        .expect("source");
    let source_activation = source
        .execution
        .as_ref()
        .expect("execution")
        .provenance
        .activation_id
        .clone()
        .expect("activation");
    let db = open(&crate::workflow_store_path(&state), false).expect("db");
    let target = representation::graph::activation_id(&db, "cancel-edit", "right")
        .await
        .expect("id")
        .expect("target");
    let batch = cancellation_batch(source_activation, target.clone());
    let staging_call = bcode_model::ToolCall {
        id: "stage-cancel".into(),
        name: "workflow.stage_run_graph_edit".into(),
        arguments: serde_json::json!({"edit_json":serde_json::to_string(&batch).expect("batch")}),
    };
    let staged = Box::pin(invoke_delegation_tool(&state, source.id, &staging_call)).await;
    assert!(!staged.is_error, "{}", staged.output);
    let accept = bcode_model::ToolCall {
        id: "accept-cancel".into(),
        name: "workflow.accept_run_graph_publication".into(),
        arguments: staging_call.arguments.clone(),
    };
    state.start_workflow_driver().await;
    let pending = Box::pin(invoke_delegation_tool(&state, source.id, &accept)).await;
    assert!(!pending.is_error, "{}", pending.output);
    assert_eq!(
        representation::graph_pages::revision(&db, "cancel-edit")
            .await
            .expect("revision"),
        1
    );
    state.start_workflow_driver().await;
    let finished = tokio::time::timeout(crate::workflow_test_support::WATCHDOG, async {
        let mut progress = crate::workflow_test_support::Progress::subscribe(&state);
        let dispatch = bcode_workflow_store::dispatch_identity("cancel-edit", "right", &target, 1);
        loop {
            if representation::attempts::owner_boundary::terminal_outcome(
                &db,
                "cancel-edit",
                &dispatch,
            )
            .await
            .expect("outcome")
                == Some(representation::attempts::TerminalOutcome::Cancelled)
            {
                break;
            }
            progress.changed().await;
        }
    })
    .await;
    finished.expect("receipt acknowledged cancellation");
    let committed = Box::pin(invoke_delegation_tool(&state, source.id, &accept)).await;
    assert!(!committed.is_error, "{}", committed.output);
    assert_eq!(
        representation::graph_pages::revision(&db, "cancel-edit")
            .await
            .expect("revision"),
        2
    );
    // The fixture's original run is still gated; settle it before draining callbacks.
    crate::workflow_operations::cancel_run(&state, "new")
        .await
        .expect("cancel original fixture run");
    cleanup.finish().await;
    drop(cleanup);
}

fn cancellation_batch(
    source_activation: String,
    target: String,
) -> bcode_workflow::WorkflowRunGraphEditBatch {
    bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "cancel-edit".into(),
        expected_revision: 1,
        mutation_id: "remove-right".into(),
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::RemoveNode {
            node_id: "right".into(),
        }],
        reconciliation: vec![
            bcode_workflow::WorkflowRunGraphReconciliation::Retain {
                activation_id: source_activation,
            },
            bcode_workflow::WorkflowRunGraphReconciliation::Cancel {
                activation_id: target,
            },
        ],
    }
}
