use bcode::{
    Agent, BcodeError, ModelProviderInvoker, PermissionDecision, PermissionPolicy, RuntimeFuture,
    RuntimePermissionRequest, StopReason, ToolCall, ToolDefinition, ToolInvocationDescriptor,
    ToolInvocationResponse, ToolPolicyMetadata, ToolSideEffect, ToolUiMetadata,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse,
};

fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "echo".to_string(),
        description: "Echo text".to_string(),
        input_schema: serde_json::json!({"type":"object"}),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn echo(request: ToolInvocationDescriptor) -> Result<ToolInvocationResponse, String> {
    Ok(ToolInvocationResponse {
        output: request.arguments.to_string(),
        is_error: false,
        content: Vec::new(),
        full_output: None,
        result: None,
    })
}

#[derive(Debug, Default)]
struct ToolThenTextProvider {
    round: u32,
    events: Vec<ProviderTurnEvent>,
    repeat_tools: bool,
}

impl ToolThenTextProvider {
    fn repeating() -> Self {
        Self {
            repeat_tools: true,
            ..Self::default()
        }
    }
}

impl ModelProviderInvoker for ToolThenTextProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.round += 1;
        let request_tool = self.round == 1 || self.repeat_tools;
        self.events = if request_tool {
            vec![
                ProviderTurnEvent::ToolCallFinished {
                    call: ToolCall {
                        id: format!("call-{}", self.round),
                        name: "echo".to_string(),
                        arguments: serde_json::json!({"round": self.round}),
                    },
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::ToolCall,
                },
            ]
        } else {
            vec![
                ProviderTurnEvent::TextDelta {
                    text: "finished after tool".to_string(),
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
            ]
        };
        Box::pin(async move {
            Ok(StartTurnResponse {
                provider_turn_id: format!("turn-{}", self.round),
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

#[derive(Debug)]
struct Deny;

impl PermissionPolicy for Deny {
    fn evaluate_tool_call<'a>(
        &'a self,
        _request: &'a RuntimePermissionRequest,
    ) -> RuntimeFuture<'a, PermissionDecision> {
        Box::pin(async { Ok(PermissionDecision::Deny("example denial".to_string())) })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> bcode::Result<()> {
    let agent = Agent::builder().inline_tool(definition(), echo).build();
    let response = agent
        .run(&mut ToolThenTextProvider::default(), "run one tool")
        .await?;
    println!("{} ({} steps)", response.text, response.steps.len());

    let denied = Agent::builder()
        .inline_tool(definition(), echo)
        .custom_permission_policy(Deny)
        .build()
        .run(&mut ToolThenTextProvider::default(), "deny the tool")
        .await?;
    assert!(denied.steps.iter().any(|step| matches!(
        step,
        bcode::GenerationStep::ToolResult { result, .. }
            if result.is_error && result.output.contains("example denial")
    )));

    let limited = Agent::builder()
        .max_tool_rounds(1)
        .inline_tool(definition(), echo)
        .build()
        .run(&mut ToolThenTextProvider::repeating(), "hit the limit")
        .await;
    assert!(matches!(
        limited,
        Err(BcodeError::Runtime(bcode::RuntimeError::MaxToolRounds(1)))
    ));

    Ok(())
}
