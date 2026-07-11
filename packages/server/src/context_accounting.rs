//! Context occupancy accounting for model requests and compaction policy.
//!
//! Provider observations are exact only through their attributed canonical sequence. Newer events
//! are conservatively estimated and added to that observation. If no compatible observation
//! exists, accounting estimates the entire model-visible projection. Structural retention uses
//! canonical serialized model-message accounting so cuts include protocol overhead rather than raw
//! content character counts; occupancy fallback remains a conservative character estimate when no
//! exact provider observation exists.

use super::{
    ContentBlock, ModelMessage, SessionEventKind, SessionModelSelection,
    model_id_for_provider_request, session_events_to_model_messages_with_limit,
};

pub fn context_occupancy_tokens(
    history: &[bcode_session_models::SessionEvent],
    selection: &SessionModelSelection,
    projected_context_chars: usize,
    tool_output_context_chars: usize,
) -> u64 {
    let provider = selection.provider_plugin_id.as_deref().unwrap_or("<auto>");
    let model = model_id_for_provider_request(selection.model_id.as_deref());
    let Some(snapshot) = history.iter().rev().find_map(|event| match &event.kind {
        SessionEventKind::ContextUsageObserved { snapshot }
            if snapshot.provider_plugin_id == provider && snapshot.model_id == model =>
        {
            Some(snapshot)
        }
        _ => None,
    }) else {
        return estimated_tokens_from_chars(projected_context_chars);
    };
    let delta_events = history
        .iter()
        .filter(|event| {
            event.sequence > snapshot.context_through_sequence
                && !matches!(event.kind, SessionEventKind::ContextUsageObserved { .. })
        })
        .cloned()
        .collect::<Vec<_>>();
    let delta_chars = projected_model_context_chars(&delta_events, tool_output_context_chars);
    snapshot
        .input_tokens
        .saturating_add(estimated_tokens_from_chars(delta_chars))
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
    serde_json::to_vec(messages).map_or(u64::MAX, |serialized| {
        u64::try_from(serialized.len())
            .unwrap_or(u64::MAX)
            .saturating_add(3)
            / 4
    })
}

pub fn model_message_context_chars(message: &ModelMessage) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.chars().count(),
            ContentBlock::Image { .. } | ContentBlock::CachePoint { .. } => 0,
            ContentBlock::ToolCall { call } => {
                call.name.chars().count() + call.arguments.to_string().chars().count()
            }
            ContentBlock::ToolResult { result } => result.output.chars().count(),
            ContentBlock::ProviderExtension { value } => value.to_string().chars().count(),
        })
        .sum()
}
