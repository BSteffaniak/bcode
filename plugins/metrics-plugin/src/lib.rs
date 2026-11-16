//! Metrics dashboard plugin for Bcode.

mod cli;

/// Return the plugin-owned CLI grammar and invocation adapter for either packaging mode.
#[must_use]
pub fn cli_registration() -> bcode_plugin_sdk::StaticCliRegistration {
    cli::registration()
}

pub mod metrics_dashboard;
pub mod tui;

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
    SlashCommandContribution,
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
    fn component_registrations() -> bcode_plugin_sdk::definition::ProductRegistrations {
        product_registrations()
    }

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
        slash: Some(SlashCommandContribution {
            name: "metrics".to_owned(),
            aliases: BTreeSet::new(),
        }),
        arguments: Vec::new(),
        session: bcode_command::CommandSessionRequirement::Optional,
        execution: bcode_command::CommandExecution::Normal,
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
            let mut options = serde_json::json!({
                "metrics_path": DEFAULT_METRICS_ROOT,
            });
            let mut filters = Vec::new();
            for (key, value) in &request.args {
                if let Some(label_key) = key
                    .strip_prefix("label.")
                    .or_else(|| key.strip_prefix("filter."))
                {
                    filters.push(serde_json::json!({
                        "target": { "label": label_key },
                        "op": "equals",
                        "value": value,
                    }));
                }
            }
            if let Some(session_id) = request
                .context
                .as_ref()
                .and_then(|context| context.session_id)
            {
                filters.push(serde_json::json!({
                    "target": { "label": "session_id" },
                    "op": "equals",
                    "value": session_id.to_string(),
                }));
            }
            if !filters.is_empty() {
                options["query"] = serde_json::json!({ "filters": filters });
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

#[cfg(test)]
mod shared_definition_tests {
    #[test]
    fn cli_registration_is_available_without_static_packaging() {
        let registration = super::cli_registration();
        let matches = (registration.command)()
            .try_get_matches_from(["metrics", "--json"])
            .expect("metrics grammar");
        assert!((registration.requires_daemon)(&matches));
        assert!(matches.get_flag("json"));
        assert_eq!(
            super::plugin_definition().metadata.plugin_id,
            "bcode.metrics"
        );
    }
}

/// Component capabilities retained by the same shared plugin instance.
#[must_use]
pub fn product_registrations() -> bcode_plugin_sdk::definition::ProductRegistrations {
    bcode_plugin_sdk::definition::ProductRegistrations {
        cli: Some(cli_registration()),
        portable_cli: Some(cli::invoke_portable),
        frontend: Some(tui::tui_registry),
        ..bcode_plugin_sdk::definition::ProductRegistrations::default()
    }
}

bcode_plugin_sdk::define_plugin!(
    serial MetricsPlugin, include_str!("../bcode-plugin.toml"),
    registrations = product_registrations(),
);
/// Bind host-authorized invocation capabilities to the shared definition.
#[must_use]
pub fn plugin_definition_with_host(
    host: impl Fn(
        bcode_plugin_sdk::definition::PluginCreateContext,
    ) -> std::sync::Arc<dyn bcode_plugin_sdk::definition::ProductInvocationHost>
    + Send
    + Sync
    + 'static,
) -> bcode_plugin_sdk::definition::PluginDefinition {
    bcode_plugin_sdk::definition::rust_plugin_definition_with_registrations::<MetricsPlugin>(
        plugin_definition().metadata,
        product_registrations(),
        host,
    )
}
