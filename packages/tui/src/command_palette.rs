//! TUI command palette state and actions.

use bcode_plugin::PluginOwnedCommandContribution;
use bmux_tui::palette::{CommandPaletteState, PaletteItem};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

/// Command palette action selected by the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteCommandAction {
    /// Host-routed command contribution.
    Host {
        /// Opaque host route.
        route: String,
    },
    /// Plugin-routed command contribution.
    Plugin {
        /// Owning plugin id.
        plugin_id: String,
        /// Plugin-owned command id.
        command_id: String,
    },
}

/// Command palette contribution, regardless of owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteCommandContribution {
    /// Contribution id.
    pub id: String,
    /// Display title.
    pub title: String,
    /// Optional display description.
    pub description: Option<String>,
    /// Optional command category.
    pub category: Option<String>,
    /// Search text.
    pub search_text: String,
    /// Execution action.
    pub action: PaletteCommandAction,
}

impl PaletteCommandContribution {
    fn host(id: &str, title: &str, description: &str, search_text: &str) -> Self {
        Self {
            id: id.to_owned(),
            title: title.to_owned(),
            description: Some(description.to_owned()),
            category: None,
            search_text: search_text.to_owned(),
            action: PaletteCommandAction::Host {
                route: id.to_owned(),
            },
        }
    }

    fn plugin(contribution: &PluginOwnedCommandContribution) -> Option<Self> {
        let command = &contribution.command;
        if command.surface.as_deref() != Some("palette") {
            return None;
        }
        Some(Self {
            id: command.id.clone(),
            title: command.title.clone(),
            description: command.description.clone(),
            category: command.category.clone(),
            search_text: plugin_command_search_text(&contribution.plugin_id, command),
            action: PaletteCommandAction::Plugin {
                plugin_id: contribution.plugin_id.clone(),
                command_id: command.id.clone(),
            },
        })
    }

    fn palette_item(&self) -> PaletteItem {
        raw_item(
            &self.id,
            &self.title,
            self.description.as_deref().unwrap_or_default(),
            &self.search_text,
        )
    }
}

/// Command palette state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BmuxCommandPalette {
    contributions: Vec<PaletteCommandContribution>,
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
        Self {
            contributions: palette_contributions(commands),
            state: CommandPaletteState::default(),
        }
    }

    /// Return cloned items for rendering/handling.
    #[must_use]
    pub fn cloned_items(&self) -> Vec<PaletteItem> {
        self.contributions
            .iter()
            .map(PaletteCommandContribution::palette_item)
            .collect()
    }

    /// Return palette state mutably.
    pub const fn state_mut(&mut self) -> &mut CommandPaletteState {
        &mut self.state
    }

    /// Resolve an item index to a command action.
    #[must_use]
    pub fn command_at(&self, index: usize) -> Option<PaletteCommandAction> {
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

fn palette_contributions(
    plugin_commands: &[PluginOwnedCommandContribution],
) -> Vec<PaletteCommandContribution> {
    let mut contributions = host_palette_contributions();
    apply_plugin_command_contributions(&mut contributions, plugin_commands);
    contributions
}

fn apply_plugin_command_contributions(
    contributions: &mut Vec<PaletteCommandContribution>,
    plugin_commands: &[PluginOwnedCommandContribution],
) {
    for contribution in plugin_commands {
        let Some(command) = PaletteCommandContribution::plugin(contribution) else {
            continue;
        };
        if let Some(existing) = contributions
            .iter_mut()
            .find(|existing| existing.id == command.id)
        {
            *existing = command;
        } else {
            contributions.push(command);
        }
    }
}

fn plugin_command_search_text(
    plugin_id: &str,
    command: &bcode_plugin::PluginCommandContribution,
) -> String {
    [
        command.id.as_str(),
        command.title.as_str(),
        command.description.as_deref().unwrap_or_default(),
        command.category.as_deref().unwrap_or_default(),
        plugin_id,
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

#[allow(clippy::too_many_lines)]
fn host_palette_contributions() -> Vec<PaletteCommandContribution> {
    vec![
        PaletteCommandContribution::host(
            "session.new",
            "New Session",
            "Create a new chat session",
            "new session create chat",
        ),
        PaletteCommandContribution::host(
            "session.switch",
            "Switch Session",
            "Open the session picker",
            "switch session picker open",
        ),
        PaletteCommandContribution::host(
            "session.fork",
            "Fork Session",
            "Create a new session from an earlier prompt",
            "fork session conversation branch prompt",
        ),
        PaletteCommandContribution::host(
            "session.clone",
            "Clone Session",
            "Copy the full current conversation into a new session",
            "clone session conversation copy duplicate",
        ),
        PaletteCommandContribution::host(
            "command.work-tree.list",
            "Worktree: List",
            "Show repository worktrees",
            "worktree list git branch repository",
        ),
        PaletteCommandContribution::host(
            "command.work-tree.createSession",
            "Worktree: Create for Session",
            "Create a worktree branch for this session",
            "worktree create session branch git",
        ),
        PaletteCommandContribution::host(
            "command.work-tree.attach",
            "Worktree: Attach Session",
            "Attach this session to an existing worktree",
            "worktree attach session directory git",
        ),
        PaletteCommandContribution::host(
            "command.work-tree.remove",
            "Worktree: Remove",
            "Remove a repository worktree",
            "worktree remove prune delete git",
        ),
        PaletteCommandContribution::host(
            "model.status",
            "Model: Current Status",
            "Show configured provider/model status",
            "model provider status current",
        ),
        PaletteCommandContribution::host(
            "model.serverStatus",
            "Model: Server Status",
            "Show server default provider/model status",
            "model provider server default status",
        ),
        PaletteCommandContribution::host(
            "runtime.status",
            "Runtime: Status",
            "Show active runtime work",
            "runtime status work tools activity",
        ),
        PaletteCommandContribution::host(
            "model.select",
            "Model: Select",
            "Pick a model for this session",
            "model select choose provider",
        ),
        PaletteCommandContribution::host(
            "skills.list",
            "Skills: Available",
            "List available skills",
            "skills list available",
        ),
        PaletteCommandContribution::host(
            "skills.active",
            "Skills: Active",
            "Show active session skills",
            "skills active enabled",
        ),
        PaletteCommandContribution::host(
            "diff.toggle",
            "Diff: Toggle Panel",
            "Show or hide the inline diff review panel",
            "diff toggle panel review file changes",
        ),
        PaletteCommandContribution::host("help", "Help", "Show TUI help", "help keyboard commands"),
        PaletteCommandContribution::host(
            "session.rename",
            "Session: Rename",
            "Rename an existing session",
            "session rename title",
        ),
        PaletteCommandContribution::host(
            "session.delete",
            "Session: Delete",
            "Delete an existing session",
            "session delete remove",
        ),
        PaletteCommandContribution::host(
            "turn.cancel",
            "Turn: Cancel",
            "Cancel the active assistant turn",
            "cancel stop interrupt turn generation",
        ),
        PaletteCommandContribution::host(
            "context.compact",
            "Context: Compact",
            "Compact the current conversation context",
            "context compact summarize compress conversation",
        ),
    ]
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
            Some(PaletteCommandAction::Plugin {
                plugin_id: "bcode.example".to_string(),
                command_id: "command.work-tree.list".to_string(),
            })
        );
    }

    #[test]
    fn host_command_routes_through_same_action_model() {
        let palette = BmuxCommandPalette::new();

        assert_eq!(
            palette.command_at(0),
            Some(PaletteCommandAction::Host {
                route: "session.new".to_string(),
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
            Some(PaletteCommandAction::Plugin {
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
