//! Session picker event flow for the BMUX backend.

use std::io::Write;

use bcode_client::BcodeClient;
use bcode_session_models::SessionId;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::crossterm::poll_event;
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyOutcome};
use bmux_tui::terminal::Terminal;

use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use super::picker_mouse::picker_row_from_mouse;
use super::{EVENT_POLL_TIMEOUT, TuiError, handle_text_buffer_key, terminal_area};
use super::{session_picker, session_picker_render};

/// Initial mutation mode for opening the session picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionPickerStartMode {
    /// Start in rename mode.
    Rename,
    /// Start in delete-confirmation mode.
    Delete,
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

/// Pick an existing session or create one.
pub(super) async fn pick_session<W: Write>(
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

/// Pick a session to rename or delete.
pub(super) async fn pick_session_for_mutation<W: Write>(
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
        session_picker::SessionPickerMode::Rename => {
            handle_picker_rename_key(picker, keymap, stroke)
        }
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
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) -> PickerKeyOutcome {
    if stroke.key == KeyCode::Escape {
        picker.cancel_rename();
        return PickerKeyOutcome::Continue;
    }
    if stroke.key == KeyCode::Enter {
        return PickerKeyOutcome::Rename;
    }
    let outcome = handle_text_buffer_key(
        picker.rename_mut(),
        keymap,
        stroke,
        TextInputEnterBehavior::Submit,
    );
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
