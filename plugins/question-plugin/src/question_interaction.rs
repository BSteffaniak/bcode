//! Renderer-neutral question interaction controller.

use bcode_plugin_sdk::interaction::PluginInteraction;
use bcode_tool::{
    InteractionControlId, InteractionController, InteractionInput, InteractionNavigation,
    InteractionOutput, InteractionValue,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    NormalizedQuestionRequest, QuestionAnswerPayload, QuestionCustomMode, QuestionSelectionMode,
};

/// Renderer-neutral question interaction kind.
pub const QUESTION_INTERACTION_KIND: &str = "bcode.question";

/// Renderer-neutral question focus target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum QuestionFocusTarget {
    /// Focus the primary control for a question.
    Question {
        /// Question index.
        question_index: usize,
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
            Self::Question { question_index } => {
                InteractionControlId::new(format!("question-{question_index}"))
            }
            Self::Custom { question_index } => {
                InteractionControlId::new(format!("question-{question_index}.custom"))
            }
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
}

/// Renderer-neutral question controller.
pub struct QuestionInteractionController {
    request: NormalizedQuestionRequest,
    answers: Vec<QuestionAnswerPayload>,
    focus: QuestionFocusTarget,
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
        Self {
            request,
            answers,
            focus: QuestionFocusTarget::Question { question_index: 0 },
        }
    }

    fn focus_targets(&self) -> Vec<QuestionFocusTarget> {
        let mut targets = Vec::new();
        for (question_index, question) in self.request.questions.iter().enumerate() {
            targets.push(QuestionFocusTarget::Question { question_index });
            if question.custom || question.options.is_empty() {
                targets.push(QuestionFocusTarget::Custom { question_index });
            }
        }
        targets.push(QuestionFocusTarget::Submit);
        targets.push(QuestionFocusTarget::Cancel);
        targets
    }

    fn navigate(&mut self, direction: InteractionNavigation) {
        let targets = self.focus_targets();
        if targets.is_empty() {
            return;
        }
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
        if let Some(target) = parse_focus_target(control_id.as_str()) {
            self.focus = target;
            InteractionOutput::Redraw
        } else {
            InteractionOutput::None
        }
    }

    fn activate_control(&mut self, control_id: &InteractionControlId) -> InteractionOutput {
        match control_id.as_str() {
            "submit" => self.submit(),
            "cancel" => InteractionOutput::Cancelled,
            value => {
                if let Some((question_index, option_index)) = parse_option_control_id(value) {
                    self.toggle_option(question_index, option_index);
                    InteractionOutput::Redraw
                } else if let Some(target) = parse_focus_target(value) {
                    self.focus = target;
                    InteractionOutput::Redraw
                } else {
                    InteractionOutput::None
                }
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
        self.set_custom(question_index, text.to_owned());
        InteractionOutput::Redraw
    }

    fn toggle_option(&mut self, question_index: usize, option_index: usize) {
        let Some(question) = self.request.questions.get(question_index) else {
            return;
        };
        let Some(option) = question.options.get(option_index) else {
            return;
        };
        let value = option
            .value
            .clone()
            .unwrap_or_else(|| option_index.to_string());
        let answer = &mut self.answers[question_index];
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
            if question.custom_mode == QuestionCustomMode::Exclusive {
                answer.custom = None;
            }
        }
    }

    fn set_custom(&mut self, question_index: usize, text: String) {
        let answer = &mut self.answers[question_index];
        answer.custom = (!text.is_empty()).then_some(text);
        if self.request.questions[question_index].custom_mode == QuestionCustomMode::Exclusive {
            answer.selected.clear();
        }
    }

    fn submit(&self) -> InteractionOutput {
        InteractionOutput::Submitted {
            payload: json!({
                "status": "answered",
                "questions": self.answers,
            }),
        }
    }
}

impl InteractionController for QuestionInteractionController {
    type Snapshot = QuestionSnapshot;

    fn kind(&self) -> &'static str {
        QUESTION_INTERACTION_KIND
    }

    fn snapshot(&self) -> Self::Snapshot {
        QuestionSnapshot {
            request: self.request.clone(),
            answers: self.answers.clone(),
            focus: self.focus,
            focused_control_id: self.focus.control_id(),
        }
    }

    fn handle_input(&mut self, input: InteractionInput) -> InteractionOutput {
        match input {
            InteractionInput::Activate { control_id } => self.activate_control(&control_id),
            InteractionInput::Change { control_id, value } => {
                self.change_control(&control_id, &value)
            }
            InteractionInput::Focus { control_id } => self.focus_control(&control_id),
            InteractionInput::Blur { .. } => InteractionOutput::None,
            InteractionInput::Navigate { direction } => {
                self.navigate(direction);
                InteractionOutput::Redraw
            }
            InteractionInput::Submit => self.submit(),
            InteractionInput::Cancel => InteractionOutput::Cancelled,
        }
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
        InteractionController::snapshot(self)
    }

    fn handle_input(&mut self, input: InteractionInput) -> InteractionOutput {
        InteractionController::handle_input(self, input)
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
        _ => parse_question_control_id(value)
            .map(|question_index| QuestionFocusTarget::Question { question_index })
            .or_else(|| {
                parse_custom_control_id(value)
                    .map(|question_index| QuestionFocusTarget::Custom { question_index })
            }),
    }
}

fn parse_question_control_id(value: &str) -> Option<usize> {
    value.strip_prefix("question-")?.parse().ok()
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

    fn request() -> NormalizedQuestionRequest {
        NormalizedQuestionRequest {
            questions: vec![Question {
                header: None,
                text: "Proceed?".to_owned(),
                options: vec![QuestionOption {
                    label: "Yes".to_owned(),
                    value: Some("yes".to_owned()),
                    description: None,
                }],
                control: QuestionControl::Radio,
                selection_mode: QuestionSelectionMode::Single,
                custom: true,
                custom_mode: QuestionCustomMode::Additional,
                required: false,
            }],
        }
    }

    #[test]
    fn activates_option_and_submits_domain_payload() {
        let mut controller = QuestionInteractionController::new(request());
        assert_eq!(
            InteractionController::handle_input(
                &mut controller,
                InteractionInput::Activate {
                    control_id: option_control_id(0, 0),
                }
            ),
            InteractionOutput::Redraw
        );
        let output = InteractionController::handle_input(&mut controller, InteractionInput::Submit);
        let InteractionOutput::Submitted { payload } = output else {
            panic!("expected submitted output");
        };
        assert_eq!(payload["questions"][0]["selected"][0], "yes");
    }
}
