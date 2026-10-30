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
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
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

/// Capture a bounded JSON projection only while deep observation is active.
pub fn record(name: &str, payload: &impl Serialize) {
    let Some(context) = CaptureContext::current().filter(CaptureContext::deep_capture_active)
    else {
        return;
    };
    let mut writer = BoundedJson(Vec::new());
    if serde_json::to_writer(&mut writer, payload).is_err() {
        context.emit("provider.openai_compatible", "payload_omitted", json!({"capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION, "record": name, "reason": "serialization_or_size_limit", "limit_bytes": MAX_PAYLOAD_BYTES}), false);
        return;
    }
    emit_json(&context, name, &writer.0);
}

fn emit_json(context: &CaptureContext, name: &str, bytes: &[u8]) {
    let Ok(mut value) = serde_json::from_slice::<Value>(bytes) else {
        context.emit("provider.openai_compatible", "payload_omitted", json!({"capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION, "record": name, "reason": "invalid_or_excessively_nested_json"}), false);
        return;
    };
    redact(&mut value);
    // Serialize again only after redaction. Never retain unsanitized input bytes.
    let mut sanitized = BoundedJson(Vec::new());
    if serde_json::to_writer(&mut sanitized, &value).is_err() {
        context.emit("provider.openai_compatible", "payload_omitted", json!({"capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION, "record": name, "reason": "sanitized_size_limit", "limit_bytes": MAX_PAYLOAD_BYTES}), false);
        return;
    }
    if sanitized.0.len() <= INLINE_BYTES {
        context.emit(
            "provider.openai_compatible",
            name,
            json!({
                "capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION,
                "representation": "redacted_json", "redaction": "structural_credentials_v1",
                "value": value
            }),
            true,
        );
        return;
    }
    match context.capture_payload("application/json", &sanitized.0) {
        Ok(Some(reference)) => context.emit("provider.openai_compatible", name, json!({
            "capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION,
            "representation": "redacted_json_payload_ref", "payload_ref": reference,
            "redaction": "structural_credentials_v1"
        }), true),
        Ok(None) => {}, // Observation ended during capture; never retain after detach.
        Err(_) => context.emit("provider.openai_compatible", "payload_omitted", json!({
            "capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION,
            "record": name, "reason": "payload_retention_unavailable", "byte_len": sanitized.0.len()
        }), false),
    }
}

/// Capture a bounded received JSON record after structural credential redaction.
pub fn bytes(name: &str, bytes: &[u8]) {
    let Some(context) = CaptureContext::current().filter(CaptureContext::deep_capture_active)
    else {
        return;
    };
    if bytes.len() > MAX_PAYLOAD_BYTES {
        context.emit("provider.openai_compatible", "payload_omitted", json!({"capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION, "record": name, "reason": "size_limit", "byte_len": bytes.len(), "limit_bytes": MAX_PAYLOAD_BYTES}), false);
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
    async fn received_sse_records_keep_provider_shape_and_redact_credentials() {
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
            request_id: Some("request-1".into()),
            ..Default::default()
        });
        capture.scope(async {
            // scope() captures here, before the task is handed to another executor.
            let task = scope(async {
                stream_line("data: {\"type\":\"response.output_text.delta\",\"delta\":\"fixture text\",\"api_key\":\"fixture-secret\"}");
                stream_line("data: [DONE]");
                stream_line("data: malformed fixture-secret");
            });
            tokio::spawn(task).await.unwrap();
        }).await;
        observation.finalize().unwrap();
        assert_eq!(observation.recv().await.unwrap().event.name, "ready");
        let record = observation.recv().await.unwrap();
        assert_eq!(record.event.name, "stream_record");
        assert_eq!(
            record.event.data["value"]["type"],
            "response.output_text.delta"
        );
        assert_eq!(record.event.data["value"]["delta"], "fixture text");
        assert_eq!(record.event.data["value"]["api_key"], "[REDACTED]");
        assert_eq!(
            record.event.correlation.request_id.as_deref(),
            Some("request-1")
        );
        assert_eq!(
            observation.recv().await.unwrap().event.name,
            "stream_terminal"
        );
        let omission = observation.recv().await.unwrap();
        assert_eq!(omission.event.name, "payload_omitted");
        assert_eq!(
            omission.event.data["reason"],
            "invalid_or_excessively_nested_json"
        );
        assert!(!omission.event.data.to_string().contains("fixture-secret"));
        assert_eq!(observation.recv().await.unwrap().event.name, "finalized");
        assert!(observation.recv().await.is_none());
    }

    #[test]
    fn no_observation_does_not_even_serialize_payload() {
        struct MustNotSerialize;
        impl Serialize for MustNotSerialize {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                panic!("disabled capture serialized payload");
            }
        }
        record("disabled_fixture", &MustNotSerialize);
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
    fn bounded_writer_never_retains_a_partial_oversized_write() {
        let mut writer = BoundedJson(Vec::new());
        writer.write_all(b"{").unwrap();
        assert!(writer.write_all(&vec![b'x'; MAX_PAYLOAD_BYTES]).is_err());
        assert_eq!(writer.0, b"{");
    }
}
