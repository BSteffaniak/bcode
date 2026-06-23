//! TUI command palette state and actions.

use bcode_plugin::PluginOwnedCommandContribution;
use bmux_tui::palette::{CommandPaletteState, PaletteItem};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

/// Command identifiers supported by the TUI palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteCommand {
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

impl PaletteCommand {
    /// Return this command's stable ID.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::NewSession => "session.new",
            Self::SwitchSession => "session.switch",
            Self::ListWorktrees => "worktree.list",
            Self::CreateSessionWorktree => "worktree.createSession",
            Self::AttachWorktree => "worktree.attach",
            Self::RemoveWorktree => "worktree.remove",
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
            "worktree.list" => Some(Self::ListWorktrees),
            "worktree.createSession" => Some(Self::CreateSessionWorktree),
            "worktree.attach" => Some(Self::AttachWorktree),
            "worktree.remove" => Some(Self::RemoveWorktree),
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

/// Command palette state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BmuxCommandPalette {
    items: Vec<PaletteItem>,
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
            items: palette_items(commands),
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
        self.items
            .get(index)
            .and_then(|item| PaletteCommand::from_id(&item.id))
    }
}

#[allow(clippy::too_many_lines)]
fn palette_items(plugin_commands: &[PluginOwnedCommandContribution]) -> Vec<PaletteItem> {
    let mut items = default_palette_items();
    apply_plugin_command_contributions(&mut items, plugin_commands);
    items
}

fn apply_plugin_command_contributions(
    items: &mut Vec<PaletteItem>,
    plugin_commands: &[PluginOwnedCommandContribution],
) {
    for contribution in plugin_commands {
        let command = &contribution.command;
        if PaletteCommand::from_id(&command.id).is_none() {
            continue;
        }
        let description = command.description.as_deref().unwrap_or_default();
        let replacement = raw_item(
            &command.id,
            &command.title,
            description,
            &plugin_command_search_text(&contribution.plugin_id, command),
        );
        if let Some(existing) = items.iter_mut().find(|item| item.id == command.id) {
            *existing = replacement;
        } else {
            items.push(replacement);
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
fn default_palette_items() -> Vec<PaletteItem> {
    vec![
        item(
            PaletteCommand::NewSession,
            "New Session",
            "Create a new chat session",
            "new session create chat",
        ),
        item(
            PaletteCommand::SwitchSession,
            "Switch Session",
            "Open the session picker",
            "switch session picker open",
        ),
        item(
            PaletteCommand::ForkSession,
            "Fork Session",
            "Create a new session from an earlier prompt",
            "fork session conversation branch prompt",
        ),
        item(
            PaletteCommand::CloneSession,
            "Clone Session",
            "Copy the full current conversation into a new session",
            "clone session conversation copy duplicate",
        ),
        item(
            PaletteCommand::ListWorktrees,
            "Worktree: List",
            "Show repository worktrees",
            "worktree list git branch repository",
        ),
        item(
            PaletteCommand::CreateSessionWorktree,
            "Worktree: Create for Current Session",
            "Create and move this session into a worktree",
            "worktree create current session branch",
        ),
        item(
            PaletteCommand::AttachWorktree,
            "Worktree: Attach Current Session",
            "Choose an existing worktree for this session",
            "worktree attach switch current session picker",
        ),
        item(
            PaletteCommand::RemoveWorktree,
            "Worktree: Remove",
            "Choose a linked worktree to remove",
            "worktree remove delete linked cleanup",
        ),
        item(
            PaletteCommand::ShowModelStatus,
            "Show Model Status",
            "Show active session model metadata",
            "model provider status active current",
        ),
        item(
            PaletteCommand::ShowServerModelStatus,
            "Show Server Model Defaults",
            "Show selected default provider/model",
            "server model provider default status",
        ),
        item(
            PaletteCommand::ShowRuntimeStatus,
            "Show Runtime Status",
            "Show active daemon/plugin work",
            "runtime daemon plugin tool status active work",
        ),
        item(
            PaletteCommand::SelectModel,
            "Select Model",
            "Choose a model for this session",
            "model select choose session provider",
        ),
        item(
            PaletteCommand::ToggleDiff,
            "Toggle Diff Panel",
            "Show or hide changed files and diff preview",
            "diff changed files toggle preview",
        ),
        item(
            PaletteCommand::ListSkills,
            "List Skills",
            "Show available skills",
            "skills list available",
        ),
        item(
            PaletteCommand::ActiveSkills,
            "Active Skills",
            "Show active session skills",
            "skills active context enabled",
        ),
        item(
            PaletteCommand::Help,
            "Help",
            "Show TUI shortcuts",
            "help shortcuts keybindings",
        ),
        item(
            PaletteCommand::RenameSession,
            "Rename Session",
            "Rename a session from the picker",
            "rename session current selected",
        ),
        item(
            PaletteCommand::DeleteSession,
            "Delete Session",
            "Delete a session with confirmation",
            "delete remove session current selected",
        ),
        item(
            PaletteCommand::CancelTurn,
            "Cancel Turn",
            "Cancel the active model turn",
            "cancel stop interrupt turn generation",
        ),
        item(
            PaletteCommand::CompactContext,
            "Compact Context",
            "Summarize and compact model context",
            "compact summarize context history",
        ),
    ]
}

fn item(command: PaletteCommand, title: &str, description: &str, search_text: &str) -> PaletteItem {
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
    fn plugin_command_contribution_replaces_known_palette_item() {
        let palette = BmuxCommandPalette::with_plugin_commands(&[PluginOwnedCommandContribution {
            plugin_id: "bcode.worktree".to_string(),
            command: bcode_plugin::PluginCommandContribution {
                id: PaletteCommand::ListWorktrees.id().to_string(),
                title: "Worktrees From Plugin".to_string(),
                description: Some("Plugin-owned command".to_string()),
                category: Some("worktree".to_string()),
                surface: Some("palette".to_string()),
            },
        }]);
        let items = palette.cloned_items();
        let item = items
            .iter()
            .find(|item| item.id == PaletteCommand::ListWorktrees.id())
            .expect("worktree item should exist");

        let rendered = format!("{:?}", item.label);
        assert!(rendered.contains("Worktrees From Plugin"));
        assert_eq!(palette.command_at(0), Some(PaletteCommand::NewSession));
    }

    #[test]
    fn unknown_plugin_command_is_ignored_by_core_palette_until_handler_exists() {
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

        assert!(
            palette
                .cloned_items()
                .iter()
                .all(|item| item.id != "example.dynamic")
        );
    }
}
