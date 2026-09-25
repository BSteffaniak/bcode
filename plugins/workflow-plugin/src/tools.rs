//! Plugin-owned execution-scoped graph staging tool.

use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ToolDefinition, ToolInvocationRequest, ToolInvocationServiceRequest,
    ToolInvocationServiceResolution, ToolList,
};
use bcode_workflow::{WORKFLOW_APPLICATION_INTERFACE_ID, WorkflowRunGraphEditBatch};
use serde_json::json;

mod task_group;
const GROUP_NAME: &str = "workflow.stage_task_group";

const CONTEXT_NAME: &str = "workflow.execution_context";
const CONTEXT_OPERATION: &str = "execution_context";

fn parse_context(
    mut arguments: serde_json::Value,
) -> Result<bcode_workflow::WorkflowExecutionContextRequest, String> {
    arguments
        .as_object_mut()
        .ok_or("execution context request must be an object")?
        .entry("limit")
        .or_insert(json!(50));
    let context: bcode_workflow::WorkflowExecutionContextRequest =
        serde_json::from_value(arguments)
            .map_err(|_| "invalid execution context request".to_owned())?;
    if !(1..=100).contains(&context.limit) {
        return Err("invalid execution context limit".into());
    }
    Ok(context)
}

fn context_definition() -> ToolDefinition {
    ToolDefinition {
        name: CONTEXT_NAME.to_owned(),
        description: "Read this active workflow execution's authenticated identity and bounded graph page. Omit revision and cursors initially; continue with the returned revision and last node/edge identities. Restart on revision conflict. This grants no mutation authority.".to_owned(),
        input_schema: json!({"type":"object", "additionalProperties":false,
            "properties": {
                "after_output_id":{"type":["string","null"], "description":"Exclusive last output ID. Outputs arriving behind the cursor require a fresh scan; this is not a durable event stream."},
                "output_id":{"type":["string","null"], "description":"Exact canonical output identity from this run; returns checksum-verified value without opening artifacts."},
                "limit":{"type":"integer", "minimum":1, "maximum":100, "default":50},
                "expected_revision":{"type":["integer","null"], "minimum":1},
                "after_node_id":{"type":["string","null"]},
                "after_edge_id":{"type":["integer","null"]}
            }}),
    }
}

const TASK_NAME: &str = "workflow.stage_agent_task";
const PROMPT_TASK_NAME: &str = "workflow.stage_prompt_task";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptTaskRequest {
    run_id: String,
    expected_revision: u64,
    mutation_id: String,
    task_id: String,
    objective: String,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
    #[serde(default)]
    tool_allowlist: Vec<String>,
    #[serde(default)]
    timeout_ms: Option<std::num::NonZeroU64>,
    #[serde(default)]
    model_selection: Option<task_group::ModelSelection>,
    #[serde(default)]
    context: bcode_workflow::PromptContextTarget,
    #[serde(default)]
    resources: Vec<bcode_workflow::ResourceClaim>,
    agent_profile: String,
    input: bcode_workflow::ValueSchema,
    #[serde(default = "task_group::default_worker_output")]
    output: bcode_workflow::ValueSchema,
    entry: bool,
    exit: bool,
    #[serde(default)]
    edges: Vec<AgentTaskEdge>,
    #[serde(default)]
    depends_on: Option<PromptTaskDependency>,
    reconciliation: Vec<bcode_workflow::WorkflowRunGraphReconciliation>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptTaskDependency {
    node_id: String,
    edge_id: u64,
}

fn prompt_task_definition() -> ToolDefinition {
    ToolDefinition {
        name: PROMPT_TASK_NAME.into(),
        description: "Stage a read-only prompt task in this active workflow. Supply an objective, agent profile and typed input/output schemas, not a NodeDefinition. Defaults to a fresh model context in the run workspace; optional context selects pinned fork or sequential parent (never filesystem isolation). Entry tasks consume run input; other tasks require explicit dependency edges. Returns the exact candidate for separate authorized publication; does not dispatch or wait.".into(),
        input_schema: json!({"type":"object","additionalProperties":false,
            "required":["run_id","expected_revision","mutation_id","task_id","objective",
                "agent_profile","input","entry","exit","reconciliation"],
            "properties":{
                "run_id":{"type":"string"},"expected_revision":{"type":"integer","minimum":1},
                "mutation_id":{"type":"string"},"task_id":{"type":"string"},
                "acceptance_criteria":{"type":"array","items":{"type":"string","minLength":1},"description":"Evidence requirements included in the task prompt; not an automatic completion verdict. Blank criteria reject."},
                "timeout_ms":{"type":"integer","minimum":1,"description":"Positive per-task timeout; omission retains canonical default. Does not extend run allowances."},
                "tool_allowlist":{"type":"array","items":{"type":"string","minLength":1},"description":"Restricts tools without granting authority. Empty retains normal agent-policy selection; blank entries reject."},
                "model_selection":{"type":"object","additionalProperties":false,"required":["provider","model"],"properties":{"provider":{"type":"string","minLength":1},"model":{"type":"string","minLength":1}},"description":"Optional selection through normal model resolution; grants no capability or permission."},
                "context":{"type":"string","enum":["fresh_isolated","fixed_generation_fork","shared_parent_sequential"],"default":"fresh_isolated","description":"Model context only, not filesystem isolation or authority. Fork uses pinned parent generation; shared parent executes sequentially."},
                "resources":{"type":"array","description":"Canonical scheduler claims, not tool authority or filesystem isolation. Omission declares no claims.","items":{"type":"object","additionalProperties":false,"required":["resource","access"],"properties":{"resource":{"type":"string","minLength":1},"access":{"type":"string","enum":["read","write"]}}}},
                "depends_on":{"type":"object","additionalProperties":false,"required":["node_id","edge_id"],"properties":{"node_id":{"type":"string","minLength":1},"edge_id":{"type":"integer","minimum":0}},"description":"Direct dependency on one existing source; requires entry:false. Input must match its output. Supply an unused edge ID. Does not remove existing successors or publish."},
                "objective":{"type":"string","minLength":1},"agent_profile":{"type":"string","minLength":1},
                "input":{"type":"object","description":"ValueSchema with type_name and schema"},
                "output":{"type":"object","description":"Optional ValueSchema; omission uses bounded bcode.delegated_task_result.v1 with summary, evidence and blockers. Explicit null rejects."},
                "entry":{"type":"boolean"},"exit":{"type":"boolean"},
                "edges":{"type":"array"},"reconciliation":{"type":"array"}
            }}),
    }
}

fn parse_prompt_task(arguments: &serde_json::Value) -> Result<AgentTaskRequest, String> {
    let mut task: PromptTaskRequest = serde_json::from_value(arguments.clone())
        .map_err(|_| "invalid prompt task request".to_owned())?;
    if let Some(dependency) = task.depends_on.take() {
        if task.entry || dependency.node_id.trim().is_empty() || dependency.node_id == task.task_id
        {
            return Err("depends_on requires a non-entry task and distinct nonblank source".into());
        }
        task.edges.push(AgentTaskEdge {
            edge_id: dependency.edge_id,
            edge: bcode_workflow::EdgeDefinition {
                from: dependency.node_id,
                to: task.task_id.clone(),
                kind: bcode_workflow::EdgeKind::Direct,
                transform: None,
            },
        });
    }
    if task.objective.trim().is_empty() || task.agent_profile.trim().is_empty() {
        return Err("prompt task requires an objective and agent profile".into());
    }
    let mut configuration = bcode_workflow::WorkflowPromptConfiguration::structured(
        task.agent_profile,
        task.output.clone(),
        task_group::task_instructions(task.objective, &task.acceptance_criteria)?,
    );
    if task
        .tool_allowlist
        .iter()
        .any(|tool| tool.trim().is_empty())
    {
        return Err("tool allowlist entries must not be empty".into());
    }
    configuration.execution_target = task.context;
    configuration.tool_allowlist = task.tool_allowlist;
    if let Some(timeout) = task.timeout_ms {
        configuration.timeout_ms = timeout.get();
    }
    if let Some(selection) = task.model_selection {
        configuration = selection.apply(configuration)?;
    }
    Ok(AgentTaskRequest {
        run_id: task.run_id,
        expected_revision: task.expected_revision,
        mutation_id: task.mutation_id,
        node: bcode_workflow::NodeDefinition {
            id: task.task_id.clone(),
            name: task.task_id,
            kind: bcode_workflow::NodeKind::Agent,
            dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
            input: task.input,
            output: task.output,
            resources: task.resources,
            configuration: serde_json::to_value(configuration)
                .map_err(|error| error.to_string())?,
        },
        entry: task.entry,
        exit: task.exit,
        edges: task.edges,
        reconciliation: task.reconciliation,
    })
}

/// Plugin-owned shorthand; canonical graph publication still owns admission.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentTaskRequest {
    run_id: String,
    expected_revision: u64,
    mutation_id: String,
    node: bcode_workflow::NodeDefinition,
    entry: bool,
    exit: bool,
    #[serde(default)]
    edges: Vec<AgentTaskEdge>,
    reconciliation: Vec<bcode_workflow::WorkflowRunGraphReconciliation>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentTaskEdge {
    edge_id: u64,
    edge: bcode_workflow::EdgeDefinition,
}

fn task_definition() -> ToolDefinition {
    ToolDefinition {
        name: TASK_NAME.to_owned(),
        description: "Stage one executable agent task in this active run. Supply an exact Agent NodeDefinition with WorkflowPromptConfiguration, explicit entry/exit flags, and active-work reconciliation. Entry tasks receive the run input; non-entry tasks require connected edges before publication. Staging does not dispatch. Publish the exact returned candidate separately with workflow.publish_run_graph_edit; publication and normal execution authorization remain required.".to_owned(),
        input_schema: json!({"type":"object","additionalProperties":false,
            "required":["run_id","expected_revision","mutation_id","node","entry","exit","reconciliation"],
            "properties":{
                "run_id":{"type":"string"},"expected_revision":{"type":"integer","minimum":1},
                "mutation_id":{"type":"string"},"node":{"type":"object"},
                "edges":{"type":"array","description":"Edges staged atomically with the task; canonical graph validation checks endpoints and identities.","items":{"type":"object","additionalProperties":false,"required":["edge_id","edge"],"properties":{"edge_id":{"type":"integer","minimum":1},"edge":{"type":"object"}}}},
                "entry":{"type":"boolean"},"exit":{"type":"boolean"},
                "reconciliation":{"type":"array"}
            }}),
    }
}

fn parse_tool_edit(
    name: &str,
    arguments: &serde_json::Value,
) -> Result<WorkflowRunGraphEditBatch, String> {
    if name == GROUP_NAME {
        return task_group::parse(arguments);
    }
    if name != TASK_NAME && name != PROMPT_TASK_NAME {
        return parse_edit(arguments);
    }
    let task: AgentTaskRequest = if name == PROMPT_TASK_NAME {
        parse_prompt_task(arguments)?
    } else {
        serde_json::from_value(arguments.clone())
            .map_err(|_| "invalid agent task request".to_owned())?
    };
    if task.node.kind != bcode_workflow::NodeKind::Agent {
        return Err("agent task requires an Agent node".to_owned());
    }
    let _: bcode_workflow::WorkflowPromptConfiguration =
        serde_json::from_value(task.node.configuration.clone())
            .map_err(|_| "invalid agent prompt configuration".to_owned())?;
    let mut edits = vec![bcode_workflow::WorkflowRunGraphEdit::AddNode {
        node: task.node,
        entry: task.entry,
        exit: task.exit,
    }];
    edits.extend(task.edges.into_iter().map(|edge| {
        bcode_workflow::WorkflowRunGraphEdit::AddEdge {
            edge_id: edge.edge_id,
            edge: edge.edge,
        }
    }));
    let edit = WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: task.run_id,
        expected_revision: task.expected_revision,
        mutation_id: task.mutation_id,
        edits,
        reconciliation: task.reconciliation,
    };
    edit.validate()
        .map_err(|_| "invalid agent task facts".to_owned())?;
    Ok(edit)
}

const NAME: &str = "workflow.stage_run_graph_edit";
const OPERATION: &str = "stage_run_graph_edit";
const PUBLISH_NAME: &str = "workflow.publish_run_graph_edit";
const PUBLISH_OPERATION: &str = "publish_run_graph_edit";
const ACCEPT_NAME: &str = "workflow.accept_run_graph_publication";
const ACCEPT_OPERATION: &str = "accept_run_graph_publication";

fn acceptance_definition() -> ToolDefinition {
    ToolDefinition {
        name: ACCEPT_NAME.to_owned(),
        description: "Accept an exact staged workflow edit requiring cancellation of receipt-backed work. Requires publication authorization. Acceptance durably requests cancellation; it does not mean the graph is published. Retry the identical candidate to observe its status while this execution remains active. A conflict preserves cancellation already requested and requires an explicitly revised candidate, never a silent rebase.".to_owned(),
        input_schema: edit_input_schema(),
    }
}

fn publication_definition() -> ToolDefinition {
    ToolDefinition {
        name: PUBLISH_NAME.to_owned(),
        description: "Publish the exact edit returned by staging for this active execution, using edit directly (or legacy edit_json). Requires separate publication authorization and explicit active-work reconciliation. Preserve the staged edit and mutation_id exactly when retrying; unsupported topology is rejected.".to_owned(),
        input_schema: edit_input_schema(),
    }
}

fn operation(name: &str) -> Result<&'static str, String> {
    match name {
        NAME | TASK_NAME | PROMPT_TASK_NAME | GROUP_NAME => Ok(OPERATION),
        PUBLISH_NAME => Ok(PUBLISH_OPERATION),
        ACCEPT_NAME => Ok(ACCEPT_OPERATION),
        _ => Err("unsupported workflow tool".to_owned()),
    }
}

fn definition() -> ToolDefinition {
    ToolDefinition {
        name: NAME.to_owned(),
        description: "Stage a revision-checked edit for the workflow run owning this active execution. Requires workflow application authorization. Does not publish or execute topology. Supply a WorkflowRunGraphEditBatch as edit (or legacy edit_json); preserve mutation_id when retrying.".to_owned(),
        input_schema: edit_input_schema(),
    }
}

fn edit_input_schema() -> serde_json::Value {
    json!({"type":"object","additionalProperties":false,
        "properties":{"edit":{"type":"object","description":"Exact edit object returned by staging; do not modify it."},
        "edit_json":{"type":"string","description":"Legacy JSON-encoded WorkflowRunGraphEditBatch."}},
        "oneOf":[{"required":["edit"]},{"required":["edit_json"]}]})
}

fn parse_edit(arguments: &serde_json::Value) -> Result<WorkflowRunGraphEditBatch, String> {
    let envelope = arguments
        .as_object()
        .ok_or("workflow edit request must be an object")?;
    if envelope
        .keys()
        .any(|key| key != "edit" && key != "edit_json")
    {
        return Err("unknown workflow edit request field".into());
    }
    let edit: WorkflowRunGraphEditBatch = match (arguments.get("edit"), arguments.get("edit_json"))
    {
        (Some(edit), None) => serde_json::from_value(edit.clone()),
        (None, Some(serde_json::Value::String(text))) => serde_json::from_str(text),
        _ => return Err("supply exactly one of edit or edit_json".into()),
    }
    .map_err(|_| "invalid workflow edit representation".to_owned())?;
    edit.validate()
        .map_err(|_| "invalid workflow edit facts".to_owned())?;
    Ok(edit)
}

/// Dispatch the plugin-owned workflow tool service.
pub fn invoke(context: &NativeServiceContext) -> ServiceResponse {
    match context.request.operation.as_str() {
        bcode_tool::OP_LIST_TOOLS => super::json_response(&ToolList {
            tools: vec![
                definition(),
                publication_definition(),
                acceptance_definition(),
                context_definition(),
                task_definition(),
                prompt_task_definition(),
                task_group::definition(),
            ],
        }),
        bcode_tool::OP_PREPARE_TOOL => prepare_tool_service_response(
            &context.request,
            [
                definition(),
                publication_definition(),
                acceptance_definition(),
                context_definition(),
                task_definition(),
                prompt_task_definition(),
                task_group::definition(),
            ],
            |request, _| {
                let is_context = request.invocation.tool_name == CONTEXT_NAME;
                let operation = if is_context {
                    CONTEXT_OPERATION
                } else {
                    operation(&request.invocation.tool_name)?
                };
                let payload = if is_context {
                    let context = parse_context(request.invocation.arguments.clone())?;
                    serde_json::to_value(context).map_err(|error| error.to_string())?
                } else {
                    serde_json::to_value(parse_tool_edit(
                        &request.invocation.tool_name,
                        &request.invocation.arguments,
                    )?)
                    .map_err(|error| error.to_string())?
                };
                let route = request
                    .host_context
                    .iter()
                    .filter(|entry| {
                        entry.schema == bcode_tool::TOOL_INVOCATION_SERVICE_ROUTES_SCHEMA
                            && entry.schema_version == 1
                    })
                    .filter_map(|entry| {
                        serde_json::from_value::<Vec<bcode_tool::ToolInvocationServiceRoute>>(
                            entry.payload.clone(),
                        )
                        .ok()
                    })
                    .flatten()
                    .find(|route| {
                        route.interface_id == WORKFLOW_APPLICATION_INTERFACE_ID
                            && route.operations.iter().any(|op| op == operation)
                    })
                    .ok_or_else(|| "workflow staging route is unavailable".to_owned())?;
                Ok(bcode_plugin_sdk::ToolPolicyPreparation::new(
                    !is_context,
                    if is_context {
                        bcode_plugin_sdk::ToolPolicyOperation::ReadOnly
                    } else {
                        bcode_plugin_sdk::ToolPolicyOperation::Mutating
                    },
                )
                .with_descriptor(
                    json!({"route_id":route.route_id, "operation": operation, "edit": payload}),
                ))
            },
        ),
        bcode_tool::OP_INVOKE_TOOL => invoke_edit(context),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported workflow tool operation",
        ),
    }
}

fn prepared_edit_matches(
    descriptor: &serde_json::Value,
    operation: &str,
    edit: &WorkflowRunGraphEditBatch,
) -> bool {
    descriptor
        .get("operation")
        .and_then(serde_json::Value::as_str)
        == Some(operation)
        && descriptor
            .get("edit")
            .cloned()
            .and_then(|value| serde_json::from_value::<WorkflowRunGraphEditBatch>(value).ok())
            .as_ref()
            == Some(edit)
}

fn invoke_context(
    context: &NativeServiceContext,
    request: ToolInvocationRequest,
) -> ServiceResponse {
    let Ok(query) = parse_context(request.arguments) else {
        return ServiceResponse::error("invalid_request", "invalid execution context request");
    };
    let Ok(payload) = serde_json::to_value(query) else {
        return ServiceResponse::error("invalid_request", "invalid execution context request");
    };
    let descriptor = &request.preparation_descriptor;
    if descriptor
        .get("operation")
        .and_then(serde_json::Value::as_str)
        != Some(CONTEXT_OPERATION)
        || descriptor.get("edit") != Some(&payload)
        || context.cancellation.is_cancelled()
    {
        return ServiceResponse::error(
            "invalid_request",
            "execution context preparation is not current",
        );
    }
    let Some(route_id) = descriptor
        .get("route_id")
        .and_then(serde_json::Value::as_str)
    else {
        return ServiceResponse::error("invalid_request", "execution context route missing");
    };
    match context.bridge.request(&ServiceBridgeRequest::InvokeService(
        ToolInvocationServiceRequest {
            invocation_id: request.tool_call_id.clone(),
            request_id: request.tool_call_id,
            route_id: Some(route_id.to_owned()),
            interface_id: WORKFLOW_APPLICATION_INTERFACE_ID.to_owned(),
            operation: CONTEXT_OPERATION.to_owned(),
            payload,
        },
    )) {
        Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded {
            payload,
        })) => serde_json::from_value::<bcode_workflow::WorkflowExecutionContext>(payload)
            .map_or_else(
                |_| ServiceResponse::error("invalid_response", "invalid workflow context"),
                |context| {
                    super::json_response(&bcode_tool::ToolInvocationResponse {
                        output: serde_json::to_string(&context).unwrap_or_default(),
                        is_error: false,
                        content: Vec::new(),
                        full_output: None,
                        result: None,
                    })
                },
            ),
        _ => ServiceResponse::error("context_unavailable", "active workflow context unavailable"),
    }
}

fn invoke_edit(context: &NativeServiceContext) -> ServiceResponse {
    let Ok(request) = context.request.payload_json::<ToolInvocationRequest>() else {
        return ServiceResponse::error("invalid_request", "invalid workflow tool invocation");
    };
    if request.name == CONTEXT_NAME {
        return invoke_context(context, request);
    }
    let Ok(operation) = operation(&request.name) else {
        return ServiceResponse::error("unsupported_tool", "unsupported workflow tool");
    };
    let edit = match parse_tool_edit(&request.name, &request.arguments) {
        Ok(edit) => edit,
        Err(message) => return ServiceResponse::error("invalid_request", message),
    };
    if !prepared_edit_matches(&request.preparation_descriptor, operation, &edit) {
        return ServiceResponse::error(
            "invalid_request",
            "workflow edit differs from prepared authorization",
        );
    }
    if operation == PUBLISH_OPERATION {
        // Evaluate locally: calling our own service through the bridge would re-enter
        // the exclusive plugin slot. The host independently derives this plugin's
        // authenticated identity; no policy decision or actor is sent as authority.
        let facts = bcode_workflow::WorkflowRunGraphPublicationFacts {
            version: 1,
            actor: bcode_workflow::WorkflowApplicationActor {
                kind: bcode_workflow::WorkflowApplicationActorKind::Plugin,
                actor_id: super::PLUGIN_ID.to_owned(),
            },
            request: edit.clone(),
        };
        if super::publication_policy(&facts)
            == bcode_workflow::WorkflowPublicationPolicyDecision::Deny
        {
            return ServiceResponse::error(
                "publication_denied",
                "workflow publication denied by plugin policy",
            );
        }
    }
    if context.cancellation.is_cancelled() {
        return ServiceResponse::error("cancelled", "workflow staging cancelled");
    }
    let Some(route_id) = request
        .preparation_descriptor
        .get("route_id")
        .and_then(serde_json::Value::as_str)
    else {
        return ServiceResponse::error("invalid_request", "workflow route preparation is missing");
    };
    let response = context.bridge.request(&ServiceBridgeRequest::InvokeService(
        ToolInvocationServiceRequest {
            invocation_id: request.tool_call_id,
            request_id: edit.mutation_id.clone(),
            route_id: Some(route_id.to_owned()),
            interface_id: WORKFLOW_APPLICATION_INTERFACE_ID.to_owned(),
            operation: operation.to_owned(),
            payload: match serde_json::to_value(&edit) {
                Ok(payload) => payload,
                Err(_) => {
                    return ServiceResponse::error(
                        "invalid_request",
                        "cannot encode workflow edit",
                    );
                }
            },
        },
    ));
    match response {
        Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded {
            payload,
        })) => {
            if operation == ACCEPT_OPERATION {
                acceptance_response(payload)
            } else if operation == PUBLISH_OPERATION {
                publication_response(&payload)
            } else if matches!(
                request.name.as_str(),
                TASK_NAME | PROMPT_TASK_NAME | GROUP_NAME
            ) {
                task_staging_response(payload, &edit)
            } else {
                staging_response(payload)
            }
        }
        Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Cancelled)) => {
            ServiceResponse::error("cancelled", "workflow staging cancelled")
        }
        _ => ServiceResponse::error(
            "workflow_staging_failed",
            "workflow edit was not admitted; verify route, policy, and active execution",
        ),
    }
}

fn task_staging_response(
    payload: serde_json::Value,
    edit: &WorkflowRunGraphEditBatch,
) -> ServiceResponse {
    let Ok(staged) =
        serde_json::from_value::<bcode_workflow::WorkflowRunGraphStageResponse>(payload)
    else {
        return ServiceResponse::error("invalid_response", "task staging outcome unknown");
    };
    super::json_response(&bcode_tool::ToolInvocationResponse {
        output: json!({"staged":staged.staged,"published":false,"edit":edit}).to_string(),
        is_error: false,
        content: Vec::new(),
        full_output: None,
        result: None,
    })
}

fn acceptance_response(payload: serde_json::Value) -> ServiceResponse {
    use bcode_workflow::WorkflowRunGraphPublicationStatus as Status;
    let Ok(status) = serde_json::from_value::<Status>(payload) else {
        return ServiceResponse::error(
            "invalid_response",
            "unsupported acceptance response; outcome is unknown",
        );
    };
    let output = match status {
        Status::Pending { expected_revision } if expected_revision > 0 => format!(
            "Workflow publication pending against revision {expected_revision}. Cancellation has been requested; topology has not been published."
        ),
        Status::Conflicted {
            expected_revision,
            current_revision,
        } if expected_revision > 0 && current_revision >= expected_revision => format!(
            "Workflow publication conflicted: expected revision {expected_revision}, current revision {current_revision}. Equal revisions indicate a conflicting terminal outcome. Already-requested cancellation remains durable. Author and stage a revised candidate explicitly."
        ),
        Status::Committed { revision } if revision > 1 => {
            format!("Workflow edit published at revision {revision}.")
        }
        _ => {
            return ServiceResponse::error(
                "invalid_response",
                "inconsistent acceptance response; outcome is unknown",
            );
        }
    };
    super::json_response(&bcode_tool::ToolInvocationResponse {
        output,
        is_error: false,
        content: Vec::new(),
        full_output: None,
        result: None,
    })
}

fn publication_response(payload: &serde_json::Value) -> ServiceResponse {
    let revision = payload
        .as_object()
        .filter(|object| object.len() == 1)
        .and_then(|object| object.get("revision"))
        .and_then(serde_json::Value::as_u64)
        .filter(|revision| *revision > 0);
    let Some(revision) = revision else {
        return ServiceResponse::error(
            "invalid_response",
            "unsupported publication response; outcome is unknown",
        );
    };
    super::json_response(&bcode_tool::ToolInvocationResponse {
        output: format!("Workflow edit published at revision {revision}."),
        is_error: false,
        content: Vec::new(),
        full_output: None,
        result: None,
    })
}

fn staging_response(payload: serde_json::Value) -> ServiceResponse {
    let Ok(response) =
        serde_json::from_value::<bcode_workflow::WorkflowRunGraphStageResponse>(payload)
    else {
        return ServiceResponse::error(
            "invalid_response",
            "workflow staging returned an unsupported response; admission outcome is unknown",
        );
    };
    super::json_response(&bcode_tool::ToolInvocationResponse {
        output: if response.staged {
            "Workflow edit staged. Topology has not been published.".to_owned()
        } else {
            "Identical workflow edit was already staged. Topology has not been published."
                .to_owned()
        },
        is_error: false,
        content: Vec::new(),
        full_output: None,
        result: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_task_defaults_output_without_changing_explicit_contracts() {
        let mut arguments = json!({"run_id":"run","expected_revision":1,"mutation_id":"proposal",
            "task_id":"review","objective":"Review correctness.","agent_profile":"plan",
            "input":{"type_name":"bool","schema":{"type":"boolean"}},
            "entry":true,"exit":true,"reconciliation":[]});
        let default = parse_prompt_task(&arguments).expect("default output");
        assert_eq!(default.node.output, task_group::default_worker_output());
        arguments["output"] = json!(task_group::default_worker_output());
        assert_eq!(default.node, parse_prompt_task(&arguments).unwrap().node);
        arguments["output"] = serde_json::Value::Null;
        assert!(parse_prompt_task(&arguments).is_err());
    }

    #[test]
    fn prompt_task_delivers_acceptance_criteria_and_rejects_blank_entries() {
        let mut arguments = json!({"run_id":"run","expected_revision":1,"mutation_id":"proposal",
            "task_id":"review","objective":"Review correctness.","agent_profile":"plan",
            "input":{"type_name":"bool","schema":{"type":"boolean"}},
            "entry":true,"exit":true,"reconciliation":[],
            "acceptance_criteria":["Verify 日本語", "Quote \"evidence\""]});
        let task = parse_prompt_task(&arguments).unwrap();
        let config: bcode_workflow::WorkflowPromptConfiguration =
            serde_json::from_value(task.node.configuration).unwrap();
        assert_eq!(
            config.system_prompt,
            task_group::task_instructions(
                "Review correctness.".into(),
                &["Verify 日本語".into(), "Quote \"evidence\"".into()]
            )
            .unwrap()
        );
        assert!(config.read_only);
        for invalid in [json!([" "]), json!([null]), json!(null)] {
            arguments["acceptance_criteria"] = invalid;
            assert!(parse_prompt_task(&arguments).is_err());
        }
    }

    #[test]
    fn prompt_task_preserves_restrictions_without_elevating_authority() {
        let mut arguments = json!({"run_id":"run","expected_revision":1,"mutation_id":"proposal",
            "task_id":"review","objective":"Review correctness.","agent_profile":"plan",
            "input":{"type_name":"bool","schema":{"type":"boolean"}},
            "entry":true,"exit":true,"reconciliation":[],
            "timeout_ms":1234,"tool_allowlist":["filesystem.read"]});
        let task = parse_prompt_task(&arguments).unwrap();
        let config: bcode_workflow::WorkflowPromptConfiguration =
            serde_json::from_value(task.node.configuration).unwrap();
        assert_eq!(config.timeout_ms, 1234);
        assert_eq!(config.tool_allowlist, vec!["filesystem.read"]);
        assert!(config.read_only);
        assert_eq!(
            config.tool_capability,
            bcode_workflow::WorkflowToolCapability::ReadOnly
        );
        for (field, value) in [
            ("timeout_ms", json!(0)),
            ("timeout_ms", json!(-1)),
            ("tool_allowlist", json!([" "])),
            ("tool_allowlist", json!(null)),
        ] {
            let mut invalid = arguments.clone();
            invalid[field] = value;
            assert!(parse_prompt_task(&invalid).is_err());
        }
        arguments.as_object_mut().unwrap().remove("timeout_ms");
        arguments.as_object_mut().unwrap().remove("tool_allowlist");
        let task = parse_prompt_task(&arguments).unwrap();
        let config: bcode_workflow::WorkflowPromptConfiguration =
            serde_json::from_value(task.node.configuration).unwrap();
        assert!(config.timeout_ms > 0);
        assert!(config.tool_allowlist.is_empty());
    }

    #[test]
    fn prompt_task_model_selection_lowers_without_local_resolution() {
        let mut arguments = json!({"run_id":"run","expected_revision":1,"mutation_id":"proposal",
            "task_id":"review","objective":"Review correctness.","agent_profile":"plan",
            "input":{"type_name":"bool","schema":{"type":"boolean"}},
            "entry":true,"exit":true,"reconciliation":[],
            "model_selection":{"provider":"selected-provider","model":"selected-model"}});
        let task = parse_prompt_task(&arguments).unwrap();
        let config: bcode_workflow::WorkflowPromptConfiguration =
            serde_json::from_value(task.node.configuration).unwrap();
        assert_eq!(config.provider.as_deref(), Some("selected-provider"));
        assert_eq!(config.model.as_deref(), Some("selected-model"));
        assert!(config.read_only);
        for invalid in [
            json!({"provider":"", "model":"m"}),
            json!({"provider":"p", "model":" "}),
            json!({"provider":"p"}),
            json!({"provider":"p","model":"m","secret":"x"}),
        ] {
            arguments["model_selection"] = invalid;
            assert!(parse_prompt_task(&arguments).is_err());
        }
    }

    #[test]
    fn prompt_task_context_selection_preserves_read_only_authority() {
        let mut arguments = json!({"run_id":"run","expected_revision":1,"mutation_id":"proposal",
            "task_id":"review","objective":"Review correctness.","agent_profile":"plan",
            "input":{"type_name":"bool","schema":{"type":"boolean"}},
            "entry":true,"exit":true,"reconciliation":[]});
        for context in [
            bcode_workflow::PromptContextTarget::FreshIsolated,
            bcode_workflow::PromptContextTarget::FixedGenerationFork,
            bcode_workflow::PromptContextTarget::SharedParentSequential,
        ] {
            arguments["context"] = json!(context);
            let task = parse_prompt_task(&arguments).unwrap();
            let config: bcode_workflow::WorkflowPromptConfiguration =
                serde_json::from_value(task.node.configuration).unwrap();
            assert_eq!(config.execution_target, context);
            assert!(config.read_only);
            assert_eq!(
                config.tool_capability,
                bcode_workflow::WorkflowToolCapability::ReadOnly
            );
        }
        for invalid in [json!("future"), json!(null)] {
            arguments["context"] = invalid;
            assert!(parse_prompt_task(&arguments).is_err());
        }
    }

    #[test]
    fn prompt_task_resource_claims_do_not_grant_mutation() {
        let mut arguments = json!({"run_id":"run","expected_revision":1,"mutation_id":"proposal",
            "task_id":"review","objective":"Review correctness.","agent_profile":"plan",
            "input":{"type_name":"bool","schema":{"type":"boolean"}},
            "entry":true,"exit":true,"reconciliation":[],
            "resources":[{"resource":"repository","access":"write"}]});
        let edit = parse_tool_edit(PROMPT_TASK_NAME, &arguments).unwrap();
        let bcode_workflow::WorkflowRunGraphEdit::AddNode { node, .. } = &edit.edits[0] else {
            panic!("agent node");
        };
        assert_eq!(
            node.resources,
            vec![bcode_workflow::ResourceClaim::write("repository")]
        );
        let config: bcode_workflow::WorkflowPromptConfiguration =
            serde_json::from_value(node.configuration.clone()).unwrap();
        assert!(config.read_only);
        assert_eq!(
            config.tool_capability,
            bcode_workflow::WorkflowToolCapability::ReadOnly
        );
        arguments["resources"][0]["access"] = json!("future");
        assert!(parse_tool_edit(PROMPT_TASK_NAME, &arguments).is_err());
        arguments.as_object_mut().unwrap().remove("resources");
        assert!(
            parse_prompt_task(&arguments)
                .unwrap()
                .node
                .resources
                .is_empty()
        );
    }

    #[test]
    fn prompt_task_dependency_lowers_to_direct_edge() {
        let mut arguments = json!({"run_id":"run","expected_revision":1,"mutation_id":"proposal",
            "task_id":"review","objective":"Review correctness.","agent_profile":"plan",
            "input":{"type_name":"bool","schema":{"type":"boolean"}},
            "entry":false,"exit":true,"reconciliation":[],
            "depends_on":{"node_id":"planner","edge_id":42}});
        let edit = parse_tool_edit(PROMPT_TASK_NAME, &arguments).unwrap();
        assert_eq!(
            edit.edits[1],
            bcode_workflow::WorkflowRunGraphEdit::AddEdge {
                edge_id: 42,
                edge: bcode_workflow::EdgeDefinition {
                    from: "planner".into(),
                    to: "review".into(),
                    kind: bcode_workflow::EdgeKind::Direct,
                    transform: None,
                }
            }
        );
        arguments["entry"] = json!(true);
        assert!(parse_tool_edit(PROMPT_TASK_NAME, &arguments).is_err());
        arguments["entry"] = json!(false);
        for source in [" ", "review"] {
            arguments["depends_on"]["node_id"] = json!(source);
            assert!(parse_tool_edit(PROMPT_TASK_NAME, &arguments).is_err());
        }
    }

    #[test]
    fn prompt_task_lowers_to_read_only_canonical_agent() {
        let arguments = json!({"run_id":"run","expected_revision":1,"mutation_id":"proposal",
            "task_id":"review","objective":"Review correctness and return findings.",
            "agent_profile":"plan", "input":{"type_name":"bool","schema":{"type":"boolean"}},
            "output":{"type_name":"bool","schema":{"type":"boolean"}},
            "entry":true,"exit":true,"reconciliation":[]});
        let edit = parse_tool_edit(PROMPT_TASK_NAME, &arguments).expect("lower task");
        assert_eq!(
            edit,
            parse_tool_edit(PROMPT_TASK_NAME, &arguments).expect("retry")
        );
        assert_eq!(operation(PROMPT_TASK_NAME).expect("route"), OPERATION);
        let bcode_workflow::WorkflowRunGraphEdit::AddNode { node, entry, exit } = &edit.edits[0]
        else {
            panic!("agent addition");
        };
        assert!(*entry && *exit);
        assert_eq!(node.id, "review");
        let config: bcode_workflow::WorkflowPromptConfiguration =
            serde_json::from_value(node.configuration.clone()).expect("prompt config");
        assert!(config.read_only);
        assert_eq!(config.agent_profile, "plan");
        assert_eq!(
            config.execution_target,
            bcode_workflow::PromptContextTarget::FreshIsolated
        );
        assert_eq!(
            config.tool_capability,
            bcode_workflow::WorkflowToolCapability::ReadOnly
        );
        for (field, value) in [
            ("objective", json!("  ")),
            ("agent_profile", json!("")),
            ("read_only", json!(false)),
        ] {
            let mut invalid = arguments.clone();
            invalid[field] = value;
            assert!(parse_tool_edit(PROMPT_TASK_NAME, &invalid).is_err());
        }
    }

    #[test]
    fn acceptance_response_preserves_lifecycle_semantics() {
        for (payload, expected) in [
            (
                json!({"status":"pending", "expected_revision":1}),
                "topology has not been published",
            ),
            (
                json!({"status":"conflicted", "expected_revision":1, "current_revision":2}),
                "cancellation remains durable",
            ),
            (
                json!({"status":"committed", "revision":2}),
                "published at revision 2",
            ),
        ] {
            let response = acceptance_response(payload);
            assert!(response.error.is_none());
            let tool: bcode_tool::ToolInvocationResponse =
                serde_json::from_slice(&response.payload).expect("response");
            assert!(!tool.is_error);
            assert!(tool.output.contains(expected));
        }
        for payload in [
            json!({"revision":2}),
            json!({"status":"future"}),
            json!({"status":"pending", "expected_revision":0}),
            json!({"status":"conflicted", "expected_revision":2, "current_revision":1}),
        ] {
            assert!(acceptance_response(payload).error.is_some());
        }
    }

    #[test]
    fn staging_response_rejects_ambiguous_success() {
        for payload in [
            json!(null),
            json!({}),
            json!({"staged":"true"}),
            json!({"staged":true,"version":2}),
        ] {
            assert!(staging_response(payload).error.is_some());
        }
        for staged in [true, false] {
            let response = staging_response(json!({"staged":staged}));
            assert!(response.error.is_none());
            let tool: bcode_tool::ToolInvocationResponse =
                serde_json::from_slice(&response.payload).expect("tool response");
            assert!(!tool.is_error);
            assert!(tool.output.contains("Topology has not been published"));
        }
    }

    #[test]
    fn execution_context_default_is_bounded_and_strict() {
        assert_eq!(
            parse_context(json!({})).expect("default"),
            parse_context(json!({"limit":50})).expect("explicit")
        );
        for limit in [1, 100] {
            assert_eq!(
                parse_context(json!({"limit":limit}))
                    .expect("bounded")
                    .limit,
                limit
            );
        }
        for invalid in [
            json!([]),
            json!({"limit":null}),
            json!({"limit":0}),
            json!({"limit":101}),
            json!({"future":true}),
        ] {
            assert!(parse_context(invalid).is_err());
        }
    }

    #[test]
    fn staging_tool_validates_current_edit_contract() {
        let batch = WorkflowRunGraphEditBatch {
            version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
            run_id: "run".to_owned(),
            mutation_id: "edit".to_owned(),
            expected_revision: 1,
            edits: vec![bcode_workflow::WorkflowRunGraphEdit::RemoveEdge { edge_id: 0 }],
            reconciliation: vec![],
        };
        assert_eq!(
            parse_edit(&json!({"edit": batch.clone()})).expect("structured edit"),
            batch
        );
        assert!(
            parse_edit(
                &json!({"edit": batch, "edit_json": serde_json::to_string(&batch).expect("batch")})
            )
            .is_err()
        );
        assert!(parse_edit(&json!({"edit": null})).is_err());
        assert!(parse_edit(&json!({"edit": batch, "authorize": true})).is_err());
        assert!(
            parse_edit(
                &json!({"edit_json": serde_json::to_string(&batch).expect("batch"), "future": {}})
            )
            .is_err()
        );
        assert!(parse_edit(&json!([])).is_err());
        let arguments = json!({"edit_json": serde_json::to_string(&batch).expect("batch")});
        assert_eq!(parse_edit(&arguments).expect("valid edit"), batch);
        assert!(parse_edit(&json!({"edit_json":"not json"})).is_err());
        assert!(parse_edit(&json!({"edit_json": {}})).is_err());
        let mut future = batch;
        future.version = u32::MAX;
        assert!(
            parse_edit(&json!({"edit_json":serde_json::to_string(&future).expect("future")}))
                .is_err()
        );
    }
}
