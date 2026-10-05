use super::*;

fn fanout_definition(
    leaf: bcode_workflow::NodeDefinition,
    count: u32,
) -> bcode_workflow::WorkflowDefinition {
    let mut node = leaf.clone();
    node.id = "fanout".into();
    node.kind = bcode_workflow::NodeKind::FanOut;
    node.input = bcode_workflow::ValueSchema {
        type_name: "fanout.input".into(),
        schema: serde_json::json!({"type":"array","items":leaf.input.schema}),
    };
    node.output = bcode_workflow::ValueSchema {
        type_name: format!("workflow.fan-out-result/v1<{}>", leaf.output.type_name),
        schema: serde_json::json!({"type":"object","additionalProperties":false,"required":["version","members"],"properties":{"version":{"type":"integer","const":1},"members":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["index","value"],"properties":{"index":{"type":"integer","minimum":0},"value":leaf.output.schema}}}}}),
    };
    node.configuration = serde_json::to_value(bcode_workflow::WorkflowFanOutConfiguration {
        version: bcode_workflow::WORKFLOW_FAN_OUT_CONFIGURATION_VERSION,
        member_node: Box::new(leaf),
        max_members: count,
        max_concurrency: count,
        failure_policy: bcode_workflow::ParallelFailurePolicy::WaitAll,
    })
    .expect("config");
    bcode_workflow::WorkflowDefinition {
        schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: "fanout".into(),
        input: node.input.clone(),
        output: node.output.clone(),
        nodes: std::collections::BTreeMap::from([("fanout".into(), node)]),
        entries: vec!["fanout".into()],
        exits: vec!["fanout".into()],
        edges: vec![],
    }
}

#[tokio::test]
async fn fanout_real_provider_members_overlap_at_32_and_100() {
    for count in [32, 100] {
        fanout_overlap(count).await;
    }
}

async fn fanout_overlap(count: u32) {
    let (_root, _sessions, state, _) = typed_provider_fixture_with_delegation(true).await;
    let _release =
        ReleaseGate(state.selected_provider_context.settings["delegation_fixture_gate"].clone());
    let leaf = parallel_provider_definition().nodes["left"].clone();
    let definition = fanout_definition(leaf, count);
    let db = open(&crate::workflow_store_path(&state), true).expect("db");
    let authority = owner::local_authority(&state, "new")
        .await
        .expect("owner")
        .expect("authority");
    let mut run = representation::hierarchy::parent_context(&db, &authority.authority, "new")
        .await
        .expect("context")
        .admission;
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("fanout", &definition)
            .expect("identity");
    run.run_id = "fanout".into();
    run.definition_id = identity.definition_id;
    run.definition_version = identity.definition_version;
    run.input = Some(serde_json::json!(vec![1_u32; count as usize]));
    run.limits.node_execution_cap = u64::from(count) + 1;
    representation::admit(&db, &run, &definition)
        .await
        .expect("fixture typed admission");
    let initial = tokio::time::timeout(
        std::time::Duration::from_secs(u64::from(count) * 2),
        driver::drive(&state, "fanout"),
    )
    .await;
    if initial.is_err() {
        let pending = representation::attempts::pending(&db, "fanout", None, 100).await;
        bcode_fake_provider_plugin::release_fake_event_script_gate(
            &state.selected_provider_context.settings["delegation_fixture_gate"],
        );
        panic!(
            "initial drive did not return at external wait; pending_count={:?}",
            pending.map(|rows| rows.len())
        );
    }
    initial
        .expect("bounded drive")
        .expect("materialize and dispatch");
    let pending = representation::attempts::pending(&db, "fanout", None, 100)
        .await
        .expect("overlap");
    assert_eq!(pending.len(), count as usize);
    assert!(
        pending.iter().all(|attempt| attempt.receipt.is_some()),
        "all provider owners admitted before releasing any"
    );
    bcode_fake_provider_plugin::release_fake_event_script_gate(
        &state.selected_provider_context.settings["delegation_fixture_gate"],
    );
    let completion =
        tokio::time::timeout(std::time::Duration::from_secs(u64::from(count)), async {
            loop {
                driver::drive(&state, "fanout").await.expect("drive");
                if status(&state, "fanout")
                    .await
                    .expect("status")
                    .expect("run")
                    .status
                    == bcode_workflow::RunStatus::Completed
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
    assert!(
        completion.is_ok(),
        "members complete count={count} remaining={:?}",
        representation::attempts::pending(&db, "fanout", None, 100)
            .await
            .map(|v| v.len())
    );
}

pub(super) struct ReleaseGate(pub(super) String);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        bcode_fake_provider_plugin::release_fake_event_script_gate(&self.0);
    }
}

#[tokio::test]
async fn normal_start_fanout_respects_cap8_and_uncapped32() {
    for cap in [Some(8), None] {
        let (_root, _sessions, state, _) = typed_provider_fixture_with_delegation(true).await;
        let gate = ReleaseGate(
            state.selected_provider_context.settings["delegation_fixture_gate"].clone(),
        );
        let definition =
            fanout_definition(parallel_provider_definition().nodes["left"].clone(), 32);
        start_public(
            &state,
            "normal-fanout",
            definition,
            serde_json::json!(vec![1; 32]),
            cap,
        )
        .await;
        let db = open(&crate::workflow_store_path(&state), false).expect("db");
        let attempts = representation::attempts::pending(&db, "normal-fanout", None, 100)
            .await
            .expect("attempts");
        assert_eq!(attempts.len(), cap.unwrap_or(32) as usize);
        assert!(attempts.iter().all(|a| a.receipt.is_some()));
        state.start_workflow_driver().await;
        drop(gate);
        // Up to 24 members still need session setup after the cap8 gate releases.
        // Budget by the configured 32-member workload, not a production retry limit.
        let result = wait_public_with_budget(
            &state,
            "normal-fanout",
            std::time::Duration::from_secs(32 * 2),
        )
        .await;
        stop_fixture_driver(&state).await;
        assert!(
            result.is_ok(),
            "normal scheduler completion cap={cap:?}, remaining={:?}",
            representation::attempts::pending(&db, "normal-fanout", None, 100)
                .await
                .map(|v| v.len())
        );
        let outputs = super::super::outputs::run_outputs(&state, "normal-fanout", 100)
            .await
            .expect("outputs");
        let result = outputs
            .iter()
            .find(|output| output.node_id == "fanout")
            .expect("controller output");
        let members = result.value["members"].as_array().expect("members");
        assert_eq!(members.len(), 32);
        for (index, member) in members.iter().enumerate() {
            assert_eq!(member["index"], serde_json::json!(index));
            assert_eq!(member["value"], serde_json::json!(9));
        }
    }
}

pub(super) async fn start_public(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
    definition: bcode_workflow::WorkflowDefinition,
    input: serde_json::Value,
    cap: Option<u32>,
) {
    let parent = state
        .sessions
        .create_session(None, std::path::PathBuf::from("."))
        .await
        .expect("parent");
    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(run_id, &definition)
        .expect("identity");
    state
        .workflow_store
        .lock()
        .expect("store")
        .persist_definition(
            &identity.definition_id,
            identity.definition_version,
            &definition,
        )
        .expect("definition");
    tokio::time::timeout(
        std::time::Duration::from_secs(40),
        crate::workflow_operations::start_run(
            state,
            bcode_workflow::WorkflowRunStartRequest {
                definition_id: identity.definition_id,
                definition_version: identity.definition_version,
                run_id: Some(run_id.into()),
                workspace_snapshot: "fixture".into(),
                parent_session_id: parent.id,
                parent_session_generation: None,
                binding: None,
                input: Some(input),
                limits: bcode_workflow::WorkflowRunLimits {
                    concurrency_cap: cap,
                    ..Default::default()
                },
            },
            None,
        ),
    )
    .await
    .expect("bounded normal start")
    .expect("normal start");
}

pub(super) async fn wait_public(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
) -> Result<(), tokio::time::error::Elapsed> {
    wait_public_with_budget(state, run_id, std::time::Duration::from_secs(30)).await
}

async fn wait_public_with_budget(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
    budget: std::time::Duration,
) -> Result<(), tokio::time::error::Elapsed> {
    tokio::time::timeout(budget, async {
        loop {
            let application = crate::workflow_operations::WorkflowAuthoringApplication::new(
                state,
                crate::ClientId::new(),
            );
            let summary = bcode_workflow::WorkflowRunApplication::workflow_run_status(
                &application,
                run_id.into(),
            )
            .await
            .expect("public status")
            .expect("run");
            drop(application);
            if summary.status == bcode_workflow::RunStatus::Completed {
                break;
            }
            assert_ne!(summary.status, bcode_workflow::RunStatus::Failed);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
}
