//! Thinking settings dialog input flow for the TUI.

use bcode_client::BcodeClient;
use bmux_keyboard::{KeyCode, KeyStroke};

use super::TuiError;
use super::session_flow::ActiveChat;
use super::thinking_dialog::ThinkingDialogState;

/// Handle one thinking-dialog key.
pub async fn handle_thinking_key(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    thinking_dialog: &mut Option<ThinkingDialogState>,
    stroke: KeyStroke,
) -> Result<bool, TuiError> {
    let Some(dialog) = thinking_dialog else {
        return Ok(false);
    };
    match stroke.key {
        KeyCode::Up => {
            dialog.focus_previous();
            chat.app.set_status("thinking settings".to_owned());
            Ok(true)
        }
        KeyCode::Down => {
            dialog.focus_next();
            chat.app.set_status("thinking settings".to_owned());
            Ok(true)
        }
        KeyCode::Char(' ') => {
            dialog.cycle_focused();
            chat.app.set_status("thinking setting changed".to_owned());
            Ok(true)
        }
        KeyCode::Enter => apply_thinking_dialog(client, chat, thinking_dialog).await,
        KeyCode::Escape => {
            *thinking_dialog = None;
            chat.app.set_status("thinking settings canceled".to_owned());
            Ok(true)
        }
        _ => Ok(false),
    }
}

async fn apply_thinking_dialog(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    thinking_dialog: &mut Option<ThinkingDialogState>,
) -> Result<bool, TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        *thinking_dialog = None;
        return Ok(true);
    };
    let Some(dialog) = thinking_dialog.take() else {
        return Ok(false);
    };
    client
        .set_session_reasoning(
            session_id,
            dialog.effort().map(ToOwned::to_owned),
            dialog.summary().map(ToOwned::to_owned),
        )
        .await?;
    chat.app.set_reasoning_visible(dialog.visible());
    if let Ok(status) = client.session_model_status(session_id).await {
        chat.app.apply_model_status(status);
    }
    chat.app.set_status(format!(
        "thinking settings applied: {}",
        chat.app.thinking_label()
    ));
    Ok(true)
}
