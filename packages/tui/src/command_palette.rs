//! TUI command palette state and actions.

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
    /// Show active model status.
    ShowModelStatus,
    /// Show server default model/provider.
    ShowServerModelStatus,
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
    /// Request active turn cancellation.
    CancelTurn,
    /// Request context compaction.
    CompactContext,
}

impl PaletteCommand {
    const fn id(self) -> &'static str {
        match self {
            Self::NewSession => "session.new",
            Self::SwitchSession => "session.switch",
            Self::ShowModelStatus => "model.status",
            Self::ShowServerModelStatus => "model.serverStatus",
            Self::SelectModel => "model.select",
            Self::ToggleDiff => "diff.toggle",
            Self::ListSkills => "skills.list",
            Self::ActiveSkills => "skills.active",
            Self::Help => "help",
            Self::RenameSession => "session.rename",
            Self::DeleteSession => "session.delete",
            Self::CancelTurn => "turn.cancel",
            Self::CompactContext => "context.compact",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        match id {
            "session.new" => Some(Self::NewSession),
            "session.switch" => Some(Self::SwitchSession),
            "model.status" => Some(Self::ShowModelStatus),
            "model.serverStatus" => Some(Self::ShowServerModelStatus),
            "model.select" => Some(Self::SelectModel),
            "diff.toggle" => Some(Self::ToggleDiff),
            "skills.list" => Some(Self::ListSkills),
            "skills.active" => Some(Self::ActiveSkills),
            "help" => Some(Self::Help),
            "session.rename" => Some(Self::RenameSession),
            "session.delete" => Some(Self::DeleteSession),
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
        Self {
            items: palette_items(),
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

fn palette_items() -> Vec<PaletteItem> {
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
    PaletteItem::new(
        command.id(),
        Line::from_spans(vec![
            Span::styled(title.to_owned(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(description.to_owned(), Style::new().fg(Color::BrightBlack)),
        ]),
    )
    .search_text(search_text)
}
