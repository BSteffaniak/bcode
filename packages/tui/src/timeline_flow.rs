//! Timeline dialog input flow for the TUI.

use bmux_keyboard::{KeyCode, KeyStroke};

use super::session_flow::ActiveChat;
use super::timeline_dialog::TimelineDialogState;

/// Handle one timeline-dialog key.
pub fn handle_timeline_key(
    chat: &mut ActiveChat,
    timeline_dialog: &mut Option<TimelineDialogState>,
    stroke: KeyStroke,
) -> bool {
    let Some(dialog) = timeline_dialog else {
        return false;
    };
    match stroke.key {
        KeyCode::Up | KeyCode::Char('k') => {
            dialog.select_previous();
            chat.app.set_status("timeline".to_owned());
            true
        }
        KeyCode::Down | KeyCode::Char('j') => {
            dialog.select_next();
            chat.app.set_status("timeline".to_owned());
            true
        }
        KeyCode::PageUp => {
            dialog.page_previous(10);
            chat.app.set_status("timeline".to_owned());
            true
        }
        KeyCode::PageDown => {
            dialog.page_next(10);
            chat.app.set_status("timeline".to_owned());
            true
        }
        KeyCode::Home => {
            dialog.select_first();
            chat.app.set_status("timeline".to_owned());
            true
        }
        KeyCode::End => {
            dialog.select_last();
            chat.app.set_status("timeline".to_owned());
            true
        }
        KeyCode::Enter => {
            let selected = dialog.selected_entry().cloned();
            *timeline_dialog = None;
            if let Some(entry) = selected {
                if let Some(index) = entry.transcript_index() {
                    if chat.app.jump_to_transcript_index(index) {
                        chat.app.set_status("jumped to timeline message".to_owned());
                    } else {
                        chat.app
                            .set_status("timeline message is not currently visible".to_owned());
                    }
                } else {
                    chat.app.set_status(format!(
                        "timeline message seq {} is outside the loaded transcript window",
                        entry.sequence()
                    ));
                }
            }
            true
        }
        KeyCode::Escape => {
            *timeline_dialog = None;
            chat.app.set_status("timeline closed".to_owned());
            true
        }
        _ => false,
    }
}
