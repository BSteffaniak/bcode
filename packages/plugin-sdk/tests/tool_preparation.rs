use bcode_plugin_sdk::{ServiceRequest, prepare_tool_from_definitions};
use bcode_tool::{
    OP_PREPARE_TOOL, TOOL_POLICY_AUTHORIZATION_ACTION_INVOKE, TOOL_POLICY_AUTHORIZATION_NAMESPACE,
    TOOL_POLICY_AUTHORIZATION_SCHEMA_VERSION, TOOL_SERVICE_INTERFACE_ID, ToolDefinition,
    ToolInvocationDescriptor, ToolPolicyAuthorizationMetadata, ToolPolicyMetadata,
    ToolPreparationRequest, ToolSideEffect, ToolUiMetadata,
};

fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "owner.tool".to_string(),
        description: "test tool".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: ToolSideEffect::WriteFiles,
        requires_permission: true,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

#[test]
fn preparation_dispatches_to_owner_definition_without_transport_fields() {
    let preparation = ToolPreparationRequest {
        invocation: ToolInvocationDescriptor {
            invocation_id: "call-1".to_string(),
            tool_name: "owner.tool".to_string(),
            arguments: serde_json::json!({"path": "file"}),
        },
        host_context: Vec::new(),
    };
    let service = ServiceRequest {
        interface_id: TOOL_SERVICE_INTERFACE_ID.to_string(),
        operation: OP_PREPARE_TOOL.to_string(),
        payload: serde_json::to_vec(&preparation).expect("request should encode"),
    };

    let response = prepare_tool_from_definitions(&service, [definition()])
        .expect("owner preparation should succeed");

    assert_eq!(response.authorization.len(), 1);
    let fact = &response.authorization[0];
    assert_eq!(fact.namespace, TOOL_POLICY_AUTHORIZATION_NAMESPACE);
    assert_eq!(
        fact.schema_version,
        TOOL_POLICY_AUTHORIZATION_SCHEMA_VERSION
    );
    assert_eq!(fact.action, TOOL_POLICY_AUTHORIZATION_ACTION_INVOKE);
    assert_eq!(fact.resource.as_deref(), Some("owner.tool"));
    let metadata: ToolPolicyAuthorizationMetadata =
        serde_json::from_value(fact.metadata.clone()).expect("fact should decode");
    assert_eq!(metadata.side_effect, ToolSideEffect::WriteFiles);
    assert!(metadata.requires_permission);
    assert_eq!(metadata.arguments, serde_json::json!({"path": "file"}));
}

#[test]
fn preparation_rejects_unknown_tool() {
    let preparation = ToolPreparationRequest {
        invocation: ToolInvocationDescriptor {
            invocation_id: "call-1".to_string(),
            tool_name: "unknown.tool".to_string(),
            arguments: serde_json::Value::Null,
        },
        host_context: Vec::new(),
    };
    let service = ServiceRequest {
        interface_id: TOOL_SERVICE_INTERFACE_ID.to_string(),
        operation: OP_PREPARE_TOOL.to_string(),
        payload: serde_json::to_vec(&preparation).expect("request should encode"),
    };

    let error = prepare_tool_from_definitions(&service, [definition()])
        .expect_err("unknown tool must fail preparation");

    assert!(error.contains("tool not found"));
}
