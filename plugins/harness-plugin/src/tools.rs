//! Invocation-bound tools. A bridge request is never a direct native dispatch.
use crate::protocol::bounded_response;
use crate::{
    MANAGED_SESSION_INTERFACE_ID, MAX_REQUEST_BYTES, OPERATIONS, json_response, validated_payload,
};
use bcode_plugin_sdk::prelude::*;
use bcode_plugin_sdk::{ToolPolicyOperation, ToolPolicyPreparation};
use bcode_tool::{
    ToolDefinition, ToolInvocationRequest, ToolInvocationResponse, ToolInvocationServiceRequest,
    ToolInvocationServiceResolution, ToolList,
};
use serde_json::{Value, json};

fn operation(name: &str) -> Result<&str, String> {
    name.strip_prefix("harness.")
        .filter(|op| OPERATIONS.contains(op))
        .ok_or_else(|| "unknown harness tool".into())
}

fn definitions() -> Vec<ToolDefinition> {
    OPERATIONS.into_iter().map(|op| ToolDefinition {
        name: format!("harness.{op}"),
        description: format!("Managed external session {op}. Supply a typed managed-session request as request_json; retain all mutation IDs after lost acknowledgements. Inspect/reconcile instead of replaying uncertain mutations. Parentage grants no response, approval, cancellation or execution authority. Respond permits explicit parent assistance while a parent waits; the host checks current generation and independent response authority. Native tool observations never invoke Bcode tools. Interrupt acknowledgement is not settlement."),
        input_schema: json!({"type":"object", "additionalProperties":false, "required":["request_json"], "properties": {
            "request_json": {"type":"string", "maxLength":MAX_REQUEST_BYTES, "description":"ManagedSessionRequest: harness (versioned HarnessRequest), parent_session_id (creation only or null), assistance_destination_session_id (explicit host-admitted destination or null). Never include secrets/auth inputs."}
        }}),
    }).collect()
}

pub fn payload(op: &str, arguments: &Value) -> Result<Value, String> {
    let object = arguments.as_object().ok_or("expected request_json")?;
    if object.len() != 1 {
        return Err("unexpected harness arguments".into());
    }
    let encoded = object
        .get("request_json")
        .and_then(Value::as_str)
        .filter(|s| s.len() <= MAX_REQUEST_BYTES)
        .ok_or("expected bounded request_json")?;
    validated_payload(
        op,
        serde_json::from_str(encoded).map_err(|_| "invalid request JSON")?,
    )
    .map_err(str::to_owned)
}

fn route(request: &bcode_tool::ToolPreparationRequest, op: &str) -> Result<String, String> {
    let mut routes = Vec::new();
    for entry in &request.host_context {
        if entry.schema != bcode_tool::TOOL_INVOCATION_SERVICE_ROUTES_SCHEMA {
            continue;
        }
        if entry.schema_version != 1 {
            return Err("unsupported managed route version".into());
        }
        let entries: Vec<bcode_tool::ToolInvocationServiceRoute> =
            serde_json::from_value(entry.payload.clone()).map_err(|_| "invalid managed routes")?;
        routes.extend(entries.into_iter().filter(|r| {
            r.interface_id == MANAGED_SESSION_INTERFACE_ID
                && r.operations.iter().any(|allowed| allowed == op)
        }));
    }
    if routes.len() != 1 || routes[0].route_id.is_empty() {
        return Err("one exact managed-session route is required".into());
    }
    Ok(routes.remove(0).route_id)
}

pub fn invoke(context: &NativeServiceContext) -> ServiceResponse {
    match context.request.operation.as_str() {
        bcode_tool::OP_LIST_TOOLS => json_response(&ToolList::with_runtime_stable_discovery(
            definitions(),
            |_| bcode_tool::ToolDiscoveryPolicy::new(true),
        )),
        bcode_tool::OP_PREPARE_TOOL => {
            prepare_tool_service_response(&context.request, definitions(), |request, _| {
                let op = operation(&request.invocation.tool_name)?;
                let payload = payload(op, &request.invocation.arguments)?;
                let route_id = route(request, op)?;
                let read_only = op == "inspect";
                Ok(ToolPolicyPreparation::new(
                    !read_only,
                    if read_only {
                        ToolPolicyOperation::ReadOnly
                    } else {
                        ToolPolicyOperation::Mutating
                    },
                )
                .with_descriptor(json!({"operation":op,"payload":payload,"route_id":route_id})))
            })
        }
        bcode_tool::OP_INVOKE_TOOL => invoke_prepared(context),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported harness tool operation",
        ),
    }
}

fn invoke_prepared(context: &NativeServiceContext) -> ServiceResponse {
    let Ok(request) = context.request.payload_json::<ToolInvocationRequest>() else {
        return ServiceResponse::error("invalid_request", "invalid harness invocation");
    };
    let Ok(op) = operation(&request.name) else {
        return ServiceResponse::error("unknown_tool", "unknown harness tool");
    };
    let Ok(payload) = payload(op, &request.arguments) else {
        return ServiceResponse::error("invalid_request", "invalid managed request");
    };
    let descriptor = &request.preparation_descriptor;
    if !prepared_matches(descriptor, op, &payload) || context.cancellation.is_cancelled() {
        return ServiceResponse::error(
            "stale_preparation",
            "managed-session preparation is not current",
        );
    }
    let route_id = descriptor["route_id"].as_str().map(str::to_owned);
    match bridge(context, request.tool_call_id, route_id, op, payload) {
        Ok(value) => json_response(&ToolInvocationResponse {
            output: value.to_string(),
            is_error: false,
            content: Vec::new(),
            full_output: None,
            result: None,
        }),
        Err(()) => ServiceResponse::error(
            "managed_session_unavailable",
            "managed-session operation requires reconciliation or application admission",
        ),
    }
}

fn prepared_matches(descriptor: &Value, op: &str, payload: &Value) -> bool {
    descriptor["operation"].as_str() == Some(op)
        && descriptor.get("payload") == Some(payload)
        && descriptor["route_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
}

pub fn bridge(
    context: &NativeServiceContext,
    invocation_id: String,
    route_id: Option<String>,
    operation: &str,
    payload: Value,
) -> Result<Value, ()> {
    if context.cancellation.is_cancelled() {
        return Err(());
    }
    let request: bcode_harness_models::HarnessRequest =
        serde_json::from_value(payload["harness"].clone()).map_err(|_| ())?;
    match context.bridge.request(&ServiceBridgeRequest::InvokeService(
        ToolInvocationServiceRequest {
            request_id: invocation_id.clone(),
            invocation_id,
            route_id,
            interface_id: MANAGED_SESSION_INTERFACE_ID.into(),
            operation: operation.into(),
            payload,
        },
    )) {
        Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded {
            payload,
        })) => {
            bounded_response(&payload).map_err(|_| ())?;
            let response: bcode_harness_models::HarnessServiceResponse =
                serde_json::from_value(payload).map_err(|_| ())?;
            response.validate_for(&request).map_err(|_| ())?;
            if matches!(
                response.kind,
                bcode_harness_models::HarnessServiceResponseKind::Failure(_)
            ) {
                return Err(());
            }
            serde_json::to_value(response).map_err(|_| ())
        }
        // Never repeat a bridge mutation after cancellation, timeout, unsupported routing or
        // a lost response; the retained IDs are the only reconciliation handles.
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_preparation_binds_operation_payload_and_route() {
        let input = json!({"submission_id":"retained", "generation":2});
        let prepared = json!({"operation":"followup", "payload":input, "route_id":"host-route"});
        assert!(prepared_matches(&prepared, "followup", &input));
        assert!(!prepared_matches(&prepared, "interrupt", &input));
        assert!(!prepared_matches(
            &prepared,
            "followup",
            &json!({"submission_id":"retained", "generation":3})
        ));
        assert!(!prepared_matches(
            &json!({"operation":"followup","payload":input}),
            "followup",
            &input
        ));
    }
    #[test]
    fn all_five_operations_are_discoverable_without_native_execution() {
        assert_eq!(definitions().len(), 5);
        for definition in definitions() {
            assert!(operation(&definition.name).is_ok());
        }
        assert!(operation("harness.auth_continue").is_err());
    }
}
