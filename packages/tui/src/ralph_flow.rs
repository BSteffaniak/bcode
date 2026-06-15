//! Ralph loop TUI flow.

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use bcode_ipc::{
    RalphApproveRequest, RalphCancelRequest, RalphLifecycleRequest, RalphListIterationsRequest,
    RalphListRunsRequest, RalphResumeRequest, RalphRunRequest, RalphRunStatusRequest,
    RalphRunSummary, RalphStatusSummary,
};
use bcode_ralph as ralph_state;
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
use super::{TuiError, ralph_start_dialog, ralph_start_dialog_render};

/// Open the plugin-owned Ralph home UI.
pub async fn open_home<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    match super::ralph_launcher::run_home(io.terminal, repo_root).await? {
        super::ralph_launcher::RalphHomeOutcome::RunCommand(command) => {
            chat.app.replace_composer_with(&command);
            chat.app.set_status(format!(
                "selected {command}; press Enter to run it in this Bcode session"
            ));
        }
        super::ralph_launcher::RalphHomeOutcome::Exit => {
            chat.app.set_status("Ralph UI closed".to_owned());
        }
    }
    Ok(())
}

/// Show latest Ralph loop status for the current repository.
pub async fn show_status(
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let response = services
        .client
        .ralph_run_status(RalphRunStatusRequest {
            repo_root,
            loop_state_dir: None,
        })
        .await?;
    let Some(summary) = response.loop_summary else {
        chat.app
            .set_status("no Ralph loops for current repository".to_owned());
        return Ok(());
    };
    chat.app.push_system_note(format_status_note(
        &summary,
        response.active_run.as_ref(),
        response.interrupted_runs.len(),
    ));
    chat.app.set_status("Ralph status shown".to_owned());
    Ok(())
}

/// Prepare the latest Ralph loop through the server-side runner API.
pub async fn run_loop(services: &TuiServices<'_>, chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let response = services
        .client
        .run_ralph_loop(RalphRunRequest {
            repo_root,
            loop_state_dir: None,
            max_iterations: None,
            no_progress_limit: None,
            require_approval: true,
        })
        .await?;
    chat.app.push_system_note(format!(
        "Ralph run prepared\n* Run: {}\n* Status: {}\n* State: {}\n* Session: {}\n* Next: /ralph approve",
        response.run.run_id,
        response.run.status,
        response.run.state_dir.display(),
        response.run.session_id.as_deref().unwrap_or("<none>")
    ));
    chat.app
        .set_status("Ralph run prepared; approve to start".to_owned());
    Ok(())
}

/// Approve and start the latest prepared Ralph run through the server-side runner API.
pub async fn approve_run(
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let response = services
        .client
        .approve_ralph_run(RalphApproveRequest {
            repo_root,
            loop_state_dir: None,
            run_id: None,
        })
        .await?;
    chat.app.push_system_note(format!(
        "Ralph run approved\n* Run: {}\n* Status: {}\n* State: {}\n* Session: {}",
        response.run.run_id,
        response.run.status,
        response.run.state_dir.display(),
        response.run.session_id.as_deref().unwrap_or("<none>")
    ));
    chat.app.set_status("Ralph run approved".to_owned());
    Ok(())
}

/// List recent Ralph runs for the current repository.
pub async fn list_runs(services: &TuiServices<'_>, chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let response = services
        .client
        .list_ralph_runs(RalphListRunsRequest {
            repo_root,
            loop_state_dir: None,
        })
        .await?;
    let Some(summary) = response.loop_summary else {
        chat.app
            .set_status("no Ralph loops for current repository".to_owned());
        return Ok(());
    };
    let runs = if response.runs.is_empty() {
        "* <none>".to_owned()
    } else {
        response
            .runs
            .iter()
            .map(|run| {
                format!(
                    "* {} — {}{}",
                    run.run_id,
                    run.status,
                    run.stop_reason
                        .as_deref()
                        .map_or_else(String::new, |reason| format!(" ({reason})"))
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    chat.app.push_system_note(format!(
        "Ralph runs\n* Loop: {}\n{}",
        summary.loop_name, runs
    ));
    chat.app.set_status("Ralph runs shown".to_owned());
    Ok(())
}

/// List iterations for the latest Ralph run in the current repository.
pub async fn list_iterations(
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let response = services
        .client
        .list_ralph_iterations(RalphListIterationsRequest {
            repo_root,
            loop_state_dir: None,
            run_id: None,
        })
        .await?;
    let Some(summary) = response.loop_summary else {
        chat.app
            .set_status("no Ralph loops for current repository".to_owned());
        return Ok(());
    };
    let run_label = response
        .run
        .as_ref()
        .map_or_else(|| "<none>".to_owned(), |run| run.run_id.clone());
    let iterations = if response.iterations.is_empty() {
        "* <none>".to_owned()
    } else {
        response
            .iterations
            .iter()
            .map(|iteration| {
                format!(
                    "* #{} — {}{}",
                    iteration.iteration_number,
                    iteration.status,
                    iteration
                        .stop_reason
                        .as_deref()
                        .map_or_else(String::new, |reason| format!(" ({reason})"))
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let validations = if response.validations.is_empty() {
        "* <none>".to_owned()
    } else {
        response
            .validations
            .iter()
            .map(|validation| {
                format!(
                    "* {} — {}{}",
                    validation.command,
                    validation.status,
                    validation
                        .exit_code
                        .map_or_else(String::new, |code| format!(" (exit {code})"))
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    chat.app.push_system_note(format!(
        "Ralph iterations\n* Loop: {}\n* Run: {}\nIterations:\n{}\nValidations:\n{}",
        summary.loop_name, run_label, iterations, validations
    ));
    chat.app.set_status("Ralph iterations shown".to_owned());
    Ok(())
}

/// Prepare an approval-gated resume run for the latest interrupted Ralph run.
pub async fn resume_run(services: &TuiServices<'_>, chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let response = services
        .client
        .resume_ralph_run(RalphResumeRequest {
            repo_root,
            loop_state_dir: None,
            interrupted_run_id: None,
        })
        .await?;
    chat.app.push_system_note(format!(
        "Ralph resume prepared\n* Interrupted run: {}\n* New run: {}\n* Status: {}\n* Next: approve before autonomous execution continues",
        response.interrupted_run.run_id,
        response.resumed_run.run_id,
        response.resumed_run.status
    ));
    chat.app
        .set_status("Ralph resume prepared; approval required".to_owned());
    Ok(())
}

/// Request cancellation for the active Ralph loop run.
pub async fn stop_loop(services: &TuiServices<'_>, chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let response = services
        .client
        .cancel_ralph_loop(RalphCancelRequest {
            repo_root,
            run_id: None,
            loop_state_dir: None,
        })
        .await?;
    chat.app.push_system_note(format!(
        "Ralph stop requested\n* Run: {}\n* Status: {}\n* Cancel requested: {}",
        response.run.run_id, response.run.status, response.cancel_requested
    ));
    chat.app.set_status("Ralph stop requested".to_owned());
    Ok(())
}

fn format_status_note(
    summary: &RalphStatusSummary,
    active_run: Option<&RalphRunSummary>,
    interrupted_run_count: usize,
) -> String {
    let run_status = active_run.map_or_else(
        || "none".to_owned(),
        |run| {
            format!(
                "{} ({}){}{}{}",
                run.run_id,
                run.status,
                run.runtime_work_id
                    .as_deref()
                    .map_or_else(String::new, |work_id| format!(", work: {work_id}")),
                run.stop_reason
                    .as_deref()
                    .map_or_else(String::new, |reason| format!(", stop: {reason}")),
                if run.cancel_requested {
                    ", cancel requested"
                } else {
                    ""
                }
            )
        },
    );
    let validation_commands = if summary.validation_commands.is_empty() {
        "<none>".to_owned()
    } else {
        summary.validation_commands.join("; ")
    };
    format!(
        "Ralph loop status\n* Loop: {}\n* Status: {}\n* Active run: {}\n* Interrupted runs: {}\n* Iterations: {}\n* Checklist: {} checked, {} unchecked\n* Validation: {}\n* Next: {}\n* Progress doc: {}\n* State: {}\n* Isolated work area: {}\n* Session: {}",
        summary.loop_name,
        summary.status,
        run_status,
        interrupted_run_count,
        summary.iteration_count,
        summary.checked_count,
        summary.unchecked_count,
        validation_commands,
        summary.next_action,
        summary.progress_doc_path.display(),
        summary.state_dir.display(),
        summary
            .work_area_path
            .as_ref()
            .map_or_else(|| "<none>".to_owned(), |path| path.display().to_string()),
        summary.session_id.as_deref().unwrap_or("<none>")
    )
}

/// Build and show a Ralph orchestration prompt for the current repository.
pub fn show_prompt(
    chat: &mut ActiveChat,
    kind: ralph_state::RalphPromptKind,
) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let Some(summary) = ralph_state::latest_loop(&repo_root)? else {
        chat.app
            .set_status("no Ralph loops for current repository".to_owned());
        return Ok(());
    };
    let prompt = ralph_state::build_prompt(&summary, kind)?;
    ralph_state::append_lifecycle_event_for_summary(
        &summary,
        ralph_state::RalphLifecycleEventKind::PromptPrepared,
        "Prepared Ralph orchestration prompt",
    )?;
    chat.app.push_system_note(format!(
        "Ralph prompt prepared\n* Loop: {}\n* Progress doc: {}\n\n{}",
        summary.loop_name,
        summary.progress_doc_path.display(),
        prompt
    ));
    chat.app
        .set_status("Ralph prompt prepared; submit manually when ready".to_owned());
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
    ralph_state::append_lifecycle_event_for_summary(
        &summary,
        ralph_state::RalphLifecycleEventKind::ProgressOpened,
        "Viewed Ralph progress doc path",
    )?;
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
    let repo_root = chat
        .app
        .working_directory()
        .map_or_else(std::env::current_dir, |path| Ok(path.to_path_buf()))?;
    let default_validation_commands = ralph_state::default_validation_commands(&repo_root);
    let mut dialog =
        ralph_start_dialog::RalphStartDialog::new(&default_name, &default_validation_commands);
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
                let _ = TextInputControl::new(&ralph_start_dialog::input_policy())
                    .handle_paste(dialog.focused_input_mut(), &text);
            }
            Event::Key(stroke) => match stroke.key {
                KeyCode::Escape => return Err(TuiError::Canceled),
                KeyCode::Tab => dialog.focus_next(),
                KeyCode::Enter => {
                    if confirm_start_loop(&mut dialog, services, chat).await? {
                        return Ok(());
                    }
                }
                _ => handle_loop_name_key(&mut dialog, services.keymap, stroke),
            },
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
            Event::Mouse(mouse) => handle_loop_name_mouse(&mut dialog, mouse),
        }
    }
}

async fn confirm_start_loop(
    dialog: &mut ralph_start_dialog::RalphStartDialog,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<bool, TuiError> {
    let loop_name = dialog.loop_name_text();
    if loop_name.is_empty() {
        dialog.set_status("Ralph loop name is required");
        return Ok(false);
    }
    let repo_root = chat
        .app
        .working_directory()
        .map_or_else(std::env::current_dir, |path| Ok(path.to_path_buf()))?;
    let state =
        ralph_state::create_initial_loop_state(&loop_name, &repo_root, chat.app.session_title())?;
    let validation_commands = dialog.validation_command_texts();
    ralph_state::set_validation_commands(&state.state_dir, &validation_commands, "setup")?;
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
        ralph_state::write_context_pack(&state, chat.app.session_title(), &history.events)?;
        ralph_state::generate_progress_doc_from_context(&state, &loop_name, &repo_root)?;
    }
    let work_area = services
        .client
        .create_worktree(WorktreeCreateRequest {
            name: format!("ralph-{loop_name}"),
            cwd: Some(repo_root),
            path: dialog.work_area_path_text().map(PathBuf::from),
            branch: None,
            new_branch: dialog.branch_text(),
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
    if let Some(session) = &work_area.session {
        let _event = services
            .client
            .record_ralph_lifecycle(RalphLifecycleRequest {
                session_id: session.id,
                loop_name: loop_name.clone(),
                state_dir: state.state_dir.clone(),
                kind: "work_area_created".to_owned(),
                message: "Created Ralph isolated work area".to_owned(),
                occurred_at_ms: now_ms(),
            })
            .await?;
    }
    let validation_summary = if validation_commands.is_empty() {
        "<none>".to_owned()
    } else {
        validation_commands.join("; ")
    };
    chat.app.push_system_note(format!(
        "Ralph loop created\n* Loop: {loop_name}\n* Progress doc: {}\n* State: {}\n* Isolated work area: {}\n* Session: {}\n* Validation: {}\n* Next: capture conversation context into the progress doc",
        state.progress_doc_path.display(),
        state.state_dir.display(),
        work_area.path.display(),
        work_area_session_id.as_deref().unwrap_or("<none>"),
        validation_summary
    ));
    chat.app.set_status("Ralph loop created".to_owned());
    Ok(true)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn handle_loop_name_key(
    dialog: &mut ralph_start_dialog::RalphStartDialog,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) {
    if let Some(motion) = keymap.editor_selection_motion_for_key(stroke) {
        dialog
            .focused_input_mut()
            .buffer_mut()
            .move_cursor_with_selection(motion, SelectionMode::Extend);
        dialog
            .focused_input_mut()
            .sync_scroll_to_cursor(&ralph_start_dialog::input_policy());
        return;
    }
    if let Some(command) = keymap.editor_command_for_key(stroke) {
        dialog
            .focused_input_mut()
            .buffer_mut()
            .apply_command(command);
        dialog
            .focused_input_mut()
            .sync_scroll_to_cursor(&ralph_start_dialog::input_policy());
        return;
    }
    let _ = TextInputControl::new(&ralph_start_dialog::input_policy())
        .handle_key(dialog.focused_input_mut(), stroke);
}

fn handle_loop_name_mouse(dialog: &mut ralph_start_dialog::RalphStartDialog, mouse: MouseEvent) {
    let _ = TextInputControl::new(&ralph_start_dialog::input_policy())
        .handle_mouse(dialog.focused_input_mut(), mouse);
}
