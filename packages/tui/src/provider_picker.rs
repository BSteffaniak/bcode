//! TUI provider picker state.

use bcode_ipc::PluginServiceSummary;
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::text_input::TextInputState;

use super::filtered_list::FilteredListState;

/// Model provider picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPickerApp {
    providers: Vec<PluginServiceSummary>,
    filter: TextInputState,
    list: FilteredListState,
}

impl ProviderPickerApp {
    /// Create a provider picker.
    #[must_use]
    pub fn new(providers: Vec<PluginServiceSummary>) -> Self {
        let list = FilteredListState::new(providers.len());
        Self {
            providers,
            filter: super::text_input_flow::empty_state(),
            list,
        }
    }

    /// Return filter input mutably.
    pub const fn filter_mut(&mut self) -> &mut TextInputState {
        &mut self.filter
    }

    /// Return list state mutably.
    pub const fn list_state_mut(&mut self) -> &mut ListState {
        self.list.list_state_mut()
    }

    /// Return visible list items.
    #[must_use]
    pub fn list_items(&self) -> Vec<ListItem> {
        if self.list.indices().is_empty() {
            return vec![empty_item("No matching providers.")];
        }
        self.list
            .indices()
            .iter()
            .map(|index| provider_item(&self.providers[*index]))
            .collect()
    }

    /// Return selected provider id.
    #[must_use]
    pub fn selected_provider_id(&self) -> Option<String> {
        let index = self.list.selected_source_index()?;
        Some(self.providers[index].plugin_id.clone())
    }

    /// Refresh filter.
    pub fn refresh_filter(&mut self) {
        let query = self.filter.buffer().text().trim().to_ascii_lowercase();
        let filtered_indices = self
            .providers
            .iter()
            .enumerate()
            .filter_map(|(index, provider)| provider_matches(provider, &query).then_some(index))
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

fn provider_item(provider: &PluginServiceSummary) -> ListItem {
    let label = provider.name.as_deref().unwrap_or(&provider.plugin_id);
    let description = provider.description.as_deref().unwrap_or("model provider");
    ListItem::new(Line::from_spans(vec![
        Span::styled(label.to_owned(), Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(
            provider.plugin_id.clone(),
            Style::new().fg(Color::BrightBlack),
        ),
        Span::raw("  "),
        Span::styled(description.to_owned(), Style::new().fg(Color::BrightBlack)),
    ]))
}

fn provider_matches(provider: &PluginServiceSummary, query: &str) -> bool {
    query.is_empty()
        || provider.plugin_id.to_ascii_lowercase().contains(query)
        || provider
            .name
            .as_deref()
            .is_some_and(|name| name.to_ascii_lowercase().contains(query))
        || provider
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
