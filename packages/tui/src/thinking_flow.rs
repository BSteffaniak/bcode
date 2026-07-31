//! Thinking settings dialog input flow for the TUI.

use bcode_client::BcodeClient;
use bcode_session_view::execute_session_view_action;
use bcode_session_view_models::SessionViewAction;
use bmux_keyboard::{KeyCode, KeyStroke};

use super::TuiError;
use super::session_flow::ActiveChat;
use super::thinking_dialog::ThinkingDialogState;

/// Cycle the selected reasoning effort locally for the next message.
pub fn cycle_thinking_effort(chat: &mut ActiveChat) {
    let Some(next_effort) = chat
        .app
        .cycle_pending_reasoning_effort()
        .map(ToOwned::to_owned)
    else {
        chat.app
            .set_status("reasoning effort unavailable for current model".to_owned());
        return;
    };
    chat.app.set_status(format!(
        "reasoning effort {next_effort} selected for next message"
    ));
}

#[cfg(test)]
fn next_effort_value(values: &[String], current: Option<&str>) -> Option<String> {
    bcode_model::next_reasoning_effort_value(values, current)
}

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
            chat.app.set_status("reasoning output settings".to_owned());
            Ok(true)
        }
        KeyCode::Down => {
            dialog.focus_next();
            chat.app.set_status("reasoning output settings".to_owned());
            Ok(true)
        }
        KeyCode::Char(' ') => {
            dialog.cycle_focused();
            chat.app
                .set_status("reasoning output setting changed".to_owned());
            Ok(true)
        }
        KeyCode::Enter => apply_thinking_dialog(client, chat, thinking_dialog).await,
        KeyCode::Escape => {
            *thinking_dialog = None;
            chat.app
                .set_status("reasoning output settings canceled".to_owned());
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
    let Some(dialog) = thinking_dialog.take() else {
        return Ok(false);
    };
    let effort = dialog.effort().map(ToOwned::to_owned);
    let summary = dialog.summary().map(ToOwned::to_owned);
    let visible = dialog.visible();
    let mode = dialog.mode();
    let Some(session_id) = chat.app.session_id() else {
        chat.app.apply_reasoning_selection(effort, summary);
        chat.app.set_reasoning_visible(visible);
        chat.app.set_reasoning_display_mode(mode);
        chat.app.set_status(format!(
            "reasoning output settings applied: {}",
            chat.app.thinking_label()
        ));
        return Ok(true);
    };
    execute_session_view_action(
        client,
        SessionViewAction::SetReasoning {
            session_id,
            effort,
            summary,
        },
    )
    .await?;
    chat.app.set_reasoning_visible(visible);
    chat.app.set_reasoning_display_mode(mode);
    if let Ok(status) = client.session_model_status(session_id).await {
        chat.app.apply_model_status(status);
    }
    let mode = match chat.app.reasoning_display_mode() {
        bcode_config::TuiThinkingMode::All => "all",
        bcode_config::TuiThinkingMode::Summary => "summary",
        bcode_config::TuiThinkingMode::Raw => "raw",
    };
    chat.app.set_status(format!(
        "reasoning output settings applied: {} · displayed reasoning: {mode}",
        chat.app.thinking_label()
    ));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::next_effort_value;

    fn values() -> Vec<String> {
        ["none", "low", "medium"]
            .into_iter()
            .map(ToOwned::to_owned)
            .collect()
    }

    #[test]
    fn next_effort_value_advances_and_wraps() {
        let values = values();

        assert_eq!(
            next_effort_value(&values, Some("none")).as_deref(),
            Some("low")
        );
        assert_eq!(
            next_effort_value(&values, Some("low")).as_deref(),
            Some("medium")
        );
        assert_eq!(
            next_effort_value(&values, Some("medium")).as_deref(),
            Some("none")
        );
    }

    #[test]
    fn next_effort_value_uses_first_for_unknown_or_missing_current() {
        let values = values();

        assert_eq!(next_effort_value(&values, None).as_deref(), Some("none"));
        assert_eq!(
            next_effort_value(&values, Some("unsupported")).as_deref(),
            Some("none")
        );
    }

    #[test]
    fn next_effort_value_returns_none_for_empty_values() {
        assert_eq!(next_effort_value(&[], Some("medium")), None);
    }
}
