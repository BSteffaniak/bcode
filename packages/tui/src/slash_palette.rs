//! Slash completion state for the TUI.

use bcode_client::BcodeClient;
use bcode_session_models::SessionId;

const MAX_SLASH_COMPLETIONS: usize = 8;

/// Slash completion picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashPalette {
    items: Vec<SlashItem>,
    selected: usize,
}

/// One slash completion item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashItem {
    command: String,
    description: String,
}

/// Visible slash completion item with its source index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleSlashItem<'a> {
    /// Source item index.
    pub source_index: usize,
    /// Completion item.
    pub item: &'a SlashItem,
}

impl SlashPalette {
    /// Create slash completion state.
    pub async fn new(client: &BcodeClient, _session_id: Option<SessionId>, query: &str) -> Self {
        let items = slash_items(client, query).await;
        Self { items, selected: 0 }
    }

    /// Return true if there are no completions.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Return the selected source item index.
    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected.min(self.items.len().saturating_sub(1))
    }

    /// Return the number of completion items.
    #[must_use]
    pub const fn item_count(&self) -> usize {
        self.items.len()
    }

    /// Return visible items for the popup height.
    pub fn visible_items(&self, height: usize) -> impl Iterator<Item = VisibleSlashItem<'_>> {
        let selected = self.selected_index();
        let start = selected.saturating_sub(height.saturating_sub(1));
        self.items
            .iter()
            .enumerate()
            .skip(start)
            .take(height)
            .map(|(source_index, item)| VisibleSlashItem { source_index, item })
    }

    /// Return the currently selected command.
    #[must_use]
    pub fn selected_command(&self) -> Option<&str> {
        self.items
            .get(self.selected_index())
            .map(|item| item.command.as_str())
    }

    /// Select a command if it exists in the current completion list.
    pub fn select_command(&mut self, command: &str) {
        if let Some(index) = self.items.iter().position(|item| item.command == command) {
            self.selected = index;
        }
    }

    /// Select an item by visible row, returning the selected command.
    pub fn select_visible_row(&mut self, row: usize, height: usize) -> Option<&str> {
        let selected = self.selected_index();
        let start = selected.saturating_sub(height.saturating_sub(1));
        self.selected = start
            .saturating_add(row)
            .min(self.items.len().saturating_sub(1));
        self.selected_command()
    }

    /// Move selection to previous item.
    pub const fn move_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move selection to next item.
    pub fn move_next(&mut self) {
        self.selected = self
            .selected
            .saturating_add(1)
            .min(self.items.len().saturating_sub(1));
    }

    /// Return whether the selected command exactly matches current composer text.
    #[must_use]
    pub fn selected_matches(&self, text: &str) -> bool {
        self.selected_command()
            .is_some_and(|command| text == command.trim_end())
    }

    #[cfg(test)]
    pub fn from_items(items: Vec<(&str, &str)>) -> Self {
        Self {
            items: items
                .into_iter()
                .map(|(command, description)| item(command, description))
                .collect(),
            selected: 0,
        }
    }
}

impl SlashItem {
    /// Return replacement command text.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// Return item description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
}

async fn slash_items(client: &BcodeClient, query: &str) -> Vec<SlashItem> {
    let trimmed = query.trim_start_matches('/');
    let parts = trimmed.split_whitespace().collect::<Vec<_>>();
    let candidates = if parts.first() == Some(&"agent")
        && let Ok(agents) = client.list_agents().await
    {
        agents
            .into_iter()
            .map(|agent| item(format!("/agent {}", agent.id), agent.name))
            .collect()
    } else if matches!(parts.first(), Some(&"skill"))
        && let Ok(skills) = client.list_skills().await
    {
        skills
            .skills
            .into_iter()
            .flat_map(|skill| {
                let description = skill
                    .description
                    .unwrap_or_else(|| "invoke skill".to_owned());
                [
                    item(format!("/skill {}", skill.id), description),
                    item(format!("/skill describe {}", skill.id), "describe skill"),
                ]
            })
            .collect()
    } else if matches!(parts.first(), Some(&"model" | &"set-model"))
        && let Ok(models) = client.session_model_list(None).await
    {
        models
            .models
            .into_iter()
            .map(|model| item(format!("/model {}", model.model_id), model.display_name))
            .collect()
    } else if matches!(parts.first(), Some(&"provider" | &"set-provider"))
        && let Ok(services) = client.plugin_services().await
    {
        services
            .into_iter()
            .filter(|service| service.interface_id == bcode_model::MODEL_PROVIDER_INTERFACE_ID)
            .map(|service| {
                item(
                    format!("/provider {}", service.plugin_id),
                    service.name.unwrap_or_else(|| "model provider".to_owned()),
                )
            })
            .collect()
    } else {
        static_items()
    };
    filter_items(candidates, trimmed)
        .into_iter()
        .take(MAX_SLASH_COMPLETIONS)
        .collect()
}

fn filter_items(items: Vec<SlashItem>, query: &str) -> Vec<SlashItem> {
    let normalized_query = normalize(query);
    if normalized_query.is_empty() {
        return items;
    }
    items
        .into_iter()
        .filter(|item| {
            normalize(&item.command).contains(&normalized_query)
                || normalize(&item.description).contains(&normalized_query)
        })
        .collect()
}

fn normalize(value: &str) -> String {
    value
        .trim_start_matches('/')
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn static_items() -> Vec<SlashItem> {
    [
        ("/plan", "Switch to plan agent"),
        ("/build", "Switch to build agent"),
        ("/sessions", "Open session picker"),
        ("/new", "Create and switch to a new session"),
        ("/compact", "Compact current session context"),
        ("/model", "Open model picker"),
        ("/models", "Open model picker"),
        ("/set-model ", "Set model by id"),
        ("/provider", "Show current provider"),
        ("/set-provider ", "Set provider by id"),
        ("/diff", "Toggle diff panel"),
        ("/skills", "Open skill picker"),
        ("/agent ", "Set session agent by id"),
        ("/skill ", "Invoke skill by id"),
        ("/skill describe ", "Describe skill by id"),
    ]
    .into_iter()
    .map(|(command, description)| item(command, description))
    .collect()
}

fn item(command: impl Into<String>, description: impl Into<String>) -> SlashItem {
    SlashItem {
        command: command.into(),
        description: description.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{filter_items, static_items};

    #[test]
    fn filters_static_slash_items_by_typed_prefix() {
        let items = filter_items(static_items(), "pl");

        assert_eq!(items[0].command(), "/plan");
        assert!(
            items
                .iter()
                .all(|item| item.command().contains("pl") || item.description().contains("pl"))
        );
    }
}
