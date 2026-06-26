#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled skills command palette plugin for Bcode.

use bcode_command::{CommandAction, CommandContribution, CommandOwner, CommandSurface};
use bcode_plugin_sdk::prelude::*;

/// Bundled skills command plugin.
#[derive(Default)]
pub struct SkillsPlugin;

impl RustPlugin for SkillsPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        for command in skills_palette_command_contributions() {
            registrar
                .register(&command)
                .map_err(|error| PluginError::failed(error.to_string()))?;
        }
        Ok(())
    }
}

fn skills_palette_command_contributions() -> Vec<CommandContribution> {
    vec![
        skills_command("skills.list", "Skills: Available", "List available skills"),
        skills_command(
            "skills.active",
            "Skills: Active",
            "Show active session skills",
        ),
    ]
}

fn skills_command(id: &str, title: &str, description: &str) -> CommandContribution {
    CommandContribution {
        id: id.to_string(),
        title: title.to_string(),
        description: Some(description.to_string()),
        category: Some("skills".to_string()),
        surfaces: std::collections::BTreeSet::from([CommandSurface::Palette]),
        owner: CommandOwner::Plugin {
            plugin_id: "bcode.skills".to_string(),
        },
        action: CommandAction::Host {
            route: id.to_string(),
        },
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(SkillsPlugin, include_str!("../bcode-plugin.toml"))
}

bcode_plugin_sdk::export_plugin!(SkillsPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_plugin_registers_palette_commands_from_plugin_code() {
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

        let mut plugin = SkillsPlugin;
        let mut registry = bcode_command::CommandRegistry::new();
        plugin
            .register_commands(CommandRegistrar::new(
                Some(register_command),
                std::ptr::from_mut(&mut registry).cast::<std::ffi::c_void>(),
            ))
            .expect("skills plugin should register commands");

        let commands = registry.commands_for_surface(&CommandSurface::Palette);

        assert!(commands.iter().any(|command| command.id == "skills.list"));
        assert!(commands.iter().any(|command| command.id == "skills.active"));
    }
}
