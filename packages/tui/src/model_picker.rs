//! TUI model picker state.

use bcode_model::ModelInfo;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

use super::filtered_list::FilteredListState;

/// Model picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPickerApp {
    models: Vec<ModelInfo>,
    filter: TextEditBuffer,
    list: FilteredListState,
    status: String,
}

impl ModelPickerApp {
    /// Create a model picker with status text.
    #[must_use]
    pub fn new_with_status(models: Vec<ModelInfo>, status: impl Into<String>) -> Self {
        let list = FilteredListState::new(models.len());
        Self {
            models,
            filter: TextEditBuffer::new(),
            list,
            status: status.into(),
        }
    }

    /// Return filter input.
    #[must_use]
    pub const fn filter(&self) -> &TextEditBuffer {
        &self.filter
    }

    /// Return filter input mutably.
    pub const fn filter_mut(&mut self) -> &mut TextEditBuffer {
        &mut self.filter
    }

    /// Return list state mutably.
    pub const fn list_state_mut(&mut self) -> &mut ListState {
        self.list.list_state_mut()
    }

    /// Return status.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Return visible list items.
    #[must_use]
    pub fn list_items(&self) -> Vec<ListItem> {
        if self.list.indices().is_empty() {
            return vec![empty_item("No matching models.")];
        }
        self.list
            .indices()
            .iter()
            .map(|index| model_item(&self.models[*index]))
            .collect()
    }

    /// Return selected model id.
    #[must_use]
    pub fn selected_model_id(&self) -> Option<String> {
        let index = self.list.selected_source_index()?;
        Some(self.models[index].model_id.clone())
    }

    /// Refresh filter.
    pub fn refresh_filter(&mut self) {
        let query = self.filter.text().trim().to_ascii_lowercase();
        let filtered_indices = self
            .models
            .iter()
            .enumerate()
            .filter_map(|(index, model)| model_matches(model, &query).then_some(index))
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

fn empty_item(message: &str) -> ListItem {
    ListItem::new(Line::from_spans(vec![Span::styled(
        message.to_owned(),
        Style::new().fg(Color::BrightBlack),
    )]))
}
