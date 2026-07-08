//! Native TUI rendering for filesystem request and result visuals.

use bmux_tui::prelude::{Color, Line, Span, Style};
use serde_json::Value;

/// Filesystem request/result TUI visual adapter.
pub struct FilesystemTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for FilesystemTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        matches!(
            kind,
            "bcode.filesystem.request"
                | "bcode.filesystem.read"
                | "bcode.filesystem.image"
                | "bcode.filesystem.exists"
                | "bcode.filesystem.list"
                | "bcode.filesystem.find"
                | "bcode.filesystem.grep"
                | "bcode.filesystem.stat"
                | "bcode.filesystem.artifact.metadata"
                | "bcode.filesystem.artifact.read"
                | "bcode.filesystem.artifact.grep"
        )
    }

    fn render_mode(
        &self,
        _kind: &str,
        _payload: &Value,
    ) -> bcode_plugin_sdk::tui::PluginTuiVisualRenderMode {
        bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::TranscriptBlock
    }

    fn rows(&self, kind: &str, payload: &Value, width: u16) -> Vec<Line> {
        match kind {
            "bcode.filesystem.request" => request_rows(payload),
            "bcode.filesystem.read" | "bcode.filesystem.artifact.read" => {
                read_rows(kind, payload, width)
            }
            "bcode.filesystem.image" => image_rows(payload),
            "bcode.filesystem.exists" => exists_rows(payload),
            "bcode.filesystem.list" => list_rows(payload),
            "bcode.filesystem.find" => find_rows(payload),
            "bcode.filesystem.grep" | "bcode.filesystem.artifact.grep" => grep_rows(payload, width),
            "bcode.filesystem.stat" | "bcode.filesystem.artifact.metadata" => {
                metadata_rows(kind, payload)
            }
            _ => Vec::new(),
        }
    }
}

fn request_rows(payload: &Value) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let operation = text(payload, "operation").unwrap_or("filesystem tool");
    let mut rows = vec![Line::from_spans(vec![
        Span::styled("◆ ", accent()),
        Span::styled(operation.to_owned(), title()),
    ])];
    push_kv(&mut rows, "path", text(arguments, "path"));
    push_kv(&mut rows, "pattern", text(arguments, "pattern"));
    push_kv(&mut rows, "glob", text(arguments, "glob"));
    push_kv(&mut rows, "offset", number(arguments, "offset"));
    push_kv(&mut rows, "limit", number(arguments, "limit"));
    push_kv(&mut rows, "recursive", bool_text(arguments, "recursive"));
    push_kv(
        &mut rows,
        "ignore case",
        bool_text(arguments, "ignore_case"),
    );
    push_kv(&mut rows, "from end", bool_text(arguments, "from_end"));
    rows
}

fn read_rows(kind: &str, payload: &Value, width: u16) -> Vec<Line> {
    let mut rows = card_header(if kind.contains("artifact") {
        "Artifact bytes"
    } else {
        "File contents"
    });
    push_kv(&mut rows, "path", text(payload, "path"));
    push_kv(
        &mut rows,
        "lines",
        range_text(payload, "start_line", "end_line", "total_lines"),
    );
    push_kv(&mut rows, "bytes", byte_summary(payload));
    push_kv(&mut rows, "truncated", bool_text(payload, "truncated"));
    if let Some(contents) = text(payload, "contents").or_else(|| text(payload, "preview")) {
        rows.push(Line::raw(""));
        rows.extend(preview_lines(contents, width));
    }
    rows
}

fn image_rows(payload: &Value) -> Vec<Line> {
    let mut rows = card_header("Image file");
    push_kv(&mut rows, "path", text(payload, "path"));
    push_kv(&mut rows, "type", text(payload, "mime_type"));
    push_kv(&mut rows, "dimensions", dimensions(payload));
    push_kv(&mut rows, "size", number(payload, "byte_len"));
    rows
}

fn exists_rows(payload: &Value) -> Vec<Line> {
    let exists = payload
        .get("exists")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut rows = card_header(if exists {
        "Path exists"
    } else {
        "Path missing"
    });
    push_kv(&mut rows, "path", text(payload, "path"));
    push_kv(
        &mut rows,
        "exists",
        Some(if exists { "yes" } else { "no" }.to_owned()),
    );
    rows
}

fn list_rows(payload: &Value) -> Vec<Line> {
    let entries = payload
        .get("entries")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let mut rows = card_header(&format!("Directory entries ({entries})"));
    push_kv(&mut rows, "backend", text(payload, "backend"));
    push_kv(&mut rows, "visited", number(payload, "visited_entries"));
    push_kv(&mut rows, "partial", bool_text(payload, "partial"));
    if let Some(message) = text(payload, "message") {
        push_kv(&mut rows, "note", Some(message));
    }
    rows.push(Line::raw(""));
    if let Some(values) = payload.get("entries").and_then(Value::as_array) {
        for entry in values.iter().take(25) {
            let kind = text(entry, "kind").unwrap_or("file");
            let icon = match kind {
                "directory" => "󰉋",
                "symlink" => "↪",
                _ => "󰈙",
            };
            rows.push(Line::from_spans(vec![
                Span::styled(format!("  {icon} "), accent()),
                Span::styled(text(entry, "path").unwrap_or_default(), path_style()),
                Span::styled(format!("  {kind}"), muted()),
            ]));
        }
        if values.len() > 25 {
            rows.push(Line::from_spans(vec![Span::styled(
                format!("  … {} more entries", values.len() - 25),
                muted(),
            )]));
        }
    }
    rows
}

fn find_rows(payload: &Value) -> Vec<Line> {
    let paths = payload
        .get("paths")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let mut rows = card_header(&format!("Path matches ({paths})"));
    push_kv(&mut rows, "backend", text(payload, "backend"));
    push_kv(&mut rows, "visited", number(payload, "visited_entries"));
    push_kv(&mut rows, "partial", bool_text(payload, "partial"));
    rows.push(Line::raw(""));
    if let Some(values) = payload.get("paths").and_then(Value::as_array) {
        for path in values.iter().filter_map(Value::as_str).take(30) {
            rows.push(Line::from_spans(vec![
                Span::styled("  • ", accent()),
                Span::styled(path.to_owned(), path_style()),
            ]));
        }
        if values.len() > 30 {
            rows.push(Line::from_spans(vec![Span::styled(
                format!("  … {} more paths", values.len() - 30),
                muted(),
            )]));
        }
    }
    rows
}

fn grep_rows(payload: &Value, width: u16) -> Vec<Line> {
    let matches = payload
        .get("matches")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let mut rows = card_header(&format!("Text matches ({matches})"));
    push_kv(&mut rows, "backend", text(payload, "backend"));
    push_kv(&mut rows, "partial", bool_text(payload, "partial"));
    if let Some(message) = text(payload, "message") {
        push_kv(&mut rows, "note", Some(message));
    }
    rows.push(Line::raw(""));
    if let Some(values) = payload.get("matches").and_then(Value::as_array) {
        for value in values.iter().take(25) {
            let location = format!(
                "{}:{}",
                text(value, "path").unwrap_or_default(),
                number(value, "line_number").unwrap_or_default()
            );
            rows.push(Line::from_spans(vec![
                Span::styled("  ▸ ", accent()),
                Span::styled(location, path_style()),
            ]));
            if let Some(line) = text(value, "line") {
                rows.push(Line::from_spans(vec![
                    Span::styled("    │ ", muted()),
                    Span::raw(truncate(line, usize::from(width.saturating_sub(8)))),
                ]));
            }
        }
        if values.len() > 25 {
            rows.push(Line::from_spans(vec![Span::styled(
                format!("  … {} more matches", values.len() - 25),
                muted(),
            )]));
        }
    }
    rows
}

fn metadata_rows(kind: &str, payload: &Value) -> Vec<Line> {
    let mut rows = card_header(if kind.contains("artifact") {
        "Artifact metadata"
    } else {
        "Path metadata"
    });
    push_kv(&mut rows, "path", text(payload, "path"));
    push_kv(&mut rows, "kind", text(payload, "kind"));
    push_kv(&mut rows, "exists", bool_text(payload, "exists"));
    push_kv(&mut rows, "bytes", number(payload, "byte_len"));
    push_kv(&mut rows, "content type", text(payload, "content_type"));
    push_kv(&mut rows, "complete", bool_text(payload, "complete"));
    if let Some(message) = text(payload, "message") {
        push_kv(&mut rows, "note", Some(message));
    }
    rows
}

fn card_header(title_text: &str) -> Vec<Line> {
    vec![Line::from_spans(vec![
        Span::styled("◆ ", accent()),
        Span::styled(title_text.to_owned(), title()),
    ])]
}

fn preview_lines(contents: &str, width: u16) -> Vec<Line> {
    let max_width = usize::from(width.saturating_sub(4)).max(20);
    let mut rows = Vec::new();
    for line in contents.lines().take(30) {
        rows.push(Line::from_spans(vec![
            Span::styled("  │ ", muted()),
            Span::raw(truncate(line, max_width)),
        ]));
    }
    if contents.lines().count() > 30 {
        rows.push(Line::from_spans(vec![Span::styled(
            "  … preview truncated",
            muted(),
        )]));
    }
    rows
}

fn push_kv<T>(rows: &mut Vec<Line>, key: &str, value: Option<T>)
where
    T: Into<String>,
{
    if let Some(value) = value.map(Into::into).filter(|value| !value.is_empty()) {
        rows.push(Line::from_spans(vec![
            Span::styled(format!("  {key}: "), label()),
            Span::styled(value, value_style()),
        ]));
    }
}

fn text<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

fn number(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value.to_string())
}

fn bool_text(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_bool)
        .map(|value| if value { "yes" } else { "no" }.to_owned())
}

fn range_text(payload: &Value, start: &str, end: &str, total: &str) -> Option<String> {
    let start = payload.get(start).and_then(Value::as_u64)?;
    let end = payload.get(end).and_then(Value::as_u64)?;
    let total = payload.get(total).and_then(Value::as_u64)?;
    Some(format!("{start}-{end} of {total}"))
}

fn byte_summary(payload: &Value) -> Option<String> {
    if let Some(total) = payload.get("total_bytes").and_then(Value::as_u64) {
        let returned = payload.get("returned_bytes").and_then(Value::as_u64);
        return Some(returned.map_or_else(
            || total.to_string(),
            |returned| format!("{returned} of {total}"),
        ));
    }
    number(payload, "byte_len")
}

fn dimensions(payload: &Value) -> Option<String> {
    let width = payload.get("width").and_then(Value::as_u64)?;
    let height = payload.get("height").and_then(Value::as_u64)?;
    Some(format!("{width}×{height}"))
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut output = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

const fn accent() -> Style {
    Style::new().fg(Color::Cyan)
}

const fn title() -> Style {
    Style::new().fg(Color::White)
}

const fn label() -> Style {
    Style::new().fg(Color::BrightBlack)
}

const fn value_style() -> Style {
    Style::new().fg(Color::White)
}

const fn path_style() -> Style {
    Style::new().fg(Color::Blue)
}

const fn muted() -> Style {
    Style::new().fg(Color::BrightBlack)
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
    fn renders_grep_matches() {
        let payload = serde_json::json!({
            "matches": [{"path": "src/lib.rs", "line_number": 7, "line": "needle here"}],
            "backend": "rust",
            "partial": false
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &FilesystemTuiVisualAdapter,
            "bcode.filesystem.grep",
            &payload,
            80,
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("Text matches (1)"), "{rendered}");
        assert!(rendered.contains("src/lib.rs:7"), "{rendered}");
        assert!(rendered.contains("needle here"), "{rendered}");
    }
}
