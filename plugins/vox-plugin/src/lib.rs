#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! User-operated Vox integration. The daemon never owns microphone capture.

pub mod frontend;
pub mod service;
#[cfg(feature = "static-bundled")]
pub mod tui;
#[cfg(feature = "static-bundled")]
pub use tui::tui_registry;

use bcode_command::{
    CommandAction, CommandContribution, CommandExecution, CommandOwner, CommandSessionRequirement,
    CommandSurface, SlashCommandContribution,
};
use bcode_plugin_sdk::prelude::*;
use std::collections::BTreeSet;

/// Owning plugin identity.
pub const PLUGIN_ID: &str = "bcode.vox";

/// Discoverable user commands; none is a model-callable tool.
#[must_use]
pub fn commands() -> Vec<CommandContribution> {
    ["start", "stop", "toggle", "cancel", "status"]
        .into_iter()
        .map(|name| {
            let id = format!("vox.{name}");
            CommandContribution {
                id: id.clone(),
                title: format!("Vox: {name}"),
                description: Some("Control dictation on the attached local frontend only".into()),
                category: Some("vox".into()),
                surfaces: BTreeSet::from([CommandSurface::Palette, CommandSurface::Slash]),
                slash: Some(SlashCommandContribution {
                    name: format!("vox-{name}"),
                    aliases: BTreeSet::new(),
                }),
                arguments: Vec::new(),
                session: CommandSessionRequirement::Optional,
                execution: CommandExecution::Normal,
                owner: CommandOwner::Plugin {
                    plugin_id: PLUGIN_ID.into(),
                },
                action: CommandAction::Plugin {
                    plugin_id: PLUGIN_ID.into(),
                    command_id: id,
                },
            }
        })
        .collect()
}

/// Daemon-side discovery and fail-closed service provider.
///
/// There is deliberately no installation API for microphone resources here.
#[derive(Default)]
pub struct VoxPlugin;

impl RustPlugin for VoxPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        for command in commands() {
            registrar
                .register(&command)
                .map_err(|_| PluginError::failed("Vox command registration failed"))?;
        }
        Ok(())
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        service::dispatch(&context.request, None)
    }
}

/// Static bundled plugin entry point.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(VoxPlugin, include_str!("../bcode-plugin.toml"))
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(VoxPlugin, include_str!("../bcode-plugin.toml"));
