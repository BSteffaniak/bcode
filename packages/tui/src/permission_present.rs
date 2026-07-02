//! Permission dialog presentation models.

use serde_json::Value;

use super::transcript::pretty_jsonish;

/// One labeled permission-detail row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDetail {
    /// Field label.
    pub label: String,
    /// Field value.
    pub value: String,
}

impl PermissionDetail {
    /// Create a permission detail row.
    #[must_use]
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// Structured presentation for a pending permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPresentation {
    /// Human-readable title.
    pub title: String,
    /// Tool side-effect/risk label.
    pub risk: String,
    /// Primary details to render in the dialog body.
    pub details: Vec<PermissionDetail>,
    /// Optional fallback raw detail block.
    pub raw_details: Option<String>,
}

/// Build a structured permission presentation from a tool name and arguments.
#[must_use]
pub fn permission_presentation(tool_name: &str, arguments_json: &str) -> PermissionPresentation {
    PermissionPresentation {
        title: tool_name.to_owned(),
        risk: "tool request".to_owned(),
        details: generic_json_details(arguments_json),
        raw_details: Some(pretty_jsonish(arguments_json)),
    }
}

fn generic_json_details(arguments_json: &str) -> Vec<PermissionDetail> {
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(arguments_json) else {
        return Vec::new();
    };
    object
        .into_iter()
        .map(|(label, value)| {
            PermissionDetail::new(label.clone(), display_json_value(&label, &value))
        })
        .collect()
}

fn display_json_value(label: &str, value: &Value) -> String {
    if is_duration_or_timeout_label(label)
        && let Some(ms) = duration_millis(value)
    {
        return crate::time_format::format_millis(ms);
    }
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => value.clone(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string_pretty(value).unwrap_or_default()
        }
    }
}

fn duration_millis(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(value) => value.trim().parse::<u64>().ok(),
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => None,
    }
}

fn is_duration_or_timeout_label(label: &str) -> bool {
    let label = label.to_ascii_lowercase();
    label.contains("duration") || label.contains("timeout") || label.ends_with("_ms")
}

#[cfg(test)]
mod tests {
    use super::permission_presentation;

    #[test]
    fn generic_json_string_values_are_unescaped() {
        let presentation = permission_presentation("custom.tool", r#"{"text":"hello\nworld"}"#);

        assert_eq!(presentation.details[0].value, "hello\nworld");
    }
}
