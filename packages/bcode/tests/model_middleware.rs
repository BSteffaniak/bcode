use bcode::{
    AgentTurnRequest, BcodeError, GenerateTextResponse, ModelMiddleware, ModelProviderInvoker,
    ProviderTurnEvent, RuntimeFuture, StopReason, generate_text_builder,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, StartTurnResponse,
};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct RecordingProvider {
    prompt: Arc<Mutex<Option<String>>>,
}

impl ModelProviderInvoker for RecordingProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        let prompt = request
            .messages
            .last()
            .and_then(|message| message.content.first())
            .and_then(|content| match content {
                bcode::ModelContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            });
        *self.prompt.lock().expect("prompt lock should be available") = prompt;
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "middleware-turn".to_string(),
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
                events: vec![
                    ProviderTurnEvent::TextDelta {
                        text: "provider response".to_string(),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
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
struct TransformMiddleware;

impl ModelMiddleware for TransformMiddleware {
    fn before_request(&self, mut request: AgentTurnRequest) -> bcode::Result<AgentTurnRequest> {
        request.prompt = format!("redacted: {}", request.prompt);
        Ok(request)
    }

    fn after_response(
        &self,
        _request: &AgentTurnRequest,
        mut response: GenerateTextResponse,
    ) -> bcode::Result<GenerateTextResponse> {
        response.text = response.text.to_uppercase();
        response.runtime.text.clone_from(&response.text);
        Ok(response)
    }
}

#[derive(Debug)]
struct RejectMiddleware;

impl ModelMiddleware for RejectMiddleware {
    fn before_request(&self, _request: AgentTurnRequest) -> bcode::Result<AgentTurnRequest> {
        Err(BcodeError::Hook("request budget exhausted".to_string()))
    }
}

#[tokio::test]
async fn middleware_transforms_complete_requests_and_responses() {
    let prompt = Arc::new(Mutex::new(None));
    let mut provider = RecordingProvider {
        prompt: Arc::clone(&prompt),
    };

    let response = generate_text_builder()
        .prompt("secret")
        .middleware_layer(TransformMiddleware)
        .run(&mut provider)
        .await
        .expect("middleware request should succeed");

    assert_eq!(
        prompt
            .lock()
            .expect("prompt lock should be available")
            .as_deref(),
        Some("redacted: secret")
    );
    assert_eq!(response.text, "PROVIDER RESPONSE");
    assert_eq!(response.runtime.text, response.text);
}

#[tokio::test]
async fn middleware_can_reject_before_provider_invocation() {
    let prompt = Arc::new(Mutex::new(None));
    let mut provider = RecordingProvider {
        prompt: Arc::clone(&prompt),
    };

    let error = generate_text_builder()
        .prompt("too expensive")
        .middleware_layer(RejectMiddleware)
        .run(&mut provider)
        .await
        .expect_err("middleware should reject the request");

    assert!(error.to_string().contains("budget exhausted"));
    assert!(
        prompt
            .lock()
            .expect("prompt lock should be available")
            .is_none()
    );
}
