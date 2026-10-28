//! Connection settings are presentation state; discovery and persistence are domain-owned.
#[cfg(test)]
mod tests;
mod viewport;
use super::{App, Message};
use crate::connections::{Discovery, Selection};
use bmux_tui::{
    component::{Element, EventCx, LayoutId},
    composition::{Column, Flex, Row, Surface},
    event::Event,
    style::Style,
};
use bmux_tui_components::scroll_view::ScrollViewState;
use bmux_tui_components::select_dropdown::{
    SelectDropdown, SelectDropdownComponent, SelectDropdownOutcome, SelectDropdownState,
    SelectDropdownStyles, SelectOption,
};
use bmux_tui_runtime::Update;
use std::cell::Cell;

pub(super) struct Settings {
    pub open: bool,
    pub scroll: Cell<ScrollViewState>,
    pub discovery: Option<Discovery>,
    pub(super) generation: u64,
    pub status: String,
    pub connection_options: Vec<SelectOption>,
    pub model_options: Vec<SelectOption>,
    pub connection: Cell<SelectDropdownState>,
    pub model: Cell<SelectDropdownState>,
    pub permit: std::sync::Arc<tokio::sync::Semaphore>,
    pub custom: bool,
    pub custom_model: bool,
    pub dropdown_focus: usize,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            open: false,
            scroll: Cell::new(ScrollViewState::default()),
            discovery: None,
            generation: 0,
            status: "Discovering your Bcode connections…".into(),
            connection_options: Vec::new(),
            model_options: Vec::new(),
            connection: Cell::new(SelectDropdownState::new(None)),
            model: Cell::new(SelectDropdownState::new(Some(0))),
            permit: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            custom: false,
            custom_model: false,
            dropdown_focus: 0,
        }
    }
}
impl App {
    pub(super) fn settings_action(&mut self, action: &str) -> Option<Update<Message>> {
        match action {
            "settings-refresh" => Some(self.discover_connections(false)),
            "settings-reset" => Some(self.reset_connection()),
            "settings-add" => Some(self.add_connection()),
            "settings-model" if !self.text(7).is_empty() => {
                self.options.model = Some(self.text(7));
                Some(self.save_connection())
            }
            _ => None,
        }
    }
    pub(super) fn connection_summary(&self) -> String {
        let Some(discovery) = &self.settings_state.discovery else {
            return format!("{} · F7 Settings", self.settings_state.status);
        };
        if discovery.selected.provider.is_none() || discovery.selected.model.is_none() {
            return "No usable model connection · Open Settings to configure one".into();
        }
        let label = if self.options.model_config.is_none()
            && self.options.profile.is_none()
            && self.options.context.is_none()
        {
            discovery
                .selected
                .profile
                .as_ref()
                .map(|profile| format!("Profile: {profile}"))
        } else {
            None
        }
        .or_else(|| {
            discovery
                .connections
                .iter()
                .find(|connection| {
                    connection.selection.config_path == self.options.model_config
                        && connection.selection.profile == self.options.profile
                })
                .map(|connection| connection.label.clone())
        });
        let label = label.unwrap_or_else(|| {
            if self.options.endpoint.is_some() {
                "Custom endpoint".into()
            } else {
                "Custom Bcode configuration".into()
            }
        });
        format!(
            "Using {label} · {} · F7 Settings",
            self.options
                .model
                .as_deref()
                .or(discovery.selected.model.as_deref())
                .unwrap_or("connection default")
        )
    }
    pub(super) fn rebuild_choices(&mut self) {
        let query = self.text(6).to_lowercase();
        let state = &mut self.settings_state;
        state.connection_options.clear();
        state.model_options = vec![SelectOption::new("default", "Use connection default")];
        if let Some(discovery) = &state.discovery {
            state.connection_options.extend(
                discovery
                    .connections
                    .iter()
                    .enumerate()
                    .filter(|(_, connection)| connection.label.to_lowercase().contains(&query))
                    .map(|(index, connection)| {
                        SelectOption::new(index.to_string(), &connection.label)
                    }),
            );
            state.model_options.extend(
                discovery
                    .models
                    .iter()
                    .enumerate()
                    .filter(|(_, model)| {
                        model.images
                            && (model.label.to_lowercase().contains(&query)
                                || model.id.to_lowercase().contains(&query))
                    })
                    .map(|(index, model)| SelectOption::new(index.to_string(), &model.label)),
            );
        }
        state
            .connection_options
            .push(SelectOption::new("custom", "+ Add custom connection…"));
        state
            .model_options
            .push(SelectOption::new("custom", "+ Custom model ID…"));
        let connection = state.connection_options.iter().position(|option| {
            option
                .id
                .parse::<usize>()
                .ok()
                .and_then(|index| state.discovery.as_ref()?.connections.get(index))
                .is_some_and(|connection| {
                    connection.selection.config_path == self.options.model_config
                        && connection.selection.profile == self.options.profile
                        && connection.selection.context == self.options.context
                })
        });
        let model = if self.options.model.is_none() {
            Some(0)
        } else {
            state.model_options.iter().position(|option| {
                option
                    .id
                    .parse::<usize>()
                    .ok()
                    .and_then(|index| state.discovery.as_ref()?.models.get(index))
                    .is_some_and(|model| Some(&model.id) == self.options.model.as_ref())
            })
        };
        state.connection.set(SelectDropdownState::new(connection));
        state.model.set(SelectDropdownState::new(model));
    }
    pub(super) fn settings_view(&self) -> Element<'_> {
        let p = self.palette;
        let mut connection_state = self.settings_state.connection.get();
        connection_state.interaction.focused = self.settings_state.dropdown_focus == 1;
        self.settings_state.connection.set(connection_state);
        let mut model_state = self.settings_state.model.get();
        model_state.interaction.focused = self.settings_state.dropdown_focus == 2;
        self.settings_state.model.set(model_state);
        let styles = SelectDropdownStyles {
            normal: p.text(),
            focused: p.title(),
            hovered: p.title(),
            pressed: p.title(),
            disabled: p.quiet(),
        };
        let mut body = Column::new()
            .child(Self::label("YOUR CONNECTION", p.title()))
            .child(Self::label(&self.settings_state.status, p.quiet()))
            .child(Self::label(
                "Local discovery only · No inference while browsing",
                p.quiet(),
            ))
            .child(self.field(
                6,
                "SEARCH CONNECTIONS & MODELS",
                "Type to filter discovered options",
            ))
            .child(Self::label("CONNECTION", p.quiet()))
            .child(
                SelectDropdownComponent::new(
                    "settings.connections",
                    &self.settings_state.connection_options,
                    &self.settings_state.connection,
                )
                .placeholder("Choose a connection")
                .styles(styles)
                .fallback_style(p.text()),
            )
            .child(Self::label("MODEL", p.quiet()))
            .child(
                SelectDropdownComponent::new(
                    "settings.models",
                    &self.settings_state.model_options,
                    &self.settings_state.model,
                )
                .placeholder("Use connection default")
                .styles(styles)
                .fallback_style(p.text()),
            );
        if self.settings_state.custom_model {
            body = body
                .child(self.field(7, "CUSTOM MODEL ID", "Exact model ID"))
                .child(self.button("settings-model", "Use model ID", 63));
        }
        if self.settings_state.custom {
            body = body
                .child(self.field(
                    3,
                    "CONFIG FILE · OR ENDPOINT BELOW",
                    "Path to existing Bcode config",
                ))
                .child(self.field(8, "ENDPOINT URL · OPTIONAL", "https://api.example.com/v1"))
                .child(self.field(
                    9,
                    "AUTH REFERENCE · NOT A SECRET",
                    "Existing authentication profile reference",
                ))
                .child(self.button("settings-add", "Save custom connection", 64));
        }
        body = body.child(
            Row::new()
                .gap(1)
                .child(self.button("settings-refresh", "Refresh", 60))
                .child(self.button("settings-reset", "Reset to Bcode defaults", 61))
                .child(self.button("settings", "Done · Esc", 62)),
        );
        let focus = if self.settings_state.dropdown_focus == 1 {
            "settings.connections"
        } else if self.settings_state.dropdown_focus == 2 {
            "settings.models"
        } else {
            super::FIELDS
                .get(self.focused)
                .copied()
                .unwrap_or("settings.search.input")
        };
        let viewport = viewport::SettingsViewport {
            content: Element::new(body),
            state: &self.settings_state.scroll,
            focus,
        };
        Element::new(
            Surface::new(Column::new().flex(Flex::new(1, viewport)))
                .background(Style::new().bg(p.base))
                .content_style(p.text()),
        )
    }
    pub(super) fn settings_event(
        &mut self,
        event: &Event,
        target: Option<&str>,
    ) -> Option<Update<Message>> {
        if !self.settings_state.open {
            return None;
        }
        let connection = target.is_some_and(|id| id.starts_with("settings.connections"));
        let model = target.is_some_and(|id| id.starts_with("settings.models"));
        if connection || model {
            self.focused = usize::MAX;
            self.settings_state.dropdown_focus = if connection { 1 } else { 2 };
        }
        let index = self.settings_state.dropdown_focus;
        if index == 0 || (event.key().is_none() && !connection && !model) {
            return None;
        }
        let state = &self.settings_state;
        let (id, options, cell) = if index == 1 {
            (
                "settings.connections",
                &state.connection_options,
                &state.connection,
            )
        } else {
            ("settings.models", &state.model_options, &state.model)
        };
        let outcome = {
            let layout = self.layout.borrow();
            let layout = layout.as_ref()?;
            let cx = EventCx::new(layout);
            let area = cx.find_rect(&LayoutId::new(id))?;
            let mut value = cell.get();
            value.interaction.focused = true;
            let outcome = SelectDropdown::new(options).handle_event(area, &mut value, event);
            cell.set(value);
            outcome
        };
        if let SelectDropdownOutcome::Selected(selected) = outcome {
            let option = options.get(selected)?.id.clone();
            self.select_setting(index, &option);
            return Some(if option == "custom" {
                Update::redraw()
            } else {
                self.save_connection()
            });
        }
        (!matches!(outcome, SelectDropdownOutcome::Ignored)).then(Update::redraw)
    }
    fn select_setting(&mut self, index: usize, option: &str) {
        if option == "custom" {
            if index == 1 {
                self.settings_state.custom = true;
                self.focused = 3;
            } else {
                self.settings_state.custom_model = true;
                self.focused = 7;
            }
            self.settings_state.dropdown_focus = 0;
            return;
        }
        let Some(discovery) = &self.settings_state.discovery else {
            return;
        };
        if index == 1 {
            if let Some(connection) = option
                .parse::<usize>()
                .ok()
                .and_then(|index| discovery.connections.get(index))
            {
                self.apply_selection(connection.selection.clone());
            }
        } else if option == "default" {
            self.options.model = None;
            self.options.provider = None;
        } else if let Some(model) = option
            .parse::<usize>()
            .ok()
            .and_then(|index| discovery.models.get(index))
        {
            self.options.model = Some(model.id.clone());
            self.options.provider.clone_from(&model.provider);
        }
    }
    pub(super) fn apply_selection(&mut self, selection: Selection) {
        self.options.model_config = selection.config_path;
        self.options.profile = selection.profile;
        self.options.context = selection.context;
        self.options.model = selection.model;
        self.options.provider = selection.provider;
        self.options.endpoint = selection.endpoint;
        self.options.auth_profile = selection.auth_profile;
    }

    fn current_selection(&self) -> Selection {
        Selection {
            config_path: self.options.model_config.clone(),
            profile: self.options.profile.clone(),
            context: self.options.context.clone(),
            model: self.options.model.clone(),
            provider: self.options.provider.clone(),
            endpoint: self.options.endpoint.clone(),
            auth_profile: self.options.auth_profile.clone(),
        }
    }

    pub(super) fn discover_connections(&mut self, load_saved: bool) -> Update<Message> {
        self.connection_job(if load_saved {
            PreferenceJob::Load
        } else {
            PreferenceJob::Browse
        })
    }
    pub(super) fn save_connection(&mut self) -> Update<Message> {
        self.connection_job(PreferenceJob::Save)
    }
    pub(super) fn reset_connection(&mut self) -> Update<Message> {
        self.connection_job(PreferenceJob::Reset)
    }
    pub(super) fn add_connection(&mut self) -> Update<Message> {
        let config = self.text(3);
        let endpoint = self.text(8);
        if config.is_empty() && endpoint.is_empty() {
            self.settings_state.status = "Enter a config file path or endpoint URL".into();
            return Update::redraw();
        }
        self.options.auth_profile = (!self.text(9).is_empty()).then(|| self.text(9));
        self.options.model_config = (!config.is_empty()).then(|| config.into());
        self.options.endpoint = (!endpoint.is_empty()).then_some(endpoint);
        self.save_connection()
    }
    fn connection_job(&mut self, job: PreferenceJob) -> Update<Message> {
        self.settings_state.generation = self.settings_state.generation.saturating_add(1);
        let id = self.settings_state.generation;
        self.settings_state.status = "Checking local Bcode configuration…".into();
        self.settings_state.discovery = None;
        let selection = self.current_selection();
        let permit = self.settings_state.permit.clone();
        let mut update = Update::redraw();
        update
            .commands
            .push(bmux_tui_runtime::Command::queue_latest(
                bmux_tui_runtime::CommandKey::new("recap.connections"),
                async move {
                    // The permit is held by the blocking job even after timeout. Repeated refreshes
                    // cannot accumulate unbounded filesystem/config work.
                    let future = async move {
                        let permit = permit
                            .acquire_owned()
                            .await
                            .map_err(|_| "Connection discovery unavailable".to_owned())?;
                        tokio::task::spawn_blocking(move || {
                            let _permit = permit;
                            let path = crate::connections::preferences_path()
                                .map_err(|error| error.to_string())?;
                            let selected = match job {
                                PreferenceJob::Load => selection.over(
                                    &crate::connections::load_preferences(&path)
                                        .map_err(|error| error.to_string())?,
                                ),
                                PreferenceJob::Reset => {
                                    crate::connections::reset_automatic(&path)
                                        .map_err(|error| error.to_string())?;
                                    Selection::default()
                                }
                                PreferenceJob::Save | PreferenceJob::Browse => selection,
                            };
                            let discovery = crate::connections::discover(&selected)
                                .map_err(|error| error.to_string())?;
                            if matches!(job, PreferenceJob::Save) {
                                crate::connections::save_preferences(&path, &selected)
                                    .map_err(|error| error.to_string())?;
                            }
                            Ok((discovery, selected))
                        })
                        .await
                        .map_err(|_| "Connection discovery could not complete".to_owned())?
                    };
                    let result = tokio::time::timeout(std::time::Duration::from_secs(8), future)
                        .await
                        .unwrap_or_else(|_| {
                            Err("Connection discovery timed out; retry in Settings".into())
                        });
                    Some(Message::Discovered(id, Box::new(result)))
                },
            ));
        update
    }
}

#[derive(Clone, Copy)]
enum PreferenceJob {
    Load,
    Browse,
    Save,
    Reset,
}
