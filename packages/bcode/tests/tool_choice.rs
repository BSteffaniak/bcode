use bcode::{ModelProviderInvoker, RuntimeFuture, StopReason, ToolChoice, generate_text_builder};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse,
};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct PolicyProvider {
    policy: Arc<Mutex<Option<bcode_model::ToolCallRequestPolicy>>>,
}

impl ModelProviderInvoker for PolicyProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        *self.policy.lock().expect("policy lock should be available") =
            Some(request.tool_call_policy.clone());
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "policy-turn".to_string(),
            })
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        Box::pin(async {
            Ok(PollTurnEventsResponse {
                events: vec![ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                }],
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

#[tokio::test]
async fn generation_builder_forwards_typed_tool_choice() {
    let policy = Arc::new(Mutex::new(None));
    let mut provider = PolicyProvider {
        policy: Arc::clone(&policy),
    };

    generate_text_builder()
        .prompt("use lookup")
        .tool_choice(ToolChoice::Tool {
            name: "lookup".to_string(),
        })
        .run(&mut provider)
        .await
        .expect("request should complete");

    assert_eq!(
        policy
            .lock()
            .expect("policy lock should be available")
            .as_ref()
            .map(|policy| &policy.choice),
        Some(&ToolChoice::Tool {
            name: "lookup".to_string()
        })
    );
}
