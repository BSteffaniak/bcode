//! Native TUI rendering for Git tool visuals.

use bcode_tui_components::tool_card::{ToolCardStyle, tool_card_header_rows};
use bmux_tui::prelude::{Line, Span};
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

    fn rows(
        &self,
        kind: &str,
        payload: &Value,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        let width = context.width();
        let style = tool_card_style(context);
        match kind {
            "bcode.git.clone_request" => clone_request_rows(payload, width, context, style),
            "bcode.git.clone_result" => clone_result_rows(payload, width, context, style),
            _ => Vec::new(),
        }
    }
}

fn clone_request_rows(
    payload: &Value,
    width: u16,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    style: ToolCardStyle,
) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let metadata = [
        text(arguments, "url").map(|value| Span::styled(value.to_owned(), style.value)),
        text(arguments, "ref")
            .or_else(|| text(arguments, "branch"))
            .map(|value| Span::styled(value.to_owned(), style.value)),
        text(arguments, "destination")
            .map(|value| Span::styled(format!("→ {}", context.display_path(value)), style.value)),
    ]
    .into_iter()
    .flatten();
    tool_card_header_rows(
        Span::styled("◆ ", style.accent),
        Span::styled("Clone repository", style.title),
        metadata,
        width,
        style.muted,
    )
}

fn clone_result_rows(
    payload: &Value,
    width: u16,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    style: ToolCardStyle,
) -> Vec<Line> {
    let existed = payload.get("already_exists").and_then(Value::as_bool) == Some(true);
    let metadata = [
        repo_name(payload).map(|value| Span::styled(value, style.value)),
        text(payload, "path")
            .map(|value| Span::styled(format!("→ {}", context.display_path(value)), style.value)),
        text(payload, "git_ref").map(|value| Span::styled(value.to_owned(), style.value)),
        text(payload, "artifact_scope").map(|value| Span::styled(value.to_owned(), style.value)),
    ]
    .into_iter()
    .flatten();
    tool_card_header_rows(
        Span::styled(if existed { "◆ " } else { "✓ " }, style.accent),
        Span::styled(
            if existed {
                "Repository already exists"
            } else {
                "Repository cloned"
            },
            style.title,
        ),
        metadata,
        width,
        style.muted,
    )
}

fn repo_name(payload: &Value) -> Option<String> {
    let repo = text(payload, "repo")?;
    Some(text(payload, "owner").map_or_else(|| repo.to_owned(), |owner| format!("{owner}/{repo}")))
}

fn text<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
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
    fn renders_clone_request_from_generic_contribution_payload() {
        let payload = serde_json::json!({
            "url": "https://github.com/bmorphism/bcode",
            "ref": "main",
            "destination": "/tmp/bcode"
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &GitTuiVisualAdapter,
            "bcode.git.clone_request",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                80,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("Clone repository"), "{rendered}");
        assert!(
            rendered.contains("github.com/bmorphism/bcode"),
            "{rendered}"
        );
        assert!(rendered.contains("main"), "{rendered}");
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
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                80,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("Repository clone"), "{rendered}");
        assert!(rendered.contains("bmorphism/bcode"), "{rendered}");
    }
}
