use bcode::{Agent, ToolCall, ToolDefinition, ToolExecutionOptions};
use bcode_tool::{ToolInvocationResponse, ToolPolicyMetadata, ToolSideEffect, ToolUiMetadata};
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: format!("test tool {name}"),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn response(output: &str) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output: output.to_string(),
        is_error: false,
        content: Vec::new(),
        full_output: None,
        host_action: None,
        result: None,
    }
}

#[tokio::test]
async fn advanced_batch_api_schedules_once_and_preserves_provider_order() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let first_count = Arc::clone(&invocations);
    let second_count = Arc::clone(&invocations);
    let agent = Agent::builder()
        .max_tool_rounds(1)
        .execution_options(ToolExecutionOptions {
            parallel: true,
            max_concurrency: Some(NonZeroUsize::new(2).expect("two is non-zero")),
            preparation_timeout_ms: NonZeroU64::new(1_000).expect("one thousand is non-zero"),
        })
        .inline_tool(definition("first"), move |_| {
            first_count.fetch_add(1, Ordering::SeqCst);
            Ok(response("first-result"))
        })
        .inline_tool(definition("second"), move |_| {
            second_count.fetch_add(1, Ordering::SeqCst);
            Ok(response("second-result"))
        })
        .build();
    let calls = [
        ToolCall {
            id: "call-first".to_string(),
            name: "first".to_string(),
            arguments: serde_json::Value::Null,
        },
        ToolCall {
            id: "call-second".to_string(),
            name: "second".to_string(),
            arguments: serde_json::Value::Null,
        },
    ];
    let mut rounds = agent.tool_round_state();

    let output = agent
        .execute_tool_batch_with_round_state(&calls, &mut rounds)
        .await
        .expect("batch should execute");

    assert_eq!(invocations.load(Ordering::SeqCst), 2);
    assert_eq!(output.results.len(), 2);
    assert_eq!(
        output.results[0]
            .as_ref()
            .expect("first call succeeds")
            .model_result
            .call_id,
        "call-first"
    );
    assert_eq!(
        output.results[1]
            .as_ref()
            .expect("second call succeeds")
            .model_result
            .call_id,
        "call-second"
    );
    assert!(
        agent
            .execute_tool_batch_with_round_state(&calls, &mut rounds)
            .await
            .is_err(),
        "the complete provider batch must consume exactly one round"
    );
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        2,
        "round rejection must not invoke tools again"
    );
}
