use super::super::super::{
    attempts,
    tests::{database, run},
};
use super::*;
fn definition(count: u32) -> bcode_workflow::WorkflowDefinition {
    let leaf = bcode_workflow::WorkflowBuilder::new(
        "leaf",
        bcode_workflow::Step::task("member", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .unwrap()
    .definition()
    .nodes["member"]
        .clone();
    let mut controller = leaf.clone();
    controller.id = "fanout".into();
    controller.kind = bcode_workflow::NodeKind::FanOut;
    controller.input = bcode_workflow::ValueSchema {
        type_name: "fanout.input".into(),
        schema: serde_json::json!({"type":"array","items":leaf.input.schema}),
    };
    controller.output = bcode_workflow::ValueSchema {
        type_name: format!("workflow.fan-out-result/v1<{}>", leaf.output.type_name),
        schema: serde_json::json!({"type":"object","additionalProperties":false,"required":["version","members"],"properties":{"version":{"type":"integer","const":bcode_workflow::WORKFLOW_FAN_OUT_RESULT_VERSION},"members":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["index","value"],"properties":{"index":{"type":"integer","minimum":0},"value":leaf.output.schema}}}}}),
    };
    controller.configuration = serde_json::to_value(bcode_workflow::WorkflowFanOutConfiguration {
        version: bcode_workflow::WORKFLOW_FAN_OUT_CONFIGURATION_VERSION,
        member_node: Box::new(leaf),
        max_members: count,
        max_concurrency: 2,
        failure_policy: bcode_workflow::ParallelFailurePolicy::WaitAll,
    })
    .unwrap();
    bcode_workflow::WorkflowDefinition {
        schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: "fanout".into(),
        input: controller.input.clone(),
        output: controller.output.clone(),
        nodes: std::collections::BTreeMap::from([("fanout".into(), controller)]),
        entries: vec!["fanout".into()],
        exits: vec!["fanout".into()],
        edges: vec![],
    }
}
async fn completed(db: &dyn Database, owner: &WorkflowExecutionAuthority, node: &str, value: u32) {
    let activation = super::super::activation_id(db, "fanout", node)
        .await
        .unwrap()
        .unwrap();
    let request = crate::PreparedAttempt {
        run_id: "fanout".into(),
        node_id: node.into(),
        activation_id: activation.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let dispatch = attempts::prepare(db, owner, &request).await.unwrap();
    attempts::attach_receipt(
        db,
        owner,
        &crate::DispatchReceipt {
            run_id: "fanout".into(),
            node_id: node.into(),
            activation_id: activation.clone(),
            attempt: 1,
            dispatch_identity: dispatch.clone(),
            receipt: serde_json::json!({}),
            admitted_at_ms: 2,
        },
    )
    .await
    .unwrap();
    super::super::completion::complete(
        db,
        owner,
        1,
        &dispatch,
        &crate::ValidatedOutput {
            output_id: format!("{activation}:out"),
            run_id: "fanout".into(),
            node_id: node.into(),
            activation_id: activation,
            schema_id: bcode_workflow::ValueSchema::of::<u32>().type_name,
            schema_version: 1,
            value: serde_json::json!(value),
            artifact_reference: None,
            created_at_ms: 3,
        },
    )
    .await
    .unwrap();
}
#[tokio::test]
async fn bounded_member_batches_cover_32_100_and_257_without_prefix_truncation() {
    for count in [32, 100, 257] {
        let db = database().await;
        super::super::super::install(&*db).await.unwrap();
        let definition = definition(count);
        let mut request = run("fanout", false);
        request.input = Some(serde_json::json!((0..count).collect::<Vec<_>>()));
        let exact =
            bcode_workflow::WorkflowDefinitionIdentity::for_definition("fanout", &definition)
                .unwrap();
        request.definition_id = exact.definition_id;
        request.definition_version = exact.definition_version;
        super::super::super::admit(&*db, &request, &definition)
            .await
            .unwrap();
        let owner = WorkflowExecutionAuthority {
            target_artifact_id: "a".into(),
            daemon_instance_id: "d".into(),
            generation: 1,
            fencing_token: "f".into(),
        };
        attempts::claim_authority(&*db, "fanout", &owner)
            .await
            .unwrap();
        let controller = super::super::activation_id(&*db, "fanout", "fanout")
            .await
            .unwrap()
            .unwrap();
        let mut total = 0;
        loop {
            let batch = materialize(&*db, &owner, "fanout", "fanout", &controller, 1, 32)
                .await
                .unwrap();
            assert!(batch <= 32);
            total += batch;
            if batch == 0 {
                break;
            }
        }
        assert_eq!(total, usize::try_from(count).unwrap());
        let mut after = None;
        let mut index = 0;
        loop {
            let page = members(&*db, "fanout", &controller, after, 32)
                .await
                .unwrap();
            if page.is_empty() {
                break;
            }
            for node in page {
                completed(&*db, &owner, &node, index).await;
                after = Some(index);
                index += 1;
            }
        }
        for _ in 1..count.div_ceil(32) {
            assert!(
                !settle(&*db, &owner, "fanout", "fanout", &controller, 1)
                    .await
                    .unwrap()
            );
            assert!(
                super::super::fanout_progress::pending(&*db, "fanout", &controller)
                    .await
                    .unwrap()
            );
        }
        assert!(
            settle(&*db, &owner, "fanout", "fanout", &controller, 1)
                .await
                .unwrap()
        );
        let output = super::super::output(&*db, "fanout", "fanout")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            output["members"].as_array().unwrap().len(),
            usize::try_from(count).unwrap()
        );
        assert_eq!(
            output["members"][usize::try_from(count - 1).unwrap()]["value"],
            count - 1
        );
    }
}

#[tokio::test]
async fn failed_fanout_skips_never_prepared_but_retains_ambiguous_intent() {
    for count in [32, 100] {
        let db = database().await;
        super::super::super::install(&*db).await.unwrap();
        let mut definition = definition(count);
        definition.nodes.get_mut("fanout").unwrap().configuration["failure_policy"] =
            serde_json::json!("fail_fast");
        let mut request = run("fanout", false);
        request.input = Some(serde_json::json!((0..count).collect::<Vec<_>>()));
        let exact =
            bcode_workflow::WorkflowDefinitionIdentity::for_definition("fanout", &definition)
                .unwrap();
        request.definition_id = exact.definition_id;
        request.definition_version = exact.definition_version;
        super::super::super::admit(&*db, &request, &definition)
            .await
            .unwrap();
        let owner = WorkflowExecutionAuthority {
            target_artifact_id: "a".into(),
            daemon_instance_id: "d".into(),
            generation: 1,
            fencing_token: "f".into(),
        };
        attempts::claim_authority(&*db, "fanout", &owner)
            .await
            .unwrap();
        let controller = super::super::activation_id(&*db, "fanout", "fanout")
            .await
            .unwrap()
            .unwrap();
        materialize(&*db, &owner, "fanout", "fanout", &controller, 1, 100)
            .await
            .unwrap();
        let members = members(&*db, "fanout", &controller, None, 100)
            .await
            .unwrap();
        let first = prepare_member(&*db, &owner, &members[0]).await;
        let unknown = prepare_member(&*db, &owner, &members[1]).await;
        receipt_terminal(&*db, &owner, &first, attempts::TerminalOutcome::Failed).await;
        assert!(
            !settle(&*db, &owner, "fanout", "fanout", &controller, 1)
                .await
                .unwrap()
        );
        let rows = db
            .select(super::super::ACTIVATIONS)
            .where_eq("run_id", "fanout")
            .where_eq("skipped", true)
            .execute(&*db)
            .await
            .unwrap();
        assert_eq!(rows.len(), usize::try_from(count - 2).unwrap());
        assert_eq!(
            attempts::pending(&*db, "fanout", None, 100)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            super::super::super::runs::terminalize(&*db, &owner, "fanout")
                .await
                .unwrap(),
            None
        );
        receipt_terminal(&*db, &owner, &unknown, attempts::TerminalOutcome::Cancelled).await;
        settle(&*db, &owner, "fanout", "fanout", &controller, 1)
            .await
            .unwrap();
        assert!(
            super::super::super::runs::terminalize(&*db, &owner, "fanout")
                .await
                .unwrap()
                .is_some()
        );
    }
}
async fn prepare_member(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    node: &str,
) -> crate::PreparedAttempt {
    let request = crate::PreparedAttempt {
        run_id: "fanout".into(),
        node_id: node.into(),
        activation_id: super::super::activation_id(db, "fanout", node)
            .await
            .unwrap()
            .unwrap(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    attempts::prepare(db, owner, &request).await.unwrap();
    request
}
async fn receipt_terminal(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    request: &crate::PreparedAttempt,
    outcome: attempts::TerminalOutcome,
) {
    let id = request.dispatch_identity();
    attempts::attach_receipt(
        db,
        owner,
        &crate::DispatchReceipt {
            run_id: request.run_id.clone(),
            node_id: request.node_id.clone(),
            activation_id: request.activation_id.clone(),
            attempt: 1,
            dispatch_identity: id.clone(),
            receipt: serde_json::json!({}),
            admitted_at_ms: 2,
        },
    )
    .await
    .unwrap();
    attempts::settle(db, owner, &request.run_id, &id, outcome, 3)
        .await
        .unwrap();
}
