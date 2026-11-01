//! Deterministic, in-process provider capture fixtures. These do not prove dylib propagation.
use super::*;
use bcode_xray::{XRAY_SCHEMA_VERSION, XrayCorrelation, XrayHub, XrayObserveRequest};

fn fixture_context() -> NativeServiceContext {
    let request: ModelTurnRequest = serde_json::from_value(serde_json::json!({
        "session_id": "00000000-0000-0000-0000-000000000000",
        "turn_id": "xray-fixture-turn", "model_id": "fake-echo",
        "messages": [{"role":"user", "content":[{"type":"text", "text":"xray deterministic fixture"}]}],
        "provider_context": {"settings":{"api_key":"fixture-credential-never-capture"}}
    })).unwrap();
    serde_json::from_value(serde_json::json!({
        "plugin_id": "bcode.fake-provider",
        "request": {
            "interface_id": MODEL_PROVIDER_INTERFACE_ID_V3,
            "operation": OP_RUN_TURN,
            "payload": serde_json::to_vec(&request).unwrap()
        }
    }))
    .unwrap()
}

fn observe(hub: &XrayHub, session: &str, deep: bool) -> bcode_xray::XrayObservation {
    hub.observe(XrayObserveRequest {
        schema_version: XRAY_SCHEMA_VERSION,
        session_id: session.into(),
        deep_capture: deep,
    })
    .unwrap()
}

#[tokio::test]
async fn real_fake_adapter_and_push_stream_are_captured_without_changing_response() {
    let hub = XrayHub::default();
    let session = "00000000-0000-0000-0000-000000000000";
    let mut deep = observe(&hub, session, true);
    let mut shallow = observe(&hub, session, false);
    let mut foreign = observe(&hub, "foreign", true);
    let capture = hub.context(XrayCorrelation {
        session_id: session.into(),
        turn_id: Some("xray-fixture-turn".into()),
        attempt_id: Some("attempt-1".into()),
        ..Default::default()
    });
    let plugin = FakeProviderPlugin::default();
    let baseline = plugin.invoke_provider_service(&fixture_context());
    let response = capture
        .scope(async { plugin.invoke_provider_service(&fixture_context()) })
        .await;
    assert!(response.error.is_none());
    assert_eq!(response.payload, baseline.payload);
    deep.finalize().unwrap();
    shallow.finalize().unwrap();
    foreign.finalize().unwrap();
    let mut request_seen = false;
    let mut stream_seen = false;
    while let Some(envelope) = deep.recv().await {
        if envelope.event.domain != "provider.fake" {
            continue;
        }
        assert!(envelope.event.sensitive);
        assert_eq!(
            envelope.event.correlation.turn_id.as_deref(),
            Some("xray-fixture-turn")
        );
        assert_eq!(
            envelope.event.correlation.attempt_id.as_deref(),
            Some("attempt-1")
        );
        let data = serde_json::to_string(&envelope.event.data).unwrap();
        assert!(!data.contains("fixture-credential-never-capture"));
        if envelope.event.name == "adapted_request" {
            request_seen = true;
            assert_eq!(envelope.event.data["value"]["model_id"], "fake-echo");
            assert!(data.contains("xray deterministic fixture"));
        }
        stream_seen |= envelope.event.name == "stream_record";
    }
    assert!(request_seen && stream_seen);
    while let Some(envelope) = shallow.recv().await {
        if envelope.event.domain == "provider.fake" {
            assert_eq!(envelope.event.name, "normalized_outcome");
            assert!(!envelope.event.sensitive);
        }
    }
    while let Some(envelope) = foreign.recv().await {
        assert_ne!(envelope.event.domain, "provider.fake");
    }
}

#[tokio::test]
async fn typed_payload_locations_ignore_reference_shaped_user_content() {
    let hub = XrayHub::default();
    let mut observation = observe(&hub, "fixture", true);
    let capture = hub.context(XrayCorrelation {
        session_id: "fixture".into(),
        ..Default::default()
    });
    capture
        .scope(async {
            diagnostics::record(
                "inline_fixture",
                &serde_json::json!({
                    "payload_ref":{"payload_id":"user-text", "sha256":"not-a-digest", "byte_len":7}
                }),
            );
            diagnostics::record(
                "retained_fixture",
                &serde_json::json!({
                    "text":"fixture".repeat(8192),
                    "payload_ref":{"payload_id":"user-text", "sha256":"not-a-digest", "byte_len":7}
                }),
            );
        })
        .await;
    observation.finalize().unwrap();
    assert_eq!(observation.recv().await.unwrap().event.name, "ready");
    let inline = observation.recv().await.unwrap();
    assert!(inline.payload_references().unwrap().is_empty());
    assert_eq!(
        inline.event.data["value"]["payload_ref"]["payload_id"],
        "user-text"
    );
    let retained = observation.recv().await.unwrap();
    let references = retained.payload_references().unwrap();
    assert_eq!(references.len(), 1);
    assert_eq!(
        serde_json::to_value(&references[0]).unwrap(),
        retained.event.data["payload_ref"]
    );
    assert_eq!(observation.recv().await.unwrap().event.name, "finalized");
    assert!(observation.recv().await.is_none());
}

#[tokio::test]
async fn shallow_outcomes_exclude_arbitrary_error_and_protected_metadata() {
    let hub = XrayHub::default();
    let mut observation = observe(&hub, "fixture", false);
    let capture = hub.context(XrayCorrelation {
        session_id: "fixture".into(),
        ..Default::default()
    });
    capture
        .scope(async {
            let mut error = fake_persist_error("private-message");
            error.category = ProviderErrorCategory::Network;
            error.provider_message = Some("private-provider-message".into());
            error.diagnostic_context = Box::new(BTreeMap::from([(
                "private-key".into(),
                "private-value".into(),
            )]));
            diagnostics::event(&ProviderTurnEvent::Error { error });
            diagnostics::event(&ProviderTurnEvent::Cancelled);
            diagnostics::event(&ProviderTurnEvent::Usage {
                usage: TokenUsage::default(),
            });
        })
        .await;
    observation.finalize().unwrap();
    assert_eq!(observation.recv().await.unwrap().event.name, "ready");
    let error = observation.recv().await.unwrap();
    assert_eq!(error.event.data["category"], "network");
    assert!(!serde_json::to_string(&error).unwrap().contains("private"));
    assert!(!error.event.sensitive);
    assert_eq!(
        observation.recv().await.unwrap().event.data["outcome"],
        "cancelled"
    );
    assert_eq!(observation.recv().await.unwrap().event.name, "finalized");
    assert!(observation.recv().await.is_none());
}

#[tokio::test]
async fn oversized_capture_is_explicit_loss_without_private_json_and_finish_is_normalized() {
    let hub = XrayHub::default();
    let mut observation = observe(&hub, "fixture", true);
    let capture = hub.context(XrayCorrelation {
        session_id: "fixture".into(),
        ..Default::default()
    });
    capture.scope_sync(|| {
        diagnostics::record(
            "oversized_fixture",
            &serde_json::json!({
                "text": "界🙂".repeat(160_000),
                "api_key": "private-oversized-secret"
            }),
        );
        diagnostics::event(&ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::EndTurn,
        });
    });
    observation.finalize().unwrap();
    let mut omitted = false;
    let mut finished = false;
    let mut loss = None;
    while let Some(envelope) = observation.recv().await {
        assert!(
            !serde_json::to_string(&envelope)
                .unwrap()
                .contains("private-oversized-secret")
        );
        assert!(envelope.payload_references().unwrap().is_empty());
        if envelope.event.name == "payload_omitted" {
            assert_eq!(envelope.event.data["reason"], "serialization_or_size_limit");
            assert!(!envelope.event.sensitive);
            omitted = true;
        }
        if envelope.event.name == "normalized_outcome" {
            assert_eq!(envelope.event.data["outcome"], "finished");
            assert_eq!(envelope.event.data["stop_reason"], "end_turn");
            finished = true;
        }
        if envelope.event.name == "finalized" {
            loss = envelope.event.data["hub_contention_drops"].as_u64();
        }
    }
    assert!(omitted);
    assert!(finished);
    assert_eq!(loss, Some(1));
}

#[tokio::test]
async fn shallow_and_finalized_observations_never_serialize_sensitive_payloads() {
    struct MustNotSerialize;
    impl serde::Serialize for MustNotSerialize {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            panic!("inactive deep capture serialized sensitive payload");
        }
    }
    let hub = XrayHub::default();
    let mut shallow = observe(&hub, "fixture", false);
    let capture = hub.context(XrayCorrelation {
        session_id: "fixture".into(),
        ..Default::default()
    });
    capture
        .scope(async {
            diagnostics::record("fixture", &MustNotSerialize);
        })
        .await;
    let deep = observe(&hub, "fixture", true);
    drop(deep);
    capture
        .scope(async {
            diagnostics::record("fixture", &MustNotSerialize);
        })
        .await;
    shallow.finalize().unwrap();
    while let Some(envelope) = shallow.recv().await {
        assert_ne!(envelope.event.domain, "provider.fake");
    }
}

#[tokio::test]
async fn large_redacted_payload_is_retrievable_by_transient_reference_after_finalize() {
    use bcode_xray::{XrayPayloadRef, XrayReadPayloadRequest};
    let hub = XrayHub::default();
    let mut observation = observe(&hub, "fixture", true);
    let capture = hub.context(XrayCorrelation {
        session_id: "fixture".into(),
        ..Default::default()
    });
    let text = "large sensitive fixture".repeat(4096);
    capture
        .scope(async {
            diagnostics::record(
                "fixture",
                &serde_json::json!({
                    "text": text, "api_key": "fixture-secret-never-retain"
                }),
            );
        })
        .await;
    observation.finalize().unwrap();
    assert_eq!(observation.recv().await.unwrap().event.name, "ready");
    let envelope = observation.recv().await.unwrap();
    assert_eq!(envelope.event.name, "fixture");
    assert_eq!(
        envelope.event.data["capture_format"],
        "bcode.provider.capture"
    );
    assert_eq!(envelope.event.data["capture_version"], 1);
    assert_eq!(
        envelope.event.data["representation"],
        "redacted_json_payload_ref"
    );
    assert_eq!(envelope.payload_references().unwrap().len(), 1);
    let reference: XrayPayloadRef =
        serde_json::from_value(envelope.event.data["payload_ref"].clone()).unwrap();
    let mut bytes = Vec::new();
    loop {
        let range = hub
            .read_payload(&XrayReadPayloadRequest {
                schema_version: XRAY_SCHEMA_VERSION,
                observation_id: observation.id().into(),
                session_id: "fixture".into(),
                payload_id: reference.payload_id.clone(),
                offset: u64::try_from(bytes.len()).unwrap(),
                length: 16 * 1024,
            })
            .unwrap();
        assert_eq!(range.payload, reference);
        bytes.extend_from_slice(&range.bytes);
        if range.eof {
            break;
        }
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["text"], text);
    assert_eq!(value["api_key"], "[REDACTED]");
    assert!(
        !String::from_utf8(bytes)
            .unwrap()
            .contains("fixture-secret-never-retain")
    );
}

#[tokio::test]
async fn oversized_payload_is_explicitly_omitted_not_partially_delivered() {
    let hub = XrayHub::default();
    let mut observation = observe(&hub, "fixture", true);
    let capture = hub.context(XrayCorrelation {
        session_id: "fixture".into(),
        ..Default::default()
    });
    capture
        .scope(async {
            diagnostics::record("fixture", &"x".repeat(2 * 1024 * 1024));
        })
        .await;
    observation.finalize().unwrap();
    assert_eq!(observation.recv().await.unwrap().event.name, "ready");
    let omission = observation.recv().await.unwrap();
    assert_eq!(omission.event.name, "payload_omitted");
    assert_eq!(omission.event.data["reason"], "serialization_or_size_limit");
    assert_eq!(observation.recv().await.unwrap().event.name, "finalized");
    assert!(observation.recv().await.is_none());
}
