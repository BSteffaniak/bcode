//! TUI command palette state and actions.

use std::collections::BTreeMap;

use bcode_plugin::PluginOwnedCommandContribution;
use bmux_tui::palette::{CommandPaletteState, PaletteItem};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

/// Command identifiers supported by the TUI palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteCommand {
    /// Core TUI command.
    Core(CorePaletteCommand),
    /// Plugin-owned contribution that is routed by contribution metadata.
    Plugin(PluginPaletteCommand),
}

/// Plugin-owned palette command metadata needed to route execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPaletteCommand {
    /// Owning plugin id.
    pub plugin_id: String,
    /// Plugin-owned command id.
    pub command_id: String,
}

/// Core command identifiers supported by the TUI palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorePaletteCommand {
    /// Create a new session and switch to it.
    NewSession,
    /// Open the session picker.
    SwitchSession,
    /// List Git worktrees.
    ListWorktrees,
    /// Create a worktree for the current session.
    CreateSessionWorktree,
    /// Attach current session to an existing worktree.
    AttachWorktree,
    /// Remove a Git worktree.
    RemoveWorktree,
    /// Show active model status.
    ShowModelStatus,
    /// Show server default model/provider.
    ShowServerModelStatus,
    /// Show runtime status.
    ShowRuntimeStatus,
    /// Select active session model.
    SelectModel,
    /// Toggle diff panel.
    ToggleDiff,
    /// Show available skills.
    ListSkills,
    /// Show active skills for the current session.
    ActiveSkills,
    /// Show TUI help.
    Help,
    /// Open the session picker in rename mode.
    RenameSession,
    /// Open the session picker in delete mode.
    DeleteSession,
    /// Fork the current session from a selected prompt.
    ForkSession,
    /// Clone the current session.
    CloneSession,
    /// Request active turn cancellation.
    CancelTurn,
    /// Request context compaction.
    CompactContext,
}

impl CorePaletteCommand {
    /// Return this command's stable ID.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::NewSession => "session.new",
            Self::SwitchSession => "session.switch",
            Self::ListWorktrees => "command.work-tree.list",
            Self::CreateSessionWorktree => "command.work-tree.createSession",
            Self::AttachWorktree => "command.work-tree.attach",
            Self::RemoveWorktree => "command.work-tree.remove",
            Self::ShowModelStatus => "model.status",
            Self::ShowServerModelStatus => "model.serverStatus",
            Self::ShowRuntimeStatus => "runtime.status",
            Self::SelectModel => "model.select",
            Self::ToggleDiff => "diff.toggle",
            Self::ListSkills => "skills.list",
            Self::ActiveSkills => "skills.active",
            Self::Help => "help",
            Self::RenameSession => "session.rename",
            Self::DeleteSession => "session.delete",
            Self::ForkSession => "session.fork",
            Self::CloneSession => "session.clone",
            Self::CancelTurn => "turn.cancel",
            Self::CompactContext => "context.compact",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        match id {
            "session.new" => Some(Self::NewSession),
            "session.switch" => Some(Self::SwitchSession),
            "command.work-tree.list" => Some(Self::ListWorktrees),
            "command.work-tree.createSession" => Some(Self::CreateSessionWorktree),
            "command.work-tree.attach" => Some(Self::AttachWorktree),
            "command.work-tree.remove" => Some(Self::RemoveWorktree),
            "model.status" => Some(Self::ShowModelStatus),
            "model.serverStatus" => Some(Self::ShowServerModelStatus),
            "runtime.status" => Some(Self::ShowRuntimeStatus),
            "model.select" => Some(Self::SelectModel),
            "diff.toggle" => Some(Self::ToggleDiff),
            "skills.list" => Some(Self::ListSkills),
            "skills.active" => Some(Self::ActiveSkills),
            "help" => Some(Self::Help),
            "session.rename" => Some(Self::RenameSession),
            "session.delete" => Some(Self::DeleteSession),
            "session.fork" => Some(Self::ForkSession),
            "session.clone" => Some(Self::CloneSession),
            "turn.cancel" => Some(Self::CancelTurn),
            "context.compact" => Some(Self::CompactContext),
            _ => None,
        }
    }
}

impl From<CorePaletteCommand> for PaletteCommand {
    fn from(value: CorePaletteCommand) -> Self {
        Self::Core(value)
    }
}

/// Command palette state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BmuxCommandPalette {
    items: Vec<PaletteItem>,
    plugin_commands: BTreeMap<String, PluginPaletteCommand>,
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
        let (items, plugin_commands) = palette_items(commands);
        Self {
            items,
            plugin_commands,
            state: CommandPaletteState::default(),
        }
    }

    /// Return cloned items for rendering/handling.
    #[must_use]
    pub fn cloned_items(&self) -> Vec<PaletteItem> {
        self.items.clone()
    }

    /// Return palette state mutably.
    pub const fn state_mut(&mut self) -> &mut CommandPaletteState {
        &mut self.state
    }

    /// Resolve an item index to a command.
    #[must_use]
    pub fn command_at(&self, index: usize) -> Option<PaletteCommand> {
        let item = self.items.get(index)?;
        self.plugin_commands
            .get(&item.id)
            .cloned()
            .map(PaletteCommand::Plugin)
            .or_else(|| CorePaletteCommand::from_id(&item.id).map(PaletteCommand::Core))
    }
}

#[allow(clippy::too_many_lines)]
fn palette_items(
    plugin_commands: &[PluginOwnedCommandContribution],
) -> (Vec<PaletteItem>, BTreeMap<String, PluginPaletteCommand>) {
    let mut items = default_palette_items();
    let plugin_commands = apply_plugin_command_contributions(&mut items, plugin_commands);
    (items, plugin_commands)
}

fn apply_plugin_command_contributions(
    items: &mut Vec<PaletteItem>,
    plugin_commands: &[PluginOwnedCommandContribution],
) -> BTreeMap<String, PluginPaletteCommand> {
    let mut routed_commands = BTreeMap::new();
    for contribution in plugin_commands {
        let command = &contribution.command;
        if command.surface.as_deref() != Some("palette") {
            continue;
        }
        let description = command.description.as_deref().unwrap_or_default();
        let replacement = raw_item(
            &command.id,
            &command.title,
            description,
            &plugin_command_search_text(&contribution.plugin_id, command),
        );
        routed_commands.insert(
            command.id.clone(),
            PluginPaletteCommand {
                plugin_id: contribution.plugin_id.clone(),
                command_id: command.id.clone(),
            },
        );
        if let Some(existing) = items.iter_mut().find(|item| item.id == command.id) {
            *existing = replacement;
        } else {
            items.push(replacement);
        }
    }
    routed_commands
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
fn default_palette_items() -> Vec<PaletteItem> {
    vec![
        item(
            CorePaletteCommand::NewSession,
            "New Session",
            "Create a new chat session",
            "new session create chat",
        ),
        item(
            CorePaletteCommand::SwitchSession,
            "Switch Session",
            "Open the session picker",
            "switch session picker open",
        ),
        item(
            CorePaletteCommand::ForkSession,
            "Fork Session",
            "Create a new session from an earlier prompt",
            "fork session conversation branch prompt",
        ),
        item(
            CorePaletteCommand::CloneSession,
            "Clone Session",
            "Copy the full current conversation into a new session",
            "clone session conversation copy duplicate",
        ),
        item(
            CorePaletteCommand::ListWorktrees,
            "Worktree: List",
            "Show repository worktrees",
            "worktree list git branch repository",
        ),
        item(
            CorePaletteCommand::CreateSessionWorktree,
            "Worktree: Create for Current Session",
            "Create and move this session into a worktree",
            "worktree create current session branch",
        ),
        item(
            CorePaletteCommand::AttachWorktree,
            "Worktree: Attach Current Session",
            "Choose an existing worktree for this session",
            "worktree attach switch current session picker",
        ),
        item(
            CorePaletteCommand::RemoveWorktree,
            "Worktree: Remove",
            "Choose a linked worktree to remove",
            "worktree remove delete linked cleanup",
        ),
        item(
            CorePaletteCommand::ShowModelStatus,
            "Show Model Status",
            "Show active session model metadata",
            "model provider status active current",
        ),
        item(
            CorePaletteCommand::ShowServerModelStatus,
            "Show Server Model Defaults",
            "Show selected default provider/model",
            "server model provider default status",
        ),
        item(
            CorePaletteCommand::ShowRuntimeStatus,
            "Show Runtime Status",
            "Show active daemon/plugin work",
            "runtime daemon plugin tool status active work",
        ),
        item(
            CorePaletteCommand::SelectModel,
            "Select Model",
            "Choose a model for this session",
            "model select choose session provider",
        ),
        item(
            CorePaletteCommand::ToggleDiff,
            "Toggle Diff Panel",
            "Show or hide changed files and diff preview",
            "diff changed files toggle preview",
        ),
        item(
            CorePaletteCommand::ListSkills,
            "List Skills",
            "Show available skills",
            "skills list available",
        ),
        item(
            CorePaletteCommand::ActiveSkills,
            "Active Skills",
            "Show active session skills",
            "skills active context enabled",
        ),
        item(
            CorePaletteCommand::Help,
            "Help",
            "Show TUI shortcuts",
            "help shortcuts keybindings",
        ),
        item(
            CorePaletteCommand::RenameSession,
            "Rename Session",
            "Rename a session from the picker",
            "rename session current selected",
        ),
        item(
            CorePaletteCommand::DeleteSession,
            "Delete Session",
            "Delete a session with confirmation",
            "delete remove session current selected",
        ),
        item(
            CorePaletteCommand::CancelTurn,
            "Cancel Turn",
            "Cancel the active model turn",
            "cancel stop interrupt turn generation",
        ),
        item(
            CorePaletteCommand::CompactContext,
            "Compact Context",
            "Summarize and compact model context",
            "compact summarize context history",
        ),
    ]
}

fn item(
    command: CorePaletteCommand,
    title: &str,
    description: &str,
    search_text: &str,
) -> PaletteItem {
    raw_item(command.id(), title, description, search_text)
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
    fn plugin_command_contribution_routes_known_palette_item_to_plugin_owner() {
        let palette = BmuxCommandPalette::with_plugin_commands(&[PluginOwnedCommandContribution {
            plugin_id: "bcode.example".to_string(),
            command: bcode_plugin::PluginCommandContribution {
                id: CorePaletteCommand::ListWorktrees.id().to_string(),
                title: "Worktrees From Plugin".to_string(),
                description: Some("Plugin-owned command".to_string()),
                category: Some("worktree".to_string()),
                surface: Some("palette".to_string()),
            },
        }]);
        let items = palette.cloned_items();
        let item = items
            .iter()
            .find(|item| item.id == CorePaletteCommand::ListWorktrees.id())
            .expect("worktree item should exist");

        let rendered = format!("{:?}", item.label);
        assert!(rendered.contains("Worktrees From Plugin"));
        assert_eq!(
            palette.command_at(
                items
                    .iter()
                    .position(|item| item.id == CorePaletteCommand::ListWorktrees.id())
                    .expect("worktree item index")
            ),
            Some(PaletteCommand::Plugin(PluginPaletteCommand {
                plugin_id: "bcode.example".to_string(),
                command_id: CorePaletteCommand::ListWorktrees.id().to_string(),
            }))
        );
        assert_eq!(
            palette.command_at(0),
            Some(PaletteCommand::Core(CorePaletteCommand::NewSession))
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
            Some(PaletteCommand::Plugin(PluginPaletteCommand {
                plugin_id: "bcode.example".to_string(),
                command_id: "example.dynamic".to_string(),
            }))
        );
    }
}
