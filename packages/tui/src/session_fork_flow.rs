//! Session fork/clone flow for the TUI.

use std::io::Write;

use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionHistoryCursor, SessionHistoryDirection,
    SessionHistoryQuery,
};
use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui_components::text_input::TextInputControl;

use super::helpers;
use super::runtime_context::{TuiIo, TuiServices};
use super::session_flow::{self, ActiveChat};
use super::{TuiError, session_fork_dialog, session_fork_dialog_render};

/// Open the fork dialog for the current session.
pub async fn fork_current_session<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let source_title = chat
        .app
        .session_title()
        .map_or_else(|| session_id.to_string(), ToString::to_string);
    let mut dialog = session_fork_dialog::SessionForkDialog::new(
        session_fork_dialog::SessionForkDialogMode::Fork,
        &format!("[fork] {source_title}"),
    );
    let submission = run_dialog(io, chat, &mut dialog).await?;
    let Some(prompt) = latest_user_prompt_before_tail(services, session_id).await? else {
        chat.app
            .set_status("no user prompt found to fork from".to_owned());
        return Ok(());
    };
    let result = services
        .client
        .fork_session(session_id, prompt.sequence, submission.name)
        .await?;
    let draft = result.draft.or(Some(prompt.text));
    if submission.switch_after_create {
        let new_session_id = result.session.id;
        session_flow::switch_session(io.terminal, services.client, chat, new_session_id)?;
        if submission.install_draft
            && let Some(draft) = draft.as_deref()
        {
            chat.app.replace_composer_with(draft);
        }
        chat.app
            .set_status("forked session and switched".to_owned());
    } else {
        if submission.install_draft
            && let Some(draft) = draft.as_deref()
        {
            chat.app.replace_composer_with(draft);
        }
        chat.app
            .set_status(format!("forked session {}", result.session.id));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ForkPromptCandidate {
    sequence: u64,
    text: String,
}

async fn latest_user_prompt_before_tail(
    services: &TuiServices<'_>,
    session_id: bcode_session_models::SessionId,
) -> Result<Option<ForkPromptCandidate>, TuiError> {
    let page = services
        .client
        .session_history_page(
            session_id,
            SessionHistoryQuery {
                cursor: Some(SessionHistoryCursor { sequence: u64::MAX }),
                limit: 256,
                direction: SessionHistoryDirection::Backward,
            },
        )
        .await?;
    Ok(page
        .events
        .iter()
        .find_map(user_prompt_candidate_from_event))
}

fn user_prompt_candidate_from_event(event: &SessionEvent) -> Option<ForkPromptCandidate> {
    let SessionEventKind::UserMessage { text, .. } = &event.kind else {
        return None;
    };
    Some(ForkPromptCandidate {
        sequence: event.sequence,
        text: text.clone(),
    })
}

/// Open the clone dialog for the current session, create the clone, and optionally switch to it.
pub async fn clone_current_session<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let source_title = chat
        .app
        .session_title()
        .map_or_else(|| session_id.to_string(), ToString::to_string);
    let mut dialog = session_fork_dialog::SessionForkDialog::new(
        session_fork_dialog::SessionForkDialogMode::Clone,
        &format!("[clone] {source_title}"),
    );
    let submission = run_dialog(io, chat, &mut dialog).await?;
    let result = services
        .client
        .clone_session(session_id, submission.name)
        .await?;
    if submission.switch_after_create {
        let new_session_id = result.session.id;
        session_flow::switch_session(io.terminal, services.client, chat, new_session_id)?;
        chat.app
            .set_status("cloned session and switched".to_owned());
    } else {
        chat.app.apply_session_summary(&result.session);
        chat.app
            .set_status(format!("cloned session {}", result.session.id));
    }
    Ok(())
}

async fn run_dialog<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    chat: &mut ActiveChat,
    dialog: &mut session_fork_dialog::SessionForkDialog,
) -> Result<session_fork_dialog::SessionForkDialogSubmission, TuiError> {
    loop {
        io.terminal.resize(helpers::terminal_area()?);
        io.terminal
            .draw(|frame| session_fork_dialog_render::render_dialog(dialog, frame))?;
        let Some(event) = io.input.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text)
                if dialog.focus() == session_fork_dialog::SessionForkDialogFocus::Name =>
            {
                let _ = TextInputControl::new(&session_fork_dialog::name_input_policy())
                    .handle_paste(dialog.name_mut(), &text);
            }
            Event::Paste(_) => {}
            Event::Key(stroke) => match stroke.key {
                KeyCode::Escape => return Err(TuiError::Canceled),
                KeyCode::Tab => dialog.focus_next(),
                KeyCode::Enter => return Ok(dialog.submission()),
                KeyCode::Left => dialog.value_previous(),
                KeyCode::Right => dialog.value_next(),
                _ if dialog.focus() == session_fork_dialog::SessionForkDialogFocus::Name => {
                    let _ = TextInputControl::new(&session_fork_dialog::name_input_policy())
                        .handle_key(dialog.name_mut(), stroke);
                }
                _ => {}
            },
            Event::Mouse(_) => {}
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
        let _ = &chat;
    }
}
