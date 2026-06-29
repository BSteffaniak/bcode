#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Interactive question tool plugin for Bcode.

use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    InteractiveToolResolution, InteractiveToolResumeRequest, ListToolsRequest, OP_INVOKE_TOOL,
    OP_LIST_TOOLS, OP_PRESENT_ARTIFACT, OP_PRESENT_TOOL_RESULT, OP_RESUME_INTERACTIVE_TOOL,
    TOOL_SERVICE_INTERFACE_ID, ToolArtifactPresentationRequest, ToolArtifactPresentationResponse,
    ToolCompatibilityAlias, ToolDefinition, ToolInvocationRequest, ToolInvocationResponse,
    ToolInvocationResult, ToolList, ToolPolicyMetadata, ToolPresentationEvent,
    ToolPresentationTarget, ToolProtocolPresentation, ToolResultPresentationRequest,
    ToolResultPresentationResponse, ToolSideEffect,
};
use bmux_tui_component_protocol::model::{ComponentKind, ComponentNode};
use bmux_tui_component_protocol::value::ComponentValue;
use bmux_tui_components::protocol::{
    ACTION_ROW_TYPE_ID, CheckboxGroupProps, ChoiceOptionProps, FormBuilder, RadioGroupProps,
    TextInputProps,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fmt::Write as _;

const TOOL_NAME: &str = "question";
const DEFAULT_ASK_AGGRESSIVENESS: u8 = 5;

#[derive(Debug, Default)]
struct QuestionPlugin;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NormalizedQuestionRequest {
    questions: Vec<Question>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct QuestionOption {
    label: String,
    value: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QuestionControl {
    Radio,
    Checkbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QuestionSelectionMode {
    Single,
    Multiple,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QuestionCustomMode {
    Exclusive,
    Additional,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct QuestionToolOutcome {
    status: QuestionRequestStatus,
    questions: Vec<QuestionOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QuestionRequestStatus {
    Answered,
    Unanswered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct QuestionOutcome {
    question_index: usize,
    header: Option<String>,
    question: String,
    status: QuestionStatus,
    selected: Vec<SelectedAnswer>,
    custom: Option<String>,
    required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QuestionStatus {
    Answered,
    Unanswered,
    Dismissed,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
enum QuestionResolutionPayload {
    Answered {
        questions: Vec<QuestionAnswerPayload>,
    },
    Unanswered,
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ProtocolResolutionPayload {
    #[serde(default)]
    values: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct QuestionAnswerPayload {
    question_index: usize,
    #[serde(default)]
    selected: Vec<String>,
    #[serde(default)]
    custom: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SelectedAnswer {
    label: String,
    value: String,
}

impl RustPlugin for QuestionPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.interface_id.as_str() {
            TOOL_SERVICE_INTERFACE_ID => invoke_tool_service(&context),
            _ => ServiceResponse::error(
                "unsupported_interface",
                "unsupported question plugin service interface",
            ),
        }
    }
}

fn invoke_tool_service(context: &NativeServiceContext) -> ServiceResponse {
    match context.request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(&context.request),
        OP_INVOKE_TOOL => invoke_tool(context),
        OP_RESUME_INTERACTIVE_TOOL => resume_interactive_tool(&context.request),
        OP_PRESENT_TOOL_RESULT => present_tool_result_service(&context.request),
        OP_PRESENT_ARTIFACT => present_artifact_service(&context.request),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported question tool service operation",
        ),
    }
}

fn list_tools(request: &ServiceRequest) -> ServiceResponse {
    match request.payload_json::<ListToolsRequest>() {
        Ok(ListToolsRequest {}) => json_response(&ToolList {
            tools: vec![question_tool_definition()],
        }),
        Err(error) => ServiceResponse::error("invalid_request", error.to_string()),
    }
}

fn question_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: TOOL_NAME.to_string(),
        description: concat!(
            "Ask the user one or more concise questions during execution. ",
            "Use questions: [...] with canonical field names only. ",
            "Prefer optional questions; set required=true only when genuinely blocked. ",
            "Use control=radio and selection_mode=single for exclusive choices, or ",
            "control=checkbox and selection_mode=multiple for checkboxes. ",
            "Use custom=false only when the answer must be one of the listed options. ",
            "Keep question text concise; put long context in assistant text before calling this tool. ",
            "Default ask aggressiveness is 5/10."
        )
        .to_string(),
        input_schema: canonical_input_schema(),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata {
            aliases: vec!["ask".to_string()],
            compatibility_aliases: vec![ToolCompatibilityAlias::new("opencode", "question")],
            capabilities: vec!["ask_user".to_string(), "interactive_question".to_string()],
            permission_category: Some("read".to_string()),
            argument_extractors: Vec::new(),
        },
        ui: bcode_tool::ToolUiMetadata::default(),
    }
}

fn canonical_input_schema() -> Value {
    json!({
        "type": "object",
        "required": ["questions"],
        "properties": {
            "questions": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "required": ["question"],
                    "properties": {
                        "header": { "type": "string", "description": "Short label for this question" },
                        "question": { "type": "string", "description": "Complete concise question" },
                        "options": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["label"],
                                "properties": {
                                    "label": { "type": "string" },
                                    "value": { "type": "string" },
                                    "description": { "type": "string" }
                                }
                            }
                        },
                        "control": { "type": "string", "enum": ["radio", "checkbox"], "default": "radio" },
                        "selection_mode": { "type": "string", "enum": ["single", "multiple"], "default": "single" },
                        "custom": { "type": "boolean", "default": true },
                        "custom_mode": { "type": "string", "enum": ["exclusive", "additional"], "default": "additional" },
                        "required": { "type": "boolean", "default": false },
                        "multiple": { "type": "boolean", "description": "Compatibility field; prefer selection_mode" }
                    }
                }
            }
        }
    })
}

fn invoke_tool(context: &NativeServiceContext) -> ServiceResponse {
    let invocation = match context.request.payload_json::<ToolInvocationRequest>() {
        Ok(invocation) => invocation,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    if invocation.name != TOOL_NAME && invocation.name != "ask" {
        return json_response(&tool_error(format!(
            "unsupported question tool: {}",
            invocation.name
        )));
    }
    let request = match parse_question_request(invocation.arguments) {
        Ok(request) => request,
        Err(error) => return json_response(&tool_error(error)),
    };
    let outcome = unanswered_outcome(&request);
    let value = match serde_json::to_string(&outcome) {
        Ok(value) => value,
        Err(error) => return json_response(&tool_error(error.to_string())),
    };
    json_response(&ToolInvocationResponse {
        output: format_unanswered_output(&request, DEFAULT_ASK_AGGRESSIVENESS),
        is_error: false,
        content: Vec::new(),
        full_output: Some(match serde_json::to_string_pretty(&request) {
            Ok(output) => output,
            Err(error) => error.to_string(),
        }),
        host_action: Some(
            bcode_tool::ToolInvocationHostAction::InteractiveToolRequest(
                bcode_tool::InteractiveToolRequest {
                    interaction_id: format!("{}-question", invocation.tool_call_id),
                    surface_kind: "bmux.protocol.inline".to_string(),
                    request: component_tree_request(&request),
                    required: request.questions.iter().any(|question| question.required),
                    turn_behavior: bcode_tool::InteractiveToolTurnBehavior::AwaitBeforeContinuing,
                    render_target: bcode_tool::InteractiveToolRenderTarget::TranscriptToolCall,
                },
            ),
        ),
        result: Some(ToolInvocationResult::Json { value }),
    })
}

fn resume_interactive_tool(request: &ServiceRequest) -> ServiceResponse {
    let resume = match request.payload_json::<InteractiveToolResumeRequest>() {
        Ok(resume) => resume,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let question_request = match parse_question_request(resume.original_arguments.clone()) {
        Ok(request) => request,
        Err(error) => return json_response(&tool_error(error)),
    };
    let outcome = match question_outcome_from_resolution(&question_request, resume.resolution) {
        Ok(outcome) => outcome,
        Err(error) => return json_response(&tool_error(error)),
    };
    json_response(&question_response(&outcome))
}

fn question_outcome_from_resolution(
    request: &NormalizedQuestionRequest,
    resolution: InteractiveToolResolution,
) -> Result<QuestionToolOutcome, String> {
    match resolution {
        InteractiveToolResolution::Submitted { payload } => {
            if let Ok(payload) =
                serde_json::from_value::<QuestionResolutionPayload>(payload.clone())
            {
                return Ok(match payload {
                    QuestionResolutionPayload::Answered { questions } => {
                        answered_outcome(request, questions)?
                    }
                    QuestionResolutionPayload::Unanswered => unanswered_outcome(request),
                    QuestionResolutionPayload::Dismissed => dismissed_outcome(request),
                });
            }
            let payload = serde_json::from_value::<ProtocolResolutionPayload>(payload)
                .map_err(|error| format!("invalid protocol response payload: {error}"))?;
            answered_outcome(request, protocol_answers(request, &payload.values))
        }
        InteractiveToolResolution::Aborted { reason, message } => {
            Ok(aborted_outcome(request, reason, message))
        }
    }
}

fn question_response(outcome: &QuestionToolOutcome) -> ToolInvocationResponse {
    let output = format_question_outcome_output(outcome);
    let value = serde_json::to_string(&outcome)
        .unwrap_or_else(|error| json!({"status":"error","error":error.to_string()}).to_string());
    ToolInvocationResponse {
        output,
        is_error: false,
        content: Vec::new(),
        full_output: Some(value.clone()),
        host_action: None,
        result: Some(ToolInvocationResult::Json { value }),
    }
}

fn present_tool_result_service(
    request: &bcode_plugin_sdk::prelude::ServiceRequest,
) -> ServiceResponse {
    let request = match serde_json::from_slice::<ToolResultPresentationRequest>(&request.payload) {
        Ok(request) => request,
        Err(error) => {
            return ServiceResponse::error(
                "invalid_request",
                format!("invalid tool result presentation request: {error}"),
            );
        }
    };
    present_tool_result(&request).map_or_else(
        || json_response(&ToolResultPresentationResponse::default()),
        |presentation| {
            json_response(&ToolResultPresentationResponse {
                presentation: Some(presentation),
            })
        },
    )
}

fn present_artifact_service(
    request: &bcode_plugin_sdk::prelude::ServiceRequest,
) -> ServiceResponse {
    let request = match serde_json::from_slice::<ToolArtifactPresentationRequest>(&request.payload)
    {
        Ok(request) => request,
        Err(error) => {
            return ServiceResponse::error(
                "invalid_request",
                format!("invalid artifact presentation request: {error}"),
            );
        }
    };
    let Some(outcome) = question_outcome_from_artifact(&request) else {
        return json_response(&ToolArtifactPresentationResponse::default());
    };
    json_response(&ToolArtifactPresentationResponse {
        presentation: Some(question_result_presentation(&outcome)),
        state: request.state,
    })
}

fn question_outcome_from_artifact(
    request: &ToolArtifactPresentationRequest,
) -> Option<QuestionToolOutcome> {
    if request.artifact.schema != "bcode.question.outcome" || request.artifact.schema_version != 1 {
        return None;
    }
    serde_json::from_value(request.artifact.metadata.clone()).ok()
}

/// Present a semantic question tool result as a generic component protocol tree.
#[must_use]
pub fn present_tool_result(
    request: &ToolResultPresentationRequest,
) -> Option<ToolPresentationEvent> {
    if request.tool_name != TOOL_NAME {
        return None;
    }
    let outcome = match &request.semantic_result {
        Some(ToolInvocationResult::Json { value }) => serde_json::from_str(value).ok(),
        Some(ToolInvocationResult::Text { text }) => serde_json::from_str(text).ok(),
        Some(
            ToolInvocationResult::ShellRun { .. }
            | ToolInvocationResult::FileChange { .. }
            | ToolInvocationResult::Artifact { .. },
        )
        | None => serde_json::from_str(&request.fallback_result).ok(),
    }?;
    Some(question_result_presentation(&outcome))
}

fn question_result_presentation(outcome: &QuestionToolOutcome) -> ToolPresentationEvent {
    ToolPresentationEvent::Protocol(ToolProtocolPresentation {
        target: ToolPresentationTarget::Result,
        surface_kind: "bmux.protocol.inline.result".to_owned(),
        tree: question_result_component_tree(outcome),
        state: None,
    })
}

fn question_result_component_tree(outcome: &QuestionToolOutcome) -> Value {
    let mut builder = FormBuilder::new("question-result-form");
    for question in &outcome.questions {
        let prompt = question.header.as_ref().map_or_else(
            || question.question.clone(),
            |header| format!("{header}\n{}", question.question),
        );
        builder = builder.text(prompt);
        if question.selected.is_empty() {
            builder = builder.child(
                TextInputProps {
                    value: question.custom.clone().unwrap_or_default(),
                    placeholder: Some(String::new()),
                    label: None,
                    help: None,
                    error: None,
                    required: false,
                    disabled: true,
                }
                .into_box_node(format!("question-{}-answer", question.question_index)),
            );
        } else {
            builder = builder.child(answer_list_node(question));
            if let Some(custom) = &question.custom {
                builder = builder.child(
                    TextInputProps {
                        value: custom.clone(),
                        placeholder: None,
                        label: Some("Custom answer".to_owned()),
                        help: None,
                        error: None,
                        required: false,
                        disabled: true,
                    }
                    .into_box_node(format!(
                        "question-{}-custom-answer",
                        question.question_index
                    )),
                );
            }
        }
    }
    serde_json::to_value(builder.build()).unwrap_or(Value::Null)
}

fn answer_list_node(question: &QuestionOutcome) -> ComponentNode {
    let text = question
        .selected
        .iter()
        .map(|answer| format!("✓ {}", answer.label))
        .collect::<Vec<_>>()
        .join("\n");
    ComponentNode::leaf(ComponentKind::Text { text, align: None })
}

fn protocol_answers(
    request: &NormalizedQuestionRequest,
    values: &Map<String, Value>,
) -> Vec<QuestionAnswerPayload> {
    request
        .questions
        .iter()
        .enumerate()
        .map(|(question_index, question)| {
            let value = values.get(&format!("question-{question_index}"));
            let selected = match component_value_string(value) {
                Some(value) if !question.options.is_empty() => vec![value],
                _ => component_value_list(value),
            };
            let custom = if question.options.is_empty() {
                component_value_string(value)
            } else {
                component_value_string(values.get(&format!("question-{question_index}-custom")))
            };
            QuestionAnswerPayload {
                question_index,
                selected,
                custom,
            }
        })
        .collect()
}

fn component_value_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => Some(value.to_owned()),
        Value::Object(map) => map.get("string").and_then(Value::as_str).map(str::to_owned),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => None,
    }
}

fn component_value_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::Object(map)) => {
            map.get("list")
                .and_then(Value::as_array)
                .map_or_else(Vec::new, |values| {
                    values
                        .iter()
                        .filter_map(|value| {
                            value
                                .as_str()
                                .map(str::to_owned)
                                .or_else(|| component_value_string(Some(value)))
                        })
                        .collect()
                })
        }
        Some(Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) | None => {
            Vec::new()
        }
    }
}

fn answered_outcome(
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

fn dismissed_outcome(request: &NormalizedQuestionRequest) -> QuestionToolOutcome {
    request_status_outcome(
        request,
        QuestionRequestStatus::Unanswered,
        QuestionStatus::Dismissed,
    )
}

fn aborted_outcome(
    request: &NormalizedQuestionRequest,
    _reason: bcode_tool::InteractiveToolAbortReason,
    _message: Option<String>,
) -> QuestionToolOutcome {
    request_status_outcome(
        request,
        QuestionRequestStatus::Unanswered,
        QuestionStatus::Aborted,
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

fn unanswered_outcome(request: &NormalizedQuestionRequest) -> QuestionToolOutcome {
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

fn format_question_outcome_output(outcome: &QuestionToolOutcome) -> String {
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

fn component_tree_request(request: &NormalizedQuestionRequest) -> Value {
    let mut builder = FormBuilder::new("question-form");
    for (index, question) in request.questions.iter().enumerate() {
        let prompt = question.header.as_ref().map_or_else(
            || question.text.clone(),
            |header| format!("{header}\n{}", question.text),
        );
        builder = builder.text(prompt);
        let id = format!("question-{index}");
        let options = question
            .options
            .iter()
            .enumerate()
            .map(|(option_index, option)| {
                ChoiceOptionProps::new(
                    option
                        .value
                        .clone()
                        .unwrap_or_else(|| option_index.to_string()),
                    option.label.clone(),
                )
            })
            .collect::<Vec<_>>();
        let field = if question.selection_mode == QuestionSelectionMode::Multiple {
            CheckboxGroupProps::new(options).into_node(id)
        } else if options.is_empty() {
            TextInputProps::new()
                .placeholder("Type your answer")
                .into_box_node(id)
        } else {
            RadioGroupProps::new(options).into_node(id)
        };
        builder = builder.child(field);
        if question.custom && !question.options.is_empty() {
            builder = builder.child(
                TextInputProps::new()
                    .label("Custom answer")
                    .placeholder("Type a custom answer")
                    .into_box_node(format!("question-{index}-custom")),
            );
        }
    }
    builder = builder.child(action_row_node());
    serde_json::to_value(builder.build()).unwrap_or(Value::Null)
}

fn action_row_node() -> ComponentNode {
    let actions = vec![
        action_value("submit", "Submit"),
        action_value("cancel", "Cancel"),
    ];
    let mut map = std::collections::BTreeMap::new();
    map.insert("actions".to_owned(), ComponentValue::List(actions));
    ComponentNode::component(ACTION_ROW_TYPE_ID, ComponentValue::Map(map)).with_id("actions")
}

fn action_value(id: &str, label: &str) -> ComponentValue {
    let mut map = std::collections::BTreeMap::new();
    map.insert("id".to_owned(), ComponentValue::String(id.to_owned()));
    map.insert("label".to_owned(), ComponentValue::String(label.to_owned()));
    ComponentValue::Map(map)
}

fn format_unanswered_output(request: &NormalizedQuestionRequest, ask_aggressiveness: u8) -> String {
    let mut output = format!(
        "Question prompt is awaiting user input (ask aggressiveness: {ask_aggressiveness}/10)."
    );
    for (index, question) in request.questions.iter().enumerate() {
        let _ = write!(
            output,
            "\n* {}. {}{}",
            index.saturating_add(1),
            question
                .header
                .as_ref()
                .map_or_else(String::new, |header| format!("{header}: ")),
            question.text
        );
    }
    output
}

fn parse_question_request(value: Value) -> Result<NormalizedQuestionRequest, String> {
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
        .ok_or_else(|| {
            format!(
                "Invalid question tool input: question {} is missing `question`.",
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
        Value::String(label) => Ok(QuestionOption {
            label: label.clone(),
            value: None,
            description: None,
        }),
        Value::Object(object) => {
            let label = string_field(object, "label")
                .filter(|value| !value.trim().is_empty())
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

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

const fn tool_error(output: String) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output,
        is_error: true,
        content: Vec::new(),
        full_output: None,
        host_action: None,
        result: None,
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(QuestionPlugin, include_str!("../bcode-plugin.toml"))
}

bcode_plugin_sdk::export_plugin!(QuestionPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_wrapped_schema() {
        let request = parse_question_request(json!({
            "questions": [{
                "header": "Scope",
                "question": "What should I do?",
                "options": [{ "label": "Fix", "value": "fix" }],
                "control": "checkbox",
                "selection_mode": "multiple",
                "custom": false,
                "required": true
            }]
        }))
        .unwrap();
        assert_eq!(request.questions.len(), 1);
        assert_eq!(request.questions[0].control, QuestionControl::Checkbox);
        assert_eq!(
            request.questions[0].selection_mode,
            QuestionSelectionMode::Multiple
        );
        assert!(request.questions[0].required);
    }

    #[test]
    fn parses_single_question_root_and_string_options() {
        let request = parse_question_request(json!({
            "question": "Proceed?",
            "options": ["Yes", "No"]
        }))
        .unwrap();
        assert_eq!(request.questions[0].options[0].label, "Yes");
        assert_eq!(
            request.questions[0].custom_mode,
            QuestionCustomMode::Additional
        );
    }

    #[test]
    fn parses_root_array_and_key_casing() {
        let request = parse_question_request(json!([{
            "Question": "Pick some?",
            "Selection-Mode": "multiple",
            "CustomMode": "exclusive"
        }]))
        .unwrap();
        assert_eq!(
            request.questions[0].selection_mode,
            QuestionSelectionMode::Multiple
        );
        assert_eq!(request.questions[0].control, QuestionControl::Checkbox);
        assert_eq!(
            request.questions[0].custom_mode,
            QuestionCustomMode::Exclusive
        );
    }

    #[test]
    fn parses_multiple_compatibility_field() {
        let request = parse_question_request(json!({
            "questions": [{ "question": "Pick?", "multiple": true }]
        }))
        .unwrap();
        assert_eq!(
            request.questions[0].selection_mode,
            QuestionSelectionMode::Multiple
        );
        assert_eq!(request.questions[0].control, QuestionControl::Checkbox);
    }

    #[test]
    fn rejects_semantic_aliases() {
        let error = parse_question_request(json!({ "prompt": "Unsupported alias?" })).unwrap_err();
        assert!(error.contains("missing `question`"));
    }
}
