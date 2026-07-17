use bcode_agent_permissions::runtime_permission_request_to_profile_request;
use bcode_agent_profile::prepare_tool_policy;
use bcode_agent_runtime::{RegisteredTool, RuntimePermissionContext, RuntimePermissionRequest};
use bcode_model::ToolCall;
use bcode_tool::{
    ToolDefinition, ToolInvocationDescriptor, ToolPolicyMetadata, ToolSideEffect, ToolUiMetadata,
};

fn definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: "test tool".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: ToolSideEffect::WriteFiles,
        requires_permission: true,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn permission_request() -> RuntimePermissionRequest {
    let definition = definition("owner.tool");
    let arguments = serde_json::json!({"path": "owned"});
    let preparation = prepare_tool_policy(
        &bcode_tool::ToolPreparationRequest {
            invocation: ToolInvocationDescriptor {
                invocation_id: "call-1".to_string(),
                tool_name: definition.name.clone(),
                arguments: arguments.clone(),
            },
            host_context: Vec::new(),
        },
        &definition,
    )
    .expect("tool owner preparation should succeed");
    RuntimePermissionRequest {
        context: RuntimePermissionContext::default(),
        call: ToolCall {
            id: "call-1".to_string(),
            name: definition.name.clone(),
            arguments,
        },
        tool: RegisteredTool::inline(definition),
        facts: preparation.authorization,
    }
}

#[test]
fn policy_adapter_consumes_owner_produced_fact_metadata() {
    let mut request = permission_request();
    request.call.arguments = serde_json::json!({"path": "tampered-call"});
    request.tool.definition.side_effect = ToolSideEffect::ReadOnly;

    let profile =
        runtime_permission_request_to_profile_request(&request, std::path::Path::new("."))
            .expect("owner fact should be accepted");

    assert_eq!(profile.tool_name, "owner.tool");
    assert_eq!(
        profile.operation,
        bcode_agent_profile::ToolPolicyOperation::Mutating
    );
    assert!(profile.requires_permission);
}

#[test]
fn policy_adapter_rejects_missing_owner_fact() {
    let mut request = permission_request();
    request.facts.clear();

    let error = runtime_permission_request_to_profile_request(&request, std::path::Path::new("."))
        .expect_err("missing owner fact must fail closed");

    assert!(error.to_string().contains("omitted"));
}

#[test]
fn policy_adapter_rejects_mismatched_owner_resource() {
    let mut request = permission_request();
    request.facts[0].resource = Some("another.tool".to_string());

    let error = runtime_permission_request_to_profile_request(&request, std::path::Path::new("."))
        .expect_err("mismatched resource must fail closed");

    assert!(error.to_string().contains("does not match"));
}
