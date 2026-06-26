//! TUI command palette state and actions.

use std::collections::BTreeSet;

use bcode_command::{
    CommandAction, CommandContribution, CommandOwner, CommandRegistry, CommandSurface,
};
use bcode_plugin::PluginOwnedCommandContribution;
use bmux_tui::palette::{CommandPaletteState, PaletteItem};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

/// Command palette state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BmuxCommandPalette {
    contributions: Vec<CommandContribution>,
    state: CommandPaletteState,
}

impl BmuxCommandPalette {
    /// Create a command palette.
    #[must_use]
    pub fn new() -> Self {
        Self::with_plugin_commands(&[])
    }

    /// Create a command palette using manifest-declared plugin commands where available.
    #[must_use]
    pub fn with_plugin_commands(commands: &[PluginOwnedCommandContribution]) -> Self {
        let mut registry = host_command_registry();
        registry.extend(plugin_command_contributions(commands));
        Self::from_registry(&registry, &CommandSurface::Palette)
    }

    /// Create a command palette from a command registry and surface.
    #[must_use]
    pub fn from_registry(registry: &CommandRegistry, surface: &CommandSurface) -> Self {
        Self {
            contributions: registry.commands_for_surface(surface),
            state: CommandPaletteState::default(),
        }
    }

    /// Return cloned items for rendering/handling.
    #[must_use]
    pub fn cloned_items(&self) -> Vec<PaletteItem> {
        self.contributions.iter().map(palette_item).collect()
    }

    /// Return palette state mutably.
    pub const fn state_mut(&mut self) -> &mut CommandPaletteState {
        &mut self.state
    }

    /// Resolve an item index to a command action.
    #[must_use]
    pub fn command_at(&self, index: usize) -> Option<CommandAction> {
        self.contributions
            .get(index)
            .map(|contribution| contribution.action.clone())
    }
}

impl Default for BmuxCommandPalette {
    fn default() -> Self {
        Self::new()
    }
}

fn plugin_command_contributions(
    plugin_commands: &[PluginOwnedCommandContribution],
) -> impl Iterator<Item = CommandContribution> + '_ {
    plugin_commands.iter().filter_map(|contribution| {
        let command = &contribution.command;
        let surface = command
            .surface
            .as_deref()
            .map_or(CommandSurface::Palette, CommandSurface::parse);
        if surface != CommandSurface::Palette {
            return None;
        }
        Some(CommandContribution {
            id: command.id.clone(),
            title: command.title.clone(),
            description: command.description.clone(),
            category: command.category.clone(),
            surfaces: BTreeSet::from([surface]),
            owner: CommandOwner::Plugin {
                plugin_id: contribution.plugin_id.clone(),
            },
            action: CommandAction::Plugin {
                plugin_id: contribution.plugin_id.clone(),
                command_id: command.id.clone(),
            },
        })
    })
}

fn host_command_registry() -> CommandRegistry {
    let mut registry = CommandRegistry::new();
    registry.extend(bcode_command::bundled_host_palette_commands());
    registry
}

fn palette_item(contribution: &CommandContribution) -> PaletteItem {
    raw_item(
        &contribution.id,
        &contribution.title,
        contribution.description.as_deref().unwrap_or_default(),
        &command_search_text(contribution),
    )
}

fn command_search_text(contribution: &CommandContribution) -> String {
    [
        contribution.id.as_str(),
        contribution.title.as_str(),
        contribution.description.as_deref().unwrap_or_default(),
        contribution.category.as_deref().unwrap_or_default(),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

fn raw_item(id: &str, title: &str, description: &str, search_text: &str) -> PaletteItem {
    PaletteItem::new(
        id,
        Line::from_spans(vec![
            Span::styled(title.to_owned(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(description.to_owned(), Style::new().fg(Color::BrightBlack)),
        ]),
    )
    .search_text(search_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_command_contribution_replaces_host_display_and_routes_to_plugin_owner() {
        let palette = BmuxCommandPalette::with_plugin_commands(&[PluginOwnedCommandContribution {
            plugin_id: "bcode.example".to_string(),
            command: bcode_plugin::PluginCommandContribution {
                id: "command.work-tree.list".to_string(),
                title: "Worktrees From Plugin".to_string(),
                description: Some("Plugin-owned command".to_string()),
                category: Some("worktree".to_string()),
                surface: Some("palette".to_string()),
            },
        }]);
        let items = palette.cloned_items();
        let index = items
            .iter()
            .position(|item| item.id == "command.work-tree.list")
            .expect("worktree item should exist");

        let rendered = format!("{:?}", items[index].label);
        assert!(rendered.contains("Worktrees From Plugin"));
        assert_eq!(
            palette.command_at(index),
            Some(CommandAction::Plugin {
                plugin_id: "bcode.example".to_string(),
                command_id: "command.work-tree.list".to_string(),
            })
        );
    }

    #[test]
    fn host_command_routes_through_registry_action_model() {
        let palette = BmuxCommandPalette::new();

        assert_eq!(
            palette.command_at(0),
            Some(CommandAction::Host {
                route: "command.work-tree.attach".to_string(),
            })
        );
    }

    #[test]
    fn plugin_command_contribution_adds_unknown_palette_command() {
        let palette = BmuxCommandPalette::with_plugin_commands(&[PluginOwnedCommandContribution {
            plugin_id: "bcode.example".to_string(),
            command: bcode_plugin::PluginCommandContribution {
                id: "example.dynamic".to_string(),
                title: "Dynamic".to_string(),
                description: None,
                category: None,
                surface: Some("palette".to_string()),
            },
        }]);

        let items = palette.cloned_items();
        let index = items
            .iter()
            .position(|item| item.id == "example.dynamic")
            .expect("dynamic plugin command should be present");

        assert_eq!(
            palette.command_at(index),
            Some(CommandAction::Plugin {
                plugin_id: "bcode.example".to_string(),
                command_id: "example.dynamic".to_string(),
            })
        );
    }

    #[test]
    fn non_palette_plugin_command_is_ignored() {
        let palette = BmuxCommandPalette::with_plugin_commands(&[PluginOwnedCommandContribution {
            plugin_id: "bcode.example".to_string(),
            command: bcode_plugin::PluginCommandContribution {
                id: "example.hidden".to_string(),
                title: "Hidden".to_string(),
                description: None,
                category: None,
                surface: Some("other".to_string()),
            },
        }]);

        assert!(
            palette
                .cloned_items()
                .iter()
                .all(|item| item.id != "example.hidden")
        );
    }
}
