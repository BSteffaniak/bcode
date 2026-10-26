//! Validation for serialized ephemeral compaction publication.
use super::{
    CompactionCheckpointCommit, MAX_COMPACTION_COMMIT_SOURCE_EVENTS, compaction_event_identity,
};
use crate::{SessionError, SessionEvent, SessionEventKind};
use bcode_session_models::CompactionSynthesisCheckpoint;

pub fn validate_memory_commit(
    history: &[SessionEvent],
    request: &CompactionCheckpointCommit,
) -> Result<(), SessionError> {
    let invalid = || {
        SessionError::EventSerialization(
            "compaction publication precondition changed or invalid".into(),
        )
    };
    CompactionSynthesisCheckpoint::validate_key(&request.operation_key)
        .map_err(SessionError::EventSerialization)?;
    if request.source.is_empty() || request.source.len() > MAX_COMPACTION_COMMIT_SOURCE_EVENTS {
        return Err(invalid());
    }
    let boundary = match &request.marker {
        SessionEventKind::ContextCompacted {
            compacted_through_sequence,
            ..
        }
        | SessionEventKind::ProviderContextCompacted {
            compacted_through_sequence,
            ..
        } => *compacted_through_sequence,
        _ => return Err(invalid()),
    };
    if request
        .source
        .last()
        .is_none_or(|source| source.sequence != boundary)
    {
        return Err(invalid());
    }
    let prior = history
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event.kind,
                SessionEventKind::ContextCompacted { .. }
                    | SessionEventKind::ProviderContextCompacted { .. }
            )
        })
        .map(compaction_event_identity)
        .transpose()
        .map_err(SessionError::EventSerialization)?;
    if prior != request.expected_prior_marker {
        return Err(invalid());
    }
    let source = crate::model_context_events_from_history(history)
        .iter()
        .filter(|event| {
            event.sequence <= boundary
                && !matches!(
                    event.kind,
                    SessionEventKind::ContextCompacted { .. }
                        | SessionEventKind::ProviderContextCompacted { .. }
                )
        })
        .map(compaction_event_identity)
        .collect::<Result<Vec<_>, _>>()
        .map_err(SessionError::EventSerialization)?;
    if source != request.source {
        return Err(invalid());
    }
    let mut latest = None;
    for event in history {
        if let Some(checkpoint) = CompactionSynthesisCheckpoint::from_event(event)
            .map_err(SessionError::EventSerialization)?
            && checkpoint.operation_key == request.operation_key
        {
            latest = Some((event.sequence, checkpoint));
        }
    }
    let Some((sequence, checkpoint)) = latest else {
        return Err(invalid());
    };
    if sequence != request.expected_checkpoint_sequence || checkpoint.host_payload.is_none() {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
#[path = "compaction_checkpoint_memory_tests.rs"]
mod tests;
