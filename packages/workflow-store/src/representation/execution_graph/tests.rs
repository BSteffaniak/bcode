mod cancellation;
use super::super::{
    attempts, graph, graph_revision,
    tests::{database, run},
};
use super::*;
#[tokio::test]
async fn execution_candidate_is_staged_exactly_and_fenced_at_publication() {
    let (db, run, owner, link, definition, dispatch) = fixture().await;
    let activation = link.activation_id.clone();
    let request = bcode_workflow::WorkflowExecutionContextRequest {
        source_local: true,
        output_id: None,
        after_output_id: None,
        expected_revision: Some(1),
        after_node_id: None,
        after_edge_id: None,
        limit: 1,
    };
    let (page, _, _) = execution_context_graph_page(&*db, &link, &owner, &request)
        .await
        .unwrap();
    assert_eq!(page.edges[0].edge_id, 1);
    assert_eq!(page.next_edge_id, Some(2));
    let mut end = definition.nodes["end"].clone();
    end.name = "changed pending".into();
    let batch = WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        expected_revision: 1,
        mutation_id: "candidate".into(),
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::ReplaceNode {
            node: end,
            entry: false,
            exit: true,
        }],
        reconciliation: vec![WorkflowRunGraphReconciliation::Retain {
            activation_id: activation,
        }],
    };
    assert!(
        publication_from_execution(&*db, &batch, &owner, &link)
            .await
            .is_err()
    );
    assert!(
        stage_run_graph_edit_from_execution(&*db, &batch, &owner, &link, 3)
            .await
            .unwrap()
    );
    assert!(
        !stage_run_graph_edit_from_execution(&*db, &batch, &owner, &link, 4)
            .await
            .unwrap()
    );
    assert_eq!(
        graph_revision::current(&*db, &run.run_id).await.unwrap().0,
        1
    );
    assert_eq!(
        staged(&*db, &run.run_id, "candidate", &owner)
            .await
            .unwrap(),
        Some(batch.clone())
    );
    let mut changed = batch.clone();
    changed.edits.clear();
    assert!(
        publication_from_execution(&*db, &changed, &owner, &link)
            .await
            .is_err()
    );
    assert_eq!(
        publication_from_execution(&*db, &batch, &owner, &link)
            .await
            .unwrap(),
        2
    );
    assert!(
        execution_context_graph_page(&*db, &link, &owner, &request)
            .await
            .is_err()
    );
    attempts::settle(
        &*db,
        &owner,
        &run.run_id,
        &dispatch,
        attempts::TerminalOutcome::Succeeded,
        5,
    )
    .await
    .unwrap();
    assert!(
        publication_from_execution(&*db, &batch, &owner, &link)
            .await
            .is_err()
    );
}

async fn fixture() -> (
    Box<dyn Database>,
    crate::NewWorkflowRun,
    WorkflowExecutionAuthority,
    WorkflowExecutionSessionLink,
    bcode_workflow::WorkflowDefinition,
    String,
) {
    fixture_with_sibling(false).await
}

async fn fixture_with_sibling(
    sibling: bool,
) -> (
    Box<dyn Database>,
    crate::NewWorkflowRun,
    WorkflowExecutionAuthority,
    WorkflowExecutionSessionLink,
    bcode_workflow::WorkflowDefinition,
    String,
) {
    let db = database().await;
    super::super::install(&*db).await.unwrap();
    let mut definition = bcode_workflow::WorkflowBuilder::new(
        "execution",
        bcode_workflow::Step::task("source", |v: u32, _| async move { Ok(v) }).then(
            bcode_workflow::Step::task("end", |v: u32, _| async move { Ok(v) }),
        ),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    if sibling {
        let mut node = definition.nodes["source"].clone();
        node.id = "sibling".into();
        definition.nodes.insert("sibling".into(), node);
        definition.entries.push("sibling".into());
    }
    let mut run = run("execution", false);
    run.parent_session_id = Some("session".into());
    let id = bcode_workflow::WorkflowDefinitionIdentity::for_definition("execution", &definition)
        .unwrap();
    run.definition_id = id.definition_id;
    run.definition_version = id.definition_version;
    super::super::admit(&*db, &run, &definition).await.unwrap();
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "token".into(),
    };
    attempts::claim_authority(&*db, &run.run_id, &owner)
        .await
        .unwrap();
    let activation = graph::activation_id(&*db, &run.run_id, "source")
        .await
        .unwrap()
        .unwrap();
    let prepared = crate::PreparedAttempt {
        run_id: run.run_id.clone(),
        node_id: "source".into(),
        activation_id: activation.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let dispatch = attempts::prepare(&*db, &owner, &prepared).await.unwrap();
    let link = WorkflowExecutionSessionLink::new(
        run.run_id.clone(),
        "source".into(),
        activation.clone(),
        1,
        "execution-session".into(),
        run.workspace_snapshot.clone(),
        2,
    );
    attempts::owner_boundary::persist_execution_session_link(&*db, &owner, &link)
        .await
        .unwrap();
    attempts::attach_receipt(
        &*db,
        &owner,
        &crate::DispatchReceipt {
            run_id: run.run_id.clone(),
            node_id: "source".into(),
            activation_id: activation.clone(),
            attempt: 1,
            dispatch_identity: dispatch.clone(),
            receipt: serde_json::json!({}),
            admitted_at_ms: 2,
        },
    )
    .await
    .unwrap();
    (db, run, owner, link, definition, dispatch)
}
