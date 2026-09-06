use bcode::{
    AgentBuilder, AgentRuntime, ProviderRequestIdentity, ProviderTurnEvent, TokenUsage, testing::*,
};
use std::sync::Arc;
use std::time::Duration;

#[cfg(not(feature = "simulation-example"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = switchy::unsync::Builder::new().build()?;
    let result = runtime.block_on(run());
    runtime.wait()?;
    result?;
    Ok(())
}

#[cfg(feature = "simulation-example")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The harness, not application code, advances simulated time. One poll per step
    // is an explicit exploration policy, not a claim of exhaustive schedule coverage.
    switchy::time::simulator::reset_step();
    let runtime = switchy::unsync::Builder::new().build()?;
    let mut task = runtime.spawn(run());
    for _ in 0..10_000 {
        runtime.tick();
        if task.is_finished() {
            let result = runtime.block_on(task)?;
            // Do not call unbounded `wait`: runtime-wide bounded draining is not
            // exposed upstream yet. This diagnostic example runs once per process
            // and does not certify cleanup or in-process run isolation.
            result?;
            return Ok(());
        }
        let _ = switchy::time::simulator::next_step();
    }
    task.abort();
    // Request cancellation; a single poll is not proof of runtime-wide cleanup.
    runtime.tick();
    Err("simulation harness step budget exhausted (not a product timeout)".into())
}

struct InProcessEcho;

impl bcode::InProcessModelProvider for InProcessEcho {
    fn run_turn(
        &self,
        _request: bcode::ModelTurnRequest,
        context: bcode::InProcessProviderContext,
    ) -> bcode::InProcessProviderFuture<'_> {
        Box::pin(async move {
            switchy::unsync::time::sleep(Duration::from_millis(1)).await;
            context
                .events()
                .emit(ProviderTurnEvent::TextDelta {
                    text: "in-process answer".into(),
                })
                .expect("active in-process turn accepts text");
            Ok(bcode::InProcessProviderOutcome::EndTurn)
        })
    }
}

struct PendingInProcess {
    started: bcode::CancellationToken,
    released: bcode::CancellationToken,
    context: Arc<std::sync::Mutex<Option<bcode::InProcessProviderContext>>>,
}

struct WorkerRelease(bcode::CancellationToken);

impl Drop for WorkerRelease {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl bcode::InProcessModelProvider for PendingInProcess {
    fn run_turn(
        &self,
        _request: bcode::ModelTurnRequest,
        context: bcode::InProcessProviderContext,
    ) -> bcode::InProcessProviderFuture<'_> {
        Box::pin(async move {
            if self.started.is_cancelled() {
                assert!(
                    !context.cancellation().is_cancelled(),
                    "fresh turn is active"
                );
                let old_context = self
                    .context
                    .lock()
                    .expect("context lock")
                    .clone()
                    .expect("previous worker context");
                assert_eq!(
                    old_context.events().emit(ProviderTurnEvent::TextDelta {
                        text: "stale during replacement".into(),
                    }),
                    Err(bcode::InProcessProviderEmitError::TurnFinished),
                    "old context must not inject into an active replacement"
                );
                context
                    .events()
                    .emit(ProviderTurnEvent::TextDelta {
                        text: "recovered worker".into(),
                    })
                    .expect("fresh turn accepts output");
                return Ok(bcode::InProcessProviderOutcome::EndTurn);
            }
            let _release = WorkerRelease(self.released.clone());
            *self.context.lock().expect("context lock") = Some(context);
            self.started.cancel();
            std::future::pending().await
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum InProcessCleanup {
    AdapterDrop,
    Cancel,
    Deadline,
}

async fn run_in_process_cleanup(mode: InProcessCleanup) -> bcode::Result<()> {
    let started = bcode::CancellationToken::new();
    let released = bcode::CancellationToken::new();
    let context = Arc::new(std::sync::Mutex::new(None));
    let mut provider = bcode::InProcessModelProviderAdapter::new(PendingInProcess {
        started: started.clone(),
        released: released.clone(),
        context: context.clone(),
    });
    let timeout = Duration::from_secs(3);
    let agent = bcode::Agent::builder().timeout(timeout).build();
    let cancellation = bcode::CancellationToken::new();
    let generation_started = switchy::time::instant_now();
    let mut generation = Box::pin(agent.generate_text_with_provider_and_cancellation(
        &mut provider,
        "worker cleanup",
        cancellation.clone(),
    ));
    switchy::unsync::select! {
        result = &mut generation => panic!("pending provider completed: {result:?}"),
        () = started.cancelled() => {},
        () = switchy::unsync::time::sleep(Duration::from_secs(2)) => panic!("worker did not start"),
    }
    if matches!(mode, InProcessCleanup::Cancel) {
        cancellation.cancel();
        switchy::unsync::select! {
            result = &mut generation => assert!(matches!(result,
                Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Cancelled)))),
            () = switchy::unsync::time::sleep(Duration::from_secs(2)) => panic!("SDK cancellation did not complete"),
        }
    }
    if matches!(mode, InProcessCleanup::Deadline) {
        switchy::unsync::select! {
            result = &mut generation => assert!(matches!(result,
                Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Timeout { timeout: actual })) if actual == timeout)),
            () = switchy::unsync::time::sleep(Duration::from_secs(5)) => panic!("SDK deadline did not complete"),
        }
        assert!(
            switchy::time::instant_now().duration_since(generation_started) >= timeout,
            "in-process SDK deadline fired early"
        );
    }
    drop(generation);
    // Returned terminal outcomes must release the worker without adapter destruction.
    let provider = (!matches!(mode, InProcessCleanup::AdapterDrop)).then_some(provider);
    switchy::unsync::select! {
        () = released.cancelled() => {},
        () = switchy::unsync::time::sleep(Duration::from_secs(2)) => panic!("cleanup did not release worker future ({mode:?})"),
    }
    let context = context
        .lock()
        .expect("context lock")
        .clone()
        .expect("worker context");
    assert!(
        context.cancellation().is_cancelled(),
        "{mode:?}: cancellation visible"
    );
    assert_eq!(
        context.events().emit(ProviderTurnEvent::TextDelta {
            text: "late output".into(),
        }),
        Err(bcode::InProcessProviderEmitError::TurnFinished),
        "{mode:?}: terminal worker rejects late output"
    );
    if let Some(mut provider) = provider {
        let response = agent
            .generate_text_with_provider(&mut provider, "recover worker")
            .await?;
        assert_eq!(response.text, "recovered worker");
        assert_eq!(
            response.runtime.stop_reason,
            Some(bcode::StopReason::EndTurn)
        );
        assert_eq!(
            context.events().emit(ProviderTurnEvent::TextDelta {
                text: "stale after reuse".into(),
            }),
            Err(bcode::InProcessProviderEmitError::TurnFinished),
            "old context stays terminal after adapter reuse"
        );
    }
    Ok(())
}

async fn run() -> bcode::Result<()> {
    run_cache_lookup_panic().await?;
    run_cache_storage_panic().await?;
    run_cache_clock_errors().await?;
    for mode in [
        InProcessCleanup::AdapterDrop,
        InProcessCleanup::Cancel,
        InProcessCleanup::Deadline,
    ] {
        run_in_process_cleanup(mode).await?;
    }
    let mut in_process = bcode::InProcessModelProviderAdapter::new(InProcessEcho);
    let agent = bcode::Agent::builder().build();
    for prompt in ["in-process smoke", "in-process reuse"] {
        let response = agent
            .generate_text_with_provider(&mut in_process, prompt)
            .await?;
        assert_eq!(response.text, "in-process answer");
        assert_eq!(
            response.runtime.stop_reason,
            Some(bcode::StopReason::EndTurn)
        );
    }
    let provider = ScriptedProvider::new([ScriptedProviderTurn::new()
        .events([
            ProviderTurnEvent::TurnStarted,
            ProviderTurnEvent::Warning {
                message: "deterministic warning".to_string(),
            },
            ProviderTurnEvent::Usage {
                usage: TokenUsage {
                    input_tokens: Some(2),
                    output_tokens: Some(1),
                    total_tokens: Some(3),
                    ..TokenUsage::default()
                },
            },
        ])
        .delay(Duration::from_millis(1))
        .events([
            ProviderTurnEvent::TextDelta {
                text: "scripted answer".to_string(),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: bcode::StopReason::EndTurn,
            },
            // A malformed provider batch must not reopen or overwrite completion.
            ProviderTurnEvent::TextDelta {
                text: "late text must not appear".into(),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: bcode::StopReason::Cancelled,
            },
        ])]);
    let probe = provider.probe();
    let session_id = "00000000-0000-4000-8000-000000000001"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
        session_id,
        turn_id: "scripted-turn-0".to_string(),
    }])?;
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .provider_plugin("test-provider")
        .model("test-model")
        .build();

    let transcript = TextStreamRecorder::new(agent.stream_text_with_provider(provider, "hello"))
        .finish_up_to(100)
        .await;
    let response = transcript
        .assert_finished()
        .expect("coherent successful stream");
    assert_eq!(response.text, "scripted answer");
    let deltas: Vec<_> = transcript
        .events()
        .into_iter()
        .filter_map(|event| match event {
            bcode::AgentEvent::TextDelta(text) => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["scripted answer"]);
    probe
        .assert_requests(&[ScriptedRequestExpectation::new()
            .provider_plugin_id("test-provider")
            .model_id("test-model")])
        .expect("captured request");
    probe.assert_finish_count(1).expect("provider cleanup");
    run_response_cache_scenario().await?;
    run_rate_limit_scenarios().await?;
    run_retry_scenarios().await?;
    run_response_cache_failure().await?;
    run_response_cache_cancellation().await?;
    run_terminal_scenarios().await?;
    run_tool_scenarios(false).await?;
    run_tool_scenarios(true).await?;
    for capacity in [1, 2, 4, 32] {
        run_backpressure_scenario(
            std::num::NonZeroUsize::new(capacity).expect("positive capacity"),
        )
        .await?;
    }
    run_pending_tool_cancellation(ToolCancellation::Explicit).await?;
    run_pending_tool_cancellation(ToolCancellation::StreamDrop).await?;
    run_pending_tool_cancellation(ToolCancellation::RecorderBudget).await?;
    run_pending_tool_cancellation(ToolCancellation::Deadline).await?;
    for operation in [ProviderFailure::Event, ProviderFailure::Poll] {
        for partial_output in [false, true] {
            run_provider_error(operation, partial_output).await?;
        }
    }
    run_provider_error(ProviderFailure::Start, false).await?;
    run_pre_cancelled().await?;
    run_sibling_cancellation().await?;
    // Keep the known abandonment regression mandatory, but run it last so it
    // cannot mask tool, permission, backpressure, and provider-error coverage.
    run_response_cache_terminal_case(CacheTermination::Drop).await?;
    Ok(())
}

#[derive(Default)]
struct PanickingStorageCache {
    aborts: std::sync::atomic::AtomicUsize,
    puts: std::sync::atomic::AtomicUsize,
    response: std::sync::Mutex<Option<bcode::GenerateTextResponse>>,
}

impl bcode::ModelResponseCache for PanickingStorageCache {
    fn get(
        &self,
        _request: &bcode::AgentTurnRequest,
    ) -> bcode::Result<Option<bcode::GenerateTextResponse>> {
        Ok(self.response.lock().expect("fixture cache lock").clone())
    }

    fn put(
        &self,
        _request: &bcode::AgentTurnRequest,
        response: &bcode::GenerateTextResponse,
    ) -> bcode::Result<()> {
        if self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            panic!("fixture storage panic payload");
        }
        *self.response.lock().expect("fixture cache lock") = Some(response.clone());
        Ok(())
    }

    fn abort(&self, _request: &bcode::AgentTurnRequest) {
        self.aborts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

async fn run_cache_storage_panic() -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000021"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new([
        ProviderRequestIdentity {
            session_id,
            turn_id: "cache-storage-panic".into(),
        },
        ProviderRequestIdentity {
            session_id,
            turn_id: "cache-storage-recovery".into(),
        },
    ])?;
    let cache = Arc::new(PanickingStorageCache::default());
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .provider_plugin("test-provider")
        .model("test-model")
        .response_cache(cache.clone())
        .build();
    let mut provider = ScriptedProvider::new([
        ScriptedProviderTurn::complete_text("completed before storage"),
        ScriptedProviderTurn::complete_text("recovered after storage panic"),
    ]);
    let probe = provider.probe();
    let result = agent
        .generate_text_with_provider(&mut provider, "storage panic")
        .await;
    assert!(
        matches!(result, Err(bcode::BcodeError::Cache(message)) if message == "cache storage task failed")
    );
    assert_eq!(probe.requests().len(), 1);
    probe
        .assert_finish_count(1)
        .expect("provider finished before cache storage");
    probe
        .assert_cancellation_count(0)
        .expect("completed provider not cancelled");
    assert_eq!(cache.aborts.load(std::sync::atomic::Ordering::SeqCst), 1);
    for cached in [false, true] {
        let response = agent
            .generate_text_with_provider(&mut provider, "storage panic")
            .await?;
        assert_eq!(response.text, "recovered after storage panic");
        assert_recovery_cache_status(&response, cached);
    }
    assert_eq!(probe.requests().len(), 2);
    probe
        .assert_finish_count(2)
        .expect("both providers finished");
    probe
        .assert_cancellation_count(0)
        .expect("recovery not cancelled");
    assert_eq!(cache.puts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(cache.aborts.load(std::sync::atomic::Ordering::SeqCst), 1);
    Ok(())
}

async fn run_cache_clock_errors() -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000022"
        .parse()
        .expect("fixture ID");
    for lease_overflow in [false, true] {
        let identities =
            ScriptedRequestIdentities::new((0..2).map(|attempt| ProviderRequestIdentity {
                session_id,
                turn_id: format!("cache-overflow-{lease_overflow}-{attempt}"),
            }))?;
        let cache = Arc::new(
            bcode::InMemoryModelResponseCache::new(
                if lease_overflow {
                    Duration::from_secs(60)
                } else {
                    Duration::MAX
                },
                std::num::NonZeroUsize::new(1).expect("positive capacity"),
            )
            .with_single_flight_timeout(if lease_overflow {
                Duration::MAX
            } else {
                Duration::from_secs(30)
            }),
        );
        let agent = AgentBuilder::from_context(session_id, "/".into())
            .runtime(
                AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)),
            )
            .provider_plugin("test-provider")
            .model("test-model")
            .response_cache(cache.clone())
            .build();
        let mut provider = ScriptedProvider::new([
            ScriptedProviderTurn::complete_text("overflow fixture"),
            ScriptedProviderTurn::complete_text("overflow fixture"),
        ]);
        let probe = provider.probe();
        for attempt in 1..=2 {
            let result = agent
                .generate_text_with_provider(&mut provider, "overflow")
                .await;
            assert!(matches!(result, Err(bcode::BcodeError::Cache(_))));
            let expected = if lease_overflow { 0 } else { attempt };
            assert_eq!(probe.requests().len(), expected);
            probe
                .assert_finish_count(expected)
                .expect("provider finished before storage error");
            probe
                .assert_cancellation_count(0)
                .expect("completed provider not cancelled");
        }
        cache.invalidate_all()?;
    }
    Ok(())
}

struct PanickingLookupCache;

impl bcode::ModelResponseCache for PanickingLookupCache {
    fn get(
        &self,
        _request: &bcode::AgentTurnRequest,
    ) -> bcode::Result<Option<bcode::GenerateTextResponse>> {
        panic!("fixture lookup panic payload");
    }

    fn put(
        &self,
        _request: &bcode::AgentTurnRequest,
        _response: &bcode::GenerateTextResponse,
    ) -> bcode::Result<()> {
        panic!("failed lookup must not reach storage");
    }
}

async fn run_cache_lookup_panic() -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000020"
        .parse()
        .expect("fixture ID");
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .provider_plugin("test-provider")
        .model("test-model")
        .response_cache(Arc::new(PanickingLookupCache))
        .build();
    let mut provider = ScriptedProvider::new([ScriptedProviderTurn::complete_text("must not run")]);
    let probe = provider.probe();
    let result = agent
        .generate_text_with_provider(&mut provider, "lookup panic")
        .await;
    assert!(
        matches!(result, Err(bcode::BcodeError::Cache(message)) if message == "cache lookup task failed")
    );
    assert!(
        probe.requests().is_empty(),
        "failed lookup must not dispatch provider"
    );
    probe
        .assert_finish_count(0)
        .expect("no provider cleanup without start");
    Ok(())
}

fn assert_cancelled_cache_write(response: &bcode::GenerateTextResponse) -> bcode::Result<()> {
    use bcode::ModelResponseCache;

    let cache = bcode::InMemoryModelResponseCache::new(
        Duration::from_secs(60),
        std::num::NonZeroUsize::new(1).expect("positive capacity"),
    );
    let request = bcode::AgentTurnRequest::new("test-model", "cancelled cache write");
    request.cancellation.cancel();
    assert!(matches!(
        cache.put(&request, response),
        Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Cancelled))
    ));
    let fresh = bcode::AgentTurnRequest::new("test-model", "cancelled cache write");
    assert!(
        cache.get(&fresh)?.is_none(),
        "cancelled write must not populate cache"
    );
    cache.abort(&fresh);
    for lease_overflow in [false, true] {
        let cache = bcode::InMemoryModelResponseCache::new(
            if lease_overflow {
                Duration::from_secs(60)
            } else {
                Duration::MAX
            },
            std::num::NonZeroUsize::new(1).expect("positive capacity"),
        )
        .with_single_flight_timeout(if lease_overflow {
            Duration::MAX
        } else {
            Duration::from_secs(30)
        });
        let result = if lease_overflow {
            cache.get(&fresh).map(|_| ())
        } else {
            assert!(cache.get(&fresh)?.is_none());
            cache.put(&fresh, response)
        };
        assert!(
            matches!(result, Err(bcode::BcodeError::Cache(_))),
            "unrepresentable cache duration must fail"
        );
        cache.abort(&fresh);
        cache.invalidate_all()?;
    }
    Ok(())
}

async fn run_sibling_cancellation() -> bcode::Result<()> {
    let mut streams = Vec::new();
    for index in 0..2 {
        let session_id = format!("00000000-0000-4000-8000-00000000000{}", index + 8)
            .parse()
            .expect("fixture ID");
        let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
            session_id,
            turn_id: format!("sibling-{index}"),
        }])?;
        let agent = AgentBuilder::from_context(session_id, "/".into())
            .runtime(
                AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)),
            )
            .provider_plugin("test-provider")
            .model("test-model")
            .build();
        let turn = ScriptedProviderTurn::new().events([ProviderTurnEvent::TextDelta {
            text: format!("sibling-{index}"),
        }]);
        let provider = ScriptedProvider::new([turn.pending()]);
        let probe = provider.probe();
        let cancellation = bcode::CancellationToken::new();
        let recorder = TextStreamRecorder::new(agent.stream_text_with_provider_and_cancellation(
            provider,
            "concurrent streams",
            cancellation.clone(),
        ));
        streams.push((recorder, cancellation, probe));
    }
    // Both producers have been spawned before either consumer is drained.
    for (recorder, _, _) in &mut streams {
        assert_eq!(recorder.consume_up_to(2).await, 2);
    }
    streams[0].1.cancel();
    for (index, (recorder, cancellation, probe)) in streams.into_iter().enumerate() {
        // The second provider cannot finish by itself: its script remains pending.
        // Check after the first stream's terminal has been consumed, then explicitly
        // cancel the sibling rather than relying on a delay to establish overlap.
        if index == 1 {
            assert!(!cancellation.is_cancelled());
            probe
                .assert_cancellation_count(0)
                .expect("sibling was not cancelled");
            probe
                .assert_finish_count(0)
                .expect("sibling remains active");
            cancellation.cancel();
        }
        let transcript = recorder.finish_up_to(100).await;
        transcript
            .assert_cancelled()
            .expect("explicitly selected stream cancelled");
        transcript
            .assert_event_order(&[
                bcode::AgentEvent::TurnStarted,
                bcode::AgentEvent::TextDelta(format!("sibling-{index}")),
            ])
            .expect("each stream receives only its own ordered events");
        probe
            .assert_finish_count(1)
            .expect("each provider finishes once");
        probe
            .assert_cancellation_count(1)
            .expect("each explicit cancellation delivered once");
        assert_eq!(probe.requests().len(), 1);
        assert_eq!(
            probe.requests()[0].request.turn_id,
            format!("sibling-{index}")
        );
    }
    Ok(())
}

async fn run_response_cache_scenario() -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000008"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new((0..3).map(|index| ProviderRequestIdentity {
        session_id,
        turn_id: format!("cache-{index}"),
    }))?;
    let cache = Arc::new(bcode::InMemoryModelResponseCache::new(
        Duration::from_secs(1),
        std::num::NonZeroUsize::new(2).expect("positive capacity"),
    ));
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .provider_plugin("test-provider")
        .model("test-model")
        .response_cache(cache)
        .build();
    let mut provider = ScriptedProvider::new([
        ScriptedProviderTurn::complete_text("before expiry"),
        ScriptedProviderTurn::complete_text("after expiry"),
    ]);
    let probe = provider.probe();
    for populated in [false, true] {
        let cancellation = bcode::CancellationToken::new();
        cancellation.cancel();
        let cancelled = agent
            .generate_text_with_provider_and_cancellation(&mut provider, "cached", cancellation)
            .await;
        assert!(
            matches!(
                cancelled,
                Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Cancelled))
            ),
            "pre-cancelled request must not succeed"
        );
        assert_eq!(probe.requests().len(), usize::from(populated));
        probe
            .assert_finish_count(usize::from(populated))
            .expect("cancelled request starts no provider");
        let response = agent
            .generate_text_with_provider(&mut provider, "cached")
            .await?;
        assert_eq!(response.text, "before expiry");
        assert_cancelled_cache_write(&response)?;
        assert_eq!(probe.requests().len(), 1, "hit must not dispatch provider");
        probe
            .assert_finish_count(1)
            .expect("miss provider released");
    }
    let streaming_provider =
        ScriptedProvider::new([ScriptedProviderTurn::complete_text("stream bypasses cache")]);
    let streaming_probe = streaming_provider.probe();
    let transcript =
        TextStreamRecorder::new(agent.stream_text_with_provider(streaming_provider, "cached"))
            .finish_up_to(100)
            .await;
    assert_eq!(
        transcript.assert_finished().expect("stream succeeds").text,
        "stream bypasses cache"
    );
    assert_eq!(streaming_probe.requests().len(), 1);
    streaming_probe
        .assert_finish_count(1)
        .expect("stream provider released");
    streaming_probe
        .assert_cancellation_count(0)
        .expect("stream not cancelled");
    let response = agent
        .generate_text_with_provider(&mut provider, "cached")
        .await?;
    assert_eq!(
        response.text, "before expiry",
        "stream must not overwrite cache"
    );
    assert_eq!(probe.requests().len(), 1, "buffered entry remains cached");
    switchy::unsync::time::sleep(Duration::from_secs(2)).await;
    let response = agent
        .generate_text_with_provider(&mut provider, "cached")
        .await?;
    assert_eq!(response.text, "after expiry");
    assert_eq!(
        probe.requests().len(),
        2,
        "expired entry must dispatch provider"
    );
    probe
        .assert_finish_count(2)
        .expect("both providers released");
    probe.assert_cancellation_count(0).expect("no cancellation");
    Ok(())
}

async fn run_response_cache_cancellation() -> bcode::Result<()> {
    for mode in [CacheTermination::Cancel, CacheTermination::Deadline] {
        run_response_cache_terminal_case(mode).await?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum CacheTermination {
    Cancel,
    Deadline,
    Drop,
}

async fn run_response_cache_terminal_case(mode: CacheTermination) -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000019"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new((0..2).map(|index| ProviderRequestIdentity {
        session_id,
        turn_id: format!("cache-cancel-{index}"),
    }))?;
    let timeout = if matches!(mode, CacheTermination::Deadline) {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(120)
    };
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .timeout(timeout)
        .runtime(
            AgentRuntime::new()
                .with_poll_interval(Duration::from_mins(1))
                .with_provider_request_identity_source(Arc::new(identities)),
        )
        .provider_plugin("test-provider")
        .model("test-model")
        .response_cache(Arc::new(bcode::InMemoryModelResponseCache::new(
            Duration::from_secs(60),
            std::num::NonZeroUsize::new(2).expect("positive capacity"),
        )))
        .build();
    let mut provider = ScriptedProvider::new([
        ScriptedProviderTurn::new().pending(),
        ScriptedProviderTurn::complete_text("after cancellation"),
    ]);
    let probe = provider.probe();
    let cancellation = bcode::CancellationToken::new();
    let generation_started = switchy::time::instant_now();
    let mut generation = Box::pin(agent.generate_text_with_provider_and_cancellation(
        &mut provider,
        "recover",
        cancellation.clone(),
    ));
    loop {
        switchy::unsync::select! {
            result = &mut generation => panic!("pending provider completed: {result:?}"),
            () = switchy::unsync::task::yield_now() => {
                if !probe.requests().is_empty() { break; }
            }
        }
    }
    let termination_started = switchy::time::instant_now();
    if matches!(mode, CacheTermination::Drop) {
        drop(generation);
    } else if matches!(mode, CacheTermination::Deadline) {
        assert!(matches!(generation.await,
            Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Timeout { timeout: actual })) if actual == timeout
        ));
        assert!(
            switchy::time::instant_now().duration_since(generation_started) >= timeout,
            "product deadline must not fire early"
        );
    } else {
        cancellation.cancel();
        assert!(matches!(
            generation.await,
            Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Cancelled))
        ));
    }
    assert!(
        switchy::time::instant_now().duration_since(termination_started) < Duration::from_secs(30),
        "{mode:?}: termination must interrupt the one-minute poll interval"
    );
    probe
        .assert_finish_count(1)
        .unwrap_or_else(|error| panic!("{mode:?}: cancelled provider not released: {error:?}"));
    probe
        .assert_cancellation_count(1)
        .expect("provider cancelled once");
    let started = switchy::time::instant_now();
    for cached in [false, true] {
        let response = agent
            .generate_text_with_provider(&mut provider, "recover")
            .await?;
        assert_eq!(response.text, "after cancellation");
        assert_recovery_cache_status(&response, cached);
        assert_eq!(probe.requests().len(), 2, "recovery cached");
    }
    assert!(switchy::time::instant_now().duration_since(started) < Duration::from_secs(30));
    probe
        .assert_finish_count(2)
        .expect("all providers released");
    probe
        .assert_cancellation_count(1)
        .expect("recovery not cancelled");
    Ok(())
}

fn assert_recovery_cache_status(response: &bcode::GenerateTextResponse, cached: bool) {
    assert!(
        matches!(
            (&response.cache_status, cached),
            (bcode::ModelResponseCacheStatus::Stored { .. }, false)
                | (bcode::ModelResponseCacheStatus::Hit { .. }, true)
        ),
        "unexpected recovery cache provenance: {:?}",
        response.cache_status
    );
}

async fn run_response_cache_failure() -> bcode::Result<()> {
    for started in [false, true] {
        run_response_cache_failure_case(started).await?;
    }
    Ok(())
}

async fn run_response_cache_failure_case(started: bool) -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000009"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new((0..2).map(|index| ProviderRequestIdentity {
        session_id,
        turn_id: format!("cache-failure-{index}"),
    }))?;
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .provider_plugin("test-provider")
        .model("test-model")
        .response_cache(Arc::new(bcode::InMemoryModelResponseCache::new(
            Duration::from_secs(60),
            std::num::NonZeroUsize::new(2).expect("positive capacity"),
        )))
        .build();
    let error = bcode::ProviderError {
        code: "cache_fixture_failure".into(),
        category: bcode::ProviderErrorCategory::ProviderInternal,
        message: "cache fixture failure".into(),
        retryable: false,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    };
    let failed_turn = if started {
        ScriptedProviderTurn::new().poll_error(error)
    } else {
        ScriptedProviderTurn::start_error(error)
    };
    let mut provider = ScriptedProvider::new([
        failed_turn,
        ScriptedProviderTurn::complete_text("recovered cache miss"),
    ]);
    let probe = provider.probe();
    let failure = agent
        .generate_text_with_provider(&mut provider, "recover")
        .await
        .expect_err("scripted provider must fail");
    assert!(
        matches!(
            failure,
            bcode::BcodeError::Runtime(bcode::RuntimeError::Provider { ref code, .. })
                if code == "cache_fixture_failure"
        ),
        "unexpected failure: {failure:?}"
    );
    assert_eq!(probe.requests().len(), 1);
    probe
        .assert_finish_count(usize::from(started))
        .expect("only a started provider has a handle to release");
    // A leaked miss lease must not be allowed to expire and mask missing abort cleanup.
    let recovery_started = switchy::time::instant_now();
    for cached in [false, true] {
        let response = agent
            .generate_text_with_provider(&mut provider, "recover")
            .await?;
        assert_eq!(response.text, "recovered cache miss");
        assert_recovery_cache_status(&response, cached);
        assert_eq!(probe.requests().len(), 2, "recovery is cached");
    }
    assert!(
        switchy::time::instant_now().duration_since(recovery_started) < Duration::from_secs(30)
    );
    probe
        .assert_finish_count(1 + usize::from(started))
        .expect("failed and recovered providers released");
    probe
        .assert_cancellation_count(usize::from(started))
        .expect("only failed started provider cancelled");
    Ok(())
}

struct FixtureRateLimiter(std::result::Result<bcode::ApplicationRateLimitDecision, String>);

impl bcode::ApplicationRateLimiter for FixtureRateLimiter {
    fn check(
        &self,
        request: &bcode::AgentTurnRequest,
    ) -> std::result::Result<bcode::ApplicationRateLimitDecision, String> {
        assert_eq!(request.model_id, "test-model");
        self.0.clone()
    }
}

async fn run_rate_limit_scenarios() -> bcode::Result<()> {
    for outcome in 0..3 {
        let decision = match outcome {
            0 => Ok(bcode::ApplicationRateLimitDecision::Allow),
            1 => Ok(bcode::ApplicationRateLimitDecision::Deny {
                reason: "fixture quota".into(),
                retry_at_unix: Some(1_700_000_001),
            }),
            _ => Err("fixture unavailable".into()),
        };
        let session_id = "00000000-0000-4000-8000-000000000010"
            .parse()
            .expect("fixture ID");
        let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
            session_id,
            turn_id: format!("rate-limit-{outcome}"),
        }])?;
        let agent = AgentBuilder::from_context(session_id, "/".into())
            .runtime(
                AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)),
            )
            .provider_plugin("test-provider")
            .model("test-model")
            .middleware_layer(bcode::RateLimitMiddleware::new(
                "fixture",
                Arc::new(FixtureRateLimiter(decision)),
            ))
            .build();
        let mut provider = ScriptedProvider::new([ScriptedProviderTurn::complete_text("admitted")]);
        let probe = provider.probe();
        let result = agent
            .generate_text_with_provider(&mut provider, "limited")
            .await;
        match outcome {
            0 => assert_eq!(result?.text, "admitted"),
            1 => assert!(
                matches!(result, Err(bcode::BcodeError::RateLimited { limiter_id, reason, retry_at_unix: Some(1_700_000_001) }) if limiter_id == "fixture" && reason == "fixture quota")
            ),
            _ => assert!(
                matches!(result, Err(bcode::BcodeError::RateLimiter { limiter_id, message }) if limiter_id == "fixture" && message == "fixture unavailable")
            ),
        }
        let admitted = usize::from(outcome == 0);
        assert_eq!(
            probe.requests().len(),
            admitted,
            "only admitted requests dispatch"
        );
        probe
            .assert_finish_count(admitted)
            .expect("only admitted provider finishes");
        probe
            .assert_cancellation_count(0)
            .expect("no provider cancellation");
    }
    Ok(())
}

async fn run_retry_scenarios() -> bcode::Result<()> {
    for (retryable, retries, exhausted, hint_ms) in [
        (true, 1, false, None),
        (true, 0, false, None),
        (false, 1, false, None),
        (true, 1, true, None),
        (true, 1, false, Some(50)),
    ] {
        let session_id = "00000000-0000-4000-8000-000000000011"
            .parse()
            .expect("fixture ID");
        let identities =
            ScriptedRequestIdentities::new((0..2).map(|index| ProviderRequestIdentity {
                session_id,
                turn_id: format!("retry-{retryable}-{retries}-{index}"),
            }))?;
        let agent = AgentBuilder::from_context(session_id, "/".into())
            .runtime(
                AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)),
            )
            .provider_plugin("test-provider")
            .model("test-model")
            .retry_policy(
                bcode::RetryPolicy::new(retries, Duration::from_millis(10))
                    .with_max_delay(Duration::from_millis(100)),
            )
            .build();
        let error = bcode::ProviderError {
            code: "retry_fixture".into(),
            category: bcode::ProviderErrorCategory::ProviderInternal,
            message: "retry fixture".into(),
            retryable,
            provider_message: None,
            failure: None,
            request_id: None,
            diagnostic_context: Box::default(),
            sources: Box::default(),
            retry: hint_ms.map(|delay| {
                Box::new(bcode::ProviderRetryHint {
                    retry_after_ms: Some(delay),
                    retry_at_unix: None,
                    source: Some("fixture".into()),
                })
            }),
        };
        let mut turns = vec![ScriptedProviderTurn::start_error(error.clone())];
        if exhausted {
            let mut final_error = error;
            final_error.code = "retry_exhausted".into();
            turns.push(ScriptedProviderTurn::start_error(final_error));
        }
        turns.push(ScriptedProviderTurn::complete_text("retry recovered"));
        let mut provider = ScriptedProvider::new(turns);
        let probe = provider.probe();
        let started = switchy::time::instant_now();
        let result = agent
            .generate_text_with_provider(&mut provider, "retry")
            .await;
        let retried = retryable && retries > 0;
        let recovered = retried && !exhausted;
        if retried {
            assert!(
                switchy::time::instant_now().duration_since(started)
                    >= Duration::from_millis(hint_ms.unwrap_or(10))
            );
        }
        if recovered {
            assert_eq!(result?.text, "retry recovered");
        } else {
            let expected_code = if exhausted {
                "retry_exhausted"
            } else {
                "retry_fixture"
            };
            assert!(
                matches!(result, Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Provider { code, .. })) if code == expected_code)
            );
        }
        assert_eq!(probe.requests().len(), 1 + usize::from(retried));
        probe
            .assert_finish_count(usize::from(recovered))
            .expect("only successful start is finished");
        probe.assert_cancellation_count(0).expect("no cancellation");
    }
    Ok(())
}

async fn run_pre_cancelled() -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000007"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
        session_id,
        turn_id: "pre-cancelled-0".into(),
    }])?;
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .provider_plugin("test-provider")
        .model("test-model")
        .build();
    let provider = ScriptedProvider::new([ScriptedProviderTurn::complete_text("must not start")]);
    let probe = provider.probe();
    let cancellation = bcode::CancellationToken::new();
    cancellation.cancel();
    let transcript = TextStreamRecorder::new(agent.stream_text_with_provider_and_cancellation(
        provider,
        "already cancelled",
        cancellation,
    ))
    .finish_up_to(100)
    .await;
    transcript
        .assert_cancelled()
        .expect("coherent pre-start cancellation");
    probe
        .assert_requests(&[])
        .expect("no provider request after cancellation");
    probe
        .assert_finish_count(0)
        .expect("no provider round to finish");
    probe
        .assert_cancellation_count(0)
        .expect("no provider round to cancel");
    Ok(())
}

#[derive(Clone, Copy)]
enum ProviderFailure {
    Start,
    Poll,
    Event,
}

async fn run_provider_error(operation: ProviderFailure, partial_output: bool) -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000006"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
        session_id,
        turn_id: "provider-error-0".into(),
    }])?;
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .provider_plugin("test-provider")
        .model("test-model")
        .retry_policy(bcode::RetryPolicy::new(
            u32::from(partial_output),
            Duration::from_millis(10),
        ))
        .build();
    let error = bcode::ProviderError {
        code: "fixture_failure".into(),
        category: bcode::ProviderErrorCategory::ProviderInternal,
        message: "fixture failure".into(),
        retryable: partial_output,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    };
    let deltas = partial_output.then(|| ProviderTurnEvent::TextDelta {
        text: "partial".into(),
    });
    let turn = match operation {
        ProviderFailure::Start => ScriptedProviderTurn::start_error(error),
        ProviderFailure::Poll => ScriptedProviderTurn::new().events(deltas).poll_error(error),
        ProviderFailure::Event => ScriptedProviderTurn::new().events(
            deltas
                .into_iter()
                .chain([ProviderTurnEvent::Error { error }]),
        ),
    };
    let provider = ScriptedProvider::new([
        turn,
        ScriptedProviderTurn::complete_text("must not retry visible output"),
    ]);
    let probe = provider.probe();
    let transcript =
        TextStreamRecorder::new(agent.stream_text_with_provider(provider, "fail after text"))
            .finish_up_to(100)
            .await;
    let error = transcript
        .assert_runtime_error()
        .expect("coherent error terminal");
    let source = if partial_output {
        let bcode::RuntimeError::ProviderAfterOutput(source) = error else {
            panic!("expected provider failure after output, got {error:?}");
        };
        source.as_ref()
    } else {
        error
    };
    assert!(
        matches!(source, bcode::RuntimeError::Provider { code, .. } if code == "fixture_failure")
    );
    let deltas: Vec<_> = transcript
        .events()
        .into_iter()
        .filter_map(|event| match event {
            bcode::AgentEvent::TextDelta(text) => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas,
        if partial_output {
            vec!["partial"]
        } else {
            vec![]
        }
    );
    let started = usize::from(!matches!(operation, ProviderFailure::Start));
    probe
        .assert_finish_count(started)
        .expect("finish only a started turn");
    probe
        .assert_cancellation_count(started)
        .expect("cancel only a started turn");
    probe
        .assert_requests(&[ScriptedRequestExpectation::new()
            .provider_plugin_id("test-provider")
            .model_id("test-model")])
        .expect("no extra provider request");
    Ok(())
}

enum ToolCancellation {
    Explicit,
    StreamDrop,
    RecorderBudget,
    Deadline,
}

async fn run_pending_tool_cancellation(mode: ToolCancellation) -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000005"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
        session_id,
        turn_id: "pending-tool-0".into(),
    }])?;
    let tool = ScriptedTool::new([ScriptedToolOutcome::PendingUntilCancelled]);
    let probe = tool.probe();
    let permissions = ScriptedPermissionPolicy::new([bcode::PermissionDecision::Allow]);
    let permission_probe = permissions.clone();
    let builder = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .provider_plugin("test-provider")
        .model("test-model");
    // A deadline must not rescue broken drop-cancellation in the other scenarios.
    let builder = if matches!(mode, ToolCancellation::Deadline) {
        builder.timeout(Duration::from_secs(2))
    } else {
        builder
    };
    let agent = tool
        .register(
            builder,
            bcode::ToolDefinition {
                name: "scripted".into(),
                description: "Fixture tool".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
        )
        .custom_permission_policy(permissions)
        .build();
    let provider = ScriptedProvider::new([ScriptedProviderTurn::new().events([
        ProviderTurnEvent::ToolCallFinished {
            call: bcode::ToolCall {
                id: "pending-call".into(),
                name: "scripted".into(),
                arguments: serde_json::json!({"input": 1}),
            },
        },
        ProviderTurnEvent::TurnFinished {
            stop_reason: bcode::StopReason::ToolCall,
        },
    ])]);
    let provider_probe = provider.probe();
    let cancellation = bcode::CancellationToken::new();
    let stream = agent.stream_text_with_provider_and_cancellation(
        provider,
        "cancel active tool",
        cancellation.clone(),
    );
    // Observe invocation admission before cancellation; a fixed yield count cannot
    // establish that the permission and tool path was actually reached.
    for _ in 0..1_000 {
        if probe.invocation_count() == 1 {
            break;
        }
        switchy::unsync::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(probe.invocation_count(), 1);
    assert_eq!(probe.active_invocation_count(), 1);
    assert_eq!(permission_probe.requests().len(), 1);
    match mode {
        ToolCancellation::Deadline => {
            let transcript = TextStreamRecorder::new(stream).finish_up_to(100).await;
            assert!(matches!(
                transcript
                    .assert_runtime_error()
                    .expect("coherent tool deadline terminal"),
                bcode::RuntimeError::Timeout { .. }
            ));
        }
        ToolCancellation::StreamDrop => drop(stream),
        ToolCancellation::RecorderBudget => {
            let transcript = TextStreamRecorder::new(stream).finish_up_to(1).await;
            assert_eq!(transcript.items().len(), 1);
            assert!(!transcript.is_exhausted());
            assert!(transcript.assert_finished().is_err());
        }
        ToolCancellation::Explicit => {
            cancellation.cancel();
            let transcript = TextStreamRecorder::new(stream).finish_up_to(100).await;
            transcript
                .assert_cancelled()
                .expect("active tool cancellation reaches coherent terminal");
        }
    }
    for _ in 0..1_000 {
        if probe.active_invocation_count() == 0 {
            break;
        }
        switchy::unsync::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(probe.active_invocation_count(), 0, "tool future released");
    provider_probe
        .assert_requests(&[ScriptedRequestExpectation::new()
            .provider_plugin_id("test-provider")
            .model_id("test-model")])
        .expect("no provider continuation after tool cancellation");
    provider_probe
        .assert_finish_count(1)
        .expect("initial provider round finished once");
    Ok(())
}

async fn run_backpressure_scenario(capacity: std::num::NonZeroUsize) -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000004"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
        session_id,
        turn_id: "backpressure-0".into(),
    }])?;
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(
            AgentRuntime::new()
                .with_provider_request_identity_source(Arc::new(identities))
                .with_stream_buffer_capacity(capacity),
        )
        .provider_plugin("test-provider")
        .model("test-model")
        .build();
    let provider = ScriptedProvider::new([ScriptedProviderTurn::new()
        .events((0..8).map(|index| ProviderTurnEvent::TextDelta {
            text: index.to_string(),
        }))
        .events([ProviderTurnEvent::TurnFinished {
            stop_reason: bcode::StopReason::EndTurn,
        }])]);
    let probe = provider.probe();
    let stream = agent.stream_text_with_provider(provider, "buffered burst");
    // Wait for observable provider cleanup, not an assumed number of scheduler
    // yields. The consumer remains detached while the bounded producer fills.
    for _ in 0..1_000 {
        if probe.assert_finish_count(1).is_ok() {
            break;
        }
        switchy::unsync::time::sleep(Duration::from_millis(1)).await;
    }
    probe
        .assert_finish_count(1)
        .expect("producer finished within fixture budget");
    let transcript = TextStreamRecorder::new(stream).finish_up_to(100).await;
    if capacity.get() == 32 {
        let response = transcript.assert_finished().expect("burst fits buffer");
        assert_eq!(response.text, "01234567");
        let deltas: Vec<_> = transcript
            .events()
            .into_iter()
            .filter_map(|event| match event {
                bcode::AgentEvent::TextDelta(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(
            deltas,
            (0..8).map(|index| index.to_string()).collect::<Vec<_>>()
        );
        probe
            .assert_cancellation_count(0)
            .expect("successful burst is not cancelled");
    } else {
        transcript
            .assert_backpressure_overflow(capacity.get())
            .expect("overflow is typed and terminal, not silent event loss");
        probe
            .assert_cancellation_count(1)
            .expect("overflow cancels provider work");
    }
    probe
        .assert_requests(&[ScriptedRequestExpectation::new()
            .provider_plugin_id("test-provider")
            .model_id("test-model")])
        .expect("overflow does not start another request");
    Ok(())
}

async fn run_tool_scenarios(retry: bool) -> bcode::Result<()> {
    for (decision, outcome, expected_error, expected) in [
        (
            Some(bcode::PermissionDecision::Allow),
            ScriptedToolOutcome::text("tool output"),
            false,
            "tool output",
        ),
        (
            Some(bcode::PermissionDecision::Deny("fixture denial".into())),
            ScriptedToolOutcome::text("must not execute"),
            true,
            "tool execution denied: fixture denial",
        ),
        (
            Some(bcode::PermissionDecision::Allow),
            ScriptedToolOutcome::text("delayed output").after(Duration::from_millis(3)),
            false,
            "delayed output",
        ),
        (
            Some(bcode::PermissionDecision::Allow),
            ScriptedToolOutcome::Error("fixture failure".into()),
            true,
            "fixture failure",
        ),
        (
            None,
            ScriptedToolOutcome::text("exhaustion must not execute"),
            true,
            "tool execution denied: scripted permission decisions exhausted",
        ),
    ] {
        let allowed = matches!(decision, Some(bcode::PermissionDecision::Allow));
        let session_id = "00000000-0000-4000-8000-000000000003"
            .parse()
            .expect("fixture ID");
        let identities =
            ScriptedRequestIdentities::new((0..3).map(|index| ProviderRequestIdentity {
                session_id,
                turn_id: format!("tool-{allowed}-{index}"),
            }))?;
        let permissions = ScriptedPermissionPolicy::new(decision);
        let permission_probe = permissions.clone();
        let tool = ScriptedTool::new([outcome]);
        let tool_probe = tool.probe();
        let agent = tool
            .register(
                AgentBuilder::from_context(session_id, "/".into())
                    .runtime(
                        AgentRuntime::new()
                            .with_provider_request_identity_source(Arc::new(identities)),
                    )
                    .provider_plugin("test-provider")
                    .model("test-model")
                    .retry_policy(bcode::RetryPolicy::new(1, Duration::from_millis(10))),
                bcode::ToolDefinition {
                    name: "scripted".into(),
                    description: "Fixture tool".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            )
            .custom_permission_policy(permissions)
            .build();
        let mut turns = vec![ScriptedProviderTurn::new().events([
            ProviderTurnEvent::ToolCallFinished {
                call: bcode::ToolCall {
                    id: "call-1".into(),
                    name: "scripted".into(),
                    arguments: serde_json::json!({"input": 1}),
                },
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: bcode::StopReason::ToolCall,
            },
        ])];
        if retry {
            turns.push(ScriptedProviderTurn::start_error(bcode::ProviderError {
                code: "continuation_retry".into(),
                category: bcode::ProviderErrorCategory::ProviderInternal,
                message: "fixture continuation failure".into(),
                retryable: true,
                provider_message: None,
                failure: None,
                request_id: None,
                diagnostic_context: Box::default(),
                sources: Box::default(),
                retry: None,
            }));
        }
        turns.push(ScriptedProviderTurn::complete_text("after tool"));
        let mut provider = ScriptedProvider::new(turns);
        let probe = provider.probe();
        let response = agent.run(&mut provider, "use tool").await?;
        assert_eq!(response.text, "after tool");
        assert_eq!(tool_probe.invocation_count(), usize::from(allowed));
        assert_eq!(
            tool_probe.active_invocation_count(),
            0,
            "completed tool invocation released"
        );
        let requests = permission_probe.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].context.session_id, session_id);
        assert!(response.steps.iter().any(|step| matches!(
            step,
            bcode::GenerationStep::ToolResult { result, .. }
                if result.is_error == expected_error &&
                    (if allowed && expected_error { result.output.contains(expected) }
                     else { result.output == expected })
        )));
        if allowed {
            assert_eq!(tool_probe.invocations()[0].request.arguments["input"], 1);
        }
        let provider_requests = probe.requests();
        assert_eq!(
            provider_requests.len(),
            2 + usize::from(retry),
            "exact continuation attempts"
        );
        if retry {
            assert_eq!(
                provider_requests[1].request.messages, provider_requests[2].request.messages,
                "retry preserves the committed tool result"
            );
        }
        let results: Vec<_> = provider_requests[1]
            .request
            .messages
            .iter()
            .filter(|message| message.role == bcode::MessageRole::Tool)
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                bcode::ModelContentBlock::ToolResult { result } => Some(result),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 1, "one tool result delivered to provider");
        assert_eq!(results[0].is_error, expected_error);
        assert!(
            response.steps.iter().any(|step| matches!(
                step, bcode::GenerationStep::ToolResult { result, .. } if result == results[0]
            )),
            "continuation must carry the actual runtime result"
        );
        probe
            .assert_finish_count(2)
            .expect("both provider requests finished");
        probe
            .assert_cancellation_count(0)
            .expect("successful continuation does not cancel providers");
    }
    Ok(())
}

async fn run_terminal_scenarios() -> bcode::Result<()> {
    for cancelled in [true, false] {
        let session_id = "00000000-0000-4000-8000-000000000002"
            .parse()
            .expect("fixture ID");
        let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
            session_id,
            turn_id: format!("terminal-{cancelled}"),
        }])?;
        let timeout = Duration::from_millis(100);
        let agent = AgentBuilder::from_context(session_id, "/".into())
            .runtime(
                AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)),
            )
            .provider_plugin("test-provider")
            .model("test-model")
            .timeout(timeout)
            .build();
        let provider = ScriptedProvider::new([ScriptedProviderTurn::new()
            .events([ProviderTurnEvent::TextDelta {
                text: "before terminal".into(),
            }])
            .pending()]);
        let probe = provider.probe();
        let cancellation = bcode::CancellationToken::new();
        let stream = agent.stream_text_with_provider_and_cancellation(
            provider,
            "hello",
            cancellation.clone(),
        );
        let mut recorder = TextStreamRecorder::new(stream);
        // Consume the runtime start and provider delta before cancelling, so the
        // scenario exercises active provider work rather than pre-start rejection.
        assert_eq!(recorder.consume_up_to(2).await, 2);
        assert!(matches!(
            recorder.items(),
            [bcode::TextStreamItem::Event(bcode::AgentEvent::TurnStarted),
             bcode::TextStreamItem::Event(bcode::AgentEvent::TextDelta(text))]
                if text == "before terminal"
        ));
        if cancelled {
            cancellation.cancel();
        }
        let transcript = recorder.finish_up_to(100).await;
        transcript
            .assert_terminal_coherence()
            .expect("one stable terminal followed by stream exhaustion");
        let expected_events = [
            bcode::AgentEvent::TurnStarted,
            bcode::AgentEvent::TextDelta("before terminal".into()),
        ];
        // This SDK surface reports cancellation through the typed terminal error,
        // not an additional AgentEvent::Cancelled notification.
        transcript
            .assert_event_order(&expected_events)
            .expect("exact terminal event sequence");
        if cancelled {
            transcript.assert_cancelled().expect("typed cancellation");
        } else {
            assert!(matches!(
                transcript.assert_runtime_error().expect("typed timeout"),
                bcode::RuntimeError::Timeout { timeout: actual } if *actual == timeout
            ));
        }
        probe
            .assert_cancellation_count(1)
            .expect("provider cancelled");
        probe
            .assert_finish_count(1)
            .expect("provider finished once");
    }
    Ok(())
}
