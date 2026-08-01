//! Isolated terminal-native plugin visual projection.
//!
//! This module converts shared plugin-owned artifact contracts into the TUI's
//! local visual-adapter input. It does not define shared tool semantics.

use bcode_session_models::ToolArtifact;
use serde_json::Value;

/// Terminal-local plugin visual for a tool surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalToolVisual {
    /// Plugin-owned visual routed by schema and optional producer preference.
    Plugin(CanonicalPluginVisual),
}

/// Canonical plugin visual routed through the plugin visual-adapter registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalPluginVisual {
    /// Stable invocation identity used for adapter state and cache isolation.
    pub invocation_id: Option<String>,
    /// Semantic/presentation revision used for cache invalidation.
    pub revision: u64,
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
    /// Build a plugin visual from one complete canonical tool request.
    #[must_use]
    pub fn from_request(
        invocation_id: &str,
        producer_plugin_id: Option<&str>,
        tool_name: &str,
        schema: String,
        schema_version: u32,
        arguments_json: &str,
    ) -> Option<Self> {
        let mut payload: serde_json::Value = serde_json::from_str(arguments_json).ok()?;
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "_bcode_invocation".to_owned(),
                serde_json::json!({"tool_name": tool_name}),
            );
        }
        Some(Self::Plugin(CanonicalPluginVisual {
            invocation_id: Some(invocation_id.to_owned()),
            revision: 0,
            producer_plugin_id: producer_plugin_id.map(ToOwned::to_owned),
            schema,
            schema_version,
            title: None,
            subtitle: None,
            payload,
            streaming: false,
        }))
    }

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
            invocation_id: artifact.tool_call_id.clone(),
            revision: artifact
                .metadata
                .get("_bcode_presentation_revision")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
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
