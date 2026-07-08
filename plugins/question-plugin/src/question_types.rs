//! Domain types for the question tool.

use serde::{Deserialize, Serialize};

/// Renderer-neutral question interaction kind.
pub const QUESTION_INTERACTION_KIND: &str = "bcode.question";

/// Native inline TUI surface kind for question requests.
pub const QUESTION_INLINE_SURFACE: &str = "bcode.question.inline";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedQuestionRequest {
    pub(crate) questions: Vec<Question>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub(crate) header: Option<String>,
    #[serde(rename = "question")]
    pub(crate) text: String,
    pub(crate) options: Vec<QuestionOption>,
    pub(crate) control: QuestionControl,
    pub(crate) selection_mode: QuestionSelectionMode,
    pub(crate) custom: bool,
    pub(crate) custom_mode: QuestionCustomMode,
    pub(crate) required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub(crate) label: String,
    pub(crate) value: Option<String>,
    pub(crate) description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionControl {
    Radio,
    Checkbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionSelectionMode {
    Single,
    Multiple,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionCustomMode {
    Exclusive,
    Additional,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionToolOutcome {
    pub(crate) status: QuestionRequestStatus,
    pub(crate) questions: Vec<QuestionOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionRequestStatus {
    Answered,
    Unanswered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOutcome {
    pub(crate) question_index: usize,
    pub(crate) header: Option<String>,
    pub(crate) question: String,
    pub(crate) status: QuestionStatus,
    pub(crate) selected: Vec<SelectedAnswer>,
    pub(crate) custom: Option<String>,
    pub(crate) required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionStatus {
    Answered,
    Unanswered,
    Dismissed,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum QuestionResolutionPayload {
    Answered {
        questions: Vec<QuestionAnswerPayload>,
    },
    Unanswered,
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionAnswerPayload {
    pub(crate) question_index: usize,
    #[serde(default)]
    pub(crate) selected: Vec<String>,
    #[serde(default)]
    pub(crate) custom: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedAnswer {
    pub label: String,
    pub value: String,
}
