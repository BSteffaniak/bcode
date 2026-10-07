mod fixtures;
use super::super::{
    attempts, graph,
    tests::{database, run},
};
use super::*;

struct Fixture {
    db: Box<dyn Database>,
    owner: WorkflowExecutionAuthority,
    caller: WorkflowExecutionSessionLink,
    source: WorkflowExecutionSessionLink,
}

async fn dispatch(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run: &crate::NewWorkflowRun,
    node: &str,
) -> WorkflowExecutionSessionLink {
    let activation = graph::activation_id(db, &run.run_id, node)
        .await
        .unwrap()
        .unwrap();
    let prepared = crate::PreparedAttempt {
        run_id: run.run_id.clone(),
        node_id: node.into(),
        activation_id: activation.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let identity = attempts::prepare(db, owner, &prepared).await.unwrap();
    let link = WorkflowExecutionSessionLink::new(
        run.run_id.clone(),
        node.into(),
        activation,
        1,
        format!("session-{node}"),
        run.workspace_snapshot.clone(),
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
            node_id: link.node_id.clone(),
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

async fn fixture() -> Fixture {
    let db = database().await;
    super::super::install(&*db).await.unwrap();
    let mut definition = bcode_workflow::WorkflowBuilder::new(
        "contributors",
        bcode_workflow::Step::task("source", |v: u32, _| async move { Ok(v) }).then(
            bcode_workflow::Step::task("evaluate", |v: u32, _| async move { Ok(v) }),
        ),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let mut sibling = definition.nodes["source"].clone();
    sibling.id = "sibling".into();
    definition.nodes.insert("sibling".into(), sibling);
    definition.entries.push("sibling".into());
    let mut request = run("contributors", false);
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("contributors", &definition)
            .unwrap();
    request.definition_id = identity.definition_id;
    request.definition_version = identity.definition_version;
    super::super::admit(&*db, &request, &definition)
        .await
        .unwrap();
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "token".into(),
    };
    attempts::claim_authority(&*db, &request.run_id, &owner)
        .await
        .unwrap();
    // Equal-valued sibling output cannot acquire contributor authority.
    for node in ["sibling", "source"] {
        let link = dispatch(&*db, &owner, &request, node).await;
        let output = crate::ValidatedOutput {
            output_id: format!("output-{node}"),
            run_id: request.run_id.clone(),
            node_id: node.into(),
            activation_id: link.activation_id.clone(),
            schema_id: definition.nodes[node].output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!(7),
            artifact_reference: None,
            created_at_ms: 3,
        };
        graph::completion::complete(
            &*db,
            &owner,
            1,
            &crate::dispatch_identity(&link.run_id, node, &link.activation_id, 1),
            &output,
        )
        .await
        .unwrap();
    }
    let source_activation = graph::activation_id(&*db, &request.run_id, "source")
        .await
        .unwrap()
        .unwrap();
    let source = attempts::owner_boundary::execution_session_link(
        &*db,
        &request.run_id,
        "source",
        &source_activation,
        1,
    )
    .await
    .unwrap()
    .unwrap();
    let caller = dispatch(&*db, &owner, &request, "evaluate").await;
    Fixture {
        db,
        owner,
        caller,
        source,
    }
}

#[tokio::test]
async fn exact_edge_selects_source_not_equal_valued_sibling() {
    let f = fixture().await;
    let Observation::Available(reference) = direct_contributor(&*f.db, &f.owner, &f.caller, 1)
        .await
        .unwrap()
    else {
        panic!("expected exact contributor");
    };
    assert_eq!(reference.execution, f.source);
    assert_eq!(reference.output_id, "output-source");
}
use DirectContributorObservation as Observation;

#[tokio::test]
async fn forged_caller_authority_and_revision_do_not_grant_access() {
    let f = fixture().await;
    let mut caller = f.caller.clone();
    caller.session_id = "session-sibling".into();
    assert!(
        direct_contributor(&*f.db, &f.owner, &caller, 1)
            .await
            .is_err()
    );
    caller = f.caller.clone();
    caller.attempt = 2;
    assert!(
        direct_contributor(&*f.db, &f.owner, &caller, 1)
            .await
            .is_err()
    );
    let mut owner = f.owner.clone();
    owner.fencing_token = "foreign".into();
    assert!(
        direct_contributor(&*f.db, &owner, &f.caller, 1)
            .await
            .is_err()
    );
    assert_eq!(
        direct_contributor(&*f.db, &f.owner, &f.caller, 2)
            .await
            .unwrap(),
        Observation::ProvenanceUnavailable
    );
}

#[tokio::test]
async fn missing_exact_link_does_not_borrow_sibling_link() {
    let f = fixture().await;
    f.db.delete("workflow_execution_sessions_v2")
        .where_eq(
            "dispatch_identity",
            crate::dispatch_identity(
                &f.source.run_id,
                &f.source.node_id,
                &f.source.activation_id,
                f.source.attempt,
            ),
        )
        .execute(&*f.db)
        .await
        .unwrap();
    assert_eq!(
        direct_contributor(&*f.db, &f.owner, &f.caller, 1)
            .await
            .unwrap(),
        Observation::ProvenanceUnavailable
    );
}

#[tokio::test]
async fn retired_source_is_not_borrowed_from_history() {
    let f = fixture().await;
    f.db.update("workflow_activations_v2")
        .value("current", false)
        .where_eq("identity", f.source.activation_id.as_str())
        .execute(&*f.db)
        .await
        .unwrap();
    assert_eq!(
        direct_contributor(&*f.db, &f.owner, &f.caller, 1)
            .await
            .unwrap(),
        Observation::ProvenanceUnavailable
    );
}

#[tokio::test]
async fn ambiguous_incoming_edges_fail_closed_without_reading_unselected_tail() {
    let f = fixture().await;
    fixtures::ambiguous_edges(&*f.db, &f.caller).await;
    assert_eq!(
        direct_contributor(&*f.db, &f.owner, &f.caller, 1)
            .await
            .unwrap(),
        Observation::ProvenanceUnavailable
    );
}

#[tokio::test]
async fn malformed_latest_attempt_cannot_borrow_previous_success() {
    let f = fixture().await;
    let identity = crate::dispatch_identity(
        &f.source.run_id,
        &f.source.node_id,
        &f.source.activation_id,
        f.source.attempt,
    );
    f.db.update("workflow_attempts_v2")
        .value("attempt_number", 2_i64)
        .where_eq("dispatch_identity", identity)
        .execute(&*f.db)
        .await
        .unwrap();
    assert!(
        direct_contributor(&*f.db, &f.owner, &f.caller, 1)
            .await
            .is_err()
    );
}
