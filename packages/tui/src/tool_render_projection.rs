//! Isolated canonical tool-render projection for the TUI.
//!
//! This module is the temporary compatibility boundary between raw/legacy tool
//! event shapes and the renderer. Render code should consume these canonical
//! visuals instead of interpreting legacy request/live/result details directly.

use bcode_session_models::{LiveToolArgumentPreview, ToolArtifact, ToolInvocationResult};
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

/// Return whether a final semantic result supersedes an in-flight live preview.
#[must_use]
pub fn semantic_result_supersedes_live_preview(
    tool_call_id: &str,
    preview: &LiveToolArgumentPreview,
    semantic_result: Option<&ToolInvocationResult>,
) -> bool {
    let Some(ToolInvocationResult::Artifact { artifact }) = semantic_result else {
        return false;
    };
    if artifact.tool_call_id.as_deref() != Some(tool_call_id) {
        return false;
    }
    let identity = live_preview_artifact_identity(preview);
    identity.producer_plugin_id == artifact.producer_plugin_id
        && identity.schema == artifact.schema
        && identity.schema_version == artifact.schema_version
}

struct LivePreviewArtifactIdentity<'a> {
    producer_plugin_id: &'a str,
    schema: &'a str,
    schema_version: u32,
}

fn live_preview_artifact_identity(
    preview: &LiveToolArgumentPreview,
) -> LivePreviewArtifactIdentity<'_> {
    LivePreviewArtifactIdentity {
        producer_plugin_id: preview
            .visual
            .producer_plugin_id
            .as_deref()
            .unwrap_or_default(),
        schema: &preview.visual.schema,
        schema_version: preview.visual.schema_version,
    }
}

impl CanonicalToolVisual {
    /// Build a canonical plugin visual from a final semantic artifact.
    #[must_use]
    pub fn from_artifact(artifact: &ToolArtifact) -> Self {
        let mut payload = artifact.metadata.clone();
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

    pub fn from_plugin_descriptor(
        descriptor: &bcode_session_models::PluginVisualDescriptor,
        streaming: bool,
    ) -> Self {
        Self::Plugin(CanonicalPluginVisual {
            producer_plugin_id: descriptor.producer_plugin_id.clone(),
            schema: descriptor.schema.clone(),
            schema_version: descriptor.schema_version,
            title: descriptor.title.clone(),
            subtitle: descriptor.subtitle.clone(),
            payload: descriptor.payload.clone(),
            streaming,
        })
    }

    /// Build a canonical live visual from a live argument visual.
    #[must_use]
    pub fn from_live_preview(_tool_name: &str, preview: &LiveToolArgumentPreview) -> Self {
        let mut descriptor = preview.visual.clone();
        if let Value::Object(payload) = &mut descriptor.payload {
            payload.insert(
                "argument_bytes".to_owned(),
                serde_json::json!(preview.argument_bytes),
            );
        }
        Self::from_plugin_descriptor(&descriptor, true)
    }
}
