use bcode::{
    ModelContentBlock, ModelProviderInvoker, ProviderError, ProviderErrorCategory,
    ProviderRetryHint, RetryPolicy, RuntimeFuture, StopReason, TextStreamItem, ToolCall,
    ToolDefinition, ToolInvocationResponse, ToolPolicyMetadata, ToolSideEffect, ToolUiMetadata,
    generate_text_builder, stream_text_builder,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[derive(Debug, Default)]
struct FlakyProvider {
    starts: u32,
    events: Vec<ProviderTurnEvent>,
}

impl ModelProviderInvoker for FlakyProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.starts += 1;
        Box::pin(async move {
            if self.starts == 1 {
                Err(bcode::RuntimeError::ProviderInvocation(
                    "temporary failure".to_string(),
                ))
            } else {
                self.events = vec![
                    ProviderTurnEvent::TextDelta {
                        text: "recovered".to_string(),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::EndTurn,
                    },
                ];
                Ok(StartTurnResponse {
                    provider_turn_id: format!("retry-turn-{}", self.starts),
                })
            }
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

#[derive(Debug, Default)]
struct ContinuationRetryProvider {
    starts: u32,
    continuation_attempts: u32,
    events: Vec<ProviderTurnEvent>,
}

impl ModelProviderInvoker for ContinuationRetryProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.starts += 1;
        let continued = request.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|content| matches!(content, ModelContentBlock::ToolResult { .. }))
        });
        if continued {
            self.continuation_attempts += 1;
        }
        let continuation_attempt = self.continuation_attempts;
        Box::pin(async move {
            if continued && continuation_attempt == 1 {
                return Err(bcode::RuntimeError::ProviderInvocation(
                    "continuation temporarily failed".to_string(),
                ));
            }
            self.events = if continued {
                vec![
                    ProviderTurnEvent::TextDelta {
                        text: "done".to_string(),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::EndTurn,
                    },
                ]
            } else {
                vec![
                    ProviderTurnEvent::ToolCallFinished {
                        call: ToolCall {
                            id: "unsafe-call".to_string(),
                            name: "unsafe".to_string(),
                            arguments: serde_json::json!({}),
                        },
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::ToolCall,
                    },
                ]
            };
            Ok(StartTurnResponse {
                provider_turn_id: format!("continuation-{continuation_attempt}"),
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

#[tokio::test]
async fn provider_retry_after_unsafe_tool_result_never_reexecutes_the_tool() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let count = invocations.clone();
    let mut provider = ContinuationRetryProvider::default();
    let response = generate_text_builder()
        .prompt("unsafe")
        .retry_policy(RetryPolicy::new(1, Duration::ZERO))
        .configure_agent(|agent| agent.custom_permission_policy(bcode::AllowAllPolicy))
        .inline_tool(
            ToolDefinition {
                name: "unsafe".to_string(),
                description: "unsafe side effect".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                side_effect: ToolSideEffect::ExecuteProcess,
                requires_permission: false,
                policy: ToolPolicyMetadata::default(),
                ui: ToolUiMetadata::default(),
            },
            move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(ToolInvocationResponse {
                    output: "effect committed".to_string(),
                    is_error: false,
                    content: Vec::new(),
                    full_output: None,
                    result: None,
                })
            },
        )
        .run(&mut provider)
        .await
        .expect("continuation retry should recover");

    assert_eq!(response.text, "done");
    assert_eq!(provider.starts, 3);
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
}

#[derive(Debug)]
struct StructuredFailureProvider {
    retryable: bool,
    starts: u32,
}

impl ModelProviderInvoker for StructuredFailureProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.starts += 1;
        let retryable = self.retryable;
        Box::pin(async move {
            Err(bcode::RuntimeError::Provider {
                code: "typed_failure".to_string(),
                message: "typed provider failure".to_string(),
                error: Box::new(ProviderError {
                    code: "typed_failure".to_string(),
                    category: if retryable {
                        ProviderErrorCategory::RateLimit
                    } else {
                        ProviderErrorCategory::InvalidRequest
                    },
                    message: "typed provider failure".to_string(),
                    retryable,
                    provider_message: None,
                    failure: None,
                    request_id: None,
                    diagnostic_context: Box::default(),
                    sources: Box::default(),
                    retry: Some(Box::new(ProviderRetryHint {
                        retry_after_ms: Some(1),
                        retry_at_unix: None,
                        source: Some("provider".to_string()),
                    })),
                }),
            })
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        unreachable!("start always fails")
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
async fn retry_policy_uses_typed_retryability_and_provider_delay_hint() {
    let mut provider = StructuredFailureProvider {
        retryable: true,
        starts: 0,
    };
    let started = std::time::Instant::now();
    let _ = generate_text_builder()
        .prompt("typed retry")
        .retry_policy(
            RetryPolicy::new(1, Duration::ZERO)
                .with_max_delay(Duration::from_millis(10))
                .with_jitter_millis(2),
        )
        .run(&mut provider)
        .await
        .expect_err("both typed attempts fail");

    assert_eq!(provider.starts, 2);
    assert!(started.elapsed() >= Duration::from_millis(1));
}

#[tokio::test]
async fn retry_policy_never_retries_nonretryable_typed_provider_error() {
    let mut provider = StructuredFailureProvider {
        retryable: false,
        starts: 0,
    };
    let _ = generate_text_builder()
        .prompt("typed terminal")
        .retry_policy(RetryPolicy::new(3, Duration::ZERO))
        .run(&mut provider)
        .await
        .expect_err("nonretryable error is terminal");

    assert_eq!(provider.starts, 1);
}

#[derive(Debug, Default)]
struct VisibleThenFailsProvider {
    starts: u32,
    polls: u32,
}

impl ModelProviderInvoker for VisibleThenFailsProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.starts += 1;
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "visible-failure".to_string(),
            })
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        self.polls += 1;
        Box::pin(async move {
            if self.polls == 1 {
                Ok(PollTurnEventsResponse {
                    events: vec![ProviderTurnEvent::TextDelta {
                        text: "visible".to_string(),
                    }],
                })
            } else {
                Err(bcode::RuntimeError::ProviderInvocation(
                    "stream failed".to_string(),
                ))
            }
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
async fn streaming_never_retries_after_model_visible_output() {
    let provider = VisibleThenFailsProvider::default();
    let mut stream = stream_text_builder()
        .prompt("visible")
        .retry_policy(RetryPolicy::new(3, Duration::ZERO))
        .run(provider);
    let mut visible = false;
    let mut terminal = None;
    while let Some(item) = stream.next().await {
        match item {
            TextStreamItem::Event(bcode::AgentEvent::TextDelta(text)) if text == "visible" => {
                visible = true;
            }
            TextStreamItem::Error(error) => terminal = Some(error),
            TextStreamItem::Finished(_) => panic!("failed stream cannot finish"),
            TextStreamItem::Event(_) | TextStreamItem::ScopedEvent(_) => {}
        }
    }
    assert!(visible);
    assert!(matches!(
        terminal,
        Some(bcode::BcodeError::Runtime(
            bcode::RuntimeError::ProviderAfterOutput(_)
        ))
    ));
}

#[tokio::test]
async fn streaming_retry_policy_recovers_and_preserves_event_delivery() {
    let mut stream = stream_text_builder()
        .prompt("retry stream")
        .retry_policy(RetryPolicy::new(1, Duration::ZERO))
        .run(FlakyProvider::default());
    let mut delta = None;
    let mut finished = None;

    while let Some(item) = stream.next().await {
        match item {
            TextStreamItem::Event(bcode::AgentEvent::TextDelta(text)) => delta = Some(text),
            TextStreamItem::Finished(response) => finished = Some(response.text),
            TextStreamItem::Error(error) => panic!("stream failed: {error}"),
            TextStreamItem::Event(_) | TextStreamItem::ScopedEvent(_) => {}
        }
    }

    assert_eq!(delta.as_deref(), Some("recovered"));
    assert_eq!(finished.as_deref(), Some("recovered"));
}

#[tokio::test]
async fn retry_policy_retries_provider_failures_within_bound() {
    let mut provider = FlakyProvider::default();

    let response = generate_text_builder()
        .prompt("retry")
        .retry_policy(RetryPolicy::new(1, Duration::ZERO))
        .run(&mut provider)
        .await
        .expect("one configured retry should recover");

    assert_eq!(provider.starts, 2);
    assert_eq!(response.text, "recovered");
}

#[tokio::test]
async fn retry_policy_preserves_failure_when_bound_is_zero() {
    let mut provider = FlakyProvider::default();

    let error = generate_text_builder()
        .prompt("do not retry")
        .retry_policy(RetryPolicy::new(0, Duration::ZERO))
        .run(&mut provider)
        .await
        .expect_err("zero retries should preserve the first failure");

    assert_eq!(provider.starts, 1);
    assert!(error.to_string().contains("temporary failure"));
}
