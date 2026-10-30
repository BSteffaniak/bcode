//! Portable BMUX rendering for resolved question artifacts.
use bcode_tui_components::tool_card::{ToolCardStyle, push_tool_card_detail, tool_card_header};
use bmux_tui::prelude::{Line, Span};
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
