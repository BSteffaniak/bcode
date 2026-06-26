//! TUI command palette state and actions.

use bcode_command::{CommandAction, CommandContribution, CommandSurface};
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
    /// Create a command palette from bundled host commands.
    #[must_use]
    pub fn new() -> Self {
        Self::with_command_contributions(bcode_command::bundled_host_palette_commands())
    }

    /// Create a command palette from registry-resolved command contributions.
    #[must_use]
    pub fn with_command_contributions(
        contributions: impl IntoIterator<Item = CommandContribution>,
    ) -> Self {
        Self {
            contributions: contributions
                .into_iter()
                .filter(|contribution| contribution.supports_surface(&CommandSurface::Palette))
                .collect(),
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
    use std::collections::BTreeSet;

    use bcode_command::{CommandOwner, CommandRegistry};

    use super::*;

    #[test]
    fn palette_renders_registry_contributions_without_local_plugin_conversion() {
        let mut registry = CommandRegistry::new();
        registry.register(CommandContribution::host_palette(
            "session.new",
            "New Session",
            "Create a new chat session",
            "session",
        ));
        registry.register(CommandContribution {
            id: "example.dynamic".to_string(),
            title: "Dynamic".to_string(),
            description: None,
            category: None,
            surfaces: BTreeSet::from([CommandSurface::Palette]),
            owner: CommandOwner::Plugin {
                plugin_id: "bcode.example".to_string(),
            },
            action: CommandAction::Plugin {
                plugin_id: "bcode.example".to_string(),
                command_id: "example.dynamic".to_string(),
            },
        });
        let palette = BmuxCommandPalette::with_command_contributions(
            registry.commands_for_surface(&CommandSurface::Palette),
        );
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
    fn host_command_routes_through_registry_action_model() {
        let palette = BmuxCommandPalette::new();

        assert_eq!(
            palette.command_at(0),
            Some(CommandAction::Host {
                route: "session.new".to_string(),
            })
        );
    }

    #[test]
    fn non_palette_command_is_ignored() {
        let palette = BmuxCommandPalette::with_command_contributions([CommandContribution {
            id: "example.hidden".to_string(),
            title: "Hidden".to_string(),
            description: None,
            category: None,
            surfaces: BTreeSet::from([CommandSurface::Slash]),
            owner: CommandOwner::Plugin {
                plugin_id: "bcode.example".to_string(),
            },
            action: CommandAction::Plugin {
                plugin_id: "bcode.example".to_string(),
                command_id: "example.hidden".to_string(),
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
