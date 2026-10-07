use super::super::super::{
    attempts, execution_graph, graph,
    tests::{database, run},
};
use super::*;
use bcode_workflow::{WorkflowRunGraphEdit as Edit, WorkflowRunGraphReconciliation as Reconcile};

async fn dispatch(
    db: &dyn Database,
    owner: &crate::WorkflowExecutionAuthority,
    request: &crate::NewWorkflowRun,
    node: &str,
) -> WorkflowExecutionSessionLink {
    let activation = graph::activation_id(db, &request.run_id, node)
        .await
        .unwrap()
        .unwrap();
    let prepared = crate::PreparedAttempt {
        run_id: request.run_id.clone(),
        node_id: node.into(),
        activation_id: activation.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let identity = attempts::prepare(db, owner, &prepared).await.unwrap();
    let link = WorkflowExecutionSessionLink::new(
        request.run_id.clone(),
        node.into(),
        activation,
        1,
        format!("session-{node}"),
        request.workspace_snapshot.clone(),
        2,
    );
    attempts::owner_boundary::persist_execution_session_link(db, owner, &link)
        .await
        .unwrap();
    attempts::attach_receipt(
        db,
        owner,
        &crate::DispatchReceipt {
            run_id: link.run_id.clone(),
            node_id: node.into(),
            activation_id: link.activation_id.clone(),
            attempt: 1,
            dispatch_identity: identity,
            receipt: serde_json::json!({}),
            admitted_at_ms: 2,
        },
    )
    .await
    .unwrap();
    link
}

async fn complete(
    db: &dyn Database,
    owner: &crate::WorkflowExecutionAuthority,
    link: &WorkflowExecutionSessionLink,
    revision: u64,
    schema: &bcode_workflow::ValueSchema,
    value: serde_json::Value,
) {
    graph::completion::complete(
        db,
        owner,
        revision,
        &crate::dispatch_identity(&link.run_id, &link.node_id, &link.activation_id, 1),
        &crate::ValidatedOutput {
            output_id: format!("output-{}", link.node_id),
            run_id: link.run_id.clone(),
            node_id: link.node_id.clone(),
            activation_id: link.activation_id.clone(),
            schema_id: schema.type_name.clone(),
            schema_version: 1,
            value,
            artifact_reference: None,
            created_at_ms: 3,
        },
    )
    .await
    .unwrap();
}

fn delegation_batch(
    run_id: &str,
    source: &WorkflowExecutionSessionLink,
    template: &bcode_workflow::NodeDefinition,
) -> WorkflowRunGraphEditBatch {
    let mut edits = Vec::new();
    for id in ["worker", "integrate"] {
        let mut node = template.clone();
        node.id = id.into();
        edits.push(Edit::AddNode {
            node,
            entry: false,
            exit: false,
        });
    }
    let edge = |from: &str, to: &str| bcode_workflow::EdgeDefinition {
        from: from.into(),
        to: to.into(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    edits.push(Edit::AddEdge {
        edge_id: 2,
        edge: edge("source", "worker"),
    });
    edits.push(Edit::AddEdge {
        edge_id: 3,
        edge: edge("worker", "integrate"),
    });
    let mut reconnect = edge("integrate", "evaluate");
    reconnect.transform = Some(bcode_workflow::WorkflowTransform {
        version: bcode_workflow::WORKFLOW_TRANSFORM_VERSION,
        expression: bcode_workflow::WorkflowTransformExpression::Input {
            source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT.into(),
            path: "source".into(),
        },
        output: template.output.clone(),
    });
    edits.push(Edit::ReplaceEdge {
        edge_id: 1,
        edge: reconnect,
    });
    WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run_id.into(),
        expected_revision: 1,
        mutation_id: "delegate".into(),
        edits,
        reconciliation: vec![Reconcile::RetainWithBindings {
            activation_id: source.activation_id.clone(),
            edge_ids: vec![2],
        }],
    }
}

async fn fixture() -> (
    Box<dyn Database>,
    crate::WorkflowExecutionAuthority,
    WorkflowExecutionSessionLink,
) {
    let db = database().await;
    super::super::super::install(&*db).await.unwrap();
    let mut definition = bcode_workflow::WorkflowBuilder::new(
        "generated",
        bcode_workflow::Step::task(
            "source",
            |value: serde_json::Value, _| async move { Ok(value) },
        )
        .then(bcode_workflow::Step::task(
            "evaluate",
            |value: serde_json::Value, _| async move { Ok(value) },
        )),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let mut sibling = definition.nodes["source"].clone();
    sibling.id = "sibling".into();
    definition.nodes.insert("sibling".into(), sibling);
    definition.entries.push("sibling".into());
    let mut request = run("generated", false);
    request.input = Some(serde_json::json!({"goal":1}));
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("generated", &definition)
            .unwrap();
    request.definition_id = identity.definition_id;
    request.definition_version = identity.definition_version;
    super::super::super::admit(&*db, &request, &definition)
        .await
        .unwrap();
    let owner = crate::WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    attempts::claim_authority(&*db, &request.run_id, &owner)
        .await
        .unwrap();
    let source = dispatch(&*db, &owner, &request, "source").await;
    let template = definition.nodes["source"].clone();
    let batch = delegation_batch(&request.run_id, &source, &template);
    execution_graph::stage_run_graph_edit_from_execution(&*db, &batch, &owner, &source, 2)
        .await
        .unwrap();
    execution_graph::publication_from_execution(&*db, &batch, &owner, &source)
        .await
        .unwrap();
    complete(
        &*db,
        &owner,
        &source,
        2,
        &template.output,
        serde_json::json!({"goal":1}),
    )
    .await;
    let worker = dispatch(&*db, &owner, &request, "worker").await;
    complete(
        &*db,
        &owner,
        &worker,
        2,
        &template.output,
        serde_json::json!({"source":{"goal":1},"worker":"ok"}),
    )
    .await;
    let integration = dispatch(&*db, &owner, &request, "integrate").await;
    complete(
        &*db,
        &owner,
        &integration,
        2,
        &template.output,
        serde_json::json!({"source":{"goal":1},"worker":"ok"}),
    )
    .await;
    let caller = dispatch(&*db, &owner, &request, "evaluate").await;
    (db, owner, caller)
}

#[tokio::test]
async fn committed_source_worker_integration_lineage_is_bounded_and_excludes_sibling() {
    let (db, owner, caller) = fixture().await;
    let super::super::GeneratedContributorObservation::Available(page) =
        super::super::generated_contributors(&*db, &owner, &caller, 2, None, 1)
            .await
            .unwrap()
    else {
        panic!("proved lineage");
    };
    assert_eq!(page.contributors.len(), 1);
    assert_eq!(page.contributors[0].execution.node_id, "integrate");
    let cursor = page.next_node_id.unwrap();
    let super::super::GeneratedContributorObservation::Available(page) =
        super::super::generated_contributors(&*db, &owner, &caller, 2, Some(&cursor), 100)
            .await
            .unwrap()
    else {
        panic!("proved next page");
    };
    assert_eq!(
        page.contributors
            .iter()
            .map(|reference| reference.execution.node_id.as_str())
            .collect::<Vec<_>>(),
        vec!["source", "worker"]
    );
    assert!(page.next_node_id.is_none());
}

#[tokio::test]
async fn cancelled_run_and_unrelated_cursor_do_not_authorize_lineage() {
    let (db, owner, caller) = fixture().await;
    assert!(
        super::super::generated_contributors(&*db, &owner, &caller, 2, Some("sibling"), 1)
            .await
            .is_err()
    );
    super::super::super::safety::request_cancellation(&*db, &caller.run_id, &owner, 5)
        .await
        .unwrap();
    assert!(
        super::super::generated_contributors(&*db, &owner, &caller, 2, None, 1)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn stale_owner_revision_forged_session_and_prior_generation_fail_closed() {
    let (db, owner, caller) = fixture().await;
    let mut foreign = owner.clone();
    foreign.fencing_token = "foreign".into();
    assert!(
        super::super::generated_contributors(&*db, &foreign, &caller, 2, None, 1)
            .await
            .is_err()
    );
    let mut forged = caller.clone();
    forged.session_id = "session-sibling".into();
    assert!(
        super::super::generated_contributors(&*db, &owner, &forged, 2, None, 1)
            .await
            .is_err()
    );
    assert_eq!(
        super::super::generated_contributors(&*db, &owner, &caller, 3, None, 1)
            .await
            .unwrap(),
        super::super::GeneratedContributorObservation::ProvenanceUnavailable
    );
    db.update("workflow_activations_v2")
        .value("generation", 2_i64)
        .where_eq("run_id", caller.run_id.as_str())
        .where_eq("node_id", "worker")
        .execute(&*db)
        .await
        .unwrap();
    assert_eq!(
        super::super::generated_contributors(&*db, &owner, &caller, 2, None, 1)
            .await
            .unwrap(),
        super::super::GeneratedContributorObservation::ProvenanceUnavailable
    );
}
