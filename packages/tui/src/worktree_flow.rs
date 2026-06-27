//! Worktree creation flow for the TUI.

use std::io::Write;

use super::effects::TuiEffect;
use super::runtime_context::{TuiIo, TuiServices};
use super::session_flow::ActiveChat;
use super::{TuiError, wt_create_dialog, wt_create_dialog_render};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::SelectionMode;
use bmux_tui::event::{Event, FocusEvent, MouseEvent};
use bmux_tui::geometry::Rect;
use bmux_tui_components::text_input::TextInputControl;

use super::helpers;
use super::keymap::BmuxKeyMap;

/// Create a worktree using a dialog.
#[allow(clippy::too_many_lines)]
pub async fn create_for_current_session<W: Write>(
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
    let mut dialog =
        wt_create_dialog::WorktreeCreateDialog::new(&default_name, current_session_id.is_some());
    loop {
        io.terminal.resize(helpers::terminal_area()?);
        io.terminal.draw(|frame| {
            wt_create_dialog_render::render_dialog(&mut dialog, frame, services.theme);
        })?;
        let Some(event) = io.input.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) if dialog.focus() == wt_create_dialog::WorktreeCreateFocus::Name => {
                let _ = TextInputControl::new(&wt_create_dialog::name_input_policy())
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
                        wt_create_dialog::WorktreeCreateTarget::CurrentSession => {
                            current_session_id
                        }
                        wt_create_dialog::WorktreeCreateTarget::NewSession => None,
                    };
                    let new_session = target == wt_create_dialog::WorktreeCreateTarget::NewSession;
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
                KeyCode::Left if dialog.focus() != wt_create_dialog::WorktreeCreateFocus::Name => {
                    dialog.previous_choice();
                }
                KeyCode::Right if dialog.focus() != wt_create_dialog::WorktreeCreateFocus::Name => {
                    dialog.next_choice();
                }
                _ if dialog.focus() == wt_create_dialog::WorktreeCreateFocus::Name => {
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
    dialog: &mut wt_create_dialog::WorktreeCreateDialog,
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
            .sync_scroll_to_cursor(&wt_create_dialog::name_input_policy());
        return;
    }
    if let Some(command) = keymap.editor_command_for_key(stroke) {
        dialog.name_mut().buffer_mut().apply_command(command);
        dialog
            .name_mut()
            .sync_scroll_to_cursor(&wt_create_dialog::name_input_policy());
        return;
    }
    let _ = TextInputControl::new(&wt_create_dialog::name_input_policy())
        .handle_key(dialog.name_mut(), stroke);
}

fn handle_dialog_mouse(dialog: &mut wt_create_dialog::WorktreeCreateDialog, mouse: MouseEvent) {
    if dialog.focus() != wt_create_dialog::WorktreeCreateFocus::Name {
        return;
    }
    let _ = TextInputControl::new(&wt_create_dialog::name_input_policy())
        .handle_mouse(dialog.name_mut(), mouse);
}
