use bcode::{
    Agent, AgentTurnRequest, GenerateTextResponse, InMemoryModelResponseCache, ModelMiddleware,
    ModelProviderInvoker, ModelResponseCache, ModelResponseCacheKey, ModelResponseCachePrivacy,
    ModelResponseCacheStatus, RuntimeFuture, StopReason, ToolCall, ToolDefinition,
    ToolInvocationResponse, ToolPolicyMetadata, ToolSideEffect, ToolUiMetadata,
    generate_object_builder, generate_text_builder,
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
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
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
                host_action: None,
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
                host_action: None,
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
