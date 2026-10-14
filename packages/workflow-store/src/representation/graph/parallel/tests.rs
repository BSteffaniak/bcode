use super::super::super::attempts;
use super::super::tests::{admitted, success, task};
use super::*;

#[tokio::test]
async fn completed_join_settlement_is_noop_after_run_terminalizes_but_stale_callers_reject() {
    let (db, owner) = admitted(&definition(ParallelFailurePolicy::WaitAll)).await;
    for member in ["left", "right"] {
        success(&*db, &owner, member).await;
        super::super::settle_output(&*db, &owner, "graph", member, 1, &serde_json::json!(9))
            .await
            .unwrap();
    }
    settle(&*db, &owner, "graph", "join", 1).await.unwrap();
    let state = super::super::super::runs::terminalize(&*db, &owner, "graph")
        .await
        .unwrap();
    assert_eq!(state, Some(super::super::super::runs::RunState::Completed));
    let before = super::super::parallel_progress::scan_version(&*db, "graph")
        .await
        .unwrap();
    let value = output(&*db, "graph", "join").await.unwrap();
    assert!(!settle(&*db, &owner, "graph", "join", 1).await.unwrap());
    assert!(settle(&*db, &owner, "graph", "join", 2).await.is_err());
    assert!(settle(&*db, &owner, "graph", "left", 1).await.is_err());
    let foreign_owner = WorkflowExecutionAuthority {
        generation: owner.generation + 1,
        ..owner.clone()
    };
    assert!(matches!(
        settle(&*db, &foreign_owner, "graph", "join", 1).await,
        Err(Error::AuthorityConflict)
    ));
    assert_eq!(
        super::super::parallel_progress::scan_version(&*db, "graph")
            .await
            .unwrap(),
        before
    );
    assert_eq!(output(&*db, "graph", "join").await.unwrap(), value);
    assert_eq!(
        super::super::super::runs::terminalize(&*db, &owner, "graph")
            .await
            .unwrap(),
        state
    );
}

fn definition(policy: ParallelFailurePolicy) -> bcode_workflow::WorkflowDefinition {
    let mut definition = bcode_workflow::WorkflowBuilder::new(
        "parallel",
        bcode_workflow::parallel_named("join", task("left"), task("right")),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    definition.nodes.get_mut("join").unwrap().configuration["failure_policy"] =
        serde_json::to_value(policy).unwrap();
    if policy == ParallelFailurePolicy::CollectOutcomes {
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
        let node = definition.nodes.get_mut("join").unwrap();
        node.input = schema.clone();
        node.output = schema.clone();
        definition.output = schema;
    }
    definition
}
async fn fail(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    node: &str,
    outcome: attempts::TerminalOutcome,
) {
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
    attempts::settle(db, owner, "graph", &id, outcome, 3)
        .await
        .unwrap();
}
#[tokio::test]
async fn wait_all_preserves_successful_sibling_and_failure() {
    let (db, owner) = admitted(&definition(ParallelFailurePolicy::WaitAll)).await;
    fail(&*db, &owner, "left", attempts::TerminalOutcome::Failed).await;
    assert!(!settle(&*db, &owner, "graph", "join", 1).await.unwrap());
    success(&*db, &owner, "right").await;
    super::super::settle_output(&*db, &owner, "graph", "right", 1, &serde_json::json!(9))
        .await
        .unwrap();
    assert!(settle(&*db, &owner, "graph", "join", 1).await.unwrap());
    assert_eq!(
        output(&*db, "graph", "right").await.unwrap(),
        Some(serde_json::json!(9))
    );
    assert_eq!(
        super::super::super::runs::terminalize(&*db, &owner, "graph")
            .await
            .unwrap(),
        Some(super::super::super::runs::RunState::Failed)
    );
}
#[tokio::test]
async fn collect_outcomes_emits_named_map_without_inventing_failed_values() {
    let (db, owner) = admitted(&definition(ParallelFailurePolicy::CollectOutcomes)).await;
    fail(&*db, &owner, "left", attempts::TerminalOutcome::Failed).await;
    success(&*db, &owner, "right").await;
    super::super::settle_output(&*db, &owner, "graph", "right", 1, &serde_json::json!(9))
        .await
        .unwrap();
    settle(&*db, &owner, "graph", "join", 1).await.unwrap();
    assert_eq!(
        output(&*db, "graph", "join").await.unwrap(),
        Some(
            serde_json::json!({"left":{"status":"failed"},"right":{"status":"completed","value":9}})
        )
    );
    assert_eq!(
        super::super::super::runs::terminalize(&*db, &owner, "graph")
            .await
            .unwrap(),
        Some(super::super::super::runs::RunState::Completed)
    );
}
#[tokio::test]
async fn fail_fast_commits_cancellation_without_releasing_or_terminalizing_sibling() {
    let (db, owner) = admitted(&definition(ParallelFailurePolicy::FailFast)).await;
    fail(&*db, &owner, "left", attempts::TerminalOutcome::Failed).await;
    assert!(settle(&*db, &owner, "graph", "join", 1).await.unwrap());
    // The first pass records intent; the next observes the never-prepared sibling's skip.
    assert!(settle(&*db, &owner, "graph", "join", 1).await.unwrap());
    let intents = cancellations(&*db, "graph", None, 1).await.unwrap();
    assert_eq!(intents.len(), 1);
    assert!(
        !super::super::is_ready_activation(&*db, "graph", "right", &intents[0])
            .await
            .unwrap()
    );
    assert!(output(&*db, "graph", "right").await.unwrap().is_none());
}

#[tokio::test]
async fn collection_keeps_integrator_pending_after_failed_member() {
    let mut definition = definition(ParallelFailurePolicy::CollectOutcomes);
    let mut integrate = definition.nodes["left"].clone();
    integrate.id = "integrate".into();
    integrate.input = definition.output.clone();
    integrate.output = definition.output.clone();
    definition.nodes.insert("integrate".into(), integrate);
    definition.exits = vec!["integrate".into()];
    definition.edges.push(bcode_workflow::EdgeDefinition {
        from: "join".into(),
        to: "integrate".into(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    });
    let (db, owner) = admitted(&definition).await;
    fail(&*db, &owner, "left", attempts::TerminalOutcome::Failed).await;
    success(&*db, &owner, "right").await;
    super::super::settle_output(&*db, &owner, "graph", "right", 1, &serde_json::json!(9))
        .await
        .unwrap();
    assert_eq!(
        super::super::super::runs::terminalize(&*db, &owner, "graph")
            .await
            .unwrap(),
        None
    );
    settle(&*db, &owner, "graph", "join", 1).await.unwrap();
    assert_eq!(
        super::super::super::runs::terminalize(&*db, &owner, "graph")
            .await
            .unwrap(),
        None
    );
    let ready = super::super::ready(&*db, "graph", 1, None, 100)
        .await
        .unwrap();
    assert!(ready.iter().any(|a| a.node_id == "integrate"));
    complete_integrator(&*db, &owner, &definition).await;
}

async fn complete_integrator(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    definition: &bcode_workflow::WorkflowDefinition,
) {
    let activation = super::super::activation_id(db, "graph", "integrate")
        .await
        .unwrap()
        .unwrap();
    let prepared = crate::PreparedAttempt {
        run_id: "graph".into(),
        node_id: "integrate".into(),
        activation_id: activation.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let dispatch = attempts::prepare(db, owner, &prepared).await.unwrap();
    attempts::attach_receipt(
        db,
        owner,
        &crate::DispatchReceipt {
            run_id: "graph".into(),
            node_id: "integrate".into(),
            activation_id: activation.clone(),
            attempt: 1,
            dispatch_identity: dispatch.clone(),
            receipt: serde_json::json!({}),
            admitted_at_ms: 2,
        },
    )
    .await
    .unwrap();
    let value = output(db, "graph", "join").await.unwrap().unwrap();
    let validated = crate::ValidatedOutput {
        output_id: "integrated".into(),
        run_id: "graph".into(),
        node_id: "integrate".into(),
        activation_id: activation,
        schema_id: definition.nodes["integrate"].output.type_name.clone(),
        schema_version: 1,
        value,
        artifact_reference: None,
        created_at_ms: 3,
    };
    super::super::completion::complete(db, owner, 1, &dispatch, &validated)
        .await
        .unwrap();
    assert!(output(db, "graph", "left").await.unwrap().is_none());
    assert_eq!(
        super::super::super::runs::terminalize(db, owner, "graph")
            .await
            .unwrap(),
        Some(super::super::super::runs::RunState::Completed)
    );
}

#[tokio::test]
async fn fail_fast_intents_exclude_unrelated_ready_branch() {
    let mut definition = definition(ParallelFailurePolicy::FailFast);
    let mut unrelated = definition.nodes["right"].clone();
    unrelated.id = "unrelated".into();
    definition.nodes.insert("unrelated".into(), unrelated);
    definition.entries.push("unrelated".into());
    let (db, owner) = admitted(&definition).await;
    fail(&*db, &owner, "left", attempts::TerminalOutcome::Failed).await;
    settle(&*db, &owner, "graph", "join", 1).await.unwrap();
    let cancel = cancellations(&*db, "graph", None, 100).await.unwrap();
    let unrelated = super::super::activation_id(&*db, "graph", "unrelated")
        .await
        .unwrap()
        .unwrap();
    assert!(!cancel.contains(&unrelated));
    assert_eq!(cancel.len(), 1);
}
