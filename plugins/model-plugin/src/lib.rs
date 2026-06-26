#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled model and runtime command palette plugin for Bcode.

use bcode_command::{CommandAction, CommandContribution, CommandOwner, CommandSurface};
use bcode_plugin_sdk::prelude::*;

/// Bundled model command plugin.
#[derive(Default)]
pub struct ModelPlugin;

impl RustPlugin for ModelPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        for command in model_palette_command_contributions() {
            registrar
                .register(&command)
                .map_err(|error| PluginError::failed(error.to_string()))?;
        }
        Ok(())
    }
}

fn model_palette_command_contributions() -> Vec<CommandContribution> {
    vec![
        model_command(
            "model.status",
            "Model: Current Status",
            "Show configured provider/model status",
            "model",
        ),
        model_command(
            "model.serverStatus",
            "Model: Server Status",
            "Show server default provider/model status",
            "model",
        ),
        model_command(
            "runtime.status",
            "Runtime: Status",
            "Show active runtime work",
            "runtime",
        ),
        model_command(
            "model.select",
            "Model: Select",
            "Pick a model for this session",
            "model",
        ),
    ]
}

fn model_command(id: &str, title: &str, description: &str, category: &str) -> CommandContribution {
    CommandContribution {
        id: id.to_string(),
        title: title.to_string(),
        description: Some(description.to_string()),
        category: Some(category.to_string()),
        surfaces: std::collections::BTreeSet::from([CommandSurface::Palette]),
        owner: CommandOwner::Plugin {
            plugin_id: "bcode.model".to_string(),
        },
        action: CommandAction::Host {
            route: id.to_string(),
        },
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(ModelPlugin, include_str!("../bcode-plugin.toml"))
}

bcode_plugin_sdk::export_plugin!(ModelPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_plugin_registers_palette_commands_from_plugin_code() {
        extern "C" fn register_command(
            payload: *const u8,
            payload_len: usize,
            user_data: *mut std::ffi::c_void,
        ) {
            assert!(!payload.is_null());
            assert!(!user_data.is_null());
            let bytes = unsafe { std::slice::from_raw_parts(payload, payload_len) };
            let contribution = serde_json::from_slice::<CommandContribution>(bytes)
                .expect("command contribution should decode");
            let registry = unsafe { &mut *(user_data.cast::<bcode_command::CommandRegistry>()) };
            registry.register(contribution);
        }

        let mut plugin = ModelPlugin;
        let mut registry = bcode_command::CommandRegistry::new();
        plugin
            .register_commands(CommandRegistrar::new(
                Some(register_command),
                std::ptr::from_mut(&mut registry).cast::<std::ffi::c_void>(),
            ))
            .expect("model plugin should register commands");

        let commands = registry.commands_for_surface(&CommandSurface::Palette);

        assert!(commands.iter().any(|command| command.id == "model.status"));
        assert!(
            commands
                .iter()
                .any(|command| command.id == "model.serverStatus")
        );
        assert!(
            commands
                .iter()
                .any(|command| command.id == "runtime.status")
        );
        assert!(commands.iter().any(|command| command.id == "model.select"));
    }
}
