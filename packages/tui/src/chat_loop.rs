//! Main chat event loop for the TUI.

use bcode_plugin_sdk::path::display_from_current_dir;
use std::collections::BTreeSet;
use std::io::Write;
use std::time::{Duration, Instant, SystemTime};

use bcode_client::{BcodeClient, ClientError, DaemonAvailability};
use bcode_config::TuiConfig;
use bcode_ipc::{ComposerDraftScope, Event as BcodeEvent};
use bcode_plugin::PluginRuntimeHost;
use bcode_session_models::SessionEventKind;
use bcode_session_view::execute_session_view_action;
use bcode_session_view_models::{SessionViewAction, SessionViewActionOutcome};
use bmux_keyboard::KeyStroke;
use bmux_tui::event::{Event, FocusEvent, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;

use super::activity::ActivityState;
use super::artifact_stream::{ActiveArtifactFetchCompletion, ArtifactStreamCoordinator};
use super::clipboard_image;
use super::command_palette::BmuxCommandPalette;
use super::daemon_host::TuiDaemonHost;
use super::daemon_issue;
use super::effects::{DaemonObservation, TuiEffect, TuiEffectResult, TuiEffectRunner};
use super::helpers;
use super::interactive_surface::{
    InteractiveSurfaceQueue, InteractiveSurfaceRequest, InteractiveSurfaceState,
};
use super::invalidation::InvalidationQueue;
use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use super::permission_dialog::PermissionDialogState;
use super::runtime_context::{TuiIo, TuiServices};
use super::session_flow::{self, ActiveChat};
use super::terminal_events::TuiInput;
use super::{
    TuiError, command_palette_render, composer_flow, input, input::KeyRequest, mouse_flow,
    palette_flow, permission_dialog_render, permission_flow, render, slash_flow, slash_palette,
    slash_palette_render, thinking_dialog_render, thinking_flow, timeline_dialog_render,
    timeline_flow,
};

const TARGET_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const BCODE_EVENT_DRAIN_BUDGET: usize = 32;
const ARTIFACT_COMPLETION_DRAIN_BUDGET: usize = 8;
const DRAFT_SAVE_DEBOUNCE: Duration = Duration::from_millis(900);
#[derive(Debug, Clone)]
struct DraftAutosave {
    launch_working_directory: std::path::PathBuf,
    last_seen_text: String,
    last_saved_text: Option<String>,
    dirty: bool,
    save_at: Option<Instant>,
}

impl DraftAutosave {
    fn new(launch_working_directory: std::path::PathBuf, initial_text: String) -> Self {
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

    fn observe(&mut self, chat: &ActiveChat, now: Instant) {
        let text = chat.app.composer().text();
        if text == self.last_seen_text {
            return;
        }
        text.clone_into(&mut self.last_seen_text);
        self.dirty = true;
        self.save_at = Some(now + DRAFT_SAVE_DEBOUNCE);
    }

    fn next_save_at(&self) -> Option<Instant> {
        self.dirty.then_some(self.save_at).flatten()
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

    const fn clear_scope_request(scope: ComposerDraftScope) -> (ComposerDraftScope, String) {
        (scope, String::new())
    }

    fn mark_dirty_now(&mut self) {
        self.dirty = true;
        self.save_at = Some(Instant::now());
    }
}

struct ChatLoopState {
    palette: Option<BmuxCommandPalette>,
    slash_palette: Option<slash_palette::SlashPalette>,
    effects: TuiEffectRunner,
    daemon_connection: DaemonConnectionMonitor,
    permission_dialog: Option<PermissionDialogState>,
    thinking_dialog: Option<super::thinking_dialog::ThinkingDialogState>,
    timeline_dialog: Option<super::timeline_dialog::TimelineDialogState>,
    interactive_surface: Option<InteractiveSurfaceState>,
    interactive_surface_queue: InteractiveSurfaceQueue,
    plugin_runtime: Option<PluginRuntimeHost>,
    artifact_stream: ArtifactStreamCoordinator,
    telemetry: super::telemetry::TuiTelemetry,
    frame_index: u64,
}

impl ChatLoopState {
    fn new(
        foreground_client: &BcodeClient,
        passive_client: &BcodeClient,
        daemon_host: TuiDaemonHost,
        metrics_enabled: bool,
    ) -> Self {
        Self {
            palette: None,
            slash_palette: None,
            effects: TuiEffectRunner::new(foreground_client, passive_client, daemon_host),
            daemon_connection: DaemonConnectionMonitor::default(),
            permission_dialog: None,
            thinking_dialog: None,
            timeline_dialog: None,
            interactive_surface: None,
            interactive_surface_queue: InteractiveSurfaceQueue::default(),
            plugin_runtime: None,
            artifact_stream: ArtifactStreamCoordinator::new(passive_client.clone()),
            telemetry: super::telemetry::TuiTelemetry::new(passive_client.clone(), metrics_enabled),
            frame_index: 0,
        }
    }

    fn drain_pending_effects(&mut self, chat: &mut ActiveChat) -> bool {
        self.effects.drain_pending(&mut chat.pending_effects)
    }

    async fn poll_finished_effects(&mut self) -> Vec<TuiEffectResult> {
        self.effects.poll_finished().await
    }

    fn abort_all_effects(&mut self) {
        self.effects.abort_all();
    }

    fn start_effect(&mut self, effect: TuiEffect) -> bool {
        self.effects.start(effect)
    }

    fn replace_effect(&mut self, effect: TuiEffect) {
        self.effects.replace(effect);
    }

    fn abort_matching_effect(&mut self, effect: &TuiEffect) {
        self.effects.abort_matching(effect);
    }

    fn observe_daemon(&mut self, chat: &mut ActiveChat, observation: DaemonObservation) {
        if let Some(state) = self.daemon_connection.observe(observation) {
            chat.app.set_daemon_connection(state);
        }
    }
}

#[derive(Debug, Default)]
struct DaemonConnectionMonitor {
    saw_success: bool,
    last_error: Option<String>,
}

impl DaemonConnectionMonitor {
    fn observe(
        &mut self,
        observation: DaemonObservation,
    ) -> Option<super::app::DaemonConnectionState> {
        match observation {
            DaemonObservation::None => None,
            DaemonObservation::Success => {
                self.saw_success = true;
                self.last_error = None;
                Some(super::app::DaemonConnectionState::Connected)
            }
            DaemonObservation::Unavailable(error) | DaemonObservation::Failed(error) => {
                self.last_error = Some(error);
                Some(if self.saw_success {
                    super::app::DaemonConnectionState::IdleOffline
                } else {
                    super::app::DaemonConnectionState::Unavailable
                })
            }
        }
    }
}

pub struct TuiRuntimeSettings {
    keymap: BmuxKeyMap,
    mouse_scroll_rows: usize,
    metrics_enabled: bool,
    launch_working_directory: std::path::PathBuf,
}

impl TuiRuntimeSettings {
    pub fn bootstrap(launch_working_directory: std::path::PathBuf) -> Self {
        let tui_config = TuiConfig::default();
        Self {
            keymap: BmuxKeyMap::from_config(&tui_config),
            mouse_scroll_rows: tui_config.mouse.effective_scroll_rows(),
            metrics_enabled: false,
            launch_working_directory,
        }
    }

    pub fn apply_tui_config(&mut self, tui_config: &TuiConfig) {
        self.keymap = BmuxKeyMap::from_config(tui_config);
        self.mouse_scroll_rows = tui_config.mouse.effective_scroll_rows();
    }

    pub const fn set_metrics_enabled(&mut self, enabled: bool) {
        self.metrics_enabled = enabled;
    }

    pub fn launch_working_directory(&self) -> &std::path::Path {
        &self.launch_working_directory
    }
}

struct ChatEventContext<'a, 'b, W: Write> {
    services: TuiServices<'a>,
    terminal: &'a mut Terminal<&'b mut W>,
    terminal_events: &'a mut TuiInput,
    mouse_scroll_rows: usize,
}

impl<'a, 'b, W: Write> ChatEventContext<'a, 'b, W> {
    const fn flow_context(&mut self) -> (TuiIo<'_, 'b, W>, TuiServices<'a>) {
        let services = self.services;
        let io = TuiIo {
            terminal: self.terminal,
            input: self.terminal_events,
        };
        (io, services)
    }
}

/// Run the active chat UI loop.
#[allow(clippy::future_not_send)]
#[allow(clippy::too_many_lines)]
pub async fn run_with_client<W: Write>(
    terminal: &mut Terminal<&mut W>,
    terminal_events: &mut TuiInput,
    client: &BcodeClient,
    settings: &mut TuiRuntimeSettings,
    chat: &mut ActiveChat,
    startup_action: super::startup_action::StartupTuiAction,
    daemon_host: TuiDaemonHost,
) -> Result<(), TuiError> {
    let passive_client = client
        .clone()
        .with_daemon_availability(DaemonAvailability::RequireRunning);
    let mut loop_state = ChatLoopState::new(
        client,
        &passive_client,
        daemon_host,
        settings.metrics_enabled,
    );
    let result = run_chat_loop(
        terminal,
        terminal_events,
        client,
        &passive_client,
        settings,
        chat,
        startup_action,
        &mut loop_state,
    )
    .await;
    loop_state.abort_all_effects();
    result
}

#[allow(
    clippy::future_not_send,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]
async fn run_chat_loop<W: Write>(
    terminal: &mut Terminal<&mut W>,
    terminal_events: &mut TuiInput,
    client: &BcodeClient,
    passive_client: &BcodeClient,
    settings: &mut TuiRuntimeSettings,
    chat: &mut ActiveChat,
    startup_action: super::startup_action::StartupTuiAction,
    loop_state: &mut ChatLoopState,
) -> Result<(), TuiError> {
    loop_state.drain_pending_effects(chat);
    sync_chat_key_labels(chat, &settings.keymap);
    let mut draft_autosave = DraftAutosave::new(
        settings.launch_working_directory.clone(),
        chat.app.composer().text().to_owned(),
    );
    let mut invalidation_queue = InvalidationQueue::default();
    refresh_invalidation_queue(chat, &mut invalidation_queue);
    let mut needs_redraw = true;
    let mut last_redraw = Instant::now()
        .checked_sub(TARGET_FRAME_INTERVAL)
        .unwrap_or_else(Instant::now);

    let mut startup_action = Some(startup_action);

    while !chat.app.should_exit() {
        sync_chat_key_labels(chat, &settings.keymap);
        loop_state.telemetry.flush_if_due(Instant::now());
        record_artifact_stream_stats(loop_state);
        if drain_artifact_completions(chat, loop_state, ARTIFACT_COMPLETION_DRAIN_BUDGET) {
            needs_redraw = true;
        }
        if drain_bcode_events(chat, loop_state, BCODE_EVENT_DRAIN_BUDGET) {
            needs_redraw = true;
        }
        if drain_artifact_completions(chat, loop_state, ARTIFACT_COMPLETION_DRAIN_BUDGET) {
            needs_redraw = true;
        }

        if handle_loop_housekeeping(settings, chat, &mut draft_autosave, loop_state).await {
            needs_redraw = true;
        }

        if helpers::resize_from_terminal(terminal)? {
            needs_redraw = true;
        }

        draft_autosave.observe(chat, Instant::now());
        if let Some(save_at) = draft_autosave.next_save_at()
            && Instant::now() >= save_at
        {
            start_draft_save(chat, &mut draft_autosave);
        }

        let redraw_at = next_redraw_at(last_redraw);
        if needs_redraw && Instant::now() >= redraw_at {
            let schedule_delay = Instant::now().saturating_duration_since(redraw_at);
            draw_chat_frame(terminal, chat, loop_state, schedule_delay)?;
            if let Some(action) = startup_action.take()
                && action == super::startup_action::StartupTuiAction::OpenRalphHome
            {
                let mut io = TuiIo {
                    terminal,
                    input: terminal_events,
                };
                let services = TuiServices {
                    client,
                    passive_client,
                    launch_working_directory: settings.launch_working_directory(),
                    keymap: &settings.keymap,
                    theme: render::TuiTheme::for_app(&chat.app),
                };
                if let Err(error) = super::ralph_flow::open_home(&mut io, &services, chat).await {
                    if daemon_issue::is_nonfatal_tui_error(&error) {
                        daemon_issue::report_tui_issue(&mut chat.app, "Ralph unavailable", &error);
                    } else {
                        return Err(error);
                    }
                }
            }
            refresh_invalidation_queue(chat, &mut invalidation_queue);
            needs_redraw = false;
            last_redraw = Instant::now();
        }

        let event = next_chat_loop_event(
            terminal_events,
            &mut invalidation_queue,
            chat,
            &mut loop_state.artifact_stream,
            ChatLoopDeadlines {
                interaction_retry: loop_state.interactive_surface_queue.next_retry_at(),
                telemetry_flush: loop_state.telemetry.next_flush_at(),
                redraw: needs_redraw.then_some(redraw_at),
                draft_save: draft_autosave.next_save_at(),
            },
        )
        .await?;
        let before_session_id = chat.session_id;
        match event {
            ChatLoopEvent::Terminal(event) => {
                let event_invalidation = if matches!(event, Event::Resize(_)) {
                    super::invalidation::UiInvalidation::Full
                } else {
                    super::invalidation::UiInvalidation::Layout
                };
                let mut context = ChatEventContext {
                    services: TuiServices {
                        client,
                        passive_client,
                        launch_working_directory: settings.launch_working_directory(),
                        keymap: &settings.keymap,
                        theme: render::TuiTheme::for_app(&chat.app),
                    },
                    terminal,
                    terminal_events,
                    mouse_scroll_rows: settings.mouse_scroll_rows,
                };
                match handle_event(&mut context, chat, loop_state, event, &mut draft_autosave).await
                {
                    Ok(handled) => {
                        if handled {
                            needs_redraw = event_invalidation.needs_render();
                        }
                    }
                    Err(error) if is_nonfatal_tui_daemon_error(&error) => {
                        report_nonfatal_tui_error(chat, "Daemon unavailable", &error);
                        needs_redraw = true;
                    }
                    Err(error) => return Err(error),
                }
            }
            ChatLoopEvent::Bcode(event) => {
                if absorb_bcode_event(chat, loop_state, *event)
                    || drain_bcode_events(
                        chat,
                        loop_state,
                        BCODE_EVENT_DRAIN_BUDGET.saturating_sub(1),
                    )
                {
                    needs_redraw = true;
                }
                if drain_artifact_completions(chat, loop_state, ARTIFACT_COMPLETION_DRAIN_BUDGET) {
                    needs_redraw = true;
                }
            }
            ChatLoopEvent::ArtifactFetchCompleted(completion) => {
                needs_redraw |= handle_artifact_completion(chat, loop_state, *completion);
            }
            ChatLoopEvent::TimedInvalidations(keys) => {
                if chat
                    .app
                    .handle_invalidations(&keys, Instant::now())
                    .needs_render()
                {
                    needs_redraw = true;
                }
            }
            ChatLoopEvent::Timer => {}
        }
        if before_session_id != chat.session_id {
            loop_state.artifact_stream.retain_session(chat.session_id);
            draft_autosave.last_saved_text = None;
            draft_autosave.dirty = true;
            draft_autosave.save_at = Some(Instant::now());
        }
    }

    Ok(())
}

enum ChatLoopEvent {
    Terminal(Event),
    Bcode(Box<BcodeEvent>),
    ArtifactFetchCompleted(Box<ActiveArtifactFetchCompletion>),
    TimedInvalidations(Vec<super::invalidation::InvalidationKey>),
    Timer,
}

async fn handle_loop_housekeeping(
    settings: &mut TuiRuntimeSettings,
    chat: &mut ActiveChat,
    draft_autosave: &mut DraftAutosave,
    loop_state: &mut ChatLoopState,
) -> bool {
    let mut needs_redraw = false;
    needs_redraw |= poll_finished_effects(settings, chat, draft_autosave, loop_state).await;
    needs_redraw |= loop_state.drain_pending_effects(chat);
    needs_redraw |= maybe_start_interactive_surface(chat, loop_state).await;
    loop_state.artifact_stream.start_due_fetches(Instant::now());
    needs_redraw |= maybe_start_older_history_load(chat, loop_state);
    needs_redraw |= maybe_start_newer_history_load(chat, loop_state);
    needs_redraw
}

fn maybe_start_older_history_load(chat: &mut ActiveChat, loop_state: &mut ChatLoopState) -> bool {
    if !chat.app.should_load_older_history() {
        return false;
    }
    let Some(cursor) = chat.app.older_history_cursor() else {
        return false;
    };
    let Some(session_id) = chat.session_id else {
        return false;
    };
    let started = loop_state.start_effect(TuiEffect::LoadOlderHistory { session_id, cursor });
    if started {
        chat.app.set_loading_older_history(true);
    }
    started
}

fn maybe_start_newer_history_load(chat: &mut ActiveChat, loop_state: &mut ChatLoopState) -> bool {
    if !chat.app.should_load_newer_history() {
        return false;
    }
    let Some(cursor) = chat.app.newer_history_cursor() else {
        return false;
    };
    let Some(session_id) = chat.session_id else {
        return false;
    };
    let started = loop_state.start_effect(TuiEffect::LoadNewerHistory { session_id, cursor });
    if started {
        chat.app.set_loading_newer_history(true);
    }
    started
}

async fn poll_finished_effects(
    settings: &mut TuiRuntimeSettings,
    chat: &mut ActiveChat,
    draft_autosave: &mut DraftAutosave,
    loop_state: &mut ChatLoopState,
) -> bool {
    let results = loop_state.poll_finished_effects().await;
    let needs_redraw = !results.is_empty();
    for result in results {
        loop_state.observe_daemon(chat, result.daemon_observation());
        apply_effect_result(settings, chat, draft_autosave, loop_state, result);
    }
    needs_redraw
}

#[allow(clippy::too_many_lines)]
fn apply_effect_result(
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
        TuiEffectResult::ConfigLoaded { config } => {
            apply_config_result(settings, chat, loop_state, *config);
        }
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
        TuiEffectResult::SubmitMessage { message, result } => {
            apply_submit_message_result(chat, &message, *result);
        }
        TuiEffectResult::RenameSession { result } => {
            apply_rename_session_result(chat, result);
        }
        TuiEffectResult::DeleteSession { session_id, result } => {
            apply_delete_session_result(chat, session_id, result);
        }
        TuiEffectResult::ForkSession {
            switch_after_create,
            install_draft,
            draft,
            initial_window_request,
            result,
        } => {
            apply_fork_session_result(
                chat,
                switch_after_create,
                install_draft,
                draft,
                initial_window_request,
                result,
            );
        }
        TuiEffectResult::CloneSession {
            switch_after_create,
            install_draft,
            initial_window_request,
            result,
        } => {
            apply_clone_session_result(
                chat,
                switch_after_create,
                install_draft,
                initial_window_request,
                result,
            );
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
        TuiEffectResult::SetSessionReasoning { status, result } => {
            apply_set_session_reasoning_result(chat, status, result);
        }
        TuiEffectResult::CompactContext { session_id, result } => {
            apply_compact_context_result(chat, session_id, result);
        }
        TuiEffectResult::AttachWorktree { path, result } => {
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
        TuiEffectResult::CycleThinkingEffort { session_id, result } => {
            apply_thinking_cycle_result(chat, session_id, *result);
        }
    }
}

fn apply_config_result(
    settings: &mut TuiRuntimeSettings,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    config: Result<bcode_config::BcodeConfig, String>,
) {
    match config {
        Ok(config) => {
            settings.apply_tui_config(&config.tui);
            settings.set_metrics_enabled(config.metrics.enabled);
            loop_state.telemetry.set_enabled(config.metrics.enabled);
            chat.app.apply_tui_config(config.tui.clone());
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

fn apply_rename_session_result(
    chat: &mut ActiveChat,
    result: Result<bcode_session_models::SessionSummary, ClientError>,
) {
    match result {
        Ok(session) => {
            chat.app.apply_session_summary(&session);
            chat.app.set_status("Session renamed".to_owned());
        }
        Err(error) => {
            daemon_issue::report_client_issue(&mut chat.app, "session rename failed", &error);
        }
    }
}

fn apply_delete_session_result(
    chat: &mut ActiveChat,
    session_id: bcode_session_models::SessionId,
    result: Result<bcode_session_models::SessionSummary, ClientError>,
) {
    match result {
        Ok(_session) => {
            if chat.app.session_id() == Some(session_id) {
                session_flow::switch_to_draft_session(chat);
            }
            chat.app.set_status("Session deleted".to_owned());
        }
        Err(error) => {
            daemon_issue::report_client_issue(&mut chat.app, "session delete failed", &error);
        }
    }
}

fn apply_fork_session_result(
    chat: &mut ActiveChat,
    switch_after_create: bool,
    install_draft: bool,
    draft: Option<String>,
    initial_window_request: bcode_session_models::ProjectionWindowRequest,
    result: Result<bcode_session_models::SessionForkResult, ClientError>,
) {
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            daemon_issue::report_client_issue(&mut chat.app, "session fork failed", &error);
            return;
        }
    };
    let draft = result.draft.or(draft);
    if switch_after_create {
        let new_session_id = result.session.id;
        session_flow::start_switch_session(chat, new_session_id, initial_window_request);
        if install_draft {
            if let Some(draft) = draft.as_deref() {
                chat.app.replace_composer_with(draft);
            }
        } else {
            chat.app.replace_composer_with("");
        }
        chat.app
            .set_status("forked session and switched".to_owned());
    } else {
        chat.app.apply_session_summary(&result.session);
        if install_draft {
            if let Some(draft) = draft.as_deref() {
                chat.app.replace_composer_with(draft);
            }
        } else {
            chat.app.replace_composer_with("");
        }
        chat.app
            .set_status(format!("forked session {}", result.session.id));
    }
}

fn apply_clone_session_result(
    chat: &mut ActiveChat,
    switch_after_create: bool,
    install_draft: bool,
    initial_window_request: bcode_session_models::ProjectionWindowRequest,
    result: Result<bcode_session_models::SessionForkResult, ClientError>,
) {
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            daemon_issue::report_client_issue(&mut chat.app, "session clone failed", &error);
            return;
        }
    };
    if !install_draft {
        chat.app.replace_composer_with("");
    }
    if switch_after_create {
        let new_session_id = result.session.id;
        session_flow::start_switch_session(chat, new_session_id, initial_window_request);
        chat.app
            .set_status("cloned session and switched".to_owned());
    } else {
        chat.app.apply_session_summary(&result.session);
        chat.app
            .set_status(format!("cloned session {}", result.session.id));
    }
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
    status: String,
    result: Result<(), ClientError>,
) {
    match result {
        Ok(()) => chat.app.set_status(status),
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
            chat.app
                .set_status(format!("worktree: {}", display_from_current_dir(path)));
        }
        Err(error) => {
            daemon_issue::report_client_issue(&mut chat.app, "worktree attach failed", &error);
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
    chat.app.push_system_note(format!(
        "Created worktree\n* Path: {}",
        display_from_current_dir(&path)
    ));
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

fn refresh_permissions_after_cancellation(loop_state: &mut ChatLoopState) {
    loop_state.abort_matching_effect(&TuiEffect::ListPermissions);
    loop_state.replace_effect(TuiEffect::ListPermissions);
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
            refresh_permissions_after_cancellation(loop_state);
            chat.app.set_cancelling();
            chat.app
                .set_status("turn cancellation requested".to_owned());
        }
        Ok(false) if Some(session_id) == chat.app.session_id() => {
            close_permission_dialog_for_session(&mut loop_state.permission_dialog, session_id);
            refresh_permissions_after_cancellation(loop_state);
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

fn apply_thinking_cycle_result(
    chat: &mut ActiveChat,
    session_id: Option<bcode_session_models::SessionId>,
    result: Result<super::effects::ThinkingCycleResult, ClientError>,
) {
    match result {
        Ok(result) if session_id == chat.app.session_id() => {
            if let Some(status) = result.status {
                chat.app.apply_model_status(status);
            }
            if let Some(next_effort) = result.next_effort {
                chat.app.apply_reasoning_selection(
                    Some(next_effort.clone()),
                    result.summary,
                    result.visible,
                );
                chat.app
                    .set_status(format!("reasoning effort set to {next_effort}"));
            } else {
                chat.app
                    .set_status("reasoning effort unavailable for current model".to_owned());
            }
        }
        Ok(_stale) => {}
        Err(error) => report_nonfatal_client_error(chat, "reasoning effort failed", &error),
    }
}

fn start_thinking_cycle(chat: &mut ActiveChat, loop_state: &mut ChatLoopState) {
    let started = loop_state.start_effect(TuiEffect::CycleThinkingEffort {
        session_id: chat.app.session_id(),
        current_effort: chat.app.reasoning_effort().map(ToOwned::to_owned),
        current_summary: chat.app.reasoning_summary().map(ToOwned::to_owned),
        visible: chat.app.reasoning_visible(),
    });
    if started {
        chat.app.set_status("updating reasoning effort…".to_owned());
    } else {
        chat.app
            .set_status("reasoning effort change already in progress".to_owned());
    }
}

fn start_cancel_turn(chat: &mut ActiveChat, loop_state: &mut ChatLoopState) {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return;
    };
    let started = loop_state.start_effect(TuiEffect::CancelTurn { session_id });
    if started {
        chat.app.set_cancelling();
        chat.app
            .set_status("turn cancellation requested".to_owned());
    } else {
        chat.app
            .set_status("turn cancellation already requested".to_owned());
    }
}

fn start_draft_save(chat: &mut ActiveChat, draft_autosave: &mut DraftAutosave) {
    let Some((scope, text)) = draft_autosave.pending_save(chat) else {
        return;
    };
    draft_autosave.mark_save_started();
    chat.queue_latest_effect(TuiEffect::SaveDraft { scope, text });
}

fn update_slash_palette_async(chat: &ActiveChat, loop_state: &mut ChatLoopState) -> bool {
    let current_query = chat.app.composer().text();
    if !current_query.starts_with('/') {
        loop_state.slash_palette = None;
        loop_state.abort_matching_effect(&TuiEffect::LoadSlashPalette {
            query: String::new(),
            session_id: None,
        });
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
    loop_state.replace_effect(TuiEffect::LoadSlashPalette {
        query,
        session_id: chat.app.session_id(),
    });
    true
}

const fn is_nonfatal_tui_daemon_error(error: &TuiError) -> bool {
    daemon_issue::is_nonfatal_tui_error(error)
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

fn next_redraw_at(last_redraw: Instant) -> Instant {
    last_redraw
        .checked_add(TARGET_FRAME_INTERVAL)
        .unwrap_or(last_redraw)
}

#[allow(clippy::too_many_lines)]
fn draw_chat_frame<W: Write>(
    terminal: &mut Terminal<&mut W>,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    schedule_delay: Duration,
) -> Result<(), TuiError> {
    let frame_started = Instant::now();
    let prepare_started = frame_started;
    let full_transcript_area = render::transcript_area_for_frame(&chat.app, terminal.area());
    let dock_height = loop_state
        .interactive_surface
        .as_mut()
        .map_or(0, |surface| {
            interactive_surface_height(surface, full_transcript_area)
        });
    let prepared =
        render::prepare_frame_with_bottom_dock(&mut chat.app, terminal.area(), dock_height);
    let layout = prepared.map(|(layout, _dock)| layout);
    let surface_area = prepared.map_or_else(
        || {
            Rect::new(
                full_transcript_area.x,
                full_transcript_area.bottom(),
                full_transcript_area.width,
                0,
            )
        },
        |(_layout, dock)| dock,
    );
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
    terminal.draw(|frame| {
        if let Some(layout) = layout {
            render::render_prepared(&mut chat.app, frame, layout);
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
        if let Some(dialog) = &loop_state.permission_dialog {
            permission_dialog_render::render_permission_dialog(dialog, frame);
        }
        if let Some(dialog) = &loop_state.thinking_dialog {
            thinking_dialog_render::render_thinking_dialog(dialog, frame, theme);
        }
        if let Some(dialog) = &mut loop_state.timeline_dialog {
            timeline_dialog_render::render_timeline_dialog(dialog, frame, theme);
        }
        if let Some(surface) = &mut loop_state.interactive_surface {
            surface.render(surface_area, frame);
        }
    })?;
    let draw_ms = elapsed_millis(draw_started);
    let total_ms = elapsed_millis(frame_started);
    loop_state.telemetry.add_counter("tui.frame.total", 1);
    if total_ms >= u64::try_from(TARGET_FRAME_INTERVAL.as_millis()).unwrap_or(u64::MAX) {
        loop_state
            .telemetry
            .add_counter("tui.frame.over_budget_total", 1);
    }
    let frame_index = loop_state.frame_index;
    loop_state.frame_index = loop_state.frame_index.wrapping_add(1);
    if frame_index.is_multiple_of(16)
        || total_ms >= u64::try_from(TARGET_FRAME_INTERVAL.as_millis()).unwrap_or(u64::MAX)
    {
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
    Ok(())
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn interactive_surface_height(surface: &mut InteractiveSurfaceState, viewport: Rect) -> u16 {
    let preferred = surface.preferred_height(viewport.width);
    let maximum = viewport.height.saturating_mul(2).div_ceil(3);
    preferred
        .min(maximum)
        .min(viewport.height.saturating_sub(1))
}

fn interactive_surface_area(surface: &mut InteractiveSurfaceState, viewport: Rect) -> Rect {
    let height = interactive_surface_height(surface, viewport);
    Rect::new(
        viewport.x,
        viewport.bottom().saturating_sub(height),
        viewport.width,
        height,
    )
}

fn take_bcode_events(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<BcodeEvent>,
    budget: usize,
) -> Vec<BcodeEvent> {
    (0..budget)
        .map_while(|_| receiver.try_recv().ok())
        .collect()
}

fn drain_bcode_events(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    budget: usize,
) -> bool {
    let mut needs_redraw = false;
    for event in take_bcode_events(&mut chat.event_receiver, budget) {
        needs_redraw |= absorb_bcode_event(chat, loop_state, event);
    }
    needs_redraw
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

fn drain_artifact_completions(
    chat: &ActiveChat,
    loop_state: &mut ChatLoopState,
    budget: usize,
) -> bool {
    let mut needs_redraw = false;
    for _ in 0..budget {
        let Some(completion) = loop_state.artifact_stream.try_next_completion() else {
            break;
        };
        needs_redraw |= handle_artifact_completion(chat, loop_state, completion);
    }
    needs_redraw
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
                    loop_state.replace_effect(TuiEffect::ListPermissions);
                }
                if matches!(event.kind, SessionEventKind::ModelChanged { .. }) {
                    loop_state.abort_matching_effect(&TuiEffect::LoadSessionStatus {
                        session_id: event.session_id,
                    });
                    loop_state.replace_effect(TuiEffect::LoadSessionModelStatus {
                        session_id: event.session_id,
                    });
                }
                if matches!(
                    event.kind,
                    SessionEventKind::RalphLifecycle { .. }
                        | SessionEventKind::PluginStatusNote { .. }
                        | SessionEventKind::LegacyEvent { .. }
                ) {
                    loop_state.replace_effect(TuiEffect::LoadPluginStatus {
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
                    loop_state.replace_effect(TuiEffect::ListPermissions);
                }
                chat.app.absorb_session_event(&event);
                observe_interactive_surface_event(loop_state, &event.kind);
            }
            true
        }
        BcodeEvent::SessionLive(event) if Some(event.session_id) == chat.session_id => {
            match &event.kind {
                bcode_session_models::SessionLiveEventKind::ToolOutputDelta { event: stream } => {
                    loop_state
                        .artifact_stream
                        .observe_live_event(event.session_id, stream);
                }
                bcode_session_models::SessionLiveEventKind::ToolContribution {
                    event: contribution,
                } => loop_state
                    .artifact_stream
                    .observe_contribution(event.session_id, contribution),
                bcode_session_models::SessionLiveEventKind::ToolContributionPlaced { envelope } => {
                    loop_state
                        .artifact_stream
                        .observe_contribution(event.session_id, &envelope.contribution);
                }
                _ => {}
            }
            chat.app.absorb_session_live_event(&event);
            true
        }
        BcodeEvent::RuntimeWork(event) if Some(event.session_id) == chat.session_id => {
            chat.app.absorb_session_event(&event);
            true
        }
        BcodeEvent::SessionViewResyncRequired { session_id }
            if Some(session_id) == chat.session_id =>
        {
            handle_session_view_resync(loop_state, session_id)
        }
        BcodeEvent::Session(_)
        | BcodeEvent::SessionLive(_)
        | BcodeEvent::RuntimeWork(_)
        | BcodeEvent::SessionViewResyncRequired { .. }
        | BcodeEvent::SessionCatalogUpdated { .. } => false,
    }
}

fn interaction_surface_request(
    interaction: &bcode_session_view_models::InteractionViewSummary,
) -> Option<InteractiveSurfaceRequest> {
    (!interaction.resolved && !interaction.surface_kind.is_empty()).then(|| {
        InteractiveSurfaceRequest::new(
            interaction.interaction_id.clone(),
            interaction.surface_kind.clone(),
            interaction
                .snapshot
                .clone()
                .unwrap_or(serde_json::Value::Null)
                .to_string(),
        )
    })
}

fn tool_exchange_surface_request(
    request: &bcode_session_models::ToolExchangeRequest,
) -> Option<InteractiveSurfaceRequest> {
    let adapter = bcode_bundled_plugins::interaction_adapter(
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

fn handle_session_view_resync(
    loop_state: &mut ChatLoopState,
    session_id: bcode_session_models::SessionId,
) -> bool {
    loop_state.replace_effect(TuiEffect::LoadSessionStatus { session_id });
    loop_state.replace_effect(TuiEffect::ListPermissions);
    true
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

async fn maybe_start_interactive_surface(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
) -> bool {
    if loop_state.interactive_surface.is_some()
        || loop_state
            .interactive_surface_queue
            .front_ready(Instant::now())
            .is_none()
    {
        return false;
    }
    let runtime = loop_state.plugin_runtime.get_or_insert_with(|| {
        super::plugin_tui::load_default_runtime_with_static_bundled(
            &bcode_bundled_plugins::static_bundled_plugins(),
        )
        .expect("load plugin runtime for interactive TUI surfaces")
    });
    let opened = InteractiveSurfaceState::open_request(
        runtime,
        loop_state
            .interactive_surface_queue
            .front_ready(Instant::now())
            .expect("ready request checked above"),
    )
    .await;
    match opened {
        Ok(surface) => {
            loop_state.interactive_surface_queue.pop_front();
            loop_state.interactive_surface = Some(surface);
            true
        }
        Err(error) => {
            loop_state
                .interactive_surface_queue
                .defer_front(Instant::now());
            tracing::warn!(%error, "failed to open interactive TUI surface");
            chat.app.set_status(format!(
                "Interactive request unavailable; retrying: {error}"
            ));
            true
        }
    }
}

async fn next_artifact_fetch_event(
    artifact_stream: &mut ArtifactStreamCoordinator,
) -> ChatLoopEvent {
    artifact_stream.next_completion().await.map_or_else(
        || ChatLoopEvent::TimedInvalidations(Vec::new()),
        |completion| ChatLoopEvent::ArtifactFetchCompleted(Box::new(completion)),
    )
}

fn try_next_ready_artifact_event(
    artifact_stream: &mut ArtifactStreamCoordinator,
) -> Option<ChatLoopEvent> {
    artifact_stream
        .try_next_completion()
        .map(|completion| ChatLoopEvent::ArtifactFetchCompleted(Box::new(completion)))
}

struct ChatLoopDeadlines {
    interaction_retry: Option<Instant>,
    telemetry_flush: Option<Instant>,
    redraw: Option<Instant>,
    draft_save: Option<Instant>,
}

async fn next_chat_loop_event(
    terminal_events: &mut TuiInput,
    invalidation_queue: &mut InvalidationQueue,
    chat: &mut ActiveChat,
    artifact_stream: &mut ArtifactStreamCoordinator,
    deadlines: ChatLoopDeadlines,
) -> Result<ChatLoopEvent, TuiError> {
    if let Some(event) = try_next_ready_artifact_event(artifact_stream) {
        return Ok(event);
    }
    let now = Instant::now();
    let due = invalidation_queue.take_due(now);
    if !due.is_empty() {
        return Ok(ChatLoopEvent::TimedInvalidations(due));
    }
    let next_timer_at = [
        invalidation_queue.next_at(),
        artifact_stream.next_retry_at(),
        deadlines.interaction_retry,
        deadlines.telemetry_flush,
        deadlines.redraw,
        deadlines.draft_save,
    ]
    .into_iter()
    .flatten()
    .min();
    if let Some(next_at) = next_timer_at {
        let delay = next_at.saturating_duration_since(now);
        return tokio::select! {
            artifact_event = next_artifact_fetch_event(artifact_stream) => Ok(artifact_event),
            bcode_event = chat.event_receiver.recv() => Ok(bcode_event.map_or_else(
                || ChatLoopEvent::TimedInvalidations(Vec::new()),
                |event| ChatLoopEvent::Bcode(Box::new(event)),
            )),
            event = terminal_events.recv() => event.map(|event| {
                event.map_or_else(
                    || ChatLoopEvent::TimedInvalidations(Vec::new()),
                    ChatLoopEvent::Terminal,
                )
            }),
            () = tokio::time::sleep(delay) => {
                let now = Instant::now();
                let due = invalidation_queue.take_due(now);
                if due.is_empty() {
                    Ok(ChatLoopEvent::Timer)
                } else {
                    Ok(ChatLoopEvent::TimedInvalidations(due))
                }
            },
        };
    }
    tokio::select! {
        artifact_event = next_artifact_fetch_event(artifact_stream) => Ok(artifact_event),
        bcode_event = chat.event_receiver.recv() => Ok(bcode_event.map_or_else(
            || ChatLoopEvent::TimedInvalidations(Vec::new()),
            |event| ChatLoopEvent::Bcode(Box::new(event)),
        )),
        event = terminal_events.recv() => event.map(|event| {
            event.map_or_else(
                || ChatLoopEvent::TimedInvalidations(Vec::new()),
                ChatLoopEvent::Terminal,
            )
        }),
    }
}

fn sync_chat_key_labels(chat: &mut ActiveChat, keymap: &BmuxKeyMap) {
    chat.app.set_key_hints(keymap.chat_hints());
    if let Some(label) = keymap.chat_action_label(BmuxAction::TranscriptBottom) {
        chat.app.set_jump_to_latest_key_label(label);
    }
}

fn refresh_invalidation_queue(chat: &ActiveChat, queue: &mut InvalidationQueue) {
    queue.replace(
        chat.app
            .invalidation_requests(Instant::now(), SystemTime::now()),
    );
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn handle_event<W: Write>(
    context: &mut ChatEventContext<'_, '_, W>,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    event: Event,
    draft_autosave: &mut DraftAutosave,
) -> Result<bool, TuiError> {
    match event {
        Event::Resize(size) => {
            context
                .terminal
                .resize(Rect::new(0, 0, size.width, size.height));
            if let Some(session_id) = chat.session_id {
                let runtime = loop_state.plugin_runtime.get_or_insert_with(|| {
                    super::plugin_tui::load_default_runtime_with_static_bundled(
                        &bcode_bundled_plugins::static_bundled_plugins(),
                    )
                    .expect("load plugin runtime for visual inputs")
                });
                for (tool_call_id, visual) in chat.app.active_plugin_visuals() {
                    let producer = visual.producer_plugin_id.as_deref();
                    let Some(route) = runtime.visual_adapter(
                        &visual.schema,
                        visual.schema_version,
                        "tui",
                        producer,
                    ) else {
                        continue;
                    };
                    let Some(input) = chat
                        .app
                        .plugin_presentation()
                        .and_then(|presentation| presentation.registry(&route.plugin_id))
                        .and_then(|registry| {
                            registry.visual_invocation_event_input(
                                &tool_call_id,
                                &route.schema,
                                &visual.payload,
                                &Event::Resize(size),
                            )
                        })
                    else {
                        continue;
                    };
                    if input.invocation_id != tool_call_id {
                        tracing::warn!(
                            expected = %tool_call_id,
                            actual = %input.invocation_id,
                            "visual adapter returned input for a different invocation"
                        );
                        continue;
                    }
                    if let Err(error) = context
                        .services
                        .client
                        .send_invocation_input(session_id, input)
                        .await
                    {
                        tracing::debug!(%error, "active plugin invocation did not accept visual input");
                    }
                }
            }
            Ok(true)
        }
        Event::Mouse(mouse)
            if loop_state.interactive_surface.is_some()
                && interactive_surface_event_route(
                    context.services.keymap,
                    &Event::Mouse(mouse),
                ) == InteractiveSurfaceEventRoute::TranscriptMouse =>
        {
            let hit_id = mouse_flow::mouse_hit_id(context.terminal.hits(), mouse);
            mouse_flow::handle_mouse(
                hit_id,
                context.services.client,
                chat,
                &mut loop_state.permission_dialog,
                mouse,
                context.mouse_scroll_rows,
            )
            .await
        }
        Event::Mouse(mouse)
            if loop_state.interactive_surface.is_some()
                && interactive_surface_event_route(
                    context.services.keymap,
                    &Event::Mouse(mouse),
                ) == InteractiveSurfaceEventRoute::HostMouse =>
        {
            let surface_area = loop_state
                .interactive_surface
                .as_mut()
                .map(|surface| {
                    interactive_surface_area(
                        surface,
                        render::transcript_area_for_frame(&chat.app, context.terminal.area()),
                    )
                })
                .expect("active interactive surface");
            if surface_area.contains(mouse.position) {
                handle_interactive_surface_event(context, chat, loop_state, Event::Mouse(mouse))
                    .await
            } else {
                let hit_id = mouse_flow::mouse_hit_id(context.terminal.hits(), mouse);
                mouse_flow::handle_mouse(
                    hit_id,
                    context.services.client,
                    chat,
                    &mut loop_state.permission_dialog,
                    mouse,
                    context.mouse_scroll_rows,
                )
                .await
            }
        }
        Event::Key(stroke)
            if loop_state.interactive_surface.is_some()
                && matches!(
                    interactive_surface_event_route(context.services.keymap, &Event::Key(stroke)),
                    InteractiveSurfaceEventRoute::Host(_)
                ) =>
        {
            handle_interactive_surface_host_key(context, chat, loop_state, stroke).await
        }
        event @ (Event::Key(_)
        | Event::Paste(_)
        | Event::Focus(_)
        | Event::Tick
        | Event::Mouse(_))
            if loop_state.interactive_surface.is_some() =>
        {
            handle_interactive_surface_event(context, chat, loop_state, event).await
        }
        Event::Key(stroke) => {
            handle_chat_key(context, chat, loop_state, stroke, draft_autosave).await
        }
        Event::Paste(text) => {
            if let Some(palette) = &mut loop_state.palette {
                palette.state_mut().query.insert_str(&text);
                return Ok(true);
            }
            chat.app.reset_input_history_navigation();
            chat.app.paste_composer_text(&text);
            chat.app.wake_cursor();
            update_slash_palette_async(chat, loop_state);
            Ok(true)
        }
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick => Ok(true),
        Event::Mouse(mouse) => {
            if loop_state.palette.is_some() {
                let (mut io, services) = context.flow_context();
                return palette_flow::handle_palette_mouse(
                    &mut io,
                    &services,
                    chat,
                    &mut loop_state.palette,
                    mouse,
                )
                .await;
            }
            if loop_state.slash_palette.is_some() {
                return Ok(slash_flow::handle_slash_palette_mouse(
                    chat,
                    &mut loop_state.slash_palette,
                    context.terminal,
                    mouse,
                ));
            }
            let hit_id = mouse_flow::mouse_hit_id(context.terminal.hits(), mouse);
            mouse_flow::handle_mouse(
                hit_id,
                context.services.client,
                chat,
                &mut loop_state.permission_dialog,
                mouse,
                context.mouse_scroll_rows,
            )
            .await
        }
        Event::User(_) => Ok(false),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractiveSurfaceEventRoute {
    Host(BmuxAction),
    TranscriptMouse,
    HostMouse,
    Surface,
}

fn interactive_surface_event_route(
    keymap: &BmuxKeyMap,
    event: &Event,
) -> InteractiveSurfaceEventRoute {
    // Host-owned transcript and app actions win first. Wheel events always scroll the
    // transcript, while other pointer events are classified by dock bounds at dispatch.
    match event {
        Event::Key(stroke) => interactive_surface_host_key(keymap, *stroke).map_or(
            InteractiveSurfaceEventRoute::Surface,
            InteractiveSurfaceEventRoute::Host,
        ),
        Event::Mouse(mouse)
            if matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            ) =>
        {
            InteractiveSurfaceEventRoute::TranscriptMouse
        }
        Event::Mouse(_) => InteractiveSurfaceEventRoute::HostMouse,
        Event::Paste(_) | Event::Focus(_) | Event::Tick | Event::Resize(_) | Event::User(_) => {
            InteractiveSurfaceEventRoute::Surface
        }
    }
}

#[allow(clippy::future_not_send)]
async fn handle_interactive_surface_host_key<W: Write>(
    context: &ChatEventContext<'_, '_, W>,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    stroke: KeyStroke,
) -> Result<bool, TuiError> {
    match interactive_surface_host_key(context.services.keymap, stroke) {
        Some(BmuxAction::AppExit) => {
            chat.app.request_exit();
            Ok(true)
        }
        Some(BmuxAction::AppInterrupt) => {
            resolve_interactive_surface_dismissed(context, chat, loop_state).await?;
            Ok(true)
        }
        Some(action) if is_transcript_action(action) => {
            Ok(input::handle_host_action(&mut chat.app, action).redraw)
        }
        _ => Ok(false),
    }
}

fn interactive_surface_host_key(keymap: &BmuxKeyMap, stroke: KeyStroke) -> Option<BmuxAction> {
    let action = keymap.action_for_key(BmuxScope::Chat, stroke)?;
    (matches!(action, BmuxAction::AppExit | BmuxAction::AppInterrupt)
        || is_transcript_action(action))
    .then_some(action)
}

const fn is_transcript_action(action: BmuxAction) -> bool {
    matches!(
        action,
        BmuxAction::TranscriptPageUp
            | BmuxAction::TranscriptPageDown
            | BmuxAction::TranscriptTop
            | BmuxAction::TranscriptBottom
            | BmuxAction::TranscriptLineUp
            | BmuxAction::TranscriptLineDown
    )
}

#[allow(clippy::future_not_send)]
async fn resolve_interactive_surface_dismissed<W: Write>(
    context: &ChatEventContext<'_, '_, W>,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
) -> Result<(), TuiError> {
    let Some(surface) = loop_state.interactive_surface.take() else {
        return Ok(());
    };
    let interaction_id = surface.interaction_id().to_owned();
    let outcome = match execute_session_view_action(
        context.services.client,
        SessionViewAction::ResolveExchange {
            interaction_id,
            resolution: InteractiveSurfaceState::dismissed_resolution(),
        },
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            loop_state.interactive_surface = Some(surface);
            chat.app
                .set_status(format!("Interactive dismissal failed; retry: {error}"));
            return Ok(());
        }
    };
    if !matches!(
        outcome,
        SessionViewActionOutcome::InteractionResolved {
            resolved: true | false
        }
    ) {
        loop_state.interactive_surface = Some(surface);
        chat.app.set_status(
            "Interactive dismissal failed: unexpected daemon response; retry".to_owned(),
        );
        return Ok(());
    }
    chat.app
        .set_status("interactive request dismissed".to_owned());
    Ok(())
}

#[allow(clippy::future_not_send)]
async fn handle_interactive_surface_event<W: Write>(
    context: &ChatEventContext<'_, '_, W>,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    event: Event,
) -> Result<bool, TuiError> {
    let Some(surface) = &mut loop_state.interactive_surface else {
        return Ok(false);
    };
    let _surface_area = interactive_surface_area(
        surface,
        render::transcript_area_for_frame(&chat.app, context.terminal.area()),
    );
    if let Some(resolution) = surface.handle_event(&event) {
        let interaction_id = surface.interaction_id().to_owned();
        match execute_session_view_action(
            context.services.client,
            SessionViewAction::ResolveExchange {
                interaction_id,
                resolution,
            },
        )
        .await
        {
            Ok(SessionViewActionOutcome::InteractionResolved { resolved: true }) => {
                loop_state.interactive_surface = None;
            }
            Ok(SessionViewActionOutcome::InteractionResolved { resolved: false }) => {
                loop_state.interactive_surface = None;
                chat.app.set_status(
                    "Interactive request was already resolved by another client".to_owned(),
                );
            }
            Ok(_) => {
                surface.clear_pending_resolution();
                chat.app.set_status(
                    "Interactive response failed: unexpected daemon response; retry".to_owned(),
                );
            }
            Err(error) => {
                surface.clear_pending_resolution();
                tracing::warn!(%error, "failed to resolve interactive request");
                chat.app
                    .set_status(format!("Interactive response failed; retry: {error}"));
            }
        }
    }
    Ok(true)
}

async fn handle_chat_key<W: Write>(
    context: &mut ChatEventContext<'_, '_, W>,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    stroke: KeyStroke,
    draft_autosave: &mut DraftAutosave,
) -> Result<bool, TuiError> {
    if loop_state.timeline_dialog.is_some() {
        return timeline_flow::handle_timeline_key(
            context.services.client,
            chat,
            &mut loop_state.timeline_dialog,
            stroke,
        )
        .await;
    }
    if loop_state.thinking_dialog.is_some() {
        return thinking_flow::handle_thinking_key(
            context.services.client,
            chat,
            &mut loop_state.thinking_dialog,
            stroke,
        )
        .await;
    }
    if loop_state.slash_palette.is_some() {
        if let Some(dialog) = {
            let (mut io, services) = context.flow_context();
            slash_flow::handle_slash_palette_key(
                &mut io,
                &services,
                chat,
                &mut loop_state.slash_palette,
                stroke,
            )
            .await?
            .flatten()
        } {
            apply_composer_modal_request(loop_state, dialog);
        }
        return Ok(true);
    }
    if loop_state.permission_dialog.is_some() {
        return permission_flow::handle_permission_key(
            context.services.client,
            context.services.keymap,
            chat,
            &mut loop_state.permission_dialog,
            stroke,
        )
        .await;
    }
    if loop_state.palette.is_some() {
        let (mut io, services) = context.flow_context();
        return palette_flow::handle_palette_key(
            &mut io,
            &services,
            chat,
            &mut loop_state.palette,
            stroke,
        )
        .await;
    }
    if is_palette_open_key(context.services.keymap, stroke) {
        loop_state.palette = Some(palette_flow::open_palette(&context.services, chat).await);
        chat.app
            .set_status("command palette: type to filter, enter to run, esc close".to_owned());
        return Ok(true);
    }
    if is_clipboard_image_paste_key(context.services.keymap, stroke) {
        paste_clipboard_image(chat);
        update_slash_palette_async(chat, loop_state);
        return Ok(true);
    }
    let outcome = input::handle_key(&mut chat.app, context.services.keymap, stroke);
    if chat.app.should_exit() {
        return Ok(true);
    }
    update_slash_palette_async(chat, loop_state);
    handle_chat_key_request(
        context,
        chat,
        loop_state,
        outcome.request,
        Some(draft_autosave),
    )
    .await?;
    Ok(outcome.redraw)
}

async fn handle_chat_key_request<W: Write>(
    context: &mut ChatEventContext<'_, '_, W>,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    request: KeyRequest,
    draft_autosave: Option<&mut DraftAutosave>,
) -> Result<(), TuiError> {
    match request {
        KeyRequest::None => {}
        KeyRequest::Interrupt => {
            start_cancel_turn(chat, loop_state);
        }
        KeyRequest::CycleAgent => cycle_session_agent(chat),
        KeyRequest::CycleThinkingEffort => {
            start_thinking_cycle(chat, loop_state);
        }
        KeyRequest::Submit { placement } => {
            let pre_submit_scope = draft_autosave.as_ref().map(|autosave| autosave.scope(chat));
            let (mut io, services) = context.flow_context();
            match composer_flow::submit_composer(&mut io, &services, chat, placement).await {
                Ok(Some(request)) => {
                    apply_composer_modal_request(loop_state, request);
                }
                Ok(None) => {}
                Err(error) => helpers::report_client_error(&mut chat.app, "send failed", &error),
            }
            if let Some(autosave) = draft_autosave {
                if let Some(scope) = pre_submit_scope {
                    let (scope, text) = DraftAutosave::clear_scope_request(scope);
                    chat.queue_latest_effect(TuiEffect::SaveDraft { scope, text });
                }
                autosave.mark_dirty_now();
                start_draft_save(chat, autosave);
            }
        }
    }
    Ok(())
}

fn agent_selection_status(chat: &ActiveChat, agent_name: &str) -> String {
    if matches!(chat.app.activity(), ActivityState::Idle) {
        format!("agent {agent_name} selected")
    } else {
        format!("agent {agent_name} selected for next message")
    }
}

fn cycle_session_agent(chat: &mut ActiveChat) {
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

fn apply_composer_modal_request(
    loop_state: &mut ChatLoopState,
    request: composer_flow::ComposerModalRequest,
) {
    match request {
        composer_flow::ComposerModalRequest::Thinking(dialog) => {
            loop_state.thinking_dialog = Some(dialog);
        }
        composer_flow::ComposerModalRequest::Timeline(dialog) => {
            loop_state.timeline_dialog = Some(dialog);
        }
    }
}

fn is_palette_open_key(keymap: &BmuxKeyMap, stroke: KeyStroke) -> bool {
    keymap.action_for_key(BmuxScope::Chat, stroke) == Some(BmuxAction::CommandPaletteOpen)
}

fn is_clipboard_image_paste_key(keymap: &BmuxKeyMap, stroke: KeyStroke) -> bool {
    keymap.action_for_key(BmuxScope::Chat, stroke) == Some(BmuxAction::ClipboardPasteImage)
}

fn paste_clipboard_image(chat: &mut ActiveChat) {
    let launch_working_directory = chat
        .app
        .working_directory()
        .map_or_else(std::env::current_dir, |path| Ok(path.to_path_buf()));
    let Ok(launch_working_directory) = launch_working_directory else {
        chat.app
            .set_status("image paste failed: current directory unavailable".to_owned());
        return;
    };
    match clipboard_image::save_clipboard_image(chat.app.session_id(), &launch_working_directory) {
        Ok(artifact) => {
            let text = clipboard_image::pasted_image_text(&artifact.model);
            chat.app.reset_input_history_navigation();
            chat.app.paste_composer_text(&text);
            chat.app.wake_cursor();
            chat.app.set_status(format!(
                "Image pasted: {}; source saved in session artifacts",
                display_from_current_dir(&artifact.model)
            ));
        }
        Err(error) => {
            chat.app.set_status(format!("image paste failed: {error}"));
        }
    }
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;
    use bmux_keyboard::KeyCode;
    fn test_chat() -> ActiveChat {
        let (event_sender, event_receiver) = tokio::sync::mpsc::unbounded_channel();
        ActiveChat {
            app: super::super::app::BmuxApp::new_with_history(None, &[], &[], false),
            agents: super::super::session_flow::AgentCatalog::default(),
            session_id: None,
            event_sender,
            event_receiver,
            event_task: None,
            opening_session_id: None,
            opening_session_progress: None,
            pending_effects: super::super::effects::TuiEffectQueue::default(),
        }
    }

    #[tokio::test]
    async fn initial_submit_installs_session_before_releasing_event_stream() {
        let mut chat = test_chat();
        chat.app.replace_composer_with("initial prompt");
        chat.app.stage_submission();
        let session_id = bcode_session_models::SessionId::new();
        let (release_sender, mut release_receiver) = tokio::sync::oneshot::channel();
        let event_task = tokio::spawn(std::future::pending());

        apply_submit_message_result(
            &mut chat,
            "initial prompt",
            Ok(super::super::effects::SubmitMessageResult {
                session_id,
                created_session: None,
                acceptance: bcode_client::MessageAcceptance::sent(),
                committed_agent_id: None,
                event_task: Some(event_task),
                event_stream_release: Some(release_sender),
            }),
        );

        assert_eq!(chat.session_id, Some(session_id));
        assert!(matches!(
            release_receiver.try_recv(),
            Ok(()) | Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        assert!(chat.event_task.is_some());
        if let Some(event_task) = chat.event_task.take() {
            event_task.abort();
        }
    }

    fn interaction(
        id: &str,
        surface_kind: &str,
    ) -> bcode_session_view_models::InteractionViewSummary {
        bcode_session_view_models::InteractionViewSummary {
            interaction_id: id.to_owned(),
            kind: "bcode.question".to_owned(),
            surface_kind: surface_kind.to_owned(),
            tool_call_id: Some(format!("call-{id}")),
            title: Some("Question".to_owned()),
            required: true,
            snapshot: Some(serde_json::json!({"questions": []})),
            resolved: false,
            resolution: None,
        }
    }

    fn install_question_runtime(state: &mut ChatLoopState) {
        let plugin = bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/question-plugin/bcode-plugin.toml"),
            bcode_question_plugin::static_plugin(),
        );
        state.plugin_runtime = Some(
            bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
                &bcode_plugin::PluginSelection::all_enabled(),
                &[plugin],
            )
            .expect("question runtime"),
        );
    }

    #[tokio::test]
    async fn hydration_reconciles_pending_queue_idempotently_and_removes_stale_entries() {
        let mut state = ChatLoopState::new(
            &BcodeClient::default_endpoint(),
            &BcodeClient::default_endpoint(),
            TuiDaemonHost::new(&[]),
            false,
        );
        let first = interaction("first", "bcode.question.inline");
        let second = interaction("second", "bcode.question.inline");
        reconcile_interactive_surfaces(&mut state, &[first.clone(), second.clone()]);
        reconcile_interactive_surfaces(&mut state, &[first, second.clone()]);
        assert_eq!(
            state.interactive_surface_queue.interaction_ids(),
            ["first", "second"]
        );

        reconcile_interactive_surfaces(&mut state, &[second]);
        assert_eq!(
            state.interactive_surface_queue.interaction_ids(),
            ["second"]
        );
    }

    #[tokio::test]
    async fn hydrated_requests_deduplicate_and_external_resolution_removes_matching_queue_entry() {
        let mut state = ChatLoopState::new(
            &BcodeClient::default_endpoint(),
            &BcodeClient::default_endpoint(),
            TuiDaemonHost::new(&[]),
            false,
        );
        let request = interaction("question-1", "bcode.question.inline");
        reconcile_interactive_surfaces(&mut state, std::slice::from_ref(&request));
        reconcile_interactive_surfaces(&mut state, &[request]);
        assert_eq!(
            state.interactive_surface_queue.interaction_ids(),
            ["question-1"]
        );

        observe_interactive_surface_event(
            &mut state,
            &SessionEventKind::ToolExchangeResolved {
                event: bcode_session_models::ToolExchangeResolutionEvent {
                    invocation_id: "call-question-1".to_owned(),
                    exchange_id: "question-1".to_owned(),
                    resolution: bcode_session_models::ToolExchangeResolution::ConsumerDetached,
                },
            },
        );
        assert!(state.interactive_surface_queue.interaction_ids().is_empty());
    }

    #[tokio::test]
    async fn failed_surface_open_is_retained_and_retried_after_delay() {
        let mut state = ChatLoopState::new(
            &BcodeClient::default_endpoint(),
            &BcodeClient::default_endpoint(),
            TuiDaemonHost::new(&[]),
            false,
        );
        state.interactive_surface_queue.enqueue(
            InteractiveSurfaceRequest::new("broken", "unknown.surface", "{}"),
            None,
        );
        let mut chat = test_chat();
        assert!(maybe_start_interactive_surface(&mut chat, &mut state).await);
        assert_eq!(
            state.interactive_surface_queue.interaction_ids(),
            ["broken"]
        );
        assert!(state.interactive_surface_queue.next_retry_at().is_some());
        assert!(chat.app.status().contains("retrying"));
    }

    #[tokio::test]
    async fn hydrated_multiple_questions_open_in_fifo_order() {
        let mut state = ChatLoopState::new(
            &BcodeClient::default_endpoint(),
            &BcodeClient::default_endpoint(),
            TuiDaemonHost::new(&[]),
            false,
        );
        install_question_runtime(&mut state);
        let payload = serde_json::json!({
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
        });
        let mut first = interaction("first", "bcode.question.inline");
        first.snapshot = Some(payload.clone());
        let mut second = interaction("second", "bcode.question.inline");
        second.snapshot = Some(payload);
        reconcile_interactive_surfaces(&mut state, &[first, second]);
        let mut chat = test_chat();

        assert!(maybe_start_interactive_surface(&mut chat, &mut state).await);
        assert_eq!(
            state
                .interactive_surface
                .as_ref()
                .map(InteractiveSurfaceState::interaction_id),
            Some("first")
        );
        assert_eq!(
            state.interactive_surface_queue.interaction_ids(),
            ["second"]
        );
        state.interactive_surface = None;
        assert!(maybe_start_interactive_surface(&mut chat, &mut state).await);
        assert_eq!(
            state
                .interactive_surface
                .as_ref()
                .map(InteractiveSurfaceState::interaction_id),
            Some("second")
        );
    }

    #[test]
    fn permission_hydration_preserves_batch_and_policy_semantics() {
        let session_id = bcode_session_models::SessionId::new();
        let permission = permission_summary_view(bcode_ipc::PermissionSummary {
            permission_id: "permission-1".to_owned(),
            session_id,
            tool_call_id: "call-1".to_owned(),
            tool_name: "shell.run".to_owned(),
            arguments_json: r#"{"command":"cargo test"}"#.to_owned(),
            batch: Some(bcode_ipc::PermissionBatchCorrelation {
                batch_id: "batch-1".to_owned(),
                call_index: 1,
                call_count: 3,
            }),
            agent_id: "build".to_owned(),
            policy_source: Some("skill".to_owned()),
            policy_reason: Some("requires approval".to_owned()),
            can_remember_policy: true,
        });

        assert_eq!(permission.session_id, Some(session_id));
        assert_eq!(permission.tool_name, "shell.run");
        assert_eq!(permission.policy_source.as_deref(), Some("skill"));
        assert_eq!(permission.detail.as_deref(), Some("requires approval"));
        assert!(permission.can_remember);
        let batch = permission.batch.expect("batch correlation");
        assert_eq!(batch.batch_id, "batch-1");
        assert_eq!(batch.call_index, 1);
        assert_eq!(batch.call_count, 3);
    }

    #[test]
    fn active_surface_routing_preserves_all_semantic_transcript_actions() {
        let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
        for (key, expected) in [
            (KeyCode::PageUp, BmuxAction::TranscriptPageUp),
            (KeyCode::PageDown, BmuxAction::TranscriptPageDown),
        ] {
            assert_eq!(
                interactive_surface_event_route(
                    &keymap,
                    &Event::Key(KeyStroke {
                        key,
                        modifiers: bmux_keyboard::Modifiers::NONE,
                    }),
                ),
                InteractiveSurfaceEventRoute::Host(expected)
            );
        }
        for (key, expected) in [
            (KeyCode::Home, BmuxAction::TranscriptTop),
            (KeyCode::End, BmuxAction::TranscriptBottom),
            (KeyCode::Up, BmuxAction::TranscriptLineUp),
            (KeyCode::Down, BmuxAction::TranscriptLineDown),
        ] {
            assert_eq!(
                interactive_surface_event_route(
                    &keymap,
                    &Event::Key(KeyStroke {
                        key,
                        modifiers: bmux_keyboard::Modifiers {
                            ctrl: true,
                            ..bmux_keyboard::Modifiers::NONE
                        },
                    }),
                ),
                InteractiveSurfaceEventRoute::Host(expected)
            );
        }
    }

    #[test]
    fn active_surface_routing_honors_custom_transcript_bindings() {
        let mut config = bcode_config::TuiConfig::default();
        config.keybindings.chat = std::collections::BTreeMap::from([(
            "alt+u".to_owned(),
            "transcript.pageUp".to_owned(),
        )]);
        let keymap = BmuxKeyMap::from_config(&config);
        assert_eq!(
            interactive_surface_event_route(
                &keymap,
                &Event::Key(KeyStroke {
                    key: KeyCode::Char('u'),
                    modifiers: bmux_keyboard::Modifiers {
                        alt: true,
                        ..bmux_keyboard::Modifiers::NONE
                    },
                }),
            ),
            InteractiveSurfaceEventRoute::Host(BmuxAction::TranscriptPageUp)
        );
    }

    #[test]
    fn active_surface_routing_reserves_mouse_wheel_for_transcript() {
        let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
        for kind in [MouseEventKind::ScrollUp, MouseEventKind::ScrollDown] {
            assert_eq!(
                interactive_surface_event_route(
                    &keymap,
                    &Event::Mouse(bmux_tui::event::MouseEvent::new(
                        kind,
                        bmux_tui::geometry::Point::new(4, 8),
                    )),
                ),
                InteractiveSurfaceEventRoute::TranscriptMouse
            );
        }
        assert_eq!(
            interactive_surface_event_route(
                &keymap,
                &Event::Mouse(bmux_tui::event::MouseEvent::new(
                    MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                    bmux_tui::geometry::Point::new(4, 8),
                )),
            ),
            InteractiveSurfaceEventRoute::HostMouse
        );
    }

    #[test]
    fn cancellation_closes_only_the_matching_session_permission_dialog() {
        let session_id = bcode_session_models::SessionId::new();
        let other_session_id = bcode_session_models::SessionId::new();
        let permission = |session_id| {
            PermissionDialogState::new(permission_summary_view(bcode_ipc::PermissionSummary {
                permission_id: "permission-1".to_owned(),
                session_id,
                tool_call_id: "call-1".to_owned(),
                tool_name: "example.tool".to_owned(),
                arguments_json: "{}".to_owned(),
                batch: Some(bcode_ipc::PermissionBatchCorrelation {
                    batch_id: "batch-1".to_owned(),
                    call_index: 0,
                    call_count: 2,
                }),
                agent_id: "build".to_owned(),
                policy_source: None,
                policy_reason: None,
                can_remember_policy: false,
            }))
        };
        let mut dialog = Some(permission(session_id));

        close_permission_dialog_for_session(&mut dialog, other_session_id);
        assert!(dialog.is_some());
        close_permission_dialog_for_session(&mut dialog, session_id);
        assert!(dialog.is_none());
    }

    #[test]
    fn daemon_queue_draining_obeys_its_per_tick_budget() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        for revision in 0..100 {
            sender
                .send(BcodeEvent::SessionCatalogUpdated { revision })
                .expect("daemon event");
        }

        assert_eq!(take_bcode_events(&mut receiver, 32).len(), 32);
        assert!(receiver.try_recv().is_ok());
    }

    #[test]
    fn ready_artifact_completion_precedes_a_saturated_daemon_queue() {
        let mut chat = test_chat();
        for revision in 0..1_000 {
            chat.event_sender
                .send(BcodeEvent::SessionCatalogUpdated { revision })
                .expect("daemon event");
        }
        let session_id = bcode_session_models::SessionId::new();
        let mut artifacts = ArtifactStreamCoordinator::new(BcodeClient::default_endpoint());
        artifacts.enqueue_test_completion(session_id);

        assert!(matches!(
            try_next_ready_artifact_event(&mut artifacts),
            Some(ChatLoopEvent::ArtifactFetchCompleted(_))
        ));
        assert!(chat.event_receiver.try_recv().is_ok());
        assert!(chat.event_receiver.try_recv().is_ok());
    }

    #[test]
    fn session_open_progress_formats_determinate_bytes() {
        let snapshot = progress_snapshot(
            bcode_session_models::SessionId::new(),
            Some(512),
            Some(1024),
            Some(bcode_session_models::SessionMigrationProgressUnit::Bytes),
        );
        let status = session_open_progress_status(&snapshot);
        assert!(status.contains("epoch 3 → 4"));
        assert!(status.contains("██████░░░░░░"));
        assert!(status.contains("512 B / 1.0 KiB"));
    }

    #[test]
    fn session_open_progress_formats_indeterminate_stage() {
        let snapshot = progress_snapshot(bcode_session_models::SessionId::new(), None, None, None);
        let status = session_open_progress_status(&snapshot);
        assert!(status.contains("Upgrading session (epoch 3 → 4)"));
        assert!(status.contains("Preparing session backup"));
    }

    #[test]
    fn session_open_progress_ignores_stale_session_updates() {
        let mut chat = test_chat();
        let opening = bcode_session_models::SessionId::new();
        chat.opening_session_id = Some(opening);
        chat.app.set_status("opening session".to_owned());
        let stale = progress_snapshot(
            bcode_session_models::SessionId::new(),
            Some(1),
            Some(2),
            Some(bcode_session_models::SessionMigrationProgressUnit::Files),
        );
        assert!(!apply_session_open_progress(&mut chat, &stale));
        assert_eq!(chat.app.status(), "opening session");

        let current = progress_snapshot(
            opening,
            Some(1),
            Some(2),
            Some(bcode_session_models::SessionMigrationProgressUnit::Files),
        );
        assert!(apply_session_open_progress(&mut chat, &current));
        assert_eq!(chat.opening_session_progress.as_ref(), Some(&current));
        assert!(chat.app.status().contains("1 / 2 files"));
        assert!(!apply_session_open_progress(&mut chat, &current));

        let mut older = current.clone();
        older.revision = 0;
        assert!(!apply_session_open_progress(&mut chat, &older));

        let mut newer = current.clone();
        newer.revision = 2;
        newer.progress.completed_units = Some(2);
        assert!(apply_session_open_progress(&mut chat, &newer));
        assert!(chat.app.status().contains("2 / 2 files"));

        let mut replacement = newer.clone();
        replacement.operation_id = bcode_session_models::SessionOpenOperationId::new();
        replacement.revision = 0;
        replacement.progress.message = "Reclassified after reconnect".to_owned();
        assert!(apply_session_open_progress(&mut chat, &replacement));
        assert_eq!(chat.opening_session_progress.as_ref(), Some(&replacement));
    }

    fn progress_snapshot(
        session_id: bcode_session_models::SessionId,
        completed_units: Option<u64>,
        total_units: Option<u64>,
        unit: Option<bcode_session_models::SessionMigrationProgressUnit>,
    ) -> bcode_session_models::SessionOpenOperationSnapshot {
        bcode_session_models::SessionOpenOperationSnapshot {
            operation_id: bcode_session_models::SessionOpenOperationId::new(),
            revision: 1,
            session_id,
            source_writer_epoch: Some(3),
            target_writer_epoch: 4,
            progress: bcode_session_models::SessionMigrationProgress {
                stage: bcode_session_models::SessionMigrationStage::PlanningBackup,
                completed_units,
                total_units,
                unit,
                message: "Preparing session backup".to_owned(),
            },
            outcome: None,
            backup_path: None,
        }
    }
}
