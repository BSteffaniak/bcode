//! Observation-only hooks at provider-owned execution boundaries.

use bcode_model::diagnostics::ProviderDiagnosticPayload;
use bcode_model::{ModelTurnRequest, ProviderTurnEvent};
use bcode_xray::CaptureContext;
use serde::Serialize;
use serde_json::json;
use std::future::Future;
use std::io::Write;

/// Maximum encoded diagnostic payload bytes, before envelope overhead.
/// Oversized payloads produce explicit omission events, never partial JSON.
pub const MAX_PROVIDER_DIAGNOSTIC_BYTES: usize = 16 * 1024;

/// Originating capture context, retained explicitly across provider task boundaries.
///
/// This does not enable capture, alter usage accounting, select retries, or own execution.
/// Observation detach is checked by the shared engine on every emission.
#[derive(Clone, Default)]
pub struct ProviderDiagnostics {
    capture: Option<CaptureContext>,
}

impl std::fmt::Debug for ProviderDiagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderDiagnostics")
            .field("context_available", &self.capture.is_some())
            .finish()
    }
}

#[derive(Serialize)]
struct RequestBody<'a> {
    system_prompt: &'a Option<String>,
    messages: &'a [bcode_model::ModelMessage],
    tools: &'a [bcode_model::ToolDefinition],
}

impl ProviderDiagnostics {
    /// Capture the caller's context before spawning work or crossing plugin dispatch.
    #[must_use]
    pub fn current() -> Self {
        Self::new(CaptureContext::current())
    }

    /// Bind an explicit originating context. Absence is a no-op, not a global fallback.
    #[must_use]
    pub const fn new(capture: Option<CaptureContext>) -> Self {
        Self { capture }
    }

    /// Return the context for explicit propagation into a provider-created task.
    // Option is already must-use; repository policy forbids redundant must-use attributes.
    #[allow(clippy::must_use_candidate)]
    pub const fn capture_context(&self) -> Option<&CaptureContext> {
        self.capture.as_ref()
    }

    /// Run work in the originating scope; Tokio task locals do not inherit across spawn.
    pub async fn scope<F: Future>(self, future: F) -> F::Output {
        match self.capture {
            Some(capture) => capture.scope(future).await,
            None => future.await,
        }
    }

    /// Capture a provider-owned projection lazily, only during active deep observation.
    ///
    /// The closure must select body fields only, never headers, authentication, configuration,
    /// unrestricted errors, or transport objects. A capture failure never fails provider work.
    pub fn payload<P: ProviderDiagnosticPayload>(&self, build: impl FnOnce() -> P) {
        if P::SCHEMA_VERSION == 0 || P::FORMAT.is_empty() || P::FORMAT.len() > 128 {
            return;
        }
        self.sensitive(P::STAGE.event_name(), || {
            Ok(json!({
                "format": P::FORMAT,
                "schema_version": P::SCHEMA_VERSION,
                "payload": bounded_value(&build())?,
            }))
        });
    }

    /// Observe the normalized request, excluding routing/auth metadata and provider settings.
    /// This is not the adapted wire request and must not be presented as delivery evidence.
    pub fn request(&self, request: &ModelTurnRequest) {
        if !self
            .capture
            .as_ref()
            .is_some_and(CaptureContext::capture_active)
        {
            return;
        }
        self.emit(
            "request_started",
            json!({
                "message_count": request.messages.len(),
                "tool_count": request.tools.len(),
            }),
        );
        self.sensitive("normalized_request", || {
            bounded_value(&RequestBody {
                system_prompt: &request.system_prompt,
                messages: &request.messages,
                tools: &request.tools,
            })
        });
    }

    /// Observe an accepted event without changing its ordering or accounting semantics.
    /// Arbitrary errors, metadata, warning/retry messages and private usage are excluded.
    pub fn event(&self, event: &ProviderTurnEvent) {
        if !self
            .capture
            .as_ref()
            .is_some_and(CaptureContext::capture_active)
        {
            return;
        }
        match event {
            ProviderTurnEvent::TurnStarted => self.emit("turn_started", json!({})),
            ProviderTurnEvent::TurnFinished { stop_reason } => {
                self.emit("turn_finished", json!({ "stop_reason": stop_reason }));
            }
            ProviderTurnEvent::Cancelled => self.emit("cancelled", json!({})),
            ProviderTurnEvent::RetryScheduled { retry_at_unix, .. } => {
                self.emit("retry_scheduled", json!({ "retry_at_unix": retry_at_unix }));
            }
            ProviderTurnEvent::Usage { usage } => self.emit(
                "usage",
                json!({
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "total_tokens": usage.total_tokens,
                    "cached_input_tokens": usage.cached_input_tokens,
                    "cache_write_input_tokens": usage.cache_write_input_tokens,
                    "reasoning_tokens": usage.reasoning_tokens,
                    "semantics": "final_cumulative_per_request",
                }),
            ),
            ProviderTurnEvent::Error { error } => {
                self.emit("error", json!({ "category": error.category }));
            }
            ProviderTurnEvent::Output { .. }
            | ProviderTurnEvent::TextDelta { .. }
            | ProviderTurnEvent::ReasoningDelta { .. }
            | ProviderTurnEvent::ReasoningActivity { .. }
            | ProviderTurnEvent::ToolCallStarted { .. }
            | ProviderTurnEvent::ToolCallDelta { .. }
            | ProviderTurnEvent::ToolCallFinished { .. } => {
                self.emit("stream_event", json!({}));
                self.sensitive("normalized_stream_event", || bounded_value(event));
            }
            // These contracts may carry opaque provider content. Their owners must supply an
            // explicit diagnostic projection instead of bulk-serializing them here.
            _ => {}
        }
    }

    /// Record a cancellation request, not proof of provider cancellation or release.
    pub fn cancellation_requested(&self) {
        self.emit("cancellation_requested", json!({}));
    }

    fn emit(&self, name: &str, data: serde_json::Value) {
        if let Some(capture) = &self.capture {
            capture.emit("provider", name, data, false);
        }
    }

    fn sensitive(&self, name: &str, build: impl FnOnce() -> Result<serde_json::Value, ()>) {
        let Some(capture) = &self.capture else { return };
        if !capture.deep_capture_active() {
            return;
        }
        match build() {
            Ok(data) => capture.emit("provider", name, data, true),
            Err(()) => capture.emit(
                "provider",
                "payload_omitted",
                json!({
                    "event": name,
                    "reason": "encoding_failed_or_budget_exceeded",
                    "max_bytes": MAX_PROVIDER_DIAGNOSTIC_BYTES,
                    "complete": false,
                }),
                false,
            ),
        }
    }
}

fn bounded_value(value: &impl Serialize) -> Result<serde_json::Value, ()> {
    let mut writer = BoundedJson(Vec::new());
    serde_json::to_writer(&mut writer, value).map_err(|_| ())?;
    serde_json::from_slice(&writer.0).map_err(|_| ())
}

struct BoundedJson(Vec<u8>);

impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_PROVIDER_DIAGNOSTIC_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("diagnostic byte budget exceeded"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod budget_tests {
    use super::*;
    use bcode_model::diagnostics::ProviderDiagnosticStage;

    #[derive(Serialize)]
    struct Payload;
    impl ProviderDiagnosticPayload for Payload {
        const FORMAT: &'static str = "test.body";
        const SCHEMA_VERSION: u16 = 1;
        const STAGE: ProviderDiagnosticStage = ProviderDiagnosticStage::RequestAdapted;
    }

    #[test]
    fn disabled_capture_does_not_construct_payloads() {
        ProviderDiagnostics::default().payload::<Payload>(|| panic!("must stay lazy"));
    }

    #[test]
    fn oversized_json_is_omitted_instead_of_truncated() {
        assert!(bounded_value(&"x".repeat(MAX_PROVIDER_DIAGNOSTIC_BYTES)).is_err());
        assert_eq!(bounded_value(&"small"), Ok(json!("small")));
    }

    #[test]
    fn byte_budget_bounds_buffer_even_after_failure() {
        let mut writer = BoundedJson(Vec::new());
        writer
            .write_all(&[b'x'; MAX_PROVIDER_DIAGNOSTIC_BYTES])
            .unwrap();
        assert!(writer.write_all(b"x").is_err());
        assert_eq!(writer.0.len(), MAX_PROVIDER_DIAGNOSTIC_BYTES);
    }
}
