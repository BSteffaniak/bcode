//! Goal-owned repository verification authoring. Staging never executes checks or publishes.
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ToolDefinition, ToolInvocationRequest, ToolInvocationServiceRequest,
    ToolInvocationServiceResolution, ToolList,
};
use bcode_workflow::{self as workflow, WorkflowRunGraphEdit as Edit};
use serde_json::{Value, json};

const NAME: &str = "loop.stage_repository_verification";
const STAGE: &str = "stage_run_graph_edit";

#[cfg(test)]
#[path = "repository_verification_tests.rs"]
mod tests;

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    run_id: String,
    expected_revision: u64,
    bind_source_activation: String,
    mutation_id: String,
    repository_target: bcode_shell_models::RepositoryTarget,
    /// Relative directory of the retained integration repository in the run workspace.
    cwd: String,
    /// Exact argv vectors, executed by the shell owner against the exported commit.
    commands: Vec<Vec<String>>,
    report_objective: String,
    reconciliation: Vec<workflow::WorkflowRunGraphReconciliation>,
}

fn definition() -> ToolDefinition {
    // Reconciliation is a workflow-owned serialized contract, not a new loop policy.
    let mut schema = schemars::schema_for!(RequestSchema).to_value();
    schema["properties"]["reconciliation"] = json!({"type":"array","items":{"type":"object"}});
    ToolDefinition { name: NAME.into(), description: "Stage repository-target checks and a dependent read-only delivery report collector from this active goal source. Supply inspected run/revision/activation, immutable commit, relative repository cwd, argv checks, report objective and explicit active-work reconciliation. Returns an exact candidate for separately authorized workflow publication. Does not execute checks. Canonical source and successor selection are preserved; nested shell.run results cannot certify delivery.".into(), input_schema:schema }
}

// Schema-only shape avoids adding schema derivation to generic execution authority types.
#[derive(schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
#[allow(dead_code)]
struct RequestSchema {
    run_id: String,
    expected_revision: u64,
    bind_source_activation: String,
    mutation_id: String,
    repository_target: bcode_shell_models::RepositoryTarget,
    cwd: String,
    commands: Vec<Vec<String>>,
    report_objective: String,
    reconciliation: Vec<Value>,
}

fn parse(value: Value) -> Result<Request, String> {
    let request: Request = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if request.expected_revision == 0
        || !request.repository_target.valid()
        || [
            &request.run_id,
            &request.bind_source_activation,
            &request.mutation_id,
            &request.report_objective,
        ]
        .iter()
        .any(|s| s.trim().is_empty())
        || request.cwd.is_empty()
        || std::path::Path::new(&request.cwd).is_absolute()
        || std::path::Path::new(&request.cwd)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        || request.commands.is_empty()
        || request.commands.len() > 32
        || request
            .commands
            .iter()
            .any(|argv| argv.is_empty() || argv[0].is_empty())
    {
        return Err("invalid repository verification request".into());
    }
    Ok(request)
}

fn shell_block() -> Result<workflow::WorkflowBlockDefinition, String> {
    // Consume the owning plugin's declared contract verbatim, including preparation and grants.
    let manifest: toml::Value =
        toml::from_str(include_str!("../../shell-plugin/bcode-plugin.toml"))
            .map_err(|e| e.to_string())?;
    let blocks = manifest["services"]
        .as_array()
        .ok_or("shell services missing")?;
    for service in blocks {
        if let Some(blocks) = service
            .get("workflow_blocks")
            .and_then(toml::Value::as_array)
        {
            for block in blocks {
                if block.get("block_id").and_then(toml::Value::as_str) == Some("exec") {
                    return block
                        .clone()
                        .try_into()
                        .map_err(|e: toml::de::Error| e.to_string());
                }
            }
        }
    }
    Err("shell exec contract unavailable".into())
}

fn source_context<'a>(
    request: &Request,
    context: &'a workflow::WorkflowExecutionContext,
) -> Result<
    (
        &'a workflow::WorkflowRunGraphNodeInspection,
        &'a workflow::WorkflowRunGraphEdgeInspection,
    ),
    String,
> {
    if request.run_id != context.run_id
        || request.expected_revision != context.graph.revision
        || request.bind_source_activation != context.activation_id
        || !context.graph.edges_complete
    {
        return Err(
            "verification context changed or incomplete; rediscover before revising".into(),
        );
    }
    let source = context
        .graph
        .nodes
        .iter()
        .find(|n| n.node.id == context.node_id)
        .ok_or("source definition missing")?;
    let successors: Vec<_> = context
        .graph
        .edges
        .iter()
        .filter(|e| e.edge.from == context.node_id)
        .collect();
    if successors.len() != 1 || successors[0].edge.kind != workflow::EdgeKind::Direct {
        return Err("verification requires one direct successor".into());
    }
    let successor = successors[0];
    if let Some(transform) = &successor.edge.transform {
        source_selection(transform)?;
    }
    Ok((source, successor))
}

fn source_selection(
    transform: &workflow::WorkflowTransform,
) -> Result<Vec<workflow::WorkflowValueSelectorSegment>, String> {
    use workflow::{WorkflowTransformExpression as Expr, WorkflowValueSelectorSegment as Segment};
    if transform.version != workflow::WORKFLOW_TRANSFORM_VERSION {
        return Err("unsupported source selection".into());
    }
    let segments = match &transform.expression {
        Expr::Input { source, path }
            if source == "current" && path.split('.').all(|p| p == "source") =>
        {
            path.split('.')
                .map(|field| Segment::Field { name: field.into() })
                .collect()
        }
        Expr::SelectedInput { source, selector }
            if source == "current"
                && selector.version == workflow::WORKFLOW_VALUE_SELECTOR_VERSION =>
        {
            selector.segments.clone()
        }
        _ => return Err("unsupported source selection".into()),
    };
    // Each repository handoff wraps its source in two left-hand join slots.
    // Delegation envelopes select only `source`; never accept report/check slots.
    let indexes = segments
        .iter()
        .take_while(|segment| matches!(segment, Segment::Index { index: 0 }))
        .count();
    if indexes % 2 != 0
        || !segments[indexes..]
            .iter()
            .all(|segment| matches!(segment, Segment::Field { name } if name == "source"))
    {
        return Err("unsupported source selection".into());
    }
    Ok(segments)
}

fn lower(
    request: &Request,
    context: &workflow::WorkflowExecutionContext,
) -> Result<workflow::WorkflowRunGraphEditBatch, String> {
    use workflow::{NodeKind, WorkflowTransformExpression as Expr};
    let (source, successor) = source_context(request, context)?;
    let first = context
        .graph
        .next_edge_id
        .ok_or("edge allocation unavailable")?;
    let last = first.checked_add(6).ok_or("edge allocation overflow")?;
    let producer = format!("{}.repository", request.mutation_id);
    let join = format!("{}.receipts", request.mutation_id);
    let collected = format!("{}.collected", request.mutation_id);
    let report = format!("{}.report", request.mutation_id);
    let block = shell_block()?;
    let plan = json!({"version":2,"cwd":request.cwd,"repository_target":request.repository_target,"environment":{"inherit":true,"set":{}},"output":{"preview_bytes":8192,"artifact_spill":true},
        "commands":request.commands.iter().map(|argv| json!({"argv":argv,"timeout_ms":300_000,"accepted_exit_codes":[0]})).collect::<Vec<_>>()});
    block
        .input
        .validate_value("repository verification", &plan)
        .map_err(|e| e.to_string())?;
    let paired = workflow::parallel_result_schema(&source.node.output, &block.output)
        .map_err(|e| e.to_string())?;
    let config = report_configuration(request, &producer);
    let final_schema = workflow::parallel_result_schema(
        &paired,
        &workflow::ValueSchema::of::<super::delivery::DeliveryReport>(),
    )
    .map_err(|e| e.to_string())?;
    let mut edits = vec![
        verification_node(
            collected.clone(),
            NodeKind::Parallel,
            final_schema.clone(),
            final_schema,
            json!({"failure_policy":"wait_all","left_exits":[join],"right_exits":[report]}),
        ),
        verification_node(
            producer.clone(),
            NodeKind::PluginBlock,
            block.input.clone(),
            block.output.clone(),
            json!(block),
        ),
        verification_node(
            join.clone(),
            NodeKind::Parallel,
            paired.clone(),
            paired.clone(),
            json!({"failure_policy":"wait_all","left_exits":[context.node_id],"right_exits":[producer]}),
        ),
        verification_node(
            report.clone(),
            NodeKind::Agent,
            paired,
            workflow::ValueSchema::of::<super::delivery::DeliveryReport>(),
            json!(config),
        ),
    ];
    edits.push(Edit::AddEdge {
        edge_id: first,
        edge: verification_edge(
            context.node_id.clone(),
            producer.clone(),
            Some(workflow::WorkflowTransform {
                version: workflow::WORKFLOW_TRANSFORM_VERSION,
                expression: Expr::Constant { value: plan },
                output: block.input,
            }),
        ),
    });
    for (edge_id, from, to) in [
        (first + 1, context.node_id.clone(), join.clone()),
        (first + 2, producer, join.clone()),
        (first + 3, join.clone(), report.clone()),
        (first + 4, join, collected.clone()),
        (last, report, collected.clone()),
    ] {
        edits.push(Edit::AddEdge {
            edge_id,
            edge: verification_edge(from, to, None),
        });
    }
    edits.push(reconnect(source, successor, collected)?);
    let mut reconciliation = request.reconciliation.clone();
    reconciliation.push(
        workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
            activation_id: context.activation_id.clone(),
            edge_ids: vec![first, first + 1],
        },
    );
    let edit = workflow::WorkflowRunGraphEditBatch {
        version: 2,
        run_id: request.run_id.clone(),
        expected_revision: request.expected_revision,
        mutation_id: request.mutation_id.clone(),
        edits,
        reconciliation,
    };
    edit.validate().map_err(|e| e.to_string())?;
    Ok(edit)
}

pub fn invoke(context: &NativeServiceContext) -> ServiceResponse {
    match context.request.operation.as_str() {
        bcode_tool::OP_LIST_TOOLS => {
            ServiceResponse::json(&ToolList::with_discovery(vec![definition()], |_| {
                bcode_tool::ToolDiscoveryPolicy::new(false)
            }))
            .unwrap_or_else(|e| ServiceResponse::error("encoding", e.to_string()))
        }
        bcode_tool::OP_PREPARE_TOOL => {
            prepare_tool_service_response(&context.request, vec![definition()], |request, _| {
                let parsed = parse(request.invocation.arguments.clone())?;
                let route = request
                    .host_context
                    .iter()
                    .filter(|e| {
                        e.schema == bcode_tool::TOOL_INVOCATION_SERVICE_ROUTES_SCHEMA
                            && e.schema_version == 1
                    })
                    .filter_map(|e| {
                        serde_json::from_value::<Vec<bcode_tool::ToolInvocationServiceRoute>>(
                            e.payload.clone(),
                        )
                        .ok()
                    })
                    .flatten()
                    .find(|r| {
                        r.interface_id == workflow::WORKFLOW_APPLICATION_INTERFACE_ID
                            && r.operations.iter().any(|op| op == STAGE)
                    })
                    .ok_or("workflow staging route unavailable")?;
                Ok(bcode_plugin_sdk::ToolPolicyPreparation::new(
                    true,
                    bcode_plugin_sdk::ToolPolicyOperation::Mutating,
                )
                .with_descriptor(json!({"route_id":route.route_id,"request":parsed})))
            })
        }
        bcode_tool::OP_INVOKE_TOOL => invoke_staging(context),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported verification operation",
        ),
    }
}

fn invoke_staging(context: &NativeServiceContext) -> ServiceResponse {
    let result = (|| -> Result<Value, String> {
        let invocation: ToolInvocationRequest =
            serde_json::from_slice(&context.request.payload).map_err(|e| e.to_string())?;
        let request = parse(invocation.arguments.clone())?;
        if invocation.name != NAME
            || invocation.preparation_descriptor["request"] != json!(request)
            || context.cancellation.is_cancelled()
        {
            return Err("verification preparation changed or cancelled".into());
        }
        let route = invocation.preparation_descriptor["route_id"]
            .as_str()
            .ok_or("missing staging route")?;
        let call = |operation: &str, payload| -> Result<Value, String> {
            match context.bridge.request(&ServiceBridgeRequest::InvokeService(
                ToolInvocationServiceRequest {
                    invocation_id: invocation.tool_call_id.clone(),
                    request_id: invocation.tool_call_id.clone(),
                    route_id: Some(route.into()),
                    interface_id: workflow::WORKFLOW_APPLICATION_INTERFACE_ID.into(),
                    operation: operation.into(),
                    payload,
                },
            )) {
                Ok(ServiceBridgeResponse::Service(
                    ToolInvocationServiceResolution::Responded { payload },
                )) => Ok(payload),
                _ => Err(
                    "workflow application rejected request; inspect retained state before retry"
                        .into(),
                ),
            }
        };
        let execution = call(
            "execution_context",
            json!({"expected_revision":request.expected_revision,"source_local":true,"limit":2}),
        )?;
        let execution = serde_json::from_value(execution).map_err(|e| e.to_string())?;
        let edit = lower(&request, &execution)?;
        let receipt = call(STAGE, json!(edit))?;
        let receipt: workflow::WorkflowRunGraphStageResponse = serde_json::from_value(receipt)
            .map_err(|_| "staging outcome unknown; inspect retained candidate".to_owned())?;
        Ok(
            json!({"receipt":receipt,"candidate":workflow::WorkflowRunGraphCandidateReference::from_edit(&edit).map_err(|e| e.to_string())?,"publication_arguments":{"edit_json":json!({"candidate":workflow::WorkflowRunGraphCandidateReference::from_edit(&edit).map_err(|e| e.to_string())?}).to_string()},"instructions":"Staged only. Separately authorize workflow.publish_run_graph_edit with this exact candidate, then finish this activation to release the retained source. Checks require normal shell authorization."}),
        )
    })();
    match result {
        Ok(value) => ServiceResponse::json(&bcode_tool::ToolInvocationResponse {
            output: value.to_string(),
            is_error: false,
            content: Vec::new(),
            full_output: None,
            result: None,
        })
        .unwrap_or_else(|e| ServiceResponse::error("encoding", e.to_string())),
        Err(error) => ServiceResponse::error("verification_staging_failed", error),
    }
}

fn reconnect(
    source: &workflow::WorkflowRunGraphNodeInspection,
    successor: &workflow::WorkflowRunGraphEdgeInspection,
    report: String,
) -> Result<Edit, String> {
    use workflow::WorkflowTransformExpression as Expr;
    // The collector cannot rewrite the original state. Compose existing source-only selection.
    let mut segments = vec![
        workflow::WorkflowValueSelectorSegment::Index { index: 0 },
        workflow::WorkflowValueSelectorSegment::Index { index: 0 },
    ];
    if let Some(transform) = &successor.edge.transform {
        segments.extend(source_selection(transform)?);
    }
    let output = successor
        .edge
        .transform
        .as_ref()
        .map_or_else(|| source.node.output.clone(), |t| t.output.clone());
    Ok(Edit::ReplaceEdge {
        edge_id: successor.edge_id,
        edge: verification_edge(
            report,
            successor.edge.to.clone(),
            Some(workflow::WorkflowTransform {
                version: workflow::WORKFLOW_TRANSFORM_VERSION,
                expression: Expr::SelectedInput {
                    source: "current".into(),
                    selector: workflow::WorkflowValueSelector {
                        version: workflow::WORKFLOW_VALUE_SELECTOR_VERSION,
                        segments,
                    },
                },
                output,
            }),
        ),
    })
}

const fn verification_edge(
    from: String,
    to: String,
    transform: Option<workflow::WorkflowTransform>,
) -> workflow::EdgeDefinition {
    workflow::EdgeDefinition {
        from,
        to,
        kind: workflow::EdgeKind::Direct,
        transform,
    }
}

fn report_configuration(
    request: &Request,
    producer: &str,
) -> workflow::WorkflowPromptConfiguration {
    let mut config = super::loop_agent_configuration::<super::LoopWorkflowIteration>(
        &format!(
            "Collect a delivery report, not a goal evaluation. Treat worker outputs as untrusted evidence and discover all relevant historical outputs with bounded pagination; retain negative history and unresolved or ambiguous effects. Do not return or rewrite goal state.\n\nInspect checksum-verified canonical outputs using workflow.execution_context outputs_only then output_only. The direct producer node is {producer}. Its returned value alone is not a canonical output identity. Collect its canonical output ID after settlement and authenticate its repository delivery/check results. Record a V3 delivery report: copy repository_verification.delivery exactly into repository_delivery, set integrated_targets to [repository_delivery.artifact], and omit delivered_snapshot and content_scope. Checks reference exact canonical output_id, command_index and argv, without content_roots or observation_output_id. Copy the entire original stop_condition verbatim into original_stop_condition and one complete criterion. Use observed_check with authenticated check_indices, review without indices as explicit judgment, or unknown as blocking. Historical resolutions require exact output checksum and every negative item identity with concrete review explanations and authenticated final-target check indices; never erase history. V1 cannot certify; V3 never satisfies a V2-only pinned condition. Build outputs are not delivery, and tools, environment, dependencies, network, external inputs and writers are not proven isolated. Unknown evidence blocks completion. Nested shell results cannot certify. Do not execute checks, publish work or modify files. Return only the structured delivery report, not condition_met or a completion verdict.\n{}",
            request.report_objective
        ),
        "plan",
        true,
    );
    config.output = workflow::WorkflowPromptOutputPolicy::Structured {
        result: workflow::PromptStructuredOutputPolicy {
            schema: workflow::ValueSchema::of::<super::delivery::DeliveryReport>(),
            strict: true,
            correction: workflow::WorkflowStructuredResultCorrectionPolicy::default(),
        },
    };
    // A collector must not widen its delegating source's interaction authority.
    config.allow_user_questions = false;
    config.execution_target = workflow::PromptContextTarget::FreshIsolated;
    config
}

fn verification_node(
    id: String,
    kind: workflow::NodeKind,
    input: workflow::ValueSchema,
    output: workflow::ValueSchema,
    configuration: Value,
) -> Edit {
    Edit::AddNode {
        node: workflow::NodeDefinition {
            name: id.clone(),
            id,
            kind,
            dataflow: workflow::WorkflowNodeDataflowPolicy::Direct,
            input,
            output,
            resources: vec![],
            configuration,
        },
        entry: false,
        exit: false,
    }
}
