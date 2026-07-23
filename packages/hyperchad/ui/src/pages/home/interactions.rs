//! Permission and interactive-tool presentation.

use bcode_session_view_models::InteractionViewSummary;
use hyperchad::template::{Containers, container};
use serde::Deserialize;

use super::adapters::json_panel;

#[derive(Debug, Deserialize)]
struct QuestionSnapshot {
    request: QuestionRequest,
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

pub(super) fn interaction_request(
    interaction: &InteractionViewSummary,
    session_id: Option<bcode_session_models::SessionId>,
    access_token: &str,
) -> Containers {
    container! {
        div border="1, #58a6ff" border-radius=8 padding=10 margin-bottom=10 {
            div color="#58a6ff" margin-bottom=6 {
                (interaction.title.as_deref().unwrap_or("Interactive request"))
            }
            div color="#8b949e" font-size=12 margin-bottom=8 {
                (interaction.kind)
                @if interaction.required { " · required" }
            }
            @if interaction.resolved {
                div color="#8b949e" font-size=12 margin-top=8 { "resolved" }
                @if let Some(resolution) = &interaction.resolution {
                    (json_panel("resolution", resolution))
                }
            } @else {
                @if interaction.kind == "bcode.question" {
                    @if let Some(snapshot) = interaction.snapshot.as_ref().and_then(|value| serde_json::from_value::<QuestionSnapshot>(value.clone()).ok()) {
                        (question_interaction(&snapshot, interaction, session_id, access_token))
                    } @else if let Some(snapshot) = &interaction.snapshot {
                        (json_panel("controller snapshot", snapshot))
                    }
                } @else if let Some(snapshot) = &interaction.snapshot {
                    (json_panel("controller snapshot", snapshot))
                }
                @if let Some(session_id) = session_id {
                    (generic_interaction_controls(interaction, session_id, access_token))
                }
            }
        }
    }
}

fn generic_interaction_controls(
    interaction: &InteractionViewSummary,
    session_id: bcode_session_models::SessionId,
    access_token: &str,
) -> Containers {
    container! {
        details margin-top=10 {
            summary color="#8b949e" { "generic semantic controls" }
            form hx-post=(format!("/actions/interaction?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this margin-top=8 {
                input type=hidden name="session_id" value=(session_id.to_string());
                input type=hidden name="interaction_id" value=(interaction.interaction_id.clone());
                div gap=8 {
                    select name="kind" selected="submit" padding=8 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9" {
                        option value="activate" { "activate control" }
                        option value="change" { "change control value" }
                        option value="focus" { "focus control" }
                        option value="blur" { "blur control" }
                        option value="navigate" { "navigate focus" }
                        option value="submit" { "submit interaction" }
                        option value="cancel" { "cancel interaction" }
                    }
                    input name="control_id" type=text placeholder="control id (activate/change/focus/blur)" padding=8 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9";
                    input name="value" type=text placeholder="JSON value (change only)" padding=8 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9";
                    input type=hidden name="value_is_json" value="true";
                    select name="direction" selected="next" padding=8 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9" {
                        option value="next" { "next" }
                        option value="previous" { "previous" }
                    }
                }
                button type=submit background="#1f6feb" color=white border-radius=6 padding="6, 12" margin-top=8 {
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
    access_token: &str,
) -> Containers {
    container! {
        div gap=12 {
            @for (question_index, question) in snapshot.request.questions.iter().enumerate() {
                div background="#0d1117" border="1, #30363d" border-radius=6 padding=10 {
                    @if let Some(header) = &question.header {
                        div color="#58a6ff" font-size=12 margin-bottom=4 { (header) }
                    }
                    div color="#f0f6fc" margin-bottom=8 {
                        (question.text.clone())
                        @if question.required { span color="#f85149" { " *" } }
                    }
                    div color="#8b949e" font-size=11 margin-bottom=8 {
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
                        div gap=6 {
                            @for (option_index, option) in question.options.iter().enumerate() {
                                (question_option(
                                    snapshot,
                                    interaction,
                                    session_id,
                                    access_token,
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
                                    access_token,
                                    question_index,
                                ))
                            }
                        }
                    }
                }
            }
            @if let Some(session_id) = session_id {
                div direction=row gap=8 {
                    (question_terminal_action(interaction, session_id, access_token, "submit", "submit answers", "#238636"))
                    (question_terminal_action(interaction, session_id, access_token, "cancel", "cancel", "#da3633"))
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
    access_token: &str,
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
    container! {
        form hx-post=(format!("/actions/interaction?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this {
            input type=hidden name="session_id" value=(session_id.to_string());
            input type=hidden name="interaction_id" value=(interaction.interaction_id.clone());
            input type=hidden name="kind" value="activate";
            input type=hidden name="control_id" value=(format!("question-{question_index}.option-{option_index}"));
            button type=submit width=100% background=(if selected { "#1f6feb" } else { "#161b22" }) color="#f0f6fc" border="1, #30363d" border-radius=6 padding=8 {
                (match (control, selected) {
                    (QuestionControl::Radio, true) => "◉ ",
                    (QuestionControl::Radio, false) => "○ ",
                    (QuestionControl::Checkbox, true) => "☑ ",
                    (QuestionControl::Checkbox, false) => "☐ ",
                })
                (option.label.clone())
                @if let Some(description) = &option.description {
                    div color="#8b949e" font-size=11 margin-top=3 { (description) }
                }
            }
        }
    }
}

fn question_custom_answer(
    snapshot: &QuestionSnapshot,
    interaction: &InteractionViewSummary,
    session_id: bcode_session_models::SessionId,
    access_token: &str,
    question_index: usize,
) -> Containers {
    let value = snapshot
        .answers
        .iter()
        .find(|answer| answer.question_index == question_index)
        .and_then(|answer| answer.custom.as_deref())
        .unwrap_or_default();
    container! {
        form hx-post=(format!("/actions/interaction?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this direction=row gap=6 {
            input type=hidden name="session_id" value=(session_id.to_string());
            input type=hidden name="interaction_id" value=(interaction.interaction_id.clone());
            input type=hidden name="kind" value="change";
            input type=hidden name="control_id" value=(format!("question-{question_index}.custom"));
            input name="value" type=text value=(value) placeholder="custom answer" flex=1 padding=8 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9";
            button type=submit background="#1f6feb" color=white border-radius=6 padding="6, 12" { "set" }
        }
    }
}

fn question_terminal_action(
    interaction: &InteractionViewSummary,
    session_id: bcode_session_models::SessionId,
    access_token: &str,
    kind: &str,
    label: &str,
    color: &str,
) -> Containers {
    container! {
        form hx-post=(format!("/actions/interaction?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this {
            input type=hidden name="session_id" value=(session_id.to_string());
            input type=hidden name="interaction_id" value=(interaction.interaction_id.clone());
            input type=hidden name="kind" value=(kind);
            button type=submit background=(color) color=white border-radius=6 padding="6, 12" { (label) }
        }
    }
}
