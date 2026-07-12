//! Native TUI rendering for document extraction visuals.

use bcode_tui_components::compact::header_rows;
use bmux_tui::prelude::{Color, Line, Span, Style};
use serde_json::Value;

/// Document TUI visual adapter.
pub struct DocumentTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for DocumentTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        matches!(
            kind,
            "bcode.document.request" | "bcode.document.extract_result" | "bcode.document.status"
        )
    }

    fn render_mode(
        &self,
        _kind: &str,
        _payload: &Value,
    ) -> bcode_plugin_sdk::tui::PluginTuiVisualRenderMode {
        bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::TranscriptBlock
    }

    fn rows(
        &self,
        kind: &str,
        payload: &Value,
        context: bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        let width = context.width;
        match kind {
            "bcode.document.request" => request_rows(payload),
            "bcode.document.extract_result" => extract_rows(payload, width),
            "bcode.document.status" => status_rows(payload),
            _ => Vec::new(),
        }
    }
}

fn request_rows(payload: &Value) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let mut rows = header("Document request");
    for key in ["operation", "path", "url", "max_bytes", "timeout_ms"] {
        push_kv(&mut rows, key, value(arguments, key));
    }
    rows
}

fn extract_rows(payload: &Value, width: u16) -> Vec<Line> {
    let metadata = [
        text(payload, "source").map(|value| Span::styled(value.to_owned(), value_style())),
        text(payload, "content_type").map(|value| Span::styled(value.to_owned(), muted())),
        text(payload, "extractor").map(|value| Span::styled(value.to_owned(), muted())),
    ]
    .into_iter()
    .flatten();
    let mut rows = header_rows(
        Span::styled("◆ ", accent()),
        Span::styled("Document extraction", title_style()),
        metadata,
        width,
        muted(),
    );
    if text(payload, "fallback_used").is_some_and(|value| value != "false") {
        push_kv(&mut rows, "fallback", text(payload, "fallback_used"));
    }
    push_kv(&mut rows, "document", text(payload, "document_path"));
    push_kv(&mut rows, "text path", text(payload, "text_path"));
    if payload.get("truncated").and_then(Value::as_bool) == Some(true) {
        push_kv(&mut rows, "truncated", Some("yes"));
    }
    if let Some(text) = text(payload, "text") {
        rows.push(Line::raw(""));
        rows.extend(preview_rows(text, width));
    }
    rows
}

fn status_rows(payload: &Value) -> Vec<Line> {
    let mut rows = header("Document extractors");
    let Some(extract) = payload.get("extract") else {
        return rows;
    };
    push_kv(&mut rows, "available", value(extract, "available"));
    push_kv(&mut rows, "order", array_text(extract, "configured_order"));
    if let Some(extractors) = extract.get("extractors").and_then(Value::as_array) {
        rows.push(Line::raw(""));
        for extractor in extractors {
            rows.push(Line::from_spans(vec![
                Span::styled("  ◆ ", accent()),
                Span::styled(
                    text(extractor, "name").unwrap_or("extractor").to_owned(),
                    title_style(),
                ),
                Span::styled(
                    format!(
                        "  {}  {}",
                        value(extractor, "available").unwrap_or_else(|| "unknown".to_string()),
                        text(extractor, "quality").unwrap_or_default()
                    ),
                    muted(),
                ),
            ]));
        }
    }
    rows
}

fn preview_rows(text: &str, width: u16) -> Vec<Line> {
    let max_width = usize::from(width.saturating_sub(4)).max(20);
    text.lines()
        .take(24)
        .map(|line| {
            Line::from_spans(vec![
                Span::styled("  │ ", muted()),
                Span::raw(truncate(line, max_width)),
            ])
        })
        .collect()
}

fn header(title: &str) -> Vec<Line> {
    vec![Line::from_spans(vec![
        Span::styled("◆ ", accent()),
        Span::styled(title.to_owned(), title_style()),
    ])]
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

fn value(payload: &Value, key: &str) -> Option<String> {
    payload.get(key).and_then(|value| {
        value
            .as_str()
            .map(ToOwned::to_owned)
            .or_else(|| {
                value
                    .as_bool()
                    .map(|value| if value { "yes" } else { "no" }.to_string())
            })
            .or_else(|| value.as_u64().map(|value| value.to_string()))
    })
}

fn array_text(payload: &Value, key: &str) -> Option<String> {
    payload.get(key).and_then(Value::as_array).map(|values| {
        values
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    })
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

const fn title_style() -> Style {
    Style::new().fg(Color::White)
}

const fn label() -> Style {
    Style::new().fg(Color::BrightBlack)
}

const fn value_style() -> Style {
    Style::new().fg(Color::White)
}

const fn muted() -> Style {
    Style::new().fg(Color::BrightBlack)
}
