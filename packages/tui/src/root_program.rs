//! Bcode-owned root runtime message and model contracts.
//!
//! These types establish the application boundary before orchestration migrates from the existing
//! chat loop. BMUX treats messages and model state as opaque application data.

use std::collections::{BTreeMap, VecDeque};
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
    /// Plugin surface host invalidation became pending.
    PluginSurfaceInvalidated,
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
            | Self::PluginSurfaceInvalidated
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
    /// Previously active root screen retained while temporary native-session navigation runs.
    suspended_screen: Option<BcodeRuntimeScreen>,
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
    /// Whether closing the root plugin surface should terminate this runtime invocation.
    exit_after_plugin_surface: bool,
    /// Standalone plugin close result returned to the launcher.
    plugin_surface_result: Option<(String, Option<serde_json::Value>)>,
    /// Whether the root program should terminate after its dirty state is committed.
    pub exit_requested: bool,
    theme_input_signature: u64,
    theme_reload_at: Instant,
    scheduled_deadlines: BTreeMap<bmux_tui_runtime::TimerId, Instant>,
}

enum RootTimer {
    Invalidations,
    ArtifactRetry,
    StreamingPresentation,
    DraftSave,
    InteractiveSurfaceRetry,
    TelemetryFlush,
    ThemeReload,
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
            Self::ThemeReload => "bcode.theme_reload",
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
        let theme_input_signature = super::theme::active_theme_input_signature(&chat.app);
        let theme_reload_at = Instant::now() + Duration::from_millis(750);
        Self {
            chat,
            loop_state,
            settings,
            draft_autosave,
            screen: BcodeRuntimeScreen::Chat,
            suspended_screen: None,
            runtime_handle: None,
            deferred: VecDeque::new(),
            ordered_notes: OrderedPresentationQueue::default(),
            invalidation: super::invalidation::UiInvalidation::Full,
            committed_hits: bmux_tui::hit::HitMap::default(),
            committed_area: bmux_tui::geometry::Rect::new(0, 0, 0, 0),
            last_presented_at: None,
            exit_after_plugin_surface: false,
            plugin_surface_result: None,
            exit_requested: false,
            theme_input_signature,
            theme_reload_at,
            scheduled_deadlines: BTreeMap::new(),
        }
    }

    fn active_screen(&self) -> BcodeRuntimeScreen {
        self.loop_state.active_root_screen()
    }

    fn synchronize_screen(&mut self) -> super::invalidation::UiInvalidation {
        let next = self.active_screen();
        if next == self.screen {
            return super::invalidation::UiInvalidation::None;
        }
        self.screen = next;
        super::invalidation::UiInvalidation::Structural
    }

    const fn suspend_screen_for_session_navigation(&mut self) {
        self.suspended_screen = Some(self.screen);
        self.screen = BcodeRuntimeScreen::Chat;
    }

    fn resume_screen_after_session_navigation(&mut self) {
        let expected = self.suspended_screen.take();
        let active = self.active_screen();
        self.screen = expected
            .filter(|screen| *screen == active)
            .unwrap_or(active);
    }

    pub fn handle_ralph_surface_action(
        &mut self,
        action: super::ralph_launcher::RalphHomeAction,
    ) -> super::invalidation::UiInvalidation {
        use super::ralph_flow::RalphRootAction;

        let action = match action {
            super::ralph_launcher::RalphHomeAction::Plan => RalphRootAction::Plan,
            super::ralph_launcher::RalphHomeAction::SaveDraft => RalphRootAction::SaveDraft,
            super::ralph_launcher::RalphHomeAction::ViewDraft => RalphRootAction::ViewDraft,
            super::ralph_launcher::RalphHomeAction::ReviseDraft => RalphRootAction::ReviseDraft,
            super::ralph_launcher::RalphHomeAction::ApproveDraft => RalphRootAction::ApproveDraft,
            super::ralph_launcher::RalphHomeAction::ApplyDraftToLoop => {
                RalphRootAction::ApplyDraftToLoop
            }
            super::ralph_launcher::RalphHomeAction::CreateFromDraft => {
                RalphRootAction::CreateFromDraft
            }
            super::ralph_launcher::RalphHomeAction::Run
            | super::ralph_launcher::RalphHomeAction::Goal => RalphRootAction::Run,
            super::ralph_launcher::RalphHomeAction::Approve => RalphRootAction::Approve,
            super::ralph_launcher::RalphHomeAction::Stop => RalphRootAction::Stop,
            super::ralph_launcher::RalphHomeAction::Resume => RalphRootAction::Resume,
            super::ralph_launcher::RalphHomeAction::Status => RalphRootAction::ShowStatus,
            super::ralph_launcher::RalphHomeAction::Runs => RalphRootAction::ListRuns,
            super::ralph_launcher::RalphHomeAction::Iterations => RalphRootAction::ListIterations,
            super::ralph_launcher::RalphHomeAction::Open => RalphRootAction::OpenProgress,
            super::ralph_launcher::RalphHomeAction::Audit => RalphRootAction::Audit,
            super::ralph_launcher::RalphHomeAction::Replan => RalphRootAction::Replan,
            super::ralph_launcher::RalphHomeAction::RebuildLoopContext => {
                self.chat
                    .app
                    .set_status("Ralph rebuild is unavailable from this surface".to_owned());
                return super::invalidation::UiInvalidation::Structural;
            }
            super::ralph_launcher::RalphHomeAction::Start => {
                self.loop_state.open_ralph_start_dialog(
                    self.settings.launch_working_directory(),
                    &mut self.chat,
                );
                return super::invalidation::UiInvalidation::Structural;
            }
        };
        if action.requires_client() {
            let repo_root = self.chat.app.working_directory().map_or_else(
                || self.settings.launch_working_directory().to_path_buf(),
                std::path::Path::to_path_buf,
            );
            self.chat
                .replace_effect(super::effects::TuiEffect::RalphAction { repo_root, action });
            self.chat.app.set_status("running Ralph action…".to_owned());
        } else if let Err(error) =
            super::ralph_flow::execute_root_local_action(&mut self.chat, action)
        {
            self.chat
                .app
                .set_status(format!("Ralph action failed: {error}"));
        }
        super::invalidation::UiInvalidation::Structural
    }

    /// Return whether a root plugin surface is active.
    #[cfg(test)]
    pub const fn has_plugin_surface(&self) -> bool {
        self.loop_state.has_root_plugin_surface()
    }

    /// Queue a plugin-owned surface that should be active when the root runtime starts.
    pub fn queue_plugin_surface(
        &mut self,
        plugin_id: impl Into<String>,
        surface: bcode_plugin_sdk::tui::BoxedPluginTuiSurface,
    ) {
        self.loop_state
            .queue_root_plugin_surface(plugin_id, surface);
        let _invalidation = self.synchronize_screen();
        self.invalidation = super::invalidation::UiInvalidation::Full;
    }

    /// Queue a plugin-owned surface as the complete standalone product screen.
    pub fn queue_standalone_plugin_surface(
        &mut self,
        plugin_id: impl Into<String>,
        surface: bcode_plugin_sdk::tui::BoxedPluginTuiSurface,
    ) {
        self.queue_plugin_surface(plugin_id, surface);
        self.exit_after_plugin_surface = true;
    }

    /// Take the close result produced by a standalone plugin surface.
    pub const fn take_plugin_surface_result(
        &mut self,
    ) -> Option<(String, Option<serde_json::Value>)> {
        self.plugin_surface_result.take()
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

    fn apply_plugin_surface_close(
        &mut self,
        plugin_id: &str,
        outcome: Option<serde_json::Value>,
    ) -> super::invalidation::UiInvalidation {
        if self.exit_after_plugin_surface {
            self.plugin_surface_result = Some((plugin_id.to_owned(), outcome));
            self.exit_requested = true;
            return super::invalidation::UiInvalidation::Structural;
        }
        if plugin_id == "bcode.ralph" {
            let action = outcome
                .and_then(|value| value.get("ralph_action").cloned())
                .and_then(|value| serde_json::from_value(value).ok());
            if let Some(action) = action {
                return self.handle_ralph_surface_action(action);
            }
            self.chat.app.set_status("Ralph UI closed".to_owned());
            return super::invalidation::UiInvalidation::Structural;
        }
        super::palette_flow::apply_plugin_surface_outcome(&mut self.chat, plugin_id, outcome);
        super::invalidation::UiInvalidation::Structural
    }

    fn finish_terminal_route_update(
        &mut self,
        damage: super::invalidation::UiInvalidation,
    ) -> super::invalidation::UiInvalidation {
        damage.merge(self.synchronize_screen())
    }

    #[allow(clippy::let_and_return, clippy::too_many_lines)]
    fn route_terminal_event(&mut self, event: Event) -> super::invalidation::UiInvalidation {
        let damage = self.handle_basic_terminal_event(event);
        self.finish_terminal_route_update(damage)
    }

    #[allow(clippy::let_and_return, clippy::too_many_lines)]
    fn handle_basic_terminal_event(&mut self, event: Event) -> super::invalidation::UiInvalidation {
        if self.loop_state.has_root_plugin_surface()
            && !self.loop_state.root_plugin_surface_is_suspended()
        {
            let action = self
                .loop_state
                .handle_root_plugin_surface_event(&event, &self.loop_state.foreground_client())
                .expect("plugin surface was present");
            match action {
                bcode_plugin_sdk::tui::PluginTuiAction::None
                | bcode_plugin_sdk::tui::PluginTuiAction::Redraw => {}
                bcode_plugin_sdk::tui::PluginTuiAction::Close { outcome } => {
                    if let Some((plugin_id, outcome)) = self
                        .loop_state
                        .close_root_plugin_surface_with_outcome(outcome)
                    {
                        return self.apply_plugin_surface_close(&plugin_id, outcome);
                    }
                }
                bcode_plugin_sdk::tui::PluginTuiAction::OpenSession { session_id } => {
                    self.suspend_screen_for_session_navigation();
                    self.loop_state
                        .suspend_root_plugin_surface_for_session(session_id);
                    super::session_flow::start_switch_session(
                        &mut self.chat,
                        session_id,
                        super::history_flow::initial_transcript_window_request(self.committed_area),
                    );
                }
                bcode_plugin_sdk::tui::PluginTuiAction::OpenSurface { surface_id } => {
                    self.chat
                        .app
                        .set_status(format!("surface toggle requested: {surface_id}"));
                }
                bcode_plugin_sdk::tui::PluginTuiAction::RunCommand { command } => {
                    self.chat.app.replace_composer_with(&command);
                    self.loop_state.close_root_plugin_surface();
                }
            }
            return super::invalidation::UiInvalidation::Structural;
        }
        if self.loop_state.has_session_picker() {
            match self.loop_state.handle_session_picker_event(
                &mut self.chat,
                self.settings.keymap(),
                &event,
            ) {
                super::chat_loop::SessionPickerRootOutcome::Create => {
                    super::session_flow::switch_to_draft_session(&mut self.chat);
                    self.chat
                        .replace_effect(super::effects::TuiEffect::LoadDraftStatus {
                            launch_working_directory: self
                                .settings
                                .launch_working_directory()
                                .to_path_buf(),
                        });
                }
                super::chat_loop::SessionPickerRootOutcome::SearchHit(hit) => {
                    match super::session_flow::start_switch_session_from_search_hit(
                        &mut self.chat,
                        &hit,
                        super::history_flow::initial_transcript_window_request(self.committed_area),
                    ) {
                        Ok(()) => self
                            .chat
                            .app
                            .set_status("opening search result…".to_owned()),
                        Err(error) => self
                            .chat
                            .app
                            .set_status(format!("search result unavailable: {error:?}")),
                    }
                }
                super::chat_loop::SessionPickerRootOutcome::Select(session_id) => {
                    super::session_flow::start_switch_session(
                        &mut self.chat,
                        session_id,
                        super::history_flow::initial_transcript_window_request(self.committed_area),
                    );
                    self.chat.app.set_status("opening session…".to_owned());
                }
                super::chat_loop::SessionPickerRootOutcome::Canceled => {
                    self.chat.app.set_status("session picker closed".to_owned());
                }
                super::chat_loop::SessionPickerRootOutcome::Handled => {}
                super::chat_loop::SessionPickerRootOutcome::Unhandled => {
                    unreachable!("session picker was present");
                }
            }
            return super::invalidation::UiInvalidation::Structural;
        }
        if self.loop_state.has_session_fork_flow() {
            match self
                .loop_state
                .handle_session_fork_event(&self.chat, &event)
            {
                super::chat_loop::SessionForkRootOutcome::Handled => {}
                super::chat_loop::SessionForkRootOutcome::Canceled => {
                    self.chat.app.set_status("fork canceled".to_owned());
                }
                super::chat_loop::SessionForkRootOutcome::LoadPrompts {
                    session_id,
                    submission,
                } => {
                    self.chat
                        .replace_effect(super::effects::TuiEffect::LoadForkPrompts {
                            session_id,
                            submission,
                        });
                    self.chat.app.set_status("loading fork prompts…".to_owned());
                }
                super::chat_loop::SessionForkRootOutcome::CreateClone {
                    session_id,
                    submission,
                } => {
                    self.chat
                        .start_effect(super::effects::TuiEffect::CloneSession {
                            session_id,
                            name: submission.name,
                            switch_after_create: submission.switch_after_create,
                            install_draft: submission.install_draft,
                            initial_window_request:
                                super::history_flow::initial_transcript_window_request(
                                    self.committed_area,
                                ),
                        });
                    self.chat.app.set_status("cloning session…".to_owned());
                }
                super::chat_loop::SessionForkRootOutcome::CreateFork {
                    session_id,
                    submission,
                    prompt,
                } => {
                    self.chat
                        .start_effect(super::effects::TuiEffect::ForkSession {
                            session_id,
                            prompt_sequence: prompt.sequence,
                            name: submission.name,
                            draft: Some(prompt.text),
                            switch_after_create: submission.switch_after_create,
                            install_draft: submission.install_draft,
                            initial_window_request:
                                super::history_flow::initial_transcript_window_request(
                                    self.committed_area,
                                ),
                        });
                    self.chat.app.set_status("forking session…".to_owned());
                }
            }
            return super::invalidation::UiInvalidation::Structural;
        }
        if self.loop_state.has_model_picker() {
            if let Some((provider_plugin_id, action)) = self
                .loop_state
                .handle_model_picker_event(self.settings.keymap(), &event)
            {
                match action {
                    super::model_flow::ModelPickerAction::Continue => {
                        if self.loop_state.has_model_picker() {
                            return super::invalidation::UiInvalidation::Structural;
                        }
                        self.chat
                            .replace_effect(super::effects::TuiEffect::LoadModelPicker {
                                provider_plugin_id,
                            });
                        self.chat.app.set_status("loading models…".to_owned());
                    }
                    super::model_flow::ModelPickerAction::Cancel => {
                        self.chat.app.set_status("model picker closed".to_owned());
                    }
                    super::model_flow::ModelPickerAction::Select(model_id) => {
                        if let Some(session_id) = self.chat.session_id {
                            self.chat
                                .start_effect(super::effects::TuiEffect::SetSessionModel {
                                    session_id,
                                    provider_plugin_id,
                                    model_id,
                                });
                            self.chat.app.set_status("applying model…".to_owned());
                        } else {
                            self.chat
                                .app
                                .apply_local_model_selection(provider_plugin_id, &model_id);
                        }
                    }
                }
            }
            return super::invalidation::UiInvalidation::Structural;
        }
        if self.loop_state.has_skill_picker() {
            let action = self
                .loop_state
                .handle_skill_picker_event(self.settings.keymap(), &event)
                .expect("skill picker was present");
            match action {
                super::skill_picker::SkillPickerAction::Continue => {}
                super::skill_picker::SkillPickerAction::Cancel => {
                    self.chat.app.set_status("skill picker closed".to_owned());
                }
                super::skill_picker::SkillPickerAction::Help(skill_id) => {
                    self.chat
                        .replace_effect(super::effects::TuiEffect::DescribeSkill { skill_id });
                    self.chat
                        .app
                        .set_status("loading skill details…".to_owned());
                }
                super::skill_picker::SkillPickerAction::Activate(skill_id) => {
                    self.start_root_skill_action(
                        super::effects::SkillActionKind::Activate,
                        skill_id,
                        String::new(),
                    );
                }
                super::skill_picker::SkillPickerAction::Deactivate(skill_id) => {
                    self.start_root_skill_action(
                        super::effects::SkillActionKind::Deactivate,
                        skill_id,
                        String::new(),
                    );
                }
                super::skill_picker::SkillPickerAction::Invoke {
                    skill_id,
                    arguments,
                } => {
                    self.start_root_skill_action(
                        super::effects::SkillActionKind::Invoke,
                        skill_id,
                        arguments,
                    );
                }
            }
            return super::invalidation::UiInvalidation::Structural;
        }
        if self.loop_state.has_ralph_start_dialog() {
            match self
                .loop_state
                .handle_ralph_start_dialog_event(&event, self.settings.keymap())
                .expect("Ralph start dialog was present")
            {
                super::ralph_start_dialog::RalphStartDialogOutcome::Handled => {}
                super::ralph_start_dialog::RalphStartDialogOutcome::Canceled => {
                    self.chat.app.set_status("Ralph start canceled".to_owned());
                }
                super::ralph_start_dialog::RalphStartDialogOutcome::Submit => {
                    let mut dialog = self
                        .loop_state
                        .take_ralph_start_dialog()
                        .expect("Ralph start dialog was present");
                    if dialog.loop_name_text().is_empty() {
                        dialog.set_status("Ralph loop name is required");
                        self.loop_state.restore_ralph_start_dialog(dialog);
                    } else {
                        let repo_root = self.chat.app.working_directory().map_or_else(
                            || self.settings.launch_working_directory().to_path_buf(),
                            std::path::Path::to_path_buf,
                        );
                        self.chat
                            .start_effect(super::effects::TuiEffect::RalphStart {
                                request: super::ralph_flow::RalphStartRequest {
                                    loop_name: dialog.loop_name_text(),
                                    repo_root,
                                    session_id: self.chat.session_id,
                                    session_title: self
                                        .chat
                                        .app
                                        .session_title()
                                        .map(ToOwned::to_owned),
                                    work_area_path: dialog.work_area_path_text(),
                                    branch: dialog.branch_text(),
                                    validation_commands: dialog.validation_command_texts(),
                                },
                            });
                        self.chat.app.set_status("creating Ralph loop…".to_owned());
                    }
                }
            }
            return super::invalidation::UiInvalidation::Structural;
        }
        if self.loop_state.has_worktree_create_dialog() {
            match self
                .loop_state
                .handle_worktree_create_dialog_event(self.settings.keymap(), &event)
            {
                super::chat_loop::WorktreeCreateDialogRootOutcome::Create {
                    name,
                    target,
                    base,
                } => {
                    let attach_session_id = match target {
                        super::wt_create_dialog::WorktreeCreateTarget::CurrentSession => {
                            self.chat.session_id
                        }
                        super::wt_create_dialog::WorktreeCreateTarget::NewSession => None,
                    };
                    self.chat
                        .start_effect(super::effects::TuiEffect::CreateWorktree {
                            request: bcode_worktree_models::WorktreeCreateRequest {
                                name,
                                cwd: Some(
                                    self.chat
                                        .app
                                        .working_directory()
                                        .unwrap_or_else(|| self.settings.launch_working_directory())
                                        .to_path_buf(),
                                ),
                                path: None,
                                branch: None,
                                new_branch: None,
                                base_ref: Some(base.model()),
                                detach: false,
                                force: false,
                                attach_session_id,
                                new_session: target
                                    == super::wt_create_dialog::WorktreeCreateTarget::NewSession,
                                no_setup: false,
                            },
                        });
                    self.chat.app.set_status("creating worktree…".to_owned());
                }
                super::chat_loop::WorktreeCreateDialogRootOutcome::Canceled => {
                    self.chat
                        .app
                        .set_status("worktree creation canceled".to_owned());
                }
                super::chat_loop::WorktreeCreateDialogRootOutcome::Handled => {}
                super::chat_loop::WorktreeCreateDialogRootOutcome::Unhandled => {
                    unreachable!("worktree dialog was present")
                }
            }
            return super::invalidation::UiInvalidation::Structural;
        }
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
                match self
                    .loop_state
                    .handle_timeline_dialog_key(&mut self.chat, stroke)
                {
                    super::chat_loop::TimelineDialogRootOutcome::Handled => {
                        return super::invalidation::UiInvalidation::Structural;
                    }
                    super::chat_loop::TimelineDialogRootOutcome::Jump(entry) => {
                        self.apply_root_timeline_jump(&entry);
                        return super::invalidation::UiInvalidation::Structural;
                    }
                    super::chat_loop::TimelineDialogRootOutcome::Unhandled => {}
                }
                match self
                    .loop_state
                    .handle_thinking_dialog_key(&mut self.chat, stroke)
                {
                    super::chat_loop::ThinkingDialogRootOutcome::Handled => {
                        return super::invalidation::UiInvalidation::Structural;
                    }
                    super::chat_loop::ThinkingDialogRootOutcome::Apply {
                        effort,
                        summary,
                        visible,
                        mode,
                    } => {
                        self.apply_root_thinking_dialog(effort, summary, visible, mode);
                        return super::invalidation::UiInvalidation::Structural;
                    }
                    super::chat_loop::ThinkingDialogRootOutcome::Unhandled => {}
                }
                if self
                    .loop_state
                    .handle_theme_picker_key(&mut self.chat, stroke)
                {
                    return super::invalidation::UiInvalidation::Structural;
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
                            self.stage_root_submission(bcode_ipc::PromptPlacement::Steering);
                            return super::invalidation::UiInvalidation::Structural;
                        }
                        super::chat_loop::SlashPaletteRootOutcome::Unhandled => {}
                    }
                }
                if self
                    .settings
                    .keymap()
                    .action_for_key(super::keymap::BmuxScope::Chat, stroke)
                    == Some(super::keymap::BmuxAction::ClipboardPasteImage)
                {
                    self.paste_clipboard_image();
                    return super::invalidation::UiInvalidation::Structural;
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
                        self.stage_root_submission(placement);
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
                if self.loop_state.handle_theme_picker_mouse(
                    &mut self.chat,
                    mouse,
                    self.committed_area,
                ) {
                    return super::invalidation::UiInvalidation::Structural;
                }
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

    fn start_root_skill_action(
        &mut self,
        action: super::effects::SkillActionKind,
        skill_id: bcode_skill_models::SkillId,
        arguments: String,
    ) {
        super::skill_flow::start_skill_action(
            self.settings.launch_working_directory(),
            &mut self.chat,
            action,
            skill_id,
            arguments,
        );
    }

    fn apply_root_timeline_jump(&mut self, entry: &super::timeline_dialog::TimelineEntry) {
        if let Some(index) = entry.transcript_index()
            && self.chat.app.jump_to_transcript_index(index)
        {
            self.chat
                .app
                .set_status("jumped to timeline message".to_owned());
            return;
        }
        let Some(session_id) = self.chat.session_id else {
            self.chat
                .app
                .set_status("timeline requires an active session".to_owned());
            return;
        };
        self.chat
            .replace_effect(super::effects::TuiEffect::LoadTimelineJump {
                session_id,
                sequence: entry.sequence(),
            });
        self.chat
            .app
            .set_status("loading timeline message…".to_owned());
    }

    fn paste_clipboard_image(&mut self) {
        let working_directory = self.chat.app.working_directory().map_or_else(
            || self.settings.launch_working_directory().to_path_buf(),
            std::path::Path::to_path_buf,
        );
        match super::clipboard_image::save_clipboard_image(
            self.chat.app.session_id(),
            &working_directory,
        ) {
            Ok(artifact) => {
                let text = super::clipboard_image::pasted_image_text(&artifact.model);
                self.chat.app.reset_input_history_navigation();
                self.chat.app.paste_composer_text(&text);
                self.chat.app.wake_cursor();
                self.chat.app.set_status(format!(
                    "Image pasted: {}; source saved in session artifacts",
                    bcode_plugin_sdk::path::display_from_current_dir(&artifact.model)
                ));
            }
            Err(error) => self
                .chat
                .app
                .set_status(format!("image paste failed: {error}")),
        }
    }

    fn apply_root_thinking_dialog(
        &mut self,
        effort: Option<String>,
        summary: Option<String>,
        visible: bool,
        mode: bcode_config::TuiThinkingMode,
    ) {
        self.chat.app.set_reasoning_visible(visible);
        self.chat.app.set_reasoning_display_mode(mode);
        let effort_generation = effort
            .as_ref()
            .filter(|effort| self.chat.app.reasoning_effort() != Some(effort.as_str()))
            .map(|effort| self.chat.app.set_pending_reasoning_effort(effort.clone()));
        if let Some(session_id) = self.chat.session_id {
            self.chat
                .start_effect(super::effects::TuiEffect::SetSessionReasoning {
                    session_id,
                    effort,
                    summary,
                    effort_generation,
                    status: "reasoning output settings applied".to_owned(),
                });
            self.chat.app.set_status("setting thinking…".to_owned());
        } else {
            self.chat.app.apply_reasoning_selection(effort, summary);
            self.chat.app.set_status(format!(
                "reasoning output settings applied: {}",
                self.chat.app.thinking_label()
            ));
        }
    }

    fn stage_root_submission(&mut self, placement: bcode_ipc::PromptPlacement) {
        let launch_working_directory = self.settings.launch_working_directory().to_path_buf();
        match super::composer_flow::stage_root_submission(
            &launch_working_directory,
            &mut self.chat,
            placement,
        ) {
            super::composer_flow::RootSubmission::MessageStaged(staged) => {
                if staged {
                    self.draft_autosave.mark_dirty_now();
                }
            }
            super::composer_flow::RootSubmission::SlashCommand(message) => {
                let working_directory = self
                    .chat
                    .app
                    .working_directory()
                    .unwrap_or_else(|| self.settings.launch_working_directory())
                    .to_path_buf();
                self.chat
                    .replace_effect(super::effects::TuiEffect::ExecuteSlashCommand {
                        session_id: self.chat.session_id,
                        working_directory,
                        current_agent_id: self.chat.app.current_agent_id().to_owned(),
                        reasoning_display_mode: self.chat.app.reasoning_display_mode(),
                        reasoning_visible: self.chat.app.reasoning_visible(),
                        message,
                    });
                self.chat
                    .app
                    .set_status("running slash command…".to_owned());
            }
        }
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
                "session.switch" | "session.rename" | "session.delete" => {
                    self.loop_state.open_session_picker(&mut self.chat);
                }
                "session.fork" => self.loop_state.open_session_fork_dialog(&mut self.chat),
                "session.clone" => {
                    if let Some(session_id) = self.chat.session_id {
                        self.chat
                            .start_effect(super::effects::TuiEffect::CloneSession {
                                session_id,
                                name: None,
                                switch_after_create: true,
                                install_draft: true,
                                initial_window_request:
                                    super::history_flow::initial_transcript_window_request(
                                        self.committed_area,
                                    ),
                            });
                        self.chat.app.set_status("cloning session…".to_owned());
                    } else {
                        self.chat.app.set_status("No active session".to_owned());
                    }
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
                "theme.select" => self.loop_state.open_theme_picker(&mut self.chat),
                "help" => {
                    self.chat.push_presentation_note(
                        "bcode.host",
                        "# TUI help\n\n* Use the command palette for sessions, cancellation, and context compaction."
                            .to_owned(),
                        bcode_command::CommandTextFormat::Markdown,
                    );
                    self.chat.app.set_status("help shown".to_owned());
                }
                route => self
                    .chat
                    .app
                    .set_status(format!("unsupported host command route: {route}")),
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
            let keymap = self.settings.keymap().clone();
            commands.push(bmux_tui_runtime::Command::start_if_idle(
                bmux_tui_runtime::CommandKey::new("bcode.interactive_surface_open"),
                async move {
                    let runtime = super::plugin_tui::load_default_runtime_with_static_bundled(
                        &super::static_bundled_plugins(),
                    );
                    let result = match runtime {
                        Ok(runtime) => {
                            super::interactive_surface::InteractiveSurfaceState::open_request(
                                &runtime, &request, &keymap,
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
        if let Some(invalidation) = self.loop_state.root_plugin_surface_invalidation() {
            commands.push(bmux_tui_runtime::Command::start_if_idle(
                bmux_tui_runtime::CommandKey::new("bcode.plugin_surface_invalidation"),
                async move {
                    invalidation.wait().await;
                    Some(BcodeRuntimeMessage::PluginSurfaceInvalidated)
                },
            ));
        } else {
            commands.push(bmux_tui_runtime::Command::cancel(
                bmux_tui_runtime::CommandKey::new("bcode.plugin_surface_invalidation"),
            ));
        }
        commands
    }

    fn schedule_deadlines(&mut self) {
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
            (RootTimer::ThemeReload, Some(self.theme_reload_at)),
        ];
        for (timer, deadline) in deadlines {
            let id = timer.id();
            match deadline {
                Some(deadline) if self.scheduled_deadlines.get(&id) != Some(&deadline) => {
                    handle.schedule_timer(id.clone(), deadline);
                    self.scheduled_deadlines.insert(id, deadline);
                }
                Some(_) => {}
                None => {
                    let _cancelled = handle.cancel_timer(&id);
                    self.scheduled_deadlines.remove(&id);
                }
            }
        }
    }

    fn handle_timer(
        &mut self,
        timer: &bmux_tui_runtime::TimerId,
    ) -> super::invalidation::UiInvalidation {
        self.scheduled_deadlines.remove(timer);
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
            "bcode.theme_reload" => {
                self.theme_reload_at = now + Duration::from_millis(750);
                let signature = super::theme::active_theme_input_signature(&self.chat.app);
                if signature == self.theme_input_signature {
                    return super::invalidation::UiInvalidation::None;
                }
                self.theme_input_signature = signature;
                self.chat.app.invalidate_theme_catalog();
                if let Some(id) = self.chat.app.reload_theme_if_valid().map(str::to_owned) {
                    self.chat.app.set_status(format!("theme {id} reloaded"));
                } else {
                    self.chat.app.cancel_theme_preview();
                    self.loop_state.close_theme_picker();
                    self.chat
                        .app
                        .set_status("preview invalidated; restored configured theme".to_owned());
                }
                super::invalidation::UiInvalidation::Full
            }
            _ => super::invalidation::UiInvalidation::None,
        }
    }

    #[allow(dead_code)]
    pub fn shutdown_owned_work(&mut self) {
        self.loop_state.abort_all_effects();
        if let Some(event_task) = self.chat.event_task.take() {
            event_task.abort();
        }
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
                self.route_terminal_event(event)
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
                let refresh_permissions = matches!(
                    update.as_ref(),
                    super::history_flow::SessionStreamUpdate::Event(event)
                        if matches!(
                            event.as_ref(),
                            bcode_ipc::Event::Session(session_event)
                                if matches!(
                                    session_event.kind,
                                    bcode_session_models::SessionEventKind::PermissionRequested { .. }
                                        | bcode_session_models::SessionEventKind::PermissionResolved { .. }
                                )
                        )
                );
                let changed = self
                    .loop_state
                    .apply_session_stream_update(&mut self.chat, *update);
                if refresh_permissions {
                    self.chat
                        .replace_effect(super::effects::TuiEffect::ListPermissions);
                }
                if changed {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::PluginSurfaceInvalidated,
            ) => {
                let client = self.loop_state.foreground_client();
                match self.loop_state.poll_root_plugin_surface(&client) {
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::Close { outcome }) => {
                        if let Some((plugin_id, outcome)) = self
                            .loop_state
                            .close_root_plugin_surface_with_outcome(outcome)
                        {
                            self.apply_plugin_surface_close(&plugin_id, outcome)
                        } else {
                            super::invalidation::UiInvalidation::Structural
                        }
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::OpenSession { session_id }) => {
                        let initial_window_request =
                            super::history_flow::initial_transcript_window_request(
                                super::render::transcript_area_for_frame(
                                    &self.chat.app,
                                    self.committed_area,
                                ),
                            );
                        self.suspend_screen_for_session_navigation();
                        self.loop_state
                            .suspend_root_plugin_surface_for_session(session_id);
                        super::session_flow::start_switch_session(
                            &mut self.chat,
                            session_id,
                            initial_window_request,
                        );
                        self.handle_session_changed();
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::OpenSurface { surface_id }) => {
                        self.chat
                            .app
                            .set_status(format!("surface toggle requested: {surface_id}"));
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::RunCommand { command }) => {
                        self.chat.app.replace_composer_with(&command);
                        self.loop_state.close_root_plugin_surface();
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::Redraw) => {
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::None) | None => {
                        super::invalidation::UiInvalidation::None
                    }
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
                let navigation_session = self
                    .loop_state
                    .root_plugin_surface_pending_session_navigation();
                let navigation_result = match result.as_ref() {
                    TuiEffectResult::SessionOpened {
                        session_id, result, ..
                    } if navigation_session == Some(*session_id) => Some((
                        *session_id,
                        result.as_ref().map(|_| ()).map_err(ToString::to_string),
                    )),
                    _ => None,
                };
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
                if let Some((session_id, navigation_result)) = navigation_result {
                    let _completed = self
                        .loop_state
                        .complete_root_plugin_session_navigation(session_id, navigation_result);
                    self.resume_screen_after_session_navigation();
                } else {
                    let _invalidation = self.synchronize_screen();
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
        let route_invalidation = self.synchronize_screen();
        self.invalidation = self.invalidation.merge(route_invalidation);
        self.draft_autosave.observe(&self.chat, Instant::now());
        let housekeeping = self
            .loop_state
            .prepare_runtime_work(&mut self.chat, self.committed_area);
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
    output.program.shutdown_owned_work();
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
            output.program.shutdown_owned_work();
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

/// Root route whose state, input, rendering, and effects remain owned by Bcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BcodeRuntimeScreen {
    /// Normal chat/session presentation.
    #[default]
    Chat,
    /// Plugin-contributed terminal surface, including Ralph home flows.
    PluginSurface,
    /// Session catalog and transcript-search picker.
    SessionPicker,
    /// Provider/model selection flow.
    ModelPicker,
    /// Skill selection and invocation flow.
    SkillPicker,
    /// Session fork/create flow.
    SessionFork,
    /// Worktree creation flow.
    WorktreeCreate,
    /// Ralph loop creation flow.
    RalphStart,
    /// Host/plugin command palette.
    CommandPalette,
    /// Slash completion palette.
    SlashPalette,
    /// Permission interaction overlay.
    Permission,
    /// Reasoning/thinking configuration overlay.
    Thinking,
    /// Timeline navigation overlay.
    Timeline,
    /// Session-owned plugin interaction surface.
    InteractiveSurface,
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

    fn root_test_chat() -> super::super::session_flow::ActiveChat {
        let (event_sender, event_receiver) = super::super::history_flow::session_stream_channel();
        super::super::session_flow::ActiveChat {
            app: super::super::app::BmuxApp::new_with_history(None, &[], &[], false),
            agents: super::super::session_flow::AgentCatalog::default(),
            session_id: None,
            event_sender,
            event_receiver,
            event_task: None,
            opening_session_id: None,
            opening_session_progress: None,
            opening_session_anchor_sequence: None,
            pending_effects: super::super::effects::TuiEffectQueue::default(),
        }
    }

    #[tokio::test]
    async fn root_submission_queues_runtime_effect_work() {
        let mut chat = root_test_chat();
        chat.app.replace_composer_with("hello root runtime");
        let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let loop_state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        let mut model = super::BcodeRuntimeModel::new(chat, settings, loop_state);
        model.chat.app.stage_submission();

        model.stage_root_submission(bcode_ipc::PromptPlacement::Steering);

        assert_eq!(model.chat.queued_effect_count(), 1);
        assert_eq!(model.chat.app.composer().text(), "");
    }

    #[tokio::test]
    async fn root_screen_tracks_owned_routes_and_returns_to_chat() {
        let chat = root_test_chat();
        let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let loop_state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        let mut model = super::BcodeRuntimeModel::new(chat, settings, loop_state);

        model.loop_state.open_command_palette(&mut model.chat);
        assert!(model.synchronize_screen().needs_render());
        assert_eq!(model.screen, super::BcodeRuntimeScreen::CommandPalette);

        let escape = bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
            key: bmux_keyboard::KeyCode::Escape,
            modifiers: bmux_keyboard::Modifiers::NONE,
        });
        let _damage = model.route_terminal_event(escape);
        assert_eq!(model.screen, super::BcodeRuntimeScreen::Chat);
    }

    #[tokio::test]
    async fn root_session_picker_is_a_retained_route_and_escape_returns_to_chat() {
        let chat = root_test_chat();
        let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let loop_state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        let mut model = super::BcodeRuntimeModel::new(chat, settings, loop_state);

        model.loop_state.open_session_picker(&mut model.chat);
        assert!(model.synchronize_screen().needs_render());
        assert_eq!(model.screen, super::BcodeRuntimeScreen::SessionPicker);
        let escape = bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
            key: bmux_keyboard::KeyCode::Escape,
            modifiers: bmux_keyboard::Modifiers::NONE,
        });
        let _damage = model.route_terminal_event(escape);

        assert_eq!(model.screen, super::BcodeRuntimeScreen::Chat);
        assert!(!model.loop_state.has_session_picker());
    }

    #[tokio::test]
    async fn root_plugin_session_navigation_retains_and_resumes_surface() {
        struct NavigationSurface {
            resumed: std::sync::Arc<std::sync::Mutex<Vec<bcode_session_models::SessionId>>>,
        }

        impl bcode_plugin_sdk::tui::PluginTuiSurface for NavigationSurface {
            fn id(&self) -> &'static str {
                "test-navigation"
            }

            fn title(&self) -> &'static str {
                "Test Navigation"
            }

            fn render(
                &mut self,
                _area: bmux_tui::geometry::Rect,
                _frame: &mut bmux_tui::frame::Frame<'_>,
            ) {
            }

            fn handle_event(
                &mut self,
                _event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                bcode_plugin_sdk::tui::PluginTuiAction::None
            }

            fn session_navigation_finished(
                &mut self,
                session_id: bcode_session_models::SessionId,
                result: Result<(), String>,
            ) {
                result.expect("navigation succeeds");
                self.resumed.lock().expect("resumed lock").push(session_id);
            }
        }

        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let mut state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        let resumed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        state.queue_root_plugin_surface(
            "test.plugin",
            Box::new(NavigationSurface {
                resumed: std::sync::Arc::clone(&resumed),
            }),
        );
        let session_id = bcode_session_models::SessionId::new();

        assert!(state.suspend_root_plugin_surface_for_session(session_id));
        assert!(state.root_plugin_surface_is_suspended());
        assert!(state.complete_root_plugin_session_navigation(session_id, Ok(())));
        assert!(!state.root_plugin_surface_is_suspended());
        assert_eq!(
            state.active_root_plugin_surface_id(),
            Some("test-navigation")
        );
        assert_eq!(*resumed.lock().expect("resumed lock"), [session_id]);
    }

    #[tokio::test]
    async fn root_request_draft_handoff_waits_for_committed_paint() {
        let session_id = bcode_session_models::SessionId::new();
        let mut chat = root_test_chat();
        chat.session_id = Some(session_id);
        chat.app = super::super::app::BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let mut model = super::BcodeRuntimeModel::new(
            chat,
            settings,
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false),
        );
        let draft = |revision, text: &str| {
            super::super::history_flow::SessionStreamUpdate::Event(Box::new(
                bcode_ipc::Event::SessionLive(bcode_session_models::SessionLiveEvent {
                    session_id,
                    kind: bcode_session_models::SessionLiveEventKind::ToolRequestDraft {
                        event: bcode_session_models::ToolRequestDraftEvent {
                            output_position: None,
                            turn_id: "turn-live".to_owned(),
                            tool_call_id: "call-write".to_owned(),
                            tool_name: "filesystem.write".to_owned(),
                            producer_plugin_id: Some("bcode.filesystem".to_owned()),
                            schema: "bcode.filesystem.request-draft.write".to_owned(),
                            schema_version: 1,
                            placement: bcode_session_models::ToolContributionPlacement::Request,
                            generation: 1,
                            revision,
                            operation:
                                bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                                    start_offset: 0,
                                    text: text.to_owned(),
                                },
                            argument_bytes: text.len(),
                            truncated: false,
                        },
                    },
                }),
            ))
        };

        for update in [draft(1, ""), draft(2, r#"{"path":"live.txt""#)] {
            bmux_tui_runtime::Program::update(
                &mut model,
                bmux_tui_runtime::RuntimeEvent::Message(super::BcodeRuntimeMessage::SessionStream(
                    Box::new(update),
                )),
            )
            .expect("root update");
        }
        let first = model.chat.app.session_view_snapshot().tools["call-write"]
            .request_draft
            .as_ref()
            .expect("first draft painted");
        assert!(first.preview.is_empty());

        model.presentation_committed(std::time::Instant::now());
        let second = model.chat.app.session_view_snapshot().tools["call-write"]
            .request_draft
            .as_ref()
            .expect("deferred draft applied");
        assert_eq!(second.preview, r#"{"path":"live.txt""#);
    }

    #[tokio::test]
    async fn ralph_surface_effect_updates_root_screen_state() {
        struct RalphSurface;

        impl bcode_plugin_sdk::tui::PluginTuiSurface for RalphSurface {
            fn id(&self) -> &'static str {
                "ralph-home"
            }

            fn title(&self) -> &'static str {
                "Ralph"
            }

            fn render(
                &mut self,
                _area: bmux_tui::geometry::Rect,
                _frame: &mut bmux_tui::frame::Frame<'_>,
            ) {
            }

            fn handle_event(
                &mut self,
                _event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                bcode_plugin_sdk::tui::PluginTuiAction::None
            }
        }

        let chat = root_test_chat();
        let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let mut model = super::BcodeRuntimeModel::new(
            chat,
            settings,
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false),
        );
        bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Message(super::BcodeRuntimeMessage::EffectCompleted(
                Box::new(
                    super::super::effects::TuiEffectResult::PluginSurfaceOpened {
                        plugin_id: "bcode.ralph".to_owned(),
                        result: Ok(Box::new(RalphSurface)),
                    },
                ),
            )),
        )
        .expect("root update");

        assert!(model.has_plugin_surface());
        assert_eq!(model.screen, super::BcodeRuntimeScreen::PluginSurface);
    }

    #[tokio::test]
    async fn temporary_plugin_navigation_restores_root_route() {
        struct NavigationSurface;

        impl bcode_plugin_sdk::tui::PluginTuiSurface for NavigationSurface {
            fn id(&self) -> &'static str {
                "test-navigation"
            }

            fn title(&self) -> &'static str {
                "Test Navigation"
            }

            fn render(
                &mut self,
                _area: bmux_tui::geometry::Rect,
                _frame: &mut bmux_tui::frame::Frame<'_>,
            ) {
            }

            fn handle_event(
                &mut self,
                _event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                bcode_plugin_sdk::tui::PluginTuiAction::None
            }
        }

        let chat = root_test_chat();
        let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let loop_state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        let mut model = super::BcodeRuntimeModel::new(chat, settings, loop_state);
        model.queue_plugin_surface("test.plugin", Box::new(NavigationSurface));
        let session_id = bcode_session_models::SessionId::new();

        model.suspend_screen_for_session_navigation();
        assert!(
            model
                .loop_state
                .suspend_root_plugin_surface_for_session(session_id)
        );
        assert_eq!(model.screen, super::BcodeRuntimeScreen::Chat);
        assert!(
            model
                .loop_state
                .complete_root_plugin_session_navigation(session_id, Ok(()))
        );
        model.resume_screen_after_session_navigation();

        assert_eq!(model.screen, super::BcodeRuntimeScreen::PluginSurface);
    }

    #[tokio::test]
    async fn standalone_plugin_surface_close_exits_with_owned_outcome() {
        let chat = root_test_chat();
        let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let loop_state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        let mut model = super::BcodeRuntimeModel::new(chat, settings, loop_state);
        let expected = serde_json::json!({"selected": "workspace"});
        model.exit_after_plugin_surface = true;

        let invalidation = model.apply_plugin_surface_close("test.plugin", Some(expected.clone()));

        assert!(invalidation.needs_render());
        assert!(model.exit_requested);
        assert_eq!(
            model.take_plugin_surface_result(),
            Some(("test.plugin".to_owned(), Some(expected)))
        );
    }

    #[tokio::test]
    async fn root_ralph_local_action_returns_to_chat_without_generic_outcome_decode() {
        let chat = root_test_chat();
        let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let loop_state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        let mut model = super::BcodeRuntimeModel::new(chat, settings, loop_state);

        let invalidation = model.apply_plugin_surface_close(
            "bcode.ralph",
            Some(serde_json::json!({"ralph_action": "open"})),
        );

        assert!(invalidation.needs_render());
        assert!(model.chat.app.status().contains("no Ralph loops"));
    }

    #[derive(Default)]
    struct LatencyAcceptanceProgram {
        application_messages: usize,
        terminal_latency: Option<std::time::Duration>,
        timer_latency: Option<std::time::Duration>,
        probe_started: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    }

    impl bmux_tui_runtime::Program for LatencyAcceptanceProgram {
        type Message = u64;
        type Error = Infallible;

        fn update(
            &mut self,
            event: bmux_tui_runtime::RuntimeEvent<Self::Message>,
        ) -> Result<bmux_tui_runtime::Update<Self::Message>, Self::Error> {
            let probe_started = *self.probe_started.lock().expect("probe lock");
            match event {
                bmux_tui_runtime::RuntimeEvent::Message(_) => {
                    self.application_messages = self.application_messages.saturating_add(1);
                }
                bmux_tui_runtime::RuntimeEvent::Terminal(_) => {
                    self.terminal_latency = probe_started.map(|started| started.elapsed());
                }
                bmux_tui_runtime::RuntimeEvent::Timer(_) => {
                    self.timer_latency = probe_started.map(|started| started.elapsed());
                }
            }
            Ok(
                if self.application_messages == 10_000
                    && self.terminal_latency.is_some()
                    && self.timer_latency.is_some()
                {
                    bmux_tui_runtime::Update::exit()
                } else {
                    bmux_tui_runtime::Update::none()
                },
            )
        }
    }

    #[tokio::test]
    async fn terminal_and_timer_latency_stay_within_flood_acceptance_budget() {
        const ACCEPTANCE_BUDGET: std::time::Duration = std::time::Duration::from_millis(100);
        let probe_started = std::sync::Arc::new(std::sync::Mutex::new(None));
        let config = bmux_tui_runtime::RuntimeConfig {
            reliable_capacity: 20_000,
            messages_per_turn: 4,
            processing_time_per_turn: std::time::Duration::from_millis(1),
            frame_interval: None,
            ..bmux_tui_runtime::RuntimeConfig::default()
        };
        let (runtime, handle) = bmux_tui_runtime::Runtime::new(
            LatencyAcceptanceProgram {
                probe_started: std::sync::Arc::clone(&probe_started),
                ..LatencyAcceptanceProgram::default()
            },
            bmux_tui_runtime::HeadlessPresenter::default(),
            config,
        );
        for value in 0..10_000 {
            handle.try_send(value).expect("configured flood fits");
        }
        *probe_started.lock().expect("probe lock") = Some(std::time::Instant::now());
        handle
            .try_send_terminal(bmux_tui::event::Event::Tick)
            .expect("terminal admission remains independent");
        handle.schedule_timer(
            bmux_tui_runtime::TimerId::new("acceptance-latency"),
            std::time::Instant::now(),
        );

        let output = runtime
            .run()
            .await
            .unwrap_or_else(|_| panic!("runtime acceptance succeeds"));
        let terminal_latency = output.program.terminal_latency.expect("terminal delivered");
        let timer_latency = output.program.timer_latency.expect("timer delivered");
        assert!(
            terminal_latency <= ACCEPTANCE_BUDGET,
            "terminal latency {terminal_latency:?} exceeded {ACCEPTANCE_BUDGET:?}"
        );
        assert!(
            timer_latency <= ACCEPTANCE_BUDGET,
            "timer latency {timer_latency:?} exceeded {ACCEPTANCE_BUDGET:?}"
        );
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
