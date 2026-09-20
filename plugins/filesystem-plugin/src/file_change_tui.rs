//! Native TUI rendering for filesystem file-change previews.

use bcode_tui_components::diff_viewer::{
    DiffViewerInput, DiffViewerLayout, DiffViewerStyle, diff_viewer_layout_with_style,
};
use bmux_tui::prelude::Line;

/// Filesystem file-change TUI visual adapter.
#[derive(Default)]
pub struct FileChangeTuiVisualAdapter {
    previews: std::sync::Mutex<std::collections::BTreeMap<(String, String), SourcePreview>>,
}

struct SourcePreview {
    revision: u64,
    total_bytes: u64,
    bytes: Vec<u8>,
}

const SOURCE_PREVIEW_BYTES: usize = 8192;
const SOURCE_PREVIEW_ENTRIES: usize = 64;

fn source_preview_key(key: &str) -> bool {
    let fields: Vec<_> = key.split('-').collect();
    matches!(fields.as_slice(), ["file", index, "old" | "new"] | ["file", index, "old" | "new", "0"] if index.parse::<usize>().is_ok_and(|index| index < 64))
}

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for FileChangeTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        // Keep the old plugin-view schema as a local TUI-only replay shim for pre-artifact logs.
        matches!(
            kind,
            "bcode.filesystem.change" | "bcode.filesystem.file_change" | "bcode.filesystem.batch"
        )
    }

    fn accepts_artifact_reference(
        &self,
        kind: &str,
        reference_key: &str,
        content_type: Option<&str>,
    ) -> bool {
        kind == "bcode.filesystem.batch"
            && source_preview_key(reference_key)
            && content_type == Some("application/octet-stream")
    }

    fn artifact_chunk(
        &self,
        chunk: &bcode_plugin_sdk::tui::PluginTuiArtifactChunk,
    ) -> Result<(), String> {
        // Batch sources are immutable finalized artifacts, never live streams.
        if !chunk.finalized
            || chunk.schema_version != 1
            || chunk.artifact_id.len() > 4096
            || chunk.reference_key.len() > 64
            || chunk.producer_plugin_id != "bcode.filesystem"
            || !self.accepts_artifact_reference(
                &chunk.schema,
                &chunk.reference_key,
                chunk.content_type.as_deref(),
            )
            || chunk.offset.saturating_add(chunk.bytes.len() as u64) > chunk.total_bytes
        {
            return Err("invalid filesystem preview chunk".to_owned());
        }
        // Retain only the first bounded window; the full source remains in artifact storage.
        if chunk.offset >= SOURCE_PREVIEW_BYTES as u64 {
            return Ok(());
        }
        let mut previews = self
            .previews
            .lock()
            .map_err(|_| "filesystem preview unavailable")?;
        let key = (chunk.artifact_id.clone(), chunk.reference_key.clone());
        // The host advances independently of this disposable preview cache. An
        // evicted stream may still deliver its tail; do not turn that into a
        // permanent host fetch error or render the tail as a source prefix.
        if chunk.offset != 0 && !previews.contains_key(&key) {
            return Ok(());
        }
        if !previews.contains_key(&key) && previews.len() >= SOURCE_PREVIEW_ENTRIES {
            previews.pop_first();
        }
        let preview = previews.entry(key).or_insert_with(|| SourcePreview {
            revision: chunk.revision,
            total_bytes: chunk.total_bytes,
            bytes: Vec::new(),
        });
        if preview.revision != chunk.revision || preview.total_bytes != chunk.total_bytes {
            return Err("conflicting finalized filesystem preview".to_owned());
        }
        let bytes = &mut preview.bytes;
        if chunk.offset == 0 {
            bytes.clear();
        }
        if chunk.offset != bytes.len() as u64 {
            return Err("noncontiguous filesystem preview chunk".to_owned());
        }
        bytes.extend(chunk.bytes.iter().take(SOURCE_PREVIEW_BYTES - bytes.len()));
        drop(previews);
        Ok(())
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
        kind: &str,
        payload: &serde_json::Value,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> (
        Vec<Line>,
        Vec<bcode_plugin_sdk::tui_visual::TuiVisualAnchor>,
    ) {
        if kind == "bcode.filesystem.batch" {
            let mut payload = payload.clone();
            if let (Some(id), Ok(previews)) = (
                payload["_bcode_artifact"]["artifact_id"]
                    .as_str()
                    .map(str::to_owned),
                self.previews.lock(),
            ) && let Some(files) = payload["files"].as_array_mut()
            {
                for (index, file) in files.iter_mut().enumerate().take(64) {
                    if file["change"]["omitted"] != true {
                        continue;
                    }
                    let sources: Option<Vec<_>> = ["old", "new"]
                        .into_iter()
                        .map(|side| {
                            let key = format!("file-{index}-{side}");
                            let bytes = &previews
                                .get(&(id.clone(), key.clone()))
                                .or_else(|| previews.get(&(id.clone(), format!("{key}-0"))))?
                                .bytes;
                            // A byte window can end inside a UTF-8 scalar. Drop only that suffix.
                            let end = std::str::from_utf8(bytes)
                                .map_or_else(|error| error.valid_up_to(), str::len);
                            Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
                        })
                        .collect();
                    if let Some(sources) = sources {
                        file["change"]["old_text"] = serde_json::json!(sources[0]);
                        file["change"]["new_text"] = serde_json::json!(sources[1]);
                        file["change"]["preview_loaded"] = serde_json::json!(true);
                        file["change"]["truncated"] = serde_json::json!(true);
                    }
                }
            }
            batch_layout(&payload, context)
        } else {
            file_change_layout(payload, context)
        }
    }

    fn rows(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        self.layout(kind, payload, context).0
    }
}

/// Validate the versioned display envelope before interpreting any file outcome.
fn batch_files(payload: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    if payload["version"].as_u64()? != 1 {
        return None;
    }
    let files = payload["files"].as_array()?;
    if files.is_empty() || files.len() > 64 {
        return None;
    }
    let mut remaining: usize = 64 * 1024;
    for file in files {
        if file["path"].as_str()?.len() > 4096 {
            return None;
        }
        if !matches!(
            file["status"].as_str()?,
            "committed" | "unchanged" | "failed" | "cancelled" | "not_attempted" | "unknown"
        ) {
            return None;
        }
        if !file["error"].is_null() && file["error"].as_str()?.len() > 4096 {
            return None;
        }
        let change = &file["change"];
        if change.is_null() {
            continue;
        }
        if file["status"] != "committed" {
            return None;
        }
        if !change["omitted"].as_bool()? {
            let bytes = change["old_text"]
                .as_str()?
                .len()
                .checked_add(change["new_text"].as_str()?.len())?;
            if bytes > 16 * 1024 {
                return None;
            }
            remaining = remaining.checked_sub(bytes)?;
        }
    }
    Some(files)
}

/// Describe retained source availability without treating missing snapshots as a diff.
fn retained_change_summary(change: &serde_json::Value) -> String {
    let mut summary = String::from("Diff exceeds inline budget.");
    for (side, label) in [
        ("diff", "unified diff"),
        ("old", "old snapshot"),
        ("new", "new snapshot"),
    ] {
        use std::fmt::Write as _;
        let retained = &change["retained"][side];
        if retained["unavailable"] == true {
            let _ = write!(summary, " {label} unavailable.");
        } else if let Some(uri) = retained["reference"]["uri"]
            .as_str()
            .filter(|uri| !uri.is_empty() && uri.len() <= 4096)
        {
            let _ = write!(summary, " {label}: {uri}");
        } else if let Some(parts) = retained_source_parts(retained) {
            let _ = write!(summary, " {label} parts (byte order):");
            for (offset, uri) in parts {
                let _ = write!(summary, " [{offset}] {uri}");
            }
        } else {
            let _ = write!(summary, " {label} unavailable.");
        }
    }
    summary
}

/// Accept only a complete, bounded version of the filesystem multipart envelope.
fn retained_source_parts(value: &serde_json::Value) -> Option<Vec<(u64, &str)>> {
    if value["version"] != 1 || value["unavailable"] == true {
        return None;
    }
    let parts = value["parts"].as_array()?;
    if parts.is_empty() || parts.len() > 64 {
        return None;
    }
    let mut offset = 0u64;
    let mut references = Vec::with_capacity(parts.len());
    for part in parts {
        let uri = part["reference"]["uri"].as_str()?;
        let length = part["byte_len"].as_u64()?;
        if part["unavailable"] == true
            || part["offset"].as_u64()? != offset
            || length == 0
            || uri.is_empty()
            || uri.len() > 4096
        {
            return None;
        }
        references.push((offset, uri));
        offset = offset.checked_add(length)?;
    }
    (value["byte_len"].as_u64()? == offset).then_some(references)
}

fn batch_layout(
    payload: &serde_json::Value,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> (
    Vec<Line>,
    Vec<bcode_plugin_sdk::tui_visual::TuiVisualAnchor>,
) {
    if context.width() == 0 {
        return (Vec::new(), Vec::new());
    }
    let Some(files) = batch_files(payload) else {
        let (rows, _) = file_change_layout(
            &serde_json::json!({"title":"Unsupported or invalid batch outcome", "path":"multi-edit", "subtitle":"Outcome unavailable; inspect files before retrying"}),
            context,
        );
        return (
            rows.into_iter()
                .map(|row| row.viewport(0, usize::from(context.width())))
                .collect(),
            Vec::new(),
        );
    };
    let mut rows = Vec::new();
    let mut anchors = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let change = &file["change"];
        let mut preview = if change["omitted"] == false || change["preview_loaded"] == true {
            change.clone()
        } else {
            serde_json::json!({})
        };
        preview["path"] = file["path"].clone();
        preview["title"] = file["status"].clone();
        preview["subtitle"] = if !file["error"].is_null() {
            file["error"].clone()
        } else if change["omitted"] == true {
            serde_json::json!(if change["preview_loaded"] == true {
                format!(
                    "Partial source preview (first 8192 bytes per side/first part; not the complete diff). {}",
                    retained_change_summary(change)
                )
            } else {
                retained_change_summary(change)
            })
        } else {
            serde_json::Value::Null
        };
        let (file_rows, file_anchors) = file_change_layout(&preview, context);
        let offset = rows.len();
        anchors.extend(file_anchors.into_iter().map(|mut anchor| {
            anchor.key = format!("batch:{index}:{}", anchor.key);
            anchor.row += offset;
            anchor
        }));
        rows.extend(file_rows);
    }
    anchors.retain(|anchor| {
        !rows[anchor.row]
            .viewport(0, usize::from(context.width()))
            .spans
            .is_empty()
    });
    let rows = rows
        .into_iter()
        .map(|row| row.viewport(0, usize::from(context.width())))
        .collect();
    (rows, anchors)
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
    fn retained_diff_is_advertised_without_confusing_missing_or_partial_data() {
        let available =
            serde_json::json!({"retained":{"diff":{"reference":{"uri":"artifact://diff"}}}});
        assert!(retained_change_summary(&available).contains("unified diff: artifact://diff"));
        let multipart = serde_json::json!({"retained":{"diff":{"version":1,"byte_len":4,"parts":[
            {"offset":0,"byte_len":2,"reference":{"uri":"artifact://first"}},
            {"offset":2,"byte_len":2,"reference":{"uri":"artifact://second"}}
        ]}}});
        assert!(retained_change_summary(&multipart).contains(
            "unified diff parts (byte order): [0] artifact://first [2] artifact://second"
        ));
        let mut invalid = multipart;
        invalid["retained"]["diff"]["parts"][1]["offset"] = serde_json::json!(3);
        let summary = retained_change_summary(&invalid);
        assert!(summary.contains("unified diff unavailable."));
        assert!(!summary.contains("artifact://first"));
    }

    #[test]
    fn retained_summary_distinguishes_available_and_missing_sources() {
        let change = serde_json::json!({"retained": {
            "old": {"reference": {"uri": "artifact://old"}},
            "new": {"unavailable": true}
        }});
        let summary = super::retained_change_summary(&change);
        assert!(summary.contains("old snapshot: artifact://old"));
        assert!(summary.contains("new snapshot unavailable"));
        let oversized = serde_json::json!({"retained": {
            "old": {"reference": {"uri": "x".repeat(4097)}}
        }});
        assert!(super::retained_change_summary(&oversized).contains("old snapshot unavailable"));
    }
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
    fn batch_layout_preserves_file_identity_and_rejects_future_outcomes() {
        use bcode_plugin_sdk::tui::{PluginTuiDiffLayout, PluginTuiVisualRenderContext};
        let mut payload = serde_json::json!({"version":1,"files":[
            {"path":"一.rs","status":"committed","error":null,"change":{"omitted":false,"old_text":"before\n","new_text":"after 👩‍💻 e\u{301}\n","old_start_line":1,"new_start_line":1}},
            {"path":"two.rs","status":"committed","error":null,"change":{"omitted":false,"old_text":"before\n","new_text":"second\n","old_start_line":1,"new_start_line":1}},
            {"path":"three.rs","status":"unknown","error":"inspect before retrying","change":null}
        ]});
        for width in [0, 1, 2, 20, 80] {
            let context =
                PluginTuiVisualRenderContext::new(width, PluginTuiDiffLayout::Unified, None);
            let (rows, anchors) = batch_layout(&payload, &context);
            bcode_plugin_sdk::tui_visual::validate_visual_anchors(&anchors, rows.len()).unwrap();
            assert!(
                rows.iter().all(|row| row.width() <= usize::from(width)),
                "width {width}: {:?}",
                rows.iter().map(Line::width).collect::<Vec<_>>()
            );
            if width == 80 {
                let text = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");
                assert!(text.contains("unknown"));
                assert!(text.contains("inspect before retrying"));
                assert!(anchors.iter().any(|a| a.key.starts_with("batch:0:")));
                assert!(anchors.iter().any(|a| a.key.starts_with("batch:1:")));
            }
        }
        payload["version"] = serde_json::json!(2);
        assert!(batch_files(&payload).is_none());
        payload["version"] = serde_json::json!(1);
        payload["files"][0]["status"] = serde_json::json!("future");
        assert!(batch_files(&payload).is_none());
    }

    #[test]
    fn unavailable_or_empty_snapshot_references_are_not_advertised() {
        for source in [
            serde_json::json!({"unavailable":true,"reference":{"uri":"artifact://stale"}}),
            serde_json::json!({"reference":{"uri":""}}),
        ] {
            let summary = retained_change_summary(&serde_json::json!({"retained":{"old":source}}));
            assert!(summary.contains("old snapshot unavailable."));
            assert!(!summary.contains("artifact://stale"));
        }
    }

    #[test]
    fn multipart_snapshots_are_presented_in_byte_order_and_fail_closed() {
        let source = serde_json::json!({"version":1,"byte_len":5,"parts":[
            {"offset":0,"byte_len":2,"reference":{"uri":"artifact://first"}},
            {"offset":2,"byte_len":3,"reference":{"uri":"artifact://second"}}
        ]});
        let summary = retained_change_summary(&serde_json::json!({"retained":{"old":source}}));
        assert!(summary.contains(
            "old snapshot parts (byte order): [0] artifact://first [2] artifact://second"
        ));
        assert!(summary.contains("new snapshot unavailable"));
        for invalid in [
            serde_json::json!({"version":2,"byte_len":5,"parts":source["parts"]}),
            serde_json::json!({"version":1,"byte_len":6,"parts":source["parts"]}),
            serde_json::json!({"version":1,"byte_len":0,"parts":[]}),
        ] {
            assert!(retained_source_parts(&invalid).is_none());
        }
        for (field, value) in [
            ("offset", serde_json::json!(3)),
            ("byte_len", serde_json::json!(0)),
            ("unavailable", serde_json::json!(true)),
            ("reference", serde_json::json!({"uri":"x".repeat(4097)})),
        ] {
            let mut invalid = source.clone();
            invalid["parts"][1][field] = value;
            assert!(retained_source_parts(&invalid).is_none());
        }
    }

    #[test]
    fn evicted_preview_ignores_tail_and_rehydrates_from_start() {
        use bcode_plugin_sdk::tui::{PluginTuiArtifactChunk, PluginTuiVisualAdapter};
        let adapter = FileChangeTuiVisualAdapter::default();
        let mut chunk = PluginTuiArtifactChunk {
            tool_call_id: "call".into(),
            artifact_id: "000".into(),
            reference_key: "file-0-old".into(),
            producer_plugin_id: "bcode.filesystem".into(),
            schema: "bcode.filesystem.batch".into(),
            schema_version: 1,
            content_type: Some("application/octet-stream".into()),
            offset: 0,
            total_bytes: 6,
            revision: 1,
            finalized: true,
            bytes: b"abc".to_vec(),
        };
        for index in 0..=SOURCE_PREVIEW_ENTRIES {
            chunk.artifact_id = format!("{index:03}");
            adapter.artifact_chunk(&chunk).unwrap();
        }
        chunk.artifact_id = "000".into();
        chunk.offset = 3;
        chunk.bytes = b"def".to_vec();
        adapter.artifact_chunk(&chunk).unwrap();
        let key = (chunk.artifact_id.clone(), chunk.reference_key.clone());
        assert!(!adapter.previews.lock().unwrap().contains_key(&key));
        chunk.offset = 0;
        chunk.bytes = b"abc".to_vec();
        adapter.artifact_chunk(&chunk).unwrap();
        chunk.offset = 3;
        chunk.bytes = b"def".to_vec();
        adapter.artifact_chunk(&chunk).unwrap();
        let previews = adapter.previews.lock().unwrap();
        assert_eq!(previews[&key].bytes, b"abcdef");
        assert_eq!(previews.len(), SOURCE_PREVIEW_ENTRIES);
        drop(previews);
    }

    #[test]
    fn retained_sources_hydrate_bounded_batch_preview() {
        use bcode_plugin_sdk::tui::{PluginTuiArtifactChunk, PluginTuiVisualAdapter};
        let adapter = FileChangeTuiVisualAdapter::default();
        let mut chunk = PluginTuiArtifactChunk {
            tool_call_id: "call".into(),
            artifact_id: "batch".into(),
            reference_key: "file-0-old-0".into(),
            producer_plugin_id: "bcode.filesystem".into(),
            schema: "bcode.filesystem.batch".into(),
            schema_version: 1,
            content_type: Some("application/octet-stream".into()),
            offset: 0,
            total_bytes: 7,
            revision: 1,
            finalized: true,
            bytes: b"before\n".to_vec(),
        };
        adapter.artifact_chunk(&chunk).unwrap();
        chunk.reference_key = "file-0-new-0".into();
        chunk.bytes = b"after\n".to_vec();
        chunk.total_bytes = 6;
        adapter.artifact_chunk(&chunk).unwrap();
        let payload = serde_json::json!({"version":1,"_bcode_artifact":{"artifact_id":"batch"},"files":[{
            "path":"a.rs","status":"committed","change":{"omitted":true}
        }]});
        let context = bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
            80,
            bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
            None,
        );
        let text = adapter
            .rows("bcode.filesystem.batch", &payload, &context)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("before") && text.contains("after"), "{text}");
        chunk.offset = 1;
        assert!(adapter.artifact_chunk(&chunk).is_err());
        chunk.offset = 0;
        chunk.revision += 1;
        assert!(adapter.artifact_chunk(&chunk).is_err());
        chunk.revision -= 1;
        chunk.finalized = false;
        assert!(adapter.artifact_chunk(&chunk).is_err());
        chunk.finalized = true;
        chunk.artifact_id = "another-batch".into();
        chunk.bytes = vec![b'x'; SOURCE_PREVIEW_BYTES * 2];
        chunk.total_bytes = chunk.bytes.len() as u64;
        adapter.artifact_chunk(&chunk).unwrap();
        assert!(
            adapter
                .previews
                .lock()
                .unwrap()
                .values()
                .all(|preview| preview.bytes.len() <= SOURCE_PREVIEW_BYTES)
        );
        chunk.schema_version = 2;
        assert!(adapter.artifact_chunk(&chunk).is_err());
    }

    #[test]
    fn adapter_supports_raw_filesystem_change_artifact_schema() {
        let payload = serde_json::json!({
            "path": "src/lib.rs",
            "summary": "edited file",
            "old_text": "before\n",
            "new_text": "after\n"
        });
        let adapter = FileChangeTuiVisualAdapter::default();
        assert!(bcode_plugin_sdk::tui::PluginTuiVisualAdapter::supports(
            &adapter,
            "bcode.filesystem.change"
        ));

        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &adapter,
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
