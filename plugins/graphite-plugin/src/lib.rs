#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Graphite stack discovery as a Bcode plugin: serves `bcode_code_review_graphite`'s contributor
//! as `bcode.review_discovery/v1` for agents and other plugins. The review itself links that crate
//! and calls it in-process.

use bcode_code_review_graphite::GraphiteDiscovery;
use bcode_code_review_models::REVIEW_DISCOVERY_INTERFACE_ID;
use bcode_code_review_provider::serve_discovery;
use bcode_plugin_sdk::prelude::*;

/// Graphite discovery plugin.
#[derive(Default)]
pub struct GraphitePlugin;

impl RustPlugin for GraphitePlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        invoke(&context.request)
    }
}

/// Handle one `bcode.review_discovery/v1` operation.
#[must_use]
pub fn invoke(request: &ServiceRequest) -> ServiceResponse {
    if request.interface_id != REVIEW_DISCOVERY_INTERFACE_ID {
        return ServiceResponse::error(
            "unsupported_interface",
            "unsupported Graphite service interface",
        );
    }
    match serve_discovery(&GraphiteDiscovery, &request.operation, &request.payload) {
        Ok(payload) => ServiceResponse::ok(payload),
        Err(error) => ServiceResponse::error(error.code, error.message),
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(GraphitePlugin, include_str!("../bcode-plugin.toml"))
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(GraphitePlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::{OP_REVIEW_DISCOVERY_MANIFEST, ReviewDiscoveryManifest};

    #[test]
    fn the_plugin_serves_the_contributor_on_its_interface_only() {
        let request = |interface: &str| ServiceRequest {
            interface_id: interface.into(),
            operation: OP_REVIEW_DISCOVERY_MANIFEST.into(),
            payload: b"{}".to_vec(),
        };
        let response = invoke(&request(REVIEW_DISCOVERY_INTERFACE_ID));
        assert!(response.error.is_none(), "{:?}", response.error);
        let manifest: ReviewDiscoveryManifest = serde_json::from_slice(&response.payload).unwrap();
        assert_eq!(
            manifest.id,
            bcode_code_review_graphite::GRAPHITE_DISCOVERY_ID
        );
        let other = invoke(&request("bcode.other/v1"));
        assert_eq!(other.error.unwrap().code, "unsupported_interface");
    }
}
