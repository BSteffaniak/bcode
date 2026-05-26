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
use super::{EVENT_POLL_TIMEOUT, TuiError, session_flow, worktree_picker, worktree_picker_render};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerKeyOutcome {
    Continue,
    Selected,
    Canceled,
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
                PickerKeyOutcome::Continue => {}
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
