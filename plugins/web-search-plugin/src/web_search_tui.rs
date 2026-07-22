//! Native TUI rendering for web search and fetch visuals.

use bcode_tui_components::compact::{header_rows, truncate_width};
use bmux_tui::prelude::{Color, Line, Span, Style};
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
        match kind {
            "bcode.web-search.search_request" => search_request_rows(payload),
            "bcode.web-search.fetch_request" => fetch_request_rows(payload),
            "bcode.web-search.status_request" => simple_request_rows("Web status"),
            "bcode.web-search.inspect_request" => inspect_request_rows(payload),
            "bcode.web-search.search_results" => search_result_rows(payload, width),
            "bcode.web-search.fetch_result" => fetch_result_rows(payload, width),
            "bcode.web-search.status" => status_rows(payload),
            "bcode.web-search.inspect_result" => inspect_result_rows(payload),
            _ => Vec::new(),
        }
    }
}

fn search_request_rows(payload: &Value) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let mut rows = header("Web search");
    push_kv(&mut rows, "query", text(arguments, "query"));
    push_kv(&mut rows, "provider", text(arguments, "provider"));
    push_kv(&mut rows, "site", text(arguments, "site"));
    push_kv(&mut rows, "freshness", text(arguments, "freshness"));
    push_kv(&mut rows, "region", text(arguments, "region"));
    push_kv(&mut rows, "safe search", text(arguments, "safe_search"));
    push_kv(&mut rows, "max results", number(arguments, "max_results"));
    rows
}

fn fetch_request_rows(payload: &Value) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let mut rows = header("Web fetch");
    push_kv(&mut rows, "url", text(arguments, "url"));
    push_kv(&mut rows, "rendered", bool_text(arguments, "render"));
    push_kv(&mut rows, "provider", text(arguments, "provider"));
    push_kv(&mut rows, "max bytes", number(arguments, "max_bytes"));
    push_kv(&mut rows, "prompt", text(arguments, "prompt"));
    rows
}

fn simple_request_rows(title: &str) -> Vec<Line> {
    header(title)
}

fn inspect_request_rows(payload: &Value) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let mut rows = header("Inspect URL");
    push_kv(&mut rows, "url", text(arguments, "url"));
    rows
}

fn search_result_rows(payload: &Value, width: u16) -> Vec<Line> {
    let result_count = payload
        .get("results")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let metadata = std::iter::once(
        text(payload, "query").map(|value| Span::styled(format!("“{value}”"), value_style())),
    )
    .flatten();
    let mut rows = header_rows(
        Span::styled("◆ ", accent()),
        Span::styled(format!("Search results ({result_count})"), title_style()),
        metadata,
        width,
        muted(),
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
                Span::styled(format!("  {}  ", index + 1), accent()),
                Span::styled(
                    text(result, "title").unwrap_or("Untitled").to_owned(),
                    title_style(),
                ),
                Span::styled(format!(" · {host}"), muted()),
            ]));
            if !url.is_empty() {
                rows.push(Line::from_spans(vec![
                    Span::styled("     ↳ ", muted()),
                    Span::styled(
                        truncate_width(url, usize::from(width.saturating_sub(8))),
                        url_style(),
                    ),
                ]));
            }
            if let Some(snippet) = text(result, "snippet") {
                rows.push(Line::from_spans(vec![
                    Span::styled("     │ ", muted()),
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
                muted(),
            )]));
        }
    }
    if text(payload, "provider").is_some()
        || payload.get("partial").and_then(Value::as_bool) == Some(true)
        || text(payload, "message").is_some()
    {
        rows.push(Line::raw(""));
        push_kv(&mut rows, "provider", text(payload, "provider"));
        if payload.get("partial").and_then(Value::as_bool) == Some(true) {
            push_kv(&mut rows, "partial", Some("yes"));
        }
        push_kv(&mut rows, "note", text(payload, "message"));
    }
    rows
}

fn fetch_result_rows(payload: &Value, width: u16) -> Vec<Line> {
    let mut rows = header("Fetched page");
    push_kv(&mut rows, "title", text(payload, "title"));
    push_kv(
        &mut rows,
        "url",
        text(payload, "final_url").or_else(|| text(payload, "url")),
    );
    push_kv(&mut rows, "status", number(payload, "status"));
    push_kv(&mut rows, "type", text(payload, "content_type"));
    push_kv(&mut rows, "format", text(payload, "content_format"));
    push_kv(&mut rows, "rendered", bool_text(payload, "rendered"));
    push_kv(&mut rows, "truncated", bool_text(payload, "truncated"));
    rows.push(Line::raw(""));
    if let Some(text) = text(payload, "markdown").or_else(|| text(payload, "text")) {
        rows.extend(preview_rows(text, width));
    }
    rows
}

fn status_rows(payload: &Value) -> Vec<Line> {
    let mut rows = header("Web capabilities");
    if let Some(search) = payload.get("search") {
        rows.push(Line::from_spans(vec![Span::styled(
            "  Search",
            title_style(),
        )]));
        push_kv(&mut rows, "available", bool_text(search, "available"));
        push_kv(&mut rows, "provider", text(search, "provider"));
        push_kv(&mut rows, "quality", text(search, "quality"));
        push_kv(
            &mut rows,
            "configured",
            string_array(search, "configured_providers"),
        );
        push_kv(
            &mut rows,
            "recommended",
            string_array(search, "recommended"),
        );
    }
    if let Some(fetch) = payload.get("fetch") {
        rows.push(Line::raw(""));
        rows.push(Line::from_spans(vec![Span::styled(
            "  Fetch",
            title_style(),
        )]));
        push_kv(&mut rows, "available", bool_text(fetch, "available"));
        push_kv(&mut rows, "fallbacks", string_array(fetch, "fallbacks"));
        push_kv(
            &mut rows,
            "rendered fetch",
            bool_text(fetch, "rendered_fetch"),
        );
        push_kv(&mut rows, "max bytes", number(fetch, "max_bytes"));
    }
    rows
}

fn inspect_result_rows(payload: &Value) -> Vec<Line> {
    let mut rows = header("URL inspection");
    push_kv(&mut rows, "url", text(payload, "url"));
    push_kv(&mut rows, "kind", text(payload, "kind"));
    push_kv(
        &mut rows,
        "recommended tool",
        text(payload, "recommended_tool"),
    );
    push_kv(&mut rows, "action", text(payload, "recommended_action"));
    if let Some(notes) = payload.get("notes").and_then(Value::as_array) {
        rows.push(Line::raw(""));
        for note in notes.iter().filter_map(Value::as_str) {
            rows.push(Line::from_spans(vec![
                Span::styled("  • ", accent()),
                Span::raw(note.to_owned()),
            ]));
        }
    }
    rows
}

fn preview_rows(text: &str, width: u16) -> Vec<Line> {
    let max_width = usize::from(width.saturating_sub(4)).max(20);
    let mut rows = Vec::new();
    for line in text.lines().take(24) {
        rows.push(Line::from_spans(vec![
            Span::styled("  │ ", muted()),
            Span::raw(truncate(line, max_width)),
        ]));
    }
    if text.lines().count() > 24 {
        rows.push(Line::from_spans(vec![Span::styled(
            "  … preview truncated",
            muted(),
        )]));
    }
    rows
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

const fn url_style() -> Style {
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
