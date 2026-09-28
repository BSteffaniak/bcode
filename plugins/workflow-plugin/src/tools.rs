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
    let object = arguments
        .as_object_mut()
        .ok_or("execution context request must be an object")?;
    let output_only = object.get("output_only") == Some(&json!(true));
    if output_only
        && (object
            .get("output_id")
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty)
            || object.get("delegation_only") == Some(&json!(true))
            || object.contains_key("delegation_part"))
    {
        return Err("output_only requires an exact output_id and cannot request delegation".into());
    }
    for field in ["compact", "delegation_only", "output_only"] {
        if let Some(value) = object.remove(field)
            && !value.is_boolean()
        {
            return Err(format!("{field} must be a boolean"));
        }
    }
    if let Some(field) = object.remove("delegation_part")
        && !matches!(
            field.as_str(),
            Some("bindings" | "input" | "reconnect" | "serialized")
        )
    {
        return Err("delegation_part must be bindings, input, reconnect or serialized".into());
    }
    if let Some(offset) = object.remove("delegation_offset")
        && offset.as_u64().is_none()
    {
        return Err("delegation_offset must be a nonnegative integer".into());
    }
    object.entry("limit").or_insert(json!(50));
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
                "delegation_part":{"type":"string","enum":["bindings","input","reconnect","serialized"],"description":"Read recipe fields, or use serialized for bounded JSON string chunks. Concatenate chunks in order and parse once; do not reconstruct schemas. Follow next_arguments unchanged at the pinned revision."},
                "delegation_offset":{"type":"integer","minimum":0,"description":"Character offset for serialized recipe chunks; use returned next_arguments."},
                "delegation_only":{"type":"boolean","default":false,"description":"Return only the authenticated delegation recipe from this bounded page, without duplicating graph facts. Use for default-budget staging; unavailable recipes remain explicit. Does not change authorization or discovery completeness."},
                "compact":{"type":"boolean","default":false,"description":"Return graph node identities instead of full executable definitions. Edges and authenticated identity remain available; omitted node definitions require a normal paged read."},
                "output_only":{"type":"boolean","default":false,"description":"With output_id, return only authenticated identity, revision and the exact checksum-verified output. Omits graph, discovery and delegation payloads; does not truncate the value or grant authority."},
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
                "output":{"type":"object","description":"Optional ValueSchema; omission uses bounded bcode.delegated_task_result.v2 with summary, evidence, blockers and optional contribution provenance. Explicit null rejects."},
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
    configuration.allow_user_questions = false;
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

fn portable_task_definition(mut tool: ToolDefinition) -> ToolDefinition {
    // Task payloads contain caller-authored schemas and configuration. Keep those
    // opaque to provider schema rewriting, but validate them normally after decoding.
    let payload_schema = tool.input_schema.to_string();
    tool.input_schema = json!({
        "type":"object", "additionalProperties":false, "required":["request_json"],
        "properties":{"request_json":{"type":"string","description":format!(
            "JSON-encoded task request. Payload contract: {payload_schema}"
        )}}
    });
    tool
}

fn parse_tool_edit(
    name: &str,
    arguments: &serde_json::Value,
) -> Result<WorkflowRunGraphEditBatch, String> {
    let decoded;
    let arguments = if matches!(name, GROUP_NAME | TASK_NAME | PROMPT_TASK_NAME)
        && arguments.get("request_json").is_some()
    {
        if arguments.as_object().is_none_or(|object| object.len() != 1) {
            return Err("request_json cannot be combined with task fields".into());
        }
        let text = arguments["request_json"]
            .as_str()
            .ok_or("request_json must be a JSON string")?;
        decoded = serde_json::from_str(text).map_err(|_| "invalid task request JSON")?;
        &decoded
    } else {
        arguments
    };
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
        description: "Publish the exact edit returned by staging for this active execution, using JSON-encoded edit_json. Requires separate publication authorization and explicit active-work reconciliation. Preserve the staged edit and mutation_id exactly when retrying; unsupported topology is rejected.".to_owned(),
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
        description: "Stage a revision-checked edit for the workflow run owning this active execution. Requires workflow application authorization. Does not publish or execute topology. Supply a WorkflowRunGraphEditBatch as JSON-encoded edit_json; preserve mutation_id when retrying.".to_owned(),
        input_schema: edit_input_schema(),
    }
}

fn edit_input_schema() -> serde_json::Value {
    json!({"type":"object","additionalProperties":false,
        "properties":{"edit_json":{"type":"string","description":"JSON-encoded WorkflowRunGraphEditBatch, a {candidate} reference returned by staging, or {task_tool,request} replay of an original workflow.stage_agent_task, workflow.stage_prompt_task or workflow.stage_task_group payload. Replay deterministically lowers the exact original request; publication still requires equality with the retained staged edit. Never change mutation identity or fields on retry."}},
        "required":["edit_json"]})
}

fn candidate_reference(
    arguments: &serde_json::Value,
) -> Option<bcode_workflow::WorkflowRunGraphCandidateReference> {
    if arguments.as_object()?.len() != 1 {
        return None;
    }
    let value: serde_json::Value =
        serde_json::from_str(arguments.get("edit_json")?.as_str()?).ok()?;
    if value.as_object()?.len() != 1 {
        return None;
    }
    let reference: bcode_workflow::WorkflowRunGraphCandidateReference =
        serde_json::from_value(value.get("candidate")?.clone()).ok()?;
    (reference.version == 1
        && reference.expected_revision > 0
        && !reference.run_id.is_empty()
        && !reference.mutation_id.is_empty()
        && reference.checksum_sha256.len() == 64
        && reference
            .checksum_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit()))
    .then_some(reference)
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
        (None, Some(serde_json::Value::String(text))) => {
            let value: serde_json::Value = serde_json::from_str(text)
                .map_err(|_| "invalid workflow edit representation".to_owned())?;
            if value.get("task_tool").is_some() {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct TaskReplay {
                    task_tool: String,
                    request: serde_json::Value,
                }
                let replay: TaskReplay = serde_json::from_value(value)
                    .map_err(|_| "invalid task replay representation".to_owned())?;
                if !matches!(
                    replay.task_tool.as_str(),
                    TASK_NAME | PROMPT_TASK_NAME | GROUP_NAME
                ) {
                    return Err("unsupported task replay tool".into());
                }
                return parse_tool_edit(&replay.task_tool, &replay.request);
            }
            serde_json::from_value(value)
        }
        _ => return Err("supply exactly one of edit or edit_json".into()),
    }
    .map_err(|_| "invalid workflow edit representation".to_owned())?;
    edit.validate()
        .map_err(|_| "invalid workflow edit facts".to_owned())?;
    Ok(edit)
}

fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        definition(),
        publication_definition(),
        acceptance_definition(),
        context_definition(),
        portable_task_definition(task_definition()),
        portable_task_definition(prompt_task_definition()),
        portable_task_definition(task_group::definition()),
    ]
}

/// Dispatch the plugin-owned workflow tool service.
pub fn invoke(context: &NativeServiceContext) -> ServiceResponse {
    match context.request.operation.as_str() {
        bcode_tool::OP_LIST_TOOLS => {
            super::json_response(&ToolList::with_discovery(tool_definitions(), |tool| {
                bcode_tool::ToolDiscoveryPolicy::new(tool.name == CONTEXT_NAME)
            }))
        }
        bcode_tool::OP_PREPARE_TOOL => {
            prepare_tool_service_response(&context.request, tool_definitions(), |request, _| {
                let is_context = request.invocation.tool_name == CONTEXT_NAME;
                let operation = if is_context {
                    CONTEXT_OPERATION
                } else {
                    operation(&request.invocation.tool_name)?
                };
                let payload = if is_context {
                    let context = parse_context(request.invocation.arguments.clone())?;
                    serde_json::to_value(context).map_err(|error| error.to_string())?
                } else if matches!(operation, PUBLISH_OPERATION | ACCEPT_OPERATION)
                    && candidate_reference(&request.invocation.arguments).is_some()
                {
                    json!({"candidate": candidate_reference(&request.invocation.arguments)})
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
            })
        }
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
    let compact = request.arguments.get("compact") == Some(&json!(true));
    let output_only = request.arguments.get("output_only") == Some(&json!(true));
    let delegation_only = request.arguments.get("delegation_only") == Some(&json!(true));
    let delegation_part = request
        .arguments
        .get("delegation_part")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let delegation_offset = request
        .arguments
        .get("delegation_offset")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let Ok(query) = parse_context(request.arguments) else {
        return ServiceResponse::error("invalid_request", "invalid execution context request");
    };
    let Ok(payload) = serde_json::to_value(&query) else {
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
                        output: if output_only {
                            exact_output_view(&context)
                        } else if delegation_only || delegation_part.is_some() {
                            json!({"delegation": if delegation_part.as_deref() == Some("serialized") { serialized_delegation_recipe(&context, &query, delegation_offset) } else { delegation_recipe_part(&context, &query, delegation_part.as_deref()) }}).to_string()
                        } else {
                            context_output_with_navigation(&context, compact, &query)
                        },
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

// Projection only: host authentication, checksum verification and size limits are unchanged.
fn exact_output_view(context: &bcode_workflow::WorkflowExecutionContext) -> String {
    json!({
        "run_id": context.run_id,
        "node_id": context.node_id,
        "activation_id": context.activation_id,
        "attempt": context.attempt,
        "revision": context.graph.revision,
        "output": context.output,
    })
    .to_string()
}

// Compact inspection is presentation only: the host still validates the same
// bounded revision-pinned request and supplies all authenticated facts.
fn context_output_with_navigation(
    context: &bcode_workflow::WorkflowExecutionContext,
    compact: bool,
    query: &bcode_workflow::WorkflowExecutionContextRequest,
) -> String {
    let mut output: serde_json::Value =
        serde_json::from_str(&context_output(context, compact)).expect("serialized context");
    // Independent cursors must survive empty/exhausted pages in the other collections.
    // Outputs can arrive behind the cursor: this recipe is not a durable subscription.
    let more = !context.graph.nodes_complete
        || !context.graph.edges_complete
        || context.outputs.len() == query.limit;
    output["next_page_arguments"] = if more {
        json!({
            "expected_revision": context.graph.revision,
            "after_node_id": context.graph.nodes.last().map(|entry| &entry.node.id).or(query.after_node_id.as_ref()),
            "after_edge_id": context.graph.edges.last().map(|entry| entry.edge_id).or(query.after_edge_id),
            "after_output_id": context.outputs.last().map(|entry| &entry.output_id).or(query.after_output_id.as_ref()),
            "limit": query.limit,
            "compact": compact,
        })
    } else {
        serde_json::Value::Null
    };
    output["delegation"] = delegation_recipe(context, query);
    for result in output["outputs"].as_array_mut().expect("output metadata") {
        result["inspection_arguments"] = json!({
            "output_id": result["output_id"],
            "output_only": true,
            "expected_revision": context.graph.revision,
            "limit": 1,
            "compact": compact,
        });
    }
    output.to_string()
}

// Chunk the exact JSON representation rather than attempting to split arbitrary
// JSON Schema constructs. Revision-pinned reads prevent mixing graph revisions.
fn serialized_delegation_recipe(
    context: &bcode_workflow::WorkflowExecutionContext,
    query: &bcode_workflow::WorkflowExecutionContextRequest,
    offset: u64,
) -> serde_json::Value {
    let recipe = delegation_recipe(context, query);
    if recipe["available"] != true {
        return recipe;
    }
    let serialized = recipe["arguments"].to_string();
    let Ok(offset) = usize::try_from(offset) else {
        return json!({"available":false,"reason":"Invalid serialized recipe offset"});
    };
    let total = serialized.chars().count();
    if offset > total || (offset > 0 && query.expected_revision != Some(context.graph.revision)) {
        return json!({"available":false,"reason":"Invalid offset or missing pinned revision"});
    }
    // At most six JSON bytes per source character, leaving room for navigation.
    let chunk: String = serialized.chars().skip(offset).take(384).collect();
    let end = offset + chunk.chars().count();
    let next = (end < total).then(|| {
        let mut next = serde_json::to_value(query).expect("serializable query");
        next["expected_revision"] = json!(context.graph.revision);
        next["delegation_part"] = json!("serialized");
        next["delegation_offset"] = json!(end);
        next
    });
    json!({"available":true,"revision":context.graph.revision,"offset":offset,
        "chunk":chunk,"next_arguments":next})
}

fn delegation_recipe_part(
    context: &bcode_workflow::WorkflowExecutionContext,
    query: &bcode_workflow::WorkflowExecutionContextRequest,
    part: Option<&str>,
) -> serde_json::Value {
    let mut recipe = delegation_recipe(context, query);
    let Some(part) = part else {
        return recipe;
    };
    let Some(arguments) = recipe
        .get_mut("arguments")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return recipe;
    };
    if part == "bindings" {
        arguments.remove("input");
        arguments.remove("reconnect");
        for field in ["input", "reconnect"] {
            let mut inspection = serde_json::to_value(query).expect("serializable query");
            inspection["expected_revision"] = json!(context.graph.revision);
            inspection["delegation_part"] = json!(field);
            recipe["inspection_arguments"][field] = inspection;
        }
    } else {
        arguments.retain(|key, _| key == part);
        recipe.as_object_mut().unwrap().remove("instructions");
    }
    recipe
}

// Advice is computed from the full typed response, never the compact presentation.
// A tail page's completion flags do not prove that preceding edges were inspected.
fn delegation_recipe(
    context: &bcode_workflow::WorkflowExecutionContext,
    query: &bcode_workflow::WorkflowExecutionContextRequest,
) -> serde_json::Value {
    let unavailable = |reason| json!({"available":false,"reason":reason});
    if query.after_edge_id.is_some() || !context.graph.edges_complete {
        return unavailable(
            "Complete source-edge discovery from the initial cursor is required; use advanced task-group staging for larger graphs.",
        );
    }
    let Some(first_edge_id) = context.graph.next_edge_id else {
        return unavailable("Edge allocation is unavailable.");
    };
    let Some(source) = context
        .graph
        .nodes
        .iter()
        .find(|node| node.node.id == context.node_id)
    else {
        return unavailable("The source node definition is not in this bounded page.");
    };
    let mut successors = context
        .graph
        .edges
        .iter()
        .filter(|edge| edge.edge.from == context.node_id);
    let Some(successor) = successors.next() else {
        return unavailable("Source has no successor to reconnect.");
    };
    if successors.next().is_some()
        || successor.edge.to == context.node_id
        || successor.edge.kind != bcode_workflow::EdgeKind::Direct
    {
        return unavailable(
            "Multiple or control-flow successors require explicit advanced staging.",
        );
    }
    if let Some(transform) = &successor.edge.transform {
        let bcode_workflow::WorkflowTransformExpression::Input { source, path } =
            &transform.expression
        else {
            return unavailable("Successor transform is not a canonical source selection.");
        };
        if transform.version != bcode_workflow::WORKFLOW_TRANSFORM_VERSION
            || source != bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT
            || path.split('.').any(|segment| segment != "source")
        {
            return unavailable("Successor transform is not a canonical source selection.");
        }
    }
    json!({
        "available":true,
        "tool":"workflow.stage_task_group",
        "arguments":{
            "version":2,"generated_ids":true,
            "run_id":context.run_id,"expected_revision":context.graph.revision,
            "source_node_id":context.node_id,"bind_source_activation":context.activation_id,
            "input":source.node.output,"first_edge_id":first_edge_id,
            "preserve_source_output":true,"retain_source_edge_ids":[],"reconciliation":[],
            "reconnect":{"edge_id":successor.edge_id,"node_id":successor.edge.to,"transform":successor.edge.transform}
        },
        "instructions":"Copy arguments and add a fresh mutation_id, semantic tasks and continuation objective/agent_profile. Choose workspace safety, access, dependencies and criteria explicitly. This advice grants no authority and does not reserve identities. Staging validates affected active work; explicitly reconcile any additional affected activations. Publish the returned publication_arguments separately, then finish this source turn without waiting. On revision conflict rediscover; never silently rebase."
    })
}

fn context_output(context: &bcode_workflow::WorkflowExecutionContext, compact: bool) -> String {
    let mut output = serde_json::to_value(context).expect("serializable workflow context");
    if compact {
        output["graph"].as_object_mut().unwrap().remove("nodes");
        output["graph"]["node_ids"] = json!(
            context
                .graph
                .nodes
                .iter()
                .map(|entry| &entry.node.id)
                .collect::<Vec<_>>()
        );
        for edge in output["graph"]["edges"].as_array_mut().unwrap() {
            // Cursors are exclusive numeric identities, not offsets into this page.
            // A one-edge read also works for sparse/retired identities and edge zero.
            let edge_id = edge["edge_id"].as_u64().expect("typed edge identity");
            edge["inspection_arguments"] = json!({
                "expected_revision": context.graph.revision,
                "after_edge_id": edge_id.checked_sub(1),
                "limit": 1,
                "compact": false
            });
            if let Some(definition) = edge
                .get_mut("edge")
                .and_then(serde_json::Value::as_object_mut)
            {
                definition.remove("transform");
            }
        }
        // Result discovery lists identities only; exact values remain available
        // through an explicit output_id request, never silently substituted.
        for result in output["outputs"].as_array_mut().unwrap() {
            if let Some(result) = result.as_object_mut() {
                result.retain(|key, _| {
                    matches!(key.as_str(), "output_id" | "node_id" | "checksum_sha256")
                });
            }
        }
        output["output_values_omitted"] = json!(true);
        output["graph"]["edge_transforms_omitted"] = json!(true);
        output["graph"]["node_definitions_omitted"] = json!(true);
    }
    output.to_string()
}

fn invoke_candidate(
    context: &NativeServiceContext,
    request: ToolInvocationRequest,
    operation: &str,
    reference: &bcode_workflow::WorkflowRunGraphCandidateReference,
) -> ServiceResponse {
    let payload = json!({"candidate":reference});
    if request
        .preparation_descriptor
        .get("operation")
        .and_then(serde_json::Value::as_str)
        != Some(operation)
        || request.preparation_descriptor.get("edit") != Some(&payload)
        || context.cancellation.is_cancelled()
    {
        return ServiceResponse::error(
            "invalid_request",
            "candidate differs from prepared authorization",
        );
    }
    let Some(route_id) = request
        .preparation_descriptor
        .get("route_id")
        .and_then(serde_json::Value::as_str)
    else {
        return ServiceResponse::error("invalid_request", "candidate route missing");
    };
    // The application authorizes the complete retained edit, not the reference.
    match context.bridge.request(&ServiceBridgeRequest::InvokeService(
        ToolInvocationServiceRequest {
            invocation_id: request.tool_call_id.clone(),
            request_id: request.tool_call_id,
            route_id: Some(route_id.to_owned()),
            interface_id: WORKFLOW_APPLICATION_INTERFACE_ID.into(),
            operation: operation.into(),
            payload,
        },
    )) {
        Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded {
            payload,
        })) => {
            if operation == ACCEPT_OPERATION {
                acceptance_response(payload)
            } else {
                publication_response(&payload)
            }
        }
        Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Failed {
            code,
            ..
        })) => edit_failure(&code),
        _ => ServiceResponse::error(
            "workflow_publication_failed",
            "candidate publication outcome unknown",
        ),
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
    if matches!(operation, PUBLISH_OPERATION | ACCEPT_OPERATION)
        && let Some(reference) = candidate_reference(&request.arguments)
    {
        return invoke_candidate(context, request, operation, &reference);
    }
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
                task_staging_response(payload, &edit, &request.name, &request.arguments)
            } else {
                staging_response(payload)
            }
        }
        Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Cancelled)) => {
            ServiceResponse::error("cancelled", "workflow staging cancelled")
        }
        Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Failed {
            code,
            ..
        })) => edit_failure(&code),
        _ => ServiceResponse::error(
            "workflow_staging_failed",
            "workflow edit was not admitted; verify route, policy, and active execution",
        ),
    }
}

fn edit_failure(code: &str) -> ServiceResponse {
    match code {
        "workflow_authorization_failed" => ServiceResponse::error(
            code,
            "workflow edit requires matching active execution, retained candidate and application authorization",
        ),
        "workflow_candidate_rejected" => ServiceResponse::error(
            code,
            "workflow candidate rejected; verify revision, topology, schemas and active-work reconciliation",
        ),
        _ => ServiceResponse::error(
            "workflow_staging_failed",
            "workflow edit was not admitted; verify route, policy, and active execution",
        ),
    }
}

fn task_staging_response(
    payload: serde_json::Value,
    edit: &WorkflowRunGraphEditBatch,
    task_tool: &str,
    request: &serde_json::Value,
) -> ServiceResponse {
    let Ok(staged) =
        serde_json::from_value::<bcode_workflow::WorkflowRunGraphStageResponse>(payload)
    else {
        return ServiceResponse::error("invalid_response", "task staging outcome unknown");
    };
    // A replay is a convenience, not authority: re-lowering must produce exactly
    // the candidate admitted by the application before it can be published.
    let replay = json!({"task_tool":task_tool,"request":request});
    let publication_arguments = json!({"edit_json":replay.to_string()});
    if parse_edit(&publication_arguments).as_ref() != Ok(edit) {
        return ServiceResponse::error("invalid_response", "task replay differs from staged edit");
    }
    let publication_arguments =
        match bcode_workflow::WorkflowRunGraphCandidateReference::from_edit(edit) {
            Ok(reference) => json!({"edit_json":json!({"candidate":reference}).to_string()}),
            Err(_) => {
                return ServiceResponse::error(
                    "invalid_response",
                    "candidate reference unavailable",
                );
            }
        };
    let decoded = request
        .get("request_json")
        .and_then(serde_json::Value::as_str)
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok());
    let compact = task_tool == GROUP_NAME
        && decoded.as_ref().unwrap_or(request).get("version") == Some(&json!(2));
    let mut output = json!({
        "staged":staged.staged,
        "published":false,
        "publication_arguments":publication_arguments,
        "publication_hint":"Use publication_arguments with workflow.publish_run_graph_edit under separate authorization. Retain this exact request on retry; staging is not publication.",
    });
    if compact {
        // V2 carries a bounded exact-candidate reference. Do not repeat expanded prompts and
        // schemas in the model context merely to expose generated identities.
        let node_ids: Vec<_> = edit
            .edits
            .iter()
            .filter_map(|operation| match operation {
                bcode_workflow::WorkflowRunGraphEdit::AddNode { node, .. } => Some(&node.id),
                _ => None,
            })
            .collect();
        output["node_ids"] = json!(node_ids);
    } else {
        output["edit"] = json!(edit);
    }
    if task_tool == GROUP_NAME {
        let Ok((mapped_edit, mut mapping)) =
            task_group::mapped_candidate(decoded.as_ref().unwrap_or(request))
        else {
            return ServiceResponse::error("invalid_response", "task result mapping unavailable");
        };
        if &mapped_edit != edit {
            return ServiceResponse::error(
                "invalid_response",
                "task mapping differs from staged edit",
            );
        }
        if compact {
            // Assignment details remain in the canonical nodes and authored request.
            // Receipts need identities and paths, not repeated prompts and schemas.
            for worker in mapping["workers"].as_array_mut().expect("worker mappings") {
                worker
                    .as_object_mut()
                    .expect("worker mapping")
                    .retain(|key, _| {
                        matches!(
                            key.as_str(),
                            "task_id" | "input_path" | "terminal_outcome" | "evidence_paths"
                        )
                    });
            }
            // The successor transform can repeat a large source schema. Corrective
            // delegation must inspect the revision-pinned edge anyway; do not let
            // redundant topology truncate the publication reference and named paths.
            mapping
                .as_object_mut()
                .expect("mapping object")
                .remove("reconnect");
        }
        output["result_mapping"] = mapping;
    }
    super::json_response(&bcode_tool::ToolInvocationResponse {
        output: output.to_string(),
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
    fn staging_receipt_replays_portable_v2_request_without_reconstruction() {
        let request = json!({"version":2,"generated_ids":true,
            "run_id":"run","expected_revision":1,"mutation_id":"delegate",
            "input":{"type_name":"bool","schema":{"type":"boolean"}},
            "tasks":[
                {"task_id":"review.日本語","objective":"Review".repeat(10_000),"agent_profile":"plan"},
                {"task_id":"verify","objective":"Verify","agent_profile":"plan"}
            ],
            "continuation":{"objective":"Integrate","agent_profile":"plan",
                "output":{"type_name":"bool","schema":{"type":"boolean"}}},
            "first_edge_id":1,"reconciliation":[]});
        for arguments in [request.clone(), json!({"request_json":request.to_string()})] {
            let edit = parse_tool_edit(GROUP_NAME, &arguments).unwrap();
            for staged in [true, false] {
                let response =
                    task_staging_response(json!({"staged":staged}), &edit, GROUP_NAME, &arguments);
                assert!(response.error.is_none());
                let tool: bcode_tool::ToolInvocationResponse =
                    serde_json::from_slice(&response.payload).unwrap();
                let receipt: serde_json::Value = serde_json::from_str(&tool.output).unwrap();
                assert_eq!(receipt["published"], false);
                assert_eq!(receipt["staged"], staged);
                assert_eq!(
                    candidate_reference(&receipt["publication_arguments"]).unwrap(),
                    bcode_workflow::WorkflowRunGraphCandidateReference::from_edit(&edit).unwrap()
                );
                assert!(receipt.get("edit").is_none());
                let mapping = &receipt["result_mapping"];
                for (worker, task_id) in mapping["workers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .zip(["review.日本語", "verify"])
                {
                    assert_eq!(worker["task_id"], task_id);
                    assert_eq!(worker["input_path"], json!(["results", task_id]));
                    assert!(worker.get("objective").is_none());
                    assert!(worker.get("output").is_none());
                    assert_eq!(
                        worker["evidence_paths"]["summary"],
                        json!(["results", task_id, "summary"])
                    );
                }
                assert_eq!(mapping["source_path"], serde_json::Value::Null);
                assert_eq!(mapping["preserves_source_output"], false);
                assert!(
                    receipt["node_ids"]
                        .as_array()
                        .unwrap()
                        .contains(&mapping["continuation_id"])
                );
                assert!(
                    receipt["node_ids"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("review.日本語"))
                );
                assert!(tool.output.len() < 4_000);
                assert!(tool.output.len() < serde_json::to_string(&edit).unwrap().len());
            }
            let mut different = edit.clone();
            different.mutation_id = "different".into();
            assert!(
                task_staging_response(json!({"staged":true}), &different, GROUP_NAME, &arguments)
                    .error
                    .is_some()
            );
            assert!(
                task_staging_response(
                    json!({"staged":true,"version":99}),
                    &edit,
                    GROUP_NAME,
                    &arguments
                )
                .error
                .is_some()
            );
        }
    }

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
        assert!(!config.allow_user_questions);
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
    fn all_exposed_tools_have_strict_portable_parameters() {
        fn check(schema: &serde_json::Value) {
            if schema["type"] == "array" {
                assert!(schema.get("items").is_some(), "array needs items: {schema}");
                check(&schema["items"]);
            }
            if schema["type"] == "object" {
                assert_eq!(schema["additionalProperties"], json!(false));
                let properties = schema["properties"]
                    .as_object()
                    .expect("explicit properties");
                for (key, value) in properties {
                    assert!(schema["required"].as_array().unwrap().contains(&json!(key)));
                    check(value);
                }
            }
            for keyword in ["anyOf", "oneOf", "allOf"] {
                if let Some(branches) = schema[keyword].as_array() {
                    for branch in branches {
                        check(branch);
                    }
                }
            }
        }
        for tool in tool_definitions() {
            let normalized = bcode_model_schema::normalize(
                &tool.input_schema,
                &bcode_model_schema::SchemaDialect {
                    object_properties: bcode_model_schema::ObjectPropertyPolicy::RequireAllAndClose,
                    one_of: bcode_model_schema::OneOfPolicy::CollapseAnnotatedConstants,
                    reference_siblings:
                        bcode_model_schema::ReferenceSiblingPolicy::RemoveAnnotationsRejectSemantic,
                    ..bcode_model_schema::SchemaDialect::default()
                },
            )
            .expect("strict normalization");
            check(&normalized);
        }
    }

    #[test]
    fn task_envelope_preserves_typed_validation() {
        let arguments = json!({"run_id":"run","expected_revision":1,"mutation_id":"proposal",
            "task_id":"review","objective":"Review correctness.","agent_profile":"plan",
            "input":{"type_name":"bool","schema":{"type":"boolean"}},
            "entry":true,"exit":true,"reconciliation":[]});
        let wrapped = json!({"request_json": arguments.to_string()});
        assert_eq!(
            parse_tool_edit(PROMPT_TASK_NAME, &wrapped).unwrap(),
            parse_tool_edit(PROMPT_TASK_NAME, &arguments).unwrap()
        );
        let replay = json!({"edit_json":json!({"task_tool":PROMPT_TASK_NAME,"request":arguments}).to_string()});
        assert_eq!(
            parse_edit(&replay).unwrap(),
            parse_tool_edit(PROMPT_TASK_NAME, &wrapped).unwrap()
        );
        assert!(
            parse_edit(
                &json!({"edit_json":json!({"task_tool":PUBLISH_NAME,"request":{}}).to_string()})
            )
            .is_err()
        );
        for name in [TASK_NAME, PROMPT_TASK_NAME, GROUP_NAME] {
            for invalid in [
                json!({"request_json":null}),
                json!({"request_json":"not json"}),
                json!({"request_json":"{}","run_id":"run"}),
                json!({"request_json":"{}"}),
            ] {
                assert!(parse_tool_edit(name, &invalid).is_err());
            }
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
    fn delegation_recipe_lowers_authenticated_source_and_rejects_partial_discovery() {
        let schema = json!({"type_name":"goal","schema":{"type":"object"}});
        let mut context: bcode_workflow::WorkflowExecutionContext = serde_json::from_value(json!({
            "run_id":"run", "node_id":"source", "activation_id":"activation", "attempt":1,
            "graph":{"revision":7,"next_edge_id":19,
                "nodes":[{"revision":7,"entry":true,"exit":false,"node":{
                    "id":"source","name":"source","kind":"agent",
                    "input":schema,"output":schema,"configuration":{},"resources":[]
                }}],
                "edges":[{"revision":7,"edge_id":3,"edge":{"from":"source","to":"evaluate"}}],
                "nodes_complete":true,"edges_complete":true},
            "output":null,"outputs":[]
        }))
        .unwrap();
        let query = parse_context(json!({})).unwrap();
        let recipe = delegation_recipe(&context, &query);
        assert_eq!(recipe["available"], true);
        let mut request = recipe["arguments"].clone();
        request["mutation_id"] = json!("delegate");
        request["tasks"] =
            json!([{"task_id":"review","objective":"Review changes","agent_profile":"plan"}]);
        request["continuation"] = json!({"objective":"Integrate evidence","agent_profile":"build"});
        assert!(parse_tool_edit(GROUP_NAME, &request).is_ok());
        assert_eq!(request["bind_source_activation"], "activation");
        assert_eq!(request["input"], schema);
        assert_eq!(request["first_edge_id"], 19);
        let mut tail = query.clone();
        tail.after_edge_id = Some(2);
        assert_eq!(delegation_recipe(&context, &tail)["available"], false);
        context.graph.edges_complete = false;
        assert_eq!(delegation_recipe(&context, &query)["available"], false);
        context.graph.edges_complete = true;
        context.graph.edges[0].edge.transform = Some(bcode_workflow::WorkflowTransform {
            version: bcode_workflow::WORKFLOW_TRANSFORM_VERSION,
            expression: bcode_workflow::WorkflowTransformExpression::Input {
                source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT.into(),
                path: "source.source".into(),
            },
            output: context.graph.nodes[0].node.output.clone(),
        });
        let recipe = delegation_recipe(&context, &query);
        assert_eq!(
            recipe["arguments"]["reconnect"]["transform"],
            serde_json::to_value(&context.graph.edges[0].edge.transform).unwrap()
        );
        let bindings = delegation_recipe_part(&context, &query, Some("bindings"));
        let mut assembled = bindings["arguments"].as_object().unwrap().clone();
        for field in ["input", "reconnect"] {
            let inspection = &bindings["inspection_arguments"][field];
            assert_eq!(inspection["expected_revision"], context.graph.revision);
            let part_query = parse_context(inspection.clone()).unwrap();
            let part = delegation_recipe_part(&context, &part_query, Some(field));
            assembled.extend(part["arguments"].as_object().unwrap().clone());
        }
        assert_eq!(json!(assembled), recipe["arguments"]);
        // Large Unicode schemas can be retrieved losslessly without raising the
        // normal tool-output budget, including across escaped JSON boundaries.
        context.graph.nodes[0].node.output.schema["description"] = json!("界\\\"\n".repeat(4000));
        let mut chunk_query = query.clone();
        let mut offset = 0;
        let mut serialized = String::new();
        loop {
            let page = serialized_delegation_recipe(&context, &chunk_query, offset);
            assert_eq!(page["available"], true);
            assert!(page.to_string().len() < 4000);
            serialized.push_str(page["chunk"].as_str().unwrap());
            if page["next_arguments"].is_null() {
                break;
            }
            offset = page["next_arguments"]["delegation_offset"]
                .as_u64()
                .unwrap();
            chunk_query = parse_context(page["next_arguments"].clone()).unwrap();
        }
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&serialized).unwrap(),
            delegation_recipe(&context, &query)["arguments"]
        );
        assert_eq!(
            serialized_delegation_recipe(&context, &query, 1)["available"],
            false
        );
        assert!(parse_context(json!({"delegation_offset":-1})).is_err());
        assert!(parse_context(json!({"delegation_part":"unknown"})).is_err());
        assert!(parse_context(json!({"delegation_only":1})).is_err());
        context.graph.edges.push(context.graph.edges[0].clone());
        assert_eq!(delegation_recipe(&context, &query)["available"], false);
        context.graph.edges.pop();
        context.graph.next_edge_id = None;
        assert_eq!(delegation_recipe(&context, &query)["available"], false);
    }

    #[test]
    fn compact_context_preserves_identity_and_revision_without_node_payloads() {
        let context: bcode_workflow::WorkflowExecutionContext = serde_json::from_value(json!({
            "run_id":"run", "node_id":"source", "activation_id":"activation", "attempt":1,
            "graph":{"revision":7,"next_edge_id":19,"nodes":[],"edges":[],
                "nodes_complete":false,"edges_complete":true},
            "output":null,"outputs":[]
        }))
        .unwrap();
        let compact: serde_json::Value =
            serde_json::from_str(&context_output(&context, true)).unwrap();
        assert_eq!(compact["activation_id"], "activation");
        assert_eq!(compact["graph"]["revision"], 7);
        assert_eq!(compact["graph"]["next_edge_id"], 19);
        assert_eq!(compact["graph"]["nodes_complete"], false);
        assert_eq!(compact["graph"]["node_definitions_omitted"], true);
        assert!(compact["graph"].get("nodes").is_none());
        assert_eq!(
            serde_json::from_str::<bcode_workflow::WorkflowExecutionContext>(&context_output(
                &context, false
            ))
            .unwrap(),
            context
        );
        assert_eq!(
            parse_context(json!({"compact":true})).unwrap(),
            parse_context(json!({})).unwrap()
        );
        assert!(parse_context(json!({"compact":"true"})).is_err());
    }

    #[test]
    fn exact_output_projection_preserves_evidence_without_graph_overhead() {
        let context: bcode_workflow::WorkflowExecutionContext = serde_json::from_value(json!({
            "run_id":"run", "node_id":"evaluator", "activation_id":"evaluation", "attempt":1,
            "graph":{"revision":7,"next_edge_id":19,"nodes":[],"edges":[],
                "nodes_complete":false,"edges_complete":false},
            "output":{
                "version":1,"output_id":"worker-result","run_id":"run","node_id":"worker",
                "activation_id":"worker-activation","schema_id":"contribution","schema_version":1,
                "checksum_sha256":"checksum","created_at_ms":1,
                "value":{"contributions":[{"artifacts":["src/結果.rs"]}],"blockers":["unverified"]}
            },"outputs":[]
        }))
        .unwrap();
        let projected: serde_json::Value =
            serde_json::from_str(&exact_output_view(&context)).unwrap();
        assert_eq!(
            projected["output"],
            serde_json::to_value(&context.output).unwrap()
        );
        assert_eq!(projected["revision"], 7);
        assert_eq!(projected["activation_id"], "evaluation");
        assert!(projected.get("graph").is_none());
        assert!(projected.get("delegation").is_none());
        assert_eq!(
            parse_context(json!({"output_only":true,"output_id":"worker-result","limit":1}))
                .unwrap(),
            parse_context(json!({"output_id":"worker-result","limit":1})).unwrap()
        );
        for invalid in [
            json!({"output_only":true}),
            json!({"output_only":true,"output_id":""}),
            json!({"output_only":"true","output_id":"result"}),
            json!({"output_only":true,"output_id":"result","delegation_only":true}),
            json!({"output_only":true,"output_id":"result","delegation_part":"input"}),
        ] {
            assert!(parse_context(invalid).is_err());
        }
    }

    #[test]
    fn context_navigation_preserves_independent_cursors_and_fetches_exact_outputs() {
        let mut context: bcode_workflow::WorkflowExecutionContext = serde_json::from_value(json!({
            "run_id":"run", "node_id":"source", "activation_id":"activation", "attempt":1,
            "graph":{"revision":7,"nodes":[],"edges":[],
                "nodes_complete":true,"edges_complete":true},
            "output":null,"outputs":[{
                "output_id":"worker.結果", "run_id":"run", "node_id":"worker",
                "activation_id":"worker-activation", "schema_id":"result", "schema_version":1,
                "artifact_reference":null,"checksum_sha256":"checksum","created_at_ms":0
            }]
        }))
        .unwrap();
        for compact in [false, true] {
            let query = parse_context(json!({
                "limit":1,"after_node_id":"last-node","after_edge_id":91,
                "after_output_id":"previous", "output_id":"old-exact-output"
            }))
            .unwrap();
            let rendered: serde_json::Value =
                serde_json::from_str(&context_output_with_navigation(&context, compact, &query))
                    .unwrap();
            let next = parse_context(rendered["next_page_arguments"].clone()).unwrap();
            assert_eq!(next.expected_revision, Some(7));
            assert_eq!(next.after_node_id, query.after_node_id);
            assert_eq!(next.after_edge_id, Some(91));
            assert_eq!(next.after_output_id.as_deref(), Some("worker.結果"));
            assert_eq!(next.output_id, None);
            assert_eq!(
                rendered["outputs"][0]["inspection_arguments"]["output_only"],
                true
            );
            let exact =
                parse_context(rendered["outputs"][0]["inspection_arguments"].clone()).unwrap();
            assert_eq!(exact.output_id.as_deref(), Some("worker.結果"));
            assert_eq!(exact.expected_revision, Some(7));
            assert_eq!(exact.after_output_id, None);
        }
        context.outputs.clear();
        let query = parse_context(json!({"limit":1})).unwrap();
        let rendered: serde_json::Value =
            serde_json::from_str(&context_output_with_navigation(&context, true, &query)).unwrap();
        assert!(rendered["next_page_arguments"].is_null());
    }

    #[test]
    fn compact_edge_inspection_is_revision_pinned_and_recovers_exact_transform() {
        for edge_id in [0_u64, 9, u64::MAX] {
            let mut context: bcode_workflow::WorkflowExecutionContext =
                serde_json::from_value(json!({
                    "run_id":"run", "node_id":"source", "activation_id":"activation", "attempt":1,
                    "graph":{"revision":7,"next_edge_id":null,"nodes":[],"edges":[],
                        "nodes_complete":true,"edges_complete":true},
                    "output":null,"outputs":[]
                }))
                .unwrap();
            let transform = bcode_workflow::WorkflowTransform {
                version: 1,
                expression: bcode_workflow::WorkflowTransformExpression::Input {
                    source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT.into(),
                    path: "source".into(),
                },
                output: bcode_workflow::ValueSchema {
                    type_name: "goal".into(),
                    schema: json!({"type":"object"}),
                },
            };
            context
                .graph
                .edges
                .push(bcode_workflow::WorkflowRunGraphEdgeInspection {
                    edge_id,
                    revision: 7,
                    edge: bcode_workflow::EdgeDefinition {
                        from: "integrate".into(),
                        to: "evaluate".into(),
                        kind: bcode_workflow::EdgeKind::Direct,
                        transform: Some(transform.clone()),
                    },
                });
            let compact: serde_json::Value =
                serde_json::from_str(&context_output(&context, true)).unwrap();
            let edge = &compact["graph"]["edges"][0];
            assert!(edge["edge"].get("transform").is_none());
            let request = parse_context(edge["inspection_arguments"].clone()).unwrap();
            assert_eq!(request.expected_revision, Some(7));
            assert_eq!(request.after_edge_id, edge_id.checked_sub(1));
            assert_eq!(request.limit, 1);
            let full: bcode_workflow::WorkflowExecutionContext =
                serde_json::from_str(&context_output(&context, false)).unwrap();
            assert_eq!(full.graph.edges[0].edge.transform, Some(transform));
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
    fn edit_tools_preserve_payloads_through_strict_schema_normalization() {
        let batch = WorkflowRunGraphEditBatch {
            version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
            run_id: "run".to_owned(),
            mutation_id: "edit".to_owned(),
            expected_revision: 1,
            edits: vec![bcode_workflow::WorkflowRunGraphEdit::RemoveEdge { edge_id: 0 }],
            reconciliation: vec![],
        };
        let arguments = json!({"edit_json": serde_json::to_string(&batch).expect("batch")});
        for tool in [
            definition(),
            publication_definition(),
            acceptance_definition(),
        ] {
            let schema = bcode_model_schema::normalize(
                &tool.input_schema,
                &bcode_model_schema::SchemaDialect {
                    object_properties: bcode_model_schema::ObjectPropertyPolicy::RequireAllAndClose,
                    one_of: bcode_model_schema::OneOfPolicy::CollapseAnnotatedConstants,
                    reference_siblings:
                        bcode_model_schema::ReferenceSiblingPolicy::RemoveAnnotationsRejectSemantic,
                    ..bcode_model_schema::SchemaDialect::default()
                },
            )
            .expect("portable strict tool schema");
            let validator = jsonschema::validator_for(&schema).expect("valid schema");
            assert!(validator.is_valid(&arguments), "{}", tool.name);
            for invalid in [
                json!({}),
                json!({"edit_json": null}),
                json!({"edit_json": {}}),
                json!({"edit_json": "{}", "edit": {}}),
            ] {
                assert!(!validator.is_valid(&invalid), "{}", tool.name);
            }
            assert_eq!(parse_edit(&arguments).expect("decoded edit"), batch);
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
