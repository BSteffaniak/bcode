//! Renderer-neutral bounded presentation of structured output.

use std::borrow::Cow;

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
