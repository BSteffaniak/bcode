use bcode::{
    AgentEvent, BcodeError, CancellationToken, ModelProviderInvoker, RuntimeError, RuntimeFuture,
    StopReason, TextStreamItem, generate_text_builder, stream_text_builder,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, ExactRequestInputTokens, FinishTurnRequest, ModelTurnRequest,
    PollTurnEventsRequest, PollTurnEventsResponse, ProviderError, ProviderErrorCategory,
    ProviderErrorSource, ProviderRequestProjection, ProviderRetryHint, ProviderTurnEvent,
    StartTurnResponse, TokenUsage,
};
use std::collections::VecDeque;

#[derive(Debug)]
struct EventProvider {
    events: VecDeque<ProviderTurnEvent>,
}

impl EventProvider {
    fn successful() -> Self {
        Self {
            events: VecDeque::from(success_events()),
        }
    }

    fn cancelled() -> Self {
        Self {
            events: VecDeque::from([ProviderTurnEvent::Cancelled]),
        }
    }

    fn failing() -> Self {
        Self {
            events: VecDeque::from([
                ProviderTurnEvent::Warning {
                    message: "before failure".to_string(),
                },
                ProviderTurnEvent::Error {
                    error: ProviderError {
                        code: "provider_failed".to_string(),
                        category: ProviderErrorCategory::ProviderInternal,
                        message: "provider failed".to_string(),
                        retryable: false,
                        provider_message: Some("raw safe detail".into()),
                        failure: None,
                        request_id: Some("req_test".into()),
                        diagnostic_context: Box::new(
                            std::iter::once(("http_status".to_string(), "500".to_string()))
                                .collect(),
                        ),
                        sources: Box::new(vec![ProviderErrorSource {
                            source: "test_provider".to_string(),
                            code: Some("upstream_failure".to_string()),
                            message: Some("raw safe detail".to_string()),
                        }]),
                        retry: Some(Box::new(ProviderRetryHint {
                            retry_after_ms: Some(100),
                            retry_at_unix: None,
                            source: Some("retry-after".to_string()),
                        })),
                    },
                },
            ]),
        }
    }
}

impl ModelProviderInvoker for EventProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "event-coherence".to_string(),
            })
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        let events = self.events.drain(..).collect();
        Box::pin(async move { Ok(PollTurnEventsResponse { events }) })
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

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: Some(10),
        output_tokens: Some(4),
        total_tokens: Some(14),
        cached_input_tokens: Some(3),
        cache_write_input_tokens: Some(2),
        reasoning_tokens: Some(1),
    }
}

fn success_events() -> Vec<ProviderTurnEvent> {
    vec![
        ProviderTurnEvent::TextDelta {
            text: "answer".to_string(),
        },
        ProviderTurnEvent::ReasoningDelta {
            text: "reason".to_string(),
        },
        ProviderTurnEvent::Usage { usage: usage() },
        ProviderTurnEvent::ExactRequestInputTokens {
            tokens: ExactRequestInputTokens::new(10),
        },
        ProviderTurnEvent::RequestProjection {
            projection: ProviderRequestProjection {
                provider: Some("test-provider".to_string()),
                api_shape: Some("test-api".to_string()),
                sent_message_count: Some(2),
                detail: Some("projection detail".to_string()),
                ..ProviderRequestProjection::default()
            },
        },
        ProviderTurnEvent::ProviderMetadata {
            key: "conversation_id".to_string(),
            value: "conversation-1".to_string(),
        },
        ProviderTurnEvent::Warning {
            message: "provider warning".to_string(),
        },
        ProviderTurnEvent::RetryScheduled {
            message: "provider retry notice".to_string(),
            retry_at_unix: 42,
        },
        ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::EndTurn,
        },
    ]
}

fn assert_success_metadata(events: &[AgentEvent]) {
    assert!(events.iter().any(
        |event| matches!(event, AgentEvent::Warning(message) if message == "provider warning")
    ));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Usage(value) if value == &usage()))
    );
    assert!(events.iter().any(
        |event| matches!(event, AgentEvent::ExactRequestInputTokens(tokens) if tokens.get() == 10)
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::RequestProjection(projection)
            if projection.provider.as_deref() == Some("test-provider")
                && projection.api_shape.as_deref() == Some("test-api")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ProviderMetadata { key, value }
            if key == "conversation_id" && value == "conversation-1"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::RetryScheduled { message, retry_at_unix }
            if message == "provider retry notice" && *retry_at_unix == 42
    )));
}

#[tokio::test]
async fn live_events_are_retained_verbatim_in_stream_terminal_response() {
    let mut stream = stream_text_builder()
        .prompt("event coherence")
        .run(EventProvider::successful());
    let mut live = Vec::new();
    let mut terminal = None;

    while let Some(item) = stream.next().await {
        match item {
            TextStreamItem::Event(event) => live.push(event),
            TextStreamItem::Finished(response) => terminal = Some(response),
            TextStreamItem::Error(error) => panic!("stream failed: {error}"),
            TextStreamItem::ScopedEvent(_) => {}
        }
    }

    let terminal = terminal.expect("stream should finish");
    assert_eq!(live, terminal.runtime.events);
    assert_eq!(terminal.text, "answer");
    assert_eq!(terminal.runtime.usage, Some(usage()));
    assert_eq!(terminal.runtime.stop_reason, Some(StopReason::EndTurn));
    assert_success_metadata(&terminal.runtime.events);
    assert!(
        matches!(terminal.runtime.events.last(), Some(AgentEvent::Finished {
        stop_reason: StopReason::EndTurn,
        usage: Some(value),
        ..
    }) if value == &usage())
    );
}

#[tokio::test]
async fn non_streaming_response_retains_the_same_provider_metadata_families() {
    let mut provider = EventProvider::successful();
    let response = generate_text_builder()
        .prompt("event coherence")
        .run(&mut provider)
        .await
        .expect("generation should finish");

    assert_eq!(response.text, "answer");
    assert_eq!(response.runtime.usage, Some(usage()));
    assert_eq!(response.runtime.stop_reason, Some(StopReason::EndTurn));
    assert_success_metadata(&response.runtime.events);
    assert!(
        matches!(response.runtime.events.last(), Some(AgentEvent::Finished {
        stop_reason: StopReason::EndTurn,
        usage: Some(value),
        ..
    }) if value == &usage())
    );
}

#[tokio::test]
async fn provider_failure_has_the_same_typed_cause_in_streaming_and_non_streaming_calls() {
    let mut stream = stream_text_builder()
        .prompt("fail")
        .run(EventProvider::failing());
    let mut warning = false;
    let mut stream_error = None;
    while let Some(item) = stream.next().await {
        match item {
            TextStreamItem::Event(AgentEvent::Warning(message)) => {
                warning = message == "before failure";
            }
            TextStreamItem::Error(error) => stream_error = Some(error),
            TextStreamItem::Finished(_) => panic!("provider failure must not finish"),
            TextStreamItem::Event(_) | TextStreamItem::ScopedEvent(_) => {}
        }
    }

    let mut provider = EventProvider::failing();
    let non_stream_error = generate_text_builder()
        .prompt("fail")
        .run(&mut provider)
        .await
        .expect_err("provider failure must be returned");

    assert!(warning);
    for error in [stream_error.expect("stream error"), non_stream_error] {
        assert!(matches!(
            error,
            BcodeError::Runtime(RuntimeError::Provider { code, message, error })
                if code == "provider_failed"
                    && message == "provider failed"
                    && error.provider_message.as_deref() == Some("raw safe detail")
                    && error.request_id.as_deref() == Some("req_test")
                    && error.diagnostic_context.get("http_status").map(String::as_str)
                        == Some("500")
                    && error.sources.first().map(|source| source.source.as_str())
                        == Some("test_provider")
                    && error.retry.as_ref().and_then(|hint| hint.retry_after_ms) == Some(100)
        ));
    }
}

#[tokio::test]
async fn provider_cancellation_is_a_visible_event_followed_by_the_typed_terminal_error() {
    let mut stream = stream_text_builder()
        .prompt("cancel")
        .run(EventProvider::cancelled());
    let mut saw_cancelled = false;
    let mut terminal = None;

    while let Some(item) = stream.next().await {
        match item {
            TextStreamItem::Event(AgentEvent::Cancelled) => saw_cancelled = true,
            TextStreamItem::Error(error) => terminal = Some(error),
            TextStreamItem::Finished(_) => panic!("cancelled stream must not finish"),
            TextStreamItem::Event(_) | TextStreamItem::ScopedEvent(_) => {}
        }
    }

    assert!(saw_cancelled);
    assert!(matches!(
        terminal,
        Some(BcodeError::Runtime(RuntimeError::Cancelled))
    ));
}

#[tokio::test]
async fn pre_start_cancellation_returns_the_same_typed_terminal_error_without_provider_events() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let mut stream = stream_text_builder()
        .prompt("cancel")
        .cancellation(cancellation)
        .run(EventProvider::successful());
    let mut events = Vec::new();
    let mut terminal = None;

    while let Some(item) = stream.next().await {
        match item {
            TextStreamItem::Event(event) => events.push(event),
            TextStreamItem::Error(error) => terminal = Some(error),
            TextStreamItem::Finished(_) => panic!("cancelled stream must not finish"),
            TextStreamItem::ScopedEvent(_) => {}
        }
    }

    assert!(events.is_empty());
    assert!(matches!(
        terminal,
        Some(BcodeError::Runtime(RuntimeError::Cancelled))
    ));
}
