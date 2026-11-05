//! Typed harness service adapter for native plugins.
//!
//! Plugins declare `bcode.harness/v1` in their ordinary manifest `services` list.
//! Registration uses that existing manifest boundary, not a new native ABI callback.
//! The host remains responsible for admission, credential delivery and ownership fencing.

use crate::{NativeServiceContext, ServiceResponse};
pub use bcode_harness_models::*;

/// Stateful plugin implementation of the managed execution service.
///
/// Implementations continuously drain native streams independently of observation calls.
/// They must retain bounded live correlation state, validate outstanding interactions,
/// and never replay uncertain mutations or dispatch observed native tools as Bcode tools.
/// Authentication handling is ephemeral and excluded from event/log publication.
pub trait HarnessService {
    /// Execute exactly one validated semantic operation.
    ///
    /// Return sanitized failures, not raw native protocol errors. Mutation admission is
    /// host-owned; this hook does not grant caller authority or widen plugin permissions.
    fn handle_harness(&mut self, request: &HarnessRequest) -> HarnessServiceResponse;
}

/// Decode one bounded invocation and validate operation and plugin ownership before dispatch.
///
/// # Errors
/// Returns secret-safe contract errors for oversize/invalid JSON, operation mismatch,
/// invalid identities, unsupported versions or a request claiming another plugin owner.
pub fn decode_harness_request(
    context: &NativeServiceContext,
) -> Result<HarnessRequest, HarnessContractError> {
    if context.request.interface_id != HARNESS_INTERFACE_ID {
        return Err(HarnessContractError::UnsupportedVersion);
    }
    if context.request.payload.len() > MAX_HARNESS_WIRE_BYTES {
        return Err(HarnessContractError::BoundsExceeded);
    }
    let request: HarnessRequest = serde_json::from_slice(&context.request.payload)
        .map_err(|_| HarnessContractError::InvalidShape)?;
    request.validate()?;
    if request.instance.plugin_id != context.plugin_id {
        return Err(HarnessContractError::WrongOwner);
    }
    if request.kind.operation().as_str() != context.request.operation {
        return Err(HarnessContractError::OperationMismatch);
    }
    Ok(request)
}

/// Dispatch a typed bounded harness request through an ordinary native service hook.
///
/// Error messages are deliberately static. This helper never emits request/response
/// payloads as service events, and validates the reply before encoding it. Cancellation
/// during a native mutation is not converted into a successful or no-effects receipt.
#[must_use]
pub fn dispatch_harness(
    context: &NativeServiceContext,
    service: &mut impl HarnessService,
) -> ServiceResponse {
    let Ok(request) = decode_harness_request(context) else {
        return ServiceResponse::error("harness.invalid_request", "invalid harness contract");
    };
    if context.cancellation.is_cancelled() {
        return ServiceResponse::error(
            "harness.cancelled",
            "harness invocation cancelled before dispatch",
        );
    }
    let response = service.handle_harness(&request);
    if response.validate_for(&request).is_err() {
        return ServiceResponse::error(
            "harness.invalid_response",
            "invalid harness response contract",
        );
    }
    ServiceResponse::json(&response).unwrap_or_else(|_| {
        ServiceResponse::error("harness.encode", "harness response encoding failed")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> NativeServiceContext {
        let request = HarnessRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: "inspect-1".into(),
            instance: HarnessInstance {
                instance_id: "instance".into(),
                plugin_id: "plugin".into(),
                executable: "/native/bin".into(),
                config_namespace: "/native/config".into(),
                auth: HarnessAuthBinding {
                    provider_id: "provider".into(),
                    profile: "profile".into(),
                    credential_namespace: "namespace".into(),
                    custody: HarnessAuthCustody::NativeManaged,
                },
            },
            kind: HarnessRequestKind::Inspect,
        };
        NativeServiceContext {
            plugin_id: "plugin".into(),
            request: crate::ServiceRequest {
                interface_id: HARNESS_INTERFACE_ID.into(),
                operation: "inspect".into(),
                payload: serde_json::to_vec(&request).unwrap(),
            },
            config: crate::PluginConfigContext::default(),
            events: crate::ServiceEventEmitter::default(),
            cancellation: crate::ServiceCancellation::default(),
            bridge: crate::ServiceBridge::default(),
            transient_progress_limits: crate::TransientProgressLimits::default(),
            diagnostics: None,
        }
    }
    #[test]
    fn dispatch_never_calls_handler_for_a_foreign_owner() {
        struct Handler {
            calls: usize,
        }
        impl HarnessService for Handler {
            fn handle_harness(&mut self, request: &HarnessRequest) -> HarnessServiceResponse {
                self.calls += 1;
                HarnessServiceResponse {
                    schema_version: HARNESS_SCHEMA_VERSION,
                    request_id: request.request_id.clone(),
                    operation: request.kind.operation(),
                    kind: HarnessServiceResponseKind::Failure(HarnessFailure::Unsupported),
                }
            }
        }
        let mut service = Handler { calls: 0 };
        let mut context = context();
        context.plugin_id = "foreign".into();
        assert!(dispatch_harness(&context, &mut service).error.is_some());
        assert_eq!(service.calls, 0);
        context.plugin_id = "plugin".into();
        assert!(dispatch_harness(&context, &mut service).error.is_none());
        assert_eq!(service.calls, 1);
        context.cancellation.cancel();
        assert!(dispatch_harness(&context, &mut service).error.is_some());
        assert_eq!(service.calls, 1);
    }
    #[test]
    fn decoding_fences_plugin_operation_and_wire_size() {
        let mut context = context();
        assert!(decode_harness_request(&context).is_ok());
        context.plugin_id = "other".into();
        assert_eq!(
            decode_harness_request(&context).unwrap_err(),
            HarnessContractError::WrongOwner
        );
        context.plugin_id = "plugin".into();
        context.request.operation = "submit".into();
        assert_eq!(
            decode_harness_request(&context).unwrap_err(),
            HarnessContractError::OperationMismatch
        );
        context.request.payload = vec![b' '; MAX_HARNESS_WIRE_BYTES + 1];
        assert_eq!(
            decode_harness_request(&context).unwrap_err(),
            HarnessContractError::BoundsExceeded
        );
    }
}
