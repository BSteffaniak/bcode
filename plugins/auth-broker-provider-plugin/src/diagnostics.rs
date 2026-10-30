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
        context.emit("provider.auth_broker", "payload_omitted", json!({"capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION, "record": name, "reason": "serialization_or_size_limit", "limit_bytes": MAX_PAYLOAD_BYTES}), false);
        return;
    }
    emit_json(&context, name, &writer.0);
}

fn emit_json(context: &CaptureContext, name: &str, bytes: &[u8]) {
    let Ok(mut value) = serde_json::from_slice::<Value>(bytes) else {
        context.emit("provider.auth_broker", "payload_omitted", json!({"capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION, "record": name, "reason": "invalid_or_excessively_nested_json"}), false);
        return;
    };
    redact(&mut value);
    // Serialize again only after redaction. Never retain unsanitized input bytes.
    let mut sanitized = BoundedJson(Vec::new());
    if serde_json::to_writer(&mut sanitized, &value).is_err() {
        context.emit("provider.auth_broker", "payload_omitted", json!({"capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION, "record": name, "reason": "sanitized_size_limit", "limit_bytes": MAX_PAYLOAD_BYTES}), false);
        return;
    }
    if sanitized.0.len() <= INLINE_BYTES {
        context.emit(
            "provider.auth_broker",
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
        Ok(Some(reference)) => context.emit("provider.auth_broker", name, json!({
            "capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION,
            "representation": "redacted_json_payload_ref", "payload_ref": reference,
            "redaction": "structural_credentials_v1"
        }), true),
        Ok(None) => {}, // Observation ended during capture; never retain after detach.
        Err(_) => context.emit("provider.auth_broker", "payload_omitted", json!({
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
        context.emit("provider.auth_broker", "payload_omitted", json!({"capture_format": CAPTURE_FORMAT, "capture_version": CAPTURE_VERSION, "record": name, "reason": "size_limit", "byte_len": bytes.len(), "limit_bytes": MAX_PAYLOAD_BYTES}), false);
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

#[cfg(test)]
mod tests {
    use super::*;

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
