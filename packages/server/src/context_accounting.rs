//! Context accounting helpers for model requests and structural compaction.
//!
//! Proactive compaction uses a conservative estimate of the complete candidate request assembled
//! for the current round. Structural retention uses canonical serialized model-message accounting
//! so cuts include protocol overhead rather than raw content character counts.

use super::{
    ContentBlock, ModelMessage, ModelTurnRequest, session_events_to_model_messages_with_limit,
};

/// Current version of the local semantic request-context estimator.
pub const LOCAL_CONTEXT_ESTIMATOR_VERSION: u16 = 1;

/// Estimate the complete semantic model-visible request context.
///
/// Host metadata, prompt-cache controls, continuation identifiers, and opaque provider state are
/// excluded because they are not semantic active context.
pub fn local_request_estimate(
    request: &ModelTurnRequest,
) -> bcode_session_models::LocalContextEstimate {
    let tokens = serde_json::to_string(&(
        request.system_prompt.as_ref(),
        &request.messages,
        &request.tools,
        &request.parameters,
        request.structured_output.as_ref(),
        &request.provider_context.request,
    ))
    .map_or(u64::MAX, |serialized| {
        estimated_tokens_from_chars(serialized.chars().count())
    })
    .saturating_add(image_tokens(&request.messages));
    bcode_session_models::LocalContextEstimate {
        tokens,
        algorithm_version: LOCAL_CONTEXT_ESTIMATOR_VERSION,
    }
}

pub fn projected_model_context_chars(
    history: &[bcode_session_models::SessionEvent],
    tool_output_context_chars: usize,
) -> usize {
    session_events_to_model_messages_with_limit(history, tool_output_context_chars)
        .iter()
        .map(model_message_context_chars)
        .sum()
}

pub fn estimated_tokens_from_chars(chars: usize) -> u64 {
    u64::try_from(chars).unwrap_or(u64::MAX).saturating_add(3) / 4
}

pub fn estimated_model_messages_tokens(messages: &[ModelMessage]) -> u64 {
    estimated_semantic_messages_tokens(messages)
}

fn estimated_semantic_messages_tokens(messages: &[ModelMessage]) -> u64 {
    let structural = serde_json::to_vec(messages).map_or(u64::MAX, |serialized| {
        u64::try_from(serialized.len())
            .unwrap_or(u64::MAX)
            .saturating_add(3)
            / 4
    });
    structural.saturating_add(image_tokens(messages))
}

fn image_tokens(messages: &[ModelMessage]) -> u64 {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .map(|block| match block {
            ContentBlock::Image { image } => image_metadata_tokens(&image.metadata),
            ContentBlock::ToolResult { result } => result
                .content
                .iter()
                .map(|content| match content {
                    bcode_model::ToolResultContent::Image { image } => {
                        image_metadata_tokens(&image.metadata)
                    }
                    bcode_model::ToolResultContent::ImageRef { image } => {
                        image_metadata_tokens(&image.metadata)
                    }
                    bcode_model::ToolResultContent::Text { .. } => 0,
                })
                .sum(),
            ContentBlock::Text { .. }
            | ContentBlock::ToolCall { .. }
            | ContentBlock::CachePoint { .. }
            | ContentBlock::ProviderExtension { .. } => 0,
        })
        .sum()
}

fn image_metadata_tokens(metadata: &bcode_model::ImageMetadata) -> u64 {
    metadata
        .width
        .zip(metadata.height)
        .map_or(0, |(width, height)| {
            u64::from(width)
                .saturating_mul(u64::from(height))
                .saturating_add(749)
                / 750
        })
}

pub fn model_message_context_chars(message: &ModelMessage) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.chars().count(),
            ContentBlock::Image { image } => {
                usize::try_from(image_metadata_tokens(&image.metadata).saturating_mul(4))
                    .unwrap_or(usize::MAX)
            }
            ContentBlock::CachePoint { .. } => 0,
            ContentBlock::ToolCall { call } => {
                call.name.chars().count() + call.arguments.to_string().chars().count()
            }
            ContentBlock::ToolResult { result } => result.output.chars().count(),
            ContentBlock::ProviderExtension { value } => value.to_string().chars().count(),
        })
        .sum()
}
