//! Structural tool presentation (plugin contract v3) for shell runs.
//!
//! Supplies the one-line summary and operation target the host shows for a
//! collapsed or summarized `shell.run` unit, for example `cargo test · exit 0`.
//! The host bounds both values; this module only chooses their content.

use serde_json::Value;

/// Request payload schema of `shell.run`.
const SHELL_RUN_REQUEST_SCHEMA: &str = "bcode.tool.request.shell.run";

/// Summary and target for a shell visual payload.
///
/// * requests: the command, with no outcome yet;
/// * results: the command and its outcome (`exit 0`, `timed out`, `cancelled`);
/// * live recordings: the command and `running`.
///
/// Draft previews (partial argument JSON) and streaming output chunks supply
/// nothing, so the host keeps its generic summary sources.
#[must_use]
pub fn shell_tool_presentation(
    kind: &str,
    payload: &Value,
) -> bcode_plugin_sdk::tui::PluginTuiToolPresentation {
    if payload.get("preview").is_some()
        || !matches!(
            kind,
            crate::contracts::SHELL_RUN_SCHEMA | SHELL_RUN_REQUEST_SCHEMA
        )
    {
        return bcode_plugin_sdk::tui::PluginTuiToolPresentation::default();
    }
    let arguments = payload.get("arguments").unwrap_or(payload);
    let command = arguments
        .get("command")
        .and_then(Value::as_str)
        .and_then(first_line);
    let outcome = (kind == crate::contracts::SHELL_RUN_SCHEMA)
        .then(|| outcome(payload))
        .flatten();
    let summary = match (&command, outcome) {
        (Some(command), Some(outcome)) => Some(format!("{command} · {outcome}")),
        (Some(command), None) => Some(command.clone()),
        (None, outcome) => outcome,
    };
    bcode_plugin_sdk::tui::PluginTuiToolPresentation {
        summary,
        target: command,
        sections: Vec::new(),
    }
}

/// First non-blank line of a command, trimmed.
fn first_line(command: &str) -> Option<String> {
    command
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Outcome phrase of a result payload; `None` when nothing is known yet.
fn outcome(payload: &Value) -> Option<String> {
    let flag = |key: &str| payload.get(key).and_then(Value::as_bool) == Some(true);
    if flag("timed_out") {
        return Some("timed out".to_owned());
    }
    if flag("cancelled") {
        return Some("cancelled".to_owned());
    }
    if let Some(code) = payload.get("exit_code").and_then(Value::as_i64) {
        return Some(format!("exit {code}"));
    }
    let live = payload.get("mode").and_then(Value::as_str) == Some("terminal")
        && payload.get("exit_code").is_none();
    live.then(|| "running".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_summarize_command_and_exit_code() {
        let presentation = shell_tool_presentation(
            crate::contracts::SHELL_RUN_SCHEMA,
            &serde_json::json!({
                "arguments": { "command": "\n  cargo test  \nsecond line" },
                "exit_code": 0,
                "timed_out": false,
                "cancelled": false,
            }),
        );
        assert_eq!(presentation.summary.as_deref(), Some("cargo test · exit 0"));
        assert_eq!(presentation.target.as_deref(), Some("cargo test"));
        assert_eq!(presentation.sections, Vec::new());
    }

    #[test]
    fn outcomes_prefer_timeouts_and_cancellation() {
        let summary = |payload: Value| {
            shell_tool_presentation(crate::contracts::SHELL_RUN_SCHEMA, &payload).summary
        };
        assert_eq!(
            summary(serde_json::json!({
                "arguments": { "command": "sleep 9" }, "exit_code": null, "timed_out": true
            }))
            .as_deref(),
            Some("sleep 9 · timed out")
        );
        assert_eq!(
            summary(serde_json::json!({
                "arguments": { "command": "make" }, "cancelled": true, "exit_code": 130
            }))
            .as_deref(),
            Some("make · cancelled")
        );
        assert_eq!(
            summary(serde_json::json!({
                "arguments": { "command": "top" }, "mode": "terminal"
            }))
            .as_deref(),
            Some("top · running")
        );
    }

    #[test]
    fn requests_name_the_command_and_drafts_supply_nothing() {
        let request = shell_tool_presentation(
            SHELL_RUN_REQUEST_SCHEMA,
            &serde_json::json!({ "command": "ls -la", "exit_code": 3 }),
        );
        assert_eq!(request.summary.as_deref(), Some("ls -la"));
        assert_eq!(request.target.as_deref(), Some("ls -la"));
        let draft = shell_tool_presentation(
            SHELL_RUN_REQUEST_SCHEMA,
            &serde_json::json!({ "preview": "{\"command\": \"ls" }),
        );
        assert!(draft.is_empty());
        let output = shell_tool_presentation(
            "bcode.shell.output",
            &serde_json::json!({ "command": "ls" }),
        );
        assert!(output.is_empty());
    }

    #[test]
    fn blank_commands_have_no_target() {
        let presentation = shell_tool_presentation(
            crate::contracts::SHELL_RUN_SCHEMA,
            &serde_json::json!({ "arguments": { "command": "  \n " }, "exit_code": 1 }),
        );
        assert_eq!(presentation.target, None);
        assert_eq!(presentation.summary.as_deref(), Some("exit 1"));
    }
}
