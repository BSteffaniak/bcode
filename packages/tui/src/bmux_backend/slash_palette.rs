//! Slash completion state for the BMUX backend.

use bcode_client::BcodeClient;
use bcode_session_models::SessionId;
use bmux_tui::palette::{CommandPaletteState, PaletteItem};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

/// Slash completion picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SlashPalette {
    items: Vec<SlashItem>,
    state: CommandPaletteState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SlashItem {
    command: String,
    description: String,
}

impl SlashPalette {
    /// Create slash completion state.
    pub(super) async fn new(
        client: &BcodeClient,
        _session_id: Option<SessionId>,
        query: &str,
    ) -> Self {
        let mut state = CommandPaletteState::default();
        state.query.insert_str(query.trim_start_matches('/'));
        let items = slash_items(client, query).await;
        Self { items, state }
    }

    /// Return state mutably.
    pub(super) const fn state_mut(&mut self) -> &mut CommandPaletteState {
        &mut self.state
    }

    /// Return palette widget items.
    #[must_use]
    pub(super) fn palette_items(&self) -> Vec<PaletteItem> {
        self.items
            .iter()
            .map(|item| {
                PaletteItem::new(
                    item.command.clone(),
                    Line::from_spans(vec![
                        Span::styled(
                            item.command.clone(),
                            Style::new().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  "),
                        Span::styled(
                            item.description.clone(),
                            Style::new().fg(Color::BrightBlack),
                        ),
                    ]),
                )
                .search_text(format!("{} {}", item.command, item.description))
            })
            .collect()
    }

    /// Return command at source index.
    #[must_use]
    pub(super) fn command_at(&self, index: usize) -> Option<&str> {
        self.items.get(index).map(|item| item.command.as_str())
    }
}

async fn slash_items(client: &BcodeClient, query: &str) -> Vec<SlashItem> {
    let trimmed = query.trim_start_matches('/');
    let parts = trimmed.split_whitespace().collect::<Vec<_>>();
    if parts.first() == Some(&"agent")
        && let Ok(agents) = client.list_agents().await
    {
        return agents
            .into_iter()
            .map(|agent| item(format!("/agent {}", agent.id), agent.name))
            .collect();
    }
    if matches!(parts.first(), Some(&"skill"))
        && let Ok(skills) = client.list_skills().await
    {
        return skills
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
            .collect();
    }
    if matches!(parts.first(), Some(&"model" | &"set-model"))
        && let Ok(models) = client.session_model_list(None).await
    {
        return models
            .models
            .into_iter()
            .map(|model| item(format!("/model {}", model.model_id), model.display_name))
            .collect();
    }
    if matches!(parts.first(), Some(&"provider" | &"set-provider"))
        && let Ok(services) = client.plugin_services().await
    {
        return services
            .into_iter()
            .filter(|service| service.interface_id == bcode_model::MODEL_PROVIDER_INTERFACE_ID)
            .map(|service| {
                item(
                    format!("/provider {}", service.plugin_id),
                    service.name.unwrap_or_else(|| "model provider".to_owned()),
                )
            })
            .collect();
    }
    static_items()
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
