//! BMUX backend provider picker state.

use bcode_ipc::PluginServiceSummary;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

/// Model provider picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProviderPickerApp {
    providers: Vec<PluginServiceSummary>,
    filter: TextEditBuffer,
    list_state: ListState,
    filtered_indices: Vec<usize>,
}

impl ProviderPickerApp {
    /// Create a provider picker.
    #[must_use]
    pub(super) fn new(providers: Vec<PluginServiceSummary>) -> Self {
        let filtered_indices = (0..providers.len()).collect::<Vec<_>>();
        let mut list_state = ListState::new();
        if !filtered_indices.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            providers,
            filter: TextEditBuffer::new(),
            list_state,
            filtered_indices,
        }
    }

    #[must_use]
    pub(super) const fn filter(&self) -> &TextEditBuffer {
        &self.filter
    }

    pub(super) const fn filter_mut(&mut self) -> &mut TextEditBuffer {
        &mut self.filter
    }

    pub(super) const fn list_state_mut(&mut self) -> &mut ListState {
        &mut self.list_state
    }

    #[must_use]
    pub(super) fn list_items(&self) -> Vec<ListItem> {
        if self.filtered_indices.is_empty() {
            return vec![ListItem::new(Line::from_spans(vec![Span::styled(
                "No matching providers.",
                Style::new().fg(Color::BrightBlack),
            )]))];
        }
        self.filtered_indices
            .iter()
            .map(|index| provider_item(&self.providers[*index]))
            .collect()
    }

    #[must_use]
    pub(super) fn selected_provider_id(&self) -> Option<String> {
        let selected = self.list_state.selected?;
        let index = *self.filtered_indices.get(selected)?;
        Some(self.providers[index].plugin_id.clone())
    }

    pub(super) fn refresh_filter(&mut self) {
        let query = self.filter.text().trim().to_ascii_lowercase();
        self.filtered_indices = self
            .providers
            .iter()
            .enumerate()
            .filter_map(|(index, provider)| provider_matches(provider, &query).then_some(index))
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
