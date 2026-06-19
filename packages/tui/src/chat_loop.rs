//! Main chat event loop for the TUI.

use std::io::Write;
use std::time::{Duration, Instant, SystemTime};

use bcode_client::{BcodeClient, ClientError};
use bcode_config::TuiConfig;
use bcode_ipc::{ComposerDraftScope, Event as BcodeEvent, PermissionSummary};
use bcode_session_models::{
    SessionEventKind, SessionHistoryCursor, SessionHistoryDirection, SessionHistoryPage,
    SessionHistoryQuery, SessionId,
};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;

use super::activity::ActivityState;
use super::clipboard_image;
use super::command_palette::BmuxCommandPalette;
use super::daemon_issue;
use super::helpers;
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

struct ModalState {
    palette: Option<BmuxCommandPalette>,
    slash_palette: Option<slash_palette::SlashPalette>,
    slash_palette_load: AsyncSlashPaletteLoad,
    draft_save: AsyncDraftSave,
    cancel_turn: AsyncCancelTurn,
    permission_dialog: Option<PermissionDialogState>,
    permission_poll: AsyncPermissionPoll,
    older_history_load: AsyncHistoryPageLoad,
    newer_history_load: AsyncHistoryPageLoad,
    thinking_dialog: Option<super::thinking_dialog::ThinkingDialogState>,
    timeline_dialog: Option<super::timeline_dialog::TimelineDialogState>,
}

#[derive(Debug)]
struct AsyncCancelTurn {
    task: Option<tokio::task::JoinHandle<Result<bool, ClientError>>>,
    session_id: Option<SessionId>,
}

impl AsyncCancelTurn {
    const fn new() -> Self {
        Self {
            task: None,
            session_id: None,
        }
    }
}

#[derive(Debug)]
struct AsyncDraftSave {
    task: Option<tokio::task::JoinHandle<Result<String, ClientError>>>,
    pending: Option<(ComposerDraftScope, String)>,
}

impl AsyncDraftSave {
    const fn new() -> Self {
        Self {
            task: None,
            pending: None,
        }
    }
}

#[derive(Debug)]
struct AsyncPermissionPoll {
    task: Option<tokio::task::JoinHandle<Result<Vec<PermissionSummary>, ClientError>>>,
    next_poll_at: Instant,
    last_error_status: Option<String>,
}

impl AsyncPermissionPoll {
    const fn new(now: Instant) -> Self {
        Self {
            task: None,
            next_poll_at: now,
            last_error_status: None,
        }
    }
}

#[derive(Debug)]
struct AsyncSlashPaletteLoad {
    task: Option<tokio::task::JoinHandle<slash_palette::SlashPalette>>,
    query: Option<String>,
}

impl AsyncSlashPaletteLoad {
    const fn new() -> Self {
        Self {
            task: None,
            query: None,
        }
    }
}

#[derive(Debug)]
struct AsyncHistoryPageLoad {
    task: Option<tokio::task::JoinHandle<Result<SessionHistoryPage, ClientError>>>,
    session_id: Option<SessionId>,
    cursor: Option<SessionHistoryCursor>,
}

impl AsyncHistoryPageLoad {
    const fn new() -> Self {
        Self {
            task: None,
            session_id: None,
            cursor: None,
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
#[allow(clippy::too_many_lines)]
pub async fn run_with_client<W: Write>(
    terminal: &mut Terminal<&mut W>,
    terminal_events: &mut TuiInput,
    client: &BcodeClient,
    settings: &mut TuiRuntimeSettings,
    chat: &mut ActiveChat,
    startup_action: super::startup_action::StartupTuiAction,
) -> Result<(), TuiError> {
    let mut modals = ModalState {
        palette: None,
        slash_palette: None,
        slash_palette_load: AsyncSlashPaletteLoad::new(),
        draft_save: AsyncDraftSave::new(),
        cancel_turn: AsyncCancelTurn::new(),
        permission_dialog: None,
        permission_poll: AsyncPermissionPoll::new(Instant::now()),
        older_history_load: AsyncHistoryPageLoad::new(),
        newer_history_load: AsyncHistoryPageLoad::new(),
        thinking_dialog: None,
        timeline_dialog: None,
    };
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
        if drain_bcode_events(chat) {
            needs_redraw = true;
        }

        if handle_loop_housekeeping(client, chat, &mut draft_autosave, &mut modals).await {
            needs_redraw = true;
        }

        if helpers::resize_from_terminal(terminal)? {
            needs_redraw = true;
        }

        draft_autosave.observe(chat, Instant::now());
        if let Some(save_at) = draft_autosave.next_save_at()
            && Instant::now() >= save_at
        {
            start_draft_save(client, chat, &mut draft_autosave, &mut modals.draft_save);
        }

        let redraw_at = next_redraw_at(last_redraw);
        if needs_redraw && Instant::now() >= redraw_at {
            draw_chat_frame(terminal, chat, &mut modals)?;
            if let Some(action) = startup_action.take()
                && action == super::startup_action::StartupTuiAction::OpenRalphHome
            {
                let mut io = TuiIo {
                    terminal,
                    input: terminal_events,
                };
                let services = TuiServices {
                    client,
                    keymap: &settings.keymap,
                    theme: render::TuiTheme::for_app(&mut chat.app, Instant::now()),
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
                        keymap: &settings.keymap,
                        theme: render::TuiTheme::for_app(&mut chat.app, Instant::now()),
                    },
                    terminal,
                    terminal_events,
                    mouse_scroll_rows: settings.mouse_scroll_rows,
                };
                match handle_event(&mut context, chat, &mut modals, event, &mut draft_autosave)
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
                if absorb_bcode_event(chat, *event) || drain_bcode_events(chat) {
                    needs_redraw = true;
                }
            }
            ChatLoopEvent::Async(event) => {
                handle_async_event(client, settings, chat, event);
                needs_redraw = true;
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

    if let Some(task) = modals.draft_save.task.take() {
        task.abort();
    }
    if let Some(task) = modals.cancel_turn.task.take() {
        task.abort();
    }
    Ok(())
}

enum ChatLoopEvent {
    Terminal(Event),
    Bcode(Box<BcodeEvent>),
    Async(Box<session_flow::ChatAsyncEvent>),
    TimedInvalidations(Vec<super::invalidation::InvalidationKey>),
    Timer,
}

async fn handle_loop_housekeeping(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    draft_autosave: &mut DraftAutosave,
    modals: &mut ModalState,
) -> bool {
    let mut needs_redraw = false;
    needs_redraw |= poll_older_history_load(chat, &mut modals.older_history_load).await;
    needs_redraw |= poll_newer_history_load(chat, &mut modals.newer_history_load).await;
    needs_redraw |= poll_draft_save(client, chat, draft_autosave, &mut modals.draft_save).await;
    needs_redraw |= poll_cancel_turn(chat, &mut modals.cancel_turn).await;
    needs_redraw |= maybe_start_older_history_load(client, chat, &mut modals.older_history_load);
    needs_redraw |= maybe_start_newer_history_load(client, chat, &mut modals.newer_history_load);
    needs_redraw |= poll_slash_palette_load(chat, modals).await;
    needs_redraw |= poll_permission_list(chat, modals).await;
    maybe_start_permission_poll(client, chat, modals);
    needs_redraw
}

fn maybe_start_older_history_load(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    load: &mut AsyncHistoryPageLoad,
) -> bool {
    if load.task.is_some() || !chat.app.should_load_older_history() {
        return false;
    }
    let Some(cursor) = chat.app.older_history_cursor() else {
        return false;
    };
    let Some(session_id) = chat.session_id else {
        return false;
    };
    chat.app.set_loading_older_history(true);
    load.session_id = Some(session_id);
    load.cursor = Some(cursor);
    let client = client.clone();
    load.task = Some(tokio::spawn(async move {
        client
            .session_history_page(
                session_id,
                SessionHistoryQuery {
                    cursor: Some(cursor),
                    limit: super::OLDER_HISTORY_EVENT_LIMIT,
                    direction: SessionHistoryDirection::Backward,
                },
            )
            .await
    }));
    true
}

async fn poll_older_history_load(chat: &mut ActiveChat, load: &mut AsyncHistoryPageLoad) -> bool {
    let Some(task) = load.task.take_if(|task| task.is_finished()) else {
        return false;
    };
    let request_session_id = load.session_id.take();
    load.cursor = None;
    match task.await {
        Ok(Ok(page)) if request_session_id == chat.session_id => {
            chat.app.prepend_older_history(&page.events, page.has_more);
        }
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            chat.app.set_loading_older_history(false);
            report_nonfatal_client_error(chat, "Older history unavailable", &error);
        }
        Err(error) => {
            chat.app.set_loading_older_history(false);
            chat.app
                .set_status(format!("Older history task failed: {error}"));
        }
    }
    true
}

fn maybe_start_newer_history_load(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    load: &mut AsyncHistoryPageLoad,
) -> bool {
    if load.task.is_some() || !chat.app.should_load_newer_history() {
        return false;
    }
    let Some(cursor) = chat.app.newer_history_cursor() else {
        return false;
    };
    let Some(session_id) = chat.session_id else {
        return false;
    };
    chat.app.set_loading_newer_history(true);
    load.session_id = Some(session_id);
    load.cursor = Some(cursor);
    let client = client.clone();
    load.task = Some(tokio::spawn(async move {
        client
            .session_history_page(
                session_id,
                SessionHistoryQuery {
                    cursor: Some(cursor),
                    limit: super::OLDER_HISTORY_EVENT_LIMIT,
                    direction: SessionHistoryDirection::Forward,
                },
            )
            .await
    }));
    true
}

async fn poll_newer_history_load(chat: &mut ActiveChat, load: &mut AsyncHistoryPageLoad) -> bool {
    let Some(task) = load.task.take_if(|task| task.is_finished()) else {
        return false;
    };
    let request_session_id = load.session_id.take();
    load.cursor = None;
    match task.await {
        Ok(Ok(page)) if request_session_id == chat.session_id => {
            chat.app.append_newer_history(&page.events, page.has_more);
        }
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            chat.app.set_loading_newer_history(false);
            report_nonfatal_client_error(chat, "Newer history unavailable", &error);
        }
        Err(error) => {
            chat.app.set_loading_newer_history(false);
            chat.app
                .set_status(format!("Newer history task failed: {error}"));
        }
    }
    true
}

fn start_cancel_turn(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    cancel_turn: &mut AsyncCancelTurn,
) {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return;
    };
    if cancel_turn.task.is_some() {
        chat.app
            .set_status("turn cancellation already requested".to_owned());
        return;
    }
    chat.app.set_cancelling();
    chat.app
        .set_status("turn cancellation requested".to_owned());
    let client = client.clone();
    cancel_turn.session_id = Some(session_id);
    cancel_turn.task = Some(tokio::spawn(async move {
        client.cancel_session_turn(session_id).await
    }));
}

async fn poll_cancel_turn(chat: &mut ActiveChat, cancel_turn: &mut AsyncCancelTurn) -> bool {
    let Some(task) = cancel_turn.task.take_if(|task| task.is_finished()) else {
        return false;
    };
    let session_id = cancel_turn.session_id.take();
    match task.await {
        Ok(Ok(true)) if session_id == chat.app.session_id() => {
            chat.app
                .set_status("turn cancellation requested".to_owned());
        }
        Ok(Ok(false)) if session_id == chat.app.session_id() => {
            chat.app.set_idle();
            chat.app.set_status("no active turn".to_owned());
        }
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            if session_id == chat.app.session_id() {
                chat.app.set_idle();
            }
            report_nonfatal_client_error(chat, "Cancel unavailable", &error);
        }
        Err(error) => {
            if session_id == chat.app.session_id() {
                chat.app.set_idle();
            }
            chat.app
                .set_status(format!("Cancel turn task failed: {error}"));
        }
    }
    true
}

fn start_draft_save(
    client: &BcodeClient,
    chat: &ActiveChat,
    draft_autosave: &mut DraftAutosave,
    draft_save: &mut AsyncDraftSave,
) {
    let Some(request) = draft_autosave.pending_save(chat) else {
        return;
    };
    draft_autosave.mark_save_started();
    if draft_save.task.is_some() {
        draft_save.pending = Some(request);
        return;
    }
    start_draft_save_request(client, draft_save, request);
}

fn start_draft_save_request(
    client: &BcodeClient,
    draft_save: &mut AsyncDraftSave,
    request: (ComposerDraftScope, String),
) {
    let client = client.clone();
    let (scope, text) = request;
    draft_save.task = Some(tokio::spawn(async move {
        client.set_composer_draft(scope, text.clone()).await?;
        Ok(text)
    }));
}

async fn poll_draft_save(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    draft_autosave: &mut DraftAutosave,
    draft_save: &mut AsyncDraftSave,
) -> bool {
    let Some(task) = draft_save.task.take_if(|task| task.is_finished()) else {
        return false;
    };
    match task.await {
        Ok(Ok(saved_text)) => draft_autosave.mark_save_completed(saved_text),
        Ok(Err(error)) => report_nonfatal_client_error(chat, "Draft autosave unavailable", &error),
        Err(error) => chat
            .app
            .set_status(format!("Draft autosave task failed: {error}")),
    }
    if let Some(request) = draft_save.pending.take() {
        start_draft_save_request(client, draft_save, request);
    }
    true
}

fn update_slash_palette_async(
    client: &BcodeClient,
    chat: &ActiveChat,
    modals: &mut ModalState,
) -> bool {
    let current_query = chat.app.composer().text();
    if !current_query.starts_with('/') {
        modals.slash_palette = None;
        if let Some(task) = modals.slash_palette_load.task.take() {
            task.abort();
        }
        modals.slash_palette_load.query = None;
        return true;
    }
    if modals.slash_palette_load.query.as_deref() == Some(current_query) {
        return true;
    }
    let previous = modals
        .slash_palette
        .as_ref()
        .filter(|palette| palette.query() == current_query)
        .and_then(|palette| palette.selected_command().map(str::to_owned));
    if previous.is_none() {
        modals.slash_palette = None;
    }
    if let Some(task) = modals.slash_palette_load.task.take() {
        task.abort();
    }
    let client = client.clone();
    let query = current_query.to_owned();
    let session_id = chat.app.session_id();
    modals.slash_palette_load.query = Some(query.clone());
    modals.slash_palette_load.task = Some(tokio::spawn(async move {
        let mut palette = slash_palette::SlashPalette::new(&client, session_id, &query).await;
        if let Some(previous) = previous {
            palette.select_command(&previous);
        }
        palette
    }));
    true
}

async fn poll_slash_palette_load(chat: &mut ActiveChat, modals: &mut ModalState) -> bool {
    let Some(task) = modals
        .slash_palette_load
        .task
        .take_if(|task| task.is_finished())
    else {
        return false;
    };
    let query = modals.slash_palette_load.query.take();
    match task.await {
        Ok(palette) if query.as_deref() == Some(chat.app.composer().text()) => {
            modals.slash_palette = (!palette.is_empty()).then_some(palette);
        }
        Ok(_stale) => {}
        Err(error) => {
            if !error.is_cancelled() {
                chat.app
                    .set_status(format!("slash command load failed: {error}"));
            }
        }
    }
    true
}

fn maybe_start_permission_poll(client: &BcodeClient, chat: &ActiveChat, modals: &mut ModalState) {
    if modals.permission_dialog.is_some()
        || modals.permission_poll.task.is_some()
        || Instant::now() < modals.permission_poll.next_poll_at
        || chat.session_id.is_none()
    {
        return;
    }
    let client = client.clone();
    modals.permission_poll.task =
        Some(tokio::spawn(async move { client.list_permissions().await }));
}

async fn poll_permission_list(chat: &mut ActiveChat, modals: &mut ModalState) -> bool {
    let Some(task) = modals
        .permission_poll
        .task
        .take_if(|task| task.is_finished())
    else {
        return false;
    };
    modals.permission_poll.next_poll_at = Instant::now() + PERMISSION_POLL_INTERVAL;
    match task.await {
        Ok(Ok(permissions)) => {
            modals.permission_poll.last_error_status = None;
            if modals.permission_dialog.is_none()
                && let Some(permission) = permissions
                    .into_iter()
                    .find(|permission| Some(permission.session_id) == chat.session_id)
            {
                modals.permission_dialog = Some(PermissionDialogState::new(permission));
                return true;
            }
        }
        Ok(Err(error)) => {
            let label = if error.is_daemon_unavailable() {
                modals.permission_poll.next_poll_at =
                    Instant::now() + PERMISSION_POLL_DAEMON_DOWN_INTERVAL;
                "Permissions unavailable"
            } else {
                "Permission check failed"
            };
            let status = daemon_issue::client_issue_status(label, &error);
            let changed = modals.permission_poll.last_error_status.as_deref() != Some(&status);
            modals.permission_poll.last_error_status = Some(status.clone());
            chat.app.set_status(status);
            return changed;
        }
        Err(error) => {
            chat.app
                .set_status(format!("Permission check task failed: {error}"));
            return true;
        }
    }
    false
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
    modals: &mut ModalState,
) -> Result<(), TuiError> {
    let layout = render::prepare_frame(&mut chat.app, terminal.area());
    let theme = render::TuiTheme::for_app(&mut chat.app, Instant::now());
    terminal.draw(|frame| {
        if let Some(layout) = layout {
            render::render_prepared(&mut chat.app, frame, layout);
        }
        if let Some(slash_palette) = &modals.slash_palette {
            slash_palette_render::render_palette(
                slash_palette,
                chat.app.composer_content_area(),
                frame,
                theme,
            );
        }
        if let Some(palette) = &mut modals.palette {
            command_palette_render::render_palette(palette, frame, theme);
        }
        if let Some(dialog) = &modals.permission_dialog {
            permission_dialog_render::render_permission_dialog(dialog, frame);
        }
        if let Some(dialog) = &modals.thinking_dialog {
            thinking_dialog_render::render_thinking_dialog(dialog, frame, theme);
        }
        if let Some(dialog) = &mut modals.timeline_dialog {
            timeline_dialog_render::render_timeline_dialog(dialog, frame, theme);
        }
    })?;
    Ok(())
}

fn drain_bcode_events(chat: &mut ActiveChat) -> bool {
    let mut needs_redraw = false;
    while let Ok(event) = chat.event_receiver.try_recv() {
        needs_redraw |= absorb_bcode_event(chat, event);
    }
    needs_redraw
}

fn absorb_bcode_event(chat: &mut ActiveChat, event: BcodeEvent) -> bool {
    match event {
        BcodeEvent::Session(event) if Some(event.session_id) == chat.session_id => {
            if let SessionEventKind::AgentChanged { agent_id } = &event.kind {
                chat.agents
                    .apply_agent_to_app(&mut chat.app, agent_id.clone());
            } else {
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
            async_event = chat.async_event_receiver.recv() => Ok(async_event.map_or_else(
                || ChatLoopEvent::TimedInvalidations(Vec::new()),
                |event| ChatLoopEvent::Async(Box::new(event)),
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
        async_event = chat.async_event_receiver.recv() => Ok(async_event.map_or_else(
            || ChatLoopEvent::TimedInvalidations(Vec::new()),
            |event| ChatLoopEvent::Async(Box::new(event)),
        )),
        event = terminal_events.recv() => event.map(|event| {
            event.map_or_else(
                || ChatLoopEvent::TimedInvalidations(Vec::new()),
                ChatLoopEvent::Terminal,
            )
        }),
    }
}

fn handle_async_event(
    client: &BcodeClient,
    settings: &mut TuiRuntimeSettings,
    chat: &mut ActiveChat,
    event: Box<session_flow::ChatAsyncEvent>,
) {
    match *event {
        session_flow::ChatAsyncEvent::SessionOpened(opened) => {
            chat.session_open_task = None;
            session_flow::complete_switch_session(client, chat, opened);
        }
        session_flow::ChatAsyncEvent::StatusHydrated(hydrated) => {
            chat.status_hydration_task = None;
            session_flow::complete_status_hydration(chat, hydrated);
        }
        session_flow::ChatAsyncEvent::DraftStatusHydrated(hydrated) => {
            chat.status_hydration_task = None;
            session_flow::complete_draft_status_hydration(chat, hydrated);
        }
        session_flow::ChatAsyncEvent::AgentCatalogHydrated(hydrated) => {
            session_flow::complete_agent_catalog_hydration(chat, hydrated);
        }
        session_flow::ChatAsyncEvent::ConfigHydrated(hydrated) => {
            session_flow::complete_config_hydration(client, settings, chat, *hydrated);
        }
        session_flow::ChatAsyncEvent::AuthSecurityHydrated(hydrated) => {
            session_flow::complete_auth_security_hydration(chat, hydrated);
        }
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

async fn handle_event<W: Write>(
    context: &mut ChatEventContext<'_, '_, W>,
    chat: &mut ActiveChat,
    modals: &mut ModalState,
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
        Event::Key(stroke) => handle_chat_key(context, chat, modals, stroke, draft_autosave).await,
        Event::Paste(text) => {
            if let Some(palette) = &mut modals.palette {
                palette.state_mut().query.insert_str(&text);
                return Ok(true);
            }
            chat.app.reset_input_history_navigation();
            chat.app.paste_composer_text(&text);
            chat.app.wake_cursor();
            update_slash_palette_async(context.services.client, chat, modals);
            Ok(true)
        }
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick => Ok(true),
        Event::Mouse(mouse) => {
            if modals.palette.is_some() {
                let (mut io, services) = context.flow_context();
                return palette_flow::handle_palette_mouse(
                    &mut io,
                    &services,
                    chat,
                    &mut modals.palette,
                    mouse,
                )
                .await;
            }
            if modals.slash_palette.is_some() {
                return Ok(slash_flow::handle_slash_palette_mouse(
                    chat,
                    &mut modals.slash_palette,
                    context.terminal,
                    mouse,
                ));
            }
            let hit_id = mouse_flow::mouse_hit_id(context.terminal.hits(), mouse);
            mouse_flow::handle_mouse(
                hit_id,
                context.services.client,
                chat,
                &mut modals.permission_dialog,
                mouse,
                context.mouse_scroll_rows,
            )
            .await
        }
        Event::User(_) => Ok(false),
    }
}

async fn handle_chat_key<W: Write>(
    context: &mut ChatEventContext<'_, '_, W>,
    chat: &mut ActiveChat,
    modals: &mut ModalState,
    stroke: KeyStroke,
    draft_autosave: &mut DraftAutosave,
) -> Result<bool, TuiError> {
    if modals.timeline_dialog.is_some() {
        return timeline_flow::handle_timeline_key(
            context.services.client,
            chat,
            &mut modals.timeline_dialog,
            stroke,
        )
        .await;
    }
    if modals.thinking_dialog.is_some() {
        return thinking_flow::handle_thinking_key(
            context.services.client,
            chat,
            &mut modals.thinking_dialog,
            stroke,
        )
        .await;
    }
    if modals.slash_palette.is_some() {
        if let Some(dialog) = {
            let (mut io, services) = context.flow_context();
            slash_flow::handle_slash_palette_key(
                &mut io,
                &services,
                chat,
                &mut modals.slash_palette,
                stroke,
            )
            .await?
            .flatten()
        } {
            apply_composer_modal_request(modals, dialog);
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
    if modals.permission_dialog.is_some() {
        return permission_flow::handle_permission_key(
            context.services.client,
            context.services.keymap,
            chat,
            &mut modals.permission_dialog,
            stroke,
        )
        .await;
    }
    if modals.palette.is_some() {
        let (mut io, services) = context.flow_context();
        return palette_flow::handle_palette_key(
            &mut io,
            &services,
            chat,
            &mut modals.palette,
            stroke,
        )
        .await;
    }
    if is_palette_open_key(context.services.keymap, stroke) {
        modals.palette = Some(BmuxCommandPalette::new());
        chat.app
            .set_status("command palette: type to filter, enter to run, esc close".to_owned());
        return Ok(true);
    }
    if is_clipboard_image_paste_key(context.services.keymap, stroke) {
        paste_clipboard_image(chat);
        update_slash_palette_async(context.services.client, chat, modals);
        return Ok(true);
    }
    let outcome = input::handle_key(&mut chat.app, context.services.keymap, stroke);
    if chat.app.should_exit() {
        return Ok(true);
    }
    update_slash_palette_async(context.services.client, chat, modals);
    handle_chat_key_request(context, chat, modals, outcome.request, Some(draft_autosave)).await?;
    Ok(outcome.redraw)
}

async fn handle_chat_key_request<W: Write>(
    context: &mut ChatEventContext<'_, '_, W>,
    chat: &mut ActiveChat,
    modals: &mut ModalState,
    request: KeyRequest,
    draft_autosave: Option<&mut DraftAutosave>,
) -> Result<(), TuiError> {
    match request {
        KeyRequest::None => {}
        KeyRequest::Interrupt => {
            start_cancel_turn(context.services.client, chat, &mut modals.cancel_turn);
        }
        KeyRequest::CycleAgent => cycle_session_agent(chat),
        KeyRequest::CycleThinkingEffort => {
            if let Err(error) =
                thinking_flow::cycle_thinking_effort(context.services.client, chat).await
            {
                helpers::report_client_error(&mut chat.app, "thinking effort failed", &error);
            }
        }
        KeyRequest::Submit { placement } => {
            let pre_submit_scope = draft_autosave.as_ref().map(|autosave| autosave.scope(chat));
            let (mut io, services) = context.flow_context();
            match composer_flow::submit_composer(&mut io, &services, chat, placement).await {
                Ok(Some(request)) => {
                    apply_composer_modal_request(modals, request);
                }
                Ok(None) => {}
                Err(error) => helpers::report_client_error(&mut chat.app, "send failed", &error),
            }
            if let Some(autosave) = draft_autosave {
                if let Some(scope) = pre_submit_scope {
                    let request = DraftAutosave::clear_scope_request(scope);
                    if modals.draft_save.task.is_some() {
                        modals.draft_save.pending = Some(request);
                    } else {
                        start_draft_save_request(
                            context.services.client,
                            &mut modals.draft_save,
                            request,
                        );
                    }
                }
                autosave.mark_dirty_now();
                start_draft_save(
                    context.services.client,
                    chat,
                    autosave,
                    &mut modals.draft_save,
                );
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
    modals: &mut ModalState,
    request: composer_flow::ComposerModalRequest,
) {
    match request {
        composer_flow::ComposerModalRequest::Thinking(dialog) => {
            modals.thinking_dialog = Some(dialog);
        }
        composer_flow::ComposerModalRequest::Timeline(dialog) => {
            modals.timeline_dialog = Some(dialog);
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
    match clipboard_image::save_clipboard_image(chat.app.session_id()) {
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
