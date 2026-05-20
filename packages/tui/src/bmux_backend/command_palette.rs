//! BMUX backend command palette state and actions.

use bmux_tui::palette::{CommandPaletteState, PaletteItem};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

/// Command identifiers supported by the BMUX backend palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaletteCommand {
    /// Create a new session and switch to it.
    NewSession,
    /// Open the session picker.
    SwitchSession,
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
            Self::CancelTurn => "turn.cancel",
            Self::CompactContext => "context.compact",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        match id {
            "session.new" => Some(Self::NewSession),
            "session.switch" => Some(Self::SwitchSession),
            "turn.cancel" => Some(Self::CancelTurn),
            "context.compact" => Some(Self::CompactContext),
            _ => None,
        }
    }
}

/// Command palette state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BmuxCommandPalette {
    items: Vec<PaletteItem>,
    state: CommandPaletteState,
}

impl BmuxCommandPalette {
    /// Create a command palette.
    #[must_use]
    pub(super) fn new() -> Self {
        Self {
            items: palette_items(),
            state: CommandPaletteState::default(),
        }
    }

    /// Return cloned items for rendering/handling.
    #[must_use]
    pub(super) fn cloned_items(&self) -> Vec<PaletteItem> {
        self.items.clone()
    }

    /// Return palette state mutably.
    pub(super) const fn state_mut(&mut self) -> &mut CommandPaletteState {
        &mut self.state
    }

    /// Resolve an item index to a command.
    #[must_use]
    pub(super) fn command_at(&self, index: usize) -> Option<PaletteCommand> {
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
