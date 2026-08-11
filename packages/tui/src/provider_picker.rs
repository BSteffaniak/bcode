//! TUI provider picker state.

use bcode_ipc::PluginServiceSummary;
use bmux_tui::event::Event;
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;
use bmux_tui_components::text_input::TextInputState;

use super::filtered_list::FilteredListState;

/// Outcome from one provider-picker event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderPickerOutcome {
    /// Picker state changed and remains open.
    Continue,
    /// Continue to model selection with the selected provider.
    Select(Option<String>),
    /// Close without choosing a provider.
    Cancel,
}

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

    /// Synchronize list visibility before rendering and return its render state.
    pub fn list_render_state(&mut self, viewport_height: u16) -> &mut ListState {
        self.list.render_state(viewport_height)
    }

    /// Return visible list items.
    #[must_use]
    pub fn list_items(&self, muted: Style) -> Vec<ListItem> {
        if self.list.indices().is_empty() {
            return vec![empty_item("No matching providers.", muted)];
        }
        self.list
            .indices()
            .iter()
            .map(|index| provider_item(&self.providers[*index], muted))
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

    /// Handle one terminal event through picker-owned input and selection policy.
    pub fn handle_event(
        &mut self,
        event: &Event,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> ProviderPickerOutcome {
        match event {
            Event::Paste(text) => {
                let _ = super::text_input_flow::handle_paste(&mut self.filter, text);
                self.refresh_filter();
            }
            Event::Key(stroke) => match stroke.key {
                bmux_keyboard::KeyCode::Escape => return ProviderPickerOutcome::Cancel,
                bmux_keyboard::KeyCode::Enter => {
                    return ProviderPickerOutcome::Select(self.selected_provider_id());
                }
                bmux_keyboard::KeyCode::Up => self.select_previous(),
                bmux_keyboard::KeyCode::Down => self.select_next(),
                _ => {
                    if super::text_input_flow::handle_key(&mut self.filter, keymap, *stroke)
                        != bmux_tui_components::text_input::TextInputOutcome::Ignored
                    {
                        self.refresh_filter();
                    }
                }
            },
            Event::Mouse(mouse) => {
                if let Some(row) = super::picker_mouse::picker_row_from_mouse(*mouse)
                    && self.select_visible(row)
                {
                    return ProviderPickerOutcome::Select(self.selected_provider_id());
                }
            }
            Event::Focus(_) | Event::Resize(_) | Event::Tick | Event::User(_) => {}
        }
        ProviderPickerOutcome::Continue
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

fn provider_item(provider: &PluginServiceSummary, muted: Style) -> ListItem {
    let label = provider.name.as_deref().unwrap_or(&provider.plugin_id);
    let description = provider.description.as_deref().unwrap_or("model provider");
    ListItem::new(Line::from_spans(vec![
        Span::styled(label.to_owned(), Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(provider.plugin_id.clone(), muted),
        Span::raw("  "),
        Span::styled(description.to_owned(), muted),
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

fn empty_item(message: &str, muted: Style) -> ListItem {
    ListItem::new(Line::from_spans(vec![Span::styled(
        message.to_owned(),
        muted,
    )]))
}

#[cfg(test)]
mod tests {
    use super::{ProviderPickerApp, ProviderPickerOutcome};
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::event::Event;

    #[test]
    fn event_policy_owns_navigation_selection_and_cancel() {
        let mut picker = ProviderPickerApp::new(vec![
            bcode_ipc::PluginServiceSummary {
                plugin_id: "one".to_owned(),
                interface_id: "bcode.model-provider".to_owned(),
                name: None,
                description: None,
                workflow_blocks: Vec::new(),
            },
            bcode_ipc::PluginServiceSummary {
                plugin_id: "two".to_owned(),
                interface_id: "bcode.model-provider".to_owned(),
                name: None,
                description: None,
                workflow_blocks: Vec::new(),
            },
        ]);
        let keymap =
            super::super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

        assert_eq!(
            picker.handle_event(&Event::Key(KeyStroke::simple(KeyCode::Down)), &keymap),
            ProviderPickerOutcome::Continue
        );
        assert_eq!(
            picker.handle_event(&Event::Key(KeyStroke::simple(KeyCode::Enter)), &keymap),
            ProviderPickerOutcome::Select(Some("two".to_owned()))
        );
        assert_eq!(
            picker.handle_event(&Event::Key(KeyStroke::simple(KeyCode::Escape)), &keymap),
            ProviderPickerOutcome::Cancel
        );
    }
}
