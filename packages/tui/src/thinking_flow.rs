//! Thinking settings dialog input flow for the TUI.

use bcode_client::BcodeClient;
use bcode_ipc::SessionModelStatus;
use bmux_keyboard::{KeyCode, KeyStroke};

use super::TuiError;
use super::session_flow::ActiveChat;
use super::thinking_dialog::ThinkingDialogState;

/// Cycle the selected thinking effort for the current model.
pub async fn cycle_thinking_effort(
    client: &BcodeClient,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let session_id = chat.app.session_id();
    let status = if let Some(session_id) = session_id {
        client.session_model_status(session_id).await?
    } else {
        client.default_model_status().await?
    };
    let Some(next_effort) = next_effort_for_status(&status, chat.app.reasoning_effort()) else {
        chat.app
            .set_status("thinking effort unavailable for current model".to_owned());
        return Ok(());
    };
    let summary = chat
        .app
        .reasoning_summary()
        .map(ToOwned::to_owned)
        .or(status.reasoning_summary);
    chat.app.apply_reasoning_selection(
        Some(next_effort.clone()),
        summary.clone(),
        chat.app.reasoning_visible(),
    );
    if let Some(session_id) = session_id {
        client
            .set_session_reasoning(session_id, Some(next_effort.clone()), summary)
            .await?;
    }
    chat.app
        .set_status(format!("thinking effort set to {next_effort}"));
    Ok(())
}

pub fn next_effort_for_status(
    status: &SessionModelStatus,
    app_effort: Option<&str>,
) -> Option<String> {
    let reasoning = status.reasoning.as_ref()?;
    let current = app_effort
        .or(status.reasoning_effort.as_deref())
        .or(reasoning.default_effort.as_deref());
    next_effort_value(&reasoning.effort_values, current)
}

fn next_effort_value(values: &[String], current: Option<&str>) -> Option<String> {
    if values.is_empty() {
        return None;
    }
    let current_index =
        current.and_then(|current| values.iter().position(|value| value == current));
    let next_index = current_index.map_or(0, |index| (index + 1) % values.len());
    values.get(next_index).cloned()
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
    let Some(dialog) = thinking_dialog.take() else {
        return Ok(false);
    };
    let effort = dialog.effort().map(ToOwned::to_owned);
    let summary = dialog.summary().map(ToOwned::to_owned);
    let visible = dialog.visible();
    let Some(session_id) = chat.app.session_id() else {
        chat.app.apply_reasoning_selection(effort, summary, visible);
        chat.app.set_status(format!(
            "thinking settings applied: {}",
            chat.app.thinking_label()
        ));
        return Ok(true);
    };
    client
        .set_session_reasoning(session_id, effort, summary)
        .await?;
    chat.app.set_reasoning_visible(visible);
    if let Ok(status) = client.session_model_status(session_id).await {
        chat.app.apply_model_status(status);
    }
    chat.app.set_status(format!(
        "thinking settings applied: {}",
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
