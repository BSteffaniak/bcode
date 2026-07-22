use bcode::{
    Agent, ScopedTurnEvent, ToolApplicationError, ToolCall, ToolInvocationLifecycleStage,
    ToolInvocationResult, TurnEventSink, TypedTool,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

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

    assert!(
        error
            .to_string()
            .contains("typed tool input failed schema validation")
    );
}

#[derive(Debug, Serialize)]
struct ToolProgress {
    completed: u8,
}

#[derive(Debug, Serialize)]
struct AddErrorDetails {
    left: i64,
    right: i64,
}

#[derive(Debug, Default)]
struct CapturingSink(Mutex<Vec<ScopedTurnEvent>>);

impl TurnEventSink for CapturingSink {
    fn emit(&self, event: ScopedTurnEvent) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
        true
    }
}

#[tokio::test]
async fn async_typed_tool_receives_state_cancellation_context_and_reports_progress() {
    let events = Arc::new(CapturingSink::default());
    let state = Arc::new(40_i64);
    let agent = Agent::builder()
        .invocation_event_sink(events.clone())
        .typed_tool_with_state(
            TypedTool::<AddInput, AddOutput>::new("add", "Add two integers"),
            state,
            |input, context| async move {
                assert_eq!(context.invocation_id(), "call-context");
                assert!(!context.is_cancelled());
                assert!(!context.cancellation().is_cancelled());
                assert!(
                    context
                        .report_progress("halfway", ToolProgress { completed: 50 })
                        .expect("progress should serialize")
                );
                Ok::<_, ToolApplicationError<serde_json::Value>>(AddOutput {
                    sum: input.left + input.right + *context.state(),
                })
            },
        )
        .build();

    let output = agent
        .execute_tool_call(&ToolCall {
            id: "call-context".to_string(),
            name: "add".to_string(),
            arguments: serde_json::json!({"left": 1, "right": 1}),
        })
        .await
        .expect("async typed tool should execute");

    assert_eq!(output.model_result.output, r#"{"sum":42}"#);
    let events = events
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(events.iter().any(|event| matches!(
        event,
        ScopedTurnEvent::InvocationLifecycle(event)
            if event.stage == ToolInvocationLifecycleStage::Progress
                && event.message.as_deref() == Some("halfway")
                && event.metadata == serde_json::json!({"completed": 50})
    )));
}

#[tokio::test]
async fn async_typed_tool_observes_external_cancellation() {
    let (token_sender, token_receiver) = tokio::sync::oneshot::channel();
    let state = Arc::new(Mutex::new(Some(token_sender)));
    let agent = Arc::new(
        Agent::builder()
            .typed_tool_with_state(
                TypedTool::<AddInput, AddOutput>::new("add", "Add two integers"),
                state,
                |_input, context| async move {
                    let cancellation = context.cancellation();
                    if let Some(sender) = context
                        .state()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take()
                    {
                        let _ = sender.send(cancellation.clone());
                    }
                    cancellation.cancelled().await;
                    Ok::<_, ToolApplicationError<serde_json::Value>>(AddOutput { sum: 0 })
                },
            )
            .build(),
    );
    let task = {
        let agent = Arc::clone(&agent);
        tokio::spawn(async move {
            agent
                .execute_tool_call(&ToolCall {
                    id: "call-cancel".to_string(),
                    name: "add".to_string(),
                    arguments: serde_json::json!({"left": 1, "right": 1}),
                })
                .await
        })
    };
    let cancellation = token_receiver.await.expect("handler cancellation token");
    cancellation.cancel();

    let error = task
        .await
        .expect("tool task should join")
        .expect_err("cancelled tool must not return normal output");
    assert!(matches!(
        error,
        bcode::BcodeError::Runtime(bcode::RuntimeError::Cancelled)
    ));
}

#[tokio::test]
async fn typed_application_error_separates_model_and_application_details() {
    let agent = Agent::builder()
        .typed_tool_async(
            TypedTool::<AddInput, AddOutput>::new("add", "Add two integers"),
            |input, _context| async move {
                Err(ToolApplicationError::new(
                    "addition_refused",
                    "internal detail: account secret 123",
                    "addition is unavailable",
                    AddErrorDetails {
                        left: input.left,
                        right: input.right,
                    },
                )
                .retryable(true))
            },
        )
        .build();

    let output = agent
        .execute_tool_call(&ToolCall {
            id: "call-error".to_string(),
            name: "add".to_string(),
            arguments: serde_json::json!({"left": 20, "right": 22}),
        })
        .await
        .expect("application failure should remain a typed tool result");

    assert!(output.invocation.is_error);
    assert_eq!(output.model_result.output, "addition is unavailable");
    assert!(!output.model_result.output.contains("secret"));
    let ToolInvocationResult::Json { value } = output
        .invocation
        .result
        .expect("structured application error")
    else {
        panic!("typed application error should be JSON");
    };
    let value: serde_json::Value = serde_json::from_str(&value).expect("valid error JSON");
    assert_eq!(value["status"], "error");
    assert_eq!(value["code"], "addition_refused");
    assert_eq!(
        value["details"],
        serde_json::json!({"left": 20, "right": 22})
    );
    assert_eq!(value["retryable"], true);
    assert!(
        value["message"]
            .as_str()
            .is_some_and(|message| message.contains("secret"))
    );
}
