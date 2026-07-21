use bcode::{
    Agent, AgentTurnRequest, BcodeError, GenerateTextResponse, ModelMiddleware,
    ModelProviderInvoker, ProviderTurnEvent, RuntimeFuture, StopReason, TextStreamItem,
    generate_text_builder, stream_object_builder, stream_text_builder,
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

struct RejectResponseMiddleware;

impl ModelMiddleware for RejectResponseMiddleware {
    fn after_response(
        &self,
        _request: &AgentTurnRequest,
        _response: GenerateTextResponse,
    ) -> bcode::Result<GenerateTextResponse> {
        Err(BcodeError::Hook("response audit failed".to_string()))
    }
}

#[tokio::test]
async fn streaming_middleware_transforms_request_and_terminal_response_only() {
    let prompt = Arc::new(Mutex::new(None));
    let provider = RecordingProvider {
        prompt: Arc::clone(&prompt),
    };
    let mut stream = stream_text_builder()
        .prompt("secret")
        .middleware_layer(TransformMiddleware)
        .run(provider);
    let mut delta = None;
    let mut finished = None;

    while let Some(item) = stream.next().await {
        match item {
            TextStreamItem::Event(bcode::AgentEvent::TextDelta(text)) => delta = Some(text),
            TextStreamItem::Finished(response) => finished = Some(response),
            TextStreamItem::Error(error) => panic!("stream failed: {error}"),
            TextStreamItem::Event(_) | TextStreamItem::ScopedEvent(_) => {}
        }
    }

    assert_eq!(
        prompt
            .lock()
            .expect("prompt lock should be available")
            .as_deref(),
        Some("redacted: secret")
    );
    assert_eq!(delta.as_deref(), Some("provider response"));
    let finished = finished.expect("terminal response");
    assert_eq!(finished.text, "PROVIDER RESPONSE");
    assert_eq!(finished.runtime.text, finished.text);
    assert!(matches!(
        finished.steps.last(),
        Some(bcode::GenerationStep::FinalResponse { text, .. }) if text == "PROVIDER RESPONSE"
    ));
}

#[tokio::test]
async fn streaming_request_middleware_rejection_never_starts_provider() {
    let prompt = Arc::new(Mutex::new(None));
    let provider = RecordingProvider {
        prompt: Arc::clone(&prompt),
    };
    let mut stream = stream_text_builder()
        .prompt("too expensive")
        .middleware_layer(RejectMiddleware)
        .run(provider);

    assert!(matches!(
        stream.next().await,
        Some(TextStreamItem::Error(BcodeError::Hook(message))) if message == "request budget exhausted"
    ));
    assert!(stream.next().await.is_none());
    assert!(prompt.lock().expect("prompt lock").is_none());
}

#[tokio::test]
async fn streaming_response_middleware_failure_is_typed_and_terminal() {
    let prompt = Arc::new(Mutex::new(None));
    let provider = RecordingProvider { prompt };
    let mut stream = stream_text_builder()
        .prompt("audit")
        .middleware_layer(RejectResponseMiddleware)
        .run(provider);
    let mut saw_delta = false;
    let mut terminal = None;

    while let Some(item) = stream.next().await {
        match item {
            TextStreamItem::Event(bcode::AgentEvent::TextDelta(_)) => saw_delta = true,
            TextStreamItem::Error(error) => terminal = Some(error),
            TextStreamItem::Finished(_) => panic!("rejected response must not finish"),
            TextStreamItem::Event(_) | TextStreamItem::ScopedEvent(_) => {}
        }
    }

    assert!(saw_delta);
    assert!(
        matches!(terminal, Some(BcodeError::Hook(message)) if message == "response audit failed")
    );
}

#[tokio::test]
async fn structured_stream_applies_response_middleware_before_final_decode() {
    #[derive(Debug, serde::Deserialize, schemars::JsonSchema, PartialEq, Eq)]
    struct Output {
        value: String,
    }

    struct StructuredMiddleware;
    impl ModelMiddleware for StructuredMiddleware {
        fn after_response(
            &self,
            _request: &AgentTurnRequest,
            mut response: GenerateTextResponse,
        ) -> bcode::Result<GenerateTextResponse> {
            response.text = r#"{"value":"middleware"}"#.to_string();
            Ok(response)
        }
    }

    let prompt = Arc::new(Mutex::new(None));
    let provider = RecordingProvider { prompt };
    let mut stream = stream_object_builder::<Output>()
        .prompt("structured")
        .middleware_layer(StructuredMiddleware)
        .run(provider);
    let mut finished = None;

    while let Some(item) = stream.next().await {
        match item {
            bcode::ObjectStreamItem::Finished { object, .. } => finished = Some(object),
            bcode::ObjectStreamItem::Error(error) => panic!("stream failed: {error}"),
            _ => {}
        }
    }

    assert_eq!(
        finished,
        Some(Output {
            value: "middleware".to_string()
        })
    );
}

#[tokio::test]
async fn streaming_runs_model_hooks_once_around_provider_lifecycle() {
    let prompt = Arc::new(Mutex::new(None));
    let before = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let after = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let before_hook = Arc::clone(&before);
    let after_hook = Arc::clone(&after);
    let agent = Agent::builder()
        .on_before_model(move |_| {
            before_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .on_after_model(move |_, _| {
            after_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .build();
    let mut stream = agent.stream_text_with_provider(RecordingProvider { prompt }, "observe");

    while stream.next().await.is_some() {}

    assert_eq!(before.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(after.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn scoped_agent_stream_applies_response_middleware() {
    let prompt = Arc::new(Mutex::new(None));
    let provider = RecordingProvider { prompt };
    let agent = Agent::builder()
        .middleware_layer(TransformMiddleware)
        .build();
    let mut stream = agent.stream(provider, "secret");
    let mut finished = None;

    while let Some(item) = stream.next().await {
        match item {
            bcode::ScopedAgentStreamItem::Finished(response) => finished = Some(response),
            bcode::ScopedAgentStreamItem::Error(error) => panic!("stream failed: {error}"),
            bcode::ScopedAgentStreamItem::Event(_) => {}
        }
    }

    assert_eq!(
        finished.expect("terminal response").text,
        "PROVIDER RESPONSE"
    );
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
    assert!(matches!(
        response.steps.last(),
        Some(bcode::GenerationStep::FinalResponse { text, .. }) if text == "PROVIDER RESPONSE"
    ));
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
