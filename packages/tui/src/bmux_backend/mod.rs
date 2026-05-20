//! BMUX-native TUI backend.

mod app;
mod command_palette;
mod command_palette_render;
mod input;
mod keymap;
mod model_picker;
mod model_picker_render;
mod permission_dialog;
mod permission_dialog_render;
mod provider_picker;
mod provider_picker_render;
mod render;
mod session_picker;
mod session_picker_render;
mod skill_picker;
mod skill_picker_render;
mod slash_commands;
mod slash_palette;

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
use bmux_tui::palette::{CommandPalette, CommandPaletteKeyOutcome};
use bmux_tui::terminal::Terminal;
use bmux_tui::widget::StatefulWidget;
use crossterm::terminal::size;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use self::app::BmuxApp;
use self::command_palette::{BmuxCommandPalette, PaletteCommand};
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
        None => pick_session(terminal, &client, &keymap).await?,
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
            let area = terminal.area();
            terminal.draw(|frame| {
                render::render(&mut chat.app, frame);
                if let Some(slash_palette) = &mut modals.slash_palette {
                    let items = slash_palette.palette_items();
                    CommandPalette::new(&items).render(area, frame, slash_palette.state_mut());
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

async fn pick_session<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
) -> Result<SessionId, TuiError> {
    let sessions = client.list_sessions().await?;
    let mut picker = session_picker::SessionPickerApp::new(sessions);
    loop {
        terminal.resize(terminal_area()?);
        terminal.draw(|frame| session_picker_render::render_picker(&mut picker, frame))?;
        let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? else {
            continue;
        };
        match event {
            Event::Resize(size) => terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => {
                picker.filter_mut().insert_str(&text);
                picker.refresh_filter();
            }
            Event::Key(stroke) => match handle_picker_key(&mut picker, keymap, stroke) {
                PickerKeyOutcome::Continue => {}
                PickerKeyOutcome::Create => return Ok(client.create_session(None).await?.id),
                PickerKeyOutcome::Rename => rename_picker_session(client, &mut picker).await?,
                PickerKeyOutcome::Delete => delete_picker_session(client, &mut picker).await?,
                PickerKeyOutcome::Selected => {
                    if let Some(session_id) = picker.selected_session_id() {
                        return Ok(session_id);
                    }
                    picker.set_status("No session selected; press Ctrl-N to create one".to_owned());
                }
                PickerKeyOutcome::Canceled => return Err(TuiError::Canceled),
            },
            Event::Mouse(mouse) => {
                if let Some(row) = picker_row_from_mouse(mouse)
                    && picker.select_visible(row)
                    && let Some(session_id) = picker.selected_session_id()
                {
                    return Ok(session_id);
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerKeyOutcome {
    Continue,
    Create,
    Rename,
    Delete,
    Selected,
    Canceled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionPickerStartMode {
    Rename,
    Delete,
}

async fn pick_session_for_mutation<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    start_mode: SessionPickerStartMode,
) -> Result<(), TuiError> {
    let keymap = BmuxKeyMap::from_config(&bcode_config::load_config()?.tui);
    let sessions = client.list_sessions().await?;
    let mut picker = session_picker::SessionPickerApp::new(sessions);
    match start_mode {
        SessionPickerStartMode::Rename => {
            picker.start_rename();
        }
        SessionPickerStartMode::Delete => {
            picker.start_delete_confirmation();
        }
    }
    loop {
        terminal.resize(terminal_area()?);
        terminal.draw(|frame| session_picker_render::render_picker(&mut picker, frame))?;
        let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? else {
            continue;
        };
        match event {
            Event::Resize(size) => terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => match picker.mode() {
                session_picker::SessionPickerMode::Rename => picker.rename_mut().insert_str(&text),
                session_picker::SessionPickerMode::Filter
                | session_picker::SessionPickerMode::DeleteConfirm => {
                    picker.filter_mut().insert_str(&text);
                    picker.refresh_filter();
                }
            },
            Event::Key(stroke) => match handle_picker_key(&mut picker, &keymap, stroke) {
                PickerKeyOutcome::Continue
                | PickerKeyOutcome::Create
                | PickerKeyOutcome::Selected => {}
                PickerKeyOutcome::Rename => rename_picker_session(client, &mut picker).await?,
                PickerKeyOutcome::Delete => delete_picker_session(client, &mut picker).await?,
                PickerKeyOutcome::Canceled => return Ok(()),
            },
            Event::Mouse(mouse) => {
                if let Some(row) = picker_row_from_mouse(mouse) {
                    let _selected = picker.select_visible(row);
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
        if matches!(picker.mode(), session_picker::SessionPickerMode::Filter) {
            return Ok(());
        }
    }
}

fn handle_picker_key(
    picker: &mut session_picker::SessionPickerApp,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) -> PickerKeyOutcome {
    match picker.mode() {
        session_picker::SessionPickerMode::Filter => {
            handle_picker_filter_key(picker, keymap, stroke)
        }
        session_picker::SessionPickerMode::Rename => handle_picker_rename_key(picker, stroke),
        session_picker::SessionPickerMode::DeleteConfirm => {
            handle_picker_delete_key(picker, stroke)
        }
    }
}

fn handle_picker_filter_key(
    picker: &mut session_picker::SessionPickerApp,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) -> PickerKeyOutcome {
    if let Some(action) = keymap.action_for_key(BmuxScope::SessionPicker, stroke) {
        return match action {
            BmuxAction::SelectCancel => PickerKeyOutcome::Canceled,
            BmuxAction::SessionNew => PickerKeyOutcome::Create,
            BmuxAction::SessionRename => {
                picker.start_rename();
                PickerKeyOutcome::Continue
            }
            BmuxAction::SessionDelete => {
                picker.start_delete_confirmation();
                PickerKeyOutcome::Continue
            }
            BmuxAction::SelectConfirm => PickerKeyOutcome::Selected,
            BmuxAction::SelectUp => {
                picker.select_previous();
                PickerKeyOutcome::Continue
            }
            BmuxAction::SelectDown => {
                picker.select_next();
                PickerKeyOutcome::Continue
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
            | BmuxAction::PermissionApprove
            | BmuxAction::PermissionDeny
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
            | BmuxAction::SkillHelp => PickerKeyOutcome::Continue,
        };
    }
    match stroke.key {
        KeyCode::Enter => PickerKeyOutcome::Selected,
        KeyCode::Up if stroke.modifiers.is_empty() => {
            picker.select_previous();
            PickerKeyOutcome::Continue
        }
        KeyCode::Down if stroke.modifiers.is_empty() => {
            picker.select_next();
            PickerKeyOutcome::Continue
        }
        _ => {
            let outcome = handle_text_buffer_key(
                picker.filter_mut(),
                keymap,
                stroke,
                TextInputEnterBehavior::InsertNewline,
            );
            if outcome == TextInputKeyOutcome::Edited {
                picker.refresh_filter();
            }
            PickerKeyOutcome::Continue
        }
    }
}

fn handle_picker_rename_key(
    picker: &mut session_picker::SessionPickerApp,
    stroke: KeyStroke,
) -> PickerKeyOutcome {
    if stroke.key == KeyCode::Escape {
        picker.cancel_rename();
        return PickerKeyOutcome::Continue;
    }
    if stroke.key == KeyCode::Enter {
        return PickerKeyOutcome::Rename;
    }
    let outcome = TextInputKeyHandler::new(TextKeymap::default(), TextInputEnterBehavior::Submit)
        .handle_key(picker.rename_mut(), stroke);
    if outcome == TextInputKeyOutcome::Submitted {
        PickerKeyOutcome::Rename
    } else {
        PickerKeyOutcome::Continue
    }
}

fn handle_picker_delete_key(
    picker: &mut session_picker::SessionPickerApp,
    stroke: KeyStroke,
) -> PickerKeyOutcome {
    match stroke.key {
        KeyCode::Escape | KeyCode::Char('n' | 'N') => {
            picker.cancel_delete();
            PickerKeyOutcome::Continue
        }
        KeyCode::Char('y' | 'Y') => PickerKeyOutcome::Delete,
        _ => PickerKeyOutcome::Continue,
    }
}

async fn rename_picker_session(
    client: &BcodeClient,
    picker: &mut session_picker::SessionPickerApp,
) -> Result<(), TuiError> {
    let Some(session_id) = picker.selected_session_id() else {
        picker.finish_mutation("No session selected to rename".to_owned());
        return Ok(());
    };
    let name = picker.rename().text().trim();
    let name = (!name.is_empty()).then(|| name.to_owned());
    match client.rename_session(session_id, name).await {
        Ok(_) => {
            picker.replace_sessions(client.list_sessions().await?);
            picker.finish_mutation("Session renamed".to_owned());
        }
        Err(error) => picker.finish_mutation(format!("rename failed: {error}")),
    }
    Ok(())
}

async fn delete_picker_session(
    client: &BcodeClient,
    picker: &mut session_picker::SessionPickerApp,
) -> Result<(), TuiError> {
    let Some(session_id) = picker.selected_session_id() else {
        picker.finish_mutation("No session selected to delete".to_owned());
        return Ok(());
    };
    match client.delete_session(session_id).await {
        Ok(_) => {
            picker.replace_sessions(client.list_sessions().await?);
            picker.finish_mutation("Session deleted".to_owned());
        }
        Err(error) => picker.finish_mutation(format!("delete failed: {error}")),
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
            if let Some(slash_palette) = &mut modals.slash_palette {
                slash_palette
                    .state_mut()
                    .query
                    .insert_str(text.trim_start_matches('/'));
                return Ok(true);
            }
            if let Some(palette) = &mut modals.palette {
                palette.state_mut().query.insert_str(&text);
                return Ok(true);
            }
            chat.app.composer_mut().insert_str(&text);
            chat.app.wake_cursor();
            Ok(true)
        }
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick => Ok(true),
        Event::Mouse(mouse) => {
            if modals.palette.is_some() {
                return handle_palette_mouse(
                    client,
                    keymap,
                    chat,
                    &mut modals.palette,
                    terminal,
                    mouse,
                )
                .await;
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
        *slash_palette = Some(
            slash_palette::SlashPalette::new(
                client,
                chat.app.session_id(),
                chat.app.composer().text(),
            )
            .await,
        );
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
        return handle_slash_palette_key(
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
        return handle_palette_key(client, keymap, chat, &mut modals.palette, terminal, stroke)
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

fn command_palette_row_from_mouse(mouse: MouseEvent) -> Option<usize> {
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return None;
    };
    usize::from(mouse.position.y).checked_sub(3)
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

fn picker_row_from_mouse(mouse: MouseEvent) -> Option<usize> {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {}
        _ => return None,
    }
    let y = usize::from(mouse.position.y);
    y.checked_sub(5)
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
            Some("diff-files" | "diff-detail") => Ok(chat.app.scroll_diff_up(MOUSE_WHEEL_ROWS)),
            _ => Ok(chat.app.scroll_transcript_up(MOUSE_WHEEL_ROWS)),
        },
        MouseEventKind::ScrollDown => match hit_id.as_deref() {
            Some("composer") => Ok(chat.app.next_input_history()),
            Some("diff-files" | "diff-detail") => Ok(chat.app.scroll_diff_down(MOUSE_WHEEL_ROWS)),
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
            } else if hit_id.as_deref() == Some("diff-files") {
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

async fn handle_palette_key<W: Write>(
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    palette: &mut Option<BmuxCommandPalette>,
    terminal: &mut Terminal<&mut W>,
    stroke: KeyStroke,
) -> Result<bool, TuiError> {
    let Some(active_palette) = palette else {
        return Ok(false);
    };
    let items = active_palette.cloned_items();
    let widget = CommandPalette::new(&items);
    let outcome = widget.handle_key(active_palette.state_mut(), terminal.area().height, stroke);
    match outcome {
        CommandPaletteKeyOutcome::Activated(index) => {
            let command = active_palette.command_at(index);
            *palette = None;
            if let Some(command) = command
                && let Err(error) =
                    execute_palette_command(client, chat, terminal, keymap, command).await
            {
                report_client_error(&mut chat.app, "command failed", &error);
            }
            Ok(true)
        }
        CommandPaletteKeyOutcome::Canceled => {
            *palette = None;
            Ok(true)
        }
        CommandPaletteKeyOutcome::Ignored => Ok(false),
        CommandPaletteKeyOutcome::QueryEdited | CommandPaletteKeyOutcome::SelectionMoved => {
            Ok(true)
        }
    }
}

async fn handle_slash_palette_key<W: Write>(
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    slash_palette: &mut Option<slash_palette::SlashPalette>,
    terminal: &mut Terminal<&mut W>,
    stroke: KeyStroke,
) -> Result<bool, TuiError> {
    let Some(active_palette) = slash_palette else {
        return Ok(false);
    };
    let items = active_palette.palette_items();
    let widget = CommandPalette::new(&items);
    let outcome = widget.handle_key(active_palette.state_mut(), terminal.area().height, stroke);
    match outcome {
        CommandPaletteKeyOutcome::Activated(index) => {
            if let Some(command) = active_palette.command_at(index).map(str::to_owned) {
                chat.app.replace_composer_with(&command);
            }
            *slash_palette = None;
            Ok(true)
        }
        CommandPaletteKeyOutcome::Canceled => {
            *slash_palette = None;
            Ok(true)
        }
        CommandPaletteKeyOutcome::Ignored => {
            let outcome = input::handle_key(&mut chat.app, keymap, stroke);
            update_slash_palette(client, chat, slash_palette).await;
            if outcome.submitted
                && let Err(error) = submit_composer(client, keymap, chat, terminal).await
            {
                report_client_error(&mut chat.app, "send failed", &error);
            }
            Ok(outcome.redraw)
        }
        CommandPaletteKeyOutcome::QueryEdited | CommandPaletteKeyOutcome::SelectionMoved => {
            Ok(true)
        }
    }
}

async fn handle_palette_mouse<W: Write>(
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    palette: &mut Option<BmuxCommandPalette>,
    terminal: &mut Terminal<&mut W>,
    mouse: MouseEvent,
) -> Result<bool, TuiError> {
    let Some(index) = command_palette_row_from_mouse(mouse) else {
        return Ok(false);
    };
    let Some(active_palette) = palette else {
        return Ok(false);
    };
    let command = active_palette.command_at(index);
    *palette = None;
    if let Some(command) = command
        && let Err(error) = execute_palette_command(client, chat, terminal, keymap, command).await
    {
        report_client_error(&mut chat.app, "command failed", &error);
    }
    Ok(true)
}

async fn execute_palette_command<W: Write>(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    terminal: &mut Terminal<&mut W>,
    keymap: &BmuxKeyMap,
    command: PaletteCommand,
) -> Result<(), TuiError> {
    match command {
        PaletteCommand::NewSession => {
            let session = client.create_session(None).await?;
            switch_session(client, chat, session.id).await?;
        }
        PaletteCommand::SwitchSession => {
            let selected_session_id = pick_session(
                terminal,
                client,
                &BmuxKeyMap::from_config(&bcode_config::load_config()?.tui),
            )
            .await?;
            switch_session(client, chat, selected_session_id).await?;
        }
        PaletteCommand::ShowModelStatus => {
            show_model_status(client, chat).await?;
        }
        PaletteCommand::ShowServerModelStatus => {
            show_server_model_status(client, chat).await?;
        }
        PaletteCommand::SelectModel => {
            pick_model_for_session(terminal, client, chat, keymap).await?;
        }
        PaletteCommand::ListSkills => {
            pick_skill_for_session(terminal, client, chat, keymap).await?;
        }
        PaletteCommand::ActiveSkills => {
            show_active_skills(client, chat).await?;
        }
        PaletteCommand::Help => {
            show_bmux_help(chat);
        }
        PaletteCommand::RenameSession => {
            pick_session_for_mutation(terminal, client, SessionPickerStartMode::Rename).await?;
        }
        PaletteCommand::DeleteSession => {
            pick_session_for_mutation(terminal, client, SessionPickerStartMode::Delete).await?;
        }
        PaletteCommand::CancelTurn => {
            let Some(session_id) = chat.app.session_id() else {
                chat.app.set_status("No active session".to_owned());
                return Ok(());
            };
            let cancelled = client.cancel_session_turn(session_id).await?;
            chat.app.set_status(if cancelled {
                "cancel requested".to_owned()
            } else {
                "no active turn to cancel".to_owned()
            });
        }
        PaletteCommand::CompactContext => {
            let Some(session_id) = chat.app.session_id() else {
                chat.app.set_status("No active session".to_owned());
                return Ok(());
            };
            let message = client.compact_session(session_id).await?;
            chat.app.set_status(message);
        }
    }
    Ok(())
}

async fn show_model_status(client: &BcodeClient, chat: &mut ActiveChat) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let status = client.session_model_status(session_id).await?;
    let provider = status
        .provider_plugin_id
        .as_deref()
        .unwrap_or("default provider");
    let model = status.model_id.as_deref().unwrap_or("default model");
    let mut lines = vec![format!("Active model: {provider}/{model}")];
    if let Some(info) = status.model {
        lines.push(format!("Display name: {}", info.display_name));
        if let Some(context_window) = info.context_window {
            lines.push(format!("Context window: {context_window}"));
        }
        if let Some(max_output_tokens) = info.max_output_tokens {
            lines.push(format!("Max output tokens: {max_output_tokens}"));
        }
        if !info.capabilities.is_empty() {
            lines.push(format!("Capabilities: {:?}", info.capabilities));
        }
    }
    let text = lines.join("\n");
    chat.app.set_status(format!("model: {provider}/{model}"));
    chat.app.push_system_note(text);
    Ok(())
}

async fn show_server_model_status(
    client: &BcodeClient,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let status = client.server_status().await?;
    let provider = status
        .selected_provider_plugin_id
        .as_deref()
        .unwrap_or("default provider");
    let model = status
        .selected_model_id
        .as_deref()
        .unwrap_or("default model");
    let text = format!("Server default model: {provider}/{model}");
    chat.app.set_status(text.clone());
    chat.app.push_system_note(text);
    Ok(())
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

async fn pick_model_provider<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
) -> Result<Option<String>, TuiError> {
    let providers = client
        .plugin_services()
        .await?
        .into_iter()
        .filter(|service| service.interface_id == bcode_model::MODEL_PROVIDER_INTERFACE_ID)
        .collect::<Vec<_>>();
    if providers.len() <= 1 {
        return Ok(providers.first().map(|provider| provider.plugin_id.clone()));
    }
    let mut picker = provider_picker::ProviderPickerApp::new(providers);
    loop {
        terminal.resize(terminal_area()?);
        terminal
            .draw(|frame| provider_picker_render::render_provider_picker(&mut picker, frame))?;
        let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? else {
            continue;
        };
        match event {
            Event::Resize(size) => terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => {
                picker.filter_mut().insert_str(&text);
                picker.refresh_filter();
            }
            Event::Key(stroke) => match stroke.key {
                KeyCode::Escape => return Ok(None),
                KeyCode::Enter => return Ok(picker.selected_provider_id()),
                KeyCode::Up => picker.select_previous(),
                KeyCode::Down => picker.select_next(),
                _ => {
                    let outcome = handle_text_buffer_key(
                        picker.filter_mut(),
                        keymap,
                        stroke,
                        TextInputEnterBehavior::InsertNewline,
                    );
                    if outcome == TextInputKeyOutcome::Edited {
                        picker.refresh_filter();
                    }
                }
            },
            Event::Mouse(mouse) => {
                if let Some(row) = picker_row_from_mouse(mouse)
                    && picker.select_visible(row)
                {
                    return Ok(picker.selected_provider_id());
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

async fn pick_model_for_session<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    chat: &mut ActiveChat,
    keymap: &BmuxKeyMap,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let provider_plugin_id = pick_model_provider(terminal, client, keymap).await?;
    let models = client
        .session_model_list(provider_plugin_id.clone())
        .await?
        .models;
    let status = provider_plugin_id.as_ref().map_or_else(
        || "Select a model".to_owned(),
        |provider| format!("Select a model from {provider}"),
    );
    let mut picker = model_picker::ModelPickerApp::new_with_status(models, status);
    loop {
        terminal.resize(terminal_area()?);
        terminal.draw(|frame| model_picker_render::render_model_picker(&mut picker, frame))?;
        let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? else {
            continue;
        };
        match event {
            Event::Resize(size) => terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => {
                picker.filter_mut().insert_str(&text);
                picker.refresh_filter();
            }
            Event::Key(stroke) => match stroke.key {
                KeyCode::Escape => return Ok(()),
                KeyCode::Enter => {
                    if let Some(model_id) = picker.selected_model_id() {
                        if let Err(error) = client
                            .set_session_model(
                                session_id,
                                provider_plugin_id.clone(),
                                model_id.clone(),
                            )
                            .await
                        {
                            report_client_error(
                                &mut chat.app,
                                "model selection failed",
                                &error.into(),
                            );
                        } else {
                            chat.app.set_status(provider_plugin_id.as_ref().map_or_else(
                                || format!("model set to {model_id}"),
                                |provider| format!("model set to {provider}/{model_id}"),
                            ));
                        }
                        return Ok(());
                    }
                }
                KeyCode::Up => picker.select_previous(),
                KeyCode::Down => picker.select_next(),
                _ => {
                    let outcome = handle_text_buffer_key(
                        picker.filter_mut(),
                        keymap,
                        stroke,
                        TextInputEnterBehavior::InsertNewline,
                    );
                    if outcome == TextInputKeyOutcome::Edited {
                        picker.refresh_filter();
                    }
                }
            },
            Event::Mouse(mouse) => {
                if let Some(row) = picker_row_from_mouse(mouse)
                    && picker.select_visible(row)
                    && let Some(model_id) = picker.selected_model_id()
                {
                    if let Err(error) = client
                        .set_session_model(session_id, provider_plugin_id.clone(), model_id.clone())
                        .await
                    {
                        report_client_error(&mut chat.app, "model selection failed", &error.into());
                    } else {
                        chat.app.set_status(provider_plugin_id.as_ref().map_or_else(
                            || format!("model set to {model_id}"),
                            |provider| format!("model set to {provider}/{model_id}"),
                        ));
                    }
                    return Ok(());
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

async fn pick_skill_for_session<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    chat: &mut ActiveChat,
    keymap: &BmuxKeyMap,
) -> Result<(), TuiError> {
    let skills = client.list_skills().await?;
    if skills.skills.is_empty() {
        chat.app.set_status("no skills available".to_owned());
        chat.app
            .push_system_note("No skills are available.".to_owned());
        return Ok(());
    }
    let mut picker = skill_picker::SkillPickerApp::new(skills.skills);
    loop {
        terminal.resize(terminal_area()?);
        terminal.draw(|frame| skill_picker_render::render_skill_picker(&mut picker, frame))?;
        let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? else {
            continue;
        };
        match event {
            Event::Resize(size) => terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => match picker.mode() {
                skill_picker::SkillPickerMode::Filter => {
                    picker.filter_mut().insert_str(&text);
                    picker.refresh_filter();
                }
                skill_picker::SkillPickerMode::Argument => picker.argument_mut().insert_str(&text),
            },
            Event::Key(stroke) => match handle_skill_picker_key(&mut picker, keymap, stroke) {
                skill_picker::SkillPickerAction::Continue => {}
                skill_picker::SkillPickerAction::Cancel => return Ok(()),
                skill_picker::SkillPickerAction::Help(skill_id) => {
                    if let Err(error) = describe_skill(client, chat, skill_id).await {
                        report_client_error(&mut chat.app, "skill help failed", &error);
                    }
                    return Ok(());
                }
                skill_picker::SkillPickerAction::Activate(skill_id) => {
                    if let Err(error) = activate_skill(client, chat, skill_id).await {
                        report_client_error(&mut chat.app, "skill activation failed", &error);
                    }
                    return Ok(());
                }
                skill_picker::SkillPickerAction::Deactivate(skill_id) => {
                    if let Err(error) = deactivate_skill(client, chat, skill_id).await {
                        report_client_error(&mut chat.app, "skill deactivation failed", &error);
                    }
                    return Ok(());
                }
                skill_picker::SkillPickerAction::Invoke {
                    skill_id,
                    arguments,
                } => {
                    if let Err(error) = invoke_skill(client, chat, skill_id, arguments).await {
                        report_client_error(&mut chat.app, "skill invocation failed", &error);
                    }
                    return Ok(());
                }
            },
            Event::Mouse(mouse) => {
                if let Some(row) = picker_row_from_mouse(mouse)
                    && picker.select_visible(row)
                {
                    picker.start_argument();
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

fn handle_skill_picker_key(
    picker: &mut skill_picker::SkillPickerApp,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) -> skill_picker::SkillPickerAction {
    match picker.mode() {
        skill_picker::SkillPickerMode::Filter => handle_skill_filter_key(picker, keymap, stroke),
        skill_picker::SkillPickerMode::Argument => {
            handle_skill_argument_key(picker, keymap, stroke)
        }
    }
}

fn handle_skill_filter_key(
    picker: &mut skill_picker::SkillPickerApp,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) -> skill_picker::SkillPickerAction {
    if let Some(action) = keymap.action_for_key(BmuxScope::SkillPicker, stroke) {
        return handle_skill_picker_action(picker, action);
    }
    match stroke.key {
        KeyCode::Escape => skill_picker::SkillPickerAction::Cancel,
        KeyCode::Enter => {
            if picker.selected_skill_id().is_some() {
                picker.start_argument();
            }
            skill_picker::SkillPickerAction::Continue
        }
        KeyCode::Up if stroke.modifiers.is_empty() => {
            picker.select_previous();
            skill_picker::SkillPickerAction::Continue
        }
        KeyCode::Down if stroke.modifiers.is_empty() => {
            picker.select_next();
            skill_picker::SkillPickerAction::Continue
        }
        KeyCode::Char('a') if stroke.modifiers.is_empty() => picker.selected_skill_id().map_or(
            skill_picker::SkillPickerAction::Continue,
            skill_picker::SkillPickerAction::Activate,
        ),
        KeyCode::Char('d') if stroke.modifiers.is_empty() => picker.selected_skill_id().map_or(
            skill_picker::SkillPickerAction::Continue,
            skill_picker::SkillPickerAction::Deactivate,
        ),
        KeyCode::Char('?') if stroke.modifiers.is_empty() => picker.selected_skill_id().map_or(
            skill_picker::SkillPickerAction::Continue,
            skill_picker::SkillPickerAction::Help,
        ),
        _ => {
            let outcome = handle_text_buffer_key(
                picker.filter_mut(),
                keymap,
                stroke,
                TextInputEnterBehavior::InsertNewline,
            );
            if outcome == TextInputKeyOutcome::Edited {
                picker.refresh_filter();
            }
            skill_picker::SkillPickerAction::Continue
        }
    }
}

fn handle_skill_picker_action(
    picker: &mut skill_picker::SkillPickerApp,
    action: BmuxAction,
) -> skill_picker::SkillPickerAction {
    match action {
        BmuxAction::SelectCancel => skill_picker::SkillPickerAction::Cancel,
        BmuxAction::SelectUp => {
            picker.select_previous();
            skill_picker::SkillPickerAction::Continue
        }
        BmuxAction::SelectDown => {
            picker.select_next();
            skill_picker::SkillPickerAction::Continue
        }
        BmuxAction::SelectConfirm | BmuxAction::SkillInvoke => {
            if picker.selected_skill_id().is_some() {
                picker.start_argument();
            }
            skill_picker::SkillPickerAction::Continue
        }
        BmuxAction::SkillActivate => picker.selected_skill_id().map_or(
            skill_picker::SkillPickerAction::Continue,
            skill_picker::SkillPickerAction::Activate,
        ),
        BmuxAction::SkillDeactivate => picker.selected_skill_id().map_or(
            skill_picker::SkillPickerAction::Continue,
            skill_picker::SkillPickerAction::Deactivate,
        ),
        BmuxAction::SkillHelp => picker.selected_skill_id().map_or(
            skill_picker::SkillPickerAction::Continue,
            skill_picker::SkillPickerAction::Help,
        ),
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
        | BmuxAction::PermissionApprove
        | BmuxAction::PermissionDeny
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
        | BmuxAction::EditorDeleteToEnd => skill_picker::SkillPickerAction::Continue,
    }
}

fn handle_skill_argument_key(
    picker: &mut skill_picker::SkillPickerApp,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) -> skill_picker::SkillPickerAction {
    match stroke.key {
        KeyCode::Escape => skill_picker::SkillPickerAction::Cancel,
        KeyCode::Enter => picker.selected_skill_id().map_or(
            skill_picker::SkillPickerAction::Continue,
            |skill_id| skill_picker::SkillPickerAction::Invoke {
                skill_id,
                arguments: picker.argument().text().to_owned(),
            },
        ),
        _ => {
            let _outcome = handle_text_buffer_key(
                picker.argument_mut(),
                keymap,
                stroke,
                TextInputEnterBehavior::InsertNewline,
            );
            skill_picker::SkillPickerAction::Continue
        }
    }
}

async fn describe_skill(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    skill_id: bcode_skill_models::SkillId,
) -> Result<(), TuiError> {
    let manifest = client.describe_skill(skill_id.clone()).await?;
    let description = manifest
        .summary
        .description
        .as_deref()
        .unwrap_or("no description");
    chat.app.push_system_note(format!(
        "Skill: {}\nName: {}\nDescription: {description}\nSource: {}\nInstructions:\n{}",
        manifest.summary.id,
        manifest.summary.name,
        manifest.summary.source.label,
        truncate_for_status(&manifest.instructions, 2_000)
    ));
    chat.app.set_status(format!("shown skill {skill_id}"));
    Ok(())
}

async fn activate_skill(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    skill_id: bcode_skill_models::SkillId,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    client.activate_skill(session_id, skill_id.clone()).await?;
    chat.app.set_status(format!("activated skill {skill_id}"));
    Ok(())
}

async fn deactivate_skill(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    skill_id: bcode_skill_models::SkillId,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    client
        .deactivate_skill(session_id, skill_id.clone())
        .await?;
    chat.app.set_status(format!("deactivated skill {skill_id}"));
    Ok(())
}

async fn invoke_skill(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    skill_id: bcode_skill_models::SkillId,
    arguments: String,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let display_text = if arguments.trim().is_empty() {
        format!("Invoke skill {skill_id}")
    } else {
        format!("Invoke skill {skill_id}: {arguments}")
    };
    let acceptance = client
        .invoke_skill(session_id, skill_id.clone(), arguments, display_text)
        .await?;
    chat.app.set_status(if acceptance.queued {
        format!("skill {skill_id} queued")
    } else {
        format!("skill {skill_id} invoked")
    });
    Ok(())
}

fn truncate_for_status(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}\n…")
    } else {
        truncated
    }
}

async fn show_active_skills(client: &BcodeClient, chat: &mut ActiveChat) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let skills = client.active_skills(session_id).await?;
    let mut lines = vec![format!("Active skills: {}", skills.len())];
    lines.extend(skills.iter().map(|skill| {
        let suffix = if skill.truncated { " truncated" } else { "" };
        format!(
            "* {} — {} bytes{} from {}",
            skill.skill_id, skill.bytes_loaded, suffix, skill.source.label
        )
    }));
    chat.app
        .set_status(format!("active skills: {}", skills.len()));
    chat.app.push_system_note(lines.join("\n"));
    Ok(())
}

fn show_bmux_help(chat: &mut ActiveChat) {
    chat.app.push_system_note(
        [
            "BMUX backend help",
            "* Use the command palette for sessions, model status, skills, cancel, and compact.",
            "* Transcript scrolling, composer history, session picker, and permissions honor configured keybindings where wired.",
            "* Permission dialogs: approve/deny or move focus and confirm.",
        ]
        .join("\n"),
    );
    chat.app.set_status("shown help".to_owned());
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
        return Ok(());
    }
    if message.starts_with('/') {
        chat.app.clear_pending_submission();
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
                let next_session_id = pick_session(terminal, client, keymap).await?;
                switch_session(client, chat, next_session_id).await?;
            }
            slash_commands::SlashCommandOutcome::PickModel => {
                pick_model_for_session(terminal, client, chat, keymap).await?;
            }
            slash_commands::SlashCommandOutcome::PickSkill => {
                pick_skill_for_session(terminal, client, chat, keymap).await?;
            }
            slash_commands::SlashCommandOutcome::Unknown(command) => {
                chat.app
                    .set_status(format!("unknown slash command: {command}"));
            }
        }
        return Ok(());
    }
    match client.send_user_message(session_id, message).await {
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
            chat.app.restore_pending_submission();
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

    use super::{app::BmuxApp, render};

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
