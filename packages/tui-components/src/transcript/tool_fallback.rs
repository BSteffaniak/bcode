//! Readable tool fallback facts reused by both BMUX consumers.
use bcode_session_view_models::{ToolInvocationView, ToolInvocationViewStatus, ToolResultView};

/// Human-readable lifecycle label, independent of adapter schema or payload.
#[must_use]
pub const fn tool_status_label(status: ToolInvocationViewStatus) -> &'static str {
    match status {
        ToolInvocationViewStatus::Requested => "requested",
        ToolInvocationViewStatus::Running => "running",
        ToolInvocationViewStatus::Waiting => "waiting",
        ToolInvocationViewStatus::Finished => "finished",
        ToolInvocationViewStatus::Failed => "failed",
        ToolInvocationViewStatus::Cancelled => "cancelled",
    }
}

/// Small readable argument fields. Opaque presentation payloads are never inspected.
#[must_use]
pub fn bounded_tool_argument_fields(arguments: &str) -> Vec<(String, String)> {
    const MAX_FIELDS: usize = 6;
    const MAX_VALUE_CHARS: usize = 160;
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_str(arguments) else {
        return Vec::new();
    };
    fields
        .into_iter()
        .take(MAX_FIELDS)
        .filter_map(|(label, value)| {
            let value = match value {
                serde_json::Value::String(value) => value,
                serde_json::Value::Number(value) => value.to_string(),
                serde_json::Value::Bool(value) => value.to_string(),
                serde_json::Value::Null => "none".to_owned(),
                serde_json::Value::Array(values) => format!("{} values", values.len()),
                serde_json::Value::Object(values) => format!("{} fields", values.len()),
            };
            let mut bounded = value.chars().take(MAX_VALUE_CHARS).collect::<String>();
            if value.chars().count() > MAX_VALUE_CHARS {
                bounded.push('…');
            }
            (!bounded.is_empty()).then_some((label, bounded))
        })
        .collect()
}

/// Readable fallback for an unknown/disabled visual adapter, using canonical
/// invocation facts only, never its opaque presentation metadata.
#[must_use]
pub fn invocation_text(invocation: &ToolInvocationView) -> String {
    let mut lines = vec![format!(
        "{} · {}{}",
        invocation.tool_name.as_deref().unwrap_or("Tool"),
        tool_status_label(invocation.status),
        if invocation.timing.timed_out == Some(true) {
            " · timed out"
        } else {
            ""
        }
    )];
    if invocation.status != ToolInvocationViewStatus::Requested
        && let Some(arguments) = &invocation.arguments_json
    {
        lines.extend(
            bounded_tool_argument_fields(arguments)
                .into_iter()
                .map(|(label, value)| format!("{label}: {value}")),
        );
    }
    match &invocation.result {
        Some(ToolResultView::Text { text }) => lines.push(text.clone()),
        Some(ToolResultView::Json { value }) => lines.push(value.clone()),
        Some(ToolResultView::Artifact { artifact }) => lines.push(
            artifact
                .artifact
                .title
                .clone()
                .unwrap_or_else(|| "artifact result".into()),
        ),
        None => {}
    }
    if let Some(text) = &invocation.result_text
        && !lines.iter().any(|line| line == text)
    {
        lines.push(text.clone());
    }
    lines.join("\n")
}
