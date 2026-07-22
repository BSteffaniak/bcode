use bcode::{
    Action, Agent, AgentConfig, AgentEvent, BcodeError, ModelCallContext, ModelProviderInvoker,
    PermissionDecision, RuntimeFuture, TextStreamItem, ToolCall, ToolCallContext,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use bcode_tool::{
    ToolDefinition, ToolInvocationDescriptor, ToolInvocationResponse, ToolPolicyMetadata,
    ToolSideEffect, ToolUiMetadata,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct ExampleProvider {
    events: VecDeque<ProviderTurnEvent>,
}

impl ExampleProvider {
    fn new() -> Self {
        Self::text("observed")
    }

    fn text(text: impl Into<String>) -> Self {
        Self {
            events: VecDeque::from([
                ProviderTurnEvent::TextDelta { text: text.into() },
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

fn echo(request: ToolInvocationDescriptor) -> Result<ToolInvocationResponse, String> {
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

    let mut agent_config = AgentConfig::default();
    agent_config.tools.insert("echo".to_string(), true);
    agent_config.permission.read = BTreeMap::from([("*".to_string(), Action::Ask)]);

    let agent = Agent::builder()
        .name("observability-example")
        .agent_id("build")
        .model("example-model")
        .agent_config_with_ask(agent_config, |_request, evaluation| {
            println!(
                "permission requested: {}",
                evaluation.reason.as_deref().unwrap_or("policy asked")
            );
            PermissionDecision::Allow
        })
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

    let mut stream =
        agent.stream_text_with_provider(ExampleProvider::text("streamed trace"), "trace streaming");
    while let Some(item) = stream.next().await {
        match item {
            TextStreamItem::Event(event) => println!("stream event: {event:?}"),
            TextStreamItem::ScopedEvent(event) => println!("scoped stream event: {event:?}"),
            TextStreamItem::Finished(response) => {
                println!("stream finished: {}ms", response.runtime.latency_ms);
                break;
            }
            TextStreamItem::Error(error) => return Err(error),
        }
    }

    let structured: serde_json::Value = agent
        .generate_object_with_provider(
            &mut ExampleProvider::text(r#"{"status":"traced"}"#),
            "trace structured output",
        )
        .await?;
    println!("structured trace: {structured}");

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
