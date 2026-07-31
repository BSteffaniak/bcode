//! Current model-context event classification.

use bcode_session_models::{SessionEvent, SessionEventKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextHistoryRole {
    ModelVisible,
    Structural,
    Excluded,
}

#[must_use]
pub const fn context_history_role(kind: &SessionEventKind) -> ContextHistoryRole {
    match kind {
        SessionEventKind::UserMessage { .. }
        | SessionEventKind::AssistantMessage { .. }
        | SessionEventKind::AssistantResponseSegment { .. }
        | SessionEventKind::PositionedAssistantResponseSegment { .. }
        | SessionEventKind::ToolCallRequested { .. }
        | SessionEventKind::PositionedToolCallRequested { .. }
        | SessionEventKind::ToolInvocationResultRecorded { .. }
        | SessionEventKind::SystemMessage { .. }
        | SessionEventKind::WorkingDirectoryChanged { .. }
        | SessionEventKind::ContextCompacted { .. }
        | SessionEventKind::ProviderContextCompacted { .. } => ContextHistoryRole::ModelVisible,
        SessionEventKind::ModelTurnStarted { .. } | SessionEventKind::ModelTurnFinished { .. } => {
            ContextHistoryRole::Structural
        }
        _ => ContextHistoryRole::Excluded,
    }
}

#[must_use]
pub const fn context_history_role_from_name(event_type: &str) -> ContextHistoryRole {
    match event_type.as_bytes() {
        b"user_message"
        | b"assistant_message"
        | b"assistant_response_segment"
        | b"positioned_assistant_response_segment"
        | b"tool_call_requested"
        | b"positioned_tool_call_requested"
        | b"tool_invocation_result_recorded"
        | b"system_message"
        | b"working_directory_changed"
        | b"context_compacted"
        | b"provider_context_compacted" => ContextHistoryRole::ModelVisible,
        b"model_turn_started" | b"model_turn_finished" => ContextHistoryRole::Structural,
        _ => ContextHistoryRole::Excluded,
    }
}

#[must_use]
pub const fn is_model_context_event_type(event_type: &str) -> bool {
    matches!(
        context_history_role_from_name(event_type),
        ContextHistoryRole::ModelVisible | ContextHistoryRole::Structural
    )
}

#[must_use]
pub fn canonical_model_context_from_events(
    events: impl IntoIterator<Item = SessionEvent>,
) -> Vec<SessionEvent> {
    let events = events.into_iter().collect::<Vec<_>>();
    let Some(marker) = events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                SessionEventKind::ContextCompacted { .. }
                    | SessionEventKind::ProviderContextCompacted { .. }
            )
        })
        .max_by_key(|event| event.sequence)
        .cloned()
    else {
        return events
            .into_iter()
            .filter(|event| is_model_context_event_type(model_context_event_kind_name(&event.kind)))
            .collect();
    };
    let boundary = match &marker.kind {
        SessionEventKind::ContextCompacted {
            compacted_through_sequence,
            ..
        }
        | SessionEventKind::ProviderContextCompacted {
            compacted_through_sequence,
            ..
        } => *compacted_through_sequence,
        _ => unreachable!("marker selection accepts only compaction events"),
    };
    let mut retained = events
        .into_iter()
        .filter(|event| {
            event.sequence > boundary
                && event.sequence != marker.sequence
                && is_model_context_event_type(model_context_event_kind_name(&event.kind))
                && !matches!(
                    event.kind,
                    SessionEventKind::ContextCompacted { .. }
                        | SessionEventKind::ProviderContextCompacted { .. }
                )
        })
        .collect::<Vec<_>>();
    retained.sort_by_key(|event| event.sequence);
    let mut context = Vec::with_capacity(retained.len().saturating_add(1));
    context.push(marker);
    context.extend(retained);
    context
}

#[must_use]
pub const fn model_context_event_kind_name(kind: &SessionEventKind) -> &'static str {
    match kind {
        SessionEventKind::UserMessage { .. } => "user_message",
        SessionEventKind::AssistantMessage { .. } => "assistant_message",
        SessionEventKind::AssistantResponseSegment { .. } => "assistant_response_segment",
        SessionEventKind::PositionedAssistantResponseSegment { .. } => {
            "positioned_assistant_response_segment"
        }
        SessionEventKind::ToolCallRequested { .. } => "tool_call_requested",
        SessionEventKind::PositionedToolCallRequested { .. } => "positioned_tool_call_requested",
        SessionEventKind::ToolInvocationResultRecorded { .. } => "tool_invocation_result_recorded",
        SessionEventKind::SystemMessage { .. } => "system_message",
        SessionEventKind::WorkingDirectoryChanged { .. } => "working_directory_changed",
        SessionEventKind::ContextCompacted { .. } => "context_compacted",
        SessionEventKind::ProviderContextCompacted { .. } => "provider_context_compacted",
        SessionEventKind::ModelTurnStarted { .. } => "model_turn_started",
        SessionEventKind::ModelTurnFinished { .. } => "model_turn_finished",
        SessionEventKind::RequestContextObserved { .. } => "request_context_observed",
        _ => "non_model_context",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_preserve_structural_and_excluded_roles() {
        assert!(is_model_context_event_type("model_turn_started"));
        assert!(is_model_context_event_type("model_turn_finished"));
        assert!(is_model_context_event_type("assistant_response_segment"));
        assert!(is_model_context_event_type(
            "positioned_assistant_response_segment"
        ));
        assert!(is_model_context_event_type(
            "positioned_tool_call_requested"
        ));
        assert!(!is_model_context_event_type("request_context_observed"));
    }
}
