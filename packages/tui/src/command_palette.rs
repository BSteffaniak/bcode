//! TUI command palette state and actions.

use bcode_command::{CommandContribution, CommandSurface};
use bmux_keyboard::KeyCode;
use bmux_keyboard::KeyStroke;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;
use bmux_tui_components::selectable_list::SelectableListItem;
use bmux_tui_components::text_input::{TextInputControl, TextInputState};

/// Result of application-owned command filtering and activation.
pub enum CommandPaletteKeyOutcome {
    Ignored,
    QueryEdited,
    SelectionMoved,
    Canceled,
    Activated(usize),
}

/// Command palette state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BmuxCommandPalette {
    contributions: Vec<CommandContribution>,
    query: TextInputState,
    list: super::filtered_list::FilteredListState,
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
        let contributions: Vec<_> = contributions
            .into_iter()
            .filter(|contribution| contribution.supports_surface(&CommandSurface::Palette))
            .collect();
        Self {
            list: super::filtered_list::FilteredListState::new(contributions.len()),
            contributions,
            query: TextInputState::new(TextEditBuffer::new()),
        }
    }

    /// Return cloned items for rendering/handling.
    #[must_use]
    #[cfg(test)]
    pub fn cloned_items(&self, muted: Style) -> Vec<SelectableListItem> {
        self.contributions
            .iter()
            .map(|contribution| palette_item(contribution, muted))
            .collect()
    }

    /// Handle one keyboard input through the palette component's policy.
    pub fn handle_key(&mut self, stroke: KeyStroke, visible_rows: u16) -> CommandPaletteKeyOutcome {
        match stroke.key {
            KeyCode::Escape => return CommandPaletteKeyOutcome::Canceled,
            KeyCode::Enter => {
                return self.list.selected_source_index().map_or(
                    CommandPaletteKeyOutcome::Ignored,
                    CommandPaletteKeyOutcome::Activated,
                );
            }
            KeyCode::Down => {
                self.list.select_next();
                let _ = self.list.render_state(visible_rows);
                return CommandPaletteKeyOutcome::SelectionMoved;
            }
            KeyCode::Up => {
                self.list.select_previous();
                let _ = self.list.render_state(visible_rows);
                return CommandPaletteKeyOutcome::SelectionMoved;
            }
            _ => {}
        }
        let policy = super::text_input_flow::single_line_policy();
        let before = self.query.buffer().text().to_owned();
        TextInputControl::new(&policy).handle_key(&mut self.query, stroke);
        if self.query.buffer().text() == before {
            return CommandPaletteKeyOutcome::Ignored;
        }
        let query = self.query.buffer().text().to_lowercase();
        self.list.replace_indices(
            self.contributions
                .iter()
                .enumerate()
                .filter(|(_, c)| command_search_text(c).to_lowercase().contains(&query))
                .map(|(i, _)| i)
                .collect(),
        );
        CommandPaletteKeyOutcome::QueryEdited
    }

    pub const fn query_mut(&mut self) -> &mut TextInputState {
        &mut self.query
    }
    pub fn visible_items(&self, muted: Style) -> Vec<Line> {
        self.list
            .indices()
            .iter()
            .map(|i| palette_item(&self.contributions[*i], muted).lines.remove(0))
            .collect()
    }
    pub fn render_state(
        &mut self,
        height: u16,
    ) -> &mut bmux_tui_components::selectable_list::SelectableListState {
        self.list.render_state(height)
    }

    /// Resolve an item index to its full command contribution.
    #[must_use]
    pub fn contribution_at(&self, index: usize) -> Option<CommandContribution> {
        self.contributions.get(index).cloned()
    }
}

impl Default for BmuxCommandPalette {
    fn default() -> Self {
        Self::new()
    }
}

fn palette_item(contribution: &CommandContribution, muted: Style) -> SelectableListItem {
    raw_item(
        &contribution.id,
        &contribution.title,
        contribution.description.as_deref().unwrap_or_default(),
        &command_search_text(contribution),
        muted,
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

fn raw_item(
    id: &str,
    title: &str,
    description: &str,
    _search_text: &str,
    muted: Style,
) -> SelectableListItem {
    SelectableListItem::rich(
        id,
        Line::from_spans(vec![
            Span::styled(title.to_owned(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(description.to_owned(), muted),
        ]),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use bcode_command::{CommandAction, CommandOwner, CommandRegistry};

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
            slash: None,
            arguments: Vec::new(),
            session: bcode_command::CommandSessionRequirement::Optional,
            execution: bcode_command::CommandExecution::Normal,
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
        let items = palette.cloned_items(Style::new());
        let index = items
            .iter()
            .position(|item| item.id == "example.dynamic")
            .expect("dynamic plugin command should be present");

        assert_eq!(
            palette.contribution_at(index).map(|item| item.action),
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
            palette.contribution_at(0).map(|item| item.action),
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
            slash: Some(bcode_command::SlashCommandContribution {
                name: "hidden".to_owned(),
                aliases: BTreeSet::new(),
            }),
            arguments: Vec::new(),
            session: bcode_command::CommandSessionRequirement::Optional,
            execution: bcode_command::CommandExecution::Normal,
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
                .cloned_items(Style::new())
                .iter()
                .all(|item| item.id != "example.hidden")
        );
    }
}
