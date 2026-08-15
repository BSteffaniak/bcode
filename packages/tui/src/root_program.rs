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
    /// Streaming-configurator source or shared-presentation deadline.
    StreamingConfiguratorDue,
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
            Self::StreamingConfiguratorDue => Some(bmux_tui_runtime::MessageKey::new(
                "bcode.streaming_configurator",
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
    /// Generic terminal-space damage selected by Bcode for the next presentation.
    presentation_damage: bmux_tui::damage::Damage,
    /// Whether the next stable temporal presentation can skip full frame preparation.
    fast_temporal_presentation: bool,
    /// Last successfully committed terminal hit map used for pointer routing.
    pub committed_hits: bmux_tui::hit::HitMap,
    /// Last successfully committed terminal frame area.
    pub committed_area: bmux_tui::geometry::Rect,
    /// Last successfully committed frame layout used for regional damage projection.
    committed_layout: Option<super::render::FrameLayout>,
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
    StreamingConfigurator,
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
            Self::StreamingConfigurator => "bcode.streaming_configurator",
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
            presentation_damage: bmux_tui::damage::Damage::Full,
            fast_temporal_presentation: false,
            committed_hits: bmux_tui::hit::HitMap::default(),
            committed_area: bmux_tui::geometry::Rect::new(0, 0, 0, 0),
            committed_layout: None,
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
        match dialog.handle_action(action) {
            super::permission_dialog::PermissionDialogOutcome::FocusChanged(label) => {
                self.chat
                    .app
                    .set_status(format!("permission choice: {label}"));
            }
            super::permission_dialog::PermissionDialogOutcome::Resolve(resolution) => {
                self.queue_permission_resolution(resolution);
            }
            super::permission_dialog::PermissionDialogOutcome::Ignored => {
                return super::invalidation::UiInvalidation::None;
            }
        }
        super::invalidation::UiInvalidation::Structural
    }

    fn queue_permission_resolution(
        &mut self,
        resolution: super::permission_dialog::PermissionDialogResolution,
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
        self.chat
            .start_effect(super::effects::TuiEffect::ResolvePermission {
                permission_id,
                approved: resolution.approved,
                remember: resolution.remember,
                apply_to_batch: resolution.apply_to_batch,
                batch_id,
            });
        self.loop_state.permission_dialog = None;
        self.chat
            .app
            .set_status(format!("resolving permission: {}", resolution.label));
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
                bcode_plugin_sdk::tui::PluginTuiAction::SubscribeWorkflowRuns => {
                    let updates = super::plugin_surface_host::subscribe_workflow_views(
                        self.loop_state.foreground_client(),
                        self.loop_state
                            .root_plugin_surface_invalidation()
                            .expect("plugin surface invalidation"),
                    );
                    self.loop_state.attach_root_plugin_surface_updates(updates);
                }
                bcode_plugin_sdk::tui::PluginTuiAction::InvokePluginCommand {
                    plugin_id,
                    command_id,
                    arguments,
                } => {
                    self.chat
                        .replace_effect(super::effects::TuiEffect::InvokePluginCommand {
                            plugin_id,
                            command_id,
                            arguments,
                            working_directory: self
                                .settings
                                .launch_working_directory()
                                .to_path_buf(),
                            session_id: self.chat.session_id,
                        });
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
            match self.loop_state.handle_session_fork_event(
                &self.chat,
                &event,
                self.settings.keymap(),
            ) {
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
        if self.loop_state.has_auth_pool_picker() {
            match self.loop_state.handle_auth_pool_picker_event(&event) {
                super::chat_loop::AuthPoolPickerRootOutcome::Continue => {}
                super::chat_loop::AuthPoolPickerRootOutcome::Cancel => {
                    self.chat
                        .app
                        .set_status("auth pool picker closed".to_owned());
                }
                super::chat_loop::AuthPoolPickerRootOutcome::Promote { pool, profile } => {
                    self.chat
                        .start_effect(super::effects::TuiEffect::SetAuthPoolPreference {
                            pool,
                            profile: Some(profile),
                        });
                    self.chat
                        .app
                        .set_status("updating preferred subscription…".to_owned());
                }
                super::chat_loop::AuthPoolPickerRootOutcome::Clear { pool } => {
                    self.chat
                        .start_effect(super::effects::TuiEffect::SetAuthPoolPreference {
                            pool,
                            profile: None,
                        });
                    self.chat
                        .app
                        .set_status("clearing subscription preference…".to_owned());
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
                if self
                    .loop_state
                    .handle_streaming_configurator_key(&mut self.chat, stroke)
                {
                    return super::invalidation::UiInvalidation::Structural;
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
                if self
                    .settings
                    .keymap()
                    .action_for_key(super::keymap::BmuxScope::Chat, stroke)
                    == Some(super::keymap::BmuxAction::SessionSearchOpen)
                {
                    self.loop_state.open_session_search(&mut self.chat);
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
                "session.search" => self.loop_state.open_session_search(&mut self.chat),
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
                "streaming.configure" => {
                    self.loop_state.open_streaming_configurator(&mut self.chat);
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
            super::keymap::BmuxAction::AppExit
                | super::keymap::BmuxAction::AppInterrupt
                | super::keymap::BmuxAction::InteractionFocusActive
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
        let now_system = std::time::SystemTime::now();
        let invalidation_requests = self.chat.app.invalidation_requests(now, now_system);
        let invalidation_at = invalidation_requests.iter().map(|request| request.at).min();
        let deadlines = [
            (RootTimer::Invalidations, invalidation_at),
            (
                RootTimer::ArtifactRetry,
                self.loop_state.next_artifact_retry_at(),
            ),
            (
                RootTimer::StreamingConfigurator,
                self.loop_state.streaming_configurator_deadline(now),
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

    fn select_presentation_damage(
        &self,
        invalidation: super::invalidation::UiInvalidation,
        temporal_damage: &[super::app::TemporalDamage],
    ) -> bmux_tui::damage::Damage {
        if invalidation == super::invalidation::UiInvalidation::Paint
            && let Some(layout) = self.committed_layout
        {
            let regions = temporal_damage.iter().filter_map(|damage| match damage {
                super::app::TemporalDamage::Composer => Some(layout.composer()),
                super::app::TemporalDamage::Status => Some(layout.status()),
                super::app::TemporalDamage::LatestBar => layout.latest_bar(),
                super::app::TemporalDamage::Transcript => Some(layout.body()),
                super::app::TemporalDamage::Full => None,
            });
            if !temporal_damage.contains(&super::app::TemporalDamage::Full) {
                let damage = bmux_tui::damage::Damage::regions(
                    regions,
                    self.committed_area,
                    bmux_tui::damage::DamagePolicy {
                        max_regions: 64,
                        max_area_percent: 100,
                    },
                );
                if !damage.is_none() {
                    return damage;
                }
            }
        }
        bmux_tui::damage::Damage::Full
    }

    fn handle_timer(
        &mut self,
        timer: &bmux_tui_runtime::TimerId,
    ) -> (
        super::invalidation::UiInvalidation,
        Vec<super::app::TemporalDamage>,
    ) {
        self.scheduled_deadlines.remove(timer);
        let now = Instant::now();
        let invalidation = match timer.as_str() {
            "bcode.invalidations" => {
                let now_system = std::time::SystemTime::now();
                let requests = self.chat.app.invalidation_requests(now, now_system);
                let due = requests
                    .iter()
                    .filter(|request| request.at <= now)
                    .map(|request| request.key.clone())
                    .collect::<Vec<_>>();
                if due.is_empty() {
                    let overdue_slack = Duration::from_millis(2);
                    if let Some(request) = requests
                        .iter()
                        .min_by_key(|request| request.at)
                        .filter(|request| request.at <= now + overdue_slack)
                    {
                        return self.chat.app.handle_invalidations_with_damage(
                            std::slice::from_ref(&request.key),
                            now,
                        );
                    }
                }
                return self.chat.app.handle_invalidations_with_damage(&due, now);
            }
            "bcode.artifact_retry" => {
                self.loop_state.start_due_artifact_fetches(now);
                super::invalidation::UiInvalidation::None
            }
            "bcode.streaming_configurator" => {
                if self.loop_state.advance_streaming_configurator(now) {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            "bcode.streaming_presentation" => {
                if self.chat.app.advance_streaming_presentation(now) {
                    return (
                        super::invalidation::UiInvalidation::Paint,
                        vec![super::app::TemporalDamage::Transcript],
                    );
                }
                super::invalidation::UiInvalidation::None
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
                    return (super::invalidation::UiInvalidation::None, Vec::new());
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
        };
        (invalidation, Vec::new())
    }

    #[allow(dead_code)]
    pub fn shutdown_owned_work(&mut self) {
        self.loop_state.abort_all_effects();
        if let Some(event_task) = self.chat.event_task.take() {
            event_task.abort();
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
        let frame_interval = program.settings.bmux_runtime_config().frame_interval;
        let damage = std::mem::replace(
            &mut program.presentation_damage,
            bmux_tui::damage::Damage::None,
        );
        let fast_temporal_presentation = std::mem::take(&mut program.fast_temporal_presentation);
        let (draw_stats, layout) = super::chat_loop::draw_chat_frame(
            self.terminal,
            &mut program.chat,
            &mut program.loop_state,
            Duration::ZERO,
            frame_interval,
            damage,
            fast_temporal_presentation,
            program.committed_layout,
        )?;
        program.committed_hits = self.terminal.hits().clone();
        program.committed_area = self.terminal.area();
        program.committed_layout = layout;
        Ok(bmux_tui_runtime::PresentReport {
            changed_cells: draw_stats.changed_cells,
            full_repaint: draw_stats.full_repaint,
        })
    }
}

impl bmux_tui_runtime::Program for BcodeRuntimeModel {
    type Message = BcodeRuntimeMessage;
    type Error = TuiError;

    fn presentation_committed(
        &mut self,
        _report: bmux_tui_runtime::PresentReport,
    ) -> bmux_tui_runtime::Update<Self::Message> {
        self.invalidation = super::invalidation::UiInvalidation::None;
        self.last_presented_at = Some(Instant::now());
        self.loop_state.mark_presentation_committed();
        if self
            .loop_state
            .apply_deferred_session_stream_updates(&mut self.chat)
        {
            self.invalidation = super::invalidation::UiInvalidation::Structural;
            self.presentation_damage = bmux_tui::damage::Damage::Full;
            self.fast_temporal_presentation = false;
            bmux_tui_runtime::Update::redraw()
        } else {
            bmux_tui_runtime::Update::none()
        }
    }

    #[allow(clippy::too_many_lines)]
    fn update(
        &mut self,
        event: bmux_tui_runtime::RuntimeEvent<Self::Message>,
    ) -> Result<bmux_tui_runtime::Update<Self::Message>, Self::Error> {
        let mut temporal_damage = Vec::new();
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
                            super::keymap::BmuxAction::InteractionFocusActive => {
                                if self.chat.app.tui_config().interactions.placement
                                    == bcode_config::TuiInteractionPlacement::Transcript
                                    && let Some(interaction_id) =
                                        self.loop_state.active_interactive_surface_id()
                                {
                                    let _changed = self
                                        .chat
                                        .app
                                        .focus_interaction_in_transcript(interaction_id);
                                }
                            }
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
                        .active_interactive_surface_geometry()
                        .is_some_and(|geometry| geometry.destination.contains(mouse.position)),
                    _ => self
                        .loop_state
                        .routes_non_mouse_event_to_interactive_surface(
                            self.chat.app.tui_config().interactions,
                        ),
                };
                if route_to_surface {
                    match self.loop_state.handle_interactive_surface_event(&event) {
                        super::interactive_surface::InteractiveSurfaceEventOutcome::Ignored => {}
                        super::interactive_surface::InteractiveSurfaceEventOutcome::Consumed => {
                            return Ok(bmux_tui_runtime::Update::redraw());
                        }
                        super::interactive_surface::InteractiveSurfaceEventOutcome::Resolved(
                            resolution,
                        ) => {
                            let Some(interaction_id) = self
                                .loop_state
                                .active_interactive_surface_id()
                                .map(ToOwned::to_owned)
                            else {
                                return Ok(bmux_tui_runtime::Update::none());
                            };
                            let command = self
                                .interactive_surface_resolution_command(interaction_id, resolution);
                            return Ok(bmux_tui_runtime::Update::redraw().with_command(command));
                        }
                    }
                }
                self.route_terminal_event(event)
            }
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::Invalidations(keys)) => {
                self.chat.app.handle_invalidations(&keys, Instant::now())
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::StreamingConfiguratorDue,
            ) => {
                if self
                    .loop_state
                    .advance_streaming_configurator(Instant::now())
                {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
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
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::SubscribeWorkflowRuns) => {
                        let updates = super::plugin_surface_host::subscribe_workflow_views(
                            self.loop_state.foreground_client(),
                            self.loop_state
                                .root_plugin_surface_invalidation()
                                .expect("plugin surface invalidation"),
                        );
                        self.loop_state.attach_root_plugin_surface_updates(updates);
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::InvokePluginCommand {
                        plugin_id,
                        command_id,
                        arguments,
                    }) => {
                        self.chat
                            .replace_effect(super::effects::TuiEffect::InvokePluginCommand {
                                plugin_id,
                                command_id,
                                arguments,
                                working_directory: self
                                    .settings
                                    .launch_working_directory()
                                    .to_path_buf(),
                                session_id: self.chat.session_id,
                            });
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
            ) => match self.loop_state.complete_interactive_surface_open(result) {
                super::chat_loop::InteractiveSurfaceOpenCompletion::Opened => {
                    super::invalidation::UiInvalidation::Structural
                }
                super::chat_loop::InteractiveSurfaceOpenCompletion::Stale => {
                    super::invalidation::UiInvalidation::None
                }
                super::chat_loop::InteractiveSurfaceOpenCompletion::Failed => {
                    self.chat
                        .app
                        .set_status("Interactive request unavailable; retrying".to_owned());
                    super::invalidation::UiInvalidation::Paint
                }
            },
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
            bmux_tui_runtime::RuntimeEvent::Timer(timer) => {
                let (damage, due_temporal_damage) = self.handle_timer(&timer);
                temporal_damage = due_temporal_damage;
                damage
            }
        };
        self.invalidation = self.invalidation.merge(damage);
        let route_invalidation = self.synchronize_screen();
        self.invalidation = self.invalidation.merge(route_invalidation);
        self.draft_autosave.observe(&self.chat, Instant::now());
        let housekeeping = self
            .loop_state
            .prepare_runtime_work(&mut self.chat, self.committed_area);
        self.invalidation = self.invalidation.merge(housekeeping);
        self.presentation_damage =
            self.select_presentation_damage(self.invalidation, &temporal_damage);
        self.fast_temporal_presentation = self.invalidation
            == super::invalidation::UiInvalidation::Paint
            && !temporal_damage.is_empty()
            && !temporal_damage.contains(&super::app::TemporalDamage::Full)
            && !temporal_damage.contains(&super::app::TemporalDamage::Transcript)
            && !self.presentation_damage.is_full();
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

    fn root_test_model() -> super::BcodeRuntimeModel {
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
        super::BcodeRuntimeModel::new(chat, settings, loop_state)
    }

    fn root_filesystem_plugin_host() -> bcode_plugin::PluginHost {
        let bundled = [bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/filesystem-plugin/bcode-plugin.toml"),
            bcode_filesystem_plugin::static_plugin(),
        )];
        let selected = bcode_plugin::filter_selected_static_plugins(
            &bundled,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("static filesystem plugin manifest should parse");
        bcode_plugin::PluginHost::load_static_plugins(&selected)
            .expect("static filesystem plugin should load")
    }

    #[derive(Debug)]
    struct RecordedRootFrame {
        text: String,
        invocation_primary_items: usize,
        invocation_item_ids: Vec<bcode_session_view_models::TranscriptViewItemId>,
    }

    struct RecordingRootPresenter {
        frames: Vec<RecordedRootFrame>,
    }

    impl RecordingRootPresenter {
        const fn new() -> Self {
            Self { frames: Vec::new() }
        }
    }

    impl bmux_tui_runtime::Presenter<super::BcodeRuntimeModel> for RecordingRootPresenter {
        type Error = super::TuiError;

        fn reset(&mut self, _reason: bmux_tui_runtime::ResetReason) {}

        fn present(
            &mut self,
            program: &mut super::BcodeRuntimeModel,
        ) -> Result<bmux_tui_runtime::PresentReport, Self::Error> {
            let area = bmux_tui::geometry::Rect::new(0, 0, 100, 40);
            let mut bytes = Vec::new();
            let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, area);
            program.presentation_damage = bmux_tui::damage::Damage::Full;
            let report = bmux_tui_runtime::Presenter::present(
                &mut super::BcodeRuntimePresenter::new(&mut terminal),
                program,
            )?;
            let text = terminal
                .retained_buffer()
                .map_or_else(String::new, |buffer| {
                    (0..buffer.area().height)
                        .filter_map(|row| buffer.row_symbols(row))
                        .collect::<Vec<_>>()
                        .join("\n")
                });
            let invocation_item_ids = program
                .chat
                .app
                .session_view_snapshot()
                .transcript
                .items
                .iter()
                .filter(|item| {
                    matches!(
                        item.kind,
                        bcode_session_view_models::TranscriptViewItemKind::ToolRequestDraft { .. }
                            | bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { .. }
                    )
                })
                .map(|item| item.id.clone())
                .collect::<Vec<_>>();
            let invocation_primary_items = invocation_item_ids.len();
            self.frames.push(RecordedRootFrame {
                text,
                invocation_primary_items,
                invocation_item_ids,
            });
            Ok(report)
        }
    }

    #[tokio::test]
    async fn status_partial_presentation_matches_full_production_presenter() {
        use bmux_tui_runtime::Presenter;

        let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
        let mut partial_model = root_test_model();
        let mut full_model = root_test_model();
        let mut partial_bytes = Vec::new();
        let mut full_bytes = Vec::new();
        let mut partial_terminal = bmux_tui::terminal::Terminal::new(&mut partial_bytes, area);
        let mut full_terminal = bmux_tui::terminal::Terminal::new(&mut full_bytes, area);
        {
            super::BcodeRuntimePresenter::new(&mut partial_terminal)
                .present(&mut partial_model)
                .expect("initial partial frame");
            super::BcodeRuntimePresenter::new(&mut full_terminal)
                .present(&mut full_model)
                .expect("initial full frame");
        }

        for model in [&mut partial_model, &mut full_model] {
            model.chat.app.set_status("timer advanced".to_owned());
            model.invalidation = super::super::invalidation::UiInvalidation::Paint;
        }
        partial_model.presentation_damage = partial_model.select_presentation_damage(
            super::super::invalidation::UiInvalidation::Paint,
            &[super::super::app::TemporalDamage::Status],
        );
        partial_model.fast_temporal_presentation = true;
        full_model.presentation_damage = bmux_tui::damage::Damage::Full;

        super::BcodeRuntimePresenter::new(&mut partial_terminal)
            .present(&mut partial_model)
            .expect("partial status frame");
        super::BcodeRuntimePresenter::new(&mut full_terminal)
            .present(&mut full_model)
            .expect("full status frame");

        assert_eq!(
            partial_terminal.retained_buffer(),
            full_terminal.retained_buffer()
        );
        assert_eq!(partial_terminal.cursor(), full_terminal.cursor());
        assert_eq!(partial_terminal.image_scene(), full_terminal.image_scene());
    }

    #[tokio::test]
    async fn cursor_partial_presentation_matches_full_production_presenter() {
        use bmux_tui_runtime::Presenter;

        let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
        let mut partial_model = root_test_model();
        let mut full_model = root_test_model();
        let mut partial_bytes = Vec::new();
        let mut full_bytes = Vec::new();
        let mut partial_terminal = bmux_tui::terminal::Terminal::new(&mut partial_bytes, area);
        let mut full_terminal = bmux_tui::terminal::Terminal::new(&mut full_bytes, area);

        {
            let mut presenter = super::BcodeRuntimePresenter::new(&mut partial_terminal);
            presenter
                .present(&mut partial_model)
                .expect("initial partial-side frame presents");
        }
        {
            let mut presenter = super::BcodeRuntimePresenter::new(&mut full_terminal);
            presenter
                .present(&mut full_model)
                .expect("initial full-side frame presents");
        }

        let now = std::time::Instant::now() + std::time::Duration::from_secs(1);
        for model in [&mut partial_model, &mut full_model] {
            let cursor_key = model
                .chat
                .app
                .invalidation_requests(now, std::time::SystemTime::now())
                .into_iter()
                .next()
                .expect("cursor invalidation is always scheduled")
                .key;
            assert_eq!(
                model.chat.app.handle_invalidations(&[cursor_key], now),
                super::super::invalidation::UiInvalidation::Paint
            );
            model.invalidation = super::super::invalidation::UiInvalidation::Paint;
        }
        let temporal_damage = [super::super::app::TemporalDamage::Composer];
        partial_model.presentation_damage = partial_model.select_presentation_damage(
            super::super::invalidation::UiInvalidation::Paint,
            &temporal_damage,
        );
        partial_model.fast_temporal_presentation = true;
        full_model.presentation_damage = bmux_tui::damage::Damage::Full;

        {
            let mut presenter = super::BcodeRuntimePresenter::new(&mut partial_terminal);
            presenter
                .present(&mut partial_model)
                .expect("cursor partial frame presents");
        }
        {
            let mut presenter = super::BcodeRuntimePresenter::new(&mut full_terminal);
            presenter
                .present(&mut full_model)
                .expect("cursor full frame presents");
        }

        assert_eq!(
            partial_terminal.retained_buffer(),
            full_terminal.retained_buffer()
        );
        assert_eq!(partial_terminal.cursor(), full_terminal.cursor());
        assert_eq!(partial_terminal.image_scene(), full_terminal.image_scene());
        for point in [
            bmux_tui::geometry::Point::new(0, 0),
            bmux_tui::geometry::Point::new(2, 22),
            bmux_tui::geometry::Point::new(79, 23),
        ] {
            assert_eq!(
                partial_terminal
                    .hits()
                    .hit_test(point)
                    .map(|hit| hit.id().as_str().to_owned()),
                full_terminal
                    .hits()
                    .hit_test(point)
                    .map(|hit| hit.id().as_str().to_owned())
            );
        }
    }

    #[tokio::test]
    async fn presentation_damage_localizes_stable_temporal_regions() {
        use bmux_tui_runtime::Presenter;

        let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
        let mut model = root_test_model();
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, area);
        {
            let mut presenter = super::BcodeRuntimePresenter::new(&mut terminal);
            presenter
                .present(&mut model)
                .expect("initial frame presents");
        }

        for temporal_damage in [
            super::super::app::TemporalDamage::Composer,
            super::super::app::TemporalDamage::Status,
            super::super::app::TemporalDamage::Transcript,
        ] {
            assert!(matches!(
                model.select_presentation_damage(
                    super::super::invalidation::UiInvalidation::Paint,
                    &[temporal_damage],
                ),
                bmux_tui::damage::Damage::Regions(_)
            ));
        }
        assert!(
            model
                .select_presentation_damage(
                    super::super::invalidation::UiInvalidation::Paint,
                    &[super::super::app::TemporalDamage::Full],
                )
                .is_full()
        );
        for invalidation in [
            super::super::invalidation::UiInvalidation::Items,
            super::super::invalidation::UiInvalidation::Structural,
            super::super::invalidation::UiInvalidation::Full,
        ] {
            assert!(
                model
                    .select_presentation_damage(
                        invalidation,
                        &[super::super::app::TemporalDamage::Status],
                    )
                    .is_full()
            );
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

    fn root_test_chat_with_input_history(
        input_history: &[bcode_session_models::SessionInputHistoryEntry],
    ) -> super::super::session_flow::ActiveChat {
        let (event_sender, event_receiver) = super::super::history_flow::session_stream_channel();
        super::super::session_flow::ActiveChat {
            app: super::super::app::BmuxApp::new_with_history(None, &[], input_history, false),
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

    fn root_test_chat() -> super::super::session_flow::ActiveChat {
        root_test_chat_with_input_history(&[])
    }

    async fn question_surface_for_root_test(
        keymap: &super::super::keymap::BmuxKeyMap,
    ) -> super::super::interactive_surface::InteractiveSurfaceState {
        let plugin = bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/question-plugin/bcode-plugin.toml"),
            bcode_question_plugin::static_plugin(),
        );
        let runtime = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection::all_enabled(),
            &[plugin],
        )
        .expect("question plugin runtime");
        super::super::interactive_surface::InteractiveSurfaceState::open(
            &runtime,
            "question-root-test",
            "bcode.question.inline",
            &serde_json::json!({
                "questions": [{
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
                }]
            })
            .to_string(),
            keymap,
        )
        .await
        .expect("question surface")
    }

    #[tokio::test]
    async fn failed_delivery_preserves_surface_and_clears_only_pending_resolution() {
        let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let mut loop_state = super::super::chat_loop::ChatLoopState::new(&client, &passive, false);
        loop_state.install_interactive_surface_for_test(
            question_surface_for_root_test(settings.keymap()).await,
        );
        assert!(matches!(
            loop_state.handle_interactive_surface_event(&bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke {
                    key: bmux_keyboard::KeyCode::Enter,
                    modifiers: bmux_keyboard::Modifiers::NONE,
                },
            )),
            super::super::interactive_surface::InteractiveSurfaceEventOutcome::Consumed
        ));
        let tab = bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
            key: bmux_keyboard::KeyCode::Tab,
            modifiers: bmux_keyboard::Modifiers::NONE,
        });
        let enter = bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
            key: bmux_keyboard::KeyCode::Enter,
            modifiers: bmux_keyboard::Modifiers::NONE,
        });
        let _ = loop_state.handle_interactive_surface_event(&tab);
        let _ = loop_state.handle_interactive_surface_event(&tab);
        assert!(matches!(
            loop_state.handle_interactive_surface_event(&enter),
            super::super::interactive_surface::InteractiveSurfaceEventOutcome::Resolved(_)
        ));
        assert!(loop_state.latched_surface_outcome_for_test().is_some());

        loop_state.complete_interactive_surface_resolution(false);
        assert_eq!(
            loop_state.active_interactive_surface_id(),
            Some("question-root-test")
        );
        assert!(loop_state.latched_surface_outcome_for_test().is_none());

        let _ = loop_state.handle_interactive_surface_event(&enter);
        assert!(loop_state.latched_surface_outcome_for_test().is_some());
        loop_state.complete_interactive_surface_resolution(true);
        assert!(loop_state.active_interactive_surface_id().is_none());
    }

    #[tokio::test]
    async fn hidden_transcript_geometry_applies_retain_and_suspend_routing() {
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let mut loop_state = super::super::chat_loop::ChatLoopState::new(&client, &passive, false);
        loop_state.set_interactive_surface_geometry_for_test(Some(
            super::super::chat_loop::InteractiveSurfaceGeometry {
                placement: super::super::chat_loop::InteractiveSurfacePlacement::Transcript,
                logical_height: 20,
                visible_logical_offset: 0,
                destination: bmux_tui::geometry::Rect::new(0, 0, 80, 0),
            },
        ));
        let mut config = bcode_config::TuiInteractionConfig::default();
        assert!(loop_state.routes_non_mouse_event_to_interactive_surface(config));
        config.offscreen_focus = bcode_config::TuiInteractionOffscreenFocus::Suspend;
        assert!(!loop_state.routes_non_mouse_event_to_interactive_surface(config));

        loop_state.set_interactive_surface_geometry_for_test(Some(
            super::super::chat_loop::InteractiveSurfaceGeometry {
                placement: super::super::chat_loop::InteractiveSurfacePlacement::Pinned,
                logical_height: 20,
                visible_logical_offset: 0,
                destination: bmux_tui::geometry::Rect::new(0, 0, 80, 0),
            },
        ));
        assert!(loop_state.routes_non_mouse_event_to_interactive_surface(config));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // One production-route scenario exercises both clipping boundaries and the complete pointer gesture.
    async fn committed_geometry_routes_top_and_bottom_clipped_pointer_input() {
        use std::sync::{Arc, Mutex};

        struct RecordingSurface(
            Arc<Mutex<Vec<(bmux_tui::event::MouseEventKind, bmux_tui::geometry::Point)>>>,
        );

        impl bcode_plugin_sdk::tui::PluginTuiSurface for RecordingSurface {
            fn id(&self) -> &'static str {
                "recording"
            }

            fn title(&self) -> &'static str {
                "recording"
            }

            fn render(
                &mut self,
                _area: bmux_tui::geometry::Rect,
                _frame: &mut bmux_tui::frame::Frame<'_>,
            ) {
            }

            fn handle_event(
                &mut self,
                event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                if let bmux_tui::event::Event::Mouse(mouse) = event {
                    self.0
                        .lock()
                        .expect("events")
                        .push((mouse.kind, mouse.position));
                    bcode_plugin_sdk::tui::PluginTuiAction::Redraw
                } else {
                    bcode_plugin_sdk::tui::PluginTuiAction::None
                }
            }
        }

        let client = bcode_client::BcodeClient::default_endpoint();
        let passive = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let mut loop_state = super::super::chat_loop::ChatLoopState::new(&client, &passive, false);
        let events = Arc::new(Mutex::new(Vec::new()));
        loop_state.install_interactive_surface_for_test(
            super::super::interactive_surface::InteractiveSurfaceState::from_surface_for_test(
                "interaction",
                Box::new(RecordingSurface(Arc::clone(&events))),
                &super::super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
            ),
        );

        for (visible_logical_offset, destination, screen_point, expected_logical_point) in [
            (
                0,
                bmux_tui::geometry::Rect::new(5, 7, 20, 4),
                bmux_tui::geometry::Point::new(8, 10),
                bmux_tui::geometry::Point::new(3, 3),
            ),
            (
                26,
                bmux_tui::geometry::Rect::new(5, 7, 20, 4),
                bmux_tui::geometry::Point::new(8, 9),
                bmux_tui::geometry::Point::new(3, 28),
            ),
        ] {
            loop_state.set_interactive_surface_geometry_for_test(Some(
                super::super::chat_loop::InteractiveSurfaceGeometry {
                    placement: super::super::chat_loop::InteractiveSurfacePlacement::Transcript,
                    logical_height: 30,
                    visible_logical_offset,
                    destination,
                },
            ));

            for kind in [
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
            ] {
                let event = bmux_tui::event::Event::Mouse(bmux_tui::event::MouseEvent::new(
                    kind,
                    screen_point,
                ));
                assert!(matches!(
                    loop_state.handle_interactive_surface_event(&event),
                    super::super::interactive_surface::InteractiveSurfaceEventOutcome::Consumed
                ));
            }

            let outside = bmux_tui::event::Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(destination.x.saturating_sub(1), screen_point.y),
            ));
            assert!(matches!(
                loop_state.handle_interactive_surface_event(&outside),
                super::super::interactive_surface::InteractiveSurfaceEventOutcome::Ignored
            ));

            let recorded = events.lock().expect("events");
            let recent = &recorded[recorded.len().saturating_sub(3)..];
            assert!(
                recent
                    .iter()
                    .all(|(_, point)| *point == expected_logical_point)
            );
            drop(recorded);
        }

        assert_eq!(events.lock().expect("events").len(), 6);
        loop_state.set_interactive_surface_geometry_for_test(None);
        let event = bmux_tui::event::Event::Mouse(bmux_tui::event::MouseEvent::new(
            bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
            bmux_tui::geometry::Point::new(8, 9),
        ));
        assert!(matches!(
            loop_state.handle_interactive_surface_event(&event),
            super::super::interactive_surface::InteractiveSurfaceEventOutcome::Ignored
        ));
    }

    #[tokio::test]
    async fn interaction_host_keys_keep_application_and_transcript_precedence() {
        let model = root_test_model();
        let key = |key, modifiers| bmux_keyboard::KeyStroke { key, modifiers };
        let ctrl = bmux_keyboard::Modifiers {
            ctrl: true,
            ..bmux_keyboard::Modifiers::NONE
        };

        for (stroke, expected) in [
            (
                key(
                    bmux_keyboard::KeyCode::PageUp,
                    bmux_keyboard::Modifiers::NONE,
                ),
                super::super::keymap::BmuxAction::TranscriptPageUp,
            ),
            (
                key(
                    bmux_keyboard::KeyCode::PageDown,
                    bmux_keyboard::Modifiers::NONE,
                ),
                super::super::keymap::BmuxAction::TranscriptPageDown,
            ),
            (
                key(bmux_keyboard::KeyCode::Home, ctrl),
                super::super::keymap::BmuxAction::TranscriptTop,
            ),
            (
                key(bmux_keyboard::KeyCode::End, ctrl),
                super::super::keymap::BmuxAction::TranscriptBottom,
            ),
            (
                key(bmux_keyboard::KeyCode::Up, ctrl),
                super::super::keymap::BmuxAction::TranscriptLineUp,
            ),
            (
                key(bmux_keyboard::KeyCode::Down, ctrl),
                super::super::keymap::BmuxAction::TranscriptLineDown,
            ),
            (
                key(bmux_keyboard::KeyCode::Char('d'), ctrl),
                super::super::keymap::BmuxAction::AppExit,
            ),
        ] {
            assert_eq!(
                model.root_interactive_surface_host_key(stroke),
                Some(expected)
            );
        }
        assert_eq!(
            model.root_interactive_surface_host_key(key(
                bmux_keyboard::KeyCode::Up,
                bmux_keyboard::Modifiers::NONE,
            )),
            None
        );
    }

    #[tokio::test]
    async fn stale_surface_open_completion_does_not_report_a_retry_failure() {
        let mut model = root_test_model();
        model.chat.app.set_status("steady".to_owned());
        let surface = question_surface_for_root_test(model.settings.keymap()).await;

        let _update = bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Message(
                super::BcodeRuntimeMessage::InteractiveSurfaceOpened(Ok(surface)),
            ),
        )
        .expect("stale open completion is handled");

        assert_eq!(model.chat.app.status(), "steady");
        assert!(!model.loop_state.has_interactive_surface());
    }

    #[tokio::test]
    async fn question_navigation_through_root_is_consumed_once_without_history_fallthrough() {
        let history = [bcode_session_models::SessionInputHistoryEntry {
            timestamp_ms: 1,
            sequence: 1,
            text: "previous prompt".to_owned(),
        }];
        let mut chat = root_test_chat_with_input_history(&history);
        chat.app.replace_composer_with("draft prompt");
        let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let mut loop_state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        loop_state.install_interactive_surface_for_test(
            question_surface_for_root_test(settings.keymap()).await,
        );
        let mut model = super::BcodeRuntimeModel::new(chat, settings, loop_state);

        let down = bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
            key: bmux_keyboard::KeyCode::Down,
            modifiers: bmux_keyboard::Modifiers::NONE,
        });
        bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Terminal(down),
        )
        .expect("root update handles question navigation");

        assert_eq!(model.chat.app.composer().text(), "draft prompt");
        assert!(!model.chat.app.input_history_navigation_active());
        assert_eq!(
            model
                .loop_state
                .active_interactive_surface_native_id_for_test(),
            Some("question-inline")
        );

        let key = |key| {
            bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
                key,
                modifiers: bmux_keyboard::Modifiers::NONE,
            })
        };
        assert!(matches!(
            model
                .loop_state
                .handle_interactive_surface_event(&key(bmux_keyboard::KeyCode::Enter)),
            super::super::interactive_surface::InteractiveSurfaceEventOutcome::Consumed
        ));
        assert!(matches!(
            model
                .loop_state
                .handle_interactive_surface_event(&key(bmux_keyboard::KeyCode::Tab)),
            super::super::interactive_surface::InteractiveSurfaceEventOutcome::Consumed
        ));
        let outcome = model
            .loop_state
            .handle_interactive_surface_event(&key(bmux_keyboard::KeyCode::Enter));
        let super::super::interactive_surface::InteractiveSurfaceEventOutcome::Resolved(
            bcode_session_models::ToolExchangeResolution::Responded { payload },
        ) = outcome
        else {
            panic!("question should submit after selecting the second option");
        };
        assert_eq!(payload["questions"][0]["selected"][0], "two");
    }

    #[tokio::test]
    async fn consumed_interaction_navigation_does_not_reach_composer_history() {
        struct ConsumingSurface;

        impl bcode_plugin_sdk::tui::PluginTuiSurface for ConsumingSurface {
            fn id(&self) -> &'static str {
                "consuming-test"
            }

            fn title(&self) -> &'static str {
                "Consuming test"
            }

            fn render(
                &mut self,
                _area: bmux_tui::geometry::Rect,
                _frame: &mut bmux_tui::frame::Frame<'_>,
            ) {
            }

            fn handle_event(
                &mut self,
                event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                if matches!(
                    event,
                    bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
                        key: bmux_keyboard::KeyCode::Up | bmux_keyboard::KeyCode::Down,
                        ..
                    })
                ) {
                    bcode_plugin_sdk::tui::PluginTuiAction::Redraw
                } else {
                    bcode_plugin_sdk::tui::PluginTuiAction::None
                }
            }
        }

        let history = [bcode_session_models::SessionInputHistoryEntry {
            timestamp_ms: 1,
            sequence: 1,
            text: "previous prompt".to_owned(),
        }];
        let mut chat = root_test_chat_with_input_history(&history);
        chat.app.replace_composer_with("draft prompt");
        let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let mut loop_state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        loop_state.install_interactive_surface_for_test(
            super::super::interactive_surface::InteractiveSurfaceState::from_surface_for_test(
                "test-interaction",
                Box::new(ConsumingSurface),
                settings.keymap(),
            ),
        );
        let mut model = super::BcodeRuntimeModel::new(chat, settings, loop_state);

        let _update = bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke {
                    key: bmux_keyboard::KeyCode::Up,
                    modifiers: bmux_keyboard::Modifiers::NONE,
                },
            )),
        )
        .expect("root update handles consumed interaction input");

        assert_eq!(model.chat.app.composer().text(), "draft prompt");
        assert!(!model.chat.app.input_history_navigation_active());
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
    #[allow(clippy::too_many_lines)]
    async fn managed_runtime_commits_progressive_filesystem_write_frames_without_external_wakeup() {
        let session_id = bcode_session_models::SessionId::new();
        let mut model = root_test_model();
        model.chat.session_id = Some(session_id);
        model.chat.app =
            super::super::app::BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        model
            .chat
            .app
            .set_plugin_host(std::sync::Arc::new(root_filesystem_plugin_host()));
        let config = bmux_tui_runtime::RuntimeConfig {
            frame_interval: None,
            ..bmux_tui_runtime::RuntimeConfig::default()
        };
        let (runtime, handle) =
            bmux_tui_runtime::Runtime::new(model, RecordingRootPresenter::new(), config);
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
        let durable = |sequence, kind| {
            super::super::history_flow::SessionStreamUpdate::Event(Box::new(
                bcode_ipc::Event::Session(bcode_session_models::SessionEvent {
                    schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    sequence,
                    timestamp_ms: sequence,
                    session_id,
                    provenance: None,
                    kind,
                }),
            ))
        };
        for update in [
            draft(1, r#"{"path":"src/lib.rs","contents":"first"}"#),
            draft(2, r#"{"path":"src/lib.rs","contents":"first second"}"#),
            durable(
                1,
                bcode_session_models::SessionEventKind::ToolCallRequested {
                    tool_call_id: "call-write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    tool_name: "filesystem.write".to_owned(),
                    arguments_json: r#"{"path":"src/lib.rs","contents":"first second"}"#.to_owned(),
                    working_directory: None,
                },
            ),
            durable(
                2,
                bcode_session_models::SessionEventKind::ToolInvocationResultRecorded {
                    record: bcode_session_models::ToolInvocationResultRecord {
                        invocation_id: "call-write".to_owned(),
                        model_output: "wrote 12 bytes".to_owned(),
                        is_error: false,
                        presentation: None,
                        result: None,
                        content: Vec::new(),
                    },
                },
            ),
        ] {
            assert!(
                handle
                    .try_send(super::BcodeRuntimeMessage::SessionStream(Box::new(update)))
                    .is_ok(),
                "session update fits"
            );
        }
        let runtime_task = tokio::spawn(runtime.run());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while handle.stats().frames_presented < 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both draft and canonical handoff frames commit without external wakeup");
        assert_eq!(handle.stats().frames_presented, 4);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            handle.stats().frames_presented,
            4,
            "runtime remains idle after the handoff queue drains"
        );
        handle
            .try_send_terminal(bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke::with_modifiers(
                    bmux_keyboard::KeyCode::Char('d'),
                    bmux_keyboard::Modifiers {
                        ctrl: true,
                        ..bmux_keyboard::Modifiers::NONE
                    },
                ),
            ))
            .expect("exit input fits");

        let output = tokio::time::timeout(std::time::Duration::from_secs(2), runtime_task)
            .await
            .expect("managed runtime exits")
            .expect("runtime task joins")
            .unwrap_or_else(|_| panic!("managed runtime succeeds"));
        let first = output
            .presenter
            .frames
            .iter()
            .position(|frame| frame.text.contains("first") && !frame.text.contains("first second"))
            .expect("first draft committed");
        let second = output
            .presenter
            .frames
            .iter()
            .position(|frame| frame.text.contains("first second"))
            .expect("second draft committed");
        let result = output
            .presenter
            .frames
            .iter()
            .position(|frame| frame.text.contains("wrote 12 bytes"))
            .expect("canonical result committed");
        assert!(
            first < second && second < result,
            "frames: {:?}",
            output.presenter.frames
        );
        assert_eq!(
            output.presenter.frames[result]
                .text
                .matches("wrote 12 bytes")
                .count(),
            1
        );
        assert!(!output.presenter.frames[result].text.contains("assembling"));
        assert!(
            output
                .presenter
                .frames
                .iter()
                .all(|frame| frame.invocation_primary_items <= 1),
            "more than one primary invocation item was rendered"
        );
        let invocation_ids = output
            .presenter
            .frames
            .iter()
            .flat_map(|frame| frame.invocation_item_ids.iter())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            invocation_ids.len(),
            1,
            "invocation identity changed across frames"
        );
        assert_eq!(output.stats.frames_presented, 5);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn managed_runtime_commits_progressive_filesystem_edit_frames_without_external_wakeup() {
        let session_id = bcode_session_models::SessionId::new();
        let mut model = root_test_model();
        model.chat.session_id = Some(session_id);
        model.chat.app =
            super::super::app::BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        model
            .chat
            .app
            .set_plugin_host(std::sync::Arc::new(root_filesystem_plugin_host()));
        let (runtime, handle) = bmux_tui_runtime::Runtime::new(
            model,
            RecordingRootPresenter::new(),
            bmux_tui_runtime::RuntimeConfig {
                frame_interval: None,
                ..bmux_tui_runtime::RuntimeConfig::default()
            },
        );
        let draft = |revision, operation, argument_bytes| {
            super::super::history_flow::SessionStreamUpdate::Event(Box::new(
                bcode_ipc::Event::SessionLive(bcode_session_models::SessionLiveEvent {
                    session_id,
                    kind: bcode_session_models::SessionLiveEventKind::ToolRequestDraft {
                        event: bcode_session_models::ToolRequestDraftEvent {
                            output_position: None,
                            turn_id: "turn-edit".to_owned(),
                            tool_call_id: "call-edit".to_owned(),
                            tool_name: "filesystem.edit".to_owned(),
                            producer_plugin_id: Some("bcode.filesystem".to_owned()),
                            schema: "bcode.filesystem.request-draft.edit".to_owned(),
                            schema_version: 1,
                            placement: bcode_session_models::ToolContributionPlacement::Request,
                            generation: 1,
                            revision,
                            operation,
                            argument_bytes,
                            truncated: false,
                        },
                    },
                }),
            ))
        };
        let durable = |sequence, kind| {
            super::super::history_flow::SessionStreamUpdate::Event(Box::new(
                bcode_ipc::Event::Session(bcode_session_models::SessionEvent {
                    schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    sequence,
                    timestamp_ms: sequence,
                    session_id,
                    provenance: None,
                    kind,
                }),
            ))
        };
        let fragments = [
            r#"{"path":"src/lib.rs","#,
            r#""old_text":"before""#,
            r#", "new_text":"aft"#,
            r#"er two"}"#,
        ];
        let mut offset: usize = 0;
        let mut revision: u64 = 1;
        let mut updates = Vec::new();
        updates.push(draft(
            revision,
            bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                start_offset: 0,
                text: String::new(),
            },
            0,
        ));
        for fragment in fragments {
            revision = revision.saturating_add(1);
            offset = offset.saturating_add(fragment.len());
            updates.push(draft(
                revision,
                bcode_session_models::ToolRequestDraftOperation::Append {
                    offset: offset.saturating_sub(fragment.len()),
                    text: fragment.to_owned(),
                },
                offset,
            ));
        }
        updates.extend([
            durable(
                1,
                bcode_session_models::SessionEventKind::ToolCallRequested {
                    tool_call_id: "call-edit".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    tool_name: "filesystem.edit".to_owned(),
                    arguments_json:
                        r#"{"path":"src/lib.rs","old_text":"before","new_text":"after two"}"#
                            .to_owned(),
                    working_directory: None,
                },
            ),
            durable(
                2,
                bcode_session_models::SessionEventKind::ToolInvocationResultRecorded {
                    record: bcode_session_models::ToolInvocationResultRecord {
                        invocation_id: "call-edit".to_owned(),
                        model_output: "edited src/lib.rs".to_owned(),
                        is_error: false,
                        presentation: None,
                        result: None,
                        content: Vec::new(),
                    },
                },
            ),
        ]);
        for update in updates {
            assert!(
                handle
                    .try_send(super::BcodeRuntimeMessage::SessionStream(Box::new(update)))
                    .is_ok(),
                "session update fits"
            );
        }
        let runtime_task = tokio::spawn(runtime.run());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while handle.stats().frames_presented < 7 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fragmented edit drafts and canonical handoff commit without external wakeup");
        assert_eq!(handle.stats().frames_presented, 7);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            handle.stats().frames_presented,
            7,
            "runtime remains idle after the edit handoff queue drains"
        );
        handle
            .try_send_terminal(bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke::with_modifiers(
                    bmux_keyboard::KeyCode::Char('d'),
                    bmux_keyboard::Modifiers {
                        ctrl: true,
                        ..bmux_keyboard::Modifiers::NONE
                    },
                ),
            ))
            .expect("exit input fits");
        let output = tokio::time::timeout(std::time::Duration::from_secs(2), runtime_task)
            .await
            .expect("managed runtime exits")
            .expect("runtime task joins")
            .unwrap_or_else(|_| panic!("managed runtime succeeds"));
        let original = output
            .presenter
            .frames
            .iter()
            .position(|frame| {
                frame.text.contains("receiving original text")
                    && frame.text.contains("before")
                    && !frame.text.contains("new_text")
            })
            .expect("original edit text committed before replacement starts");
        let partial = output
            .presenter
            .frames
            .iter()
            .position(|frame| frame.text.contains("aft") && !frame.text.contains("after two"))
            .expect("partial replacement committed");
        let complete = output
            .presenter
            .frames
            .iter()
            .position(|frame| frame.text.contains("after two"))
            .expect("complete replacement committed");
        let result = output
            .presenter
            .frames
            .iter()
            .position(|frame| frame.text.contains("edited src/lib.rs"))
            .expect("canonical edit result committed");
        assert!(
            original < partial && partial < complete && complete < result,
            "frames: {:?}",
            output.presenter.frames
        );
        assert_eq!(
            output.presenter.frames[result]
                .text
                .matches("edited src/lib.rs")
                .count(),
            1
        );
        assert!(!output.presenter.frames[result].text.contains("assembling"));
        assert!(
            output
                .presenter
                .frames
                .iter()
                .all(|frame| frame.invocation_primary_items <= 1),
            "more than one primary invocation item was rendered"
        );
        let invocation_ids = output
            .presenter
            .frames
            .iter()
            .flat_map(|frame| frame.invocation_item_ids.iter())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            invocation_ids.len(),
            1,
            "invocation identity changed across frames"
        );
        assert_eq!(output.stats.frames_presented, 8);
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
            .expect("first draft awaits presentation");
        assert!(first.preview.is_empty());
        assert_eq!(
            bmux_tui_runtime::Program::presentation_committed(
                &mut model,
                bmux_tui_runtime::PresentReport::default(),
            )
            .invalidation,
            bmux_tui_runtime::Invalidation::Redraw
        );
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
        runtime_active: std::sync::Arc<tokio::sync::Notify>,
    }

    #[derive(Default)]
    struct CommittedLatencyProgram;

    impl bmux_tui_runtime::Program for CommittedLatencyProgram {
        type Message = ();
        type Error = Infallible;

        fn update(
            &mut self,
            _event: bmux_tui_runtime::RuntimeEvent<Self::Message>,
        ) -> Result<bmux_tui_runtime::Update<Self::Message>, Self::Error> {
            Ok(bmux_tui_runtime::Update::redraw().merge(bmux_tui_runtime::Update::exit()))
        }
    }

    struct CommittedLatencyPresenter {
        admitted_at: std::time::Instant,
        committed_latency: std::sync::Arc<std::sync::Mutex<Option<std::time::Duration>>>,
    }

    impl bmux_tui_runtime::Presenter<CommittedLatencyProgram> for CommittedLatencyPresenter {
        type Error = Infallible;

        fn reset(&mut self, _reason: bmux_tui_runtime::ResetReason) {}

        fn present(
            &mut self,
            _program: &mut CommittedLatencyProgram,
        ) -> Result<bmux_tui_runtime::PresentReport, Self::Error> {
            *self.committed_latency.lock().expect("latency lock") =
                Some(self.admitted_at.elapsed());
            Ok(bmux_tui_runtime::PresentReport {
                changed_cells: 1,
                full_repaint: false,
            })
        }
    }

    async fn committed_latency_sample(terminal: bool) -> std::time::Duration {
        let committed_latency = std::sync::Arc::new(std::sync::Mutex::new(None));
        let admitted_at = std::time::Instant::now();
        let (runtime, handle) = bmux_tui_runtime::Runtime::new(
            CommittedLatencyProgram,
            CommittedLatencyPresenter {
                admitted_at,
                committed_latency: std::sync::Arc::clone(&committed_latency),
            },
            bmux_tui_runtime::RuntimeConfig {
                frame_interval: None,
                messages_per_turn: 1,
                ..bmux_tui_runtime::RuntimeConfig::default()
            },
        );
        if terminal {
            handle
                .try_send_terminal(bmux_tui::event::Event::Tick)
                .expect("terminal admitted");
        } else {
            handle.schedule_timer(
                bmux_tui_runtime::TimerId::new("product-latency-probe"),
                admitted_at,
            );
        }
        runtime
            .run()
            .await
            .unwrap_or_else(|_| panic!("latency probe runtime succeeds"));
        let latency = *committed_latency.lock().expect("latency lock");
        latency.expect("presentation committed")
    }

    fn latency_summary(samples: &[std::time::Duration]) -> serde_json::Value {
        let mut values = samples
            .iter()
            .map(|duration| duration.as_secs_f64() * 1_000.0)
            .collect::<Vec<_>>();
        values.sort_by(f64::total_cmp);
        let percentile = |percent: usize| {
            let rank = values.len().saturating_mul(percent).div_ceil(100);
            values[rank.saturating_sub(1).min(values.len() - 1)]
        };
        serde_json::json!({
            "samples_ms": values,
            "min_ms": values[0],
            "p50_ms": percentile(50),
            "p95_ms": percentile(95),
            "p99_ms": percentile(99),
            "max_ms": values[values.len() - 1],
        })
    }

    #[tokio::test]
    #[ignore = "manual already-built product latency distribution probe"]
    async fn product_input_and_timer_to_committed_presentation_latency_report() {
        const SAMPLES: usize = 50;
        const LOCKED_P99_BUDGET: std::time::Duration = std::time::Duration::from_millis(100);
        let mut terminal_samples = Vec::with_capacity(SAMPLES);
        let mut timer_samples = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            terminal_samples.push(committed_latency_sample(true).await);
            timer_samples.push(committed_latency_sample(false).await);
        }
        let terminal = latency_summary(&terminal_samples);
        let timer = latency_summary(&timer_samples);
        println!(
            "BCODE_PERF_CASE {}",
            serde_json::json!({
                "domain": "product_committed_presentation_latency",
                "profile": "test",
                "sample_count": SAMPLES,
                "locked_p99_budget_ms": LOCKED_P99_BUDGET.as_millis(),
                "terminal": terminal,
                "timer": timer,
            })
        );
        for (name, samples) in [("terminal", terminal_samples), ("timer", timer_samples)] {
            let mut samples = samples;
            samples.sort_unstable();
            let p99 = samples[SAMPLES.saturating_mul(99).div_ceil(100) - 1];
            assert!(
                p99 <= LOCKED_P99_BUDGET,
                "{name} p99 {p99:?} exceeded {LOCKED_P99_BUDGET:?}"
            );
        }
    }

    impl bmux_tui_runtime::Program for LatencyAcceptanceProgram {
        type Message = u64;
        type Error = Infallible;

        fn update(
            &mut self,
            event: bmux_tui_runtime::RuntimeEvent<Self::Message>,
        ) -> Result<bmux_tui_runtime::Update<Self::Message>, Self::Error> {
            self.runtime_active.notify_one();
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
    #[ignore = "run directly outside parallel test-suite contention via capture-tui-product-latency.sh"]
    async fn terminal_and_timer_latency_stay_within_flood_acceptance_budget() {
        const ACCEPTANCE_BUDGET: std::time::Duration = std::time::Duration::from_millis(100);
        let probe_started = std::sync::Arc::new(std::sync::Mutex::new(None));
        let runtime_active = std::sync::Arc::new(tokio::sync::Notify::new());
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
                runtime_active: std::sync::Arc::clone(&runtime_active),
                ..LatencyAcceptanceProgram::default()
            },
            bmux_tui_runtime::HeadlessPresenter::default(),
            config,
        );
        for value in 0..10_000 {
            handle.try_send(value).expect("configured flood fits");
        }
        let runtime_task = tokio::spawn(runtime.run());
        runtime_active.notified().await;
        *probe_started.lock().expect("probe lock") = Some(std::time::Instant::now());
        handle
            .try_send_terminal(bmux_tui::event::Event::Tick)
            .expect("terminal admission remains independent");
        handle.schedule_timer(
            bmux_tui_runtime::TimerId::new("acceptance-latency"),
            std::time::Instant::now(),
        );

        let output = runtime_task
            .await
            .expect("runtime task joins")
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
            Ok(if self.received == 3 {
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
        admit(&handle, BcodeRuntimeMessage::StreamingConfiguratorDue)
            .await
            .expect("configurator latest message admitted");
        let output = runtime
            .run()
            .await
            .unwrap_or_else(|_| panic!("runtime succeeds"));
        assert_eq!(output.program.received, 3);
        assert_eq!(output.stats.reliable_processed, 1);
        assert_eq!(output.stats.latest_processed, 2);
        assert_ne!(
            BcodeRuntimeAdmissionError::Full,
            BcodeRuntimeAdmissionError::Closed
        );
    }
}
