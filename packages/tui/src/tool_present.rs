//! Tool-call presentation models for transcript rendering.

use std::collections::BTreeMap;

use bcode_session_models::{
    ToolPluginViewPresentation, ToolPresentationFieldKind, ToolPresentationTarget,
    ToolRequestPresentationMetadata, ToolRequestPreviewMetadata,
};
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

/// Build a metadata-driven request presentation from raw tool arguments.
#[must_use]
pub fn tool_request_presentation(
    arguments_json: &str,
    metadata: Option<&ToolRequestPresentationMetadata>,
) -> Option<ToolRequestPresentation> {
    metadata.and_then(|metadata| metadata_request_presentation(arguments_json, metadata))
}

/// Build a plugin-owned request preview from raw tool arguments.
#[must_use]
pub fn tool_request_plugin_view_preview(
    arguments_json: &str,
    metadata: Option<&ToolRequestPresentationMetadata>,
) -> Option<ToolPluginViewPresentation> {
    let metadata = metadata?;
    let ToolRequestPreviewMetadata::PluginView { view } = metadata.preview.as_ref()? else {
        return None;
    };
    let arguments = serde_json::from_str::<Value>(arguments_json).ok()?;
    let payload = plugin_view_payload_from_arguments(&arguments, &view.payload)?;
    Some(ToolPluginViewPresentation {
        target: ToolPresentationTarget::Preview,
        producer_plugin_id: view.producer_plugin_id.clone()?,
        schema: view.schema.clone(),
        schema_version: view.schema_version,
        title: view.title.clone(),
        subtitle: view.subtitle.clone(),
        payload,
    })
}

fn plugin_view_payload_from_arguments(
    arguments: &Value,
    selectors: &BTreeMap<String, bcode_session_models::ToolPresentationPayloadSelector>,
) -> Option<Value> {
    let mut payload = serde_json::Map::new();
    for (key, selector) in selectors {
        if let Some(value) = resolve_payload_selector(arguments, selector) {
            payload.insert(key.clone(), value);
        } else if selector.required {
            return None;
        }
    }
    Some(Value::Object(payload))
}

fn resolve_payload_selector(
    arguments: &Value,
    selector: &bcode_session_models::ToolPresentationPayloadSelector,
) -> Option<Value> {
    selector
        .fields
        .iter()
        .find_map(|field| arguments.get(field).cloned())
        .or_else(|| selector.literal.clone())
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bcode_session_models::{
        ToolPluginViewMetadata, ToolPresentationPayloadSelector, ToolRequestPresentationMetadata,
        ToolRequestPreviewMetadata,
    };
    use serde_json::json;

    use super::tool_request_plugin_view_preview;

    #[test]
    fn request_plugin_view_preview_resolves_payload_from_arguments() {
        let metadata = ToolRequestPresentationMetadata {
            title: "Edit".to_string(),
            fields: Vec::new(),
            preview: Some(ToolRequestPreviewMetadata::PluginView {
                view: ToolPluginViewMetadata {
                    schema: "bcode.example.change".to_string(),
                    schema_version: 1,
                    producer_plugin_id: Some("bcode.example".to_string()),
                    title: Some("Example change".to_string()),
                    subtitle: None,
                    payload: BTreeMap::from([
                        (
                            "path".to_string(),
                            ToolPresentationPayloadSelector {
                                fields: vec!["path".to_string()],
                                literal: None,
                                required: true,
                            },
                        ),
                        (
                            "old_text".to_string(),
                            ToolPresentationPayloadSelector {
                                fields: Vec::new(),
                                literal: Some(json!("")),
                                required: false,
                            },
                        ),
                        (
                            "new_text".to_string(),
                            ToolPresentationPayloadSelector {
                                fields: vec!["new_text".to_string(), "contents".to_string()],
                                literal: None,
                                required: true,
                            },
                        ),
                    ]),
                },
            }),
        };

        let preview = tool_request_plugin_view_preview(
            r#"{"path":"src/lib.rs","contents":"fn main() {}"}"#,
            Some(&metadata),
        )
        .expect("request plugin preview should resolve");

        assert_eq!(preview.producer_plugin_id, "bcode.example");
        assert_eq!(preview.schema, "bcode.example.change");
        assert_eq!(preview.payload["path"], json!("src/lib.rs"));
        assert_eq!(preview.payload["old_text"], json!(""));
        assert_eq!(preview.payload["new_text"], json!("fn main() {}"));
    }

    #[test]
    fn request_plugin_view_preview_requires_required_payload_fields() {
        let metadata = ToolRequestPresentationMetadata {
            title: "Edit".to_string(),
            fields: Vec::new(),
            preview: Some(ToolRequestPreviewMetadata::PluginView {
                view: ToolPluginViewMetadata {
                    schema: "bcode.example.change".to_string(),
                    schema_version: 1,
                    producer_plugin_id: Some("bcode.example".to_string()),
                    title: None,
                    subtitle: None,
                    payload: BTreeMap::from([(
                        "path".to_string(),
                        ToolPresentationPayloadSelector {
                            fields: vec!["path".to_string()],
                            literal: None,
                            required: true,
                        },
                    )]),
                },
            }),
        };

        assert!(tool_request_plugin_view_preview("{}", Some(&metadata)).is_none());
    }
}
