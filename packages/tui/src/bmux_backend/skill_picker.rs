//! BMUX backend skill picker state.

use bcode_skill_models::{SkillId, SkillSummary};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

use super::filtered_list::FilteredListState;

/// Skill picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SkillPickerApp {
    skills: Vec<SkillSummary>,
    filter: TextEditBuffer,
    argument: TextEditBuffer,
    list: FilteredListState,
    mode: SkillPickerMode,
}

/// Skill picker input mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SkillPickerMode {
    /// Filtering/selecting skills.
    Filter,
    /// Editing invocation arguments.
    Argument,
}

/// Skill picker action selected by the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SkillPickerAction {
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
    pub(super) fn new(skills: Vec<SkillSummary>) -> Self {
        let list = FilteredListState::new(skills.len());
        Self {
            skills,
            filter: TextEditBuffer::new(),
            argument: TextEditBuffer::new(),
            list,
            mode: SkillPickerMode::Filter,
        }
    }

    /// Return filter input.
    #[must_use]
    pub(super) const fn filter(&self) -> &TextEditBuffer {
        &self.filter
    }

    /// Return filter input mutably.
    pub(super) const fn filter_mut(&mut self) -> &mut TextEditBuffer {
        &mut self.filter
    }

    /// Return argument input.
    #[must_use]
    pub(super) const fn argument(&self) -> &TextEditBuffer {
        &self.argument
    }

    /// Return argument input mutably.
    pub(super) const fn argument_mut(&mut self) -> &mut TextEditBuffer {
        &mut self.argument
    }

    /// Return active input mode.
    #[must_use]
    pub(super) const fn mode(&self) -> SkillPickerMode {
        self.mode
    }

    /// Return list state mutably.
    pub(super) const fn list_state_mut(&mut self) -> &mut ListState {
        self.list.list_state_mut()
    }

    /// Return visible list items.
    #[must_use]
    pub(super) fn list_items(&self) -> Vec<ListItem> {
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
    pub(super) fn selected_skill_id(&self) -> Option<SkillId> {
        let index = self.list.selected_source_index()?;
        Some(self.skills[index].id.clone())
    }

    /// Refresh filter.
    pub(super) fn refresh_filter(&mut self) {
        let query = self.filter.text().trim().to_ascii_lowercase();
        let filtered_indices = self
            .skills
            .iter()
            .enumerate()
            .filter_map(|(index, skill)| skill_matches(skill, &query).then_some(index))
            .collect();
        self.list.replace_indices(filtered_indices);
    }

    /// Move selection down.
    pub(super) fn select_next(&mut self) {
        self.list.select_next();
    }

    /// Move selection up.
    pub(super) fn select_previous(&mut self) {
        self.list.select_previous();
    }

    /// Select a visible row by zero-based index.
    pub(super) const fn select_visible(&mut self, row: usize) -> bool {
        self.list.select_visible(row)
    }

    /// Switch to argument-entry mode.
    pub(super) const fn start_argument(&mut self) {
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
