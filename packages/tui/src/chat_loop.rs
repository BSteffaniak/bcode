//! Main chat event loop for the TUI.

use bcode_plugin_sdk::path::display_from_current_dir;
use std::collections::{BTreeSet, VecDeque};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bcode_client::{BcodeClient, ClientError};
use bcode_command::CommandAction;
use bcode_config::TuiConfig;
use bcode_ipc::{ComposerDraftScope, Event as BcodeEvent};
use bcode_session_models::SessionEventKind;
use bmux_keyboard::KeyStroke;
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;

use super::activity::ActivityState;
use super::artifact_stream::{ActiveArtifactFetchCompletion, ArtifactStreamCoordinator};
use super::command_palette::BmuxCommandPalette;
use super::daemon_issue;
use super::effects::{DaemonObservation, TuiEffect, TuiEffectResult};
use super::interactive_surface::{
    InteractiveSurfaceEventOutcome, InteractiveSurfaceQueue, InteractiveSurfaceRequest,
    InteractiveSurfaceState,
};
use super::keymap::BmuxKeyMap;
use super::permission_dialog::PermissionDialogState;
use super::session_flow::{self, ActiveChat};
use super::{
    TuiError, command_palette_render, history_flow, permission_dialog_render, render,
    slash_palette, slash_palette_render, thinking_dialog_render, timeline_dialog_render,
};

const DRAFT_SAVE_DEBOUNCE: Duration = Duration::from_millis(900);
#[derive(Debug, Clone)]
pub struct DraftAutosave {
    launch_working_directory: std::path::PathBuf,
    last_seen_text: String,
    last_saved_text: Option<String>,
    dirty: bool,
    save_at: Option<Instant>,
}

impl DraftAutosave {
    pub fn new(launch_working_directory: std::path::PathBuf, initial_text: String) -> Self {
        Self {
            launch_working_directory,
            last_seen_text: initial_text.clone(),
            last_saved_text: Some(initial_text),
            dirty: false,
            save_at: None,
        }
    }

    fn scope(&self, chat: &ActiveChat) -> ComposerDraftScope {
        chat.attached_session_id().map_or_else(
            || ComposerDraftScope::DraftSession {
                launch_working_directory: self.launch_working_directory.clone(),
            },
            |session_id| ComposerDraftScope::Session { session_id },
        )
    }

    pub(super) fn observe(&mut self, chat: &ActiveChat, now: Instant) {
        let text = chat.app.composer().text();
        if text == self.last_seen_text {
            return;
        }
        text.clone_into(&mut self.last_seen_text);
        self.dirty = true;
        self.save_at = Some(now + DRAFT_SAVE_DEBOUNCE);
    }

    pub(super) fn next_save_at(&self) -> Option<Instant> {
        self.dirty.then_some(self.save_at).flatten()
    }

    pub(super) fn reset_for_session_change(&mut self) {
        self.last_saved_text = None;
        self.dirty = true;
        self.save_at = Some(Instant::now());
    }

    const fn mark_save_started(&mut self) {
        self.dirty = false;
        self.save_at = None;
    }

    fn mark_save_completed(&mut self, saved_text: String) {
        self.last_saved_text = Some(saved_text);
    }

    fn pending_save(&self, chat: &ActiveChat) -> Option<(ComposerDraftScope, String)> {
        if !self.dirty && self.last_saved_text.as_deref() == Some(chat.app.composer().text()) {
            return None;
        }
        Some((self.scope(chat), chat.app.composer().text().to_owned()))
    }

    pub(super) fn mark_dirty_now(&mut self) {
        self.dirty = true;
        self.save_at = Some(Instant::now());
    }
}

/// One command palette activation carrying its declared session applicability.
///
/// Keeping the requirement alongside the action ensures the host can enforce declared
/// applicability before dispatching any command side effect.
pub struct CommandDispatchRequest {
    pub action: CommandAction,
    pub session: bcode_command::CommandSessionRequirement,
}

impl CommandDispatchRequest {
    #[must_use]
    pub fn from_contribution(contribution: bcode_command::CommandContribution) -> Self {
        Self {
            action: contribution.action,
            session: contribution.session,
        }
    }
}

pub enum SlashPaletteRootOutcome {
    Unhandled,
    Handled,
    Submit,
}

pub enum ThinkingDialogRootOutcome {
    Unhandled,
    Handled,
    Apply {
        effort: Option<String>,
        summary: Option<String>,
        visible: bool,
        mode: bcode_config::TuiThinkingMode,
    },
}

pub enum TimelineDialogRootOutcome {
    Unhandled,
    Handled,
    Jump(super::timeline_dialog::TimelineEntry),
}

type PluginSurfaceOutcome = (String, bcode_plugin_sdk::tui::PluginTuiAction);

type PluginEffectCompletion = (
    bcode_plugin_sdk::tui::BoxedPluginTuiSurface,
    bcode_plugin_sdk::tui::PluginTuiAction,
);

/// Whether dispatched plugin effects still have a verifiable completion path.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PluginEffectIntegrity {
    Verifiable,
    UnknownEffects,
}

struct RootPluginSurface {
    plugin_id: String,
    surface: Option<bcode_plugin_sdk::tui::BoxedPluginTuiSurface>,
    drain: Option<tokio::sync::oneshot::Receiver<PluginEffectCompletion>>,
    drain_task: Option<tokio::task::JoinHandle<()>>,
    navigation_callback: Option<(bcode_session_models::SessionId, Result<(), String>)>,
    effect_integrity: PluginEffectIntegrity,
    close_after_drain: bool,
    close_outcome: Option<serde_json::Value>,
    effects_pending: bool,
    session_navigation_attached: bool,
    deferred_input: std::collections::VecDeque<Event>,
    invalidation: bmux_tui_runtime::InvalidationSignal,
    active_tasks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pending_session_navigation: Option<bcode_session_models::SessionId>,
    workflow_view_requests: Option<tokio::sync::mpsc::Sender<WorkflowViewRequest>>,
}

impl RootPluginSurface {
    fn observe_finished_drain_task(
        &mut self,
    ) -> Result<(), bcode_plugin_sdk::tui::PluginTuiAction> {
        // A task may have sent its result just before finishing its wakeup. Observe its
        // terminal result before allowing another exclusive operation to replace it.
        if self
            .drain_task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            let mut task = self.drain_task.take().expect("finished task exists");
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            match std::future::Future::poll(std::pin::Pin::new(&mut task), &mut context) {
                std::task::Poll::Ready(Ok(())) => {}
                std::task::Poll::Ready(Err(_)) => {
                    self.drain = None;
                    self.effect_integrity = PluginEffectIntegrity::UnknownEffects;
                    return Err(bcode_plugin_sdk::tui::PluginTuiAction::None);
                }
                std::task::Poll::Pending => self.drain_task = Some(task),
            }
        }
        Ok(())
    }

    fn accept_drain_completion(
        &mut self,
        active: bcode_plugin_sdk::tui::BoxedPluginTuiSurface,
        action: bcode_plugin_sdk::tui::PluginTuiAction,
        host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) -> bcode_plugin_sdk::tui::PluginTuiAction {
        self.surface = Some(active);
        self.drain = None;
        if let Some((session_id, result)) = self.navigation_callback.take() {
            self.surface
                .as_mut()
                .expect("settled surface available")
                .session_navigation_finished(session_id, result);
        }
        // Returning exclusive ownership must trigger one foreground poll so newly
        // linked subscriptions/navigation are admitted without another keypress.
        self.invalidation.request();
        if self.close_after_drain {
            if let bcode_plugin_sdk::tui::PluginTuiAction::Close { outcome } = action {
                self.close_outcome = outcome.or_else(|| self.close_outcome.take());
            }
            bcode_plugin_sdk::tui::PluginTuiAction::Close {
                outcome: self.close_outcome.take(),
            }
        } else if self.pending_session_navigation.is_some() {
            // Settlement retains the surface without changing the borrowed chat route.
            bcode_plugin_sdk::tui::PluginTuiAction::None
        } else if matches!(
            action,
            bcode_plugin_sdk::tui::PluginTuiAction::None
                | bcode_plugin_sdk::tui::PluginTuiAction::Redraw
        ) {
            // Foreground reconciliation is part of settlement, not a later user
            // event. In particular, effects can introduce observation subscriptions
            // whose first snapshot must be admitted before the next frame.
            let next = self
                .surface
                .as_mut()
                .expect("settled surface available")
                .poll(host);
            self.effects_pending |= matches!(next, bcode_plugin_sdk::tui::PluginTuiAction::Redraw);
            if matches!(next, bcode_plugin_sdk::tui::PluginTuiAction::None) {
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw
            } else {
                next
            }
        } else {
            action
        }
    }
}

struct RootModelPicker {
    generation: u64,
    provider_plugin_id: Option<String>,
    picker: super::model_picker::ModelPickerApp,
}

pub enum AuthPoolPickerRootOutcome {
    Continue,
    Cancel,
    Promote { pool: String, profile: String },
    Clear { pool: String },
}

#[derive(Debug)]
pub enum WorkflowViewRequest {
    SelectRun(String),
    UpdateQuery {
        filter: bcode_workflow_view_models::WorkflowCatalogFilter,
        sort: bcode_workflow_view_models::WorkflowCatalogSort,
        group: bcode_workflow_view_models::WorkflowCatalogGroup,
        search: Option<String>,
    },
    LoadMore(bcode_workflow_view_models::WorkflowCatalogCursor),
}

pub enum SessionPickerRootOutcome {
    Unhandled,
    Handled,
    Canceled,
    Create,
    Select(bcode_session_models::SessionId),
    SearchHit(bcode_session_search::HydratedSessionSearchHit),
}

pub enum WorkingDirectoryDialogRootOutcome {
    Unhandled,
    Handled,
    Canceled,
    Apply(std::path::PathBuf),
}

pub enum WorktreeCreateDialogRootOutcome {
    Unhandled,
    Handled,
    Canceled,
    Create {
        name: String,
        target: super::wt_create_dialog::WorktreeCreateTarget,
        base: super::wt_create_dialog::WorktreeCreateBase,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractiveSurfacePlacement {
    Transcript,
    Pinned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractiveSurfaceGeometry {
    pub placement: InteractiveSurfacePlacement,
    pub logical_height: u16,
    pub visible_logical_offset: u16,
    pub destination: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractiveSurfaceOpenCompletion {
    Opened,
    Stale,
    Failed,
}

pub struct ChatLoopState {
    render_launch: Option<super::plugin_surface_host::FreshSessionSettings>,
    palette: Option<BmuxCommandPalette>,
    pub(crate) composer_interaction: super::composer_interaction::ComposerInteraction,
    slash_palette: Option<slash_palette::SlashPalette>,
    slash_completion: slash_palette::SlashCompletionState,
    foreground_client: BcodeClient,
    passive_client: BcodeClient,
    viewport_input_task: Option<tokio::task::JoinHandle<()>>,
    viewport_input_connection:
        std::sync::Arc<tokio::sync::Mutex<Option<bcode_client::ClientConnection>>>,
    viewport_input_retry_at: Instant,
    daemon_connection: DaemonConnectionMonitor,
    pub(super) permission_dialog: Option<PermissionDialogState>,
    thinking_dialog: Option<super::thinking_dialog::ThinkingDialogState>,
    theme_picker: Option<super::theme_picker::ThemePickerState>,
    streaming_configurator: Option<super::streaming_configurator::StreamingConfiguratorState>,
    timeline_dialog: Option<super::timeline_dialog::TimelineDialogState>,
    milestone_requests: super::timeline_dialog::MilestonePageRequests,
    plugin_surface: Option<RootPluginSurface>,
    parent_plugin_surfaces: Vec<RootPluginSurface>,
    provider_picker: Option<super::provider_picker::ProviderPickerApp>,
    model_picker: Option<RootModelPicker>,
    model_picker_generation: u64,
    model_check_in_flight: bool,
    auth_pool_picker: Option<super::auth_pool_picker::AuthPoolPickerApp>,
    skill_picker: Option<super::skill_picker::SkillPickerApp>,
    working_directory_dialog: Option<super::working_directory_dialog::WorkingDirectoryDialog>,
    worktree_create_dialog: Option<super::wt_create_dialog::WorktreeCreateDialog>,
    ralph_start_dialog: Option<super::ralph_start_dialog::RalphStartDialog>,
    interactive_surface: Option<InteractiveSurfaceState>,
    interactive_surface_geometry: Option<InteractiveSurfaceGeometry>,
    pinned_interaction_viewport: Option<(String, u16)>,
    session_picker: Option<super::session_picker::SessionPickerApp>,
    session_picker_generation: u64,
    session_picker_launch_directory: std::path::PathBuf,
    interactive_surface_queue: InteractiveSurfaceQueue,
    artifact_stream: ArtifactStreamCoordinator,
    markdown_projection: super::markdown_projection_coordinator::MarkdownProjectionCoordinator,
    markdown_presentation: Option<super::markdown_image::MarkdownPresentationRuntime>,
    markdown_mermaid: Option<super::markdown_mermaid::MarkdownMermaidRuntime>,
    markdown_image_tasks:
        Vec<tokio::task::JoinHandle<super::markdown_image::MarkdownImageLoadCompletion>>,
    markdown_mermaid_tasks:
        Vec<tokio::task::JoinHandle<super::markdown_mermaid::MarkdownMermaidCompletion>>,
    modal_backdrop: Option<bmux_tui::buffer::Buffer>,
    markdown_image_compositor: bmux_image::tui::TuiImageCompositor,
    markdown_image_capabilities: bmux_image::HostImageCapabilities,
    markdown_image_config: bmux_image::ImageConfig,
    telemetry: super::telemetry::TuiTelemetry,
    runtime_stats: super::runtime_adapter::RuntimeStatsRecorder,
    request_draft_handoff: RequestDraftHandoff,
    pub(super) declarative_streaming_policy: bcode_session_view_models::StreamingPresentationPolicy,
    pub(super) streaming_presentation_override:
        Option<bcode_session_view_models::StreamingPresentationPolicy>,
    frame_index: u64,
}

impl Drop for ChatLoopState {
    fn drop(&mut self) {
        if let Some(task) = self.viewport_input_task.take() {
            task.abort();
        }
    }
}

impl ChatLoopState {
    pub fn new(
        foreground_client: &BcodeClient,
        passive_client: &BcodeClient,
        metrics_enabled: bool,
    ) -> Self {
        Self {
            render_launch: None,
            palette: None,
            slash_palette: None,
            composer_interaction: super::composer_interaction::ComposerInteraction::default(),
            slash_completion: slash_palette::SlashCompletionState::default(),
            foreground_client: foreground_client.clone(),
            passive_client: passive_client.clone(),
            viewport_input_task: None,
            viewport_input_connection: std::sync::Arc::default(),
            viewport_input_retry_at: Instant::now(),
            daemon_connection: DaemonConnectionMonitor::default(),
            permission_dialog: None,
            thinking_dialog: None,
            theme_picker: None,
            streaming_configurator: None,
            timeline_dialog: None,
            milestone_requests: super::timeline_dialog::MilestonePageRequests::default(),
            plugin_surface: None,
            parent_plugin_surfaces: Vec::new(),
            provider_picker: None,
            model_picker: None,
            model_picker_generation: 0,
            model_check_in_flight: false,
            auth_pool_picker: None,
            skill_picker: None,
            working_directory_dialog: None,
            worktree_create_dialog: None,
            ralph_start_dialog: None,
            interactive_surface: None,
            interactive_surface_geometry: None,
            pinned_interaction_viewport: None,
            session_picker: None,
            session_picker_generation: 0,
            session_picker_launch_directory: std::path::PathBuf::new(),
            interactive_surface_queue: InteractiveSurfaceQueue::default(),
            artifact_stream: ArtifactStreamCoordinator::new(passive_client.clone()),
            markdown_projection:
                super::markdown_projection_coordinator::MarkdownProjectionCoordinator::new(),
            markdown_presentation: super::markdown_image::MarkdownPresentationRuntime::new().ok(),
            markdown_mermaid: super::markdown_mermaid::MarkdownMermaidRuntime::packaged().ok(),
            markdown_image_tasks: Vec::new(),
            markdown_mermaid_tasks: Vec::new(),
            modal_backdrop: None,
            markdown_image_compositor: bmux_image::tui::TuiImageCompositor::new(),
            markdown_image_capabilities: bmux_image::host_caps::detect_from_env(),
            markdown_image_config: bmux_image::ImageConfig::default(),
            telemetry: super::telemetry::TuiTelemetry::new(passive_client.clone(), metrics_enabled),
            runtime_stats: super::runtime_adapter::RuntimeStatsRecorder::default(),
            request_draft_handoff: RequestDraftHandoff::default(),
            declarative_streaming_policy:
                bcode_session_view_models::StreamingPresentationPolicy::default(),
            streaming_presentation_override: None,
            frame_index: 0,
        }
    }

    pub fn set_render_launch(&mut self, launch: super::plugin_surface_host::FreshSessionSettings) {
        if let Some(surface) = self.plugin_surface.as_mut() {
            let host = super::plugin_surface_host::root_host(
                surface.invalidation.clone(),
                self.foreground_client.clone(),
                launch.clone(),
                std::sync::Arc::clone(&surface.active_tasks),
            );
            if let Some(surface) = surface.surface.as_mut() {
                surface.prepare_keymap(&host);
            }
        }
        self.render_launch = Some(launch);
    }

    pub fn streaming_configurator_deadline(&self, now: Instant) -> Option<Instant> {
        self.streaming_configurator
            .as_ref()
            .and_then(|configurator| configurator.next_deadline(now))
    }

    pub fn advance_streaming_configurator(&mut self, now: Instant) -> bool {
        self.streaming_configurator
            .as_mut()
            .is_some_and(|configurator| configurator.advance(now))
    }

    pub fn open_streaming_configurator(&mut self, chat: &mut ActiveChat) {
        let effective = self
            .streaming_presentation_override
            .unwrap_or(self.declarative_streaming_policy);
        self.thinking_dialog = None;
        self.theme_picker = None;
        self.timeline_dialog = None;
        self.milestone_requests.cancel();
        self.provider_picker = None;
        self.model_picker = None;
        self.auth_pool_picker = None;
        self.skill_picker = None;
        self.working_directory_dialog = None;
        self.worktree_create_dialog = None;
        self.ralph_start_dialog = None;
        self.session_picker = None;
        self.streaming_configurator = Some(
            super::streaming_configurator::StreamingConfiguratorState::new(
                Instant::now(),
                effective,
                self.declarative_streaming_policy,
            ),
        );
        chat.app.set_status("streaming configurator".to_owned());
    }

    pub fn handle_streaming_configurator_mouse(
        &mut self,
        chat: &mut ActiveChat,
        mouse: bmux_tui::event::MouseEvent,
        frame_area: Rect,
    ) -> bool {
        let Some(configurator) = self.streaming_configurator.as_mut() else {
            return false;
        };
        let _ = frame_area;
        match configurator.handle_committed_mouse(mouse, Instant::now()) {
            super::streaming_configurator::StreamingConfiguratorOutcome::Handled
            | super::streaming_configurator::StreamingConfiguratorOutcome::Ignored => true,
            super::streaming_configurator::StreamingConfiguratorOutcome::Apply(policy) => {
                self.streaming_configurator = None;
                Self::request_streaming_presentation_override(chat, policy);
                true
            }
            super::streaming_configurator::StreamingConfiguratorOutcome::Reset => {
                self.streaming_configurator = None;
                Self::request_clear_streaming_presentation_override(
                    chat,
                    self.declarative_streaming_policy,
                );
                true
            }
            super::streaming_configurator::StreamingConfiguratorOutcome::Cancel => {
                self.streaming_configurator = None;
                chat.app
                    .set_status("streaming presentation changes canceled".to_owned());
                true
            }
        }
    }

    pub fn handle_streaming_configurator_key(
        &mut self,
        chat: &mut ActiveChat,
        stroke: KeyStroke,
        keymap: &BmuxKeyMap,
    ) -> bool {
        let Some(configurator) = self.streaming_configurator.as_mut() else {
            return false;
        };
        match configurator.handle_key_with_keymap(stroke, Instant::now(), keymap) {
            super::streaming_configurator::StreamingConfiguratorOutcome::Handled
            | super::streaming_configurator::StreamingConfiguratorOutcome::Ignored => true,
            super::streaming_configurator::StreamingConfiguratorOutcome::Apply(policy) => {
                self.streaming_configurator = None;
                Self::request_streaming_presentation_override(chat, policy);
                true
            }
            super::streaming_configurator::StreamingConfiguratorOutcome::Reset => {
                self.streaming_configurator = None;
                Self::request_clear_streaming_presentation_override(
                    chat,
                    self.declarative_streaming_policy,
                );
                true
            }
            super::streaming_configurator::StreamingConfiguratorOutcome::Cancel => {
                self.streaming_configurator = None;
                chat.app
                    .set_status("streaming presentation changes canceled".to_owned());
                true
            }
        }
    }

    pub fn request_streaming_presentation_override(
        chat: &mut ActiveChat,
        policy: bcode_session_view_models::StreamingPresentationPolicy,
    ) {
        let policy = policy.normalized();
        let _ = chat.app.apply_streaming_presentation_policy(policy);
        chat.replace_effect(TuiEffect::PersistStreamingPresentation { policy });
    }

    pub fn request_clear_streaming_presentation_override(
        chat: &mut ActiveChat,
        declarative_streaming_policy: bcode_session_view_models::StreamingPresentationPolicy,
    ) {
        let _ = chat
            .app
            .apply_streaming_presentation_policy(declarative_streaming_policy);
        chat.replace_effect(TuiEffect::ClearStreamingPresentation);
    }

    pub fn mark_presentation_committed(&mut self) {
        self.request_draft_handoff.mark_painted();
    }

    pub fn apply_session_stream_update(
        &mut self,
        chat: &mut ActiveChat,
        update: history_flow::SessionStreamUpdate,
    ) -> bool {
        if self.request_draft_handoff.blocks_session_stream() {
            self.request_draft_handoff.deferred.push_back(update);
            return false;
        }
        let paint_id = request_handoff_paint_id(&update).map(ToOwned::to_owned);
        let changed = absorb_session_stream_update(chat, self, update);
        self.request_draft_handoff
            .observe_applied(paint_id, changed);
        changed
    }

    pub fn apply_deferred_session_stream_updates(&mut self, chat: &mut ActiveChat) -> bool {
        if self.request_draft_handoff.blocks_session_stream() {
            return false;
        }
        let mut changed = false;
        while let Some(update) = self.request_draft_handoff.deferred.pop_front() {
            let paint_id = request_handoff_paint_id(&update).map(ToOwned::to_owned);
            let update_changed = absorb_session_stream_update(chat, self, update);
            self.request_draft_handoff
                .observe_applied(paint_id, update_changed);
            changed |= update_changed;
            if self.request_draft_handoff.blocks_session_stream() {
                break;
            }
        }
        changed
    }

    pub fn apply_artifact_completion(
        &mut self,
        chat: &ActiveChat,
        completion: ActiveArtifactFetchCompletion,
    ) -> bool {
        handle_artifact_completion(chat, self, completion)
    }

    pub fn apply_markdown_projection_completion(
        &mut self,
        chat: &mut ActiveChat,
        completion: super::markdown_projection_coordinator::MarkdownProjectionCompletion,
    ) -> bool {
        self.accept_markdown_projection_completion(chat, completion)
    }

    pub fn take_artifact_completion_receiver(
        &mut self,
    ) -> tokio::sync::mpsc::Receiver<ActiveArtifactFetchCompletion> {
        self.artifact_stream.take_completion_receiver()
    }

    pub fn take_pending_effects(
        &self,
        chat: &mut ActiveChat,
        handle: &bmux_tui_runtime::RuntimeHandle<super::root_program::BcodeRuntimeMessage>,
    ) -> (
        Vec<bmux_tui_runtime::Command<super::root_program::BcodeRuntimeMessage>>,
        std::collections::BTreeMap<
            bcode_session_models::SessionId,
            std::collections::VecDeque<TuiEffect>,
        >,
    ) {
        // The catalog stream belongs to the modal, not to the chat session. Closing
        // via any route cancels the keyed effect and drops its client subscription.
        if !self.has_session_picker() {
            chat.pending_effects.cancel(TuiEffect::LoadSessionPicker {
                generation: self.session_picker_generation,
                scope: bcode_session_models::SessionDiscoveryScope::AllWorkingDirectories,
                query: bcode_session_models::SessionCatalogQuery::default(),
                cursor: None,
            });
        }
        let (effects, notes) = chat.pending_effects.drain_runtime();
        let commands = effects
            .into_iter()
            .map(|(schedule, effect)| {
                effect.command(
                    schedule,
                    &self.foreground_client,
                    &self.passive_client,
                    handle.clone(),
                )
            })
            .collect();
        (commands, notes)
    }

    pub fn ordered_effect_command(
        &self,
        effect: TuiEffect,
        handle: &bmux_tui_runtime::RuntimeHandle<super::root_program::BcodeRuntimeMessage>,
    ) -> bmux_tui_runtime::Command<super::root_program::BcodeRuntimeMessage> {
        effect.command(
            super::effects::EffectSchedule::StartIfIdle,
            &self.foreground_client,
            &self.passive_client,
            handle.clone(),
        )
    }

    pub fn take_markdown_completion_receiver(
        &self,
    ) -> tokio::sync::watch::Receiver<
        Option<super::markdown_projection_coordinator::MarkdownProjectionCompletion>,
    > {
        self.markdown_projection.completion_receiver()
    }

    pub fn observe_daemon(&mut self, chat: &mut ActiveChat, observation: &DaemonObservation) {
        if let Some(state) = self.daemon_connection.observe(observation) {
            chat.app.set_daemon_connection(state);
        }
    }

    pub fn record_runtime_stats(&mut self, stats: &bmux_tui_runtime::RuntimeStats) {
        self.runtime_stats.record(&mut self.telemetry, stats);
    }

    pub fn foreground_client(&self) -> BcodeClient {
        self.foreground_client.clone()
    }

    pub fn flush_telemetry_if_due(&mut self, now: Instant) {
        self.telemetry.flush_if_due(now);
    }

    pub fn next_telemetry_flush_at(&self) -> Option<Instant> {
        self.telemetry.next_flush_at()
    }

    pub fn next_artifact_retry_at(&self) -> Option<Instant> {
        self.artifact_stream.next_retry_at()
    }

    pub fn start_due_artifact_fetches(&mut self, now: Instant) {
        self.artifact_stream.start_due_fetches(now);
    }

    /// Apply renderer-neutral completion configuration on startup or reload.
    pub fn set_slash_palette_settings(&mut self, settings: bcode_config::CompletionConfig) {
        self.slash_completion.settings = settings;
        if let Some(palette) = self.slash_palette.as_mut() {
            let query = palette.query().to_owned();
            palette.refilter(&query, settings);
        }
    }

    /// Invalidate discovery after configuration, plugin, skill or catalog changes.
    pub fn invalidate_slash_palette_discovery(&mut self) {
        self.slash_completion.invalidate();
    }

    pub fn refresh_slash_palette(&mut self, chat: &mut ActiveChat) -> bool {
        update_slash_palette_async(chat, self)
    }

    pub fn open_theme_picker(&mut self, chat: &mut ActiveChat) {
        let variant = super::theme::resolve_variant_for_config(chat.app.tui_config().theme.variant);
        let catalog = super::theme::catalog_view(&mut chat.app);
        self.theme_picker = Some(super::theme_picker::ThemePickerState::new(
            catalog.entries,
            catalog.diagnostics,
        ));
        self.prepare_theme_picker_previews(chat, variant);
        chat.app.set_status("theme picker opened".to_owned());
    }

    fn prepare_theme_picker_previews(
        &mut self,
        chat: &mut ActiveChat,
        variant: super::theme::definition::ResolvedThemeVariant,
    ) {
        let Some(picker) = self.theme_picker.as_ref() else {
            return;
        };
        let ids = picker
            .entries()
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        let overlays = chat.app.tui_config().theme.overlays.clone();
        let resolved = chat.app.theme_catalog().map(|discovered| {
            ids.into_iter()
                .filter_map(|id| {
                    discovered
                        .catalog
                        .resolve(
                            &super::theme::definition::ThemeSelection::new(&id)
                                .overlays(overlays.clone())
                                .variant(variant),
                        )
                        .ok()
                        .map(|theme| {
                            let accent = theme
                                .style("border.focused")
                                .and_then(|style| style.fg)
                                .or_else(|| theme.color("accent"))
                                .unwrap_or(super::theme::PENDING_AGENT_METADATA_ACCENT);
                            (
                                id,
                                super::theme::resolved_definition_theme(Some(&theme), accent),
                            )
                        })
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        });
        if let (Some(picker), Some(resolved)) = (self.theme_picker.as_mut(), resolved) {
            picker.set_resolved_previews(resolved);
        }
    }

    pub fn close_theme_picker(&mut self) {
        self.theme_picker = None;
    }

    pub fn handle_theme_picker_key(
        &mut self,
        chat: &mut ActiveChat,
        stroke: KeyStroke,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> bool {
        let Some(picker) = self.theme_picker.as_mut() else {
            return false;
        };
        let outcome = picker.handle_key_with_keymap(stroke, keymap);
        self.apply_theme_picker_outcome(chat, outcome);
        true
    }

    pub fn handle_theme_picker_mouse(
        &mut self,
        chat: &mut ActiveChat,
        mouse: bmux_tui::event::MouseEvent,
        frame_area: Rect,
    ) -> bool {
        let Some(picker) = self.theme_picker.as_mut() else {
            return false;
        };
        let theme = super::render::TuiTheme::for_app(&chat.app);
        let Some((row, activate)) =
            super::theme_picker_render::theme_picker_row(picker, mouse, frame_area, theme)
        else {
            return true;
        };
        let outcome = if activate {
            picker.activate_row(row)
        } else {
            picker.select_row(row)
        };
        self.apply_theme_picker_outcome(chat, outcome);
        true
    }

    fn apply_theme_picker_outcome(
        &mut self,
        chat: &mut ActiveChat,
        outcome: super::theme_picker::ThemePickerOutcome,
    ) {
        match outcome {
            super::theme_picker::ThemePickerOutcome::Preview(id) => {
                let resolved = self
                    .theme_picker
                    .as_ref()
                    .and_then(|picker| picker.resolved_preview(&id));
                if let Some(resolved) = resolved {
                    chat.app.preview_resolved_theme(&id, &resolved);
                    chat.app.set_status(format!("previewing theme {id}"));
                }
            }
            super::theme_picker::ThemePickerOutcome::Apply(id) => {
                let resolved = self
                    .theme_picker
                    .as_ref()
                    .and_then(|picker| picker.resolved_preview(&id));
                self.theme_picker = None;
                if let Some(resolved) = resolved {
                    chat.app.preview_resolved_theme(&id, &resolved);
                    chat.replace_effect(TuiEffect::PersistThemeSelection { name: id.clone() });
                    chat.app
                        .set_status(format!("saving theme {id} to user state…"));
                }
            }
            super::theme_picker::ThemePickerOutcome::Cancel => {
                self.theme_picker = None;
                chat.app.cancel_theme_preview();
                chat.app.set_status("theme preview cancelled".to_owned());
            }
            super::theme_picker::ThemePickerOutcome::Ignored => {}
        }
    }

    pub const fn has_command_palette(&self) -> bool {
        self.palette.is_some()
    }

    pub fn open_command_palette(&mut self, chat: &mut ActiveChat) {
        self.palette = Some(BmuxCommandPalette::new());
        chat.replace_effect(TuiEffect::LoadCommandPalette);
        chat.app
            .set_status("command palette: type to filter, enter to run, esc close".to_owned());
    }

    pub fn handle_command_palette_key(
        &mut self,
        stroke: KeyStroke,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> Option<CommandDispatchRequest> {
        let palette = self.palette.as_mut()?;
        match palette.handle_key_with_keymap(stroke, 12, keymap) {
            super::command_palette::CommandPaletteKeyOutcome::Ignored
            | super::command_palette::CommandPaletteKeyOutcome::QueryEdited
            | super::command_palette::CommandPaletteKeyOutcome::SelectionMoved => None,
            super::command_palette::CommandPaletteKeyOutcome::Canceled => {
                self.palette = None;
                None
            }
            super::command_palette::CommandPaletteKeyOutcome::Activated(index) => {
                let request = palette
                    .contribution_at(index)
                    .map(CommandDispatchRequest::from_contribution);
                self.palette = None;
                request
            }
        }
    }

    pub fn handle_command_palette_mouse(
        &mut self,
        mouse: bmux_tui::event::MouseEvent,
        frame_area: Rect,
    ) -> Option<CommandDispatchRequest> {
        let index = super::picker_mouse::command_palette_row_in_area(
            mouse,
            super::command_palette_render::palette_list_area(frame_area),
        )?;
        let palette = self.palette.as_mut()?;
        let request = palette
            .contribution_at(index)
            .map(CommandDispatchRequest::from_contribution);
        self.palette = None;
        request
    }

    pub const fn has_slash_palette(&self) -> bool {
        self.slash_palette.is_some()
    }

    pub fn handle_slash_palette_key(
        &mut self,
        chat: &mut ActiveChat,
        stroke: KeyStroke,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> SlashPaletteRootOutcome {
        if keymap.action("slash_palette", stroke) == Some("tui.control.cancel") {
            self.slash_completion.dismiss(chat.app.composer().text());
            if self.slash_palette.take().is_some() {
                chat.app.set_status("slash completions hidden".to_owned());
                return SlashPaletteRootOutcome::Handled;
            }
        }
        let Some(palette) = self.slash_palette.as_mut() else {
            return SlashPaletteRootOutcome::Unhandled;
        };
        match keymap.action("slash_palette", stroke) {
            Some("tui.control.up") => {
                palette.move_previous();
                SlashPaletteRootOutcome::Handled
            }
            Some("tui.control.down") => {
                palette.move_next();
                SlashPaletteRootOutcome::Handled
            }
            Some("tui.control.focus_next") => {
                let command = palette.selected_command().map(str::to_owned);
                self.slash_completion.dismiss(chat.app.composer().text());
                self.slash_palette = None;
                if let Some(command) = command {
                    chat.app.reset_input_history_navigation();
                    chat.app.replace_composer_with(&command);
                }
                SlashPaletteRootOutcome::Handled
            }
            Some("tui.control.submit") => {
                if palette.selected_matches(chat.app.composer().text()) {
                    self.slash_completion.dismiss(chat.app.composer().text());
                    self.slash_palette = None;
                    chat.app.stage_submission();
                    SlashPaletteRootOutcome::Submit
                } else {
                    let command = palette.selected_command().map(str::to_owned);
                    self.slash_completion.dismiss(chat.app.composer().text());
                    self.slash_palette = None;
                    if let Some(command) = command {
                        chat.app.reset_input_history_navigation();
                        chat.app.replace_composer_with(&command);
                    }
                    SlashPaletteRootOutcome::Handled
                }
            }
            Some("tui.control.cancel") => {
                self.slash_completion.dismiss(chat.app.composer().text());
                self.slash_palette = None;
                chat.app.set_status("slash completions hidden".to_owned());
                SlashPaletteRootOutcome::Handled
            }
            _ => SlashPaletteRootOutcome::Unhandled,
        }
    }

    pub fn handle_slash_palette_mouse(
        &mut self,
        chat: &mut ActiveChat,
        mouse: bmux_tui::event::MouseEvent,
        frame_area: Rect,
    ) -> bool {
        let Some(palette) = self.slash_palette.as_mut() else {
            return false;
        };
        let Some(row) = super::slash_palette_render::slash_palette_row_from_mouse(
            frame_area,
            chat.app.composer_content_area(),
            mouse.position.x,
            mouse.position.y,
            palette.item_count(),
        ) else {
            self.slash_completion.dismiss(chat.app.composer().text());
            self.slash_palette = None;
            return true;
        };
        let Some(list_area) = slash_palette_render::slash_palette_list_area(
            frame_area,
            chat.app.composer_content_area(),
            palette.item_count(),
        ) else {
            return false;
        };
        if let Some(command) = palette
            .select_visible_row(row, usize::from(list_area.height))
            .map(str::to_owned)
        {
            chat.app.reset_input_history_navigation();
            chat.app.replace_composer_with(&command);
        }
        self.slash_completion.dismiss(chat.app.composer().text());
        self.slash_palette = None;
        true
    }

    pub fn open_ralph_start_dialog(
        &mut self,
        launch_working_directory: &std::path::Path,
        chat: &mut ActiveChat,
    ) {
        let default_name = chat
            .app
            .session_title()
            .map_or_else(|| "new-ralph-loop".to_owned(), ToString::to_string);
        let repo_root = chat.app.working_directory().map_or_else(
            || launch_working_directory.to_path_buf(),
            std::path::Path::to_path_buf,
        );
        let validation_commands = bcode_ralph::default_validation_commands(&repo_root);
        self.ralph_start_dialog = Some(super::ralph_start_dialog::RalphStartDialog::new(
            &default_name,
            &validation_commands,
        ));
        chat.app.set_status("configure Ralph loop".to_owned());
    }

    pub const fn has_ralph_start_dialog(&self) -> bool {
        self.ralph_start_dialog.is_some()
    }

    pub fn handle_ralph_start_dialog_event(
        &mut self,
        event: &Event,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> Option<super::ralph_start_dialog::RalphStartDialogOutcome> {
        let dialog = self.ralph_start_dialog.as_mut()?;
        let outcome = dialog.handle_event(event, keymap);
        if matches!(
            outcome,
            super::ralph_start_dialog::RalphStartDialogOutcome::Canceled
        ) {
            self.ralph_start_dialog = None;
        }
        Some(outcome)
    }

    pub const fn take_ralph_start_dialog(
        &mut self,
    ) -> Option<super::ralph_start_dialog::RalphStartDialog> {
        self.ralph_start_dialog.take()
    }

    pub fn restore_ralph_start_dialog(
        &mut self,
        dialog: super::ralph_start_dialog::RalphStartDialog,
    ) {
        self.ralph_start_dialog = Some(dialog);
    }

    pub const fn has_worktree_create_dialog(&self) -> bool {
        self.worktree_create_dialog.is_some()
    }

    pub const fn has_root_plugin_surface(&self) -> bool {
        self.plugin_surface.is_some()
    }

    pub fn queue_root_plugin_surface(
        &mut self,
        plugin_id: impl Into<String>,
        surface: bcode_plugin_sdk::tui::BoxedPluginTuiSurface,
    ) {
        if let Some(active) = self.plugin_surface.take() {
            self.parent_plugin_surfaces.push(active);
        }
        let invalidation = bmux_tui_runtime::InvalidationSignal::new();
        let active_tasks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        invalidation.request();
        self.plugin_surface = Some(RootPluginSurface {
            plugin_id: plugin_id.into(),
            surface: Some(surface),
            drain: None,
            drain_task: None,
            navigation_callback: None,
            effect_integrity: PluginEffectIntegrity::Verifiable,
            close_after_drain: false,
            close_outcome: None,
            effects_pending: true,
            session_navigation_attached: false,
            deferred_input: std::collections::VecDeque::new(),
            invalidation,
            active_tasks,
            pending_session_navigation: None,
            workflow_view_requests: None,
        });
    }

    pub fn suspend_root_plugin_surface_for_session(
        &mut self,
        session_id: bcode_session_models::SessionId,
    ) -> bool {
        let Some(surface) = self.plugin_surface.as_mut() else {
            return false;
        };
        surface.pending_session_navigation = Some(session_id);
        surface.session_navigation_attached = false;
        // Input belongs to the surface visit that accepted it, never a later visit.
        surface.deferred_input.clear();
        true
    }

    pub fn mark_root_plugin_session_attached(
        &mut self,
        session_id: bcode_session_models::SessionId,
    ) -> bool {
        let Some(surface) = self.plugin_surface.as_mut() else {
            return false;
        };
        if surface.pending_session_navigation != Some(session_id) {
            return false;
        }
        surface.session_navigation_attached = true;
        true
    }

    pub fn root_plugin_session_is_attached(&self) -> bool {
        self.plugin_surface
            .as_ref()
            .is_some_and(|surface| surface.session_navigation_attached)
    }

    pub fn complete_root_plugin_session_navigation(
        &mut self,
        session_id: bcode_session_models::SessionId,
        result: Result<(), String>,
    ) -> bool {
        let Some(surface) = self.plugin_surface.as_mut() else {
            return false;
        };
        if surface.pending_session_navigation != Some(session_id) {
            return false;
        }
        surface.pending_session_navigation = None;
        surface.session_navigation_attached = false;
        if let Some(active) = surface.surface.as_mut() {
            active.session_navigation_finished(session_id, result);
        } else {
            surface.navigation_callback = Some((session_id, result));
        }
        surface.invalidation.request();
        true
    }

    pub fn root_plugin_surface_is_suspended(&self) -> bool {
        self.plugin_surface
            .as_ref()
            .is_some_and(|surface| surface.pending_session_navigation.is_some())
    }

    pub const fn root_plugin_surface_pending_session_navigation(
        &self,
    ) -> Option<bcode_session_models::SessionId> {
        match self.plugin_surface.as_ref() {
            Some(surface) => surface.pending_session_navigation,
            None => None,
        }
    }

    #[cfg(test)]
    pub fn active_root_plugin_surface_id(&self) -> Option<&'static str> {
        self.plugin_surface
            .as_ref()
            .and_then(|surface| surface.surface.as_ref().map(|surface| surface.id()))
    }

    /// Materialize only the active, unsuspended standalone owner's opt-in field.
    pub fn root_focused_text_field(
        &self,
    ) -> Option<bcode_plugin_sdk::tui::FocusedTextFieldSnapshot> {
        if self.root_plugin_surface_is_suspended() {
            return None;
        }
        self.plugin_surface
            .as_ref()?
            .surface
            .as_ref()?
            .focused_text_field()
            .filter(|field| {
                field.eligible && !field.content_rect.is_empty() && !field.anchor.is_empty()
            })
    }

    /// Apply a fenced edit through the standalone plugin's semantic owner.
    pub fn apply_root_focused_text_field_edit(
        &mut self,
        edit: &bcode_plugin_sdk::tui::FocusedTextFieldEdit,
        client: &BcodeClient,
        launch: super::plugin_surface_host::FreshSessionSettings,
    ) -> Option<bcode_plugin_sdk::tui::PluginTuiFocusedTextFieldEditResult> {
        if self.root_plugin_surface_is_suspended() {
            return None;
        }
        let surface = self.plugin_surface.as_mut()?;
        let active = surface.surface.as_mut()?;
        let host = super::plugin_surface_host::root_host(
            surface.invalidation.clone(),
            client.clone(),
            launch,
            std::sync::Arc::clone(&surface.active_tasks),
        );
        let result = active.apply_focused_text_field_edit_result(edit, &host);
        surface.effects_pending = true;
        surface.invalidation.request();
        Some(result)
    }

    pub fn handle_root_plugin_surface_event(
        &mut self,
        event: &Event,
        client: &BcodeClient,
        launch: super::plugin_surface_host::FreshSessionSettings,
    ) -> Option<bcode_plugin_sdk::tui::PluginTuiEventResult> {
        let surface = self.plugin_surface.as_mut()?;
        if surface.pending_session_navigation.is_some() {
            return Some(bcode_plugin_sdk::tui::PluginTuiEventResult {
                outcome: bmux_tui::event::EventOutcome::Handled,
                action: bcode_plugin_sdk::tui::PluginTuiAction::None,
            });
        }
        let host = super::plugin_surface_host::root_host(
            surface.invalidation.clone(),
            client.clone(),
            launch,
            std::sync::Arc::clone(&surface.active_tasks),
        );
        let Some(active) = surface.surface.as_mut() else {
            if let Event::Key(key) = event
                && (key.key == bmux_keyboard::KeyCode::Escape
                    || (key.key == bmux_keyboard::KeyCode::Char('c') && key.modifiers.ctrl))
            {
                surface.close_after_drain = true;
                surface.deferred_input.clear();
                surface.invalidation.request();
            } else if !surface.close_after_drain && matches!(event, Event::Key(_) | Event::Paste(_))
            {
                if surface.deferred_input.len() < 32 {
                    surface.deferred_input.push_back(event.clone());
                } else {
                    // Fail closed instead of silently losing a reliable input or replaying it.
                    surface.close_after_drain = true;
                    surface.deferred_input.clear();
                    surface.invalidation.request();
                }
            }
            return Some(bcode_plugin_sdk::tui::PluginTuiEventResult::from_legacy(
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw,
            ));
        };
        surface.effects_pending = true;
        surface.invalidation.request();
        Some(super::plugin_surface_host::compose_surface_input(
            active.as_mut(),
            event,
            &host,
        ))
    }

    pub fn root_plugin_surface_invalidation(&self) -> Option<bmux_tui_runtime::InvalidationSignal> {
        self.plugin_surface
            .as_ref()
            .map(|surface| surface.invalidation.clone())
    }

    pub fn poll_root_plugin_surface(
        &mut self,
        client: &BcodeClient,
        launch: super::plugin_surface_host::FreshSessionSettings,
    ) -> Option<bcode_plugin_sdk::tui::PluginTuiAction> {
        let surface = self.plugin_surface.as_mut()?;
        if surface.effect_integrity == PluginEffectIntegrity::UnknownEffects {
            return Some(bcode_plugin_sdk::tui::PluginTuiAction::None);
        }
        if let Err(action) = surface.observe_finished_drain_task() {
            return Some(action);
        }
        if surface.drain_task.is_some() && surface.drain.is_none() {
            surface.invalidation.request();
        }
        if surface.close_after_drain && surface.drain.is_none() && surface.drain_task.is_none() {
            return Some(bcode_plugin_sdk::tui::PluginTuiAction::Close {
                outcome: surface.close_outcome.take(),
            });
        }
        let host = super::plugin_surface_host::root_host(
            surface.invalidation.clone(),
            client.clone(),
            launch,
            std::sync::Arc::clone(&surface.active_tasks),
        );
        let invalidated = surface.invalidation.take();
        let action = if let Some(receiver) = surface.drain.as_mut() {
            match receiver.try_recv() {
                Ok((active, action)) => surface.accept_drain_completion(active, action, &host),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    bcode_plugin_sdk::tui::PluginTuiAction::None
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    surface.drain = None;
                    surface.effect_integrity = PluginEffectIntegrity::UnknownEffects;
                    bcode_plugin_sdk::tui::PluginTuiAction::None
                }
            }
        } else if surface.pending_session_navigation.is_some() {
            bcode_plugin_sdk::tui::PluginTuiAction::None
        } else if let Some(active) = surface.surface.as_mut() {
            let action = if let Some(event) = surface.deferred_input.pop_front() {
                surface.effects_pending = true;
                let action = active.handle_event(&event, &host);
                if !surface.deferred_input.is_empty() {
                    surface.invalidation.request();
                }
                action
            } else {
                active.poll(&host)
            };
            surface.effects_pending |=
                matches!(action, bcode_plugin_sdk::tui::PluginTuiAction::Redraw);
            if surface.effects_pending
                && surface.drain_task.is_none()
                && matches!(
                    action,
                    bcode_plugin_sdk::tui::PluginTuiAction::None
                        | bcode_plugin_sdk::tui::PluginTuiAction::Redraw
                )
            {
                surface.effects_pending = false;
                let mut active = surface.surface.take().expect("exclusive surface available");
                let (sender, receiver) = tokio::sync::oneshot::channel();
                let invalidation = surface.invalidation.clone();
                surface.drain_task = Some(tokio::spawn(async move {
                    let action = active.drain_effects(&host).await;
                    let _ = sender.send((active, action));
                    invalidation.request();
                }));
                surface.drain = Some(receiver);
            }
            action
        } else {
            bcode_plugin_sdk::tui::PluginTuiAction::None
        };
        if invalidated && matches!(action, bcode_plugin_sdk::tui::PluginTuiAction::None) {
            Some(bcode_plugin_sdk::tui::PluginTuiAction::Redraw)
        } else {
            Some(action)
        }
    }

    /// Verify that no retained surface has an unresolved operation failure.
    ///
    /// # Errors
    /// Returns a normalized error for failed tasks or missing completion results.
    pub fn verify_plugin_operations(&self) -> Result<(), TuiError> {
        if self
            .plugin_surface
            .iter()
            .chain(self.parent_plugin_surfaces.iter())
            .any(|surface| surface.effect_integrity == PluginEffectIntegrity::UnknownEffects)
        {
            return Err(std::io::Error::other("plugin operation ended without a verified result; effects may have occurred; inspect state before retrying").into());
        }
        Ok(())
    }

    /// Observe all already-dispatched plugin operations before runtime resources are released.
    /// This never replays input or starts a new effect, including after fatal terminal I/O.
    ///
    /// # Errors
    /// Returns an error if a dispatched task fails or ends without a verified completion.
    pub async fn settle_root_plugin_effects(
        &mut self,
    ) -> Result<Vec<PluginSurfaceOutcome>, TuiError> {
        let mut actions = Vec::new();
        let mut failed = false;
        for surface in self
            .plugin_surface
            .iter_mut()
            .chain(self.parent_plugin_surfaces.iter_mut())
        {
            failed |= surface.effect_integrity == PluginEffectIntegrity::UnknownEffects;
            surface.deferred_input.clear();
            if let Some(task) = surface.drain_task.as_mut() {
                failed |= task.await.is_err();
            }
            surface.drain_task = None;
            if let Some(receiver) = surface.drain.as_mut() {
                match receiver.await {
                    Ok((mut active, action)) => {
                        if let Some((session_id, result)) = surface.navigation_callback.take() {
                            active.session_navigation_finished(session_id, result);
                        }
                        surface.surface = Some(active);
                        actions.push((surface.plugin_id.clone(), action));
                    }
                    Err(_) => failed = true,
                }
                surface.drain = None;
            }
        }
        if failed {
            return Err(std::io::Error::other(
                "plugin operation ended without a verified result; effects may have occurred",
            )
            .into());
        }
        Ok(actions)
    }

    pub fn attach_root_plugin_surface_updates(
        &mut self,
        updates: bcode_plugin_sdk::tui::PluginTuiSurfaceUpdateReceiver,
        workflow_view_requests: tokio::sync::mpsc::Sender<WorkflowViewRequest>,
    ) {
        if let Some(surface) = self.plugin_surface.as_mut() {
            if let Some(surface) = surface.surface.as_mut() {
                surface.attach_updates(updates);
            }
            surface.workflow_view_requests = Some(workflow_view_requests);
        }
    }

    fn root_workflow_view_requests(
        &self,
    ) -> Option<&tokio::sync::mpsc::Sender<WorkflowViewRequest>> {
        self.plugin_surface
            .as_ref()
            .and_then(|surface| surface.workflow_view_requests.as_ref())
    }

    pub fn request_root_workflow_run(&self, run_id: String) {
        if let Some(requests) = self.root_workflow_view_requests() {
            let _ = requests.try_send(WorkflowViewRequest::SelectRun(run_id));
        }
    }

    pub fn request_root_workflow_query(
        &self,
        filter: bcode_workflow_view_models::WorkflowCatalogFilter,
        sort: bcode_workflow_view_models::WorkflowCatalogSort,
        group: bcode_workflow_view_models::WorkflowCatalogGroup,
        search: Option<String>,
    ) {
        if let Some(requests) = self.root_workflow_view_requests() {
            let _ = requests.try_send(WorkflowViewRequest::UpdateQuery {
                filter,
                sort,
                group,
                search,
            });
        }
    }

    pub fn request_more_root_workflow_runs(
        &self,
        cursor: bcode_workflow_view_models::WorkflowCatalogCursor,
    ) {
        if let Some(requests) = self.root_workflow_view_requests() {
            let _ = requests.try_send(WorkflowViewRequest::LoadMore(cursor));
        }
    }

    pub fn close_root_plugin_surface_with_outcome(
        &mut self,
        outcome: Option<serde_json::Value>,
    ) -> Option<(String, Option<serde_json::Value>)> {
        if let Some(surface) = self.plugin_surface.as_mut()
            && (surface.effect_integrity == PluginEffectIntegrity::UnknownEffects
                || surface.drain.is_some()
                || surface.drain_task.is_some())
        {
            surface.close_after_drain = true;
            surface.close_outcome = outcome;
            surface.invalidation.request();
            return None;
        }
        let surface = self.plugin_surface.take()?;
        self.restore_suspended_root_plugin_surface();
        Some((surface.plugin_id, outcome))
    }

    pub fn close_root_plugin_surface(&mut self) {
        if let Some(surface) = self.plugin_surface.as_mut()
            && (surface.effect_integrity == PluginEffectIntegrity::UnknownEffects
                || surface.drain.is_some()
                || surface.drain_task.is_some())
        {
            surface.close_after_drain = true;
            surface.invalidation.request();
            return;
        }
        self.plugin_surface = None;
        self.restore_suspended_root_plugin_surface();
    }

    fn restore_suspended_root_plugin_surface(&mut self) {
        self.plugin_surface = self.parent_plugin_surfaces.pop();
        if let Some(surface) = self.plugin_surface.as_ref() {
            surface.invalidation.request();
        }
    }

    pub const fn has_model_picker(&self) -> bool {
        self.provider_picker.is_some() || self.model_picker.is_some()
    }

    pub const fn has_auth_pool_picker(&self) -> bool {
        self.auth_pool_picker.is_some()
    }

    pub fn handle_auth_pool_picker_event(
        &mut self,
        event: &Event,
        keymap: &BmuxKeyMap,
    ) -> AuthPoolPickerRootOutcome {
        let Some(picker) = self.auth_pool_picker.as_mut() else {
            return AuthPoolPickerRootOutcome::Continue;
        };
        let action = match event {
            Event::Key(stroke) => match keymap.action("auth_pool_picker", *stroke) {
                Some("tui.control.cancel") => AuthPoolPickerRootOutcome::Cancel,
                Some("tui.control.submit") => picker
                    .selected()
                    .map_or(AuthPoolPickerRootOutcome::Continue, |(pool, profile)| {
                        AuthPoolPickerRootOutcome::Promote { pool, profile }
                    }),
                Some("tui.auth.clear") => picker
                    .selected_pool()
                    .map_or(AuthPoolPickerRootOutcome::Continue, |pool| {
                        AuthPoolPickerRootOutcome::Clear { pool }
                    }),
                Some("tui.control.up") => {
                    picker.select_previous();
                    AuthPoolPickerRootOutcome::Continue
                }
                Some("tui.control.down") => {
                    picker.select_next();
                    AuthPoolPickerRootOutcome::Continue
                }
                _ => AuthPoolPickerRootOutcome::Continue,
            },
            Event::Mouse(mouse) => {
                if let Some(row) = super::picker_mouse::picker_row_from_mouse(*mouse) {
                    picker.select_visible(row);
                }
                AuthPoolPickerRootOutcome::Continue
            }
            Event::Focus(_) | Event::Resize(_) | Event::Tick | Event::User(_) | Event::Paste(_) => {
                AuthPoolPickerRootOutcome::Continue
            }
        };
        if !matches!(action, AuthPoolPickerRootOutcome::Continue) {
            self.auth_pool_picker = None;
        }
        action
    }

    pub fn handle_model_picker_event(
        &mut self,
        keymap: &super::keymap::BmuxKeyMap,
        event: &Event,
    ) -> Option<(Option<String>, super::model_flow::ModelPickerAction)> {
        if let Some(picker) = self.provider_picker.as_mut() {
            match picker.handle_event(event, keymap) {
                super::provider_picker::ProviderPickerOutcome::Continue => return None,
                super::provider_picker::ProviderPickerOutcome::Cancel => {
                    self.provider_picker = None;
                    return Some((None, super::model_flow::ModelPickerAction::Cancel));
                }
                super::provider_picker::ProviderPickerOutcome::Select(provider) => {
                    self.provider_picker = None;
                    return Some((provider, super::model_flow::ModelPickerAction::Continue));
                }
            }
        }
        let model = self.model_picker.as_mut()?;
        if matches!(
            model.picker.mode(),
            super::model_picker::ModelPickerMode::Actions
                | super::model_picker::ModelPickerMode::Filter
        ) && matches!(event, Event::Key(stroke) if keymap.action("model_picker", *stroke) == Some("tui.control.submit") || keymap.action("model_picker_actions", *stroke) == Some("tui.control.submit"))
            && let Some(model_id) = model.picker.selected_ignored_model_id()
        {
            model.picker.set_status(format!(
                "{model_id} is ignored; use Unignore to remove state ignore or Show ignored to hide ignored models"
            ));
            return None;
        }
        let action = match event {
            Event::Paste(text) => {
                if matches!(
                    model.picker.mode(),
                    super::model_picker::ModelPickerMode::Actions
                        | super::model_picker::ModelPickerMode::Filter
                ) {
                    model.picker.focus_filter();
                    let _ = super::text_input_flow::handle_paste(model.picker.filter_mut(), text);
                    model.picker.refresh_filter();
                }
                super::model_flow::ModelPickerAction::Continue
            }
            Event::Key(stroke) => super::model_flow::handle_model_picker_key(
                &mut model.picker,
                keymap,
                model.provider_plugin_id.as_deref(),
                *stroke,
            ),
            Event::Mouse(mouse) => model
                .picker
                .select_mouse_row(*mouse)
                .map_or(super::model_flow::ModelPickerAction::Continue, |model_id| {
                    super::model_flow::ModelPickerAction::Select(model_id)
                }),
            Event::Focus(_) | Event::Resize(_) | Event::Tick | Event::User(_) => {
                super::model_flow::ModelPickerAction::Continue
            }
        };
        let provider = model.provider_plugin_id.clone();
        if matches!(
            action,
            super::model_flow::ModelPickerAction::Cancel
                | super::model_flow::ModelPickerAction::Select(_)
        ) {
            self.model_picker = None;
        }
        Some((provider, action))
    }

    pub fn start_selected_model_check(&mut self) -> Option<u64> {
        let model = self.model_picker.as_mut()?;
        if self.model_check_in_flight {
            model.picker.finish_check(
                None,
                "A model check is already running; wait for its bounded outcome before retrying",
            );
            return None;
        }
        self.model_check_in_flight = true;
        Some(model.generation)
    }

    pub const fn has_skill_picker(&self) -> bool {
        self.skill_picker.is_some()
    }

    pub fn handle_skill_picker_event(
        &mut self,
        keymap: &super::keymap::BmuxKeyMap,
        event: &Event,
    ) -> Option<super::skill_picker::SkillPickerAction> {
        let picker = self.skill_picker.as_mut()?;
        let action = match event {
            Event::Paste(text) => {
                match picker.mode() {
                    super::skill_picker::SkillPickerMode::Filter => {
                        let _ = super::text_input_flow::handle_paste(picker.filter_mut(), text);
                        picker.refresh_filter();
                    }
                    super::skill_picker::SkillPickerMode::Argument => {
                        let _ = super::text_input_flow::handle_paste(picker.argument_mut(), text);
                    }
                }
                super::skill_picker::SkillPickerAction::Continue
            }
            Event::Key(stroke) => {
                super::skill_flow::handle_skill_picker_key(picker, keymap, *stroke)
            }
            Event::Mouse(mouse) => {
                if let Some(row) = super::picker_mouse::picker_row_from_mouse(*mouse)
                    && picker.select_visible(row)
                {
                    picker.start_argument();
                }
                super::skill_picker::SkillPickerAction::Continue
            }
            Event::Focus(_) | Event::Resize(_) | Event::Tick | Event::User(_) => {
                super::skill_picker::SkillPickerAction::Continue
            }
        };
        if !matches!(action, super::skill_picker::SkillPickerAction::Continue) {
            self.skill_picker = None;
        }
        Some(action)
    }

    pub const fn has_working_directory_dialog(&self) -> bool {
        self.working_directory_dialog.is_some()
    }

    pub fn open_working_directory_dialog(&mut self, chat: &mut ActiveChat) {
        let Some(working_directory) = chat.app.working_directory() else {
            chat.app
                .set_status("no active session working directory".to_owned());
            return;
        };
        self.working_directory_dialog =
            Some(super::working_directory_dialog::WorkingDirectoryDialog::new(working_directory));
        chat.app.set_status("change working directory".to_owned());
    }

    pub fn handle_working_directory_dialog_event(
        &mut self,
        keymap: &super::keymap::BmuxKeyMap,
        event: &Event,
    ) -> WorkingDirectoryDialogRootOutcome {
        let Some(dialog) = self.working_directory_dialog.as_mut() else {
            return WorkingDirectoryDialogRootOutcome::Unhandled;
        };
        match dialog.handle_event(event, keymap) {
            super::working_directory_dialog::WorkingDirectoryDialogOutcome::Handled => {
                WorkingDirectoryDialogRootOutcome::Handled
            }
            super::working_directory_dialog::WorkingDirectoryDialogOutcome::Canceled => {
                self.working_directory_dialog = None;
                WorkingDirectoryDialogRootOutcome::Canceled
            }
            super::working_directory_dialog::WorkingDirectoryDialogOutcome::Apply(path) => {
                WorkingDirectoryDialogRootOutcome::Apply(path)
            }
        }
    }

    pub fn apply_working_directory_dialog_result(
        &mut self,
        result: &Result<bcode_session_models::SessionSummary, ClientError>,
    ) {
        match result {
            Ok(_) => self.working_directory_dialog = None,
            Err(error) => {
                if let Some(dialog) = &mut self.working_directory_dialog {
                    dialog.set_submission_error(format!("Unable to change directory: {error}"));
                }
            }
        }
    }

    pub fn open_worktree_create_dialog(&mut self, chat: &mut ActiveChat) {
        let current_session_id = chat.app.session_id();
        let default_name = current_session_id.map_or_else(
            || "new-session".to_owned(),
            |session_id| {
                chat.app
                    .session_title()
                    .map_or_else(|| format!("session-{session_id}"), ToString::to_string)
            },
        );
        self.worktree_create_dialog = Some(super::wt_create_dialog::WorktreeCreateDialog::new(
            &default_name,
            current_session_id.is_some(),
        ));
        chat.app.set_status("create worktree".to_owned());
    }

    pub fn handle_worktree_create_dialog_event(
        &mut self,
        keymap: &super::keymap::BmuxKeyMap,
        event: &Event,
    ) -> WorktreeCreateDialogRootOutcome {
        let Some(dialog) = self.worktree_create_dialog.as_mut() else {
            return WorktreeCreateDialogRootOutcome::Unhandled;
        };
        match dialog.handle_event(event, keymap) {
            super::wt_create_dialog::WorktreeCreateDialogOutcome::Handled => {
                WorktreeCreateDialogRootOutcome::Handled
            }
            super::wt_create_dialog::WorktreeCreateDialogOutcome::Canceled => {
                self.worktree_create_dialog = None;
                WorktreeCreateDialogRootOutcome::Canceled
            }
            super::wt_create_dialog::WorktreeCreateDialogOutcome::Create(submission) => {
                self.worktree_create_dialog = None;
                WorktreeCreateDialogRootOutcome::Create {
                    name: submission.name,
                    target: submission.target,
                    base: submission.base,
                }
            }
        }
    }

    pub fn handle_timeline_dialog_key(
        &mut self,
        chat: &mut ActiveChat,
        stroke: KeyStroke,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> TimelineDialogRootOutcome {
        let Some(dialog) = self.timeline_dialog.as_mut() else {
            return TimelineDialogRootOutcome::Unhandled;
        };
        match dialog.handle_key_with_keymap(stroke, keymap) {
            super::timeline_dialog::TimelineDialogOutcome::Milestones => {
                if let Some(session_id) = chat.viewing_session_id() {
                    if let Some(request) = self.milestone_requests.begin(session_id, None, dialog) {
                        chat.replace_effect(TuiEffect::LoadMilestones { request });
                    }
                    chat.app
                        .set_status("loading canonical milestones…".to_owned());
                }
                TimelineDialogRootOutcome::Handled
            }
            super::timeline_dialog::TimelineDialogOutcome::NextMilestonePage => {
                if !self.milestone_requests.is_pending()
                    && let (Some(session_id), Some(after_sequence)) =
                        (chat.viewing_session_id(), dialog.next_after_sequence())
                {
                    if let Some(request) =
                        self.milestone_requests
                            .begin(session_id, Some(after_sequence), dialog)
                    {
                        chat.replace_effect(TuiEffect::LoadMilestones { request });
                    }
                    chat.app
                        .set_status("loading next milestone page…".to_owned());
                }
                TimelineDialogRootOutcome::Handled
            }
            super::timeline_dialog::TimelineDialogOutcome::Prompts => {
                self.milestone_requests.cancel();
                *dialog =
                    super::timeline_dialog::TimelineDialogState::new(chat.app.timeline_entries());
                TimelineDialogRootOutcome::Handled
            }
            super::timeline_dialog::TimelineDialogOutcome::Handled => {
                chat.app.set_status("timeline".to_owned());
                TimelineDialogRootOutcome::Handled
            }
            super::timeline_dialog::TimelineDialogOutcome::Activate => {
                let selected = dialog.selected_entry().cloned();
                self.timeline_dialog = None;
                selected.map_or(
                    TimelineDialogRootOutcome::Handled,
                    TimelineDialogRootOutcome::Jump,
                )
            }
            super::timeline_dialog::TimelineDialogOutcome::Cancel => {
                self.milestone_requests.cancel();
                self.timeline_dialog = None;
                chat.app.set_status("timeline closed".to_owned());
                TimelineDialogRootOutcome::Handled
            }
            super::timeline_dialog::TimelineDialogOutcome::Ignored => {
                TimelineDialogRootOutcome::Unhandled
            }
        }
    }

    pub fn handle_thinking_dialog_key(
        &mut self,
        chat: &mut ActiveChat,
        stroke: KeyStroke,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> ThinkingDialogRootOutcome {
        let Some(dialog) = self.thinking_dialog.as_mut() else {
            return ThinkingDialogRootOutcome::Unhandled;
        };
        match dialog.handle_key_with_keymap(stroke, keymap) {
            super::thinking_dialog::ThinkingDialogOutcome::Handled => {
                chat.app
                    .set_status("reasoning output setting changed".to_owned());
                ThinkingDialogRootOutcome::Handled
            }
            super::thinking_dialog::ThinkingDialogOutcome::Apply => {
                let dialog = self.thinking_dialog.take().expect("dialog checked above");
                ThinkingDialogRootOutcome::Apply {
                    effort: dialog.effort().map(ToOwned::to_owned),
                    summary: dialog.summary().map(ToOwned::to_owned),
                    visible: dialog.visible(),
                    mode: dialog.mode(),
                }
            }
            super::thinking_dialog::ThinkingDialogOutcome::Cancel => {
                self.thinking_dialog = None;
                chat.app
                    .set_status("reasoning output settings canceled".to_owned());
                ThinkingDialogRootOutcome::Handled
            }
            super::thinking_dialog::ThinkingDialogOutcome::Ignored => {
                ThinkingDialogRootOutcome::Unhandled
            }
        }
    }

    pub fn session_changed(&mut self, session_id: Option<bcode_session_models::SessionId>) {
        if let Some(task) = self.viewport_input_task.take() {
            task.abort();
        }
        self.viewport_input_connection = std::sync::Arc::default();
        self.viewport_input_retry_at = Instant::now();
        self.request_draft_handoff.clear();
        self.markdown_projection.invalidate();
        self.artifact_stream.retain_session(session_id);
    }

    fn prepare_viewport_inputs(&mut self, chat: &ActiveChat, frame_area: Rect) {
        if Instant::now() < self.viewport_input_retry_at
            || self
                .viewport_input_task
                .as_ref()
                .is_some_and(|task| !task.is_finished())
        {
            return;
        }
        let Some(session_id) = chat.attached_session_id() else {
            return;
        };
        let Some(presentation) = chat.app.plugin_presentation() else {
            return;
        };
        let area = super::render::transcript_area_for_frame(&chat.app, frame_area);
        let inputs = self.artifact_stream.viewport_inputs(
            session_id,
            presentation,
            bmux_tui::geometry::Size::new(area.width, area.height.saturating_sub(3).max(1)),
            |id| chat.app.tool_invocation_is_terminal(id),
        );
        self.viewport_input_retry_at = Instant::now() + std::time::Duration::from_millis(250);
        if inputs.is_empty() {
            return;
        }
        let client = self.foreground_client.clone();
        let connection = std::sync::Arc::clone(&self.viewport_input_connection);
        self.viewport_input_task = Some(tokio::spawn(async move {
            let mut connection = connection.lock().await;
            if connection.is_none() {
                match client.connect("bcode-viewport-control").await {
                    Ok(client) => *connection = Some(client),
                    Err(error) => {
                        tracing::debug!(%error, "viewport connection deferred");
                        return;
                    }
                }
            }
            let Some(client) = connection.as_mut() else {
                return;
            };
            for input in inputs {
                if let Err(error) = client.send_invocation_input(session_id, input).await {
                    tracing::debug!(%error, "viewport input deferred");
                }
            }
            drop(connection);
        }));
    }

    pub fn prepare_runtime_work(
        &mut self,
        chat: &mut ActiveChat,
        frame_area: Rect,
    ) -> super::invalidation::UiInvalidation {
        self.prepare_viewport_inputs(chat, frame_area);
        self.artifact_stream.start_due_fetches(Instant::now());
        self.request_latest_markdown_projection(chat, frame_area.width);
        record_artifact_stream_stats(self);
        let changed = chat
            .app
            .plugin_presentation()
            .is_some_and(crate::plugin_tui::PluginTuiPresentation::poll_dynamic_visuals)
            | maybe_start_older_history_load(chat, self, frame_area.width)
            | maybe_start_newer_history_load(chat, self, frame_area.width);
        if changed {
            super::invalidation::UiInvalidation::Structural
        } else {
            super::invalidation::UiInvalidation::None
        }
    }

    pub fn next_interactive_surface_retry_at(&self) -> Option<Instant> {
        self.interactive_surface_queue.next_retry_at()
    }

    pub fn next_surface_open_request(
        &self,
    ) -> Option<super::interactive_surface::InteractiveSurfaceRequest> {
        if self.interactive_surface.is_some() {
            return None;
        }
        self.interactive_surface_queue
            .front_ready(Instant::now())
            .cloned()
    }

    pub fn complete_interactive_surface_open(
        &mut self,
        result: Result<InteractiveSurfaceState, String>,
    ) -> InteractiveSurfaceOpenCompletion {
        match result {
            Ok(surface) => {
                let is_expected = self
                    .interactive_surface_queue
                    .front_ready(Instant::now())
                    .is_some_and(|request| request.interaction_id() == surface.interaction_id());
                if !is_expected || self.interactive_surface.is_some() {
                    return InteractiveSurfaceOpenCompletion::Stale;
                }
                self.interactive_surface_queue.pop_front();
                self.interactive_surface = Some(surface);
                InteractiveSurfaceOpenCompletion::Opened
            }
            Err(error) => {
                self.interactive_surface_queue.defer_front(Instant::now());
                tracing::warn!(%error, "failed to open interactive TUI surface");
                InteractiveSurfaceOpenCompletion::Failed
            }
        }
    }

    pub(super) const fn active_interactive_surface_geometry(
        &self,
    ) -> Option<InteractiveSurfaceGeometry> {
        self.interactive_surface_geometry
    }

    /// Project only a currently visible owner field through the retained interaction geometry.
    pub fn interactive_focused_text_field(
        &self,
    ) -> Option<bcode_plugin_sdk::tui::FocusedTextFieldSnapshot> {
        let geometry = self.interactive_surface_geometry?;
        if geometry.destination.is_empty() {
            return None;
        }
        let mut field = self.interactive_surface.as_ref()?.focused_text_field()?;
        // Owners report retained visible geometry in host coordinates, including sliced
        // presentation. Clip to the actual destination; reprojecting would translate twice.
        let content = field.content_rect.intersection(geometry.destination);
        let anchor = field.anchor.intersection(content);
        if content.is_empty() || anchor.is_empty() || !field.eligible {
            return None;
        }
        field.content_rect = content;
        field.anchor = anchor;
        Some(field)
    }

    pub fn apply_interactive_focused_text_field_edit(
        &mut self,
        edit: &bcode_plugin_sdk::tui::FocusedTextFieldEdit,
    ) -> (
        bcode_plugin_sdk::tui::FocusedTextFieldEditResult,
        InteractiveSurfaceEventOutcome,
    ) {
        if self
            .interactive_focused_text_field()
            .is_none_or(|current| edit.validate(&current).is_err())
        {
            return (
                bcode_plugin_sdk::tui::FocusedTextFieldEditResult::Rejected(
                    bcode_plugin_sdk::composer::ComposerValidationError::Stale,
                ),
                InteractiveSurfaceEventOutcome::Consumed,
            );
        }
        self.interactive_surface.as_mut().map_or(
            (
                bcode_plugin_sdk::tui::FocusedTextFieldEditResult::Rejected(
                    bcode_plugin_sdk::composer::ComposerValidationError::Stale,
                ),
                InteractiveSurfaceEventOutcome::Consumed,
            ),
            |surface| surface.apply_focused_text_field_edit(edit),
        )
    }

    pub(super) fn routes_non_mouse_event_to_interactive_surface(
        &self,
        config: bcode_config::TuiInteractionConfig,
    ) -> bool {
        !self.interactive_surface_geometry.is_some_and(|geometry| {
            geometry.placement == InteractiveSurfacePlacement::Transcript
                && geometry.destination.is_empty()
                && config.offscreen_focus == bcode_config::TuiInteractionOffscreenFocus::Suspend
        })
    }

    /// Resolve one foreground in input priority order. Painting and routing share this policy.
    pub(crate) fn foreground(&self) -> Option<super::foreground::Foreground> {
        use super::foreground::Foreground;
        [
            (
                self.plugin_surface.is_some() && !self.root_plugin_surface_is_suspended(),
                Foreground::Plugin,
            ),
            (self.session_picker.is_some(), Foreground::Sessions),
            (self.auth_pool_picker.is_some(), Foreground::AuthPool),
            (
                self.provider_picker.is_some() || self.model_picker.is_some(),
                Foreground::Model,
            ),
            (self.skill_picker.is_some(), Foreground::Skill),
            (self.ralph_start_dialog.is_some(), Foreground::Ralph),
            (
                self.working_directory_dialog.is_some(),
                Foreground::WorkingDirectory,
            ),
            (self.worktree_create_dialog.is_some(), Foreground::Worktree),
            (self.permission_dialog.is_some(), Foreground::Permission),
            (self.streaming_configurator.is_some(), Foreground::Streaming),
            (self.timeline_dialog.is_some(), Foreground::Timeline),
            (self.thinking_dialog.is_some(), Foreground::Thinking),
            (self.theme_picker.is_some(), Foreground::Theme),
            (self.palette.is_some(), Foreground::Commands),
            (self.slash_palette.is_some(), Foreground::Slash),
        ]
        .into_iter()
        .find_map(|(active, foreground)| active.then_some(foreground))
    }

    pub(crate) fn modal_foreground(&self) -> bool {
        self.foreground()
            .is_some_and(super::foreground::Foreground::is_modal)
    }

    fn foreground_owns_canvas(&self) -> bool {
        use super::foreground::Foreground;
        matches!(
            self.foreground(),
            Some(
                Foreground::Plugin
                    | Foreground::Sessions
                    | Foreground::AuthPool
                    | Foreground::Model
                    | Foreground::Skill
            )
        )
    }

    pub const fn has_session_picker(&self) -> bool {
        self.session_picker.is_some()
    }

    pub fn set_session_picker_launch_directory(&mut self, directory: std::path::PathBuf) {
        self.session_picker_launch_directory = directory;
    }

    pub fn open_session_picker(&mut self, chat: &mut ActiveChat) {
        self.open_session_picker_with_mode(chat, false);
    }

    pub fn open_session_search(&mut self, chat: &mut ActiveChat) {
        self.open_session_picker_with_mode(chat, true);
    }

    fn open_session_picker_with_mode(&mut self, chat: &mut ActiveChat, search: bool) {
        let mut picker = super::session_picker::SessionPickerApp::remembered(Vec::new());
        if search {
            picker.start_transcript_search();
        } else {
            picker.set_loading_status("Loading sessions…".to_owned());
        }
        self.session_picker = Some(picker);
        self.restart_session_picker_subscription(chat);
        chat.app.set_status(if search {
            "session search".to_owned()
        } else {
            "session picker".to_owned()
        });
    }

    fn restart_session_picker_subscription(&mut self, chat: &mut ActiveChat) {
        self.session_picker_generation = self
            .session_picker_generation
            .checked_add(1)
            .expect("picker generation exhausted");
        let Some(picker) = self.session_picker.as_ref() else {
            return;
        };
        chat.replace_effect(TuiEffect::LoadSessionPicker {
            generation: self.session_picker_generation,
            scope: if picker.all_directories() {
                bcode_session_models::SessionDiscoveryScope::AllWorkingDirectories
            } else {
                bcode_session_models::SessionDiscoveryScope::WorkingDirectory(
                    self.session_picker_launch_directory.clone(),
                )
            },
            query: bcode_session_models::SessionCatalogQuery {
                text: Some(picker.filter().buffer().text().to_owned()),
            },
            cursor: picker.catalog_cursor().cloned(),
        });
    }

    fn apply_session_catalog_update(
        &mut self,
        generation: u64,
        result: Result<bcode_session_models::SessionCatalogPage, bcode_client::ClientError>,
        restart: bool,
    ) {
        if generation != self.session_picker_generation {
            return;
        }
        if restart && let Some(picker) = self.session_picker.as_mut() {
            picker.restart_catalog();
            picker.replace_sessions(Vec::new());
        }
        self.apply_session_picker_result(generation, result);
    }

    pub fn apply_session_picker_result(
        &mut self,
        generation: u64,
        result: Result<bcode_session_models::SessionCatalogPage, bcode_client::ClientError>,
    ) {
        if generation != self.session_picker_generation {
            return;
        }
        let Some(picker) = self.session_picker.as_mut() else {
            return;
        };
        match result {
            Ok(session_list) => {
                let previous_mode = picker.mode();
                picker.set_catalog_next_cursor(session_list.next_cursor);
                picker.replace_sessions(session_list.sessions);
                let canceled = matches!(
                    previous_mode,
                    super::session_picker::SessionPickerMode::Rename
                        | super::session_picker::SessionPickerMode::DeleteConfirm
                ) && picker.mode()
                    == super::session_picker::SessionPickerMode::Filter;
                let cancellation = canceled.then(|| picker.status().to_owned());
                picker.set_catalog_status(&session_list.catalog_status);
                if picker.all_directories() {
                    let continuation = if picker.has_catalog_continuation() {
                        "Next page: Alt-P (including empty windows)"
                    } else {
                        "End of catalog"
                    };
                    picker.set_status(format!(
                        "{}; Native only · location/ID order; {continuation}",
                        picker.status()
                    ));
                }
                if let Some(cancellation) = cancellation {
                    picker.set_status(format!("{cancellation}; {}", picker.status()));
                }
            }
            Err(error) => {
                picker.restart_catalog();
                picker.set_status(format!(
                    "Session catalog unavailable: {error}; Alt-R restarts discovery"
                ));
            }
        }
    }

    pub fn apply_session_import_result(
        &mut self,
        result: Result<
            (
                bcode_session_models::SessionSummary,
                Vec<bcode_ipc::SessionImportWarning>,
            ),
            bcode_client::ClientError,
        >,
    ) -> Option<bcode_session_models::SessionId> {
        let picker = self.session_picker.as_mut()?;
        match result {
            Ok((session, warnings)) => {
                let session_id = session.id;
                picker.set_last_import(Some((session, warnings)));
                self.session_picker = None;
                Some(session_id)
            }
            Err(error) => {
                picker.set_status(format!("Import failed: {error}"));
                None
            }
        }
    }

    pub fn apply_session_mutation_result(
        &mut self,
        action: &str,
        result: Result<bcode_session_models::SessionSummary, bcode_client::ClientError>,
    ) {
        let Some(picker) = self.session_picker.as_mut() else {
            return;
        };
        match result {
            Ok(_) => picker.finish_mutation(format!("Session {action}; refreshing…")),
            Err(error) => picker.finish_mutation(format!("Session {action} failed: {error}")),
        }
    }

    pub fn apply_session_search_result(
        &mut self,
        result: Result<
            (
                bcode_session_search::FederatedSessionSearchResponse,
                Vec<bcode_session_search::HydratedSessionSearchHit>,
            ),
            bcode_client::ClientError,
        >,
    ) {
        let Some(picker) = self.session_picker.as_mut() else {
            return;
        };
        match result {
            Ok((response, _)) if response.hits.is_empty() => {
                picker.set_search_results(&response, Vec::new());
                let status = if !response.query_complete {
                    "Search incomplete; inspect provider failures and retry"
                } else if !response.coverage_complete {
                    "No matches in searched data; corpus coverage is incomplete"
                } else if !response.failures.is_empty() {
                    "No matches; one or more providers failed"
                } else {
                    "No transcript matches in complete searchable coverage"
                };
                picker.set_status(status.to_owned());
            }
            Ok((response, hydrated)) => picker.set_search_results(&response, hydrated),
            Err(error) => picker.set_status(format!("Transcript search failed: {error}")),
        }
    }

    /// Handle a catalog control from the same measured hit map used for painting.
    pub fn handle_session_catalog_control(&mut self, chat: &mut ActiveChat, id: &str) -> bool {
        let Some(picker) = self.session_picker.as_mut() else {
            return false;
        };
        if picker.mode() != super::session_picker::SessionPickerMode::Filter {
            return false;
        }
        let changed = match id {
            "session.scope.local" => picker.set_all_directories(false),
            "session.scope.all" => picker.set_all_directories(true),
            "session.scope.next" => picker.next_catalog_page(),
            "session.scope.restart" => {
                picker.restart_catalog();
                picker.replace_sessions(Vec::new());
                true
            }
            _ => return false,
        };
        if changed {
            self.restart_session_picker_subscription(chat);
        }
        true
    }

    #[allow(clippy::too_many_lines)]
    pub fn handle_session_picker_event(
        &mut self,
        chat: &mut ActiveChat,
        keymap: &super::keymap::BmuxKeyMap,
        event: &Event,
    ) -> SessionPickerRootOutcome {
        if let Event::Key(stroke) = event {
            let control = match keymap.action("session_picker", *stroke) {
                Some("tui.session.scopeToggle") => self.session_picker.as_ref().map(|picker| {
                    if picker.all_directories() {
                        "session.scope.local"
                    } else {
                        "session.scope.all"
                    }
                }),
                Some("tui.session.catalogNextPage") => Some("session.scope.next"),
                Some("tui.session.catalogRestart") => Some("session.scope.restart"),
                _ => None,
            };
            if let Some(control) = control
                && self.handle_session_catalog_control(chat, control)
            {
                return SessionPickerRootOutcome::Handled;
            }
        }
        let Some(picker) = self.session_picker.as_mut() else {
            return SessionPickerRootOutcome::Unhandled;
        };
        let previous_query = picker.filter().buffer().text().to_owned();
        match event {
            Event::Paste(text) => {
                let _ = super::text_input_flow::handle_paste(picker.active_input_mut(), text);
                if picker.mode() == super::session_picker::SessionPickerMode::Filter {
                    picker.refresh_filter();
                }
            }
            Event::Key(stroke) => match picker.mode() {
                super::session_picker::SessionPickerMode::Filter => {
                    let search =
                        keymap.action("session_picker", *stroke) == Some("tui.session.search");
                    if keymap.action("session_picker", *stroke)
                        == Some("tui.session.childrenNextPage")
                        && !picker.all_directories()
                    {
                        if let Some((parent_id, after)) = picker.next_child_page() {
                            chat.replace_effect(TuiEffect::LoadSessionChildren {
                                parent_id,
                                after: Some(after),
                                generation: 0,
                            });
                        }
                        return SessionPickerRootOutcome::Handled;
                    }
                    if keymap.action("session_picker", *stroke) == Some("tui.control.right")
                        && !picker.all_directories()
                    {
                        if let Some(parent_id) = picker.toggle_selected_branch() {
                            chat.replace_effect(TuiEffect::LoadSessionChildren {
                                parent_id,
                                after: None,
                                generation: 0,
                            });
                        }
                        return SessionPickerRootOutcome::Handled;
                    }
                    if keymap.action("session_picker", *stroke) == Some("tui.control.left") {
                        picker.select_parent();
                        return SessionPickerRootOutcome::Handled;
                    }
                    if search {
                        let query = picker.filter().buffer().text().trim().to_owned();
                        if query.is_empty() {
                            picker.set_status(
                                "Type a transcript query, then invoke Search".to_owned(),
                            );
                        } else {
                            picker.start_transcript_search();
                            let Ok((request, policy)) = picker.search_request(&query, false) else {
                                picker.set_status(
                                    "Invalid search controls; invoke Search syntax for details"
                                        .to_owned(),
                                );
                                return SessionPickerRootOutcome::Handled;
                            };
                            picker.set_status("Searching transcripts…".to_owned());
                            chat.replace_effect(TuiEffect::SearchSessions {
                                request: Box::new(request),
                                policy,
                            });
                        }
                    } else if let Some(action) =
                        keymap.action_for_key(super::keymap::BmuxScope::SessionPicker, *stroke)
                    {
                        match action {
                            super::keymap::BmuxAction::SelectCancel => {
                                self.session_picker = None;
                                return SessionPickerRootOutcome::Canceled;
                            }
                            super::keymap::BmuxAction::SessionNew => {
                                self.session_picker = None;
                                return SessionPickerRootOutcome::Create;
                            }
                            super::keymap::BmuxAction::SessionRename => {
                                picker.start_rename();
                            }
                            super::keymap::BmuxAction::SessionDelete => {
                                picker.start_delete_confirmation();
                            }
                            super::keymap::BmuxAction::SelectConfirm => {
                                if let Some(import) = picker
                                    .selected_import()
                                    .filter(|import| import.imported_at_ms == 0)
                                    .cloned()
                                {
                                    picker.set_status("Importing session…".to_owned());
                                    chat.replace_effect(TuiEffect::ImportSession {
                                        source_id: import.source_id,
                                        external_session_id: import.external_session_id,
                                    });
                                } else if let Some(session_id) = picker.selected_session_id() {
                                    self.session_picker = None;
                                    return SessionPickerRootOutcome::Select(session_id);
                                }
                            }
                            super::keymap::BmuxAction::SelectUp => picker.select_previous(),
                            super::keymap::BmuxAction::SelectDown => picker.select_next(),
                            _ => {}
                        }
                    } else if super::text_input_flow::handle_key_in_scope(
                        picker.filter_mut(),
                        keymap,
                        "session_picker",
                        *stroke,
                    ) != bmux_tui_components::text_input::TextInputOutcome::Ignored
                    {
                        picker.refresh_filter();
                    }
                }
                super::session_picker::SessionPickerMode::Rename => {
                    if keymap.action("session_rename", *stroke) == Some("tui.control.cancel") {
                        picker.cancel_rename();
                    } else if keymap.action("session_rename", *stroke) == Some("tui.control.submit")
                    {
                        if let Some(session_id) = picker.selected_session_id() {
                            let name = picker.rename().buffer().text().trim();
                            let name = (!name.is_empty()).then(|| name.to_owned());
                            picker.finish_mutation("Renaming session…".to_owned());
                            chat.replace_effect(TuiEffect::RenameSession { session_id, name });
                        }
                    } else {
                        let _ = super::text_input_flow::handle_key_in_scope(
                            picker.rename_mut(),
                            keymap,
                            "session_rename",
                            *stroke,
                        );
                    }
                }
                super::session_picker::SessionPickerMode::DeleteConfirm => {
                    match keymap.action("session_delete", *stroke) {
                        Some("tui.control.cancel") => {
                            picker.cancel_delete();
                        }
                        Some("tui.control.submit") => {
                            if let Some(session_id) = picker.selected_session_id() {
                                picker.finish_mutation("Deleting session…".to_owned());
                                chat.replace_effect(TuiEffect::DeleteSession { session_id });
                            }
                        }
                        _ => {}
                    }
                }
                super::session_picker::SessionPickerMode::TranscriptSearch => match keymap
                    .action("session_search", *stroke)
                {
                    Some("tui.control.cancel") => picker.close_search_results(),
                    Some("tui.control.submit") => {
                        if let Some(hit) = picker.selected_search_result().cloned() {
                            self.session_picker = None;
                            return SessionPickerRootOutcome::SearchHit(hit);
                        }
                        let query = picker.filter().buffer().text().trim().to_owned();
                        if query.is_empty() {
                            picker.set_status(
                                "Type a transcript query, then confirm Search".to_owned(),
                            );
                        } else {
                            let Ok((request, policy)) = picker.search_request(&query, false) else {
                                picker.set_status(
                                    "Invalid search controls; invoke Search syntax for details"
                                        .to_owned(),
                                );
                                return SessionPickerRootOutcome::Handled;
                            };
                            picker.set_status("Searching transcripts…".to_owned());
                            chat.replace_effect(TuiEffect::SearchSessions {
                                request: Box::new(request),
                                policy,
                            });
                        }
                    }
                    Some("tui.search.mode") => {
                        picker.cycle_search_match_mode();
                    }
                    Some("tui.search.deep") => {
                        picker.toggle_search_depth();
                    }
                    Some("tui.search.content") => {
                        picker.cycle_search_sort();
                    }
                    Some("tui.search.nextMatch") => {
                        let query = picker.filter().buffer().text().trim().to_owned();
                        if query.is_empty() || !picker.has_next_search_page() {
                            picker.set_status("No next search page available".to_owned());
                        } else {
                            let Ok((request, policy)) = picker.search_request(&query, true) else {
                                picker.set_status(
                                    "Invalid search controls; invoke Search syntax for details"
                                        .to_owned(),
                                );
                                return SessionPickerRootOutcome::Handled;
                            };
                            picker.set_status("Loading next search page…".to_owned());
                            chat.replace_effect(TuiEffect::SearchSessions {
                                request: Box::new(request),
                                policy,
                            });
                        }
                    }
                    Some("tui.search.index") => {
                        picker.set_status("Inventorying canonical compatibility…".to_owned());
                        chat.replace_effect(TuiEffect::StartBulkMigration {
                            request: bcode_ipc::SessionBulkMigrationStartRequest {
                                mode: bcode_ipc::SessionBulkMigrationMode::Inventory,
                                session_ids: std::collections::BTreeSet::new(),
                                after_timestamp_ms: None,
                                before_timestamp_ms: None,
                                confirmation: None,
                            },
                        });
                    }
                    Some("tui.search.migrate") => {
                        if picker.confirm_canonical_migration() {
                            picker.set_status("Starting confirmed canonical migration…".to_owned());
                            chat.replace_effect(TuiEffect::StartBulkMigration {
                                request: bcode_ipc::SessionBulkMigrationStartRequest {
                                    mode: bcode_ipc::SessionBulkMigrationMode::Migrate,
                                    session_ids: std::collections::BTreeSet::new(),
                                    after_timestamp_ms: None,
                                    before_timestamp_ms: None,
                                    confirmation: Some(
                                        bcode_ipc::SESSION_BULK_MIGRATION_CONFIRMATION.to_owned(),
                                    ),
                                },
                            });
                        } else {
                            picker.set_status(
                                "Canonical migration is separate from indexing and creates verified backups. Invoke Migrate again to confirm"
                                    .to_owned(),
                            );
                        }
                    }
                    Some("tui.search.backfill") => {
                        picker.set_status("Starting separate derived search backfill…".to_owned());
                        chat.replace_effect(TuiEffect::StartSearchIndexing {
                            request: bcode_session_search::CompleteSessionSearchBackfillRequest {
                                provider_id: None,
                                session_ids: std::collections::BTreeSet::new(),
                                after_timestamp_ms: None,
                                before_timestamp_ms: None,
                                slice_deadline_ms: 30_000,
                            },
                        });
                    }
                    Some("tui.search.cancelMaintenance") => {
                        if let Some(operation_id) = picker.take_bulk_migration_operation_id() {
                            picker.set_status(
                                "Cancelling canonical migration between sessions…".to_owned(),
                            );
                            chat.replace_effect(TuiEffect::CancelBulkMigration { operation_id });
                        } else if let Some(operation_id) =
                            picker.take_search_backfill_operation_id()
                        {
                            picker.set_status("Cancelling derived search backfill…".to_owned());
                            chat.replace_effect(TuiEffect::CancelSearchIndexing { operation_id });
                        } else {
                            picker
                                .set_status("No active maintenance operation to cancel".to_owned());
                        }
                    }
                    Some("tui.search.help") => {
                        picker.set_status(super::session_search::SEARCH_CONTROL_HELP.to_owned());
                    }
                    Some("tui.control.up") => picker.select_previous(),
                    Some("tui.control.down") => picker.select_next(),
                    _ => {
                        let _ = super::text_input_flow::handle_key_in_scope(
                            picker.filter_mut(),
                            keymap,
                            "session_search",
                            *stroke,
                        );
                    }
                },
            },
            Event::Mouse(mouse) => picker.select_mouse(*mouse),
            Event::Focus(_) | Event::Resize(_) | Event::Tick | Event::User(_) => {}
        }
        if picker.mode() == super::session_picker::SessionPickerMode::Filter
            && picker.all_directories()
            && previous_query != picker.filter().buffer().text()
        {
            picker.restart_catalog();
            picker.replace_sessions(Vec::new());
            self.restart_session_picker_subscription(chat);
        }
        SessionPickerRootOutcome::Handled
    }

    pub const fn has_interactive_surface(&self) -> bool {
        self.interactive_surface.is_some()
    }

    pub fn active_root_screen(&self) -> super::root_program::BcodeRuntimeScreen {
        use super::foreground::Foreground;
        use super::root_program::BcodeRuntimeScreen;
        match self.foreground() {
            Some(Foreground::Plugin) => BcodeRuntimeScreen::PluginSurface,
            Some(Foreground::Sessions) => BcodeRuntimeScreen::SessionPicker,
            Some(Foreground::AuthPool) => BcodeRuntimeScreen::AuthPool,
            Some(Foreground::Model) => BcodeRuntimeScreen::ModelPicker,
            Some(Foreground::Skill) => BcodeRuntimeScreen::SkillPicker,
            Some(Foreground::Ralph) => BcodeRuntimeScreen::RalphStart,
            Some(Foreground::WorkingDirectory) => BcodeRuntimeScreen::WorkingDirectory,
            Some(Foreground::Worktree) => BcodeRuntimeScreen::WorktreeCreate,
            Some(Foreground::Permission) => BcodeRuntimeScreen::Permission,
            Some(Foreground::Streaming) => BcodeRuntimeScreen::Streaming,
            Some(Foreground::Timeline) => BcodeRuntimeScreen::Timeline,
            Some(Foreground::Thinking) => BcodeRuntimeScreen::Thinking,
            Some(Foreground::Theme) => BcodeRuntimeScreen::Theme,
            Some(Foreground::Commands) => BcodeRuntimeScreen::CommandPalette,
            Some(Foreground::Slash) => BcodeRuntimeScreen::SlashPalette,
            None if self.has_interactive_surface() => BcodeRuntimeScreen::InteractiveSurface,
            None => BcodeRuntimeScreen::Chat,
        }
    }

    pub fn dismiss_interactive_surface(
        &self,
    ) -> Option<(String, bcode_session_models::ToolExchangeResolution)> {
        let surface = self.interactive_surface.as_ref()?;
        Some((
            surface.interaction_id().to_owned(),
            InteractiveSurfaceState::dismissed_resolution(),
        ))
    }

    pub fn handle_interactive_surface_event(
        &mut self,
        event: &Event,
    ) -> InteractiveSurfaceEventOutcome {
        let translated;
        let event = if matches!(event, Event::Mouse(_)) {
            let Some(geometry) = self.interactive_surface_geometry else {
                return InteractiveSurfaceEventOutcome::Ignored;
            };
            if geometry.destination.is_empty()
                || !matches!(event, Event::Mouse(mouse) if geometry.destination.contains(mouse.position))
            {
                return InteractiveSurfaceEventOutcome::Ignored;
            }
            translated = InteractiveSurfaceState::translate_clipped_event(
                event.clone(),
                Rect::new(0, 0, geometry.destination.width, geometry.logical_height),
                geometry.visible_logical_offset,
                geometry.destination,
            );
            &translated
        } else {
            event
        };
        self.interactive_surface
            .as_mut()
            .map_or(InteractiveSurfaceEventOutcome::Ignored, |surface| {
                surface.handle_event_outcome(event)
            })
    }

    #[cfg(test)]
    pub fn active_interactive_surface_native_id_for_test(&self) -> Option<&'static str> {
        self.interactive_surface
            .as_ref()
            .map(InteractiveSurfaceState::surface_id_for_test)
    }

    #[cfg(test)]
    pub fn active_interactive_surface_work_shape_for_test(
        &self,
    ) -> bcode_plugin_sdk::tui::PluginTuiWorkShape {
        self.interactive_surface.as_ref().map_or_else(
            bcode_plugin_sdk::tui::PluginTuiWorkShape::default,
            InteractiveSurfaceState::work_shape_for_test,
        )
    }

    pub fn active_interactive_surface_id(&self) -> Option<&str> {
        self.interactive_surface
            .as_ref()
            .map(InteractiveSurfaceState::interaction_id)
    }

    #[cfg(test)]
    pub fn install_interactive_surface_for_test(&mut self, surface: InteractiveSurfaceState) {
        self.interactive_surface = Some(surface);
    }

    #[cfg(test)]
    pub const fn set_interactive_surface_geometry_for_test(
        &mut self,
        geometry: Option<InteractiveSurfaceGeometry>,
    ) {
        self.interactive_surface_geometry = geometry;
    }

    #[cfg(test)]
    pub fn latched_surface_outcome_for_test(
        &mut self,
    ) -> Option<bcode_session_models::ToolExchangeResolution> {
        match self
            .interactive_surface
            .as_mut()?
            .handle_event_outcome(&Event::Tick)
        {
            InteractiveSurfaceEventOutcome::Resolved(resolution) => Some(resolution),
            InteractiveSurfaceEventOutcome::Ignored
            | InteractiveSurfaceEventOutcome::Consumed
            | InteractiveSurfaceEventOutcome::Redraw
            | InteractiveSurfaceEventOutcome::Relayout => None,
        }
    }

    pub fn complete_interactive_surface_resolution(&mut self, resolved: bool) {
        if resolved {
            self.interactive_surface = None;
        } else if let Some(surface) = self.interactive_surface.as_mut() {
            surface.clear_pending_resolution();
        }
    }

    fn accept_markdown_projection_completion(
        &mut self,
        chat: &mut ActiveChat,
        completion: super::markdown_projection_coordinator::MarkdownProjectionCompletion,
    ) -> bool {
        if self.markdown_projection.latest_requested() != Some(&completion.generation) {
            return false;
        }
        let Some((index, item)) = chat
            .app
            .transcript()
            .iter()
            .enumerate()
            .find(|(_, item)| item.id().get() == completion.generation.item_id)
        else {
            return false;
        };
        if item.revision() != completion.generation.item_revision {
            return false;
        }
        let render_duration_ms =
            u64::try_from(completion.render_duration.as_millis()).unwrap_or(u64::MAX);
        self.telemetry
            .record_histogram("tui.markdown.projection.render_ms", render_duration_ms);
        match completion.outcome {
            super::markdown_projection_coordinator::MarkdownProjectionOutcome::Rendered(result) => {
                chat.app.transcript_markdown_cache().install(
                    completion.generation.item_id,
                    completion.generation.item_revision,
                    completion.generation.options.clone(),
                    result,
                );
                self.markdown_projection.complete(&completion.generation);
                self.telemetry
                    .add_counter("tui.markdown.projection.accepted_total", 1);
                chat.app.mark_transcript_item_dirty(index);
                true
            }
            super::markdown_projection_coordinator::MarkdownProjectionOutcome::Failed(_) => {
                self.markdown_projection.complete(&completion.generation);
                self.telemetry
                    .add_counter("tui.markdown.projection.failed_total", 1);
                false
            }
        }
    }

    fn request_latest_markdown_projection(&mut self, chat: &ActiveChat, width: u16) {
        let Some(item) = chat
            .app
            .transcript()
            .iter()
            .rev()
            .find(|item| item.text_format() == bcode_session_view_models::TextFormat::Markdown)
        else {
            return;
        };
        let options = render::markdown_render_options(&chat.app, item, width);
        if chat
            .app
            .transcript_markdown_cache()
            .contains(item.id().get(), item.revision(), &options)
        {
            return;
        }
        if self
            .markdown_projection
            .latest_requested()
            .is_some_and(|generation| {
                generation.item_id == item.id().get()
                    && generation.item_revision == item.revision()
                    && generation.options == options
            })
        {
            return;
        }
        if self.markdown_projection.latest_requested().is_some() {
            self.telemetry
                .add_counter("tui.markdown.projection.coalesced_total", 1);
        }
        self.telemetry
            .add_counter("tui.markdown.projection.requested_total", 1);
        self.markdown_projection.request(
            super::markdown_projection_coordinator::MarkdownProjectionRequest {
                generation: super::markdown_projection_coordinator::MarkdownProjectionGeneration {
                    item_id: item.id().get(),
                    item_revision: item.revision(),
                    options,
                },
                source: item.text().to_owned(),
            },
        );
    }

    pub fn abort_all_effects(&mut self) {
        self.markdown_projection.invalidate();
        if let Some(markdown) = &mut self.markdown_presentation {
            markdown.cancel_all();
        }
        if let Some(mermaid) = &mut self.markdown_mermaid {
            mermaid.cancel_all();
        }
        for task in self.markdown_image_tasks.drain(..) {
            task.abort();
        }
        for task in self.markdown_mermaid_tasks.drain(..) {
            task.abort();
        }
    }

    pub fn queue_effect_cancellation(chat: &mut ActiveChat, effect: TuiEffect) {
        chat.pending_effects.cancel(effect);
    }
}

#[derive(Debug)]
struct DaemonConnectionMonitor {
    saw_success: bool,
    startup_phase: Option<bcode_metrics::startup::Phase>,
}

impl Default for DaemonConnectionMonitor {
    fn default() -> Self {
        Self {
            saw_success: false,
            startup_phase: Some(bcode_metrics::startup::phase("tui.initial_connection")),
        }
    }
}

impl DaemonConnectionMonitor {
    fn observe(
        &mut self,
        observation: &DaemonObservation,
    ) -> Option<super::app::DaemonConnectionState> {
        match observation {
            DaemonObservation::None | DaemonObservation::Failed(_) => None,
            DaemonObservation::Success => {
                if let Some(phase) = self.startup_phase.take() {
                    phase.finish();
                    bcode_metrics::startup::ready();
                }
                self.saw_success = true;
                Some(super::app::DaemonConnectionState::Connected)
            }
            DaemonObservation::Unavailable(_) => Some(if self.saw_success {
                super::app::DaemonConnectionState::IdleOffline
            } else {
                super::app::DaemonConnectionState::Unavailable
            }),
        }
    }
}

#[cfg(test)]
mod daemon_connection_monitor_tests {
    use super::{DaemonConnectionMonitor, DaemonObservation};
    use crate::app::DaemonConnectionState;

    #[test]
    fn application_failure_does_not_change_connectivity_state() {
        let mut monitor = DaemonConnectionMonitor::default();
        assert_eq!(
            monitor.observe(&DaemonObservation::Failed("rejected".to_owned())),
            None
        );
        assert_eq!(
            monitor.observe(&DaemonObservation::Success),
            Some(DaemonConnectionState::Connected)
        );
        assert_eq!(
            monitor.observe(&DaemonObservation::Failed("rejected".to_owned())),
            None
        );
    }

    #[test]
    fn unavailability_tracks_initial_and_post_success_offline_states() {
        let mut monitor = DaemonConnectionMonitor::default();
        assert_eq!(
            monitor.observe(&DaemonObservation::Unavailable("offline".to_owned())),
            Some(DaemonConnectionState::Unavailable)
        );
        assert_eq!(
            monitor.observe(&DaemonObservation::Success),
            Some(DaemonConnectionState::Connected)
        );
        assert_eq!(
            monitor.observe(&DaemonObservation::Unavailable("offline".to_owned())),
            Some(DaemonConnectionState::IdleOffline)
        );
    }
}

pub struct TuiRuntimeSettings {
    keymap: BmuxKeyMap,
    mouse_scroll_rows: usize,
    selection_autoscroll: bool,
    selection: bcode_config::TuiSelectionConfig,
    completion_settings: bcode_config::CompletionConfig,
    frame_interval: Option<Duration>,
    runtime_config: bmux_tui_runtime::RuntimeConfig,
    metrics_enabled: bool,
    static_plugins: Vec<bcode_plugin::StaticBundledPlugin>,
    tui_extensions: Vec<bcode_plugin_sdk::tui::StaticPluginTuiExtension>,
    launch_working_directory: std::path::PathBuf,
    launch_options: super::TuiLaunchOptions,
}

impl TuiRuntimeSettings {
    pub fn bootstrap(
        launch_working_directory: std::path::PathBuf,
        static_plugins: &[bcode_plugin::StaticBundledPlugin],
    ) -> Self {
        let tui_config = TuiConfig::default();
        Self {
            keymap: BmuxKeyMap::from_config(&tui_config),
            mouse_scroll_rows: tui_config.mouse.effective_scroll_rows(),
            selection_autoscroll: tui_config.mouse.selection_autoscroll,
            selection: tui_config.selection,
            completion_settings: bcode_config::CompletionConfig::default(),
            frame_interval: tui_config.render.frame_interval(),
            runtime_config: super::runtime_adapter::config(&tui_config),
            metrics_enabled: false,
            static_plugins: static_plugins.to_vec(),
            tui_extensions: super::bundled_tui_extensions(),
            launch_working_directory,
            launch_options: super::TuiLaunchOptions::default(),
        }
    }

    #[must_use]
    pub const fn with_launch_options(mut self, launch_options: super::TuiLaunchOptions) -> Self {
        self.launch_options = launch_options;
        self
    }

    /// Return renderer-neutral command completion preferences.
    #[must_use]
    pub const fn completion_settings(&self) -> bcode_config::CompletionConfig {
        self.completion_settings
    }

    /// Update command completion preferences without rereading configuration.
    pub const fn set_completion_settings(&mut self, settings: bcode_config::CompletionConfig) {
        self.completion_settings = settings;
    }

    pub fn integrate_keymap_contributors(
        &mut self,
        actions: Result<
            Vec<bcode_plugin_sdk::tui::keymap::KeymapAction>,
            bcode_plugin_sdk::tui::keymap::KeymapError,
        >,
    ) {
        self.keymap.integrate_contributors(actions);
    }

    pub fn apply_tui_config(&mut self, tui_config: &TuiConfig) {
        self.keymap = BmuxKeyMap::from_config(tui_config);
        self.mouse_scroll_rows = tui_config.mouse.effective_scroll_rows();
        self.selection_autoscroll = tui_config.mouse.selection_autoscroll;
        self.selection = tui_config.selection;
        self.frame_interval = tui_config.render.frame_interval();
        self.runtime_config = super::runtime_adapter::config(tui_config);
    }

    /// Build the domain-neutral BMUX runtime configuration for current TUI settings.
    #[must_use]
    pub const fn bmux_runtime_config(&self) -> bmux_tui_runtime::RuntimeConfig {
        self.runtime_config
    }

    pub const fn set_metrics_enabled(&mut self, enabled: bool) {
        self.metrics_enabled = enabled;
    }

    pub const fn metrics_enabled(&self) -> bool {
        self.metrics_enabled
    }

    pub const fn keymap(&self) -> &BmuxKeyMap {
        &self.keymap
    }

    /// Current transcript copy and clearing policy.
    #[must_use]
    pub const fn selection(&self) -> &bcode_config::TuiSelectionConfig {
        &self.selection
    }

    pub const fn selection_autoscroll(&self) -> bool {
        self.selection_autoscroll
    }

    pub const fn mouse_scroll_rows(&self) -> usize {
        self.mouse_scroll_rows
    }

    pub fn static_plugins(&self) -> &[bcode_plugin::StaticBundledPlugin] {
        &self.static_plugins
    }

    pub fn tui_extensions(&self) -> &[bcode_plugin_sdk::tui::StaticPluginTuiExtension] {
        &self.tui_extensions
    }

    pub fn launch_working_directory(&self) -> &std::path::Path {
        &self.launch_working_directory
    }

    pub const fn launch_options(&self) -> super::TuiLaunchOptions {
        self.launch_options
    }
}

fn maybe_start_older_history_load(
    chat: &mut ActiveChat,
    _loop_state: &mut ChatLoopState,
    width_columns: u16,
) -> bool {
    if !chat.app.should_load_older_history() {
        return false;
    }
    let Some(cursor) = chat.app.older_history_cursor() else {
        return false;
    };
    let Some(session_id) = chat.attached_session_id() else {
        return false;
    };
    let generation = chat.app.history_window_generation();
    let effect = TuiEffect::LoadOlderHistory {
        session_id,
        cursor,
        generation,
        width_columns,
    };
    let started = !chat.pending_effects.contains_effect(&effect);
    // A replacement window can request the same direction while the old effect
    // is still running. Replace it; generation/cursor checks fence late results.
    chat.replace_effect(effect);
    if started {
        chat.app.set_loading_older_history(true);
    }
    started
}

fn maybe_start_newer_history_load(
    chat: &mut ActiveChat,
    _loop_state: &mut ChatLoopState,
    width_columns: u16,
) -> bool {
    if !chat.app.should_load_newer_history() {
        return false;
    }
    let Some(cursor) = chat.app.newer_history_cursor() else {
        return false;
    };
    let Some(session_id) = chat.attached_session_id() else {
        return false;
    };
    let generation = chat.app.history_window_generation();
    let effect = TuiEffect::LoadNewerHistory {
        session_id,
        cursor,
        generation,
        width_columns,
    };
    let started = !chat.pending_effects.contains_effect(&effect);
    chat.replace_effect(effect);
    if started {
        chat.app.set_loading_newer_history(true);
    }
    started
}

fn bulk_migration_status_message(
    status: &bcode_ipc::SessionBulkMigrationOperationStatus,
) -> String {
    let state = match status.state {
        bcode_ipc::SessionBulkMigrationState::Running => "running",
        bcode_ipc::SessionBulkMigrationState::CancellationRequested => "cancelling",
        bcode_ipc::SessionBulkMigrationState::Completed => "completed",
        bcode_ipc::SessionBulkMigrationState::NeedsAttention => "needs attention",
        bcode_ipc::SessionBulkMigrationState::Cancelled => "cancelled",
    };
    format!(
        "Canonical migration {state}: visited {}, migrated {}, blocked {}, failed {}",
        status.visited, status.migrated, status.blocked, status.failed
    )
}

fn search_backfill_status_message(
    status: &bcode_session_search::SessionSearchBackfillOperationStatus,
) -> String {
    let state = match status.state {
        bcode_session_search::SessionSearchBackfillOperationState::Running => "running",
        bcode_session_search::SessionSearchBackfillOperationState::CancellationRequested => {
            "cancelling"
        }
        bcode_session_search::SessionSearchBackfillOperationState::Completed => "completed",
        bcode_session_search::SessionSearchBackfillOperationState::NeedsAttention => {
            "needs attention"
        }
        bcode_session_search::SessionSearchBackfillOperationState::Cancelled => "cancelled",
        bcode_session_search::SessionSearchBackfillOperationState::Failed => "failed",
    };
    let Some(progress) = status.complete_progress.as_ref() else {
        return format!(
            "Derived search backfill {state}: revision {}",
            status.revision
        );
    };
    let unvisited = progress
        .selected_sessions
        .saturating_sub(progress.visited_sessions);
    format!(
        "Derived search backfill {state}: visited {}, complete {}, blocked/incomplete {}, failed {}, unvisited {}",
        progress.visited_sessions,
        progress.completed_sessions,
        progress.incomplete_sessions,
        progress.failed_sessions,
        unvisited
    )
}

// Failed command responses may carry diagnostics but cannot request native effects.
fn plugin_command_presentation(
    response: bcode_command::InvokeCommandResponse,
) -> (Option<String>, Vec<bcode_command::CommandEffect>) {
    if response.success {
        (response.message, response.effects)
    } else {
        (
            Some(
                response
                    .message
                    .unwrap_or_else(|| "Plugin command failed".to_owned()),
            ),
            Vec::new(),
        )
    }
}

#[allow(clippy::too_many_lines)]
pub fn apply_effect_result(
    settings: &mut TuiRuntimeSettings,
    chat: &mut ActiveChat,
    draft_autosave: &mut DraftAutosave,
    loop_state: &mut ChatLoopState,
    result: TuiEffectResult,
) {
    match result {
        TuiEffectResult::SessionChildrenLoaded {
            parent_id,
            generation,
            result,
        } => {
            if !chat.app.accepts_child_result(false, parent_id, generation) {
                return;
            }
            if let Ok(page) = &result {
                chat.app.apply_monitor_children(parent_id, page);
            }
            if let Some(picker) = loop_state.session_picker.as_mut() {
                match result {
                    Ok(page) => picker.apply_children(parent_id, page),
                    Err(error) => {
                        picker.set_status(format!("Children unavailable/degraded: {error}"));
                    }
                }
            }
        }
        TuiEffectResult::ChildActivityLoaded {
            session_id,
            generation,
            result,
        } => {
            if !chat.app.accepts_child_result(true, session_id, generation) {
                return;
            }
            if chat.attached_session_id() == Some(session_id) {
                chat.app.apply_child_activity(result);
            } else if chat.app.monitor_discovery_frontier().contains(&session_id) {
                match result {
                    Ok(page) => chat.app.apply_monitor_activity(session_id, page),
                    Err(error) => chat
                        .app
                        .monitor_observation_failed(session_id, &error.to_string()),
                }
            }
        }
        TuiEffectResult::SessionOpenProgress { snapshot } => {
            apply_session_open_progress(chat, &snapshot);
        }
        TuiEffectResult::SessionOpened {
            session_id,
            has_older_history,
            result,
        } => {
            if result.is_ok() {
                loop_state.artifact_stream.retain_session(Some(session_id));
            }
            if let Ok((attached, _)) = result.as_ref() {
                let presentation = chat.app.plugin_presentation();
                for event in &attached.history {
                    loop_state.artifact_stream.observe_finalized_artifact(
                        event.session_id,
                        event.sequence,
                        &event.kind,
                        |producer_plugin_id,
                         schema,
                         schema_version,
                         reference_key,
                         content_type| {
                            presentation.is_some_and(|presentation| {
                                presentation.accepts_artifact_reference(
                                    producer_plugin_id,
                                    schema,
                                    schema_version,
                                    reference_key,
                                    content_type,
                                )
                            })
                        },
                    );
                }
            }
            session_flow::complete_switch_session(chat, session_id, has_older_history, *result);
            if chat.attached_session_id() == Some(session_id) {
                chat.replace_effect(TuiEffect::LoadChildActivity {
                    session_id,
                    after: None,
                    generation: 0,
                });
                chat.replace_effect(TuiEffect::LoadSessionChildren {
                    parent_id: session_id,
                    after: None,
                    generation: 0,
                });
            }
            loop_state.interactive_surface = None;
            loop_state.interactive_surface_queue.clear();
        }
        TuiEffectResult::ConfigLoaded {
            config,
            theme_selection,
            streaming_presentation,
        } => {
            apply_config_result(
                settings,
                chat,
                loop_state,
                *config,
                theme_selection,
                streaming_presentation,
            );
        }
        TuiEffectResult::ThemeSelectionPersisted { name, result } => match result {
            Ok(path) => {
                if let Some(name) = name {
                    if chat.app.apply_theme(&name) {
                        chat.app.set_status(format!(
                            "theme {name} saved to user state at {}",
                            bcode_plugin_sdk::path::display_from_current_dir(&path)
                        ));
                    } else {
                        chat.app.cancel_theme_preview();
                        chat.app.set_status(format!(
                            "theme {name} saved, but is not available in this repository"
                        ));
                    }
                } else {
                    chat.app.set_status(format!(
                        "theme override cleared from {}; reloading configuration…",
                        bcode_plugin_sdk::path::display_from_current_dir(&path)
                    ));
                    chat.replace_effect(TuiEffect::LoadConfig);
                }
            }
            Err(error) => {
                chat.app.cancel_theme_preview();
                let action = name.as_deref().unwrap_or("configured default");
                chat.app
                    .set_status(format!("could not save theme {action}: {error}"));
            }
        },
        TuiEffectResult::StreamingPresentationPersisted { policy, result } => match result {
            Ok(path) => {
                loop_state.streaming_presentation_override = policy;
                let effective = policy.unwrap_or(loop_state.declarative_streaming_policy);
                let _ = chat.app.apply_streaming_presentation_policy(effective);
                let action = if policy.is_some() {
                    "streaming presentation saved"
                } else {
                    "streaming presentation override cleared"
                };
                chat.app.set_status(format!(
                    "{action} in {}",
                    bcode_plugin_sdk::path::display_from_current_dir(&path)
                ));
            }
            Err(error) => chat
                .app
                .set_status(format!("could not update streaming presentation: {error}")),
        },
        TuiEffectResult::AuthSecurityReconciled { status } => {
            apply_auth_security_result(chat, status);
        }
        TuiEffectResult::DraftStatusLoaded {
            daemon_connected: _,
            model,
            composer_draft,
            error,
        } => {
            apply_draft_status_result(chat, model, composer_draft, error);
        }
        TuiEffectResult::SessionStatusLoaded {
            daemon_connected: _,
            session_id,
            hydration,
        } => {
            apply_session_status_result(chat, loop_state, session_id, *hydration);
        }
        TuiEffectResult::QueueRecovery {
            session_id,
            generation,
            result,
        } => {
            if chat.viewing_session_id() == Some(session_id)
                && chat.app.queue_recovery.generation == generation
            {
                match *result {
                    Ok((outcome, status)) => {
                        chat.app
                            .apply_queue_recovery_result(generation, outcome, status);
                    }
                    Err(error) => {
                        report_nonfatal_client_error(chat, "Queue recovery unavailable", &error);
                    }
                }
            }
        }
        TuiEffectResult::SessionModelStatusLoaded { session_id, result } => {
            if chat.viewing_session_id() == Some(session_id) {
                match result {
                    Ok(status)
                        if status
                            .requested_model_id
                            .as_deref()
                            .or(status.model_id.as_deref())
                            == chat.app.selected_model_id() =>
                    {
                        chat.app.apply_model_status(status);
                    }
                    Ok(_stale) => {}
                    Err(error) => report_nonfatal_client_error(
                        chat,
                        "model metadata refresh unavailable",
                        &error,
                    ),
                }
            }
        }
        TuiEffectResult::PluginStatusLoaded {
            session_id,
            plugin_status,
            error,
        } => {
            if chat.viewing_session_id() == Some(session_id) {
                chat.app.set_plugin_status(plugin_status);
                if let Some(error) = error {
                    chat.app
                        .set_status(format!("Plugin status unavailable: {error}"));
                }
            }
        }
        TuiEffectResult::AgentCatalogLoaded { agents } => {
            apply_agent_catalog_result(chat, agents);
        }
        TuiEffectResult::OlderHistoryLoaded {
            session_id,
            cursor,
            generation,
            result,
        } => {
            apply_older_history_result(chat, loop_state, session_id, cursor, generation, result);
        }
        TuiEffectResult::NewerHistoryLoaded {
            session_id,
            cursor,
            generation,
            result,
        } => {
            apply_newer_history_result(chat, loop_state, session_id, cursor, generation, result);
        }
        TuiEffectResult::PermissionList { result } => {
            apply_permission_list_result(chat, loop_state, result);
        }
        TuiEffectResult::SaveDraft { text, result } => {
            apply_save_draft_result(chat, draft_autosave, text, result);
        }
        TuiEffectResult::SlashPaletteLoaded {
            query,
            palette,
            generation,
        } => {
            apply_slash_palette_result(chat, loop_state, &query, palette, generation);
        }
        TuiEffectResult::CommandPaletteLoaded { result } => {
            if loop_state.palette.is_some() {
                match result {
                    Ok(contributions) => {
                        loop_state.palette = Some(BmuxCommandPalette::with_command_contributions(
                            contributions,
                        ));
                    }
                    Err(error) => chat.app.set_status(format!(
                        "plugin commands unavailable; using host commands: {error}"
                    )),
                }
            }
        }
        TuiEffectResult::SessionPickerLoaded {
            result,
            generation,
            restart,
        } => {
            loop_state.apply_session_catalog_update(generation, result, restart);
        }
        TuiEffectResult::SessionImported { result } => {
            if let Some(session_id) = loop_state.apply_session_import_result(result) {
                super::session_flow::start_switch_session(
                    chat,
                    session_id,
                    super::history_flow::initial_transcript_window_request(
                        bmux_tui::geometry::Rect::new(0, 0, 80, 24),
                    ),
                );
            }
        }
        TuiEffectResult::SessionRenamed { result } => {
            loop_state.apply_session_mutation_result("renamed", result);
            loop_state.restart_session_picker_subscription(chat);
        }
        TuiEffectResult::SessionDeleted { result } => {
            loop_state.apply_session_mutation_result("deleted", result);
            loop_state.restart_session_picker_subscription(chat);
        }
        TuiEffectResult::SessionsSearched { result } => {
            loop_state.apply_session_search_result(result);
        }
        TuiEffectResult::BulkMigrationUpdated { result } => {
            if let Some(picker) = loop_state.session_picker.as_mut() {
                match result {
                    Ok(status) => {
                        picker.set_bulk_migration_operation_id(status.operation_id.clone());
                        picker.set_status(bulk_migration_status_message(&status));
                        if matches!(
                            status.state,
                            bcode_ipc::SessionBulkMigrationState::Running
                                | bcode_ipc::SessionBulkMigrationState::CancellationRequested
                        ) {
                            chat.replace_effect(TuiEffect::WaitBulkMigration {
                                operation_id: status.operation_id,
                                after_revision: status.revision,
                            });
                        }
                    }
                    Err(error) => picker.set_status(format!("Canonical migration failed: {error}")),
                }
            }
        }
        TuiEffectResult::BulkMigrationWaited { result } => {
            if let Some(picker) = loop_state.session_picker.as_mut() {
                match result {
                    Ok(status) => {
                        picker.set_bulk_migration_operation_id(status.operation_id.clone());
                        picker.set_status(bulk_migration_status_message(&status));
                        if matches!(
                            status.state,
                            bcode_ipc::SessionBulkMigrationState::Running
                                | bcode_ipc::SessionBulkMigrationState::CancellationRequested
                        ) {
                            chat.replace_effect(TuiEffect::WaitBulkMigration {
                                operation_id: status.operation_id,
                                after_revision: status.revision,
                            });
                        }
                    }
                    Err(error) => {
                        picker.set_status(format!("Canonical migration wait failed: {error}"));
                    }
                }
            }
        }
        TuiEffectResult::SearchIndexingStarted { result } => {
            if let Some(picker) = loop_state.session_picker.as_mut() {
                match result {
                    Ok(started) => {
                        picker.set_search_backfill_operation_id(started.operation_id.clone());
                        picker.set_status(format!(
                            "Derived search backfill started: {}. Invoke Cancel maintenance to cancel",
                            started.operation_id
                        ));
                        chat.replace_effect(TuiEffect::WaitSearchIndexing {
                            operation_id: started.operation_id,
                            after_revision: 0,
                        });
                    }
                    Err(error) => picker.set_status(format!("Search backfill failed: {error}")),
                }
            }
        }
        TuiEffectResult::SearchIndexingUpdated { result }
        | TuiEffectResult::SearchIndexingWaited { result } => {
            if let Some(picker) = loop_state.session_picker.as_mut() {
                match result {
                    Ok(status) => {
                        picker.set_search_backfill_operation_id(status.operation_id.clone());
                        picker.set_status(search_backfill_status_message(&status));
                        if matches!(
                            status.state,
                            bcode_session_search::SessionSearchBackfillOperationState::Running
                                | bcode_session_search::SessionSearchBackfillOperationState::CancellationRequested
                        ) {
                            chat.replace_effect(TuiEffect::WaitSearchIndexing {
                                operation_id: status.operation_id,
                                after_revision: status.revision,
                            });
                        }
                    }
                    Err(error) => {
                        picker.set_status(format!("Search backfill action failed: {error}"));
                    }
                }
            }
        }
        TuiEffectResult::SlashCommandExecuted { message, result } => match result {
            Ok(outcome) => {
                apply_root_slash_command_outcome(settings, chat, loop_state, &message, outcome);
            }
            Err(error) => {
                chat.app.restore_pending_submission(&message);
                report_nonfatal_client_error(chat, "Slash command failed", &error);
            }
        },
        TuiEffectResult::PluginSurfaceOpened { plugin_id, result } => match result {
            Ok(surface) => {
                loop_state.queue_root_plugin_surface(plugin_id, surface);
                chat.app.set_status("plugin surface opened".to_owned());
            }
            Err(error) => report_nonfatal_tui_error(chat, "Plugin surface unavailable", &error),
        },
        TuiEffectResult::RalphStarted { result } => match result {
            Ok(output) => {
                chat.push_presentation_markdown("bcode.ralph", output.markdown);
                chat.app.set_status(output.status);
            }
            Err(error) => report_nonfatal_tui_error(chat, "Ralph loop creation failed", &error),
        },
        TuiEffectResult::RalphAction { action, result } => match result {
            Ok(output) => {
                if let Some(markdown) = output.markdown {
                    chat.push_presentation_markdown("bcode.ralph", markdown);
                }
                chat.app.set_status(output.status);
            }
            Err(error) => {
                report_nonfatal_tui_error(chat, &format!("Ralph {action:?} action failed"), &error);
            }
        },
        TuiEffectResult::ModelProvidersLoaded { result } => match result {
            Ok(providers) if providers.len() > 1 => {
                loop_state.provider_picker =
                    Some(super::provider_picker::ProviderPickerApp::new(providers));
                chat.app.set_status("select a model provider".to_owned());
            }
            Ok(providers) => {
                let provider_plugin_id =
                    providers.first().map(|provider| provider.plugin_id.clone());
                chat.replace_effect(TuiEffect::LoadModelPicker { provider_plugin_id });
                chat.app.set_status("loading models…".to_owned());
            }
            Err(error) => report_nonfatal_client_error(chat, "Model providers unavailable", &error),
        },
        TuiEffectResult::AuthPoolPickerLoaded { result } => match result {
            Ok(pools) if pools.iter().any(|pool| !pool.profiles.is_empty()) => {
                loop_state.auth_pool_picker =
                    Some(super::auth_pool_picker::AuthPoolPickerApp::new(pools));
                chat.app
                    .set_status("select the preferred subscription".to_owned());
            }
            Ok(_) => chat.app.set_status("no auth pools configured".to_owned()),
            Err(error) => report_nonfatal_client_error(chat, "Auth pools unavailable", &error),
        },
        TuiEffectResult::AuthPoolPreferenceSet {
            pool,
            profile,
            result,
        } => match result {
            Ok(()) => {
                chat.app.set_status(profile.map_or_else(
                    || format!("cleared interactive preference for {pool}"),
                    |profile| format!("preferred subscription for {pool}: {profile}"),
                ));
            }
            Err(error) => {
                report_nonfatal_client_error(chat, "Auth pool preference update failed", &error);
            }
        },
        TuiEffectResult::SelectedModelChecked {
            session_id,
            provider_plugin_id,
            generation,
            result,
            models,
        } => {
            loop_state.model_check_in_flight = false;
            if chat.attached_session_id() != session_id {
                return;
            }
            let Some(model) = loop_state.model_picker.as_mut().filter(|model| {
                model.generation == generation && model.provider_plugin_id == provider_plugin_id
            }) else {
                return;
            };
            let status = result
                .as_ref()
                .ok()
                .and_then(|report| report.results.first())
                .map_or_else(
                    || "Check unavailable; retry when the service is available".to_owned(),
                    |result| {
                        use bcode_model::ModelCheckStatus;
                        let outcome = match result.status {
                            ModelCheckStatus::Working => "Verified: completed provider request",
                            ModelCheckStatus::Unsupported => {
                                "Unavailable: serving surface unsupported"
                            }
                            ModelCheckStatus::AccountDenied => {
                                "Unavailable: current account lacks access"
                            }
                            ModelCheckStatus::Inconclusive => "Unverified: check inconclusive",
                            ModelCheckStatus::Timeout => "Unverified: check timed out",
                            ModelCheckStatus::Cancelled => "Unverified: check cancelled",
                        };
                        format!(
                            "{outcome}{}{}",
                            if result.evidence_persisted {
                                " · saved"
                            } else {
                                " · not saved"
                            },
                            if models.is_err() {
                                " · list refresh unavailable"
                            } else {
                                ""
                            }
                        )
                    },
                );
            model.picker.finish_check(models.ok(), status);
        }
        TuiEffectResult::ModelPickerLoaded {
            provider_plugin_id,
            result,
        } => match result {
            Ok(models) => {
                let status = provider_plugin_id.as_ref().map_or_else(
                    || "Select a model".to_owned(),
                    |provider| format!("Select a model from {provider}"),
                );
                loop_state.model_picker_generation =
                    loop_state.model_picker_generation.wrapping_add(1);
                loop_state.model_picker = Some(RootModelPicker {
                    generation: loop_state.model_picker_generation,
                    provider_plugin_id,
                    picker: super::model_picker::ModelPickerApp::from_model_list(models, status),
                });
                chat.app.set_status("select a model".to_owned());
            }
            Err(error) => report_nonfatal_client_error(chat, "Model list unavailable", &error),
        },
        TuiEffectResult::SkillPickerLoaded { result } => match result {
            Ok(skills) if skills.skills.is_empty() => {
                chat.app.set_status("no skills available".to_owned());
                chat.append_durable_presentation_note(
                    "bcode.host",
                    "No skills are available.".to_owned(),
                    bcode_command::CommandTextFormat::PlainText,
                );
            }
            Ok(skills) => {
                loop_state.skill_picker =
                    Some(super::skill_picker::SkillPickerApp::new(skills.skills));
                chat.app.set_status("select a skill".to_owned());
            }
            Err(error) => report_nonfatal_client_error(chat, "Skills unavailable", &error),
        },
        TuiEffectResult::SkillDescribed { skill_id, result } => match result {
            Ok(manifest) => {
                chat.append_durable_presentation_note(
                    "bcode.host",
                    super::skill_flow::format_skill_manifest_markdown(&manifest),
                    bcode_command::CommandTextFormat::Markdown,
                );
                chat.app.set_status(format!("shown skill {skill_id}"));
            }
            Err(error) => report_nonfatal_client_error(chat, "Skill details unavailable", &error),
        },
        TuiEffectResult::MilestonesLoaded { request, result } => {
            if loop_state.milestone_requests.accept(
                request,
                chat.viewing_session_id(),
                loop_state.timeline_dialog.as_mut(),
            ) {
                match result {
                    Ok(page) => {
                        let truncated = page.next_after_sequence.is_some();
                        if let Some(dialog) = loop_state.timeline_dialog.as_mut() {
                            dialog.replace_milestone_page(&page);
                        }
                        chat.app.set_status(
                            if truncated {
                                "milestones: more pages available"
                            } else {
                                "milestones: final page"
                            }
                            .to_owned(),
                        );
                    }
                    Err(error) => report_nonfatal_tui_error(
                        chat,
                        "milestones unavailable",
                        &TuiError::from(error),
                    ),
                }
            }
        }
        TuiEffectResult::ThinkingDialogLoaded { focus, result } => match result {
            Ok(mut status) => {
                if let Some(pending_effort) = chat.app.pending_reasoning_effort() {
                    status.reasoning_effort = Some(pending_effort.to_owned());
                }
                chat.app.apply_model_status(status.clone());
                loop_state.thinking_dialog =
                    Some(super::thinking_dialog::ThinkingDialogState::new_focused(
                        chat.app.reasoning_visible(),
                        chat.app.reasoning_display_mode(),
                        &status,
                        focus,
                    ));
                chat.app
                    .set_status("reasoning output settings: enter apply, esc cancel".to_owned());
            }
            Err(error) => {
                report_nonfatal_client_error(chat, "Reasoning output settings unavailable", &error);
            }
        },
        TuiEffectResult::TimelineJumpLoaded {
            session_id,
            generation,
            sequence,
            result,
        } => {
            if chat.viewing_session_id() != Some(session_id)
                || chat.app.history_window_generation() != generation
            {
                return;
            }
            match result {
                Ok((events, has_older, has_newer, submissions)) => {
                    if !super::history_flow::valid_timeline_window(
                        session_id, sequence, &events, has_older, has_newer,
                    ) {
                        chat.app.set_status(
                            "Timeline history window inconsistent; retry or inspect session history"
                                .to_owned(),
                        );
                        return;
                    }
                    chat.app
                        .replace_transcript_window(&events, has_older, has_newer, sequence);
                    chat.app.apply_submission_records(&submissions);
                    chat.app.request_transcript_top_anchor_sequence(sequence);
                    chat.app.set_status("jumped to timeline message".to_owned());
                }
                Err(error) => report_nonfatal_tui_error(chat, "Timeline jump unavailable", &error),
            }
        }
        TuiEffectResult::PluginCommandInvoked { plugin_id, result } => match result {
            Ok(response) => {
                let (message, effects) = plugin_command_presentation(response);
                if let Some(message) = message {
                    chat.app.set_status(message);
                }
                for effect in effects {
                    match effect {
                        bcode_command::CommandEffect::Status { message } => {
                            chat.app.set_status(message);
                        }
                        bcode_command::CommandEffect::CopyText { text } => {
                            let status = match super::markdown_activation::copy_text(&text) {
                                Ok(()) => "copied to clipboard".to_owned(),
                                Err(error) => format!("copy failed: {error}"),
                            };
                            chat.app.set_status(status);
                        }
                        bcode_command::CommandEffect::AppendText { text, format } => {
                            chat.append_durable_presentation_note(plugin_id.clone(), text, format);
                        }
                        bcode_command::CommandEffect::ToggleSurface { surface_id } => chat
                            .app
                            .set_status(format!("surface toggle requested: {surface_id}")),
                        bcode_command::CommandEffect::OpenSession { session_id, focus } => {
                            super::session_flow::start_switch_session(
                                chat,
                                session_id,
                                super::session_flow::initial_transcript_window_request(
                                    bmux_tui::geometry::Rect::new(0, 0, 80, 24),
                                ),
                            );
                            if focus == bcode_command::SessionOpenFocus::Composer {
                                chat.app
                                    .set_status("session opened; composer ready".to_owned());
                            }
                        }
                        bcode_command::CommandEffect::OpenPluginSurface {
                            surface_kind,
                            instance_id,
                            options,
                        } => {
                            chat.replace_effect(TuiEffect::OpenPluginSurface {
                                plugin_id: plugin_id.clone(),
                                surface_kind,
                                instance_id,
                                options,
                                working_directory: chat.app.working_directory().map_or_else(
                                    || settings.launch_working_directory().to_path_buf(),
                                    std::path::Path::to_path_buf,
                                ),
                                session_id: chat.attached_session_id(),
                            });
                            chat.app.set_status("opening plugin surface…".to_owned());
                        }
                    }
                }
            }
            Err(error) => report_nonfatal_tui_error(chat, "Plugin command outcome", &error),
        },
        TuiEffectResult::SubmitMessage {
            submission_id,
            message,
            result,
        } => {
            apply_submit_message_result(chat, &submission_id, &message, *result);
        }
        TuiEffectResult::SkillAction {
            action,
            skill_id,
            result,
        } => {
            apply_skill_action_result(chat, action, &skill_id, *result);
        }
        TuiEffectResult::SetSessionModel {
            session_id,
            provider_plugin_id,
            model_id,
            result,
        } => {
            apply_set_session_model_result(
                chat,
                session_id,
                provider_plugin_id.as_ref(),
                &model_id,
                result,
            );
        }
        TuiEffectResult::SetSessionReasoning {
            session_id,
            effort,
            effort_generation,
            status,
            result,
        } => {
            apply_set_session_reasoning_result(
                chat,
                session_id,
                effort,
                effort_generation,
                status,
                result,
            );
        }
        TuiEffectResult::AppendPresentationNote { result, .. } => {
            if let Err(error) = result {
                daemon_issue::report_client_issue(
                    &mut chat.app,
                    "presentation note persistence failed",
                    &error,
                );
            }
        }
        TuiEffectResult::CompactContext { session_id, result } => {
            apply_compact_context_result(chat, session_id, result);
        }
        TuiEffectResult::AttachWorktree { path, result } => {
            loop_state.apply_working_directory_dialog_result(&result);
            apply_attach_worktree_result(chat, &path, result);
        }
        TuiEffectResult::CreateWorktree { result } => {
            apply_create_worktree_result(chat, result);
        }
        TuiEffectResult::CancelRuntimeWork { work_id, result } => {
            apply_cancel_runtime_work_result(chat, &work_id, result);
        }
        TuiEffectResult::CancelTurn { session_id, result } => {
            apply_cancel_turn_result(chat, loop_state, session_id, result);
        }
        TuiEffectResult::PermissionResolved {
            permission_id,
            approved,
            remember,
            apply_to_batch,
            result,
        } => {
            apply_permission_resolution_result(
                chat,
                loop_state,
                &permission_id,
                approved,
                remember,
                apply_to_batch,
                result,
            );
        }
    }
}

pub fn apply_config_result(
    settings: &mut TuiRuntimeSettings,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    config: Result<bcode_config::BcodeConfig, String>,
    theme_selection: Result<Option<String>, String>,
    streaming_presentation: Result<
        Option<bcode_session_view_models::StreamingPresentationPolicy>,
        String,
    >,
) {
    match config {
        Ok(config) => {
            let previous_interactions = chat.app.tui_config().interactions;
            settings.apply_tui_config(&config.tui);
            if let Some(error) = settings.keymap().error() {
                chat.app
                    .set_status(format!("keymap configuration invalid: {error}"));
            }
            settings.set_completion_settings(config.presentation.completion);
            loop_state.set_slash_palette_settings(settings.completion_settings());
            loop_state.invalidate_slash_palette_discovery();
            loop_state.refresh_slash_palette(chat);
            settings.set_metrics_enabled(config.metrics.enabled);
            loop_state.telemetry.set_enabled(config.metrics.enabled);
            let default_plugin_ids =
                bcode_plugin::static_bundled_default_plugin_ids(settings.static_plugins())
                    .unwrap_or_default();
            let selection = bcode_config::plugin_selection_with_default_plugin_ids(
                &config,
                &default_plugin_ids,
            );
            match super::plugin_tui::load_default_presentation_with_static_bundled(
                &selection,
                config.tui.visual_adapters.clone(),
                settings.static_plugins(),
                settings.tui_extensions(),
            ) {
                Ok(presentation) => {
                    settings
                        .keymap
                        .integrate_contributors(presentation.keymap_actions());
                    if let Some(error) = settings.keymap.error() {
                        chat.app
                            .set_status(format!("plugin keymap unavailable: {error}"));
                    }
                    chat.app.set_plugin_presentation(Arc::new(presentation));
                }
                Err(error) => {
                    settings.keymap.integrate_contributors(Err(
                        bcode_plugin_sdk::tui::keymap::KeymapError(error.to_string()),
                    ));
                    chat.app
                        .set_status(format!("plugin presentation unavailable: {error}"));
                }
            }
            chat.app
                .set_effective_keymap(settings.keymap.registry().clone());
            if let Err(error) = chat.app.apply_plugin_presentation_config(&config) {
                chat.app.set_status(format!(
                    "plugin presentation configuration unavailable: {error}"
                ));
            }
            chat.app.apply_tui_config(config.tui.clone());
            if previous_interactions.placement != config.tui.interactions.placement {
                loop_state.pinned_interaction_viewport = None;
                loop_state.interactive_surface_geometry = None;
            }
            match theme_selection {
                Ok(Some(theme_id)) if !chat.app.apply_theme(&theme_id) => {
                    chat.app.set_status(format!(
                        "saved theme {theme_id} is unavailable; using configured default {}",
                        config.tui.theme.name
                    ));
                }
                Ok(_) => {}
                Err(error) => chat
                    .app
                    .set_status(format!("TUI state unavailable: {error}")),
            }
            if let Some(surface) = loop_state.interactive_surface.as_mut() {
                surface.update_keymap(&settings.keymap);
            }
            loop_state.declarative_streaming_policy = config.presentation.streaming.policy();
            match streaming_presentation {
                Ok(streaming_override) => {
                    loop_state.streaming_presentation_override = streaming_override;
                    let effective =
                        streaming_override.unwrap_or(loop_state.declarative_streaming_policy);
                    let _ = chat.app.apply_streaming_presentation_policy(effective);
                }
                Err(error) => {
                    loop_state.streaming_presentation_override = None;
                    let _ = chat.app.apply_streaming_presentation_policy(
                        loop_state.declarative_streaming_policy,
                    );
                    chat.app
                        .set_status(format!("TUI streaming state unavailable: {error}"));
                }
            }
            chat.replace_effect(TuiEffect::ReconcileAuthSecurity {
                config: Box::new(config),
            });
            if chat.viewing_session_id().is_none() {
                chat.replace_effect(TuiEffect::LoadDraftStatus {
                    launch_working_directory: settings.launch_working_directory().to_path_buf(),
                });
            }
        }
        Err(error) => chat.app.set_status(format!("Config unavailable: {error}")),
    }
}

fn apply_auth_security_result(chat: &mut ActiveChat, status: Option<String>) {
    if let Some(status) = status {
        chat.app.set_status(status);
    }
}

fn apply_draft_status_result(
    chat: &mut ActiveChat,
    model: Option<bcode_model::SessionModelStatus>,
    composer_draft: Option<String>,
    error: Option<String>,
) {
    if chat.viewing_session_id().is_some() {
        return;
    }
    if let Some(draft) = composer_draft
        && chat.app.composer().is_empty()
    {
        chat.app.replace_composer_with(&draft);
        chat.app.set_status("Draft restored".to_owned());
    }
    if let Some(error) = error {
        chat.app
            .set_status(format!("Draft status unavailable: {error}"));
    }
    if let Some(model) = model {
        chat.app.apply_model_status(model);
    }
}

fn apply_session_status_result(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    session_id: bcode_session_models::SessionId,
    hydration: super::effects::SessionStatusHydration,
) {
    let super::effects::SessionStatusHydration {
        model,
        active_skills,
        runtime_work,
        interactions,
        plugin_status,
        error,
    } = hydration;
    if chat.viewing_session_id() != Some(session_id) {
        return;
    }
    chat.app.set_plugin_status(plugin_status);
    let model_text = model.as_ref().map_or_else(
        || "model unknown".to_owned(),
        |status| {
            let provider = status.provider_plugin_id.as_deref().unwrap_or("auto");
            let model = status.model_id.as_deref().unwrap_or("default");
            format!("{provider}/{model}")
        },
    );
    if let Some(model) = model {
        chat.app.apply_model_status(model);
    }
    if let Some(skills) = active_skills {
        chat.app.set_active_skills(&skills);
    }
    if let Some(work) = runtime_work {
        chat.app.apply_runtime_work_snapshots(&work);
    }
    if let Some(interactions) = interactions {
        reconcile_interactive_surfaces(loop_state, &interactions);
        chat.app.set_pending_interactions(interactions);
    }
    let skill_count = chat.app.active_skill_count();
    if let Some(error) = error {
        chat.app
            .set_status(format!("Session status unavailable: {error}"));
        return;
    }
    chat.app
        .set_status(format!("model: {model_text}; active skills: {skill_count}"));
}

fn apply_agent_catalog_result(
    chat: &mut ActiveChat,
    agents: Result<session_flow::AgentCatalog, String>,
) {
    match agents {
        Ok(agents) => {
            chat.app.set_agent_metadata_hydrated(true);
            chat.agents = agents;
            chat.agents.refresh_app_agent_metadata(&mut chat.app);
        }
        Err(error) => {
            chat.app
                .set_status(format!("Agent metadata unavailable: {error}"));
        }
    }
}

fn hydrate_history_artifacts(
    chat: &ActiveChat,
    loop_state: &mut ChatLoopState,
    events: &[bcode_session_models::SessionEvent],
) {
    let presentation = chat.app.plugin_presentation();
    for event in events {
        loop_state.artifact_stream.observe_finalized_artifact(
            event.session_id,
            event.sequence,
            &event.kind,
            |producer, schema, version, key, content_type| {
                presentation.is_some_and(|presentation| {
                    presentation.accepts_artifact_reference(
                        producer,
                        schema,
                        version,
                        key,
                        content_type,
                    )
                })
            },
        );
    }
}

fn apply_older_history_result(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    session_id: bcode_session_models::SessionId,
    cursor: bcode_session_models::SessionHistoryCursor,
    generation: u64,
    result: Result<
        (
            bcode_session_models::SessionHistoryPage,
            Vec<bcode_session_models::SubmissionRecord>,
        ),
        ClientError,
    >,
) {
    if Some(session_id) != chat.viewing_session_id()
        || !chat.app.accepts_history_page(generation, cursor, true)
    {
        return;
    }
    match result {
        Ok((page, submissions)) if chat.app.accepts_history_page_payload(cursor, true, &page) => {
            hydrate_history_artifacts(chat, loop_state, &page.events);
            chat.app.prepend_older_history(&page.events, page.has_more);
            chat.app.apply_submission_records(&submissions);
        }
        Ok(_) => {
            chat.app.set_loading_older_history(false);
            chat.app.set_status(
                "Older history page inconsistent; retry or inspect session history".to_owned(),
            );
        }
        Err(error) => {
            chat.app.set_loading_older_history(false);
            report_nonfatal_client_error(chat, "Older history unavailable", &error);
        }
    }
}

fn apply_newer_history_result(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    session_id: bcode_session_models::SessionId,
    cursor: bcode_session_models::SessionHistoryCursor,
    generation: u64,
    result: Result<
        (
            bcode_session_models::SessionHistoryPage,
            Vec<bcode_session_models::SubmissionRecord>,
        ),
        ClientError,
    >,
) {
    if Some(session_id) != chat.viewing_session_id()
        || !chat.app.accepts_history_page(generation, cursor, false)
    {
        return;
    }
    match result {
        Ok((page, submissions)) if chat.app.accepts_history_page_payload(cursor, false, &page) => {
            hydrate_history_artifacts(chat, loop_state, &page.events);
            chat.app.append_newer_history(&page.events, page.has_more);
            chat.app.apply_submission_records(&submissions);
        }
        Ok(_) => {
            chat.app.set_loading_newer_history(false);
            chat.app.set_status(
                "Newer history page inconsistent; retry or inspect session history".to_owned(),
            );
        }
        Err(error) => {
            chat.app.set_loading_newer_history(false);
            report_nonfatal_client_error(chat, "Newer history unavailable", &error);
        }
    }
}

fn permission_summary_view(
    permission: bcode_ipc::PermissionSummary,
) -> bcode_session_view_models::PermissionView {
    let title = Some(format!("Permission requested: {}", permission.tool_name));
    let detail = permission.policy_reason.clone();
    bcode_session_view_models::PermissionView {
        permission_id: permission.permission_id,
        session_id: Some(permission.session_id),
        tool_call_id: permission.tool_call_id,
        tool_name: permission.tool_name,
        arguments_json: permission.arguments_json,
        batch: permission
            .batch
            .map(|batch| bcode_session_view_models::PermissionBatchView {
                batch_id: batch.batch_id,
                call_index: batch.call_index,
                call_count: batch.call_count,
            }),
        agent_id: permission.agent_id,
        title,
        policy_source: permission.policy_source,
        detail,
        resolved: false,
        approved: None,
        can_remember: permission.can_remember_policy,
    }
}

fn reconcile_permission_dialog(
    dialog: &mut Option<PermissionDialogState>,
    permissions: Vec<bcode_session_view_models::PermissionView>,
) {
    if dialog.as_ref().is_some_and(|dialog| {
        !permissions
            .iter()
            .any(|permission| permission.permission_id == dialog.permission_id())
    }) {
        *dialog = None;
    }
    if dialog.is_none()
        && let Some(permission) = permissions.into_iter().next()
    {
        *dialog = Some(PermissionDialogState::new(permission));
    }
}

fn apply_permission_list_result(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    result: Result<Vec<bcode_ipc::PermissionSummary>, ClientError>,
) {
    match result {
        Ok(permissions) => {
            let active_permissions = permissions
                .iter()
                .filter(|permission| {
                    chat.viewing_session_id()
                        .is_some_and(|session_id| permission.is_addressed_to(session_id))
                })
                .cloned()
                .map(permission_summary_view)
                .collect::<Vec<_>>();
            chat.app
                .set_pending_permission_views(active_permissions.clone());
            if chat.app.status() == "Refreshing pending permissions…" {
                chat.app.set_status(if active_permissions.is_empty() {
                    "No pending permissions for this session".to_owned()
                } else {
                    format!("{} pending permission(s)", active_permissions.len())
                });
            }
            reconcile_permission_dialog(&mut loop_state.permission_dialog, active_permissions);
        }
        Err(error) => report_nonfatal_client_error(chat, "Pending permissions unavailable", &error),
    }
}

fn apply_save_draft_result(
    chat: &mut ActiveChat,
    draft_autosave: &mut DraftAutosave,
    text: String,
    result: Result<(), ClientError>,
) {
    match result {
        Ok(()) => draft_autosave.mark_save_completed(text),
        Err(error) => report_nonfatal_client_error(chat, "Draft autosave unavailable", &error),
    }
}

fn slash_completion_context(chat: &ActiveChat) -> slash_palette::SlashCompletionContext {
    slash_palette::SlashCompletionContext {
        session_id: chat.app.session_id(),
        working_directory: chat
            .app
            .working_directory()
            .map(std::path::Path::to_path_buf),
        provider_plugin_id: chat.app.selected_provider_plugin_id().map(str::to_owned),
        model_id: chat.app.selected_model_id().map(str::to_owned),
    }
}

fn apply_slash_palette_result(
    chat: &ActiveChat,
    loop_state: &mut ChatLoopState,
    query: &str,
    palette: slash_palette::SlashPalette,
    generation: u64,
) {
    if loop_state
        .slash_completion
        .set_context(slash_completion_context(chat))
    {
        loop_state.slash_palette = None;
    }
    let fallback = palette.clone();
    if !loop_state
        .slash_completion
        .accept(generation, query, palette)
    {
        return;
    }
    let current_query = chat.app.composer().text();
    if !current_query.starts_with('/') || loop_state.slash_completion.is_dismissed(current_query) {
        return;
    }
    if let Some(mut palette) = loop_state
        .slash_completion
        .cached(current_query)
        .or_else(|| loop_state.slash_completion.snapshot(current_query))
        .or_else(|| {
            (slash_palette::discovery_scope(query) == slash_palette::discovery_scope(current_query))
                .then(|| {
                    let mut palette = fallback;
                    palette.refilter(current_query, loop_state.slash_completion.settings);
                    palette
                })
        })
    {
        if let Some(previous) = loop_state
            .slash_palette
            .as_ref()
            .and_then(slash_palette::SlashPalette::explicit_selection)
        {
            palette.select_command(previous);
        }
        loop_state.slash_palette = (!palette.is_empty()).then_some(palette);
    }
}

#[allow(clippy::too_many_lines)]
fn apply_root_slash_command_outcome(
    settings: &TuiRuntimeSettings,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    message: &str,
    outcome: super::slash_commands::SlashCommandOutcome,
) {
    use super::slash_commands::SlashCommandOutcome;
    match outcome {
        SlashCommandOutcome::Handled(status) => chat.app.set_status(status),
        SlashCommandOutcome::SystemMarkdown(text) => chat.append_durable_presentation_note(
            "bcode.host",
            text,
            bcode_command::CommandTextFormat::Markdown,
        ),
        SlashCommandOutcome::SystemPlain(text) => chat.append_durable_presentation_note(
            "bcode.host",
            text,
            bcode_command::CommandTextFormat::PlainText,
        ),
        SlashCommandOutcome::SetThinkingDisplay(show) => {
            chat.app.set_reasoning_visible(show);
            chat.app.set_status(if show {
                "reasoning display enabled".to_owned()
            } else {
                "reasoning display hidden".to_owned()
            });
        }
        SlashCommandOutcome::ToggleThinkingDisplay => {
            let show = !chat.app.reasoning_visible();
            chat.app.set_reasoning_visible(show);
            chat.app.set_status(if show {
                "reasoning output shown".to_owned()
            } else {
                "reasoning output hidden".to_owned()
            });
        }
        SlashCommandOutcome::SetThinkingMode(mode) => {
            chat.app.set_reasoning_display_mode(mode);
        }
        SlashCommandOutcome::NewDraftSession => session_flow::switch_to_draft_session(chat),
        SlashCommandOutcome::CancelTurn { session_id } => {
            chat.start_effect(TuiEffect::CancelTurn { session_id });
            chat.app.set_cancelling();
            chat.app.set_status("requesting cancellation…".to_owned());
        }
        SlashCommandOutcome::CancelRuntimeWork {
            session_id,
            work_id,
        } => {
            let work_id = bcode_session_models::WorkId::new(work_id);
            chat.start_effect(TuiEffect::CancelRuntimeWork {
                session_id,
                work_id,
            });
        }
        SlashCommandOutcome::CompactContext { session_id } => {
            chat.start_effect(TuiEffect::CompactContext { session_id });
        }
        SlashCommandOutcome::AttachWorktree { session_id, path } => {
            chat.start_effect(TuiEffect::AttachWorktree { session_id, path });
        }
        SlashCommandOutcome::PickAuthPool => {
            chat.start_effect(TuiEffect::LoadAuthPoolPicker);
            chat.app
                .set_status("loading auth subscriptions…".to_owned());
        }
        SlashCommandOutcome::SetLocalModel {
            provider_plugin_id,
            model_id,
        } => {
            chat.app
                .apply_local_model_selection(provider_plugin_id, &model_id);
        }
        SlashCommandOutcome::SetSessionModel {
            session_id,
            provider_plugin_id,
            model_id,
        } => chat.start_effect(TuiEffect::SetSessionModel {
            session_id,
            provider_plugin_id,
            model_id,
        }),
        SlashCommandOutcome::SetSessionReasoning {
            session_id,
            effort,
            summary,
            status,
        } => chat.start_effect(TuiEffect::SetSessionReasoning {
            session_id,
            effort,
            summary,
            effort_generation: chat.app.pending_reasoning_effort_generation(),
            status,
        }),
        SlashCommandOutcome::PluginCommand {
            action,
            execution: _,
            session,
            arguments,
        } => match action {
            bcode_command::CommandAction::Plugin {
                plugin_id,
                command_id,
            } => {
                let session_id = chat.attached_session_id();
                if let Some(refusal) = session.refusal(session_id.is_some()) {
                    chat.app.set_status(refusal.message().to_owned());
                    return;
                }
                if let Some(result) = chat.app.plugin_presentation().and_then(|presentation| {
                    presentation.invoke_frontend_command(&plugin_id, &command_id)
                }) {
                    chat.app.set_status(match result {
                        Ok(()) => "local frontend action accepted".to_owned(),
                        Err(message) => message,
                    });
                    return;
                }
                let working_directory = chat.app.working_directory().map_or_else(
                    || settings.launch_working_directory().to_path_buf(),
                    std::path::Path::to_path_buf,
                );
                chat.start_effect(TuiEffect::InvokePluginCommand {
                    plugin_id,
                    command_id,
                    arguments: Some(arguments),
                    working_directory,
                    session_id,
                });
            }
            bcode_command::CommandAction::Host { route } => chat
                .app
                .set_status(format!("host slash route pending root navigation: {route}")),
        },
        SlashCommandOutcome::ReviewPermissions => {
            chat.replace_effect(TuiEffect::ListPermissions);
            chat.app
                .set_status("Refreshing pending permissions…".to_owned());
        }
        SlashCommandOutcome::OpenTimeline => {
            loop_state.milestone_requests.cancel();
            let entries = if chat.viewing_session_id().is_some() {
                chat.app.timeline_entries()
            } else {
                Vec::new()
            };
            loop_state.timeline_dialog =
                Some(super::timeline_dialog::TimelineDialogState::new(entries));
            chat.app
                .set_status("timeline: select a user message".to_owned());
        }
        SlashCommandOutcome::DraftAgentSelected {
            agent_id,
            agent_name,
            agent_accent,
        } => {
            if chat.app.session_id().is_some() {
                chat.app.set_pending_agent(agent_id, agent_accent);
                chat.app
                    .set_status(agent_selection_status(chat, &agent_name));
            } else {
                chat.app.set_current_agent(agent_id, agent_accent);
                chat.app.set_status(format!("agent set to {agent_name}"));
            }
        }
        SlashCommandOutcome::OpenThinkingSettings(focus) => {
            chat.replace_effect(TuiEffect::LoadThinkingDialog {
                session_id: chat.attached_session_id(),
                focus,
            });
            chat.app
                .set_status("loading reasoning output settings…".to_owned());
        }
        SlashCommandOutcome::BuildRalphPrompt(kind) => {
            if let Err(error) = super::ralph_flow::show_prompt(chat, kind) {
                report_nonfatal_tui_error(chat, "Ralph prompt unavailable", &error);
            }
        }
        SlashCommandOutcome::OpenRalphProgress => {
            if let Err(error) = super::ralph_flow::open_progress(chat) {
                report_nonfatal_tui_error(chat, "Ralph progress unavailable", &error);
            }
        }
        SlashCommandOutcome::InvokeSkill {
            skill_id,
            arguments,
        } => {
            chat.start_effect(TuiEffect::SkillAction {
                request: Box::new(super::effects::SkillActionRequest {
                    session_id: chat.attached_session_id(),
                    launch_working_directory: chat.app.working_directory().map_or_else(
                        || settings.launch_working_directory().to_path_buf(),
                        std::path::Path::to_path_buf,
                    ),
                    skill_id,
                    action: super::effects::SkillActionKind::Invoke,
                    arguments,
                    provider_plugin_id: chat
                        .app
                        .selected_provider_plugin_id()
                        .map(ToOwned::to_owned),
                    model_id: chat.app.selected_model_id().map(ToOwned::to_owned),
                    agent_id: Some(chat.app.display_agent_id().to_owned()),
                    reasoning_effort: chat.app.reasoning_effort().map(ToOwned::to_owned),
                    reasoning_summary: chat.app.reasoning_summary().map(ToOwned::to_owned),
                    reasoning_effort_generation: chat.app.pending_reasoning_effort_generation(),
                    execution: settings.launch_options().turn_execution_options(),
                    event_sender: chat.event_sender.clone(),
                }),
            });
        }
        SlashCommandOutcome::OpenWorkingDirectoryDialog => {
            loop_state.open_working_directory_dialog(chat);
        }
        SlashCommandOutcome::OpenWorktreeCreateDialog => {
            loop_state.open_worktree_create_dialog(chat);
        }
        SlashCommandOutcome::PreviewTheme { theme_id } => {
            if chat.app.preview_theme(&theme_id) {
                chat.app.set_status(format!(
                    "previewing theme {theme_id}; use /theme apply {theme_id} or /theme cancel"
                ));
            } else {
                chat.app
                    .set_status(format!("unknown bundled theme: {theme_id}"));
            }
        }
        SlashCommandOutcome::ApplyTheme { theme_id } => {
            if chat.app.preview_theme(&theme_id) {
                chat.replace_effect(TuiEffect::PersistThemeSelection {
                    name: theme_id.clone(),
                });
                chat.app
                    .set_status(format!("saving theme {theme_id} to user state…"));
            } else {
                chat.app
                    .set_status(format!("unknown bundled theme: {theme_id}"));
            }
        }
        SlashCommandOutcome::ResetTheme => {
            chat.replace_effect(TuiEffect::ClearThemeSelection);
            chat.app
                .set_status("clearing saved theme override…".to_owned());
        }
        SlashCommandOutcome::CancelThemePreview => {
            chat.app.cancel_theme_preview();
            chat.app.set_status("theme preview cancelled".to_owned());
        }
        SlashCommandOutcome::OpenThemePicker => loop_state.open_theme_picker(chat),
        SlashCommandOutcome::OpenStreamingConfigurator => {
            loop_state.open_streaming_configurator(chat);
        }
        SlashCommandOutcome::ShowThemeCatalog => {
            let markdown = super::slash_commands::format_theme_catalog_markdown(
                &super::theme::catalog_view(&mut chat.app),
            );
            chat.append_durable_presentation_note(
                "bcode.host",
                markdown,
                bcode_command::CommandTextFormat::Markdown,
            );
            chat.app.set_status("theme catalog shown".to_owned());
        }
        SlashCommandOutcome::PickModel => {
            chat.replace_effect(TuiEffect::LoadModelProviders);
            chat.app.set_status("loading model providers…".to_owned());
        }
        SlashCommandOutcome::PickSkill => {
            chat.replace_effect(TuiEffect::LoadSkillPicker);
            chat.app.set_status("loading skills…".to_owned());
        }
        SlashCommandOutcome::ShowRalphStatus => {
            start_root_ralph_action(
                settings,
                chat,
                super::ralph_flow::RalphRootAction::ShowStatus,
            );
        }
        SlashCommandOutcome::RunRalphLoop => {
            start_root_ralph_action(settings, chat, super::ralph_flow::RalphRootAction::Run);
        }
        SlashCommandOutcome::ApproveRalphRun => {
            start_root_ralph_action(settings, chat, super::ralph_flow::RalphRootAction::Approve);
        }
        SlashCommandOutcome::StopRalphLoop => {
            start_root_ralph_action(settings, chat, super::ralph_flow::RalphRootAction::Stop);
        }
        SlashCommandOutcome::ListRalphRuns => {
            start_root_ralph_action(settings, chat, super::ralph_flow::RalphRootAction::ListRuns);
        }
        SlashCommandOutcome::ListRalphIterations => {
            start_root_ralph_action(
                settings,
                chat,
                super::ralph_flow::RalphRootAction::ListIterations,
            );
        }
        SlashCommandOutcome::ResumeRalphRun => {
            start_root_ralph_action(settings, chat, super::ralph_flow::RalphRootAction::Resume);
        }
        SlashCommandOutcome::OpenRalphStartDialog => {
            loop_state.open_ralph_start_dialog(settings.launch_working_directory(), chat);
        }
        SlashCommandOutcome::PickSession => {
            loop_state.open_session_picker(chat);
        }
        SlashCommandOutcome::OpenChildMonitor => {
            chat.app.open_child_panel();
            if let Some(session_id) = chat.attached_session_id() {
                chat.replace_effect(TuiEffect::LoadChildActivity {
                    session_id,
                    after: None,
                    generation: 0,
                });
                chat.replace_effect(TuiEffect::LoadSessionChildren {
                    parent_id: session_id,
                    after: None,
                    generation: 0,
                });
            }
        }
        SlashCommandOutcome::SearchSessions => {
            loop_state.open_session_search(chat);
        }
        SlashCommandOutcome::OpenRalphHome => {
            chat.replace_effect(TuiEffect::OpenPluginSurface {
                plugin_id: "bcode.ralph".to_owned(),
                surface_kind: "ralph-home".to_owned(),
                instance_id: "ralph-home".to_owned(),
                options: serde_json::Value::Null,
                working_directory: chat.app.working_directory().map_or_else(
                    || settings.launch_working_directory().to_path_buf(),
                    std::path::Path::to_path_buf,
                ),
                session_id: chat.attached_session_id(),
            });
            chat.app.set_status("opening Ralph UI…".to_owned());
        }
        SlashCommandOutcome::Unknown(_) => {
            chat.app.restore_pending_submission(message);
            chat.app.set_status("unknown slash command".to_owned());
        }
    }
}

fn start_root_ralph_action(
    settings: &TuiRuntimeSettings,
    chat: &mut ActiveChat,
    action: super::ralph_flow::RalphRootAction,
) {
    let repo_root = chat.app.working_directory().map_or_else(
        || settings.launch_working_directory().to_path_buf(),
        std::path::Path::to_path_buf,
    );
    chat.replace_effect(TuiEffect::RalphAction { repo_root, action });
    chat.app.set_status("running Ralph action…".to_owned());
}

fn apply_submit_message_result(
    chat: &mut ActiveChat,
    submission_id: &bcode_session_models::SubmissionId,
    _message: &str,
    result: Result<super::effects::SubmitMessageResult, ClientError>,
) {
    match result {
        Ok(result) => {
            chat.mark_attached(result.session_id);
            chat.app
                .set_daemon_connection(super::app::DaemonConnectionState::Connected);
            if let Some(session) = result.created_session {
                chat.app.apply_session_summary(&session);
            }
            if let Some(event_task) = result.event_task
                && let Some(previous_task) = chat.event_task.replace(event_task)
            {
                previous_task.abort();
            }
            if let Some(agent_id) = result.committed_agent_id.as_deref() {
                chat.app.commit_submitted_agent(agent_id);
            }
            if let Some(generation) = result.committed_reasoning_effort_generation {
                chat.app.clear_pending_reasoning_effort(generation);
            }
            if let Some(release) = result.event_stream_release {
                let _released = release.send(());
            }
            match result.submission {
                Ok(Some(record)) => {
                    chat.app
                        .apply_submission_records(std::slice::from_ref(&record));
                    chat.app.set_status(format!(
                        "Submission {} accepted",
                        result.receipt.submission_id.0
                    ));
                }
                Ok(None) => chat
                    .app
                    .set_status("Submission accepted; lifecycle unavailable".to_owned()),
                Err(error) => daemon_issue::report_client_issue(
                    &mut chat.app,
                    "Submission accepted; lifecycle lookup failed",
                    &error,
                ),
            }
            ensure_session_stream_after_foreground_wake(chat);
        }
        Err(error) => {
            // An error can follow durable acceptance: retain identity and do not stage a duplicate.
            daemon_issue::report_client_issue(
                &mut chat.app,
                &format!(
                    "submission {} unresolved; retained pending",
                    submission_id.0
                ),
                &error,
            );
        }
    }
}

fn ensure_session_stream_after_foreground_wake(chat: &mut ActiveChat) {
    let Some(session_id) = chat.attached_session_id() else {
        return;
    };
    if chat
        .event_task
        .as_ref()
        .is_some_and(|event_task| !event_task.is_finished())
    {
        return;
    }
    session_flow::start_switch_session(
        chat,
        session_id,
        session_flow::initial_transcript_window_request(bmux_tui::geometry::Rect::new(
            0, 0, 80, 24,
        )),
    );
}

fn apply_skill_action_result(
    chat: &mut ActiveChat,
    action: super::effects::SkillActionKind,
    skill_id: &bcode_skill_models::SkillId,
    result: Result<super::effects::SkillActionResult, ClientError>,
) {
    match result {
        Ok(result) => {
            chat.mark_attached(result.session_id);
            if let Some(session) = result.created_session {
                chat.app.apply_session_summary(&session);
            }
            if let Some(event_task) = result.event_task
                && let Some(previous_task) = chat.event_task.replace(event_task)
            {
                previous_task.abort();
            }
            if let Some(agent_id) = result.committed_agent_id.as_deref() {
                chat.app.commit_submitted_agent(agent_id);
            }
            if let Some(generation) = result.committed_reasoning_effort_generation {
                chat.app.clear_pending_reasoning_effort(generation);
            }
            if let Some(release) = result.event_stream_release {
                let _released = release.send(());
            }
            match action {
                super::effects::SkillActionKind::Activate => {
                    chat.app.set_status(format!("activated skill {skill_id}"));
                }
                super::effects::SkillActionKind::Deactivate => {
                    chat.app.set_status(format!("deactivated skill {skill_id}"));
                }
                super::effects::SkillActionKind::Invoke => {
                    let queued = result
                        .acceptance
                        .is_some_and(|acceptance| acceptance.queued);
                    chat.app.set_status(if queued {
                        format!("skill {skill_id} queued")
                    } else {
                        format!("skill {skill_id} invoked")
                    });
                }
            }
        }
        Err(error) => {
            let label = match action {
                super::effects::SkillActionKind::Activate => "skill activation failed",
                super::effects::SkillActionKind::Deactivate => "skill deactivation failed",
                super::effects::SkillActionKind::Invoke => "skill invocation failed",
            };
            daemon_issue::report_client_issue(&mut chat.app, label, &error);
        }
    }
}

fn apply_set_session_model_result(
    chat: &mut ActiveChat,
    session_id: bcode_session_models::SessionId,
    provider_plugin_id: Option<&String>,
    model_id: &str,
    result: Result<(), ClientError>,
) {
    if chat.viewing_session_id() != Some(session_id) {
        return;
    }
    match result {
        Ok(()) => chat.app.set_status(provider_plugin_id.map_or_else(
            || format!("model set to {model_id}"),
            |provider| format!("model set to {provider}/{model_id}"),
        )),
        Err(error) => {
            daemon_issue::report_client_issue(&mut chat.app, "model selection failed", &error);
        }
    }
}

fn apply_set_session_reasoning_result(
    chat: &mut ActiveChat,
    session_id: bcode_session_models::SessionId,
    effort: Option<String>,
    effort_generation: Option<u64>,
    status: String,
    result: Result<(), ClientError>,
) {
    if chat.viewing_session_id() != Some(session_id) {
        return;
    }
    match result {
        Ok(()) => {
            if let Some(generation) = effort_generation {
                chat.app.clear_pending_reasoning_effort(generation);
            } else if let Some(effort) = effort {
                chat.app.reconcile_pending_reasoning_effort(&effort);
            }
            chat.app.set_status(status);
        }
        Err(error) => {
            daemon_issue::report_client_issue(&mut chat.app, "reasoning setting failed", &error);
        }
    }
}

fn apply_compact_context_result(
    chat: &mut ActiveChat,
    session_id: bcode_session_models::SessionId,
    result: Result<String, ClientError>,
) {
    if chat.viewing_session_id() != Some(session_id) {
        return;
    }
    match result {
        Ok(message) => chat.app.set_status(message),
        Err(error) => {
            daemon_issue::report_client_issue(&mut chat.app, "compact unavailable", &error);
        }
    }
}

fn apply_attach_worktree_result(
    chat: &mut ActiveChat,
    path: &std::path::Path,
    result: Result<bcode_session_models::SessionSummary, ClientError>,
) {
    match result {
        Ok(session) => {
            chat.app.apply_session_summary(&session);
            chat.app.set_status(format!(
                "working directory: {}",
                display_from_current_dir(path)
            ));
        }
        Err(error) => {
            daemon_issue::report_client_issue(
                &mut chat.app,
                "working directory change failed",
                &error,
            );
        }
    }
}

fn apply_create_worktree_result(
    chat: &mut ActiveChat,
    result: Result<bcode_worktree_models::WorktreeCreateOperationStatus, ClientError>,
) {
    let status = match result {
        Ok(status) => status,
        Err(error) => {
            daemon_issue::report_client_issue(&mut chat.app, "worktree create failed", &error);
            return;
        }
    };
    if !status.is_terminal() {
        let label = match status.state {
            bcode_worktree_models::WorktreeCreateOperationState::Queued => {
                "worktree creation queued"
            }
            bcode_worktree_models::WorktreeCreateOperationState::Creating => "creating worktree…",
            bcode_worktree_models::WorktreeCreateOperationState::ApplyingSetup => {
                "applying worktree setup…"
            }
            bcode_worktree_models::WorktreeCreateOperationState::FinalizingSession => {
                "finalizing worktree session…"
            }
            bcode_worktree_models::WorktreeCreateOperationState::Succeeded
            | bcode_worktree_models::WorktreeCreateOperationState::Failed => unreachable!(),
        };
        chat.app.set_status(label.to_owned());
        chat.replace_effect(TuiEffect::WaitWorktreeCreate {
            operation_id: status.operation_id,
            after_revision: status.revision,
        });
        return;
    }
    if let Some(error) = status.error {
        let suffix = error.created_path.map_or_else(String::new, |path| {
            format!(
                "; worktree was created at {} before finalization failed",
                display_from_current_dir(&path)
            )
        });
        chat.app
            .set_status(format!("worktree create failed: {}{suffix}", error.message));
        return;
    }
    let Some(response) = status.response else {
        chat.app
            .set_status("worktree operation ended without a result".to_owned());
        return;
    };
    let path = response.path.clone();
    if let Some(session) = response.session {
        let session_id = session.id;
        chat.app.apply_session_summary(&session);
        chat.mark_attached(session_id);
    }
    chat.push_presentation_markdown(
        "bcode.host",
        format!(
            "Created worktree\n* Path: {}",
            display_from_current_dir(&path)
        ),
    );
    chat.app.set_status("created worktree".to_owned());
}

fn apply_cancel_runtime_work_result(
    chat: &mut ActiveChat,
    work_id: &bcode_session_models::WorkId,
    result: Result<bool, ClientError>,
) {
    match result {
        Ok(true) => chat
            .app
            .set_status(format!("runtime work cancellation requested: {work_id}")),
        Ok(false) => chat
            .app
            .set_status(format!("runtime work not active: {work_id}")),
        Err(error) => {
            daemon_issue::report_client_issue(&mut chat.app, "runtime cancellation failed", &error);
        }
    }
}

fn apply_permission_resolution_result(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    permission_id: &str,
    approved: bool,
    remember: bool,
    apply_to_batch: bool,
    result: Result<bool, ClientError>,
) {
    match result {
        Ok(resolved) => {
            loop_state.permission_dialog = None;
            chat.replace_effect(TuiEffect::ListPermissions);
            chat.app.set_status(if !resolved {
                format!("permission {permission_id} was already resolved")
            } else if apply_to_batch {
                format!(
                    "{} permission batch",
                    if approved { "approved" } else { "denied" }
                )
            } else if approved {
                if remember {
                    format!("approved and remembered permission {permission_id}")
                } else {
                    format!("approved permission {permission_id}")
                }
            } else if remember {
                format!("denied and remembered permission {permission_id}")
            } else {
                format!("denied permission {permission_id}")
            });
        }
        Err(error) => {
            report_nonfatal_client_error(chat, "Permission resolution unavailable", &error);
        }
    }
}

fn refresh_permissions_after_cancellation(chat: &mut ActiveChat) {
    ChatLoopState::queue_effect_cancellation(chat, TuiEffect::ListPermissions);
    chat.replace_effect(TuiEffect::ListPermissions);
}

fn close_permission_dialog_for_session(
    permission_dialog: &mut Option<PermissionDialogState>,
    session_id: bcode_session_models::SessionId,
) {
    if permission_dialog
        .as_ref()
        .is_some_and(|dialog| dialog.permission().session_id == Some(session_id))
    {
        *permission_dialog = None;
    }
}

fn apply_cancel_turn_result(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    session_id: bcode_session_models::SessionId,
    result: Result<bool, ClientError>,
) {
    match result {
        Ok(true) if Some(session_id) == chat.app.session_id() => {
            close_permission_dialog_for_session(&mut loop_state.permission_dialog, session_id);
            refresh_permissions_after_cancellation(chat);
            chat.app.set_cancelling();
            chat.app
                .set_status("turn cancellation requested".to_owned());
        }
        Ok(false) if Some(session_id) == chat.app.session_id() => {
            close_permission_dialog_for_session(&mut loop_state.permission_dialog, session_id);
            refresh_permissions_after_cancellation(chat);
            chat.app.set_idle();
            chat.app.set_status("no active turn".to_owned());
        }
        Ok(_) => {}
        Err(error) => {
            if Some(session_id) == chat.app.session_id() {
                chat.app.set_idle();
            }
            report_nonfatal_client_error(chat, "Cancel unavailable", &error);
        }
    }
}

pub fn start_cancel_turn(chat: &mut ActiveChat, _loop_state: &mut ChatLoopState) {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return;
    };
    let effect = TuiEffect::CancelTurn { session_id };
    if chat.pending_effects.contains_effect(&effect) {
        chat.app
            .set_status("turn cancellation already requested".to_owned());
        return;
    }
    chat.start_effect(effect);
    chat.app.set_cancelling();
    chat.app
        .set_status("turn cancellation requested".to_owned());
}

pub fn start_draft_save(chat: &mut ActiveChat, draft_autosave: &mut DraftAutosave) {
    let Some((scope, text)) = draft_autosave.pending_save(chat) else {
        return;
    };
    draft_autosave.mark_save_started();
    chat.queue_latest_effect(TuiEffect::SaveDraft { scope, text });
}

fn update_slash_palette_async(chat: &mut ActiveChat, loop_state: &mut ChatLoopState) -> bool {
    let context = slash_completion_context(chat);
    if loop_state.slash_completion.set_context(context) {
        loop_state.slash_palette = None;
    }
    let query = chat.app.composer().text().to_owned();
    if !query.starts_with('/') {
        loop_state.slash_completion.dismiss(&query);
        loop_state.slash_palette = None;
        ChatLoopState::queue_effect_cancellation(
            chat,
            TuiEffect::LoadSlashPalette {
                provider_plugin_id: None,
                query: String::new(),
                session_id: None,
                generation: 0,
            },
        );
        return true;
    }
    if loop_state.slash_completion.is_dismissed(&query) {
        return false;
    }
    if let Some(mut palette) = loop_state.slash_completion.cached(&query) {
        if let Some(previous) = loop_state
            .slash_palette
            .as_ref()
            .and_then(slash_palette::SlashPalette::explicit_selection)
        {
            palette.select_command(previous);
        }
        loop_state.slash_palette = (!palette.is_empty()).then_some(palette);
        return true;
    }
    // Expiry triggers discovery without discarding the last scoped snapshot.
    if let Some(mut palette) = loop_state.slash_completion.snapshot(&query) {
        if let Some(previous) = loop_state
            .slash_palette
            .as_ref()
            .and_then(slash_palette::SlashPalette::explicit_selection)
        {
            palette.select_command(previous);
        }
        loop_state.slash_palette = (!palette.is_empty()).then_some(palette);
    }
    // Retain a same-scope popup while discovery is pending; never hide/fetch/show on typing.
    if let Some(palette) = loop_state.slash_palette.as_mut() {
        if slash_palette::discovery_scope(palette.query()) == slash_palette::discovery_scope(&query)
        {
            palette.refilter(&query, loop_state.slash_completion.settings);
        } else {
            loop_state.slash_palette = None;
        }
    }
    if loop_state.slash_palette.is_none() {
        let palette =
            slash_palette::SlashPalette::immediate(&query, loop_state.slash_completion.settings);
        loop_state.slash_palette = (!palette.is_empty()).then_some(palette);
    }
    if let Some(generation) = loop_state.slash_completion.request(&query) {
        chat.replace_effect(TuiEffect::LoadSlashPalette {
            provider_plugin_id: chat.app.selected_provider_plugin_id().map(str::to_owned),
            query,
            session_id: chat.app.session_id(),
            generation,
        });
    }
    true
}

fn report_nonfatal_tui_error(chat: &mut ActiveChat, label: &str, error: &TuiError) {
    daemon_issue::report_tui_issue(&mut chat.app, label, error);
}

fn report_nonfatal_client_error(chat: &mut ActiveChat, label: &str, error: &ClientError) {
    chat.app
        .set_status(daemon_issue::client_issue_status(label, error));
}

fn record_artifact_stream_stats(loop_state: &mut ChatLoopState) {
    let stats = loop_state.artifact_stream.drain_stats();
    loop_state
        .telemetry
        .add_counter("tui.artifact.target_observed_total", stats.observed_targets);
    loop_state.telemetry.add_counter(
        "tui.artifact.target_coalesced_total",
        stats.coalesced_targets,
    );
    loop_state
        .telemetry
        .add_counter("tui.artifact.fetch_started_total", stats.fetches_started);
    loop_state
        .telemetry
        .add_counter("tui.artifact.completion_total", stats.completions);
    loop_state.telemetry.add_counter(
        "tui.artifact.stale_completion_total",
        stats.stale_completions,
    );
    loop_state
        .telemetry
        .add_counter("tui.artifact.delivered_total", stats.delivered_chunks);
    loop_state
        .telemetry
        .add_counter("tui.artifact.delivered_bytes", stats.delivered_bytes);
    loop_state
        .telemetry
        .add_counter("tui.artifact.retry_total", stats.retries);
    loop_state.telemetry.add_counter(
        "tui.artifact.terminal_failure_total",
        stats.terminal_failures,
    );
    loop_state.telemetry.set_gauge(
        "tui.artifact.backlog",
        i64::try_from(stats.backlog).unwrap_or(i64::MAX),
    );
}

fn markdown_destination_cache_source(
    destination: &bcode_markdown_render::MarkdownDestination,
) -> String {
    match destination {
        bcode_markdown_render::MarkdownDestination::Web(url) => url.as_str().to_owned(),
        bcode_markdown_render::MarkdownDestination::LocalPath(path) => {
            path.to_string_lossy().into_owned()
        }
        bcode_markdown_render::MarkdownDestination::Fragment(fragment) => format!("#{fragment}"),
        bcode_markdown_render::MarkdownDestination::UnresolvedRelative(source) => source.clone(),
        bcode_markdown_render::MarkdownDestination::Inert { reason } => format!("inert:{reason:?}"),
    }
}

#[derive(Debug)]
struct MarkdownFramePresentation {
    rich: Vec<render::MarkdownRichRegion>,
    image_removed: Vec<String>,
    mermaid_removed: Vec<String>,
}

const fn markdown_image_destination_rect(placeholder: Rect) -> Rect {
    placeholder
}

const fn markdown_mermaid_destination_rect(placeholder: Rect) -> Rect {
    placeholder
}

fn write_markdown_fallback(
    frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
    area: Rect,
    fallback: &str,
) {
    if area.is_empty() {
        return;
    }
    let width = usize::from(area.width);
    let lines = fallback.lines().collect::<Vec<_>>();
    for row in 0..area.height {
        let text = lines.get(usize::from(row)).copied().unwrap_or_default();
        let text = bmux_tui::text_width::truncate_to_display_width(text, width);
        frame.write_line(
            bmux_tui::paint::LocalRect::terminal(Rect::new(
                area.x,
                area.y.saturating_add(row),
                area.width,
                1,
            )),
            &bmux_tui::prelude::Line::raw(text),
        );
    }
}

fn image_region_fallback(
    runtime: &super::markdown_image::MarkdownPresentationRuntime,
    contribution_id: &str,
    contribution: &bcode_markdown_render::MarkdownContributionKind,
) -> Option<String> {
    let bcode_markdown_render::MarkdownContributionKind::Image {
        alt,
        source,
        linked_destination,
        ..
    } = contribution
    else {
        return None;
    };
    runtime
        .images
        .fallback(contribution_id, alt, source, linked_destination.as_ref())
}

fn reconcile_markdown_presentation(
    chat: &ActiveChat,
    loop_state: &mut ChatLoopState,
    area: Rect,
) -> MarkdownFramePresentation {
    let rich = render::transcript_markdown_rich_regions(&chat.app, area);
    let terminal_supported = loop_state.markdown_image_capabilities.any_supported();
    let image_inputs = rich
        .iter()
        .filter_map(|region| match &region.contribution_kind {
            bcode_markdown_render::MarkdownContributionKind::Image { source, .. } => {
                Some(super::markdown_image::MarkdownImagePresentationInput {
                    contribution_id: region.contribution_id.clone(),
                    cache_key: super::markdown_image::MarkdownImageCacheKey::new(
                        &markdown_destination_cache_source(source),
                        "decoded-rgba8",
                    ),
                    destination: source.clone(),
                    residency: if region.visible_rect.is_some() {
                        super::markdown_image::MarkdownImageResidency::Visible
                    } else {
                        super::markdown_image::MarkdownImageResidency::Hidden
                    },
                })
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mermaid_inputs = if chat.app.tui_config().markdown.mermaid {
        rich.iter()
            .filter_map(|region| match &region.contribution_kind {
                bcode_markdown_render::MarkdownContributionKind::Mermaid {
                    source,
                    cache_key,
                    ..
                } => Some(super::markdown_mermaid::MarkdownMermaidInput {
                    contribution_id: region.contribution_id.clone(),
                    cache_key: cache_key.clone(),
                    source: source.clone(),
                    visible: region.visible_rect.is_some(),
                    prefetch: false,
                }),
                _ => None,
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let image_removed =
        loop_state
            .markdown_presentation
            .as_mut()
            .map_or_else(Vec::new, |runtime| {
                let (removed, tasks) = runtime.reconcile_and_start(
                    &image_inputs,
                    super::markdown_image::MarkdownImagePresentationPolicy {
                        interactive_resident_frame: true,
                        network_enabled: chat.app.tui_config().markdown.network_images,
                        terminal_supported,
                    },
                );
                loop_state.markdown_image_tasks.extend(tasks);
                removed
            });
    let mermaid_removed = loop_state
        .markdown_mermaid
        .as_mut()
        .map_or_else(Vec::new, |runtime| {
            let (removed, tasks) = runtime.reconcile_and_start(&mermaid_inputs, terminal_supported);
            loop_state.markdown_mermaid_tasks.extend(tasks);
            removed
        });
    MarkdownFramePresentation {
        rich,
        image_removed,
        mermaid_removed,
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn draw_chat_frame<W: Write>(
    terminal: &mut Terminal<&mut W>,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    schedule_delay: Duration,
    frame_interval: Option<Duration>,
    damage: bmux_tui::damage::Damage,
    fast_temporal_presentation: bool,
    committed_layout: Option<render::FrameLayout>,
    transcript_selection: &bmux_tui::selection::SelectionController,
) -> Result<(bmux_tui::terminal::DrawStats, Option<render::FrameLayout>), TuiError> {
    // Modal scenes replace all interaction metadata, not just damaged cells.
    // The temporal path still reuses committed geometry and avoids transcript preparation.
    let damage = if loop_state.modal_foreground() || terminal.retained_buffer().is_none() {
        bmux_tui::damage::Damage::Full
    } else {
        damage
    };
    let frame_started = Instant::now();
    let prepare_started = frame_started;
    if fast_temporal_presentation
        && terminal.retained_buffer().is_some()
        && (!loop_state.modal_foreground()
            || loop_state
                .modal_backdrop
                .as_ref()
                .is_some_and(|buffer| buffer.area() == terminal.area()))
        && let Some(layout) = committed_layout
    {
        return draw_temporal_frame(
            terminal,
            chat,
            loop_state,
            damage,
            frame_interval,
            frame_started,
            layout,
        );
    }
    let full_transcript_area = render::transcript_area_for_frame(&chat.app, terminal.area());
    let interaction_rows = loop_state
        .interactive_surface
        .as_mut()
        .map(|surface| surface.preferred_height(full_transcript_area.width).max(1));
    let pinned = chat.app.tui_config().interactions.placement
        == bcode_config::TuiInteractionPlacement::Pinned;
    let pinned_offset = if pinned {
        let previous = loop_state.pinned_interaction_viewport.clone();
        let result = loop_state.interactive_surface.as_mut().map(|surface| {
            let interaction_id = surface.interaction_id().to_owned();
            let viewport_height = interactive_surface_height(surface, full_transcript_area);
            let logical_height = interaction_rows.unwrap_or(1);
            let offset = previous
                .filter(|(id, _)| id == &interaction_id)
                .map_or(0, |(_, offset)| offset)
                .min(logical_height.saturating_sub(viewport_height));
            let focused = surface.focused_row_range(full_transcript_area.width);
            let offset = focused.map_or(offset, |focused| {
                if focused.start < offset {
                    focused.start
                } else if focused.end > offset.saturating_add(viewport_height) {
                    focused.end.saturating_sub(viewport_height)
                } else {
                    offset
                }
            });
            let offset = offset.min(logical_height.saturating_sub(viewport_height));
            (interaction_id, offset)
        });
        if let Some((interaction_id, offset)) = result {
            loop_state.pinned_interaction_viewport = Some((interaction_id, offset));
            offset
        } else {
            loop_state.pinned_interaction_viewport = None;
            0
        }
    } else {
        loop_state.pinned_interaction_viewport = None;
        0
    };
    let dock_height = if pinned {
        loop_state
            .interactive_surface
            .as_mut()
            .map_or(0, |surface| {
                interactive_surface_height(surface, full_transcript_area)
            })
    } else {
        0
    };
    let active_inline_rows = (!pinned)
        .then(|| {
            Some((
                loop_state.interactive_surface.as_ref()?.interaction_id(),
                interaction_rows?,
            ))
        })
        .flatten();
    chat.app.set_active_interaction_layout(
        active_inline_rows.map(|(interaction_id, rows)| (interaction_id.to_owned(), rows)),
    );
    let prepared =
        render::prepare_frame_with_bottom_dock(&mut chat.app, terminal.area(), dock_height);
    let layout = prepared.map(|(layout, _dock)| layout);
    let rich_presentation = layout.map_or_else(
        || MarkdownFramePresentation {
            rich: Vec::new(),
            image_removed: Vec::new(),
            mermaid_removed: Vec::new(),
        },
        |layout| reconcile_markdown_presentation(chat, loop_state, layout.body),
    );
    let inline_placement = (!pinned)
        .then(|| {
            inline_interaction_surface_placement(
                &chat.app,
                layout.map(|layout| layout.body),
                loop_state
                    .interactive_surface
                    .as_ref()
                    .map(InteractiveSurfaceState::interaction_id),
            )
        })
        .flatten();
    let surface_area = if pinned {
        prepared.map_or_else(
            || {
                Rect::new(
                    full_transcript_area.x,
                    full_transcript_area.bottom(),
                    full_transcript_area.width,
                    0,
                )
            },
            |(_layout, dock)| dock,
        )
    } else {
        inline_placement.map_or_else(
            || {
                Rect::new(
                    full_transcript_area.x,
                    full_transcript_area.bottom(),
                    full_transcript_area.width,
                    0,
                )
            },
            |placement| placement.destination,
        )
    };
    for stats in chat.app.transcript_layout_mut().drain_sync_stats() {
        let mut labels = bcode_metrics::MetricLabels::new();
        labels.insert(
            "invalidation".to_owned(),
            stats.invalidation.label().to_owned(),
        );
        loop_state.telemetry.add_counter_with_labels(
            "tui.transcript.sync_total",
            1,
            labels.clone(),
        );
        loop_state.telemetry.add_counter_with_labels(
            "tui.transcript.entries_scanned",
            u64::try_from(stats.entries_scanned).unwrap_or(u64::MAX),
            labels.clone(),
        );
        loop_state.telemetry.add_counter_with_labels(
            "tui.transcript.signatures_changed",
            u64::try_from(stats.signatures_changed).unwrap_or(u64::MAX),
            labels.clone(),
        );
        loop_state.telemetry.add_counter_with_labels(
            "tui.transcript.entries_rebuilt",
            u64::try_from(stats.entries_rebuilt).unwrap_or(u64::MAX),
            labels.clone(),
        );
        loop_state.telemetry.add_counter_with_labels(
            "tui.transcript.rows_regenerated",
            u64::try_from(stats.rows_regenerated).unwrap_or(u64::MAX),
            labels.clone(),
        );
        loop_state.telemetry.record_histogram_with_labels(
            "tui.transcript.sync_us",
            stats.duration_micros,
            labels,
        );
    }
    if let Some(presentation) = chat.app.plugin_presentation() {
        for diagnostic in presentation.drain_diagnostics() {
            let mut labels = bcode_metrics::MetricLabels::new();
            labels.insert("plugin_id".to_owned(), diagnostic.plugin_id);
            labels.insert("diagnostic".to_owned(), diagnostic.name);
            loop_state.telemetry.add_counter_with_labels(
                "tui.plugin_visual.work",
                diagnostic.value,
                labels,
            );
        }
        for timing in presentation.drain_timings() {
            let mut labels = bcode_metrics::MetricLabels::new();
            labels.insert("operation".to_owned(), timing.operation.to_owned());
            labels.insert("plugin_id".to_owned(), timing.plugin_id);
            labels.insert("schema".to_owned(), timing.schema);
            loop_state.telemetry.record_histogram_with_labels(
                "tui.plugin_visual.duration_us",
                timing.duration_micros,
                labels,
            );
        }
    }
    let prepare_ms = elapsed_millis(prepare_started);
    let theme = render::TuiTheme::for_app(&chat.app);
    let draw_started = Instant::now();
    let regions = damage.retained_regions().to_vec();
    let full_damage = damage.is_full();
    let intersects = |area: bmux_tui::geometry::Rect| {
        full_damage
            || regions
                .iter()
                .any(|region| !area.intersection(*region).is_empty())
    };
    let geometry = loop_state.interactive_surface.as_ref().map(|_| {
        if pinned {
            InteractiveSurfaceGeometry {
                placement: InteractiveSurfacePlacement::Pinned,
                logical_height: interaction_rows.unwrap_or(surface_area.height),
                visible_logical_offset: pinned_offset,
                destination: surface_area,
            }
        } else if let Some(placement) = inline_placement {
            InteractiveSurfaceGeometry {
                placement: InteractiveSurfacePlacement::Transcript,
                logical_height: placement.full_area.height,
                visible_logical_offset: placement.visible_content_offset,
                destination: placement.destination,
            }
        } else {
            InteractiveSurfaceGeometry {
                placement: InteractiveSurfacePlacement::Transcript,
                logical_height: interaction_rows.unwrap_or(0),
                visible_logical_offset: 0,
                destination: surface_area,
            }
        }
    });
    let foreground_owns_canvas = loop_state.foreground_owns_canvas();
    let mut compositor = std::mem::take(&mut loop_state.markdown_image_compositor);
    let capabilities = loop_state.markdown_image_capabilities.clone();
    let config = loop_state.markdown_image_config.clone();
    let terminal_area = terminal.area();
    let draw_result = terminal.draw_damage_with_overlay(
        damage,
        |frame| {
            let modal = loop_state.modal_foreground();
            let area = Rect::new(0, 0, frame.area().width, frame.area().height);
            let mut paint_background = |frame: &mut bmux_tui::paint::PaintCx<'_, '_>| {
                if let Some(layout) = layout.filter(|_| !foreground_owns_canvas) {
                    render::render_prepared_damage(&mut chat.app, frame, layout, intersects);
                    let selection_scene =
                        super::root_program::transcript_selection_scene(&chat.app, layout.body());
                    for scope in selection_scene.scopes() {
                        frame.push_selection_scope(scope.clone());
                    }
                    for fragment in selection_scene.fragments() {
                        frame.push_selection_fragment(fragment.clone());
                    }
                }
                for contribution_id in &rich_presentation.image_removed {
                    super::markdown_image::MarkdownImagePresentationStore::remove_from_frame(
                        contribution_id,
                        frame,
                    );
                }
                for contribution_id in &rich_presentation.mermaid_removed {
                    super::markdown_mermaid::MarkdownMermaidPresentationStore::remove_from_frame(
                        contribution_id,
                        frame,
                    );
                }
                if !foreground_owns_canvas && layout.is_some_and(|layout| intersects(layout.body)) {
                    for region in &rich_presentation.rich {
                        let Some(visible_rect) = region.visible_rect else {
                            continue;
                        };
                        match &region.contribution_kind {
                            bcode_markdown_render::MarkdownContributionKind::Image { .. } => {
                                if let Some(runtime) = &loop_state.markdown_presentation {
                                    let destination = markdown_image_destination_rect(visible_rect);
                                    if !runtime.images.present_ready(
                                        &region.contribution_id,
                                        destination,
                                        layout.map_or(visible_rect, |layout| layout.body),
                                        frame,
                                    ) && let Some(fallback) = image_region_fallback(
                                        runtime,
                                        &region.contribution_id,
                                        &region.contribution_kind,
                                    ) {
                                        write_markdown_fallback(frame, destination, &fallback);
                                    }
                                }
                            }
                            bcode_markdown_render::MarkdownContributionKind::Mermaid { .. } => {
                                if let Some(runtime) = &loop_state.markdown_mermaid {
                                    if let Some(placement) = runtime.presentations.ready_placement(
                                        &region.contribution_id,
                                        markdown_mermaid_destination_rect(visible_rect),
                                        layout.map_or(visible_rect, |layout| layout.body),
                                    ) {
                                        frame.push_image(placement);
                                    } else if let Some(fallback) =
                                        runtime.presentations.fallback(&region.contribution_id)
                                    {
                                        write_markdown_fallback(frame, visible_rect, &fallback);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if !foreground_owns_canvas {
                    paint_interactive_surface(loop_state, geometry, frame);
                }
                if !modal && let Some(snapshot) = transcript_selection.snapshot(frame.selection()) {
                    frame.paint_selection(&snapshot, theme.selection);
                }
            };
            if modal {
                let backdrop = super::foreground::paint_backdrop(area, frame, paint_background);
                loop_state.modal_backdrop = Some(backdrop);
            } else {
                paint_background(frame);
                loop_state.modal_backdrop = None;
            }
            paint_foreground(chat, loop_state, geometry, frame);
        },
        |writer, scene, delta| {
            compositor.apply_delta(delta);
            compositor
                .render(
                    writer,
                    scene,
                    bmux_image::compositor::PaneRect {
                        x: terminal_area.x,
                        y: terminal_area.y,
                        w: terminal_area.width,
                        h: terminal_area.height,
                    },
                    &capabilities,
                    &config,
                )
                .map_err(std::io::Error::other)
        },
    );
    loop_state.markdown_image_compositor = compositor;
    let draw_stats = draw_result?;
    loop_state.interactive_surface_geometry = geometry;
    loop_state.telemetry.record_histogram(
        "tui.frame.changed_cells",
        u64::try_from(draw_stats.changed_cells).unwrap_or(u64::MAX),
    );
    if draw_stats.full_repaint {
        loop_state
            .telemetry
            .add_counter("tui.frame.full_repaint_total", 1);
    }
    let draw_ms = elapsed_millis(draw_started);
    let total_ms = elapsed_millis(frame_started);
    loop_state.telemetry.add_counter("tui.frame.total", 1);
    let frame_budget_ms = frame_interval.map_or(u64::MAX, |interval| {
        u64::try_from(interval.as_millis()).unwrap_or(u64::MAX)
    });
    if total_ms >= frame_budget_ms {
        loop_state
            .telemetry
            .add_counter("tui.frame.over_budget_total", 1);
    }
    let frame_index = loop_state.frame_index;
    loop_state.frame_index = loop_state.frame_index.wrapping_add(1);
    if frame_index.is_multiple_of(16) || total_ms >= frame_budget_ms {
        loop_state
            .telemetry
            .record_histogram("tui.frame.prepare_ms", prepare_ms);
        loop_state
            .telemetry
            .record_histogram("tui.frame.draw_ms", draw_ms);
        loop_state
            .telemetry
            .record_histogram("tui.frame.total_ms", total_ms);
        loop_state.telemetry.record_histogram(
            "tui.frame.schedule_delay_ms",
            u64::try_from(schedule_delay.as_millis()).unwrap_or(u64::MAX),
        );
    }
    Ok((draw_stats, layout))
}

fn paint_launch_dialogs(
    loop_state: &mut ChatLoopState,
    foreground: Option<super::foreground::Foreground>,
    frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
    theme: render::TuiTheme,
    effective: &BmuxKeyMap,
) {
    if let Some(dialog) = &mut loop_state.ralph_start_dialog
        && foreground == Some(super::foreground::Foreground::Ralph)
    {
        super::ralph_start_dialog_render::render_dialog_with_keymap(
            dialog, frame, theme, effective,
        );
    }
    if let Some(dialog) = &mut loop_state.working_directory_dialog
        && foreground == Some(super::foreground::Foreground::WorkingDirectory)
    {
        super::working_directory_dialog_render::render_dialog_with_keymap(
            dialog, frame, theme, effective,
        );
    }
    if let Some(dialog) = &mut loop_state.worktree_create_dialog
        && foreground == Some(super::foreground::Foreground::Worktree)
    {
        super::wt_create_dialog_render::render_dialog_with_keymap(dialog, frame, theme, effective);
    }
}

fn paint_plugin_foreground(
    chat: &ActiveChat,
    loop_state: &mut ChatLoopState,
    foreground: Option<super::foreground::Foreground>,
    frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
) {
    if let Some(surface) = &mut loop_state.plugin_surface
        && foreground == Some(super::foreground::Foreground::Plugin)
    {
        let area = Rect::new(0, 0, frame.area().width, frame.area().height);
        let plugin_theme = render::plugin_theme_for_app(&chat.app);
        if let Some(active) = surface.surface.as_mut() {
            if let Some(launch) = loop_state.render_launch.clone() {
                let host = super::plugin_surface_host::root_host(
                    surface.invalidation.clone(),
                    loop_state.foreground_client.clone(),
                    launch,
                    std::sync::Arc::clone(&surface.active_tasks),
                );
                active.render_with_host(area, frame, Some(&plugin_theme), &host);
            } else {
                active.render_with_theme(area, frame, Some(&plugin_theme));
            }
        } else {
            let message = if surface.close_after_drain {
                "Finishing operation safely before closing…"
            } else {
                "Finishing plugin operation… Esc / Ctrl-C requests exit after settlement"
            };
            frame.write_line_with_fallback_style(
                bmux_tui::paint::LocalRect::new(0, 0, frame.area().width, 1),
                &bmux_tui::prelude::Line::from(message),
                bmux_tui::prelude::Style::new(),
            );
        }
    }
}

fn paint_owner_completion_overlay(
    chat: &ActiveChat,
    state: &mut ChatLoopState,
    foreground: Option<super::foreground::Foreground>,
    frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
    theme: render::TuiTheme,
) {
    if foreground.is_none() || foreground == Some(super::foreground::Foreground::Plugin) {
        super::composer_interaction_render::paint(
            &mut state.composer_interaction,
            chat.app.composer_panel_area(),
            frame,
            theme,
        );
    } else {
        state.composer_interaction.painted_list = None;
        state.composer_interaction.painted_layout = None;
    }
}

/// Compose every foreground surface in the same order for full and temporal frames.
/// Preparation may be skipped; composition must never be skipped.
fn paint_foreground(
    chat: &ActiveChat,
    loop_state: &mut ChatLoopState,
    _geometry: Option<InteractiveSurfaceGeometry>,
    frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
) {
    let foreground = loop_state.foreground();
    let effective = super::keymap::BmuxKeyMap::from_registry(chat.app.effective_keymap().clone());
    if loop_state.modal_foreground() {
        frame.set_cursor(bmux_tui::geometry::Point::new(0, 0), false);
        frame.set_focus_scope(Some(bmux_tui::hit::HitId::new("bcode.foreground")));
    }
    let theme = render::TuiTheme::for_app(&chat.app);
    paint_owner_completion_overlay(chat, loop_state, foreground, frame, theme);
    if let Some(slash_palette) = &loop_state.slash_palette
        && foreground == Some(super::foreground::Foreground::Slash)
    {
        slash_palette_render::render_palette(
            slash_palette,
            chat.app.composer_content_area(),
            frame,
            theme,
        );
    }
    if let Some(palette) = &mut loop_state.palette
        && foreground == Some(super::foreground::Foreground::Commands)
    {
        command_palette_render::render_palette(palette, frame, theme);
    }
    paint_foreground_configurator(chat, loop_state, frame, &effective);
    if let Some(picker) = &mut loop_state.theme_picker
        && foreground == Some(super::foreground::Foreground::Theme)
    {
        super::theme_picker_render::render_theme_picker_with_keymap(
            picker, frame, theme, &effective,
        );
    }
    if let Some(dialog) = &loop_state.permission_dialog
        && foreground == Some(super::foreground::Foreground::Permission)
    {
        permission_dialog_render::render_permission_dialog(dialog, frame, theme);
    }
    if let Some(dialog) = &loop_state.thinking_dialog
        && foreground == Some(super::foreground::Foreground::Thinking)
    {
        thinking_dialog_render::render_thinking_dialog_with_keymap(
            dialog, frame, theme, &effective,
        );
    }
    if let Some(dialog) = &mut loop_state.timeline_dialog
        && foreground == Some(super::foreground::Foreground::Timeline)
    {
        timeline_dialog_render::render_timeline_dialog_with_keymap(
            dialog, frame, theme, &effective,
        );
    }
    if let Some(picker) = &mut loop_state.session_picker
        && foreground == Some(super::foreground::Foreground::Sessions)
    {
        super::session_picker_render::render_picker_with_keymap(picker, frame, theme, &effective);
    }
    paint_plugin_foreground(chat, loop_state, foreground, frame);
    if foreground == Some(super::foreground::Foreground::Plugin) {
        super::composer_interaction_render::paint(
            &mut loop_state.composer_interaction,
            chat.app.composer_panel_area(),
            frame,
            theme,
        );
    }
    if let Some(picker) = &mut loop_state.provider_picker
        && foreground == Some(super::foreground::Foreground::Model)
    {
        super::provider_picker_render::render_provider_picker_with_keymap(
            picker, frame, theme, &effective,
        );
    }
    if let Some(model) = &mut loop_state.model_picker
        && foreground == Some(super::foreground::Foreground::Model)
    {
        super::model_picker_render::render_model_picker_with_keymap(
            &mut model.picker,
            frame,
            theme,
            &effective,
        );
    }
    if let Some(picker) = &mut loop_state.auth_pool_picker
        && foreground == Some(super::foreground::Foreground::AuthPool)
    {
        super::auth_pool_picker_render::render_auth_pool_picker_with_keymap(
            picker, frame, theme, &effective,
        );
    }
    if let Some(picker) = &mut loop_state.skill_picker
        && foreground == Some(super::foreground::Foreground::Skill)
    {
        super::skill_picker_render::render_skill_picker_with_keymap(
            picker, frame, theme, &effective,
        );
    }
    paint_launch_dialogs(loop_state, foreground, frame, theme, &effective);
}

fn paint_foreground_configurator(
    chat: &ActiveChat,
    loop_state: &mut ChatLoopState,
    frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
    keymap: &BmuxKeyMap,
) {
    let foreground = loop_state.foreground();
    let theme = render::TuiTheme::for_app(&chat.app);
    if let Some(configurator) = &mut loop_state.streaming_configurator
        && foreground == Some(super::foreground::Foreground::Streaming)
    {
        let geometry = super::streaming_configurator_render::streaming_configurator_geometry(
            configurator,
            Rect::new(0, 0, frame.area().width, frame.area().height),
            theme,
        );
        configurator.commit_geometry(geometry);
        super::streaming_configurator_render::render_streaming_configurator_with_keymap(
            configurator,
            frame,
            theme,
            keymap,
        );
    }
}

fn paint_interactive_surface(
    loop_state: &mut ChatLoopState,
    geometry: Option<InteractiveSurfaceGeometry>,
    frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
) {
    if let Some(surface) = &mut loop_state.interactive_surface
        && let Some(geometry) = geometry
        && !geometry.destination.is_empty()
    {
        if geometry.placement == InteractiveSurfacePlacement::Pinned {
            frame.fill(
                bmux_tui::paint::LocalRect::terminal(geometry.destination),
                " ",
                bmux_tui::prelude::Style::new(),
            );
        }
        surface.render_slice(
            geometry.logical_height,
            geometry.visible_logical_offset,
            geometry.destination,
            frame,
        );
    }
}

fn draw_temporal_frame<W: Write>(
    terminal: &mut Terminal<&mut W>,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    damage: bmux_tui::damage::Damage,
    frame_interval: Option<Duration>,
    frame_started: Instant,
    layout: render::FrameLayout,
) -> Result<(bmux_tui::terminal::DrawStats, Option<render::FrameLayout>), TuiError> {
    let full_damage = damage.is_full();
    let regions = damage.retained_regions().to_vec();
    let intersects = |area: Rect| {
        full_damage
            || regions
                .iter()
                .any(|region| !area.intersection(*region).is_empty())
    };
    let draw_started = Instant::now();
    let mut compositor = std::mem::take(&mut loop_state.markdown_image_compositor);
    let capabilities = loop_state.markdown_image_capabilities.clone();
    let config = loop_state.markdown_image_config.clone();
    let terminal_area = terminal.area();
    let draw_result = terminal.draw_damage_with_overlay(
        damage,
        |frame| {
            if loop_state.modal_foreground() {
                if let Some(backdrop) = &loop_state.modal_backdrop {
                    super::foreground::paint_backdrop_buffer(backdrop, frame);
                }
            } else {
                render::render_prepared_damage(&mut chat.app, frame, layout, intersects);
                paint_interactive_surface(
                    loop_state,
                    loop_state.interactive_surface_geometry,
                    frame,
                );
            }
            paint_foreground(
                chat,
                loop_state,
                loop_state.interactive_surface_geometry,
                frame,
            );
        },
        |writer, scene, delta| {
            compositor.apply_delta(delta);
            compositor
                .render(
                    writer,
                    scene,
                    bmux_image::compositor::PaneRect {
                        x: terminal_area.x,
                        y: terminal_area.y,
                        w: terminal_area.width,
                        h: terminal_area.height,
                    },
                    &capabilities,
                    &config,
                )
                .map_err(std::io::Error::other)
        },
    );
    loop_state.markdown_image_compositor = compositor;
    let draw_stats = draw_result?;
    record_frame_telemetry(
        loop_state,
        &draw_stats,
        Duration::ZERO,
        frame_interval,
        frame_started,
        draw_started,
        0,
    );
    Ok((draw_stats, Some(layout)))
}

fn record_frame_telemetry(
    loop_state: &mut ChatLoopState,
    draw_stats: &bmux_tui::terminal::DrawStats,
    schedule_delay: Duration,
    frame_interval: Option<Duration>,
    frame_started: Instant,
    draw_started: Instant,
    prepare_ms: u64,
) {
    loop_state.telemetry.record_histogram(
        "tui.frame.changed_cells",
        u64::try_from(draw_stats.changed_cells).unwrap_or(u64::MAX),
    );
    if draw_stats.full_repaint {
        loop_state
            .telemetry
            .add_counter("tui.frame.full_repaint_total", 1);
    }
    let draw_ms = elapsed_millis(draw_started);
    let total_ms = elapsed_millis(frame_started);
    loop_state.telemetry.add_counter("tui.frame.total", 1);
    let frame_budget_ms = frame_interval.map_or(u64::MAX, |interval| {
        u64::try_from(interval.as_millis()).unwrap_or(u64::MAX)
    });
    if total_ms >= frame_budget_ms {
        loop_state
            .telemetry
            .add_counter("tui.frame.over_budget_total", 1);
    }
    let frame_index = loop_state.frame_index;
    loop_state.frame_index = loop_state.frame_index.wrapping_add(1);
    if frame_index.is_multiple_of(16) || total_ms >= frame_budget_ms {
        loop_state
            .telemetry
            .record_histogram("tui.frame.prepare_ms", prepare_ms);
        loop_state
            .telemetry
            .record_histogram("tui.frame.draw_ms", draw_ms);
        loop_state
            .telemetry
            .record_histogram("tui.frame.total_ms", total_ms);
        loop_state.telemetry.record_histogram(
            "tui.frame.schedule_delay_ms",
            u64::try_from(schedule_delay.as_millis()).unwrap_or(u64::MAX),
        );
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone, Copy)]
struct InlineInteractionPlacement {
    destination: Rect,
    full_area: Rect,
    visible_content_offset: u16,
}

fn inline_interaction_surface_placement(
    app: &super::app::BmuxApp,
    body: Option<Rect>,
    interaction_id: Option<&str>,
) -> Option<InlineInteractionPlacement> {
    let body = body?;
    let index = app.interaction_transcript_index(interaction_id?)?;
    let range = app.transcript_item_row_range(index)?;
    let top = app.transcript_top_row(body.height);
    let visible_start = range.start.max(top);
    let visible_end = range.end.min(top.saturating_add(usize::from(body.height)));
    if visible_start >= visible_end {
        return None;
    }
    Some(InlineInteractionPlacement {
        destination: Rect::new(
            body.x,
            body.y.saturating_add(
                u16::try_from(visible_start.saturating_sub(top)).unwrap_or(u16::MAX),
            ),
            body.width,
            u16::try_from(visible_end.saturating_sub(visible_start)).unwrap_or(u16::MAX),
        ),
        full_area: Rect::new(
            0,
            0,
            body.width,
            u16::try_from(range.end.saturating_sub(range.start)).unwrap_or(u16::MAX),
        ),
        visible_content_offset: u16::try_from(visible_start.saturating_sub(range.start))
            .unwrap_or(u16::MAX),
    })
}

fn interactive_surface_height(surface: &mut InteractiveSurfaceState, viewport: Rect) -> u16 {
    let preferred = surface.preferred_height(viewport.width);
    let maximum = viewport.height.saturating_mul(2).div_ceil(3);
    preferred
        .min(maximum)
        .min(viewport.height.saturating_sub(1))
}

#[derive(Debug, Default)]
struct RequestDraftHandoff {
    deferred: VecDeque<history_flow::SessionStreamUpdate>,
    awaiting_paint: BTreeSet<String>,
}

impl RequestDraftHandoff {
    fn observe_applied(&mut self, tool_call_id: Option<String>, changed: bool) {
        if changed && let Some(tool_call_id) = tool_call_id {
            self.awaiting_paint.insert(tool_call_id);
        }
    }

    fn blocks_session_stream(&self) -> bool {
        !self.awaiting_paint.is_empty()
    }

    fn mark_painted(&mut self) {
        self.awaiting_paint.clear();
    }

    fn clear(&mut self) {
        self.deferred.clear();
        self.awaiting_paint.clear();
    }
}

fn request_handoff_paint_id(update: &history_flow::SessionStreamUpdate) -> Option<&str> {
    let history_flow::SessionStreamUpdate::Event(event) = update else {
        return None;
    };
    match event.as_ref() {
        BcodeEvent::SessionLive(event) => match &event.kind {
            bcode_session_models::SessionLiveEventKind::ToolRequestDraft { event }
                if !matches!(
                    event.operation,
                    bcode_session_models::ToolRequestDraftOperation::Remove { .. }
                ) =>
            {
                Some(event.tool_call_id.as_str())
            }
            _ => None,
        },
        BcodeEvent::Session(event) => match &event.kind {
            bcode_session_models::SessionEventKind::ToolCallRequested { tool_call_id, .. }
            | bcode_session_models::SessionEventKind::PositionedToolCallRequested {
                tool_call_id,
                ..
            } => Some(tool_call_id.as_str()),
            _ => None,
        },
        _ => None,
    }
}

fn handle_artifact_completion(
    chat: &ActiveChat,
    loop_state: &mut ChatLoopState,
    completion: ActiveArtifactFetchCompletion,
) -> bool {
    let presentation = chat.app.plugin_presentation();
    let changed = loop_state.artifact_stream.handle_completion(
        chat.viewing_session_id(),
        completion,
        |chunk| {
            presentation.map_or_else(
                || Err("plugin presentation unavailable".to_owned()),
                |presentation| presentation.deliver_artifact_chunk(chunk),
            )
        },
    );
    let failures = loop_state.artifact_stream.failed_invocations();
    if let Some(presentation) = presentation.as_ref() {
        for invocation_id in loop_state.artifact_stream.recovered_invocations() {
            presentation.clear_artifact_failure(&invocation_id);
        }
        for invocation_id in &failures {
            presentation.mark_artifact_unavailable(invocation_id);
        }
    }
    changed || !failures.is_empty()
}

fn absorb_session_stream_update(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    update: history_flow::SessionStreamUpdate,
) -> bool {
    match update {
        history_flow::SessionStreamUpdate::QueueUnavailable { session_id } => {
            if chat.viewing_session_id() != Some(session_id) {
                return false;
            }
            chat.app.set_submission_queue_unavailable();
            true
        }
        history_flow::SessionStreamUpdate::QueueStatus { session_id, status } => {
            if chat.viewing_session_id() != Some(session_id) {
                return false;
            }
            chat.app.set_submission_queue(*status);
            true
        }
        history_flow::SessionStreamUpdate::Submission { session_id, record } => {
            if chat.viewing_session_id() != Some(session_id) {
                return false;
            }
            chat.app
                .apply_submission_records(std::slice::from_ref(&record));
            true
        }
        history_flow::SessionStreamUpdate::Event(event) => {
            absorb_bcode_event(chat, loop_state, *event)
        }
        history_flow::SessionStreamUpdate::ResyncStarted { session_id }
            if chat.viewing_session_id() == Some(session_id) =>
        {
            loop_state
                .telemetry
                .add_counter("tui.session_view.resync_started_total", 1);
            // The stream connection is dropped for replacement while identity stays valid, so the
            // view is stale but session-scoped commands remain dispatchable.
            chat.mark_stream_detached(session_id);
            chat.app.set_status("Reconnecting session view…".to_owned());
            true
        }
        history_flow::SessionStreamUpdate::Resynchronized {
            session_id,
            attached,
        } if chat.viewing_session_id() == Some(session_id) => {
            chat.mark_attached(session_id);
            apply_session_stream_resynchronization(chat, loop_state, &attached);
            true
        }
        history_flow::SessionStreamUpdate::ResyncStarted { .. }
        | history_flow::SessionStreamUpdate::Resynchronized { .. } => false,
    }
}

fn apply_session_stream_resynchronization(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    attached: &bcode_client::AttachedSessionHistory,
) {
    let has_older = attached
        .projection_window
        .as_ref()
        .is_some_and(|window| window.has_older);
    chat.app
        .replace_latest_transcript_window(&attached.history, has_older);
    chat.app.apply_submission_records(&attached.submissions);
    chat.app.apply_session_summary(&attached.session);
    chat.app.apply_usage_summary(&attached.usage_summary);
    chat.agents
        .apply_runtime_selection(&mut chat.app, attached.runtime_selection.clone());

    loop_state
        .artifact_stream
        .reset_session(attached.session.id);
    let presentation = chat.app.plugin_presentation();
    for event in &attached.history {
        loop_state.artifact_stream.observe_finalized_artifact(
            event.session_id,
            event.sequence,
            &event.kind,
            |producer_plugin_id, schema, schema_version, reference_key, content_type| {
                presentation.is_some_and(|presentation| {
                    presentation.accepts_artifact_reference(
                        producer_plugin_id,
                        schema,
                        schema_version,
                        reference_key,
                        content_type,
                    )
                })
            },
        );
    }
    loop_state.interactive_surface = None;
    loop_state.interactive_surface_queue.clear();
    chat.replace_effect(TuiEffect::LoadSessionStatus {
        session_id: attached.session.id,
    });
    chat.replace_effect(TuiEffect::ListPermissions);
    loop_state
        .telemetry
        .add_counter("tui.session_view.resync_completed_total", 1);
    chat.app
        .set_status("Session view resynchronized".to_owned());
}

pub fn absorb_session_live_event(
    app: &mut super::app::BmuxApp,
    artifact_stream: &mut ArtifactStreamCoordinator,
    event: &bcode_session_models::SessionLiveEvent,
) {
    app.absorb_session_live_event(event);
    let invocation_id = match &event.kind {
        bcode_session_models::SessionLiveEventKind::ToolContributionPlaced { envelope } => {
            Some(envelope.contribution.invocation_id.as_str())
        }
        bcode_session_models::SessionLiveEventKind::ToolPresentationUpdated { update } => {
            Some(update.invocation_id.as_str())
        }
        _ => None,
    };
    if invocation_id.is_some_and(|invocation_id| app.tool_invocation_is_terminal(invocation_id)) {
        return;
    }
    let presentation = app.plugin_presentation();
    artifact_stream.observe_session_live_artifact(
        event.session_id,
        &event.kind,
        |producer_plugin_id, schema, schema_version, reference_key, content_type| {
            presentation.is_some_and(|presentation| {
                presentation.accepts_artifact_reference(
                    producer_plugin_id,
                    schema,
                    schema_version,
                    reference_key,
                    content_type,
                )
            })
        },
    );
}

fn absorb_bcode_event(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    event: BcodeEvent,
) -> bool {
    match event {
        BcodeEvent::Session(event) if Some(event.session_id) == chat.viewing_session_id() => {
            let presentation = chat.app.plugin_presentation();
            loop_state.artifact_stream.observe_finalized_artifact(
                event.session_id,
                event.sequence,
                &event.kind,
                |producer_plugin_id, schema, schema_version, reference_key, content_type| {
                    presentation.is_some_and(|presentation| {
                        presentation.accepts_artifact_reference(
                            producer_plugin_id,
                            schema,
                            schema_version,
                            reference_key,
                            content_type,
                        )
                    })
                },
            );
            if let SessionEventKind::AgentChanged { agent_id } = &event.kind {
                chat.agents
                    .apply_agent_to_app(&mut chat.app, agent_id.clone());
            } else {
                if matches!(event.kind, SessionEventKind::PermissionRequested { .. }) {
                    chat.replace_effect(TuiEffect::ListPermissions);
                }
                if matches!(event.kind, SessionEventKind::ModelChanged { .. }) {
                    ChatLoopState::queue_effect_cancellation(
                        chat,
                        TuiEffect::LoadSessionStatus {
                            session_id: event.session_id,
                        },
                    );
                    chat.replace_effect(TuiEffect::LoadSessionModelStatus {
                        session_id: event.session_id,
                    });
                }
                if matches!(
                    event.kind,
                    SessionEventKind::RalphLifecycle { .. }
                        | SessionEventKind::PluginStatusNote { .. }
                        | SessionEventKind::InertHistory { .. }
                ) {
                    chat.replace_effect(TuiEffect::LoadPluginStatus {
                        session_id: event.session_id,
                    });
                }
                if let SessionEventKind::PermissionResolved { permission_id, .. } = &event.kind
                    && loop_state
                        .permission_dialog
                        .as_ref()
                        .is_some_and(|dialog| dialog.permission_id() == permission_id)
                {
                    loop_state.permission_dialog = None;
                    chat.replace_effect(TuiEffect::ListPermissions);
                }
                chat.app.absorb_session_event(&event);
                observe_interactive_surface_event(loop_state, &event.kind);
            }
            true
        }
        BcodeEvent::SessionLive(event) if Some(event.session_id) == chat.viewing_session_id() => {
            absorb_session_live_event(&mut chat.app, &mut loop_state.artifact_stream, &event);
            true
        }
        BcodeEvent::RuntimeWork(event) if Some(event.session_id) == chat.viewing_session_id() => {
            chat.app.absorb_session_event(&event);
            true
        }
        BcodeEvent::Session(_)
        | BcodeEvent::SessionLive(_)
        | BcodeEvent::RuntimeWork(_)
        | BcodeEvent::Workflow(_)
        | BcodeEvent::SessionViewResyncRequired { .. }
        | BcodeEvent::SessionCatalogUpdated { .. } => false,
    }
}

fn interaction_surface_request(
    interaction: &bcode_session_view_models::InteractionViewSummary,
) -> Option<InteractiveSurfaceRequest> {
    if interaction.resolved {
        return None;
    }
    let surface_kind = interaction_adapter_for_summary(interaction)?.tui_surface_kind?;
    Some(InteractiveSurfaceRequest::new(
        interaction.interaction_id.clone(),
        surface_kind,
        interaction
            .snapshot
            .clone()
            .unwrap_or(serde_json::Value::Null)
            .to_string(),
    ))
}

#[cfg(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-workflow-plugin"
))]
fn interaction_adapter_for_summary(
    interaction: &bcode_session_view_models::InteractionViewSummary,
) -> Option<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability> {
    bcode_bundled_plugins::interaction_adapter(
        interaction.producer_id.as_deref()?,
        interaction.exchange_schema.as_deref()?,
        interaction.exchange_schema_version?,
        "tui",
    )
}

#[cfg(not(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-workflow-plugin"
)))]
const fn interaction_adapter_for_summary(
    _interaction: &bcode_session_view_models::InteractionViewSummary,
) -> Option<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability> {
    None
}

#[cfg(not(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-workflow-plugin"
)))]
const fn tool_exchange_surface_request(
    _request: &bcode_session_models::ToolExchangeRequest,
) -> Option<InteractiveSurfaceRequest> {
    None
}

#[cfg(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-workflow-plugin"
))]
fn tool_exchange_surface_request(
    request: &bcode_session_models::ToolExchangeRequest,
) -> Option<InteractiveSurfaceRequest> {
    let adapter = super::bundled_interaction_adapter(
        &request.producer_id,
        &request.schema,
        request.schema_version,
        "tui",
    )?;
    let surface_kind = adapter.tui_surface_kind?;
    Some(InteractiveSurfaceRequest::new(
        request.exchange_id.clone(),
        surface_kind,
        request.payload.to_string(),
    ))
}

fn apply_session_open_progress(
    chat: &mut ActiveChat,
    snapshot: &bcode_session_models::SessionOpenOperationSnapshot,
) -> bool {
    if chat.opening_session_id() != Some(snapshot.session_id) {
        return false;
    }
    if chat
        .opening_session_progress
        .as_ref()
        .is_some_and(|current| {
            current.operation_id == snapshot.operation_id && current.revision >= snapshot.revision
        })
    {
        return false;
    }
    chat.opening_session_progress = Some(snapshot.clone());
    chat.app.set_status(session_open_progress_status(snapshot));
    true
}

fn session_open_progress_status(
    snapshot: &bcode_session_models::SessionOpenOperationSnapshot,
) -> String {
    if matches!(
        snapshot.outcome,
        Some(bcode_session_models::SessionOpenTerminalOutcome::Ready)
    ) {
        return "Session storage validated; attaching writable runtime…".to_owned();
    }
    let epoch = snapshot.source_writer_epoch.map_or_else(
        || format!("epoch {}", snapshot.target_writer_epoch),
        |source| format!("epoch {source} → {}", snapshot.target_writer_epoch),
    );
    match (
        snapshot.progress.completed_units,
        snapshot.progress.total_units,
        snapshot.progress.unit,
    ) {
        (Some(completed), Some(total), Some(unit)) if total > 0 => {
            let filled = usize::try_from(completed.saturating_mul(12) / total)
                .unwrap_or(12)
                .min(12);
            let bar = format!("{}{}", "█".repeat(filled), "░".repeat(12 - filled));
            let units = match unit {
                bcode_session_models::SessionMigrationProgressUnit::Bytes => {
                    format!("{} / {}", readable_bytes(completed), readable_bytes(total))
                }
                bcode_session_models::SessionMigrationProgressUnit::Files => {
                    format!("{completed} / {total} files")
                }
                bcode_session_models::SessionMigrationProgressUnit::Events => {
                    format!("{completed} / {total} events")
                }
            };
            format!(
                "Upgrading session ({epoch}) · {} · {bar} {units}",
                snapshot.progress.message
            )
        }
        _ => format!(
            "{} Upgrading session ({epoch}) · {}",
            migration_spinner_frame(),
            snapshot.progress.message
        ),
    }
}

#[cfg(test)]
pub fn test_session_open_progress_status(
    snapshot: &bcode_session_models::SessionOpenOperationSnapshot,
) -> String {
    session_open_progress_status(snapshot)
}

fn readable_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    if bytes >= MIB {
        format_decimal_unit(bytes, MIB, "MiB")
    } else if bytes >= KIB {
        format_decimal_unit(bytes, KIB, "KiB")
    } else {
        format!("{bytes} B")
    }
}

fn format_decimal_unit(value: u64, divisor: u64, suffix: &str) -> String {
    let whole = value / divisor;
    let tenth = value % divisor * 10 / divisor;
    format!("{whole}.{tenth} {suffix}")
}

fn migration_spinner_frame() -> &'static str {
    const FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let index = usize::try_from((elapsed / 100) % FRAMES.len() as u128).unwrap_or(0);
    FRAMES[index]
}

fn reconcile_interactive_surfaces(
    loop_state: &mut ChatLoopState,
    interactions: &[bcode_session_view_models::InteractionViewSummary],
) {
    let pending_ids = interactions
        .iter()
        .filter(|interaction| !interaction.resolved)
        .map(|interaction| interaction.interaction_id.clone())
        .collect::<BTreeSet<_>>();
    if loop_state
        .interactive_surface
        .as_ref()
        .is_some_and(|surface| !pending_ids.contains(surface.interaction_id()))
    {
        loop_state.interactive_surface = None;
    }
    loop_state.interactive_surface_queue.retain(&pending_ids);
    let active_id = loop_state
        .interactive_surface
        .as_ref()
        .map(InteractiveSurfaceState::interaction_id);
    for request in interactions.iter().filter_map(interaction_surface_request) {
        loop_state
            .interactive_surface_queue
            .enqueue(request, active_id);
    }
}

fn observe_interactive_surface_event(loop_state: &mut ChatLoopState, event: &SessionEventKind) {
    match event {
        SessionEventKind::ToolExchangeRequested { request } => {
            if let Some(request) = tool_exchange_surface_request(request) {
                let active_id = loop_state
                    .interactive_surface
                    .as_ref()
                    .map(InteractiveSurfaceState::interaction_id);
                loop_state
                    .interactive_surface_queue
                    .enqueue(request, active_id);
            }
        }
        SessionEventKind::ToolExchangeResolved { event } => {
            loop_state
                .interactive_surface_queue
                .remove(&event.exchange_id);
            if loop_state
                .interactive_surface
                .as_ref()
                .is_some_and(|surface| surface.interaction_id() == event.exchange_id)
            {
                loop_state.interactive_surface = None;
            }
        }
        _ => {}
    }
}

fn agent_selection_status(chat: &ActiveChat, agent_name: &str) -> String {
    if matches!(chat.app.activity(), ActivityState::Idle) {
        format!("agent {agent_name} selected")
    } else {
        format!("agent {agent_name} selected for next message")
    }
}

pub fn cycle_session_agent(chat: &mut ActiveChat) {
    if chat.agents.is_empty() {
        chat.app
            .set_status("Agent metadata is still loading".to_owned());
        return;
    }
    let current_agent_id = chat
        .app
        .pending_agent_id()
        .unwrap_or_else(|| chat.app.current_agent_id());
    let Some(agent) = chat.agents.next_agent(current_agent_id) else {
        chat.app.set_status("no agents available".to_owned());
        return;
    };
    let agent_id = agent.id.clone();
    let agent_name = agent.name.clone();
    let agent_accent = agent.accent.clone();
    if chat.app.session_id().is_some() {
        chat.app.set_pending_agent(agent_id, agent_accent);
        chat.app
            .set_status(agent_selection_status(chat, &agent_name));
    } else {
        chat.agents.apply_agent_to_app(&mut chat.app, agent_id);
        chat.app.set_status(format!("agent set to {agent_name}"));
    }
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;

    fn command_response(success: bool) -> bcode_command::InvokeCommandResponse {
        bcode_command::InvokeCommandResponse {
            success,
            message: Some("command diagnostic".to_owned()),
            updated_model: None,
            updated_provider: None,
            updated_thinking: None,
            effects: vec![
                bcode_command::CommandEffect::Status {
                    message: "must not replace failure".to_owned(),
                },
                bcode_command::CommandEffect::CopyText {
                    text: "clipboard".to_owned(),
                },
                bcode_command::CommandEffect::AppendText {
                    text: "transcript".to_owned(),
                    format: bcode_command::CommandTextFormat::PlainText,
                },
                bcode_command::CommandEffect::ToggleSurface {
                    surface_id: "surface".to_owned(),
                },
                bcode_command::CommandEffect::OpenSession {
                    session_id: bcode_session_models::SessionId::new(),
                    focus: bcode_command::SessionOpenFocus::Composer,
                },
                bcode_command::CommandEffect::OpenPluginSurface {
                    surface_kind: "kind".to_owned(),
                    instance_id: "instance".to_owned(),
                    options: serde_json::Value::Null,
                },
            ],
        }
    }

    #[test]
    fn failed_command_preserves_diagnostic_and_suppresses_native_effects() {
        let (message, effects) = plugin_command_presentation(command_response(false));
        assert_eq!(message.as_deref(), Some("command diagnostic"));
        assert!(effects.is_empty());
    }

    #[test]
    fn successful_command_preserves_native_effects_in_order() {
        let response = command_response(true);
        let expected = response.effects.clone();
        let (message, effects) = plugin_command_presentation(response);
        assert_eq!(message.as_deref(), Some("command diagnostic"));
        assert_eq!(effects, expected);
    }

    #[test]
    fn failed_command_without_message_has_failure_status() {
        let mut response = command_response(false);
        response.message = None;
        let (message, effects) = plugin_command_presentation(response);
        assert_eq!(message.as_deref(), Some("Plugin command failed"));
        assert!(effects.is_empty());
    }

    #[test]
    fn search_maintenance_status_messages_are_human_readable_and_truthful() {
        let migration = bcode_ipc::SessionBulkMigrationOperationStatus {
            operation_id: "migration".to_owned(),
            revision: 2,
            state: bcode_ipc::SessionBulkMigrationState::NeedsAttention,
            mode: bcode_ipc::SessionBulkMigrationMode::Migrate,
            selected: 6,
            visited: 5,
            migrated: 3,
            blocked: 1,
            failed: 1,
            current_session_id: None,
            outcomes: Vec::new(),
        };
        assert_eq!(
            bulk_migration_status_message(&migration),
            "Canonical migration needs attention: visited 5, migrated 3, blocked 1, failed 1"
        );

        let backfill = bcode_session_search::SessionSearchBackfillOperationStatus {
            operation_id: "backfill".to_owned(),
            provider_id: "all".to_owned(),
            revision: 4,
            state: bcode_session_search::SessionSearchBackfillOperationState::NeedsAttention,
            response: None,
            complete_progress: Some(
                bcode_session_search::CompleteSessionSearchBackfillProgress {
                    provider_ids: vec!["provider".to_owned()],
                    current_provider_id: None,
                    catalog_revision_started: 1,
                    convergence_pass: 1,
                    providers_completed: 1,
                    selected_sessions: 8,
                    visited_sessions: 6,
                    completed_sessions: 3,
                    incomplete_sessions: 2,
                    failed_sessions: 1,
                    providers: vec![
                        bcode_session_search::CompleteSessionSearchBackfillProviderResult {
                            provider_id: "provider".to_owned(),
                            selected_sessions: 8,
                            completed_sessions: 3,
                            incomplete_sessions: 2,
                            failed_sessions: 1,
                            catalog_pages: 1,
                            issues: Vec::new(),
                            error: None,
                        },
                    ],
                },
            ),
            complete_response: None,
            error: None,
        };
        assert_eq!(
            search_backfill_status_message(&backfill),
            "Derived search backfill needs attention: visited 6, complete 3, blocked/incomplete 2, failed 1, unvisited 2"
        );
        backfill
            .complete_progress
            .as_ref()
            .expect("progress")
            .validate()
            .expect("portable progress");
    }

    struct DelayedEffectSurface {
        commits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        release: Option<tokio::sync::oneshot::Receiver<()>>,
        polls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        callbacks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl bcode_plugin_sdk::tui::PluginTuiSurface for DelayedEffectSurface {
        fn id(&self) -> &'static str {
            "delayed-effect"
        }
        fn title(&self) -> &'static str {
            "Delayed effect"
        }
        fn render(&mut self, _area: Rect, _frame: &mut bmux_tui::paint::PaintCx<'_, '_>) {}
        fn handle_event(
            &mut self,
            event: &Event,
            _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
        ) -> bcode_plugin_sdk::tui::PluginTuiAction {
            if matches!(event, Event::Key(key) if key.key == bmux_keyboard::KeyCode::Char('o')) {
                return bcode_plugin_sdk::tui::PluginTuiAction::OpenSession {
                    session_id: bcode_session_models::SessionId::new(),
                };
            }
            bcode_plugin_sdk::tui::PluginTuiAction::Redraw
        }
        fn session_navigation_finished(
            &mut self,
            _session_id: bcode_session_models::SessionId,
            _result: Result<(), String>,
        ) {
            self.callbacks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        fn poll(
            &mut self,
            _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
        ) -> bcode_plugin_sdk::tui::PluginTuiAction {
            self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            bcode_plugin_sdk::tui::PluginTuiAction::None
        }
        fn drain_effects<'a>(
            &'a mut self,
            _host: &'a dyn bcode_plugin_sdk::tui::PluginTuiHost,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = bcode_plugin_sdk::tui::PluginTuiAction>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                if let Some(release) = self.release.take() {
                    release.await.expect("release delayed operation");
                    self.commits
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw
            })
        }
    }

    #[tokio::test]
    async fn surface_completion_wakes_poll_and_preserves_navigation_during_drain() {
        use bcode_plugin_sdk::tui::PluginTuiAction;
        let mut state = loop_state();
        let client = bcode_client::BcodeClient::default_endpoint();
        let launch = super::super::plugin_surface_host::FreshSessionSettings::default();
        let (release, receiver) = tokio::sync::oneshot::channel();
        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        state.queue_root_plugin_surface(
            "test",
            Box::new(DelayedEffectSurface {
                commits: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                release: Some(receiver),
                polls: std::sync::Arc::clone(&polls),
                callbacks: std::sync::Arc::default(),
            }),
        );
        let _ = state.poll_root_plugin_surface(&client, launch.clone());
        let open = Event::Key(bmux_keyboard::KeyStroke::simple(
            bmux_keyboard::KeyCode::Char('o'),
        ));
        let _ = state.handle_root_plugin_surface_event(&open, &client, launch.clone());
        release.send(()).expect("operation alive");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while state.plugin_surface.as_ref().unwrap().surface.is_none() {
                let _ = state.poll_root_plugin_surface(&client, launch.clone());
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("operation settled");
        assert_eq!(
            polls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "settlement reconciles the surface before exposing completion"
        );
        assert!(state.root_plugin_surface_invalidation().unwrap().take());
        assert!(matches!(
            state.poll_root_plugin_surface(&client, launch),
            Some(PluginTuiAction::OpenSession { .. })
        ));
        state.close_root_plugin_surface();
    }

    #[tokio::test]
    async fn nested_mounts_retain_every_unresolved_parent_drain() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut state = loop_state();
        let client = bcode_client::BcodeClient::default_endpoint();
        let launch = super::super::plugin_surface_host::FreshSessionSettings::default();
        let commits = std::sync::Arc::new(AtomicUsize::new(0));
        let mut releases = Vec::new();
        for _ in 0..3 {
            let (release, receiver) = tokio::sync::oneshot::channel();
            releases.push(release);
            state.queue_root_plugin_surface(
                "test",
                Box::new(DelayedEffectSurface {
                    commits: commits.clone(),
                    release: Some(receiver),
                    polls: std::sync::Arc::default(),
                    callbacks: std::sync::Arc::default(),
                }),
            );
            let _action = state.poll_root_plugin_surface(&client, launch.clone());
        }
        assert_eq!(state.parent_plugin_surfaces.len(), 2);
        for release in releases {
            release.send(()).expect("parent drain still owned");
        }
        assert_eq!(
            state
                .settle_root_plugin_effects()
                .await
                .expect("all settled")
                .len(),
            3
        );
        assert_eq!(commits.load(Ordering::SeqCst), 3);
        state.close_root_plugin_surface();
        assert_eq!(state.parent_plugin_surfaces.len(), 1);
        state.close_root_plugin_surface();
        assert_eq!(state.parent_plugin_surfaces.len(), 0);
        state.close_root_plugin_surface();
        assert!(!state.has_root_plugin_surface());
    }

    #[tokio::test]
    async fn failed_drain_never_becomes_a_successful_surface_close() {
        let mut state = loop_state();
        let client = bcode_client::BcodeClient::default_endpoint();
        let launch = super::super::plugin_surface_host::FreshSessionSettings::default();
        let (_release, receiver) = tokio::sync::oneshot::channel();
        state.queue_root_plugin_surface(
            "test",
            Box::new(DelayedEffectSurface {
                commits: std::sync::Arc::default(),
                release: Some(receiver),
                polls: std::sync::Arc::default(),
                callbacks: std::sync::Arc::default(),
            }),
        );
        let _action = state.poll_root_plugin_surface(&client, launch.clone());
        state
            .plugin_surface
            .as_ref()
            .expect("surface")
            .drain_task
            .as_ref()
            .expect("tracked task")
            .abort();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let action = state.poll_root_plugin_surface(&client, launch.clone());
                assert!(!matches!(
                    action,
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::Close { .. })
                ));
                if state.verify_plugin_operations().is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed task observed");
        let error = state
            .verify_plugin_operations()
            .expect_err("normalized failure");
        assert!(error.to_string().contains("effects may have occurred"));
        assert!(state.settle_root_plugin_effects().await.is_err());
    }

    #[tokio::test]
    async fn shutdown_observes_drain_completion_and_deferred_navigation_callback() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut state = loop_state();
        let client = bcode_client::BcodeClient::default_endpoint();
        let launch = super::super::plugin_surface_host::FreshSessionSettings::default();
        let commits = std::sync::Arc::new(AtomicUsize::new(0));
        let callbacks = std::sync::Arc::new(AtomicUsize::new(0));
        let (release, receiver) = tokio::sync::oneshot::channel();
        state.queue_root_plugin_surface(
            "test",
            Box::new(DelayedEffectSurface {
                commits: commits.clone(),
                release: Some(receiver),
                polls: std::sync::Arc::default(),
                callbacks: callbacks.clone(),
            }),
        );
        let _action = state.poll_root_plugin_surface(&client, launch);
        let session_id = bcode_session_models::SessionId::new();
        assert!(state.suspend_root_plugin_surface_for_session(session_id));
        assert!(state.complete_root_plugin_session_navigation(session_id, Ok(())));
        assert_eq!(callbacks.load(Ordering::SeqCst), 0);
        // No terminal events/presentation are required to observe settlement.
        let settlement = state.settle_root_plugin_effects();
        tokio::pin!(settlement);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut settlement)
                .await
                .is_err()
        );
        release.send(()).expect("accepted operation remains owned");
        let actions = settlement.await.expect("verified shutdown settlement");
        assert_eq!(actions.len(), 1);
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn navigation_callback_waits_for_foreground_drain_settlement() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut state = loop_state();
        let client = bcode_client::BcodeClient::default_endpoint();
        let launch = super::super::plugin_surface_host::FreshSessionSettings::default();
        let callbacks = std::sync::Arc::new(AtomicUsize::new(0));
        let (release, receiver) = tokio::sync::oneshot::channel();
        state.queue_root_plugin_surface(
            "test",
            Box::new(DelayedEffectSurface {
                commits: std::sync::Arc::default(),
                release: Some(receiver),
                polls: std::sync::Arc::default(),
                callbacks: callbacks.clone(),
            }),
        );
        let _action = state.poll_root_plugin_surface(&client, launch.clone());
        let session_id = bcode_session_models::SessionId::new();
        assert!(state.suspend_root_plugin_surface_for_session(session_id));
        assert!(
            state.complete_root_plugin_session_navigation(session_id, Err("attach failed".into()))
        );
        assert_eq!(callbacks.load(Ordering::SeqCst), 0);
        release.send(()).expect("accepted operation still owned");
        tokio::time::timeout(Duration::from_secs(2), async {
            while callbacks.load(Ordering::SeqCst) == 0 {
                let _action = state.poll_root_plugin_surface(&client, launch.clone());
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("callback delivered on settlement");
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
        let _actions = state
            .settle_root_plugin_effects()
            .await
            .expect("task relinquished");
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn suspended_surface_does_not_replay_queued_input_after_drain() {
        let mut state = loop_state();
        let client = bcode_client::BcodeClient::default_endpoint();
        let launch = super::super::plugin_surface_host::FreshSessionSettings::default();
        let (release, receiver) = tokio::sync::oneshot::channel();
        state.queue_root_plugin_surface(
            "test",
            Box::new(DelayedEffectSurface {
                commits: std::sync::Arc::default(),
                release: Some(receiver),
                polls: std::sync::Arc::default(),
                callbacks: std::sync::Arc::default(),
            }),
        );
        let _action = state.poll_root_plugin_surface(&client, launch.clone());
        let open = Event::Key(bmux_keyboard::KeyStroke::simple(
            bmux_keyboard::KeyCode::Char('o'),
        ));
        let _action = state.handle_root_plugin_surface_event(&open, &client, launch.clone());
        assert_eq!(
            state
                .plugin_surface
                .as_ref()
                .expect("surface")
                .deferred_input
                .len(),
            1
        );
        assert!(
            state.suspend_root_plugin_surface_for_session(bcode_session_models::SessionId::new())
        );
        release.send(()).expect("accepted operation remains owned");
        tokio::time::timeout(Duration::from_secs(2), async {
            while state
                .plugin_surface
                .as_ref()
                .expect("surface")
                .surface
                .is_none()
            {
                let action = state.poll_root_plugin_surface(&client, launch.clone());
                assert!(!matches!(
                    action,
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::OpenSession { .. })
                ));
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("suspended operation settled");
        assert!(
            state
                .plugin_surface
                .as_ref()
                .expect("surface")
                .deferred_input
                .is_empty()
        );
        assert!(matches!(
            state.poll_root_plugin_surface(&client, launch.clone()),
            Some(
                bcode_plugin_sdk::tui::PluginTuiAction::None
                    | bcode_plugin_sdk::tui::PluginTuiAction::Redraw
            )
        ));
    }

    #[tokio::test]
    async fn mounted_surface_effect_runs_without_input_and_is_not_replayed_after_close() {
        use bcode_plugin_sdk::tui::PluginTuiAction;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut state = loop_state();
        let client = bcode_client::BcodeClient::default_endpoint();
        let launch = super::super::plugin_surface_host::FreshSessionSettings::default();
        let commits = std::sync::Arc::new(AtomicUsize::new(0));
        let (release, receiver) = tokio::sync::oneshot::channel();
        state.queue_root_plugin_surface(
            "test",
            Box::new(DelayedEffectSurface {
                commits: commits.clone(),
                release: Some(receiver),
                polls: std::sync::Arc::default(),
                callbacks: std::sync::Arc::default(),
            }),
        );
        // Mount invalidation alone dispatches; no terminal event is admitted.
        let _ = state.poll_root_plugin_surface(&client, launch.clone());
        assert!(state.plugin_surface.as_ref().unwrap().drain.is_some());
        release
            .send(())
            .expect("operation dispatched without keyboard input");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let action = state.poll_root_plugin_surface(&client, launch.clone());
                if state.plugin_surface.as_ref().unwrap().surface.is_some() {
                    assert!(matches!(action, Some(PluginTuiAction::Redraw)));
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completion observed without keyboard input");
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        state.close_root_plugin_surface();
        assert!(state.poll_root_plugin_surface(&client, launch).is_none());
        tokio::task::yield_now().await;
        assert_eq!(commits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn root_surface_drains_exclusively_once_and_defers_close_until_settled() {
        use bcode_plugin_sdk::tui::PluginTuiAction;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut state = loop_state();
        let client = bcode_client::BcodeClient::default_endpoint();
        let launch = super::super::plugin_surface_host::FreshSessionSettings::default();
        let commits = std::sync::Arc::new(AtomicUsize::new(0));
        let (release, receiver) = tokio::sync::oneshot::channel();
        state.queue_root_plugin_surface(
            "test",
            Box::new(DelayedEffectSurface {
                commits: commits.clone(),
                release: Some(receiver),
                polls: std::sync::Arc::default(),
                callbacks: std::sync::Arc::default(),
            }),
        );
        let submit = Event::Key(bmux_keyboard::KeyStroke::simple(
            bmux_keyboard::KeyCode::Enter,
        ));
        let _ = state.handle_root_plugin_surface_event(&submit, &client, launch.clone());
        let _ = state.poll_root_plugin_surface(&client, launch.clone());
        assert!(state.plugin_surface.as_ref().unwrap().surface.is_none());
        let exit = Event::Key(bmux_keyboard::KeyStroke::simple(
            bmux_keyboard::KeyCode::Escape,
        ));
        let _ = state.handle_root_plugin_surface_event(&exit, &client, launch.clone());
        assert!(!matches!(
            state.poll_root_plugin_surface(&client, launch.clone()),
            Some(PluginTuiAction::Close { .. })
        ));
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        // Programmatic close obeys the same settlement fence as terminal cancellation.
        state.close_root_plugin_surface();
        assert!(state.has_root_plugin_surface());
        release.send(()).expect("drain still owns operation");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if matches!(
                    state.poll_root_plugin_surface(&client, launch.clone()),
                    Some(PluginTuiAction::Close { .. })
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("settled close");
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        state.close_root_plugin_surface();
    }

    struct PassiveSurface;

    impl bcode_plugin_sdk::tui::PluginTuiSurface for PassiveSurface {
        fn id(&self) -> &'static str {
            "passive-test"
        }

        fn title(&self) -> &'static str {
            "Passive test"
        }

        fn render(&mut self, _area: Rect, _frame: &mut bmux_tui::paint::PaintCx<'_, '_>) {}

        fn handle_event(
            &mut self,
            _event: &Event,
            _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
        ) -> bcode_plugin_sdk::tui::PluginTuiAction {
            bcode_plugin_sdk::tui::PluginTuiAction::None
        }
    }

    fn loop_state() -> ChatLoopState {
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        ChatLoopState::new(&client, &passive, false)
    }

    #[test]
    fn permission_refresh_preserves_focus_and_replaces_resolved_dialogs() {
        let permission = |id: &str| bcode_session_view_models::PermissionView {
            permission_id: id.to_owned(),
            session_id: Some(bcode_session_models::SessionId::new()),
            tool_call_id: id.to_owned(),
            tool_name: "shell.run".to_owned(),
            arguments_json: "{}".to_owned(),
            batch: None,
            agent_id: "build".to_owned(),
            title: None,
            policy_source: None,
            detail: None,
            resolved: false,
            approved: None,
            can_remember: false,
        };
        let first = permission("first");
        let second = permission("second");
        let mut dialog = None;
        reconcile_permission_dialog(&mut dialog, vec![first.clone(), second.clone()]);
        assert_eq!(dialog.as_ref().unwrap().permission_id(), "first");
        reconcile_permission_dialog(&mut dialog, vec![second.clone(), first]);
        assert_eq!(dialog.as_ref().unwrap().permission_id(), "first");
        reconcile_permission_dialog(&mut dialog, vec![second]);
        assert_eq!(dialog.as_ref().unwrap().permission_id(), "second");
        reconcile_permission_dialog(&mut dialog, Vec::new());
        assert!(dialog.is_none());
    }

    #[tokio::test]
    async fn session_picker_rejects_old_subscription_results() {
        let mut state = loop_state();
        state.session_picker_generation = 2;
        state.session_picker = Some(super::super::session_picker::SessionPickerApp::new(
            Vec::new(),
        ));
        let initial = state.session_picker.as_ref().unwrap().status().to_owned();
        state.apply_session_picker_result(1, Err(ClientError::Protocol("old connection".into())));
        assert_eq!(state.session_picker.as_ref().unwrap().status(), initial);
        state.apply_session_picker_result(
            2,
            Err(ClientError::Protocol("current connection".into())),
        );
        assert!(
            state
                .session_picker
                .as_ref()
                .unwrap()
                .status()
                .contains("current connection")
        );
        state.session_picker = None;
        state
            .apply_session_picker_result(2, Err(ClientError::Protocol("closed connection".into())));
        assert!(state.session_picker.is_none());
    }

    #[tokio::test]
    async fn catalog_restart_updates_are_fenced_and_preserve_scope_and_query() {
        use super::super::session_picker::SessionPickerApp;
        use bcode_session_models::{
            SessionCatalogCursor, SessionCatalogPage, SessionCatalogQuery, SessionCatalogStatus,
            SessionDiscoveryScope,
        };
        let mut state = loop_state();
        state.session_picker_generation = 4;
        let mut picker = SessionPickerApp::new(Vec::new());
        *picker.filter_mut() = super::super::text_input_flow::state_with_text("東京 query", false);
        let cursor = SessionCatalogCursor {
            scope: SessionDiscoveryScope::AllWorkingDirectories,
            query: SessionCatalogQuery {
                text: Some("東京 query".to_owned()),
            },
            revision: 1,
            location_index: 0,
            after: None,
        };
        picker.set_catalog_next_cursor(Some(cursor.clone()));
        assert!(picker.next_catalog_page());
        state.session_picker = Some(picker);
        let page = || SessionCatalogPage {
            sessions: Vec::new(),
            catalog_status: SessionCatalogStatus::Loaded,
            catalog_sources: Vec::new(),
            catalog_revision: 2,
            next_cursor: Some(cursor.clone()),
        };
        // A result from a previous scope/query/page load cannot reset navigation.
        state.apply_session_catalog_update(3, Ok(page()), true);
        assert_eq!(
            state.session_picker.as_ref().unwrap().catalog_cursor(),
            Some(&cursor)
        );
        state.apply_session_catalog_update(4, Ok(page()), true);
        let picker = state.session_picker.as_ref().unwrap();
        assert!(picker.catalog_cursor().is_none());
        assert!(picker.has_catalog_continuation());
        assert!(!picker.all_directories());
        assert_eq!(picker.filter().buffer().text(), "東京 query");
    }

    fn surface(interaction_id: &str) -> InteractiveSurfaceState {
        InteractiveSurfaceState::from_surface_for_test(
            interaction_id,
            Box::new(PassiveSurface),
            &BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
        )
    }

    fn resolved_event(exchange_id: &str) -> SessionEventKind {
        SessionEventKind::ToolExchangeResolved {
            event: bcode_session_models::ToolExchangeResolutionEvent {
                invocation_id: format!("call-{exchange_id}"),
                exchange_id: exchange_id.to_owned(),
                resolution: bcode_session_models::ToolExchangeResolution::Cancelled,
            },
        }
    }

    #[tokio::test]
    async fn authoritative_resolution_closes_active_surface_in_every_presentation_state() {
        let geometries = [
            None,
            Some(InteractiveSurfaceGeometry {
                placement: InteractiveSurfacePlacement::Transcript,
                logical_height: 20,
                visible_logical_offset: 0,
                destination: Rect::new(0, 0, 80, 10),
            }),
            Some(InteractiveSurfaceGeometry {
                placement: InteractiveSurfacePlacement::Transcript,
                logical_height: 20,
                visible_logical_offset: 5,
                destination: Rect::new(0, 0, 80, 10),
            }),
            Some(InteractiveSurfaceGeometry {
                placement: InteractiveSurfacePlacement::Transcript,
                logical_height: 20,
                visible_logical_offset: 20,
                destination: Rect::new(0, 0, 80, 0),
            }),
            Some(InteractiveSurfaceGeometry {
                placement: InteractiveSurfacePlacement::Pinned,
                logical_height: 20,
                visible_logical_offset: 5,
                destination: Rect::new(0, 0, 80, 10),
            }),
        ];

        for geometry in geometries {
            let mut state = loop_state();
            state.install_interactive_surface_for_test(surface("resolved"));
            state.set_interactive_surface_geometry_for_test(geometry);
            observe_interactive_surface_event(&mut state, &resolved_event("resolved"));
            assert!(!state.has_interactive_surface());
        }
    }

    #[tokio::test]
    async fn resolution_removes_opening_and_queued_requests_and_stale_open_cannot_reopen() {
        let mut state = loop_state();
        assert!(state.interactive_surface_queue.enqueue(
            InteractiveSurfaceRequest::new("opening", "surface", "{}"),
            None,
        ));
        assert!(state.interactive_surface_queue.enqueue(
            InteractiveSurfaceRequest::new("queued", "surface", "{}"),
            None,
        ));
        assert!(!state.interactive_surface_queue.enqueue(
            InteractiveSurfaceRequest::new("queued", "surface", "{}"),
            None,
        ));
        assert_eq!(
            state.interactive_surface_queue.interaction_ids(),
            ["opening", "queued"]
        );

        observe_interactive_surface_event(&mut state, &resolved_event("opening"));
        assert_eq!(
            state.interactive_surface_queue.interaction_ids(),
            ["queued"]
        );
        assert_eq!(
            state.complete_interactive_surface_open(Ok(surface("opening"))),
            InteractiveSurfaceOpenCompletion::Stale
        );
        assert!(!state.has_interactive_surface());
        assert_eq!(
            state.interactive_surface_queue.interaction_ids(),
            ["queued"]
        );

        observe_interactive_surface_event(&mut state, &resolved_event("queued"));
        assert!(state.interactive_surface_queue.interaction_ids().is_empty());
    }

    #[tokio::test]
    async fn failed_open_is_distinct_from_stale_completion_and_defers_retry() {
        let mut state = loop_state();
        assert!(state.interactive_surface_queue.enqueue(
            InteractiveSurfaceRequest::new("first", "surface", "{}"),
            None,
        ));

        assert_eq!(
            state.complete_interactive_surface_open(Err("unavailable".to_owned())),
            InteractiveSurfaceOpenCompletion::Failed
        );
        assert!(state.next_surface_open_request().is_none());
        assert!(state.next_interactive_surface_retry_at().is_some());
        assert_eq!(state.interactive_surface_queue.interaction_ids(), ["first"]);
    }

    #[tokio::test]
    async fn fifo_open_completion_requires_the_authoritative_queue_front() {
        let mut state = loop_state();
        for id in ["first", "second"] {
            assert!(
                state
                    .interactive_surface_queue
                    .enqueue(InteractiveSurfaceRequest::new(id, "surface", "{}"), None,)
            );
        }

        assert_eq!(
            state.complete_interactive_surface_open(Ok(surface("second"))),
            InteractiveSurfaceOpenCompletion::Stale
        );
        assert_eq!(
            state.interactive_surface_queue.interaction_ids(),
            ["first", "second"]
        );
        assert_eq!(
            state.complete_interactive_surface_open(Ok(surface("first"))),
            InteractiveSurfaceOpenCompletion::Opened
        );
        assert_eq!(state.active_interactive_surface_id(), Some("first"));
        assert_eq!(
            state.interactive_surface_queue.interaction_ids(),
            ["second"]
        );
    }
}
