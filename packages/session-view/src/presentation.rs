//! Renderer-neutral presentation of structured model output and correlated activity.
use std::borrow::Cow;

use bcode_session_view_models::{SessionViewSnapshot, TranscriptViewItemKind};

/// Present structured model output as provisional readable fields, never as raw JSON.
/// Ordinary prose is unchanged. Parsing is bounded and cannot validate or authorize execution.
#[must_use]
pub fn model_output_text(text: &str) -> Cow<'_, str> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('{') && !trimmed.starts_with("```json") {
        return Cow::Borrowed(text);
    }
    let candidate = trimmed.strip_prefix("```json").unwrap_or(trimmed).trim();
    let candidate = candidate.strip_suffix("```").unwrap_or(candidate);
    if candidate.len() > 65_536 {
        return Cow::Borrowed("Structured output exceeds the preview limit.");
    }
    let Ok(serde_json::Value::Object(fields)) =
        serde_json::from_str::<serde_json::Value>(&partial_json_fixer::fix_json(candidate))
    else {
        return Cow::Borrowed("Receiving structured output…");
    };
    let mut sections = Vec::new();
    // These are readable field labels, not workflow decisions. Never publish the input envelope.
    for (key, label) in [
        ("clarification", "Clarification"),
        ("summary", "Summary"),
        ("implementation_prompt", "Implementation instructions"),
        ("stop_condition", "Stop condition"),
    ] {
        if let Some(value) = fields.get(key).and_then(serde_json::Value::as_str)
            && !value.trim().is_empty()
        {
            sections.push(format!("{label}\n{value}"));
        }
    }
    if let Some(evidence) = fields.get("evidence").and_then(serde_json::Value::as_array) {
        let evidence: Vec<_> = evidence
            .iter()
            .take(16)
            .filter_map(serde_json::Value::as_str)
            .map(|line| format!("• {line}"))
            .collect();
        if !evidence.is_empty() {
            sections.push(format!("Evidence\n{}", evidence.join("\n")));
        }
    }
    if sections.is_empty() {
        Cow::Borrowed("Receiving structured result…")
    } else {
        Cow::Owned(sections.join("\n\n"))
    }
}

/// Current activity label from existing plugin status and host-correlated presentation.
/// A status contribution is required: old transcript entries cannot resurrect terminal work.
#[must_use]
pub fn persistent_activity_text(snapshot: &SessionViewSnapshot) -> Option<String> {
    let status = snapshot
        .plugin_status
        .values()
        .find(|status| status.metadata.contains_key("run_id"))?;
    let run_id = status.metadata.get("run_id")?.as_str()?;
    if status
        .metadata
        .get("status")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| matches!(value, "Completed" | "Failed" | "Cancelled"))
    {
        return None;
    }
    let activity = snapshot
        .transcript
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            TranscriptViewItemKind::UserMessage { message } => message.activity.as_ref(),
            _ => None,
        })
        .filter(|activity| activity.execution.execution_id == run_id)
        .max_by_key(|activity| activity.source_sequence);
    Some(activity.map_or_else(
        || status.text.clone(),
        |activity| {
            let title = activity
                .presentation
                .fallback
                .lines()
                .next()
                .unwrap_or(&status.text);
            format!(
                "{title} · {}",
                status
                    .metadata
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("active")
            )
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_fields_are_readable_without_json_or_execution_claims() {
        let text = model_output_text(
            r#"{"implementation_prompt":"Inspect 界\nthen test","stop_condition":"Tests pa"#,
        );
        assert!(text.contains("Implementation instructions\nInspect 界\nthen test"));
        assert!(text.contains("Stop condition\nTests pa"));
        assert!(!text.contains("implementation_prompt"));
    }
    #[test]
    fn terminal_status_cannot_be_reopened_by_presentation() {
        let mut snapshot = SessionViewSnapshot::empty();
        let status = bcode_session_view_models::PluginStatusView {
            plugin_id: "test".into(),
            note_id: "active".into(),
            text: "Waiting for evaluator".into(),
            priority: 20,
            metadata: std::collections::BTreeMap::from([
                ("run_id".into(), serde_json::json!("run")),
                ("status".into(), serde_json::json!("Running")),
            ]),
        };
        snapshot.plugin_status.insert("test".into(), status);
        assert_eq!(
            persistent_activity_text(&snapshot).as_deref(),
            Some("Waiting for evaluator")
        );
        snapshot
            .plugin_status
            .get_mut("test")
            .unwrap()
            .metadata
            .insert("status".into(), serde_json::json!("Completed"));
        assert!(persistent_activity_text(&snapshot).is_none());
        snapshot.plugin_status.clear();
        assert!(persistent_activity_text(&snapshot).is_none());
    }

    #[test]
    fn malformed_and_unknown_objects_never_fall_back_to_json() {
        for text in ["{", "{broken", r#"{"secret_envelope":true}"#] {
            assert!(!model_output_text(text).starts_with('{'));
        }
        assert_eq!(model_output_text("Ordinary prose"), "Ordinary prose");
        assert!(persistent_activity_text(&SessionViewSnapshot::empty()).is_none());
    }
}
