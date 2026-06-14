//! Ralph loop TUI flow.

use std::io::Write;

use bcode_session_models::{SessionHistoryDirection, SessionHistoryQuery};
use bcode_worktree_models::WorktreeCreateRequest;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::SelectionMode;
use bmux_tui::event::{Event, FocusEvent, MouseEvent};
use bmux_tui::geometry::Rect;
use bmux_tui_components::text_input::TextInputControl;

use super::helpers;
use super::keymap::BmuxKeyMap;
use super::runtime_context::{TuiIo, TuiServices};
use super::session_flow::ActiveChat;
use super::{TuiError, ralph_start_dialog, ralph_start_dialog_render, ralph_state};

/// Show latest Ralph loop status for the current repository.
pub fn show_status(chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let Some(summary) = ralph_state::latest_loop(&repo_root)? else {
        chat.app
            .set_status("no Ralph loops for current repository".to_owned());
        return Ok(());
    };
    chat.app.push_system_note(format!(
        "Ralph loop status\n* Loop: {}\n* Status: {}\n* Iterations: {}\n* Next: {}\n* Progress doc: {}\n* State: {}\n* Isolated work area: {}\n* Session: {}",
        summary.loop_name,
        summary.status,
        summary.iteration_count,
        summary.next_action,
        summary.progress_doc_path.display(),
        summary.state_dir.display(),
        summary
            .work_area_path
            .as_ref()
            .map_or_else(|| "<none>".to_owned(), |path| path.display().to_string()),
        summary.session_id.as_deref().unwrap_or("<none>")
    ));
    chat.app.set_status("Ralph status shown".to_owned());
    Ok(())
}

/// Show latest Ralph progress doc path for the current repository.
pub fn open_progress(chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let Some(summary) = ralph_state::latest_loop(&repo_root)? else {
        chat.app
            .set_status("no Ralph loops for current repository".to_owned());
        return Ok(());
    };
    chat.app.push_system_note(format!(
        "Ralph progress doc\n* Loop: {}\n* Path: {}",
        summary.loop_name,
        summary.progress_doc_path.display()
    ));
    chat.app
        .set_status("Ralph progress doc path shown".to_owned());
    Ok(())
}

fn current_repo_root(chat: &ActiveChat) -> Result<std::path::PathBuf, TuiError> {
    chat.app
        .working_directory()
        .map_or_else(std::env::current_dir, |path| Ok(path.to_path_buf()))
        .map_err(TuiError::Io)
}

/// Start the Ralph loop setup flow.
pub async fn start_loop<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let default_name = chat
        .app
        .session_title()
        .map_or_else(|| "new-ralph-loop".to_owned(), ToString::to_string);
    let mut dialog = ralph_start_dialog::RalphStartDialog::new(&default_name);
    loop {
        io.terminal.resize(helpers::terminal_area()?);
        io.terminal
            .draw(|frame| ralph_start_dialog_render::render_dialog(&mut dialog, frame))?;
        let Some(event) = io.input.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => {
                let _ = TextInputControl::new(&ralph_start_dialog::loop_name_input_policy())
                    .handle_paste(dialog.loop_name_mut(), &text);
            }
            Event::Key(stroke) => match stroke.key {
                KeyCode::Escape => return Err(TuiError::Canceled),
                KeyCode::Enter => {
                    let loop_name = dialog.loop_name_text();
                    if loop_name.is_empty() {
                        dialog.set_status("Ralph loop name is required");
                        continue;
                    }
                    let repo_root = chat
                        .app
                        .working_directory()
                        .map_or_else(std::env::current_dir, |path| Ok(path.to_path_buf()))?;
                    let state = ralph_state::create_initial_loop_state(
                        &loop_name,
                        &repo_root,
                        chat.app.session_title(),
                    )?;
                    if let Some(session_id) = chat.app.session_id() {
                        let history = services
                            .client
                            .session_history_page(
                                session_id,
                                SessionHistoryQuery {
                                    cursor: None,
                                    limit: 64,
                                    direction: SessionHistoryDirection::Backward,
                                },
                            )
                            .await?;
                        ralph_state::write_context_pack(
                            &state,
                            chat.app.session_title(),
                            &history.events,
                        )?;
                        ralph_state::generate_progress_doc_from_context(
                            &state, &loop_name, &repo_root,
                        )?;
                    }
                    let work_area = services
                        .client
                        .create_worktree(WorktreeCreateRequest {
                            name: format!("ralph-{loop_name}"),
                            cwd: Some(repo_root.clone()),
                            path: None,
                            branch: None,
                            new_branch: None,
                            base_ref: Some(bcode_worktree_models::WorktreeBaseRef::Head),
                            detach: false,
                            force: false,
                            attach_session_id: None,
                            new_session: true,
                            no_setup: false,
                        })
                        .await?;
                    let work_area_session_id = work_area
                        .session
                        .as_ref()
                        .map(|session| session.id.to_string());
                    ralph_state::record_work_area(
                        &state,
                        &work_area.path,
                        work_area.branch.as_deref(),
                        work_area_session_id.as_deref(),
                    )?;
                    chat.app.push_system_note(format!(
                        "Ralph loop created\n* Loop: {loop_name}\n* Progress doc: {}\n* State: {}\n* Isolated work area: {}\n* Session: {}\n* Next: capture conversation context into the progress doc",
                        state.progress_doc_path.display(),
                        state.state_dir.display(),
                        work_area.path.display(),
                        work_area_session_id.as_deref().unwrap_or("<none>")
                    ));
                    chat.app.set_status("Ralph loop created".to_owned());
                    return Ok(());
                }
                _ => handle_loop_name_key(&mut dialog, services.keymap, stroke),
            },
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
            Event::Mouse(mouse) => handle_loop_name_mouse(&mut dialog, mouse),
        }
    }
}

fn handle_loop_name_key(
    dialog: &mut ralph_start_dialog::RalphStartDialog,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) {
    if let Some(motion) = keymap.editor_selection_motion_for_key(stroke) {
        dialog
            .loop_name_mut()
            .buffer_mut()
            .move_cursor_with_selection(motion, SelectionMode::Extend);
        dialog
            .loop_name_mut()
            .sync_scroll_to_cursor(&ralph_start_dialog::loop_name_input_policy());
        return;
    }
    if let Some(command) = keymap.editor_command_for_key(stroke) {
        dialog.loop_name_mut().buffer_mut().apply_command(command);
        dialog
            .loop_name_mut()
            .sync_scroll_to_cursor(&ralph_start_dialog::loop_name_input_policy());
        return;
    }
    let _ = TextInputControl::new(&ralph_start_dialog::loop_name_input_policy())
        .handle_key(dialog.loop_name_mut(), stroke);
}

fn handle_loop_name_mouse(dialog: &mut ralph_start_dialog::RalphStartDialog, mouse: MouseEvent) {
    let _ = TextInputControl::new(&ralph_start_dialog::loop_name_input_policy())
        .handle_mouse(dialog.loop_name_mut(), mouse);
}
