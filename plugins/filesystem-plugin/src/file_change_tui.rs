//! Native TUI rendering for filesystem file-change previews.

use bcode_tui_components::diff_viewer::{DiffViewerInput, DiffViewerLayout, diff_viewer_rows};
use bmux_tui::prelude::Line;

/// Filesystem file-change TUI visual adapter.
pub struct FileChangeTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for FileChangeTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        // Keep the old plugin-view schema as a local TUI-only replay shim for pre-artifact logs.
        matches!(
            kind,
            "bcode.filesystem.change" | "bcode.filesystem.file_change"
        )
    }

    fn rows(
        &self,
        _kind: &str,
        payload: &serde_json::Value,
        context: bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        let width = context.width;
        let path = payload
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<path>");
        let old_text = payload
            .get("old_text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let new_text = payload
            .get("new_text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let old_start_line = payload
            .get("old_start_line")
            .and_then(serde_json::Value::as_u64)
            .and_then(|line| u32::try_from(line).ok())
            .unwrap_or(1);
        let new_start_line = payload
            .get("new_start_line")
            .and_then(serde_json::Value::as_u64)
            .and_then(|line| u32::try_from(line).ok())
            .unwrap_or(old_start_line);
        let title = payload
            .get("title")
            .and_then(serde_json::Value::as_str)
            .or_else(|| payload.get("summary").and_then(serde_json::Value::as_str))
            .unwrap_or_else(|| {
                if payload.get("tool_name").is_some() {
                    "File change"
                } else {
                    "Streaming preview"
                }
            });
        let subtitle = payload.get("subtitle").and_then(serde_json::Value::as_str);
        let argument_bytes = payload
            .get("argument_bytes")
            .and_then(serde_json::Value::as_u64)
            .and_then(|bytes| usize::try_from(bytes).ok());
        let truncated = payload
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        diff_viewer_rows(
            DiffViewerInput {
                label: path,
                old_text,
                new_text,
                old_start_line,
                new_start_line,
                title,
                subtitle,
                argument_bytes,
                truncated,
                layout: match context.diff_layout {
                    bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint } => {
                        DiffViewerLayout::Auto { breakpoint }
                    }
                    bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified => {
                        DiffViewerLayout::Unified
                    }
                    bcode_plugin_sdk::tui::PluginTuiDiffLayout::SideBySide => {
                        DiffViewerLayout::SideBySide
                    }
                },
            },
            width,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref() as &str)
            .collect::<String>()
    }

    #[test]
    fn adapter_supports_raw_filesystem_change_artifact_schema() {
        let payload = serde_json::json!({
            "path": "src/lib.rs",
            "summary": "edited file",
            "old_text": "before\n",
            "new_text": "after\n"
        });
        assert!(bcode_plugin_sdk::tui::PluginTuiVisualAdapter::supports(
            &FileChangeTuiVisualAdapter,
            "bcode.filesystem.change"
        ));

        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &FileChangeTuiVisualAdapter,
            "bcode.filesystem.change",
            &payload,
            bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(80),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("src/lib.rs"), "{rendered}");
        assert!(rendered.contains("before"), "{rendered}");
        assert!(rendered.contains("after"), "{rendered}");
    }
}
