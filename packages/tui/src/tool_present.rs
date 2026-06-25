//! Tool-call presentation models for transcript rendering.

use bcode_session_models::ToolRequestPresentationMetadata;
use serde_json::Value;

/// Human-readable presentation for a tool request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRequestPresentation {
    /// Human-readable title.
    pub title: String,
    /// Labeled detail fields.
    pub fields: Vec<(String, String)>,
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
            let rendered = render_metadata_value(argument);
            (!rendered.is_empty()).then(|| (field.label.clone(), rendered))
        })
        .collect::<Vec<_>>();
    Some(ToolRequestPresentation {
        title: metadata.title.clone(),
        fields,
    })
}

fn render_metadata_value(value: &Value) -> String {
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
