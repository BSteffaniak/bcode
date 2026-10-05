use super::super::{
    attempts,
    tests::{database, run},
};
use super::*;

pub(super) async fn admitted(
    definition: &WorkflowDefinition,
) -> (Box<dyn Database>, WorkflowExecutionAuthority) {
    let db = database().await;
    super::super::install(&*db).await.unwrap();
    let mut request = run("graph", false);
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("graph", definition).unwrap();
    request.definition_id = identity.definition_id;
    request.definition_version = identity.definition_version;
    super::super::admit(&*db, &request, definition)
        .await
        .unwrap();
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    attempts::claim_authority(&*db, "graph", &owner)
        .await
        .unwrap();
    (db, owner)
}
pub(super) async fn success(db: &dyn Database, owner: &WorkflowExecutionAuthority, node: &str) {
    let prepared = crate::PreparedAttempt {
        run_id: "graph".into(),
        node_id: node.into(),
        activation_id: activation_id(db, "graph", node).await.unwrap().unwrap(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let id = attempts::prepare(db, owner, &prepared).await.unwrap();
    let receipt = crate::DispatchReceipt {
        run_id: "graph".into(),
        node_id: node.into(),
        activation_id: activation_id(db, "graph", node).await.unwrap().unwrap(),
        attempt: 1,
        dispatch_identity: id.clone(),
        receipt: serde_json::json!({"receipt":"owner"}),
        admitted_at_ms: 2,
    };
    attempts::attach_receipt(db, owner, &receipt).await.unwrap();
    attempts::settle(
        db,
        owner,
        "graph",
        &id,
        attempts::TerminalOutcome::Succeeded,
        3,
    )
    .await
    .unwrap();
}
pub(super) fn task(name: &str) -> bcode_workflow::Step<u32, u32> {
    bcode_workflow::Step::task(name, |value: u32, _| async move { Ok(value) })
}
#[tokio::test]
async fn sequence_settles_exact_output_and_validates_terminal_schema() {
    let definition =
        bcode_workflow::WorkflowBuilder::new("sequence", task("first").then(task("last")))
            .build()
            .unwrap()
            .definition()
            .clone();
    let (db, owner) = admitted(&definition).await;
    assert_eq!(
        ready(&*db, "graph", 1, None, 10).await.unwrap()[0].node_id,
        "first"
    );
    success(&*db, &owner, "first").await;
    assert!(
        settle_output(&*db, &owner, "graph", "first", 2, &serde_json::json!(2))
            .await
            .is_err()
    );
    let result = settle_output(&*db, &owner, "graph", "first", 1, &serde_json::json!(2))
        .await
        .unwrap();
    assert!(result.graph_output.is_none());
    assert!(
        settle_output(&*db, &owner, "graph", "first", 1, &serde_json::json!(2))
            .await
            .unwrap()
            .duplicate
    );
    assert!(
        settle_output(&*db, &owner, "graph", "first", 1, &serde_json::json!(3))
            .await
            .is_err()
    );
    let pending = ready(&*db, "graph", 1, None, 10).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].input, serde_json::json!(2));
    success(&*db, &owner, "last").await;
    assert!(
        settle_output(
            &*db,
            &owner,
            "graph",
            "last",
            1,
            &serde_json::json!("wrong")
        )
        .await
        .is_err()
    );
    assert_eq!(ready(&*db, "graph", 1, None, 10).await.unwrap().len(), 1);
    assert_eq!(
        settle_output(&*db, &owner, "graph", "last", 1, &serde_json::json!(4))
            .await
            .unwrap()
            .graph_output,
        Some(serde_json::json!(4))
    );
    assert!(ready(&*db, "graph", 1, None, 10).await.unwrap().is_empty());
}
#[tokio::test]
async fn parallel_fan_in_waits_and_preserves_left_right_order() {
    let definition = bcode_workflow::WorkflowBuilder::new(
        "parallel",
        bcode_workflow::parallel_named("join", task("left"), task("right")),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let (db, owner) = admitted(&definition).await;
    assert_eq!(ready(&*db, "graph", 1, None, 10).await.unwrap().len(), 2);
    success(&*db, &owner, "right").await;
    assert!(
        settle_output(&*db, &owner, "graph", "right", 1, &serde_json::json!(9))
            .await
            .unwrap()
            .graph_output
            .is_none()
    );
    success(&*db, &owner, "left").await;
    let result = settle_output(&*db, &owner, "graph", "left", 1, &serde_json::json!(7))
        .await
        .unwrap();
    assert_eq!(result.graph_output, Some(serde_json::json!([7, 9])));
    let rows = db
        .select(ACTIVATIONS)
        .where_eq("run_id", "graph")
        .where_eq("node_id", "right")
        .execute(&*db)
        .await
        .unwrap();
    assert_eq!(
        decode::<Value>(&rows[0], "output_json").unwrap(),
        serde_json::json!(9)
    );
}

#[tokio::test]
async fn transform_reads_canonical_run_input_without_replacing_source_output() {
    let mut definition =
        bcode_workflow::WorkflowBuilder::new("transform", task("first").then(task("last")))
            .build()
            .unwrap()
            .definition()
            .clone();
    definition.edges[0].transform = Some(bcode_workflow::WorkflowTransform {
        version: bcode_workflow::WORKFLOW_TRANSFORM_VERSION,
        expression: bcode_workflow::WorkflowTransformExpression::Input {
            source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_STATE.into(),
            path: String::new(),
        },
        output: bcode_workflow::ValueSchema::of::<u32>(),
    });
    let (db, owner) = admitted(&definition).await;
    success(&*db, &owner, "first").await;
    settle_output(&*db, &owner, "graph", "first", 1, &serde_json::json!(99))
        .await
        .unwrap();
    let activations = ready(&*db, "graph", 1, None, 10).await.unwrap();
    assert_eq!(activations[0].input, serde_json::json!(1));
    assert_eq!(
        output(&*db, "graph", "first").await.unwrap(),
        Some(serde_json::json!(99))
    );
}

#[tokio::test]
async fn loop_body_evaluation_repeat_uses_new_generation_and_retains_output() {
    let body =
        bcode_workflow::Step::task("implement", |value: Value, _| async move { Ok(value) }).then(
            bcode_workflow::Step::task("evaluate", |value: Value, _| async move { Ok(value) }),
        );
    let mut definition = bcode_workflow::WorkflowBuilder::new(
        "loop",
        body.repeat_while(
            "repeat",
            bcode_workflow::field::<Value>("done").eq(false),
            3,
        ),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    // Retain the actual goal-style state transform; first input must be an object.
    definition.input = bcode_workflow::ValueSchema::of::<Value>();
    let db = database().await;
    super::super::install(&*db).await.unwrap();
    let mut request = run("graph", false);
    request.input = Some(serde_json::json!({"done":false,"iteration":1}));
    let exact =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("loop", &definition).unwrap();
    request.definition_id = exact.definition_id;
    request.definition_version = exact.definition_version;
    super::super::admit(&*db, &request, &definition)
        .await
        .unwrap();
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    attempts::claim_authority(&*db, "graph", &owner)
        .await
        .unwrap();
    let first = activation_id(&*db, "graph", "implement")
        .await
        .unwrap()
        .unwrap();
    for iteration in 1..=2 {
        let value = serde_json::json!({"done":iteration==2,"iteration":iteration});
        for node in ["implement", "evaluate"] {
            success(&*db, &owner, node).await;
            settle_output(&*db, &owner, "graph", node, 1, &value)
                .await
                .unwrap();
        }
        let id = activation_id(&*db, "graph", "repeat")
            .await
            .unwrap()
            .unwrap();
        control::settle(&*db, &owner, "graph", "repeat", &id, 1)
            .await
            .unwrap();
        if iteration == 1 {
            let next = ready(&*db, "graph", 1, None, 10).await.unwrap();
            assert_eq!(next[0].generation, 2);
            assert_ne!(next[0].activation_id, first);
            assert_eq!(next[0].input["iteration"], 2);
        }
    }
    let rows = db
        .select(ACTIVATIONS)
        .where_eq("run_id", "graph")
        .where_eq("node_id", "implement")
        .execute(&*db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(ready(&*db, "graph", 1, None, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn conditional_edge_uses_shared_predicate_and_does_not_activate_unselected_target() {
    let mut definition =
        bcode_workflow::WorkflowBuilder::new("branch", task("first").then(task("last")))
            .build()
            .unwrap()
            .definition()
            .clone();
    definition.edges[0].kind = bcode_workflow::EdgeKind::Conditional {
        predicate: bcode_workflow::PredicateExpression::Equals {
            version: bcode_workflow::WORKFLOW_PREDICATE_VERSION,
            path: String::new(),
            value: serde_json::json!(99),
        },
        expected: true,
    };
    let (db, owner) = admitted(&definition).await;
    success(&*db, &owner, "first").await;
    settle_output(&*db, &owner, "graph", "first", 1, &serde_json::json!(1))
        .await
        .unwrap();
    assert!(ready(&*db, "graph", 1, None, 10).await.unwrap().is_empty());
    assert!(output(&*db, "graph", "last").await.unwrap().is_none());
}

#[tokio::test]
async fn gates_require_exact_resolution_and_preserve_approval_input() {
    let definition = bcode_workflow::WorkflowBuilder::new(
        "gates",
        bcode_workflow::Step::<u32, u32>::input("input")
            .then(bcode_workflow::Step::<u32, u32>::approval("approve")),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let (db, owner) = admitted(&definition).await;
    let waits = gate_waits::page(&*db, "graph", 10).await.unwrap();
    assert_eq!(waits.gates.len(), 1);
    assert_eq!(waits.gates[0].node_id, "input");
    assert_eq!(waits.gates[0].kind, gate_waits::GateKind::Input);
    assert_eq!(
        waits.gates[0].response_schema,
        Some(definition.nodes["input"].output.clone())
    );
    assert!(
        gate_waits::page_after(&*db, "graph", Some(99), None, 10)
            .await
            .is_err()
    );
    let id = activation_id(&*db, "graph", "input")
        .await
        .unwrap()
        .unwrap();
    assert!(
        gates::resolve(&*db, &owner, "graph", "input", &id, 1, None)
            .await
            .is_err()
    );
    let before = u64::try_from(super::super::runs::timestamp().unwrap()).unwrap();
    let resolution = gates::resolve_observed(
        &*db,
        &owner,
        "graph",
        "input",
        &id,
        1,
        Some(&serde_json::json!(9)),
    )
    .await
    .unwrap();
    let after = u64::try_from(super::super::runs::timestamp().unwrap()).unwrap();
    assert!(!resolution.duplicate);
    assert_eq!(resolution.activated.len(), 1);
    let created = &resolution.activated[0];
    assert_eq!(created.node_id, "approve");
    assert!((before..=after).contains(&created.created_at_ms));
    let stored = activation(&*db, "graph", "approve").await.unwrap().unwrap();
    assert_eq!(creation_time(&stored).unwrap(), created.created_at_ms);
    assert!(
        !gates::resolve(
            &*db,
            &owner,
            "graph",
            "input",
            &id,
            1,
            Some(&serde_json::json!(9))
        )
        .await
        .unwrap()
    );
    assert!(
        gates::resolve(
            &*db,
            &owner,
            "graph",
            "input",
            &id,
            1,
            Some(&serde_json::json!(10))
        )
        .await
        .is_err()
    );
    let approval = activation_id(&*db, "graph", "approve")
        .await
        .unwrap()
        .unwrap();
    gates::resolve(&*db, &owner, "graph", "approve", &approval, 1, None)
        .await
        .unwrap();
    assert_eq!(
        output(&*db, "graph", "approve").await.unwrap(),
        Some(serde_json::json!(9))
    );
}

#[tokio::test]
async fn blocked_branch_resumes_repeat_and_unselected_gates_do_not_block_completion() {
    let evaluation =
        bcode_workflow::Step::task("evaluate", |value: Value, _| async move { Ok(value) }).branch(
            "progress",
            bcode_workflow::field::<Value>("blocked").eq(false),
            bcode_workflow::Step::<Value, Value>::input("continue"),
            bcode_workflow::Step::<Value, Value>::input("blocked"),
        );
    let definition = bcode_workflow::WorkflowBuilder::new(
        "blocked-loop",
        evaluation.repeat_while(
            "repeat",
            bcode_workflow::field::<Value>("done").eq(false),
            2,
        ),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let db = database().await;
    super::super::install(&*db).await.unwrap();
    let mut request = run("graph", false);
    request.input = Some(serde_json::json!({"blocked":true,"done":false,"iteration":1}));
    let exact =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("blocked-loop", &definition)
            .unwrap();
    request.definition_id = exact.definition_id;
    request.definition_version = exact.definition_version;
    super::super::admit(&*db, &request, &definition)
        .await
        .unwrap();
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    attempts::claim_authority(&*db, "graph", &owner)
        .await
        .unwrap();
    for round in 1..=2 {
        let value = serde_json::json!({"blocked":round==1,"done":round==2,"iteration":round});
        success(&*db, &owner, "evaluate").await;
        settle_output(&*db, &owner, "graph", "evaluate", 1, &value)
            .await
            .unwrap();
        let id = activation_id(&*db, "graph", "progress")
            .await
            .unwrap()
            .unwrap();
        control::settle(&*db, &owner, "graph", "progress", &id, 1)
            .await
            .unwrap();
        let gate = if round == 1 { "blocked" } else { "continue" };
        let id = activation_id(&*db, "graph", gate).await.unwrap().unwrap();
        gates::resolve(&*db, &owner, "graph", gate, &id, 1, Some(&value))
            .await
            .unwrap();
        let id = activation_id(&*db, "graph", "repeat")
            .await
            .unwrap()
            .unwrap();
        control::settle(&*db, &owner, "graph", "repeat", &id, 1)
            .await
            .unwrap();
    }
    assert!(ready(&*db, "graph", 1, None, 100).await.unwrap().is_empty());
}

#[tokio::test]
async fn nested_repeat_resets_inner_cycle_budget_without_reusing_activation_ids() {
    let inner = bcode_workflow::Step::task("body", |v: Value, _| async move { Ok(v) })
        .repeat_while(
            "inner",
            bcode_workflow::field::<Value>("inner_done").eq(false),
            2,
        );
    let definition = bcode_workflow::WorkflowBuilder::new(
        "nested",
        inner.repeat_while(
            "outer",
            bcode_workflow::field::<Value>("outer_done").eq(false),
            2,
        ),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let db = database().await;
    super::super::install(&*db).await.unwrap();
    let mut request = run("graph", false);
    request.input = Some(serde_json::json!({"iteration":1,"inner_done":false,"outer_done":false}));
    let exact =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("nested", &definition).unwrap();
    request.definition_id = exact.definition_id;
    request.definition_version = exact.definition_version;
    super::super::admit(&*db, &request, &definition)
        .await
        .unwrap();
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    attempts::claim_authority(&*db, "graph", &owner)
        .await
        .unwrap();
    for outer in 1..=2 {
        for inner in 1..=2 {
            let value =
                serde_json::json!({"iteration":inner,"inner_done":inner==2,"outer_done":outer==2});
            success(&*db, &owner, "body").await;
            settle_output(&*db, &owner, "graph", "body", 1, &value)
                .await
                .unwrap();
            let id = activation_id(&*db, "graph", "inner")
                .await
                .unwrap()
                .unwrap();
            control::settle(&*db, &owner, "graph", "inner", &id, 1)
                .await
                .unwrap();
        }
        let id = activation_id(&*db, "graph", "outer")
            .await
            .unwrap()
            .unwrap();
        control::settle(&*db, &owner, "graph", "outer", &id, 1)
            .await
            .unwrap();
    }
    assert_eq!(
        control_state::completed(&*db, "graph", "inner")
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        control_state::completed(&*db, "graph", "outer")
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        db.select(ACTIVATIONS)
            .where_eq("node_id", "body")
            .execute(&*db)
            .await
            .unwrap()
            .len(),
        4
    );
}

#[tokio::test]
async fn atomic_completion_rolls_back_attempt_and_keeps_receipt_when_output_invalid() {
    let definition = bcode_workflow::WorkflowBuilder::new("atomic", task("work"))
        .build()
        .unwrap()
        .definition()
        .clone();
    let (db, owner) = admitted(&definition).await;
    let activation = activation_id(&*db, "graph", "work").await.unwrap().unwrap();
    let attempt = crate::PreparedAttempt {
        run_id: "graph".into(),
        node_id: "work".into(),
        activation_id: activation.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let id = attempts::prepare(&*db, &owner, &attempt).await.unwrap();
    attempts::attach_receipt(
        &*db,
        &owner,
        &crate::DispatchReceipt {
            run_id: "graph".into(),
            node_id: "work".into(),
            activation_id: activation.clone(),
            attempt: 1,
            dispatch_identity: id.clone(),
            receipt: serde_json::json!({}),
            admitted_at_ms: 2,
        },
    )
    .await
    .unwrap();
    let mut output = crate::ValidatedOutput {
        output_id: "canonical-output".into(),
        run_id: "graph".into(),
        node_id: "work".into(),
        activation_id: activation,
        schema_id: definition.nodes["work"].output.type_name.clone(),
        schema_version: 1,
        value: serde_json::json!("invalid"),
        artifact_reference: None,
        created_at_ms: 3,
    };
    assert!(
        completion::complete(&*db, &owner, 1, &id, &output)
            .await
            .is_err()
    );
    assert_eq!(
        attempts::pending(&*db, "graph", None, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        completion::output(&*db, "graph", "canonical-output")
            .await
            .unwrap()
            .is_none()
    );
    output.value = serde_json::json!(4);
    completion::complete(&*db, &owner, 1, &id, &output)
        .await
        .unwrap();
    assert!(
        completion::complete(&*db, &owner, 1, &id, &output)
            .await
            .unwrap()
            .duplicate
    );
    assert_eq!(
        completion::output(&*db, "graph", "canonical-output")
            .await
            .unwrap(),
        Some(output)
    );
}

#[tokio::test]
async fn approval_denial_is_durable_failure_not_a_grant() {
    let definition = bcode_workflow::WorkflowBuilder::new(
        "approval",
        bcode_workflow::Step::<u32, u32>::approval("approval"),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let (db, owner) = admitted(&definition).await;
    let id = activation_id(&*db, "graph", "approval")
        .await
        .unwrap()
        .unwrap();
    gates::deny(&*db, &owner, "graph", "approval", &id, 1)
        .await
        .unwrap();
    assert!(
        gates::resolve(&*db, &owner, "graph", "approval", &id, 1, None)
            .await
            .is_err()
    );
    assert_eq!(
        super::super::runs::terminalize(&*db, &owner, "graph")
            .await
            .unwrap(),
        Some(super::super::runs::RunState::Failed)
    );
    assert!(output(&*db, "graph", "approval").await.unwrap().is_none());
}

#[tokio::test]
async fn parallel_branch_alternatives_use_only_selected_canonical_output() {
    let left = task("choose").branch(
        "branch",
        bcode_workflow::field::<u32>("").eq(1),
        task("selected"),
        task("unselected"),
    );
    let definition = bcode_workflow::WorkflowBuilder::new(
        "alternatives",
        bcode_workflow::parallel_named("join", left, task("right")),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let (db, owner) = admitted(&definition).await;
    success(&*db, &owner, "choose").await;
    settle_output(&*db, &owner, "graph", "choose", 1, &serde_json::json!(1))
        .await
        .unwrap();
    let id = activation_id(&*db, "graph", "branch")
        .await
        .unwrap()
        .unwrap();
    control::settle(&*db, &owner, "graph", "branch", &id, 1)
        .await
        .unwrap();
    assert!(!is_ready(&*db, "graph", "unselected").await.unwrap());
    success(&*db, &owner, "right").await;
    settle_output(&*db, &owner, "graph", "right", 1, &serde_json::json!(9))
        .await
        .unwrap();
    success(&*db, &owner, "selected").await;
    assert_eq!(
        settle_output(&*db, &owner, "graph", "selected", 1, &serde_json::json!(7))
            .await
            .unwrap()
            .graph_output,
        Some(serde_json::json!([7, 9]))
    );
}

#[tokio::test]
async fn duplicate_completion_ignores_observation_time_after_repeat_advances() {
    let definition = bcode_workflow::WorkflowBuilder::new(
        "repeat-duplicate",
        bcode_workflow::Step::task("body", |v: Value, _| async move { Ok(v) }).repeat_while(
            "repeat",
            bcode_workflow::field::<Value>("done").eq(false),
            3,
        ),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let db = database().await;
    super::super::install(&*db).await.unwrap();
    let (run, owner) = duplicate_fixture(&*db, &definition).await;
    let activation = activation_id(&*db, "graph", "body").await.unwrap().unwrap();
    let prepared = crate::PreparedAttempt {
        run_id: "graph".into(),
        node_id: "body".into(),
        activation_id: activation.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let dispatch = attempts::prepare(&*db, &owner, &prepared).await.unwrap();
    attempts::attach_receipt(
        &*db,
        &owner,
        &crate::DispatchReceipt {
            run_id: "graph".into(),
            node_id: "body".into(),
            activation_id: activation.clone(),
            attempt: 1,
            dispatch_identity: dispatch.clone(),
            receipt: serde_json::json!({}),
            admitted_at_ms: 2,
        },
    )
    .await
    .unwrap();
    let mut value = crate::ValidatedOutput {
        output_id: "original".into(),
        run_id: "graph".into(),
        node_id: "body".into(),
        activation_id: activation.clone(),
        schema_id: definition.nodes["body"].output.type_name.clone(),
        schema_version: 1,
        value: run.input.unwrap(),
        artifact_reference: None,
        created_at_ms: 3,
    };
    completion::complete(&*db, &owner, 1, &dispatch, &value)
        .await
        .unwrap();
    value.created_at_ms = 99;
    assert!(
        completion::complete(&*db, &owner, 1, &dispatch, &value)
            .await
            .unwrap()
            .duplicate
    );
    let repeat = activation_id(&*db, "graph", "repeat")
        .await
        .unwrap()
        .unwrap();
    control::settle(&*db, &owner, "graph", "repeat", &repeat, 1)
        .await
        .unwrap();
    assert_ne!(
        activation_id(&*db, "graph", "body").await.unwrap().unwrap(),
        activation
    );
    assert!(
        completion::complete(&*db, &owner, 1, &dispatch, &value)
            .await
            .unwrap()
            .duplicate
    );
    assert_eq!(
        completion::output(&*db, "graph", "original")
            .await
            .unwrap()
            .unwrap()
            .created_at_ms,
        3
    );
    value.value["done"] = serde_json::json!(true);
    assert!(
        completion::complete(&*db, &owner, 1, &dispatch, &value)
            .await
            .is_err()
    );
}

async fn duplicate_fixture(
    db: &dyn Database,
    definition: &WorkflowDefinition,
) -> (crate::NewWorkflowRun, WorkflowExecutionAuthority) {
    let mut run = run("graph", false);
    run.input = Some(serde_json::json!({"done":false,"iteration":1}));
    let exact =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("repeat-duplicate", definition)
            .unwrap();
    run.definition_id = exact.definition_id;
    run.definition_version = exact.definition_version;
    super::super::admit(db, &run, definition).await.unwrap();
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    attempts::claim_authority(db, "graph", &owner)
        .await
        .unwrap();
    (run, owner)
}

#[tokio::test]
async fn successor_frontier_resumes_fanout_edges_in_bounded_batches() {
    let mut definition = bcode_workflow::WorkflowBuilder::new("frontier", task("source"))
        .build()
        .unwrap()
        .definition()
        .clone();
    let source = definition.nodes["source"].clone();
    for index in 0..100 {
        let mut node = source.clone();
        node.id = format!("worker{index:03}");
        definition.edges.push(bcode_workflow::EdgeDefinition {
            from: "source".into(),
            to: node.id.clone(),
            kind: EdgeKind::Direct,
            transform: None,
        });
        definition.nodes.insert(node.id.clone(), node);
    }
    definition.exits = vec!["worker099".into()];
    let (db, owner) = admitted(&definition).await;
    success(&*db, &owner, "source").await;
    settle_output(&*db, &owner, "graph", "source", 1, &serde_json::json!(7))
        .await
        .unwrap();
    assert!(frontier::pending(&*db, "graph").await.unwrap());
    assert_eq!(ready(&*db, "graph", 1, None, 100).await.unwrap().len(), 16);
    while frontier::pending(&*db, "graph").await.unwrap() {
        assert!(frontier::advance(&*db, &owner, "graph", 17).await.unwrap() <= 17);
    }
    assert_eq!(ready(&*db, "graph", 1, None, 100).await.unwrap().len(), 100);
    assert_eq!(
        frontier::advance(&*db, &owner, "graph", 17).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn incoming_dependencies_resume_past_page_without_early_activation() {
    let mut definition = bcode_workflow::WorkflowBuilder::new("dependencies", task("target"))
        .build()
        .unwrap()
        .definition()
        .clone();
    let template = definition.nodes["target"].clone();
    definition.entries.clear();
    for index in 0..130 {
        let mut node = template.clone();
        node.id = format!("source{index:03}");
        definition.entries.push(node.id.clone());
        definition.edges.push(bcode_workflow::EdgeDefinition {
            from: node.id.clone(),
            to: "target".into(),
            kind: EdgeKind::Direct,
            transform: None,
        });
        definition.nodes.insert(node.id.clone(), node);
    }
    let (db, owner) = admitted(&definition).await;
    for index in 0..130 {
        let node = format!("source{index:03}");
        success(&*db, &owner, &node).await;
        settle_output(&*db, &owner, "graph", &node, 1, &serde_json::json!(7))
            .await
            .unwrap();
        if index < 129 {
            assert!(
                activation_id(&*db, "graph", "target")
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }
    assert!(frontier::pending(&*db, "graph").await.unwrap());
    // New calls reconstruct the cursor exclusively from committed state; no in-memory scan state.
    let mut batches = 0;
    while frontier::pending(&*db, "graph").await.unwrap() {
        let used = frontier::advance(&*db, &owner, "graph", 7).await.unwrap();
        assert!(used <= 7);
        batches += 1;
        assert!(batches < 5000);
    }
    assert!(batches > 1);
    let ready = ready_nodes(&*db, "graph", None, 10).await.unwrap().1;
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].0.node_id, "target");
    assert_eq!(ready[0].0.input, serde_json::json!(7));
}
