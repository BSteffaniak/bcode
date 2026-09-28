use super::*;
use bcode_workflow_store::{ValidatedOutput, WorkflowExecutionAuthority, WorkflowStore};

fn prepare_group(value: serde_json::Value) -> bcode_workflow::WorkflowRunGraphEditBatch {
    let response = bcode_workflow_plugin::WorkflowPlugin.invoke_service(NativeServiceContext {
        plugin_id: "bcode.workflow".into(),
        request: ServiceRequest {
            interface_id: bcode_tool::TOOL_SERVICE_INTERFACE_ID.into(),
            operation: bcode_tool::OP_PREPARE_TOOL.into(),
            payload: serde_json::to_vec(&bcode_tool::ToolPreparationRequest {
                invocation: bcode_tool::ToolInvocationDescriptor {
                    invocation_id: "goal-acceptance".into(),
                    tool_name: "workflow.stage_task_group".into(),
                    arguments: value,
                },
                host_context: vec![bcode_tool::ToolHostContextEntry {
                    schema: bcode_tool::TOOL_INVOCATION_SERVICE_ROUTES_SCHEMA.into(),
                    schema_version: 1,
                    payload: serde_json::json!([{
                        "route_id":"test-route",
                        "interface_id":bcode_workflow::WORKFLOW_APPLICATION_INTERFACE_ID,
                        "operations":["stage_run_graph_edit"]
                    }]),
                }],
            })
            .unwrap(),
        },
        config: bcode_plugin_sdk::PluginConfigContext::default(),
        events: bcode_plugin_sdk::ServiceEventEmitter::default(),
        cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
        bridge: bcode_plugin_sdk::ServiceBridge::default(),
        transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
    });
    assert!(response.error.is_none(), "{:?}", response.error);
    let prepared: bcode_tool::ToolPreparationResponse =
        serde_json::from_slice(&response.payload).unwrap();
    serde_json::from_value(prepared.descriptor["edit"].clone()).unwrap()
}

fn settle(store: &mut WorkflowStore, run: &str) {
    for _ in 0..8 {
        store.settle_pending_control_nodes(run, 100, 40).unwrap();
    }
}

fn complete(store: &mut WorkflowStore, node: &str, value: serde_json::Value) {
    let activation = store
        .pending_activations(100)
        .unwrap()
        .into_iter()
        .find(|activation| activation.node_id == node)
        .unwrap();
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: format!("{}:output", activation.activation_id),
            run_id: activation.run_id,
            node_id: activation.node_id,
            activation_id: activation.activation_id,
            schema_id: activation.node.output.type_name,
            schema_version: 1,
            value,
            artifact_reference: None,
            created_at_ms: 30,
        })
        .unwrap();
}

fn publish_group(
    store: &mut WorkflowStore,
    authority: &WorkflowExecutionAuthority,
    run: &str,
    source: &str,
    revision: u64,
    allocation: (u64, u64),
    transform: Option<bcode_workflow::WorkflowTransform>,
) -> (String, Option<bcode_workflow::WorkflowTransform>) {
    let (successor, first_edge) = allocation;
    let activation = store
        .pending_activations(100)
        .unwrap()
        .into_iter()
        .find(|activation| activation.node_id == source)
        .unwrap();
    let schema = serde_json::json!({"type_name":"contribution", "schema":{"type":"string"}});
    let task = |id: &str| serde_json::json!({"task_id":id,"objective":"Produce an identifiable contribution", "output":schema});
    let mut reconnect = serde_json::json!({"edge_id":successor,"node_id":"loop.evaluation"});
    if let Some(transform) = transform {
        reconnect["transform"] = serde_json::to_value(transform).unwrap();
    }
    let request = serde_json::json!({
        "version":2,"generated_ids":true,"mutation_id":format!("round-{revision}"),
        "run_id":run,"expected_revision":revision,"source_node_id":source,
        "bind_source_activation":activation.activation_id,
        "input":activation.node.output,"preserve_source_output":true,
        "worker_defaults":{"agent_profile":"plan"},
        "tasks":[task(&format!("left-{revision}")),task(&format!("right-{revision}"))],
        "continuation":{"objective":"Integrate and verify contributions; delegate correction if needed", "agent_profile":"plan"},
        "first_edge_id":first_edge,"reconnect":reconnect,"reconciliation":[]
    });
    let edit = prepare_group(request.clone());
    assert_eq!(edit, prepare_group(request));
    let edge = edit
        .edits
        .iter()
        .find_map(|edit| match edit {
            bcode_workflow::WorkflowRunGraphEdit::ReplaceEdge { edge, .. } => Some(edge.clone()),
            _ => None,
        })
        .unwrap();
    store.stage_run_graph_edit(&edit, authority, 20).unwrap();
    store
        .publish_retained_leaf_run_graph_edit(run, &edit.mutation_id, authority, 21)
        .unwrap();
    (edge.from, edge.transform)
}

// Exercises real goal generation/start construction and plugin lowering, but deliberately
// simulates agent outputs. It is not daemon, permission, provider or filesystem acceptance.
#[tokio::test]
async fn generated_goal_collects_named_workers_and_preserves_state_through_correction() {
    let host = Host {
        prerequisites: true,
        ..Host::default()
    };
    let mut surface = GoalSurface::new(Some(SessionId::new()));
    surface.editor.collaboration = CollaborationMode::Requested;
    surface.editor.worker_attempts = Some(30);
    surface.editor.prompt = text_state("Implement and verify two collaborating contributions");
    surface.editor.limit = text_state("2");
    surface.generate(&host, false);
    for _ in 0..8 {
        host.finish().await;
        surface.poll(&host);
    }
    let start = host.starts.lock().unwrap().first().unwrap().clone();
    verify_generated_goal(&start);
}

fn verify_generated_goal(start: &PluginWorkflowStartRequest) {
    let root = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open_in_state_dir(root.path()).unwrap();
    let authority = WorkflowExecutionAuthority {
        target_artifact_id: "test-artifact".into(),
        daemon_instance_id: "test-daemon".into(),
        generation: 1,
        fencing_token: "test-fence".into(),
    };
    let run = start.run_id.as_deref().unwrap();
    store
        .persist_definition("goal-acceptance", 1, &start.definition)
        .unwrap();
    store
        .create_run(&bcode_workflow_store::NewWorkflowRun {
            run_id: run.into(),
            definition_id: "goal-acceptance".into(),
            definition_version: 1,
            workspace_snapshot: root.path().to_string_lossy().into_owned(),
            parent_session_id: Some(start.parent_session_id.to_string()),
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(start.input.clone()),
            execution_authority: Some(authority.clone()),
            created_at_ms: 10,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".into(),
                profile_id: "build".into(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: bcode_workflow_store::WorkflowRunLimits {
                deadline_at_ms: None,
                node_execution_cap: start.limits.node_execution_cap,
                concurrency_cap: start.limits.concurrency_cap,
                cycle_cap: start.limits.cycle_cap,
                retry_cap: start.limits.retry_cap,
                recursion_depth_cap: start.limits.recursion_depth_cap,
                descendant_cap: start.limits.descendant_cap,
            },
        })
        .unwrap();
    if start.definition.nodes.contains_key("goal.initialization") {
        let mut ready = start.input.clone();
        ready["planning_ready"] = serde_json::json!(true);
        complete(&mut store, "goal.initialization", ready);
    }
    settle(&mut store, run);
    verify_delegation(&mut store, &authority, start);
}

fn verify_delegation(
    store: &mut WorkflowStore,
    authority: &WorkflowExecutionAuthority,
    start: &PluginWorkflowStartRequest,
) {
    let run = start.run_id.as_deref().unwrap();
    let successor = u64::try_from(
        start
            .definition
            .edges
            .iter()
            .position(|edge| edge.from == "loop.implementation" && edge.to == "loop.evaluation")
            .unwrap(),
    )
    .unwrap();
    let first_edge = u64::try_from(start.definition.edges.len()).unwrap();
    let (integration, transform) = publish_group(
        store,
        authority,
        run,
        "loop.implementation",
        1,
        (successor, first_edge),
        None,
    );
    complete(store, "loop.implementation", start.input.clone());
    settle(store, run);
    complete(store, "left-1", serde_json::json!("left contribution"));
    settle(store, run);
    assert_eq!(store.pending_activations(100).unwrap().len(), 1);
    complete(
        store,
        "right-1",
        serde_json::json!("right requires correction"),
    );
    settle(store, run);
    let envelope = serde_json::json!({"source":start.input,"results":{"left-1":"left contribution","right-1":"right requires correction"}});
    assert_eq!(
        store.pending_activations(100).unwrap()[0].input,
        Some(envelope.clone())
    );
    let (correction, _) = publish_group(
        store,
        authority,
        run,
        &integration,
        2,
        (successor, first_edge + 20),
        transform,
    );
    complete(store, &integration, envelope.clone());
    settle(store, run);
    for id in ["left-2", "right-2"] {
        complete(store, id, serde_json::json!("corrected contribution"));
    }
    settle(store, run);
    complete(
        store,
        &correction,
        serde_json::json!({"source":envelope,"results":{"left-2":"corrected contribution","right-2":"corrected contribution"}}),
    );
    settle(store, run);
    let pending = store.pending_activations(100).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].node_id, "loop.evaluation");
    assert_eq!(pending[0].input.as_ref(), Some(&start.input));
}
