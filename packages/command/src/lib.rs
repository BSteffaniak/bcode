#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Command contract types and `bcode.command/v1` service interface for Bcode.
//!
//! Plugins declare this interface in their manifest to contribute commands
//! discoverable via the control panel and slash commands.

#[cfg(test)]
mod bpdl_contract_tests;

use bcode_model::ReasoningEffort;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Plugin service interface for command providers / core command registry.
pub const COMMAND_INTERFACE_ID: &str = "bcode.command/v1";

/// Operation to list available commands (returns `CommandList`).
pub const OP_LIST_COMMANDS: &str = "list";

/// Operation to invoke a command (request `InvokeCommandRequest`, response `InvokeCommandResponse`).
pub const OP_INVOKE_COMMAND: &str = "invoke";

/// Command metadata for palette / slash discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub requires_args: bool,
    #[serde(default)]
    pub category: Option<String>,
}

/// Response to `OP_LIST_COMMANDS`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandList {
    pub commands: Vec<CommandInfo>,
}

/// Request payload for `OP_INVOKE_COMMAND`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvokeCommandRequest {
    pub command_id: String,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
}

/// Response from command invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvokeCommandResponse {
    pub success: bool,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub updated_model: Option<String>,
    #[serde(default)]
    pub updated_provider: Option<String>,
    #[serde(default)]
    pub updated_thinking: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<CommandEffect>,
}

/// Generic effect requested by a plugin-owned command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandEffect {
    /// Show a user-facing status message.
    Status {
        /// Message to show.
        message: String,
    },
    /// Append a text note to the current transcript.
    AppendText {
        /// Text to append.
        text: String,
    },
    /// Toggle a generic host surface by stable surface id.
    ToggleSurface {
        /// Surface id to toggle.
        surface_id: String,
    },
    /// Open a plugin-contributed TUI surface.
    OpenPluginSurface {
        /// Surface kind declared by the owning plugin.
        surface_kind: String,
        /// Surface instance id.
        instance_id: String,
        /// Plugin-defined JSON options.
        #[serde(default)]
        options: serde_json::Value,
    },
}

/// Command owner identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandOwner {
    /// Host-owned command contribution.
    Host,
    /// Plugin-owned command contribution.
    Plugin {
        /// Owning plugin id.
        plugin_id: String,
    },
}

/// Command execution target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandAction {
    /// Host-routed command action.
    Host {
        /// Opaque host route.
        route: String,
    },
    /// Plugin-routed command action.
    Plugin {
        /// Owning plugin id.
        plugin_id: String,
        /// Plugin-owned command id.
        command_id: String,
    },
}

/// Command contribution surface.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandSurface {
    /// Command palette surface.
    Palette,
    /// Slash command surface.
    Slash,
    /// Named custom surface.
    Custom(String),
}

impl CommandSurface {
    /// Parse a manifest surface string.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "palette" => Self::Palette,
            "slash" => Self::Slash,
            other => Self::Custom(other.to_owned()),
        }
    }
}

/// Host scheduling class for command execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandExecution {
    /// Ordinary command execution.
    #[default]
    Normal,
    /// Control-plane command that must execute directly rather than entering model-turn queues.
    Immediate,
}

/// Registry contribution for a user-visible command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandContribution {
    /// Stable command id.
    pub id: String,
    /// Display title.
    pub title: String,
    /// Optional display description.
    #[serde(default)]
    pub description: Option<String>,
    /// Optional category.
    #[serde(default)]
    pub category: Option<String>,
    /// Surfaces this command appears on.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub surfaces: BTreeSet<CommandSurface>,
    /// Host scheduling class.
    #[serde(default)]
    pub execution: CommandExecution,
    /// Command owner.
    pub owner: CommandOwner,
    /// Command action.
    pub action: CommandAction,
}

impl CommandContribution {
    /// Build a host-owned command contribution.
    #[must_use]
    pub fn host_palette(id: &str, title: &str, description: &str, category: &str) -> Self {
        Self {
            id: id.to_owned(),
            title: title.to_owned(),
            description: Some(description.to_owned()),
            category: Some(category.to_owned()),
            surfaces: BTreeSet::from([CommandSurface::Palette]),
            execution: CommandExecution::Normal,
            owner: CommandOwner::Host,
            action: CommandAction::Host {
                route: id.to_owned(),
            },
        }
    }

    /// Return true when this command contributes to `surface`.
    #[must_use]
    pub fn supports_surface(&self, surface: &CommandSurface) -> bool {
        self.surfaces.contains(surface)
    }
}

/// In-memory command contribution registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandRegistry {
    contributions: BTreeMap<String, CommandContribution>,
}

impl CommandRegistry {
    /// Create an empty command registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            contributions: BTreeMap::new(),
        }
    }

    /// Register or replace a command contribution by id.
    pub fn register(&mut self, contribution: CommandContribution) {
        self.contributions
            .insert(contribution.id.clone(), contribution);
    }

    /// Extend this registry with command contributions.
    pub fn extend(&mut self, contributions: impl IntoIterator<Item = CommandContribution>) {
        for contribution in contributions {
            self.register(contribution);
        }
    }

    /// Return commands for a given surface in stable id order.
    #[must_use]
    pub fn commands_for_surface(&self, surface: &CommandSurface) -> Vec<CommandContribution> {
        self.contributions
            .values()
            .filter(|contribution| contribution.supports_surface(surface))
            .cloned()
            .collect()
    }

    /// Return a command contribution by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&CommandContribution> {
        self.contributions.get(id)
    }
}

/// Return host-owned command contributions bundled with the default TUI distribution.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn bundled_host_palette_commands() -> Vec<CommandContribution> {
    vec![
        CommandContribution::host_palette(
            "session.new",
            "New Session",
            "Create a new chat session",
            "session",
        ),
        CommandContribution::host_palette(
            "session.switch",
            "Switch Session",
            "Open the session picker",
            "session",
        ),
        CommandContribution::host_palette(
            "session.fork",
            "Fork Session",
            "Create a new session from an earlier prompt",
            "session",
        ),
        CommandContribution::host_palette(
            "session.clone",
            "Clone Session",
            "Copy the full current conversation into a new session",
            "session",
        ),
        CommandContribution::host_palette("help", "Help", "Show TUI help", "help"),
        CommandContribution::host_palette(
            "session.rename",
            "Session: Rename",
            "Rename an existing session",
            "session",
        ),
        CommandContribution::host_palette(
            "session.delete",
            "Session: Delete",
            "Delete an existing session",
            "session",
        ),
        CommandContribution::host_palette(
            "turn.cancel",
            "Turn: Cancel",
            "Cancel the active assistant turn",
            "turn",
        ),
        CommandContribution::host_palette(
            "context.compact",
            "Context: Compact",
            "Compact the current conversation context",
            "context",
        ),
    ]
}

/// Errors returned by command operations.
///
/// * Invalid command ID or arguments.
/// * Underlying provider / session error.
/// * Permission denied for the action.
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
pub enum CommandError {
    #[error("unknown command: {0}")]
    UnknownCommand(String),
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_filters_commands_by_surface() {
        let mut registry = CommandRegistry::new();
        registry.register(CommandContribution {
            id: "example.palette".to_owned(),
            title: "Palette".to_owned(),
            description: None,
            category: None,
            surfaces: BTreeSet::from([CommandSurface::Palette]),
            execution: CommandExecution::Normal,
            owner: CommandOwner::Host,
            action: CommandAction::Host {
                route: "example.palette".to_owned(),
            },
        });
        registry.register(CommandContribution {
            id: "example.slash".to_owned(),
            title: "Slash".to_owned(),
            description: None,
            category: None,
            surfaces: BTreeSet::from([CommandSurface::Slash]),
            execution: CommandExecution::Normal,
            owner: CommandOwner::Host,
            action: CommandAction::Host {
                route: "example.slash".to_owned(),
            },
        });

        let palette = registry.commands_for_surface(&CommandSurface::Palette);

        assert_eq!(palette.len(), 1);
        assert_eq!(palette[0].id, "example.palette");
    }
}
