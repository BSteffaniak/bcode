#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! `OpenAI` Responses API wire format types.
//!
//! This crate owns the serialized request shapes used by the `OpenAI` Responses API
//! (`POST /responses`) so multiple provider integrations can share one implementation of the wire
//! format instead of duplicating it.
//!
//! # Scope
//!
//! These are portable data types plus lightweight helpers on owned values. Provider behavior
//! stays with the provider integrations that consume this crate, including:
//!
//! * authentication and credential resolution
//! * endpoint/base-URL construction
//! * HTTP transport, streaming, and retry interpretation
//! * conversation-reuse and provider-state policy
//! * any dialect-specific request shaping decisions
//!
//! Callers describe the request variations they need through [`ResponsesRequestCapabilities`]
//! rather than exposing their own configuration types here.

use serde::{Deserialize, Serialize};

pub use decode::{
    ReasoningItemAccumulator, ResponsesStreamLine, ToolCallAccumulator,
    classify_responses_stream_line, drain_complete_stream_lines, ensure_reasoning_activity_started,
    parse_tool_arguments, process_responses_function_arguments_delta,
    process_responses_function_arguments_done, process_responses_output_item,
    process_responses_reasoning_delta, process_responses_reasoning_done,
    process_responses_reasoning_output_item, reasoning_output_index, reported_output_index,
    responses_event_type, responses_incomplete_reason, responses_output_item_text,
    responses_text_delta, tool_call_output_index,
};
mod decode;

/// Terminal outcome of decoding one streamed provider response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutcome {
    /// The provider completed the turn with assistant output.
    Finished,
    /// The provider requested one or more tool calls.
    ToolCall,
    /// The provider stopped because the output token limit was reached.
    MaxTokens,
    /// The turn was cancelled before reaching a provider-reported terminal state.
    Cancelled,
}

impl StreamOutcome {
    /// Whether this outcome is a provider-reported terminal state.
    ///
    /// [`Self::Cancelled`] is not terminal in this sense: it means decoding stopped without the
    /// provider reporting an outcome, so callers continue reading or unwind instead of committing
    /// a result.
    #[must_use]
    pub const fn is_provider_terminal(self) -> bool {
        matches!(self, Self::Finished | Self::ToolCall | Self::MaxTokens)
    }
}

/// Sink for provider turn events produced while decoding a Responses stream.
///
/// Stream decoding needs to report semantic events as they are parsed, but the concrete turn
/// runtime — including output-position allocation and cancellation — is owned by the provider
/// runtime rather than by this crate. Provider integrations implement this trait over their own
/// turn state so decoding stays free of runtime ownership.
///
/// Implementations must tolerate being called from a streaming decode loop and must not block.
pub trait ResponsesEventSink {
    /// Report one decoded provider turn event.
    fn push(&self, event: bcode_model::ProviderTurnEvent);
}

impl<T> ResponsesEventSink for &T
where
    T: ResponsesEventSink + ?Sized,
{
    fn push(&self, event: bcode_model::ProviderTurnEvent) {
        (**self).push(event);
    }
}

/// Request-shaping capabilities for one Responses deployment.
///
/// Provider integrations differ in which Responses features an endpoint accepts. Rather than
/// exposing provider-private configuration or dialect enums to this crate, each integration
/// resolves its own configuration into this neutral description of the request shape it needs.
///
/// Every field describes an observable property of the request, not the identity of a provider.
/// Do not add provider-, product-, or dialect-named fields here.
///
/// These flags are genuinely independent request-shape facts rather than a state machine, so they
/// are modeled the same way as `CatalogCapabilities` in `bcode_model_catalog_models`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResponsesRequestCapabilities {
    /// Endpoint accepts `previous_response_id` for server-side conversation reuse.
    pub supports_previous_response_id: bool,
    /// Endpoint accepts a `parallel_tool_calls` directive.
    pub supports_parallel_tool_calls: bool,
    /// Endpoint accepts a `prompt_cache_key` partition key.
    pub supports_prompt_cache_key: bool,
    /// Endpoint accepts an explicit `max_output_tokens` ceiling.
    pub supports_max_output_tokens: bool,
    /// Endpoint accepts a `text.verbosity` directive.
    pub supports_text_verbosity: bool,
    /// Endpoint accepts a `reasoning.context` directive.
    pub supports_reasoning_context: bool,
    /// Endpoint requires an explicit `strict` flag on tool definitions.
    ///
    /// When `false`, tool definitions omit `strict` entirely.
    pub requires_explicit_tool_strictness: bool,
    /// Endpoint replays opaque provider reasoning items as conversation input.
    pub replays_provider_reasoning_items: bool,
    /// Endpoint projects previously reused history instead of resending it.
    pub projects_reused_history: bool,
}

impl ResponsesRequestCapabilities {
    /// Capabilities for a deployment that accepts the documented public Responses request shape.
    ///
    /// This is the baseline for API-key style deployments: an explicit output ceiling, parallel
    /// tool calls, and explicit tool strictness are all accepted, while provider-state replay is
    /// not used.
    #[must_use]
    pub const fn public_responses_api() -> Self {
        Self {
            supports_previous_response_id: true,
            supports_parallel_tool_calls: true,
            supports_prompt_cache_key: false,
            supports_max_output_tokens: true,
            supports_text_verbosity: false,
            supports_reasoning_context: false,
            requires_explicit_tool_strictness: true,
            replays_provider_reasoning_items: false,
            projects_reused_history: true,
        }
    }
}

/// Streamed request body for the `OpenAI` Responses API.
///
/// Field order matches the documented request shape. Optional fields are omitted entirely when
/// unset so provider endpoints that reject explicit nulls behave correctly.
#[derive(Debug, Serialize)]
pub struct ResponsesRequest {
    /// Provider-native model id.
    pub model: String,
    /// Top-level instruction bundle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Ordered conversation input items.
    pub input: Vec<ResponsesInputItem>,
    /// Whether the response is streamed.
    pub stream: bool,
    /// Whether the provider should persist the response.
    pub store: bool,
    /// Prior response id, when reusing server-side conversation state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    /// Server-side context management directives.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_management: Vec<ResponsesContextManagement>,
    /// Callable tools exposed to the model.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ResponsesTool>,
    /// Tool-choice directive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    /// Whether parallel tool calls are permitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Text output options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<ResponsesTextOptions>,
    /// Reasoning options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ResponsesReasoningOptions>,
    /// Additional response fields to include.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<&'static str>,
    /// Prompt-cache partition key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Output token ceiling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Nucleus sampling probability mass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
}

/// Server-side context management directive.
#[derive(Debug, Serialize)]
pub struct ResponsesContextManagement {
    /// Directive discriminator, for example `compaction`.
    pub r#type: &'static str,
    /// Token threshold at which the provider compacts context.
    pub compact_threshold: u64,
}

/// Reasoning controls for a Responses request.
#[derive(Debug, Serialize)]
pub struct ResponsesReasoningOptions {
    /// Requested reasoning effort.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Requested reasoning summary verbosity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Reasoning context directive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<&'static str>,
}

/// Text output controls for a Responses request.
#[derive(Debug, Serialize)]
pub struct ResponsesTextOptions {
    /// Structured output format, when constrained.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<ResponsesTextFormat>,
    /// Output verbosity directive.
    pub verbosity: &'static str,
}

/// Structured output schema for a Responses request.
#[derive(Debug, Serialize)]
pub struct ResponsesTextFormat {
    /// Format discriminator, for example `json_schema`.
    pub r#type: &'static str,
    /// Schema name.
    pub name: String,
    /// JSON schema value.
    pub schema: serde_json::Value,
    /// Whether the provider must enforce the schema strictly.
    pub strict: bool,
}

/// One conversation input item.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesInputItem {
    /// Role-tagged message.
    Message {
        /// Message role.
        role: String,
        /// Message content parts.
        content: Vec<ResponsesContent>,
    },
    /// Model-issued tool call.
    FunctionCall {
        /// Correlation id for the call.
        call_id: String,
        /// Tool name.
        name: String,
        /// Serialized JSON arguments.
        arguments: String,
    },
    /// Result of a tool call.
    FunctionCallOutput {
        /// Correlation id of the originating call.
        call_id: String,
        /// Serialized tool output.
        output: String,
    },
    /// Replayed provider reasoning item.
    Reasoning {
        /// Provider reasoning item id.
        id: String,
        /// Reasoning summary parts.
        #[serde(default)]
        summary: Vec<ResponsesReasoningSummary>,
        /// Opaque provider reasoning payload.
        encrypted_content: String,
    },
    /// Replayed provider compaction item.
    Compaction {
        /// Provider compaction item id.
        id: String,
        /// Opaque provider compaction payload.
        encrypted_content: String,
        /// Producer of the compaction item, when reported.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_by: Option<String>,
    },
}

/// One reasoning summary part.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesReasoningSummary {
    /// Plain-text reasoning summary.
    SummaryText {
        /// Summary text.
        text: String,
    },
}

/// One message content part.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesContent {
    /// Caller-supplied text.
    InputText {
        /// Text value.
        text: String,
    },
    /// Model-produced text.
    OutputText {
        /// Text value.
        text: String,
    },
    /// Caller-supplied image reference.
    InputImage {
        /// Image URL or data URL.
        image_url: String,
    },
}

/// One callable tool definition.
#[derive(Debug, Serialize)]
pub struct ResponsesTool {
    /// Tool discriminator, for example `function`.
    pub r#type: &'static str,
    /// Tool name as exposed to the model.
    pub name: String,
    /// Tool description.
    pub description: String,
    /// JSON schema for tool parameters.
    pub parameters: serde_json::Value,
    /// Whether the provider must enforce the schema strictly.
    pub strict: Option<bool>,
}

/// Decoded provider-native web search response body.
#[derive(Debug, Deserialize)]
pub struct ResponsesNativeSearchBody {
    /// Output items returned by the provider.
    #[serde(default)]
    pub output: Vec<ResponsesNativeSearchOutputItem>,
}

/// One provider-native web search output item.
#[derive(Debug, Deserialize)]
pub struct ResponsesNativeSearchOutputItem {
    /// Content parts for this output item.
    #[serde(default)]
    pub content: Vec<ResponsesNativeSearchContentItem>,
}

/// One provider-native web search content part.
#[derive(Debug, Deserialize)]
pub struct ResponsesNativeSearchContentItem {
    /// Content text, when present.
    #[serde(default)]
    pub text: Option<String>,
    /// Citation annotations attached to this content part.
    #[serde(default)]
    pub annotations: Vec<ResponsesNativeSearchAnnotation>,
}

/// One provider-native web search citation annotation.
#[derive(Debug, Deserialize)]
pub struct ResponsesNativeSearchAnnotation {
    /// Annotation discriminator.
    #[serde(default)]
    pub r#type: String,
    /// Cited document title.
    #[serde(default)]
    pub title: Option<String>,
    /// Cited document URL.
    #[serde(default)]
    pub url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_provider_reported_outcomes_are_terminal() {
        // Cancellation means decoding stopped without a provider outcome, so it must not be
        // treated as terminal — callers keep reading or unwind instead of committing a result.
        assert!(StreamOutcome::Finished.is_provider_terminal());
        assert!(StreamOutcome::ToolCall.is_provider_terminal());
        assert!(StreamOutcome::MaxTokens.is_provider_terminal());
        assert!(!StreamOutcome::Cancelled.is_provider_terminal());
    }

    #[test]
    fn event_sink_is_implemented_for_references_and_receives_events_in_order() {
        struct Recorder {
            events: std::cell::RefCell<Vec<bcode_model::ProviderTurnEvent>>,
        }

        impl ResponsesEventSink for Recorder {
            fn push(&self, event: bcode_model::ProviderTurnEvent) {
                self.events.borrow_mut().push(event);
            }
        }

        fn emit(sink: &impl ResponsesEventSink) {
            sink.push(bcode_model::ProviderTurnEvent::TextDelta {
                text: "a".to_string(),
            });
            sink.push(bcode_model::ProviderTurnEvent::TextDelta {
                text: "b".to_string(),
            });
        }

        let recorder = Recorder {
            events: std::cell::RefCell::new(Vec::new()),
        };
        // Exercises the blanket reference impl, which lets decode helpers accept `&Sink`.
        emit(&&recorder);

        let events = recorder.events.borrow();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            bcode_model::ProviderTurnEvent::TextDelta { text } if text == "a"
        ));
        assert!(matches!(
            &events[1],
            bcode_model::ProviderTurnEvent::TextDelta { text } if text == "b"
        ));
    }

    #[test]
    fn unset_optional_request_fields_are_omitted_entirely() {
        let request = ResponsesRequest {
            model: "test-model".to_string(),
            instructions: None,
            input: Vec::new(),
            stream: true,
            store: false,
            previous_response_id: None,
            context_management: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            parallel_tool_calls: None,
            text: None,
            reasoning: None,
            include: Vec::new(),
            prompt_cache_key: None,
            temperature: None,
            max_output_tokens: None,
            top_p: None,
        };

        let value = serde_json::to_value(&request).expect("serialize request");
        let object = value.as_object().expect("request is an object");

        // Endpoints reject explicit nulls for these fields, so they must be absent rather than
        // serialized as `null`.
        for field in [
            "instructions",
            "previous_response_id",
            "context_management",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "text",
            "reasoning",
            "include",
            "prompt_cache_key",
            "temperature",
            "max_output_tokens",
            "top_p",
        ] {
            assert!(!object.contains_key(field), "{field} must be omitted");
        }
        assert_eq!(
            object.get("model").and_then(serde_json::Value::as_str),
            Some("test-model")
        );
        assert_eq!(
            object.get("stream").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            object.get("store").and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert!(object.contains_key("input"));
    }

    #[test]
    fn input_items_use_type_tagged_snake_case_wire_names() {
        let items = vec![
            ResponsesInputItem::Message {
                role: "user".to_string(),
                content: vec![ResponsesContent::InputText {
                    text: "hi".to_string(),
                }],
            },
            ResponsesInputItem::FunctionCall {
                call_id: "call_1".to_string(),
                name: "read".to_string(),
                arguments: "{}".to_string(),
            },
            ResponsesInputItem::FunctionCallOutput {
                call_id: "call_1".to_string(),
                output: "done".to_string(),
            },
            ResponsesInputItem::Reasoning {
                id: "rs_1".to_string(),
                summary: vec![ResponsesReasoningSummary::SummaryText {
                    text: "thinking".to_string(),
                }],
                encrypted_content: "opaque".to_string(),
            },
            ResponsesInputItem::Compaction {
                id: "cp_1".to_string(),
                encrypted_content: "opaque".to_string(),
                created_by: None,
            },
        ];

        let value = serde_json::to_value(&items).expect("serialize input items");
        let encoded = value.as_array().expect("input items are an array");
        let types = encoded
            .iter()
            .map(|item| item.get("type").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            vec![
                Some("message"),
                Some("function_call"),
                Some("function_call_output"),
                Some("reasoning"),
                Some("compaction"),
            ]
        );

        // `created_by` is omitted when absent.
        assert!(
            !encoded[4]
                .as_object()
                .expect("compaction item is an object")
                .contains_key("created_by")
        );

        let decoded: Vec<ResponsesInputItem> =
            serde_json::from_value(value).expect("input items round-trip");
        assert_eq!(decoded.len(), items.len());
    }

    #[test]
    fn content_parts_use_documented_wire_names() {
        let value = serde_json::to_value(vec![
            ResponsesContent::InputText {
                text: "a".to_string(),
            },
            ResponsesContent::OutputText {
                text: "b".to_string(),
            },
            ResponsesContent::InputImage {
                image_url: "https://example.invalid/i.png".to_string(),
            },
        ])
        .expect("serialize content");
        let types = value
            .as_array()
            .expect("content is an array")
            .iter()
            .map(|item| item.get("type").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            vec![Some("input_text"), Some("output_text"), Some("input_image")]
        );
    }

    #[test]
    fn native_search_body_tolerates_absent_collections() {
        let body: ResponsesNativeSearchBody =
            serde_json::from_str("{}").expect("empty body decodes");
        assert!(body.output.is_empty());

        let body: ResponsesNativeSearchBody =
            serde_json::from_str(r#"{"output":[{"content":[{"text":"t"}]}]}"#)
                .expect("body decodes");
        assert_eq!(body.output.len(), 1);
        assert_eq!(body.output[0].content.len(), 1);
        assert_eq!(body.output[0].content[0].text.as_deref(), Some("t"));
        assert!(body.output[0].content[0].annotations.is_empty());
    }
}
