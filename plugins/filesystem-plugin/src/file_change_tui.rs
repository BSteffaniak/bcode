//! Native TUI rendering for filesystem file-change previews.

use bcode_tui_components::diff_viewer::{
    DiffViewerInput, DiffViewerLayout, DiffViewerStyle, diff_viewer_layout_with_style,
};
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

    fn render_mode(
        &self,
        _kind: &str,
        _payload: &serde_json::Value,
    ) -> bcode_plugin_sdk::tui::PluginTuiVisualRenderMode {
        bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::FullBlock
    }

    fn layout(
        &self,
        _kind: &str,
        payload: &serde_json::Value,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> (
        Vec<Line>,
        Vec<bcode_plugin_sdk::tui_visual::TuiVisualAnchor>,
    ) {
        file_change_layout(payload, context)
    }

    fn rows(
        &self,
        _kind: &str,
        payload: &serde_json::Value,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        file_change_rows(payload, context)
    }
}

pub fn file_change_rows(
    payload: &serde_json::Value,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> Vec<Line> {
    file_change_layout(payload, context).0
}

pub fn file_change_layout(
    payload: &serde_json::Value,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> (
    Vec<Line>,
    Vec<bcode_plugin_sdk::tui_visual::TuiVisualAnchor>,
) {
    let width = context.width();
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
        .and_then(|line| u32::try_from(line).ok());
    let new_start_line = payload
        .get("new_start_line")
        .and_then(serde_json::Value::as_u64)
        .and_then(|line| u32::try_from(line).ok());
    let line_numbers_known = old_start_line.is_some() && new_start_line.is_some();
    let old_start_line = old_start_line.unwrap_or(1);
    let new_start_line = new_start_line.unwrap_or(old_start_line);
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

    let theme = context.theme();
    let syntax_palette = theme.map(|theme| syntax_palette(theme.syntax));
    let diff_style = theme.map_or_else(DiffViewerStyle::default, |theme| DiffViewerStyle {
        text: theme.diff.text,
        muted: theme.diff.muted,
        title: theme.diff.title,
        label: theme.diff.label,
        added: theme.diff.added,
        removed: theme.diff.removed,
        hunk: theme.diff.hunk,
        added_row: theme.diff.added_row,
        removed_row: theme.diff.removed_row,
        added_emphasis: theme.diff.added_emphasis,
        removed_emphasis: theme.diff.removed_emphasis,
    });
    let projection = diff_viewer_layout_with_style(
        DiffViewerInput {
            syntax_palette,
            label: &context.display_path(path).to_string(),
            old_text,
            new_text,
            old_start_line,
            new_start_line,
            line_numbers_known,
            title,
            subtitle,
            argument_bytes,
            truncated,
            layout: match context.diff_layout() {
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint } => {
                    DiffViewerLayout::Auto { breakpoint }
                }
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified => DiffViewerLayout::Unified,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::SideBySide => {
                    DiffViewerLayout::SideBySide
                }
            },
        },
        width,
        diff_style,
    );
    source_anchors(projection, old_start_line, new_start_line)
}

fn source_anchors(
    projection: bcode_tui_components::diff_viewer::DiffViewerProjection,
    old_start_line: u32,
    new_start_line: u32,
) -> (
    Vec<Line>,
    Vec<bcode_plugin_sdk::tui_visual::TuiVisualAnchor>,
) {
    let mut anchors = Vec::new();
    for mapping in projection.source_lines {
        let side = match mapping.side {
            bcode_tui_components::diff_viewer::DiffSourceSide::Old => "old",
            bcode_tui_components::diff_viewer::DiffSourceSide::New => "new",
        };
        // Fragment-relative line identity remains stable when execution supplies
        // absolute file line numbers that were unavailable during the draft.
        let start = if side == "old" {
            old_start_line
        } else {
            new_start_line
        };
        anchors.push(bcode_plugin_sdk::tui_visual::TuiVisualAnchor {
            key: format!("diff:{side}:{}", mapping.line.saturating_sub(start)),
            row: mapping.row,
            source: None,
        });
    }
    (projection.rows, anchors)
}

fn syntax_palette(
    theme: bcode_plugin_sdk::tui::PluginTuiSyntaxTheme,
) -> bcode_syntax_render::SyntaxPalette {
    let color = |color: bcode_plugin_sdk::tui::PluginTuiSyntaxColor| {
        bcode_syntax_render::SyntaxColor::from_tui(color.into())
    };
    bcode_syntax_render::SyntaxPalette {
        text: color(theme.text),
        comment: color(theme.comment),
        keyword: color(theme.keyword),
        function: color(theme.function),
        variable: color(theme.variable),
        string: color(theme.string),
        number: color(theme.number),
        type_name: color(theme.type_name),
        operator: color(theme.operator),
        punctuation: color(theme.punctuation),
        heading: color(theme.heading),
        link: color(theme.link),
        raw: color(theme.raw),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn wrapped_source_line_keeps_identity_at_different_widths() {
        use bcode_plugin_sdk::tui::{PluginTuiDiffLayout, PluginTuiVisualRenderContext};
        let payload = serde_json::json!({"path":"test.rs", "old_text":"", "new_text":"alpha beta gamma delta epsilon zeta eta theta\nsecond line"});
        let layout = |width| {
            super::file_change_layout(
                &payload,
                &PluginTuiVisualRenderContext::new(width, PluginTuiDiffLayout::Unified, None),
            )
        };
        let (wide_rows, wide) = layout(90);
        let (narrow_rows, narrow) = layout(35);
        let second = |anchors: &[bcode_plugin_sdk::tui_visual::TuiVisualAnchor]| {
            anchors
                .iter()
                .find(|a| a.key == "diff:new:1")
                .expect("second source line")
                .row
        };
        assert!(second(&narrow) > second(&wide));
        assert!(line_text(&wide_rows[second(&wide)]).contains("second line"));
        assert!(line_text(&narrow_rows[second(&narrow)]).contains("second line"));
    }

    #[test]
    fn source_correspondence_excludes_omitted_and_clipped_lines() {
        use bcode_plugin_sdk::tui::{PluginTuiDiffLayout, PluginTuiVisualRenderContext};
        use std::fmt::Write as _;
        let mut text = String::new();
        for i in 0..100 {
            writeln!(&mut text, "line {i}").expect("string write");
        }
        let payload = serde_json::json!({"path":"test.rs", "old_text":"", "new_text":text});
        let (rows, anchors) = super::file_change_layout(
            &payload,
            &PluginTuiVisualRenderContext::new(40, PluginTuiDiffLayout::Unified, None),
        );
        assert!(anchors.len() < 100);
        for anchor in anchors {
            let number = anchor
                .key
                .rsplit(':')
                .next()
                .expect("line")
                .parse::<usize>()
                .expect("source line");
            assert!(line_text(&rows[anchor.row]).contains(&format!("line {number}")));
            assert!(!line_text(&rows[anchor.row]).contains("omitted"));
        }
    }

    #[test]
    fn source_lines_survive_layout_changes_and_unknown_absolute_numbers() {
        let context =
            |layout| bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(90, layout, None);
        let payload = serde_json::json!({"path":"test.rs", "old_text":"old alpha\nold beta", "new_text":"new alpha\nnew beta"});
        let (unified_rows, unified) = super::file_change_layout(
            &payload,
            &context(bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified),
        );
        let (split_rows, split) = super::file_change_layout(
            &payload,
            &context(bcode_plugin_sdk::tui::PluginTuiDiffLayout::SideBySide),
        );
        let keys = |anchors: &[bcode_plugin_sdk::tui_visual::TuiVisualAnchor]| {
            anchors
                .iter()
                .map(|a| a.key.clone())
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(keys(&unified), keys(&split));
        assert_eq!(unified.len(), 4);
        for (rows, anchors) in [(&unified_rows, &unified), (&split_rows, &split)] {
            bcode_plugin_sdk::tui_visual::validate_visual_anchors(anchors, rows.len())
                .expect("valid source mapping");
            for anchor in anchors {
                let text = line_text(&rows[anchor.row]);
                let expected = if anchor.key.ends_with(":0") {
                    "alpha"
                } else {
                    "beta"
                };
                assert!(text.contains(expected), "{}: {text}", anchor.key);
            }
        }
        let mut completed = payload;
        completed["old_start_line"] = serde_json::json!(100);
        completed["new_start_line"] = serde_json::json!(120);
        let (_, final_anchors) = super::file_change_layout(
            &completed,
            &context(bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified),
        );
        assert_eq!(keys(&unified), keys(&final_anchors));
    }

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
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                80,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("src/lib.rs"), "{rendered}");
        assert!(rendered.contains("before"), "{rendered}");
        assert!(rendered.contains("after"), "{rendered}");
    }
}
