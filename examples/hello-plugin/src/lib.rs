#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Example native Bcode plugin.

use bcode_plugin_sdk::prelude::*;

/// Example plugin used by smoke tests.
#[derive(Default)]
pub struct HelloPlugin;

impl RustPlugin for HelloPlugin {
    fn activate(&mut self) -> Result<(), PluginError> {
        Ok(())
    }

    fn deactivate(&mut self) -> Result<(), PluginError> {
        Ok(())
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id == "example-hello/v1" && context.request.operation == "echo"
        {
            return ServiceResponse::ok(context.request.payload);
        }
        ServiceResponse::error(
            "unsupported_operation",
            "unsupported hello service operation",
        )
    }
}

bcode_plugin_sdk::export_plugin!(HelloPlugin, include_str!("../bcode-plugin.toml"));
