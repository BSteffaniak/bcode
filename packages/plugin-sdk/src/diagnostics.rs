//! Optional native diagnostic callback protocol. This is not a service event schema.
//!
//! The binary discriminators reserve a separate negotiated channel on the existing callback
//! ABI. Old SDKs ignore negotiation and continue ordinary stream semantics. Unknown modes or
//! versions disable diagnostics. Neither payload correlation nor activity hints authorize access.

#[cfg(test)]
mod tests;

use crate::{SERVICE_BRIDGE_STATUS_OK, ServiceBridgeCallback, ServiceEventEmitter};
use bcode_xray::{CaptureContext, XrayCaptureSink, XrayCorrelation, XrayEvent};
use serde::{Deserialize, Serialize};
use std::ffi::c_void;
use std::sync::Arc;

/// Version of the optional callback protocol, independent of service/plugin ABI versions.
pub const NATIVE_DIAGNOSTICS_VERSION: u16 = 1;
/// Hard encoded JSON event bound, excluding discriminator.
pub const MAX_NATIVE_DIAGNOSTIC_BYTES: usize = 64 * 1024;
/// Reserved diagnostic callback discriminator; never deliver it as a normal service event.
pub const DIAGNOSTIC_EVENT_PREFIX: &[u8] = b"\0bcode.diagnostic.event.v1\0";
/// Reserved diagnostic control discriminator; never route it to an application bridge.
pub const DIAGNOSTIC_CONTROL_PREFIX: &[u8] = b"\0bcode.diagnostic.control.v1\0";

/// Optional serialized host negotiation. Contains no auth/config/request content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeDiagnosticContext {
    /// Supported protocol version. Unknown versions are disabled.
    pub version: u16,
    /// Explicit side-channel mode. Unknown modes are disabled.
    pub mode: NativeDiagnosticMode,
    /// Informational host origin for plugin-side instrumentation, never inbound authority.
    pub correlation: XrayCorrelation,
    /// Host's maximum JSON event size, capped by the protocol hard bound.
    pub max_event_bytes: usize,
}

/// Unknown or malformed optional negotiation disables capture, not the service operation.
pub(super) fn deserialize_negotiation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<NativeDiagnosticContext>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

/// Diagnostic transport mode, not an authorization decision.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeDiagnosticMode {
    /// Version-1 event callback with independently retained diagnostic lease.
    CallbackV1,
    /// Future or unavailable transports do not emit.
    #[serde(other)]
    Disabled,
}

/// Typed lease/control operations over the existing bridge callback ABI.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticControlOperation {
    Acquire,
    State,
    Release,
}

/// A bounded control request. It cannot name a session, payload path, or another lease.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticControl {
    pub version: u16,
    pub operation: DiagnosticControlOperation,
}

/// Activity hints from the host's retained origin, rechecked again for every delivery.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticStatus {
    pub version: u16,
    pub active: bool,
    pub deep_active: bool,
}

/// One protected side-channel event. Never a `ServiceResponse` or ordinary service event.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeDiagnosticEvent {
    pub version: u16,
    pub event: XrayEvent,
}

struct CallbackSink {
    events: ServiceEventEmitter,
    callback: ServiceBridgeCallback,
    user_data: usize,
    max_bytes: usize,
    acquired: bool,
}

impl CallbackSink {
    fn control(&self, operation: DiagnosticControlOperation) -> Option<DiagnosticStatus> {
        let request = DiagnosticControl {
            version: NATIVE_DIAGNOSTICS_VERSION,
            operation,
        };
        let mut bytes = DIAGNOSTIC_CONTROL_PREFIX.to_vec();
        serde_json::to_writer(&mut bytes, &request).ok()?;
        let mut output = [0_u8; 256];
        let mut length = 0;
        let status = (self.callback)(
            bytes.as_ptr(),
            bytes.len(),
            output.as_mut_ptr(),
            output.len(),
            &raw mut length,
            self.user_data as *mut c_void,
        );
        if status != SERVICE_BRIDGE_STATUS_OK || length > output.len() {
            return None;
        }
        let status: DiagnosticStatus = serde_json::from_slice(&output[..length]).ok()?;
        (status.version == NATIVE_DIAGNOSTICS_VERSION).then_some(status)
    }
}

impl Drop for CallbackSink {
    fn drop(&mut self) {
        if self.acquired {
            let _ = self.control(DiagnosticControlOperation::Release);
        }
    }
}

impl XrayCaptureSink for CallbackSink {
    fn capture_active(&self) -> bool {
        self.control(DiagnosticControlOperation::State)
            .is_some_and(|s| s.active)
    }
    fn deep_capture_active(&self) -> bool {
        self.control(DiagnosticControlOperation::State)
            .is_some_and(|s| s.deep_active)
    }
    fn emit(&self, event: XrayEvent) {
        let Some(status) = self.control(DiagnosticControlOperation::State) else {
            return;
        };
        if !status.active || (event.sensitive && !status.deep_active) {
            return;
        }
        let envelope = NativeDiagnosticEvent {
            version: NATIVE_DIAGNOSTICS_VERSION,
            event,
        };
        let mut encoded = BudgetWriter {
            bytes: Vec::new(),
            remaining: self.max_bytes,
        };
        if serde_json::to_writer(&mut encoded, &envelope).is_err() {
            // Do not emit partial JSON. Fixed omission is safe even when the original is sensitive.
            let omission = NativeDiagnosticEvent {
                version: NATIVE_DIAGNOSTICS_VERSION,
                event: XrayEvent {
                    domain: "plugin".into(),
                    name: "diagnostic_payload_omitted".into(),
                    correlation: envelope.event.correlation,
                    data: serde_json::json!({"reason":"encoding_failed_or_budget_exceeded","complete":false}),
                    sensitive: false,
                },
            };
            encoded = BudgetWriter {
                bytes: Vec::new(),
                remaining: self.max_bytes,
            };
            if serde_json::to_writer(&mut encoded, &omission).is_err() {
                return;
            }
        }
        let mut bytes = DIAGNOSTIC_EVENT_PREFIX.to_vec();
        bytes.extend_from_slice(&encoded.bytes);
        self.events.emit(&bytes);
    }
}

struct DisabledSink;
impl XrayCaptureSink for DisabledSink {
    fn capture_active(&self) -> bool {
        false
    }
    fn deep_capture_active(&self) -> bool {
        false
    }
    fn emit(&self, _: XrayEvent) {}
}

/// Enter the negotiated scope, explicitly masking ambient capture when unavailable.
/// Engine synchronous scopes must take precedence over ambient async scope for the closure.
pub(super) fn scope<R>(capture: Option<CaptureContext>, invoke: impl FnOnce() -> R) -> R {
    capture
        .unwrap_or_else(|| {
            CaptureContext::from_sink(XrayCorrelation::default(), Arc::new(DisabledSink))
        })
        .scope_sync(invoke)
}

struct BudgetWriter {
    bytes: Vec<u8>,
    remaining: usize,
}
impl std::io::Write for BudgetWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("diagnostic budget exceeded"))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Reconstruct the explicitly negotiated callback-backed context in this SDK/dylib instance.
/// The sink holds only host callback parts, never a pointer to invocation stack state.
/// Host lease retention is independent from ordinary bridge/event lifetime.
pub(super) fn capture_context(
    negotiation: Option<&NativeDiagnosticContext>,
    events: ServiceEventEmitter,
    callback: Option<ServiceBridgeCallback>,
    user_data: *mut c_void,
) -> Option<CaptureContext> {
    let negotiation = negotiation?;
    if negotiation.version != NATIVE_DIAGNOSTICS_VERSION
        || negotiation.mode != NativeDiagnosticMode::CallbackV1
        || !events.is_available()
        || user_data.is_null()
        || !(1024..=MAX_NATIVE_DIAGNOSTIC_BYTES).contains(&negotiation.max_event_bytes)
    {
        return None;
    }
    let mut sink = CallbackSink {
        events,
        callback: callback?,
        user_data: user_data as usize,
        max_bytes: negotiation.max_event_bytes,
        acquired: false,
    };
    sink.control(DiagnosticControlOperation::Acquire)?;
    sink.acquired = true;
    Some(CaptureContext::from_sink(
        negotiation.correlation.clone(),
        Arc::new(sink),
    ))
}
