use super::*;

#[tokio::test]
async fn normal_tool_cancel_acceptance_waits_for_receipt_then_commits_exact_candidate() {
    let (_root, _sessions, state, _) = typed_provider_fixture_with_delegation(true).await;
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
    let staged = invoke_delegation_tool(&state, source.id, &staging_call).await;
    assert!(!staged.is_error, "{}", staged.output);
    let accept = bcode_model::ToolCall {
        id: "accept-cancel".into(),
        name: "workflow.accept_run_graph_publication".into(),
        arguments: staging_call.arguments.clone(),
    };
    state.start_workflow_driver().await;
    let pending = invoke_delegation_tool(&state, source.id, &accept).await;
    assert!(!pending.is_error, "{}", pending.output);
    assert_eq!(
        representation::graph_pages::revision(&db, "cancel-edit")
            .await
            .expect("revision"),
        1
    );
    state.start_workflow_driver().await;
    let finished = tokio::time::timeout(std::time::Duration::from_secs(10), async {
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
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    finished.expect("receipt acknowledged cancellation");
    let committed = invoke_delegation_tool(&state, source.id, &accept).await;
    stop_fixture_driver(&state).await;
    assert!(!committed.is_error, "{}", committed.output);
    assert_eq!(
        representation::graph_pages::revision(&db, "cancel-edit")
            .await
            .expect("revision"),
        2
    );
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
