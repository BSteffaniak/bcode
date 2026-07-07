use bcode::{Agent, ToolCall};
use bcode_tool::{
    ToolDefinition, ToolInvocationRequest, ToolInvocationResponse, ToolPolicyMetadata,
    ToolSideEffect, ToolUiMetadata,
};

fn uppercase_definition() -> ToolDefinition {
    ToolDefinition {
        name: "uppercase".to_string(),
        description: "Uppercase text".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string" }
            },
            "required": ["text"]
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn uppercase(request: ToolInvocationRequest) -> Result<ToolInvocationResponse, String> {
    let text = request
        .arguments
        .get("text")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "text argument is required".to_string())?;
    Ok(ToolInvocationResponse {
        output: text.to_uppercase(),
        is_error: false,
        content: Vec::new(),
        full_output: None,
        host_action: None,
        result: None,
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> bcode::Result<()> {
    let agent = Agent::builder()
        .name("tools-example")
        .inline_tool(uppercase_definition(), uppercase)
        .build();

    let output = agent
        .execute_tool_call(&ToolCall {
            id: "call-1".to_string(),
            name: "uppercase".to_string(),
            arguments: serde_json::json!({ "text": "hello tools" }),
        })
        .await?;

    println!("{}", output.model_result.output);
    Ok(())
}
