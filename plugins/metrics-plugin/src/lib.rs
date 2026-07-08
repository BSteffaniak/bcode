//! Metrics dashboard plugin for Bcode.

pub mod metrics_dashboard;
pub mod tui;

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
};
use bcode_plugin_sdk::prelude::*;
use serde::Serialize;
use std::collections::BTreeSet;

const PLUGIN_ID: &str = "bcode.metrics";
const COMMAND_OPEN_DASHBOARD: &str = "metrics.open_dashboard";
const DEFAULT_METRICS_ROOT: &str = ".local/state/bcode/metrics/events.jsonl";

/// Metrics plugin.
#[derive(Default)]
pub struct MetricsPlugin;

impl RustPlugin for MetricsPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        registrar
            .register(&open_dashboard_command())
            .map_err(|error| PluginError::failed(error.to_string()))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != COMMAND_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported metrics plugin service interface",
            );
        }
        invoke_command_service(&context.request)
    }
}

fn open_dashboard_command() -> CommandContribution {
    CommandContribution {
        id: COMMAND_OPEN_DASHBOARD.to_owned(),
        title: "Metrics: Open Dashboard".to_owned(),
        description: Some("Inspect persisted Bcode performance metrics".to_owned()),
        category: Some("metrics".to_owned()),
        surfaces: BTreeSet::from([CommandSurface::Palette, CommandSurface::Slash]),
        owner: CommandOwner::Plugin {
            plugin_id: PLUGIN_ID.to_owned(),
        },
        action: CommandAction::Plugin {
            plugin_id: PLUGIN_ID.to_owned(),
            command_id: COMMAND_OPEN_DASHBOARD.to_owned(),
        },
    }
}

fn invoke_command_service(request: &ServiceRequest) -> ServiceResponse {
    if request.operation != OP_INVOKE_COMMAND {
        return ServiceResponse::error("unsupported_operation", "unsupported command operation");
    }
    let Ok(request) = serde_json::from_slice::<InvokeCommandRequest>(&request.payload) else {
        return ServiceResponse::error("invalid_request", "invalid command request payload");
    };
    match request.command_id.as_str() {
        COMMAND_OPEN_DASHBOARD => {
            let session_id = request.args.get("session_id").cloned();
            let mut options = serde_json::json!({
                "metrics_path": DEFAULT_METRICS_ROOT,
            });
            if let Some(session_id) = session_id {
                options["session_id"] = serde_json::Value::String(session_id);
            }
            json_response(&InvokeCommandResponse {
                success: true,
                message: Some("Opening metrics dashboard".to_owned()),
                updated_model: None,
                updated_provider: None,
                updated_thinking: None,
                effects: vec![CommandEffect::OpenPluginSurface {
                    surface_kind: tui::METRICS_DASHBOARD_SURFACE_KIND.to_owned(),
                    instance_id: "metrics-dashboard".to_owned(),
                    options,
                }],
            })
        }
        _ => ServiceResponse::error("unknown_command", "unknown metrics command"),
    }
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    let mut vtable = bcode_plugin_sdk::static_plugin_vtable!(
        MetricsPlugin,
        include_str!("../bcode-plugin.toml")
    );
    vtable.tui_registry = Some(tui::tui_registry);
    vtable
}

bcode_plugin_sdk::export_plugin!(MetricsPlugin, include_str!("../bcode-plugin.toml"));
