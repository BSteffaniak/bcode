//! Native TUI rendering for question outcome artifacts.

use bmux_tui::prelude::Line;

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
        _context: bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        let Ok(outcome) = serde_json::from_value::<super::QuestionToolOutcome>(payload.clone())
        else {
            return vec![
                Line::from("Question outcome"),
                Line::from(payload.to_string()),
            ];
        };

        let mut rows = vec![Line::from(format!(
            "Question outcome · {:?}",
            outcome.status
        ))];
        for question in &outcome.questions {
            if let Some(header) = &question.header {
                rows.push(Line::from(header.clone()));
            }
            rows.push(Line::from(question.question.clone()));
            if question.selected.is_empty() {
                if let Some(custom) = &question.custom {
                    rows.push(Line::from(custom.clone()));
                }
            } else {
                rows.extend(
                    question
                        .selected
                        .iter()
                        .map(|answer| Line::from(format!("✓ {}", answer.label))),
                );
                if let Some(custom) = &question.custom {
                    rows.push(Line::from(format!("Custom answer: {custom}")));
                }
            }
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(100),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("Question outcome"), "{rendered}");
        assert!(rendered.contains("Choose one?"), "{rendered}");
        assert!(rendered.contains("✓ Yes"), "{rendered}");
    }
}
