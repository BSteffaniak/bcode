//! Thinking settings dialog input flow for the TUI.

use super::session_flow::ActiveChat;

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
