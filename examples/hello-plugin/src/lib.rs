#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Example native Bcode plugin.

use bcode_plugin_sdk::prelude::*;

/// Example plugin used by smoke tests.
#[derive(Default)]
pub struct HelloPlugin {
    event_count: usize,
}

impl RustPlugin for HelloPlugin {
    fn activate(&mut self) -> Result<(), PluginError> {
        Ok(())
    }

    fn deactivate(&mut self) -> Result<(), PluginError> {
        Ok(())
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != "example-hello/v1" {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported hello service interface",
            );
        }
        match context.request.operation.as_str() {
            "echo" => ServiceResponse::ok(context.request.payload),
            "event-count" => ServiceResponse::text(self.event_count.to_string()),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported hello service operation",
            ),
        }
    }

    fn handle_event(&mut self, context: NativeEventContext) -> Result<(), PluginError> {
        if context.event.topic == "example.event" || context.event.topic == "bcode.session.event" {
            self.event_count += 1;
        }
        Ok(())
    }
}

bcode_plugin_sdk::export_plugin!(HelloPlugin, include_str!("../bcode-plugin.toml"));
