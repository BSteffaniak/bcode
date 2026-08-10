//! Native TUI rendering for question outcome artifacts.

use bcode_tui_components::tool_card::{ToolCardStyle, push_tool_card_detail, tool_card_header};
use bmux_tui::prelude::{Line, Span};

/// Native TUI visual adapter for question outcome artifacts.
pub struct QuestionOutcomeTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for QuestionOutcomeTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        kind == "bcode.question.outcome"
    }

    fn rows(
        &self,
        _kind: &str,
        payload: &serde_json::Value,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        let style = tool_card_style(context);
        let Ok(outcome) = serde_json::from_value::<super::QuestionToolOutcome>(payload.clone())
        else {
            return vec![
                tool_card_header(
                    Span::styled("◆ ", style.accent),
                    Span::styled("Question outcome", style.title),
                ),
                Line::from_spans(vec![Span::styled(payload.to_string(), style.value)]),
            ];
        };

        let mut rows = vec![tool_card_header(
            Span::styled("◆ ", style.accent),
            Span::styled("Question outcome", style.title),
        )];
        push_tool_card_detail(
            &mut rows,
            "status",
            Some(&format!("{:?}", outcome.status)),
            style.muted,
            style.value,
        );
        for question in &outcome.questions {
            rows.push(Line::raw(""));
            if let Some(header) = &question.header {
                rows.push(Line::from_spans(vec![Span::styled(
                    header.clone(),
                    style.title,
                )]));
            }
            rows.push(Line::from_spans(vec![Span::styled(
                question.question.clone(),
                style.value,
            )]));
            if question.selected.is_empty() {
                if let Some(custom) = &question.custom {
                    push_tool_card_detail(
                        &mut rows,
                        "answer",
                        Some(custom),
                        style.muted,
                        style.value,
                    );
                }
            } else {
                rows.extend(question.selected.iter().map(|answer| {
                    Line::from_spans(vec![
                        Span::styled("  ✓ ", style.accent),
                        Span::styled(answer.label.clone(), style.value),
                    ])
                }));
                if let Some(custom) = &question.custom {
                    push_tool_card_detail(
                        &mut rows,
                        "custom answer",
                        Some(custom),
                        style.muted,
                        style.value,
                    );
                }
            }
        }
        rows
    }
}

fn tool_card_style(context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext) -> ToolCardStyle {
    ToolCardStyle::from_component_theme(context.theme().and_then(|theme| theme.component_theme()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_plugin_sdk::tui::PluginTuiSyntaxColor;
    use bmux_tui::style::{Color, Style};

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn adapter_renders_question_outcome_from_raw_artifact_metadata() {
        let payload = serde_json::json!({
            "status": "answered",
            "questions": [{
                "question_index": 0,
                "header": "Header",
                "question": "Choose one?",
                "status": "answered",
                "selected": [{"label": "Yes", "value": "yes"}],
                "custom": "because",
                "required": true
            }]
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &QuestionOutcomeTuiVisualAdapter,
            "bcode.question.outcome",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                100,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("Question outcome"), "{rendered}");
        assert!(rendered.contains("Choose one?"), "{rendered}");
        assert!(rendered.contains("✓ Yes"), "{rendered}");
    }

    #[test]
    fn adapter_uses_renderer_owned_component_theme() {
        let theme = bcode_plugin_sdk::tui::PluginTuiTheme {
            component_theme_version: bcode_plugin_sdk::tui::PLUGIN_TUI_COMPONENT_THEME_VERSION,
            canvas: Style::new(),
            text: Style::new().fg(Color::Green),
            muted: Style::new().fg(Color::BrightBlack),
            border: Style::new(),
            focused: Style::new().fg(Color::Magenta),
            selection: Style::new(),
            source: bcode_plugin_sdk::tui::PluginTuiSourceTheme {
                source: Style::new(),
                border: Style::new(),
                gutter: Style::new(),
                truncated: Style::new(),
            },
            diff: bcode_plugin_sdk::tui::PluginTuiDiffTheme {
                text: Style::new(),
                muted: Style::new(),
                title: Style::new(),
                label: Style::new(),
                added: Style::new(),
                removed: Style::new(),
                hunk: Style::new(),
                added_row: Style::new(),
                removed_row: Style::new(),
                added_emphasis: Style::new(),
                removed_emphasis: Style::new(),
            },
            syntax: bcode_plugin_sdk::tui::PluginTuiSyntaxTheme {
                text: PluginTuiSyntaxColor::from_tui(Color::Default),
                comment: PluginTuiSyntaxColor::from_tui(Color::Default),
                keyword: PluginTuiSyntaxColor::from_tui(Color::Default),
                function: PluginTuiSyntaxColor::from_tui(Color::Default),
                variable: PluginTuiSyntaxColor::from_tui(Color::Default),
                string: PluginTuiSyntaxColor::from_tui(Color::Default),
                number: PluginTuiSyntaxColor::from_tui(Color::Default),
                type_name: PluginTuiSyntaxColor::from_tui(Color::Default),
                operator: PluginTuiSyntaxColor::from_tui(Color::Default),
                punctuation: PluginTuiSyntaxColor::from_tui(Color::Default),
            },
        };
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &QuestionOutcomeTuiVisualAdapter,
            "bcode.question.outcome",
            &serde_json::json!({"status": "dismissed", "questions": []}),
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                80,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            )
            .with_theme(theme),
        );

        assert_eq!(rows[0].spans[0].style.fg, Some(Color::Magenta));
        assert_eq!(rows[0].spans[1].style.fg, Some(Color::Green));
    }
}
