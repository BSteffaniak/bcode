#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Single-owner GitHub OIDC broker. Provider credentials never enter this plugin.
mod config;
mod http;
mod leases;
mod oidc;

use bcode_plugin_sdk::prelude::*;
pub use config::{ServeRequest, TrustedWorkflow};

/// Service interface owned by this plugin.
pub const INTERFACE_ID: &str = "bcode.auth-broker/v1";

/// Long-lived broker service plugin.
#[derive(Default)]
pub struct AuthBrokerPlugin;

impl RustPlugin for AuthBrokerPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != INTERFACE_ID || context.request.operation != "serve" {
            return ServiceResponse::error("unsupported_operation", "unsupported broker operation");
        }
        let Ok(config) = context.request.payload_json::<ServeRequest>() else {
            return ServiceResponse::error("invalid_request", "invalid broker configuration");
        };
        if let Err(message) = config.validate() {
            return ServiceResponse::error("invalid_request", message);
        }
        let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        else {
            return ServiceResponse::error("runtime_unavailable", "broker runtime unavailable");
        };
        match runtime.block_on(http::serve(config, context.bridge, context.cancellation)) {
            Ok(()) => {
                ServiceResponse::json(&bcode_model::AckResponse::default()).unwrap_or_else(|_| {
                    ServiceResponse::error("encode_failed", "broker result encoding failed")
                })
            }
            Err(message) => ServiceResponse::error("broker_failed", message),
        }
    }
}

/// Static bundled entry point.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(AuthBrokerPlugin, include_str!("../bcode-plugin.toml"))
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(AuthBrokerPlugin, include_str!("../bcode-plugin.toml"));
