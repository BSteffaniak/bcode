//! Provider usage-service dispatch without protocol or plugin-specific policy.

use bcode_model::UsageDecoder;
use bcode_plugin_sdk::{ServiceRequest, ServiceResponse};

/// Serve offline normalization from the provider's registered decoders.
#[must_use]
pub fn normalize_usage_service(
    provider: &str,
    decoders: &[&dyn UsageDecoder],
    request: &ServiceRequest,
) -> ServiceResponse {
    let result = request
        .payload_json::<bcode_session_models::OriginalUsage>()
        .map_err(|_| "invalid usage evidence".to_owned())
        .and_then(|original| super::normalize_registered_usage(provider, decoders, &original));
    result.map_or_else(
        |_| {
            ServiceResponse::error(
                "usage_normalization_failed",
                "original usage is incomplete, unsupported, or invalid",
            )
        },
        |usage| {
            ServiceResponse::json(&usage).unwrap_or_else(|_| {
                ServiceResponse::error("usage_encode_failed", "normalized usage encoding failed")
            })
        },
    )
}
