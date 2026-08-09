//! Generic inline interactive surface host for tool interactions.

use bcode_plugin::{PluginLoadError, PluginRuntimeHost};
#[cfg(test)]
use bcode_plugin_sdk::tui::PluginTuiHost;
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
const MAX_INTERACTION_LOGICAL_ROWS: u16 = 4_096;
const MAX_INTERACTION_VISIBLE_ROWS: u16 = 512;
const MAX_INTERACTION_VISIBLE_CELLS: usize = 131_072;

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

/// Host routing result for one interactive-surface terminal event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveSurfaceEventOutcome {
    /// The surface ignored the event, so normal host routing may continue.
    Ignored,
    /// The surface consumed the event without completing the interaction.
    Consumed,
    /// The surface completed the interaction with a canonical resolution.
    Resolved(ToolExchangeResolution),
}

/// Runtime state for one client-rendered interactive tool surface.
pub struct InteractiveSurfaceState {
    interaction_id: String,
    surface: BoxedPluginTuiSurface,
    host: TokioPluginTuiHost,
    pending_resolution: Option<ToolExchangeResolution>,
}

impl InteractiveSurfaceState {
    #[cfg(test)]
    pub(crate) fn from_surface_for_test(
        interaction_id: impl Into<String>,
        surface: BoxedPluginTuiSurface,
        keymap: &BmuxKeyMap,
    ) -> Self {
        let (redraw_sender, _redraw_receiver) = mpsc::channel(1);
        Self {
            interaction_id: interaction_id.into(),
            surface,
            host: TokioPluginTuiHost::current(redraw_sender).with_text_input_resolver(Arc::new(
                InteractiveTextInputResolver {
                    keymap: keymap.clone(),
                },
            )),
            pending_resolution: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn redraw_channel_is_bounded_for_test() {
        let (redraw_sender, mut redraw_receiver) = mpsc::channel(1);
        let host = TokioPluginTuiHost::current(redraw_sender);

        host.request_redraw();
        host.request_redraw();

        assert_eq!(redraw_receiver.try_recv(), Ok(()));
        assert!(redraw_receiver.try_recv().is_err());
    }

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
        let (redraw_sender, _redraw_receiver) = mpsc::channel(1);
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
    pub(crate) fn text_edit_command_for_test(
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

    /// Return a bounded diagnostic identifier for the active native surface.
    #[cfg(test)]
    pub(crate) fn surface_id_for_test(&self) -> &'static str {
        self.surface.id()
    }

    /// Return preferred rendered height at `width`.
    #[must_use]
    pub fn preferred_height(&mut self, width: u16) -> u16 {
        bounded_surface_height(width, self.surface.preferred_height(width))
    }

    /// Render a bounded logical slice without allocating the full logical surface.
    pub fn render_slice(
        &mut self,
        logical_height: u16,
        logical_row_offset: u16,
        destination: Rect,
        frame: &mut Frame<'_>,
    ) {
        let logical_height = logical_height.min(MAX_INTERACTION_LOGICAL_ROWS);
        if logical_row_offset >= logical_height {
            return;
        }
        let Some(destination) = bounded_render_destination(
            destination,
            logical_height.saturating_sub(logical_row_offset),
        ) else {
            return;
        };
        self.surface
            .render_slice(logical_height, logical_row_offset, destination, frame);
    }

    /// Return focused logical rows for host-owned pinned reveal behavior.
    #[must_use]
    #[allow(dead_code)]
    pub fn focused_row_range(&mut self, width: u16) -> Option<std::ops::Range<u16>> {
        self.surface.focused_row_range(width)
    }

    /// Render the interactive surface.
    pub fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        self.surface.render(area, frame);
    }

    /// Render a clipped slice of the interactive surface using full-surface coordinates.
    #[cfg(test)]
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

    /// Handle an input event, report consumption, and retain close resolution until confirmation.
    pub fn handle_event_outcome(&mut self, event: &Event) -> InteractiveSurfaceEventOutcome {
        if let Some(resolution) = &self.pending_resolution {
            return InteractiveSurfaceEventOutcome::Resolved(resolution.clone());
        }
        let outcome = match self.surface.handle_event(event, &self.host) {
            PluginTuiAction::None => InteractiveSurfaceEventOutcome::Ignored,
            PluginTuiAction::Redraw
            | PluginTuiAction::OpenSession { .. }
            | PluginTuiAction::OpenSurface { .. } => InteractiveSurfaceEventOutcome::Consumed,
            PluginTuiAction::Close { outcome } => InteractiveSurfaceEventOutcome::Resolved(
                outcome.map_or_else(user_dismissed, |payload| {
                    ToolExchangeResolution::Responded { payload }
                }),
            ),
            PluginTuiAction::RunCommand { command } => {
                InteractiveSurfaceEventOutcome::Resolved(ToolExchangeResolution::Responded {
                    payload: json!({ "run_command": command }),
                })
            }
        };
        if let InteractiveSurfaceEventOutcome::Resolved(resolution) = &outcome {
            self.pending_resolution = Some(resolution.clone());
        }
        outcome
    }

    #[cfg(test)]
    fn handle_event(&mut self, event: &Event) -> Option<ToolExchangeResolution> {
        match self.handle_event_outcome(event) {
            InteractiveSurfaceEventOutcome::Resolved(resolution) => Some(resolution),
            InteractiveSurfaceEventOutcome::Ignored | InteractiveSurfaceEventOutcome::Consumed => {
                None
            }
        }
    }
}

fn bounded_surface_height(width: u16, preferred_height: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    preferred_height.clamp(1, MAX_INTERACTION_LOGICAL_ROWS)
}

fn bounded_render_destination(destination: Rect, remaining_rows: u16) -> Option<Rect> {
    if destination.is_empty() || remaining_rows == 0 {
        return None;
    }
    let cell_rows = u16::try_from(
        MAX_INTERACTION_VISIBLE_CELLS
            .checked_div(usize::from(destination.width))
            .unwrap_or(0),
    )
    .unwrap_or(u16::MAX);
    let height = destination
        .height
        .min(remaining_rows)
        .min(MAX_INTERACTION_VISIBLE_ROWS)
        .min(cell_rows);
    (height > 0).then(|| Rect::new(destination.x, destination.y, destination.width, height))
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

    fn render_surface_frame(
        surface: &mut InteractiveSurfaceState,
        area: Rect,
        underpaint: bool,
    ) -> (String, Option<bmux_tui::frame::Cursor>) {
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        if underpaint {
            frame.fill(area, " ", bmux_tui::prelude::Style::new());
        }
        surface.render(area, &mut frame);
        let cursor = frame.cursor();
        let text = (0..area.height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n");
        (text, cursor)
    }

    #[test]
    fn logical_height_and_visible_render_work_have_independent_host_limits() {
        assert_eq!(bounded_surface_height(0, u16::MAX), 0);
        assert_eq!(bounded_surface_height(1, 0), 1);
        assert_eq!(bounded_surface_height(1, 512), 512);
        assert_eq!(
            bounded_surface_height(1, u16::MAX),
            MAX_INTERACTION_LOGICAL_ROWS
        );

        let narrow = bounded_render_destination(Rect::new(0, 0, 1, u16::MAX), u16::MAX)
            .expect("narrow destination");
        assert_eq!(narrow.height, MAX_INTERACTION_VISIBLE_ROWS);

        let wide = bounded_render_destination(Rect::new(0, 0, u16::MAX, u16::MAX), u16::MAX)
            .expect("wide destination");
        assert_eq!(wide.height, 2);
        assert!(
            usize::from(wide.width)
                .checked_mul(usize::from(wide.height))
                .is_some_and(|cells| cells <= MAX_INTERACTION_VISIBLE_CELLS)
        );
        assert_eq!(
            bounded_render_destination(Rect::new(0, 0, 80, 20), 3)
                .expect("remaining rows")
                .height,
            3
        );
        assert!(bounded_render_destination(Rect::new(0, 0, 0, 20), 20).is_none());
        assert!(bounded_render_destination(Rect::new(0, 0, 80, 20), 0).is_none());
    }

    #[tokio::test]
    async fn accepted_limit_question_is_logically_bounded_and_renders_only_a_visible_slice() {
        let options = (0..100)
            .map(|index| {
                serde_json::json!({
                    "label": format!("option-{index}"),
                    "description": "description"
                })
            })
            .collect::<Vec<_>>();
        let questions = (0..32)
            .map(|index| {
                serde_json::json!({
                    "question": format!("question-{index}"),
                    "options": options,
                    "control": "radio",
                    "selection_mode": "single",
                    "custom": false,
                    "custom_mode": "additional",
                    "required": true
                })
            })
            .collect::<Vec<_>>();
        let mut surface = question_surface(serde_json::Value::Array(questions)).await;

        assert_eq!(surface.preferred_height(8), MAX_INTERACTION_LOGICAL_ROWS);

        let area = Rect::new(0, 0, 8, 600);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        frame.fill(area, ".", bmux_tui::prelude::Style::new());
        surface.render_slice(MAX_INTERACTION_LOGICAL_ROWS, 0, area, &mut frame);

        assert_ne!(buffer.row_symbols(0).as_deref(), Some("........"));
        assert_eq!(
            buffer.row_symbols(MAX_INTERACTION_VISIBLE_ROWS).as_deref(),
            Some("........")
        );
    }

    #[tokio::test]
    async fn clipped_surface_rendering_preserves_cursor_for_top_and_bottom_slices() {
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
        let full_area = Rect::new(0, 0, 30, surface.preferred_height(30));
        let mut full_buffer = bmux_tui::buffer::Buffer::empty(full_area);
        let mut full_frame = Frame::new(&mut full_buffer);
        surface.render(full_area, &mut full_frame);
        let cursor = full_frame.cursor().expect("focused custom cursor");

        for (offset, height, visible) in [
            (0, cursor.position.y.saturating_add(1), true),
            (cursor.position.y, 1, true),
            (cursor.position.y.saturating_add(1), 1, false),
        ] {
            let destination = Rect::new(3, 4, full_area.width, height.max(1));
            let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 40, 20));
            let mut frame = Frame::new(&mut buffer);
            surface.render_clipped(full_area, offset, destination, &mut frame);
            assert_eq!(frame.cursor().is_some_and(|cursor| cursor.visible), visible);
        }
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

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // One ordered frame scenario checks clipping, input translation, resize, and repaint together.
    async fn interaction_frame_sequence_preserves_inline_clipping_cursor_translation_and_pinned_repaint()
     {
        let questions = serde_json::json!([{
            "header": null,
            "question": "Explain the mixed-case result?",
            "options": [],
            "control": "radio",
            "selection_mode": "single",
            "custom": true,
            "custom_mode": "additional",
            "required": true
        }]);
        let mut surface = question_surface(questions.clone()).await;
        let initial_area = Rect::new(0, 0, 38, surface.preferred_height(38));
        let (initial, initial_cursor) = render_surface_frame(&mut surface, initial_area, false);
        let initial_cursor = initial_cursor.expect("initial custom cursor");
        assert!(initial.contains("Explain the mixed-case result?"));

        for character in ['A', 'b', 'Ç'] {
            assert!(
                surface
                    .handle_event(&Event::Key(KeyStroke {
                        key: KeyCode::Char(character),
                        modifiers: Modifiers::NONE,
                    }))
                    .is_none()
            );
        }
        let (edited, edited_cursor) = render_surface_frame(&mut surface, initial_area, false);
        let edited_cursor = edited_cursor.expect("edited custom cursor");
        assert!(edited.contains("AbÇ"), "{edited}");
        assert!(edited_cursor.position.x > initial_cursor.position.x);

        let visible_offset = edited_cursor.position.y;
        let clipped_destination = Rect::new(4, 6, initial_area.width, 1);
        let mut clipped_buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 48, 16));
        let mut clipped_frame = Frame::new(&mut clipped_buffer);
        surface.render_clipped(
            initial_area,
            visible_offset,
            clipped_destination,
            &mut clipped_frame,
        );
        assert_eq!(
            clipped_frame
                .cursor()
                .expect("visible clipped cursor")
                .position
                .y,
            clipped_destination.y
        );
        assert!(
            clipped_buffer
                .row_symbols(clipped_destination.y)
                .is_some_and(|row| row.contains("AbÇ"))
        );

        let click = bmux_tui::geometry::Point::new(7, clipped_destination.y);
        let translated = InteractiveSurfaceState::translate_clipped_event(
            Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                click,
            )),
            initial_area,
            visible_offset,
            clipped_destination,
        );
        assert!(matches!(
            translated,
            Event::Mouse(mouse)
                if mouse.position
                    == bmux_tui::geometry::Point::new(
                        initial_area.x + click.x - clipped_destination.x,
                        initial_area.y + visible_offset,
                    )
        ));

        let resized_area = Rect::new(1, 2, 26, surface.preferred_height(26).min(8));
        let sentinel = bmux_tui::style::Style::new().fg(bmux_tui::style::Color::Red);
        let mut pinned_buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 32, 14));
        pinned_buffer.fill(resized_area, "x", sentinel);
        {
            let mut frame = Frame::new(&mut pinned_buffer);
            frame.fill(resized_area, " ", bmux_tui::prelude::Style::new());
            surface.render(resized_area, &mut frame);
        }
        assert!(area_points(resized_area).all(|point| {
            pinned_buffer
                .get(point)
                .is_some_and(|cell| !(cell.symbol == "x" && cell.style == sentinel))
        }));

        let mut compact = question_surface(serde_json::json!([{
            "header": null,
            "question": "Done?",
            "options": [{"label": "Yes", "value": "yes", "description": null}],
            "control": "radio",
            "selection_mode": "single",
            "custom": false,
            "custom_mode": "additional",
            "required": false
        }]))
        .await;
        let compact_area = Rect::new(1, 2, 20, compact.preferred_height(20).min(6));
        pinned_buffer.fill(resized_area, "x", sentinel);
        {
            let mut frame = Frame::new(&mut pinned_buffer);
            frame.fill(resized_area, " ", bmux_tui::prelude::Style::new());
            compact.render(compact_area, &mut frame);
        }
        assert!(area_points(resized_area).all(|point| {
            pinned_buffer
                .get(point)
                .is_some_and(|cell| !(cell.symbol == "x" && cell.style == sentinel))
        }));
    }

    #[tokio::test]
    async fn pinned_surface_repaints_every_cell_after_resize_and_content_shrink() {
        let mut large = question_surface(serde_json::json!([{
            "header": null,
            "question": "A long question that occupies several rows",
            "options": [
                {"label": "One", "value": "one", "description": "details"},
                {"label": "Two", "value": "two", "description": "details"}
            ],
            "control": "radio",
            "selection_mode": "single",
            "custom": true,
            "custom_mode": "additional",
            "required": true
        }]))
        .await;
        let mut small = question_surface(serde_json::json!([{
            "header": null,
            "question": "Short?",
            "options": [{"label": "Yes", "value": "yes", "description": null}],
            "control": "radio",
            "selection_mode": "single",
            "custom": false,
            "custom_mode": "additional",
            "required": true
        }]))
        .await;
        let area = Rect::new(2, 3, 24, 8);
        let sentinel = bmux_tui::style::Style::new().fg(bmux_tui::style::Color::Red);
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 30, 15));
        buffer.fill(area, "x", sentinel);
        {
            let mut frame = Frame::new(&mut buffer);
            frame.fill(area, " ", bmux_tui::prelude::Style::new());
            large.render(area, &mut frame);
        }
        {
            let mut frame = Frame::new(&mut buffer);
            frame.fill(area, " ", bmux_tui::prelude::Style::new());
            small.render(area, &mut frame);
        }
        assert!(area_points(area).all(|point| {
            buffer
                .get(point)
                .is_some_and(|cell| cell.symbol != "x" && cell.style != sentinel)
        }));
    }

    fn area_points(area: Rect) -> impl Iterator<Item = bmux_tui::geometry::Point> {
        (area.y..area.bottom()).flat_map(move |y| {
            (area.x..area.right()).map(move |x| bmux_tui::geometry::Point::new(x, y))
        })
    }

    #[tokio::test]
    async fn inline_plugin_redraw_channel_is_bounded_and_coalesces() {
        InteractiveSurfaceState::redraw_channel_is_bounded_for_test();
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
    async fn renderer_local_question_edits_are_reported_as_consumed_redraws() {
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
        let area = Rect::new(0, 0, 48, 12);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        surface.render_for_test(area, &mut Frame::new(&mut buffer));

        assert!(matches!(
            surface.handle_event_outcome(&key(KeyCode::Left)),
            InteractiveSurfaceEventOutcome::Consumed
        ));
        assert!(matches!(
            surface.handle_event_outcome(&Event::Key(KeyStroke {
                key: KeyCode::Right,
                modifiers: Modifiers {
                    shift: true,
                    ..Modifiers::NONE
                },
            })),
            InteractiveSurfaceEventOutcome::Consumed
        ));
        assert!(matches!(
            surface.handle_event_outcome(&Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(2, 3),
            ))),
            InteractiveSurfaceEventOutcome::Consumed
        ));
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
