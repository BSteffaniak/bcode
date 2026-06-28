//! Tool-call presentation models for transcript rendering.

use bcode_session_models::{ToolPresentationFieldKind, ToolRequestPresentationMetadata};
use serde_json::Value;

use crate::time_format::format_millis;

/// Human-readable presentation for a tool request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRequestPresentation {
    /// Human-readable title.
    pub title: String,
    /// Labeled detail fields.
    pub fields: Vec<ToolRequestPresentationField>,
}

/// Human-readable presentation field for a tool request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRequestPresentationField {
    /// Human-readable field label.
    pub label: String,
    /// Human-readable field value.
    pub value: String,
    /// Semantic field kind used for generic formatting.
    pub kind: ToolPresentationFieldKind,
}

/// Human-readable presentation for shell results that were already parsed into semantic data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellResultPresentation {
    /// Terminal-backed shell output.
    Terminal {
        /// Process exit code.
        exit_code: Option<i32>,
        /// Whether execution timed out.
        timed_out: bool,
        /// Terminal output tail.
        output: String,
        /// Whether terminal output was truncated.
        output_truncated: bool,
        /// Original terminal output byte count.
        output_bytes: Option<u64>,
        /// Retained terminal output byte count.
        retained_output_bytes: Option<u64>,
        /// Terminal columns used by the producer.
        columns: u16,
        /// Terminal rows used by the producer.
        rows: u16,
    },
}

/// Build a metadata-driven request presentation from raw tool arguments.
#[must_use]
pub fn tool_request_presentation(
    arguments_json: &str,
    metadata: Option<&ToolRequestPresentationMetadata>,
) -> Option<ToolRequestPresentation> {
    metadata.and_then(|metadata| metadata_request_presentation(arguments_json, metadata))
}

fn metadata_request_presentation(
    arguments_json: &str,
    metadata: &ToolRequestPresentationMetadata,
) -> Option<ToolRequestPresentation> {
    let value = serde_json::from_str::<Value>(arguments_json).ok()?;
    let fields = metadata
        .fields
        .iter()
        .filter_map(|field| {
            let argument = value.get(&field.argument)?;
            let rendered = render_metadata_value(argument, field.kind);
            (!rendered.is_empty()).then(|| ToolRequestPresentationField {
                label: field.label.clone(),
                value: rendered,
                kind: field.kind,
            })
        })
        .collect::<Vec<_>>();
    Some(ToolRequestPresentation {
        title: metadata.title.clone(),
        fields,
    })
}

fn render_metadata_value(value: &Value, kind: ToolPresentationFieldKind) -> String {
    if kind == ToolPresentationFieldKind::DurationMs
        && let Some(ms) = duration_millis(value)
    {
        return format_millis(ms);
    }
    match value {
        Value::Null => String::new(),
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
