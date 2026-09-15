#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Disableable usage reporting presentation.

#[cfg(feature = "static-bundled")]
mod cli;
pub mod tui;
use bcode_command::{
    CommandAction, CommandContribution, CommandEffect, CommandOwner, CommandSurface,
    InvokeCommandRequest, InvokeCommandResponse, SlashCommandContribution,
};
use bcode_plugin_sdk::prelude::*;
use std::collections::BTreeSet;

/// Usage dashboard command provider.
#[derive(Default)]
pub struct UsagePlugin;
impl RustPlugin for UsagePlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        registrar
            .register(&CommandContribution {
                id: "usage.open".into(),
                title: "Usage: Open Dashboard".into(),
                description: Some("Compare recorded estimated model costs".into()),
                category: Some("usage".into()),
                surfaces: BTreeSet::from([CommandSurface::Palette, CommandSurface::Slash]),
                slash: Some(SlashCommandContribution {
                    name: "usage".into(),
                    aliases: BTreeSet::new(),
                }),
                arguments: Vec::new(),
                session: bcode_command::CommandSessionRequirement::Optional,
                execution: bcode_command::CommandExecution::Normal,
                owner: CommandOwner::Plugin {
                    plugin_id: "bcode.usage".into(),
                },
                action: CommandAction::Plugin {
                    plugin_id: "bcode.usage".into(),
                    command_id: "usage.open".into(),
                },
            })
            .map_err(|error| PluginError::failed(error.to_string()))
    }
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != bcode_command::COMMAND_INTERFACE_ID
            || context.request.operation != bcode_command::OP_INVOKE_COMMAND
        {
            return ServiceResponse::error("unsupported_interface", "unsupported usage operation");
        }
        let Ok(request) = serde_json::from_slice::<InvokeCommandRequest>(&context.request.payload)
        else {
            return ServiceResponse::error("invalid_request", "invalid usage command");
        };
        if request.command_id != "usage.open" {
            return ServiceResponse::error("unknown_command", "unknown usage command");
        }
        ServiceResponse::json(&InvokeCommandResponse {
            success: true,
            message: Some(
                "Usage reports are estimated snapshots; collect sources explicitly.".into(),
            ),
            updated_model: None,
            updated_provider: None,
            updated_thinking: None,
            effects: vec![CommandEffect::OpenPluginSurface {
                surface_kind: "usage-dashboard".into(),
                instance_id: "usage-dashboard".into(),
                options: serde_json::json!({}),
            }],
        })
        .unwrap_or_else(|_| ServiceResponse::error("encode_failed", "usage response unavailable"))
    }
}

/// Static bundled plugin entry point.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    let mut vtable =
        bcode_plugin_sdk::static_plugin_vtable!(UsagePlugin, include_str!("../bcode-plugin.toml"));
    vtable.cli_registration = Some(cli::registration);
    vtable
}
#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(UsagePlugin, include_str!("../bcode-plugin.toml"));
