//! Native TUI rendering for Git tool visuals.

use bmux_tui::prelude::{Color, Line, Span, Style};
use serde_json::Value;

/// Git TUI visual adapter.
pub struct GitTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for GitTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        matches!(kind, "bcode.git.clone_request" | "bcode.git.clone_result")
    }

    fn render_mode(
        &self,
        _kind: &str,
        _payload: &Value,
    ) -> bcode_plugin_sdk::tui::PluginTuiVisualRenderMode {
        bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::TranscriptBlock
    }

    fn rows(&self, kind: &str, payload: &Value, _width: u16) -> Vec<Line> {
        match kind {
            "bcode.git.clone_request" => clone_request_rows(payload),
            "bcode.git.clone_result" => clone_result_rows(payload),
            _ => Vec::new(),
        }
    }
}

fn clone_request_rows(payload: &Value) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let mut rows = header("Clone repository");
    push_kv(&mut rows, "url", text(arguments, "url"));
    push_kv(
        &mut rows,
        "ref",
        text(arguments, "ref").or_else(|| text(arguments, "branch")),
    );
    push_kv(&mut rows, "destination", text(arguments, "destination"));
    rows
}

fn clone_result_rows(payload: &Value) -> Vec<Line> {
    let mut rows = header("Repository clone");
    push_kv(&mut rows, "repo", repo_name(payload));
    push_kv(&mut rows, "host", text(payload, "host"));
    push_kv(
        &mut rows,
        "url",
        text(payload, "clone_url").or_else(|| text(payload, "url")),
    );
    push_kv(&mut rows, "ref", text(payload, "git_ref"));
    push_kv(&mut rows, "path", text(payload, "path"));
    push_kv(
        &mut rows,
        "already existed",
        bool_text(payload, "already_exists"),
    );
    push_kv(&mut rows, "scope", text(payload, "artifact_scope"));
    rows
}

fn repo_name(payload: &Value) -> Option<String> {
    let repo = text(payload, "repo")?;
    Some(text(payload, "owner").map_or_else(|| repo.to_owned(), |owner| format!("{owner}/{repo}")))
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

fn bool_text(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_bool)
        .map(|value| if value { "yes" } else { "no" }.to_owned())
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
    fn renders_clone_result() {
        let payload = serde_json::json!({
            "host": "github.com",
            "owner": "bmorphism",
            "repo": "bcode",
            "clone_url": "https://github.com/bmorphism/bcode.git",
            "path": "/tmp/bcode",
            "already_exists": false
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &GitTuiVisualAdapter,
            "bcode.git.clone_result",
            &payload,
            80,
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("Repository clone"), "{rendered}");
        assert!(rendered.contains("bmorphism/bcode"), "{rendered}");
    }
}
