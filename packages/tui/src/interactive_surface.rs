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
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};

#[derive(Debug)]
struct InteractiveTextInputResolver {
    keymap: BmuxKeyMap,
}

impl bcode_plugin_sdk::tui::PluginTuiTextInputResolver for InteractiveTextInputResolver {
    fn edit_command(
        &self,
        stroke: bmux_keyboard::KeyStroke,
    ) -> Option<bmux_text_edit::TextEditCommand> {
        self.keymap.editor_command_for_key(stroke)
    }

    fn selection_motion(
        &self,
        stroke: bmux_keyboard::KeyStroke,
    ) -> Option<bmux_text_edit::TextMotion> {
        self.keymap.editor_selection_motion_for_key(stroke)
    }

    fn submits(&self, stroke: bmux_keyboard::KeyStroke) -> bool {
        matches!(
            self.keymap.action_for_key(BmuxScope::Chat, stroke),
            Some(BmuxAction::InputSubmitSteering | BmuxAction::InputSubmitFollowUp)
        )
    }
}

const SURFACE_OPEN_RETRY_DELAY: Duration = Duration::from_secs(2);
const MAX_INTERACTION_SCRATCH_ROWS: u16 = 512;
const MAX_INTERACTION_SCRATCH_CELLS: usize = 131_072;

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
        keymap: &BmuxKeyMap,
    ) -> Result<Self, PluginLoadError> {
        let interaction_id = interaction_id.into();
        let surface_kind = surface_kind.into();
        let request = serde_json::from_str(request_json).unwrap_or_else(|_| json!({}));
        let (redraw_sender, _redraw_receiver) = mpsc::unbounded_channel();
        let host = TokioPluginTuiHost::current(redraw_sender).with_text_input_resolver(Arc::new(
            InteractiveTextInputResolver {
                keymap: keymap.clone(),
            },
        ));
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
        keymap: &BmuxKeyMap,
    ) -> Result<Self, PluginLoadError> {
        Self::open(
            runtime,
            request.interaction_id.clone(),
            request.surface_kind.clone(),
            &request.request_json,
            keymap,
        )
        .await
    }

    /// Update configured composer-like text input intent without reopening the surface.
    pub fn update_keymap(&mut self, keymap: &BmuxKeyMap) {
        self.host
            .set_text_input_resolver(Arc::new(InteractiveTextInputResolver {
                keymap: keymap.clone(),
            }));
    }

    #[cfg(test)]
    fn text_edit_command_for_test(
        &self,
        stroke: bmux_keyboard::KeyStroke,
    ) -> Option<bmux_text_edit::TextEditCommand> {
        bcode_plugin_sdk::tui::PluginTuiHost::text_edit_command(&self.host, stroke)
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
        bounded_surface_height(width, self.surface.preferred_height(width))
    }

    /// Render the interactive surface.
    pub fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        self.surface.render(area, frame);
    }

    /// Render a clipped slice of the interactive surface using full-surface coordinates.
    pub fn render_clipped(
        &mut self,
        full_area: Rect,
        visible_content_offset: u16,
        destination: Rect,
        frame: &mut Frame<'_>,
    ) {
        if full_area.is_empty() || destination.is_empty() {
            return;
        }
        let bounded_height = bounded_surface_height(full_area.width, full_area.height);
        let full_area = Rect::new(full_area.x, full_area.y, full_area.width, bounded_height);
        if visible_content_offset >= bounded_height {
            return;
        }
        let mut buffer = bmux_tui::buffer::Buffer::empty(full_area);
        let mut scratch = Frame::new(&mut buffer);
        self.surface.render(full_area, &mut scratch);
        for destination_y in destination.y..destination.bottom() {
            let source_y = full_area
                .y
                .saturating_add(visible_content_offset)
                .saturating_add(destination_y.saturating_sub(destination.y));
            for destination_x in destination.x..destination.right() {
                let source_x = full_area
                    .x
                    .saturating_add(destination_x.saturating_sub(destination.x));
                let Some(cell) = scratch
                    .buffer()
                    .get(bmux_tui::geometry::Point::new(source_x, source_y))
                else {
                    continue;
                };
                frame.buffer_mut().set_cell(
                    bmux_tui::geometry::Point::new(destination_x, destination_y),
                    cell.symbol.clone(),
                    cell.style,
                );
            }
        }
        if let Some(cursor) = scratch.cursor()
            && cursor.visible
            && cursor.position.y >= full_area.y.saturating_add(visible_content_offset)
            && cursor.position.y
                < full_area
                    .y
                    .saturating_add(visible_content_offset)
                    .saturating_add(destination.height)
        {
            frame.set_cursor(bmux_tui::frame::Cursor {
                position: bmux_tui::geometry::Point::new(
                    destination
                        .x
                        .saturating_add(cursor.position.x.saturating_sub(full_area.x)),
                    destination.y.saturating_add(
                        cursor
                            .position
                            .y
                            .saturating_sub(full_area.y)
                            .saturating_sub(visible_content_offset),
                    ),
                ),
                visible: true,
            });
        }
    }

    #[cfg(test)]
    pub(crate) fn render_for_test(&mut self, area: Rect, frame: &mut Frame<'_>) {
        self.render(area, frame);
    }

    /// Translate a mouse event from a clipped destination into full-surface coordinates.
    #[must_use]
    pub fn translate_clipped_event(
        event: Event,
        full_area: Rect,
        visible_content_offset: u16,
        destination: Rect,
    ) -> Event {
        let Event::Mouse(mut mouse) = event else {
            return event;
        };
        if destination.contains(mouse.position) {
            mouse.position = bmux_tui::geometry::Point::new(
                full_area
                    .x
                    .saturating_add(mouse.position.x.saturating_sub(destination.x)),
                full_area
                    .y
                    .saturating_add(visible_content_offset)
                    .saturating_add(mouse.position.y.saturating_sub(destination.y)),
            );
        }
        Event::Mouse(mouse)
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
            | PluginTuiAction::OpenSession { .. }
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

fn bounded_surface_height(width: u16, preferred_height: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    let row_limit = MAX_INTERACTION_SCRATCH_ROWS;
    let cell_limit =
        u16::try_from(MAX_INTERACTION_SCRATCH_CELLS / usize::from(width)).unwrap_or(u16::MAX);
    preferred_height.min(row_limit).min(cell_limit)
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

    async fn question_surface_with_config(
        questions: serde_json::Value,
        config: &bcode_config::TuiConfig,
    ) -> InteractiveSurfaceState {
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
            &BmuxKeyMap::from_config(config),
        )
        .await
        .expect("open local question TUI surface")
    }

    async fn question_surface(questions: serde_json::Value) -> InteractiveSurfaceState {
        question_surface_with_config(questions, &bcode_config::TuiConfig::default()).await
    }

    #[test]
    fn preferred_height_is_bounded_by_rows_and_checked_cell_budget() {
        assert_eq!(bounded_surface_height(0, u16::MAX), 0);
        assert_eq!(
            bounded_surface_height(1, u16::MAX),
            MAX_INTERACTION_SCRATCH_ROWS
        );
        assert_eq!(bounded_surface_height(u16::MAX, u16::MAX), 2);
        let height = bounded_surface_height(400, u16::MAX);
        assert!(height <= MAX_INTERACTION_SCRATCH_ROWS);
        assert!(usize::from(height) * 400 <= MAX_INTERACTION_SCRATCH_CELLS);
    }

    #[tokio::test]
    async fn clipped_surface_rendering_and_mouse_translation_preserve_full_coordinates() {
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
        let full_area = Rect::new(0, 0, 40, surface.preferred_height(40));
        let destination = Rect::new(5, 7, 40, full_area.height.saturating_sub(1));
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 50, 20));
        surface.render_clipped(full_area, 1, destination, &mut Frame::new(&mut buffer));
        assert!(
            buffer
                .row_symbols(destination.y)
                .is_some_and(|row| !row.trim().is_empty())
        );

        let translated = InteractiveSurfaceState::translate_clipped_event(
            Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(7, 9),
            )),
            full_area,
            1,
            destination,
        );
        assert!(matches!(
            translated,
            Event::Mouse(mouse)
                if mouse.position == bmux_tui::geometry::Point::new(2, 3)
        ));
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
    async fn active_surface_keymap_updates_without_reopening() {
        let mut surface = question_surface(serde_json::json!([{
            "header": null,
            "question": "Explain?",
            "options": [],
            "control": "radio",
            "selection_mode": "single",
            "custom": true,
            "custom_mode": "additional",
            "required": true
        }]))
        .await;
        let stroke = KeyStroke {
            key: KeyCode::Char('b'),
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        };
        assert_eq!(surface.text_edit_command_for_test(stroke), None);

        let mut config = bcode_config::TuiConfig::default();
        config
            .keybindings
            .chat
            .insert("ctrl+b".to_owned(), "tui.editor.moveCursorLeft".to_owned());
        surface.update_keymap(&BmuxKeyMap::from_config(&config));

        assert_eq!(
            surface.text_edit_command_for_test(stroke),
            Some(bmux_text_edit::TextEditCommand::Move(
                bmux_text_edit::TextMotion::Left
            ))
        );
    }

    #[tokio::test]
    async fn configured_editor_bindings_are_forwarded_to_question_text_input() {
        let mut config = bcode_config::TuiConfig::default();
        config
            .keybindings
            .chat
            .insert("ctrl+b".to_owned(), "tui.editor.moveCursorLeft".to_owned());
        config.keybindings.chat.insert(
            "ctrl+d".to_owned(),
            "tui.editor.deleteCharForward".to_owned(),
        );
        let mut surface = question_surface_with_config(
            serde_json::json!([{
                "header": null,
                "question": "Explain?",
                "options": [],
                "control": "radio",
                "selection_mode": "single",
                "custom": true,
                "custom_mode": "additional",
                "required": true
            }]),
            &config,
        )
        .await;
        let area = Rect::new(0, 0, 48, 12);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        surface.render_for_test(area, &mut Frame::new(&mut buffer));

        for character in ['a', 'b'] {
            assert!(
                surface
                    .handle_event(&Event::Key(KeyStroke {
                        key: KeyCode::Char(character),
                        modifiers: Modifiers::NONE,
                    }))
                    .is_none()
            );
        }
        assert!(
            surface
                .handle_event(&Event::Key(KeyStroke {
                    key: KeyCode::Char('b'),
                    modifiers: Modifiers {
                        ctrl: true,
                        ..Modifiers::NONE
                    },
                }))
                .is_none()
        );
        assert!(
            surface
                .handle_event(&Event::Key(KeyStroke {
                    key: KeyCode::Char('d'),
                    modifiers: Modifiers {
                        ctrl: true,
                        ..Modifiers::NONE
                    },
                }))
                .is_none()
        );
        let submitted = surface
            .handle_event(&key(KeyCode::Enter))
            .expect("configured edit leaves a valid answer that submits");
        assert_eq!(
            submitted,
            ToolExchangeResolution::Responded {
                payload: serde_json::json!({
                    "status": "answered",
                    "questions": [{
                        "question_index": 0,
                        "selected": [],
                        "custom": "a"
                    }]
                })
            }
        );
    }

    #[tokio::test]
    async fn submitted_resolution_is_retained_until_host_confirmation_and_can_retry() {
        let plugin_runtime = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection::all_enabled(),
            &[bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../plugins/question-plugin/bcode-plugin.toml"),
                bcode_question_plugin::static_plugin(),
            )],
        )
        .expect("question runtime");
        let mut surface = InteractiveSurfaceState::open(
            &plugin_runtime,
            "question-call-question",
            "bcode.question.inline",
            &serde_json::json!({
                "questions": [{
                    "header": null,
                    "question": "Proceed?",
                    "options": [{"label": "Yes", "value": "yes", "description": null}],
                    "control": "radio",
                    "selection_mode": "single",
                    "custom": false,
                    "custom_mode": "additional",
                    "required": true
                }]
            })
            .to_string(),
            &BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
        )
        .await
        .expect("question surface");

        assert!(surface.handle_event(&key(KeyCode::Enter)).is_none());
        assert!(surface.handle_event(&key(KeyCode::Tab)).is_none());
        let submitted = surface
            .handle_event(&key(KeyCode::Enter))
            .expect("submitted response");
        let ToolExchangeResolution::Responded {
            payload: submitted_payload,
        } = &submitted
        else {
            panic!("question must submit an answered response");
        };
        assert_eq!(submitted_payload["status"], "answered");
        assert_eq!(submitted_payload["questions"][0]["selected"][0], "yes");
        assert_eq!(
            surface.handle_event(&key(KeyCode::Escape)),
            Some(submitted.clone())
        );
        surface.clear_pending_resolution();
        assert_eq!(surface.handle_event(&key(KeyCode::Enter)), Some(submitted));
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
            &BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
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
