use bcode::{
    Agent, AgentEvent, BcodeError, ModelCallContext, ModelProviderInvoker, PermissionDecision,
    RuntimeFuture, ToolCall, ToolCallContext,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use bcode_tool::{
    ToolDefinition, ToolInvocationRequest, ToolInvocationResponse, ToolPolicyMetadata,
    ToolSideEffect, ToolUiMetadata,
};
use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct ExampleProvider {
    events: VecDeque<ProviderTurnEvent>,
}

impl ExampleProvider {
    fn new() -> Self {
        Self {
            events: VecDeque::from([
                ProviderTurnEvent::TextDelta {
                    text: "observed".to_string(),
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
            ]),
        }
    }
}

impl ModelProviderInvoker for ExampleProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "observed-turn".to_string(),
            })
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        Box::pin(async move {
            Ok(PollTurnEventsResponse {
                events: self.events.pop_front().into_iter().collect(),
            })
        })
    }

    fn cancel_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a CancelTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        Box::pin(async { Ok(AckResponse::default()) })
    }

    fn finish_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a FinishTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        Box::pin(async { Ok(AckResponse::default()) })
    }
}

fn echo_definition() -> ToolDefinition {
    ToolDefinition {
        name: "echo".to_string(),
        description: "Echo text".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: true,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn echo(request: ToolInvocationRequest) -> Result<ToolInvocationResponse, String> {
    let text = request
        .arguments
        .get("text")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "text argument is required".to_string())?;
    Ok(ToolInvocationResponse {
        output: text.to_string(),
        is_error: false,
        content: Vec::new(),
        full_output: None,
        host_action: None,
        result: None,
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> bcode::Result<()> {
    let model_hooks = Arc::new(AtomicUsize::new(0));
    let tool_hooks = Arc::new(AtomicUsize::new(0));
    let before_model = Arc::clone(&model_hooks);
    let after_model = Arc::clone(&model_hooks);
    let before_tool = Arc::clone(&tool_hooks);
    let after_tool = Arc::clone(&tool_hooks);

    let agent = Agent::builder()
        .name("observability-example")
        .model("example-model")
        .on_before_model(move |context: &ModelCallContext| {
            before_model.fetch_add(1, Ordering::Relaxed);
            if context.prompt.len() > 10_000 {
                return Err(BcodeError::Hook("prompt budget exceeded".to_string()));
            }
            println!("model start: {}", context.model_id);
            Ok(())
        })
        .on_after_model(move |_context, outcome| {
            after_model.fetch_add(1, Ordering::Relaxed);
            println!(
                "model done: latency={}ms text={}",
                outcome.response.runtime.latency_ms, outcome.response.text
            );
            Ok(())
        })
        .on_before_tool(move |context: &ToolCallContext| {
            before_tool.fetch_add(1, Ordering::Relaxed);
            println!("tool start: {}", context.call.name);
            Ok(())
        })
        .on_after_tool(move |_context, outcome| {
            after_tool.fetch_add(1, Ordering::Relaxed);
            println!("tool done: {}", outcome.output.model_result.output);
            Ok(())
        })
        .permission_callback(|call| {
            if call.name == "echo" {
                PermissionDecision::Allow
            } else {
                PermissionDecision::Deny("only echo is allowed".to_string())
            }
        })
        .inline_tool(echo_definition(), echo)
        .build();

    let mut provider = ExampleProvider::new();
    let response = agent
        .generate_text_with_provider(&mut provider, "observe this")
        .await?;
    let provider_metadata_count = response
        .runtime
        .events
        .iter()
        .filter(|event| matches!(event, AgentEvent::ProviderMetadata { .. }))
        .count();
    println!("provider metadata events: {provider_metadata_count}");

    let output = agent
        .execute_tool_call(&ToolCall {
            id: "call-1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({ "text": "instrumented" }),
        })
        .await?;
    println!("tool events: {}", output.events.len());
    println!(
        "hook counts: model={} tool={}",
        model_hooks.load(Ordering::Relaxed),
        tool_hooks.load(Ordering::Relaxed)
    );

    Ok(())
}
