//! Current model-context event classification.

use bcode_session_models::SessionEventKind;

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
        | SessionEventKind::ToolCallRequested { .. }
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
        | b"tool_call_requested"
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
pub const fn model_context_event_kind_name(kind: &SessionEventKind) -> &'static str {
    match kind {
        SessionEventKind::UserMessage { .. } => "user_message",
        SessionEventKind::AssistantMessage { .. } => "assistant_message",
        SessionEventKind::ToolCallRequested { .. } => "tool_call_requested",
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
