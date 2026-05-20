//! Slash completion state for the BMUX backend.

use bmux_tui::palette::{CommandPaletteState, PaletteItem};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

/// Slash completion picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SlashPalette {
    items: Vec<SlashItem>,
    state: CommandPaletteState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SlashItem {
    command: String,
    description: String,
}

impl SlashPalette {
    /// Create slash completion state.
    #[must_use]
    pub(super) fn new(query: &str) -> Self {
        let mut state = CommandPaletteState::default();
        state.query.insert_str(query.trim_start_matches('/'));
        Self {
            items: slash_items(),
            state,
        }
    }

    /// Return state mutably.
    pub(super) const fn state_mut(&mut self) -> &mut CommandPaletteState {
        &mut self.state
    }

    /// Return palette widget items.
    #[must_use]
    pub(super) fn palette_items(&self) -> Vec<PaletteItem> {
        self.items
            .iter()
            .map(|item| {
                PaletteItem::new(
                    item.command.clone(),
                    Line::from_spans(vec![
                        Span::styled(
                            item.command.clone(),
                            Style::new().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  "),
                        Span::styled(
                            item.description.clone(),
                            Style::new().fg(Color::BrightBlack),
                        ),
                    ]),
                )
                .search_text(format!("{} {}", item.command, item.description))
            })
            .collect()
    }

    /// Return command at source index.
    #[must_use]
    pub(super) fn command_at(&self, index: usize) -> Option<&str> {
        self.items.get(index).map(|item| item.command.as_str())
    }
}

fn slash_items() -> Vec<SlashItem> {
    [
        ("/plan", "Switch to plan agent"),
        ("/build", "Switch to build agent"),
        ("/sessions", "Open session picker"),
        ("/new", "Create and switch to a new session"),
        ("/compact", "Compact current session context"),
        ("/model", "Open model picker"),
        ("/models", "Open model picker"),
        ("/skills", "Open skill picker"),
        ("/agent ", "Set session agent by id"),
        ("/skill ", "Invoke skill by id"),
    ]
    .into_iter()
    .map(|(command, description)| SlashItem {
        command: command.to_owned(),
        description: description.to_owned(),
    })
    .collect()
}
