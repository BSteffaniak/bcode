//! Generic inline interactive surface host for tool interactions.

use bcode_plugin::{PluginLoadError, PluginRuntimeHost};
#[cfg(test)]
use bcode_plugin_sdk::tui::PluginTuiHost;
use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiAction, PluginTuiSurfaceOpenRequest, TokioPluginTuiHost,
};
use bcode_session_models::ToolExchangeResolution;
use bmux_tui::event::Event;
#[cfg(test)]
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
#[cfg(test)]
use bmux_tui::paint::LocalRect;
use bmux_tui::paint::PaintCx;
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
    fn action(&self, scope: &str, stroke: bmux_keyboard::KeyStroke) -> Option<String> {
        self.keymap
            .registry()
            .action_for_key(scope, stroke)
            .map(str::to_owned)
    }

    fn is_overridden(&self, scope: &str, stroke: bmux_keyboard::KeyStroke) -> bool {
        self.keymap.registry().is_overridden_key(scope, stroke)
    }

    fn keys(&self, scope: &str, action: &str) -> Vec<String> {
        self.keymap
            .registry()
            .keys_for_action(scope, action)
            .into_iter()
            .map(str::to_owned)
            .collect()
    }
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
    /// The surface consumed the event without requesting a presentation refresh.
    Consumed,
    /// The surface changed renderer-owned presentation without changing its logical extent.
    Redraw,
    /// The surface changed its preferred logical extent and requires layout reconciliation.
    Relayout,
    /// The surface completed the interaction with a canonical resolution.
    Resolved(ToolExchangeResolution),
}

use bcode_plugin_sdk::interaction_application::{
    DURABLE_INTERACTION_VERSION, DurableInteractionInput, DurableInteractionOperation,
    DurableInteractionQuery, DurableInteractionReceipt, DurableInteractionView,
    InteractionApplication as _, InteractionControllerError,
};

/// Canonical evidence lookup. Only explicit Unsupported permits the legacy route.
async fn discover_native_view(
    consumer: &bcode_client::InteractionControllerConsumer,
    session_id: bcode_session_models::SessionId,
    exchange_id: &str,
) -> Result<Option<DurableInteractionView>, InteractionControllerError> {
    let mut query = DurableInteractionQuery {
        limit: 100,
        after_event_sequence: None,
        expected_generation: None,
    };
    loop {
        let page = match consumer
            .durable_controllers(session_id.to_string(), query.clone())
            .await
        {
            Err(InteractionControllerError::Unsupported) if query.expected_generation.is_none() => {
                return Ok(None);
            }
            result => result?,
        };
        page.validate(&session_id.to_string(), &query)?;
        if let Some(view) = page
            .views
            .into_iter()
            .find(|view| view.binding.exchange_id == exchange_id)
        {
            return consumer
                .open_durable_controller(DURABLE_INTERACTION_VERSION, view.binding)
                .await
                .map(Some);
        }
        let Some(after) = page.next_after_event_sequence else {
            return Err(InteractionControllerError::Unavailable);
        };
        query.after_event_sequence = Some(after);
        query.expected_generation = Some(page.generation);
    }
}

type NativeInteractionChanged = Arc<dyn Fn(bool, Option<InteractionControllerError>) + Send + Sync>;

#[derive(Debug)]
struct NativeCommitState {
    view: DurableInteractionView,
    pending: Option<DurableInteractionInput>,
    failure: Option<InteractionControllerError>,
}

impl NativeCommitState {
    fn admit(
        &mut self,
        input: bcode_tool::InteractionInput,
    ) -> Result<DurableInteractionInput, InteractionControllerError> {
        if self.pending.is_some() {
            return Err(InteractionControllerError::Conflict);
        }
        if self.failure.is_some()
            || matches!(
                self.view.snapshot.output,
                bcode_tool::InteractionOutput::Submitted { .. }
                    | bcode_tool::InteractionOutput::Cancelled
            )
        {
            return Err(InteractionControllerError::NotPending);
        }
        let input = DurableInteractionInput {
            version: DURABLE_INTERACTION_VERSION,
            binding: self.view.binding.clone(),
            operation: DurableInteractionOperation {
                operation_id: format!("native-{}", bcode_session_models::SessionId::new()),
                expected_revision: self.view.revision,
            },
            input,
        };
        input.validate()?;
        self.pending = Some(input.clone());
        Ok(input)
    }

    fn acknowledge(
        &mut self,
        receipt: DurableInteractionReceipt,
    ) -> Result<bool, InteractionControllerError> {
        receipt.validate()?;
        if self.pending.as_ref() != Some(&receipt.input)
            || receipt.view.binding != self.view.binding
        {
            return Err(InteractionControllerError::Conflict);
        }
        self.view = receipt.view;
        self.pending = None;
        Ok(matches!(
            self.view.snapshot.output,
            bcode_tool::InteractionOutput::Submitted { .. }
                | bcode_tool::InteractionOutput::Cancelled
        ))
    }
}

/// Attachment-local renderer plus an exact in-flight request; canonical authority stays remote.
struct NativeDurableSurface {
    renderer: BoxedPluginTuiSurface,
    state: Arc<std::sync::Mutex<NativeCommitState>>,
    sender: mpsc::Sender<DurableInteractionInput>,
    task: tokio::task::JoinHandle<()>,
    painted_revision: u64,
}

impl Drop for NativeDurableSurface {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl NativeDurableSurface {
    fn refresh(&mut self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.view.revision != self.painted_revision {
            if self
                .renderer
                .apply_canonical_snapshot(state.view.snapshot.snapshot.clone())
                .is_err()
            {
                state.failure = Some(InteractionControllerError::InvalidContract);
            } else {
                self.painted_revision = state.view.revision;
            }
        }
    }

    fn start(
        renderer: BoxedPluginTuiSurface,
        consumer: bcode_client::InteractionControllerConsumer,
        view: DurableInteractionView,
        changed: NativeInteractionChanged,
    ) -> Self {
        let painted_revision = view.revision;
        let state = Arc::new(std::sync::Mutex::new(NativeCommitState {
            view,
            pending: None,
            failure: None,
        }));
        let (sender, mut receiver) = mpsc::channel::<DurableInteractionInput>(1);
        let committed = state.clone();
        let task = tokio::spawn(async move {
            while let Some(input) = receiver.recv().await {
                loop {
                    match consumer.durable_controller_input(input.clone()).await {
                        Err(InteractionControllerError::Unavailable) => {
                            changed(false, Some(InteractionControllerError::Unavailable));
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                        outcome => {
                            let result = committed.lock().map_or(
                                Err(InteractionControllerError::Unavailable),
                                |mut state| match outcome {
                                    Ok(receipt) => state.acknowledge(receipt),
                                    Err(error) => Err(error),
                                },
                            );
                            if let Err(error) = result
                                && let Ok(mut state) = committed.lock()
                            {
                                state.failure = Some(error);
                            }
                            match result {
                                Ok(terminal) => changed(terminal, None),
                                Err(error) => changed(false, Some(error)),
                            }
                            break;
                        }
                    }
                }
            }
        });
        Self {
            renderer,
            state,
            sender,
            task,
            painted_revision,
        }
    }
}

impl bcode_plugin_sdk::tui::PluginTuiSurface for NativeDurableSurface {
    fn id(&self) -> &'static str {
        self.renderer.id()
    }
    fn title(&self) -> &'static str {
        self.renderer.title()
    }
    fn prepare_keymap(&mut self, host: &dyn bcode_plugin_sdk::tui::PluginTuiHost) {
        self.refresh();
        self.renderer.prepare_keymap(host);
    }
    fn preferred_height(&mut self, width: u16) -> u16 {
        self.refresh();
        self.renderer.preferred_height(width)
    }
    fn focused_row_range(&mut self, width: u16) -> Option<std::ops::Range<u16>> {
        self.refresh();
        self.renderer.focused_row_range(width)
    }
    fn render(&mut self, area: Rect, frame: &mut PaintCx<'_, '_>) {
        self.refresh();
        self.renderer.render(area, frame);
    }
    fn render_slice_with_host(
        &mut self,
        height: u16,
        offset: u16,
        destination: Rect,
        frame: &mut PaintCx<'_, '_>,
        host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) {
        self.refresh();
        self.renderer
            .render_slice_with_host(height, offset, destination, frame, host);
    }
    fn render_with_host(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&bcode_plugin_sdk::tui::PluginTuiTheme>,
        host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) {
        self.refresh();
        self.renderer.render_with_host(area, frame, theme, host);
    }
    fn handle_event_result(
        &mut self,
        event: &Event,
        host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) -> bcode_plugin_sdk::tui::PluginTuiEventResult {
        let action = self.handle_event(event, host);
        if matches!(action, PluginTuiAction::None) {
            bcode_plugin_sdk::tui::PluginTuiEventResult::consumed()
        } else {
            bcode_plugin_sdk::tui::PluginTuiEventResult::from_legacy(action)
        }
    }

    fn handle_event(
        &mut self,
        event: &Event,
        host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) -> PluginTuiAction {
        self.refresh();
        if self.state.lock().map_or(true, |state| {
            state.pending.is_some() || state.failure.is_some()
        }) {
            return PluginTuiAction::None;
        }
        let result =
            super::plugin_surface_host::compose_surface_input(self.renderer.as_mut(), event, host);
        if let Some(input) = self.renderer.take_canonical_input()
            && let Ok(mut state) = self.state.lock()
        {
            match state.admit(input) {
                Ok(request) => {
                    if self.sender.try_send(request).is_err() {
                        state.failure = Some(InteractionControllerError::Unavailable);
                    }
                }
                Err(error) => state.failure = Some(error),
            }
        }
        // Canonical terminal publication is delivered by the receipt path, never a local Close.
        match result.action {
            PluginTuiAction::Redraw => PluginTuiAction::Redraw,
            _ => PluginTuiAction::None,
        }
    }
}

/// Runtime state for one client-rendered interactive tool surface.
pub struct InteractiveSurfaceState {
    interaction_id: String,
    surface: BoxedPluginTuiSurface,
    host: TokioPluginTuiHost,
    pending_resolution: Option<ToolExchangeResolution>,
    retry_resolution: Option<ToolExchangeResolution>,
    last_width: u16,
    last_preferred_height: u16,
    canonical: Option<bcode_session_models::SessionId>,
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
            retry_resolution: None,
            canonical: None,
            last_width: 0,
            last_preferred_height: 0,
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
            retry_resolution: None,
            canonical: None,
            last_width: 0,
            last_preferred_height: 0,
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

    /// Discover canonical evidence before considering the explicitly unsupported legacy route.
    ///
    /// # Errors
    /// Rejects missing/corrupt durable evidence or missing renderer adapters without fallback.
    pub async fn open_canonical_request(
        runtime: &PluginRuntimeHost,
        request: &InteractiveSurfaceRequest,
        keymap: &BmuxKeyMap,
        client: &bcode_client::BcodeClient,
        session_id: bcode_session_models::SessionId,
        changed: NativeInteractionChanged,
    ) -> Result<Self, String> {
        let consumer = client
            .interaction_controller_consumer()
            .await
            .map_err(|error| error.to_string())?;
        let Some(view) = discover_native_view(&consumer, session_id, request.interaction_id())
            .await
            .map_err(|error| format!("canonical interaction discovery: {error:?}"))?
        else {
            return Self::open_request(runtime, request, keymap)
                .await
                .map_err(|error| error.to_string());
        };
        if matches!(
            view.snapshot.output,
            bcode_tool::InteractionOutput::Submitted { .. }
                | bcode_tool::InteractionOutput::Cancelled
        ) {
            return Err("canonical interaction is terminal".to_owned());
        }
        let renderer = crate::plugin_tui::open_canonical_interaction_surface(&view).await?;
        let (redraw_sender, _redraw_receiver) = mpsc::channel(1);
        let host = TokioPluginTuiHost::current(redraw_sender).with_text_input_resolver(Arc::new(
            InteractiveTextInputResolver {
                keymap: keymap.clone(),
            },
        ));
        Ok(Self {
            interaction_id: request.interaction_id.clone(),
            surface: Box::new(NativeDurableSurface::start(
                renderer, consumer, view, changed,
            )),
            host,
            pending_resolution: None,
            retry_resolution: None,
            canonical: Some(session_id),
            last_width: 0,
            last_preferred_height: 0,
        })
    }

    /// Update configured composer-like text input intent without reopening the surface.
    pub fn update_keymap(&mut self, keymap: &BmuxKeyMap) {
        self.host
            .set_text_input_resolver(Arc::new(InteractiveTextInputResolver {
                keymap: keymap.clone(),
            }));
        self.surface.prepare_keymap(&self.host);
    }

    #[cfg(test)]
    pub(crate) fn text_edit_command_for_test(
        &self,
        stroke: bmux_keyboard::KeyStroke,
    ) -> Option<bmux_text_edit::TextEditCommand> {
        bcode_plugin_sdk::tui::PluginTuiHost::text_edit_command(&self.host, stroke)
    }

    /// Whether this attachment uses canonical commit-backed semantics.
    #[must_use]
    pub const fn is_canonical(&self) -> bool {
        self.canonical.is_some()
    }

    /// Original canonical session scope, absent for explicitly unsupported legacy surfaces.
    pub const fn canonical_session(&self) -> Option<bcode_session_models::SessionId> {
        self.canonical
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
        self.surface.prepare_keymap(&self.host);
        self.last_width = width;
        self.last_preferred_height =
            bounded_surface_height(width, self.surface.preferred_height(width));
        self.last_preferred_height
    }

    fn preferred_height_for_event(&mut self) -> u16 {
        self.surface.prepare_keymap(&self.host);
        bounded_surface_height(
            self.last_width,
            self.surface.preferred_height(self.last_width),
        )
    }

    /// Render a bounded logical slice without allocating the full logical surface.
    pub fn render_slice(
        &mut self,
        logical_height: u16,
        logical_row_offset: u16,
        destination: Rect,
        frame: &mut PaintCx<'_, '_>,
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
        self.surface.render_slice_with_host(
            logical_height,
            logical_row_offset,
            destination,
            frame,
            &self.host,
        );
    }

    /// Return focused logical rows for host-owned pinned reveal behavior.
    #[must_use]
    #[allow(dead_code)]
    pub fn focused_row_range(&mut self, width: u16) -> Option<std::ops::Range<u16>> {
        self.surface.prepare_keymap(&self.host);
        self.surface.focused_row_range(width)
    }

    /// Render the interactive surface.
    #[cfg(test)]
    pub fn render(&mut self, area: Rect, frame: &mut PaintCx<'_, '_>) {
        self.surface.render_with_host(area, frame, None, &self.host);
    }

    /// Render a clipped slice of the interactive surface using full-surface coordinates.
    #[cfg(test)]
    pub fn render_clipped(
        &mut self,
        full_area: Rect,
        visible_content_offset: u16,
        destination: Rect,
        frame: &mut PaintCx<'_, '_>,
    ) {
        if full_area.is_empty() || destination.is_empty() {
            return;
        }
        let bounded_height = bounded_surface_height(full_area.width, full_area.height);
        let full_area = Rect::new(full_area.x, full_area.y, full_area.width, bounded_height);
        if visible_content_offset >= bounded_height {
            return;
        }
        frame.with_child(
            i32::from(destination.x) - i32::from(full_area.x),
            i64::from(destination.y) - i64::from(full_area.y) - i64::from(visible_content_offset),
            LocalRect::new(
                i32::from(full_area.x),
                i64::from(full_area.y) + i64::from(visible_content_offset),
                destination.width,
                destination.height,
            ),
            |cx| {
                self.surface
                    .render_with_host(full_area, cx, None, &self.host);
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn render_for_test(&mut self, area: Rect, frame: &mut PaintCx<'_, '_>) {
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
        if let Some(resolution) = self.pending_resolution.take() {
            self.retry_resolution = Some(resolution);
        }
    }

    /// Handle an input event, report consumption, and retain close resolution until confirmation.
    #[cfg(test)]
    pub(crate) fn work_shape_for_test(&self) -> bcode_plugin_sdk::tui::PluginTuiWorkShape {
        self.surface.work_shape()
    }

    /// Return only the opted-in field from an unresolved owner.
    pub fn focused_text_field(&self) -> Option<bcode_plugin_sdk::tui::FocusedTextFieldSnapshot> {
        if self.pending_resolution.is_some() || self.retry_resolution.is_some() {
            return None;
        }
        self.surface.focused_text_field()
    }

    /// Apply through the actual owner so plugin semantics and editor history stay synchronized.
    pub fn apply_focused_text_field_edit(
        &mut self,
        edit: &bcode_plugin_sdk::tui::FocusedTextFieldEdit,
    ) -> (
        bcode_plugin_sdk::tui::FocusedTextFieldEditResult,
        InteractiveSurfaceEventOutcome,
    ) {
        if self.pending_resolution.is_some() || self.retry_resolution.is_some() {
            return (
                bcode_plugin_sdk::tui::FocusedTextFieldEditResult::Rejected(
                    bcode_plugin_sdk::composer::ComposerValidationError::Stale,
                ),
                InteractiveSurfaceEventOutcome::Consumed,
            );
        }
        let result = self
            .surface
            .apply_focused_text_field_edit_result(edit, &self.host);
        (
            result.edit_result,
            self.reconcile_event_result(result.event),
        )
    }

    pub fn handle_event_outcome(&mut self, event: &Event) -> InteractiveSurfaceEventOutcome {
        if let Some(resolution) = &self.pending_resolution {
            return InteractiveSurfaceEventOutcome::Resolved(resolution.clone());
        }
        // The producer may already be closed after proposing completion. Retry the
        // exact retained proposal, never reconstruct an answer or mutate its owner.
        if let Some(resolution) = &self.retry_resolution {
            if self.surface.retry_resolution_event(event, &self.host) {
                let resolution = resolution.clone();
                self.retry_resolution = None;
                self.pending_resolution = Some(resolution.clone());
                return InteractiveSurfaceEventOutcome::Resolved(resolution);
            }
            return InteractiveSurfaceEventOutcome::Consumed;
        }
        let result = super::plugin_surface_host::compose_surface_input(
            self.surface.as_mut(),
            event,
            &self.host,
        );
        self.reconcile_event_result(result)
    }

    fn reconcile_event_result(
        &mut self,
        result: bcode_plugin_sdk::tui::PluginTuiEventResult,
    ) -> InteractiveSurfaceEventOutcome {
        let before_height = self.last_preferred_height;
        let outcome = match result.action {
            PluginTuiAction::None if result.outcome != bmux_tui::event::EventOutcome::Redraw => {
                if result.outcome == bmux_tui::event::EventOutcome::Ignored {
                    InteractiveSurfaceEventOutcome::Ignored
                } else {
                    InteractiveSurfaceEventOutcome::Consumed
                }
            }
            PluginTuiAction::None | PluginTuiAction::Redraw => {
                let after_height = self.preferred_height_for_event();
                self.last_preferred_height = after_height;
                if before_height == 0 || after_height == before_height {
                    InteractiveSurfaceEventOutcome::Redraw
                } else {
                    InteractiveSurfaceEventOutcome::Relayout
                }
            }
            PluginTuiAction::SubscribeWorkflowRuns
            | PluginTuiAction::SelectWorkflowRun { .. }
            | PluginTuiAction::UpdateWorkflowCatalogQuery { .. }
            | PluginTuiAction::LoadMoreWorkflowRuns { .. }
            | PluginTuiAction::InvokePluginCommand { .. }
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
            InteractiveSurfaceEventOutcome::Ignored
            | InteractiveSurfaceEventOutcome::Consumed
            | InteractiveSurfaceEventOutcome::Redraw
            | InteractiveSurfaceEventOutcome::Relayout => None,
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
            session_id: None,
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
mod native_commit_tests {
    use super::*;

    fn view() -> DurableInteractionView {
        serde_json::from_value(json!({
            "version":1,"binding":{"session_id":"session","turn_id":"turn","invocation_id":"call",
                "request_sequence":1,"exchange_id":"exchange","tool_name":"generic","request_digest":"a".repeat(64),
                "exchange_schema_version":1,"capability":{"route":{"producer_id":"producer",
                "exchange_schema":"schema","min_schema_version":1,"max_schema_version":1,"interaction_kind":"kind"},
                "checkpoint_schema":"checkpoint","checkpoint_version":1,"continuation_schema":"continue","continuation_version":1}},
            "revision":4,"snapshot":{"version":1,"exchange_id":"exchange","route":{"producer_id":"producer",
                "exchange_schema":"schema","min_schema_version":1,"max_schema_version":1,"interaction_kind":"kind"},
                "schema_version":1,"revision":4,"snapshot":{"selected":["retained"]},"output":{"type":"none"}}
        })).unwrap()
    }

    #[test]
    fn native_durable_restore_preserves_acknowledged_selection_and_exact_retry() {
        let mut state = NativeCommitState {
            view: view(),
            pending: None,
            failure: None,
        };
        assert_eq!(
            state.view.snapshot.snapshot["selected"],
            json!(["retained"])
        );
        let request = state.admit(bcode_tool::InteractionInput::Submit).unwrap();
        let retry = state.pending.clone().unwrap();
        assert_eq!(retry, request);
        assert_eq!(request.operation.expected_revision, 4);
        assert_eq!(
            state.admit(bcode_tool::InteractionInput::Submit),
            Err(InteractionControllerError::Conflict)
        );
        assert_eq!(
            state.view.snapshot.snapshot["selected"],
            json!(["retained"])
        );
        let mut committed = state.view.clone();
        committed.revision = 5;
        committed.snapshot.revision = 5;
        committed.snapshot.snapshot = json!({"selected":["acknowledged"]});
        let receipt = DurableInteractionReceipt {
            version: 1,
            input: retry,
            view: committed.clone(),
        };
        assert_eq!(state.acknowledge(receipt), Ok(false));
        assert!(state.pending.is_none());
        let rejoined = NativeCommitState {
            view: committed,
            pending: None,
            failure: None,
        };
        assert_eq!(
            rejoined.view.snapshot.snapshot["selected"],
            json!(["acknowledged"])
        );
    }

    #[test]
    fn native_durable_receipt_scope_failure_retains_exact_pending_input() {
        let mut state = NativeCommitState {
            view: view(),
            pending: None,
            failure: None,
        };
        let input = state.admit(bcode_tool::InteractionInput::Submit).unwrap();
        let mut foreign = state.view.clone();
        foreign.binding.session_id = "foreign".into();
        foreign.revision = 5;
        foreign.snapshot.revision = 5;
        let receipt = DurableInteractionReceipt {
            version: 1,
            input: input.clone(),
            view: foreign,
        };
        assert!(state.acknowledge(receipt).is_err());
        assert_eq!(state.pending, Some(input));
        assert_eq!(state.view.binding.session_id, "session");
        assert_eq!(state.view.revision, 4);
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
        let mut keymap = BmuxKeyMap::from_config(config);
        keymap.integrate_contributors(Ok(bcode_question_plugin::question_tui_registry()
            .keymap_actions()
            .to_vec()));
        assert!(keymap.error().is_none(), "{:?}", keymap.error());
        InteractiveSurfaceState::open(
            &runtime,
            "question-call-question",
            "bcode.question.inline",
            &serde_json::json!({ "questions": questions }).to_string(),
            &keymap,
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
        let mut backend = Frame::new(&mut buffer);
        let mut frame = PaintCx::new(&mut backend);
        if underpaint {
            frame.fill(
                LocalRect::terminal(area),
                " ",
                bmux_tui::prelude::Style::new(),
            );
        }
        surface.render(area, &mut frame);
        let cursor = frame.cursor();
        let text = (0..area.height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n");
        (text, cursor)
    }

    struct ConsumedSurface;

    impl bcode_plugin_sdk::tui::PluginTuiSurface for ConsumedSurface {
        fn id(&self) -> &'static str {
            "consumed"
        }
        fn title(&self) -> &'static str {
            "Consumed"
        }
        fn render(&mut self, _area: Rect, _frame: &mut PaintCx<'_, '_>) {}
        fn handle_event(
            &mut self,
            _event: &Event,
            _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
        ) -> PluginTuiAction {
            PluginTuiAction::None
        }
        fn handle_event_result(
            &mut self,
            _event: &Event,
            _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
        ) -> bcode_plugin_sdk::tui::PluginTuiEventResult {
            bcode_plugin_sdk::tui::PluginTuiEventResult {
                outcome: bmux_tui::event::EventOutcome::Handled,
                action: PluginTuiAction::None,
            }
        }
    }

    #[tokio::test]
    async fn explicit_consumption_without_effect_survives_host_routing() {
        let mut surface = InteractiveSurfaceState::from_surface_for_test(
            "consumed",
            Box::new(ConsumedSurface),
            &BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
        );
        assert_eq!(
            surface.handle_event_outcome(&Event::Tick),
            InteractiveSurfaceEventOutcome::Consumed
        );
    }

    struct RoutedSurface {
        editor_outcome: bmux_tui::event::EventOutcome,
        calls: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    impl bcode_plugin_sdk::tui::PluginTuiSurface for RoutedSurface {
        fn id(&self) -> &'static str {
            "routing"
        }
        fn title(&self) -> &'static str {
            "Routing"
        }
        fn render(&mut self, _area: Rect, _frame: &mut PaintCx<'_, '_>) {}
        fn focused_text_field_event(
            &mut self,
            _event: &Event,
            _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
        ) -> bcode_plugin_sdk::tui::PluginTuiEventResult {
            self.calls.lock().unwrap().push("editor");
            bcode_plugin_sdk::tui::PluginTuiEventResult {
                outcome: self.editor_outcome,
                action: PluginTuiAction::None,
            }
        }
        fn handle_event(
            &mut self,
            _event: &Event,
            _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
        ) -> PluginTuiAction {
            self.calls.lock().unwrap().push("fallback");
            PluginTuiAction::None
        }
    }

    #[tokio::test]
    async fn only_ignored_editor_events_reach_surface_fallback_once() {
        use bmux_tui::event::EventOutcome;
        for event in [
            key(KeyCode::Enter),
            key(KeyCode::Tab),
            key(KeyCode::Escape),
            key(KeyCode::Char('x')),
            Event::Paste("text".into()),
            Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(1, 1),
            )),
            Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::ScrollDown,
                bmux_tui::geometry::Point::new(1, 1),
            )),
            Event::Tick,
        ] {
            for (editor_outcome, expected_outcome, expected_calls) in [
                (
                    EventOutcome::Ignored,
                    InteractiveSurfaceEventOutcome::Ignored,
                    vec!["editor", "fallback"],
                ),
                (
                    EventOutcome::Handled,
                    InteractiveSurfaceEventOutcome::Consumed,
                    vec!["editor"],
                ),
                (
                    EventOutcome::Redraw,
                    InteractiveSurfaceEventOutcome::Redraw,
                    vec!["editor"],
                ),
            ] {
                let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let mut surface = InteractiveSurfaceState::from_surface_for_test(
                    "routing",
                    Box::new(RoutedSurface {
                        editor_outcome,
                        calls: std::sync::Arc::clone(&calls),
                    }),
                    &BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
                );
                assert_eq!(surface.handle_event_outcome(&event), expected_outcome);
                assert_eq!(*calls.lock().unwrap(), expected_calls);
            }
        }
    }

    #[derive(Default)]
    struct GrowingSurface {
        height: u16,
    }

    impl bcode_plugin_sdk::tui::PluginTuiSurface for GrowingSurface {
        fn id(&self) -> &'static str {
            "growing"
        }

        fn title(&self) -> &'static str {
            "Growing"
        }

        fn preferred_height(&mut self, _width: u16) -> u16 {
            self.height.max(1)
        }

        fn render(&mut self, _area: Rect, _frame: &mut PaintCx<'_, '_>) {}

        fn handle_event(
            &mut self,
            _event: &Event,
            _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
        ) -> PluginTuiAction {
            self.height = self.height.saturating_add(1);
            PluginTuiAction::Redraw
        }
    }

    #[tokio::test]
    async fn event_outcome_redraw_without_plugin_effect_preserves_relayout() {
        let mut surface = InteractiveSurfaceState::from_surface_for_test(
            "growing",
            Box::new(GrowingSurface { height: 1 }),
            &BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
        );
        assert_eq!(surface.preferred_height(80), 1);
        assert_eq!(
            surface.reconcile_event_result(bcode_plugin_sdk::tui::PluginTuiEventResult {
                outcome: bmux_tui::event::EventOutcome::Redraw,
                action: PluginTuiAction::None,
            }),
            InteractiveSurfaceEventOutcome::Redraw,
        );
        // Mutate through the owner, then reconcile an independent redraw request.
        let _ = surface.surface.handle_event(&Event::Tick, &surface.host);
        assert_eq!(
            surface.reconcile_event_result(bcode_plugin_sdk::tui::PluginTuiEventResult {
                outcome: bmux_tui::event::EventOutcome::Redraw,
                action: PluginTuiAction::None,
            }),
            InteractiveSurfaceEventOutcome::Relayout,
        );
    }

    #[tokio::test]
    async fn height_changes_report_relayout_after_committed_measurement() {
        let mut surface = InteractiveSurfaceState::from_surface_for_test(
            "growing",
            Box::new(GrowingSurface { height: 1 }),
            &BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
        );
        assert_eq!(surface.preferred_height(80), 1);
        assert_eq!(
            surface.handle_event_outcome(&Event::Tick),
            InteractiveSurfaceEventOutcome::Relayout
        );
        assert_eq!(surface.preferred_height(80), 2);
        assert_eq!(
            surface.handle_event_outcome(&Event::Tick),
            InteractiveSurfaceEventOutcome::Relayout
        );
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
        let mut backend = Frame::new(&mut buffer);
        let mut frame = PaintCx::new(&mut backend);
        frame.fill(
            LocalRect::terminal(area),
            ".",
            bmux_tui::prelude::Style::new(),
        );
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
        surface.render(
            full_area,
            &mut bmux_tui::paint::PaintCx::new(&mut full_frame),
        );
        let cursor = full_frame.cursor().expect("focused custom cursor");

        for (offset, height, visible) in [
            (0, cursor.position.y.saturating_add(1), true),
            (cursor.position.y, 1, true),
            (cursor.position.y.saturating_add(1), 1, false),
        ] {
            let destination = Rect::new(3, 4, full_area.width, height.max(1));
            let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 40, 20));
            let mut backend = Frame::new(&mut buffer);
            let mut frame = PaintCx::new(&mut backend);
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
        surface.render_clipped(
            full_area,
            1,
            destination,
            &mut bmux_tui::paint::PaintCx::new(&mut Frame::new(&mut buffer)),
        );
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
            &mut bmux_tui::paint::PaintCx::new(&mut clipped_frame),
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
            PaintCx::new(&mut frame).fill(
                LocalRect::terminal(resized_area),
                " ",
                bmux_tui::prelude::Style::new(),
            );
            surface.render(resized_area, &mut PaintCx::new(&mut frame));
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
            PaintCx::new(&mut frame).fill(
                LocalRect::terminal(resized_area),
                " ",
                bmux_tui::prelude::Style::new(),
            );
            compact.render(compact_area, &mut PaintCx::new(&mut frame));
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
            let mut backend = Frame::new(&mut buffer);
            let mut frame = PaintCx::new(&mut backend);
            frame.fill(
                LocalRect::terminal(area),
                " ",
                bmux_tui::prelude::Style::new(),
            );
            large.render(area, &mut frame);
        }
        {
            let mut backend = Frame::new(&mut buffer);
            let mut frame = PaintCx::new(&mut backend);
            frame.fill(
                LocalRect::terminal(area),
                " ",
                bmux_tui::prelude::Style::new(),
            );
            small.render(area, &mut frame);
        }
        assert!(area_points(area).all(|point| {
            buffer
                .get(point)
                .is_some_and(|cell| !(cell.symbol == "x" && cell.style == sentinel))
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
    async fn renderer_local_edits_are_reported_as_redraws() {
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
        surface.render_for_test(
            area,
            &mut bmux_tui::paint::PaintCx::new(&mut Frame::new(&mut buffer)),
        );

        assert!(matches!(
            surface.handle_event_outcome(&key(KeyCode::Left)),
            InteractiveSurfaceEventOutcome::Redraw
        ));
        assert!(matches!(
            surface.handle_event_outcome(&Event::Key(KeyStroke {
                key: KeyCode::Right,
                modifiers: Modifiers {
                    shift: true,
                    ..Modifiers::NONE
                },
            })),
            InteractiveSurfaceEventOutcome::Redraw
        ));
        assert!(matches!(
            surface.handle_event_outcome(&Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(2, 3),
            ))),
            InteractiveSurfaceEventOutcome::Redraw
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
            .scopes
            .entry("question.text".into())
            .or_default()
            .insert("ctrl+b".to_owned(), "tui.control.left".to_owned());
        config
            .keybindings
            .scopes
            .entry("question.text".into())
            .or_default()
            .insert("ctrl+d".to_owned(), "tui.control.delete".to_owned());
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
        surface.render_for_test(
            area,
            &mut bmux_tui::paint::PaintCx::new(&mut Frame::new(&mut buffer)),
        );

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
        let mut keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
        keymap.integrate_contributors(Ok(bcode_question_plugin::question_tui_registry()
            .keymap_actions()
            .to_vec()));
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
            &keymap,
        )
        .await
        .expect("question surface");
        drop(plugin_runtime);

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
        surface.clear_pending_resolution();
        assert!(surface.focused_text_field().is_none());
        assert!(surface.handle_event(&Event::Tick).is_none());
        assert!(surface.handle_event(&key(KeyCode::Escape)).is_none());
        let mut remapped_config = bcode_config::TuiConfig::default();
        remapped_config.keybindings.scopes.insert(
            "question".into(),
            std::collections::BTreeMap::from([
                ("enter".into(), String::new()),
                ("ctrl+s".into(), "question.submit".into()),
            ]),
        );
        let mut remapped = BmuxKeyMap::from_config(&remapped_config);
        remapped.integrate_contributors(Ok(bcode_question_plugin::question_tui_registry()
            .keymap_actions()
            .to_vec()));
        surface.update_keymap(&remapped);
        assert!(surface.handle_event(&key(KeyCode::Enter)).is_none());
        assert_eq!(
            surface.handle_event(&Event::Key(
                bmux_keyboard::parse_key_stroke("ctrl+s").unwrap()
            )),
            Some(submitted.clone())
        );
        surface.clear_pending_resolution();
        remapped_config.keybindings.scopes.insert(
            "question".into(),
            std::collections::BTreeMap::from([
                ("enter".into(), String::new()),
                ("ctrl+s".into(), String::new()),
            ]),
        );
        let mut disabled = BmuxKeyMap::from_config(&remapped_config);
        disabled.integrate_contributors(Ok(bcode_question_plugin::question_tui_registry()
            .keymap_actions()
            .to_vec()));
        surface.update_keymap(&disabled);
        assert!(surface.handle_event(&key(KeyCode::Enter)).is_none());
        assert!(
            surface
                .handle_event(&Event::Key(
                    bmux_keyboard::parse_key_stroke("ctrl+s").unwrap()
                ))
                .is_none()
        );
        surface.update_keymap(&keymap);
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
        let mut keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
        keymap.integrate_contributors(Ok(bcode_question_plugin::question_tui_registry()
            .keymap_actions()
            .to_vec()));
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
            &keymap,
        )
        .await
        .expect("open local question TUI surface");
        drop(runtime);

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
