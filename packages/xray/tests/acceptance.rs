#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Behavioral acceptance at the public capture boundary; no daemon or canonical store.

use bcode_xray::{CaptureContext, XrayConfig, XrayHub};
use bcode_xray_models::{XrayCorrelation, XrayEnvelope, XrayEvent, XrayObserveRequest};
use serde_json::json;
use std::time::Duration;

const PAYLOAD_ACCEPTANCE_BYTES: usize = 59_030;

fn payload_acceptance_config() -> XrayConfig {
    XrayConfig {
        queue_capacity: 64,
        ingress_capacity: 64,
        max_ingress_bytes: 4 * 1024 * 1024,
        max_inflight_operations: 64,
        max_payload_bytes: 128 * 1024,
        max_pending_payloads: 64,
        max_pending_payload_bytes: 4 * 1024 * 1024,
        max_retained_payloads: 64,
        max_retained_payload_bytes: 4 * 1024 * 1024,
        max_payload_range_bytes: 64 * 1024,
        ..XrayConfig::default()
    }
}

fn payload_read(
    observer: &bcode_xray::XrayObservation,
    reference: &bcode_xray_models::XrayPayloadRef,
) -> bcode_xray_models::XrayReadPayloadRequest {
    bcode_xray_models::XrayReadPayloadRequest {
        schema_version: 1,
        observation_id: observer.id().to_owned(),
        session_id: reference.session_id.clone(),
        payload_id: reference.payload_id.clone(),
        offset: 0,
        length: 64 * 1024,
    }
}

fn capture_declared_fixture(
    hub: &XrayHub,
    session: &str,
    index: u8,
) -> bcode_xray_models::XrayPayloadRef {
    use bcode_xray_models::{XrayCaptureAttachment, XrayCaptureAttachments};

    let origin = correlation(session, Some("payload-turn"));
    let reference = hub
        .capture_payload(
            &origin,
            "application/octet-stream",
            &vec![index; PAYLOAD_ACCEPTANCE_BYTES],
        )
        .expect("below-budget capture must not fail for ordinary concurrency")
        .expect("active deep observation retains capture");
    let mut emitted = XrayEvent::new(
        "acceptance",
        "concurrent_payload",
        origin,
        json!({"index": index, "payload": reference}),
        true,
    );
    emitted.capture = Some(XrayCaptureAttachments::new(vec![
        XrayCaptureAttachment::PayloadRef {
            pointer: "/payload".to_owned(),
        },
    ]));
    hub.emit(emitted);
    reference
}

fn request(session: &str, deep_capture: bool) -> XrayObserveRequest {
    XrayObserveRequest {
        schema_version: 1,
        session_id: session.to_owned(),
        deep_capture,
    }
}

fn correlation(session: &str, turn: Option<&str>) -> XrayCorrelation {
    XrayCorrelation {
        session_id: session.to_owned(),
        turn_id: turn.map(str::to_owned),
        ..XrayCorrelation::default()
    }
}

fn event(session: &str, turn: Option<&str>, name: &str) -> XrayEvent {
    XrayEvent::new(
        "acceptance",
        name,
        correlation(session, turn),
        json!({"fixture": true}),
        false,
    )
}

async fn receive(observer: &mut bcode_xray::XrayObservation) -> XrayEnvelope {
    tokio::time::timeout(Duration::from_secs(2), observer.recv())
        .await
        .expect("bounded observation receive")
        .expect("observation remains open")
}

#[tokio::test]
async fn idle_readiness_precedes_submission_and_observation_spans_turns() {
    let hub = XrayHub::default();
    let mut observer = hub.observe(request("idle", false)).expect("attach idle");
    let ready = receive(&mut observer).await;
    assert_eq!(ready.observation_id, observer.id());
    assert_eq!(ready.event.correlation.session_id, "idle");
    assert!(ready.event.correlation.turn_id.is_none());
    assert!(hub.capture_active("idle"));
    assert!(!hub.deep_capture_active("idle"));

    let mut previous = ready.sequence;
    for (turn, name) in [
        (Some("first"), "turn_started"),
        (Some("first"), "turn_completed"),
        (None, "idle"),
        (Some("second"), "turn_started"),
        (Some("second"), "turn_cancelled"),
    ] {
        hub.emit(event("idle", turn, name));
        let received = receive(&mut observer).await;
        assert_eq!(received.event.name, name);
        assert_eq!(received.event.correlation.turn_id.as_deref(), turn);
        assert_eq!(received.observation_id, ready.observation_id);
        assert!(received.sequence > previous);
        previous = received.sequence;
    }
    drop(observer);
    assert!(!hub.capture_active("idle"));
}

#[tokio::test]
async fn session_scope_and_deep_opt_in_are_per_observer() {
    let hub = XrayHub::default();
    let mut shallow = hub.observe(request("scope", false)).expect("shallow");
    let mut deep = hub.observe(request("scope", true)).expect("explicit deep");
    receive(&mut shallow).await;
    receive(&mut deep).await;
    assert!(hub.deep_capture_active("scope"));
    assert!(!hub.deep_capture_active("foreign"));

    hub.emit(event("foreign", None, "foreign_marker"));
    let mut secret = event("scope", Some("turn"), "sensitive_marker");
    secret.sensitive = true;
    secret.data = json!({"content": "sensitive-fixture-not-a-credential"});
    hub.emit(secret);
    hub.emit(event("scope", Some("turn"), "public_marker"));

    let captured = receive(&mut deep).await;
    assert_eq!(captured.event.name, "sensitive_marker");
    assert!(captured.event.sensitive);
    assert_eq!(receive(&mut deep).await.event.name, "public_marker");
    let public = receive(&mut shallow).await;
    assert_eq!(public.event.name, "public_marker");
    assert!(
        !serde_json::to_string(&public)
            .expect("JSON")
            .contains("sensitive-fixture")
    );

    drop(deep);
    assert!(!hub.deep_capture_active("scope"));
    assert!(hub.capture_active("scope"));
    drop(shallow);
    assert!(!hub.capture_active("scope"));
}

#[tokio::test]
async fn task_context_is_explicit_and_keeps_originating_correlation() {
    let hub = XrayHub::default();
    let mut observer = hub.observe(request("origin", false)).expect("observe");
    receive(&mut observer).await;
    let origin = XrayCorrelation {
        request_id: Some("request-origin".to_owned()),
        tool_call_id: Some("tool-origin".to_owned()),
        run_id: Some("run-origin".to_owned()),
        activation_id: Some("activation-origin".to_owned()),
        attempt_id: Some("attempt-origin".to_owned()),
        ..correlation("origin", Some("turn-origin"))
    };
    let context = hub.context(origin.clone());
    assert!(CaptureContext::current().is_none());
    context
        .clone()
        .scope(async {
            let current = CaptureContext::current().expect("scoped context");
            assert_eq!(current.correlation(), &origin);
            current.emit("acceptance", "entry", json!({}), false);
            tokio::spawn(async {
                assert!(
                    CaptureContext::current().is_none(),
                    "spawn does not inherit authority"
                );
            })
            .await
            .expect("unscoped child");
            let cleanup = context.clone();
            tokio::spawn(async move {
                cleanup.emit(
                    "acceptance",
                    "cleanup",
                    json!({"outcome": "completed"}),
                    false,
                );
            })
            .await
            .expect("explicit cleanup child");
        })
        .await;
    assert!(CaptureContext::current().is_none());
    for name in ["entry", "cleanup"] {
        let received = receive(&mut observer).await;
        assert_eq!(received.event.name, name);
        assert_eq!(received.event.correlation, origin);
    }
}

#[tokio::test]
async fn detached_observation_does_not_enable_later_capture_or_replay() {
    let hub = XrayHub::default();
    let mut first = hub.observe(request("detach", true)).expect("first");
    let first_id = first.id().to_owned();
    receive(&mut first).await;
    drop(first);
    assert!(!hub.deep_capture_active("detach"));
    hub.emit(event("detach", Some("unobserved"), "must_not_replay"));
    let mut second = hub.observe(request("detach", false)).expect("second");
    assert_ne!(second.id(), first_id);
    receive(&mut second).await;
    hub.emit(event("detach", Some("next"), "new_capture"));
    assert_eq!(receive(&mut second).await.event.name, "new_capture");
}

#[tokio::test]
async fn saturated_observer_reports_exact_tail_loss_before_stable_finalization() {
    let hub = XrayHub::new(XrayConfig {
        queue_capacity: 2,
        ..XrayConfig::default()
    });
    let mut observer = hub.observe(request("loss", true)).expect("observe");
    let ready = receive(&mut observer).await;
    assert_eq!(ready.event.domain, "xray");
    assert_eq!(ready.event.name, "ready");
    for index in 0..7 {
        let mut emitted = event("loss", Some("turn"), "burst");
        emitted.data = json!({"index": index});
        hub.emit(emitted);
    }
    let cutoff = observer.finalize().expect("delivery cutoff");
    assert_eq!(observer.finalize().expect("idempotent cutoff"), cutoff);
    assert_eq!(cutoff.dropped_events, 5);
    assert!(!hub.capture_active("loss"));
    assert!(!hub.deep_capture_active("loss"));
    hub.emit(event("loss", Some("late"), "after_cutoff"));
    for index in 0..2 {
        let retained = receive(&mut observer).await;
        assert_eq!(retained.event.data["index"], index);
        assert_eq!(retained.sequence, 2 + index);
    }
    let gap = receive(&mut observer).await;
    assert_eq!(
        (gap.event.domain.as_str(), gap.event.name.as_str()),
        ("xray", "gap")
    );
    let missing: bcode_xray_models::XrayGap =
        serde_json::from_value(gap.event.data).expect("typed loss interval");
    assert_eq!((missing.first_sequence, missing.last_sequence), (4, 8));
    let finalized = receive(&mut observer).await;
    assert_eq!(finalized.event.name, "finalized");
    assert_eq!(finalized.sequence, cutoff.final_sequence);
    assert_eq!(
        serde_json::from_value::<bcode_xray_models::XrayFinalization>(finalized.event.data)
            .expect("typed finalization"),
        cutoff
    );
    assert!(observer.recv().await.is_none());
    assert!(observer.recv().await.is_none(), "terminal EOF is stable");
}

#[tokio::test]
async fn sensitive_payload_access_is_bounded_and_bound_to_live_owning_observation() {
    use bcode_xray_models::XrayReadPayloadRequest;

    let hub = XrayHub::default();
    let origin = correlation("payload", Some("turn"));
    assert!(
        hub.capture_payload(&origin, "text/plain", b"not observed")
            .expect("disabled capture")
            .is_none()
    );
    let mut deep = hub.observe(request("payload", true)).expect("deep");
    let shallow = hub.observe(request("payload", false)).expect("shallow");
    let foreign = hub.observe(request("other", true)).expect("foreign");
    receive(&mut deep).await;
    let payload = hub
        .capture_payload(&origin, "text/plain", b"fixture content")
        .expect("capture")
        .expect("retained");
    let read = XrayReadPayloadRequest {
        schema_version: 1,
        observation_id: deep.id().to_owned(),
        session_id: "payload".to_owned(),
        payload_id: payload.payload_id.clone(),
        offset: 0,
        length: 7,
    };
    let range = hub.read_payload(&read).expect("bounded retained range");
    assert_eq!(range.bytes, b"fixture");
    assert!(!range.eof);
    assert_eq!(range.payload, payload);
    for denied in [
        XrayReadPayloadRequest {
            observation_id: shallow.id().to_owned(),
            ..read.clone()
        },
        XrayReadPayloadRequest {
            observation_id: foreign.id().to_owned(),
            ..read.clone()
        },
        XrayReadPayloadRequest {
            session_id: "other".to_owned(),
            ..read.clone()
        },
        XrayReadPayloadRequest {
            payload_id: "../outside".to_owned(),
            ..read.clone()
        },
        XrayReadPayloadRequest {
            length: 0,
            ..read.clone()
        },
        XrayReadPayloadRequest {
            length: u32::MAX,
            ..read.clone()
        },
        XrayReadPayloadRequest {
            offset: u64::MAX,
            ..read.clone()
        },
        XrayReadPayloadRequest {
            schema_version: u16::MAX,
            ..read.clone()
        },
    ] {
        assert!(hub.read_payload(&denied).is_err());
    }
    let eof = hub
        .read_payload(&XrayReadPayloadRequest {
            offset: payload.byte_len,
            ..read.clone()
        })
        .expect("EOF");
    assert!(eof.eof);
    assert_eq!(eof.bytes, [] as [u8; 0]);
    deep.finalize().expect("cutoff");
    assert!(
        hub.read_payload(&read).is_ok(),
        "delivery cutoff is not payload disposal"
    );
    drop(deep);
    assert!(
        hub.read_payload(&read).is_err(),
        "dropping owner releases retained bytes"
    );
    assert!(!hub.deep_capture_active("payload"));
}

#[tokio::test]
async fn explicit_capture_locations_survive_delivery_without_discovering_ordinary_json() {
    use bcode_xray_models::{XrayCaptureAttachment, XrayCaptureAttachments};

    let hub = XrayHub::default();
    let context = hub.context(correlation("attachments", Some("turn")));
    let mut deep = hub.observe(request("attachments", true)).expect("deep");
    let mut shallow = hub.observe(request("attachments", false)).expect("shallow");
    receive(&mut deep).await;
    receive(&mut shallow).await;
    let reference = context
        .capture_payload("text/plain", b"retained")
        .unwrap()
        .unwrap();
    let ordinary = json!({
        "malformed": {"payload_id": "ordinary", "sha256": "not-a-digest", "byte_len": 42},
        "valid_looking": reference,
        "future_looking": {"schema_version": 999, "payload_id": "also-ordinary"},
    });
    context.emit("acceptance", "ordinary", ordinary.clone(), false);
    for observation in [&mut deep, &mut shallow] {
        let delivered = receive(observation).await;
        assert_eq!(delivered.event.data, ordinary);
        assert!(delivered.event.capture.is_none());
        assert_eq!(
            delivered.payload_references().unwrap(),
            [] as [bcode_xray::XrayPayloadRef; 0]
        );
    }
    let mut captured = XrayEvent::new(
        "acceptance",
        "retained",
        context.correlation().clone(),
        json!({"payload": reference, "ordinary": ordinary}),
        true,
    );
    captured.capture = Some(XrayCaptureAttachments::new(vec![
        XrayCaptureAttachment::PayloadRef {
            pointer: "/payload".into(),
        },
    ]));
    context.emit_event(captured);
    let delivered = receive(&mut deep).await;
    assert_eq!(delivered.payload_references().unwrap(), vec![reference]);
    shallow.finalize().unwrap();
    assert_eq!(
        receive(&mut shallow).await.event.name,
        "finalized",
        "sensitive attachment was not delivered to shallow observation"
    );
    deep.finalize().unwrap();
    assert_eq!(receive(&mut deep).await.event.name, "finalized");
}

#[cfg(unix)]
fn emit_recording_payload_fixture(hub: &XrayHub) -> bcode_xray_models::XrayPayloadRef {
    use bcode_xray_models::{XrayCaptureAttachment, XrayCaptureAttachments};

    let context = hub.context(correlation("record", Some("turn")));
    let payload = context
        .capture_payload("text/plain", b"retained fixture")
        .expect("capture")
        .expect("active deep observation");
    context.emit_with_capture(
        "acceptance",
        "payload_captured",
        json!({
            "payload": payload,
            "ordinary": {"payload_id": "not-a-reference", "sha256": "ordinary", "byte_len": 17},
        }),
        true,
        Some(XrayCaptureAttachments::new(vec![
            XrayCaptureAttachment::PayloadRef {
                pointer: "/payload".into(),
            },
        ])),
    );
    payload
}

#[cfg(unix)]
fn register_declared_payloads(
    recorder: &mut impl bcode_xray::recording::RecordingSink,
    references: &[bcode_xray_models::XrayPayloadRef],
) {
    for reference in references {
        recorder
            .expect_payload(reference)
            .expect("register declared reference");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn recorder_consumes_actual_engine_payloads_and_only_certifies_local_boundary() {
    use bcode_xray::recording::{
        JsonlBundleRecorder, RecordingOptions, RecordingSink, RecordingState,
    };
    use bcode_xray_models::XrayReadPayloadRequest;

    let directory = tempfile::tempdir().expect("private artifacts");
    let root = directory.path().canonicalize().expect("non-symlink root");
    let destination = root.join("bundle");
    let hub = XrayHub::default();
    let mut observer = hub
        .observe(request("record", true))
        .expect("deep observation");
    let mut recorder = JsonlBundleRecorder::create(
        &destination,
        observer.id(),
        "record",
        RecordingOptions::default(),
    )
    .expect("explicit bundle");
    let ready = receive(&mut observer).await;
    recorder.append(&ready).expect("record ready");
    let payload = emit_recording_payload_fixture(&hub);
    let captured = receive(&mut observer).await;
    let references = captured.payload_references().expect("explicit locations");
    assert_eq!(references, vec![payload.clone()]);
    recorder
        .append(&captured)
        .expect("record typed payload event");
    register_declared_payloads(&mut recorder, &references);
    let range = hub
        .read_payload(&XrayReadPayloadRequest {
            schema_version: 1,
            observation_id: observer.id().to_owned(),
            session_id: "record".to_owned(),
            payload_id: payload.payload_id.clone(),
            offset: 0,
            length: 1024,
        })
        .expect("authorized bounded range");
    assert!(range.eof);
    recorder
        .store_payload(&payload, &range.bytes)
        .expect("integrity-checked persistence");
    let boundary = observer.finalize().expect("delivery cutoff");
    assert!(
        recorder.finalize(&boundary).is_err(),
        "engine cutoff alone is not delivered evidence"
    );
    // A rejected premature finalization may close a sink fail-closed. Use a fresh sink
    // for the successful path, replaying only the fixture's explicitly retained envelopes.
    drop(recorder);
    let mut recorder = JsonlBundleRecorder::create(
        &root.join("complete"),
        observer.id(),
        "record",
        RecordingOptions::default(),
    )
    .expect("new explicit sink");
    recorder.append(&ready).expect("retained real engine ready");
    recorder
        .append(&captured)
        .expect("retained typed payload envelope");
    register_declared_payloads(&mut recorder, &references);
    recorder
        .store_payload(&payload, &range.bytes)
        .expect("payload");
    while let Some(envelope) = observer.recv().await {
        recorder.append(&envelope).expect("drained envelope");
    }
    let manifest = recorder.finalize(&boundary).expect("local flush receipt");
    assert_eq!(manifest.state, RecordingState::Finalized);
    assert!(manifest.evidence_complete);
    assert_eq!(manifest.finalization.as_ref(), Some(&boundary));
    let payload_entry = manifest
        .payloads
        .get(&payload.payload_id)
        .expect("registered payload");
    assert_eq!(
        std::fs::read(
            root.join("complete").join(
                payload_entry
                    .relative_path
                    .as_ref()
                    .expect("persisted payload")
            )
        )
        .expect("bundle bytes"),
        range.bytes
    );
    assert!(
        JsonlBundleRecorder::create(
            &root.join("complete"),
            observer.id(),
            "record",
            RecordingOptions::default()
        )
        .is_err(),
        "existing bundle is never overwritten"
    );
}

#[test]
fn unsupported_or_ambiguous_requests_do_not_activate_capture() {
    let hub = XrayHub::default();
    let mut future = request("future", true);
    future.schema_version = u16::MAX;
    assert!(hub.observe(future).is_err());
    assert!(!hub.capture_active("future"));
    assert!(!hub.deep_capture_active("future"));
    assert!(hub.observe(request("", true)).is_err());
    assert!(!hub.capture_active(""));
}

async fn complete_cutoff(
    observer: &mut bcode_xray::XrayObservation,
) -> bcode_xray_models::XrayFinalization {
    tokio::time::timeout(Duration::from_secs(5), observer.finalize_complete())
        .await
        .expect("bounded cutoff completion")
        .expect("settled cutoff")
}

#[tokio::test]
async fn concurrent_producers_and_payload_range_reads_preserve_every_below_capacity_event() {
    use bcode_xray_models::XrayReadPayloadRequest;
    use std::collections::BTreeSet;
    use std::sync::{Arc, Barrier};

    const PRODUCERS: u64 = 4;
    const EVENTS: u64 = 16;
    let hub = XrayHub::default();
    let mut observer = hub.observe(request("concurrent", true)).expect("deep");
    receive(&mut observer).await;
    let bytes = vec![0x5a; 64 * 1024];
    let payload = hub
        .capture_payload(
            &correlation("concurrent", None),
            "application/octet-stream",
            &bytes,
        )
        .expect("payload capture")
        .expect("retained payload");
    let read = XrayReadPayloadRequest {
        schema_version: 1,
        observation_id: observer.id().to_owned(),
        session_id: "concurrent".to_owned(),
        payload_id: payload.payload_id,
        offset: 0,
        length: 64 * 1024,
    };
    // The barrier starts real producer threads and bounded range reads together.
    // Forced private-lock schedules live in engine tests; this public-boundary
    // case requires exact delivery, not a probabilistic contention counter check.
    let start = Arc::new(Barrier::new(6));
    let (done, completed) = std::sync::mpsc::sync_channel(5);
    let mut workers = Vec::new();
    for producer in 0..PRODUCERS {
        let hub = hub.clone();
        let start = Arc::clone(&start);
        let done = done.clone();
        workers.push(std::thread::spawn(move || {
            start.wait();
            for index in 0..EVENTS {
                let mut emitted = event("concurrent", None, "item");
                emitted.data = json!({"producer": producer, "index": index});
                hub.emit(emitted);
            }
            done.send(()).expect("producer completion");
        }));
    }
    let reader_hub = hub.clone();
    let reader_start = Arc::clone(&start);
    workers.push(std::thread::spawn(move || {
        reader_start.wait();
        for _ in 0..256 {
            let range = reader_hub.read_payload(&read).expect("authorized range");
            assert_eq!(range.bytes, bytes);
            assert!(range.eof);
        }
        done.send(()).expect("reader completion");
    }));
    start.wait();
    // No observation receives until every producer completes: blocking channel
    // sends cannot pass merely because a receiver happens to make space.
    for _ in 0..5 {
        completed
            .recv_timeout(Duration::from_secs(5))
            .expect("producers and reader finish without observation draining");
    }
    for worker in workers {
        worker.join().expect("worker");
    }
    let cutoff = complete_cutoff(&mut observer).await;
    assert_eq!(cutoff.dropped_events, 0);
    assert_eq!(cutoff.hub_contention_drops, 0);
    assert_eq!(cutoff.evicted_payloads, 0);
    assert_eq!(cutoff.final_sequence, PRODUCERS * EVENTS + 2);
    let mut seen = BTreeSet::new();
    let mut next_index = [0_u64; 4];
    for sequence in 2..cutoff.final_sequence {
        let delivered = receive(&mut observer).await;
        assert_eq!(delivered.sequence, sequence);
        assert_eq!(delivered.event.name, "item");
        let producer = delivered.event.data["producer"].as_u64().expect("producer");
        let index = delivered.event.data["index"].as_u64().expect("index");
        let position = usize::try_from(producer).expect("bounded producer");
        assert_eq!(
            index, next_index[position],
            "per-producer publication order"
        );
        next_index[position] += 1;
        assert!(seen.insert((producer, index)), "no duplicate delivery");
    }
    let expected: BTreeSet<_> = (0..PRODUCERS)
        .flat_map(|producer| (0..EVENTS).map(move |index| (producer, index)))
        .collect();
    assert_eq!(seen, expected, "zero counters alone do not prove delivery");
    assert_eq!(receive(&mut observer).await.event.name, "finalized");
    assert!(observer.recv().await.is_none());
}

#[tokio::test]
async fn slow_observer_loss_does_not_spill_into_acknowledged_fast_observer() {
    let hub = XrayHub::new(XrayConfig {
        queue_capacity: 2,
        ..XrayConfig::default()
    });
    let mut slow = hub.observe(request("isolation", false)).expect("slow");
    let mut fast = hub.observe(request("isolation", false)).expect("fast");
    receive(&mut slow).await;
    receive(&mut fast).await;
    for index in 0..8_u64 {
        let mut emitted = event("isolation", None, "item");
        emitted.data = json!({"index": index});
        hub.emit(emitted);
        let delivered = receive(&mut fast).await;
        assert_eq!(delivered.sequence, index + 2);
        assert_eq!(delivered.event.data["index"], index);
    }
    let fast_cutoff = complete_cutoff(&mut fast).await;
    let slow_cutoff = complete_cutoff(&mut slow).await;
    assert_eq!(fast_cutoff.dropped_events, 0);
    assert_eq!(fast_cutoff.hub_contention_drops, 0);
    assert_eq!(slow_cutoff.dropped_events, 6);
    assert_eq!(slow_cutoff.hub_contention_drops, 0);
    assert_eq!(fast_cutoff.final_sequence, 10);
    assert_eq!(slow_cutoff.final_sequence, 10);
    for index in 0..2_u64 {
        assert_eq!(receive(&mut slow).await.event.data["index"], index);
    }
    let gap = receive(&mut slow).await;
    assert_eq!(gap.event.name, "gap");
    let gap: bcode_xray_models::XrayGap = serde_json::from_value(gap.event.data).expect("gap");
    assert_eq!((gap.first_sequence, gap.last_sequence), (4, 9));
    for observer in [&mut fast, &mut slow] {
        let terminal = receive(observer).await;
        assert_eq!(terminal.event.name, "finalized");
        assert_eq!(terminal.sequence, 10);
        assert!(observer.recv().await.is_none());
        assert!(observer.recv().await.is_none());
    }
}

#[tokio::test]
async fn queued_events_keep_admission_targets_across_attach_and_cutoff() {
    let hub = XrayHub::default();
    let mut first = hub.observe(request("targets", false)).expect("first");
    receive(&mut first).await;
    hub.emit(event("targets", None, "before_attach"));
    let mut second = hub.observe(request("targets", false)).expect("second");
    receive(&mut second).await;
    let first_cutoff = complete_cutoff(&mut first).await;
    hub.emit(event("targets", None, "after_cutoff"));
    let second_cutoff = complete_cutoff(&mut second).await;
    for (observer, cutoff, name) in [
        (&mut first, first_cutoff, "before_attach"),
        (&mut second, second_cutoff, "after_cutoff"),
    ] {
        assert_eq!(cutoff.final_sequence, 3);
        assert_eq!(cutoff.dropped_events, 0);
        assert_eq!(cutoff.hub_contention_drops, 0);
        let delivered = receive(observer).await;
        assert_eq!(delivered.sequence, 2);
        assert_eq!(delivered.event.name, name);
        let terminal = receive(observer).await;
        assert_eq!(terminal.event.name, "finalized");
        assert_eq!(
            serde_json::from_value::<bcode_xray_models::XrayFinalization>(terminal.event.data)
                .expect("terminal receipt"),
            cutoff
        );
        assert!(observer.recv().await.is_none());
    }
}

#[tokio::test]
async fn ingress_count_and_byte_overflow_are_unsequenced_but_never_lossless() {
    // Keep delivery capacity larger than the burst: only ingress can reject.
    for (config, expected_retained) in [
        (
            XrayConfig {
                queue_capacity: 16,
                ingress_capacity: 2,
                ..XrayConfig::default()
            },
            2,
        ),
        (
            XrayConfig {
                queue_capacity: 16,
                ingress_capacity: 16,
                max_event_bytes: 1024,
                max_ingress_bytes: 1024,
                ..XrayConfig::default()
            },
            1,
        ),
    ] {
        let hub = XrayHub::new(config);
        let mut observer = hub.observe(request("ingress", false)).expect("observe");
        receive(&mut observer).await;
        for index in 0..7_u64 {
            let mut emitted = event("ingress", None, "item");
            emitted.data = json!({"index": index, "padding": "x".repeat(512)});
            hub.emit(emitted);
        }
        let cutoff = complete_cutoff(&mut observer).await;
        assert_eq!(cutoff.dropped_events, 0, "delivery queue never fills");
        assert!(
            cutoff.hub_contention_drops > 0,
            "ingress rejection is visible"
        );
        let mut retained = 0_u64;
        loop {
            let delivered = receive(&mut observer).await;
            if delivered.event.name == "finalized" {
                assert_eq!(delivered.sequence, cutoff.final_sequence);
                break;
            }
            assert_eq!(
                delivered.event.name, "item",
                "no invented exact ingress gaps"
            );
            assert_eq!(delivered.sequence, retained + 2);
            assert_eq!(delivered.event.data["index"], retained);
            retained += 1;
        }
        assert_eq!(
            retained, expected_retained,
            "exact independent ingress bound"
        );
        assert_eq!(retained + cutoff.hub_contention_drops, 7);
        assert_eq!(cutoff.final_sequence, retained + 2);
        assert_eq!(complete_cutoff(&mut observer).await, cutoff);
        assert!(observer.recv().await.is_none());
    }
}

fn run_parallel_payload_captures(
    hub: &XrayHub,
    seed_read: bcode_xray_models::XrayReadPayloadRequest,
) -> std::collections::BTreeMap<u8, bcode_xray_models::XrayPayloadRef> {
    use std::sync::{Arc, Barrier};

    let start = Arc::new(Barrier::new(10));
    let (done, completed) = std::sync::mpsc::sync_channel(9);
    let mut workers = Vec::new();
    for index in 0..8_u8 {
        let hub = hub.clone();
        let start = Arc::clone(&start);
        let done = done.clone();
        workers.push(std::thread::spawn(move || {
            start.wait();
            let reference = capture_declared_fixture(&hub, "parallel-payload", index);
            done.send(Some((index, reference)))
                .expect("capture completion");
        }));
    }
    let reader_hub = hub.clone();
    let reader_start = Arc::clone(&start);
    workers.push(std::thread::spawn(move || {
        reader_start.wait();
        for _ in 0..128 {
            let range = reader_hub
                .read_payload(&seed_read)
                .expect("seed range remains available");
            assert_eq!(range.bytes, vec![0xa5; PAYLOAD_ACCEPTANCE_BYTES]);
            assert!(range.eof);
        }
        done.send(None).expect("reader completion");
    }));
    // The barrier aligns entry, not private lock ownership. Forced lock-held
    // interleavings belong to engine tests; no public test-only hook is needed.
    start.wait();
    let mut references = std::collections::BTreeMap::new();
    for _ in 0..9 {
        if let Some((index, reference)) = completed
            .recv_timeout(Duration::from_secs(5))
            .expect("bounded capture/read completion without observation receives")
        {
            assert!(references.insert(index, reference).is_none());
        }
    }
    for worker in workers {
        worker.join().expect("payload worker");
    }
    assert_eq!(references.len(), 8);
    references
}

#[tokio::test]
async fn eight_parallel_payload_captures_and_range_reader_preserve_all_typed_references() {
    use std::collections::BTreeSet;

    let hub = XrayHub::new(payload_acceptance_config());
    let mut observer = hub
        .observe(request("parallel-payload", true))
        .expect("deep");
    receive(&mut observer).await;
    let initial_payload = hub
        .capture_payload(
            &correlation("parallel-payload", None),
            "application/octet-stream",
            &vec![0xa5; PAYLOAD_ACCEPTANCE_BYTES],
        )
        .expect("seed capture")
        .expect("seed retained");
    let seed_read = payload_read(&observer, &initial_payload);
    hub.read_payload(&seed_read)
        .expect("seed committed before concurrency");
    let references = run_parallel_payload_captures(&hub, seed_read);
    let cutoff = complete_cutoff(&mut observer).await;
    assert_eq!(cutoff.dropped_events, 0);
    assert_eq!(cutoff.hub_contention_drops, 0);
    assert_eq!(cutoff.evicted_payloads, 0);
    assert_eq!(cutoff.final_sequence, 10);
    let mut seen = BTreeSet::new();
    for sequence in 2..10 {
        let delivered = receive(&mut observer).await;
        assert_eq!(delivered.sequence, sequence);
        assert_eq!(delivered.event.name, "concurrent_payload");
        let index = u8::try_from(delivered.event.data["index"].as_u64().expect("index"))
            .expect("bounded index");
        assert!(seen.insert(index), "each producer appears exactly once");
        let reference = references.get(&index).expect("returned reference");
        assert_eq!(
            delivered.payload_references().expect("typed attachment"),
            vec![reference.clone()]
        );
        let range = hub
            .read_payload(&payload_read(&observer, reference))
            .expect("authorized retained bytes after cutoff");
        assert_eq!(range.payload, *reference);
        assert_eq!(range.bytes, vec![index; PAYLOAD_ACCEPTANCE_BYTES]);
        assert!(range.eof);
    }
    assert_eq!(seen, (0..8).collect());
    let terminal = receive(&mut observer).await;
    assert_eq!(terminal.event.name, "finalized");
    assert_eq!(
        serde_json::from_value::<bcode_xray_models::XrayFinalization>(terminal.event.data)
            .expect("receipt"),
        cutoff
    );
    assert!(observer.recv().await.is_none());
}

#[tokio::test]
async fn payload_cutoff_settles_unread_captures_including_zero_bytes_without_phantom_refs() {
    let hub = XrayHub::new(payload_acceptance_config());
    let mut observer = hub.observe(request("payload-cutoff", true)).expect("deep");
    receive(&mut observer).await;
    let origin = correlation("payload-cutoff", None);
    let mut captures = Vec::new();
    for length in [0, 1, PAYLOAD_ACCEPTANCE_BYTES] {
        let bytes = vec![0x71; length];
        let reference = hub
            .capture_payload(&origin, "application/octet-stream", &bytes)
            .expect("capture admitted")
            .expect("owned pending bytes");
        assert_eq!(reference.byte_len, u64::try_from(length).expect("length"));
        captures.push((reference, bytes));
    }
    // No payload read or event receive drives commit before cutoff completion.
    let cutoff = complete_cutoff(&mut observer).await;
    assert_eq!(
        cutoff.final_sequence, 2,
        "payload commands are not event coordinates"
    );
    assert_eq!(cutoff.dropped_events, 0);
    assert_eq!(cutoff.hub_contention_drops, 0);
    assert_eq!(cutoff.evicted_payloads, 0);
    for (reference, bytes) in &captures {
        let read = payload_read(&observer, reference);
        let range = hub
            .read_payload(&read)
            .expect("cutoff settled every returned reference");
        assert_eq!(range.payload, *reference);
        assert_eq!(range.bytes, *bytes);
        assert!(range.eof);
        let eof = hub
            .read_payload(&bcode_xray_models::XrayReadPayloadRequest {
                offset: reference.byte_len,
                ..read
            })
            .expect("exact EOF");
        assert!(eof.eof);
        assert_eq!(eof.bytes, [] as [u8; 0]);
    }
    assert!(
        hub.capture_payload(&origin, "text/plain", b"late")
            .expect("closed capture")
            .is_none()
    );
    assert_eq!(complete_cutoff(&mut observer).await, cutoff);
    assert_eq!(receive(&mut observer).await.event.name, "finalized");
    assert!(observer.recv().await.is_none());
    let read = payload_read(&observer, &captures[0].0);
    drop(observer);
    assert_eq!(
        hub.read_payload(&read),
        Err(bcode_xray::XrayError::ObservationUnavailable)
    );
}

#[tokio::test]
async fn settled_payload_denials_are_typed_not_transient_busy_or_success() {
    use bcode_xray::XrayError;
    use bcode_xray_models::XrayReadPayloadRequest;

    let hub = XrayHub::new(payload_acceptance_config());
    let mut owner = hub.observe(request("payload-errors", true)).expect("owner");
    let foreign = hub
        .observe(request("foreign-payload", true))
        .expect("foreign");
    let shallow = hub
        .observe(request("payload-errors", false))
        .expect("shallow");
    receive(&mut owner).await;
    let reference = hub
        .capture_payload(
            &correlation("payload-errors", None),
            "text/plain",
            b"exact bytes",
        )
        .expect("capture")
        .expect("retained");
    complete_cutoff(&mut owner).await;
    let read = payload_read(&owner, &reference);
    let range = hub.read_payload(&read).expect("owner authorized");
    assert_eq!(range.bytes, b"exact bytes");
    assert_eq!(range.payload, reference);
    for (denied, expected) in [
        (
            XrayReadPayloadRequest {
                schema_version: u16::MAX,
                ..read.clone()
            },
            XrayError::UnsupportedVersion,
        ),
        (
            XrayReadPayloadRequest {
                observation_id: foreign.id().to_owned(),
                ..read.clone()
            },
            XrayError::CaptureInactive,
        ),
        (
            XrayReadPayloadRequest {
                observation_id: shallow.id().to_owned(),
                ..read.clone()
            },
            XrayError::CaptureInactive,
        ),
        (
            XrayReadPayloadRequest {
                payload_id: "not-retained".to_owned(),
                ..read.clone()
            },
            XrayError::PayloadUnavailable,
        ),
        (
            XrayReadPayloadRequest {
                offset: reference.byte_len + 1,
                ..read.clone()
            },
            XrayError::InvalidRange,
        ),
    ] {
        assert_eq!(hub.read_payload(&denied), Err(expected));
    }
    drop(owner);
    assert_eq!(
        hub.read_payload(&read),
        Err(XrayError::ObservationUnavailable)
    );
}
