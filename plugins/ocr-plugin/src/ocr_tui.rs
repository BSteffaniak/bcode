//! Native TUI rendering for OCR tool visuals.

use bmux_tui::prelude::{Color, Line, Span, Style};
use serde_json::Value;

/// OCR TUI visual adapter.
pub struct OcrTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for OcrTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        matches!(
            kind,
            "bcode.ocr.request" | "bcode.ocr.extract_result" | "bcode.ocr.status"
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
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        let width = context.width();
        match kind {
            "bcode.ocr.request" => request_rows(payload),
            "bcode.ocr.extract_result" => extract_rows(payload, width),
            "bcode.ocr.status" => status_rows(payload),
            _ => Vec::new(),
        }
    }
}

fn request_rows(payload: &Value) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let mut rows = header("OCR request");
    for key in [
        "operation",
        "path",
        "url",
        "language",
        "engine",
        "max_bytes",
    ] {
        push_kv(&mut rows, key, value(arguments, key));
    }
    rows
}

fn extract_rows(payload: &Value, width: u16) -> Vec<Line> {
    let mut rows = header("OCR text");
    push_kv(&mut rows, "source", source_text(payload));
    push_kv(&mut rows, "engine", text(payload, "engine"));
    push_kv(&mut rows, "language", text(payload, "language"));
    push_kv(&mut rows, "bytes", byte_summary(payload));
    push_kv(&mut rows, "truncated", value(payload, "truncated"));
    if let Some(text) = text(payload, "text") {
        rows.push(Line::raw(""));
        rows.extend(preview_rows(text, width));
    }
    rows
}

fn status_rows(payload: &Value) -> Vec<Line> {
    let mut rows = header("OCR status");
    let Some(extract) = payload.get("extract") else {
        return rows;
    };
    push_kv(&mut rows, "available", value(extract, "available"));
    push_kv(&mut rows, "default engine", text(extract, "default_engine"));
    if let Some(engines) = extract.get("engines").and_then(Value::as_array) {
        rows.push(Line::raw(""));
        for engine in engines {
            rows.push(Line::from_spans(vec![
                Span::styled("  ◆ ", accent()),
                Span::styled(
                    text(engine, "name").unwrap_or("engine").to_owned(),
                    title_style(),
                ),
                Span::styled(
                    format!(
                        "  {}  {}",
                        value(engine, "available").unwrap_or_else(|| "unknown".to_string()),
                        text(engine, "quality").unwrap_or_default()
                    ),
                    muted(),
                ),
            ]));
            push_kv(&mut rows, "version", text(engine, "version"));
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

fn source_text(payload: &Value) -> Option<String> {
    let source = payload.get("source")?;
    text(source, "url")
        .or_else(|| text(source, "path"))
        .map(ToOwned::to_owned)
}

fn byte_summary(payload: &Value) -> Option<String> {
    let text_bytes = payload.get("text_bytes").and_then(Value::as_u64)?;
    let full_text_bytes = payload.get("full_text_bytes").and_then(Value::as_u64)?;
    Some(format!("{text_bytes} of {full_text_bytes}"))
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
