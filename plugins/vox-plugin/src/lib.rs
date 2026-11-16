#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! User-operated Vox integration. The daemon never owns microphone capture.

pub mod frontend;
pub mod service;
#[cfg(feature = "tui")]
pub mod tui;
#[cfg(feature = "tui")]
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
    fn component_registrations() -> bcode_plugin_sdk::definition::ProductRegistrations {
        crate::product_registrations()
    }

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

// Static bundled plugin entry point.
// Identity and service declarations come from the embedded manifest only.
bcode_plugin_sdk::define_plugin!(serial VoxPlugin, include_str!("../bcode-plugin.toml"));

fn product_registrations() -> bcode_plugin_sdk::definition::ProductRegistrations {
    let registrations = bcode_plugin_sdk::definition::ProductRegistrations::default();
    #[cfg(feature = "tui")]
    let registrations = bcode_plugin_sdk::definition::ProductRegistrations {
        frontend: Some(tui_registry),
        ..registrations
    };
    registrations
}

/// Bind this implementation to the host's invocation-scoped capability resolver.
/// The resolver must retain live cancellation, caller provenance and authorized bridges.
#[must_use]
pub fn plugin_definition_with_host(
    host: impl Fn(
        bcode_plugin_sdk::definition::PluginCreateContext,
    ) -> std::sync::Arc<dyn bcode_plugin_sdk::definition::ProductInvocationHost>
    + Send
    + Sync
    + 'static,
) -> bcode_plugin_sdk::definition::PluginDefinition {
    bcode_plugin_sdk::definition::rust_plugin_definition_with_host::<VoxPlugin>(
        plugin_definition().metadata,
        host,
    )
}
