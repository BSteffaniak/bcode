//! Portable question normalization and outcomes.
use super::{
    MAX_OPTION_TEXT_BYTES, MAX_OPTIONS_PER_QUESTION, MAX_QUESTION_TEXT_BYTES, MAX_QUESTIONS,
    NormalizedQuestionRequest, Question, QuestionAnswerPayload, QuestionControl,
    QuestionCustomMode, QuestionOption, QuestionOutcome, QuestionRequestStatus,
    QuestionSelectionMode, QuestionStatus, QuestionToolOutcome, SelectedAnswer,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::fmt::Write as _;

/// Build the model-visible answered outcome.
///
/// # Errors
/// Returns an error if a required question has no meaningful answer.
pub fn answered_outcome(
    request: &NormalizedQuestionRequest,
    answers: Vec<QuestionAnswerPayload>,
) -> Result<QuestionToolOutcome, String> {
    let answers = answers
        .into_iter()
        .map(|answer| (answer.question_index, answer))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut questions = Vec::new();
    for (question_index, question) in request.questions.iter().enumerate() {
        let Some(answer) = answers.get(&question_index) else {
            if question.required {
                return Err(format!(
                    "required question {} was not answered",
                    question_index.saturating_add(1)
                ));
            }
            questions.push(question_outcome(
                question_index,
                question,
                QuestionStatus::Unanswered,
                Vec::new(),
                None,
            ));
            continue;
        };
        let selected = selected_answers(question, &answer.selected);
        let custom = answer
            .custom
            .clone()
            .filter(|value| !value.trim().is_empty());
        if question.required && selected.is_empty() && custom.is_none() {
            return Err(format!(
                "required question {} was not answered",
                question_index.saturating_add(1)
            ));
        }
        questions.push(question_outcome(
            question_index,
            question,
            QuestionStatus::Answered,
            selected,
            custom,
        ));
    }
    Ok(QuestionToolOutcome {
        status: QuestionRequestStatus::Answered,
        questions,
    })
}

fn selected_answers(question: &Question, selected_values: &[String]) -> Vec<SelectedAnswer> {
    selected_values
        .iter()
        .map(|selected| {
            let option = question.options.iter().find(|option| {
                option.value.as_ref().is_some_and(|value| value == selected)
                    || option.label == *selected
            });
            SelectedAnswer {
                label: option.map_or_else(|| selected.clone(), |option| option.label.clone()),
                value: option
                    .and_then(|option| option.value.clone())
                    .unwrap_or_else(|| selected.clone()),
            }
        })
        .collect()
}

/// Build an explicitly dismissed outcome.
#[must_use]
pub fn dismissed_outcome(request: &NormalizedQuestionRequest) -> QuestionToolOutcome {
    request_status_outcome(
        request,
        QuestionRequestStatus::Unanswered,
        QuestionStatus::Dismissed,
    )
}

fn question_outcome(
    question_index: usize,
    question: &Question,
    status: QuestionStatus,
    selected: Vec<SelectedAnswer>,
    custom: Option<String>,
) -> QuestionOutcome {
    QuestionOutcome {
        question_index,
        header: question.header.clone(),
        question: question.text.clone(),
        status,
        selected,
        custom,
        required: question.required,
    }
}

/// Build an unanswered outcome when no consumer responds.
#[must_use]
pub fn unanswered_outcome(request: &NormalizedQuestionRequest) -> QuestionToolOutcome {
    request_status_outcome(
        request,
        QuestionRequestStatus::Unanswered,
        QuestionStatus::Unanswered,
    )
}

fn request_status_outcome(
    request: &NormalizedQuestionRequest,
    request_status: QuestionRequestStatus,
    question_status: QuestionStatus,
) -> QuestionToolOutcome {
    QuestionToolOutcome {
        status: request_status,
        questions: request
            .questions
            .iter()
            .enumerate()
            .map(|(question_index, question)| {
                question_outcome(question_index, question, question_status, Vec::new(), None)
            })
            .collect(),
    }
}

/// Format a producer outcome for model-visible tool output.
#[must_use]
pub fn format_question_outcome_output(outcome: &QuestionToolOutcome) -> String {
    let mut output = format!("Question interaction completed: {:?}.", outcome.status);
    for question in &outcome.questions {
        let selected = if question.selected.is_empty() {
            String::new()
        } else {
            format!(
                " selected=[{}]",
                question
                    .selected
                    .iter()
                    .map(|answer| answer.label.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let custom = question
            .custom
            .as_ref()
            .map_or_else(String::new, |custom| format!(" custom={custom}"));
        let _ = write!(
            output,
            "\n* {}. {:?}: {}{}{}",
            question.question_index.saturating_add(1),
            question.status,
            question.question,
            selected,
            custom
        );
    }
    output
}

/// Normalize and validate a bounded question request.
///
/// # Errors
/// Returns a diagnostic for unsupported fields, controls, modes, or invalid limits.
pub fn parse_question_request(value: Value) -> Result<NormalizedQuestionRequest, String> {
    let value = normalize_value_keys(value);
    let question_values = match value {
        Value::Object(mut object) if object.contains_key("questions") => {
            match object.remove("questions").unwrap_or(Value::Null) {
                Value::Array(questions) => questions,
                other => {
                    return Err(format!(
                        "Invalid question tool input: `questions` must be an array, got {}.",
                        type_name(&other)
                    ));
                }
            }
        }
        Value::Object(object) => vec![Value::Object(object)],
        Value::Array(questions) => questions,
        other => {
            return Err(format!(
                "Invalid question tool input: expected an object or array, got {}.",
                type_name(&other)
            ));
        }
    };
    if question_values.is_empty() {
        return Err("Invalid question tool input: at least one question is required.".to_string());
    }
    if question_values.len() > MAX_QUESTIONS {
        return Err(format!(
            "Invalid question tool input: at most {MAX_QUESTIONS} questions are supported."
        ));
    }
    let questions = question_values
        .into_iter()
        .enumerate()
        .map(|(index, value)| parse_question(index, value))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(NormalizedQuestionRequest { questions })
}

fn parse_question(index: usize, value: Value) -> Result<Question, String> {
    let object = match value {
        Value::Object(object) => object,
        other => {
            return Err(format!(
                "Invalid question tool input: question {} must be an object, got {}.",
                index.saturating_add(1),
                type_name(&other)
            ));
        }
    };
    let text = string_field(&object, "question")
        .filter(|value| !value.trim().is_empty())
        .filter(|value| value.len() <= MAX_QUESTION_TEXT_BYTES)
        .ok_or_else(|| {
            format!(
                "Invalid question tool input: question {} is missing `question` or exceeds {MAX_QUESTION_TEXT_BYTES} bytes.",
                index.saturating_add(1)
            )
        })?;
    let multiple = bool_field(&object, "multiple").unwrap_or(false);
    let selection_mode = enum_field(&object, "selection_mode")?.unwrap_or(if multiple {
        QuestionSelectionMode::Multiple
    } else {
        QuestionSelectionMode::Single
    });
    let control = enum_field(&object, "control")?.unwrap_or(match selection_mode {
        QuestionSelectionMode::Single => QuestionControl::Radio,
        QuestionSelectionMode::Multiple => QuestionControl::Checkbox,
    });
    let control = match (control, selection_mode) {
        (QuestionControl::Radio, QuestionSelectionMode::Multiple) => QuestionControl::Checkbox,
        (control, _) => control,
    };
    let custom = bool_field(&object, "custom").unwrap_or(true);
    let custom_mode = enum_field(&object, "custom_mode")?.unwrap_or(QuestionCustomMode::Additional);
    let options = options_field(&object, index)?;
    if options.len() > MAX_OPTIONS_PER_QUESTION {
        return Err(format!(
            "Invalid question tool input: question {} has more than {MAX_OPTIONS_PER_QUESTION} options.",
            index.saturating_add(1)
        ));
    }
    if options.is_empty() && !custom {
        return Err(format!(
            "Invalid question tool input: question {} needs `options` unless `custom` is true.",
            index.saturating_add(1)
        ));
    }
    Ok(Question {
        header: string_field(&object, "header"),
        text,
        options,
        control,
        selection_mode,
        custom,
        custom_mode,
        required: bool_field(&object, "required").unwrap_or(false),
    })
}

fn normalize_value_keys(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (canonical_key(&key), normalize_value_keys(value)))
                .collect(),
        ),
        Value::Array(values) => {
            Value::Array(values.into_iter().map(normalize_value_keys).collect())
        }
        value => value,
    }
}

fn canonical_key(key: &str) -> String {
    match normalized_key(key).as_str() {
        "selectionmode" => "selection_mode".to_string(),
        "custommode" => "custom_mode".to_string(),
        key => key.to_string(),
    }
}

fn normalized_key(key: &str) -> String {
    key.chars()
        .filter(|character| !matches!(character, '_' | '-' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

fn string_field(object: &Map<String, Value>, field: &str) -> Option<String> {
    object.get(field).and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        _ => None,
    })
}

fn bool_field(object: &Map<String, Value>, field: &str) -> Option<bool> {
    object.get(field).and_then(Value::as_bool)
}

fn enum_field<T>(object: &Map<String, Value>, field: &str) -> Result<Option<T>, String>
where
    T: for<'de> Deserialize<'de>,
{
    object
        .get(field)
        .map(|value| serde_json::from_value(value.clone()).map_err(|error| error.to_string()))
        .transpose()
}

fn options_field(
    object: &Map<String, Value>,
    question_index: usize,
) -> Result<Vec<QuestionOption>, String> {
    match object.get("options") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .enumerate()
            .map(|(option_index, value)| parse_option(question_index, option_index, value))
            .collect(),
        Some(other) => Err(format!(
            "Invalid question tool input: question {} `options` must be an array, got {}.",
            question_index.saturating_add(1),
            type_name(other)
        )),
    }
}

fn parse_option(
    question_index: usize,
    option_index: usize,
    value: &Value,
) -> Result<QuestionOption, String> {
    match value {
        Value::String(label)
            if !label.trim().is_empty() && label.len() <= MAX_OPTION_TEXT_BYTES =>
        {
            Ok(QuestionOption {
                label: label.clone(),
                value: None,
                description: None,
            })
        }
        Value::String(_) => Err(format!(
            "Invalid question tool input: question {} option {} is empty or exceeds {MAX_OPTION_TEXT_BYTES} bytes.",
            question_index.saturating_add(1),
            option_index.saturating_add(1)
        )),
        Value::Object(object) => {
            let label = string_field(object, "label")
                .filter(|value| !value.trim().is_empty())
                .filter(|value| value.len() <= MAX_OPTION_TEXT_BYTES)
                .ok_or_else(|| {
                    format!(
                        "Invalid question tool input: question {} option {} is missing `label`.",
                        question_index.saturating_add(1),
                        option_index.saturating_add(1)
                    )
                })?;
            Ok(QuestionOption {
                label,
                value: string_field(object, "value"),
                description: string_field(object, "description"),
            })
        }
        other => Err(format!(
            "Invalid question tool input: question {} option {} must be an object or string, got {}.",
            question_index.saturating_add(1),
            option_index.saturating_add(1),
            type_name(other)
        )),
    }
}

const fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
