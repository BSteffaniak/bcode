//! Permission and interactive-tool presentation.

use super::theme::{accent, color, radius, space, surface, typeface};
use crate::context::{PresentationAction, PresentationContext};
use bcode_session_view_models::InteractionViewSummary;
use hyperchad::template::{Containers, container};
use serde::Deserialize;

use super::adapters::json_panel;
use super::components::{StatusTone, interaction_card};
use super::semantic_dom_id;

#[derive(Debug, Deserialize)]
struct QuestionSnapshot {
    request: QuestionRequest,
    validation_error: Option<String>,
    answers: Vec<QuestionAnswer>,
}

#[derive(Debug, Deserialize)]
struct QuestionRequest {
    questions: Vec<Question>,
}

#[derive(Debug, Deserialize)]
struct Question {
    header: Option<String>,
    #[serde(rename = "question")]
    text: String,
    options: Vec<QuestionOption>,
    control: QuestionControl,
    selection_mode: QuestionSelectionMode,
    custom: bool,
    custom_mode: QuestionCustomMode,
    required: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QuestionControl {
    Radio,
    Checkbox,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QuestionSelectionMode {
    Single,
    Multiple,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QuestionCustomMode {
    Exclusive,
    Additional,
}

#[derive(Debug, Deserialize)]
struct QuestionOption {
    label: String,
    value: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QuestionAnswer {
    question_index: usize,
    selected: Vec<String>,
    custom: Option<String>,
}

fn effective_interaction_state(
    interaction: &InteractionViewSummary,
) -> bcode_session_view_models::InteractionViewState {
    if interaction.resolved
        && interaction.state == bcode_session_view_models::InteractionViewState::Pending
    {
        let cancelled = interaction
            .resolution
            .as_ref()
            .and_then(|resolution| resolution.get("status"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|status| matches!(status, "cancelled" | "dismissed"));
        if cancelled {
            bcode_session_view_models::InteractionViewState::Cancelled
        } else {
            bcode_session_view_models::InteractionViewState::Resolved
        }
    } else if interaction.state == bcode_session_view_models::InteractionViewState::Pending
        && interaction
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.get("validation_error"))
            .is_some_and(|error| !error.is_null())
    {
        bcode_session_view_models::InteractionViewState::ValidationError
    } else {
        interaction.state
    }
}

pub(super) fn interaction_request(
    interaction: &InteractionViewSummary,
    session_id: Option<bcode_session_models::SessionId>,
    context: &impl PresentationContext,
) -> Containers {
    let item_id = semantic_dom_id("interaction", &interaction.interaction_id);
    let state = effective_interaction_state(interaction);
    let (status, status_detail) = match state {
        bcode_session_view_models::InteractionViewState::Pending => ("pending response", None),
        bcode_session_view_models::InteractionViewState::Submitting => {
            ("submitting", interaction.status_detail.as_deref())
        }
        bcode_session_view_models::InteractionViewState::ValidationError => {
            ("validation error", interaction.status_detail.as_deref())
        }
        bcode_session_view_models::InteractionViewState::ActionError => {
            ("action error", interaction.status_detail.as_deref())
        }
        bcode_session_view_models::InteractionViewState::Resolved => {
            ("resolved", interaction.status_detail.as_deref())
        }
        bcode_session_view_models::InteractionViewState::Cancelled => {
            ("cancelled", interaction.status_detail.as_deref())
        }
    };
    let status_color = match state {
        bcode_session_view_models::InteractionViewState::Resolved => color::SUCCESS,
        bcode_session_view_models::InteractionViewState::ValidationError
        | bcode_session_view_models::InteractionViewState::ActionError
        | bcode_session_view_models::InteractionViewState::Cancelled => color::ERROR,
        bcode_session_view_models::InteractionViewState::Pending
        | bcode_session_view_models::InteractionViewState::Submitting => color::WARNING,
    };
    let tone = match state {
        bcode_session_view_models::InteractionViewState::Resolved => StatusTone::Success,
        bcode_session_view_models::InteractionViewState::ValidationError
        | bcode_session_view_models::InteractionViewState::ActionError
        | bcode_session_view_models::InteractionViewState::Cancelled => StatusTone::Error,
        bcode_session_view_models::InteractionViewState::Pending
        | bcode_session_view_models::InteractionViewState::Submitting => StatusTone::Warning,
    };
    let body = container! {
        @if let Some(detail) = status_detail {
            div color=(status_color) font-size=((typeface::DETAIL)) margin-bottom=((space::S6)) { (detail) }
        }
        @if interaction.resolved {
            @if let Some(resolution) = &interaction.resolution {
                (json_panel("resolution", resolution))
            }
        } @else {
            @if interaction.kind == "bcode.question" {
                @if let Some(snapshot) = interaction.snapshot.as_ref().and_then(|value| serde_json::from_value::<QuestionSnapshot>(value.clone()).ok()) {
                    (question_interaction(&snapshot, interaction, session_id, context))
                } @else if let Some(snapshot) = &interaction.snapshot {
                    (json_panel("controller snapshot", snapshot))
                }
            } @else if let Some(snapshot) = &interaction.snapshot {
                (json_panel("controller snapshot", snapshot))
            }
            @if let Some(session_id) = session_id {
                (generic_interaction_controls(interaction, session_id, context))
            }
        }
    };
    let mut card = interaction_card(
        interaction
            .title
            .as_deref()
            .unwrap_or("Interactive request"),
        status,
        tone,
        &body,
    );
    if let Some(container) = card.first_mut() {
        container.str_id = Some(item_id);
    }
    card
}

fn generic_interaction_controls(
    interaction: &InteractionViewSummary,
    session_id: bcode_session_models::SessionId,
    context: &impl PresentationContext,
) -> Containers {
    container! {
        details margin-top=((space::S10)) {
            summary color=(color::MUTED) { "generic semantic controls" }
            form hx-post=(context.action_target(PresentationAction::ResolveInteraction)) hx-target="#bcode-web-shell" hx-swap=this margin-top=((space::SM)) {
                input type=hidden name="session_id" value=(session_id.to_string());
                input type=hidden name="interaction_id" value=(interaction.interaction_id.clone());
                div gap=((space::SM)) {
                    div #generic-interaction-kind-label color=(color::MUTED) font-size=((typeface::DETAIL)) { "Interaction operation" }
                    select #generic-interaction-kind name="kind" selected="submit" data-label-id="generic-interaction-kind-label" padding=((space::SM)) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) color=(color::TEXT) {
                        option value="activate" { "activate control" }
                        option value="change" { "change control value" }
                        option value="focus" { "focus control" }
                        option value="blur" { "blur control" }
                        option value="navigate" { "navigate focus" }
                        option value="submit" { "submit interaction" }
                        option value="cancel" { "cancel interaction" }
                    }
                    div #generic-interaction-control-label color=(color::MUTED) font-size=((typeface::DETAIL)) { "Control identifier" }
                    input #generic-interaction-control name="control_id" type=text data-label-id="generic-interaction-control-label" placeholder="control id (activate/change/focus/blur)" padding=((space::SM)) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) color=(color::TEXT);
                    div #generic-interaction-value-label color=(color::MUTED) font-size=((typeface::DETAIL)) { "Response value" }
                    input #generic-interaction-value name="value" type=text data-label-id="generic-interaction-value-label" placeholder="response value or JSON (submit/change)" padding=((space::SM)) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) color=(color::TEXT);
                    input type=hidden name="value_is_json" value="true";
                    div #generic-interaction-direction-label color=(color::MUTED) font-size=((typeface::DETAIL)) { "Focus direction" }
                    select #generic-interaction-direction name="direction" selected="next" data-label-id="generic-interaction-direction-label" padding=((space::SM)) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) color=(color::TEXT) {
                        option value="next" { "next" }
                        option value="previous" { "previous" }
                    }
                }
                button type=submit background=(surface::ACTIVE) color=(color::ON_ACCENT) border-radius=((radius::CONTROL)) padding="10, 14" margin-top=((space::SM)) {
                    "send interaction input"
                }
            }
        }
    }
}

fn question_interaction(
    snapshot: &QuestionSnapshot,
    interaction: &InteractionViewSummary,
    session_id: Option<bcode_session_models::SessionId>,
    context: &impl PresentationContext,
) -> Containers {
    container! {
        div gap=((space::MD)) {
            @if let Some(validation_error) = &snapshot.validation_error {
                div color=(color::ERROR) background=(surface::ERROR_INSET) border=((1, color::ERROR_BORDER)) border-radius=((radius::CONTROL)) padding=((space::SM)) {
                    "Validation error: " (validation_error)
                }
            }
            @for (question_index, question) in snapshot.request.questions.iter().enumerate() {
                div background=(surface::APP) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) padding=((space::S10)) {
                    @if let Some(header) = &question.header {
                        div color=(color::INFO) font-size=((typeface::LABEL)) margin-bottom=((space::XS)) { (header) }
                    }
                    div color=(color::STRONG) margin-bottom=((space::SM)) {
                        (question.text.clone())
                        @if question.required { span color=(color::ERROR) { " *" } }
                    }
                    div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-bottom=((space::SM)) {
                        (match (question.control, question.selection_mode) {
                            (QuestionControl::Radio, QuestionSelectionMode::Single) => "Choose one option",
                            (QuestionControl::Checkbox, QuestionSelectionMode::Multiple) => "Choose one or more options",
                            (_, QuestionSelectionMode::Single) => "Choose one option",
                            (_, QuestionSelectionMode::Multiple) => "Choose one or more options",
                        })
                        @if question.custom {
                            (match question.custom_mode {
                                QuestionCustomMode::Exclusive => " or provide a custom answer",
                                QuestionCustomMode::Additional => "; you may also provide a custom answer",
                            })
                        }
                    }
                    @if let Some(session_id) = session_id {
                        div gap=((space::S6)) {
                            @for (option_index, option) in question.options.iter().enumerate() {
                                (question_option(
                                    snapshot,
                                    interaction,
                                    session_id,
                                    context,
                                    question_index,
                                    option_index,
                                    question.control,
                                    option,
                                ))
                            }
                            @if question.custom || question.options.is_empty() {
                                (question_custom_answer(
                                    snapshot,
                                    interaction,
                                    session_id,
                                    context,
                                    question_index,
                                ))
                            }
                        }
                    }
                }
            }
            @if let Some(session_id) = session_id {
                div direction=row gap=((space::SM)) {
                    (question_terminal_action(interaction, session_id, context, "submit", "submit answers", accent::POSITIVE))
                    (question_terminal_action(interaction, session_id, context, "cancel", "cancel", accent::DESTRUCTIVE))
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn question_option(
    snapshot: &QuestionSnapshot,
    interaction: &InteractionViewSummary,
    session_id: bcode_session_models::SessionId,
    context: &impl PresentationContext,
    question_index: usize,
    option_index: usize,
    control: QuestionControl,
    option: &QuestionOption,
) -> Containers {
    let selected_value = option
        .value
        .as_deref()
        .map_or_else(|| option_index.to_string(), ToOwned::to_owned);
    let selected = snapshot
        .answers
        .iter()
        .find(|answer| answer.question_index == question_index)
        .is_some_and(|answer| answer.selected.contains(&selected_value));
    let option_label_id = format!("question-{question_index}-option-{option_index}-label");
    container! {
        form hx-post=(context.action_target(PresentationAction::ResolveInteraction)) hx-target="#bcode-web-shell" hx-swap=this {
            input type=hidden name="session_id" value=(session_id.to_string());
            input type=hidden name="interaction_id" value=(interaction.interaction_id.clone());
            input type=hidden name="kind" value="activate";
            input type=hidden name="control_id" value=(format!("question-{question_index}.option-{option_index}"));
            button type=submit data-control-role=(match control { QuestionControl::Radio => "radio", QuestionControl::Checkbox => "checkbox" }) data-control-selected=(selected.to_string()) data-label-id=(option_label_id.clone()) width=100% background=(if selected { surface::ACTIVE } else { surface::PANEL }) color=(color::STRONG) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) padding=((space::SM)) {
                span id=(option_label_id) {
                    (match (control, selected) {
                        (QuestionControl::Radio, true) => "◉ ",
                        (QuestionControl::Radio, false) => "○ ",
                        (QuestionControl::Checkbox, true) => "☑ ",
                        (QuestionControl::Checkbox, false) => "☐ ",
                    })
                    (option.label.clone())
                }
                @if let Some(description) = &option.description {
                    div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::S3)) { (description) }
                }
            }
        }
    }
}

fn question_custom_answer(
    snapshot: &QuestionSnapshot,
    interaction: &InteractionViewSummary,
    session_id: bcode_session_models::SessionId,
    context: &impl PresentationContext,
    question_index: usize,
) -> Containers {
    let value = snapshot
        .answers
        .iter()
        .find(|answer| answer.question_index == question_index)
        .and_then(|answer| answer.custom.as_deref())
        .unwrap_or_default();
    let custom_label_id = format!("question-{question_index}-custom-label");
    container! {
        form hx-post=(context.action_target(PresentationAction::ResolveInteraction)) hx-target="#bcode-web-shell" hx-swap=this direction=row gap=((space::S6)) {
            input type=hidden name="session_id" value=(session_id.to_string());
            input type=hidden name="interaction_id" value=(interaction.interaction_id.clone());
            input type=hidden name="kind" value="change";
            input type=hidden name="control_id" value=(format!("question-{question_index}.custom"));
            div id=(custom_label_id.clone()) color=(color::MUTED) font-size=((typeface::DETAIL)) { "Custom answer" }
            input name="value" type=text value=(value) placeholder="custom answer" data-label-id=(custom_label_id) flex=1 padding=((space::SM)) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) color=(color::TEXT);
            button type=submit background=(surface::ACTIVE) color=(color::ON_ACCENT) border-radius=((radius::CONTROL)) padding="10, 14" { "set" }
        }
    }
}

fn question_terminal_action(
    interaction: &InteractionViewSummary,
    session_id: bcode_session_models::SessionId,
    context: &impl PresentationContext,
    kind: &str,
    label: &str,
    color: &str,
) -> Containers {
    container! {
        form hx-post=(context.action_target(PresentationAction::ResolveInteraction)) hx-target="#bcode-web-shell" hx-swap=this {
            input type=hidden name="session_id" value=(session_id.to_string());
            input type=hidden name="interaction_id" value=(interaction.interaction_id.clone());
            input type=hidden name="kind" value=(kind);
            button type=submit background=(color) color=(color::ON_ACCENT) border-radius=((radius::CONTROL)) padding="10, 14" { (label) }
        }
    }
}
