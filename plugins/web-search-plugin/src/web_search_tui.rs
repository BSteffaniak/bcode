//! Native TUI rendering for web search and fetch visuals.

use bcode_tui_components::compact::truncate_width;
use bcode_tui_components::tool_card::{
    ToolCardStyle, push_tool_card_detail, tool_card_header, tool_card_header_rows,
};
use bmux_tui::prelude::{Line, Span};
use serde_json::Value;

/// Web search/fetch TUI visual adapter.
pub struct WebSearchTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for WebSearchTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        matches!(
            kind,
            "bcode.web-search.search_request"
                | "bcode.web-search.fetch_request"
                | "bcode.web-search.status_request"
                | "bcode.web-search.inspect_request"
                | "bcode.web-search.search_results"
                | "bcode.web-search.fetch_result"
                | "bcode.web-search.status"
                | "bcode.web-search.inspect_result"
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
            "bcode.web-search.search_request" => search_request_rows(payload, style),
            "bcode.web-search.fetch_request" => fetch_request_rows(payload, style),
            "bcode.web-search.status_request" => simple_request_rows("Web status", style),
            "bcode.web-search.inspect_request" => inspect_request_rows(payload, style),
            "bcode.web-search.search_results" => search_result_rows(payload, width, style),
            "bcode.web-search.fetch_result" => fetch_result_rows(payload, width, style),
            "bcode.web-search.status" => status_rows(payload, style),
            "bcode.web-search.inspect_result" => inspect_result_rows(payload, style),
            _ => Vec::new(),
        }
    }
}

fn search_request_rows(payload: &Value, style: ToolCardStyle) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let mut rows = header("Web search", style);
    push_kv(&mut rows, "query", text(arguments, "query"), style);
    push_kv(&mut rows, "provider", text(arguments, "provider"), style);
    push_kv(&mut rows, "site", text(arguments, "site"), style);
    push_kv(&mut rows, "freshness", text(arguments, "freshness"), style);
    push_kv(&mut rows, "region", text(arguments, "region"), style);
    push_kv(
        &mut rows,
        "safe search",
        text(arguments, "safe_search"),
        style,
    );
    push_kv(
        &mut rows,
        "max results",
        number(arguments, "max_results"),
        style,
    );
    push_kv(
        &mut rows,
        "provider options",
        compact_json(arguments, "provider_options"),
        style,
    );
    rows
}

fn fetch_request_rows(payload: &Value, style: ToolCardStyle) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let mut rows = header("Web fetch", style);
    push_kv(&mut rows, "url", text(arguments, "url"), style);
    push_kv(&mut rows, "rendered", bool_text(arguments, "render"), style);
    push_kv(&mut rows, "provider", text(arguments, "provider"), style);
    push_kv(
        &mut rows,
        "max bytes",
        number(arguments, "max_bytes"),
        style,
    );
    push_kv(&mut rows, "prompt", text(arguments, "prompt"), style);
    rows
}

fn simple_request_rows(title: &str, style: ToolCardStyle) -> Vec<Line> {
    header(title, style)
}

fn inspect_request_rows(payload: &Value, style: ToolCardStyle) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let mut rows = header("Inspect URL", style);
    push_kv(&mut rows, "url", text(arguments, "url"), style);
    rows
}

fn search_result_rows(payload: &Value, width: u16, style: ToolCardStyle) -> Vec<Line> {
    let result_count = payload
        .get("results")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let metadata = std::iter::once(
        text(payload, "query").map(|value| Span::styled(format!("“{value}”"), style.value)),
    )
    .flatten();
    let mut rows = tool_card_header_rows(
        Span::styled("◆ ", style.accent),
        Span::styled(format!("Search results ({result_count})"), style.title),
        metadata,
        width,
        style.muted,
    );
    rows.push(Line::raw(""));
    if let Some(results) = payload.get("results").and_then(Value::as_array) {
        for (index, result) in results.iter().take(10).enumerate() {
            let url = text(result, "url").unwrap_or_default();
            let host = url
                .split_once("://")
                .map_or(url, |(_, rest)| rest)
                .split('/')
                .next()
                .unwrap_or_default();
            rows.push(Line::from_spans(vec![
                Span::styled(format!("  {}  ", index + 1), style.accent),
                Span::styled(
                    text(result, "title").unwrap_or("Untitled").to_owned(),
                    style.title,
                ),
                Span::styled(format!(" · {host}"), style.muted),
            ]));
            if !url.is_empty() {
                rows.push(Line::from_spans(vec![
                    Span::styled("     ↳ ", style.muted),
                    Span::styled(
                        truncate_width(url, usize::from(width.saturating_sub(8))),
                        style.link,
                    ),
                ]));
            }
            if let Some(snippet) = text(result, "snippet") {
                rows.push(Line::from_spans(vec![
                    Span::styled("     │ ", style.muted),
                    Span::raw(truncate_width(
                        snippet,
                        usize::from(width.saturating_sub(8)),
                    )),
                ]));
            }
        }
        if results.len() > 10 {
            rows.push(Line::from_spans(vec![Span::styled(
                format!("  … {} more results", results.len() - 10),
                style.muted,
            )]));
        }
    }
    if text(payload, "provider").is_some()
        || payload.get("partial").and_then(Value::as_bool) == Some(true)
        || text(payload, "message").is_some()
    {
        rows.push(Line::raw(""));
        push_kv(&mut rows, "provider", text(payload, "provider"), style);
        if payload.get("partial").and_then(Value::as_bool) == Some(true) {
            push_kv(&mut rows, "partial", Some("yes"), style);
        }
        push_kv(&mut rows, "note", text(payload, "message"), style);
    }
    rows
}

fn fetch_result_rows(payload: &Value, width: u16, style: ToolCardStyle) -> Vec<Line> {
    let mut rows = header("Fetched page", style);
    push_kv(&mut rows, "title", text(payload, "title"), style);
    push_kv(
        &mut rows,
        "url",
        text(payload, "final_url").or_else(|| text(payload, "url")),
        style,
    );
    push_kv(&mut rows, "status", number(payload, "status"), style);
    push_kv(&mut rows, "type", text(payload, "content_type"), style);
    push_kv(&mut rows, "format", text(payload, "content_format"), style);
    push_kv(&mut rows, "rendered", bool_text(payload, "rendered"), style);
    push_kv(
        &mut rows,
        "truncated",
        bool_text(payload, "truncated"),
        style,
    );
    rows.push(Line::raw(""));
    if let Some(text) = text(payload, "markdown").or_else(|| text(payload, "text")) {
        rows.extend(preview_rows(text, width, style));
    }
    rows
}

fn status_rows(payload: &Value, style: ToolCardStyle) -> Vec<Line> {
    let mut rows = header("Web capabilities", style);
    if let Some(search) = payload.get("search") {
        rows.push(Line::from_spans(vec![Span::styled(
            "  Search",
            style.title,
        )]));
        push_kv(
            &mut rows,
            "available",
            bool_text(search, "available"),
            style,
        );
        push_kv(&mut rows, "provider", text(search, "provider"), style);
        push_kv(&mut rows, "quality", text(search, "quality"), style);
        push_kv(
            &mut rows,
            "configured",
            string_array(search, "configured_providers"),
            style,
        );
        push_kv(
            &mut rows,
            "recommended",
            string_array(search, "recommended"),
            style,
        );
    }
    if let Some(fetch) = payload.get("fetch") {
        rows.push(Line::raw(""));
        rows.push(Line::from_spans(vec![Span::styled("  Fetch", style.title)]));
        push_kv(&mut rows, "available", bool_text(fetch, "available"), style);
        push_kv(
            &mut rows,
            "fallbacks",
            string_array(fetch, "fallbacks"),
            style,
        );
        push_kv(
            &mut rows,
            "rendered fetch",
            bool_text(fetch, "rendered_fetch"),
            style,
        );
        push_kv(&mut rows, "max bytes", number(fetch, "max_bytes"), style);
    }
    rows
}

fn inspect_result_rows(payload: &Value, style: ToolCardStyle) -> Vec<Line> {
    let mut rows = header("URL inspection", style);
    push_kv(&mut rows, "url", text(payload, "url"), style);
    push_kv(&mut rows, "kind", text(payload, "kind"), style);
    push_kv(
        &mut rows,
        "recommended tool",
        text(payload, "recommended_tool"),
        style,
    );
    push_kv(
        &mut rows,
        "action",
        text(payload, "recommended_action"),
        style,
    );
    if let Some(notes) = payload.get("notes").and_then(Value::as_array) {
        rows.push(Line::raw(""));
        for note in notes.iter().filter_map(Value::as_str) {
            rows.push(Line::from_spans(vec![
                Span::styled("  • ", style.accent),
                Span::raw(note.to_owned()),
            ]));
        }
    }
    rows
}

fn preview_rows(text: &str, width: u16, style: ToolCardStyle) -> Vec<Line> {
    let max_width = usize::from(width.saturating_sub(4)).max(20);
    let mut rows = Vec::new();
    for line in text.lines().take(24) {
        rows.push(Line::from_spans(vec![
            Span::styled("  │ ", style.muted),
            Span::raw(truncate(line, max_width)),
        ]));
    }
    if text.lines().count() > 24 {
        rows.push(Line::from_spans(vec![Span::styled(
            "  … preview truncated",
            style.muted,
        )]));
    }
    rows
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

fn compact_json(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .filter(|value| !value.is_null())
        .and_then(|value| serde_json::to_string(value).ok())
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

fn string_array(payload: &Value, key: &str) -> Option<String> {
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
    fn renders_search_results() {
        let payload = serde_json::json!({
            "query": "rust tui",
            "provider": "test",
            "partial": false,
            "results": [{
                "title": "Ratatui",
                "url": "https://ratatui.rs",
                "snippet": "Build terminal user interfaces"
            }]
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &WebSearchTuiVisualAdapter,
            "bcode.web-search.search_results",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                80,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("Search results (1)"), "{rendered}");
        assert!(rendered.contains("Ratatui"), "{rendered}");
        assert!(rendered.contains("https://ratatui.rs"), "{rendered}");
    }
}
