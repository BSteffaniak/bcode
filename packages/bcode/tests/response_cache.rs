use bcode::{
    Agent, AgentTurnRequest, GenerateTextResponse, InMemoryModelResponseCache, ModelMiddleware,
    ModelProviderInvoker, ModelResponseCache, ModelResponseCacheKey, ModelResponseCachePrivacy,
    ModelResponseCacheStatus, RuntimeFuture, StopReason, ToolCall, ToolDefinition,
    ToolInvocationResponse, generate_object_builder, generate_text_builder,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse,
};
use std::num::NonZeroUsize;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, AtomicUsize, Ordering},
};
use std::time::Duration;

#[derive(Debug, Default)]
struct MemoryCache {
    response: Mutex<Option<GenerateTextResponse>>,
    puts: Mutex<u32>,
}

impl ModelResponseCache for MemoryCache {
    fn get(&self, _request: &AgentTurnRequest) -> bcode::Result<Option<GenerateTextResponse>> {
        Ok(self
            .response
            .lock()
            .expect("cache lock should be available")
            .clone())
    }

    fn put(
        &self,
        _request: &AgentTurnRequest,
        response: &GenerateTextResponse,
    ) -> bcode::Result<()> {
        *self
            .response
            .lock()
            .expect("cache lock should be available") = Some(response.clone());
        *self.puts.lock().expect("put lock should be available") += 1;
        Ok(())
    }
}

#[derive(Debug, Default)]
struct CountingProvider {
    starts: u32,
    events: Vec<ProviderTurnEvent>,
}

impl ModelProviderInvoker for CountingProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.starts += 1;
        self.events = vec![
            ProviderTurnEvent::TextDelta {
                text: "cached response".to_string(),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            },
        ];
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "cache-turn".to_string(),
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

struct PanicCache;

impl ModelResponseCache for PanicCache {
    fn get(&self, _request: &AgentTurnRequest) -> bcode::Result<Option<GenerateTextResponse>> {
        panic!("streaming must not read the buffered response cache")
    }

    fn put(
        &self,
        _request: &AgentTurnRequest,
        _response: &GenerateTextResponse,
    ) -> bcode::Result<()> {
        panic!("streaming must not write the buffered response cache")
    }
}

#[tokio::test]
async fn streaming_explicitly_bypasses_buffered_response_cache() {
    let agent = bcode::Agent::builder()
        .response_cache(Arc::new(PanicCache))
        .build();
    let mut stream = agent.stream_text_with_provider(CountingProvider::default(), "stream me");
    let mut finished = false;

    while let Some(item) = stream.next().await {
        match item {
            bcode::TextStreamItem::Finished(response) => {
                assert_eq!(response.text, "cached response");
                finished = true;
            }
            bcode::TextStreamItem::Error(error) => panic!("stream failed: {error}"),
            bcode::TextStreamItem::Event(_) | bcode::TextStreamItem::ScopedEvent(_) => {}
        }
    }

    assert!(finished);
}

#[derive(Debug)]
struct SharedProvider {
    starts: Arc<AtomicUsize>,
    events: Vec<ProviderTurnEvent>,
}

impl SharedProvider {
    fn new(starts: Arc<AtomicUsize>) -> Self {
        Self {
            starts,
            events: Vec::new(),
        }
    }
}

impl ModelProviderInvoker for SharedProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.events = vec![
            ProviderTurnEvent::Usage {
                usage: bcode::TokenUsage {
                    input_tokens: Some(8),
                    output_tokens: Some(2),
                    total_tokens: Some(10),
                    ..bcode::TokenUsage::default()
                },
            },
            ProviderTurnEvent::TextDelta {
                text: "shared response".to_string(),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            },
        ];
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            Ok(StartTurnResponse {
                provider_turn_id: "shared-turn".to_string(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_memory_cache_coalesces_concurrent_misses_and_preserves_usage() {
    let cache = Arc::new(InMemoryModelResponseCache::new(
        Duration::from_secs(60),
        NonZeroUsize::new(8).expect("eight is non-zero"),
    ));
    let starts = Arc::new(AtomicUsize::new(0));
    let first = {
        let cache = cache.clone();
        let starts = starts.clone();
        tokio::spawn(async move {
            generate_text_builder()
                .prompt("same")
                .response_cache(cache)
                .run(&mut SharedProvider::new(starts))
                .await
        })
    };
    let second = {
        let cache = cache.clone();
        let starts = starts.clone();
        tokio::spawn(async move {
            generate_text_builder()
                .prompt("same")
                .response_cache(cache)
                .run(&mut SharedProvider::new(starts))
                .await
        })
    };
    let first = first.await.expect("first joins").expect("first succeeds");
    let second = second
        .await
        .expect("second joins")
        .expect("second succeeds");
    let encoded = serde_json::to_vec(&first).expect("cached response should serialize");
    let decoded: GenerateTextResponse =
        serde_json::from_slice(&encoded).expect("cached response should deserialize");
    assert_eq!(decoded.text, first.text);
    assert_eq!(decoded.steps, first.steps);
    assert_eq!(decoded.runtime.events, first.runtime.events);
    assert_eq!(decoded.runtime.usage, first.runtime.usage);

    assert_eq!(starts.load(Ordering::SeqCst), 1);
    let statuses = [&first.cache_status, &second.cache_status];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| matches!(status, ModelResponseCacheStatus::Stored { .. }))
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| matches!(status, ModelResponseCacheStatus::Hit { .. }))
            .count(),
        1
    );
    assert_eq!(first.runtime.usage, second.runtime.usage);
    assert_eq!(first.steps, second.steps);
}

#[tokio::test]
async fn in_memory_cache_expires_invalidates_and_evicts_by_capacity() {
    let cache = Arc::new(InMemoryModelResponseCache::new(
        Duration::from_millis(10),
        NonZeroUsize::new(1).expect("one is non-zero"),
    ));
    let starts = Arc::new(AtomicUsize::new(0));
    let run = |prompt: &'static str| {
        let cache = cache.clone();
        let starts = starts.clone();
        async move {
            generate_text_builder()
                .prompt(prompt)
                .response_cache(cache)
                .run(&mut SharedProvider::new(starts))
                .await
                .expect("request succeeds")
        }
    };

    run("first").await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    run("first").await;
    run("second").await;
    run("first").await;
    let request = AgentTurnRequest::new("", "first");
    cache
        .invalidate(&request)
        .expect("invalidate exact request");
    run("first").await;
    assert_eq!(starts.load(Ordering::SeqCst), 5);

    cache.invalidate_all().expect("invalidate all");
    run("first").await;
    assert_eq!(starts.load(Ordering::SeqCst), 6);
}

#[derive(Debug)]
struct NoStoreCache;

impl ModelResponseCache for NoStoreCache {
    fn privacy(&self, _request: &AgentTurnRequest) -> ModelResponseCachePrivacy {
        ModelResponseCachePrivacy::NoStore
    }

    fn get(&self, _request: &AgentTurnRequest) -> bcode::Result<Option<GenerateTextResponse>> {
        panic!("no-store must bypass lookup")
    }

    fn put(
        &self,
        _request: &AgentTurnRequest,
        _response: &GenerateTextResponse,
    ) -> bcode::Result<()> {
        panic!("no-store must bypass storage")
    }
}

#[tokio::test]
async fn no_store_privacy_bypasses_cache_operations() {
    let mut provider = CountingProvider::default();
    let response = generate_text_builder()
        .prompt("private")
        .response_cache(Arc::new(NoStoreCache))
        .run(&mut provider)
        .await
        .expect("provider should run");

    assert_eq!(provider.starts, 1);
    assert_eq!(response.cache_status, ModelResponseCacheStatus::Bypassed);
}

#[derive(Debug)]
struct CountingMiddleware {
    before: Arc<AtomicU32>,
    after: Arc<AtomicU32>,
}

impl ModelMiddleware for CountingMiddleware {
    fn before_request(&self, mut request: AgentTurnRequest) -> bcode::Result<AgentTurnRequest> {
        self.before.fetch_add(1, Ordering::SeqCst);
        request
            .metadata
            .insert("cache-key-input".to_string(), "v1".to_string());
        Ok(request)
    }

    fn after_response(
        &self,
        _request: &AgentTurnRequest,
        mut response: GenerateTextResponse,
    ) -> bcode::Result<GenerateTextResponse> {
        self.after.fetch_add(1, Ordering::SeqCst);
        response.text.push('!');
        Ok(response)
    }
}

#[tokio::test]
async fn cache_hits_pass_through_request_response_middleware_and_hooks() {
    let cache = Arc::new(InMemoryModelResponseCache::new(
        Duration::from_secs(60),
        NonZeroUsize::new(4).expect("four is non-zero"),
    ));
    let before = Arc::new(AtomicU32::new(0));
    let after = Arc::new(AtomicU32::new(0));
    let hook = Arc::new(AtomicU32::new(0));
    let hook_count = hook.clone();
    let agent = Agent::builder()
        .response_cache(cache)
        .middleware_layer(CountingMiddleware {
            before: before.clone(),
            after: after.clone(),
        })
        .on_after_model(move |_, _| {
            hook_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .build();
    let mut provider = CountingProvider::default();

    let first = agent
        .generate_text_with_provider(&mut provider, "cached")
        .await
        .expect("miss succeeds");
    let second = agent
        .generate_text_with_provider(&mut provider, "cached")
        .await
        .expect("hit succeeds");

    assert_eq!(first.text, "cached response!");
    assert_eq!(second.text, "cached response!");
    assert_eq!(provider.starts, 1);
    assert_eq!(before.load(Ordering::SeqCst), 2);
    assert_eq!(after.load(Ordering::SeqCst), 2);
    assert_eq!(hook.load(Ordering::SeqCst), 2);
}

#[test]
fn cache_key_is_secret_safe_and_changes_with_provider_model_config_and_schema() {
    let mut request = AgentTurnRequest::new("model-a", "top-secret-prompt");
    request.provider_plugin_id = Some("provider-a".to_string());
    request.provider_context.model_profile = Some("profile-a".to_string());
    request.structured_output = Some(bcode_model::StructuredOutputRequest {
        name: "result".to_string(),
        schema: serde_json::json!({"type": "object"}),
        strict: true,
    });
    let first = ModelResponseCacheKey::from_request(&request).expect("key should derive");
    let debug = format!("{first:?}");
    assert!(!debug.contains("top-secret-prompt"));
    assert_eq!(first.digest_hex.len(), 64);

    request.model_id = "model-b".to_string();
    let model = ModelResponseCacheKey::from_request(&request).expect("model key");
    assert_ne!(first, model);
    request.model_id = "model-a".to_string();
    request.provider_context.model_profile = Some("profile-b".to_string());
    let config = ModelResponseCacheKey::from_request(&request).expect("config key");
    assert_ne!(first, config);
    request.structured_output.as_mut().expect("schema").strict = false;
    let schema = ModelResponseCacheKey::from_request(&request).expect("schema key");
    assert_ne!(config, schema);

    request.provider_context.env.insert(
        "PROVIDER_API_KEY".to_string(),
        "first-secret-value".to_string(),
    );
    let first_secret = ModelResponseCacheKey::from_request(&request).expect("secret key");
    request.provider_context.env.insert(
        "PROVIDER_API_KEY".to_string(),
        "second-secret-value".to_string(),
    );
    let second_secret = ModelResponseCacheKey::from_request(&request).expect("rotated secret key");
    assert_eq!(
        first_secret, second_secret,
        "credential rotation preserves semantic identity"
    );
    let debug = format!("{second_secret:?}");
    assert!(!debug.contains("first-secret-value"));
    assert!(!debug.contains("second-secret-value"));
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema, PartialEq, Eq)]
struct CachedObject {
    value: String,
}

#[derive(Debug, Default)]
struct JsonProvider(CountingProvider);

impl ModelProviderInvoker for JsonProvider {
    fn start_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.0.start_turn(provider_plugin_id, request)
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
                        text: r#"{"value":"cached"}"#.to_string(),
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
        provider_plugin_id: Option<&'a str>,
        request: &'a CancelTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        self.0.cancel_turn(provider_plugin_id, request)
    }

    fn finish_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a FinishTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        self.0.finish_turn(provider_plugin_id, request)
    }
}

#[tokio::test]
async fn structured_response_cache_preserves_typed_decode() {
    let cache = Arc::new(InMemoryModelResponseCache::new(
        Duration::from_secs(60),
        NonZeroUsize::new(4).expect("four is non-zero"),
    ));
    let mut provider = JsonProvider::default();
    let first: CachedObject = generate_object_builder()
        .prompt("object")
        .configure_agent(|agent| agent.response_cache(cache.clone()))
        .run(&mut provider)
        .await
        .expect("first object");
    let second: CachedObject = generate_object_builder()
        .prompt("object")
        .configure_agent(|agent| agent.response_cache(cache))
        .run(&mut provider)
        .await
        .expect("cached object");

    assert_eq!(
        first,
        CachedObject {
            value: "cached".to_string()
        }
    );
    assert_eq!(second, first);
    assert_eq!(provider.0.starts, 1);
}

fn tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "cached_tool".to_string(),
        description: "cache safety test".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
    }
}

#[tokio::test]
async fn tool_advertising_requests_bypass_cache_unless_explicitly_enabled() {
    let mut provider = CountingProvider::default();
    let agent = Agent::builder()
        .response_cache(Arc::new(PanicCache))
        .inline_tool(tool_definition(), |_| {
            Ok(ToolInvocationResponse {
                output: "unused".to_string(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                result: None,
            })
        })
        .build();

    let response = agent
        .generate_text_with_provider(&mut provider, "tools")
        .await
        .expect("tool request bypasses cache");
    assert_eq!(response.cache_status, ModelResponseCacheStatus::Bypassed);
    assert_eq!(provider.starts, 1);
}

#[derive(Debug)]
struct ToolLoopProvider {
    starts: Arc<AtomicUsize>,
    events: Vec<ProviderTurnEvent>,
}

impl ToolLoopProvider {
    fn new(starts: Arc<AtomicUsize>) -> Self {
        Self {
            starts,
            events: Vec::new(),
        }
    }
}

impl ModelProviderInvoker for ToolLoopProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        let continued = request.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|content| matches!(content, bcode::ModelContentBlock::ToolResult { .. }))
        });
        self.events = if continued {
            vec![
                ProviderTurnEvent::TextDelta {
                    text: "tool complete".to_string(),
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
            ]
        } else {
            vec![
                ProviderTurnEvent::ToolCallFinished {
                    call: ToolCall {
                        id: "cached-call".to_string(),
                        name: "cached_tool".to_string(),
                        arguments: serde_json::json!({}),
                    },
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::ToolCall,
                },
            ]
        };
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "tool-cache-turn".to_string(),
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
struct StructuredToolLoopProvider {
    starts: Arc<AtomicUsize>,
    events: Vec<ProviderTurnEvent>,
}

impl StructuredToolLoopProvider {
    fn new(starts: Arc<AtomicUsize>) -> Self {
        Self {
            starts,
            events: Vec::new(),
        }
    }
}

impl ModelProviderInvoker for StructuredToolLoopProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.events = if request.structured_output.is_some() {
            vec![
                ProviderTurnEvent::TextDelta {
                    text: r#"{"value":"final"}"#.to_string(),
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
            ]
        } else if request.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|content| matches!(content, bcode::ModelContentBlock::ToolResult { .. }))
        }) {
            vec![ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            }]
        } else {
            vec![
                ProviderTurnEvent::ToolCallFinished {
                    call: ToolCall {
                        id: "structured-cache-call".to_string(),
                        name: "cached_tool".to_string(),
                        arguments: serde_json::json!({}),
                    },
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::ToolCall,
                },
            ]
        };
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "structured-cache-turn".to_string(),
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
async fn structured_tool_work_is_never_bypassed_or_replayed_by_response_cache() {
    let cache = Arc::new(
        InMemoryModelResponseCache::new(
            Duration::from_secs(60),
            NonZeroUsize::new(4).expect("four is non-zero"),
        )
        .with_tool_responses(true),
    );
    let starts = Arc::new(AtomicUsize::new(0));
    let invocations = Arc::new(AtomicUsize::new(0));
    let invocation_count = invocations.clone();
    let agent = Agent::builder()
        .structured_output_execution(bcode::CapabilityExecution::ToolFreeProviderRound)
        .response_cache(cache)
        .inline_tool(tool_definition(), move |_| {
            invocation_count.fetch_add(1, Ordering::SeqCst);
            Ok(ToolInvocationResponse {
                output: "tool output".to_string(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                result: None,
            })
        })
        .metadata("tool-version", "structured-cache-v1")
        .build();
    let mut provider = StructuredToolLoopProvider::new(starts.clone());
    let options = bcode::StructuredOutputOptions::json_schema(
        "CachedObject",
        serde_json::json!({
            "type": "object",
            "required": ["value"],
            "properties": { "value": {"type": "string"} }
        }),
    );

    for _ in 0..2 {
        let object: CachedObject = agent
            .generate_object_with_provider_and_options(&mut provider, "use tool", options.clone())
            .await
            .expect("structured tool request");
        assert_eq!(object.value, "final");
    }

    assert_eq!(starts.load(Ordering::SeqCst), 6);
    assert_eq!(invocations.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn explicit_safe_tool_cache_preserves_complete_steps_without_reexecution() {
    let cache = Arc::new(
        InMemoryModelResponseCache::new(
            Duration::from_secs(60),
            NonZeroUsize::new(4).expect("four is non-zero"),
        )
        .with_tool_responses(true),
    );
    let starts = Arc::new(AtomicUsize::new(0));
    let invocations = Arc::new(AtomicUsize::new(0));
    let invocation_count = invocations.clone();
    let agent = Agent::builder()
        .response_cache(cache)
        .inline_tool(tool_definition(), move |_| {
            invocation_count.fetch_add(1, Ordering::SeqCst);
            Ok(ToolInvocationResponse {
                output: "tool output".to_string(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                result: None,
            })
        })
        .metadata("tool-version", "cached-tool-v1")
        .build();
    let mut provider = ToolLoopProvider::new(starts.clone());

    let first = agent
        .generate_text_with_provider(&mut provider, "use tool")
        .await
        .expect("tool miss succeeds");
    let second = agent
        .generate_text_with_provider(&mut provider, "use tool")
        .await
        .expect("tool hit succeeds");

    assert_eq!(
        starts.load(Ordering::SeqCst),
        2,
        "only the first two-round loop runs"
    );
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    assert_eq!(first.steps, second.steps);
    assert!(matches!(
        second.cache_status,
        ModelResponseCacheStatus::Hit { .. }
    ));
    assert!(second.steps.iter().any(|step| matches!(
        step,
        bcode::GenerationStep::ToolResult { result, .. } if result.output == "tool output"
    )));
}

#[derive(Debug)]
struct GatedLookupCache {
    response: Option<GenerateTextResponse>,
    entered: AtomicUsize,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    aborts: AtomicUsize,
}

impl ModelResponseCache for GatedLookupCache {
    fn get(&self, _request: &AgentTurnRequest) -> bcode::Result<Option<GenerateTextResponse>> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.release
            .lock()
            .expect("release lock")
            .recv_timeout(Duration::from_secs(5))
            .expect("test releases lookup");
        Ok(self.response.clone())
    }

    fn put(
        &self,
        _request: &AgentTurnRequest,
        _response: &GenerateTextResponse,
    ) -> bcode::Result<()> {
        panic!("abandoned lookup must not store")
    }

    fn abort(&self, _request: &AgentTurnRequest) {
        self.aborts.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn abandoned_blocking_lookup_releases_late_miss() {
    check_abandoned_lookup(false, false).await;
}

#[tokio::test]
async fn cancelled_blocking_lookup_returns_before_adapter_and_releases_late_miss() {
    check_abandoned_lookup(true, false).await;
}

#[tokio::test]
async fn cancelled_blocking_lookup_discards_late_hit_without_abort() {
    check_abandoned_lookup(true, true).await;
}

async fn check_abandoned_lookup(cancel: bool, hit: bool) {
    let response = if hit {
        Some(
            Agent::builder()
                .build()
                .generate_text_with_provider(&mut CountingProvider::default(), "fixture response")
                .await
                .expect("fixture generation"),
        )
    } else {
        None
    };
    let (release, receiver) = std::sync::mpsc::channel();
    let cache = Arc::new(GatedLookupCache {
        response,
        entered: AtomicUsize::new(0),
        release: Mutex::new(receiver),
        aborts: AtomicUsize::new(0),
    });
    let agent = Agent::builder().response_cache(cache.clone()).build();
    let mut provider = CountingProvider::default();
    let cancellation = bcode::CancellationToken::new();
    let mut generation = Box::pin(agent.generate_text_with_provider_and_cancellation(
        &mut provider,
        "abandoned lookup",
        cancellation.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            tokio::select! {
                result = &mut generation => panic!("lookup unexpectedly completed: {result:?}"),
                () = tokio::task::yield_now() => {
                    if cache.entered.load(Ordering::SeqCst) == 1 { break; }
                }
            }
        }
    })
    .await
    .expect("lookup starts");
    if cancel {
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), &mut generation)
            .await
            .expect("cancellation does not wait for adapter");
        assert!(matches!(
            result,
            Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Cancelled))
        ));
    }
    drop(generation);
    assert_eq!(provider.starts, 0);
    assert_eq!(cache.aborts.load(Ordering::SeqCst), 0);
    release.send(()).expect("release abandoned lookup");
    drop(agent);
    tokio::time::timeout(Duration::from_secs(2), async {
        while Arc::strong_count(&cache) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("late lookup result released");
    assert_eq!(cache.aborts.load(Ordering::SeqCst), usize::from(!hit));
    assert_eq!(provider.starts, 0);
}

#[derive(Debug)]
struct StoreOutcomeCache {
    fail: bool,
    puts: AtomicUsize,
    aborts: AtomicUsize,
}

impl ModelResponseCache for StoreOutcomeCache {
    fn get(&self, _request: &AgentTurnRequest) -> bcode::Result<Option<GenerateTextResponse>> {
        Ok(None)
    }

    fn put(
        &self,
        _request: &AgentTurnRequest,
        _response: &GenerateTextResponse,
    ) -> bcode::Result<()> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            Err(bcode::BcodeError::Cache("fixture storage failure".into()))
        } else {
            Ok(())
        }
    }

    fn abort(&self, _request: &AgentTurnRequest) {
        self.aborts.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn cache_storage_disarms_or_aborts_miss_exactly_once() {
    for fail in [false, true] {
        let cache = Arc::new(StoreOutcomeCache {
            fail,
            puts: AtomicUsize::new(0),
            aborts: AtomicUsize::new(0),
        });
        let agent = Agent::builder().response_cache(cache.clone()).build();
        let mut provider = CountingProvider::default();
        let result = agent
            .generate_text_with_provider(&mut provider, "store miss")
            .await;
        if fail {
            assert!(
                matches!(result, Err(bcode::BcodeError::Cache(message)) if message == "fixture storage failure")
            );
        } else {
            assert!(matches!(
                result.expect("storage succeeds").cache_status,
                ModelResponseCacheStatus::Stored { .. }
            ));
        }
        assert_eq!(provider.starts, 1);
        assert_eq!(cache.puts.load(Ordering::SeqCst), 1);
        assert_eq!(cache.aborts.load(Ordering::SeqCst), usize::from(fail));
    }
}

#[cfg(feature = "testing")]
#[derive(Debug, Default)]
struct AbortProbeCache {
    lookups: AtomicUsize,
    aborts: AtomicUsize,
}

#[cfg(feature = "testing")]
impl ModelResponseCache for AbortProbeCache {
    fn get(&self, _request: &AgentTurnRequest) -> bcode::Result<Option<GenerateTextResponse>> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }

    fn put(
        &self,
        _request: &AgentTurnRequest,
        _response: &GenerateTextResponse,
    ) -> bcode::Result<()> {
        panic!("pending generation must not store a response")
    }

    fn abort(&self, _request: &AgentTurnRequest) {
        self.aborts.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn already_cancelled_generation_does_not_dispatch_cache_lookup() {
    let cache = Arc::new(AbortProbeCache::default());
    let agent = Agent::builder().response_cache(cache.clone()).build();
    let mut provider = CountingProvider::default();
    let cancellation = bcode::CancellationToken::new();
    cancellation.cancel();
    let result = agent
        .generate_text_with_provider_and_cancellation(
            &mut provider,
            "already cancelled",
            cancellation,
        )
        .await;
    assert!(matches!(
        result,
        Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Cancelled))
    ));
    drop(agent);
    // Wait for any wrongly dispatched blocking task to relinquish its adapter reference,
    // so a delayed get cannot make the zero-lookup assertion pass accidentally.
    tokio::time::timeout(Duration::from_secs(2), async {
        while Arc::strong_count(&cache) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cache references released");
    assert_eq!(cache.lookups.load(Ordering::SeqCst), 0);
    assert_eq!(cache.aborts.load(Ordering::SeqCst), 0);
    assert_eq!(provider.starts, 0);
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn returned_provider_failure_aborts_cache_miss_once() {
    use bcode::testing::{ScriptedProvider, ScriptedProviderTurn};

    for started in [false, true] {
        let cache = Arc::new(AbortProbeCache::default());
        let agent = Agent::builder().response_cache(cache.clone()).build();
        let failure = bcode::ProviderError {
            code: "cache_abort_control".into(),
            category: bcode::ProviderErrorCategory::ProviderInternal,
            message: "fixture failure".into(),
            retryable: false,
            provider_message: None,
            failure: None,
            request_id: None,
            diagnostic_context: Box::default(),
            sources: Box::default(),
            retry: None,
        };
        let turn = if started {
            ScriptedProviderTurn::new().poll_error(failure)
        } else {
            ScriptedProviderTurn::start_error(failure)
        };
        let mut provider = ScriptedProvider::new([turn]);
        let probe = provider.probe();
        let error = agent
            .generate_text_with_provider(&mut provider, "failed miss")
            .await
            .expect_err("provider fails");
        assert!(
            matches!(error, bcode::BcodeError::Runtime(bcode::RuntimeError::Provider { code, .. }) if code == "cache_abort_control")
        );
        assert_eq!(cache.lookups.load(Ordering::SeqCst), 1);
        assert_eq!(cache.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(probe.requests().len(), 1);
        probe
            .assert_finish_count(usize::from(started))
            .expect("finish only a started provider");
        probe
            .assert_cancellation_count(usize::from(started))
            .expect("cancel only a started provider");
    }
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn cancelling_buffered_generation_releases_cache_and_provider() {
    use bcode::testing::{ScriptedProvider, ScriptedProviderTurn};

    let cache = Arc::new(AbortProbeCache::default());
    let agent = Agent::builder().response_cache(cache.clone()).build();
    let mut provider = ScriptedProvider::new([ScriptedProviderTurn::new().pending()]);
    let probe = provider.probe();
    let cancellation = bcode::CancellationToken::new();
    let mut generation = Box::pin(agent.generate_text_with_provider_and_cancellation(
        &mut provider,
        "cancel miss",
        cancellation.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            tokio::select! {
                result = &mut generation => panic!("generation unexpectedly completed: {result:?}"),
                () = tokio::task::yield_now() => {
                    if !probe.requests().is_empty() { break; }
                }
            }
        }
    })
    .await
    .expect("provider starts after cache miss");
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), generation)
        .await
        .expect("cancellation completes");
    assert!(matches!(
        result,
        Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Cancelled))
    ));
    assert_eq!(cache.lookups.load(Ordering::SeqCst), 1);
    assert_eq!(cache.aborts.load(Ordering::SeqCst), 1);
    probe.assert_finish_count(1).expect("provider released");
    probe
        .assert_cancellation_count(1)
        .expect("provider cancelled once");
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn dropping_buffered_generation_after_cache_miss_releases_reservation() {
    use bcode::testing::{ScriptedProvider, ScriptedProviderTurn};

    let cache = Arc::new(AbortProbeCache::default());
    let agent = Agent::builder().response_cache(cache.clone()).build();
    let mut provider = ScriptedProvider::new([ScriptedProviderTurn::new().pending()]);
    let probe = provider.probe();
    let mut generation = Box::pin(agent.generate_text_with_provider(&mut provider, "drop miss"));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            tokio::select! {
                result = &mut generation => panic!("generation unexpectedly completed: {result:?}"),
                () = tokio::task::yield_now() => {
                    if !probe.requests().is_empty() {
                        break;
                    }
                }
            }
        }
    })
    .await
    .expect("provider starts after cache miss");
    assert_eq!(cache.lookups.load(Ordering::SeqCst), 1);
    drop(generation);
    tokio::time::timeout(Duration::from_secs(2), async {
        while cache.aborts.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropping generation must release cache reservation");
    assert_eq!(cache.aborts.load(Ordering::SeqCst), 1);
}

#[test]
fn cancelled_cache_follower_exits_within_bounded_wait() {
    let cache = Arc::new(InMemoryModelResponseCache::new(
        Duration::from_secs(60),
        NonZeroUsize::new(2).expect("positive capacity"),
    ));
    let leader = AgentTurnRequest::new("model", "follower cancellation");
    assert!(cache.get(&leader).expect("leader reservation").is_none());
    let mut request = leader.clone();
    request.cancellation = bcode::CancellationToken::new();
    let cancellation = request.cancellation.clone();
    let follower_cache = cache.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let follower = std::thread::spawn(move || {
        sender
            .send(follower_cache.get(&request))
            .expect("receiver alive");
    });
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(100)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    cancellation.cancel();
    assert!(matches!(
        receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("bounded cancellation"),
        Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Cancelled))
    ));
    follower.join().expect("follower exits");
    assert!(!leader.cancellation.is_cancelled());

    let next_request = leader.clone();
    let next_cache = cache.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let next = std::thread::spawn(move || {
        sender
            .send(next_cache.get(&next_request))
            .expect("receiver alive");
    });
    assert!(
        matches!(
            receiver.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "cancelled follower must not release the leader reservation"
    );
    cache.abort(&leader);
    assert!(
        receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("leader release wakes follower")
            .expect("next lookup succeeds")
            .is_none()
    );
    next.join().expect("next follower exits");
    cache.abort(&leader);
}

#[tokio::test]
#[ignore = "known defect: key-only cache completion is not fenced across invalidation"]
async fn stale_cache_completion_cannot_overwrite_post_invalidation_response() {
    let cache = InMemoryModelResponseCache::new(
        Duration::from_secs(60),
        NonZeroUsize::new(2).expect("positive capacity"),
    );
    let request = AgentTurnRequest::new("model", "same key");
    let mut provider = CountingProvider::default();
    let mut stale = Agent::builder()
        .build()
        .generate_text_with_provider(&mut provider, "fixture response")
        .await
        .expect("response fixture");
    stale.text = "stale".into();
    let mut fresh = stale.clone();
    fresh.text = "fresh".into();

    assert!(cache.get(&request).expect("old miss").is_none());
    cache.invalidate_all().expect("invalidate old reservation");
    assert!(cache.get(&request).expect("replacement miss").is_none());
    cache.put(&request, &fresh).expect("replacement completes");
    cache.put(&request, &stale).expect("old completion handled");
    assert_eq!(
        cache
            .get(&request)
            .expect("cached response")
            .expect("hit")
            .text,
        "fresh"
    );
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn dropping_uncached_buffered_generation_releases_provider() {
    use bcode::testing::{ScriptedProvider, ScriptedProviderTurn};
    let runtime = bcode::AgentRuntime::new();
    let agent = Agent::builder().runtime(runtime.clone()).build();
    let mut provider = ScriptedProvider::new([ScriptedProviderTurn::new().pending()]);
    let probe = provider.probe();
    let mut generation =
        Box::pin(agent.generate_text_with_provider(&mut provider, "drop uncached"));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            tokio::select! {
                result = &mut generation => panic!("pending provider completed: {result:?}"),
                () = tokio::task::yield_now() => {
                    if !probe.requests().is_empty() { break; }
                }
            }
        }
    })
    .await
    .expect("provider starts");
    assert!(runtime.active_turn_generation().is_some());
    drop(generation);
    assert!(
        runtime.active_turn_generation().is_none(),
        "dropped loop releases runtime scope"
    );
    let cleanup = tokio::time::timeout(Duration::from_secs(2), async {
        while probe.assert_finish_count(1).is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    probe
        .assert_finish_count(1)
        .expect("dropped buffered provider released within cleanup watchdog");
    cleanup.expect("cleanup completed before watchdog");
    probe
        .assert_cancellation_count(1)
        .expect("dropped provider cancelled");
}

#[test]
fn abandoned_single_flight_lease_expires() {
    let cache = InMemoryModelResponseCache::new(
        Duration::from_secs(60),
        NonZeroUsize::new(4).expect("four is non-zero"),
    )
    .with_single_flight_timeout(Duration::from_millis(10));
    let request = AgentTurnRequest::new("model", "abandoned");
    assert!(cache.get(&request).expect("leader reservation").is_none());
    std::thread::sleep(Duration::from_millis(20));
    assert!(cache.get(&request).expect("replacement leader").is_none());
}

#[tokio::test]
async fn response_cache_short_circuits_provider_after_first_response() {
    let cache = Arc::new(MemoryCache::default());
    let mut provider = CountingProvider::default();

    let first = generate_text_builder()
        .prompt("cache me")
        .response_cache(cache.clone())
        .run(&mut provider)
        .await
        .expect("cache miss should invoke provider");
    let second = generate_text_builder()
        .prompt("cache me")
        .response_cache(cache.clone())
        .run(&mut provider)
        .await
        .expect("cache hit should return response");

    assert!(matches!(
        first.cache_status,
        ModelResponseCacheStatus::Stored { .. }
    ));
    assert!(matches!(
        second.cache_status,
        ModelResponseCacheStatus::Hit { .. }
    ));
    assert_eq!(provider.starts, 1);
    assert_eq!(first.text, second.text);
    assert_eq!(*cache.puts.lock().expect("put lock should be available"), 1);
}
