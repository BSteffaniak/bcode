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
    assert!(hub.read_payload(&read).unwrap().bytes.is_empty());
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
    fn emit(&self, event: XrayEvent) {
        self.context
            .emit(&event.domain, &event.name, event.data, event.sensitive);
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
    let guard = hub.inner.state.lock().unwrap();
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
