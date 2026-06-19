//! Worktree picker flow for the TUI.

use std::io::Write;
use std::path::PathBuf;

use super::effects::TuiEffect;
use super::runtime_context::{TuiIo, TuiServices};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::SelectionMode;
use bmux_tui::event::{Event, FocusEvent, MouseEvent};
use bmux_tui::geometry::Rect;
use bmux_tui_components::text_input::TextInputControl;

use super::helpers;
use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use super::picker_mouse::picker_row_from_mouse;
use super::session_flow::ActiveChat;
use super::{
    TuiError, text_input_flow, worktree_create_dialog, worktree_create_dialog_render,
    worktree_picker, worktree_picker_render,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerKeyOutcome {
    Continue,
    Selected,
    ForceSelected,
    Canceled,
}

/// Create a worktree using a dialog.
pub async fn create_for_current_session<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    create_with_dialog(io, services, chat).await
}

#[allow(clippy::too_many_lines)]
async fn create_with_dialog<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let current_session_id = chat.app.session_id();
    let default_name = current_session_id.map_or_else(
        || "new-session".to_owned(),
        |session_id| {
            chat.app
                .session_title()
                .map_or_else(|| format!("session-{session_id}"), ToString::to_string)
        },
    );
    let mut dialog = worktree_create_dialog::WorktreeCreateDialog::new(
        &default_name,
        current_session_id.is_some(),
    );
    loop {
        io.terminal.resize(helpers::terminal_area()?);
        io.terminal.draw(|frame| {
            worktree_create_dialog_render::render_dialog(&mut dialog, frame, services.theme);
        })?;
        let Some(event) = io.input.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text)
                if dialog.focus() == worktree_create_dialog::WorktreeCreateFocus::Name =>
            {
                let _ = TextInputControl::new(&worktree_create_dialog::name_input_policy())
                    .handle_paste(dialog.name_mut(), &text);
            }
            Event::Key(stroke) => match stroke.key {
                KeyCode::Escape => return Err(TuiError::Canceled),
                KeyCode::Tab => dialog.focus_next(),
                KeyCode::Enter => {
                    let name = dialog.name_text();
                    if name.is_empty() {
                        chat.app.set_status("worktree name is required".to_owned());
                        continue;
                    }
                    let target = dialog.target();
                    let attach_session_id = match target {
                        worktree_create_dialog::WorktreeCreateTarget::CurrentSession => {
                            current_session_id
                        }
                        worktree_create_dialog::WorktreeCreateTarget::NewSession => None,
                    };
                    let new_session =
                        target == worktree_create_dialog::WorktreeCreateTarget::NewSession;
                    chat.start_effect(TuiEffect::CreateWorktree {
                        request: bcode_worktree_models::WorktreeCreateRequest {
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
                            attach_session_id,
                            new_session,
                            no_setup: false,
                        },
                    });
                    chat.app.set_status("creating worktree…".to_owned());
                    return Ok(());
                }
                KeyCode::Left
                    if dialog.focus() != worktree_create_dialog::WorktreeCreateFocus::Name =>
                {
                    dialog.previous_choice();
                }
                KeyCode::Right
                    if dialog.focus() != worktree_create_dialog::WorktreeCreateFocus::Name =>
                {
                    dialog.next_choice();
                }
                _ if dialog.focus() == worktree_create_dialog::WorktreeCreateFocus::Name => {
                    handle_dialog_name_key(&mut dialog, services.keymap, stroke);
                }
                _ => {}
            },
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost)
            | Event::Tick
            | Event::User(_)
            | Event::Paste(_) => {}
            Event::Mouse(mouse) => {
                handle_dialog_mouse(&mut dialog, mouse);
            }
        }
    }
}

fn handle_dialog_name_key(
    dialog: &mut worktree_create_dialog::WorktreeCreateDialog,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) {
    if let Some(motion) = keymap.editor_selection_motion_for_key(stroke) {
        dialog
            .name_mut()
            .buffer_mut()
            .move_cursor_with_selection(motion, SelectionMode::Extend);
        dialog
            .name_mut()
            .sync_scroll_to_cursor(&worktree_create_dialog::name_input_policy());
        return;
    }
    if let Some(command) = keymap.editor_command_for_key(stroke) {
        dialog.name_mut().buffer_mut().apply_command(command);
        dialog
            .name_mut()
            .sync_scroll_to_cursor(&worktree_create_dialog::name_input_policy());
        return;
    }
    let _ = TextInputControl::new(&worktree_create_dialog::name_input_policy())
        .handle_key(dialog.name_mut(), stroke);
}

fn handle_dialog_mouse(
    dialog: &mut worktree_create_dialog::WorktreeCreateDialog,
    mouse: MouseEvent,
) {
    if dialog.focus() != worktree_create_dialog::WorktreeCreateFocus::Name {
        return;
    }
    let _ = TextInputControl::new(&worktree_create_dialog::name_input_policy())
        .handle_mouse(dialog.name_mut(), mouse);
}

/// Pick a worktree and attach the current session to it.
pub async fn attach_current_session<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let cwd = chat
        .app
        .working_directory()
        .map(std::path::Path::to_path_buf);
    let response = match services
        .passive_client
        .list_worktrees(bcode_worktree_models::WorktreeListRequest { cwd })
        .await
    {
        Ok(response) => response,
        Err(error) => {
            helpers::report_client_issue(&mut chat.app, "worktree list unavailable", &error);
            return Ok(());
        }
    };
    let mut picker = worktree_picker::WorktreePickerApp::new(response.worktrees);
    loop {
        io.terminal.resize(helpers::terminal_area()?);
        io.terminal.draw(|frame| {
            worktree_picker_render::render_picker(&mut picker, frame, services.theme);
        })?;
        let Some(event) = io.input.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => {
                let _ = text_input_flow::handle_paste(picker.filter_mut(), &text);
                picker.refresh_filter();
            }
            Event::Key(stroke) => match handle_picker_key(&mut picker, services.keymap, stroke) {
                PickerKeyOutcome::Continue | PickerKeyOutcome::ForceSelected => {}
                PickerKeyOutcome::Selected => {
                    let Some(path) = picker
                        .selected_worktree()
                        .map(|worktree| worktree.path.clone())
                    else {
                        continue;
                    };
                    start_attach_worktree(chat, path);
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
                    start_attach_worktree(chat, path);
                    return Ok(());
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

/// Pick a worktree and remove it after confirmation.
#[allow(clippy::too_many_lines)]
pub async fn remove_worktree<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let cwd = chat
        .app
        .working_directory()
        .map(std::path::Path::to_path_buf);
    let response = match services
        .passive_client
        .list_worktrees(bcode_worktree_models::WorktreeListRequest { cwd: cwd.clone() })
        .await
    {
        Ok(response) => response,
        Err(error) => {
            helpers::report_client_issue(&mut chat.app, "worktree list unavailable", &error);
            return Ok(());
        }
    };
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
        io.terminal.resize(helpers::terminal_area()?);
        io.terminal.draw(|frame| {
            worktree_picker_render::render_picker(&mut picker, frame, services.theme);
        })?;
        let Some(event) = io.input.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => {
                let _ = text_input_flow::handle_paste(picker.filter_mut(), &text);
                picker.refresh_filter();
            }
            Event::Key(stroke) => {
                let outcome = handle_picker_key(&mut picker, services.keymap, stroke);
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
                        start_remove_worktree(chat, cwd.clone(), path, force);
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
                    start_remove_worktree(chat, cwd.clone(), path, false);
                    return Ok(());
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

fn start_remove_worktree(chat: &mut ActiveChat, cwd: Option<PathBuf>, path: PathBuf, force: bool) {
    chat.start_effect(TuiEffect::RemoveWorktree {
        request: bcode_worktree_models::WorktreeRemoveRequest { cwd, path, force },
    });
    chat.app.set_status("removing worktree…".to_owned());
}

fn start_attach_worktree(chat: &mut ActiveChat, path: PathBuf) {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return;
    };
    chat.start_effect(TuiEffect::AttachWorktree { session_id, path });
    chat.app.set_status("attaching worktree…".to_owned());
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
        _ => {
            if text_input_flow::handle_key(picker.filter_mut(), keymap, stroke)
                != bmux_tui_components::text_input::TextInputOutcome::Ignored
            {
                picker.refresh_filter();
            }
            PickerKeyOutcome::Continue
        }
    }
}
