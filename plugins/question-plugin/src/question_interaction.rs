//! Renderer-neutral question interaction controller.

use bcode_plugin_sdk::interaction::PluginInteraction;
use bcode_tool::{
    InteractionControlId, InteractionInput, InteractionNavigation, InteractionOutput,
    InteractionValue,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    NormalizedQuestionRequest, QUESTION_INTERACTION_KIND, QuestionAnswerPayload,
    QuestionCustomMode, QuestionSelectionMode,
};

/// Renderer-neutral question focus target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum QuestionFocusTarget {
    /// Focus one selectable option.
    Option {
        /// Question index.
        question_index: usize,
        /// Option index within the question.
        option_index: usize,
    },
    /// Focus a custom text answer.
    Custom {
        /// Question index.
        question_index: usize,
    },
    /// Focus submit.
    Submit,
    /// Focus cancel.
    Cancel,
}

impl QuestionFocusTarget {
    /// Return a stable control id for this focus target.
    #[must_use]
    pub fn control_id(self) -> InteractionControlId {
        match self {
            Self::Option {
                question_index,
                option_index,
            } => option_control_id(question_index, option_index),
            Self::Custom { question_index } => custom_control_id(question_index),
            Self::Submit => InteractionControlId::new("submit"),
            Self::Cancel => InteractionControlId::new("cancel"),
        }
    }
}

/// Renderer-neutral question snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionSnapshot {
    /// Original normalized request.
    pub request: NormalizedQuestionRequest,
    /// Current answers.
    pub answers: Vec<QuestionAnswerPayload>,
    /// Current focus target.
    pub focus: QuestionFocusTarget,
    /// Current focused control id.
    pub focused_control_id: InteractionControlId,
    /// First required question that currently fails validation.
    pub invalid_question_index: Option<usize>,
}

/// Renderer-neutral question controller.
pub struct QuestionInteractionController {
    request: NormalizedQuestionRequest,
    answers: Vec<QuestionAnswerPayload>,
    focus: QuestionFocusTarget,
    invalid_question_index: Option<usize>,
}

impl QuestionInteractionController {
    /// Create a question interaction controller.
    #[must_use]
    pub fn new(request: NormalizedQuestionRequest) -> Self {
        let answers = request
            .questions
            .iter()
            .enumerate()
            .map(|(question_index, _)| QuestionAnswerPayload {
                question_index,
                selected: Vec::new(),
                custom: None,
            })
            .collect();
        let focus = first_focus_target(&request);
        Self {
            request,
            answers,
            focus,
            invalid_question_index: None,
        }
    }

    fn focus_targets(&self) -> Vec<QuestionFocusTarget> {
        focus_targets(&self.request)
    }

    fn navigate(&mut self, direction: InteractionNavigation) {
        let targets = self.focus_targets();
        let index = targets
            .iter()
            .position(|target| *target == self.focus)
            .unwrap_or(0);
        self.focus = match direction {
            InteractionNavigation::Next => targets[(index + 1) % targets.len()],
            InteractionNavigation::Previous => targets[(index + targets.len() - 1) % targets.len()],
        };
    }

    fn focus_control(&mut self, control_id: &InteractionControlId) -> InteractionOutput {
        let Some(target) = parse_focus_target(control_id.as_str()) else {
            return InteractionOutput::None;
        };
        if !self.focus_targets().contains(&target) {
            return InteractionOutput::None;
        }
        self.focus = target;
        InteractionOutput::Redraw
    }

    fn activate_control(&mut self, control_id: &InteractionControlId) -> InteractionOutput {
        match control_id.as_str() {
            "submit" => self.submit(),
            "cancel" => dismissed(),
            value => {
                let Some(target) = parse_focus_target(value) else {
                    return InteractionOutput::None;
                };
                if !self.focus_targets().contains(&target) {
                    return InteractionOutput::None;
                }
                self.focus = target;
                match target {
                    QuestionFocusTarget::Option {
                        question_index,
                        option_index,
                    } => {
                        self.toggle_option(question_index, option_index);
                        self.invalid_question_index = None;
                    }
                    QuestionFocusTarget::Custom { .. }
                    | QuestionFocusTarget::Submit
                    | QuestionFocusTarget::Cancel => {}
                }
                InteractionOutput::Redraw
            }
        }
    }

    fn change_control(
        &mut self,
        control_id: &InteractionControlId,
        value: &InteractionValue,
    ) -> InteractionOutput {
        let Some(question_index) = parse_custom_control_id(control_id.as_str()) else {
            return InteractionOutput::None;
        };
        let Some(text) = value.as_str() else {
            return InteractionOutput::None;
        };
        if !self
            .request
            .questions
            .get(question_index)
            .is_some_and(|question| question.custom || question.options.is_empty())
        {
            return InteractionOutput::None;
        }
        self.focus = QuestionFocusTarget::Custom { question_index };
        self.set_custom(question_index, text.to_owned());
        self.invalid_question_index = None;
        InteractionOutput::Redraw
    }

    fn toggle_option(&mut self, question_index: usize, option_index: usize) {
        let Some(question) = self.request.questions.get(question_index) else {
            return;
        };
        let Some(option) = question.options.get(option_index) else {
            return;
        };
        let Some(answer) = self.answers.get_mut(question_index) else {
            return;
        };
        let value = option
            .value
            .clone()
            .unwrap_or_else(|| option_index.to_string());
        if question.selection_mode == QuestionSelectionMode::Multiple {
            if let Some(index) = answer
                .selected
                .iter()
                .position(|selected| selected == &value)
            {
                answer.selected.remove(index);
            } else {
                answer.selected.push(value);
            }
        } else {
            answer.selected = vec![value];
        }
        if question.custom_mode == QuestionCustomMode::Exclusive {
            answer.custom = None;
        }
    }

    fn set_custom(&mut self, question_index: usize, text: String) {
        let Some(question) = self.request.questions.get(question_index) else {
            return;
        };
        let Some(answer) = self.answers.get_mut(question_index) else {
            return;
        };
        answer.custom = (!text.is_empty()).then_some(text);
        if question.custom_mode == QuestionCustomMode::Exclusive {
            answer.selected.clear();
        }
    }

    fn first_invalid_required_question(&self) -> Option<usize> {
        self.request
            .questions
            .iter()
            .zip(&self.answers)
            .position(|(question, answer)| question.required && !answer_is_meaningful(answer))
    }

    fn submit(&mut self) -> InteractionOutput {
        if let Some(question_index) = self.first_invalid_required_question() {
            self.invalid_question_index = Some(question_index);
            self.focus = first_question_focus_target(&self.request, question_index)
                .unwrap_or(QuestionFocusTarget::Submit);
            return InteractionOutput::Redraw;
        }
        self.invalid_question_index = None;
        let answers = self
            .answers
            .iter()
            .filter(|answer| answer_is_meaningful(answer))
            .cloned()
            .collect::<Vec<_>>();
        InteractionOutput::Submitted {
            payload: json!({
                "status": "answered",
                "questions": answers,
            }),
        }
    }

    fn snapshot(&self) -> QuestionSnapshot {
        QuestionSnapshot {
            request: self.request.clone(),
            answers: self.answers.clone(),
            focus: self.focus,
            focused_control_id: self.focus.control_id(),
            invalid_question_index: self.invalid_question_index,
        }
    }

    fn handle_input(&mut self, input: InteractionInput) -> InteractionOutput {
        match input {
            InteractionInput::Activate { control_id } => self.activate_control(&control_id),
            InteractionInput::Change { control_id, value } => {
                self.change_control(&control_id, &value)
            }
            InteractionInput::Focus { control_id } => self.focus_control(&control_id),
            InteractionInput::Blur { .. } | InteractionInput::Tick => InteractionOutput::None,
            InteractionInput::Navigate { direction } => {
                self.navigate(direction);
                InteractionOutput::Redraw
            }
            InteractionInput::Submit => self.submit(),
            InteractionInput::Cancel => dismissed(),
        }
    }
}

fn answer_is_meaningful(answer: &QuestionAnswerPayload) -> bool {
    !answer.selected.is_empty()
        || answer
            .custom
            .as_deref()
            .is_some_and(|custom| !custom.trim().is_empty())
}

fn focus_targets(request: &NormalizedQuestionRequest) -> Vec<QuestionFocusTarget> {
    let mut targets = Vec::new();
    for (question_index, question) in request.questions.iter().enumerate() {
        targets.extend((0..question.options.len()).map(|option_index| {
            QuestionFocusTarget::Option {
                question_index,
                option_index,
            }
        }));
        if question.custom || question.options.is_empty() {
            targets.push(QuestionFocusTarget::Custom { question_index });
        }
    }
    targets.push(QuestionFocusTarget::Submit);
    targets.push(QuestionFocusTarget::Cancel);
    targets
}

fn first_focus_target(request: &NormalizedQuestionRequest) -> QuestionFocusTarget {
    focus_targets(request)
        .into_iter()
        .next()
        .unwrap_or(QuestionFocusTarget::Submit)
}

fn first_question_focus_target(
    request: &NormalizedQuestionRequest,
    question_index: usize,
) -> Option<QuestionFocusTarget> {
    let question = request.questions.get(question_index)?;
    if question.options.is_empty() {
        (question.custom || question.options.is_empty())
            .then_some(QuestionFocusTarget::Custom { question_index })
    } else {
        Some(QuestionFocusTarget::Option {
            question_index,
            option_index: 0,
        })
    }
}

fn dismissed() -> InteractionOutput {
    InteractionOutput::Submitted {
        payload: json!({"status": "dismissed"}),
    }
}

impl PluginInteraction for QuestionInteractionController {
    const KIND: &'static str = QUESTION_INTERACTION_KIND;

    type Request = NormalizedQuestionRequest;
    type Snapshot = QuestionSnapshot;

    fn new(request: Self::Request) -> Self {
        Self::new(request)
    }

    fn snapshot(&self) -> Self::Snapshot {
        self.snapshot()
    }

    fn handle_input(&mut self, input: InteractionInput) -> InteractionOutput {
        self.handle_input(input)
    }
}

/// Return a stable question option control id.
#[must_use]
pub fn option_control_id(question_index: usize, option_index: usize) -> InteractionControlId {
    InteractionControlId::new(format!("question-{question_index}.option-{option_index}"))
}

/// Return a stable custom answer control id.
#[must_use]
pub fn custom_control_id(question_index: usize) -> InteractionControlId {
    InteractionControlId::new(format!("question-{question_index}.custom"))
}

fn parse_focus_target(value: &str) -> Option<QuestionFocusTarget> {
    match value {
        "submit" => Some(QuestionFocusTarget::Submit),
        "cancel" => Some(QuestionFocusTarget::Cancel),
        _ => parse_option_control_id(value)
            .map(
                |(question_index, option_index)| QuestionFocusTarget::Option {
                    question_index,
                    option_index,
                },
            )
            .or_else(|| {
                parse_custom_control_id(value)
                    .map(|question_index| QuestionFocusTarget::Custom { question_index })
            }),
    }
}

fn parse_option_control_id(value: &str) -> Option<(usize, usize)> {
    let (question, option) = value.strip_prefix("question-")?.split_once(".option-")?;
    Some((question.parse().ok()?, option.parse().ok()?))
}

fn parse_custom_control_id(value: &str) -> Option<usize> {
    value
        .strip_prefix("question-")?
        .strip_suffix(".custom")?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use bcode_tool::{InteractionInput, InteractionOutput};

    use super::*;
    use crate::{Question, QuestionControl, QuestionCustomMode, QuestionOption};

    fn question(
        options: &[(&str, &str)],
        selection_mode: QuestionSelectionMode,
        custom: bool,
        custom_mode: QuestionCustomMode,
        required: bool,
    ) -> Question {
        Question {
            header: None,
            text: "Proceed?".to_owned(),
            options: options
                .iter()
                .map(|(label, value)| QuestionOption {
                    label: (*label).to_owned(),
                    value: Some((*value).to_owned()),
                    description: None,
                })
                .collect(),
            control: if selection_mode == QuestionSelectionMode::Multiple {
                QuestionControl::Checkbox
            } else {
                QuestionControl::Radio
            },
            selection_mode,
            custom,
            custom_mode,
            required,
        }
    }

    fn request(question: Question) -> NormalizedQuestionRequest {
        NormalizedQuestionRequest {
            questions: vec![question],
        }
    }

    #[test]
    fn initially_focuses_first_option_and_navigates_each_control() {
        let controller = QuestionInteractionController::new(request(question(
            &[("Yes", "yes"), ("No", "no")],
            QuestionSelectionMode::Single,
            true,
            QuestionCustomMode::Additional,
            false,
        )));
        assert_eq!(
            controller.snapshot().focus,
            QuestionFocusTarget::Option {
                question_index: 0,
                option_index: 0,
            }
        );

        let mut controller = controller;
        controller.handle_input(InteractionInput::Navigate {
            direction: InteractionNavigation::Next,
        });
        assert_eq!(
            controller.snapshot().focus,
            QuestionFocusTarget::Option {
                question_index: 0,
                option_index: 1,
            }
        );
        controller.handle_input(InteractionInput::Navigate {
            direction: InteractionNavigation::Previous,
        });
        assert_eq!(
            controller.snapshot().focus,
            QuestionFocusTarget::Option {
                question_index: 0,
                option_index: 0,
            }
        );
    }

    #[test]
    fn cancel_submits_plugin_owned_dismissed_response() {
        let mut controller = QuestionInteractionController::new(request(question(
            &[("Yes", "yes")],
            QuestionSelectionMode::Single,
            false,
            QuestionCustomMode::Additional,
            false,
        )));
        assert_eq!(
            controller.handle_input(InteractionInput::Cancel),
            InteractionOutput::Submitted {
                payload: json!({"status": "dismissed"}),
            }
        );
        assert_eq!(
            controller.handle_input(InteractionInput::Activate {
                control_id: InteractionControlId::new("cancel"),
            }),
            InteractionOutput::Submitted {
                payload: json!({"status": "dismissed"}),
            }
        );
    }

    #[test]
    fn single_selection_replaces_previous_option() {
        let mut controller = QuestionInteractionController::new(request(question(
            &[("Yes", "yes"), ("No", "no")],
            QuestionSelectionMode::Single,
            false,
            QuestionCustomMode::Additional,
            false,
        )));
        controller.handle_input(InteractionInput::Activate {
            control_id: option_control_id(0, 0),
        });
        controller.handle_input(InteractionInput::Activate {
            control_id: option_control_id(0, 1),
        });
        assert_eq!(controller.snapshot().answers[0].selected, ["no"]);
        assert_eq!(
            controller.snapshot().focus,
            QuestionFocusTarget::Option {
                question_index: 0,
                option_index: 1,
            }
        );
    }

    #[test]
    fn multiple_selection_toggles_each_option() {
        let mut controller = QuestionInteractionController::new(request(question(
            &[("One", "one"), ("Two", "two")],
            QuestionSelectionMode::Multiple,
            false,
            QuestionCustomMode::Additional,
            false,
        )));
        for option_index in [0, 1, 0] {
            controller.handle_input(InteractionInput::Activate {
                control_id: option_control_id(0, option_index),
            });
        }
        assert_eq!(controller.snapshot().answers[0].selected, ["two"]);
    }

    #[test]
    fn exclusive_custom_and_option_answers_clear_each_other() {
        let mut controller = QuestionInteractionController::new(request(question(
            &[("Yes", "yes")],
            QuestionSelectionMode::Single,
            true,
            QuestionCustomMode::Exclusive,
            false,
        )));
        controller.handle_input(InteractionInput::Activate {
            control_id: option_control_id(0, 0),
        });
        controller.handle_input(InteractionInput::Change {
            control_id: custom_control_id(0),
            value: InteractionValue::String("something else".to_owned()),
        });
        assert!(controller.snapshot().answers[0].selected.is_empty());
        controller.handle_input(InteractionInput::Activate {
            control_id: option_control_id(0, 0),
        });
        assert_eq!(controller.snapshot().answers[0].custom, None);
    }

    #[test]
    fn required_validation_keeps_surface_open_and_focuses_question() {
        let mut controller = QuestionInteractionController::new(request(question(
            &[("Yes", "yes")],
            QuestionSelectionMode::Single,
            false,
            QuestionCustomMode::Additional,
            true,
        )));
        assert_eq!(
            controller.handle_input(InteractionInput::Submit),
            InteractionOutput::Redraw
        );
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.invalid_question_index, Some(0));
        assert_eq!(
            snapshot.focus,
            QuestionFocusTarget::Option {
                question_index: 0,
                option_index: 0,
            }
        );
    }

    #[test]
    fn optional_unanswered_questions_are_omitted_from_submission() {
        let mut controller = QuestionInteractionController::new(request(question(
            &[("Yes", "yes")],
            QuestionSelectionMode::Single,
            false,
            QuestionCustomMode::Additional,
            false,
        )));
        let InteractionOutput::Submitted { payload } =
            controller.handle_input(InteractionInput::Submit)
        else {
            panic!("expected submitted output");
        };
        assert_eq!(payload["questions"], json!([]));
    }

    #[test]
    fn malformed_and_out_of_range_control_ids_are_ignored() {
        let mut controller = QuestionInteractionController::new(request(question(
            &[("Yes", "yes")],
            QuestionSelectionMode::Single,
            false,
            QuestionCustomMode::Additional,
            false,
        )));
        for control_id in [
            "question",
            "question-0",
            "question-0.option-x",
            "question-0.option-9",
            "question-9.custom",
        ] {
            assert_eq!(
                controller.handle_input(InteractionInput::Activate {
                    control_id: InteractionControlId::new(control_id),
                }),
                InteractionOutput::None
            );
        }
        assert!(controller.snapshot().answers[0].selected.is_empty());
    }
}
