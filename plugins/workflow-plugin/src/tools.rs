//! Plugin-owned execution-scoped graph staging tool.

use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ToolDefinition, ToolInvocationRequest, ToolInvocationServiceRequest,
    ToolInvocationServiceResolution, ToolList,
};
use bcode_workflow::{WORKFLOW_APPLICATION_INTERFACE_ID, WorkflowRunGraphEditBatch};
use serde_json::json;

const CONTEXT_NAME: &str = "workflow.execution_context";
const CONTEXT_OPERATION: &str = "execution_context";

fn context_definition() -> ToolDefinition {
    ToolDefinition {
        name: CONTEXT_NAME.to_owned(),
        description: "Read this active workflow execution's authenticated identity and bounded graph page. Omit revision and cursors initially; continue with the returned revision and last node/edge identities. Restart on revision conflict. This grants no mutation authority.".to_owned(),
        input_schema: json!({"type":"object", "additionalProperties":false,
            "required":["limit"], "properties": {
                "after_output_id":{"type":["string","null"], "description":"Exclusive last output ID. Outputs arriving behind the cursor require a fresh scan; this is not a durable event stream."},
                "output_id":{"type":["string","null"], "description":"Exact canonical output identity from this run; returns checksum-verified value without opening artifacts."},
                "limit":{"type":"integer", "minimum":1, "maximum":100},
                "expected_revision":{"type":["integer","null"], "minimum":1},
                "after_node_id":{"type":["string","null"]},
                "after_edge_id":{"type":["integer","null"]}
            }}),
    }
}

const TASK_NAME: &str = "workflow.stage_agent_task";

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
    reconciliation: Vec<bcode_workflow::WorkflowRunGraphReconciliation>,
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
                "entry":{"type":"boolean"},"exit":{"type":"boolean"},
                "reconciliation":{"type":"array"}
            }}),
    }
}

fn parse_tool_edit(
    name: &str,
    arguments: &serde_json::Value,
) -> Result<WorkflowRunGraphEditBatch, String> {
    if name != TASK_NAME {
        return parse_edit(arguments);
    }
    let task: AgentTaskRequest = serde_json::from_value(arguments.clone())
        .map_err(|_| "invalid agent task request".to_owned())?;
    if task.node.kind != bcode_workflow::NodeKind::Agent {
        return Err("agent task requires an Agent node".to_owned());
    }
    let _: bcode_workflow::WorkflowPromptConfiguration =
        serde_json::from_value(task.node.configuration.clone())
            .map_err(|_| "invalid agent prompt configuration".to_owned())?;
    let edit = WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: task.run_id,
        expected_revision: task.expected_revision,
        mutation_id: task.mutation_id,
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::AddNode {
            node: task.node,
            entry: task.entry,
            exit: task.exit,
        }],
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
        input_schema: definition().input_schema,
    }
}

fn publication_definition() -> ToolDefinition {
    ToolDefinition {
        name: PUBLISH_NAME.to_owned(),
        description: "Publish an exact previously staged workflow edit for this active execution. Requires separate publication authorization and explicit active-work reconciliation. Preserve the staged edit and mutation_id exactly when retrying; unsupported topology is rejected.".to_owned(),
        input_schema: definition().input_schema,
    }
}

fn operation(name: &str) -> Result<&'static str, String> {
    match name {
        NAME | TASK_NAME => Ok(OPERATION),
        PUBLISH_NAME => Ok(PUBLISH_OPERATION),
        ACCEPT_NAME => Ok(ACCEPT_OPERATION),
        _ => Err("unsupported workflow tool".to_owned()),
    }
}

fn definition() -> ToolDefinition {
    ToolDefinition {
        name: NAME.to_owned(),
        description: "Stage a revision-checked edit for the workflow run owning this active execution. Requires workflow application authorization. Does not publish or execute topology. Supply a serialized WorkflowRunGraphEditBatch as edit_json; preserve mutation_id when retrying.".to_owned(),
        input_schema: json!({"type":"object", "additionalProperties":false,
            "required":["edit_json"], "properties":{"edit_json":{"type":"string",
            "description":"JSON WorkflowRunGraphEditBatch: version, run_id, mutation_id, expected_revision, edits, reconciliation."}}}),
    }
}

fn parse_edit(arguments: &serde_json::Value) -> Result<WorkflowRunGraphEditBatch, String> {
    let text = arguments
        .get("edit_json")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "edit_json must be a string".to_owned())?;
    let edit: WorkflowRunGraphEditBatch = serde_json::from_str(text)
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
            ],
            |request, _| {
                let is_context = request.invocation.tool_name == CONTEXT_NAME;
                let operation = if is_context {
                    CONTEXT_OPERATION
                } else {
                    operation(&request.invocation.tool_name)?
                };
                let payload = if is_context {
                    let context: bcode_workflow::WorkflowExecutionContextRequest =
                        serde_json::from_value(request.invocation.arguments.clone())
                            .map_err(|_| "invalid execution context request".to_owned())?;
                    if !(1..=100).contains(&context.limit) {
                        return Err("invalid execution context limit".to_owned());
                    }
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
    let Ok(query) = serde_json::from_value::<bcode_workflow::WorkflowExecutionContextRequest>(
        request.arguments,
    ) else {
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
            } else if request.name == TASK_NAME {
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
    fn staging_tool_validates_current_edit_contract() {
        let batch = WorkflowRunGraphEditBatch {
            version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
            run_id: "run".to_owned(),
            mutation_id: "edit".to_owned(),
            expected_revision: 1,
            edits: vec![bcode_workflow::WorkflowRunGraphEdit::RemoveEdge { edge_id: 0 }],
            reconciliation: vec![],
        };
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
