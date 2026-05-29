//! TUI skill picker state.

use bcode_skill_models::{SkillId, SkillSummary};
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::text_input::TextInputState;

use super::filtered_list::FilteredListState;

/// Skill picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPickerApp {
    skills: Vec<SkillSummary>,
    filter: TextInputState,
    argument: TextInputState,
    list: FilteredListState,
    mode: SkillPickerMode,
}

/// Skill picker input mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillPickerMode {
    /// Filtering/selecting skills.
    Filter,
    /// Editing invocation arguments.
    Argument,
}

/// Skill picker action selected by the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillPickerAction {
    /// Keep the picker open.
    Continue,
    /// Invoke a skill with arguments.
    Invoke {
        skill_id: SkillId,
        arguments: String,
    },
    /// Activate a skill for the session.
    Activate(SkillId),
    /// Deactivate a skill for the session.
    Deactivate(SkillId),
    /// Show skill help.
    Help(SkillId),
    /// Close the picker.
    Cancel,
}

impl SkillPickerApp {
    /// Create a skill picker.
    #[must_use]
    pub fn new(skills: Vec<SkillSummary>) -> Self {
        let list = FilteredListState::new(skills.len());
        Self {
            skills,
            filter: super::text_input_flow::empty_state(),
            argument: super::text_input_flow::empty_state(),
            list,
            mode: SkillPickerMode::Filter,
        }
    }

    /// Return filter input mutably.
    pub const fn filter_mut(&mut self) -> &mut TextInputState {
        &mut self.filter
    }

    /// Return argument input.
    #[must_use]
    pub const fn argument(&self) -> &TextInputState {
        &self.argument
    }

    /// Return argument input mutably.
    pub const fn argument_mut(&mut self) -> &mut TextInputState {
        &mut self.argument
    }

    /// Return active input mode.
    #[must_use]
    pub const fn mode(&self) -> SkillPickerMode {
        self.mode
    }

    /// Return list state mutably.
    pub const fn list_state_mut(&mut self) -> &mut ListState {
        self.list.list_state_mut()
    }

    /// Return visible list items.
    #[must_use]
    pub fn list_items(&self) -> Vec<ListItem> {
        if self.list.indices().is_empty() {
            return vec![empty_item("No matching skills.")];
        }
        self.list
            .indices()
            .iter()
            .map(|index| skill_item(&self.skills[*index]))
            .collect()
    }

    /// Return selected skill id.
    #[must_use]
    pub fn selected_skill_id(&self) -> Option<SkillId> {
        let index = self.list.selected_source_index()?;
        Some(self.skills[index].id.clone())
    }

    /// Refresh filter.
    pub fn refresh_filter(&mut self) {
        let query = self.filter.buffer().text().trim().to_ascii_lowercase();
        let filtered_indices = self
            .skills
            .iter()
            .enumerate()
            .filter_map(|(index, skill)| skill_matches(skill, &query).then_some(index))
            .collect();
        self.list.replace_indices(filtered_indices);
    }

    /// Move selection down.
    pub fn select_next(&mut self) {
        self.list.select_next();
    }

    /// Move selection up.
    pub fn select_previous(&mut self) {
        self.list.select_previous();
    }

    /// Select a visible row by zero-based index.
    pub const fn select_visible(&mut self, row: usize) -> bool {
        self.list.select_visible(row)
    }

    /// Switch to argument-entry mode.
    pub const fn start_argument(&mut self) {
        self.mode = SkillPickerMode::Argument;
    }
}

fn skill_item(skill: &SkillSummary) -> ListItem {
    let description = skill.description.as_deref().unwrap_or("no description");
    ListItem::new(Line::from_spans(vec![
        Span::styled(
            skill.id.to_string(),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(description.to_owned(), Style::new().fg(Color::BrightBlack)),
    ]))
}

fn skill_matches(skill: &SkillSummary, query: &str) -> bool {
    query.is_empty()
        || skill.id.as_str().to_ascii_lowercase().contains(query)
        || skill.name.to_ascii_lowercase().contains(query)
        || skill
            .description
            .as_deref()
            .is_some_and(|description| description.to_ascii_lowercase().contains(query))
}

fn empty_item(message: &str) -> ListItem {
    ListItem::new(Line::from_spans(vec![Span::styled(
        message.to_owned(),
        Style::new().fg(Color::BrightBlack),
    )]))
}
