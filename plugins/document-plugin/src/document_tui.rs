//! Native TUI rendering for document extraction visuals.

use bcode_tui_components::tool_card::{
    ToolCardStyle, push_tool_card_detail, tool_card_header, tool_card_header_rows,
};
use bmux_tui::prelude::{Line, Span};
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
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        let width = context.width();
        let style = tool_card_style(context);
        match kind {
            "bcode.document.request" => request_rows(payload, context, style),
            "bcode.document.extract_result" => extract_rows(payload, width, style),
            "bcode.document.status" => status_rows(payload, style),
            _ => Vec::new(),
        }
    }
}

fn request_rows(
    payload: &Value,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    style: ToolCardStyle,
) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let mut rows = header("Document request", style);
    push_kv(&mut rows, "operation", value(arguments, "operation"), style);
    push_kv(
        &mut rows,
        "path",
        text(arguments, "path").map(|path| context.display_path(path).to_string()),
        style,
    );
    for key in ["url", "max_bytes", "timeout_ms"] {
        push_kv(&mut rows, key, value(arguments, key), style);
    }
    rows
}

fn extract_rows(payload: &Value, width: u16, style: ToolCardStyle) -> Vec<Line> {
    let metadata = [
        text(payload, "source").map(|value| Span::styled(value.to_owned(), style.value)),
        text(payload, "content_type").map(|value| Span::styled(value.to_owned(), style.muted)),
        text(payload, "extractor").map(|value| Span::styled(value.to_owned(), style.muted)),
    ]
    .into_iter()
    .flatten();
    let mut rows = tool_card_header_rows(
        Span::styled("◆ ", style.accent),
        Span::styled("Document extraction", style.title),
        metadata,
        width,
        style.muted,
    );
    if text(payload, "fallback_used").is_some_and(|value| value != "false") {
        push_kv(&mut rows, "fallback", text(payload, "fallback_used"), style);
    }
    if payload.get("truncated").and_then(Value::as_bool) == Some(true) {
        push_kv(&mut rows, "truncated", Some("yes"), style);
    }
    if let Some(text) = text(payload, "text") {
        rows.push(Line::raw(""));
        rows.extend(preview_rows(text, width, style));
    }
    rows
}

fn status_rows(payload: &Value, style: ToolCardStyle) -> Vec<Line> {
    let mut rows = header("Document extractors", style);
    let Some(extract) = payload.get("extract") else {
        return rows;
    };
    push_kv(&mut rows, "available", value(extract, "available"), style);
    push_kv(
        &mut rows,
        "order",
        array_text(extract, "configured_order"),
        style,
    );
    if let Some(extractors) = extract.get("extractors").and_then(Value::as_array) {
        rows.push(Line::raw(""));
        for extractor in extractors {
            rows.push(Line::from_spans(vec![
                Span::styled("  ◆ ", style.accent),
                Span::styled(
                    text(extractor, "name").unwrap_or("extractor").to_owned(),
                    style.title,
                ),
                Span::styled(
                    format!(
                        "  {}  {}",
                        value(extractor, "available").unwrap_or_else(|| "unknown".to_string()),
                        text(extractor, "quality").unwrap_or_default()
                    ),
                    style.muted,
                ),
            ]));
        }
    }
    rows
}

fn preview_rows(text: &str, width: u16, style: ToolCardStyle) -> Vec<Line> {
    let max_width = usize::from(width.saturating_sub(4)).max(20);
    text.lines()
        .take(24)
        .map(|line| {
            Line::from_spans(vec![
                Span::styled("  │ ", style.muted),
                Span::raw(truncate(line, max_width)),
            ])
        })
        .collect()
}

fn header(title: &str, style: ToolCardStyle) -> Vec<Line> {
    vec![tool_card_header(
        Span::styled("◆ ", style.accent),
        Span::styled(title.to_owned(), style.title),
    )]
}

fn push_kv<T>(rows: &mut Vec<Line>, key: &str, value: Option<T>, style: ToolCardStyle)
where
    T: Into<String>,
{
    if let Some(value) = value.map(Into::into) {
        push_tool_card_detail(rows, key, Some(&value), style.muted, style.value);
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

fn tool_card_style(context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext) -> ToolCardStyle {
    ToolCardStyle::from_component_theme(context.theme().and_then(|theme| theme.component_theme()))
}
