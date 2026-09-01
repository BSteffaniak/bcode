//! Conversation projection for the `OpenAI` Responses API.
//!
//! Turns normalized Bcode messages into Responses input items, including the tool-protocol
//! sanitization that keeps `function_call` / `function_call_output` pairs well-formed.
//!
//! This module is provider-neutral. Tool-name mapping is supplied by the caller so provider naming
//! policy stays provider-owned.

use crate::{ResponsesContent, ResponsesInputItem};
use std::collections::BTreeSet;

/// Placeholder output recorded for a tool call that never produced a result.
pub const INTERRUPTED_TOOL_OUTPUT: &str =
    "tool invocation was interrupted before Bcode could persist a result";

/// Build the top-level instruction bundle from the system prompt and system messages.
///
/// The Responses API carries instructions out-of-band rather than as a system message, so system
/// content is concatenated here and system messages project to nothing in the input list.
/// Returns `None` when there is no non-blank system content.
#[must_use]
pub fn response_instruction_bundle(
    system_prompt: Option<&str>,
    messages: &[bcode_model::ModelMessage],
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(system_prompt) = system_prompt.filter(|prompt| !prompt.trim().is_empty()) {
        parts.push(system_prompt.to_string());
    }
    parts.extend(
        messages
            .iter()
            .filter(|message| message.role == bcode_model::MessageRole::System)
            .map(joined_text_content)
            .filter(|text| !text.trim().is_empty()),
    );
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// Concatenate the text blocks of a message, ignoring non-text content.
#[must_use]
pub fn joined_text_content(message: &bcode_model::ModelMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            bcode_model::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render image content as a data URL.
#[must_use]
pub fn image_data_url(image: &bcode_model::ImageContent) -> String {
    format!("data:{};base64,{}", image.mime_type, image.data_base64)
}

/// Project one message into a role-tagged input item with its text and image content.
///
/// `input_text` selects the caller-side (`input_text`) versus model-side (`output_text`) content
/// discriminator. A provider-neutral cache point on the message is projected onto the final
/// cacheable caller content block. Returns an empty vector when the message carries no projectable
/// content.
#[must_use]
pub fn responses_message(
    role: &str,
    message: &bcode_model::ModelMessage,
    input_text: bool,
) -> Vec<ResponsesInputItem> {
    let mut content = Vec::new();
    let text = joined_text_content(message);
    if !text.is_empty() {
        content.push(if input_text {
            ResponsesContent::InputText {
                text,
                prompt_cache_breakpoint: None,
            }
        } else {
            ResponsesContent::OutputText { text }
        });
    }
    for block in &message.content {
        if let bcode_model::ContentBlock::Image { image } = block {
            content.push(ResponsesContent::InputImage {
                image_url: image_data_url(image),
            });
        }
    }
    if content.is_empty() {
        return Vec::new();
    }
    if input_text
        && message
            .content
            .iter()
            .any(|block| matches!(block, bcode_model::ContentBlock::CachePoint { .. }))
        && let Some(ResponsesContent::InputText {
            prompt_cache_breakpoint,
            ..
        }) = content
            .iter_mut()
            .rev()
            .find(|content| matches!(content, ResponsesContent::InputText { .. }))
    {
        *prompt_cache_breakpoint = Some(crate::ResponsesPromptCacheBreakpoint {
            mode: "explicit".to_string(),
        });
    }
    vec![ResponsesInputItem::Message {
        role: role.to_string(),
        content,
    }]
}

/// Project an assistant message, including any tool calls it issued.
///
/// `provider_tool_name` maps a Bcode tool name to the name the provider expects.
#[must_use]
pub fn responses_assistant_items(
    message: &bcode_model::ModelMessage,
    provider_tool_name: &dyn Fn(&str) -> String,
) -> Vec<ResponsesInputItem> {
    let mut items = responses_message("assistant", message, false);
    items.extend(message.content.iter().filter_map(|block| match block {
        bcode_model::ContentBlock::ToolCall { call } => Some(ResponsesInputItem::FunctionCall {
            call_id: call.id.clone(),
            name: provider_tool_name(&call.name),
            arguments: serde_json::to_string(&call.arguments).unwrap_or_default(),
        }),
        _ => None,
    }));
    items
}

/// Describe an image reference returned by a tool call.
///
/// Image references are not inlined as data URLs, so they are rendered as descriptive text
/// including whatever dimensions and size metadata the tool reported.
#[must_use]
pub fn image_ref_text(call_id: &str, image: &bcode_model::ImageRefContent) -> String {
    let dimensions = image
        .metadata
        .width
        .zip(image.metadata.height)
        .map_or_else(String::new, |(width, height)| format!(" {width}x{height}"));
    let byte_len = image
        .metadata
        .byte_len
        .map_or_else(String::new, |byte_len| format!(" {byte_len} bytes"));
    format!(
        "Image reference returned by tool call {call_id}: {} {}{}{}",
        image.path, image.mime_type, dimensions, byte_len
    )
}

/// Project a tool message into function-call outputs, plus follow-up messages for extra content.
///
/// Every `ToolResultContent` variant is projected: inline images become data URLs, image
/// references become descriptive text, and text content becomes a user message.
#[must_use]
pub fn responses_tool_items(message: &bcode_model::ModelMessage) -> Vec<ResponsesInputItem> {
    let mut items = Vec::new();
    for block in &message.content {
        let bcode_model::ContentBlock::ToolResult { result } = block else {
            continue;
        };
        items.push(ResponsesInputItem::FunctionCallOutput {
            call_id: result.call_id.clone(),
            output: result.output.clone(),
        });
        for content in &result.content {
            match content {
                bcode_model::ToolResultContent::Image { image } => {
                    items.push(ResponsesInputItem::Message {
                        role: "user".to_string(),
                        content: vec![
                            ResponsesContent::InputText {
                                text: format!(
                                    "Image content returned by tool call {}:",
                                    result.call_id
                                ),
                                prompt_cache_breakpoint: None,
                            },
                            ResponsesContent::InputImage {
                                image_url: image_data_url(image),
                            },
                        ],
                    });
                }
                bcode_model::ToolResultContent::ImageRef { image } => {
                    items.push(ResponsesInputItem::Message {
                        role: "user".to_string(),
                        content: vec![ResponsesContent::InputText {
                            text: image_ref_text(&result.call_id, image),
                            prompt_cache_breakpoint: None,
                        }],
                    });
                }
                bcode_model::ToolResultContent::Text { text } => {
                    items.push(ResponsesInputItem::Message {
                        role: "user".to_string(),
                        content: vec![ResponsesContent::InputText {
                            text: text.clone(),
                            prompt_cache_breakpoint: None,
                        }],
                    });
                }
            }
        }
    }
    items
}

/// Append placeholder outputs for tool calls that never produced a result.
///
/// Providers reject a `function_call` without a matching `function_call_output`, so an interrupted
/// call must be closed before any later item is appended.
pub fn append_missing_responses_tool_outputs(
    input: &mut Vec<ResponsesInputItem>,
    pending_tool_call_ids: &mut BTreeSet<String>,
) {
    input.extend(
        std::mem::take(pending_tool_call_ids)
            .into_iter()
            .map(|call_id| ResponsesInputItem::FunctionCallOutput {
                call_id,
                output: INTERRUPTED_TOOL_OUTPUT.to_string(),
            }),
    );
}

/// Append one input item, repairing structural tool-protocol violations.
///
/// Duplicated tool calls and orphaned tool results are downgraded to plain messages rather than
/// emitted as structured items, because providers reject malformed call/output pairings. Any
/// pending call is closed with a placeholder output before an unrelated item is appended.
pub fn push_sanitized_responses_input_item(
    input: &mut Vec<ResponsesInputItem>,
    seen_tool_call_ids: &mut BTreeSet<String>,
    pending_tool_call_ids: &mut BTreeSet<String>,
    item: ResponsesInputItem,
) {
    match item {
        ResponsesInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        } => {
            if !seen_tool_call_ids.insert(call_id.clone()) {
                append_missing_responses_tool_outputs(input, pending_tool_call_ids);
                input.push(ResponsesInputItem::Message {
                    role: "user".to_string(),
                    content: vec![ResponsesContent::InputText {
                        text: format!(
                            "Historical assistant tool call omitted from structured tool protocol because its call id was duplicated. Call id: {call_id}; tool: {name}; arguments: {arguments}"
                        ),
                        prompt_cache_breakpoint: None,
                    }],
                });
                return;
            }
            pending_tool_call_ids.insert(call_id.clone());
            input.push(ResponsesInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            });
        }
        ResponsesInputItem::FunctionCallOutput { call_id, output } => {
            if pending_tool_call_ids.remove(&call_id) {
                input.push(ResponsesInputItem::FunctionCallOutput { call_id, output });
            } else {
                append_missing_responses_tool_outputs(input, pending_tool_call_ids);
                input.push(ResponsesInputItem::Message {
                    role: "user".to_string(),
                    content: vec![ResponsesContent::InputText {
                        text: format!(
                            "Historical tool result omitted from structured tool protocol because its matching assistant tool call is unavailable. Call id: {call_id}; result: {output}"
                        ),
                        prompt_cache_breakpoint: None,
                    }],
                });
            }
        }
        ResponsesInputItem::Message { role, content } => {
            append_missing_responses_tool_outputs(input, pending_tool_call_ids);
            input.push(ResponsesInputItem::Message { role, content });
        }
        ResponsesInputItem::Reasoning {
            id,
            summary,
            encrypted_content,
        } => {
            append_missing_responses_tool_outputs(input, pending_tool_call_ids);
            input.push(ResponsesInputItem::Reasoning {
                id,
                summary,
                encrypted_content,
            });
        }
        ResponsesInputItem::Compaction {
            id,
            encrypted_content,
            created_by,
        } => {
            append_missing_responses_tool_outputs(input, pending_tool_call_ids);
            input.push(ResponsesInputItem::Compaction {
                id,
                encrypted_content,
                created_by,
            });
        }
    }
}

/// Project one message into input items, honoring provider-extension passthrough.
///
/// A message carrying `ProviderExtension` blocks that decode as input items is replayed verbatim,
/// which preserves opaque provider state such as reasoning items.
#[must_use]
pub fn model_message_to_responses_input(
    message: &bcode_model::ModelMessage,
    provider_tool_name: &dyn Fn(&str) -> String,
) -> Vec<ResponsesInputItem> {
    let extension_items = message
        .content
        .iter()
        .filter_map(|block| match block {
            bcode_model::ContentBlock::ProviderExtension { value } => {
                serde_json::from_value::<ResponsesInputItem>(value.clone()).ok()
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if !extension_items.is_empty() {
        return extension_items;
    }
    match message.role {
        bcode_model::MessageRole::System => Vec::new(),
        bcode_model::MessageRole::User => responses_message("user", message, true),
        bcode_model::MessageRole::Assistant => {
            responses_assistant_items(message, provider_tool_name)
        }
        bcode_model::MessageRole::Tool => responses_tool_items(message),
    }
}

/// Project a message slice into sanitized Responses input items.
///
/// `start` skips already-transmitted history when the caller reuses server-side conversation
/// state; pass `0` to project the full history.
#[must_use]
pub fn model_messages_to_responses_input(
    messages: &[bcode_model::ModelMessage],
    start: usize,
    provider_tool_name: &dyn Fn(&str) -> String,
) -> Vec<ResponsesInputItem> {
    let mut input = Vec::new();
    let mut seen_tool_call_ids = BTreeSet::new();
    let mut pending_tool_call_ids = BTreeSet::new();
    for message in messages.iter().skip(start.min(messages.len())) {
        for item in model_message_to_responses_input(message, provider_tool_name) {
            push_sanitized_responses_input_item(
                &mut input,
                &mut seen_tool_call_ids,
                &mut pending_tool_call_ids,
                item,
            );
        }
    }
    append_missing_responses_tool_outputs(&mut input, &mut pending_tool_call_ids);
    input
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_message(role: bcode_model::MessageRole, text: &str) -> bcode_model::ModelMessage {
        bcode_model::ModelMessage {
            role,
            content: vec![bcode_model::ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    fn identity(name: &str) -> String {
        name.to_string()
    }

    #[test]
    fn cache_point_projects_to_the_message_input_text() {
        let message = bcode_model::ModelMessage {
            role: bcode_model::MessageRole::User,
            content: vec![
                bcode_model::ContentBlock::Text {
                    text: "stable prefix".to_string(),
                },
                bcode_model::ContentBlock::CachePoint {
                    hint: bcode_model::PromptCachePoint::default(),
                },
            ],
        };

        let input = model_message_to_responses_input(&message, &identity);
        assert!(matches!(
            &input[0],
            ResponsesInputItem::Message { content, .. }
                if matches!(
                    &content[0],
                    ResponsesContent::InputText {
                        prompt_cache_breakpoint: Some(_),
                        ..
                    }
                )
        ));
    }

    #[test]
    fn instruction_bundle_joins_system_prompt_and_system_messages() {
        let messages = vec![
            text_message(bcode_model::MessageRole::System, "second"),
            text_message(bcode_model::MessageRole::User, "ignored"),
            text_message(bcode_model::MessageRole::System, "third"),
        ];

        assert_eq!(
            response_instruction_bundle(Some("first"), &messages).as_deref(),
            Some("first\n\nsecond\n\nthird")
        );
        // Blank prompts and blank system messages are skipped entirely.
        assert_eq!(
            response_instruction_bundle(Some("   "), &messages).as_deref(),
            Some("second\n\nthird")
        );
        assert_eq!(response_instruction_bundle(None, &[]), None);
        assert_eq!(
            response_instruction_bundle(
                None,
                &[text_message(bcode_model::MessageRole::System, "  ")]
            ),
            None
        );
    }

    #[test]
    fn text_blocks_join_with_newlines_and_ignore_other_content() {
        let message = bcode_model::ModelMessage {
            role: bcode_model::MessageRole::User,
            content: vec![
                bcode_model::ContentBlock::Text {
                    text: "a".to_string(),
                },
                bcode_model::ContentBlock::Image {
                    image: bcode_model::ImageContent {
                        mime_type: "image/png".to_string(),
                        data_base64: "AAAA".to_string(),
                        metadata: bcode_model::ImageMetadata::default(),
                    },
                },
                bcode_model::ContentBlock::Text {
                    text: "b".to_string(),
                },
            ],
        };
        assert_eq!(joined_text_content(&message), "a\nb");
    }

    #[test]
    fn images_render_as_data_urls() {
        let image = bcode_model::ImageContent {
            mime_type: "image/webp".to_string(),
            data_base64: "QUJD".to_string(),
            metadata: bcode_model::ImageMetadata::default(),
        };
        assert_eq!(image_data_url(&image), "data:image/webp;base64,QUJD");
    }

    #[test]
    fn system_messages_project_to_nothing() {
        let message = text_message(bcode_model::MessageRole::System, "instructions");
        assert!(model_message_to_responses_input(&message, &identity).is_empty());
    }

    #[test]
    fn empty_messages_project_to_nothing() {
        let message = bcode_model::ModelMessage {
            role: bcode_model::MessageRole::User,
            content: Vec::new(),
        };
        assert!(responses_message("user", &message, true).is_empty());
    }

    #[test]
    fn assistant_tool_calls_use_the_provider_tool_name() {
        let message = bcode_model::ModelMessage {
            role: bcode_model::MessageRole::Assistant,
            content: vec![bcode_model::ContentBlock::ToolCall {
                call: bcode_model::ToolCall {
                    id: "call_1".to_string(),
                    name: "filesystem.read".to_string(),
                    arguments: serde_json::json!({"path": "a"}),
                },
            }],
        };

        let items = model_message_to_responses_input(&message, &|name| name.replace('.', "_"));
        assert!(matches!(
            &items[0],
            ResponsesInputItem::FunctionCall { call_id, name, .. }
                if call_id == "call_1" && name == "filesystem_read"
        ));
    }

    #[test]
    fn interrupted_tool_calls_are_closed_before_later_items() {
        let assistant = bcode_model::ModelMessage {
            role: bcode_model::MessageRole::Assistant,
            content: vec![bcode_model::ContentBlock::ToolCall {
                call: bcode_model::ToolCall {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({}),
                },
            }],
        };
        let later = text_message(bcode_model::MessageRole::User, "next");

        let input = model_messages_to_responses_input(&[assistant, later], 0, &identity);

        // The unmatched call must be closed with a placeholder before the user message.
        assert!(matches!(&input[0], ResponsesInputItem::FunctionCall { .. }));
        assert!(matches!(
            &input[1],
            ResponsesInputItem::FunctionCallOutput { call_id, output }
                if call_id == "call_1" && output == INTERRUPTED_TOOL_OUTPUT
        ));
        assert!(matches!(&input[2], ResponsesInputItem::Message { .. }));
    }

    #[test]
    fn duplicate_tool_call_ids_are_downgraded_to_messages() {
        let mut input = Vec::new();
        let mut seen = BTreeSet::new();
        let mut pending = BTreeSet::new();
        let call = || ResponsesInputItem::FunctionCall {
            call_id: "dup".to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
        };

        push_sanitized_responses_input_item(&mut input, &mut seen, &mut pending, call());
        push_sanitized_responses_input_item(&mut input, &mut seen, &mut pending, call());

        assert!(matches!(&input[0], ResponsesInputItem::FunctionCall { .. }));
        // The duplicate closes the pending call, then degrades to a plain message.
        assert!(matches!(
            &input[1],
            ResponsesInputItem::FunctionCallOutput { call_id, .. } if call_id == "dup"
        ));
        assert!(matches!(&input[2], ResponsesInputItem::Message { .. }));
    }

    #[test]
    fn orphaned_tool_results_are_downgraded_to_messages() {
        let mut input = Vec::new();
        let mut seen = BTreeSet::new();
        let mut pending = BTreeSet::new();

        push_sanitized_responses_input_item(
            &mut input,
            &mut seen,
            &mut pending,
            ResponsesInputItem::FunctionCallOutput {
                call_id: "orphan".to_string(),
                output: "result".to_string(),
            },
        );

        assert_eq!(input.len(), 1);
        assert!(matches!(&input[0], ResponsesInputItem::Message { .. }));
    }

    #[test]
    fn every_tool_result_content_variant_is_projected() {
        // Regression guard: an earlier refactor silently dropped the `ImageRef` and `Text`
        // variants, which would have lost tool output without any error.
        let message = bcode_model::ModelMessage {
            role: bcode_model::MessageRole::Tool,
            content: vec![bcode_model::ContentBlock::ToolResult {
                result: bcode_model::ToolResult {
                    call_id: "call_1".to_string(),
                    output: "primary".to_string(),
                    content: vec![
                        bcode_model::ToolResultContent::Image {
                            image: bcode_model::ImageContent {
                                mime_type: "image/png".to_string(),
                                data_base64: "AAAA".to_string(),
                                metadata: bcode_model::ImageMetadata::default(),
                            },
                        },
                        bcode_model::ToolResultContent::ImageRef {
                            image: bcode_model::ImageRefContent {
                                path: "/tmp/shot.png".to_string(),
                                mime_type: "image/png".to_string(),
                                artifact_id: None,
                                reference_key: None,
                                metadata: bcode_model::ImageMetadata {
                                    width: Some(800),
                                    height: Some(600),
                                    byte_len: Some(2048),
                                    ..bcode_model::ImageMetadata::default()
                                },
                            },
                        },
                        bcode_model::ToolResultContent::Text {
                            text: "extra".to_string(),
                        },
                    ],
                    is_error: false,
                },
            }],
        };

        let items = responses_tool_items(&message);
        assert_eq!(items.len(), 4, "output plus one item per content variant");
        assert!(matches!(
            &items[0],
            ResponsesInputItem::FunctionCallOutput { output, .. } if output == "primary"
        ));

        let rendered = serde_json::to_string(&items).expect("items serialize");
        assert!(rendered.contains("base64,AAAA"), "inline image is inlined");
        assert!(
            rendered.contains("/tmp/shot.png") && rendered.contains("800x600"),
            "image reference is described: {rendered}"
        );
        assert!(rendered.contains("extra"), "text content is preserved");
    }

    #[test]
    fn image_reference_text_omits_absent_metadata() {
        let image = bcode_model::ImageRefContent {
            path: "/tmp/a.png".to_string(),
            mime_type: "image/png".to_string(),
            artifact_id: None,
            reference_key: None,
            metadata: bcode_model::ImageMetadata::default(),
        };
        let text = image_ref_text("call_9", &image);
        assert_eq!(
            text,
            "Image reference returned by tool call call_9: /tmp/a.png image/png"
        );
    }

    #[test]
    fn matched_tool_pairs_are_preserved_verbatim() {
        let assistant = bcode_model::ModelMessage {
            role: bcode_model::MessageRole::Assistant,
            content: vec![bcode_model::ContentBlock::ToolCall {
                call: bcode_model::ToolCall {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({}),
                },
            }],
        };
        let tool = bcode_model::ModelMessage {
            role: bcode_model::MessageRole::Tool,
            content: vec![bcode_model::ContentBlock::ToolResult {
                result: bcode_model::ToolResult {
                    call_id: "call_1".to_string(),
                    output: "contents".to_string(),
                    content: Vec::new(),
                    is_error: false,
                },
            }],
        };

        let input = model_messages_to_responses_input(&[assistant, tool], 0, &identity);
        assert_eq!(input.len(), 2);
        assert!(matches!(&input[0], ResponsesInputItem::FunctionCall { .. }));
        assert!(matches!(
            &input[1],
            ResponsesInputItem::FunctionCallOutput { call_id, output }
                if call_id == "call_1" && output == "contents"
        ));
    }

    #[test]
    fn provider_extension_blocks_replay_verbatim() {
        let message = bcode_model::ModelMessage {
            role: bcode_model::MessageRole::Assistant,
            content: vec![bcode_model::ContentBlock::ProviderExtension {
                value: serde_json::json!({
                    "type": "reasoning",
                    "id": "rs_1",
                    "encrypted_content": "opaque"
                }),
            }],
        };

        let items = model_message_to_responses_input(&message, &identity);
        assert!(matches!(
            &items[0],
            ResponsesInputItem::Reasoning { id, encrypted_content, .. }
                if id == "rs_1" && encrypted_content == "opaque"
        ));
    }

    #[test]
    fn start_index_skips_already_transmitted_history() {
        let messages = vec![
            text_message(bcode_model::MessageRole::User, "first"),
            text_message(bcode_model::MessageRole::User, "second"),
        ];

        let input = model_messages_to_responses_input(&messages, 1, &identity);
        assert_eq!(input.len(), 1);

        // An out-of-range start is clamped rather than panicking.
        assert!(model_messages_to_responses_input(&messages, 99, &identity).is_empty());
    }
}
