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

const ADVANCED_DESCRIPTION: &str = "Supply a typed managed-session request as request_json; retain all mutation IDs after lost acknowledgements. Inspect/reconcile instead of replaying uncertain mutations. Parentage grants no response, approval, cancellation or execution authority beyond delegated control of the calling session's own managed children. Respond permits explicit parent assistance while a parent waits; the host checks current generation and independent response authority. Native tool observations never invoke Bcode tools. Interrupt acknowledgement is not settlement.";

fn request_json_property() -> Value {
    json!({"type":"string", "maxLength":MAX_REQUEST_BYTES, "description":"Advanced: ManagedSessionRequest: harness (versioned HarnessRequest), parent_session_id (creation only or null), assistance_destination_session_id (explicit host-admitted destination or null). Never include secrets/auth inputs."})
}

fn advanced_definition(op: &str) -> ToolDefinition {
    let (description, schema) = match op {
        "respond" => (
            format!(
                "Answer a pending approval or question of a delegated external agent session. Simple form: session_id, request_id (from harness.result) and either approve (approvals) or answer with optional choice_id (questions). The host validates the request is still pending and current. Advanced form: {ADVANCED_DESCRIPTION}"
            ),
            json!({"type":"object", "additionalProperties":false, "properties": {
                "session_id": {"type":"string", "description":"Delegated child session_id."},
                "request_id": {"type":"string", "description":"Pending request_id from harness.result."},
                "approve": {"type":"boolean", "description":"Approval decision for an approval request."},
                "answer": {"type":"string", "description":"Answer text for a question."},
                "choice_id": {"type":"string", "description":"Optional choice_id for a question answer."},
                "request_json": request_json_property(),
            }}),
        ),
        "interrupt" => (
            format!(
                "Request interruption of a delegated external agent session's active turn. Simple form: session_id. Acknowledgement is not settlement; poll harness.result. Advanced form: {ADVANCED_DESCRIPTION}"
            ),
            json!({"type":"object", "additionalProperties":false, "properties": {
                "session_id": {"type":"string", "description":"Delegated child session_id."},
                "request_json": request_json_property(),
            }}),
        ),
        _ => (
            format!("Managed external session {op} (advanced). {ADVANCED_DESCRIPTION}"),
            json!({"type":"object", "additionalProperties":false, "required":["request_json"], "properties": {
                "request_json": request_json_property(),
            }}),
        ),
    };
    ToolDefinition {
        name: format!("harness.{op}"),
        description,
        input_schema: schema,
    }
}

fn simple_definitions(instances: &str) -> [ToolDefinition; 3] {
    [
        ToolDefinition {
            name: crate::delegation::DELEGATE_TOOL.into(),
            description: format!("Delegate a self-contained coding task to a configured external coding agent. Opens an ordinary child session of this session in the same workspace and submits prompt as its first turn, then returns immediately with session_id; the child keeps running in the background. Write a complete, standalone prompt. Configured instances: {instances}. Poll harness.result for progress."),
            input_schema: json!({"type":"object", "additionalProperties":false, "required":["prompt"], "properties": {
                "instance_id": {"type":"string", "description":"Configured instance reference; omit for the default."},
                "prompt": {"type":"string", "description":"Complete task for the external agent."},
            }}),
        },
        ToolDefinition {
            name: crate::delegation::CONTINUE_TOOL.into(),
            description: "Send the next turn to a delegated external agent session once harness.result reports it settled. Returns immediately; poll harness.result.".into(),
            input_schema: json!({"type":"object", "additionalProperties":false, "required":["session_id","prompt"], "properties": {
                "session_id": {"type":"string", "description":"Delegated child session_id from harness.delegate."},
                "prompt": {"type":"string", "description":"Follow-up instruction."},
            }}),
        },
        ToolDefinition {
            name: crate::delegation::RESULT_TOOL.into(),
            description: "Check a delegated external agent session: activity (running, settled, needs_attention, interrupted, failed), latest_result text, pending approvals/questions (request_id, previews) to answer with harness.respond, and a cursor. Pass the cursor as after to read only newer activity.".into(),
            input_schema: json!({"type":"object", "additionalProperties":false, "required":["session_id"], "properties": {
                "session_id": {"type":"string", "description":"Delegated child session_id."},
                "after": {"type":"string", "description":"Cursor from a previous harness.result."},
            }}),
        },
    ]
}

fn definitions(config: &Value) -> Vec<ToolDefinition> {
    let mut definitions = OPERATIONS
        .into_iter()
        .map(advanced_definition)
        .collect::<Vec<_>>();
    // Simple delegation is offered only when an external agent instance is configured.
    if let Some(instances) = crate::delegation::configured_instances_summary(config) {
        definitions.extend(simple_definitions(&instances));
    }
    definitions
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

fn route_for(
    request: &bcode_tool::ToolPreparationRequest,
    operations: &[&str],
) -> Result<String, String> {
    let mut selected = None;
    for op in operations {
        let route_id = route(request, op)?;
        if selected.get_or_insert_with(|| route_id.clone()) != &route_id {
            return Err("one exact managed-session route is required".into());
        }
    }
    selected.ok_or_else(|| "one exact managed-session route is required".into())
}

fn prepare_simple(
    context: &NativeServiceContext,
    request: &bcode_tool::ToolPreparationRequest,
    invocation: &crate::delegation::SimpleInvocation,
) -> Result<ToolPolicyPreparation, String> {
    let route_id = route_for(request, invocation.required_operations())?;
    let scope = crate::delegation::InvocationScope::from_host_context(&request.host_context)?;
    let instance = match invocation {
        crate::delegation::SimpleInvocation::Delegate(input) => {
            Some(crate::delegation::resolve_instance(
                &context.config.config,
                input.instance_id.as_deref(),
            )?)
        }
        _ => None,
    };
    let read_only = invocation.read_only();
    Ok(ToolPolicyPreparation::new(
        !read_only,
        if read_only {
            ToolPolicyOperation::ReadOnly
        } else {
            ToolPolicyOperation::Mutating
        },
    )
    .with_descriptor(json!({
        "simple": invocation,
        "route_id": route_id,
        "scope": scope,
        "instance": instance,
    })))
}

pub fn invoke(context: &NativeServiceContext) -> ServiceResponse {
    match context.request.operation.as_str() {
        bcode_tool::OP_LIST_TOOLS => json_response(&ToolList::with_runtime_stable_discovery(
            definitions(&context.config.config),
            |_| bcode_tool::ToolDiscoveryPolicy::new(true),
        )),
        bcode_tool::OP_PREPARE_TOOL => prepare_tool_service_response(
            &context.request,
            definitions(&context.config.config),
            |request, _| {
                if let Some(invocation) = crate::delegation::SimpleInvocation::parse(
                    &request.invocation.tool_name,
                    &request.invocation.arguments,
                )? {
                    return prepare_simple(context, request, &invocation);
                }
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
            },
        ),
        bcode_tool::OP_INVOKE_TOOL => invoke_prepared(context),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported harness tool operation",
        ),
    }
}

/// Production managed application access through the host-issued invocation route.
struct HostApplication<'a> {
    context: &'a NativeServiceContext,
    invocation_id: &'a str,
    route_id: &'a str,
}

impl crate::delegation::ManagedApplication for HostApplication<'_> {
    fn call(
        &self,
        operation: &str,
        payload: Value,
    ) -> Result<Value, crate::delegation::CallFailure> {
        use crate::delegation::CallFailure;
        if self.context.cancellation.is_cancelled() {
            return Err(CallFailure::Unknown);
        }
        match self
            .context
            .bridge
            .request(&ServiceBridgeRequest::InvokeService(
                ToolInvocationServiceRequest {
                    request_id: self.invocation_id.to_owned(),
                    invocation_id: self.invocation_id.to_owned(),
                    route_id: Some(self.route_id.to_owned()),
                    interface_id: MANAGED_SESSION_INTERFACE_ID.into(),
                    operation: operation.into(),
                    payload,
                },
            )) {
            Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded {
                payload,
            })) => Ok(payload),
            // Fixed host codes only; native diagnostics never cross this boundary.
            Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Failed {
                code,
                ..
            })) if code.len() <= 128
                && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                Err(CallFailure::Host(code))
            }
            Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Unsupported)) => {
                Err(CallFailure::Host("unsupported".into()))
            }
            // Cancellation, timeout or a lost reply: the outcome is unknown, never retried.
            _ => Err(CallFailure::Unknown),
        }
    }
}

fn invoke_simple(
    context: &NativeServiceContext,
    request: &ToolInvocationRequest,
    invocation: &crate::delegation::SimpleInvocation,
) -> ServiceResponse {
    let descriptor = &request.preparation_descriptor;
    let route_id = descriptor["route_id"].as_str().filter(|id| !id.is_empty());
    let scope =
        serde_json::from_value::<crate::delegation::InvocationScope>(descriptor["scope"].clone());
    let instance = serde_json::from_value::<Option<bcode_harness_models::HarnessInstance>>(
        descriptor["instance"].clone(),
    );
    let matches = serde_json::to_value(invocation).ok().as_ref() == descriptor.get("simple");
    let (Some(route_id), Ok(scope), Ok(instance), true) = (route_id, scope, instance, matches)
    else {
        return ServiceResponse::error(
            "stale_preparation",
            "managed-session preparation is not current",
        );
    };
    if context.cancellation.is_cancelled() {
        return ServiceResponse::error(
            "stale_preparation",
            "managed-session preparation is not current",
        );
    }
    let application = HostApplication {
        context,
        invocation_id: &request.tool_call_id,
        route_id,
    };
    let outcome = crate::delegation::execute(
        &application,
        &scope,
        &request.tool_call_id,
        invocation,
        instance.as_ref(),
    );
    json_response(&ToolInvocationResponse {
        output: outcome.output.to_string(),
        is_error: outcome.is_error,
        content: Vec::new(),
        full_output: None,
        result: None,
    })
}

fn invoke_prepared(context: &NativeServiceContext) -> ServiceResponse {
    let Ok(request) = context.request.payload_json::<ToolInvocationRequest>() else {
        return ServiceResponse::error("invalid_request", "invalid harness invocation");
    };
    match crate::delegation::SimpleInvocation::parse(&request.name, &request.arguments) {
        Ok(Some(invocation)) => return invoke_simple(context, &request, &invocation),
        Ok(None) => {}
        Err(_) => return ServiceResponse::error("invalid_request", "invalid managed request"),
    }
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
        assert_eq!(definitions(&Value::Null).len(), 5);
        for definition in definitions(&Value::Null) {
            assert!(operation(&definition.name).is_ok());
        }
        assert!(operation("harness.auth_continue").is_err());
    }

    #[test]
    fn simple_delegation_tools_are_offered_only_with_a_configured_instance() {
        let config = json!({"enabled":true, "default_instance":"claude", "instances":{"claude":{
            "enabled":true, "instance":{"instance_id":"claude","plugin_id":"bcode.claude-code",
            "executable":"/explicit/claude","config_namespace":"/isolated",
            "auth":{"provider_id":"claude-code","profile":"native","credential_namespace":"/isolated",
            "custody":"native_managed"}}}}});
        let names = definitions(&config)
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        for tool in crate::delegation::SIMPLE_TOOLS {
            assert!(names.iter().any(|name| name == tool), "{tool} missing");
        }
        let delegate = definitions(&config)
            .into_iter()
            .find(|definition| definition.name == crate::delegation::DELEGATE_TOOL)
            .unwrap();
        assert!(
            delegate
                .description
                .contains("claude (bcode.claude-code, default)")
        );
        let mut disabled = config;
        disabled["enabled"] = json!(false);
        assert_eq!(definitions(&disabled).len(), 5);
    }

    #[test]
    fn simple_preparation_requires_routes_for_every_used_operation() {
        let routes = |operations: &[&str]| bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".into(),
                tool_name: crate::delegation::RESULT_TOOL.into(),
                arguments: json!({}),
            },
            host_context: vec![bcode_tool::ToolHostContextEntry {
                schema: bcode_tool::TOOL_INVOCATION_SERVICE_ROUTES_SCHEMA.into(),
                schema_version: 1,
                payload: json!([{"route_id":"host-route","interface_id":MANAGED_SESSION_INTERFACE_ID,
                    "operations":operations}]),
            }],
        };
        let needed = ["binding", "start", "followup"];
        assert_eq!(route_for(&routes(&needed), &needed).unwrap(), "host-route");
        assert!(route_for(&routes(&["start", "followup"]), &needed).is_err());
    }
}
