//! Session picker event flow for the BMUX backend.

use std::io::Write;

use bcode_client::BcodeClient;
use bcode_ipc::Event as BcodeEvent;
use bcode_session_models::SessionId;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::crossterm::poll_event;
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyOutcome};
use bmux_tui::terminal::Terminal;

use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use super::picker_mouse::picker_row_from_mouse;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::app::BmuxApp;
use super::helpers;
use super::{EVENT_POLL_TIMEOUT, TuiError, history_flow};
use super::{session_picker, session_picker_render};

/// Active chat session state shared by BMUX backend flows.
pub(super) struct ActiveChat {
    pub(super) app: BmuxApp,
    pub(super) session_id: SessionId,
    pub(super) event_sender: mpsc::UnboundedSender<BcodeEvent>,
    pub(super) event_receiver: mpsc::UnboundedReceiver<BcodeEvent>,
    pub(super) event_task: JoinHandle<()>,
}

/// Hydrate model and skill status for the active session.
pub(super) async fn hydrate_status(client: &BcodeClient, app: &mut BmuxApp) {
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
    if let Some(model) = model {
        app.apply_model_status(model);
    }
    let skill_count = active_skills.as_ref().map_or(0, Vec::len);
    app.set_status(format!("model: {model_text}; active skills: {skill_count}"));
}

/// Switch the active chat to another session.
pub(super) async fn switch_session(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    next_session_id: SessionId,
) -> Result<(), TuiError> {
    chat.event_task.abort();
    while chat.event_receiver.try_recv().is_ok() {}
    let (attached, next_task) = history_flow::attach_session_event_stream(
        client,
        next_session_id,
        chat.event_sender.clone(),
    )
    .await?;
    chat.event_task = next_task;
    chat.session_id = next_session_id;
    chat.app = BmuxApp::new_with_history(
        Some(next_session_id),
        &attached.history,
        &attached.input_history,
        attached.history.len() >= super::INITIAL_HISTORY_EVENT_LIMIT,
    );
    hydrate_status(client, &mut chat.app).await;
    Ok(())
}

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
        terminal.resize(helpers::terminal_area()?);
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
        terminal.resize(helpers::terminal_area()?);
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
            let outcome = helpers::handle_text_buffer_key(
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
    let outcome = helpers::handle_text_buffer_key(
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
