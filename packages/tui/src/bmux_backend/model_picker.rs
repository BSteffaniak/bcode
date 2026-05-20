//! BMUX backend model picker state.

use bcode_model::ModelInfo;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

/// Model picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ModelPickerApp {
    models: Vec<ModelInfo>,
    filter: TextEditBuffer,
    list_state: ListState,
    filtered_indices: Vec<usize>,
    status: String,
}

impl ModelPickerApp {
    /// Create a model picker with status text.
    #[must_use]
    pub(super) fn new_with_status(models: Vec<ModelInfo>, status: impl Into<String>) -> Self {
        let filtered_indices = (0..models.len()).collect::<Vec<_>>();
        let mut list_state = ListState::new();
        if !filtered_indices.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            models,
            filter: TextEditBuffer::new(),
            list_state,
            filtered_indices,
            status: status.into(),
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

    /// Return list state mutably.
    pub(super) const fn list_state_mut(&mut self) -> &mut ListState {
        &mut self.list_state
    }

    /// Return status.
    #[must_use]
    pub(super) fn status(&self) -> &str {
        &self.status
    }

    /// Return visible list items.
    #[must_use]
    pub(super) fn list_items(&self) -> Vec<ListItem> {
        if self.filtered_indices.is_empty() {
            return vec![ListItem::new(Line::from_spans(vec![Span::styled(
                "No matching models.",
                Style::new().fg(Color::BrightBlack),
            )]))];
        }
        self.filtered_indices
            .iter()
            .map(|index| model_item(&self.models[*index]))
            .collect()
    }

    /// Return selected model id.
    #[must_use]
    pub(super) fn selected_model_id(&self) -> Option<String> {
        let selected = self.list_state.selected?;
        let index = *self.filtered_indices.get(selected)?;
        Some(self.models[index].model_id.clone())
    }

    /// Refresh filter.
    pub(super) fn refresh_filter(&mut self) {
        let query = self.filter.text().trim().to_ascii_lowercase();
        self.filtered_indices = self
            .models
            .iter()
            .enumerate()
            .filter_map(|(index, model)| model_matches(model, &query).then_some(index))
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

    /// Move selection down.
    pub(super) fn select_next(&mut self) {
        self.list_state.select_next(self.filtered_indices.len());
    }

    /// Move selection up.
    pub(super) fn select_previous(&mut self) {
        self.list_state.select_previous(self.filtered_indices.len());
    }

    /// Select a visible row by zero-based index.
    pub(super) const fn select_visible(&mut self, row: usize) -> bool {
        if row >= self.filtered_indices.len() {
            return false;
        }
        self.list_state.select(Some(row));
        true
    }
}

fn model_item(model: &ModelInfo) -> ListItem {
    let marker = if model.is_default { "* " } else { "  " };
    ListItem::new(Line::from_spans(vec![
        Span::styled(marker, Style::new().fg(Color::BrightBlack)),
        Span::styled(
            model.model_id.clone(),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            model.display_name.clone(),
            Style::new().fg(Color::BrightBlack),
        ),
    ]))
}

fn model_matches(model: &ModelInfo, query: &str) -> bool {
    query.is_empty()
        || model.model_id.to_ascii_lowercase().contains(query)
        || model.display_name.to_ascii_lowercase().contains(query)
}
