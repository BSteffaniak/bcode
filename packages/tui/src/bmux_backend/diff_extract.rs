//! Diff summary extraction for BMUX backend transcript/tool state.

use bmux_tui::diff::{DiffFileSummary, DiffLine, DiffLineKind};

/// Extract a file diff preview from a filesystem tool request.
pub(super) fn diff_from_tool_request(
    tool_name: &str,
    arguments_json: &str,
) -> Option<(DiffFileSummary, Vec<DiffLine>)> {
    let normalized_tool = tool_name.replace(['-', '.'], "_").to_ascii_lowercase();
    if !matches!(
        normalized_tool.as_str(),
        "filesystem_edit" | "filesystem_write"
    ) {
        return None;
    }
    let value = serde_json::from_str::<serde_json::Value>(arguments_json).ok()?;
    let path = value
        .get("path")
        .or_else(|| value.get("file_path"))
        .or_else(|| value.get("file"))?
        .as_str()?;
    let (added, removed) = count_edit_lines(&value);
    let summary = DiffFileSummary::new(path, added, removed);
    let lines = diff_lines_from_value(path, &value);
    Some((summary, lines))
}

fn diff_lines_from_value(path: &str, value: &serde_json::Value) -> Vec<DiffLine> {
    let old_text = value
        .get("old_text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let new_text = value
        .get("new_text")
        .or_else(|| value.get("contents"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let mut lines = vec![
        DiffLine::new(DiffLineKind::FileHeader, None, None, format!("--- {path}")),
        DiffLine::new(DiffLineKind::FileHeader, None, None, format!("+++ {path}")),
        DiffLine::new(DiffLineKind::HunkHeader, None, None, "@@ inferred edit @@"),
    ];
    let mut old_line = 1_u32;
    for line in old_text.lines().take(200) {
        lines.push(DiffLine::new(
            DiffLineKind::Removed,
            Some(old_line),
            None,
            line.to_owned(),
        ));
        old_line = old_line.saturating_add(1);
    }
    let mut new_line = 1_u32;
    for line in new_text.lines().take(200) {
        lines.push(DiffLine::new(
            DiffLineKind::Added,
            None,
            Some(new_line),
            line.to_owned(),
        ));
        new_line = new_line.saturating_add(1);
    }
    if old_text.lines().count() > 200 || new_text.lines().count() > 200 {
        lines.push(DiffLine::new(
            DiffLineKind::Context,
            None,
            None,
            "… diff preview truncated …",
        ));
    }
    lines
}

fn count_edit_lines(value: &serde_json::Value) -> (u32, u32) {
    let new_text = value
        .get("new_text")
        .or_else(|| value.get("contents"))
        .and_then(serde_json::Value::as_str);
    let old_text = value.get("old_text").and_then(serde_json::Value::as_str);
    match (new_text, old_text) {
        (Some(new_text), Some(old_text)) => (line_count(new_text), line_count(old_text)),
        (Some(new_text), None) => (line_count(new_text), 0),
        (None, Some(old_text)) => (0, line_count(old_text)),
        (None, None) => (0, 0),
    }
}

fn line_count(value: &str) -> u32 {
    u32::try_from(value.lines().count().max(1)).unwrap_or(u32::MAX)
}
