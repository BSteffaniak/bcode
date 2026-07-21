use bcode::{Agent, ToolCall, ToolInvocationResult, TypedTool};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    left: i64,
    right: i64,
}

#[derive(Debug, Serialize)]
struct AddOutput {
    sum: i64,
}

#[tokio::test]
async fn typed_tool_derives_schema_decodes_input_and_serializes_output() {
    let tool = TypedTool::<AddInput, AddOutput>::new("add", "Add two integers");
    assert_eq!(tool.definition().name, "add");
    assert_eq!(tool.definition().input_schema["type"], "object");
    assert!(tool.definition().input_schema["properties"]["left"].is_object());

    let agent = Agent::builder()
        .typed_tool(tool, |input| {
            Ok(AddOutput {
                sum: input.left + input.right,
            })
        })
        .build();

    let output = agent
        .execute_tool_call(&ToolCall {
            id: "call-1".to_string(),
            name: "add".to_string(),
            arguments: serde_json::json!({"left": 20, "right": 22}),
        })
        .await
        .expect("typed tool should execute");

    assert_eq!(output.model_result.output, r#"{"sum":42}"#);
    assert_eq!(
        output.invocation.result,
        Some(ToolInvocationResult::Json {
            value: r#"{"sum":42}"#.to_string()
        })
    );
}

#[tokio::test]
async fn typed_tool_reports_argument_decode_failures() {
    let agent = Agent::builder()
        .typed_tool(
            TypedTool::<AddInput, AddOutput>::new("add", "Add two integers"),
            |input| {
                Ok(AddOutput {
                    sum: input.left + input.right,
                })
            },
        )
        .build();

    let error = agent
        .execute_tool_call(&ToolCall {
            id: "call-2".to_string(),
            name: "add".to_string(),
            arguments: serde_json::json!({"left": "not-an-integer", "right": 22}),
        })
        .await
        .expect_err("invalid typed input should fail");

    assert!(error.to_string().contains("invalid typed tool arguments"));
}
