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
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const SURFACE_OPEN_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Queued request to open one client-rendered interactive surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveSurfaceRequest {
    interaction_id: String,
    surface_kind: String,
    request_json: String,
    retry_at: Option<Instant>,
}

impl InteractiveSurfaceRequest {
    /// Create a queued surface-open request.
    #[must_use]
    pub fn new(
        interaction_id: impl Into<String>,
        surface_kind: impl Into<String>,
        request_json: impl Into<String>,
    ) -> Self {
        Self {
            interaction_id: interaction_id.into(),
            surface_kind: surface_kind.into(),
            request_json: request_json.into(),
            retry_at: None,
        }
    }

    /// Return the interaction identifier.
    #[must_use]
    pub fn interaction_id(&self) -> &str {
        &self.interaction_id
    }

    /// Return whether another open attempt may start now.
    #[must_use]
    pub fn ready(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|retry_at| now >= retry_at)
    }

    /// Defer another open attempt after a failed surface initialization.
    pub fn defer_retry(&mut self, now: Instant) {
        self.retry_at = now.checked_add(SURFACE_OPEN_RETRY_DELAY);
    }
}

/// Deterministic, de-duplicated queue of pending interactive surfaces.
#[derive(Debug, Default)]
pub struct InteractiveSurfaceQueue {
    pending: VecDeque<InteractiveSurfaceRequest>,
}

impl InteractiveSurfaceQueue {
    /// Queue a request unless it is already active or pending.
    pub fn enqueue(
        &mut self,
        request: InteractiveSurfaceRequest,
        active_interaction_id: Option<&str>,
    ) -> bool {
        if active_interaction_id == Some(request.interaction_id())
            || self
                .pending
                .iter()
                .any(|pending| pending.interaction_id() == request.interaction_id())
        {
            return false;
        }
        self.pending.push_back(request);
        true
    }

    /// Return the next request when its retry delay has elapsed.
    #[must_use]
    pub fn front_ready(&self, now: Instant) -> Option<&InteractiveSurfaceRequest> {
        self.pending.front().filter(|request| request.ready(now))
    }

    /// Return the next deferred open retry time.
    #[must_use]
    pub fn next_retry_at(&self) -> Option<Instant> {
        self.pending.front().and_then(|request| request.retry_at)
    }

    /// Remove and return the next request.
    pub fn pop_front(&mut self) -> Option<InteractiveSurfaceRequest> {
        self.pending.pop_front()
    }

    /// Defer the next request after a failed open attempt.
    pub fn defer_front(&mut self, now: Instant) {
        if let Some(request) = self.pending.front_mut() {
            request.defer_retry(now);
        }
    }

    /// Remove a resolved request from the queue.
    pub fn remove(&mut self, interaction_id: &str) -> bool {
        let original_len = self.pending.len();
        self.pending
            .retain(|request| request.interaction_id() != interaction_id);
        self.pending.len() != original_len
    }

    /// Retain only interactions still reported pending by authoritative hydration.
    pub fn retain(&mut self, interaction_ids: &std::collections::BTreeSet<String>) {
        self.pending
            .retain(|request| interaction_ids.contains(request.interaction_id()));
    }

    /// Clear queued requests when changing sessions.
    pub fn clear(&mut self) {
        self.pending.clear();
    }

    /// Return queued interaction ids in deterministic presentation order.
    #[cfg(test)]
    pub(crate) fn interaction_ids(&self) -> Vec<&str> {
        self.pending
            .iter()
            .map(InteractiveSurfaceRequest::interaction_id)
            .collect()
    }
}

/// Runtime state for one client-rendered interactive tool surface.
pub struct InteractiveSurfaceState {
    interaction_id: String,
    surface: BoxedPluginTuiSurface,
    host: TokioPluginTuiHost,
    pending_resolution: Option<ToolExchangeResolution>,
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
            pending_resolution: None,
        })
    }

    /// Open one queued surface request.
    ///
    /// # Errors
    ///
    /// Returns an error when no plugin declares the surface kind or the factory fails.
    pub async fn open_request(
        runtime: &PluginRuntimeHost,
        request: &InteractiveSurfaceRequest,
    ) -> Result<Self, PluginLoadError> {
        Self::open(
            runtime,
            request.interaction_id.clone(),
            request.surface_kind.clone(),
            &request.request_json,
        )
        .await
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

    /// Clear a pending resolution so the user can retry after host delivery fails.
    pub fn clear_pending_resolution(&mut self) {
        self.pending_resolution = None;
    }

    /// Handle an input event and retain a close resolution until the host confirms delivery.
    pub fn handle_event(&mut self, event: &Event) -> Option<ToolExchangeResolution> {
        if let Some(resolution) = &self.pending_resolution {
            return Some(resolution.clone());
        }
        let resolution = match self.surface.handle_event(event, &self.host) {
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
        };
        self.pending_resolution.clone_from(&resolution);
        resolution
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

    fn shifted_key(key: KeyCode) -> Event {
        Event::Key(KeyStroke {
            key,
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        })
    }

    async fn question_surface(questions: serde_json::Value) -> InteractiveSurfaceState {
        let plugin = bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/question-plugin/bcode-plugin.toml"),
            bcode_question_plugin::static_plugin(),
        );
        let runtime = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection::all_enabled(),
            &[plugin],
        )
        .expect("load local question plugin runtime");
        InteractiveSurfaceState::open(
            &runtime,
            "question-call-question",
            "bcode.question.inline",
            &serde_json::json!({ "questions": questions }).to_string(),
        )
        .await
        .expect("open local question TUI surface")
    }

    #[test]
    fn surface_queue_is_fifo_deduplicated_and_reconciles_resolved_requests() {
        let mut queue = InteractiveSurfaceQueue::default();
        let first = InteractiveSurfaceRequest::new("first", "surface", "{}");
        let duplicate = first.clone();
        let second = InteractiveSurfaceRequest::new("second", "surface", "{}");

        assert!(queue.enqueue(first, None));
        assert!(!queue.enqueue(duplicate, None));
        assert!(!queue.enqueue(
            InteractiveSurfaceRequest::new("active", "surface", "{}"),
            Some("active")
        ));
        assert!(queue.enqueue(second, None));
        assert_eq!(queue.interaction_ids(), ["first", "second"]);

        assert!(queue.remove("first"));
        assert_eq!(queue.interaction_ids(), ["second"]);
        queue.retain(&std::collections::BTreeSet::new());
        assert!(queue.interaction_ids().is_empty());
    }

    #[test]
    fn surface_queue_defers_failed_open_attempts() {
        let mut queue = InteractiveSurfaceQueue::default();
        assert!(queue.enqueue(
            InteractiveSurfaceRequest::new("first", "surface", "{}"),
            None
        ));
        let now = Instant::now();
        assert!(queue.front_ready(now).is_some());
        queue.defer_front(now);
        assert!(queue.front_ready(now).is_none());
        assert_eq!(queue.next_retry_at(), Some(now + SURFACE_OPEN_RETRY_DELAY));
        assert!(queue.front_ready(now + SURFACE_OPEN_RETRY_DELAY).is_some());
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

        assert!(surface.handle_event(&key(KeyCode::Enter)).is_none());
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

    #[tokio::test]
    async fn question_surface_supports_reverse_navigation_and_required_validation() {
        let mut surface = question_surface(serde_json::json!([{
            "header": null,
            "question": "Choose one",
            "options": [
                {"label": "One", "value": "one", "description": null},
                {"label": "Two", "value": "two", "description": null}
            ],
            "control": "radio",
            "selection_mode": "single",
            "custom": false,
            "custom_mode": "additional",
            "required": true
        }]))
        .await;

        assert!(surface.handle_event(&key(KeyCode::Tab)).is_none());
        assert!(surface.handle_event(&shifted_key(KeyCode::Tab)).is_none());
        assert!(surface.handle_event(&key(KeyCode::Tab)).is_none());
        assert!(surface.handle_event(&key(KeyCode::Tab)).is_none());
        assert!(surface.handle_event(&key(KeyCode::Enter)).is_none());
        assert!(surface.handle_event(&key(KeyCode::Enter)).is_none());
        assert!(surface.handle_event(&key(KeyCode::Tab)).is_none());
        assert!(surface.handle_event(&key(KeyCode::Tab)).is_none());
        let resolution = surface
            .handle_event(&key(KeyCode::Enter))
            .expect("submit after answering required question");
        assert_eq!(
            resolution,
            ToolExchangeResolution::Responded {
                payload: serde_json::json!({
                    "status": "answered",
                    "questions": [{
                        "question_index": 0,
                        "selected": ["one"],
                        "custom": null
                    }]
                }),
            }
        );
    }
}
