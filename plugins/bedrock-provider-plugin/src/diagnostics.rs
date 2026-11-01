//! Provider-private, opt-in diagnostic payload handling.
//!
//! Only JSON bodies/records enter this boundary; never pass transport headers,
//! endpoint URLs, credential configuration, or arbitrary transport errors here.
use bcode_xray::CaptureContext;
use serde::Serialize;
use serde_json::{Value, json};
use std::io::{self, Write};

// Leave room for envelope/correlation metadata below the engine's event bound.
const INLINE_BYTES: usize = 32 * 1024;
/// Maximum serialized diagnostic payload retained by this provider.
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
const CAPTURE_FORMAT: &str = "bcode.provider.capture";
const CAPTURE_VERSION: u16 = 1;

struct BoundedJson(Vec<u8>);
impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_PAYLOAD_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("diagnostic payload limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn redact(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            // Error/warning/retry strings can contain transport credentials. Lifecycle
            // facts are emitted separately; never retain their unrestricted JSON bodies.
            if matches!(
                fields.get("type").and_then(Value::as_str),
                Some("error" | "warning" | "retry_scheduled")
            ) {
                fields.retain(|key, _| key == "type");
                return;
            }
            // Normalized provider extensions and metadata carry replay/optimization
            // state, not diagnostic content. Preserve the discriminator, not the value.
            if matches!(
                fields.get("type").and_then(Value::as_str),
                Some("provider_extension" | "provider_metadata")
            ) && let Some(value) = fields.get_mut("value")
            {
                *value = Value::String("[REDACTED]".into());
            }
            for (key, value) in fields {
                let key = key.to_ascii_lowercase().replace(['-', '_'], "");
                if matches!(
                    key.as_str(),
                    "authorization"
                        | "proxyauthorization"
                        | "cookie"
                        | "setcookie"
                        | "apikey"
                        | "accesskeyid"
                        | "secretaccesskey"
                        | "awsaccesskeyid"
                        | "awssecretaccesskey"
                        | "awssessiontoken"
                        | "accesstoken"
                        | "refreshtoken"
                        | "idtoken"
                        | "bearertoken"
                        | "sessiontoken"
                        | "password"
                        | "clientsecret"
                        | "credentials"
                        | "token"
                        | "secret"
                        | "headers"
                        | "env"
                        | "settings"
                        | "providercontext"
                        | "privatekey"
                        | "xapikey"
                        | "xauthtoken"
                        | "signature"
                        | "thoughtsignature"
                        | "encryptedcontent"
                        | "redactedcontent"
                        | "providerprivatedata"
                        | "provideropaque"
                        | "providermetadata"
                        | "diagnosticcontext"
                        | "error"
                        | "providermessage"
                ) {
                    *value = Value::String("[REDACTED]".into());
                } else {
                    redact(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact),
        _ => {}
    }
}

/// Observe normalized lifecycle facts, never arbitrary provider error text.
pub fn event(event: &bcode_model::ProviderTurnEvent) {
    use bcode_model::ProviderTurnEvent;
    let Some(context) = CaptureContext::current() else {
        return;
    };
    let data = match event {
        ProviderTurnEvent::Error { error } => {
            json!({"outcome":"failed", "category":error.category, "retryable":error.retryable})
        }
        ProviderTurnEvent::Cancelled => json!({"outcome":"cancelled"}),
        ProviderTurnEvent::TurnFinished { stop_reason } => {
            json!({"outcome":"finished", "stop_reason":stop_reason})
        }
        ProviderTurnEvent::Usage { .. } => {
            record("normalized_usage", event);
            return;
        }
        _ => return,
    };
    context.emit_with_capture(
        "provider.bedrock",
        "normalized_outcome",
        data,
        false,
        Some(bcode_xray::XrayCaptureAttachments::new(Vec::new())),
    );
}

/// Capture a bounded JSON projection only while deep observation is active.
pub fn record(name: &str, payload: &impl Serialize) {
    let Some(context) = CaptureContext::current().filter(CaptureContext::deep_capture_active)
    else {
        return;
    };
    let mut writer = BoundedJson(Vec::new());
    if serde_json::to_writer(&mut writer, payload).is_err() {
        let _ = context.report_external_loss(1);
        context.emit_with_capture("provider.bedrock", "payload_omitted", json!({"capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION, "record": name, "reason": "serialization_or_size_limit", "limit_bytes": MAX_PAYLOAD_BYTES}), false,
Some(bcode_xray::XrayCaptureAttachments::new(Vec::new())));
        return;
    }
    emit_json(&context, name, &writer.0);
}

fn emit_json(context: &CaptureContext, name: &str, bytes: &[u8]) {
    let Ok(mut value) = serde_json::from_slice::<Value>(bytes) else {
        let _ = context.report_external_loss(1);
        context.emit_with_capture("provider.bedrock", "payload_omitted", json!({"capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION, "record": name, "reason": "invalid_or_excessively_nested_json"}), false,
Some(bcode_xray::XrayCaptureAttachments::new(Vec::new())));
        return;
    };
    redact(&mut value);
    // Serialize again only after redaction. Never retain unsanitized input bytes.
    let mut sanitized = BoundedJson(Vec::new());
    if serde_json::to_writer(&mut sanitized, &value).is_err() {
        let _ = context.report_external_loss(1);
        context.emit_with_capture("provider.bedrock", "payload_omitted", json!({"capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION, "record": name, "reason": "sanitized_size_limit", "limit_bytes": MAX_PAYLOAD_BYTES}), false,
Some(bcode_xray::XrayCaptureAttachments::new(Vec::new())));
        return;
    }
    if sanitized.0.len() <= INLINE_BYTES {
        context.emit_with_capture(
            "provider.bedrock",
            name,
            json!({
                "capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION,
                "representation": "redacted_json", "redaction": "structural_credentials_and_protected_metadata_v2", "wire_equivalent": false,
                "value": value
            }),
            true,
Some(bcode_xray::XrayCaptureAttachments::new(Vec::new())));
        return;
    }
    match context.capture_payload("application/json", &sanitized.0) {
        Ok(Some(reference)) => context.emit_with_capture("provider.bedrock", name, json!({
            "capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION,
            "representation": "redacted_json_payload_ref", "payload_ref": reference,
            "redaction": "structural_credentials_and_protected_metadata_v2", "wire_equivalent": false
        }), true, Some(bcode_xray::XrayCaptureAttachments::new(vec![
            bcode_xray::XrayCaptureAttachment::PayloadRef { pointer: "/payload_ref".into() },
        ]))),
        Ok(None) if !context.deep_capture_active() => {},
        Ok(None) | Err(_) => {
            let _ = context.report_external_loss(1);
            context.emit_with_capture("provider.bedrock", "payload_omitted", json!({
            "capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION,
            "record": name, "reason": "payload_retention_unavailable", "byte_len": sanitized.0.len()
        }), false,
Some(bcode_xray::XrayCaptureAttachments::new(Vec::new())));
        }
    }
}

/// Capture a bounded received JSON record after structural credential redaction.
pub fn bytes(name: &str, bytes: &[u8]) {
    let Some(context) = CaptureContext::current().filter(CaptureContext::deep_capture_active)
    else {
        return;
    };
    if bytes.len() > MAX_PAYLOAD_BYTES {
        let _ = context.report_external_loss(1);
        context.emit_with_capture("provider.bedrock", "payload_omitted", json!({"capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION, "record": name, "reason": "size_limit", "byte_len": bytes.len(), "limit_bytes": MAX_PAYLOAD_BYTES}), false,
Some(bcode_xray::XrayCaptureAttachments::new(Vec::new())));
        return;
    }
    emit_json(&context, name, bytes);
}

// Capture before a provider runtime spawn/block_on, not inside the new task.
// This carries an already supplied context; it cannot bridge a plugin ABI.
/// Capture originating context synchronously before polling on another runtime.
pub fn scope<F: std::future::Future>(future: F) -> impl std::future::Future<Output = F::Output> {
    let context = CaptureContext::current();
    async move {
        match context {
            Some(context) => context.scope(future).await,
            None => future.await,
        }
    }
}

/// Capture an SSE data record without transport headers or framing metadata.
pub fn stream_line(line: &str) {
    if let Some(data) = line.strip_prefix("data:") {
        let data = data.trim();
        if data == "[DONE]" {
            record("stream_terminal", &json!({"marker": "[DONE]"}));
        } else {
            bytes("stream_record", data.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capture_declarations_distinguish_inline_control_and_retained_payloads() {
        use bcode_xray::{XRAY_SCHEMA_VERSION, XrayCorrelation, XrayHub, XrayObserveRequest};
        let hub = XrayHub::default();
        let mut observation = hub
            .observe(XrayObserveRequest {
                schema_version: XRAY_SCHEMA_VERSION,
                session_id: "typed-fixture".into(),
                deep_capture: true,
            })
            .unwrap();
        let capture = hub.context(XrayCorrelation {
            session_id: "typed-fixture".into(),
            ..Default::default()
        });
        capture.scope_sync(|| {
            record("fixture_inline", &json!({"payload_id":"ordinary-user-data", "byte_len":7, "sha256":"not-a-reference"}));
            record("fixture_large", &json!({"text":"界🙂".repeat(8192), "api_key":"private-fixture-key"}));
            event(&bcode_model::ProviderTurnEvent::Cancelled);
            emit_json(&capture, "fixture_invalid", b"{");
        });
        observation.finalize().unwrap();
        let mut names = Vec::new();
        while let Some(envelope) = observation.recv().await {
            if !envelope.event.domain.starts_with("provider.") {
                continue;
            }
            let declaration = envelope
                .event
                .capture
                .as_ref()
                .expect("producer must declare capture semantics");
            assert_eq!(
                declaration.schema_version,
                bcode_xray::XRAY_CAPTURE_SCHEMA_VERSION
            );
            let references = envelope.payload_references().unwrap();
            assert!(
                !envelope
                    .event
                    .data
                    .to_string()
                    .contains("private-fixture-key")
            );
            if envelope.event.name == "fixture_large" {
                assert!(envelope.event.sensitive);
                assert_eq!(references.len(), 1);
                let reference = &references[0];
                assert_eq!(reference.session_id, "typed-fixture");
                assert_eq!(reference.content_type, "application/json");
                assert!(reference.byte_len > INLINE_BYTES as u64);
                let range = hub
                    .read_payload(&bcode_xray::XrayReadPayloadRequest {
                        schema_version: XRAY_SCHEMA_VERSION,
                        observation_id: observation.id().to_owned(),
                        session_id: "typed-fixture".into(),
                        payload_id: reference.payload_id.clone(),
                        offset: 0,
                        length: 4096,
                    })
                    .unwrap();
                assert_eq!(range.bytes.len(), 4096);
            } else {
                assert!(declaration.attachments.is_empty());
                assert!(references.is_empty());
                if envelope.event.name == "fixture_inline" {
                    assert!(envelope.event.sensitive);
                    assert_eq!(
                        envelope.event.data["value"]["payload_id"],
                        "ordinary-user-data"
                    );
                } else {
                    assert!(!envelope.event.sensitive);
                }
            }
            names.push(envelope.event.name);
        }
        assert_eq!(
            names,
            [
                "fixture_inline",
                "fixture_large",
                "normalized_outcome",
                "payload_omitted"
            ]
        );
    }

    #[tokio::test]
    async fn sdk_reasoning_stream_projection_redacts_protected_replay_state() {
        use aws_sdk_bedrockruntime::types::{
            ContentBlockDelta, ContentBlockDeltaEvent, ConverseStreamOutput,
            ReasoningContentBlockDelta,
        };
        use bcode_xray::{XRAY_SCHEMA_VERSION, XrayCorrelation, XrayHub, XrayObserveRequest};
        let hub = XrayHub::default();
        let mut observation = hub
            .observe(XrayObserveRequest {
                schema_version: XRAY_SCHEMA_VERSION,
                session_id: "fixture".into(),
                deep_capture: true,
            })
            .unwrap();
        let capture = hub.context(XrayCorrelation {
            session_id: "fixture".into(),
            ..Default::default()
        });
        capture
            .scope(async {
                for delta in [
                    ReasoningContentBlockDelta::Text("visible reasoning".into()),
                    ReasoningContentBlockDelta::Signature("protected-fixture".into()),
                    ReasoningContentBlockDelta::RedactedContent(aws_smithy_types::Blob::new(
                        b"protected-fixture",
                    )),
                ] {
                    let event = ConverseStreamOutput::ContentBlockDelta(
                        ContentBlockDeltaEvent::builder()
                            .content_block_index(0)
                            .delta(ContentBlockDelta::ReasoningContent(delta))
                            .build()
                            .unwrap(),
                    );
                    record(
                        "stream_record",
                        &super::super::converse_diagnostics::StreamRecord(&event),
                    );
                }
            })
            .await;
        observation.finalize().unwrap();
        assert_eq!(observation.recv().await.unwrap().event.name, "ready");
        let text = observation.recv().await.unwrap();
        assert_eq!(
            text.event.data["value"]["event"]["contentBlockDelta"]["delta"]["reasoningContent"]["text"],
            "visible reasoning"
        );
        let signature = observation.recv().await.unwrap();
        assert_eq!(
            signature.event.data["value"]["event"]["contentBlockDelta"]["delta"]["reasoningContent"]
                ["signature"],
            "[REDACTED]"
        );
        let redacted = observation.recv().await.unwrap();
        assert_eq!(
            redacted.event.data["value"]["event"]["contentBlockDelta"]["delta"]["reasoningContent"]
                ["redactedContent"],
            "[REDACTED]"
        );
        assert_eq!(observation.recv().await.unwrap().event.name, "finalized");
        assert!(observation.recv().await.is_none());
    }

    #[test]
    fn no_observation_does_not_even_serialize_payload() {
        use bcode_xray::{XRAY_SCHEMA_VERSION, XrayCorrelation, XrayHub, XrayObserveRequest};
        use std::future::Future as _;
        use std::task::{Context, Waker};
        struct MustNotSerialize;
        impl Serialize for MustNotSerialize {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                panic!("disabled capture serialized payload");
            }
        }
        record("disabled_fixture", &MustNotSerialize);
        let hub = XrayHub::default();
        let _shallow = hub
            .observe(XrayObserveRequest {
                schema_version: XRAY_SCHEMA_VERSION,
                session_id: "fixture".into(),
                deep_capture: false,
            })
            .unwrap();
        let deep = hub
            .observe(XrayObserveRequest {
                schema_version: XRAY_SCHEMA_VERSION,
                session_id: "foreign".into(),
                deep_capture: true,
            })
            .unwrap();
        let capture = hub.context(XrayCorrelation {
            session_id: "fixture".into(),
            ..Default::default()
        });
        let mut future = std::pin::pin!(capture.scope(async {
            record("shallow_fixture", &MustNotSerialize);
            drop(deep);
            record("detached_fixture", &MustNotSerialize);
        }));
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
    }

    #[test]
    fn structurally_redacts_nested_credentials_without_rewriting_content() {
        let mut value = json!({"authorization":"Bearer private", "nested":[{"api_key":"private", "refresh-token":"private", "text":"sensitive user content"}], "usage":{"input_tokens":7}});
        redact(&mut value);
        assert_eq!(value["authorization"], "[REDACTED]");
        assert_eq!(value["nested"][0]["api_key"], "[REDACTED]");
        assert_eq!(value["nested"][0]["refresh-token"], "[REDACTED]");
        assert_eq!(value["nested"][0]["text"], "sensitive user content");
        assert_eq!(value["usage"]["input_tokens"], 7);
    }

    #[test]
    fn protected_state_is_redacted_but_semantic_content_survives() {
        let mut value = json!({
            "items": [
                {"type":"reasoning", "text":"reason", "signature":"private", "encrypted_content":"private"},
                {"type":"provider_metadata", "key":"optimization", "value":"private"},
                {"type":"provider_extension", "value":{"arbitrary":"private"}},
                {"type":"tool_call", "value":{"answer":42}},
                {"redactedContent":"private", "thought_signature":"private"}
            ],
            "usage":{"input_tokens":7}, "provider_metadata":{"arbitrary":"private"}
        });
        redact(&mut value);
        assert!(!value.to_string().contains("private"));
        assert_eq!(value["items"][0]["text"], "reason");
        assert_eq!(value["items"][1]["type"], "provider_metadata");
        assert_eq!(value["items"][3]["value"]["answer"], 42);
        assert_eq!(value["usage"]["input_tokens"], 7);
    }

    #[test]
    fn transport_error_bodies_are_not_diagnostic_content() {
        let mut value = json!([
            {"type":"error", "error":{"message":"private bearer", "category":"network"}},
            {"type":"warning", "message":"private endpoint"},
            {"type":"retry_scheduled", "reason":"private token"},
            {"error":{"message":"private raw transport"}, "text":"user semantic text"},
            {"type":"text_delta", "text":"user semantic text"}
        ]);
        redact(&mut value);
        assert!(!value.to_string().contains("private"));
        assert_eq!(value[0], json!({"type":"error"}));
        assert_eq!(value[3]["text"], "user semantic text");
        assert_eq!(value[4]["text"], "user semantic text");
    }

    #[test]
    fn bounded_writer_never_retains_a_partial_oversized_write() {
        let mut writer = BoundedJson(Vec::new());
        writer.write_all(b"{").unwrap();
        assert!(writer.write_all(&vec![b'x'; MAX_PAYLOAD_BYTES]).is_err());
        assert_eq!(writer.0, b"{");
    }
}
