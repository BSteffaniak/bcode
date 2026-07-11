#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Eval run viewer plugin for Bcode.

#[cfg(feature = "static-bundled")]
mod cli;

pub mod eval_data;
pub mod eval_viewer;
pub mod tui;

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
};
use bcode_plugin_sdk::prelude::*;
use serde::Serialize;
use std::collections::BTreeSet;

const PLUGIN_ID: &str = "bcode.eval";
const COMMAND_OPEN_PICKER: &str = "eval.open_picker";
const COMMAND_OPEN_LATEST: &str = "eval.open_latest";
const DEFAULT_RUNS_ROOT: &str = "target/bcode-evals/runs";

/// Eval plugin.
#[derive(Default)]
pub struct EvalPlugin;

impl RustPlugin for EvalPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        registrar
            .register(&open_picker_command())
            .map_err(|error| PluginError::failed(error.to_string()))?;
        registrar
            .register(&open_latest_command())
            .map_err(|error| PluginError::failed(error.to_string()))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != COMMAND_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported eval plugin service interface",
            );
        }
        invoke_command_service(&context.request)
    }
}

fn open_picker_command() -> CommandContribution {
    CommandContribution {
        id: COMMAND_OPEN_PICKER.to_string(),
        title: "Eval: Open Run Picker".to_string(),
        description: Some("Browse completed eval runs in the TUI viewer".to_string()),
        category: Some("eval".to_string()),
        surfaces: BTreeSet::from([CommandSurface::Palette]),
        owner: CommandOwner::Plugin {
            plugin_id: PLUGIN_ID.to_string(),
        },
        action: CommandAction::Plugin {
            plugin_id: PLUGIN_ID.to_string(),
            command_id: COMMAND_OPEN_PICKER.to_string(),
        },
    }
}

fn open_latest_command() -> CommandContribution {
    CommandContribution {
        id: COMMAND_OPEN_LATEST.to_string(),
        title: "Eval: Open Latest Run".to_string(),
        description: Some("Open the latest completed eval run in the TUI viewer".to_string()),
        category: Some("eval".to_string()),
        surfaces: BTreeSet::from([CommandSurface::Palette]),
        owner: CommandOwner::Plugin {
            plugin_id: PLUGIN_ID.to_string(),
        },
        action: CommandAction::Plugin {
            plugin_id: PLUGIN_ID.to_string(),
            command_id: COMMAND_OPEN_LATEST.to_string(),
        },
    }
}

fn invoke_command_service(request: &ServiceRequest) -> ServiceResponse {
    if request.operation != OP_INVOKE_COMMAND {
        return ServiceResponse::error(
            "unsupported_operation",
            "unsupported eval command operation",
        );
    }
    let Ok(request) = serde_json::from_slice::<InvokeCommandRequest>(&request.payload) else {
        return ServiceResponse::error(
            "invalid_request",
            "invalid eval command invocation request",
        );
    };
    match request.command_id.as_str() {
        COMMAND_OPEN_PICKER => json_response(&InvokeCommandResponse {
            success: true,
            message: Some("Opening eval run picker".to_string()),
            updated_model: None,
            updated_provider: None,
            updated_thinking: None,
            effects: vec![open_picker_effect()],
        }),
        COMMAND_OPEN_LATEST => open_latest_response(),
        _ => ServiceResponse::error("unknown_command", "unknown eval command"),
    }
}

fn open_latest_response() -> ServiceResponse {
    json_response(&InvokeCommandResponse {
        success: true,
        message: Some("Opening latest eval run".to_string()),
        updated_model: None,
        updated_provider: None,
        updated_thinking: None,
        effects: vec![CommandEffect::OpenPluginSurface {
            surface_kind: tui::EVAL_RUN_PICKER_SURFACE_KIND.to_string(),
            instance_id: "eval-run-latest".to_string(),
            options: serde_json::json!({
                "runs_root": DEFAULT_RUNS_ROOT,
                "open_latest": true,
            }),
        }],
    })
}

fn open_picker_effect() -> CommandEffect {
    CommandEffect::OpenPluginSurface {
        surface_kind: tui::EVAL_RUN_PICKER_SURFACE_KIND.to_string(),
        instance_id: "eval-run-picker".to_string(),
        options: serde_json::json!({ "runs_root": DEFAULT_RUNS_ROOT }),
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
    let mut vtable =
        bcode_plugin_sdk::static_plugin_vtable!(EvalPlugin, include_str!("../bcode-plugin.toml"));
    vtable.tui_registry = Some(tui::tui_registry);
    vtable.cli_registration = Some(cli::registration);
    vtable
}

bcode_plugin_sdk::export_plugin!(EvalPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_command::CommandEffect;

    #[test]
    fn open_picker_command_returns_picker_surface_effect() {
        let response = invoke_command(COMMAND_OPEN_PICKER);
        assert!(response.success);
        assert_eq!(response.effects.len(), 1);
        assert_eq!(
            response.effects[0],
            CommandEffect::OpenPluginSurface {
                surface_kind: tui::EVAL_RUN_PICKER_SURFACE_KIND.to_string(),
                instance_id: "eval-run-picker".to_string(),
                options: serde_json::json!({ "runs_root": DEFAULT_RUNS_ROOT }),
            }
        );
    }

    #[test]
    fn open_latest_command_returns_auto_open_picker_surface_effect() {
        let response = invoke_command(COMMAND_OPEN_LATEST);
        assert!(response.success);
        assert_eq!(response.effects.len(), 1);
        assert_eq!(
            response.effects[0],
            CommandEffect::OpenPluginSurface {
                surface_kind: tui::EVAL_RUN_PICKER_SURFACE_KIND.to_string(),
                instance_id: "eval-run-latest".to_string(),
                options: serde_json::json!({
                    "runs_root": DEFAULT_RUNS_ROOT,
                    "open_latest": true,
                }),
            }
        );
    }

    #[test]
    fn unknown_eval_command_errors() {
        let request = service_request("eval.unknown");
        let response = invoke_command_service(&request);
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("unknown_command")
        );
    }

    fn invoke_command(command_id: &str) -> InvokeCommandResponse {
        let request = service_request(command_id);
        let response = invoke_command_service(&request);
        assert_eq!(response.error, None);
        serde_json::from_slice::<InvokeCommandResponse>(&response.payload).expect("response json")
    }

    fn service_request(command_id: &str) -> ServiceRequest {
        ServiceRequest {
            interface_id: COMMAND_INTERFACE_ID.to_string(),
            operation: OP_INVOKE_COMMAND.to_string(),
            payload: serde_json::to_vec(&InvokeCommandRequest {
                command_id: command_id.to_string(),
                args: std::collections::BTreeMap::new(),
            })
            .expect("request json"),
        }
    }
}
