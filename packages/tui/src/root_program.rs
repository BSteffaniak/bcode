//! Bcode-owned root runtime message and model contracts.
//!
//! These types establish the application boundary before orchestration migrates from the existing
//! chat loop. BMUX treats messages and model state as opaque application data.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use bmux_tui::event::Event;

const TRANSCRIPT_SELECTION_SCOPE_ID: &str = "bcode.transcript";

use super::TuiError;
use super::artifact_stream::ActiveArtifactFetchCompletion;
use super::chat_loop::{ChatLoopState, DraftAutosave, TuiRuntimeSettings};
use super::effects::TuiEffectResult;
use super::history_flow;
use super::invalidation::{InvalidationKey, TemporalRegistry};
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
    /// Last successfully committed logical content-selection scene.
    pub committed_selection: bmux_tui::selection::SelectionScene,
    /// Bcode-owned transcript selection gesture state.
    pub transcript_selection: bmux_tui::selection::SelectionController,
    /// Canonical plain-text export for the current logical selection.
    pub selected_plain_text: Option<String>,
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
    temporal_registry: TemporalRegistry,
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
            committed_selection: bmux_tui::selection::SelectionScene::new(),
            transcript_selection: bmux_tui::selection::SelectionController::new(),
            selected_plain_text: None,
            committed_area: bmux_tui::geometry::Rect::new(0, 0, 0, 0),
            committed_layout: None,
            last_presented_at: None,
            exit_after_plugin_surface: false,
            plugin_surface_result: None,
            exit_requested: false,
            theme_input_signature,
            theme_reload_at,
            scheduled_deadlines: BTreeMap::new(),
            temporal_registry: TemporalRegistry::default(),
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
                bcode_plugin_sdk::tui::PluginTuiAction::OpenSurface {
                    plugin_id,
                    surface_id,
                    options,
                } => {
                    self.chat
                        .replace_effect(super::effects::TuiEffect::OpenPluginSurface {
                            plugin_id,
                            surface_kind: surface_id.clone(),
                            instance_id: format!("workflow-related-{surface_id}"),
                            options,
                            working_directory: self
                                .settings
                                .launch_working_directory()
                                .to_path_buf(),
                            session_id: self.chat.attached_session_id(),
                        });
                }
                bcode_plugin_sdk::tui::PluginTuiAction::SubscribeWorkflowRuns => {
                    let (updates, requests) = super::plugin_surface_host::subscribe_workflow_views(
                        self.loop_state.foreground_client(),
                        self.loop_state
                            .root_plugin_surface_invalidation()
                            .expect("plugin surface invalidation"),
                    );
                    self.loop_state
                        .attach_root_plugin_surface_updates(updates, requests);
                }
                bcode_plugin_sdk::tui::PluginTuiAction::SelectWorkflowRun { run_id } => {
                    self.loop_state.request_root_workflow_run(run_id);
                }
                bcode_plugin_sdk::tui::PluginTuiAction::UpdateWorkflowCatalogQuery {
                    filter,
                    sort,
                    group,
                    search,
                } => {
                    self.loop_state
                        .request_root_workflow_query(filter, sort, group, search);
                }
                bcode_plugin_sdk::tui::PluginTuiAction::LoadMoreWorkflowRuns { cursor } => {
                    self.loop_state.request_more_root_workflow_runs(cursor);
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
                            session_id: self.chat.attached_session_id(),
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
                        if let Some(session_id) = self.chat.attached_session_id() {
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
                                    session_id: self.chat.attached_session_id(),
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
        if self.loop_state.has_working_directory_dialog() {
            match self
                .loop_state
                .handle_working_directory_dialog_event(self.settings.keymap(), &event)
            {
                super::chat_loop::WorkingDirectoryDialogRootOutcome::Apply(path) => {
                    let Some(session_id) = self.chat.attached_session_id() else {
                        self.chat.app.set_status("no active session".to_owned());
                        return super::invalidation::UiInvalidation::Structural;
                    };
                    self.chat
                        .start_effect(super::effects::TuiEffect::AttachWorktree {
                            session_id,
                            path,
                        });
                }
                super::chat_loop::WorkingDirectoryDialogRootOutcome::Canceled => {
                    self.chat
                        .app
                        .set_status("working directory change canceled".to_owned());
                }
                super::chat_loop::WorkingDirectoryDialogRootOutcome::Handled => {}
                super::chat_loop::WorkingDirectoryDialogRootOutcome::Unhandled => {
                    unreachable!("working-directory dialog was present")
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
                            self.chat.attached_session_id()
                        }
                        super::wt_create_dialog::WorktreeCreateTarget::NewSession => None,
                    };
                    self.chat
                        .start_effect(super::effects::TuiEffect::CreateWorktree {
                            operation_id: format!(
                                "tui-worktree-{}-{}",
                                std::process::id(),
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_nanos()
                            ),
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
                    == Some(super::keymap::BmuxAction::TranscriptCopySelection)
                {
                    self.copy_transcript_selection();
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
                if let Some(selection_damage) = self.handle_transcript_selection_mouse(mouse) {
                    return selection_damage;
                }
                if self.loop_state.handle_streaming_configurator_mouse(
                    &mut self.chat,
                    mouse,
                    self.committed_area,
                ) {
                    return super::invalidation::UiInvalidation::Structural;
                }
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

    fn handle_transcript_selection_mouse(
        &mut self,
        mouse: bmux_tui::event::MouseEvent,
    ) -> Option<super::invalidation::UiInvalidation> {
        if self.loop_state.permission_dialog.is_some()
            || self.loop_state.has_command_palette()
            || self.loop_state.has_slash_palette()
            || self
                .committed_layout
                .is_none_or(|layout| !layout.body().contains(mouse.position))
                && self.transcript_selection.phase()
                    == bmux_tui::selection::SelectionGesturePhase::Idle
        {
            return None;
        }
        let outcome = self
            .transcript_selection
            .handle_mouse(&self.committed_selection, mouse);
        match outcome {
            bmux_tui::selection::SelectionOutcome::Ignored
            | bmux_tui::selection::SelectionOutcome::Click => None,
            bmux_tui::selection::SelectionOutcome::Armed => {
                Some(super::invalidation::UiInvalidation::None)
            }
            bmux_tui::selection::SelectionOutcome::Changed { .. } => {
                self.refresh_selected_plain_text();
                let scrolled = self.settings.selection_autoscroll()
                    && self.committed_layout.is_some_and(|layout| {
                        scroll_transcript_for_selection_edge(
                            &mut self.chat.app,
                            layout.body(),
                            mouse.position,
                        )
                    });
                self.presentation_damage = bmux_tui::damage::Damage::Full;
                self.fast_temporal_presentation = false;
                Some(if scrolled {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::Paint
                })
            }
            bmux_tui::selection::SelectionOutcome::Completed => {
                self.refresh_selected_plain_text();
                self.presentation_damage = bmux_tui::damage::Damage::Full;
                self.fast_temporal_presentation = false;
                Some(super::invalidation::UiInvalidation::Paint)
            }
            bmux_tui::selection::SelectionOutcome::Cleared
            | bmux_tui::selection::SelectionOutcome::Invalidated => {
                self.selected_plain_text = None;
                self.chat.app.set_transcript_selection_pinned(false);
                self.presentation_damage = bmux_tui::damage::Damage::Full;
                self.fast_temporal_presentation = false;
                Some(super::invalidation::UiInvalidation::Paint)
            }
        }
    }

    fn refresh_selected_plain_text(&mut self) {
        self.selected_plain_text = self
            .transcript_selection
            .snapshot(&self.committed_selection)
            .and_then(|snapshot| {
                export_plain_transcript_selection(self.chat.app.transcript(), &snapshot)
            });
        self.chat
            .app
            .set_transcript_selection_pinned(self.selected_plain_text.is_some());
    }

    fn start_root_skill_action(
        &mut self,
        action: super::effects::SkillActionKind,
        skill_id: bcode_skill_models::SkillId,
        arguments: String,
    ) {
        super::skill_flow::start_skill_action(
            self.settings.launch_working_directory(),
            self.settings.launch_options(),
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
        let Some(session_id) = self.chat.attached_session_id() else {
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

    fn copy_transcript_selection(&mut self) {
        let Some(text) = self.selected_plain_text.as_deref() else {
            self.chat
                .app
                .set_status("no transcript selection".to_owned());
            return;
        };
        let result = super::markdown_activation::copy_text(text);
        self.report_transcript_copy_result(result);
    }

    fn report_transcript_copy_result<E: std::fmt::Display>(&mut self, result: Result<(), E>) {
        match result {
            Ok(()) => self
                .chat
                .app
                .set_status("transcript selection copied".to_owned()),
            Err(error) => self.chat.app.set_status(format!("copy failed: {error}")),
        }
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
        if let Some(session_id) = self.chat.attached_session_id() {
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
            self.settings.launch_options(),
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
                        session_id: self.chat.attached_session_id(),
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

    fn apply_root_command_action(&mut self, request: super::chat_loop::CommandDispatchRequest) {
        let super::chat_loop::CommandDispatchRequest { action, session } = request;
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
                "turn.cancel" => {
                    super::chat_loop::start_cancel_turn(&mut self.chat, &mut self.loop_state);
                }
                "context.compact" => {
                    if let Some(session_id) = self.chat.attached_session_id() {
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
                    self.chat.append_durable_presentation_note(
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
                let session_id = self.chat.attached_session_id();
                if let Some(refusal) = session.refusal(session_id.is_some()) {
                    self.chat.app.set_status(refusal.message().to_owned());
                    return;
                }
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
                        session_id,
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
        self.loop_state
            .session_changed(self.chat.viewing_session_id());
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

    fn reconcile_temporal_registry(&mut self) {
        let now = Instant::now();
        self.temporal_registry.reconcile(
            self.chat
                .app
                .invalidation_requests(now, std::time::SystemTime::now()),
        );
    }

    fn schedule_deadlines(&mut self) {
        self.reconcile_temporal_registry();
        let invalidation_at = self.temporal_registry.next_at();
        let Some(handle) = &self.runtime_handle else {
            return;
        };
        let now = Instant::now();
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
                let due = self.temporal_registry.take_due(now);
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
        let fast_temporal_presentation = std::mem::take(&mut program.fast_temporal_presentation)
            && program
                .transcript_selection
                .snapshot(&program.committed_selection)
                .is_none();
        let (draw_stats, layout) = super::chat_loop::draw_chat_frame(
            self.terminal,
            &mut program.chat,
            &mut program.loop_state,
            Duration::ZERO,
            frame_interval,
            damage,
            fast_temporal_presentation,
            program.committed_layout,
            &program.transcript_selection,
        )?;
        program.committed_hits = self.terminal.hits().clone();
        program.committed_selection = self.terminal.selection().clone();
        let reconciliation = program
            .transcript_selection
            .reconcile(&program.committed_selection);
        if reconciliation == bmux_tui::selection::SelectionOutcome::Invalidated {
            program.selected_plain_text = None;
            program.chat.app.set_transcript_selection_pinned(false);
            program.presentation_damage = bmux_tui::damage::Damage::Full;
            program.fast_temporal_presentation = false;
        } else if program.transcript_selection.phase()
            != bmux_tui::selection::SelectionGesturePhase::Idle
        {
            program.refresh_selected_plain_text();
        }
        program.committed_area = self.terminal.area();
        program.committed_layout = layout;
        Ok(bmux_tui_runtime::PresentReport {
            changed_cells: draw_stats.changed_cells,
            full_repaint: draw_stats.full_repaint,
        })
    }
}

fn selection_edge_scroll_rows(
    body: bmux_tui::geometry::Rect,
    point: bmux_tui::geometry::Point,
) -> Option<(bool, usize)> {
    if body.is_empty() {
        return None;
    }
    let (backward, intensity) = if point.y <= body.y {
        (true, body.y.saturating_sub(point.y).saturating_add(1))
    } else if point.y >= body.bottom().saturating_sub(1) {
        (
            false,
            point
                .y
                .saturating_sub(body.bottom().saturating_sub(1))
                .saturating_add(1),
        )
    } else {
        return None;
    };
    Some((backward, usize::from(intensity.min(body.height.max(1)))))
}

fn scroll_transcript_for_selection_edge(
    app: &mut super::app::BmuxApp,
    body: bmux_tui::geometry::Rect,
    point: bmux_tui::geometry::Point,
) -> bool {
    let Some((backward, rows)) = selection_edge_scroll_rows(body, point) else {
        return false;
    };
    if backward {
        app.scroll_transcript_up(rows)
    } else {
        app.scroll_transcript_down(rows)
    }
}

fn ensure_transcript_parent_scope(
    scene: &mut bmux_tui::selection::SelectionScene,
    body: bmux_tui::geometry::Rect,
) {
    if body.is_empty()
        || scene
            .scopes()
            .iter()
            .any(|scope| scope.id.as_str() == TRANSCRIPT_SELECTION_SCOPE_ID)
    {
        return;
    }
    scene.push_scope(bmux_tui::selection::SelectionScope::new(
        TRANSCRIPT_SELECTION_SCOPE_ID,
        body,
    ));
}

// Shared with the sibling renderer so selection metadata is built inside the frame transaction.
#[allow(clippy::redundant_pub_crate)]
pub(super) fn transcript_selection_scene(
    app: &super::app::BmuxApp,
    body: bmux_tui::geometry::Rect,
) -> bmux_tui::selection::SelectionScene {
    let mut scene = bmux_tui::selection::SelectionScene::new();
    register_transcript_selection_scene(&mut scene, app, body);
    scene
}

#[allow(clippy::too_many_lines)]
fn register_transcript_selection_scene(
    scene: &mut bmux_tui::selection::SelectionScene,
    app: &super::app::BmuxApp,
    body: bmux_tui::geometry::Rect,
) {
    ensure_transcript_parent_scope(scene, body);
    if body.is_empty() {
        return;
    }
    let top_row = app.transcript_top_row(body.height);
    let mut source_ranges =
        std::collections::BTreeMap::<usize, Vec<Option<std::ops::Range<usize>>>>::new();
    let mut y = body.y;
    for visible in app
        .transcript_layout()
        .visible_lines_from_top(top_row, body.height)
    {
        if y >= body.bottom() {
            break;
        }
        let Some(row) = app.transcript_layout().line(visible) else {
            continue;
        };
        if visible.source != super::transcript_layout::VisibleTranscriptSource::Transcript {
            y = y.saturating_add(1);
            continue;
        }
        let Some(item) = app.transcript().get(visible.entry_index) else {
            y = y.saturating_add(1);
            continue;
        };
        let scope_id = format!("bcode.transcript.item.{}", item.id().get());
        if !scene
            .scopes()
            .iter()
            .any(|scope| scope.id.as_str() == scope_id)
        {
            let entry_start = app
                .transcript_layout()
                .entry_start_row(visible.source, visible.entry_index)
                .unwrap_or(visible.row_index);
            let entry_rows = app
                .transcript_layout()
                .entry_row_count(visible.source, visible.entry_index)
                .unwrap_or(1);
            let visible_start = entry_start.max(top_row);
            let visible_end = entry_start
                .saturating_add(entry_rows)
                .min(top_row.saturating_add(usize::from(body.height)));
            scene.push_scope(
                bmux_tui::selection::SelectionScope::new(
                    scope_id.clone(),
                    bmux_tui::geometry::Rect::new(
                        body.x,
                        body.y.saturating_add(
                            u16::try_from(visible_start.saturating_sub(top_row))
                                .unwrap_or(u16::MAX),
                        ),
                        body.width,
                        u16::try_from(visible_end.saturating_sub(visible_start))
                            .unwrap_or(u16::MAX),
                    ),
                )
                .parent(TRANSCRIPT_SELECTION_SCOPE_ID)
                .order(u64::try_from(visible.entry_index).unwrap_or(u64::MAX))
                .revision(item.revision()),
            );
        }
        let text = row.plain_text();
        if item.text_format() == bcode_session_view_models::TextFormat::Markdown {
            let entry_start = app
                .transcript_layout()
                .entry_start_row(visible.source, visible.entry_index)
                .unwrap_or(visible.row_index);
            let layout = super::render::TranscriptItemLayout::resolve(
                &app.presented_theme(),
                item,
                body.width,
            );
            let content_offset = {
                let entry_rows = app
                    .transcript_layout()
                    .entry_row_count(visible.source, visible.entry_index)
                    .unwrap_or_default();
                let markdown_rows =
                    super::render::transcript_markdown_projection_for_layout(app, item, body.width)
                        .as_ref()
                        .map_or(0, |projection| projection.lines.len());
                entry_rows
                    .saturating_sub(markdown_rows)
                    .saturating_sub(1)
                    .saturating_sub(layout.bottom_rows())
            };
            let document_row = visible
                .row_in_entry
                .checked_sub(content_offset)
                .and_then(|row| u16::try_from(row).ok());
            if let Some(document_row) = document_row {
                let projection =
                    super::render::transcript_markdown_projection_for_layout(app, item, body.width);
                if let Some(projection) = projection {
                    let origin = bmux_tui::geometry::Point::new(
                        body.x.saturating_add(layout.markdown_x()),
                        body.y.saturating_add(
                            u16::try_from(
                                entry_start
                                    .saturating_add(content_offset)
                                    .saturating_sub(top_row),
                            )
                            .unwrap_or(u16::MAX),
                        ),
                    );
                    for fragment in super::markdown_selection::markdown_selection_fragments(
                        &scope_id,
                        &format!("bcode.transcript.item.{}.markdown", item.id().get()),
                        &projection,
                        origin,
                        u64::try_from(visible.entry_index)
                            .unwrap_or(u64::MAX)
                            .saturating_mul(1_000_000),
                        item.revision(),
                        Some(document_row),
                    )
                    .into_iter()
                    .filter(|fragment| {
                        fragment.area.y == y
                            && document_row == fragment.area.y.saturating_sub(origin.y)
                    }) {
                        scene.push_fragment(fragment);
                    }
                    y = y.saturating_add(1);
                    continue;
                }
            }
        }
        let ranges = source_ranges.entry(visible.entry_index).or_insert_with(|| {
            transcript_plain_row_source_ranges(
                item.text.as_str(),
                app.transcript_layout()
                    .entry_row_count(visible.source, visible.entry_index)
                    .unwrap_or(0),
            )
        });
        let (source, source_offset) = ranges
            .get(visible.row_in_entry)
            .and_then(Clone::clone)
            .and_then(|range| {
                item.text
                    .get(range.clone())
                    .map(|source| (source, range.start))
            })
            .unwrap_or((text.as_str(), 0));
        for fragment in bmux_tui::selection::plain_text_fragments(
            scope_id,
            format!("bcode.transcript.item.{}.plain", item.id().get()),
            bmux_tui::geometry::Rect::new(body.x, y, body.width, 1),
            u64::try_from(visible.row_in_entry).unwrap_or(u64::MAX),
            source,
            source_offset,
            item.revision(),
        ) {
            scene.push_fragment(fragment);
        }
        y = y.saturating_add(1);
    }
}

#[must_use]
fn export_plain_transcript_selection<'a>(
    items: impl IntoIterator<Item = &'a super::transcript::TranscriptItem>,
    snapshot: &bmux_tui::selection::SelectionSnapshot,
) -> Option<String> {
    let mut selected =
        std::collections::BTreeMap::<u64, Vec<(String, std::ops::Range<usize>)>>::new();
    for slice in &snapshot.slices {
        let content = slice.content_id.as_str();
        let remainder = content.strip_prefix("bcode.transcript.item.")?;
        let (item_id, kind) = remainder.split_once('.')?;
        let item_id = item_id.parse::<u64>().ok()?;
        if kind != "plain" && kind != "markdown" && !kind.starts_with("markdown.code.") {
            return None;
        }
        selected
            .entry(item_id)
            .or_default()
            .push((kind.to_owned(), slice.source_range.clone()));
    }
    let mut output = Vec::new();
    for item in items {
        let Some(selections) = selected.remove(&item.id().get()) else {
            continue;
        };
        let mut text = String::new();
        for (kind, range) in selections {
            if let Some(suffix) = kind.strip_prefix("markdown.") {
                let ranges = super::markdown_selection::expand_markdown_selection_range(
                    item.text(),
                    suffix,
                    range,
                )?;
                for range in ranges {
                    text.push_str(item.text().get(range)?);
                }
            } else {
                text.push_str(item.text().get(range)?);
            }
        }
        if !text.is_empty() {
            output.push(format_transcript_selection_item(item, &text));
        }
    }
    (!output.is_empty()).then(|| output.join("\n\n"))
}

/// Export a visible transcript selection through the production selection-export path.
#[cfg(test)]
pub fn export_plain_transcript_selection_for_test<'a>(
    items: impl IntoIterator<Item = &'a super::transcript::TranscriptItem>,
    snapshot: &bmux_tui::selection::SelectionSnapshot,
) -> Option<String> {
    export_plain_transcript_selection(items, snapshot)
}

fn format_transcript_selection_item(
    item: &super::transcript::TranscriptItem,
    text: &str,
) -> String {
    use super::transcript::TranscriptItemKind;

    let metadata = match item.kind() {
        TranscriptItemKind::ToolRequest {
            tool_name,
            status,
            active,
            ..
        } => Some(format!(
            "Tool request: {tool_name} [{}]",
            status.map_or(
                if *active { "active" } else { "requested" },
                |status| match status {
                    bcode_session_view_models::ToolInvocationViewStatus::Requested => "requested",
                    bcode_session_view_models::ToolInvocationViewStatus::Waiting => "waiting",
                    bcode_session_view_models::ToolInvocationViewStatus::Running => "running",
                    bcode_session_view_models::ToolInvocationViewStatus::Finished => "finished",
                    bcode_session_view_models::ToolInvocationViewStatus::Failed => "failed",
                    bcode_session_view_models::ToolInvocationViewStatus::Cancelled => "cancelled",
                }
            )
        )),
        TranscriptItemKind::ToolResult {
            tool_name,
            is_error,
            timing,
            ..
        } => Some(format!(
            "Tool result: {} [{}{}]",
            tool_name.as_deref().unwrap_or("unknown"),
            if *is_error { "failed" } else { "finished" },
            timing
                .duration_ms
                .map_or_else(String::new, |duration| format!(", {duration}ms"))
        )),
        TranscriptItemKind::ToolRequestDraft { .. } => {
            Some("Tool request draft [pending; rendered-text fallback]".to_owned())
        }
        TranscriptItemKind::ToolContribution { contribution, .. } => Some(format!(
            "Tool contribution: {} v{} by {} [rendered-text fallback]",
            contribution.schema, contribution.schema_version, contribution.producer_id
        )),
        _ => None,
    };
    metadata.map_or_else(
        || text.to_owned(),
        |metadata| {
            item.timestamp_ms().map_or_else(
                || format!("{metadata}\n{text}"),
                |timestamp| format!("{metadata} @ {timestamp}ms\n{text}"),
            )
        },
    )
}

fn transcript_plain_row_source_ranges(
    source: &str,
    rendered_rows: usize,
) -> Vec<Option<std::ops::Range<usize>>> {
    let source_rows = source
        .split_inclusive('\n')
        .scan(0_usize, |offset, line| {
            let start = *offset;
            *offset = offset.saturating_add(line.len());
            let content_len = line.strip_suffix('\n').map_or(line.len(), str::len);
            Some(start..start.saturating_add(content_len))
        })
        .collect::<Vec<_>>();
    if source_rows.len() != rendered_rows {
        return vec![None; rendered_rows];
    }
    source_rows.into_iter().map(Some).collect()
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
        let deferred = self
            .loop_state
            .apply_deferred_session_stream_updates(&mut self.chat);
        self.schedule_deadlines();
        if deferred {
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
                        let changed = super::mouse_flow::handle_non_permission_mouse(
                            None,
                            &mut self.chat,
                            mouse,
                            self.settings.mouse_scroll_rows(),
                        );
                        if changed {
                            self.invalidation = self
                                .invalidation
                                .merge(super::invalidation::UiInvalidation::Structural);
                            self.presentation_damage = bmux_tui::damage::Damage::Full;
                            self.fast_temporal_presentation = false;
                        }
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
                        super::interactive_surface::InteractiveSurfaceEventOutcome::Redraw => {
                            self.invalidation = self
                                .invalidation
                                .merge(super::invalidation::UiInvalidation::Paint);
                            let geometry = self
                                .loop_state
                                .active_interactive_surface_geometry()
                                .filter(|geometry| !geometry.destination.is_empty());
                            self.presentation_damage =
                                geometry.map_or(bmux_tui::damage::Damage::Full, |geometry| {
                                    bmux_tui::damage::Damage::regions(
                                        [geometry.destination],
                                        self.committed_area,
                                        bmux_tui::damage::DamagePolicy {
                                            max_regions: 1,
                                            max_area_percent: 100,
                                        },
                                    )
                                });
                            self.fast_temporal_presentation = geometry.is_some()
                                && self.committed_layout.is_some()
                                && !self.presentation_damage.is_full()
                                && !self.presentation_damage.is_none();
                            return Ok(bmux_tui_runtime::Update::redraw());
                        }
                        super::interactive_surface::InteractiveSurfaceEventOutcome::Relayout => {
                            self.invalidation = self
                                .invalidation
                                .merge(super::invalidation::UiInvalidation::Items);
                            self.presentation_damage = bmux_tui::damage::Damage::Full;
                            self.fast_temporal_presentation = false;
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
                let action = self.loop_state.poll_root_plugin_surface(&client);
                if self.loop_state.has_root_plugin_surface()
                    && let Some(invalidation) = self.loop_state.root_plugin_surface_invalidation()
                {
                    invalidation.request();
                }
                match action {
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
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::OpenSurface {
                        plugin_id,
                        surface_id,
                        options,
                    }) => {
                        self.chat
                            .replace_effect(super::effects::TuiEffect::OpenPluginSurface {
                                plugin_id,
                                surface_kind: surface_id.clone(),
                                instance_id: format!("workflow-related-{surface_id}"),
                                options,
                                working_directory: self
                                    .settings
                                    .launch_working_directory()
                                    .to_path_buf(),
                                session_id: self.chat.attached_session_id(),
                            });
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::SubscribeWorkflowRuns) => {
                        let (updates, requests) =
                            super::plugin_surface_host::subscribe_workflow_views(
                                self.loop_state.foreground_client(),
                                self.loop_state
                                    .root_plugin_surface_invalidation()
                                    .expect("plugin surface invalidation"),
                            );
                        self.loop_state
                            .attach_root_plugin_surface_updates(updates, requests);
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::SelectWorkflowRun { run_id }) => {
                        self.loop_state.request_root_workflow_run(run_id);
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::UpdateWorkflowCatalogQuery {
                        filter,
                        sort,
                        group,
                        search,
                    }) => {
                        self.loop_state
                            .request_root_workflow_query(filter, sort, group, search);
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::LoadMoreWorkflowRuns {
                        cursor,
                    }) => {
                        self.loop_state.request_more_root_workflow_runs(cursor);
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
                                session_id: self.chat.attached_session_id(),
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
                let previous_session_id = self.chat.viewing_session_id();
                let observation = result.daemon_observation();
                self.loop_state.observe_daemon(&mut self.chat, &observation);
                super::chat_loop::apply_effect_result(
                    &mut self.settings,
                    &mut self.chat,
                    &mut self.draft_autosave,
                    &mut self.loop_state,
                    *result,
                );
                if previous_session_id != self.chat.viewing_session_id() {
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
        if self.transcript_selection.phase() != bmux_tui::selection::SelectionGesturePhase::Idle {
            self.presentation_damage = bmux_tui::damage::Damage::Full;
        }
        self.fast_temporal_presentation = self.transcript_selection.phase()
            == bmux_tui::selection::SelectionGesturePhase::Idle
            && self.invalidation == super::invalidation::UiInvalidation::Paint
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
    /// Session working-directory change flow.
    WorkingDirectory,
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
    use crate::app::BmuxApp;
    use bcode_command::CommandTextFormat;
    use bcode_session_models::SessionId;
    use std::collections::{BTreeMap, VecDeque};
    use std::convert::Infallible;

    fn assert_runtime_message_is_send<T: Send + 'static>() {}

    #[tokio::test]
    async fn active_selection_forces_full_damage_and_disables_fast_temporal_frames() {
        let mut model = root_test_model();
        model.transcript_selection = {
            let body = bmux_tui::geometry::Rect::new(0, 0, 8, 1);
            model
                .committed_selection
                .push_scope(bmux_tui::selection::SelectionScope::new(
                    "selection.test",
                    body,
                ));
            for fragment in bmux_tui::selection::plain_text_fragments(
                "selection.test",
                "selection.test.content",
                body,
                0,
                "abcdefgh",
                0,
                1,
            ) {
                model.committed_selection.push_fragment(fragment);
            }
            let mut controller = bmux_tui::selection::SelectionController::new();
            let _ = controller.handle_mouse(
                &model.committed_selection,
                bmux_tui::event::MouseEvent::new(
                    bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                    bmux_tui::geometry::Point::new(0, 0),
                ),
            );
            let _ = controller.handle_mouse(
                &model.committed_selection,
                bmux_tui::event::MouseEvent::new(
                    bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                    bmux_tui::geometry::Point::new(4, 0),
                ),
            );
            controller
        };
        model.invalidation = super::super::invalidation::UiInvalidation::Paint;
        model.fast_temporal_presentation = true;
        model.presentation_damage = model.select_presentation_damage(model.invalidation, &[]);
        if model.transcript_selection.phase() != bmux_tui::selection::SelectionGesturePhase::Idle {
            model.presentation_damage = bmux_tui::damage::Damage::Full;
            model.fast_temporal_presentation = false;
        }

        assert!(model.presentation_damage.is_full());
        assert!(!model.fast_temporal_presentation);
    }

    #[tokio::test]
    async fn runtime_work_starts_existing_older_history_effect_for_selection_reveal() {
        let session_id = bcode_session_models::SessionId::new();
        let history = [bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 10,
            timestamp_ms: 10,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::UserMessage {
                client_id: bcode_session_models::ClientId::new(),
                text: "resident tail".to_owned(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        }];
        let mut model = root_test_model_with_history(session_id, &history);
        model
            .chat
            .app
            .replace_latest_transcript_window(&history, true);
        model.chat.app.request_older_history_for_test(2);
        let cursor = model.chat.app.older_history_cursor().expect("older cursor");

        assert_eq!(
            model.loop_state.prepare_runtime_work(
                &mut model.chat,
                bmux_tui::geometry::Rect::new(0, 0, 40, 12),
            ),
            super::super::invalidation::UiInvalidation::Structural
        );
        assert!(model.chat.pending_effects.contains_effect(
            &super::super::effects::TuiEffect::LoadOlderHistory { session_id, cursor }
        ));
        assert!(model.chat.app.loading_older_history());

        model.chat.pending_effects = super::super::effects::TuiEffectQueue::default();
        model
            .chat
            .app
            .replace_transcript_window(&history, false, true, 10);
        model.chat.app.request_newer_history_for_test(2);
        let newer_cursor = model.chat.app.newer_history_cursor().expect("newer cursor");
        assert_eq!(
            model.loop_state.prepare_runtime_work(
                &mut model.chat,
                bmux_tui::geometry::Rect::new(0, 0, 40, 12),
            ),
            super::super::invalidation::UiInvalidation::Structural
        );
        assert!(model.chat.pending_effects.contains_effect(
            &super::super::effects::TuiEffect::LoadNewerHistory {
                session_id,
                cursor: newer_cursor,
            }
        ));
    }

    #[tokio::test]
    async fn normal_present_and_mouse_input_path_creates_transcript_selection() {
        use bmux_tui_runtime::Presenter;

        let session_id = bcode_session_models::SessionId::new();
        let history = [bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::UserMessage {
                client_id: bcode_session_models::ClientId::new(),
                text: "normal input path".to_owned(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        }];
        let mut model = root_test_model_with_history(session_id, &history);
        let area = bmux_tui::geometry::Rect::new(0, 0, 40, 12);
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, area);
        super::BcodeRuntimePresenter::new(&mut terminal)
            .present(&mut model)
            .expect("startup frame presents");
        let fragment = model
            .committed_selection
            .fragments()
            .first()
            .cloned()
            .expect("startup transcript fragment");

        assert_eq!(
            model.handle_basic_terminal_event(bmux_tui::event::Event::Mouse(
                bmux_tui::event::MouseEvent::new(
                    bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                    bmux_tui::geometry::Point::new(fragment.area.x, fragment.area.y),
                ),
            )),
            super::super::invalidation::UiInvalidation::None
        );
        assert!(matches!(
            model.handle_basic_terminal_event(bmux_tui::event::Event::Mouse(
                bmux_tui::event::MouseEvent::new(
                    bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                    bmux_tui::geometry::Point::new(
                        fragment.area.x.saturating_add(fragment.area.width),
                        fragment.area.y,
                    ),
                ),
            )),
            super::super::invalidation::UiInvalidation::Paint
                | super::super::invalidation::UiInvalidation::Structural
        ));
        assert_eq!(
            model.handle_basic_terminal_event(bmux_tui::event::Event::Mouse(
                bmux_tui::event::MouseEvent::new(
                    bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
                    bmux_tui::geometry::Point::new(
                        fragment.area.x.saturating_add(fragment.area.width),
                        fragment.area.y,
                    ),
                ),
            )),
            super::super::invalidation::UiInvalidation::Paint
        );
        assert!(model.selected_plain_text.is_some());
        assert!(
            model
                .transcript_selection
                .snapshot(&model.committed_selection)
                .is_some()
        );
    }

    #[tokio::test]
    async fn scrolling_reprojects_selection_highlights_for_still_visible_content() {
        use bmux_tui_runtime::Presenter;

        let session_id = bcode_session_models::SessionId::new();
        let history = (1..=20)
            .map(|sequence| bcode_session_models::SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence,
                timestamp_ms: sequence,
                session_id,
                provenance: None,
                kind: bcode_session_models::SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: format!("message {sequence}"),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            })
            .collect::<Vec<_>>();
        let mut model = root_test_model_with_history(session_id, &history);
        let area = bmux_tui::geometry::Rect::new(0, 0, 40, 12);
        let mut initial_bytes = Vec::new();
        let mut initial_terminal = bmux_tui::terminal::Terminal::new(&mut initial_bytes, area);
        super::BcodeRuntimePresenter::new(&mut initial_terminal)
            .present(&mut model)
            .expect("initial frame presents");
        let fragment = model
            .committed_selection
            .fragments()
            .iter()
            .find(|fragment| {
                fragment.area.y
                    == model
                        .committed_layout
                        .expect("committed layout")
                        .body()
                        .bottom()
                        .saturating_sub(2)
            })
            .cloned()
            .expect("penultimate visible fragment");
        let _ = model.transcript_selection.handle_mouse(
            &model.committed_selection,
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(fragment.area.x, fragment.area.y),
            ),
        );
        let _ = model.transcript_selection.handle_mouse(
            &model.committed_selection,
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(
                    fragment.area.x.saturating_add(fragment.area.width),
                    fragment.area.y,
                ),
            ),
        );
        let before = model
            .transcript_selection
            .snapshot(&model.committed_selection)
            .expect("selection before scroll");
        let before_highlights = before.visible_highlights.clone();

        assert!(model.chat.app.scroll_transcript_up(1));
        model.presentation_damage = bmux_tui::damage::Damage::Full;
        let mut scrolled_bytes = Vec::new();
        let mut scrolled_terminal = bmux_tui::terminal::Terminal::new(&mut scrolled_bytes, area);
        super::BcodeRuntimePresenter::new(&mut scrolled_terminal)
            .present(&mut model)
            .expect("scrolled frame presents");
        let after = model
            .transcript_selection
            .snapshot(&model.committed_selection)
            .expect("selection survives scroll");

        assert_eq!(after.slices, before.slices);
        assert_ne!(after.visible_highlights, before_highlights);
        assert!(after.visible_highlights.iter().all(|highlight| {
            model
                .committed_layout
                .expect("committed layout")
                .body()
                .intersection(*highlight)
                == *highlight
        }));
    }

    #[tokio::test]
    async fn presenter_paints_selection_highlight_over_real_transcript_content() {
        use bmux_tui_runtime::Presenter;

        let session_id = bcode_session_models::SessionId::new();
        let history = [bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::UserMessage {
                client_id: bcode_session_models::ClientId::new(),
                text: "selectable transcript text".to_owned(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        }];
        let mut model = root_test_model_with_history(session_id, &history);
        let area = bmux_tui::geometry::Rect::new(0, 0, 40, 12);
        let mut initial_bytes = Vec::new();
        let mut initial_terminal = bmux_tui::terminal::Terminal::new(&mut initial_bytes, area);
        super::BcodeRuntimePresenter::new(&mut initial_terminal)
            .present(&mut model)
            .expect("initial frame presents");
        let fragment = model
            .committed_selection
            .fragments()
            .first()
            .cloned()
            .expect("transcript fragment");
        let _ = model.transcript_selection.handle_mouse(
            &model.committed_selection,
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(fragment.area.x, fragment.area.y),
            ),
        );
        let _ = model.transcript_selection.handle_mouse(
            &model.committed_selection,
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(
                    fragment.area.x.saturating_add(fragment.area.width),
                    fragment.area.y,
                ),
            ),
        );
        model.presentation_damage = bmux_tui::damage::Damage::Full;

        let mut selected_bytes = Vec::new();
        let mut selected_terminal = bmux_tui::terminal::Terminal::new(&mut selected_bytes, area);
        super::BcodeRuntimePresenter::new(&mut selected_terminal)
            .present(&mut model)
            .expect("selected frame presents");
        let selected_cell = selected_terminal
            .retained_buffer()
            .and_then(|buffer| {
                buffer.get(bmux_tui::geometry::Point::new(
                    fragment.area.x,
                    fragment.area.y,
                ))
            })
            .expect("selected cell");

        assert_eq!(
            selected_cell.style.bg,
            model.chat.app.presented_theme().selection.bg
        );
    }

    #[tokio::test]
    async fn committed_presentation_invalidates_removed_selection_and_unpins_history() {
        use bmux_tui_runtime::Presenter;

        let mut model = root_test_model();
        let area = bmux_tui::geometry::Rect::new(0, 0, 40, 12);
        model.committed_selection.push_scope(
            bmux_tui::selection::SelectionScope::new(
                "bcode.transcript.item.removed",
                bmux_tui::geometry::Rect::new(0, 1, 12, 1),
            )
            .parent("bcode.transcript"),
        );
        model
            .committed_selection
            .push_scope(bmux_tui::selection::SelectionScope::new(
                "bcode.transcript",
                bmux_tui::geometry::Rect::new(0, 1, 40, 6),
            ));
        for fragment in bmux_tui::selection::plain_text_fragments(
            "bcode.transcript.item.removed",
            "bcode.transcript.item.1.plain",
            bmux_tui::geometry::Rect::new(0, 1, 12, 1),
            0,
            "removed",
            0,
            1,
        ) {
            model.committed_selection.push_fragment(fragment);
        }
        let _ = model.transcript_selection.handle_mouse(
            &model.committed_selection,
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(0, 1),
            ),
        );
        let _ = model.transcript_selection.handle_mouse(
            &model.committed_selection,
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(6, 1),
            ),
        );
        model.selected_plain_text = Some("removed".to_owned());
        model.chat.app.set_transcript_selection_pinned(true);

        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, area);
        super::BcodeRuntimePresenter::new(&mut terminal)
            .present(&mut model)
            .expect("replacement frame presents");

        assert!(model.selected_plain_text.is_none());
        assert_eq!(
            model.transcript_selection.phase(),
            bmux_tui::selection::SelectionGesturePhase::Idle
        );
        assert!(
            model
                .chat
                .app
                .can_trim_resident_transcript_window_for_test()
        );
        assert!(model.presentation_damage.is_full());
        assert!(!model.fast_temporal_presentation);
    }

    #[tokio::test]
    async fn active_transcript_selection_pins_resident_history() {
        let mut model = root_test_model();
        assert!(
            model
                .chat
                .app
                .can_trim_resident_transcript_window_for_test()
        );

        model.chat.app.set_transcript_selection_pinned(true);

        assert!(
            !model
                .chat
                .app
                .can_trim_resident_transcript_window_for_test()
        );
        model.chat.app.set_transcript_selection_pinned(false);
        assert!(
            model
                .chat
                .app
                .can_trim_resident_transcript_window_for_test()
        );
    }

    #[tokio::test]
    async fn clipboard_failure_reports_user_visible_status() {
        let mut model = root_test_model();

        model.report_transcript_copy_result(Err("clipboard unavailable"));

        assert_eq!(
            model.chat.app.status(),
            "copy failed: clipboard unavailable"
        );
    }

    #[test]
    fn default_ctrl_shift_c_copies_transcript_selection() {
        let keymap =
            super::super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
        let stroke = bmux_keyboard::parse_key_stroke("ctrl+shift+c").expect("valid key");

        assert_eq!(
            keymap.action_for_key(super::super::keymap::BmuxScope::Chat, stroke),
            Some(super::super::keymap::BmuxAction::TranscriptCopySelection)
        );
    }

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
        root_test_model_with_tui_config(&bcode_config::TuiConfig::default())
    }

    fn root_test_model_with_tui_config(
        tui_config: &bcode_config::TuiConfig,
    ) -> super::BcodeRuntimeModel {
        let chat = root_test_chat();
        let mut settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        settings.apply_tui_config(tui_config);
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let loop_state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        super::BcodeRuntimeModel::new(chat, settings, loop_state)
    }

    #[tokio::test]
    async fn markdown_click_is_released_to_activation_while_drag_is_consumed() {
        let mut model = root_test_model();
        let layout = super::super::render::prepare_frame(
            &mut model.chat.app,
            bmux_tui::geometry::Rect::new(0, 0, 20, 8),
        )
        .expect("layout");
        let body = layout.body();
        model.committed_layout = Some(layout);
        model
            .committed_selection
            .push_scope(bmux_tui::selection::SelectionScope::new(
                "bcode.transcript",
                body,
            ));
        for fragment in bmux_tui::selection::plain_text_fragments(
            "bcode.transcript",
            "link.label",
            bmux_tui::geometry::Rect::new(body.x, body.y, 8, 1),
            0,
            "linktext",
            0,
            1,
        ) {
            model.committed_selection.push_fragment(fragment);
        }

        assert_eq!(
            model.handle_transcript_selection_mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(body.x, body.y),
            )),
            Some(super::super::invalidation::UiInvalidation::None)
        );
        assert_eq!(
            model.handle_transcript_selection_mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(body.x, body.y),
            )),
            None
        );

        let _ = model.handle_transcript_selection_mouse(bmux_tui::event::MouseEvent::new(
            bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
            bmux_tui::geometry::Point::new(body.x, body.y),
        ));
        assert!(
            model
                .handle_transcript_selection_mouse(bmux_tui::event::MouseEvent::new(
                    bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                    bmux_tui::geometry::Point::new(body.x.saturating_add(4), body.y),
                ))
                .is_some()
        );
        assert!(
            model
                .handle_transcript_selection_mouse(bmux_tui::event::MouseEvent::new(
                    bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
                    bmux_tui::geometry::Point::new(body.x.saturating_add(4), body.y),
                ))
                .is_some()
        );
    }

    #[tokio::test]
    async fn transcript_selection_arbitrates_drag_before_other_mouse_actions() {
        let mut model = root_test_model();
        assert!(model.settings.selection_autoscroll());
        let layout = super::super::render::prepare_frame(
            &mut model.chat.app,
            bmux_tui::geometry::Rect::new(0, 0, 20, 8),
        )
        .expect("layout");
        let body = layout.body();
        model.committed_layout = Some(layout);
        model
            .committed_selection
            .push_scope(bmux_tui::selection::SelectionScope::new(
                "bcode.transcript",
                body,
            ));
        model.committed_selection.push_scope(
            bmux_tui::selection::SelectionScope::new("item", body).parent("bcode.transcript"),
        );
        for fragment in bmux_tui::selection::plain_text_fragments(
            "item",
            "item.plain",
            bmux_tui::geometry::Rect::new(0, 1, 6, 1),
            0,
            "abcdef",
            0,
            1,
        ) {
            model.committed_selection.push_fragment(fragment);
        }

        assert_eq!(
            model.handle_transcript_selection_mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(1, 1),
            )),
            Some(super::super::invalidation::UiInvalidation::None)
        );
        assert_eq!(
            model.handle_transcript_selection_mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(5, 1),
            )),
            Some(super::super::invalidation::UiInvalidation::Structural)
        );
        assert_eq!(
            model.transcript_selection.phase(),
            bmux_tui::selection::SelectionGesturePhase::Dragging
        );
    }

    #[tokio::test]
    async fn disabled_selection_autoscroll_keeps_viewport_stable() {
        let mut tui_config = bcode_config::TuiConfig::default();
        tui_config.mouse.selection_autoscroll = false;
        let mut model = root_test_model_with_tui_config(&tui_config);
        let layout = super::super::render::prepare_frame(
            &mut model.chat.app,
            bmux_tui::geometry::Rect::new(0, 0, 20, 8),
        )
        .expect("layout");
        let body = layout.body();
        model.committed_layout = Some(layout);
        model
            .committed_selection
            .push_scope(bmux_tui::selection::SelectionScope::new(
                "bcode.transcript",
                body,
            ));
        model.committed_selection.push_scope(
            bmux_tui::selection::SelectionScope::new("item", body).parent("bcode.transcript"),
        );
        for fragment in bmux_tui::selection::plain_text_fragments(
            "item",
            "item.plain",
            bmux_tui::geometry::Rect::new(body.x, body.y, body.width, 1),
            0,
            "selection",
            0,
            1,
        ) {
            model.committed_selection.push_fragment(fragment);
        }

        let _ = model.handle_transcript_selection_mouse(bmux_tui::event::MouseEvent::new(
            bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
            bmux_tui::geometry::Point::new(body.x, body.y),
        ));
        assert_eq!(
            model.handle_transcript_selection_mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(body.x.saturating_add(4), body.y),
            )),
            Some(super::super::invalidation::UiInvalidation::Paint)
        );
    }

    #[test]
    fn response_local_selection_clamps_before_sibling_transcript_items() {
        let mut scene = bmux_tui::selection::SelectionScene::new();
        let body = bmux_tui::geometry::Rect::new(0, 0, 12, 3);
        scene.push_scope(bmux_tui::selection::SelectionScope::new(
            "bcode.transcript",
            body,
        ));
        for (scope, content, y, order, text) in [
            ("item.first", "item.first.plain", 0, 0, "first"),
            ("item.second", "item.second.plain", 2, 1, "second"),
        ] {
            scene.push_scope(
                bmux_tui::selection::SelectionScope::new(
                    scope,
                    bmux_tui::geometry::Rect::new(0, y, 12, 1),
                )
                .parent("bcode.transcript")
                .order(order),
            );
            for fragment in bmux_tui::selection::plain_text_fragments(
                scope,
                content,
                bmux_tui::geometry::Rect::new(0, y, 12, 1),
                order,
                text,
                0,
                0,
            ) {
                scene.push_fragment(fragment);
            }
        }

        let mut controller = bmux_tui::selection::SelectionController::new();
        assert_eq!(
            controller.handle_mouse(
                &scene,
                bmux_tui::event::MouseEvent::new(
                    bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                    bmux_tui::geometry::Point::new(1, 0),
                ),
            ),
            bmux_tui::selection::SelectionOutcome::Armed
        );
        let _ = controller.handle_mouse(
            &scene,
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(5, 2),
            ),
        );

        let snapshot = controller.snapshot(&scene).expect("selection snapshot");
        assert_eq!(snapshot.scope_id.as_str(), "item.first");
        assert!(
            snapshot
                .slices
                .iter()
                .all(|slice| slice.content_id.as_str() == "item.first.plain")
        );
    }

    #[test]
    fn transcript_chrome_selection_spans_items_in_document_order() {
        let mut scene = bmux_tui::selection::SelectionScene::new();
        let body = bmux_tui::geometry::Rect::new(0, 0, 12, 3);
        scene.push_scope(bmux_tui::selection::SelectionScope::new(
            "bcode.transcript",
            body,
        ));
        for (scope, content, y, order, text) in [
            ("item.first", "item.first.plain", 0, 0, "first"),
            ("item.second", "item.second.plain", 2, 1, "second"),
        ] {
            scene.push_scope(
                bmux_tui::selection::SelectionScope::new(
                    scope,
                    bmux_tui::geometry::Rect::new(0, y, 12, 1),
                )
                .parent("bcode.transcript")
                .order(order),
            );
            for fragment in bmux_tui::selection::plain_text_fragments(
                scope,
                content,
                bmux_tui::geometry::Rect::new(0, y, 12, 1),
                order,
                text,
                0,
                0,
            ) {
                scene.push_fragment(fragment);
            }
        }

        let mut controller = bmux_tui::selection::SelectionController::new();
        let _ = controller.handle_mouse(
            &scene,
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(0, 1),
            ),
        );
        let _ = controller.handle_mouse(
            &scene,
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(5, 2),
            ),
        );

        let snapshot = controller.snapshot(&scene).expect("selection snapshot");
        assert_eq!(snapshot.scope_id.as_str(), "bcode.transcript");
        assert_eq!(snapshot.slices.len(), 2);
        assert_eq!(snapshot.slices[0].content_id.as_str(), "item.first.plain");
        assert_eq!(snapshot.slices[1].content_id.as_str(), "item.second.plain");
    }

    #[tokio::test]
    async fn transcript_selection_scene_is_registered_before_highlight_projection() {
        let mut model = root_test_model();
        let layout = super::super::render::prepare_frame(
            &mut model.chat.app,
            bmux_tui::geometry::Rect::new(0, 0, 20, 8),
        )
        .expect("layout");

        let scene = super::transcript_selection_scene(&model.chat.app, layout.body());

        assert!(
            scene
                .scopes()
                .iter()
                .any(|scope| scope.id.as_str() == "bcode.transcript")
        );
        assert!(scene.validate().is_ok());
    }

    #[test]
    fn transcript_selection_edge_scroll_intensity_is_bounded() {
        let body = bmux_tui::geometry::Rect::new(2, 3, 20, 4);

        assert_eq!(
            super::selection_edge_scroll_rows(body, bmux_tui::geometry::Point::new(3, body.y),),
            Some((true, 1))
        );
        assert_eq!(
            super::selection_edge_scroll_rows(body, bmux_tui::geometry::Point::new(3, 0)),
            Some((true, 4))
        );
        assert_eq!(
            super::selection_edge_scroll_rows(
                body,
                bmux_tui::geometry::Point::new(3, body.bottom()),
            ),
            Some((false, 2))
        );
        assert_eq!(
            super::selection_edge_scroll_rows(
                body,
                bmux_tui::geometry::Point::new(3, body.y.saturating_add(1)),
            ),
            None
        );
    }

    #[test]
    fn plain_transcript_selection_exports_canonical_item_order() {
        let first = super::super::transcript::TranscriptItem::new("You", "first".to_owned());
        let second =
            super::super::transcript::TranscriptItem::new("Assistant", "second".to_owned());
        let first_id = first.id().get();
        let second_id = second.id().get();
        let items = [first, second];
        let snapshot = bmux_tui::selection::SelectionSnapshot {
            scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
            anchor: bmux_tui::selection::SelectionEndpoint {
                scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                content_id: bmux_tui::selection::SelectionContentId::new(format!(
                    "bcode.transcript.item.{second_id}.plain"
                )),
                offset: 6,
                order: 1,
                affinity: bmux_tui::selection::SelectionAffinity::After,
                revision: 0,
            },
            focus: bmux_tui::selection::SelectionEndpoint {
                scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                content_id: bmux_tui::selection::SelectionContentId::new(format!(
                    "bcode.transcript.item.{first_id}.plain"
                )),
                offset: 0,
                order: 0,
                affinity: bmux_tui::selection::SelectionAffinity::Before,
                revision: 0,
            },
            reversed: true,
            slices: vec![
                bmux_tui::selection::SelectionSlice {
                    content_id: bmux_tui::selection::SelectionContentId::new(format!(
                        "bcode.transcript.item.{second_id}.plain"
                    )),
                    source_range: 0..6,
                    revision: 0,
                },
                bmux_tui::selection::SelectionSlice {
                    content_id: bmux_tui::selection::SelectionContentId::new(format!(
                        "bcode.transcript.item.{first_id}.plain"
                    )),
                    source_range: 0..5,
                    revision: 0,
                },
            ],
            visible_highlights: Vec::new(),
        };

        assert_eq!(
            super::export_plain_transcript_selection(items.iter(), &snapshot).as_deref(),
            Some("first\n\nsecond")
        );
    }

    #[test]
    fn mixed_canonical_and_ephemeral_selection_exports_visible_order() {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence, text: &str| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: text.to_owned(),
            },
        };
        let mut app = BmuxApp::new_with_history(Some(session_id), &[event(1, "first")], &[], false);
        app.push_ephemeral_system_plain("local issue".to_owned());
        app.absorb_session_event(&event(2, "second"));
        let slices = app
            .transcript()
            .iter()
            .map(|item| bmux_tui::selection::SelectionSlice {
                content_id: bmux_tui::selection::SelectionContentId::new(format!(
                    "bcode.transcript.item.{}.plain",
                    item.id().get()
                )),
                source_range: 0..item.text().len(),
                revision: item.revision(),
            })
            .collect::<Vec<_>>();
        let first = &slices[0];
        let last = &slices[2];
        let snapshot = bmux_tui::selection::SelectionSnapshot {
            scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
            anchor: bmux_tui::selection::SelectionEndpoint {
                scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                content_id: first.content_id.clone(),
                offset: 0,
                order: 0,
                affinity: bmux_tui::selection::SelectionAffinity::Before,
                revision: first.revision,
            },
            focus: bmux_tui::selection::SelectionEndpoint {
                scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                content_id: last.content_id.clone(),
                offset: last.source_range.end,
                order: 2,
                affinity: bmux_tui::selection::SelectionAffinity::After,
                revision: last.revision,
            },
            reversed: false,
            slices,
            visible_highlights: Vec::new(),
        };

        assert_eq!(
            super::export_plain_transcript_selection(app.transcript(), &snapshot).as_deref(),
            Some("first\n\nlocal issue\n\nsecond")
        );
    }

    #[test]
    fn pending_tool_draft_export_is_explicit_and_uses_bounded_fallback() {
        let draft = bcode_session_view_models::ToolRequestDraftView {
            output_location: None,
            turn_id: "turn-1".to_owned(),
            tool_call_id: "call-1".to_owned(),
            tool_name: "shell".to_owned(),
            producer_plugin_id: Some("shell-plugin".to_owned()),
            schema: "shell.request".to_owned(),
            schema_version: 1,
            placement: bcode_session_models::ToolContributionPlacement::Request,
            generation: 1,
            revision: 1,
            argument_bytes: 100,
            preview_start_offset: 0,
            preview: "secret preview".to_owned(),
            truncated: true,
        };
        let item = super::super::transcript::TranscriptItem::with_kind(
            "Tool",
            "shell request · 100 bytes · truncated".to_owned(),
            true,
            super::super::transcript::TranscriptItemKind::ToolRequestDraft {
                draft: Box::new(draft),
            },
        );
        let item_id = item.id().get();
        let content_id = format!("bcode.transcript.item.{item_id}.plain");
        let snapshot = bmux_tui::selection::SelectionSnapshot {
            scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
            anchor: bmux_tui::selection::SelectionEndpoint {
                scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                content_id: bmux_tui::selection::SelectionContentId::new(content_id.clone()),
                offset: 0,
                order: 0,
                affinity: bmux_tui::selection::SelectionAffinity::Before,
                revision: 0,
            },
            focus: bmux_tui::selection::SelectionEndpoint {
                scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                content_id: bmux_tui::selection::SelectionContentId::new(content_id.clone()),
                offset: item.text.len(),
                order: 0,
                affinity: bmux_tui::selection::SelectionAffinity::After,
                revision: 0,
            },
            reversed: false,
            slices: vec![bmux_tui::selection::SelectionSlice {
                content_id: bmux_tui::selection::SelectionContentId::new(content_id),
                source_range: 0..item.text.len(),
                revision: 0,
            }],
            visible_highlights: Vec::new(),
        };
        let exported = super::export_plain_transcript_selection(std::iter::once(&item), &snapshot)
            .expect("draft export");

        assert!(exported.starts_with("Tool request draft [pending; rendered-text fallback]\n"));
        assert!(exported.contains("100 bytes · truncated"));
        assert!(!exported.contains("secret preview"));
    }

    #[test]
    fn selected_tool_items_export_canonical_context_and_status() {
        let request = super::super::transcript::tool_request_item(
            "call-1",
            Some("filesystem"),
            "read_file",
            r#"{"path":"src/lib.rs"}"#,
            None,
        )
        .with_event_metadata(1, 123);
        let result = super::super::transcript::tool_result_item(
            "call-1",
            Some("read_file"),
            Some(r#"{"path":"src/lib.rs"}"#),
            "contents",
            false,
        )
        .with_event_metadata(2, 456);
        let request_id = request.id().get();
        let result_id = result.id().get();
        let snapshot = bmux_tui::selection::SelectionSnapshot {
            scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
            anchor: bmux_tui::selection::SelectionEndpoint {
                scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                content_id: bmux_tui::selection::SelectionContentId::new(format!(
                    "bcode.transcript.item.{request_id}.plain"
                )),
                offset: 0,
                order: 0,
                affinity: bmux_tui::selection::SelectionAffinity::Before,
                revision: 0,
            },
            focus: bmux_tui::selection::SelectionEndpoint {
                scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                content_id: bmux_tui::selection::SelectionContentId::new(format!(
                    "bcode.transcript.item.{result_id}.plain"
                )),
                offset: result.text.len(),
                order: 1,
                affinity: bmux_tui::selection::SelectionAffinity::After,
                revision: 0,
            },
            reversed: false,
            slices: vec![
                bmux_tui::selection::SelectionSlice {
                    content_id: bmux_tui::selection::SelectionContentId::new(format!(
                        "bcode.transcript.item.{result_id}.plain"
                    )),
                    source_range: 0..result.text.len(),
                    revision: 0,
                },
                bmux_tui::selection::SelectionSlice {
                    content_id: bmux_tui::selection::SelectionContentId::new(format!(
                        "bcode.transcript.item.{request_id}.plain"
                    )),
                    source_range: 0..request.text.len(),
                    revision: 0,
                },
            ],
            visible_highlights: Vec::new(),
        };
        let exported = super::export_plain_transcript_selection([&request, &result], &snapshot)
            .expect("tool export");

        assert!(exported.starts_with("Tool request: read_file [requested] @ 123ms"));
        assert!(exported.contains("src/lib.rs"));
        assert!(exported.contains("Tool result: read_file [finished] @ 456ms\ncontents"));
    }

    #[test]
    fn markdown_plain_ranges_export_exact_delimiters_and_link_destination() {
        let source = "**strong** [label](https://example.com)";
        let item = super::super::transcript::TranscriptItem::with_format(
            "Assistant",
            source.to_owned(),
            bcode_session_view_models::TextFormat::Markdown,
        );
        let item_id = item.id().get();
        let export = |range: std::ops::Range<usize>| {
            let content_id = format!("bcode.transcript.item.{item_id}.markdown");
            let snapshot = bmux_tui::selection::SelectionSnapshot {
                scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                anchor: bmux_tui::selection::SelectionEndpoint {
                    scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                    content_id: bmux_tui::selection::SelectionContentId::new(content_id.clone()),
                    offset: range.start,
                    order: 0,
                    affinity: bmux_tui::selection::SelectionAffinity::Before,
                    revision: 0,
                },
                focus: bmux_tui::selection::SelectionEndpoint {
                    scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                    content_id: bmux_tui::selection::SelectionContentId::new(content_id.clone()),
                    offset: range.end,
                    order: 0,
                    affinity: bmux_tui::selection::SelectionAffinity::After,
                    revision: 0,
                },
                reversed: false,
                slices: vec![bmux_tui::selection::SelectionSlice {
                    content_id: bmux_tui::selection::SelectionContentId::new(content_id),
                    source_range: range,
                    revision: 0,
                }],
                visible_highlights: Vec::new(),
            };
            super::export_plain_transcript_selection(std::iter::once(&item), &snapshot)
        };

        assert_eq!(export(0..10).as_deref(), Some("**strong**"));
        assert_eq!(export(2..8).as_deref(), Some("strong"));
        assert_eq!(export(12..17).as_deref(), Some("label"));
        assert_eq!(export(19..38).as_deref(), Some("https://example.com"));
    }

    #[test]
    fn markdown_code_body_selection_exports_exact_canonical_bytes() {
        let source = "```rust\r\nfn main() {\r\n\tprintln!(\"hi\");\r\n}\r\n```\r\n";
        let item = super::super::transcript::TranscriptItem::with_format(
            "Assistant",
            source.to_owned(),
            bcode_session_view_models::TextFormat::Markdown,
        );
        let item_id = item.id().get();
        let snapshot = bmux_tui::selection::SelectionSnapshot {
            scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
            anchor: bmux_tui::selection::SelectionEndpoint {
                scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                content_id: bmux_tui::selection::SelectionContentId::new(format!(
                    "bcode.transcript.item.{item_id}.markdown.code.0.body"
                )),
                offset: 0,
                order: 0,
                affinity: bmux_tui::selection::SelectionAffinity::Before,
                revision: 0,
            },
            focus: bmux_tui::selection::SelectionEndpoint {
                scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                content_id: bmux_tui::selection::SelectionContentId::new(format!(
                    "bcode.transcript.item.{item_id}.markdown.code.0.body"
                )),
                offset: source.len(),
                order: 1,
                affinity: bmux_tui::selection::SelectionAffinity::After,
                revision: 0,
            },
            reversed: false,
            slices: vec![bmux_tui::selection::SelectionSlice {
                content_id: bmux_tui::selection::SelectionContentId::new(format!(
                    "bcode.transcript.item.{item_id}.markdown.code.0.body"
                )),
                source_range: 0..source.len(),
                revision: 0,
            }],
            visible_highlights: Vec::new(),
        };

        assert_eq!(
            super::export_plain_transcript_selection(std::iter::once(&item), &snapshot).as_deref(),
            Some("fn main() {\r\n\tprintln!(\"hi\");\r\n}\r\n")
        );
    }

    #[test]
    fn transcript_export_preserves_indented_and_incomplete_code_source() {
        for (source, expected) in [
            ("    first\n\tsecond\n\nnext", "    first\n\tsecond\n"),
            ("```\npartial", "partial"),
        ] {
            let item = super::super::transcript::TranscriptItem::with_format(
                "Assistant",
                source.to_owned(),
                bcode_session_view_models::TextFormat::Markdown,
            );
            let item_id = item.id().get();
            let content_id = format!("bcode.transcript.item.{item_id}.markdown.code.0.body");
            let snapshot = bmux_tui::selection::SelectionSnapshot {
                scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                anchor: bmux_tui::selection::SelectionEndpoint {
                    scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                    content_id: bmux_tui::selection::SelectionContentId::new(content_id.clone()),
                    offset: 0,
                    order: 0,
                    affinity: bmux_tui::selection::SelectionAffinity::Before,
                    revision: 0,
                },
                focus: bmux_tui::selection::SelectionEndpoint {
                    scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                    content_id: bmux_tui::selection::SelectionContentId::new(content_id.clone()),
                    offset: source.len(),
                    order: 0,
                    affinity: bmux_tui::selection::SelectionAffinity::After,
                    revision: 0,
                },
                reversed: false,
                slices: vec![bmux_tui::selection::SelectionSlice {
                    content_id: bmux_tui::selection::SelectionContentId::new(content_id),
                    source_range: 0..source.len(),
                    revision: 0,
                }],
                visible_highlights: Vec::new(),
            };
            assert_eq!(
                super::export_plain_transcript_selection(std::iter::once(&item), &snapshot)
                    .as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn markdown_code_whole_selection_exports_original_fences_and_language() {
        let source = "```rust\nfn main() {}\n```\n";
        let item = super::super::transcript::TranscriptItem::with_format(
            "Assistant",
            source.to_owned(),
            bcode_session_view_models::TextFormat::Markdown,
        );
        let item_id = item.id().get();
        let content_id = format!("bcode.transcript.item.{item_id}.markdown.code.0.whole");
        let snapshot = bmux_tui::selection::SelectionSnapshot {
            scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
            anchor: bmux_tui::selection::SelectionEndpoint {
                scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                content_id: bmux_tui::selection::SelectionContentId::new(content_id.clone()),
                offset: 0,
                order: 0,
                affinity: bmux_tui::selection::SelectionAffinity::Before,
                revision: 0,
            },
            focus: bmux_tui::selection::SelectionEndpoint {
                scope_id: bmux_tui::selection::SelectionScopeId::new("bcode.transcript"),
                content_id: bmux_tui::selection::SelectionContentId::new(content_id.clone()),
                offset: source.len(),
                order: 1,
                affinity: bmux_tui::selection::SelectionAffinity::After,
                revision: 0,
            },
            reversed: false,
            slices: vec![bmux_tui::selection::SelectionSlice {
                content_id: bmux_tui::selection::SelectionContentId::new(content_id),
                source_range: 0..source.len(),
                revision: 0,
            }],
            visible_highlights: Vec::new(),
        };

        assert_eq!(
            super::export_plain_transcript_selection(std::iter::once(&item), &snapshot).as_deref(),
            Some(source)
        );
    }

    #[test]
    fn plain_transcript_rows_map_to_item_local_utf8_offsets_only_when_exact() {
        assert_eq!(
            super::transcript_plain_row_source_ranges("a界\nsecond", 2),
            vec![Some(0..4), Some(5..11)]
        );
        assert_eq!(
            super::transcript_plain_row_source_ranges("wrapped source", 2),
            vec![None, None]
        );
    }

    #[test]
    fn transcript_parent_scope_uses_committed_body_geometry() {
        let body = bmux_tui::geometry::Rect::new(3, 4, 20, 8);
        let mut scene = bmux_tui::selection::SelectionScene::new();

        super::ensure_transcript_parent_scope(&mut scene, body);
        super::ensure_transcript_parent_scope(&mut scene, body);

        assert_eq!(scene.scopes().len(), 1);
        assert_eq!(scene.scopes()[0].id.as_str(), "bcode.transcript");
        assert_eq!(scene.scopes()[0].area, body);
        assert_eq!(scene.scopes()[0].initiation_area, body);
    }

    #[tokio::test]
    async fn root_model_starts_with_empty_committed_selection_state() {
        let model = root_test_model();

        assert!(model.committed_selection.scopes().is_empty());
        assert!(model.committed_selection.fragments().is_empty());
        assert_eq!(
            model.transcript_selection.phase(),
            bmux_tui::selection::SelectionGesturePhase::Idle
        );
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
            attachment: super::super::session_flow::ChatSessionAttachment::Draft,
            event_sender,
            event_receiver,
            event_task: None,
            opening_session_progress: None,
            pending_effects: super::super::effects::TuiEffectQueue::default(),
        }
    }

    fn root_test_chat_with_history(
        session_id: bcode_session_models::SessionId,
        history: &[bcode_session_models::SessionEvent],
    ) -> super::super::session_flow::ActiveChat {
        let (event_sender, event_receiver) = super::super::history_flow::session_stream_channel();
        super::super::session_flow::ActiveChat {
            app: super::super::app::BmuxApp::new_with_history(
                Some(session_id),
                history,
                &[],
                false,
            ),
            agents: super::super::session_flow::AgentCatalog::default(),
            attachment: super::super::session_flow::ChatSessionAttachment::Attached { session_id },
            event_sender,
            event_receiver,
            event_task: None,
            opening_session_progress: None,
            pending_effects: super::super::effects::TuiEffectQueue::default(),
        }
    }

    fn root_test_model_with_history(
        session_id: bcode_session_models::SessionId,
        history: &[bcode_session_models::SessionEvent],
    ) -> super::BcodeRuntimeModel {
        let chat = root_test_chat_with_history(session_id, history);
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

    fn root_test_chat() -> super::super::session_flow::ActiveChat {
        root_test_chat_with_input_history(&[])
    }

    async fn question_surface_with_questions_for_root_test(
        keymap: &super::super::keymap::BmuxKeyMap,
        questions: serde_json::Value,
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
            &serde_json::json!({ "questions": questions }).to_string(),
            keymap,
        )
        .await
        .expect("question surface")
    }

    async fn question_surface_for_root_test(
        keymap: &super::super::keymap::BmuxKeyMap,
    ) -> super::super::interactive_surface::InteractiveSurfaceState {
        question_surface_with_questions_for_root_test(
            keymap,
            serde_json::json!([{
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
            }]),
        )
        .await
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
            super::super::interactive_surface::InteractiveSurfaceEventOutcome::Redraw
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
                    super::super::interactive_surface::InteractiveSurfaceEventOutcome::Redraw
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
    #[allow(clippy::too_many_lines)] // One matrix keeps semantic inputs and both production placements identical.
    async fn normal_question_path_covers_radio_checkbox_custom_validation_and_both_placements() {
        for placement in [
            bcode_config::TuiInteractionPlacement::Transcript,
            bcode_config::TuiInteractionPlacement::Pinned,
        ] {
            let mut model = root_test_model();
            let mut config = model.chat.app.tui_config().clone();
            config.interactions.placement = placement;
            model.chat.app.apply_tui_config(config);
            let keymap = super::super::keymap::BmuxKeyMap::from_config(model.chat.app.tui_config());
            model.loop_state.install_interactive_surface_for_test(
                question_surface_with_questions_for_root_test(
                    &keymap,
                    serde_json::json!([
                        {
                            "header": null,
                            "question": "Choose radio",
                            "options": [
                                {"label": "One", "value": "one", "description": null},
                                {"label": "Two", "value": "two", "description": null}
                            ],
                            "control": "radio",
                            "selection_mode": "single",
                            "custom": false,
                            "custom_mode": "additional",
                            "required": true
                        },
                        {
                            "header": null,
                            "question": "Choose checks",
                            "options": [
                                {"label": "Alpha", "value": "alpha", "description": null},
                                {"label": "Beta", "value": "beta", "description": null}
                            ],
                            "control": "checkbox",
                            "selection_mode": "multiple",
                            "custom": false,
                            "custom_mode": "additional",
                            "required": false
                        },
                        {
                            "header": null,
                            "question": "Explain",
                            "options": [],
                            "control": "radio",
                            "selection_mode": "single",
                            "custom": true,
                            "custom_mode": "additional",
                            "required": true
                        }
                    ]),
                )
                .await,
            );
            let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
            let mut bytes = Vec::new();
            let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, area);
            bmux_tui_runtime::Presenter::present(
                &mut super::BcodeRuntimePresenter::new(&mut terminal),
                &mut model,
            )
            .expect("initial question matrix frame");

            let event = |key| {
                bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                    bmux_keyboard::KeyStroke {
                        key,
                        modifiers: bmux_keyboard::Modifiers::NONE,
                    },
                ))
            };
            for key in [
                bmux_keyboard::KeyCode::Enter,
                bmux_keyboard::KeyCode::Tab,
                bmux_keyboard::KeyCode::Space,
                bmux_keyboard::KeyCode::Tab,
                bmux_keyboard::KeyCode::Tab,
                bmux_keyboard::KeyCode::Char('x'),
            ] {
                bmux_tui_runtime::Program::update(&mut model, event(key))
                    .expect("question matrix input");
                bmux_tui_runtime::Presenter::present(
                    &mut super::BcodeRuntimePresenter::new(&mut terminal),
                    &mut model,
                )
                .expect("question matrix committed presentation");
            }
        }

        let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        let mut validation_surface = question_surface_with_questions_for_root_test(
            settings.keymap(),
            serde_json::json!([{
                "header": null,
                "question": "Required",
                "options": [],
                "control": "radio",
                "selection_mode": "single",
                "custom": true,
                "custom_mode": "additional",
                "required": true
            }]),
        )
        .await;
        let before = validation_surface.preferred_height(40);
        assert!(matches!(
            validation_surface.handle_event_outcome(&bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke {
                    key: bmux_keyboard::KeyCode::Enter,
                    modifiers: bmux_keyboard::Modifiers::NONE,
                },
            )),
            super::super::interactive_surface::InteractiveSurfaceEventOutcome::Relayout
        ));
        assert!(validation_surface.preferred_height(40) > before);
    }

    #[tokio::test]
    async fn interactive_surface_navigation_repaints_without_composer_history_fallthrough() {
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
            super::super::interactive_surface::InteractiveSurfaceEventOutcome::Redraw
        ));
        assert!(matches!(
            model
                .loop_state
                .handle_interactive_surface_event(&key(bmux_keyboard::KeyCode::Tab)),
            super::super::interactive_surface::InteractiveSurfaceEventOutcome::Redraw
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
        assert_eq!(
            model.invalidation,
            super::super::invalidation::UiInvalidation::Paint
        );
        assert!(model.fast_temporal_presentation);
    }

    #[tokio::test]
    async fn stable_interaction_redraw_matches_full_presentation() {
        struct CountingSurface(usize);

        impl bcode_plugin_sdk::tui::PluginTuiSurface for CountingSurface {
            fn id(&self) -> &'static str {
                "counting-test"
            }

            fn title(&self) -> &'static str {
                "Counting test"
            }

            fn render(
                &mut self,
                area: bmux_tui::geometry::Rect,
                frame: &mut bmux_tui::frame::Frame<'_>,
            ) {
                frame.write_line(area, &bmux_tui::prelude::Line::from(self.0.to_string()));
            }

            fn handle_event(
                &mut self,
                event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                if matches!(event, bmux_tui::event::Event::Key(_)) {
                    self.0 = self.0.saturating_add(1);
                    bcode_plugin_sdk::tui::PluginTuiAction::Redraw
                } else {
                    bcode_plugin_sdk::tui::PluginTuiAction::None
                }
            }
        }

        fn model() -> super::BcodeRuntimeModel {
            let mut chat = root_test_chat();
            let mut config = chat.app.tui_config().clone();
            config.interactions.placement = bcode_config::TuiInteractionPlacement::Pinned;
            chat.app.apply_tui_config(config);
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
                    "counting-interaction",
                    Box::new(CountingSurface(0)),
                    settings.keymap(),
                ),
            );
            super::BcodeRuntimeModel::new(chat, settings, loop_state)
        }

        let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
        let mut partial = model();
        let mut full = model();
        let mut partial_bytes = Vec::new();
        let mut full_bytes = Vec::new();
        let mut partial_terminal = bmux_tui::terminal::Terminal::new(&mut partial_bytes, area);
        let mut full_terminal = bmux_tui::terminal::Terminal::new(&mut full_bytes, area);
        bmux_tui_runtime::Presenter::present(
            &mut super::BcodeRuntimePresenter::new(&mut partial_terminal),
            &mut partial,
        )
        .expect("initial partial presentation");
        bmux_tui_runtime::Presenter::present(
            &mut super::BcodeRuntimePresenter::new(&mut full_terminal),
            &mut full,
        )
        .expect("initial full presentation");

        let event = || {
            bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke {
                    key: bmux_keyboard::KeyCode::Down,
                    modifiers: bmux_keyboard::Modifiers::NONE,
                },
            ))
        };
        bmux_tui_runtime::Program::update(&mut partial, event()).expect("partial input");
        bmux_tui_runtime::Program::update(&mut full, event()).expect("full input");
        assert!(partial.fast_temporal_presentation);
        full.presentation_damage = bmux_tui::damage::Damage::Full;
        full.fast_temporal_presentation = false;

        bmux_tui_runtime::Presenter::present(
            &mut super::BcodeRuntimePresenter::new(&mut partial_terminal),
            &mut partial,
        )
        .expect("partial redraw");
        bmux_tui_runtime::Presenter::present(
            &mut super::BcodeRuntimePresenter::new(&mut full_terminal),
            &mut full,
        )
        .expect("full redraw");

        assert_eq!(
            partial_terminal.retained_buffer(),
            full_terminal.retained_buffer()
        );
        assert_eq!(partial_terminal.cursor(), full_terminal.cursor());
        assert_eq!(partial_terminal.hits(), full_terminal.hits());
    }

    #[tokio::test]
    async fn stable_question_redraw_is_independent_of_unrelated_transcript_length() {
        async fn model(history_items: usize) -> super::BcodeRuntimeModel {
            let session_id = bcode_session_models::SessionId::new();
            let history = (1..=u64::try_from(history_items).unwrap_or(u64::MAX))
                .map(|sequence| bcode_session_models::SessionEvent {
                    schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    sequence,
                    timestamp_ms: sequence,
                    session_id,
                    provenance: None,
                    kind: bcode_session_models::SessionEventKind::UserMessage {
                        client_id: bcode_session_models::ClientId::new(),
                        text: format!("unrelated transcript message {sequence}"),
                        admission: bcode_session_models::TurnAdmissionMetadata::default(),
                    },
                })
                .collect::<Vec<_>>();
            let mut model = root_test_model_with_history(session_id, &history);
            let mut config = model.chat.app.tui_config().clone();
            config.interactions.placement = bcode_config::TuiInteractionPlacement::Pinned;
            model.chat.app.apply_tui_config(config);
            let keymap = super::super::keymap::BmuxKeyMap::from_config(model.chat.app.tui_config());
            model.loop_state.install_interactive_surface_for_test(
                question_surface_with_questions_for_root_test(
                    &keymap,
                    serde_json::json!([{
                        "header": null,
                        "question": "Choose one",
                        "options": [
                            {"label": "One", "value": "one", "description": null},
                            {"label": "Two", "value": "two", "description": "Second choice"}
                        ],
                        "control": "radio",
                        "selection_mode": "single",
                        "custom": false,
                        "custom_mode": "additional",
                        "required": true
                    }]),
                )
                .await,
            );
            model
        }

        let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
        let mut short = Box::pin(model(0)).await;
        let mut long = Box::pin(model(1_000)).await;
        let mut short_bytes = Vec::new();
        let mut long_bytes = Vec::new();
        let mut short_terminal = bmux_tui::terminal::Terminal::new(&mut short_bytes, area);
        let mut long_terminal = bmux_tui::terminal::Terminal::new(&mut long_bytes, area);
        bmux_tui_runtime::Presenter::present(
            &mut super::BcodeRuntimePresenter::new(&mut short_terminal),
            &mut short,
        )
        .expect("short initial frame");
        bmux_tui_runtime::Presenter::present(
            &mut super::BcodeRuntimePresenter::new(&mut long_terminal),
            &mut long,
        )
        .expect("long initial frame");

        let event = || {
            bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke {
                    key: bmux_keyboard::KeyCode::Down,
                    modifiers: bmux_keyboard::Modifiers::NONE,
                },
            ))
        };
        bmux_tui_runtime::Program::update(&mut short, event()).expect("short question input");
        bmux_tui_runtime::Program::update(&mut long, event()).expect("long question input");
        assert!(short.fast_temporal_presentation);
        assert!(long.fast_temporal_presentation);
        assert_eq!(short.presentation_damage, long.presentation_damage);
    }

    #[tokio::test]
    #[ignore = "manual already-built question interaction committed-presentation latency probe"]
    #[allow(clippy::too_many_lines)] // The emitted artifact keeps timing and work-shape attribution together.
    async fn question_interaction_to_committed_presentation_latency_report() {
        const SAMPLES: usize = 50;
        const LOCKED_FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(17);
        let mut model = root_test_model();
        let mut config = model.chat.app.tui_config().clone();
        config.interactions.placement = bcode_config::TuiInteractionPlacement::Pinned;
        model.chat.app.apply_tui_config(config);
        let keymap = super::super::keymap::BmuxKeyMap::from_config(model.chat.app.tui_config());
        model
            .loop_state
            .install_interactive_surface_for_test(question_surface_for_root_test(&keymap).await);
        let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, area);
        bmux_tui_runtime::Presenter::present(
            &mut super::BcodeRuntimePresenter::new(&mut terminal),
            &mut model,
        )
        .expect("initial question frame");

        let request_bytes = serde_json::to_vec(&serde_json::json!({
            "questions": [{
                "header": null,
                "question": "Choose one",
                "options": [
                    {"label": "One", "value": "one", "description": null},
                    {"label": "Two", "value": "two", "description": "Second choice"}
                ],
                "control": "radio",
                "selection_mode": "single",
                "custom": false,
                "custom_mode": "additional",
                "required": true
            }]
        }))
        .expect("question fixture serialization")
        .len();
        let initial_geometry = model
            .loop_state
            .active_interactive_surface_geometry()
            .expect("committed question geometry");
        let initial_height = initial_geometry.logical_height;
        let visible_rows = initial_geometry.destination.height;
        let mut samples = Vec::with_capacity(SAMPLES);
        let mut draw_samples = Vec::with_capacity(SAMPLES);
        let mut changed_cells = Vec::with_capacity(SAMPLES);
        let mut full_repaints = 0_usize;
        for index in 0..SAMPLES {
            let started = std::time::Instant::now();
            let event = bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke {
                    key: if index.is_multiple_of(2) {
                        bmux_keyboard::KeyCode::Down
                    } else {
                        bmux_keyboard::KeyCode::Up
                    },
                    modifiers: bmux_keyboard::Modifiers::NONE,
                },
            ));
            bmux_tui_runtime::Program::update(&mut model, event).expect("question input");
            let draw_started = std::time::Instant::now();
            let report = bmux_tui_runtime::Presenter::present(
                &mut super::BcodeRuntimePresenter::new(&mut terminal),
                &mut model,
            )
            .expect("question presentation");
            samples.push(started.elapsed());
            draw_samples.push(draw_started.elapsed());
            changed_cells.push(report.changed_cells);
            full_repaints = full_repaints.saturating_add(usize::from(report.full_repaint));
        }
        let work_shape = model
            .loop_state
            .active_interactive_surface_work_shape_for_test();
        let summary = latency_summary(&samples);
        let draw_summary = latency_summary(&draw_samples);
        eprintln!(
            "{}",
            serde_json::json!({
                "kind": "bcode_question_interaction_committed_presentation_latency",
                "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
                "sample_count": SAMPLES,
                "locked_p99_budget_ms": LOCKED_FRAME_BUDGET.as_secs_f64() * 1_000.0,
                "latency": summary,
                "draw_latency": draw_summary,
                "work_shape": {
                    "fixture_request_bytes": request_bytes,
                    "snapshot_materializations": work_shape.snapshot_materializations,
                    "copied_request_bytes": request_bytes * usize::try_from(work_shape.snapshot_materializations).unwrap_or(usize::MAX),
                    "preferred_height_measurements": work_shape.preferred_height_measurements,
                    "focused_row_measurements": work_shape.focused_row_measurements,
                    "wrapped_rows_visited": work_shape.wrapped_rows_visited,
                    "semantic_events": SAMPLES,
                    "logical_rows": initial_height,
                    "visible_rows_rendered_per_frame": visible_rows,
                    "broad_frame_preparations": 0,
                    "transcript_entries_scanned": 0,
                    "transcript_entries_rebuilt": 0,
                    "transcript_rows_regenerated": 0,
                    "scheduling_delay_ms": 0
                },
                "changed_cells": changed_cells,
                "full_repaints": full_repaints,
            })
        );
        assert!(
            summary["p99_ms"].as_f64().expect("numeric p99")
                <= LOCKED_FRAME_BUDGET.as_secs_f64() * 1_000.0
        );
        assert_eq!(full_repaints, 0);
    }

    #[tokio::test]
    #[ignore = "manual inline-question transcript-scroll committed-presentation probe"]
    #[allow(clippy::too_many_lines)] // The probe keeps fixture setup and per-frame attribution together.
    async fn inline_question_transcript_scroll_performance_report() {
        const SAMPLES: usize = 30;
        const LOCKED_FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(17);
        let session_id = bcode_session_models::SessionId::new();
        let history = (1..=1_000)
            .map(|sequence| bcode_session_models::SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence,
                timestamp_ms: sequence,
                session_id,
                provenance: None,
                kind: bcode_session_models::SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: format!("scroll fixture transcript message {sequence}"),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            })
            .collect::<Vec<_>>();
        let mut model = root_test_model_with_history(session_id, &history);
        let mut config = model.chat.app.tui_config().clone();
        config.interactions.placement = bcode_config::TuiInteractionPlacement::Transcript;
        model.chat.app.apply_tui_config(config);
        let questions = (0..20)
            .map(|index| {
                serde_json::json!({
                    "header": format!("Question {index}"),
                    "question": format!(
                        "Choose an answer for a deliberately long question number {index} while scrolling"
                    ),
                    "options": [
                        {"label": "One long answer", "value": "one", "description": "A deliberately long option description that wraps in the inline viewport"},
                        {"label": "Two long answer", "value": "two", "description": "Another deliberately long option description that wraps in the inline viewport"}
                    ],
                    "control": "radio",
                    "selection_mode": "single",
                    "custom": false,
                    "custom_mode": "additional",
                    "required": true
                })
            })
            .collect::<Vec<_>>();
        let question_snapshot = serde_json::json!({ "questions": questions });
        model.chat.app.set_pending_interactions(vec![
            bcode_session_view_models::InteractionViewSummary {
                interaction_id: "question-root-test".to_owned(),
                producer_id: Some("bcode.question".to_owned()),
                exchange_schema: Some("bcode.question.request".to_owned()),
                exchange_schema_version: Some(1),
                kind: "bcode.question".to_owned(),
                tool_call_id: Some("call-question-scroll".to_owned()),
                title: Some("Question".to_owned()),
                required: true,
                snapshot: Some(question_snapshot.clone()),
                state: bcode_session_view_models::InteractionViewState::Pending,
                status_detail: None,
                resolved: false,
                resolution: None,
            },
        ]);
        let keymap = super::super::keymap::BmuxKeyMap::from_config(model.chat.app.tui_config());
        model.loop_state.install_interactive_surface_for_test(
            question_surface_with_questions_for_root_test(
                &keymap,
                question_snapshot["questions"].clone(),
            )
            .await,
        );
        let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, area);
        bmux_tui_runtime::Presenter::present(
            &mut super::BcodeRuntimePresenter::new(&mut terminal),
            &mut model,
        )
        .expect("initial inline question scroll frame");

        let initial_geometry = model.loop_state.active_interactive_surface_geometry();
        let surface_work_shape = model
            .loop_state
            .active_interactive_surface_work_shape_for_test();
        let mut total_samples = Vec::with_capacity(SAMPLES);
        let mut draw_samples = Vec::with_capacity(SAMPLES);
        let mut changed_cells = Vec::with_capacity(SAMPLES);
        let mut scroll_offsets = Vec::with_capacity(SAMPLES);
        let mut fast_path_frames = 0_usize;
        let mut full_repaints = 0_usize;
        for index in 0..SAMPLES {
            let started = std::time::Instant::now();
            let mouse = bmux_tui::event::MouseEvent::new(
                if index.is_multiple_of(2) {
                    bmux_tui::event::MouseEventKind::ScrollUp
                } else {
                    bmux_tui::event::MouseEventKind::ScrollDown
                },
                bmux_tui::geometry::Point::new(2, 2),
            );
            bmux_tui_runtime::Program::update(
                &mut model,
                bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Mouse(mouse)),
            )
            .expect("inline question scroll input");
            fast_path_frames =
                fast_path_frames.saturating_add(usize::from(model.fast_temporal_presentation));
            scroll_offsets.push(model.chat.app.scroll_offset());
            let draw_started = std::time::Instant::now();
            let report = bmux_tui_runtime::Presenter::present(
                &mut super::BcodeRuntimePresenter::new(&mut terminal),
                &mut model,
            )
            .expect("inline question scroll presentation");
            total_samples.push(started.elapsed());
            draw_samples.push(draw_started.elapsed());
            changed_cells.push(report.changed_cells);
            full_repaints = full_repaints.saturating_add(usize::from(report.full_repaint));
        }
        let total = latency_summary(&total_samples);
        let draw = latency_summary(&draw_samples);
        let within_budget = total["p99_ms"].as_f64().expect("numeric p99")
            <= LOCKED_FRAME_BUDGET.as_secs_f64() * 1_000.0;
        eprintln!(
            "{}",
            serde_json::json!({
                "kind": "bcode_inline_question_transcript_scroll_performance",
                "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
                "sample_count": SAMPLES,
                "resident_transcript_events": history.len(),
                "initial_surface_geometry": format!("{initial_geometry:?}"),
                "surface_work_shape": surface_work_shape,
                "locked_p99_budget_ms": LOCKED_FRAME_BUDGET.as_secs_f64() * 1_000.0,
                "total_latency": total,
                "draw_latency": draw,
                "changed_cells": changed_cells,
                "scroll_offsets": scroll_offsets,
                "full_repaints": full_repaints,
                "fast_path_frames": fast_path_frames,
                "within_budget": within_budget
            })
        );
    }

    #[tokio::test]
    async fn stable_interaction_redraw_falls_back_without_committed_geometry() {
        struct RedrawSurface;

        impl bcode_plugin_sdk::tui::PluginTuiSurface for RedrawSurface {
            fn id(&self) -> &'static str {
                "redraw-test"
            }

            fn title(&self) -> &'static str {
                "Redraw test"
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
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw
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
        let mut loop_state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        loop_state.install_interactive_surface_for_test(
            super::super::interactive_surface::InteractiveSurfaceState::from_surface_for_test(
                "redraw-interaction",
                Box::new(RedrawSurface),
                settings.keymap(),
            ),
        );
        let mut model = super::BcodeRuntimeModel::new(chat, settings, loop_state);
        bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke {
                    key: bmux_keyboard::KeyCode::Down,
                    modifiers: bmux_keyboard::Modifiers::NONE,
                },
            )),
        )
        .expect("redraw without committed geometry");

        assert!(model.presentation_damage.is_full());
        assert!(!model.fast_temporal_presentation);
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
    async fn nested_plugin_surface_close_restores_parent_with_workflow_subscription() {
        struct TestSurface(&'static str);

        impl bcode_plugin_sdk::tui::PluginTuiSurface for TestSurface {
            fn id(&self) -> &'static str {
                self.0
            }

            fn title(&self) -> &'static str {
                self.0
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

        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let mut state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        state.queue_root_plugin_surface("bcode.workflow", Box::new(TestSurface("workflow.status")));
        let (_update_sender, updates) = tokio::sync::mpsc::channel(1);
        let (request_sender, mut requests) = tokio::sync::mpsc::channel(1);
        state.attach_root_plugin_surface_updates(updates, request_sender);

        state.queue_root_plugin_surface("bcode.workflow", Box::new(TestSurface("workflow.author")));
        assert_eq!(
            state.active_root_plugin_surface_id(),
            Some("workflow.author")
        );
        let closed = state
            .close_root_plugin_surface_with_outcome(None)
            .expect("authoring surface closes");
        assert_eq!(closed.0, "bcode.workflow");
        assert_eq!(
            state.active_root_plugin_surface_id(),
            Some("workflow.status")
        );

        state.request_root_workflow_run("run-7".to_string());
        assert!(matches!(
            requests.try_recv(),
            Ok(super::super::chat_loop::WorkflowViewRequest::SelectRun(run_id))
                if run_id == "run-7"
        ));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn managed_runtime_commits_progressive_filesystem_write_frames_without_external_wakeup() {
        let session_id = bcode_session_models::SessionId::new();
        let mut model = root_test_model();
        model.chat.mark_attached(session_id);
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
        model.chat.mark_attached(session_id);
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
        chat.mark_attached(session_id);
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
    async fn streaming_configurator_apply_cancel_and_reset_own_input_and_effects() {
        let mut apply = root_test_model();
        apply
            .loop_state
            .open_streaming_configurator(&mut apply.chat);
        assert!(apply.loop_state.handle_streaming_configurator_key(
            &mut apply.chat,
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Char('x')),
        ));
        assert!(apply.loop_state.handle_streaming_configurator_key(
            &mut apply.chat,
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Enter),
        ));
        assert!(apply.chat.pending_effects.contains_effect(
            &super::super::effects::TuiEffect::PersistStreamingPresentation {
                policy: bcode_session_view_models::StreamingPresentationPolicy::default(),
            },
        ));
        assert!(
            apply
                .loop_state
                .streaming_configurator_deadline(std::time::Instant::now())
                .is_none()
        );

        let mut cancel = root_test_model();
        cancel
            .loop_state
            .open_streaming_configurator(&mut cancel.chat);
        assert!(cancel.loop_state.handle_streaming_configurator_key(
            &mut cancel.chat,
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Escape),
        ));
        assert_eq!(cancel.chat.queued_effect_count(), 0);

        let mut reset = root_test_model();
        reset
            .loop_state
            .open_streaming_configurator(&mut reset.chat);
        for _ in 0..4 {
            assert!(reset.loop_state.handle_streaming_configurator_key(
                &mut reset.chat,
                bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Down),
            ));
        }
        assert!(reset.loop_state.handle_streaming_configurator_key(
            &mut reset.chat,
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Space),
        ));
        assert!(reset.loop_state.handle_streaming_configurator_key(
            &mut reset.chat,
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Enter),
        ));
        assert!(
            reset
                .chat
                .pending_effects
                .contains_effect(&super::super::effects::TuiEffect::ClearStreamingPresentation,)
        );
    }

    #[tokio::test]
    async fn streaming_configurator_deadline_stops_on_pause_and_close() {
        let mut model = root_test_model();
        model
            .loop_state
            .open_streaming_configurator(&mut model.chat);
        let now = std::time::Instant::now();
        assert!(
            model
                .loop_state
                .streaming_configurator_deadline(now)
                .is_some()
        );
        assert!(model.loop_state.handle_streaming_configurator_key(
            &mut model.chat,
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Char('p')),
        ));
        assert!(
            model
                .loop_state
                .streaming_configurator_deadline(now)
                .is_none()
        );
        assert!(model.loop_state.handle_streaming_configurator_key(
            &mut model.chat,
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Escape),
        ));
        assert!(
            model
                .loop_state
                .streaming_configurator_deadline(now)
                .is_none()
        );
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
