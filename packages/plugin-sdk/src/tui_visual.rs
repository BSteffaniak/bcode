//! Portable serialized contracts for dynamic TUI visual adapters.
//!
//! These models cross the native plugin service ABI and therefore must not depend on terminal
//! renderer implementation types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Versioned service interface for serialized TUI visual extensions.
pub const TUI_VISUAL_ADAPTER_INTERFACE_ID: &str = "bcode.tui-visual-adapter/v1";
/// Render one bounded visual through a serialized TUI extension.
pub const OP_RENDER_TUI_VISUAL: &str = "render";
/// Deliver one bounded artifact chunk through a serialized TUI extension.
pub const OP_DELIVER_TUI_VISUAL_ARTIFACT: &str = "artifact_chunk";
/// Earliest serialized TUI visual extension contract version accepted by this SDK.
pub const MIN_TUI_VISUAL_ADAPTER_CONTRACT_VERSION: u32 = 1;
/// Current serialized TUI visual extension contract version.
pub const TUI_VISUAL_ADAPTER_CONTRACT_VERSION: u32 = 2;
/// Maximum rows accepted from one serialized visual response.
pub const MAX_SERIALIZED_TUI_VISUAL_ROWS: usize = 256;
/// Maximum spans accepted across one serialized visual response.
pub const MAX_SERIALIZED_TUI_VISUAL_SPANS: usize = 2_048;
/// Maximum UTF-8 bytes accepted across one serialized visual response.
pub const MAX_SERIALIZED_TUI_VISUAL_TEXT_BYTES: usize = 256 * 1024;

/// Stable terminal color token exposed by the serialized extension contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerializedTuiColor {
    #[default]
    Reset,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
}

/// Stable text modifier token exposed by the serialized extension contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerializedTuiModifier {
    Bold,
    Dim,
    Italic,
    Underlined,
}

/// Stable renderer-neutral semantic style role exposed by contract version 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerializedTuiStyleRole {
    Text,
    Muted,
    Accent,
    Info,
    Success,
    Warning,
    Error,
    DiffAdded,
    DiffRemoved,
    DiffHunk,
}

/// One bounded styled text span returned by a serialized extension.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedTuiSpan {
    pub text: String,
    /// Optional semantic role. When present, renderer-owned role presentation
    /// takes precedence over the compatibility foreground color.
    #[serde(default)]
    pub role: Option<SerializedTuiStyleRole>,
    #[serde(default)]
    pub foreground: SerializedTuiColor,
    #[serde(default)]
    pub modifiers: Vec<SerializedTuiModifier>,
}

/// One bounded terminal row returned by a serialized extension.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedTuiRow {
    pub spans: Vec<SerializedTuiSpan>,
}

/// Renderer-owned context supplied to a serialized extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedTuiVisualContext {
    pub width: u16,
    pub diff_layout: String,
    pub working_directory: Option<PathBuf>,
    /// Stable renderer-owned presentation identity. Plugins may use this only
    /// to invalidate derived presentation, never for execution semantics.
    #[serde(default)]
    pub theme_fingerprint: u64,
}

/// Request to render one exact manifest adapter through a dynamic plugin service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderTuiVisualRequest {
    pub version: u32,
    pub adapter_id: String,
    pub invocation_id: String,
    pub schema: String,
    pub schema_version: u32,
    pub payload: serde_json::Value,
    pub context: SerializedTuiVisualContext,
}

/// Successful serialized extension response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderTuiVisualResponse {
    pub version: u32,
    #[serde(default)]
    pub render_mode: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    pub rows: Vec<SerializedTuiRow>,
}

impl RenderTuiVisualResponse {
    /// Validate response bounds before renderer conversion.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, invalid render modes, or excessive rows, spans,
    /// or text bytes.
    pub fn validate(&self) -> Result<(), String> {
        if !(MIN_TUI_VISUAL_ADAPTER_CONTRACT_VERSION..=TUI_VISUAL_ADAPTER_CONTRACT_VERSION)
            .contains(&self.version)
        {
            return Err(format!(
                "unsupported TUI visual response version {}",
                self.version
            ));
        }
        if self.version < 2
            && self
                .rows
                .iter()
                .flat_map(|row| &row.spans)
                .any(|span| span.role.is_some())
        {
            return Err("semantic TUI visual roles require response version 2".to_owned());
        }
        if !matches!(
            self.render_mode.as_str(),
            "" | "inline" | "transcript_block" | "full_block"
        ) {
            return Err("unsupported TUI visual render mode".to_owned());
        }
        if self.rows.len() > MAX_SERIALIZED_TUI_VISUAL_ROWS {
            return Err("serialized TUI visual row limit exceeded".to_owned());
        }
        let span_count = self.rows.iter().map(|row| row.spans.len()).sum::<usize>();
        let text_bytes = self
            .rows
            .iter()
            .flat_map(|row| &row.spans)
            .map(|span| span.text.len())
            .sum::<usize>();
        if span_count > MAX_SERIALIZED_TUI_VISUAL_SPANS
            || text_bytes > MAX_SERIALIZED_TUI_VISUAL_TEXT_BYTES
        {
            return Err("serialized TUI visual content limit exceeded".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_one_concrete_color_response_remains_compatible() {
        let response = RenderTuiVisualResponse {
            version: 1,
            render_mode: "inline".to_owned(),
            title: None,
            timeout_ms: None,
            rows: vec![SerializedTuiRow {
                spans: vec![SerializedTuiSpan {
                    text: "legacy".to_owned(),
                    role: None,
                    foreground: SerializedTuiColor::Green,
                    modifiers: vec![SerializedTuiModifier::Bold],
                }],
            }],
        };

        assert!(response.validate().is_ok());
        assert_eq!(
            response.rows[0].spans[0].foreground,
            SerializedTuiColor::Green
        );
    }

    #[test]
    fn semantic_roles_require_version_two_and_future_versions_fail_closed() {
        let semantic = RenderTuiVisualResponse {
            version: 1,
            render_mode: "inline".to_owned(),
            title: None,
            timeout_ms: None,
            rows: vec![SerializedTuiRow {
                spans: vec![SerializedTuiSpan {
                    text: "semantic".to_owned(),
                    role: Some(SerializedTuiStyleRole::Warning),
                    foreground: SerializedTuiColor::Red,
                    modifiers: Vec::new(),
                }],
            }],
        };
        assert!(semantic.validate().is_err());

        let mut future = semantic;
        future.version = TUI_VISUAL_ADAPTER_CONTRACT_VERSION + 1;
        assert!(future.validate().is_err());
    }
}

/// Artifact update delivered to one exact serialized adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedTuiArtifactChunkRequest {
    pub version: u32,
    pub adapter_id: String,
    pub chunk: SerializedTuiArtifactChunk,
}

/// Portable bounded artifact bytes for a serialized adapter service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedTuiArtifactChunk {
    pub tool_call_id: String,
    pub artifact_id: String,
    pub reference_key: String,
    pub producer_plugin_id: String,
    pub schema: String,
    pub schema_version: u32,
    pub content_type: Option<String>,
    pub offset: u64,
    pub total_bytes: u64,
    pub revision: u64,
    pub finalized: bool,
    pub bytes: Vec<u8>,
}
