//! Generic inline interactive surface host for tool interactions.

use bcode_plugin::{PluginLoadError, PluginRuntimeHost};
use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiAction, PluginTuiSurfaceOpenRequest, TokioPluginTuiHost,
};
use bcode_session_models::ToolExchangeResolution;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use serde_json::json;
use tokio::sync::mpsc;

/// Runtime state for one client-rendered interactive tool surface.
pub struct InteractiveSurfaceState {
    interaction_id: String,
    surface: BoxedPluginTuiSurface,
    host: TokioPluginTuiHost,
}

impl InteractiveSurfaceState {
    /// Open an inline surface from the plugin runtime by surface kind.
    ///
    /// # Errors
    ///
    /// Returns an error when no plugin declares the surface kind or the factory fails.
    pub async fn open(
        runtime: &PluginRuntimeHost,
        interaction_id: impl Into<String>,
        surface_kind: impl Into<String>,
        request_json: &str,
    ) -> Result<Self, PluginLoadError> {
        let interaction_id = interaction_id.into();
        let surface_kind = surface_kind.into();
        let request = serde_json::from_str(request_json).unwrap_or_else(|_| json!({}));
        let (redraw_sender, _redraw_receiver) = mpsc::unbounded_channel();
        let host = TokioPluginTuiHost::current(redraw_sender);
        let (plugin_id, surface) =
            open_surface(runtime, &interaction_id, &surface_kind, request).await?;
        let _ = plugin_id;
        Ok(Self {
            interaction_id,
            surface,
            host,
        })
    }

    /// Return the interaction id associated with this surface.
    #[must_use]
    pub fn interaction_id(&self) -> &str {
        &self.interaction_id
    }

    /// Return a user-dismissed resolution for host-level cancellation.
    #[must_use]
    pub fn dismissed_resolution() -> ToolExchangeResolution {
        user_dismissed()
    }

    /// Return preferred rendered height at `width`.
    #[must_use]
    pub fn preferred_height(&mut self, width: u16) -> u16 {
        self.surface.preferred_height(width)
    }

    /// Render the interactive surface.
    pub fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        self.surface.render(area, frame);
    }

    /// Handle an input event and return a close resolution when submitted or cancelled.
    pub fn handle_event(&mut self, event: &Event) -> Option<ToolExchangeResolution> {
        match self.surface.handle_event(event, &self.host) {
            PluginTuiAction::None
            | PluginTuiAction::Redraw
            | PluginTuiAction::OpenSurface { .. } => None,
            PluginTuiAction::Close { outcome } => {
                Some(outcome.map_or_else(user_dismissed, |payload| {
                    ToolExchangeResolution::Responded { payload }
                }))
            }
            PluginTuiAction::RunCommand { command } => Some(ToolExchangeResolution::Responded {
                payload: json!({ "run_command": command }),
            }),
        }
    }
}

async fn open_surface(
    runtime: &PluginRuntimeHost,
    interaction_id: &str,
    surface_kind: &str,
    options: serde_json::Value,
) -> Result<(String, BoxedPluginTuiSurface), PluginLoadError> {
    for plugin_id in runtime.plugin_ids() {
        if runtime
            .registry()
            .tui_surface(&plugin_id, surface_kind)
            .is_none()
        {
            continue;
        }
        let registry = crate::plugin_tui::tui_registry(&plugin_id)
            .ok_or_else(|| PluginLoadError::PluginNotLoaded(plugin_id.clone()))?;
        let request = PluginTuiSurfaceOpenRequest {
            instance_id: interaction_id.to_owned(),
            repo_path: None,
            target: None,
            options,
        };
        let surface = registry
            .open(surface_kind, request)
            .await
            .map_err(|error| PluginLoadError::TuiSurfaceOpen {
                plugin_id: plugin_id.clone(),
                message: error.to_string(),
            })?;
        return Ok((plugin_id, surface));
    }
    Err(PluginLoadError::TuiSurfaceOpen {
        plugin_id: "<unknown>".to_owned(),
        message: format!("no plugin declares TUI surface kind '{surface_kind}'"),
    })
}

fn user_dismissed() -> ToolExchangeResolution {
    ToolExchangeResolution::Responded {
        payload: json!({"status": "dismissed"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};

    fn key(key: KeyCode) -> Event {
        Event::Key(KeyStroke {
            key,
            modifiers: Modifiers::NONE,
        })
    }

    #[tokio::test]
    async fn question_exchange_payload_runs_entirely_in_local_tui_surface() {
        let plugin = bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/question-plugin/bcode-plugin.toml"),
            bcode_question_plugin::static_plugin(),
        );
        let runtime = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection::all_enabled(),
            &[plugin],
        )
        .expect("load local question plugin runtime");
        let mut surface = InteractiveSurfaceState::open(
            &runtime,
            "question-call-question",
            "bcode.question.inline",
            &serde_json::json!({
                "questions": [{
                    "header": null,
                    "question": "Proceed?",
                    "options": [{
                        "label": "Yes",
                        "value": "yes",
                        "description": null
                    }],
                    "control": "radio",
                    "selection_mode": "single",
                    "custom": false,
                    "custom_mode": "additional",
                    "required": true
                }]
            })
            .to_string(),
        )
        .await
        .expect("open local question TUI surface");

        assert!(surface.handle_event(&key(KeyCode::Char('1'))).is_none());
        assert!(surface.handle_event(&key(KeyCode::Tab)).is_none());
        let resolution = surface
            .handle_event(&key(KeyCode::Enter))
            .expect("submit selected question answer");

        assert_eq!(
            resolution,
            ToolExchangeResolution::Responded {
                payload: serde_json::json!({
                    "status": "answered",
                    "questions": [{
                        "question_index": 0,
                        "selected": ["yes"],
                        "custom": null
                    }]
                }),
            }
        );
    }
}
