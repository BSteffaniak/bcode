//! Native service and registration adapters.
#[cfg(feature = "static-bundled")]
use super::{QUESTION_INLINE_SURFACE, QUESTION_INTERACTION_KIND};
use bcode_plugin_sdk::{
    ConcurrentRustPlugin, NativeServiceContext, RustPlugin, ServiceBridgeRequest,
    ServiceBridgeResponse, ServiceRequest, ServiceResponse, prepare_tool_service_response,
};
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolArtifact,
    ToolCompatibilityAlias, ToolDefinition, ToolExchangeRequest, ToolExchangeResolution,
    ToolExchangeResponsePolicy, ToolInvocationRequest, ToolInvocationResponse,
    ToolInvocationResult, ToolList,
};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Default)]
pub struct QuestionPlugin;

use super::question_controller;
#[cfg(feature = "static-bundled")]
use super::question_interaction;
use super::{
    NormalizedQuestionRequest, QUESTION_EXCHANGE_SCHEMA, QuestionResolutionPayload,
    QuestionToolOutcome, TOOL_NAME, answered_outcome, dismissed_outcome,
    format_question_outcome_output, parse_question_request, unanswered_outcome,
};
#[cfg(feature = "static-bundled")]
use super::{question_outcome_tui, question_tui};

impl ConcurrentRustPlugin for QuestionPlugin {
    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
        invoke_question_service(&context)
    }
}

impl RustPlugin for QuestionPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        invoke_question_service(&context)
    }
}

fn invoke_question_service(context: &NativeServiceContext) -> ServiceResponse {
    match context.request.interface_id.as_str() {
        TOOL_SERVICE_INTERFACE_ID => invoke_tool_service(context),
        "bcode.interaction-controller/v1" => question_controller::invoke(context),
        _ => ServiceResponse::error(
            "unsupported_interface",
            "unsupported question plugin service interface",
        ),
    }
}

fn invoke_tool_service(context: &NativeServiceContext) -> ServiceResponse {
    match context.request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(&context.request),
        bcode_tool::OP_PREPARE_TOOL => prepare_tool_service_response(
            &context.request,
            [question_tool_definition()],
            |_request, _definition| Ok(question_policy_preparation()),
        ),
        OP_INVOKE_TOOL => invoke_tool(context),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported question tool service operation",
        ),
    }
}

fn question_policy_preparation() -> bcode_plugin_sdk::ToolPolicyPreparation {
    bcode_plugin_sdk::ToolPolicyPreparation::read_only().with_identity(
        bcode_plugin_sdk::ToolPolicyIdentity {
            aliases: vec!["ask".to_string()],
            compatibility_aliases: vec![ToolCompatibilityAlias::new("opencode", "question")],
            capabilities: vec!["ask_user".to_string(), "interactive_question".to_string()],
            permission_category: Some("read".to_string()),
        },
    )
}

fn list_tools(request: &ServiceRequest) -> ServiceResponse {
    match request.payload_json::<ListToolsRequest>() {
        Ok(ListToolsRequest {}) => json_response(&ToolList::with_runtime_stable_discovery(
            vec![question_tool_definition()],
            |_| bcode_tool::ToolDiscoveryPolicy {
                capabilities: question_policy_preparation().identity.capabilities,
                ..bcode_tool::ToolDiscoveryPolicy::new(true)
            },
        )),
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
    let exchange = ServiceBridgeRequest::Exchange(ToolExchangeRequest {
        invocation_id: invocation.tool_call_id.clone(),
        exchange_id: format!("{}-question", invocation.tool_call_id),
        producer_id: "bcode.question".to_string(),
        schema: QUESTION_EXCHANGE_SCHEMA.to_string(),
        schema_version: 1,
        payload: serde_json::to_value(&request).unwrap_or(Value::Null),
        response_policy: if request.questions.iter().any(|question| question.required) {
            ToolExchangeResponsePolicy::Required
        } else {
            ToolExchangeResponsePolicy::Optional
        },
    });
    let response = match context.bridge.request(&exchange) {
        Ok(ServiceBridgeResponse::Exchange(response)) => response,
        Ok(_) => {
            return json_response(&tool_error(
                "question exchange returned unexpected bridge response".to_string(),
            ));
        }
        Err(error) => {
            return json_response(&tool_error(format!("question exchange failed: {error}")));
        }
    };
    let outcome = match question_outcome_from_exchange(&request, response) {
        Ok(outcome) => outcome,
        Err(error) => return json_response(&tool_error(error)),
    };
    json_response(&question_response(&invocation.tool_call_id, &outcome))
}

fn question_outcome_from_exchange(
    request: &NormalizedQuestionRequest,
    resolution: ToolExchangeResolution,
) -> Result<QuestionToolOutcome, String> {
    match resolution {
        ToolExchangeResolution::Responded { payload } => {
            let payload = serde_json::from_value::<QuestionResolutionPayload>(payload)
                .map_err(|error| format!("invalid question response payload: {error}"))?;
            Ok(match payload {
                QuestionResolutionPayload::Answered { questions } => {
                    answered_outcome(request, questions)?
                }
                QuestionResolutionPayload::Unanswered => unanswered_outcome(request),
                QuestionResolutionPayload::Dismissed => dismissed_outcome(request),
            })
        }
        ToolExchangeResolution::NoCompatibleConsumer
            if !request.questions.iter().any(|question| question.required) =>
        {
            Ok(unanswered_outcome(request))
        }
        ToolExchangeResolution::Cancelled => Err("question exchange cancelled".to_string()),
        ToolExchangeResolution::TimedOut => Err("question exchange timed out".to_string()),
        ToolExchangeResolution::NoCompatibleConsumer => {
            Err("required question has no compatible consumer".to_string())
        }
        ToolExchangeResolution::ConsumerDetached => {
            Err("question exchange consumer detached".to_string())
        }
        ToolExchangeResolution::Failed { code, message } => {
            Err(format!("question exchange failed ({code}): {message}"))
        }
    }
}

fn question_response(
    interaction_id: &str,
    outcome: &QuestionToolOutcome,
) -> ToolInvocationResponse {
    let output = format_question_outcome_output(outcome);
    let value = serde_json::to_string(&outcome)
        .unwrap_or_else(|error| json!({"status":"error","error":error.to_string()}).to_string());
    ToolInvocationResponse {
        output,
        is_error: false,
        content: Vec::new(),
        full_output: Some(value.clone()),
        result: Some(ToolInvocationResult::Artifact {
            artifact: Box::new(ToolArtifact {
                artifact_id: format!("question-outcome-{interaction_id}"),
                producer_plugin_id: "bcode.question".to_string(),
                schema: "bcode.question.outcome".to_string(),
                schema_version: 1,
                tool_call_id: Some(interaction_id.to_string()),
                title: Some("Question outcome".to_string()),
                metadata: serde_json::from_str(&value).unwrap_or_else(|_| json!({})),
                refs: Vec::new(),
            }),
        }),
    }
}

pub(crate) fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
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
        result: None,
    }
}

#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_concurrent_plugin_vtable!(
        QuestionPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn question_interaction_adapter(
    platform_id: &str,
) -> bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability {
    bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability {
        producer_id: "bcode.question".to_owned(),
        exchange_schema: QUESTION_EXCHANGE_SCHEMA.to_owned(),
        min_schema_version: 1,
        max_schema_version: 1,
        platform_id: platform_id.to_owned(),
        priority: 100,
        interaction_kind: QUESTION_INTERACTION_KIND.to_owned(),
        tui_surface_kind: (platform_id == "tui").then(|| QUESTION_INLINE_SURFACE.to_owned()),
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn question_interaction_registry() -> bcode_plugin_sdk::interaction::PluginInteractionRegistry {
    let mut registry = bcode_plugin_sdk::interaction::PluginInteractionRegistry::default();
    registry.register_interaction::<question_interaction::QuestionInteractionController>();
    registry
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn question_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    question_tui::register_keymap(&mut registry);
    registry.register_interactive_surface::<
        question_interaction::QuestionInteractionController,
        question_tui::QuestionTerminalRenderer,
    >();
    registry.register_visual_adapter(
        ["question-outcome-component"],
        Box::new(question_outcome_tui::QuestionOutcomeTuiVisualAdapter),
    );
    registry
}

#[cfg(all(feature = "dynamic-export", not(feature = "static-bundled")))]
bcode_plugin_sdk::export_concurrent_plugin!(QuestionPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MAX_OPTIONS_PER_QUESTION, MAX_QUESTION_TEXT_BYTES, MAX_QUESTIONS, Question,
        QuestionAnswerPayload, QuestionControl, QuestionCustomMode, QuestionOption,
        QuestionRequestStatus, QuestionSelectionMode, QuestionStatus,
    };

    fn outcome_request() -> NormalizedQuestionRequest {
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
                custom: false,
                custom_mode: QuestionCustomMode::Additional,
                required: false,
            }],
        }
    }

    #[test]
    fn string_only_options_produce_model_visible_labels_and_values() {
        let request = parse_question_request(json!({
            "question": "Which approach?",
            "options": ["Use SQLite", "Use Postgres"]
        }))
        .expect("string-only options");
        let outcome = answered_outcome(
            &request,
            vec![QuestionAnswerPayload {
                question_index: 0,
                selected: vec!["Use SQLite".to_owned()],
                custom: None,
            }],
        )
        .expect("answered outcome");

        assert_eq!(outcome.questions[0].question, "Which approach?");
        assert_eq!(
            outcome.questions[0].selected,
            [SelectedAnswer {
                label: "Use SQLite".to_owned(),
                value: "Use SQLite".to_owned(),
            }]
        );
        let output = format_question_outcome_output(&outcome);
        assert!(output.contains("Which approach?"));
        assert!(output.contains("selected=[Use SQLite]"));
    }

    #[test]
    fn explicit_option_value_remains_distinct_from_model_visible_label() {
        let request = parse_question_request(json!({
            "question": "Which database?",
            "options": [{"label": "Use SQLite", "value": "sqlite"}]
        }))
        .expect("valued option");
        let outcome = answered_outcome(
            &request,
            vec![QuestionAnswerPayload {
                question_index: 0,
                selected: vec!["sqlite".to_owned()],
                custom: None,
            }],
        )
        .expect("answered outcome");

        assert_eq!(
            outcome.questions[0].selected,
            [SelectedAnswer {
                label: "Use SQLite".to_owned(),
                value: "sqlite".to_owned(),
            }]
        );
    }

    #[test]
    fn question_request_bounds_reject_oversized_collections_and_text() {
        let too_many = (0..=MAX_QUESTIONS)
            .map(|index| json!({"question": format!("Question {index}")}))
            .collect::<Vec<_>>();
        assert!(parse_question_request(json!({"questions": too_many})).is_err());

        assert!(
            parse_question_request(json!({
                "question": "x".repeat(MAX_QUESTION_TEXT_BYTES + 1)
            }))
            .is_err()
        );

        let too_many_options = (0..=MAX_OPTIONS_PER_QUESTION)
            .map(|index| format!("Option {index}"))
            .collect::<Vec<_>>();
        assert!(
            parse_question_request(json!({
                "question": "Choose",
                "options": too_many_options
            }))
            .is_err()
        );
    }

    #[test]
    fn question_exchange_defines_optional_and_required_no_consumer_outcomes() {
        let optional = outcome_request();
        let optional_outcome =
            question_outcome_from_exchange(&optional, ToolExchangeResolution::NoCompatibleConsumer)
                .expect("optional question may continue unanswered");
        assert_eq!(optional_outcome.status, QuestionRequestStatus::Unanswered);
        assert!(
            optional_outcome
                .questions
                .iter()
                .all(|question| question.status == QuestionStatus::Unanswered)
        );

        let mut required = optional;
        required.questions[0].required = true;
        assert_eq!(
            question_outcome_from_exchange(&required, ToolExchangeResolution::NoCompatibleConsumer,),
            Err("required question has no compatible consumer".to_owned())
        );
    }

    #[test]
    fn question_exchange_preserves_cancellation_detach_and_failure_semantics() {
        let request = outcome_request();
        assert_eq!(
            question_outcome_from_exchange(&request, ToolExchangeResolution::Cancelled),
            Err("question exchange cancelled".to_owned())
        );
        assert_eq!(
            question_outcome_from_exchange(&request, ToolExchangeResolution::ConsumerDetached),
            Err("question exchange consumer detached".to_owned())
        );
        assert_eq!(
            question_outcome_from_exchange(
                &request,
                ToolExchangeResolution::Failed {
                    code: "route_failed".to_owned(),
                    message: "consumer unavailable".to_owned(),
                },
            ),
            Err("question exchange failed (route_failed): consumer unavailable".to_owned())
        );
    }

    #[test]
    fn question_owner_prepares_catalog_policy_identity() {
        let policy = question_policy_preparation();
        assert!(!policy.requires_permission);
        assert_eq!(policy.identity.aliases, vec!["ask"]);
        assert_eq!(
            policy.identity.compatibility_aliases,
            vec![ToolCompatibilityAlias::new("opencode", "question")]
        );
        assert_eq!(
            policy.identity.capabilities,
            vec!["ask_user", "interactive_question"]
        );
        assert_eq!(policy.identity.permission_category.as_deref(), Some("read"));
    }

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
