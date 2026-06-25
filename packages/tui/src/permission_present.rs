//! Permission dialog presentation models.

use bcode_session_models::ToolRequestPresentationMetadata;
use serde_json::Value;

use super::tool_present::tool_request_presentation;
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
pub fn permission_presentation(
    tool_name: &str,
    arguments_json: &str,
    request_presentation: Option<&ToolRequestPresentationMetadata>,
) -> PermissionPresentation {
    if let Some(presentation) = tool_request_presentation(arguments_json, request_presentation) {
        return PermissionPresentation {
            title: presentation.title,
            risk: "tool request".to_owned(),
            details: presentation
                .fields
                .into_iter()
                .map(|(label, value)| PermissionDetail::new(label, value))
                .collect(),
            raw_details: None,
        };
    }

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
        .map(|(label, value)| PermissionDetail::new(label, display_json_value(&value)))
        .collect()
}

fn display_json_value(value: &Value) -> String {
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

#[cfg(test)]
mod tests {
    use bcode_session_models::{
        ToolPresentationField, ToolPresentationFieldKind, ToolRequestPresentationMetadata,
    };

    use super::permission_presentation;

    #[test]
    fn metadata_permission_uses_declared_fields() {
        let metadata = ToolRequestPresentationMetadata {
            title: "Run command".to_string(),
            fields: vec![ToolPresentationField {
                label: "command".to_string(),
                argument: "command".to_string(),
                kind: ToolPresentationFieldKind::Command,
                optional: false,
            }],
        };
        let presentation = permission_presentation(
            "shell.run",
            r#"{"command":"cargo check --workspace","cwd":"/repo"}"#,
            Some(&metadata),
        );

        assert_eq!(presentation.title, "Run command");
        assert_eq!(presentation.risk, "tool request");
        assert_eq!(presentation.details[0].label, "command");
        assert_eq!(presentation.details[0].value, "cargo check --workspace");
    }

    #[test]
    fn generic_json_string_values_are_unescaped() {
        let presentation =
            permission_presentation("custom.tool", r#"{"text":"hello\nworld"}"#, None);

        assert_eq!(presentation.details[0].value, "hello\nworld");
    }
}
