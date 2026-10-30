//! Portable BMUX rendering for resolved question artifacts.
use bcode_tui_components::tool_card::{ToolCardStyle, push_tool_card_detail, tool_card_header};
use bcode_tui_components::transcript::tool_visual::{
    CanonicalPluginVisual,
    portable::{
        PortableVisualAdapter, PortableVisualContext, PortableVisualDescriptor,
        PortableVisualOutput, PortableVisualRegistration,
    },
};
use bmux_tui::prelude::{Line, Span};
use serde::Deserialize;
use std::collections::BTreeSet;

/// Producer-owned outcome adapters available to statically linked BMUX hosts.
/// Registration grants no host, service, or filesystem capabilities.
#[must_use]
pub fn portable_visual_registrations() -> Vec<PortableVisualRegistration> {
    vec![PortableVisualRegistration {
        descriptor: PortableVisualDescriptor {
            plugin_id: "bcode.question".into(),
            adapter_id: "question-outcome-component".into(),
            schemas: BTreeSet::from([("bcode.question.outcome".into(), 1)]),
            priority: 100,
            producer_default: true,
        },
        adapter: Box::new(QuestionOutcomePortableAdapter),
    }]
}

struct QuestionOutcomePortableAdapter;
impl PortableVisualAdapter for QuestionOutcomePortableAdapter {
    fn render(
        &self,
        visual: &CanonicalPluginVisual,
        context: &PortableVisualContext<'_>,
    ) -> Option<PortableVisualOutput> {
        if visual.schema != "bcode.question.outcome"
            || visual.schema_version != 1
            || visual.streaming
            || context.width == 0
        {
            return None;
        }
        // Bound domain collections before deserialization and row construction.
        let questions = visual.payload.get("questions")?.as_array()?;
        if questions.len() > crate::MAX_QUESTIONS
            || questions.iter().any(|question| {
                let selected = question
                    .get("selected")
                    .and_then(serde_json::Value::as_array);
                question
                    .get("question")
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(|text| text.len() > crate::MAX_QUESTION_TEXT_BYTES)
                    || question
                        .get("header")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|text| text.len() > crate::MAX_QUESTION_TEXT_BYTES)
                    || question
                        .get("custom")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|text| text.len() > crate::MAX_CUSTOM_ANSWER_BYTES)
                    || selected.is_none_or(|selected| {
                        selected.len() > crate::MAX_OPTIONS_PER_QUESTION
                            || selected.iter().any(|answer| {
                                ["label", "value"].into_iter().any(|key| {
                                    answer
                                        .get(key)
                                        .and_then(serde_json::Value::as_str)
                                        .is_some_and(|text| {
                                            text.len() > crate::MAX_OPTION_TEXT_BYTES
                                        })
                                })
                            })
                    })
            })
        {
            return None;
        }
        let outcome = crate::QuestionToolOutcome::deserialize(&visual.payload).ok()?;
        Some(PortableVisualOutput {
            rows: question_outcome_rows(
                &outcome,
                ToolCardStyle::from_component_theme(context.theme.copied()),
            )
            .into(),
            ..PortableVisualOutput::default()
        })
    }
}

/// Render a producer-owned question outcome using shared native/browser content and styling.
#[must_use]
pub fn question_outcome_rows(
    outcome: &crate::QuestionToolOutcome,
    style: ToolCardStyle,
) -> Vec<Line> {
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
                push_tool_card_detail(&mut rows, "answer", Some(custom), style.muted, style.value);
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

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_tui_components::transcript::{
        tool_visual::portable::{PortableVisualPolicy, PortableVisualRegistry},
        transcript_layout::TranscriptLayoutRows,
    };
    use bmux_tui::style::{Color, Style};

    fn visual() -> CanonicalPluginVisual {
        let request = crate::parse_question_request(serde_json::json!({"questions":[{
            "question":"説明 👩‍💻", "options":["Yes"], "required":true
        }]}))
        .unwrap();
        CanonicalPluginVisual {
            invocation_id: Some("question-call".into()),
            revision: 4,
            producer_plugin_id: Some("bcode.question".into()),
            schema: "bcode.question.outcome".into(),
            schema_version: 1,
            title: None,
            subtitle: None,
            streaming: false,
            payload: serde_json::to_value(crate::dismissed_outcome(&request)).unwrap(),
        }
    }

    #[test]
    fn registration_routes_real_outcome_and_preserves_shared_rows_and_theme() {
        let registry = PortableVisualRegistry::new(
            5,
            PortableVisualPolicy {
                enabled_plugins: BTreeSet::from(["bcode.question".into()]),
                ..PortableVisualPolicy::default()
            },
            portable_visual_registrations(),
        )
        .unwrap();
        let visual = visual();
        let theme = bmux_tui_components::theme::ComponentTheme {
            focused: Style::new().fg(Color::Magenta),
            text: Style::new().fg(Color::Green),
            ..Default::default()
        };
        let context = PortableVisualContext {
            width: 60,
            theme: Some(&theme),
        };
        let prepared = registry
            .prepare(&visual, &context)
            .expect("producer adapter selected");
        assert_eq!(
            prepared.adapter_reference(),
            "bcode.question/question-outcome-component"
        );
        let output = registry.resolve(prepared, &visual, context.width).unwrap();
        let TranscriptLayoutRows::Rendered(rows) = output.rows else {
            panic!("shared outcome rows");
        };
        let outcome = crate::QuestionToolOutcome::deserialize(&visual.payload).unwrap();
        assert_eq!(
            rows,
            question_outcome_rows(&outcome, ToolCardStyle::from_component_theme(Some(theme)))
        );
        assert_eq!(rows[0].spans[0].style.fg, Some(Color::Magenta));
        assert_eq!(rows[0].spans[1].style.fg, Some(Color::Green));
        assert!(output.actions.is_empty());
        assert!(output.images.is_empty());
    }

    #[test]
    fn adapter_rejects_incompatible_malformed_and_unbounded_domain_payloads() {
        let adapter = QuestionOutcomePortableAdapter;
        let context = PortableVisualContext {
            width: 60,
            theme: None,
        };
        let visual = visual();
        assert!(adapter.render(&visual, &context).is_some());
        for bad in [
            CanonicalPluginVisual {
                schema_version: 2,
                ..visual.clone()
            },
            CanonicalPluginVisual {
                schema: "other".into(),
                ..visual.clone()
            },
            CanonicalPluginVisual {
                streaming: true,
                ..visual.clone()
            },
            CanonicalPluginVisual {
                payload: serde_json::Value::Null,
                ..visual.clone()
            },
        ] {
            assert!(adapter.render(&bad, &context).is_none());
        }
        let mut oversized = visual;
        oversized.payload["questions"][0]["custom"] =
            serde_json::Value::String("x".repeat(crate::MAX_CUSTOM_ANSWER_BYTES + 1));
        assert!(adapter.render(&oversized, &context).is_none());
    }
}
