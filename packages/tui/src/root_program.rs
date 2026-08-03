//! Bcode-owned root runtime message and model contracts.
//!
//! These types establish the application boundary before orchestration migrates from the existing
//! chat loop. BMUX treats messages and model state as opaque application data.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use bmux_tui::event::Event;

use super::TuiError;
use super::artifact_stream::ActiveArtifactFetchCompletion;
use super::chat_loop::{ChatLoopState, DraftAutosave, TuiRuntimeSettings};
use super::effects::TuiEffectResult;
use super::history_flow;
use super::invalidation::InvalidationKey;
use super::markdown_projection_coordinator::MarkdownProjectionCompletion;
use super::session_flow::ActiveChat;

/// Typed Bcode event admitted to the root TUI runtime.
#[allow(dead_code)]
pub enum BcodeRuntimeMessage {
    /// Install root-runtime subscriptions after all application state is owned by the program.
    Bootstrap {
        /// Runtime control handle retained for live cadence reconfiguration.
        handle: bmux_tui_runtime::RuntimeHandle<Self>,
    },
    /// Terminal input backend failure surfaced through the serialized root update path.
    TerminalInputFailed(std::io::Error),
    /// Reliable terminal input after BMUX decoding and admission classification.
    Terminal(Event),
    /// Ordered canonical/session-view stream update.
    SessionStream(Box<history_flow::SessionStreamUpdate>),
    /// Completed artifact fetch for the active presentation generation.
    ArtifactFetchCompleted(Box<ActiveArtifactFetchCompletion>),
    /// Latest Markdown projection completion.
    MarkdownProjectionCompleted(Box<Option<MarkdownProjectionCompletion>>),
    /// Completed Bcode-owned background effect.
    EffectCompleted(Box<TuiEffectResult>),
    /// Completed attempt to open the next inline interactive surface.
    InteractiveSurfaceOpened(Result<super::interactive_surface::InteractiveSurfaceState, String>),
    /// Completed inline interactive surface resolution request.
    InteractiveSurfaceResolved(Result<bool, bcode_client::ClientError>),
    /// Due Bcode-owned semantic invalidations.
    Invalidations(Vec<InvalidationKey>),
    /// Draft autosave deadline.
    DraftSaveDue,
    /// Interactive-surface retry deadline.
    InteractionRetryDue,
    /// Streaming-presentation interpolation deadline.
    StreamingPresentationDue,
    /// Client telemetry flush deadline.
    TelemetryFlushDue,
}

impl BcodeRuntimeMessage {
    #[allow(dead_code)]
    fn latest_key(&self) -> Option<bmux_tui_runtime::MessageKey> {
        match self {
            Self::MarkdownProjectionCompleted(_) => Some(bmux_tui_runtime::MessageKey::new(
                "bcode.markdown_projection",
            )),
            Self::StreamingPresentationDue => Some(bmux_tui_runtime::MessageKey::new(
                "bcode.streaming_presentation",
            )),
            Self::TelemetryFlushDue => {
                Some(bmux_tui_runtime::MessageKey::new("bcode.telemetry_flush"))
            }
            Self::Bootstrap { .. }
            | Self::TerminalInputFailed(_)
            | Self::Terminal(_)
            | Self::SessionStream(_)
            | Self::ArtifactFetchCompleted(_)
            | Self::EffectCompleted(_)
            | Self::InteractiveSurfaceOpened(_)
            | Self::InteractiveSurfaceResolved(_)
            | Self::Invalidations(_)
            | Self::DraftSaveDue
            | Self::InteractionRetryDue => None,
        }
    }
}

/// Admit one Bcode message using its domain-owned reliability classification.
///
/// # Errors
///
/// Returns an admission error when the runtime is closed or keyed latest-value capacity is full.
#[allow(dead_code)]
pub async fn admit(
    handle: &bmux_tui_runtime::RuntimeHandle<BcodeRuntimeMessage>,
    message: BcodeRuntimeMessage,
) -> Result<(), BcodeRuntimeAdmissionError> {
    if let Some(key) = message.latest_key() {
        handle
            .send_latest(key, message)
            .map(|_| ())
            .map_err(|error| match error {
                bmux_tui_runtime::LatestSendError::Full(_) => BcodeRuntimeAdmissionError::Full,
                bmux_tui_runtime::LatestSendError::Closed(_) => BcodeRuntimeAdmissionError::Closed,
            })
    } else {
        handle
            .send(message)
            .await
            .map_err(|_| BcodeRuntimeAdmissionError::Closed)
    }
}

/// Normalized Bcode root-runtime admission failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum BcodeRuntimeAdmissionError {
    /// Distinct-key latest-value capacity is exhausted.
    Full,
    /// Runtime admission has closed.
    Closed,
}

#[derive(Default)]
struct OrderedPresentationQueue {
    pending: std::collections::BTreeMap<
        bcode_session_models::SessionId,
        VecDeque<super::effects::TuiEffect>,
    >,
    active: std::collections::BTreeSet<bcode_session_models::SessionId>,
}

impl OrderedPresentationQueue {
    fn append(
        &mut self,
        notes: std::collections::BTreeMap<
            bcode_session_models::SessionId,
            VecDeque<super::effects::TuiEffect>,
        >,
    ) {
        for (session_id, mut pending) in notes {
            self.pending
                .entry(session_id)
                .or_default()
                .append(&mut pending);
        }
    }

    fn complete(&mut self, session_id: bcode_session_models::SessionId) {
        self.active.remove(&session_id);
    }

    fn take_ready(&mut self) -> Vec<super::effects::TuiEffect> {
        let ready = self
            .pending
            .keys()
            .filter(|session_id| !self.active.contains(session_id))
            .copied()
            .collect::<Vec<_>>();
        let mut effects = Vec::with_capacity(ready.len());
        for session_id in ready {
            if let Some(effect) = self
                .pending
                .get_mut(&session_id)
                .and_then(VecDeque::pop_front)
            {
                self.active.insert(session_id);
                effects.push(effect);
            }
            if self
                .pending
                .get(&session_id)
                .is_some_and(VecDeque::is_empty)
            {
                self.pending.remove(&session_id);
            }
        }
        effects
    }
}

///
/// Presentation caches and nested screen state remain Bcode-owned. This model deliberately does
/// not expose Bcode types through BMUX contracts.
#[allow(dead_code)]
pub struct BcodeRuntimeModel {
    /// Canonical application/session owner used by the existing chat path.
    pub chat: ActiveChat,
    /// Bcode-specific effect, plugin-surface, projection, artifact, image, and cache state.
    pub loop_state: ChatLoopState,
    /// Reloadable keymap, cadence, plugin, and launch settings.
    pub settings: TuiRuntimeSettings,
    /// Draft autosave generation and deadline state.
    pub draft_autosave: DraftAutosave,
    /// Current top-level navigation/screen state.
    pub screen: BcodeRuntimeScreen,
    /// Runtime control handle used for live cadence replacement.
    pub runtime_handle: Option<bmux_tui_runtime::RuntimeHandle<BcodeRuntimeMessage>>,
    /// Deferred application messages blocked by an explicit paint or navigation barrier.
    pub deferred: VecDeque<BcodeRuntimeMessage>,
    /// Bcode-owned per-session ordered presentation-note scheduler.
    ordered_notes: OrderedPresentationQueue,
    /// Current merged Bcode semantic presentation damage.
    pub invalidation: super::invalidation::UiInvalidation,
    /// Last successfully committed terminal hit map used for pointer routing.
    pub committed_hits: bmux_tui::hit::HitMap,
    /// Last successfully committed terminal frame area.
    pub committed_area: bmux_tui::geometry::Rect,
    /// Last successfully committed presentation timestamp.
    pub last_presented_at: Option<Instant>,
    /// Whether the root program should terminate after its dirty state is committed.
    pub exit_requested: bool,
}

enum RootTimer {
    Invalidations,
    ArtifactRetry,
    StreamingPresentation,
    DraftSave,
    InteractiveSurfaceRetry,
    TelemetryFlush,
}

impl RootTimer {
    fn id(self) -> bmux_tui_runtime::TimerId {
        bmux_tui_runtime::TimerId::new(match self {
            Self::Invalidations => "bcode.invalidations",
            Self::ArtifactRetry => "bcode.artifact_retry",
            Self::StreamingPresentation => "bcode.streaming_presentation",
            Self::DraftSave => "bcode.draft_save",
            Self::InteractiveSurfaceRetry => "bcode.interactive_surface_retry",
            Self::TelemetryFlush => "bcode.telemetry_flush",
        })
    }
}

impl BcodeRuntimeModel {
    #[allow(dead_code)]
    pub fn new(chat: ActiveChat, settings: TuiRuntimeSettings, loop_state: ChatLoopState) -> Self {
        let draft_autosave = DraftAutosave::new(
            settings.launch_working_directory().to_path_buf(),
            chat.app.composer().text().to_owned(),
        );
        Self {
            chat,
            loop_state,
            settings,
            draft_autosave,
            screen: BcodeRuntimeScreen::Chat,
            runtime_handle: None,
            deferred: VecDeque::new(),
            ordered_notes: OrderedPresentationQueue::default(),
            invalidation: super::invalidation::UiInvalidation::Full,
            committed_hits: bmux_tui::hit::HitMap::default(),
            committed_area: bmux_tui::geometry::Rect::new(0, 0, 0, 0),
            last_presented_at: None,
            exit_requested: false,
        }
    }

    fn handle_permission_key(
        &mut self,
        stroke: bmux_keyboard::KeyStroke,
    ) -> super::invalidation::UiInvalidation {
        let Some(action) = self
            .settings
            .keymap()
            .action_for_key(super::keymap::BmuxScope::Permission, stroke)
        else {
            return super::invalidation::UiInvalidation::None;
        };
        let Some(dialog) = self.loop_state.permission_dialog.as_mut() else {
            return super::invalidation::UiInvalidation::None;
        };
        match action {
            super::keymap::BmuxAction::SelectUp => {
                dialog.focus_previous();
                self.chat
                    .app
                    .set_status(format!("permission choice: {}", dialog.focused_label()));
            }
            super::keymap::BmuxAction::SelectDown => {
                dialog.focus_next();
                self.chat
                    .app
                    .set_status(format!("permission choice: {}", dialog.focused_label()));
            }
            super::keymap::BmuxAction::PermissionApprove => {
                self.queue_permission_resolution(true, false, false);
            }
            super::keymap::BmuxAction::PermissionDeny | super::keymap::BmuxAction::SelectCancel => {
                self.queue_permission_resolution(false, false, false);
            }
            super::keymap::BmuxAction::SelectConfirm => {
                self.queue_focused_permission_resolution();
            }
            _ => return super::invalidation::UiInvalidation::None,
        }
        super::invalidation::UiInvalidation::Structural
    }

    fn queue_focused_permission_resolution(&mut self) {
        let Some(dialog) = self.loop_state.permission_dialog.as_ref() else {
            return;
        };
        self.queue_permission_resolution(
            dialog.focused_approval(),
            dialog.focused_remember(),
            dialog.focused_batch(),
        );
    }

    fn queue_permission_resolution(
        &mut self,
        approved: bool,
        remember: bool,
        apply_to_batch: bool,
    ) {
        let Some(dialog) = self.loop_state.permission_dialog.as_ref() else {
            return;
        };
        let permission_id = dialog.permission().permission_id.clone();
        let batch_id = dialog
            .permission()
            .batch
            .as_ref()
            .map(|batch| batch.batch_id.clone());
        let label = dialog.focused_label();
        self.chat
            .start_effect(super::effects::TuiEffect::ResolvePermission {
                permission_id,
                approved,
                remember,
                apply_to_batch,
                batch_id,
            });
        self.loop_state.permission_dialog = None;
        self.chat
            .app
            .set_status(format!("resolving permission: {label}"));
    }

    #[allow(clippy::let_and_return, clippy::too_many_lines)]
    fn handle_basic_terminal_event(&mut self, event: Event) -> super::invalidation::UiInvalidation {
        let damage = match event {
            Event::Resize(_) => super::invalidation::UiInvalidation::Full,
            Event::Focus(_) | Event::Tick => super::invalidation::UiInvalidation::Paint,
            Event::Paste(text) => {
                self.chat.app.reset_input_history_navigation();
                self.chat.app.paste_composer_text(&text);
                self.chat.app.wake_cursor();
                self.loop_state.refresh_slash_palette(&mut self.chat);
                self.draft_autosave.observe(&self.chat, Instant::now());
                super::invalidation::UiInvalidation::Structural
            }
            Event::Key(stroke) => {
                if self.loop_state.permission_dialog.is_some() {
                    return self.handle_permission_key(stroke);
                }
                if self.loop_state.has_command_palette() {
                    if let Some(action) = self.loop_state.handle_command_palette_key(stroke) {
                        self.apply_root_command_action(action);
                    }
                    return super::invalidation::UiInvalidation::Structural;
                }
                if self.loop_state.has_slash_palette() {
                    match self
                        .loop_state
                        .handle_slash_palette_key(&mut self.chat, stroke)
                    {
                        super::chat_loop::SlashPaletteRootOutcome::Handled => {
                            return super::invalidation::UiInvalidation::Structural;
                        }
                        super::chat_loop::SlashPaletteRootOutcome::Submit => {
                            let launch_working_directory =
                                self.settings.launch_working_directory().to_path_buf();
                            let _staged = super::composer_flow::stage_session_message(
                                &launch_working_directory,
                                &mut self.chat,
                                bcode_ipc::PromptPlacement::Steering,
                            );
                            return super::invalidation::UiInvalidation::Structural;
                        }
                        super::chat_loop::SlashPaletteRootOutcome::Unhandled => {}
                    }
                }
                if self
                    .settings
                    .keymap()
                    .action_for_key(super::keymap::BmuxScope::Chat, stroke)
                    == Some(super::keymap::BmuxAction::CommandPaletteOpen)
                {
                    self.loop_state.open_command_palette(&mut self.chat);
                    return super::invalidation::UiInvalidation::Structural;
                }
                let outcome =
                    super::input::handle_key(&mut self.chat.app, self.settings.keymap(), stroke);
                match outcome.request {
                    super::input::KeyRequest::None => {}
                    super::input::KeyRequest::Interrupt => {
                        super::chat_loop::start_cancel_turn(&mut self.chat, &mut self.loop_state);
                    }
                    super::input::KeyRequest::CycleAgent => {
                        super::chat_loop::cycle_session_agent(&mut self.chat);
                    }
                    super::input::KeyRequest::CycleThinkingEffort => {
                        super::thinking_flow::cycle_thinking_effort(&mut self.chat);
                    }
                    super::input::KeyRequest::Submit { placement } => {
                        let launch_working_directory =
                            self.settings.launch_working_directory().to_path_buf();
                        if super::composer_flow::stage_session_message(
                            &launch_working_directory,
                            &mut self.chat,
                            placement,
                        ) {
                            self.draft_autosave.mark_dirty_now();
                        }
                    }
                }
                self.loop_state.refresh_slash_palette(&mut self.chat);
                self.draft_autosave.observe(&self.chat, Instant::now());
                if outcome.redraw {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            Event::Mouse(mouse) => {
                if self.loop_state.has_command_palette() {
                    if let Some(action) = self
                        .loop_state
                        .handle_command_palette_mouse(mouse, self.committed_area)
                    {
                        self.apply_root_command_action(action);
                    }
                    return super::invalidation::UiInvalidation::Structural;
                }
                if self.loop_state.has_slash_palette() {
                    let _handled = self.loop_state.handle_slash_palette_mouse(
                        &mut self.chat,
                        mouse,
                        self.committed_area,
                    );
                    return super::invalidation::UiInvalidation::Structural;
                }
                let hit_id = super::mouse_flow::mouse_hit_id(&self.committed_hits, mouse);
                let changed = if self.loop_state.permission_dialog.is_some() {
                    super::mouse_flow::handle_permission_action_mouse(
                        hit_id.as_deref(),
                        &mut self.chat,
                        &mut self.loop_state.permission_dialog,
                        mouse,
                    )
                } else {
                    super::mouse_flow::handle_non_permission_mouse(
                        hit_id.as_deref(),
                        &mut self.chat,
                        mouse,
                        self.settings.mouse_scroll_rows(),
                    )
                };
                if changed {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            Event::User(_) => {
                self.deferred
                    .push_back(BcodeRuntimeMessage::Terminal(event));
                super::invalidation::UiInvalidation::None
            }
        };
        damage
    }

    fn apply_root_command_action(&mut self, action: bcode_command::CommandAction) {
        match action {
            bcode_command::CommandAction::Host { route } => match route.as_str() {
                "session.new" => {
                    super::session_flow::switch_to_draft_session(&mut self.chat);
                    self.chat
                        .replace_effect(super::effects::TuiEffect::LoadDraftStatus {
                            launch_working_directory: self
                                .settings
                                .launch_working_directory()
                                .to_path_buf(),
                        });
                }
                "turn.cancel" => {
                    super::chat_loop::start_cancel_turn(&mut self.chat, &mut self.loop_state);
                }
                "context.compact" => {
                    if let Some(session_id) = self.chat.session_id {
                        self.chat
                            .start_effect(super::effects::TuiEffect::CompactContext { session_id });
                        self.chat.app.set_status("compacting context…".to_owned());
                    } else {
                        self.chat.app.set_status("No active session".to_owned());
                    }
                }
                "help" => {
                    self.chat.push_presentation_note(
                        "bcode.host",
                        "# TUI help\n\n* Use the command palette for sessions, cancellation, and context compaction."
                            .to_owned(),
                        bcode_command::CommandTextFormat::Markdown,
                    );
                    self.chat.app.set_status("help shown".to_owned());
                }
                route => self.chat.app.set_status(format!(
                    "command route pending root screen migration: {route}"
                )),
            },
            bcode_command::CommandAction::Plugin {
                plugin_id,
                command_id,
            } => {
                let working_directory = self
                    .chat
                    .app
                    .working_directory()
                    .unwrap_or_else(|| self.settings.launch_working_directory())
                    .to_path_buf();
                self.chat
                    .start_effect(super::effects::TuiEffect::InvokePluginCommand {
                        plugin_id,
                        command_id,
                        arguments: None,
                        working_directory,
                        session_id: self.chat.session_id,
                    });
                self.chat
                    .app
                    .set_status("running plugin command…".to_owned());
            }
        }
    }

    fn root_interactive_surface_host_key(
        &self,
        stroke: bmux_keyboard::KeyStroke,
    ) -> Option<super::keymap::BmuxAction> {
        let action = self
            .settings
            .keymap()
            .action_for_key(super::keymap::BmuxScope::Chat, stroke)?;
        (matches!(
            action,
            super::keymap::BmuxAction::AppExit | super::keymap::BmuxAction::AppInterrupt
        ) || matches!(
            action,
            super::keymap::BmuxAction::TranscriptPageUp
                | super::keymap::BmuxAction::TranscriptPageDown
                | super::keymap::BmuxAction::TranscriptTop
                | super::keymap::BmuxAction::TranscriptBottom
                | super::keymap::BmuxAction::TranscriptLineUp
                | super::keymap::BmuxAction::TranscriptLineDown
        ))
        .then_some(action)
    }

    fn interactive_surface_resolution_command(
        &self,
        interaction_id: String,
        resolution: bcode_session_models::ToolExchangeResolution,
    ) -> bmux_tui_runtime::Command<BcodeRuntimeMessage> {
        let client = self.loop_state.foreground_client();
        bmux_tui_runtime::Command::start_if_idle(
            bmux_tui_runtime::CommandKey::new("bcode.interactive_surface_resolution"),
            async move {
                let result = bcode_session_view::execute_session_view_action(
                    &client,
                    bcode_session_view_models::SessionViewAction::ResolveExchange {
                        interaction_id,
                        resolution,
                    },
                )
                .await
                .and_then(|outcome| match outcome {
                    bcode_session_view_models::SessionViewActionOutcome::InteractionResolved {
                        resolved,
                    } => Ok(resolved),
                    _ => Err(bcode_client::ClientError::UnexpectedResponse),
                });
                Some(BcodeRuntimeMessage::InteractiveSurfaceResolved(result))
            },
        )
    }

    fn handle_session_changed(&mut self) {
        self.loop_state.session_changed(self.chat.session_id);
        self.draft_autosave.reset_for_session_change();
    }

    fn collect_runtime_work(
        &mut self,
        handle: &bmux_tui_runtime::RuntimeHandle<BcodeRuntimeMessage>,
    ) -> Vec<bmux_tui_runtime::Command<BcodeRuntimeMessage>> {
        let (mut commands, notes) = self.loop_state.take_pending_effects(&mut self.chat, handle);
        self.ordered_notes.append(notes);
        for effect in self.ordered_notes.take_ready() {
            commands.push(self.loop_state.ordered_effect_command(effect, handle));
        }
        if let Some(request) = self.loop_state.next_surface_open_request() {
            commands.push(bmux_tui_runtime::Command::start_if_idle(
                bmux_tui_runtime::CommandKey::new("bcode.interactive_surface_open"),
                async move {
                    let runtime = super::plugin_tui::load_default_runtime_with_static_bundled(
                        &super::static_bundled_plugins(),
                    );
                    let result = match runtime {
                        Ok(runtime) => {
                            super::interactive_surface::InteractiveSurfaceState::open_request(
                                &runtime, &request,
                            )
                            .await
                            .map_err(|error| error.to_string())
                        }
                        Err(error) => Err(error.to_string()),
                    };
                    Some(BcodeRuntimeMessage::InteractiveSurfaceOpened(result))
                },
            ));
        }
        commands
    }

    fn schedule_deadlines(&self) {
        let Some(handle) = &self.runtime_handle else {
            return;
        };
        let now = Instant::now();
        let invalidation_at = self
            .chat
            .app
            .invalidation_requests(now, std::time::SystemTime::now())
            .into_iter()
            .map(|request| request.at)
            .min();
        let deadlines = [
            (RootTimer::Invalidations, invalidation_at),
            (
                RootTimer::ArtifactRetry,
                self.loop_state.next_artifact_retry_at(),
            ),
            (
                RootTimer::StreamingPresentation,
                self.chat.app.next_streaming_presentation_deadline(now),
            ),
            (RootTimer::DraftSave, self.draft_autosave.next_save_at()),
            (
                RootTimer::InteractiveSurfaceRetry,
                self.loop_state.next_interactive_surface_retry_at(),
            ),
            (
                RootTimer::TelemetryFlush,
                self.loop_state.next_telemetry_flush_at(),
            ),
        ];
        for (timer, deadline) in deadlines {
            if let Some(deadline) = deadline {
                handle.schedule_timer(timer.id(), deadline);
            } else {
                let _cancelled = handle.cancel_timer(&timer.id());
            }
        }
    }

    fn handle_timer(
        &mut self,
        timer: &bmux_tui_runtime::TimerId,
    ) -> super::invalidation::UiInvalidation {
        let now = Instant::now();
        match timer.as_str() {
            "bcode.invalidations" => {
                let due = self
                    .chat
                    .app
                    .invalidation_requests(now, std::time::SystemTime::now())
                    .into_iter()
                    .filter(|request| request.at <= now)
                    .map(|request| request.key)
                    .collect::<Vec<_>>();
                self.chat.app.handle_invalidations(&due, now)
            }
            "bcode.artifact_retry" => {
                self.loop_state.start_due_artifact_fetches(now);
                super::invalidation::UiInvalidation::None
            }
            "bcode.streaming_presentation" => {
                if self.chat.app.advance_streaming_presentation(now) {
                    super::invalidation::UiInvalidation::Paint
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            "bcode.draft_save" => {
                super::chat_loop::start_draft_save(&mut self.chat, &mut self.draft_autosave);
                super::invalidation::UiInvalidation::None
            }
            "bcode.telemetry_flush" => {
                if let Some(handle) = &self.runtime_handle {
                    self.loop_state.record_runtime_stats(&handle.stats());
                }
                self.loop_state.flush_telemetry_if_due(now);
                super::invalidation::UiInvalidation::None
            }
            _ => super::invalidation::UiInvalidation::None,
        }
    }

    #[allow(dead_code)]
    pub fn abort_all_effects(&mut self) {
        self.loop_state.abort_all_effects();
    }

    /// Mark the currently accumulated semantic damage as successfully presented.
    #[allow(dead_code)]
    pub fn presentation_committed(&mut self, at: Instant) {
        self.invalidation = super::invalidation::UiInvalidation::None;
        self.last_presented_at = Some(at);
        self.loop_state.mark_presentation_committed();
        if self
            .loop_state
            .apply_deferred_session_stream_updates(&mut self.chat)
        {
            self.invalidation = super::invalidation::UiInvalidation::Structural;
        }
    }
}

/// Synchronous root presenter preserving Bcode frame, hit-map, cursor, and image commit ordering.
#[allow(dead_code)]
pub struct BcodeRuntimePresenter<'a, 'b, W> {
    terminal: &'a mut bmux_tui::terminal::Terminal<&'b mut W>,
}

impl<'a, 'b, W> BcodeRuntimePresenter<'a, 'b, W> {
    /// Create a presenter around the caller-owned terminal.
    #[allow(dead_code)]
    #[must_use]
    pub const fn new(terminal: &'a mut bmux_tui::terminal::Terminal<&'b mut W>) -> Self {
        Self { terminal }
    }
}

impl<W: std::io::Write> bmux_tui_runtime::Presenter<BcodeRuntimeModel>
    for BcodeRuntimePresenter<'_, '_, W>
{
    type Error = TuiError;

    fn resize(&mut self, size: bmux_tui::geometry::Size) {
        self.terminal
            .resize(bmux_tui::geometry::Rect::new(0, 0, size.width, size.height));
    }

    fn reset(&mut self, _reason: bmux_tui_runtime::ResetReason) {
        self.terminal.reset();
    }

    fn present(
        &mut self,
        program: &mut BcodeRuntimeModel,
    ) -> Result<bmux_tui_runtime::PresentReport, Self::Error> {
        let started = Instant::now();
        let frame_interval = program.settings.bmux_runtime_config().frame_interval;
        super::chat_loop::draw_chat_frame(
            self.terminal,
            &mut program.chat,
            &mut program.loop_state,
            Duration::ZERO,
            frame_interval,
        )?;
        program.committed_hits = self.terminal.hits().clone();
        program.committed_area = self.terminal.area();
        program.presentation_committed(started);
        Ok(bmux_tui_runtime::PresentReport::default())
    }
}

impl bmux_tui_runtime::Program for BcodeRuntimeModel {
    type Message = BcodeRuntimeMessage;
    type Error = TuiError;

    #[allow(clippy::too_many_lines)]
    fn update(
        &mut self,
        event: bmux_tui_runtime::RuntimeEvent<Self::Message>,
    ) -> Result<bmux_tui_runtime::Update<Self::Message>, Self::Error> {
        let damage = match event {
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::Bootstrap { handle }) => {
                self.runtime_handle = Some(handle.clone());
                let (_replacement_sender, replacement_receiver) = tokio::sync::mpsc::channel(1);
                let mut session_stream =
                    std::mem::replace(&mut self.chat.event_receiver, replacement_receiver);
                let mut artifact_completions = self.loop_state.take_artifact_completion_receiver();
                let mut markdown_completions = self.loop_state.take_markdown_completion_receiver();
                let session_subscription = bmux_tui_runtime::Subscription::new(
                    bmux_tui_runtime::SubscriptionKey::new("bcode.session_stream"),
                    move |sender| async move {
                        while let Some(update) = session_stream.recv().await {
                            if sender
                                .send(BcodeRuntimeMessage::SessionStream(Box::new(update)))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    },
                );
                let artifact_subscription = bmux_tui_runtime::Subscription::new(
                    bmux_tui_runtime::SubscriptionKey::new("bcode.artifact_completions"),
                    move |sender| async move {
                        while let Some(completion) = artifact_completions.recv().await {
                            if sender
                                .send(BcodeRuntimeMessage::ArtifactFetchCompleted(Box::new(
                                    completion,
                                )))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    },
                );
                let markdown_subscription = bmux_tui_runtime::Subscription::new(
                    bmux_tui_runtime::SubscriptionKey::new("bcode.markdown_completions"),
                    move |sender| async move {
                        loop {
                            if markdown_completions.changed().await.is_err() {
                                break;
                            }
                            let completion = markdown_completions.borrow_and_update().clone();
                            if sender
                                .send(BcodeRuntimeMessage::MarkdownProjectionCompleted(Box::new(
                                    completion,
                                )))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    },
                );
                self.schedule_deadlines();
                let mut update = bmux_tui_runtime::Update::redraw()
                    .with_subscription(session_subscription)
                    .with_subscription(artifact_subscription)
                    .with_subscription(markdown_subscription);
                for command in self.collect_runtime_work(&handle) {
                    update = update.with_command(command);
                }
                return Ok(update);
            }
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::TerminalInputFailed(
                error,
            )) => return Err(error.into()),
            bmux_tui_runtime::RuntimeEvent::Terminal(event)
            | bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::Terminal(event)) => {
                if self.loop_state.has_interactive_surface() {
                    if let Event::Key(stroke) = event
                        && let Some(action) = self.root_interactive_surface_host_key(stroke)
                    {
                        match action {
                            super::keymap::BmuxAction::AppInterrupt => {
                                let Some((interaction_id, resolution)) =
                                    self.loop_state.dismiss_interactive_surface()
                                else {
                                    return Ok(bmux_tui_runtime::Update::none());
                                };
                                let command = self.interactive_surface_resolution_command(
                                    interaction_id,
                                    resolution,
                                );
                                return Ok(bmux_tui_runtime::Update::redraw().with_command(command));
                            }
                            super::keymap::BmuxAction::AppExit => self.chat.app.request_exit(),
                            action => {
                                let _handled = super::input::handle_chat_action(
                                    &mut self.chat.app,
                                    Some(action),
                                );
                            }
                        }
                        return Ok(bmux_tui_runtime::Update::redraw());
                    }
                    if let Event::Mouse(mouse) = event
                        && matches!(
                            mouse.kind,
                            bmux_tui::event::MouseEventKind::ScrollUp
                                | bmux_tui::event::MouseEventKind::ScrollDown
                        )
                    {
                        let _changed = super::mouse_flow::handle_non_permission_mouse(
                            None,
                            &mut self.chat,
                            mouse,
                            self.settings.mouse_scroll_rows(),
                        );
                        return Ok(bmux_tui_runtime::Update::redraw());
                    }
                }
                let route_to_surface = match event {
                    Event::Mouse(mouse) if self.loop_state.has_interactive_surface() => self
                        .loop_state
                        .active_interactive_surface_area()
                        .is_some_and(|area| area.contains(mouse.position)),
                    _ => true,
                };
                if route_to_surface
                    && let Some((interaction_id, resolution)) =
                        self.loop_state.handle_interactive_surface_event(&event)
                {
                    let command =
                        self.interactive_surface_resolution_command(interaction_id, resolution);
                    return Ok(bmux_tui_runtime::Update::redraw().with_command(command));
                }
                self.handle_basic_terminal_event(event)
            }
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::Invalidations(keys)) => {
                self.chat.app.handle_invalidations(&keys, Instant::now())
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::StreamingPresentationDue,
            ) => {
                if self.chat.app.advance_streaming_presentation(Instant::now()) {
                    super::invalidation::UiInvalidation::Paint
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::SessionStream(update)) => {
                if self
                    .loop_state
                    .apply_session_stream_update(&mut self.chat, *update)
                {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::ArtifactFetchCompleted(completion),
            ) => {
                if self
                    .loop_state
                    .apply_artifact_completion(&self.chat, *completion)
                {
                    super::invalidation::UiInvalidation::Items
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::MarkdownProjectionCompleted(completion),
            ) => {
                if (*completion).is_some_and(|completion| {
                    self.loop_state
                        .apply_markdown_projection_completion(&mut self.chat, completion)
                }) {
                    super::invalidation::UiInvalidation::Items
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::EffectCompleted(
                result,
            )) => {
                if let TuiEffectResult::AppendPresentationNote { session_id, .. } = result.as_ref()
                {
                    self.ordered_notes.complete(*session_id);
                }
                let previous_frame_interval = self.settings.bmux_runtime_config().frame_interval;
                let previous_session_id = self.chat.session_id;
                let observation = result.daemon_observation();
                self.loop_state.observe_daemon(&mut self.chat, &observation);
                super::chat_loop::apply_effect_result(
                    &mut self.settings,
                    &mut self.chat,
                    &mut self.draft_autosave,
                    &mut self.loop_state,
                    *result,
                );
                if previous_session_id != self.chat.session_id {
                    self.handle_session_changed();
                }
                let frame_interval = self.settings.bmux_runtime_config().frame_interval;
                if frame_interval != previous_frame_interval
                    && let Some(handle) = &self.runtime_handle
                {
                    handle.set_frame_interval(frame_interval);
                }
                super::invalidation::UiInvalidation::Structural
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::InteractiveSurfaceOpened(result),
            ) => {
                if self.loop_state.complete_interactive_surface_open(result) {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    self.chat
                        .app
                        .set_status("Interactive request unavailable; retrying".to_owned());
                    super::invalidation::UiInvalidation::Paint
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::InteractiveSurfaceResolved(result),
            ) => match result {
                Ok(resolved) => {
                    self.loop_state
                        .complete_interactive_surface_resolution(resolved);
                    self.chat.app.set_status(if resolved {
                        "interactive request resolved".to_owned()
                    } else {
                        "interactive request was already resolved by another client".to_owned()
                    });
                    super::invalidation::UiInvalidation::Structural
                }
                Err(error) => {
                    self.loop_state
                        .complete_interactive_surface_resolution(false);
                    self.chat
                        .app
                        .set_status(format!("Interactive response failed; retry: {error}"));
                    super::invalidation::UiInvalidation::Paint
                }
            },
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::DraftSaveDue) => {
                super::chat_loop::start_draft_save(&mut self.chat, &mut self.draft_autosave);
                super::invalidation::UiInvalidation::None
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::InteractionRetryDue | BcodeRuntimeMessage::TelemetryFlushDue,
            ) => super::invalidation::UiInvalidation::None,
            bmux_tui_runtime::RuntimeEvent::Timer(timer) => self.handle_timer(&timer),
        };
        self.invalidation = self.invalidation.merge(damage);
        self.draft_autosave.observe(&self.chat, Instant::now());
        let housekeeping = self.loop_state.prepare_runtime_work(&mut self.chat);
        self.invalidation = self.invalidation.merge(housekeeping);
        let mut update = match self.invalidation {
            super::invalidation::UiInvalidation::None => bmux_tui_runtime::Update::none(),
            super::invalidation::UiInvalidation::Full => bmux_tui_runtime::Update::reset(),
            super::invalidation::UiInvalidation::Paint
            | super::invalidation::UiInvalidation::Items
            | super::invalidation::UiInvalidation::Structural => bmux_tui_runtime::Update::redraw(),
        };
        if self.exit_requested || self.chat.app.should_exit() {
            update = update.merge(bmux_tui_runtime::Update::exit());
        }
        self.schedule_deadlines();
        if let Some(handle) = self.runtime_handle.clone() {
            for command in self.collect_runtime_work(&handle) {
                update = update.with_command(command);
            }
        }
        Ok(update)
    }
}

/// Record one live root-runtime statistics snapshot into Bcode telemetry.
pub fn record_runtime_stats(model: &mut BcodeRuntimeModel, stats: &bmux_tui_runtime::RuntimeStats) {
    model.loop_state.record_runtime_stats(stats);
    model.loop_state.flush_telemetry_if_due(Instant::now());
}

/// Consume a successful root-runtime output after recording its final neutral statistics.
#[allow(dead_code)]
pub fn finish_runtime<P>(
    mut output: bmux_tui_runtime::RuntimeOutput<BcodeRuntimeModel, P>,
) -> BcodeRuntimeModel {
    record_runtime_stats(&mut output.program, &output.stats);
    output.program.abort_all_effects();
    output.program
}

/// Run a constructed root runtime, map failures, record final statistics, and stop owned work.
#[allow(dead_code)]
pub async fn run<W: std::io::Write>(
    runtime: bmux_tui_runtime::Runtime<BcodeRuntimeModel, BcodeRuntimePresenter<'_, '_, W>>,
    handle: bmux_tui_runtime::RuntimeHandle<BcodeRuntimeMessage>,
) -> Result<BcodeRuntimeModel, TuiError> {
    let input = bmux_tui_runtime::TerminalInput::start::<BcodeRuntimeModel>(
        handle,
        BcodeRuntimeMessage::TerminalInputFailed,
    );
    let result = match Box::pin(runtime.run()).await {
        Ok(output) => Ok(finish_runtime(output)),
        Err(
            bmux_tui_runtime::RuntimeError::Program { error, mut output }
            | bmux_tui_runtime::RuntimeError::Presenter { error, mut output },
        ) => {
            record_runtime_stats(&mut output.program, &output.stats);
            output.program.abort_all_effects();
            Err(error)
        }
    };
    input.request_shutdown();
    result
}

/// Construct the root runtime and its bounded admission handle.
#[allow(dead_code)]
pub fn runtime<'a, 'b, W: std::io::Write>(
    terminal: &'a mut bmux_tui::terminal::Terminal<&'b mut W>,
    model: BcodeRuntimeModel,
) -> (
    bmux_tui_runtime::Runtime<BcodeRuntimeModel, BcodeRuntimePresenter<'a, 'b, W>>,
    bmux_tui_runtime::RuntimeHandle<BcodeRuntimeMessage>,
) {
    let config = model.settings.bmux_runtime_config();
    let (runtime, handle) =
        bmux_tui_runtime::Runtime::new(model, BcodeRuntimePresenter::new(terminal), config);
    let bootstrap = BcodeRuntimeMessage::Bootstrap {
        handle: handle.clone(),
    };
    assert!(
        handle.try_send(bootstrap).is_ok(),
        "new root runtime accepts bootstrap message"
    );
    (runtime, handle)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum BcodeRuntimeScreen {
    /// Normal chat/session presentation.
    #[default]
    Chat,
    /// Plugin-contributed terminal surface.
    PluginSurface,
    /// Session picker or transcript-search surface.
    SessionPicker,
    /// Onboarding/setup surface.
    Onboarding,
}

#[cfg(test)]
mod tests {
    use super::{BcodeRuntimeAdmissionError, BcodeRuntimeMessage, OrderedPresentationQueue, admit};
    use bcode_command::CommandTextFormat;
    use bcode_session_models::SessionId;
    use std::collections::{BTreeMap, VecDeque};
    use std::convert::Infallible;

    fn assert_runtime_message_is_send<T: Send + 'static>() {}

    #[test]
    fn root_message_contract_is_runtime_admissible() {
        assert_runtime_message_is_send::<BcodeRuntimeMessage>();
    }

    #[test]
    fn root_runtime_and_presenter_types_compose() {
        fn assert_runtime<P, R>()
        where
            P: bmux_tui_runtime::Program<Message = BcodeRuntimeMessage>,
            R: bmux_tui_runtime::Presenter<P>,
        {
        }

        assert_runtime::<
            super::BcodeRuntimeModel,
            super::BcodeRuntimePresenter<'static, 'static, Vec<u8>>,
        >();
    }

    fn note(session_id: SessionId, note_id: &str) -> super::super::effects::TuiEffect {
        super::super::effects::TuiEffect::AppendPresentationNote {
            session_id,
            source_id: "test".to_owned(),
            note_id: note_id.to_owned(),
            text: note_id.to_owned(),
            format: CommandTextFormat::PlainText,
        }
    }

    #[test]
    fn ordered_note_queue_releases_one_per_session_until_typed_completion() {
        let first_session = SessionId::new();
        let second_session = SessionId::new();
        let mut notes = BTreeMap::new();
        notes.insert(
            first_session,
            VecDeque::from([note(first_session, "0001"), note(first_session, "0002")]),
        );
        notes.insert(
            second_session,
            VecDeque::from([note(second_session, "0003")]),
        );
        let mut queue = OrderedPresentationQueue::default();
        queue.append(notes);

        let first = queue.take_ready();
        assert_eq!(first.len(), 2);
        assert!(queue.take_ready().is_empty());

        queue.complete(first_session);
        let second = queue.take_ready();
        assert_eq!(second.len(), 1);
        assert!(queue.take_ready().is_empty());
    }

    #[derive(Default)]
    struct AdmissionProgram {
        received: usize,
    }

    impl bmux_tui_runtime::Program for AdmissionProgram {
        type Message = BcodeRuntimeMessage;
        type Error = Infallible;

        fn update(
            &mut self,
            event: bmux_tui_runtime::RuntimeEvent<Self::Message>,
        ) -> Result<bmux_tui_runtime::Update<Self::Message>, Self::Error> {
            if matches!(event, bmux_tui_runtime::RuntimeEvent::Message(_)) {
                self.received += 1;
            }
            Ok(if self.received == 2 {
                bmux_tui_runtime::Update::exit()
            } else {
                bmux_tui_runtime::Update::none()
            })
        }
    }

    #[tokio::test]
    async fn domain_owned_admission_separates_reliable_and_latest_messages() {
        let config = bmux_tui_runtime::RuntimeConfig {
            frame_interval: None,
            ..bmux_tui_runtime::RuntimeConfig::default()
        };
        let (runtime, handle) = bmux_tui_runtime::Runtime::new(
            AdmissionProgram::default(),
            bmux_tui_runtime::HeadlessPresenter::default(),
            config,
        );
        admit(&handle, BcodeRuntimeMessage::DraftSaveDue)
            .await
            .expect("reliable message admitted");
        admit(&handle, BcodeRuntimeMessage::StreamingPresentationDue)
            .await
            .expect("latest message admitted");
        let output = runtime
            .run()
            .await
            .unwrap_or_else(|_| panic!("runtime succeeds"));
        assert_eq!(output.program.received, 2);
        assert_eq!(output.stats.reliable_processed, 1);
        assert_eq!(output.stats.latest_processed, 1);
        assert_ne!(
            BcodeRuntimeAdmissionError::Full,
            BcodeRuntimeAdmissionError::Closed
        );
    }
}
