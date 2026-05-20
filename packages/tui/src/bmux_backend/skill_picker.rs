//! BMUX backend skill picker state.

use bcode_skill_models::{SkillId, SkillSummary};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

/// Skill picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SkillPickerApp {
    skills: Vec<SkillSummary>,
    filter: TextEditBuffer,
    argument: TextEditBuffer,
    list_state: ListState,
    filtered_indices: Vec<usize>,
    mode: SkillPickerMode,
}

/// Skill picker input mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SkillPickerMode {
    Filter,
    Argument,
}

/// Skill picker action selected by the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SkillPickerAction {
    Continue,
    Invoke {
        skill_id: SkillId,
        arguments: String,
    },
    Activate(SkillId),
    Deactivate(SkillId),
    Help(SkillId),
    Cancel,
}

impl SkillPickerApp {
    /// Create a skill picker.
    #[must_use]
    pub(super) fn new(skills: Vec<SkillSummary>) -> Self {
        let filtered_indices = (0..skills.len()).collect::<Vec<_>>();
        let mut list_state = ListState::new();
        if !filtered_indices.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            skills,
            filter: TextEditBuffer::new(),
            argument: TextEditBuffer::new(),
            list_state,
            filtered_indices,
            mode: SkillPickerMode::Filter,
        }
    }

    #[must_use]
    pub(super) const fn filter(&self) -> &TextEditBuffer {
        &self.filter
    }

    pub(super) const fn filter_mut(&mut self) -> &mut TextEditBuffer {
        &mut self.filter
    }

    #[must_use]
    pub(super) const fn argument(&self) -> &TextEditBuffer {
        &self.argument
    }

    pub(super) const fn argument_mut(&mut self) -> &mut TextEditBuffer {
        &mut self.argument
    }

    #[must_use]
    pub(super) const fn mode(&self) -> SkillPickerMode {
        self.mode
    }

    pub(super) const fn list_state_mut(&mut self) -> &mut ListState {
        &mut self.list_state
    }

    #[must_use]
    pub(super) fn list_items(&self) -> Vec<ListItem> {
        if self.filtered_indices.is_empty() {
            return vec![ListItem::new(Line::from_spans(vec![Span::styled(
                "No matching skills.",
                Style::new().fg(Color::BrightBlack),
            )]))];
        }
        self.filtered_indices
            .iter()
            .map(|index| skill_item(&self.skills[*index]))
            .collect()
    }

    #[must_use]
    pub(super) fn selected_skill_id(&self) -> Option<SkillId> {
        let selected = self.list_state.selected?;
        let index = *self.filtered_indices.get(selected)?;
        Some(self.skills[index].id.clone())
    }

    pub(super) fn refresh_filter(&mut self) {
        let query = self.filter.text().trim().to_ascii_lowercase();
        self.filtered_indices = self
            .skills
            .iter()
            .enumerate()
            .filter_map(|(index, skill)| skill_matches(skill, &query).then_some(index))
            .collect();
        if self.filtered_indices.is_empty() {
            self.list_state.select(None);
            self.list_state.offset = 0;
        } else {
            self.list_state.select(Some(
                self.list_state
                    .selected
                    .unwrap_or(0)
                    .min(self.filtered_indices.len() - 1),
            ));
        }
    }

    pub(super) fn select_next(&mut self) {
        self.list_state.select_next(self.filtered_indices.len());
    }

    pub(super) fn select_previous(&mut self) {
        self.list_state.select_previous(self.filtered_indices.len());
    }

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
