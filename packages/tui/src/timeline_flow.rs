//! Timeline dialog input flow for the TUI.

use bcode_client::BcodeClient;
use bmux_keyboard::{KeyCode, KeyStroke};

use super::session_flow::ActiveChat;
use super::timeline_dialog::TimelineDialogState;
use super::{TuiError, history_flow};

/// Handle one timeline-dialog key.
pub async fn handle_timeline_key(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    timeline_dialog: &mut Option<TimelineDialogState>,
    stroke: KeyStroke,
) -> Result<bool, TuiError> {
    let Some(dialog) = timeline_dialog else {
        return Ok(false);
    };
    match stroke.key {
        KeyCode::Up | KeyCode::Char('k') => {
            dialog.select_previous();
            chat.app.set_status("timeline".to_owned());
            Ok(true)
        }
        KeyCode::Down | KeyCode::Char('j') => {
            dialog.select_next();
            chat.app.set_status("timeline".to_owned());
            Ok(true)
        }
        KeyCode::PageUp => {
            dialog.page_previous(10);
            chat.app.set_status("timeline".to_owned());
            Ok(true)
        }
        KeyCode::PageDown => {
            dialog.page_next(10);
            chat.app.set_status("timeline".to_owned());
            Ok(true)
        }
        KeyCode::Home => {
            dialog.select_first();
            chat.app.set_status("timeline".to_owned());
            Ok(true)
        }
        KeyCode::End => {
            dialog.select_last();
            chat.app.set_status("timeline".to_owned());
            Ok(true)
        }
        KeyCode::Enter => jump_to_selected_timeline_entry(client, chat, timeline_dialog).await,
        KeyCode::Escape => {
            *timeline_dialog = None;
            chat.app.set_status("timeline closed".to_owned());
            Ok(true)
        }
        _ => Ok(false),
    }
}

async fn jump_to_selected_timeline_entry(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    timeline_dialog: &mut Option<TimelineDialogState>,
) -> Result<bool, TuiError> {
    let selected = timeline_dialog
        .as_ref()
        .and_then(TimelineDialogState::selected_entry)
        .cloned();
    *timeline_dialog = None;
    let Some(entry) = selected else {
        return Ok(true);
    };
    if let Some(index) = entry.transcript_index()
        && chat.app.jump_to_transcript_index(index)
    {
        chat.app.set_status("jumped to timeline message".to_owned());
        return Ok(true);
    }
    let Some(session_id) = chat.app.session_id() else {
        chat.app
            .set_status("timeline requires an active session".to_owned());
        return Ok(true);
    };
    chat.app.set_status("loading timeline message…".to_owned());
    let events =
        history_flow::load_timeline_jump_events(client, session_id, entry.sequence()).await?;
    chat.app.replace_transcript_window(&events);
    if let Some(index) = chat.app.transcript_index_for_sequence(entry.sequence())
        && chat.app.jump_to_transcript_index(index)
    {
        chat.app.set_status("jumped to timeline message".to_owned());
    } else {
        chat.app.set_status(format!(
            "timeline message seq {} was not in the loaded window",
            entry.sequence()
        ));
    }
    Ok(true)
}
