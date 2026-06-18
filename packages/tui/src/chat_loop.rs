//! Main chat event loop for the TUI.

use std::io::Write;
use std::time::{Duration, Instant, SystemTime};

use bcode_client::{BcodeClient, ClientError};
use bcode_ipc::{ComposerDraftScope, Event as BcodeEvent};
use bcode_session_models::SessionEventKind;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;

use super::activity::ActivityState;
use super::clipboard_image;
use super::command_palette::BmuxCommandPalette;
use super::helpers;
use super::invalidation::InvalidationQueue;
use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use super::permission_dialog::PermissionDialogState;
use super::runtime_context::{TuiIo, TuiServices};
use super::session_flow::{self, ActiveChat};
use super::terminal_events::TuiInput;
use super::{
    TuiError, command_palette_render, composer_flow, history_flow, input, input::KeyRequest,
    mouse_flow, palette_flow, permission_dialog_render, permission_flow, render, slash_flow,
    slash_palette, slash_palette_render, thinking_dialog_render, thinking_flow,
    timeline_dialog_render, timeline_flow,
};

const TARGET_FRAME_INTERVAL: Duration = Duration::from_millis(16);
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

    async fn save_now(&mut self, client: &BcodeClient, chat: &ActiveChat) {
        if !self.dirty && self.last_saved_text.as_deref() == Some(chat.app.composer().text()) {
            return;
        }
        let text = chat.app.composer().text().to_owned();
        let scope = self.scope(chat);
        if Self::save_scope_text(client, scope, text.clone()).await {
            self.last_saved_text = Some(text);
            self.dirty = false;
            self.save_at = None;
        }
    }

    async fn clear_scope(client: &BcodeClient, scope: ComposerDraftScope) {
        let _ = Self::save_scope_text(client, scope, String::new()).await;
    }

    async fn save_scope_text(
        client: &BcodeClient,
        scope: ComposerDraftScope,
        text: String,
    ) -> bool {
        client.set_composer_draft(scope, text).await.is_ok()
    }

    fn mark_dirty_now(&mut self) {
        self.dirty = true;
        self.save_at = Some(Instant::now());
    }
}

struct ModalState {
    palette: Option<BmuxCommandPalette>,
    slash_palette: Option<slash_palette::SlashPalette>,
    permission_dialog: Option<PermissionDialogState>,
    thinking_dialog: Option<super::thinking_dialog::ThinkingDialogState>,
    timeline_dialog: Option<super::timeline_dialog::TimelineDialogState>,
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
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    mouse_scroll_rows: usize,
    launch_working_directory: std::path::PathBuf,
) -> Result<(), TuiError> {
    let mut modals = ModalState {
        palette: None,
        slash_palette: None,
        permission_dialog: None,
        thinking_dialog: None,
        timeline_dialog: None,
    };
    sync_chat_key_labels(chat, keymap);
    let mut draft_autosave = DraftAutosave::new(
        launch_working_directory,
        chat.app.composer().text().to_owned(),
    );
    let mut invalidation_queue = InvalidationQueue::default();
    refresh_invalidation_queue(chat, &mut invalidation_queue);
    let mut needs_redraw = true;
    let mut last_redraw = Instant::now()
        .checked_sub(TARGET_FRAME_INTERVAL)
        .unwrap_or_else(Instant::now);

    while !chat.app.should_exit() {
        sync_chat_key_labels(chat, keymap);
        if drain_bcode_events(chat) {
            needs_redraw = true;
        }

        if handle_loop_housekeeping(client, chat, &mut modals).await? {
            needs_redraw = true;
        }

        if helpers::resize_from_terminal(terminal)? {
            needs_redraw = true;
        }

        draft_autosave.observe(chat, Instant::now());
        if let Some(save_at) = draft_autosave.next_save_at()
            && Instant::now() >= save_at
        {
            draft_autosave.save_now(client, chat).await;
        }

        let redraw_at = next_redraw_at(last_redraw);
        if needs_redraw && Instant::now() >= redraw_at {
            draw_chat_frame(terminal, chat, &mut modals)?;
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
                        keymap,
                        theme: render::TuiTheme::for_app(&mut chat.app, Instant::now()),
                    },
                    terminal,
                    terminal_events,
                    mouse_scroll_rows,
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
                handle_async_event(client, chat, event);
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

    draft_autosave.save_now(client, chat).await;
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
    modals: &mut ModalState,
) -> Result<bool, TuiError> {
    let mut needs_redraw = false;
    if chat.app.should_load_older_history() {
        match history_flow::load_older_history(client, chat).await {
            Ok(()) => needs_redraw = true,
            Err(error) if is_nonfatal_tui_daemon_error(&error) => {
                report_nonfatal_tui_error(chat, "Older history unavailable", &error);
                needs_redraw = true;
            }
            Err(error) => return Err(error),
        }
    }

    if modals.permission_dialog.is_none() {
        match client.list_permissions().await {
            Ok(permissions) => {
                if let Some(permission) = permissions
                    .into_iter()
                    .find(|permission| Some(permission.session_id) == chat.session_id)
                {
                    modals.permission_dialog = Some(PermissionDialogState::new(permission));
                    needs_redraw = true;
                }
            }
            Err(error) if error.is_daemon_unavailable() => {
                report_nonfatal_client_error(chat, "Permissions unavailable", &error);
                needs_redraw = true;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(needs_redraw)
}

fn is_nonfatal_tui_daemon_error(error: &TuiError) -> bool {
    matches!(error, TuiError::Client(error) if error.is_daemon_unavailable())
}

fn report_nonfatal_tui_error(chat: &mut ActiveChat, label: &str, error: &TuiError) {
    chat.app.set_status(format!("{label}: {error}"));
}

fn report_nonfatal_client_error(chat: &mut ActiveChat, label: &str, error: &ClientError) {
    chat.app.set_status(format!("{label}: {error}"));
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
            slash_flow::update_slash_palette(
                context.services.client,
                chat,
                &mut modals.slash_palette,
            )
            .await;
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
        slash_flow::update_slash_palette(context.services.client, chat, &mut modals.slash_palette)
            .await;
        return Ok(true);
    }
    let outcome = input::handle_key(&mut chat.app, context.services.keymap, stroke);
    slash_flow::update_slash_palette(context.services.client, chat, &mut modals.slash_palette)
        .await;
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
        KeyRequest::Interrupt => request_turn_cancellation(context.services.client, chat).await,
        KeyRequest::CycleAgent => cycle_session_agent(context.services.client, chat).await,
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
                    DraftAutosave::clear_scope(context.services.client, scope).await;
                }
                autosave.mark_dirty_now();
                autosave.save_now(context.services.client, chat).await;
            }
        }
    }
    Ok(())
}

async fn request_turn_cancellation(client: &BcodeClient, chat: &mut ActiveChat) {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return;
    };
    match client.cancel_session_turn(session_id).await {
        Ok(true) => {
            chat.app.set_cancelling();
            chat.app
                .set_status("turn cancellation requested".to_owned());
        }
        Ok(false) => {
            chat.app.set_idle();
            chat.app.set_status("no active turn".to_owned());
        }
        Err(error) => {
            chat.app.set_idle();
            chat.app.set_status(format!("cancel failed: {error}"));
        }
    }
}

fn agent_selection_status(chat: &ActiveChat, agent_name: &str) -> String {
    if matches!(chat.app.activity(), ActivityState::Idle) {
        format!("agent {agent_name} selected")
    } else {
        format!("agent {agent_name} selected for next message")
    }
}

async fn cycle_session_agent(client: &BcodeClient, chat: &mut ActiveChat) {
    if chat.agents.is_empty() {
        match session_flow::AgentCatalog::load(client).await {
            Ok(agents) => {
                chat.agents = agents;
                chat.agents.refresh_app_agent_metadata(&mut chat.app);
            }
            Err(error) if is_nonfatal_tui_daemon_error(&error) => {
                report_nonfatal_tui_error(chat, "Agent metadata unavailable", &error);
                return;
            }
            Err(error) => {
                chat.app
                    .set_status(format!("agent metadata failed: {error}"));
                return;
            }
        }
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
