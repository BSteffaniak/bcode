//! Worktree picker flow for the TUI.

use std::io::Write;
use std::path::PathBuf;

use bcode_client::BcodeClient;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::crossterm::poll_event;
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;

use super::helpers;
use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use super::picker_mouse::picker_row_from_mouse;
use super::session_flow::ActiveChat;
use super::{
    EVENT_POLL_TIMEOUT, TuiError, session_flow, worktree_create_dialog,
    worktree_create_dialog_render, worktree_picker, worktree_picker_render,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerKeyOutcome {
    Continue,
    Selected,
    ForceSelected,
    Canceled,
}

/// Create a worktree for the current session using a dialog.
pub async fn create_for_current_session<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    chat: &mut ActiveChat,
    keymap: &BmuxKeyMap,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let default_name = chat
        .app
        .session_title()
        .map_or_else(|| format!("session-{session_id}"), ToString::to_string);
    let mut dialog = worktree_create_dialog::WorktreeCreateDialog::new(&default_name);
    loop {
        terminal.resize(helpers::terminal_area()?);
        terminal.draw(|frame| worktree_create_dialog_render::render_dialog(&dialog, frame))?;
        let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? else {
            continue;
        };
        match event {
            Event::Resize(size) => terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text)
                if dialog.focus() == worktree_create_dialog::WorktreeCreateFocus::Name =>
            {
                dialog.name_mut().insert_str(&text);
            }
            Event::Key(stroke) => match stroke.key {
                KeyCode::Escape => return Err(TuiError::Canceled),
                KeyCode::Tab => dialog.focus_next(),
                KeyCode::Left
                    if dialog.focus() == worktree_create_dialog::WorktreeCreateFocus::Base =>
                {
                    dialog.previous_base();
                }
                KeyCode::Right
                    if dialog.focus() == worktree_create_dialog::WorktreeCreateFocus::Base =>
                {
                    dialog.next_base();
                }
                KeyCode::Enter => {
                    let name = dialog.name_text();
                    if name.is_empty() {
                        chat.app.set_status("worktree name is required".to_owned());
                        continue;
                    }
                    let response = client
                        .create_worktree(bcode_worktree_models::WorktreeCreateRequest {
                            name,
                            cwd: chat
                                .app
                                .working_directory()
                                .map(std::path::Path::to_path_buf),
                            path: None,
                            branch: None,
                            new_branch: None,
                            base_ref: Some(dialog.base().model()),
                            detach: false,
                            force: false,
                            attach_session_id: Some(session_id),
                            new_session: false,
                            no_setup: false,
                        })
                        .await?;
                    if let Some(session) = response.session {
                        chat.app.apply_session_summary(&session);
                    }
                    chat.app.push_system_note(format!(
                        "Created worktree for current session\n* Path: {}",
                        response.path.display()
                    ));
                    chat.app.set_status("created worktree".to_owned());
                    return Ok(());
                }
                _ if dialog.focus() == worktree_create_dialog::WorktreeCreateFocus::Name => {
                    let _ = helpers::handle_text_buffer_key(
                        dialog.name_mut(),
                        keymap,
                        stroke,
                        bmux_tui::input::TextInputEnterBehavior::Submit,
                    );
                }
                _ => {}
            },
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost)
            | Event::Tick
            | Event::User(_)
            | Event::Mouse(_)
            | Event::Paste(_) => {}
        }
    }
}

/// Pick a worktree and attach the current session to it.
pub async fn attach_current_session<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    chat: &mut ActiveChat,
    keymap: &BmuxKeyMap,
) -> Result<(), TuiError> {
    let cwd = chat
        .app
        .working_directory()
        .map(std::path::Path::to_path_buf);
    let response = client
        .list_worktrees(bcode_worktree_models::WorktreeListRequest { cwd })
        .await?;
    let mut picker = worktree_picker::WorktreePickerApp::new(response.worktrees);
    loop {
        terminal.resize(helpers::terminal_area()?);
        terminal.draw(|frame| worktree_picker_render::render_picker(&mut picker, frame))?;
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
                PickerKeyOutcome::Continue | PickerKeyOutcome::ForceSelected => {}
                PickerKeyOutcome::Selected => {
                    let Some(path) = picker
                        .selected_worktree()
                        .map(|worktree| worktree.path.clone())
                    else {
                        continue;
                    };
                    attach_path(client, chat, path).await?;
                    return Ok(());
                }
                PickerKeyOutcome::Canceled => return Err(TuiError::Canceled),
            },
            Event::Mouse(mouse) => {
                if let Some(row) = picker_row_from_mouse(mouse)
                    && picker.select_visible(row)
                    && let Some(path) = picker
                        .selected_worktree()
                        .map(|worktree| worktree.path.clone())
                {
                    attach_path(client, chat, path).await?;
                    return Ok(());
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

/// Pick a worktree and remove it after confirmation.
pub async fn remove_worktree<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    chat: &mut ActiveChat,
    keymap: &BmuxKeyMap,
) -> Result<(), TuiError> {
    let cwd = chat
        .app
        .working_directory()
        .map(std::path::Path::to_path_buf);
    let response = client
        .list_worktrees(bcode_worktree_models::WorktreeListRequest { cwd: cwd.clone() })
        .await?;
    let linked = response
        .worktrees
        .into_iter()
        .filter(|worktree| !worktree.is_main)
        .collect::<Vec<_>>();
    let mut picker = worktree_picker::WorktreePickerApp::new(linked);
    picker.set_status(
        "Select linked worktree to remove. Enter removes; F force-removes dirty worktrees; Esc cancels"
            .to_owned(),
    );
    loop {
        terminal.resize(helpers::terminal_area()?);
        terminal.draw(|frame| worktree_picker_render::render_picker(&mut picker, frame))?;
        let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? else {
            continue;
        };
        match event {
            Event::Resize(size) => terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => {
                picker.filter_mut().insert_str(&text);
                picker.refresh_filter();
            }
            Event::Key(stroke) => {
                let outcome = handle_picker_key(&mut picker, keymap, stroke);
                match outcome {
                    PickerKeyOutcome::Continue => {}
                    PickerKeyOutcome::Selected | PickerKeyOutcome::ForceSelected => {
                        let force = matches!(outcome, PickerKeyOutcome::ForceSelected);
                        let Some(path) = picker
                            .selected_worktree()
                            .map(|worktree| worktree.path.clone())
                        else {
                            continue;
                        };
                        let removed = client
                            .remove_worktree(bcode_worktree_models::WorktreeRemoveRequest {
                                cwd: cwd.clone(),
                                path: path.clone(),
                                force,
                            })
                            .await?;
                        chat.app
                            .set_status(format!("removed worktree {}", removed.path.display()));
                        return Ok(());
                    }
                    PickerKeyOutcome::Canceled => return Err(TuiError::Canceled),
                }
            }
            Event::Mouse(mouse) => {
                if let Some(row) = picker_row_from_mouse(mouse)
                    && picker.select_visible(row)
                    && let Some(path) = picker
                        .selected_worktree()
                        .map(|worktree| worktree.path.clone())
                {
                    let removed = client
                        .remove_worktree(bcode_worktree_models::WorktreeRemoveRequest {
                            cwd: cwd.clone(),
                            path: path.clone(),
                            force: false,
                        })
                        .await?;
                    chat.app
                        .set_status(format!("removed worktree {}", removed.path.display()));
                    return Ok(());
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

async fn attach_path(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    path: PathBuf,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let session = client
        .change_session_working_directory(session_id, path.clone())
        .await?;
    chat.app.apply_session_summary(&session);
    session_flow::hydrate_status(client, &mut chat.app).await;
    chat.app.set_status(format!("worktree: {}", path.display()));
    Ok(())
}

fn handle_picker_key(
    picker: &mut worktree_picker::WorktreePickerApp,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) -> PickerKeyOutcome {
    if let Some(action) = keymap.action_for_key(BmuxScope::SessionPicker, stroke) {
        return match action {
            BmuxAction::SelectCancel => PickerKeyOutcome::Canceled,
            BmuxAction::SelectConfirm => PickerKeyOutcome::Selected,
            BmuxAction::SelectUp => {
                picker.select_previous();
                PickerKeyOutcome::Continue
            }
            BmuxAction::SelectDown => {
                picker.select_next();
                PickerKeyOutcome::Continue
            }
            _ => PickerKeyOutcome::Continue,
        };
    }
    match stroke.key {
        KeyCode::Char('f' | 'F') => PickerKeyOutcome::ForceSelected,
        KeyCode::Char(character) => {
            picker.filter_mut().insert_char(character);
            picker.refresh_filter();
            PickerKeyOutcome::Continue
        }
        KeyCode::Backspace => {
            picker.filter_mut().delete_backward();
            picker.refresh_filter();
            PickerKeyOutcome::Continue
        }
        KeyCode::Escape => PickerKeyOutcome::Canceled,
        KeyCode::Enter => PickerKeyOutcome::Selected,
        KeyCode::Up => {
            picker.select_previous();
            PickerKeyOutcome::Continue
        }
        KeyCode::Down => {
            picker.select_next();
            PickerKeyOutcome::Continue
        }
        _ => PickerKeyOutcome::Continue,
    }
}
