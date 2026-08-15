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
        chat.session_id.map_or_else(
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

struct RootPluginSurface {
    plugin_id: String,
    surface: bcode_plugin_sdk::tui::BoxedPluginTuiSurface,
    invalidation: bmux_tui_runtime::InvalidationSignal,
    pending_session_navigation: Option<bcode_session_models::SessionId>,
}

struct RootModelPicker {
    provider_plugin_id: Option<String>,
    picker: super::model_picker::ModelPickerApp,
}

pub enum AuthPoolPickerRootOutcome {
    Continue,
    Cancel,
    Promote { pool: String, profile: String },
    Clear { pool: String },
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
    palette: Option<BmuxCommandPalette>,
    slash_palette: Option<slash_palette::SlashPalette>,
    foreground_client: BcodeClient,
    passive_client: BcodeClient,
    daemon_connection: DaemonConnectionMonitor,
    pub(super) permission_dialog: Option<PermissionDialogState>,
    thinking_dialog: Option<super::thinking_dialog::ThinkingDialogState>,
    theme_picker: Option<super::theme_picker::ThemePickerState>,
    streaming_configurator: Option<super::streaming_configurator::StreamingConfiguratorState>,
    timeline_dialog: Option<super::timeline_dialog::TimelineDialogState>,
    plugin_surface: Option<RootPluginSurface>,
    provider_picker: Option<super::provider_picker::ProviderPickerApp>,
    model_picker: Option<RootModelPicker>,
    auth_pool_picker: Option<super::auth_pool_picker::AuthPoolPickerApp>,
    skill_picker: Option<super::skill_picker::SkillPickerApp>,
    working_directory_dialog: Option<super::working_directory_dialog::WorkingDirectoryDialog>,
    worktree_create_dialog: Option<super::wt_create_dialog::WorktreeCreateDialog>,
    ralph_start_dialog: Option<super::ralph_start_dialog::RalphStartDialog>,
    interactive_surface: Option<InteractiveSurfaceState>,
    interactive_surface_geometry: Option<InteractiveSurfaceGeometry>,
    pinned_interaction_viewport: Option<(String, u16)>,
    session_picker: Option<super::session_picker::SessionPickerApp>,
    interactive_surface_queue: InteractiveSurfaceQueue,
    artifact_stream: ArtifactStreamCoordinator,
    markdown_projection: super::markdown_projection_coordinator::MarkdownProjectionCoordinator,
    markdown_presentation: Option<super::markdown_image::MarkdownPresentationRuntime>,
    markdown_mermaid: Option<super::markdown_mermaid::MarkdownMermaidRuntime>,
    markdown_image_tasks:
        Vec<tokio::task::JoinHandle<super::markdown_image::MarkdownImageLoadCompletion>>,
    markdown_mermaid_tasks:
        Vec<tokio::task::JoinHandle<super::markdown_mermaid::MarkdownMermaidCompletion>>,
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

impl ChatLoopState {
    pub fn new(
        foreground_client: &BcodeClient,
        passive_client: &BcodeClient,
        metrics_enabled: bool,
    ) -> Self {
        Self {
            palette: None,
            slash_palette: None,
            foreground_client: foreground_client.clone(),
            passive_client: passive_client.clone(),
            daemon_connection: DaemonConnectionMonitor::default(),
            permission_dialog: None,
            thinking_dialog: None,
            theme_picker: None,
            streaming_configurator: None,
            timeline_dialog: None,
            plugin_surface: None,
            provider_picker: None,
            model_picker: None,
            auth_pool_picker: None,
            skill_picker: None,
            working_directory_dialog: None,
            worktree_create_dialog: None,
            ralph_start_dialog: None,
            interactive_surface: None,
            interactive_surface_geometry: None,
            pinned_interaction_viewport: None,
            session_picker: None,
            interactive_surface_queue: InteractiveSurfaceQueue::default(),
            artifact_stream: ArtifactStreamCoordinator::new(passive_client.clone()),
            markdown_projection:
                super::markdown_projection_coordinator::MarkdownProjectionCoordinator::new(),
            markdown_presentation: super::markdown_image::MarkdownPresentationRuntime::new().ok(),
            markdown_mermaid: super::markdown_mermaid::MarkdownMermaidRuntime::packaged().ok(),
            markdown_image_tasks: Vec::new(),
            markdown_mermaid_tasks: Vec::new(),
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
    ) -> bool {
        let Some(configurator) = self.streaming_configurator.as_mut() else {
            return false;
        };
        match configurator.handle_key(stroke, Instant::now()) {
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

    pub const fn observe_daemon(&mut self, chat: &mut ActiveChat, observation: &DaemonObservation) {
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

    pub fn handle_theme_picker_key(&mut self, chat: &mut ActiveChat, stroke: KeyStroke) -> bool {
        let Some(picker) = self.theme_picker.as_mut() else {
            return false;
        };
        let outcome = picker.handle_key(stroke);
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

    pub fn handle_command_palette_key(&mut self, stroke: KeyStroke) -> Option<CommandAction> {
        let palette = self.palette.as_mut()?;
        match palette.handle_key(stroke, 12) {
            bmux_tui::palette::CommandPaletteKeyOutcome::Ignored
            | bmux_tui::palette::CommandPaletteKeyOutcome::QueryEdited
            | bmux_tui::palette::CommandPaletteKeyOutcome::SelectionMoved => None,
            bmux_tui::palette::CommandPaletteKeyOutcome::Canceled => {
                self.palette = None;
                None
            }
            bmux_tui::palette::CommandPaletteKeyOutcome::Activated(index) => {
                let action = palette.contribution_at(index).map(|item| item.action);
                self.palette = None;
                action
            }
        }
    }

    pub fn handle_command_palette_mouse(
        &mut self,
        mouse: bmux_tui::event::MouseEvent,
        frame_area: Rect,
    ) -> Option<CommandAction> {
        let index = super::picker_mouse::command_palette_row_in_area(
            mouse,
            super::command_palette_render::palette_list_area(frame_area),
        )?;
        let palette = self.palette.as_mut()?;
        let action = palette.contribution_at(index).map(|item| item.action);
        self.palette = None;
        action
    }

    pub const fn has_slash_palette(&self) -> bool {
        self.slash_palette.is_some()
    }

    pub fn handle_slash_palette_key(
        &mut self,
        chat: &mut ActiveChat,
        stroke: KeyStroke,
    ) -> SlashPaletteRootOutcome {
        let Some(palette) = self.slash_palette.as_mut() else {
            return SlashPaletteRootOutcome::Unhandled;
        };
        match stroke.key {
            bmux_keyboard::KeyCode::Up if stroke.modifiers.is_empty() => {
                palette.move_previous();
                SlashPaletteRootOutcome::Handled
            }
            bmux_keyboard::KeyCode::Down if stroke.modifiers.is_empty() => {
                palette.move_next();
                SlashPaletteRootOutcome::Handled
            }
            bmux_keyboard::KeyCode::Tab if stroke.modifiers.is_empty() => {
                let command = palette.selected_command().map(str::to_owned);
                self.slash_palette = None;
                if let Some(command) = command {
                    chat.app.reset_input_history_navigation();
                    chat.app.replace_composer_with(&command);
                }
                SlashPaletteRootOutcome::Handled
            }
            bmux_keyboard::KeyCode::Enter if stroke.modifiers.is_empty() => {
                if palette.selected_matches(chat.app.composer().text()) {
                    self.slash_palette = None;
                    chat.app.stage_submission();
                    SlashPaletteRootOutcome::Submit
                } else {
                    let command = palette.selected_command().map(str::to_owned);
                    self.slash_palette = None;
                    if let Some(command) = command {
                        chat.app.reset_input_history_navigation();
                        chat.app.replace_composer_with(&command);
                    }
                    SlashPaletteRootOutcome::Handled
                }
            }
            bmux_keyboard::KeyCode::Escape if stroke.modifiers.is_empty() => {
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
            self.slash_palette = None;
            return true;
        };
        if let Some(command) = palette
            .select_visible_row(row, usize::from(frame_area.height))
            .map(str::to_owned)
        {
            chat.app.reset_input_history_navigation();
            chat.app.replace_composer_with(&command);
        }
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
        let invalidation = bmux_tui_runtime::InvalidationSignal::new();
        invalidation.request();
        self.plugin_surface = Some(RootPluginSurface {
            plugin_id: plugin_id.into(),
            surface,
            invalidation,
            pending_session_navigation: None,
        });
    }

    pub const fn suspend_root_plugin_surface_for_session(
        &mut self,
        session_id: bcode_session_models::SessionId,
    ) -> bool {
        let Some(surface) = self.plugin_surface.as_mut() else {
            return false;
        };
        surface.pending_session_navigation = Some(session_id);
        true
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
        surface
            .surface
            .session_navigation_finished(session_id, result);
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
            .map(|surface| surface.surface.id())
    }

    pub fn handle_root_plugin_surface_event(
        &mut self,
        event: &Event,
        client: &BcodeClient,
    ) -> Option<bcode_plugin_sdk::tui::PluginTuiAction> {
        let surface = self.plugin_surface.as_mut()?;
        let host =
            super::plugin_surface_host::root_host(surface.invalidation.clone(), client.clone());
        Some(surface.surface.handle_event(event, &host))
    }

    pub fn root_plugin_surface_invalidation(&self) -> Option<bmux_tui_runtime::InvalidationSignal> {
        self.plugin_surface
            .as_ref()
            .map(|surface| surface.invalidation.clone())
    }

    pub fn poll_root_plugin_surface(
        &mut self,
        client: &BcodeClient,
    ) -> Option<bcode_plugin_sdk::tui::PluginTuiAction> {
        let surface = self.plugin_surface.as_mut()?;
        let host =
            super::plugin_surface_host::root_host(surface.invalidation.clone(), client.clone());
        let invalidated = surface.invalidation.take();
        let action = surface.surface.poll(&host);
        if invalidated && matches!(action, bcode_plugin_sdk::tui::PluginTuiAction::None) {
            Some(bcode_plugin_sdk::tui::PluginTuiAction::Redraw)
        } else {
            Some(action)
        }
    }

    pub fn attach_root_plugin_surface_updates(
        &mut self,
        updates: bcode_plugin_sdk::tui::PluginTuiSurfaceUpdateReceiver,
    ) {
        if let Some(surface) = self.plugin_surface.as_mut() {
            surface.surface.attach_updates(updates);
        }
    }

    pub fn close_root_plugin_surface_with_outcome(
        &mut self,
        outcome: Option<serde_json::Value>,
    ) -> Option<(String, Option<serde_json::Value>)> {
        let surface = self.plugin_surface.take()?;
        Some((surface.plugin_id, outcome))
    }

    pub fn close_root_plugin_surface(&mut self) {
        self.plugin_surface = None;
    }

    pub const fn has_model_picker(&self) -> bool {
        self.provider_picker.is_some() || self.model_picker.is_some()
    }

    pub const fn has_auth_pool_picker(&self) -> bool {
        self.auth_pool_picker.is_some()
    }

    pub fn handle_auth_pool_picker_event(&mut self, event: &Event) -> AuthPoolPickerRootOutcome {
        let Some(picker) = self.auth_pool_picker.as_mut() else {
            return AuthPoolPickerRootOutcome::Continue;
        };
        let action = match event {
            Event::Key(stroke) => match stroke.key {
                bmux_keyboard::KeyCode::Escape => AuthPoolPickerRootOutcome::Cancel,
                bmux_keyboard::KeyCode::Enter => picker
                    .selected()
                    .map_or(AuthPoolPickerRootOutcome::Continue, |(pool, profile)| {
                        AuthPoolPickerRootOutcome::Promote { pool, profile }
                    }),
                bmux_keyboard::KeyCode::Char('c') if stroke.modifiers.is_empty() => picker
                    .selected_pool()
                    .map_or(AuthPoolPickerRootOutcome::Continue, |pool| {
                        AuthPoolPickerRootOutcome::Clear { pool }
                    }),
                bmux_keyboard::KeyCode::Up => {
                    picker.select_previous();
                    AuthPoolPickerRootOutcome::Continue
                }
                bmux_keyboard::KeyCode::Down => {
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
        if matches!(event, Event::Key(stroke) if stroke.key == bmux_keyboard::KeyCode::Enter)
            && let Some(model_id) = model.picker.selected_ignored_model_id()
        {
            model.picker.set_status(format!(
                "{model_id} is ignored; press u to remove state ignore or I to hide ignored models"
            ));
            return None;
        }
        let action = match event {
            Event::Paste(text) => {
                model.picker.focus_filter();
                let _ = super::text_input_flow::handle_paste(model.picker.filter_mut(), text);
                model.picker.refresh_filter();
                super::model_flow::ModelPickerAction::Continue
            }
            Event::Key(stroke) => super::model_flow::handle_model_picker_key(
                &mut model.picker,
                keymap,
                model.provider_plugin_id.as_deref(),
                *stroke,
            ),
            Event::Mouse(mouse) => super::picker_mouse::picker_row_from_mouse(*mouse)
                .filter(|row| model.picker.select_visible(*row))
                .and_then(|_| model.picker.selected_model_id())
                .map_or(super::model_flow::ModelPickerAction::Continue, |model_id| {
                    super::model_flow::ModelPickerAction::Select(model_id)
                }),
            Event::Focus(_) | Event::Resize(_) | Event::Tick | Event::User(_) => {
                super::model_flow::ModelPickerAction::Continue
            }
        };
        let provider = model.provider_plugin_id.clone();
        if !matches!(action, super::model_flow::ModelPickerAction::Continue) {
            self.model_picker = None;
        }
        Some((provider, action))
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
    ) -> TimelineDialogRootOutcome {
        let Some(dialog) = self.timeline_dialog.as_mut() else {
            return TimelineDialogRootOutcome::Unhandled;
        };
        match dialog.handle_key(stroke) {
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
    ) -> ThinkingDialogRootOutcome {
        let Some(dialog) = self.thinking_dialog.as_mut() else {
            return ThinkingDialogRootOutcome::Unhandled;
        };
        match dialog.handle_key(stroke) {
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
        self.request_draft_handoff.clear();
        self.markdown_projection.invalidate();
        self.artifact_stream.retain_session(session_id);
    }

    pub fn prepare_runtime_work(
        &mut self,
        chat: &mut ActiveChat,
        frame_area: Rect,
    ) -> super::invalidation::UiInvalidation {
        self.artifact_stream.start_due_fetches(Instant::now());
        self.request_latest_markdown_projection(chat, frame_area.width);
        record_artifact_stream_stats(self);
        let changed = chat
            .app
            .plugin_presentation()
            .is_some_and(crate::plugin_tui::PluginTuiPresentation::poll_dynamic_visuals)
            | maybe_start_older_history_load(chat, self)
            | maybe_start_newer_history_load(chat, self);
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

    pub const fn has_session_picker(&self) -> bool {
        self.session_picker.is_some()
    }

    pub fn open_session_picker(&mut self, chat: &mut ActiveChat) {
        self.open_session_picker_with_mode(chat, false);
    }

    pub fn open_session_search(&mut self, chat: &mut ActiveChat) {
        self.open_session_picker_with_mode(chat, true);
    }

    fn open_session_picker_with_mode(&mut self, chat: &mut ActiveChat, search: bool) {
        let mut picker = super::session_picker::SessionPickerApp::new(Vec::new());
        if search {
            picker.start_transcript_search();
        } else {
            picker.set_loading_status("Loading sessions…".to_owned());
        }
        self.session_picker = Some(picker);
        chat.replace_effect(TuiEffect::LoadSessionPicker);
        chat.app.set_status(if search {
            "session search".to_owned()
        } else {
            "session picker".to_owned()
        });
    }

    pub fn apply_session_picker_result(
        &mut self,
        result: Result<bcode_client::SessionList, bcode_client::ClientError>,
    ) {
        let Some(picker) = self.session_picker.as_mut() else {
            return;
        };
        match result {
            Ok(session_list) => {
                let count = session_list.sessions.len();
                picker.replace_sessions(session_list.sessions);
                picker.set_status(format!("{count} sessions"));
                picker.set_idle_empty_message();
            }
            Err(error) => picker.set_status(format!("Session catalog unavailable: {error}")),
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

    #[allow(clippy::too_many_lines)]
    pub fn handle_session_picker_event(
        &mut self,
        chat: &mut ActiveChat,
        keymap: &super::keymap::BmuxKeyMap,
        event: &Event,
    ) -> SessionPickerRootOutcome {
        let Some(picker) = self.session_picker.as_mut() else {
            return SessionPickerRootOutcome::Unhandled;
        };
        match event {
            Event::Paste(text) => {
                let _ = super::text_input_flow::handle_paste(picker.active_input_mut(), text);
                if picker.mode() == super::session_picker::SessionPickerMode::Filter {
                    picker.refresh_filter();
                }
            }
            Event::Key(stroke) => match picker.mode() {
                super::session_picker::SessionPickerMode::Filter => {
                    let search = *stroke
                        == bmux_keyboard::KeyStroke::with_modifiers(
                            bmux_keyboard::KeyCode::Char('f'),
                            bmux_keyboard::Modifiers {
                                ctrl: true,
                                ..bmux_keyboard::Modifiers::NONE
                            },
                        );
                    if search {
                        let query = picker.filter().buffer().text().trim().to_owned();
                        if query.is_empty() {
                            picker.set_status(
                                "Type a transcript query, then press Ctrl-F".to_owned(),
                            );
                        } else {
                            picker.start_transcript_search();
                            let Ok((request, policy)) = picker.search_request(&query, false) else {
                                picker.set_status(
                                    "Invalid search controls; press ? for syntax".to_owned(),
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
                    } else if super::text_input_flow::handle_key(
                        picker.filter_mut(),
                        keymap,
                        *stroke,
                    ) != bmux_tui_components::text_input::TextInputOutcome::Ignored
                    {
                        picker.refresh_filter();
                    }
                }
                super::session_picker::SessionPickerMode::Rename => {
                    if stroke.key == bmux_keyboard::KeyCode::Escape {
                        picker.cancel_rename();
                    } else if stroke.key == bmux_keyboard::KeyCode::Enter {
                        if let Some(session_id) = picker.selected_session_id() {
                            let name = picker.rename().buffer().text().trim();
                            let name = (!name.is_empty()).then(|| name.to_owned());
                            picker.finish_mutation("Renaming session…".to_owned());
                            chat.replace_effect(TuiEffect::RenameSession { session_id, name });
                        }
                    } else {
                        let _ = super::text_input_flow::handle_key(
                            picker.rename_mut(),
                            keymap,
                            *stroke,
                        );
                    }
                }
                super::session_picker::SessionPickerMode::DeleteConfirm => match stroke.key {
                    bmux_keyboard::KeyCode::Escape | bmux_keyboard::KeyCode::Char('n' | 'N') => {
                        picker.cancel_delete();
                    }
                    bmux_keyboard::KeyCode::Char('y' | 'Y') => {
                        if let Some(session_id) = picker.selected_session_id() {
                            picker.finish_mutation("Deleting session…".to_owned());
                            chat.replace_effect(TuiEffect::DeleteSession { session_id });
                        }
                    }
                    _ => {}
                },
                super::session_picker::SessionPickerMode::TranscriptSearch => match stroke.key {
                    bmux_keyboard::KeyCode::Escape => picker.close_search_results(),
                    bmux_keyboard::KeyCode::Enter => {
                        if let Some(hit) = picker.selected_search_result().cloned() {
                            self.session_picker = None;
                            return SessionPickerRootOutcome::SearchHit(hit);
                        }
                        let query = picker.filter().buffer().text().trim().to_owned();
                        if query.is_empty() {
                            picker
                                .set_status("Type a transcript query, then press Enter".to_owned());
                        } else {
                            let Ok((request, policy)) = picker.search_request(&query, false) else {
                                picker.set_status(
                                    "Invalid search controls; press ? for syntax".to_owned(),
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
                    bmux_keyboard::KeyCode::Char('m' | 'M') if stroke.modifiers.alt => {
                        picker.cycle_search_match_mode();
                    }
                    bmux_keyboard::KeyCode::Char('d' | 'D') if stroke.modifiers.alt => {
                        picker.toggle_search_depth();
                    }
                    bmux_keyboard::KeyCode::Char('s' | 'S') if stroke.modifiers.alt => {
                        picker.cycle_search_sort();
                    }
                    bmux_keyboard::KeyCode::Char('n' | 'N') if stroke.modifiers.alt => {
                        let query = picker.filter().buffer().text().trim().to_owned();
                        if query.is_empty() || !picker.has_next_search_page() {
                            picker.set_status("No next search page available".to_owned());
                        } else {
                            let Ok((request, policy)) = picker.search_request(&query, true) else {
                                picker.set_status(
                                    "Invalid search controls; press ? for syntax".to_owned(),
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
                    bmux_keyboard::KeyCode::Char('i' | 'I') if stroke.modifiers.alt => {
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
                    bmux_keyboard::KeyCode::Char('g' | 'G') if stroke.modifiers.alt => {
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
                                "Canonical migration is separate from indexing and creates verified backups. Press Alt-G again to confirm"
                                    .to_owned(),
                            );
                        }
                    }
                    bmux_keyboard::KeyCode::Char('b' | 'B') if stroke.modifiers.alt => {
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
                    bmux_keyboard::KeyCode::Char('x' | 'X') if stroke.modifiers.alt => {
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
                    bmux_keyboard::KeyCode::Char('?') => {
                        picker.set_status(super::session_search::SEARCH_CONTROL_HELP.to_owned());
                    }
                    bmux_keyboard::KeyCode::Up => picker.select_previous(),
                    bmux_keyboard::KeyCode::Down => picker.select_next(),
                    _ => {
                        let _ = super::text_input_flow::handle_key(
                            picker.filter_mut(),
                            keymap,
                            *stroke,
                        );
                    }
                },
            },
            Event::Mouse(mouse) => {
                if let Some(row) = super::picker_mouse::picker_row_from_mouse(*mouse) {
                    let _selected = picker.select_visible(row);
                }
            }
            Event::Focus(_) | Event::Resize(_) | Event::Tick | Event::User(_) => {}
        }
        SessionPickerRootOutcome::Handled
    }

    pub const fn has_interactive_surface(&self) -> bool {
        self.interactive_surface.is_some()
    }

    pub fn active_root_screen(&self) -> super::root_program::BcodeRuntimeScreen {
        use super::root_program::BcodeRuntimeScreen;

        if self.has_root_plugin_surface() && !self.root_plugin_surface_is_suspended() {
            return BcodeRuntimeScreen::PluginSurface;
        }
        if self.has_session_picker() {
            return BcodeRuntimeScreen::SessionPicker;
        }
        if self.has_ralph_start_dialog() {
            return BcodeRuntimeScreen::RalphStart;
        }
        if self.has_working_directory_dialog() {
            return BcodeRuntimeScreen::WorkingDirectory;
        }
        if self.has_worktree_create_dialog() {
            return BcodeRuntimeScreen::WorktreeCreate;
        }
        if self.provider_picker.is_some() || self.model_picker.is_some() {
            return BcodeRuntimeScreen::ModelPicker;
        }
        if self.has_skill_picker() {
            return BcodeRuntimeScreen::SkillPicker;
        }
        if self.has_command_palette() {
            return BcodeRuntimeScreen::CommandPalette;
        }
        if self.has_slash_palette() {
            return BcodeRuntimeScreen::SlashPalette;
        }
        if self.permission_dialog.is_some() {
            return BcodeRuntimeScreen::Permission;
        }
        if self.thinking_dialog.is_some() {
            return BcodeRuntimeScreen::Thinking;
        }
        if self.timeline_dialog.is_some() {
            return BcodeRuntimeScreen::Timeline;
        }
        if self.has_interactive_surface() {
            return BcodeRuntimeScreen::InteractiveSurface;
        }
        BcodeRuntimeScreen::Chat
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
            InteractiveSurfaceEventOutcome::Ignored | InteractiveSurfaceEventOutcome::Consumed => {
                None
            }
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

#[derive(Debug, Default)]
struct DaemonConnectionMonitor {
    saw_success: bool,
}

impl DaemonConnectionMonitor {
    const fn observe(
        &mut self,
        observation: &DaemonObservation,
    ) -> Option<super::app::DaemonConnectionState> {
        match observation {
            DaemonObservation::None | DaemonObservation::Failed(_) => None,
            DaemonObservation::Success => {
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
    frame_interval: Option<Duration>,
    runtime_config: bmux_tui_runtime::RuntimeConfig,
    metrics_enabled: bool,
    static_plugins: Vec<bcode_plugin::StaticBundledPlugin>,
    tui_extensions: Vec<bcode_plugin_sdk::tui::StaticPluginTuiExtension>,
    launch_working_directory: std::path::PathBuf,
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
            frame_interval: tui_config.render.frame_interval(),
            runtime_config: super::runtime_adapter::config(&tui_config),
            metrics_enabled: false,
            static_plugins: static_plugins.to_vec(),
            tui_extensions: super::bundled_tui_extensions(),
            launch_working_directory,
        }
    }

    pub fn apply_tui_config(&mut self, tui_config: &TuiConfig) {
        self.keymap = BmuxKeyMap::from_config(tui_config);
        self.mouse_scroll_rows = tui_config.mouse.effective_scroll_rows();
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
}

fn maybe_start_older_history_load(chat: &mut ActiveChat, _loop_state: &mut ChatLoopState) -> bool {
    if !chat.app.should_load_older_history() {
        return false;
    }
    let Some(cursor) = chat.app.older_history_cursor() else {
        return false;
    };
    let Some(session_id) = chat.session_id else {
        return false;
    };
    let started = !chat
        .pending_effects
        .contains_effect(&TuiEffect::LoadOlderHistory { session_id, cursor });
    chat.start_effect(TuiEffect::LoadOlderHistory { session_id, cursor });
    if started {
        chat.app.set_loading_older_history(true);
    }
    started
}

fn maybe_start_newer_history_load(chat: &mut ActiveChat, _loop_state: &mut ChatLoopState) -> bool {
    if !chat.app.should_load_newer_history() {
        return false;
    }
    let Some(cursor) = chat.app.newer_history_cursor() else {
        return false;
    };
    let Some(session_id) = chat.session_id else {
        return false;
    };
    let started = !chat
        .pending_effects
        .contains_effect(&TuiEffect::LoadNewerHistory { session_id, cursor });
    chat.start_effect(TuiEffect::LoadNewerHistory { session_id, cursor });
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

#[allow(clippy::too_many_lines)]
pub fn apply_effect_result(
    settings: &mut TuiRuntimeSettings,
    chat: &mut ActiveChat,
    draft_autosave: &mut DraftAutosave,
    loop_state: &mut ChatLoopState,
    result: TuiEffectResult,
) {
    match result {
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
            if let Ok((attached, _)) = &result {
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
            session_flow::complete_switch_session(chat, session_id, has_older_history, result);
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
        TuiEffectResult::SessionModelStatusLoaded { session_id, result } => {
            if chat.session_id == Some(session_id) {
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
            if chat.session_id == Some(session_id) {
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
        TuiEffectResult::OlderHistoryLoaded { session_id, result } => {
            apply_older_history_result(chat, session_id, result);
        }
        TuiEffectResult::NewerHistoryLoaded { session_id, result } => {
            apply_newer_history_result(chat, session_id, result);
        }
        TuiEffectResult::PermissionList { result } => {
            apply_permission_list_result(chat, loop_state, result);
        }
        TuiEffectResult::SaveDraft { text, result } => {
            apply_save_draft_result(chat, draft_autosave, text, result);
        }
        TuiEffectResult::SlashPaletteLoaded { query, palette } => {
            apply_slash_palette_result(chat, loop_state, &query, palette);
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
        TuiEffectResult::SessionPickerLoaded { result } => {
            loop_state.apply_session_picker_result(result);
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
            chat.replace_effect(TuiEffect::LoadSessionPicker);
        }
        TuiEffectResult::SessionDeleted { result } => {
            loop_state.apply_session_mutation_result("deleted", result);
            chat.replace_effect(TuiEffect::LoadSessionPicker);
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
                            "Derived search backfill started: {}. Press Alt-X to cancel",
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
        TuiEffectResult::ModelPickerLoaded {
            provider_plugin_id,
            result,
        } => match result {
            Ok(models) => {
                let status = provider_plugin_id.as_ref().map_or_else(
                    || "Select a model".to_owned(),
                    |provider| format!("Select a model from {provider}"),
                );
                loop_state.model_picker = Some(RootModelPicker {
                    provider_plugin_id,
                    picker: super::model_picker::ModelPickerApp::new_with_status(
                        models.models,
                        status,
                    ),
                });
                chat.app.set_status("select a model".to_owned());
            }
            Err(error) => report_nonfatal_client_error(chat, "Model list unavailable", &error),
        },
        TuiEffectResult::SkillPickerLoaded { result } => match result {
            Ok(skills) if skills.skills.is_empty() => {
                chat.app.set_status("no skills available".to_owned());
                chat.push_presentation_note(
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
                chat.push_presentation_note(
                    "bcode.host",
                    super::skill_flow::format_skill_manifest_markdown(&manifest),
                    bcode_command::CommandTextFormat::Markdown,
                );
                chat.app.set_status(format!("shown skill {skill_id}"));
            }
            Err(error) => report_nonfatal_client_error(chat, "Skill details unavailable", &error),
        },
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
        TuiEffectResult::TimelineJumpLoaded { sequence, result } => match result {
            Ok((events, has_older, has_newer)) => {
                chat.app
                    .replace_transcript_window(&events, has_older, has_newer, sequence);
                if chat.app.transcript_index_for_sequence(sequence).is_some() {
                    chat.app.request_transcript_top_anchor_sequence(sequence);
                    chat.app.set_status("jumped to timeline message".to_owned());
                } else {
                    chat.app.set_status(format!(
                        "timeline message seq {sequence} was not in the loaded window"
                    ));
                }
            }
            Err(error) => report_nonfatal_tui_error(chat, "Timeline jump unavailable", &error),
        },
        TuiEffectResult::PluginCommandInvoked { plugin_id, result } => match result {
            Ok(response) => {
                if let Some(message) = response.message {
                    chat.app.set_status(message);
                }
                for effect in response.effects {
                    match effect {
                        bcode_command::CommandEffect::Status { message } => {
                            chat.app.set_status(message);
                        }
                        bcode_command::CommandEffect::AppendText { text, format } => {
                            chat.push_presentation_note(plugin_id.clone(), text, format);
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
                                session_id: chat.session_id,
                            });
                            chat.app.set_status("opening plugin surface…".to_owned());
                        }
                    }
                }
            }
            Err(error) => report_nonfatal_tui_error(chat, "Plugin command failed", &error),
        },
        TuiEffectResult::SubmitMessage { message, result } => {
            apply_submit_message_result(chat, &message, *result);
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
                Ok(presentation) => chat.app.set_plugin_presentation(Arc::new(presentation)),
                Err(error) => chat
                    .app
                    .set_status(format!("plugin presentation unavailable: {error}")),
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
            if chat.session_id.is_none() && chat.opening_session_id.is_none() {
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
    model: Option<bcode_ipc::SessionModelStatus>,
    composer_draft: Option<String>,
    error: Option<String>,
) {
    if chat.session_id.is_some() || chat.opening_session_id.is_some() {
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
    if chat.session_id != Some(session_id) {
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

fn apply_older_history_result(
    chat: &mut ActiveChat,
    session_id: bcode_session_models::SessionId,
    result: Result<bcode_session_models::SessionHistoryPage, ClientError>,
) {
    match result {
        Ok(page) if Some(session_id) == chat.session_id => {
            chat.app.prepend_older_history(&page.events, page.has_more);
        }
        Ok(_stale) => {}
        Err(error) => {
            if Some(session_id) == chat.session_id {
                chat.app.set_loading_older_history(false);
            }
            report_nonfatal_client_error(chat, "Older history unavailable", &error);
        }
    }
}

fn apply_newer_history_result(
    chat: &mut ActiveChat,
    session_id: bcode_session_models::SessionId,
    result: Result<bcode_session_models::SessionHistoryPage, ClientError>,
) {
    match result {
        Ok(page) if Some(session_id) == chat.session_id => {
            chat.app.append_newer_history(&page.events, page.has_more);
        }
        Ok(_stale) => {}
        Err(error) => {
            if Some(session_id) == chat.session_id {
                chat.app.set_loading_newer_history(false);
            }
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

fn apply_permission_list_result(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    result: Result<Vec<bcode_ipc::PermissionSummary>, ClientError>,
) {
    match result {
        Ok(permissions) => {
            let active_permissions = permissions
                .iter()
                .filter(|permission| Some(permission.session_id) == chat.session_id)
                .cloned()
                .map(permission_summary_view)
                .collect::<Vec<_>>();
            chat.app
                .set_pending_permission_views(active_permissions.clone());
            if loop_state.permission_dialog.is_none()
                && let Some(permission) = active_permissions.into_iter().next()
            {
                loop_state.permission_dialog = Some(PermissionDialogState::new(permission));
            }
        }
        Err(_error) => {}
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

fn apply_slash_palette_result(
    chat: &ActiveChat,
    loop_state: &mut ChatLoopState,
    query: &str,
    mut palette: slash_palette::SlashPalette,
) {
    if query != chat.app.composer().text() {
        return;
    }
    if let Some(previous) = loop_state
        .slash_palette
        .as_ref()
        .filter(|current| current.query() == query)
        .and_then(|current| current.selected_command().map(str::to_owned))
    {
        palette.select_command(&previous);
    }
    loop_state.slash_palette = (!palette.is_empty()).then_some(palette);
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
        SlashCommandOutcome::SystemMarkdown(text) => chat.push_presentation_note(
            "bcode.host",
            text,
            bcode_command::CommandTextFormat::Markdown,
        ),
        SlashCommandOutcome::SystemPlain(text) => chat.push_presentation_note(
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
            arguments,
        } => match action {
            bcode_command::CommandAction::Plugin {
                plugin_id,
                command_id,
            } => {
                let working_directory = chat.app.working_directory().map_or_else(
                    || settings.launch_working_directory().to_path_buf(),
                    std::path::Path::to_path_buf,
                );
                chat.start_effect(TuiEffect::InvokePluginCommand {
                    plugin_id,
                    command_id,
                    arguments: Some(arguments),
                    working_directory,
                    session_id: chat.session_id,
                });
            }
            bcode_command::CommandAction::Host { route } => chat
                .app
                .set_status(format!("host slash route pending root navigation: {route}")),
        },
        SlashCommandOutcome::OpenTimeline => {
            let entries = if chat.session_id.is_some() {
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
                session_id: chat.session_id,
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
                    session_id: chat.session_id,
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
                    agent_id: chat.app.pending_agent_id().map(ToOwned::to_owned),
                    reasoning_effort: chat.app.reasoning_effort().map(ToOwned::to_owned),
                    reasoning_summary: chat.app.reasoning_summary().map(ToOwned::to_owned),
                    reasoning_effort_generation: chat.app.pending_reasoning_effort_generation(),
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
            chat.push_presentation_note(
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
                session_id: chat.session_id,
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
    message: &str,
    result: Result<super::effects::SubmitMessageResult, ClientError>,
) {
    match result {
        Ok(result) => {
            chat.session_id = Some(result.session_id);
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
            if result.committed_agent_id.is_some() {
                let _committed = chat.app.take_pending_agent();
            }
            if let Some(generation) = result.committed_reasoning_effort_generation {
                chat.app.clear_pending_reasoning_effort(generation);
            }
            if let Some(release) = result.event_stream_release {
                let _released = release.send(());
            }
            match result.acceptance.disposition {
                bcode_ipc::MessageAcceptanceDisposition::AppliedSteering => {
                    chat.app.mark_pending_submission_sent();
                    chat.app.set_status("Steering sent".to_owned());
                }
                bcode_ipc::MessageAcceptanceDisposition::QueuedFollowUp
                | bcode_ipc::MessageAcceptanceDisposition::QueuedTurn => {
                    chat.app.set_idle();
                    chat.app
                        .mark_pending_submission_queued(result.acceptance.queue_position);
                    chat.app.set_status(format!(
                        "Message queued{}",
                        result
                            .acceptance
                            .queue_position
                            .map_or_else(String::new, |position| format!(" at #{position}"))
                    ));
                }
                bcode_ipc::MessageAcceptanceDisposition::StartedTurn => {
                    chat.app.mark_pending_submission_sent();
                    chat.app.set_status("Message sent".to_owned());
                }
            }
            ensure_session_stream_after_foreground_wake(chat);
        }
        Err(error) => {
            chat.app.restore_pending_submission(message);
            daemon_issue::report_client_issue(&mut chat.app, "send failed", &error);
        }
    }
}

fn ensure_session_stream_after_foreground_wake(chat: &mut ActiveChat) {
    let Some(session_id) = chat.session_id else {
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
            chat.session_id = Some(result.session_id);
            if let Some(session) = result.created_session {
                chat.app.apply_session_summary(&session);
            }
            if let Some(event_task) = result.event_task
                && let Some(previous_task) = chat.event_task.replace(event_task)
            {
                previous_task.abort();
            }
            if result.committed_agent_id.is_some() {
                let _committed = chat.app.take_pending_agent();
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
    if chat.session_id != Some(session_id) {
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
    if chat.session_id != Some(session_id) {
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
    if chat.session_id != Some(session_id) {
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
    result: Result<bcode_worktree_models::WorktreeCreateResponse, ClientError>,
) {
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            daemon_issue::report_client_issue(&mut chat.app, "worktree create failed", &error);
            return;
        }
    };
    let path = response.path.clone();
    if let Some(session) = response.session {
        let session_id = session.id;
        chat.app.apply_session_summary(&session);
        chat.session_id = Some(session_id);
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
    let current_query = chat.app.composer().text();
    if !current_query.starts_with('/') {
        loop_state.slash_palette = None;
        ChatLoopState::queue_effect_cancellation(
            chat,
            TuiEffect::LoadSlashPalette {
                query: String::new(),
                session_id: None,
            },
        );
        return true;
    }
    let query = current_query.to_owned();
    let previous = loop_state
        .slash_palette
        .as_ref()
        .filter(|palette| palette.query() == current_query)
        .and_then(|palette| palette.selected_command().map(str::to_owned));
    if previous.is_none() {
        loop_state.slash_palette = None;
    }
    chat.replace_effect(TuiEffect::LoadSlashPalette {
        query,
        session_id: chat.app.session_id(),
    });
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

fn write_markdown_fallback(frame: &mut bmux_tui::frame::Frame<'_>, area: Rect, fallback: &str) {
    if area.is_empty() {
        return;
    }
    let width = usize::from(area.width);
    let lines = fallback.lines().collect::<Vec<_>>();
    for row in 0..area.height {
        let text = lines.get(usize::from(row)).copied().unwrap_or_default();
        let text = bmux_tui::text_width::truncate_to_display_width(text, width);
        frame.write_line(
            Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
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
) -> Result<(bmux_tui::terminal::DrawStats, Option<render::FrameLayout>), TuiError> {
    let frame_started = Instant::now();
    let prepare_started = frame_started;
    if fast_temporal_presentation && let Some(layout) = committed_layout {
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
    let draw_stats = terminal.draw_damage(damage, |frame| {
        if let Some(layout) = layout {
            render::render_prepared_damage(&mut chat.app, frame, layout, intersects);
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
        if layout.is_some_and(|layout| intersects(layout.body)) {
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
        if let Some(slash_palette) = &loop_state.slash_palette {
            slash_palette_render::render_palette(
                slash_palette,
                chat.app.composer_content_area(),
                frame,
                theme,
            );
        }
        if let Some(palette) = &mut loop_state.palette {
            command_palette_render::render_palette(palette, frame, theme);
        }
        if let Some(configurator) = &mut loop_state.streaming_configurator {
            let geometry = super::streaming_configurator_render::streaming_configurator_geometry(
                configurator,
                frame.area(),
                theme,
            );
            configurator.commit_geometry(geometry);
            super::streaming_configurator_render::render_streaming_configurator(
                configurator,
                frame,
                theme,
            );
        }
        if let Some(picker) = &mut loop_state.theme_picker {
            super::theme_picker_render::render_theme_picker(picker, frame, theme);
        }
        if let Some(dialog) = &loop_state.permission_dialog {
            permission_dialog_render::render_permission_dialog(dialog, frame, theme);
        }
        if let Some(dialog) = &loop_state.thinking_dialog {
            thinking_dialog_render::render_thinking_dialog(dialog, frame, theme);
        }
        if let Some(dialog) = &mut loop_state.timeline_dialog {
            timeline_dialog_render::render_timeline_dialog(dialog, frame, theme);
        }
        if let Some(surface) = &mut loop_state.interactive_surface
            && !surface_area.is_empty()
        {
            if pinned {
                frame.fill(surface_area, " ", bmux_tui::prelude::Style::new());
                surface.render_slice(
                    interaction_rows.unwrap_or(surface_area.height),
                    pinned_offset,
                    surface_area,
                    frame,
                );
            } else if let Some(placement) = inline_placement {
                surface.render_slice(
                    placement.full_area.height,
                    placement.visible_content_offset,
                    placement.destination,
                    frame,
                );
            }
        }
        if let Some(picker) = &mut loop_state.session_picker {
            super::session_picker_render::render_picker(picker, frame, theme);
        }
        if let Some(surface) = &mut loop_state.plugin_surface {
            let area = frame.area();
            surface.surface.render_with_theme(
                area,
                frame,
                Some(render::plugin_theme_for_app(&chat.app)),
            );
        }
        if let Some(picker) = &mut loop_state.provider_picker {
            super::provider_picker_render::render_provider_picker(picker, frame, theme);
        }
        if let Some(model) = &mut loop_state.model_picker {
            super::model_picker_render::render_model_picker(&mut model.picker, frame, theme);
        }
        if let Some(picker) = &mut loop_state.auth_pool_picker {
            super::auth_pool_picker_render::render_auth_pool_picker(picker, frame, theme);
        }
        if let Some(picker) = &mut loop_state.skill_picker {
            super::skill_picker_render::render_skill_picker(picker, frame, theme);
        }
        if let Some(dialog) = &mut loop_state.ralph_start_dialog {
            super::ralph_start_dialog_render::render_dialog(dialog, frame, theme);
        }
        if let Some(dialog) = &mut loop_state.working_directory_dialog {
            super::working_directory_dialog_render::render_dialog(dialog, frame, theme);
        }
        if let Some(dialog) = &mut loop_state.worktree_create_dialog {
            super::wt_create_dialog_render::render_dialog(dialog, frame, theme);
        }
    })?;
    loop_state.interactive_surface_geometry = loop_state.interactive_surface.as_ref().map(|_| {
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
    loop_state.telemetry.record_histogram(
        "tui.frame.changed_cells",
        u64::try_from(draw_stats.changed_cells).unwrap_or(u64::MAX),
    );
    if draw_stats.full_repaint {
        loop_state
            .telemetry
            .add_counter("tui.frame.full_repaint_total", 1);
    }
    loop_state
        .markdown_image_compositor
        .apply_delta(terminal.image_delta());
    let terminal_area = terminal.area();
    let image_scene = terminal.image_scene().clone();
    loop_state.markdown_image_compositor.render(
        terminal.writer_mut(),
        &image_scene,
        bmux_image::compositor::PaneRect {
            x: terminal_area.x,
            y: terminal_area.y,
            w: terminal_area.width,
            h: terminal_area.height,
        },
        &loop_state.markdown_image_capabilities,
        &loop_state.markdown_image_config,
    )?;
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

fn draw_temporal_frame<W: Write>(
    terminal: &mut Terminal<&mut W>,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    damage: bmux_tui::damage::Damage,
    frame_interval: Option<Duration>,
    frame_started: Instant,
    layout: render::FrameLayout,
) -> Result<(bmux_tui::terminal::DrawStats, Option<render::FrameLayout>), TuiError> {
    let regions = damage.retained_regions().to_vec();
    let intersects = |area: Rect| {
        regions
            .iter()
            .any(|region| !area.intersection(*region).is_empty())
    };
    let draw_started = Instant::now();
    let draw_stats = terminal.draw_damage(damage, |frame| {
        render::render_prepared_damage(&mut chat.app, frame, layout, intersects);
    })?;
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
    loop_state
        .artifact_stream
        .handle_completion(chat.session_id, completion, |chunk| {
            presentation.map_or_else(
                || Err("plugin presentation unavailable".to_owned()),
                |presentation| presentation.deliver_artifact_chunk(chunk),
            )
        })
}

fn absorb_session_stream_update(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    update: history_flow::SessionStreamUpdate,
) -> bool {
    match update {
        history_flow::SessionStreamUpdate::Event(event) => {
            absorb_bcode_event(chat, loop_state, *event)
        }
        history_flow::SessionStreamUpdate::ResyncStarted { session_id }
            if chat.session_id == Some(session_id) =>
        {
            loop_state
                .telemetry
                .add_counter("tui.session_view.resync_started_total", 1);
            chat.app.set_status("Reconnecting session view…".to_owned());
            true
        }
        history_flow::SessionStreamUpdate::Resynchronized {
            session_id,
            attached,
        } if chat.session_id == Some(session_id) => {
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
    chat.app.apply_session_summary(&attached.session);
    chat.app
        .apply_runtime_selection(attached.runtime_selection.clone());

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
        BcodeEvent::Session(event) if Some(event.session_id) == chat.session_id => {
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
        BcodeEvent::SessionLive(event) if Some(event.session_id) == chat.session_id => {
            absorb_session_live_event(&mut chat.app, &mut loop_state.artifact_stream, &event);
            true
        }
        BcodeEvent::RuntimeWork(event) if Some(event.session_id) == chat.session_id => {
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
    feature = "static-bundled-ralph-plugin",
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
    feature = "static-bundled-ralph-plugin",
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
    feature = "static-bundled-ralph-plugin",
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
    feature = "static-bundled-ralph-plugin",
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
    if chat.opening_session_id != Some(snapshot.session_id) {
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

    struct PassiveSurface;

    impl bcode_plugin_sdk::tui::PluginTuiSurface for PassiveSurface {
        fn id(&self) -> &'static str {
            "passive-test"
        }

        fn title(&self) -> &'static str {
            "Passive test"
        }

        fn render(&mut self, _area: Rect, _frame: &mut bmux_tui::frame::Frame<'_>) {}

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
