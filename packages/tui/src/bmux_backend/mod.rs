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
mod render;
mod session_picker;
mod session_picker_render;
mod skill_picker;
mod skill_picker_render;

use std::io::{self, Write};
use std::time::Duration;

use bcode_client::BcodeClient;
use bcode_ipc::Event as BcodeEvent;
use bcode_session_models::{SessionHistoryDirection, SessionHistoryQuery, SessionId};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::keyboard::TextKeymap;
use bmux_tui::crossterm::{CrosstermTerminalGuard, poll_event};
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome};
use bmux_tui::palette::{CommandPalette, CommandPaletteKeyOutcome};
use bmux_tui::terminal::Terminal;
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
    let app =
        BmuxApp::new_with_history(Some(session_id), &attached.history, &attached.input_history);
    let mut chat = ActiveChat {
        app,
        session_id,
        event_sender,
        event_receiver,
        event_task,
    };
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

async fn run_with_client<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let mut palette: Option<BmuxCommandPalette> = None;
    let mut permission_dialog: Option<PermissionDialogState> = None;
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

        if permission_dialog.is_none()
            && let Some(permission) = client
                .list_permissions()
                .await?
                .into_iter()
                .find(|permission| permission.session_id == chat.session_id)
        {
            permission_dialog = Some(PermissionDialogState::new(permission));
            needs_redraw = true;
        }

        if resize_from_terminal(terminal)? {
            needs_redraw = true;
        }

        if needs_redraw {
            terminal.draw(|frame| {
                render::render(&chat.app, frame);
                if let Some(palette) = &mut palette {
                    command_palette_render::render_palette(palette, frame);
                }
                if let Some(dialog) = &mut permission_dialog {
                    permission_dialog_render::render_permission_dialog(dialog, frame);
                }
            })?;
            needs_redraw = false;
        }

        if let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? {
            if handle_event(
                client,
                keymap,
                chat,
                &mut permission_dialog,
                &mut palette,
                terminal,
                event,
            )
            .await?
            {
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
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost)
            | Event::Mouse(_)
            | Event::Tick
            | Event::User(_) => {}
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
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost)
            | Event::Mouse(_)
            | Event::Tick
            | Event::User(_) => {}
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
            | BmuxAction::EditorDeleteToEnd => PickerKeyOutcome::Continue,
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
            let outcome = TextInputKeyHandler::new(
                TextKeymap::default(),
                TextInputEnterBehavior::InsertNewline,
            )
            .handle_key(picker.filter_mut(), stroke);
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
    permission_dialog: &mut Option<PermissionDialogState>,
    palette: &mut Option<BmuxCommandPalette>,
    terminal: &mut Terminal<&mut W>,
    event: Event,
) -> Result<bool, TuiError> {
    match event {
        Event::Resize(size) => {
            terminal.resize(Rect::new(0, 0, size.width, size.height));
            Ok(true)
        }
        Event::Key(stroke) => {
            if permission_dialog.is_some() {
                return handle_permission_key(client, keymap, chat, permission_dialog, stroke)
                    .await;
            }
            if palette.is_some() {
                return handle_palette_key(client, chat, palette, terminal, stroke).await;
            }
            if is_palette_open_key(keymap, stroke) {
                *palette = Some(BmuxCommandPalette::new());
                return Ok(true);
            }
            let outcome = input::handle_key(&mut chat.app, keymap, stroke);
            if outcome.submitted {
                submit_composer(client, &mut chat.app).await?;
            }
            Ok(outcome.redraw)
        }
        Event::Paste(text) => {
            if let Some(palette) = palette {
                palette.state_mut().query.insert_str(&text);
                return Ok(true);
            }
            chat.app.composer_mut().insert_str(&text);
            chat.app.wake_cursor();
            Ok(true)
        }
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick => Ok(true),
        Event::Mouse(_) | Event::User(_) => Ok(false),
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
        | BmuxAction::EditorDeleteToEnd => Ok(false),
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
            if let Some(command) = command {
                execute_palette_command(client, chat, terminal, command).await?;
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

async fn execute_palette_command<W: Write>(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    terminal: &mut Terminal<&mut W>,
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
            pick_model_for_session(terminal, client, chat).await?;
        }
        PaletteCommand::ListSkills => {
            pick_skill_for_session(terminal, client, chat).await?;
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

async fn pick_model_for_session<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let models = client.session_model_list(None).await?.models;
    let mut picker = model_picker::ModelPickerApp::new(models);
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
                        client
                            .set_session_model(session_id, None, model_id.clone())
                            .await?;
                        chat.app.set_status(format!("model set to {model_id}"));
                        return Ok(());
                    }
                }
                KeyCode::Up => picker.select_previous(),
                KeyCode::Down => picker.select_next(),
                _ => {
                    let outcome = TextInputKeyHandler::new(
                        TextKeymap::default(),
                        TextInputEnterBehavior::InsertNewline,
                    )
                    .handle_key(picker.filter_mut(), stroke);
                    if outcome == TextInputKeyOutcome::Edited {
                        picker.refresh_filter();
                    }
                }
            },
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost)
            | Event::Mouse(_)
            | Event::Tick
            | Event::User(_) => {}
        }
    }
}

async fn pick_skill_for_session<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    chat: &mut ActiveChat,
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
            Event::Key(stroke) => match handle_skill_picker_key(&mut picker, stroke) {
                skill_picker::SkillPickerAction::Continue => {}
                skill_picker::SkillPickerAction::Cancel => return Ok(()),
                skill_picker::SkillPickerAction::Help(skill_id) => {
                    describe_skill(client, chat, skill_id).await?;
                    return Ok(());
                }
                skill_picker::SkillPickerAction::Activate(skill_id) => {
                    activate_skill(client, chat, skill_id).await?;
                    return Ok(());
                }
                skill_picker::SkillPickerAction::Deactivate(skill_id) => {
                    deactivate_skill(client, chat, skill_id).await?;
                    return Ok(());
                }
                skill_picker::SkillPickerAction::Invoke {
                    skill_id,
                    arguments,
                } => {
                    invoke_skill(client, chat, skill_id, arguments).await?;
                    return Ok(());
                }
            },
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost)
            | Event::Mouse(_)
            | Event::Tick
            | Event::User(_) => {}
        }
    }
}

fn handle_skill_picker_key(
    picker: &mut skill_picker::SkillPickerApp,
    stroke: KeyStroke,
) -> skill_picker::SkillPickerAction {
    match picker.mode() {
        skill_picker::SkillPickerMode::Filter => handle_skill_filter_key(picker, stroke),
        skill_picker::SkillPickerMode::Argument => handle_skill_argument_key(picker, stroke),
    }
}

fn handle_skill_filter_key(
    picker: &mut skill_picker::SkillPickerApp,
    stroke: KeyStroke,
) -> skill_picker::SkillPickerAction {
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
            let outcome = TextInputKeyHandler::new(
                TextKeymap::default(),
                TextInputEnterBehavior::InsertNewline,
            )
            .handle_key(picker.filter_mut(), stroke);
            if outcome == TextInputKeyOutcome::Edited {
                picker.refresh_filter();
            }
            skill_picker::SkillPickerAction::Continue
        }
    }
}

fn handle_skill_argument_key(
    picker: &mut skill_picker::SkillPickerApp,
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
            let _outcome = TextInputKeyHandler::new(
                TextKeymap::default(),
                TextInputEnterBehavior::InsertNewline,
            )
            .handle_key(picker.argument_mut(), stroke);
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
    );
    chat.app
        .set_status(format!("attached session {next_session_id}"));
    Ok(())
}

fn is_palette_open_key(keymap: &BmuxKeyMap, stroke: KeyStroke) -> bool {
    keymap.action_for_key(BmuxScope::Chat, stroke) == Some(BmuxAction::CommandPaletteOpen)
}

async fn submit_composer(client: &BcodeClient, app: &mut BmuxApp) -> Result<(), TuiError> {
    let Some(session_id) = app.session_id() else {
        app.set_status("No active session".to_owned());
        return Ok(());
    };
    let message = app.take_pending_submission();
    if message.trim().is_empty() {
        return Ok(());
    }
    match client.send_user_message(session_id, message).await {
        Ok(acceptance) => {
            if acceptance.queued {
                app.mark_pending_submission_queued(acceptance.queue_position);
                app.set_status(format!(
                    "Message queued{}",
                    acceptance
                        .queue_position
                        .map_or_else(String::new, |position| format!(" at #{position}"))
                ));
            } else {
                app.mark_pending_submission_sent();
                app.set_status("Message sent".to_owned());
            }
            Ok(())
        }
        Err(error) => {
            app.restore_pending_submission();
            app.set_status(format!("send failed: {error}"));
            Ok(())
        }
    }
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
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;

    use super::{app::BmuxApp, render};

    #[test]
    fn render_includes_status_and_composer() {
        let app = BmuxApp::new_with_history(None, &[], &[]);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
        let cursor = {
            let mut frame = Frame::new(&mut buffer);
            render::render(&app, &mut frame);
            frame.cursor()
        };

        assert!(buffer.row_symbols(0).unwrap().contains("Bcode BMUX TUI"));
        assert!(buffer.row_symbols(3).unwrap().contains("BMUX backend"));
        assert!(buffer.row_symbols(4).unwrap().contains("Composer"));
        assert!(cursor.is_some());
    }
}
