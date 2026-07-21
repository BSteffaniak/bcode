use bcode::{
    Agent, ToolCall, ToolDefinition, ToolFailurePolicy, ToolInvocationResponse, ToolPolicyMetadata,
    ToolSideEffect, ToolUiMetadata,
};

fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "fails".to_string(),
        description: "Always fails".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn call() -> ToolCall {
    ToolCall {
        id: "call-1".to_string(),
        name: "fails".to_string(),
        arguments: serde_json::json!({}),
    }
}

fn failing_handler(
    _request: bcode::ToolInvocationDescriptor,
) -> Result<ToolInvocationResponse, String> {
    Err("recoverable failure".to_string())
}

#[tokio::test]
async fn default_tool_failure_policy_fails_turn() {
    let error = Agent::builder()
        .inline_tool(definition(), failing_handler)
        .build()
        .execute_tool_call(&call())
        .await
        .expect_err("default policy should fail");

    assert!(error.to_string().contains("recoverable failure"));
}

#[tokio::test]
async fn return_to_model_policy_emits_error_result() {
    let output = Agent::builder()
        .tool_failure_policy(ToolFailurePolicy::ReturnToModel)
        .inline_tool(definition(), failing_handler)
        .build()
        .execute_tool_call(&call())
        .await
        .expect("error should become model-visible result");

    assert!(output.invocation.is_error);
    assert!(output.model_result.is_error);
    assert_eq!(output.model_result.output, "recoverable failure");
}
