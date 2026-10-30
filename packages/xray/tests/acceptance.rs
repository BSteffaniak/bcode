#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Behavioral acceptance at the public capture boundary; no daemon or canonical store.

use bcode_xray::{CaptureContext, XrayConfig, XrayHub};
use bcode_xray_models::{XrayCorrelation, XrayEnvelope, XrayEvent, XrayObserveRequest};
use serde_json::json;
use std::time::Duration;

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
    XrayEvent {
        domain: "acceptance".to_owned(),
        name: name.to_owned(),
        correlation: correlation(session, turn),
        data: json!({"fixture": true}),
        sensitive: false,
    }
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
    assert!(eof.bytes.is_empty());
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
    let payload = hub
        .capture_payload(
            &correlation("record", Some("turn")),
            "text/plain",
            b"retained fixture",
        )
        .expect("capture")
        .expect("active deep observation");
    recorder
        .expect_payload(&payload)
        .expect("register exact reference");
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
        .expect_payload(&payload)
        .expect("payload reference");
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
