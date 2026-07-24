//! Isolated canonical tool-render projection for the TUI.
//!
//! This module is the temporary compatibility boundary between raw/legacy tool
//! event shapes and the renderer. Render code should consume these canonical
//! visuals instead of interpreting legacy request/live/result details directly.

use bcode_session_models::ToolArtifact;
use serde_json::Value;

/// Canonical renderer-neutral visual for a tool surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalToolVisual {
    /// Plugin-owned visual routed by schema and optional producer preference.
    Plugin(CanonicalPluginVisual),
}

/// Canonical plugin visual routed through the plugin visual-adapter registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalPluginVisual {
    /// Preferred producer plugin id. This is a routing preference, not a hard lookup key.
    pub producer_plugin_id: Option<String>,
    /// Plugin-owned schema id.
    pub schema: String,
    /// Plugin-owned schema version.
    pub schema_version: u32,
    /// Optional display title.
    pub title: Option<String>,
    /// Optional display subtitle.
    pub subtitle: Option<String>,
    /// Plugin-owned payload.
    pub payload: Value,
    /// Whether this visual is from live partial arguments.
    pub streaming: bool,
}

impl CanonicalToolVisual {
    /// Build a canonical plugin visual from a final semantic artifact.
    #[must_use]
    pub fn from_artifact(artifact: &ToolArtifact) -> Self {
        let mut payload = if artifact.metadata.is_object() {
            artifact.metadata.clone()
        } else {
            serde_json::json!({})
        };
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "_bcode_artifact".to_owned(),
                serde_json::json!({
                    "artifact_id": artifact.artifact_id,
                    "tool_call_id": artifact.tool_call_id,
                }),
            );
            if let Some(tool_call_id) = &artifact.tool_call_id {
                object.insert(
                    "_bcode_runtime".to_owned(),
                    serde_json::json!({"live_state_key": tool_call_id}),
                );
            }
        }
        if let Some(title) = &artifact.title
            && let Some(object) = payload.as_object_mut()
        {
            object
                .entry("title".to_owned())
                .or_insert_with(|| Value::String(title.clone()));
        }
        if let Some(summary) = artifact.metadata.get("summary").and_then(Value::as_str)
            && let Some(object) = payload.as_object_mut()
        {
            object
                .entry("subtitle".to_owned())
                .or_insert_with(|| Value::String(summary.to_owned()));
        }
        if !artifact.refs.is_empty()
            && let Some(object) = payload.as_object_mut()
            && let Ok(refs) = serde_json::to_value(&artifact.refs)
        {
            object.insert("_artifact_refs".to_owned(), refs);
        }
        Self::Plugin(CanonicalPluginVisual {
            producer_plugin_id: Some(artifact.producer_plugin_id.clone()),
            schema: artifact.schema.clone(),
            schema_version: artifact.schema_version,
            title: artifact.title.clone(),
            subtitle: artifact
                .metadata
                .get("summary")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            payload,
            streaming: false,
        })
    }
}
