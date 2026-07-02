//! Native TUI rendering for question outcome artifacts.

use bmux_tui::prelude::Line;
use serde_json::Value;

/// Native TUI visual adapter for question outcome artifacts.
pub struct QuestionOutcomeTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for QuestionOutcomeTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        kind == "bcode.question.outcome"
    }

    fn rows(&self, _kind: &str, payload: &serde_json::Value, _width: u16) -> Vec<Line> {
        let Ok(outcome) = serde_json::from_value::<super::QuestionToolOutcome>(payload.clone())
        else {
            return vec![
                Line::from("Question outcome"),
                Line::from(payload.to_string()),
            ];
        };
        let tree = super::question_result_component_tree(&outcome);
        let mut text = Vec::new();
        collect_protocol_text(&tree, &mut text);

        let mut rows = vec![Line::from(format!(
            "Question outcome · {:?}",
            outcome.status
        ))];
        rows.extend(
            text.into_iter()
                .filter(|line| !line.trim().is_empty())
                .map(Line::from),
        );
        rows
    }
}

fn collect_protocol_text(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for key in ["text", "value", "label"] {
                if let Some(text) = object.get(key).and_then(Value::as_str) {
                    output.extend(text.lines().map(str::to_owned));
                }
            }
            for value in object.values() {
                collect_protocol_text(value, output);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_protocol_text(value, output);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
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
            100,
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("Question outcome"), "{rendered}");
        assert!(rendered.contains("Choose one?"), "{rendered}");
        assert!(rendered.contains("✓ Yes"), "{rendered}");
    }
}
