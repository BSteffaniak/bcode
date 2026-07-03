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
    /// Plain text fallback for tools that do not have a semantic visual.
    PlainText {
        /// Display title.
        title: String,
        /// Text body.
        text: String,
    },
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
    let LiveToolArgumentPreview::PluginView(view) = preview else {
        return false;
    };
    view.producer_plugin_id == artifact.producer_plugin_id
        && view.schema == artifact.schema
        && view.schema_version == artifact.schema_version
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

    /// Build a canonical live visual from a live argument preview.
    #[must_use]
    pub fn from_live_preview(tool_name: &str, preview: &LiveToolArgumentPreview) -> Self {
        match preview {
            LiveToolArgumentPreview::PluginView(view) => Self::Plugin(CanonicalPluginVisual {
                producer_plugin_id: non_empty_string(&view.producer_plugin_id),
                schema: view.schema.clone(),
                schema_version: view.schema_version,
                title: view.title.clone(),
                subtitle: view.subtitle.clone(),
                payload: view.payload.clone(),
                streaming: true,
            }),
            LiveToolArgumentPreview::FileEdit(file) => {
                let mut payload = serde_json::Map::new();
                let preview_title = file
                    .preview_title
                    .clone()
                    .unwrap_or_else(|| "File change preview".to_owned());
                payload.insert("title".to_owned(), Value::String(preview_title.clone()));
                payload.insert("summary".to_owned(), Value::String(preview_title.clone()));
                if let Some(path) = &file.path {
                    payload.insert("path".to_owned(), Value::String(path.clone()));
                }
                payload.insert(
                    "old_text".to_owned(),
                    Value::String(file.old_text_prefix.clone().unwrap_or_default()),
                );
                payload.insert(
                    "new_text".to_owned(),
                    Value::String(file.new_text_prefix.clone()),
                );
                payload.insert("truncated".to_owned(), Value::Bool(file.truncated));
                payload.insert(
                    "argument_bytes".to_owned(),
                    Value::Number(serde_json::Number::from(file.argument_bytes as u64)),
                );
                if file.old_text_required && file.old_text_prefix.is_none() {
                    payload.insert(
                        "subtitle".to_owned(),
                        Value::String(
                            "original text pending; showing available new text".to_owned(),
                        ),
                    );
                }
                Self::Plugin(CanonicalPluginVisual {
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.change".to_owned(),
                    schema_version: 1,
                    title: Some(preview_title),
                    subtitle: file.streaming_status.clone(),
                    payload: Value::Object(payload),
                    streaming: true,
                })
            }
            LiveToolArgumentPreview::ShellCommand(shell) => Self::PlainText {
                title: shell
                    .preview_title
                    .clone()
                    .unwrap_or_else(|| "Shell command".to_owned()),
                text: shell_preview_text(shell),
            },
            LiveToolArgumentPreview::Query(query) => Self::PlainText {
                title: query
                    .preview_title
                    .clone()
                    .unwrap_or_else(|| tool_name.to_owned()),
                text: query_preview_text(query),
            },
        }
    }

    /// Build a canonical plugin visual from filesystem request arguments.
    #[must_use]
    pub fn from_filesystem_request(
        producer_plugin_id: Option<&str>,
        tool_name: &str,
        arguments_json: &str,
    ) -> Option<Self> {
        if producer_plugin_id != Some("bcode.filesystem") {
            return None;
        }
        let arguments = serde_json::from_str::<Value>(arguments_json).ok()?;
        let path = arguments.get("path")?.as_str()?;
        let payload = match tool_name {
            "filesystem.write" => serde_json::json!({
                "summary": "Write preview",
                "path": path,
                "old_text": "",
                "new_text": arguments.get("contents")?.as_str()?,
            }),
            "filesystem.edit" => serde_json::json!({
                "summary": "Edit preview",
                "path": path,
                "old_text": arguments.get("old_text")?.as_str()?,
                "new_text": arguments.get("new_text")?.as_str()?,
            }),
            _ => return None,
        };
        Some(Self::Plugin(CanonicalPluginVisual {
            producer_plugin_id: producer_plugin_id.map(ToOwned::to_owned),
            schema: "bcode.filesystem.change".to_owned(),
            schema_version: 1,
            title: Some("File change preview".to_owned()),
            subtitle: None,
            payload,
            streaming: false,
        }))
    }
}

fn non_empty_string(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn shell_preview_text(shell: &bcode_session_models::LiveShellCommandPreview) -> String {
    let mut lines = Vec::new();
    lines.push(format!("command: {}", shell.command_prefix));
    if let Some(cwd) = &shell.cwd {
        lines.push(format!("cwd: {cwd}"));
    }
    if shell.truncated {
        lines.push("preview truncated by live display limit".to_owned());
    }
    lines.join("\n")
}

fn query_preview_text(query: &bcode_session_models::LiveQueryPreview) -> String {
    let mut lines = query
        .fields
        .iter()
        .map(|(field, value)| format!("{field}: {value}"))
        .collect::<Vec<_>>();
    if query.truncated {
        lines.push("preview truncated by live display limit".to_owned());
    }
    lines.join("\n")
}
