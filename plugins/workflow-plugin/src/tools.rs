//! Plugin-owned execution-scoped graph staging tool.

use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ToolDefinition, ToolInvocationRequest, ToolInvocationServiceRequest,
    ToolInvocationServiceResolution, ToolList,
};
use bcode_workflow::{WORKFLOW_APPLICATION_INTERFACE_ID, WorkflowRunGraphEditBatch};
use serde_json::json;

const NAME: &str = "workflow.stage_run_graph_edit";
const OPERATION: &str = "stage_run_graph_edit";
const PUBLISH_NAME: &str = "workflow.publish_run_graph_edit";
const PUBLISH_OPERATION: &str = "publish_run_graph_edit";

fn publication_definition() -> ToolDefinition {
    ToolDefinition {
        name: PUBLISH_NAME.to_owned(),
        description: "Publish an exact previously staged workflow edit for this active execution. Requires separate publication authorization and explicit active-work reconciliation. Preserve the staged edit and mutation_id exactly when retrying; unsupported topology is rejected.".to_owned(),
        input_schema: definition().input_schema,
    }
}

fn operation(name: &str) -> Result<&'static str, String> {
    match name {
        NAME => Ok(OPERATION),
        PUBLISH_NAME => Ok(PUBLISH_OPERATION),
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
            tools: vec![definition(), publication_definition()],
        }),
        bcode_tool::OP_PREPARE_TOOL => prepare_tool_service_response(
            &context.request,
            [definition(), publication_definition()],
            |request, _| {
                let operation = operation(&request.invocation.tool_name)?;
                parse_edit(&request.invocation.arguments)?;
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
                    true,
                    bcode_plugin_sdk::ToolPolicyOperation::Mutating,
                )
                .with_descriptor(json!({"route_id":route.route_id})))
            },
        ),
        bcode_tool::OP_INVOKE_TOOL => invoke_edit(context),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported workflow tool operation",
        ),
    }
}

fn invoke_edit(context: &NativeServiceContext) -> ServiceResponse {
    let Ok(request) = context.request.payload_json::<ToolInvocationRequest>() else {
        return ServiceResponse::error("invalid_request", "invalid workflow tool invocation");
    };
    let Ok(operation) = operation(&request.name) else {
        return ServiceResponse::error("unsupported_tool", "unsupported workflow tool");
    };
    let edit = match parse_edit(&request.arguments) {
        Ok(edit) => edit,
        Err(message) => return ServiceResponse::error("invalid_request", message),
    };
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
            payload: match serde_json::to_value(edit) {
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
            if operation == PUBLISH_OPERATION {
                publication_response(&payload)
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
