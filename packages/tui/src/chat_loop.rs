//! Main chat event loop for the TUI.

use std::io::Write;
use std::time::{Duration, Instant, SystemTime};

use bcode_client::{BcodeClient, ClientError, DaemonAvailability};
use bcode_config::TuiConfig;
use bcode_ipc::{ComposerDraftScope, Event as BcodeEvent};
use bcode_session_models::SessionEventKind;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;

use super::activity::ActivityState;
use super::clipboard_image;
use super::command_palette::BmuxCommandPalette;
use super::daemon_issue;
use super::effects::{DaemonObservation, TuiEffect, TuiEffectResult, TuiEffectRunner};
use super::helpers;
use super::invalidation::InvalidationQueue;
use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use super::permission_dialog::PermissionDialogState;
use super::protocol_surface::{
    BMUX_PROTOCOL_INLINE_SURFACE, INLINE_PROTOCOL_SURFACE_ROW_OFFSET, ProtocolSurfaceState,
};
use super::runtime_context::{TuiIo, TuiServices};
use super::session_flow::{self, ActiveChat};
use super::terminal_events::TuiInput;
use super::transcript_layout::VisibleTranscriptSource;
use super::{
    TuiError, command_palette_render, composer_flow, input, input::KeyRequest, mouse_flow,
    palette_flow, permission_dialog_render, permission_flow, render, slash_flow, slash_palette,
    slash_palette_render, thinking_dialog_render, thinking_flow, timeline_dialog_render,
    timeline_flow,
};

const TARGET_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const DRAFT_SAVE_DEBOUNCE: Duration = Duration::from_millis(900);
const PERMISSION_POLL_INTERVAL: Duration = Duration::from_millis(750);
const PERMISSION_POLL_DAEMON_DOWN_INTERVAL: Duration = Duration::from_secs(15);

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
    permission_poll: PermissionPollSchedule,
    thinking_dialog: Option<super::thinking_dialog::ThinkingDialogState>,
    timeline_dialog: Option<super::timeline_dialog::TimelineDialogState>,
    protocol_surface: Option<ProtocolSurfaceState>,
}

impl ChatLoopState {
    fn new(foreground_client: &BcodeClient, passive_client: &BcodeClient) -> Self {
        Self {
            palette: None,
            slash_palette: None,
            effects: TuiEffectRunner::new(foreground_client, passive_client),
            daemon_connection: DaemonConnectionMonitor::default(),
            permission_dialog: None,
            permission_poll: PermissionPollSchedule::new(Instant::now()),
            thinking_dialog: None,
            timeline_dialog: None,
            protocol_surface: None,
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

    fn maybe_start_permission_poll(&mut self, chat: &ActiveChat) {
        if self.permission_dialog.is_some()
            || Instant::now() < self.permission_poll.next_poll_at
            || chat.session_id.is_none()
        {
            return;
        }
        if self.start_effect(TuiEffect::ListPermissions) {
            self.permission_poll.next_poll_at = Instant::now() + PERMISSION_POLL_INTERVAL;
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

#[derive(Debug)]
struct PermissionPollSchedule {
    next_poll_at: Instant,
    last_error_status: Option<String>,
}

impl PermissionPollSchedule {
    const fn new(now: Instant) -> Self {
        Self {
            next_poll_at: now,
            last_error_status: None,
        }
    }
}

pub struct TuiRuntimeSettings {
    keymap: BmuxKeyMap,
    mouse_scroll_rows: usize,
    launch_working_directory: std::path::PathBuf,
}

impl TuiRuntimeSettings {
    pub fn bootstrap(launch_working_directory: std::path::PathBuf) -> Self {
        let tui_config = TuiConfig::default();
        Self {
            keymap: BmuxKeyMap::from_config(&tui_config),
            mouse_scroll_rows: tui_config.mouse.effective_scroll_rows(),
            launch_working_directory,
        }
    }

    pub fn apply_tui_config(&mut self, tui_config: &TuiConfig) {
        self.keymap = BmuxKeyMap::from_config(tui_config);
        self.mouse_scroll_rows = tui_config.mouse.effective_scroll_rows();
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
) -> Result<(), TuiError> {
    let passive_client = client
        .clone()
        .with_daemon_availability(DaemonAvailability::RequireRunning);
    let mut loop_state = ChatLoopState::new(client, &passive_client);
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
        if drain_bcode_events(chat, &mut loop_state) {
            needs_redraw = true;
        }

        if handle_loop_housekeeping(settings, chat, &mut draft_autosave, &mut loop_state).await {
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
            draw_chat_frame(terminal, chat, &mut loop_state)?;
            if let Some(action) = startup_action.take()
                && action == super::startup_action::StartupTuiAction::OpenRalphHome
            {
                let mut io = TuiIo {
                    terminal,
                    input: terminal_events,
                };
                let services = TuiServices {
                    client,
                    passive_client: &passive_client,
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
            needs_redraw.then_some(redraw_at),
            draft_autosave.next_save_at(),
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
                        passive_client: &passive_client,
                        keymap: &settings.keymap,
                        theme: render::TuiTheme::for_app(&chat.app),
                    },
                    terminal,
                    terminal_events,
                    mouse_scroll_rows: settings.mouse_scroll_rows,
                };
                match handle_event(
                    &mut context,
                    chat,
                    &mut loop_state,
                    event,
                    &mut draft_autosave,
                )
                .await
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
                if absorb_bcode_event(chat, &mut loop_state, *event)
                    || drain_bcode_events(chat, &mut loop_state)
                {
                    needs_redraw = true;
                }
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
            draft_autosave.last_saved_text = None;
            draft_autosave.dirty = true;
            draft_autosave.save_at = Some(Instant::now());
        }
    }

    loop_state.abort_all_effects();
    Ok(())
}

enum ChatLoopEvent {
    Terminal(Event),
    Bcode(Box<BcodeEvent>),
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
    needs_redraw |= maybe_start_older_history_load(chat, loop_state);
    needs_redraw |= maybe_start_newer_history_load(chat, loop_state);
    loop_state.maybe_start_permission_poll(chat);
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
        TuiEffectResult::SessionOpened {
            session_id,
            has_older_history,
            result,
        } => {
            session_flow::complete_switch_session(chat, session_id, has_older_history, result);
        }
        TuiEffectResult::ConfigLoaded { config } => {
            apply_config_result(settings, chat, *config);
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
            model,
            active_skill_count,
            runtime_work,
            error,
        } => {
            apply_session_status_result(
                chat,
                session_id,
                model,
                active_skill_count,
                runtime_work,
                error,
            );
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
            apply_cancel_turn_result(chat, session_id, result);
        }
        TuiEffectResult::CycleThinkingEffort { session_id, result } => {
            apply_thinking_cycle_result(chat, session_id, *result);
        }
    }
}

fn apply_config_result(
    settings: &mut TuiRuntimeSettings,
    chat: &mut ActiveChat,
    config: Result<bcode_config::BcodeConfig, String>,
) {
    match config {
        Ok(config) => {
            settings.apply_tui_config(&config.tui);
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
    session_id: bcode_session_models::SessionId,
    model: Option<bcode_ipc::SessionModelStatus>,
    active_skill_count: Option<usize>,
    runtime_work: Option<Vec<bcode_ipc::RuntimeWorkSnapshot>>,
    error: Option<String>,
) {
    if chat.session_id != Some(session_id) {
        return;
    }
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
    if let Some(work) = runtime_work {
        chat.app.apply_runtime_work_snapshots(&work);
    }
    let skill_count = active_skill_count.unwrap_or(0);
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

fn apply_permission_list_result(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    result: Result<Vec<bcode_ipc::PermissionSummary>, ClientError>,
) {
    match result {
        Ok(permissions) => {
            loop_state.permission_poll.last_error_status = None;
            if loop_state.permission_dialog.is_none()
                && let Some(permission) = permissions
                    .into_iter()
                    .find(|permission| Some(permission.session_id) == chat.session_id)
            {
                loop_state.permission_dialog = Some(PermissionDialogState::new(permission));
            }
        }
        Err(error) => {
            let label = if error.is_daemon_unavailable() {
                loop_state.permission_poll.next_poll_at =
                    Instant::now() + PERMISSION_POLL_DAEMON_DOWN_INTERVAL;
                "Permissions unavailable"
            } else {
                "Permission check failed"
            };
            let status = daemon_issue::client_issue_status(label, &error);
            if loop_state.permission_poll.last_error_status.as_deref() != Some(&status) {
                chat.app.set_status(status.clone());
            }
            loop_state.permission_poll.last_error_status = Some(status);
        }
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
            chat.app.set_status(format!("worktree: {}", path.display()));
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
    chat.app
        .push_system_note(format!("Created worktree\n* Path: {}", path.display()));
    chat.app.set_status("created worktree".to_owned());
}

fn apply_cancel_runtime_work_result(
    chat: &mut ActiveChat,
    work_id: &bcode_session_models::RuntimeWorkId,
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

fn apply_cancel_turn_result(
    chat: &mut ActiveChat,
    session_id: bcode_session_models::SessionId,
    result: Result<bool, ClientError>,
) {
    match result {
        Ok(true) if Some(session_id) == chat.app.session_id() => {
            chat.app
                .set_status("turn cancellation requested".to_owned());
        }
        Ok(false) if Some(session_id) == chat.app.session_id() => {
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

fn next_redraw_at(last_redraw: Instant) -> Instant {
    last_redraw
        .checked_add(TARGET_FRAME_INTERVAL)
        .unwrap_or(last_redraw)
}

fn draw_chat_frame<W: Write>(
    terminal: &mut Terminal<&mut W>,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
) -> Result<(), TuiError> {
    let layout = render::prepare_frame(&mut chat.app, terminal.area());
    let theme = render::TuiTheme::for_app(&chat.app);
    let transcript_area = layout.map_or_else(
        || render::transcript_area_for_frame(&chat.app, terminal.area()),
        |layout| layout.transcript_area(&chat.app),
    );
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
        if let Some(surface) = &mut loop_state.protocol_surface
            && let Some(surface_area) =
                protocol_surface_area(chat, surface.interaction_id(), transcript_area)
        {
            surface.render(surface_area, frame);
        }
        render_resolved_protocol_surfaces(chat, transcript_area, frame);
    })?;
    Ok(())
}

fn render_resolved_protocol_surfaces(
    chat: &ActiveChat,
    viewport: Rect,
    frame: &mut bmux_tui::frame::Frame<'_>,
) {
    let layout = chat.app.transcript_layout();
    for visible in layout.visible_lines_from_top(
        chat.app.transcript_top_row(viewport.height),
        viewport.height,
    ) {
        if visible.source() != VisibleTranscriptSource::Transcript
            || visible.row_in_entry() != INLINE_PROTOCOL_SURFACE_ROW_OFFSET
        {
            continue;
        }
        let Some(item) = chat.app.transcript().get(visible.entry_index()) else {
            continue;
        };
        let super::transcript::TranscriptItemKind::ToolProtocolPresentation {
            tree_json,
            state_json,
            ..
        } = item.kind()
        else {
            continue;
        };
        let Some(mut surface) = super::protocol_surface::ResolvedProtocolSurface::from_tree_json(
            tree_json,
            state_json.as_deref(),
        ) else {
            continue;
        };
        let viewport_row = visible
            .row_index
            .saturating_sub(chat.app.transcript_top_row(viewport.height));
        let y = viewport
            .y
            .saturating_add(u16::try_from(viewport_row).unwrap_or(u16::MAX));
        let height = super::protocol_surface::measure_tree_json_height(tree_json, viewport.width)
            .min(viewport.bottom().saturating_sub(y));
        if height > 0 {
            surface.render(Rect::new(viewport.x, y, viewport.width, height), frame);
        }
    }
}

fn protocol_surface_area(chat: &ActiveChat, interaction_id: &str, viewport: Rect) -> Option<Rect> {
    let layout = chat.app.transcript_layout();
    for visible in layout.visible_lines_from_top(
        chat.app.transcript_top_row(viewport.height),
        viewport.height,
    ) {
        if visible.source() != VisibleTranscriptSource::Transcript
            || visible.row_in_entry() != INLINE_PROTOCOL_SURFACE_ROW_OFFSET
        {
            continue;
        }
        let Some(item) = chat.app.transcript().get(visible.entry_index()) else {
            continue;
        };
        let super::transcript::TranscriptItemKind::InteractiveToolRequest {
            interaction_id: item_interaction_id,
            surface_kind,
            request_json,
            ..
        } = item.kind()
        else {
            continue;
        };
        if item_interaction_id == interaction_id && surface_kind == BMUX_PROTOCOL_INLINE_SURFACE {
            let viewport_row = visible
                .row_index
                .saturating_sub(chat.app.transcript_top_row(viewport.height));
            let y = viewport
                .y
                .saturating_add(u16::try_from(viewport_row).unwrap_or(u16::MAX));
            let height =
                super::protocol_surface::measure_tree_json_height(request_json, viewport.width)
                    .min(viewport.bottom().saturating_sub(y));
            return (height > 0).then_some(Rect::new(viewport.x, y, viewport.width, height));
        }
    }
    None
}

fn drain_bcode_events(chat: &mut ActiveChat, loop_state: &mut ChatLoopState) -> bool {
    let mut needs_redraw = false;
    while let Ok(event) = chat.event_receiver.try_recv() {
        needs_redraw |= absorb_bcode_event(chat, loop_state, event);
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
            if let SessionEventKind::AgentChanged { agent_id } = &event.kind {
                chat.agents
                    .apply_agent_to_app(&mut chat.app, agent_id.clone());
            } else {
                maybe_open_protocol_surface(loop_state, &event.kind);
                chat.app.absorb_session_event(&event);
            }
            true
        }
        BcodeEvent::SessionLive(event) if Some(event.session_id) == chat.session_id => {
            chat.app.absorb_session_live_event(&event);
            true
        }
        BcodeEvent::Session(_)
        | BcodeEvent::SessionLive(_)
        | BcodeEvent::RuntimeWork(_)
        | BcodeEvent::SessionCatalogUpdated { .. } => false,
    }
}

fn maybe_open_protocol_surface(loop_state: &mut ChatLoopState, event: &SessionEventKind) {
    let SessionEventKind::InteractiveToolRequestCreated {
        interaction_id,
        surface_kind,
        request_json,
        ..
    } = event
    else {
        return;
    };
    if surface_kind != BMUX_PROTOCOL_INLINE_SURFACE {
        return;
    }
    loop_state.protocol_surface =
        ProtocolSurfaceState::from_request(interaction_id.clone(), request_json);
}

async fn next_chat_loop_event(
    terminal_events: &mut TuiInput,
    invalidation_queue: &mut InvalidationQueue,
    chat: &mut ActiveChat,
    redraw_at: Option<Instant>,
    draft_save_at: Option<Instant>,
) -> Result<ChatLoopEvent, TuiError> {
    let now = Instant::now();
    let due = invalidation_queue.take_due(now);
    if !due.is_empty() {
        return Ok(ChatLoopEvent::TimedInvalidations(due));
    }
    let next_timer_at = [invalidation_queue.next_at(), redraw_at, draft_save_at]
        .into_iter()
        .flatten()
        .min();
    if let Some(next_at) = next_timer_at {
        let delay = next_at.saturating_duration_since(now);
        return tokio::select! {
            biased;
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
        biased;
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

#[allow(clippy::future_not_send)]
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
            Ok(true)
        }
        Event::Key(stroke)
            if loop_state.protocol_surface.is_some()
                && protocol_surface_host_key(context.services.keymap, stroke).is_some() =>
        {
            handle_protocol_surface_host_key(context, chat, loop_state, stroke).await
        }
        event @ Event::Key(_) if loop_state.protocol_surface.is_some() => {
            handle_protocol_surface_event(context, chat, loop_state, event).await
        }
        event @ (Event::Paste(_) | Event::Focus(_) | Event::Tick | Event::Mouse(_))
            if loop_state.protocol_surface.is_some() =>
        {
            handle_protocol_surface_event(context, chat, loop_state, event).await
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

#[allow(clippy::future_not_send)]
async fn handle_protocol_surface_host_key<W: Write>(
    context: &ChatEventContext<'_, '_, W>,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    stroke: KeyStroke,
) -> Result<bool, TuiError> {
    match protocol_surface_host_key(context.services.keymap, stroke) {
        Some(BmuxAction::AppExit) => {
            chat.app.request_exit();
            Ok(true)
        }
        Some(BmuxAction::AppInterrupt) => {
            resolve_protocol_surface_dismissed(context, chat, loop_state).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn protocol_surface_host_key(keymap: &BmuxKeyMap, stroke: KeyStroke) -> Option<BmuxAction> {
    match keymap.action_for_key(BmuxScope::Chat, stroke)? {
        BmuxAction::AppExit => Some(BmuxAction::AppExit),
        BmuxAction::AppInterrupt => Some(BmuxAction::AppInterrupt),
        _ => None,
    }
}

#[allow(clippy::future_not_send)]
async fn resolve_protocol_surface_dismissed<W: Write>(
    context: &ChatEventContext<'_, '_, W>,
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
) -> Result<(), TuiError> {
    let Some(surface) = loop_state.protocol_surface.take() else {
        return Ok(());
    };
    let interaction_id = surface.interaction_id().to_owned();
    context
        .services
        .client
        .resolve_interactive_tool_request(
            interaction_id,
            ProtocolSurfaceState::dismissed_resolution(),
        )
        .await?;
    chat.app
        .set_status("interactive request dismissed".to_owned());
    Ok(())
}

#[allow(clippy::future_not_send)]
async fn handle_protocol_surface_event<W: Write>(
    context: &ChatEventContext<'_, '_, W>,
    chat: &ActiveChat,
    loop_state: &mut ChatLoopState,
    event: Event,
) -> Result<bool, TuiError> {
    let Some(surface) = &mut loop_state.protocol_surface else {
        return Ok(false);
    };
    let Some(surface_area) = protocol_surface_area(
        chat,
        surface.interaction_id(),
        render::transcript_area_for_frame(&chat.app, context.terminal.area()),
    ) else {
        return Ok(true);
    };
    if let Some(resolution) = surface.handle_event(surface_area, &event) {
        let interaction_id = surface.interaction_id().to_owned();
        loop_state.protocol_surface = None;
        context
            .services
            .client
            .resolve_interactive_tool_request(interaction_id, resolution)
            .await?;
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
    let changed = match stroke.key {
        KeyCode::Char(']') if stroke.modifiers.is_empty() => chat.app.select_next_diff_file(),
        KeyCode::Char('[') if stroke.modifiers.is_empty() => chat.app.select_previous_diff_file(),
        _ => false,
    };
    if changed {
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
                artifact.model.display()
            ));
        }
        Err(error) => {
            chat.app.set_status(format!("image paste failed: {error}"));
        }
    }
}
