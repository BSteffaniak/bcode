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

fn delegate_description(native: bool, instances: Option<&str>) -> String {
    let targets = match (native, instances) {
        (true, Some(instances)) => format!(
            "Targets: instance_id \"bcode\" for a Bcode child session (same Bcode, daemon and state as this session), or an external coding agent: {instances}. Without instance_id the configured default external agent is used, else a Bcode child."
        ),
        (true, None) => "The child is a Bcode session served by this same Bcode (never run `bcode` through the shell to create sub-sessions).".to_owned(),
        (false, Some(instances)) => format!("Configured external coding agents: {instances}."),
        (false, None) => String::new(),
    };
    format!(
        "Delegate a self-contained task to a child session of this session. Submits prompt as the child's first turn and returns immediately with session_id while the child keeps running in the background. Write a complete, standalone prompt: the child does not see this conversation. {targets} Follow children with harness.wait (block until they settle or need attention) or harness.result; send follow-ups with harness.continue."
    )
}

fn simple_definitions(native: bool, instances: Option<&str>) -> Vec<ToolDefinition> {
    let mut delegate_properties = json!({
        "instance_id": {"type":"string", "description": if native {
            "\"bcode\" for a Bcode child, or a configured external agent instance; omit for the default."
        } else {
            "Configured instance reference; omit for the default."
        }},
        "prompt": {"type":"string", "description":"Complete, standalone task for the child."},
    });
    if native
        && let (Some(properties), Value::Object(options)) = (
            delegate_properties.as_object_mut(),
            crate::native::delegate_option_properties(),
        )
    {
        properties.extend(options);
    }
    let mut definitions = vec![
        ToolDefinition {
            name: crate::delegation::DELEGATE_TOOL.into(),
            description: delegate_description(native, instances),
            input_schema: json!({"type":"object", "additionalProperties":false, "required":["prompt"], "properties": delegate_properties}),
        },
        ToolDefinition {
            name: crate::delegation::CONTINUE_TOOL.into(),
            description: "Send the next turn to a delegated child session, normally once it has settled. Returns immediately; follow it with harness.wait or harness.result.".into(),
            input_schema: json!({"type":"object", "additionalProperties":false, "required":["session_id","prompt"], "properties": {
                "session_id": {"type":"string", "description":"Delegated child session_id from harness.delegate."},
                "prompt": {"type":"string", "description":"Follow-up instruction."},
            }}),
        },
        ToolDefinition {
            name: crate::delegation::RESULT_TOOL.into(),
            description: "Check a delegated child session: status (running, settled, needs_attention, interrupted, failed), latest_result text and pending approvals/questions. Settled means idle, not accepted: review latest_result. For external agents, pass the returned cursor as after to read only newer activity.".into(),
            input_schema: json!({"type":"object", "additionalProperties":false, "required":["session_id"], "properties": {
                "session_id": {"type":"string", "description":"Delegated child session_id."},
                "after": {"type":"string", "description":"External agents only: cursor from a previous harness.result."},
            }}),
        },
    ];
    if native {
        definitions.push(ToolDefinition {
            name: crate::native::WAIT_TOOL.into(),
            description: "Block until delegated Bcode children settle, fail or need attention (until \"any\", the default, or \"all\"), or the timeout passes. Returns each child's status and latest_result. Prefer this to repeatedly calling harness.result.".into(),
            input_schema: json!({"type":"object", "additionalProperties":false, "required":["session_ids"], "properties": {
                "session_ids": {"type":"array", "items":{"type":"string"}, "minItems":1, "maxItems": bcode_session_models::delegation::MAX_DELEGATION_BATCH, "description":"Delegated Bcode child session_ids."},
                "until": {"type":"string", "enum":["any","all"], "description":"Return when any child (default) or all children stop running."},
                "timeout_seconds": {"type":"integer", "minimum":0, "maximum": bcode_session_models::delegation::MAX_DELEGATION_WAIT_MS / 1000, "description":"Deadline in seconds (default 300)."},
            }}),
        });
        definitions.push(ToolDefinition {
            name: crate::native::CONTEXT_TOOL.into(),
            description: "Describe this Bcode session for delegation: session_id, parent, nesting depth, workspace, current model, available agent profiles, existing children with their status, and the delegation limits.".into(),
            input_schema: json!({"type":"object", "additionalProperties":false, "properties": {}}),
        });
    }
    definitions
}

fn definitions(config: &Value) -> Vec<ToolDefinition> {
    let mut definitions = OPERATIONS
        .into_iter()
        .map(advanced_definition)
        .collect::<Vec<_>>();
    let native = crate::native::settings(config).is_ok_and(|settings| settings.enabled);
    let instances = crate::delegation::configured_instances_summary(config);
    // Simple delegation is offered when Bcode children or an external agent are available.
    if native || instances.is_some() {
        definitions.extend(simple_definitions(native, instances.as_deref()));
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
    interface_route(request, MANAGED_SESSION_INTERFACE_ID, op)
}

fn interface_route(
    request: &bcode_tool::ToolPreparationRequest,
    interface_id: &str,
    op: &str,
) -> Result<String, String> {
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
            r.interface_id == interface_id && r.operations.iter().any(|allowed| allowed == op)
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

/// The native delegation route offering `operation`, when the host advertises it.
fn native_route(request: &bcode_tool::ToolPreparationRequest, operation: &str) -> Option<String> {
    interface_route(
        request,
        bcode_session_models::delegation::NATIVE_DELEGATION_INTERFACE_ID,
        operation,
    )
    .ok()
}

/// Native operation used first by `invocation`, if it has a native form.
const fn native_operation(invocation: &crate::delegation::SimpleInvocation) -> &'static str {
    use crate::delegation::SimpleInvocation as S;
    use bcode_session_models::delegation as d;
    match invocation {
        S::Delegate(_) => d::NATIVE_DELEGATION_DELEGATE_OPERATION,
        S::Continue(_) => d::NATIVE_DELEGATION_CONTINUE_OPERATION,
        S::Result(_) | S::Respond(_) => d::NATIVE_DELEGATION_RESULT_OPERATION,
        S::Interrupt(_) => d::NATIVE_DELEGATION_INTERRUPT_OPERATION,
        S::Wait(_) => d::NATIVE_DELEGATION_WAIT_OPERATION,
        S::Context(_) => d::NATIVE_DELEGATION_CONTEXT_OPERATION,
    }
}

/// Whether a delegate input selects a native Bcode child.
///
/// # Errors
///
/// * Native-only options for an external target, or a disabled native target.
fn delegates_natively(
    config: &Value,
    native: crate::native::NativeSettings,
    input: &crate::delegation::DelegateInput,
) -> Result<bool, String> {
    let native_target = match input.instance_id.as_deref() {
        Some(crate::native::NATIVE_TARGET) => true,
        Some(_) => false,
        None => crate::configuration::catalog_value(config)
            .ok()
            .and_then(|catalog| catalog.default_instance)
            .is_none(),
    };
    if native_target && !native.enabled {
        return Err("Bcode child delegation is disabled".into());
    }
    if !native_target && !input.native_options().is_empty() {
        return Err("title, agent, model, cwd, worktree, permissions and cancel_with_turn apply only to Bcode children (instance_id \"bcode\")".into());
    }
    Ok(native_target)
}

fn prepare_simple(
    context: &NativeServiceContext,
    request: &bcode_tool::ToolPreparationRequest,
    invocation: &crate::delegation::SimpleInvocation,
) -> Result<ToolPolicyPreparation, String> {
    use crate::delegation::SimpleInvocation as S;
    let config = &context.config.config;
    let native = crate::native::settings(config)?;
    let scope = crate::delegation::InvocationScope::from_host_context(&request.host_context)?;
    let native_target = match invocation {
        S::Delegate(input) => delegates_natively(config, native, input)?,
        S::Wait(_) | S::Context(_) => {
            if !native.enabled {
                return Err("Bcode child delegation is disabled".into());
            }
            true
        }
        _ => false,
    };
    let native_route_id = if native.enabled {
        native_route(request, native_operation(invocation))
    } else {
        None
    };
    if native_target && native_route_id.is_none() {
        return Err("the host does not offer native delegation for this invocation".into());
    }
    // External routes are required for external targets and optional for native-first
    // follow-ups, which fall back to them for external children.
    let route_id = match route_for(request, invocation.required_operations()) {
        Ok(route_id) => Some(route_id),
        Err(_) if native_target => None,
        Err(_) if native_route_id.is_some() && !matches!(invocation, S::Delegate(_)) => None,
        Err(error) => return Err(error),
    };
    let instance = match invocation {
        S::Delegate(input) if !native_target => Some(crate::delegation::resolve_instance(
            config,
            input.instance_id.as_deref(),
        )?),
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
        "native_route_id": native_route_id,
        "native_target": native_target,
        "native_limits": native.limits,
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
    interface_id: &'a str,
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
                    interface_id: self.interface_id.into(),
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
    let native_route_id = descriptor["native_route_id"]
        .as_str()
        .filter(|id| !id.is_empty());
    let native_target = descriptor["native_target"].as_bool().unwrap_or(false);
    let limits = serde_json::from_value::<bcode_session_models::delegation::NativeDelegationLimits>(
        descriptor["native_limits"].clone(),
    );
    let scope =
        serde_json::from_value::<crate::delegation::InvocationScope>(descriptor["scope"].clone());
    let instance = serde_json::from_value::<Option<bcode_harness_models::HarnessInstance>>(
        descriptor["instance"].clone(),
    );
    let matches = serde_json::to_value(invocation).ok().as_ref() == descriptor.get("simple");
    let (Ok(scope), Ok(instance), Ok(limits), true) = (scope, instance, limits, matches) else {
        return ServiceResponse::error(
            "stale_preparation",
            "managed-session preparation is not current",
        );
    };
    if context.cancellation.is_cancelled() || (route_id.is_none() && native_route_id.is_none()) {
        return ServiceResponse::error(
            "stale_preparation",
            "managed-session preparation is not current",
        );
    }
    let native_outcome = native_route_id.and_then(|native_route_id| {
        let application = HostApplication {
            context,
            invocation_id: &request.tool_call_id,
            route_id: native_route_id,
            interface_id: bcode_session_models::delegation::NATIVE_DELEGATION_INTERFACE_ID,
        };
        let settings = crate::native::NativeSettings {
            enabled: true,
            limits,
        };
        invoke_native(
            &application,
            &scope,
            &request.tool_call_id,
            invocation,
            native_target,
            settings,
        )
    });
    let outcome = match (native_outcome, route_id) {
        (Some(outcome), _) => outcome,
        (None, Some(route_id)) => crate::delegation::execute(
            &HostApplication {
                context,
                invocation_id: &request.tool_call_id,
                route_id,
                interface_id: MANAGED_SESSION_INTERFACE_ID,
            },
            &scope,
            &request.tool_call_id,
            invocation,
            instance.as_ref(),
        ),
        (None, None) => crate::delegation::Outcome::error(json!({
            "status": "failed",
            "reason": "unknown_session",
            "hint": "Use a session_id returned by harness.delegate from this session.",
        })),
    };
    json_response(&ToolInvocationResponse {
        output: outcome.output.to_string(),
        is_error: outcome.is_error,
        content: Vec::new(),
        full_output: None,
        result: None,
    })
}

/// Native-first dispatch; `None` when the invocation belongs to the external path.
fn invoke_native(
    app: &dyn crate::delegation::ManagedApplication,
    scope: &crate::delegation::InvocationScope,
    invocation_id: &str,
    invocation: &crate::delegation::SimpleInvocation,
    native_target: bool,
    settings: crate::native::NativeSettings,
) -> Option<crate::delegation::Outcome> {
    use crate::delegation::SimpleInvocation as S;
    match invocation {
        S::Delegate(input) if native_target => Some(crate::native::delegate(
            app,
            scope,
            invocation_id,
            &input.prompt,
            &input.native_options(),
            settings,
        )),
        S::Continue(input) => crate::native::continue_child(
            app,
            scope,
            invocation_id,
            &input.session_id,
            &input.prompt,
        ),
        // External cursors only apply to external children.
        S::Result(input) if input.after.is_none() => crate::native::result(app, &input.session_id),
        // External delegates and cursor reads belong to the external path.
        S::Delegate(_) | S::Result(_) => None,
        S::Respond(input) => crate::native::respond(app, &input.session_id),
        S::Interrupt(input) => crate::native::interrupt(app, &input.session_id),
        S::Wait(input) => Some(crate::native::wait(app, input)),
        S::Context(_) => Some(crate::native::context(app, settings)),
    }
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
        let native_disabled = json!({"native": {"enabled": false}});
        assert_eq!(definitions(&native_disabled).len(), 5);
        for definition in definitions(&native_disabled) {
            assert!(operation(&definition.name).is_ok());
        }
        assert!(operation("harness.auth_continue").is_err());
    }

    #[test]
    fn bcode_children_are_offered_by_default_without_external_agents() {
        let names = definitions(&Value::Null)
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        for tool in crate::delegation::SIMPLE_TOOLS
            .into_iter()
            .chain([crate::native::WAIT_TOOL, crate::native::CONTEXT_TOOL])
        {
            assert!(names.iter().any(|name| name == tool), "{tool} missing");
        }
        let delegate = definitions(&Value::Null)
            .into_iter()
            .find(|definition| definition.name == crate::delegation::DELEGATE_TOOL)
            .unwrap();
        assert!(delegate.input_schema["properties"]["permissions"].is_object());
        assert!(
            delegate
                .description
                .contains("never run `bcode` through the shell")
        );
    }

    fn delegate_input(arguments: Value) -> crate::delegation::DelegateInput {
        serde_json::from_value(arguments).unwrap()
    }

    #[test]
    fn delegation_targets_bcode_unless_an_external_default_is_configured() {
        let native = crate::native::NativeSettings::default();
        let external = json!({"enabled":true, "default_instance":"claude", "instances":{"claude":{
            "enabled":true, "instance":{"instance_id":"claude","plugin_id":"bcode.claude-code",
            "executable":"/explicit/claude","config_namespace":"/isolated",
            "auth":{"provider_id":"claude-code","profile":"native","credential_namespace":"/isolated",
            "custody":"native_managed"}}}}});
        assert!(
            delegates_natively(&Value::Null, native, &delegate_input(json!({"prompt":"t"})))
                .unwrap()
        );
        assert!(
            !delegates_natively(&external, native, &delegate_input(json!({"prompt":"t"}))).unwrap()
        );
        assert!(
            delegates_natively(
                &external,
                native,
                &delegate_input(json!({"prompt":"t","instance_id":"bcode"}))
            )
            .unwrap()
        );
        // Native-only options are refused for external agents rather than ignored.
        assert!(
            delegates_natively(
                &external,
                native,
                &delegate_input(json!({"prompt":"t","permissions":"always_ask"}))
            )
            .is_err()
        );
        let disabled = crate::native::NativeSettings {
            enabled: false,
            ..native
        };
        assert!(
            delegates_natively(
                &Value::Null,
                disabled,
                &delegate_input(json!({"prompt":"t","instance_id":"bcode"}))
            )
            .is_err()
        );
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
        disabled["native"] = json!({"enabled": false});
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
