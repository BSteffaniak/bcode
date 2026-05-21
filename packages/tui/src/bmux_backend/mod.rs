//! BMUX-native TUI backend.

mod app;
mod command_palette;
mod command_palette_render;
mod filtered_list;
mod input;
mod keymap;
mod model_flow;
mod model_picker;
mod model_picker_render;
mod palette_flow;
mod permission_dialog;
mod permission_dialog_render;
mod picker_mouse;
mod picker_render;
mod provider_picker;
mod provider_picker_render;
mod render;
mod session_flow;
mod session_picker;
mod session_picker_render;
mod skill_flow;
mod skill_picker;
mod skill_picker_render;
mod slash_commands;
mod slash_flow;
mod slash_palette;
mod slash_palette_render;

use std::io::{self, Write};
use std::time::Duration;

use bcode_client::BcodeClient;
use bcode_ipc::Event as BcodeEvent;
use bcode_session_models::{SessionHistoryDirection, SessionHistoryQuery, SessionId};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::keyboard::TextKeymap;
use bmux_tui::crossterm::{CrosstermTerminalGuard, poll_event};
use bmux_tui::event::{Event, FocusEvent, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome};
use bmux_tui::terminal::Terminal;
use crossterm::terminal::size;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use self::app::BmuxApp;
use self::command_palette::BmuxCommandPalette;
use self::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use self::permission_dialog::PermissionDialogState;
use super::TuiError;

const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(50);
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_millis(250);
const INITIAL_HISTORY_EVENT_LIMIT: usize = 500;
const OLDER_HISTORY_EVENT_LIMIT: usize = 500;
const MOUSE_WHEEL_ROWS: usize = 1;

/// Run the BMUX-native TUI backend.
///
/// # Errors
///
/// Returns I/O errors from terminal setup, event polling, drawing, or Bcode
/// client operations.
pub async fn run(session_id: Option<SessionId>) -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().expect("guard writer exists"),
            terminal_area()?,
        );
        run_event_loop(&mut terminal, session_id).await
    };

    match result {
        Ok(()) => {
            let _writer = guard.leave()?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

async fn run_event_loop<W: Write>(
    terminal: &mut Terminal<&mut W>,
    session_id: Option<SessionId>,
) -> Result<(), TuiError> {
    let client = BcodeClient::default_endpoint();
    let config = bcode_config::load_config()?;
    let keymap = BmuxKeyMap::from_config(&config.tui);
    let session_id = match session_id {
        Some(session_id) => session_id,
        None => session_flow::pick_session(terminal, &client, &keymap).await?,
    };
    let (event_sender, event_receiver) = mpsc::unbounded_channel();
    let (attached, event_task) =
        attach_session_event_stream(&client, session_id, event_sender.clone()).await?;
    let app = BmuxApp::new_with_history(
        Some(session_id),
        &attached.history,
        &attached.input_history,
        attached.history.len() >= INITIAL_HISTORY_EVENT_LIMIT,
    );
    let mut chat = ActiveChat {
        app,
        session_id,
        event_sender,
        event_receiver,
        event_task,
    };
    hydrate_status(&client, &mut chat.app).await;
    let result = run_with_client(terminal, &client, &keymap, &mut chat).await;
    chat.event_task.abort();
    result
}

struct ActiveChat {
    app: BmuxApp,
    session_id: SessionId,
    event_sender: mpsc::UnboundedSender<BcodeEvent>,
    event_receiver: mpsc::UnboundedReceiver<BcodeEvent>,
    event_task: JoinHandle<()>,
}

async fn hydrate_status(client: &BcodeClient, app: &mut BmuxApp) {
    let Some(session_id) = app.session_id() else {
        return;
    };
    let model = client.session_model_status(session_id).await.ok();
    let active_skills = client.active_skills(session_id).await.ok();
    let model_text = model.as_ref().map_or_else(
        || "model unknown".to_owned(),
        |status| {
            let provider = status.provider_plugin_id.as_deref().unwrap_or("auto");
            let model = status.model_id.as_deref().unwrap_or("default");
            format!("{provider}/{model}")
        },
    );
    let skill_count = active_skills.as_ref().map_or(0, Vec::len);
    app.set_status(format!("model: {model_text}; active skills: {skill_count}"));
}

struct ModalState {
    palette: Option<BmuxCommandPalette>,
    slash_palette: Option<slash_palette::SlashPalette>,
    permission_dialog: Option<PermissionDialogState>,
}

async fn run_with_client<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let mut modals = ModalState {
        palette: None,
        slash_palette: None,
        permission_dialog: None,
    };
    let mut needs_redraw = true;

    while !chat.app.should_exit() {
        while let Ok(event) = chat.event_receiver.try_recv() {
            match event {
                BcodeEvent::Session(event) if event.session_id == chat.session_id => {
                    chat.app.absorb_session_event(&event);
                    needs_redraw = true;
                }
                BcodeEvent::Session(_) => {}
            }
        }

        if chat.app.should_load_older_history() {
            load_older_history(client, chat).await?;
            needs_redraw = true;
        }

        if modals.permission_dialog.is_none()
            && let Some(permission) = client
                .list_permissions()
                .await?
                .into_iter()
                .find(|permission| permission.session_id == chat.session_id)
        {
            modals.permission_dialog = Some(PermissionDialogState::new(permission));
            needs_redraw = true;
        }

        if resize_from_terminal(terminal)? {
            needs_redraw = true;
        }

        if needs_redraw {
            terminal.draw(|frame| {
                render::render(&mut chat.app, frame);
                if let Some(slash_palette) = &modals.slash_palette {
                    slash_palette_render::render_palette(slash_palette, frame);
                }
                if let Some(palette) = &mut modals.palette {
                    command_palette_render::render_palette(palette, frame);
                }
                if let Some(dialog) = &mut modals.permission_dialog {
                    permission_dialog_render::render_permission_dialog(dialog, frame);
                }
            })?;
            needs_redraw = false;
        }

        if let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? {
            if handle_event(client, keymap, chat, &mut modals, terminal, event).await? {
                needs_redraw = true;
            }
        } else if chat.app.tick() {
            needs_redraw = true;
        }
    }

    Ok(())
}

async fn load_older_history(client: &BcodeClient, chat: &mut ActiveChat) -> Result<(), TuiError> {
    let Some(cursor) = chat.app.older_history_cursor() else {
        return Ok(());
    };
    chat.app.set_loading_older_history(true);
    match client
        .session_history_page(
            chat.session_id,
            SessionHistoryQuery {
                cursor: Some(cursor),
                limit: OLDER_HISTORY_EVENT_LIMIT,
                direction: SessionHistoryDirection::Backward,
            },
        )
        .await
    {
        Ok(page) => {
            chat.app.prepend_older_history(&page.events, page.has_more);
        }
        Err(error) => {
            chat.app.set_loading_older_history(false);
            chat.app
                .set_status(format!("older history load failed: {error}"));
        }
    }
    Ok(())
}

async fn attach_session_event_stream(
    client: &BcodeClient,
    session_id: SessionId,
    event_sender: mpsc::UnboundedSender<BcodeEvent>,
) -> Result<(bcode_client::AttachedSessionHistory, JoinHandle<()>), TuiError> {
    let mut connection = client.connect("bcode-tui-bmux").await?;
    let attached = connection
        .attach_session_recent_with_input_history(session_id, INITIAL_HISTORY_EVENT_LIMIT)
        .await?;
    let event_task = tokio::spawn(async move {
        loop {
            match connection.recv_event().await {
                Ok(event) => {
                    if event_sender.send(event).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    eprintln!("BMUX TUI event stream ended: {error}");
                    break;
                }
            }
        }
    });
    Ok((attached, event_task))
}

async fn handle_event<W: Write>(
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    modals: &mut ModalState,
    terminal: &mut Terminal<&mut W>,
    event: Event,
) -> Result<bool, TuiError> {
    match event {
        Event::Resize(size) => {
            terminal.resize(Rect::new(0, 0, size.width, size.height));
            Ok(true)
        }
        Event::Key(stroke) => handle_chat_key(client, keymap, chat, modals, terminal, stroke).await,
        Event::Paste(text) => {
            if let Some(palette) = &mut modals.palette {
                palette.state_mut().query.insert_str(&text);
                return Ok(true);
            }
            chat.app.composer_mut().insert_str(&text);
            chat.app.wake_cursor();
            update_slash_palette(client, chat, &mut modals.slash_palette).await;
            Ok(true)
        }
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick => Ok(true),
        Event::Mouse(mouse) => {
            if modals.palette.is_some() {
                return palette_flow::handle_palette_mouse(
                    client,
                    keymap,
                    chat,
                    &mut modals.palette,
                    terminal,
                    mouse,
                )
                .await;
            }
            if modals.slash_palette.is_some() {
                return Ok(slash_flow::handle_slash_palette_mouse(
                    chat,
                    &mut modals.slash_palette,
                    terminal,
                    mouse,
                ));
            }
            let hit_id = mouse_hit_id(terminal.hits(), mouse);
            handle_mouse(hit_id, client, chat, &mut modals.permission_dialog, mouse).await
        }
        Event::User(_) => Ok(false),
    }
}

async fn update_slash_palette(
    client: &BcodeClient,
    chat: &ActiveChat,
    slash_palette: &mut Option<slash_palette::SlashPalette>,
) {
    if chat.app.composer().text().starts_with('/') {
        let previous = slash_palette
            .as_ref()
            .and_then(|palette| palette.selected_command().map(str::to_owned));
        let mut next = slash_palette::SlashPalette::new(
            client,
            chat.app.session_id(),
            chat.app.composer().text(),
        )
        .await;
        if let Some(previous) = previous {
            next.select_command(&previous);
        }
        *slash_palette = (!next.is_empty()).then_some(next);
    } else {
        *slash_palette = None;
    }
}

async fn handle_chat_key<W: Write>(
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    modals: &mut ModalState,
    terminal: &mut Terminal<&mut W>,
    stroke: KeyStroke,
) -> Result<bool, TuiError> {
    if modals.slash_palette.is_some() {
        return slash_flow::handle_slash_palette_key(
            client,
            keymap,
            chat,
            &mut modals.slash_palette,
            terminal,
            stroke,
        )
        .await;
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
        return handle_permission_key(client, keymap, chat, &mut modals.permission_dialog, stroke)
            .await;
    }
    if modals.palette.is_some() {
        return palette_flow::handle_palette_key(
            client,
            keymap,
            chat,
            &mut modals.palette,
            terminal,
            stroke,
        )
        .await;
    }
    if is_palette_open_key(keymap, stroke) {
        modals.palette = Some(BmuxCommandPalette::new());
        return Ok(true);
    }
    let outcome = input::handle_key(&mut chat.app, keymap, stroke);
    update_slash_palette(client, chat, &mut modals.slash_palette).await;
    if outcome.submitted
        && let Err(error) = submit_composer(client, keymap, chat, terminal).await
    {
        report_client_error(&mut chat.app, "send failed", &error);
    }
    Ok(outcome.redraw)
}

fn composer_position_from_mouse(mouse: MouseEvent) -> Option<(usize, usize)> {
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return None;
    };
    let area = terminal_area().ok()?;
    let composer_height = area.height.clamp(3, 6);
    let composer_y = area.bottom().saturating_sub(composer_height);
    let inner_x = area.x.saturating_add(2);
    let inner_y = composer_y.saturating_add(1);
    let inner_width = area.width.saturating_sub(4);
    if mouse.position.y < inner_y || mouse.position.y >= area.bottom().saturating_sub(1) {
        return None;
    }
    if mouse.position.x < inner_x || mouse.position.x >= inner_x.saturating_add(inner_width) {
        return None;
    }
    Some((
        usize::from(mouse.position.y.saturating_sub(inner_y)),
        usize::from(mouse.position.x.saturating_sub(inner_x)),
    ))
}

fn diff_file_row_from_mouse(mouse: MouseEvent) -> Option<usize> {
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return None;
    };
    let area = terminal_area().ok()?;
    let diff_top = area.height.saturating_sub(12);
    if mouse.position.y < diff_top {
        return None;
    }
    usize::from(mouse.position.y.saturating_sub(diff_top).saturating_sub(1)).into()
}

fn permission_click_approval(mouse: MouseEvent) -> Option<bool> {
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return None;
    };
    let area = terminal_area().ok()?;
    let dialog_width = area.width.saturating_sub(4).min(76);
    let dialog_height = area.height.saturating_sub(4).min(14);
    let dialog_x = area
        .x
        .saturating_add(area.width.saturating_sub(dialog_width) / 2);
    let dialog_y = area
        .y
        .saturating_add(area.height.saturating_sub(dialog_height) / 3);
    let button_y = dialog_y.saturating_add(dialog_height).saturating_sub(3);
    if mouse.position.y != button_y {
        return None;
    }
    let approve_start = dialog_x.saturating_add(2);
    let approve_end = approve_start.saturating_add(12);
    let deny_start = approve_end.saturating_add(2);
    let deny_end = deny_start.saturating_add(9);
    if (approve_start..approve_end).contains(&mouse.position.x) {
        Some(true)
    } else if (deny_start..deny_end).contains(&mouse.position.x) {
        Some(false)
    } else {
        None
    }
}

fn mouse_hit_id(hits: &bmux_tui::hit::HitMap, mouse: MouseEvent) -> Option<String> {
    hits.hit_mouse(mouse)
        .map(|hit| hit.id().as_str().to_owned())
}

async fn handle_mouse(
    hit_id: Option<String>,
    client: &BcodeClient,
    chat: &mut ActiveChat,
    permission_dialog: &mut Option<PermissionDialogState>,
    mouse: MouseEvent,
) -> Result<bool, TuiError> {
    match mouse.kind {
        MouseEventKind::ScrollUp => match hit_id.as_deref() {
            Some("composer") => Ok(chat.app.previous_input_history()),
            Some("diff-files" | "diff-detail") if chat.app.diff_visible() => {
                Ok(chat.app.scroll_diff_up(MOUSE_WHEEL_ROWS))
            }
            _ => Ok(chat.app.scroll_transcript_up(MOUSE_WHEEL_ROWS)),
        },
        MouseEventKind::ScrollDown => match hit_id.as_deref() {
            Some("composer") => Ok(chat.app.next_input_history()),
            Some("diff-files" | "diff-detail") if chat.app.diff_visible() => {
                Ok(chat.app.scroll_diff_down(MOUSE_WHEEL_ROWS))
            }
            _ => Ok(chat.app.scroll_transcript_down(MOUSE_WHEEL_ROWS)),
        },
        MouseEventKind::Down(MouseButton::Left) if permission_dialog.is_some() => {
            if let Some(approve) = permission_click_approval(mouse) {
                resolve_permission_dialog(client, chat, permission_dialog, approve).await
            } else {
                Ok(false)
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if hit_id.as_deref() == Some("composer") {
                if let Some((row, col)) = composer_position_from_mouse(mouse) {
                    let width = usize::from(terminal_area()?.width.saturating_sub(4));
                    chat.app.move_composer_to_wrapped_position(width, row, col);
                    Ok(true)
                } else {
                    Ok(false)
                }
            } else if hit_id.as_deref() == Some("diff-files") && chat.app.diff_visible() {
                if let Some(row) = diff_file_row_from_mouse(mouse) {
                    Ok(chat.app.select_diff_file(row))
                } else {
                    Ok(false)
                }
            } else {
                Ok(false)
            }
        }
        MouseEventKind::Down(MouseButton::Right | MouseButton::Middle | MouseButton::Other(_))
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::Move
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => Ok(false),
    }
}

async fn handle_permission_key(
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    permission_dialog: &mut Option<PermissionDialogState>,
    stroke: KeyStroke,
) -> Result<bool, TuiError> {
    let Some(dialog) = permission_dialog else {
        return Ok(false);
    };
    let Some(action) = keymap.action_for_key(BmuxScope::Permission, stroke) else {
        return Ok(false);
    };
    match action {
        BmuxAction::SelectUp => {
            dialog.focus_previous();
            Ok(true)
        }
        BmuxAction::SelectDown => {
            dialog.focus_next();
            Ok(true)
        }
        BmuxAction::PermissionApprove => {
            resolve_permission_dialog(client, chat, permission_dialog, true).await
        }
        BmuxAction::PermissionDeny | BmuxAction::SelectCancel => {
            resolve_permission_dialog(client, chat, permission_dialog, false).await
        }
        BmuxAction::SelectConfirm => {
            let approved = dialog.focused_approval();
            resolve_permission_dialog(client, chat, permission_dialog, approved).await
        }
        BmuxAction::InputSubmit
        | BmuxAction::InputHistoryPrevious
        | BmuxAction::InputHistoryNext
        | BmuxAction::AppExit
        | BmuxAction::AppInterrupt
        | BmuxAction::CommandPaletteOpen
        | BmuxAction::TranscriptPageUp
        | BmuxAction::TranscriptPageDown
        | BmuxAction::TranscriptTop
        | BmuxAction::TranscriptBottom
        | BmuxAction::TranscriptLineUp
        | BmuxAction::TranscriptLineDown
        | BmuxAction::SessionNew
        | BmuxAction::SessionRename
        | BmuxAction::SessionDelete
        | BmuxAction::InputNewLine
        | BmuxAction::EditorMoveLeft
        | BmuxAction::EditorMoveRight
        | BmuxAction::EditorMoveWordLeft
        | BmuxAction::EditorMoveWordRight
        | BmuxAction::EditorMoveStart
        | BmuxAction::EditorMoveEnd
        | BmuxAction::EditorDeleteBackward
        | BmuxAction::EditorDeleteForward
        | BmuxAction::EditorDeleteWordBackward
        | BmuxAction::EditorDeleteWordForward
        | BmuxAction::EditorDeleteToStart
        | BmuxAction::EditorDeleteToEnd
        | BmuxAction::SkillInvoke
        | BmuxAction::SkillActivate
        | BmuxAction::SkillDeactivate
        | BmuxAction::SkillHelp => Ok(false),
    }
}

async fn resolve_permission_dialog(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    permission_dialog: &mut Option<PermissionDialogState>,
    approved: bool,
) -> Result<bool, TuiError> {
    let Some(dialog) = permission_dialog.take() else {
        return Ok(false);
    };
    let permission_id = dialog.permission().permission_id.clone();
    let resolved = client
        .resolve_permission(permission_id.clone(), approved)
        .await?;
    chat.app.set_status(if resolved {
        if approved {
            format!("approved permission {permission_id}")
        } else {
            format!("denied permission {permission_id}")
        }
    } else {
        format!("permission {permission_id} was already resolved")
    });
    Ok(true)
}

fn handle_text_buffer_key(
    buffer: &mut bmux_text_edit::TextEditBuffer,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
    enter_behavior: TextInputEnterBehavior,
) -> TextInputKeyOutcome {
    if let Some(command) = keymap.editor_command_for_key(stroke) {
        buffer.apply_command(command);
        return TextInputKeyOutcome::Edited;
    }
    TextInputKeyHandler::new(TextKeymap::default(), enter_behavior).handle_key(buffer, stroke)
}

async fn switch_session(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    next_session_id: SessionId,
) -> Result<(), TuiError> {
    chat.event_task.abort();
    while chat.event_receiver.try_recv().is_ok() {}
    let (attached, next_task) =
        attach_session_event_stream(client, next_session_id, chat.event_sender.clone()).await?;
    chat.event_task = next_task;
    chat.session_id = next_session_id;
    chat.app = BmuxApp::new_with_history(
        Some(next_session_id),
        &attached.history,
        &attached.input_history,
        attached.history.len() >= INITIAL_HISTORY_EVENT_LIMIT,
    );
    hydrate_status(client, &mut chat.app).await;
    Ok(())
}

fn is_palette_open_key(keymap: &BmuxKeyMap, stroke: KeyStroke) -> bool {
    keymap.action_for_key(BmuxScope::Chat, stroke) == Some(BmuxAction::CommandPaletteOpen)
}

async fn submit_composer<W: Write>(
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    terminal: &mut Terminal<&mut W>,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let message = chat.app.take_pending_submission();
    if message.trim().is_empty() {
        chat.app.clear_pending_submission(&message);
        return Ok(());
    }
    if message.starts_with('/') {
        chat.app.clear_pending_submission(&message);
        match slash_commands::execute(client, session_id, &message).await? {
            slash_commands::SlashCommandOutcome::Handled(status) => chat.app.set_status(status),
            slash_commands::SlashCommandOutcome::SystemNote(note) => {
                chat.app.push_system_note(note);
                chat.app.set_status("slash command handled".to_owned());
            }
            slash_commands::SlashCommandOutcome::SwitchSession(next_session_id) => {
                switch_session(client, chat, next_session_id).await?;
            }
            slash_commands::SlashCommandOutcome::PickSession => {
                let next_session_id = session_flow::pick_session(terminal, client, keymap).await?;
                switch_session(client, chat, next_session_id).await?;
            }
            slash_commands::SlashCommandOutcome::PickModel => {
                model_flow::pick_model_for_session(terminal, client, chat, keymap).await?;
            }
            slash_commands::SlashCommandOutcome::PickSkill => {
                skill_flow::pick_skill_for_session(terminal, client, chat, keymap).await?;
            }
            slash_commands::SlashCommandOutcome::ToggleDiff => {
                let _changed = chat.app.toggle_diff_visible();
                chat.app.set_status(if chat.app.diff_visible() {
                    "diff panel shown".to_owned()
                } else {
                    "diff panel hidden".to_owned()
                });
            }
            slash_commands::SlashCommandOutcome::Unknown(command) => {
                chat.app
                    .set_status(format!("unknown slash command: {command}"));
            }
        }
        return Ok(());
    }
    match client.send_user_message(session_id, message.clone()).await {
        Ok(acceptance) => {
            if acceptance.queued {
                chat.app
                    .mark_pending_submission_queued(acceptance.queue_position);
                chat.app.set_status(format!(
                    "Message queued{}",
                    acceptance
                        .queue_position
                        .map_or_else(String::new, |position| format!(" at #{position}"))
                ));
            } else {
                chat.app.mark_pending_submission_sent();
                chat.app.set_status("Message sent".to_owned());
            }
            Ok(())
        }
        Err(error) => {
            chat.app.restore_pending_submission(&message);
            chat.app.set_status(format!("send failed: {error}"));
            Ok(())
        }
    }
}

fn report_client_error(app: &mut BmuxApp, label: &str, error: &TuiError) {
    let message = format!("{label}: {error}");
    app.set_status(message.clone());
    app.push_system_note(message);
}

fn resize_from_terminal<W: Write>(terminal: &mut Terminal<&mut W>) -> io::Result<bool> {
    let area = terminal_area()?;
    let resized = terminal.area() != area;
    terminal.resize(area);
    Ok(resized)
}

fn terminal_area() -> io::Result<Rect> {
    let (width, height) = size()?;
    Ok(Rect::new(0, 0, width, height))
}

#[cfg(test)]
mod tests {
    use bcode_session_models::{ClientId, SessionEvent, SessionEventKind, SessionId};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;

    use super::{app::BmuxApp, render, slash_palette, slash_palette_render};

    #[test]
    fn render_includes_status_and_composer() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
        let cursor = {
            let mut frame = Frame::new(&mut buffer);
            render::render(&mut app, &mut frame);
            frame.cursor()
        };

        assert!(buffer.row_symbols(0).unwrap().contains("Bcode BMUX TUI"));
        assert!(buffer.row_symbols(3).unwrap().contains("BMUX backend"));
        assert!(buffer.row_symbols(4).unwrap().contains("Composer"));
        assert!(cursor.is_some());
    }

    #[test]
    fn slash_pending_submission_clears_after_take() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.replace_composer_with("/plan");
        app.stage_submission();
        let message = app.take_pending_submission();

        app.clear_pending_submission(&message);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 10));
        let mut frame = Frame::new(&mut buffer);
        render::render(&mut app, &mut frame);
        let output = rendered_text(&buffer);

        assert!(!output.contains("/plan"));
        assert!(!output.contains("[sending]"));
    }

    #[test]
    fn taken_pending_submission_can_be_restored_after_send_failure() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.replace_composer_with("hello");
        app.stage_submission();
        let message = app.take_pending_submission();

        app.restore_pending_submission(&message);

        assert_eq!(app.composer().text(), "hello");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 10));
        let mut frame = Frame::new(&mut buffer);
        render::render(&mut app, &mut frame);
        let output = rendered_text(&buffer);

        assert!(!output.contains("[sending]"));
    }

    #[test]
    fn slash_palette_renders_above_composer() {
        let palette = slash_palette::SlashPalette::from_items(vec![
            ("/plan", "Switch to plan agent"),
            ("/build", "Switch to build agent"),
        ]);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 20));
        let mut frame = Frame::new(&mut buffer);

        slash_palette_render::render_palette(&palette, &mut frame);
        let output = rendered_text(&buffer);

        assert!(output.contains("Slash Commands"));
        assert!(output.contains("/plan"));
        assert!(buffer.row_symbols(0).unwrap().trim().is_empty());
        assert!(buffer.row_symbols(10).unwrap().contains("Slash Commands"));
    }

    #[test]
    fn prepended_history_coalesces_assistant_deltas() {
        let session_id = SessionId::new();
        let newer = [event(
            session_id,
            10,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "newer prompt".to_owned(),
            },
        )];
        let mut app = BmuxApp::new_with_history(Some(session_id), &newer, &[], true);
        let older = [
            event(
                session_id,
                1,
                SessionEventKind::AssistantDelta {
                    text: "hello ".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::AssistantDelta {
                    text: "world".to_owned(),
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::AssistantMessage {
                    text: "hello world".to_owned(),
                },
            ),
        ];

        app.prepend_older_history(&older, false);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 14));
        let mut frame = Frame::new(&mut buffer);
        render::render(&mut app, &mut frame);
        let output = rendered_text(&buffer);

        assert!(output.contains("Assistant: hello world"));
        assert!(!output.contains("Assistant …: hello"));
        assert_eq!(output.matches("Assistant").count(), 1);
    }

    #[test]
    fn scroll_up_requests_older_history_only_after_top() {
        let session_id = SessionId::new();
        let history = (10..60)
            .map(|sequence| {
                event(
                    session_id,
                    sequence,
                    SessionEventKind::UserMessage {
                        client_id: ClientId::new(),
                        text: format!("prompt {sequence}"),
                    },
                )
            })
            .collect::<Vec<_>>();
        let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], true);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 20));
        let mut frame = Frame::new(&mut buffer);
        render::render(&mut app, &mut frame);

        assert!(app.scroll_transcript_up(1));
        assert!(!app.should_load_older_history());

        assert!(app.scroll_transcript_up(usize::MAX / 2));
        assert!(app.should_load_older_history());
    }

    fn event(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            schema_version: 1,
            sequence,
            session_id,
            kind,
        }
    }

    fn rendered_text(buffer: &Buffer) -> String {
        (0..buffer.area().height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n")
    }
}
