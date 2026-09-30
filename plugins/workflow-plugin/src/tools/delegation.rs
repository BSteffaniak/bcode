//! Source-bound delegation without copying schemas or graph allocation into model context.
use super::{
    CONTEXT_OPERATION, GROUP_NAME, OPERATION, delegation_recipe, invoke_prepared_edit,
    parse_context, task_group,
};
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ToolDefinition, ToolInvocationRequest, ToolInvocationServiceRequest,
    ToolInvocationServiceResolution,
};
use bcode_workflow::WORKFLOW_APPLICATION_INTERFACE_ID;
use serde_json::json;

pub(super) const NAME: &str = "workflow.stage_delegation";

pub(super) fn definition() -> ToolDefinition {
    let mut definition = task_group::definition();
    definition.name = NAME.into();
    definition.description = "Stage workers and an integration continuation from this authenticated source at an explicitly inspected revision. Tooling reads the authenticated source and a bounded outgoing-edge page independently of total graph size and carries schemas, allocation and the exact source-preserving successor transform; unsupported topology requires advanced task-group staging. Supply run_id, expected_revision, bind_source_activation, mutation_id, tasks, continuation and explicit reconciliation. Workspace/access/criteria remain caller choices. This retains the source activation, does not dispatch, and requires separate publication authorization. On conflict rediscover; never silently retry at a new revision.".into();
    let schema = &mut definition.input_schema;
    schema.as_object_mut().unwrap().remove("oneOf");
    schema.as_object_mut().unwrap().remove("if");
    schema.as_object_mut().unwrap().remove("then");
    schema.as_object_mut().unwrap().remove("else");
    let properties = schema["properties"].as_object_mut().unwrap();
    for key in [
        "version",
        "generated_ids",
        "input",
        "source_node_id",
        "include_source_output",
        "preserve_source_output",
        "join_id",
        "first_edge_id",
        "reconnect",
        "retain_source_edge_ids",
    ] {
        properties.remove(key);
    }
    properties.get_mut("continuation").unwrap()["required"] = json!(["objective", "agent_profile"]);
    let continuation = properties.get_mut("continuation").unwrap()["properties"]
        .as_object_mut()
        .unwrap();
    continuation.remove("task_id");
    continuation.remove("output");
    schema["required"] = json!([
        "run_id",
        "expected_revision",
        "bind_source_activation",
        "mutation_id",
        "tasks",
        "continuation",
        "reconciliation"
    ]);
    definition
}

pub(super) fn request(arguments: &serde_json::Value) -> Result<serde_json::Value, String> {
    let value = if let Some(text) = arguments.get("request_json") {
        if arguments.as_object().is_none_or(|object| object.len() != 1) {
            return Err("request_json cannot be combined with delegation fields".into());
        }
        serde_json::from_str(text.as_str().ok_or("request_json must be a string")?)
            .map_err(|_| "invalid delegation JSON")?
    } else {
        arguments.clone()
    };
    let schema = definition().input_schema;
    let object = value.as_object().ok_or("delegation must be an object")?;
    if object
        .keys()
        .any(|key| schema["properties"].get(key).is_none())
        || schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|key| !object.contains_key(key.as_str().unwrap()))
        || value["expected_revision"]
            .as_u64()
            .is_none_or(|revision| revision == 0)
        || ["run_id", "bind_source_activation", "mutation_id"]
            .iter()
            .any(|key| {
                value[key]
                    .as_str()
                    .is_none_or(|text| text.trim().is_empty())
            })
        || value["continuation"].get("task_id").is_some()
        || value["continuation"].get("output").is_some()
    {
        return Err("invalid semantic delegation fields".into());
    }
    Ok(value)
}

pub(super) fn lower(
    request: &serde_json::Value,
    context: &bcode_workflow::WorkflowExecutionContext,
    query: &bcode_workflow::WorkflowExecutionContextRequest,
) -> Result<serde_json::Value, String> {
    if request["run_id"] != json!(context.run_id)
        || request["expected_revision"] != json!(context.graph.revision)
        || request["bind_source_activation"] != json!(context.activation_id)
    {
        return Err(
            "delegation identity or revision changed; rediscover before revising request".into(),
        );
    }
    let recipe = delegation_recipe(context, query);
    let mut arguments = recipe["arguments"].as_object().cloned().ok_or_else(|| {
        recipe["reason"]
            .as_str()
            .unwrap_or("delegation recipe unavailable")
            .to_owned()
    })?;
    arguments.extend(
        request
            .as_object()
            .ok_or("invalid delegation request")?
            .clone(),
    );
    let arguments = serde_json::Value::Object(arguments);
    task_group::parse(&arguments)?;
    Ok(arguments)
}

pub(super) fn invoke(
    context: &NativeServiceContext,
    mut invocation: ToolInvocationRequest,
) -> ServiceResponse {
    let request = match request(&invocation.arguments) {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error),
    };
    let descriptor = &invocation.preparation_descriptor;
    if descriptor.get("operation") != Some(&json!(OPERATION))
        || descriptor.get("edit") != Some(&request)
        || context.cancellation.is_cancelled()
    {
        return ServiceResponse::error("invalid_request", "delegation preparation is not current");
    }
    let Some(route_id) = descriptor
        .get("route_id")
        .and_then(serde_json::Value::as_str)
    else {
        return ServiceResponse::error("invalid_request", "delegation route missing");
    };
    let query = match parse_context(
        json!({"expected_revision": request["expected_revision"], "source_local":true, "limit":2}),
    ) {
        Ok(query) => query,
        Err(error) => return ServiceResponse::error("invalid_request", error),
    };
    let response = context.bridge.request(&ServiceBridgeRequest::InvokeService(
        ToolInvocationServiceRequest {
            invocation_id: invocation.tool_call_id.clone(),
            request_id: invocation.tool_call_id.clone(),
            route_id: Some(route_id.into()),
            interface_id: WORKFLOW_APPLICATION_INTERFACE_ID.into(),
            operation: CONTEXT_OPERATION.into(),
            payload: json!(query),
        },
    ));
    let Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded { payload })) =
        response
    else {
        return ServiceResponse::error(
            "context_unavailable",
            "revision-pinned delegation context unavailable; inspect current state",
        );
    };
    let Ok(execution) = serde_json::from_value(payload) else {
        return ServiceResponse::error("invalid_response", "invalid delegation context");
    };
    let arguments = match lower(&request, &execution, &query) {
        Ok(arguments) => arguments,
        Err(error) => return ServiceResponse::error("delegation_unavailable", error),
    };
    let edit = match task_group::parse(&arguments) {
        Ok(edit) => edit,
        Err(error) => return ServiceResponse::error("invalid_request", error),
    };
    // The authorized semantic request is expanded only from authenticated facts at
    // its pinned revision. Canonical staging still checks authority and reconciliation.
    invocation.name = GROUP_NAME.into();
    invocation.arguments = arguments;
    invocation.preparation_descriptor =
        json!({"operation":OPERATION,"route_id":route_id,"edit":edit});
    invoke_prepared_edit(context, invocation)
}
