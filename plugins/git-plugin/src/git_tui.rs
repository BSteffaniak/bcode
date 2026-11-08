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

    fn tool_presentation(
        &self,
        kind: &str,
        payload: &Value,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> bcode_plugin_sdk::tui::PluginTuiToolPresentation {
        clone_presentation(kind, payload, show_destination(context), &|path| {
            context.display_path(path).to_string()
        })
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
            .filter(|_| show_destination(context))
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
            .filter(|_| show_destination(context))
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

/// Summary and target of a clone visual (plugin contract v3): the repository
/// as target, with the destination or outcome as detail, for example
/// `bmorphism/bcode · cloned → bcode`.
fn clone_presentation(
    kind: &str,
    payload: &Value,
    show_destination: bool,
    display_path: &dyn Fn(&str) -> String,
) -> bcode_plugin_sdk::tui::PluginTuiToolPresentation {
    let display_path = |path| show_destination.then(|| display_path(path));
    let (target, detail) = match kind {
        "bcode.git.clone_request" => {
            let arguments = payload.get("arguments").unwrap_or(payload);
            (
                text(arguments, "url").map(str::to_owned),
                text(arguments, "destination")
                    .and_then(display_path)
                    .map(|path| format!("→ {path}")),
            )
        }
        "bcode.git.clone_result" => {
            let existed = payload.get("already_exists").and_then(Value::as_bool) == Some(true);
            let outcome = if existed { "already exists" } else { "cloned" };
            (
                repo_name(payload).or_else(|| text(payload, "clone_url").map(str::to_owned)),
                Some(
                    text(payload, "path")
                        .and_then(display_path)
                        .map_or_else(|| outcome.to_owned(), |path| format!("{outcome} → {path}")),
                ),
            )
        }
        _ => (None, None),
    };
    let summary = match (&target, detail) {
        (Some(target), Some(detail)) => Some(format!("{target} · {detail}")),
        (Some(target), None) => Some(target.clone()),
        (None, detail) => detail,
    };
    bcode_plugin_sdk::tui::PluginTuiToolPresentation {
        summary,
        target,
        sections: Vec::new(),
    }
}

/// Property name of the destination toggle declared in the plugin manifest.
const SHOW_DESTINATION: &str = "bcode.git.show_destination";

/// The resolved `bcode.git.show_destination` presentation property; `true`
/// (its declared default) when the host supplies no presentation.
fn show_destination(context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext) -> bool {
    context
        .presentation()
        .and_then(|presentation| presentation.property(SHOW_DESTINATION))
        .is_none_or(|value| *value != bcode_plugin_sdk::tui_visual::FactValue::Bool(false))
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
    fn clone_presentations_name_repository_and_outcome() {
        let display = |path: &str| path.strip_prefix("/tmp/").unwrap_or(path).to_owned();
        let request = clone_presentation(
            "bcode.git.clone_request",
            &serde_json::json!({ "url": "https://github.com/o/r", "destination": "/tmp/r" }),
            true,
            &display,
        );
        assert_eq!(request.target.as_deref(), Some("https://github.com/o/r"));
        assert_eq!(
            request.summary.as_deref(),
            Some("https://github.com/o/r · → r")
        );
        let result = clone_presentation(
            "bcode.git.clone_result",
            &serde_json::json!({
                "owner": "o", "repo": "r", "path": "/tmp/r", "already_exists": true
            }),
            true,
            &display,
        );
        assert_eq!(result.target.as_deref(), Some("o/r"));
        assert_eq!(result.summary.as_deref(), Some("o/r · already exists → r"));
        assert!(clone_presentation("other", &serde_json::json!({}), true, &display).is_empty());
    }

    #[test]
    fn show_destination_property_hides_destinations() {
        let payload = serde_json::json!({
            "url": "https://github.com/o/r",
            "destination": "/tmp/clone-target"
        });
        let context = |presentation: serde_json::Value| {
            bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                80,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            )
            .with_presentation(serde_json::from_value(presentation).expect("presentation"))
        };
        let render = |context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext| {
            let adapter = &GitTuiVisualAdapter;
            let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
                adapter,
                "bcode.git.clone_request",
                &payload,
                context,
            );
            let summary = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::tool_presentation(
                adapter,
                "bcode.git.clone_request",
                &payload,
                context,
            )
            .summary
            .unwrap_or_default();
            format!(
                "{}\n{summary}",
                rows.iter().map(line_text).collect::<Vec<_>>().join("\n")
            )
        };
        let preview = serde_json::json!({ "lines": 8, "keep": "tail" });
        let shown = render(&context(serde_json::json!({
            "disclosure": "expanded", "preview": preview,
        })));
        assert!(shown.contains("clone-target"), "{shown}");
        let hidden = render(&context(serde_json::json!({
            "disclosure": "expanded",
            "preview": preview,
            "properties": { "bcode.git.show_destination": false },
        })));
        assert!(!hidden.contains("clone-target"), "{hidden}");
        assert!(hidden.contains("github.com/o/r"), "{hidden}");
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
