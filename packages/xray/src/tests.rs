//! Engine-owned behavioral coverage. Intentionally not executed during interface convergence.
use super::*;

fn request(session_id: &str, deep_capture: bool) -> XrayObserveRequest {
    XrayObserveRequest {
        schema_version: XRAY_SCHEMA_VERSION,
        session_id: session_id.into(),
        deep_capture,
    }
}

fn correlation(session_id: &str) -> XrayCorrelation {
    XrayCorrelation {
        session_id: session_id.into(),
        ..XrayCorrelation::default()
    }
}

fn event(session_id: &str, name: &str, sensitive: bool) -> XrayEvent {
    XrayEvent {
        domain: "test".into(),
        name: name.into(),
        correlation: correlation(session_id),
        data: json!({"value": 42}),
        sensitive,
        capture: None,
    }
}

#[tokio::test]
async fn ready_is_first_even_if_producer_fills_queue_before_first_receive() {
    let hub = XrayHub::new(XrayConfig {
        queue_capacity: 1,
        ..XrayConfig::default()
    });
    let mut observation = hub.observe(request("s", false)).unwrap();
    hub.emit(event("s", "one", false));
    hub.emit(event("s", "two", false));
    assert_eq!(observation.recv().await.unwrap().event.name, "ready");
    let first = observation.recv().await.unwrap();
    assert_eq!(first.sequence, 2);
    assert_eq!(first.event.name, "one");
    let gap = observation.recv().await.unwrap();
    assert_eq!(gap.event.name, "gap");
    assert_eq!(gap.event.data["first_sequence"], 3);
    assert_eq!(gap.event.data["last_sequence"], 3);
    let finalized = observation.finalize().unwrap();
    assert_eq!(finalized.dropped_events, 1);
    assert_eq!(finalized.final_sequence, 4);
    assert_eq!(observation.recv().await.unwrap().event.name, "finalized");
    assert!(observation.recv().await.is_none());
}

#[tokio::test]
async fn finalization_is_idempotent_and_does_not_close_other_observers() {
    let hub = XrayHub::default();
    let mut first = hub.observe(request("s", true)).unwrap();
    let mut second = hub.observe(request("s", false)).unwrap();
    hub.emit(event("s", "before", false));
    let cutoff = first.finalize().unwrap();
    assert_eq!(first.finalize().unwrap(), cutoff);
    assert!(!hub.deep_capture_active("s"));
    assert!(hub.capture_active("s"));
    hub.emit(event("s", "after", false));
    assert_eq!(first.recv().await.unwrap().event.name, "ready");
    assert_eq!(first.recv().await.unwrap().event.name, "before");
    assert_eq!(first.recv().await.unwrap().event.name, "finalized");
    assert!(first.recv().await.is_none());
    assert_eq!(second.recv().await.unwrap().event.name, "ready");
    assert_eq!(second.recv().await.unwrap().event.name, "before");
    assert_eq!(second.recv().await.unwrap().event.name, "after");
}

#[tokio::test]
async fn sensitivity_and_session_scope_are_independent_for_each_observer() {
    let hub = XrayHub::default();
    let mut shallow = hub.observe(request("s", false)).unwrap();
    let mut deep = hub.observe(request("s", true)).unwrap();
    let mut foreign = hub.observe(request("foreign", true)).unwrap();
    hub.emit(event("s", "secret_diagnostic", true));
    hub.emit(event("s", "public", false));
    shallow.finalize().unwrap();
    deep.finalize().unwrap();
    foreign.finalize().unwrap();
    for observer in [&mut shallow, &mut deep, &mut foreign] {
        assert_eq!(observer.recv().await.unwrap().event.name, "ready");
    }
    assert_eq!(shallow.recv().await.unwrap().event.name, "public");
    assert_eq!(shallow.recv().await.unwrap().event.name, "finalized");
    assert_eq!(deep.recv().await.unwrap().event.name, "secret_diagnostic");
    assert_eq!(deep.recv().await.unwrap().event.name, "public");
    assert_eq!(foreign.recv().await.unwrap().event.name, "finalized");
}

#[tokio::test]
async fn oversized_events_are_omitted_explicitly_without_leaking_data() {
    let hub = XrayHub::new(XrayConfig {
        max_event_bytes: 1024,
        ..XrayConfig::default()
    });
    let mut observation = hub.observe(request("s", true)).unwrap();
    let mut oversized = event("s", "oversized", true);
    oversized.data = json!({"sensitive_fixture": "x".repeat(8192)});
    hub.emit(oversized);
    observation.recv().await.unwrap();
    let omitted = observation.recv().await.unwrap();
    assert_eq!(omitted.event.name, "event_omitted");
    assert_eq!(omitted.event.data, json!({"reason": "event_too_large"}));
    assert!(omitted.event.sensitive);
}

#[tokio::test]
async fn reserved_controls_and_deeply_nested_payloads_cannot_forge_delivery() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", false)).unwrap();
    let mut forged = event("s", "finalized", false);
    forged.domain = "xray".into();
    hub.emit(forged);
    let mut nested = event("s", "nested", false);
    let mut data = json!(null);
    for _ in 0..80 {
        data = json!([data]);
    }
    nested.data = data;
    hub.emit(nested);
    assert_eq!(observation.recv().await.unwrap().event.name, "ready");
    assert_eq!(
        observation.recv().await.unwrap().event.name,
        "event_omitted"
    );
    assert_eq!(
        observation.recv().await.unwrap().event.name,
        "event_omitted"
    );
    hub.emit(event("s", "still_active", false));
    assert_eq!(observation.recv().await.unwrap().event.name, "still_active");
}

#[test]
fn loss_attribution_preserves_aggregate_and_distinguishes_all_five_causes() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", true)).unwrap();
    let guard = hub.inner.state.lock().unwrap();
    assert_eq!(hub.observation_active(), Err(XrayError::Busy));
    assert_eq!(hub.session_observation_active("s"), Err(XrayError::Busy));
    hub.emit(event("s", "blocked", false));
    assert_eq!(
        hub.capture_payload(&correlation("s"), "text/plain", b"test"),
        Err(XrayError::Busy)
    );
    // Deterministically pause just the commit phase rather than race the hasher.
    assert!(matches!(
        hub.producer_state(LossCause::PayloadCommit),
        Err(XrayError::Busy)
    ));
    drop(guard);
    let activity = hub.inner.activity.write().unwrap();
    assert!(!hub.correlation_active(&correlation("s"), true));
    drop(activity);
    hub.report_external_loss(3);
    let causes = &hub.inner.loss_causes;
    assert_eq!(causes.emit_state.load(Ordering::Relaxed), 1);
    assert_eq!(causes.payload_preflight.load(Ordering::Relaxed), 1);
    assert_eq!(causes.payload_commit.load(Ordering::Relaxed), 1);
    assert_eq!(causes.activity_hint.load(Ordering::Relaxed), 1);
    assert_eq!(causes.external.load(Ordering::Relaxed), 3);
    let cutoff = observation.finalize().unwrap();
    assert_eq!(cutoff.hub_contention_drops, 7);
    assert_eq!(cutoff.dropped_events, 0);
    hub.report_external_loss(1);
    assert_eq!(observation.finalize().unwrap(), cutoff);
    assert_eq!(causes.external.load(Ordering::Relaxed), 4);
}

#[test]
fn busy_lifecycle_queries_do_not_report_lost_producer_work() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", true)).unwrap();
    let guard = hub.inner.state.lock().unwrap();
    assert_eq!(hub.observation_active(), Err(XrayError::Busy));
    assert_eq!(hub.session_observation_active("s"), Err(XrayError::Busy));
    assert_eq!(hub.inner.contention.load(Ordering::Relaxed), 0);
    // The same paused registry still rejects and counts actual producer work.
    hub.emit(event("s", "contended", false));
    assert_eq!(
        hub.capture_payload(&correlation("s"), "text/plain", b"test"),
        Err(XrayError::Busy)
    );
    drop(guard);
    assert!(hub.observation_active().unwrap());
    assert!(hub.session_observation_active("s").unwrap());
    assert_eq!(observation.finalize().unwrap().hub_contention_drops, 2);
}

struct RegistryWakeProbe {
    hub: XrayHub,
    wakes: AtomicU64,
    locked_wakes: AtomicU64,
}

impl std::task::Wake for RegistryWakeProbe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::Relaxed);
        if self.hub.inner.state.try_lock().is_err() {
            self.locked_wakes.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[tokio::test]
async fn delivery_and_cutoff_wake_receivers_only_after_releasing_registry() {
    use std::future::Future as _;
    use std::task::{Context, Poll, Waker};

    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", true)).unwrap();
    assert_eq!(observation.recv().await.unwrap().sequence, 1);
    let probe = Arc::new(RegistryWakeProbe {
        hub: hub.clone(),
        wakes: AtomicU64::new(0),
        locked_wakes: AtomicU64::new(0),
    });
    let waker = Waker::from(Arc::clone(&probe));
    let mut context = Context::from_waker(&waker);
    let notify = Arc::clone(&observation.notify);
    let mut notified = std::pin::pin!(notify.notified());
    assert!(matches!(
        notified.as_mut().poll(&mut context),
        Poll::Pending
    ));
    hub.emit(event("s", "delivered", false));
    assert_eq!(probe.wakes.load(Ordering::Relaxed), 1);
    assert_eq!(probe.locked_wakes.load(Ordering::Relaxed), 0);
    assert!(matches!(
        notified.as_mut().poll(&mut context),
        Poll::Ready(())
    ));
    assert_eq!(observation.recv().await.unwrap().sequence, 2);
    let mut cutoff_notice = std::pin::pin!(notify.notified());
    assert!(matches!(
        cutoff_notice.as_mut().poll(&mut context),
        Poll::Pending
    ));
    let cutoff = observation.finalize().unwrap();
    assert_eq!(probe.wakes.load(Ordering::Relaxed), 2);
    assert_eq!(probe.locked_wakes.load(Ordering::Relaxed), 0);
    assert_eq!(cutoff.hub_contention_drops, 0);
    let terminal = observation.recv().await.unwrap();
    assert_eq!(terminal.sequence, cutoff.final_sequence);
    assert_eq!(terminal.event.name, "finalized");
    assert!(observation.recv().await.is_none());
}

#[tokio::test]
async fn producer_contention_is_nonblocking_and_visible_at_finalization() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", true)).unwrap();
    let guard = hub.inner.state.lock().unwrap();
    hub.emit(event("s", "contended", false));
    assert_eq!(
        hub.capture_payload(&correlation("s"), "text/plain", b"test"),
        Err(XrayError::Busy)
    );
    drop(guard);
    let cutoff = observation.finalize().unwrap();
    assert_eq!(cutoff.hub_contention_drops, 2);
    assert_eq!(observation.recv().await.unwrap().event.name, "ready");
    assert_eq!(observation.recv().await.unwrap().event.name, "finalized");
}

fn payload_request(
    observer: &XrayObservation,
    reference: &XrayPayloadRef,
) -> XrayReadPayloadRequest {
    XrayReadPayloadRequest {
        schema_version: XRAY_SCHEMA_VERSION,
        observation_id: observer.id().into(),
        session_id: reference.session_id.clone(),
        payload_id: reference.payload_id.clone(),
        offset: 0,
        length: 3,
    }
}

#[test]
fn payload_capture_is_opt_in_bounded_and_never_reconstructed() {
    let hub = XrayHub::default();
    assert_eq!(
        hub.capture_payload(&correlation("s"), "text/plain", b"abc")
            .unwrap(),
        None
    );
    let shallow = hub.observe(request("s", false)).unwrap();
    assert_eq!(
        hub.capture_payload(&correlation("s"), "text/plain", b"abc")
            .unwrap(),
        None
    );
    let deep = hub.observe(request("s", true)).unwrap();
    let reference = hub
        .capture_payload(&correlation("s"), "text/plain", b"abcdef")
        .unwrap()
        .unwrap();
    assert_eq!(reference.byte_len, 6);
    assert_eq!(reference.sha256, hex::encode(Sha256::digest(b"abcdef")));
    let mut read = payload_request(&deep, &reference);
    let range = hub.read_payload(&read).unwrap();
    assert_eq!(range.bytes, b"abc");
    assert!(!range.eof);
    read.offset = 6;
    assert_eq!(hub.read_payload(&read).unwrap().bytes, [] as [u8; 0]);
    assert!(hub.read_payload(&read).unwrap().eof);
    read.offset = 7;
    assert_eq!(hub.read_payload(&read), Err(XrayError::InvalidRange));
    read.offset = u64::MAX;
    assert_eq!(hub.read_payload(&read), Err(XrayError::InvalidRange));
    read.offset = 0;
    read.length = 0;
    assert_eq!(hub.read_payload(&read), Err(XrayError::InvalidRange));
    read.length = 3;
    read.observation_id = shallow.id().into();
    assert_eq!(hub.read_payload(&read), Err(XrayError::CaptureInactive));
    read.observation_id = deep.id().into();
    read.session_id = "foreign".into();
    assert_eq!(hub.read_payload(&read), Err(XrayError::CaptureInactive));
    read.session_id = "s".into();
    drop(deep);
    assert_eq!(
        hub.read_payload(&read),
        Err(XrayError::ObservationUnavailable)
    );
    assert!(!hub.deep_capture_active("s"));
}

#[test]
fn payload_eviction_is_bounded_counted_and_missing_is_not_empty_success() {
    let hub = XrayHub::new(XrayConfig {
        max_payload_bytes: 8,
        max_retained_payload_bytes: 8,
        max_retained_payloads: 1,
        max_payload_range_bytes: 4,
        ..XrayConfig::default()
    });
    let mut observation = hub.observe(request("s", true)).unwrap();
    let first = hub
        .capture_payload(&correlation("s"), "text/plain", b"12345678")
        .unwrap()
        .unwrap();
    let first_request = payload_request(&observation, &first);
    assert_eq!(
        hub.capture_payload(&correlation("s"), "text/plain", b"123456789"),
        Err(XrayError::PayloadTooLarge)
    );
    let second = hub
        .capture_payload(&correlation("s"), "text/plain", b"next")
        .unwrap()
        .unwrap();
    assert_eq!(
        hub.read_payload(&first_request),
        Err(XrayError::PayloadUnavailable)
    );
    let mut second_request = payload_request(&observation, &second);
    second_request.length = 5;
    assert_eq!(
        hub.read_payload(&second_request),
        Err(XrayError::InvalidRange)
    );
    second_request.length = 4;
    assert_eq!(hub.read_payload(&second_request).unwrap().bytes, b"next");
    assert_eq!(observation.finalize().unwrap().evicted_payloads, 1);
    // Finalize disables new capture but keeps retained bytes available to finish recording.
    assert!(
        hub.capture_payload(&correlation("s"), "text/plain", b"later")
            .unwrap()
            .is_none()
    );
    assert_eq!(hub.read_payload(&second_request).unwrap().bytes, b"next");
}

#[tokio::test]
async fn context_is_scoped_and_spawn_requires_explicit_origin() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", true)).unwrap();
    let context = hub.context(XrayCorrelation {
        session_id: "s".into(),
        turn_id: Some("origin".into()),
        attempt_id: Some("attempt".into()),
        ..XrayCorrelation::default()
    });
    context
        .scope(async {
            assert_eq!(
                CaptureContext::current().unwrap().correlation(),
                context.correlation()
            );
            tokio::spawn(async {
                assert!(CaptureContext::current().is_none());
            })
            .await
            .unwrap();
            let child = context.clone();
            tokio::spawn(async move {
                child
                    .scope(async {
                        CaptureContext::current().unwrap().emit(
                            "test",
                            "cleanup",
                            json!({}),
                            false,
                        );
                    })
                    .await;
            })
            .await
            .unwrap();
        })
        .await;
    assert!(CaptureContext::current().is_none());
    observation.recv().await.unwrap();
    assert_eq!(
        observation.recv().await.unwrap().event.correlation,
        *context.correlation()
    );
}

#[test]
fn invalid_versions_identities_and_capacity_fail_closed() {
    let hub = XrayHub::new(XrayConfig {
        max_observations: 1,
        ..XrayConfig::default()
    });
    let mut unsupported = request("s", false);
    unsupported.schema_version = 99;
    assert!(matches!(
        hub.observe(unsupported),
        Err(XrayError::UnsupportedVersion)
    ));
    assert!(matches!(
        hub.observe(request("", true)),
        Err(XrayError::InvalidIdentity)
    ));
    assert!(!hub.capture_active("s"));
    let first = hub.observe(request("s", false)).unwrap();
    assert!(matches!(
        hub.observe(request("another", true)),
        Err(XrayError::Capacity)
    ));
    drop(first);
    assert!(hub.observe(request("another", true)).is_ok());
    let invalid = XrayHub::new(XrayConfig {
        queue_capacity: 0,
        ..XrayConfig::default()
    });
    assert!(matches!(
        invalid.observe(request("s", false)),
        Err(XrayError::InvalidConfiguration)
    ));
}

#[tokio::test]
async fn dropping_pending_receive_does_not_close_the_observation() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", false)).unwrap();
    observation.recv().await.unwrap();
    {
        let future = observation.recv();
        tokio::pin!(future);
        assert!(matches!(
            std::future::poll_fn(|cx| { std::task::Poll::Ready(future.as_mut().poll(cx)) }).await,
            std::task::Poll::Pending
        ));
    }
    hub.emit(event("s", "after_cancelled_receive", false));
    assert_eq!(
        observation.recv().await.unwrap().event.name,
        "after_cancelled_receive"
    );
}

/// Test adapter pins correlation to its host's originating context, rather than
/// trusting the metadata in an incoming plugin-shaped event.
struct HostPinnedSink {
    context: CaptureContext,
}

impl XrayCaptureSink for HostPinnedSink {
    fn capture_active(&self) -> bool {
        self.context.capture_active()
    }
    fn deep_capture_active(&self) -> bool {
        self.context.deep_capture_active()
    }
    fn report_external_loss(&self, count: u64) -> bool {
        self.context.report_external_loss(count)
    }
    fn emit(&self, event: XrayEvent) {
        self.context.emit_event(event);
    }
}

#[tokio::test]
async fn external_sink_bridge_pins_host_scope_and_preserves_explicit_async_origin() {
    let hub = XrayHub::default();
    let mut host = hub.observe(request("host", true)).unwrap();
    let mut foreign = hub
        .observe(request("plugin-claimed-session", true))
        .unwrap();
    let origin = XrayCorrelation {
        session_id: "host".into(),
        turn_id: Some("host-turn".into()),
        ..XrayCorrelation::default()
    };
    let sink: Arc<dyn XrayCaptureSink> = Arc::new(HostPinnedSink {
        context: hub.context(origin.clone()),
    });
    let plugin = CaptureContext::from_sink(correlation("plugin-claimed-session"), sink);
    let captured = plugin.scope_sync(|| CaptureContext::current().unwrap());
    assert!(CaptureContext::current().is_none());
    tokio::spawn(async move {
        captured
            .scope(async {
                CaptureContext::current()
                    .unwrap()
                    .emit("plugin", "service", json!({}), true);
            })
            .await;
    })
    .await
    .unwrap();
    host.recv().await.unwrap();
    let received = host.recv().await.unwrap();
    assert_eq!(received.event.name, "service");
    assert_eq!(received.event.correlation, origin);
    foreign.finalize().unwrap();
    foreign.recv().await.unwrap();
    assert_eq!(foreign.recv().await.unwrap().event.name, "finalized");
}

#[test]
fn synchronous_scope_restores_nested_context_on_return_and_unwind() {
    let hub = XrayHub::default();
    let outer = hub.context(correlation("outer"));
    let inner = hub.context(correlation("inner"));
    assert!(CaptureContext::current().is_none());
    let returned = outer.scope_sync(|| {
        assert_eq!(
            CaptureContext::current().unwrap().correlation(),
            outer.correlation()
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            inner.scope_sync(|| {
                assert_eq!(
                    CaptureContext::current().unwrap().correlation(),
                    inner.correlation()
                );
                panic!("fixture unwind");
            });
        }));
        assert!(result.is_err());
        assert_eq!(
            CaptureContext::current().unwrap().correlation(),
            outer.correlation()
        );
        42
    });
    assert_eq!(returned, 42);
    assert!(CaptureContext::current().is_none());
}

#[tokio::test]
async fn nested_sync_scope_overrides_async_and_does_not_leak_to_new_thread() {
    let hub = XrayHub::default();
    let task = hub.context(correlation("task"));
    let thread = hub.context(correlation("thread"));
    task.scope(async {
        thread.scope_sync(|| {
            assert_eq!(
                CaptureContext::current().unwrap().correlation(),
                thread.correlation()
            );
        });
        assert_eq!(
            CaptureContext::current().unwrap().correlation(),
            task.correlation()
        );
    })
    .await;
    thread.scope_sync(|| {
        std::thread::spawn(|| assert!(CaptureContext::current().is_none()))
            .join()
            .unwrap();
        assert_eq!(
            CaptureContext::current().unwrap().correlation(),
            thread.correlation()
        );
    });
    assert!(CaptureContext::current().is_none());
}

struct InactiveSink;

impl XrayCaptureSink for InactiveSink {
    fn capture_active(&self) -> bool {
        false
    }
    fn deep_capture_active(&self) -> bool {
        false
    }
    fn emit(&self, _: XrayEvent) {
        panic!("inactive sink must not receive an event");
    }
}

#[tokio::test]
async fn disabled_sync_invocation_masks_async_capture_and_nested_async_can_override_mask() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("host", true)).unwrap();
    let host = hub.context(correlation("host"));
    let disabled = CaptureContext::from_sink(XrayCorrelation::default(), Arc::new(InactiveSink));
    host.scope(async {
        disabled.scope_sync(|| {
            let current = CaptureContext::current().unwrap();
            assert!(!current.capture_active());
            assert!(!current.deep_capture_active());
            current.emit("test", "must_not_capture", json!({}), false);
        });
        assert!(CaptureContext::current().unwrap().deep_capture_active());

        let mut inner = std::pin::pin!(host.scope(async {
            let current = CaptureContext::current().unwrap();
            assert_eq!(current.correlation(), host.correlation());
            current.emit("test", "explicit_inner", json!({}), false);
        }));
        std::future::poll_fn(|cx| {
            disabled.scope_sync(|| {
                assert!(!CaptureContext::current().unwrap().capture_active());
                let result = inner.as_mut().poll(cx);
                assert!(!CaptureContext::current().unwrap().capture_active());
                result
            })
        })
        .await;
    })
    .await;
    assert!(CaptureContext::current().is_none());
    observation.finalize().unwrap();
    assert_eq!(observation.recv().await.unwrap().event.name, "ready");
    assert_eq!(
        observation.recv().await.unwrap().event.name,
        "explicit_inner"
    );
    assert_eq!(observation.recv().await.unwrap().event.name, "finalized");
}

#[tokio::test]
async fn async_per_poll_mask_restores_executor_context_after_pending_and_ready() {
    let hub = XrayHub::default();
    let executor_parent = hub.context(correlation("executor-parent"));
    let child = hub.context(correlation("child"));
    let mut polls = 0;
    let mut future = std::pin::pin!(child.scope(std::future::poll_fn(|_cx| {
        assert_eq!(
            CaptureContext::current().unwrap().correlation(),
            child.correlation()
        );
        polls += 1;
        if polls == 1 {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(42)
        }
    })));
    let first = std::future::poll_fn(|cx| {
        executor_parent.scope_sync(|| {
            let result = future.as_mut().poll(cx);
            assert_eq!(
                CaptureContext::current().unwrap().correlation(),
                executor_parent.correlation()
            );
            std::task::Poll::Ready(result)
        })
    })
    .await;
    assert!(matches!(first, std::task::Poll::Pending));
    assert!(CaptureContext::current().is_none());
    // A separate poll, outside the previous synchronous parent scope, resumes only
    // the explicit child origin; no thread-local guard survived the first Pending.
    let second = std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx))).await;
    assert_eq!(second, std::task::Poll::Ready(42));
    assert!(CaptureContext::current().is_none());
}

#[tokio::test]
async fn async_poll_unwind_restores_parent_sync_mask() {
    let hub = XrayHub::default();
    let parent = hub.context(correlation("parent"));
    let child = hub.context(correlation("child"));
    let mut future = std::pin::pin!(child.scope(async {
        assert_eq!(
            CaptureContext::current().unwrap().correlation(),
            child.correlation()
        );
        panic!("fixture async poll unwind");
    }));
    std::future::poll_fn(|cx| {
        parent.scope_sync(|| {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| future.as_mut().poll(cx)));
            assert!(result.is_err());
            assert_eq!(
                CaptureContext::current().unwrap().correlation(),
                parent.correlation()
            );
        });
        std::task::Poll::Ready(())
    })
    .await;
    assert!(CaptureContext::current().is_none());
}

#[test]
fn contended_activity_hints_mark_all_live_observations_conservatively_incomplete() {
    let hub = XrayHub::default();
    let mut subject = hub.observe(request("subject", true)).unwrap();
    let mut other = hub.observe(request("other", false)).unwrap();
    let guard = hub.inner.activity.write().unwrap();
    assert!(!hub.capture_active("subject"));
    assert!(!hub.deep_capture_active("subject"));
    assert!(!hub.deep_capture_active("unrelated-session"));
    drop(guard);
    assert_eq!(subject.finalize().unwrap().hub_contention_drops, 3);
    assert_eq!(other.finalize().unwrap().hub_contention_drops, 3);
    // There is no exact missing event count because failed hints never allocated
    // event sequences. The uncertainty field alone must prevent completeness.
    assert_eq!(subject.finalize().unwrap().dropped_events, 0);
    assert!(hub.session_observation_active("subject").unwrap());
    assert!(hub.observation_active().unwrap());
    drop(subject);
    drop(other);
    assert!(!hub.observation_active().unwrap());
}

#[tokio::test]
async fn external_sink_gates_sensitive_events_and_reports_unsupported_payload_capture() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", true)).unwrap();
    let sink: Arc<dyn XrayCaptureSink> = Arc::new(HostPinnedSink {
        context: hub.context(correlation("s")),
    });
    let context = CaptureContext::from_sink(correlation("s"), sink);
    assert_eq!(
        context.capture_payload("text/plain", b"not retained"),
        Err(XrayError::PayloadCaptureUnsupported)
    );
    observation.recv().await.unwrap();
    let omission = observation.recv().await.unwrap();
    assert_eq!(omission.event.name, "payload_omitted");
    assert_eq!(
        omission.event.data["reason"],
        "external_sink_payload_unsupported"
    );
    assert!(omission.event.sensitive);
    context.emit("plugin", "too_large", json!("x".repeat(70 * 1024)), true);
    assert_eq!(
        observation.recv().await.unwrap().event.name,
        "event_omitted"
    );
    observation.finalize().unwrap();
    assert!(!context.capture_active());
    assert!(!context.deep_capture_active());
    assert_eq!(context.capture_payload("text/plain", b"later"), Ok(None));
    context.emit("plugin", "after_finalize", json!({}), false);
    assert_eq!(observation.recv().await.unwrap().event.name, "finalized");
    assert!(observation.recv().await.is_none());
}

#[tokio::test]
async fn external_shallow_sink_never_receives_sensitive_content_or_payload_omissions() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", false)).unwrap();
    let sink: Arc<dyn XrayCaptureSink> = Arc::new(HostPinnedSink {
        context: hub.context(correlation("s")),
    });
    let context = CaptureContext::from_sink(correlation("s"), sink);
    assert!(context.capture_active());
    assert!(!context.deep_capture_active());
    context.emit("plugin", "sensitive", json!({"fixture": "private"}), true);
    assert_eq!(context.capture_payload("text/plain", b"private"), Ok(None));
    context.emit("plugin", "scalar", json!({}), false);
    observation.finalize().unwrap();
    assert_eq!(observation.recv().await.unwrap().event.name, "ready");
    assert_eq!(observation.recv().await.unwrap().event.name, "scalar");
    assert_eq!(observation.recv().await.unwrap().event.name, "finalized");
    assert!(observation.recv().await.is_none());
}

#[tokio::test]
async fn multi_sequence_tail_gap_advances_cursor_through_last_missing_before_finalization() {
    let hub = XrayHub::new(XrayConfig {
        queue_capacity: 1,
        ..XrayConfig::default()
    });
    let mut observation = hub.observe(request("s", false)).unwrap();
    for name in ["kept", "lost_one", "lost_two", "lost_three"] {
        hub.emit(event("s", name, false));
    }
    let receipt = observation.finalize().unwrap();
    assert_eq!(receipt.dropped_events, 3);
    let mut delivered_through = 0;
    while let Some(envelope) = observation.recv().await {
        assert_eq!(envelope.sequence, delivered_through + 1);
        if envelope.event.name == "gap" {
            let gap: XrayGap = serde_json::from_value(envelope.event.data).unwrap();
            assert_eq!(envelope.sequence, gap.first_sequence);
            assert_eq!(gap.first_sequence, 3);
            assert_eq!(gap.last_sequence, 5);
            delivered_through = gap.last_sequence;
        } else {
            delivered_through = envelope.sequence;
        }
    }
    assert_eq!(delivered_through, receipt.final_sequence);
    assert_eq!(receipt.final_sequence, 6);
}

#[tokio::test]
async fn run_scoped_observation_filters_hints_events_and_payloads_before_capture() {
    let hub = XrayHub::default();
    let mut scoped = hub
        .observe_run(request("shared", true), "selected".into())
        .unwrap();
    let selected = XrayCorrelation {
        run_id: Some("selected".into()),
        ..correlation("shared")
    };
    let other = XrayCorrelation {
        run_id: Some("other".into()),
        ..correlation("shared")
    };
    let accepted = hub.context(selected.clone());
    let rejected = hub.context(other);
    let unscoped = hub.context(correlation("shared"));
    assert!(accepted.deep_capture_active());
    assert!(accepted.capture_active());
    assert!(!rejected.deep_capture_active());
    assert!(!unscoped.deep_capture_active());
    assert!(!hub.deep_capture_active("shared"));
    assert!(!hub.capture_active("shared"));
    assert!(hub.session_observation_active("shared").unwrap());
    assert_eq!(
        rejected.capture_payload("text/plain", b"unrelated"),
        Ok(None)
    );
    assert_eq!(
        unscoped.capture_payload("text/plain", b"unattributed"),
        Ok(None)
    );
    rejected.emit("domain", "unrelated", json!({}), true);
    unscoped.emit("domain", "unattributed", json!({}), false);
    let reference = accepted
        .capture_payload("text/plain", b"selected fixture")
        .unwrap()
        .unwrap();
    accepted.emit("domain", "selected", json!({}), true);
    // Omission still carries the exact run filter, without retaining source content.
    accepted.emit("domain", "oversized", json!("x".repeat(70 * 1024)), true);
    let cutoff = scoped.finalize().unwrap();
    assert_eq!(cutoff.dropped_events, 0);
    assert!(!accepted.deep_capture_active());
    assert_eq!(accepted.capture_payload("text/plain", b"late"), Ok(None));
    assert_eq!(
        hub.read_payload(&payload_request(&scoped, &reference))
            .unwrap()
            .bytes,
        b"sel"
    );
    let ready = scoped.recv().await.unwrap();
    assert_eq!(ready.event.correlation.run_id.as_deref(), Some("selected"));
    let event = scoped.recv().await.unwrap();
    assert_eq!(event.event.name, "selected");
    assert_eq!(event.event.correlation, selected);
    let omission = scoped.recv().await.unwrap();
    assert_eq!(omission.event.name, "event_omitted");
    assert_eq!(
        omission.event.correlation.run_id.as_deref(),
        Some("selected")
    );
    assert_eq!(scoped.recv().await.unwrap().event.name, "finalized");
    assert!(scoped.recv().await.is_none());
}

#[test]
fn run_scopes_share_no_payloads_and_session_observers_preserve_broad_capture() {
    let hub = XrayHub::default();
    assert!(matches!(
        hub.observe_run(request("s", true), String::new()),
        Err(XrayError::InvalidIdentity)
    ));
    let mut first = hub.observe_run(request("s", true), "first".into()).unwrap();
    let second = hub
        .observe_run(request("s", true), "second".into())
        .unwrap();
    let broad = hub.observe(request("s", true)).unwrap();
    let selected = hub.context(XrayCorrelation {
        run_id: Some("first".into()),
        ..correlation("s")
    });
    let reference = selected
        .capture_payload("text/plain", b"fixture")
        .unwrap()
        .unwrap();
    assert!(
        hub.read_payload(&payload_request(&first, &reference))
            .is_ok()
    );
    assert!(
        hub.read_payload(&payload_request(&broad, &reference))
            .is_ok()
    );
    assert_eq!(
        hub.read_payload(&payload_request(&second, &reference)),
        Err(XrayError::PayloadUnavailable)
    );
    assert!(hub.deep_capture_active("s"));
    drop(broad);
    first.finalize().unwrap();
    assert!(!selected.deep_capture_active());
    assert!(!hub.deep_capture_active("s"));
    assert!(
        hub.context(XrayCorrelation {
            run_id: Some("second".into()),
            ..correlation("s")
        })
        .deep_capture_active()
    );
}

#[tokio::test]
async fn past_deadlines_disable_capture_without_waiting_for_observer_shutdown() {
    let hub = XrayHub::default();
    let deadline = Some(Instant::now());
    let mut session = hub
        .observe_with_deadline(request("s", true), deadline)
        .unwrap();
    let mut run = hub
        .observe_run_with_deadline(request("s", true), "r".into(), deadline)
        .unwrap();
    let ordinary = hub.context(correlation("s"));
    let scoped = hub.context(XrayCorrelation {
        run_id: Some("r".into()),
        ..correlation("s")
    });
    for context in [&ordinary, &scoped] {
        assert!(!context.capture_active());
        assert!(!context.deep_capture_active());
        context.emit("domain", "expired", json!({}), false);
        context.emit("domain", "expired_sensitive", json!({}), true);
        assert_eq!(context.capture_payload("text/plain", b"expired"), Ok(None));
    }
    // An expired observer still owns evidence/lifecycle state until finalized/dropped.
    assert!(hub.session_observation_active("s").unwrap());
    for observation in [&mut session, &mut run] {
        let cutoff = observation.finalize().unwrap();
        assert_eq!(cutoff.final_sequence, 2);
        assert_eq!(cutoff.dropped_events, 0);
        assert_eq!(observation.recv().await.unwrap().event.name, "ready");
        assert_eq!(observation.recv().await.unwrap().event.name, "finalized");
        assert!(observation.recv().await.is_none());
    }
    // Neither expired scoped nor broad observations silently expire an ordinary one.
    let mut unbounded = hub.observe(request("s", true)).unwrap();
    assert!(ordinary.deep_capture_active());
    assert!(scoped.deep_capture_active());
    ordinary.emit("domain", "unbounded", json!({}), false);
    assert!(
        ordinary
            .capture_payload("text/plain", b"retained")
            .unwrap()
            .is_some()
    );
    unbounded.finalize().unwrap();
    unbounded.recv().await.unwrap();
    assert_eq!(unbounded.recv().await.unwrap().event.name, "unbounded");
}

#[test]
fn deadline_is_rechecked_under_retention_lock_despite_a_stale_positive_hint() {
    let hub = XrayHub::default();
    let mut observation = hub.observe_with_deadline(request("s", true), None).unwrap();
    let context = hub.context(correlation("s"));
    // Simulate crossing expiry after a positive hint without sleeps or scheduling.
    // Deliberately leave the derived hint unchanged to exercise authoritative checks.
    hub.state()
        .unwrap()
        .observers
        .get_mut(observation.id())
        .unwrap()
        .deadline = Some(Instant::now());
    assert!(context.deep_capture_active());
    context.emit("domain", "too_late", json!({}), false);
    assert_eq!(context.capture_payload("text/plain", b"too late"), Ok(None));
    let state = hub.state().unwrap();
    let observer = &state.observers[observation.id()];
    assert!(observer.queue.is_empty());
    assert!(observer.payloads.is_empty());
    drop(state);
    assert_eq!(observation.finalize().unwrap().final_sequence, 2);
}

#[tokio::test]
async fn deadline_stops_new_capture_but_preserves_pre_cutoff_evidence_for_drain() {
    let hub = XrayHub::default();
    let mut observation = hub.observe_with_deadline(request("s", true), None).unwrap();
    let context = hub.context(correlation("s"));
    context.emit("domain", "before", json!({}), false);
    let reference = context
        .capture_payload("text/plain", b"fixture")
        .unwrap()
        .unwrap();
    {
        let mut state = hub.state().unwrap();
        state.observers.get_mut(observation.id()).unwrap().deadline = Some(Instant::now());
        hub.refresh_activity(&state);
        drop(state);
    }
    assert!(!context.capture_active());
    assert!(!context.deep_capture_active());
    context.emit("domain", "after", json!({}), false);
    assert_eq!(context.capture_payload("text/plain", b"after"), Ok(None));
    assert_eq!(
        hub.read_payload(&payload_request(&observation, &reference))
            .unwrap()
            .bytes,
        b"fix"
    );
    observation.finalize().unwrap();
    observation.recv().await.unwrap();
    assert_eq!(observation.recv().await.unwrap().event.name, "before");
    assert_eq!(observation.recv().await.unwrap().event.name, "finalized");
    assert!(observation.recv().await.is_none());
}

#[test]
fn delivery_lock_does_not_suppress_activity_hints() {
    let hub = XrayHub::default();
    let mut deep = hub.observe(request("deep", true)).unwrap();
    let mut shallow = hub.observe(request("shallow", false)).unwrap();
    let guard = hub.inner.state.lock().unwrap();
    assert!(hub.capture_active("deep"));
    assert!(hub.deep_capture_active("deep"));
    assert!(hub.capture_active("shallow"));
    assert!(!hub.deep_capture_active("shallow"));
    assert!(!hub.capture_active("absent"));
    drop(guard);
    assert_eq!(deep.finalize().unwrap().hub_contention_drops, 0);
    assert!(!hub.capture_active("deep"));
    assert!(hub.capture_active("shallow"));
    assert_eq!(shallow.finalize().unwrap().hub_contention_drops, 0);
    assert!(!hub.capture_active("shallow"));
}

#[test]
fn external_payload_callback_pins_host_scope_and_returns_real_retention() {
    let hub = XrayHub::default();
    let mut host = hub.observe(request("host", true)).unwrap();
    let foreign = hub.observe(request("foreign", true)).unwrap();
    let origin = hub.context(correlation("host"));
    let sink = Arc::new(HostPinnedSink {
        context: origin.clone(),
    });
    let calls = Arc::new(AtomicU64::new(0));
    let callback_calls = Arc::clone(&calls);
    let context = CaptureContext::with_external_sink(
        correlation("foreign"),
        sink,
        move |_untrusted, content_type, bytes| {
            callback_calls.fetch_add(1, Ordering::Relaxed);
            origin.capture_payload(content_type, bytes)
        },
    );
    let reference = context
        .capture_payload("text/plain", b"fixture")
        .unwrap()
        .unwrap();
    assert_eq!(reference.session_id, "host");
    let range = hub
        .read_payload(&payload_request(&host, &reference))
        .unwrap();
    assert_eq!(range.bytes, b"fix");
    assert_eq!(
        hub.read_payload(&payload_request(&foreign, &reference)),
        Err(XrayError::CaptureInactive)
    );
    assert_eq!(
        context.capture_payload("bad\nmedia", b"fixture"),
        Err(XrayError::InvalidContentType)
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let rebound = context.with_correlation(correlation("other-untrusted-origin"));
    assert!(
        rebound
            .capture_payload("text/plain", b"another")
            .unwrap()
            .is_some()
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    host.finalize().unwrap();
    assert_eq!(context.capture_payload("text/plain", b"late"), Ok(None));
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn emit_with_capture_preserves_explicit_references_while_emit_keeps_data_opaque() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("host", true)).unwrap();
    let host = hub.context(correlation("host"));
    let reference = host
        .capture_payload("text/plain", b"fixture")
        .unwrap()
        .unwrap();
    let adapter = CaptureContext::from_sink(
        correlation("host"),
        Arc::new(HostPinnedSink { context: host }),
    );
    let capture = XrayCaptureAttachments::new(vec![XrayCaptureAttachment::PayloadRef {
        pointer: "/body".into(),
    }]);
    adapter.emit_with_capture(
        "plugin",
        "payload",
        json!({"body": reference}),
        true,
        Some(capture.clone()),
    );
    adapter.emit("plugin", "opaque", json!({"body": reference}), true);
    observation.recv().await.unwrap();
    let explicit = observation.recv().await.unwrap();
    assert_eq!(explicit.event.capture, Some(capture));
    assert_eq!(
        explicit.event.payload_references().unwrap(),
        vec![reference]
    );
    let opaque = observation.recv().await.unwrap();
    assert!(opaque.event.capture.is_none());
    assert_eq!(
        opaque.event.payload_references().unwrap(),
        [] as [bcode_xray_models::XrayPayloadRef; 0]
    );
}

#[tokio::test]
async fn typed_external_event_preserves_references_and_invalid_metadata_becomes_omission() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("host", true)).unwrap();
    let host = hub.context(correlation("host"));
    let reference = host
        .capture_payload("text/plain", b"fixture")
        .unwrap()
        .unwrap();
    let adapter = CaptureContext::from_sink(
        correlation("host"),
        Arc::new(HostPinnedSink { context: host }),
    );
    let mut typed = event("untrusted", "payload", true);
    typed.data = json!({"body": reference});
    typed.capture = Some(XrayCaptureAttachments::new(vec![
        XrayCaptureAttachment::PayloadRef {
            pointer: "/body".into(),
        },
    ]));
    adapter.emit_event(typed.clone());
    observation.recv().await.unwrap();
    let delivered = observation.recv().await.unwrap();
    assert_eq!(delivered.event.correlation.session_id, "host");
    assert_eq!(
        delivered.event.payload_references().unwrap(),
        vec![reference]
    );
    typed.capture = Some(XrayCaptureAttachments::new(vec![
        XrayCaptureAttachment::PayloadRef {
            pointer: "/absent".into(),
        },
    ]));
    adapter.emit_event(typed);
    let omitted = observation.recv().await.unwrap();
    assert_eq!(omitted.event.name, "event_omitted");
    assert_eq!(omitted.event.data["reason"], "invalid_capture_metadata");
    assert!(omitted.event.capture.is_none());
}

#[test]
fn external_payload_callback_is_not_called_for_shallow_or_inactive_scope() {
    let hub = XrayHub::default();
    let observation = hub.observe(request("shallow", false)).unwrap();
    let context = CaptureContext::with_external_sink(
        correlation("shallow"),
        Arc::new(HostPinnedSink {
            context: hub.context(correlation("shallow")),
        }),
        |_, _, _| panic!("unauthorized payload callback"),
    );
    assert_eq!(context.capture_payload("text/plain", b"private"), Ok(None));
    drop(observation);
    assert_eq!(context.capture_payload("text/plain", b"private"), Ok(None));
}

#[test]
fn trait_payload_hook_works_without_a_constructor_callback() {
    struct PayloadSink(CaptureContext);
    impl XrayCaptureSink for PayloadSink {
        fn capture_active(&self) -> bool {
            self.0.capture_active()
        }
        fn deep_capture_active(&self) -> bool {
            self.0.deep_capture_active()
        }
        fn emit(&self, event: XrayEvent) {
            self.0.emit_event(event);
        }
        fn capture_payload(
            &self,
            media_type: &str,
            bytes: &[u8],
        ) -> Result<Option<XrayPayloadRef>, XrayError> {
            self.0.capture_payload(media_type, bytes)
        }
    }
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("host", true)).unwrap();
    let context = CaptureContext::from_sink(
        correlation("untrusted"),
        Arc::new(PayloadSink(hub.context(correlation("host")))),
    );
    let reference = context
        .capture_payload("text/plain", b"fixture")
        .unwrap()
        .unwrap();
    assert_eq!(reference.session_id, "host");
    assert_eq!(
        hub.read_payload(&payload_request(&observation, &reference))
            .unwrap()
            .bytes,
        b"fix"
    );
    observation.finalize().unwrap();
    assert_eq!(context.capture_payload("text/plain", b"late"), Ok(None));
}

#[test]
fn external_payload_callback_propagates_host_rejection_without_inventing_a_reference() {
    let hub = XrayHub::new(XrayConfig {
        max_payload_bytes: 3,
        ..XrayConfig::default()
    });
    let mut observation = hub.observe(request("s", true)).unwrap();
    let origin = hub.context(correlation("s"));
    let callback_origin = origin.clone();
    let adapter = CaptureContext::with_external_sink(
        correlation("s"),
        Arc::new(HostPinnedSink { context: origin }),
        move |_, content_type, bytes| callback_origin.capture_payload(content_type, bytes),
    );
    assert_eq!(
        adapter.capture_payload("text/plain", b"four"),
        Err(XrayError::PayloadTooLarge)
    );
    let guard = hub.inner.state.lock().unwrap();
    assert_eq!(
        adapter.capture_payload("text/plain", b"ok"),
        Err(XrayError::Busy)
    );
    drop(guard);
    assert_eq!(observation.finalize().unwrap().hub_contention_drops, 1);
    assert!(
        hub.state().unwrap().observers[observation.id()]
            .payloads
            .is_empty()
    );
}

#[test]
fn external_loss_is_acknowledged_conservative_saturating_and_cutoff_stable() {
    let hub = XrayHub::default();
    hub.report_external_loss(7); // Before attach is not charged to this observation.
    let mut observation = hub.observe(request("s", true)).unwrap();
    let direct = hub.context(correlation("s"));
    let bridged = CaptureContext::from_sink(
        correlation("foreign"),
        Arc::new(HostPinnedSink {
            context: direct.clone(),
        }),
    );
    let guard = hub.inner.state.lock().unwrap();
    assert!(bridged.report_external_loss(3));
    assert!(direct.report_external_loss(2));
    drop(guard);
    let finalization = observation.finalize().unwrap();
    assert_eq!(finalization.hub_contention_drops, 5);
    assert_eq!(finalization.dropped_events, 0);
    assert!(direct.report_external_loss(u64::MAX));
    assert_eq!(hub.inner.contention.load(Ordering::Relaxed), u64::MAX);
    assert_eq!(observation.finalize().unwrap(), finalization);
    let unsupported = CaptureContext::from_sink(correlation("s"), Arc::new(InactiveSink));
    assert!(!unsupported.report_external_loss(1));
}
