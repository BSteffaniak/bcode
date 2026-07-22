use bcode::{
    Agent, AgentLoopStopContext, AgentLoopTerminationReason, BcodeError, ModelProviderInvoker,
    RuntimeError, RuntimeFuture, ToolCall, ToolDefinition, ToolInvocationResponse,
    ToolPolicyMetadata, ToolSideEffect, ToolUiMetadata,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Debug)]
struct RepeatingProvider {
    malformed: bool,
    round: usize,
    events: Vec<ProviderTurnEvent>,
}

impl RepeatingProvider {
    fn new(malformed: bool) -> Self {
        Self {
            malformed,
            round: 0,
            events: Vec::new(),
        }
    }
}

impl ModelProviderInvoker for RepeatingProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.round += 1;
        self.events = vec![
            ProviderTurnEvent::ToolCallFinished {
                call: ToolCall {
                    id: format!("call-{}", self.round),
                    name: if self.malformed {
                        String::new()
                    } else {
                        "echo".to_string()
                    },
                    arguments: serde_json::json!({"value": "same"}),
                },
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::ToolCall,
            },
        ];
        let provider_turn_id = format!("round-{}", self.round);
        Box::pin(async move { Ok(StartTurnResponse { provider_turn_id }) })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        Box::pin(async move {
            Ok(PollTurnEventsResponse {
                events: std::mem::take(&mut self.events),
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
        description: "Echo input".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn response() -> ToolInvocationResponse {
    ToolInvocationResponse {
        output: "same".to_string(),
        is_error: false,
        content: Vec::new(),
        full_output: None,
        host_action: None,
        result: None,
    }
}

#[tokio::test]
async fn application_stop_condition_stops_before_tool_execution() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&invocations);
    let agent = Agent::builder()
        .stop_when(|context: AgentLoopStopContext<'_>| {
            context.provider_round == 0 && !context.tool_calls.is_empty()
        })
        .inline_tool(echo_definition(), move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(response())
        })
        .build();
    let mut provider = RepeatingProvider::new(false);

    let result = agent
        .generate_text_with_provider(&mut provider, "stop after planning")
        .await
        .expect("stop condition should finish successfully");

    assert_eq!(
        result.runtime.termination_reason,
        AgentLoopTerminationReason::StopCondition
    );
    assert_eq!(invocations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn malformed_provider_tool_call_is_rejected_before_invocation() {
    let agent = Agent::builder()
        .inline_tool(echo_definition(), |_| Ok(response()))
        .build();
    let mut provider = RepeatingProvider::new(true);

    let error = agent
        .generate_text_with_provider(&mut provider, "malformed")
        .await
        .expect_err("empty tool name must fail");

    assert!(matches!(
        error,
        BcodeError::Runtime(RuntimeError::MalformedProviderToolCall { index: 0, .. })
    ));
}

#[tokio::test]
async fn repeated_semantic_tool_batch_is_bounded_even_when_provider_changes_ids() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&invocations);
    let agent = Agent::builder()
        .max_tool_rounds(8)
        .max_repeated_tool_batches(1)
        .inline_tool(echo_definition(), move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(response())
        })
        .build();
    let mut provider = RepeatingProvider::new(false);

    let error = agent
        .generate_text_with_provider(&mut provider, "repeat")
        .await
        .expect_err("second identical semantic batch must fail");

    assert!(matches!(
        error,
        BcodeError::Runtime(RuntimeError::RepeatedToolCallBatch {
            repeats: 2,
            limit: 1
        })
    ));
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
}
