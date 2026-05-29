//! Main chat event loop for the TUI.

use std::io::Write;
use std::time::{Instant, SystemTime};

use bcode_agent_profile::AgentInfo;
use bcode_client::BcodeClient;
use bcode_ipc::Event as BcodeEvent;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;

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
};

struct ModalState {
    palette: Option<BmuxCommandPalette>,
    slash_palette: Option<slash_palette::SlashPalette>,
    permission_dialog: Option<PermissionDialogState>,
    thinking_dialog: Option<super::thinking_dialog::ThinkingDialogState>,
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
pub async fn run_with_client<W: Write>(
    terminal: &mut Terminal<&mut W>,
    terminal_events: &mut TuiInput,
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    mouse_scroll_rows: usize,
) -> Result<(), TuiError> {
    let mut modals = ModalState {
        palette: None,
        slash_palette: None,
        permission_dialog: None,
        thinking_dialog: None,
    };
    chat.app.set_key_hints(keymap.chat_hints());
    let mut invalidation_queue = InvalidationQueue::default();
    refresh_invalidation_queue(chat, &mut invalidation_queue);
    let mut needs_redraw = true;

    while !chat.app.should_exit() {
        chat.app.set_key_hints(keymap.chat_hints());
        while let Ok(event) = chat.event_receiver.try_recv() {
            match event {
                BcodeEvent::Session(event) if Some(event.session_id) == chat.session_id => {
                    chat.app.absorb_session_event(&event);
                    needs_redraw = true;
                }
                BcodeEvent::Session(_)
                | BcodeEvent::RuntimeWork(_)
                | BcodeEvent::SessionCatalogUpdated { .. } => {}
            }
        }

        if chat.app.should_load_older_history() {
            history_flow::load_older_history(client, chat).await?;
            needs_redraw = true;
        }

        if modals.permission_dialog.is_none()
            && let Some(permission) = client
                .list_permissions()
                .await?
                .into_iter()
                .find(|permission| Some(permission.session_id) == chat.session_id)
        {
            modals.permission_dialog = Some(PermissionDialogState::new(permission));
            needs_redraw = true;
        }

        if helpers::resize_from_terminal(terminal)? {
            needs_redraw = true;
        }

        if needs_redraw {
            terminal.draw(|frame| {
                render::render(&mut chat.app, frame);
                if let Some(slash_palette) = &modals.slash_palette {
                    slash_palette_render::render_palette(
                        slash_palette,
                        chat.app.composer_content_area(),
                        frame,
                    );
                }
                if let Some(palette) = &mut modals.palette {
                    command_palette_render::render_palette(palette, frame);
                }
                if let Some(dialog) = &modals.permission_dialog {
                    permission_dialog_render::render_permission_dialog(dialog, frame);
                }
                if let Some(dialog) = &modals.thinking_dialog {
                    thinking_dialog_render::render_thinking_dialog(dialog, frame);
                }
            })?;
            refresh_invalidation_queue(chat, &mut invalidation_queue);
            needs_redraw = false;
        }

        let event = next_chat_loop_event(terminal_events, &mut invalidation_queue, chat).await?;
        match event {
            ChatLoopEvent::Terminal(event) => {
                let event_invalidation = if matches!(event, Event::Resize(_)) {
                    super::invalidation::UiInvalidation::Full
                } else {
                    super::invalidation::UiInvalidation::Layout
                };
                let mut context = ChatEventContext {
                    services: TuiServices { client, keymap },
                    terminal,
                    terminal_events,
                    mouse_scroll_rows,
                };
                if handle_event(&mut context, chat, &mut modals, event).await? {
                    needs_redraw = event_invalidation.needs_render();
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
        }
    }

    Ok(())
}

enum ChatLoopEvent {
    Terminal(Event),
    Async(Box<session_flow::ChatAsyncEvent>),
    TimedInvalidations(Vec<super::invalidation::InvalidationKey>),
}

async fn next_chat_loop_event(
    terminal_events: &mut TuiInput,
    invalidation_queue: &mut InvalidationQueue,
    chat: &mut ActiveChat,
) -> Result<ChatLoopEvent, TuiError> {
    let now = Instant::now();
    let due = invalidation_queue.take_due(now);
    if !due.is_empty() {
        return Ok(ChatLoopEvent::TimedInvalidations(due));
    }
    if let Some(next_at) = invalidation_queue.next_at() {
        let delay = next_at.saturating_duration_since(now);
        return tokio::select! {
            biased;
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
            () = tokio::time::sleep(delay) => Ok(ChatLoopEvent::TimedInvalidations(
                invalidation_queue.take_due(Instant::now())
            )),
        };
    }
    tokio::select! {
        biased;
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
) -> Result<bool, TuiError> {
    match event {
        Event::Resize(size) => {
            context
                .terminal
                .resize(Rect::new(0, 0, size.width, size.height));
            Ok(true)
        }
        Event::Key(stroke) => handle_chat_key(context, chat, modals, stroke).await,
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
) -> Result<bool, TuiError> {
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
            modals.thinking_dialog = Some(dialog);
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
    match outcome.request {
        KeyRequest::None => {}
        KeyRequest::Interrupt => request_turn_cancellation(context.services.client, chat).await,
        KeyRequest::CycleAgent => cycle_session_agent(context.services.client, chat).await,
        KeyRequest::Submit => {
            let (mut io, services) = context.flow_context();
            match composer_flow::submit_composer(&mut io, &services, chat).await {
                Ok(Some(dialog)) => {
                    modals.thinking_dialog = Some(dialog);
                }
                Ok(None) => {}
                Err(error) => helpers::report_client_error(&mut chat.app, "send failed", &error),
            }
        }
    }
    Ok(outcome.redraw)
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

async fn cycle_session_agent(client: &BcodeClient, chat: &mut ActiveChat) {
    let agents = match client.list_agents().await {
        Ok(agents) => agents,
        Err(error) => {
            chat.app.set_status(format!("agent cycle failed: {error}"));
            return;
        }
    };
    let Some(agent) = next_agent(&agents, chat.app.current_agent_id()) else {
        chat.app.set_status("no agents available".to_owned());
        return;
    };
    let agent_id = agent.id.clone();
    let agent_name = agent.name.clone();
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_current_agent_id(agent_id);
        chat.app.set_status(format!("agent set to {agent_name}"));
        return;
    };
    match client.set_session_agent(session_id, agent_id).await {
        Ok(()) => chat.app.set_status(format!("agent set to {agent_name}")),
        Err(error) => chat.app.set_status(format!("agent switch failed: {error}")),
    }
}

#[must_use]
pub fn next_agent<'a>(agents: &'a [AgentInfo], current_agent_id: &str) -> Option<&'a AgentInfo> {
    if agents.is_empty() {
        return None;
    }
    if let Some(index) = agents.iter().position(|agent| agent.id == current_agent_id) {
        return agents.get((index + 1) % agents.len());
    }
    agents
        .iter()
        .find(|agent| agent.is_default)
        .or_else(|| agents.first())
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
