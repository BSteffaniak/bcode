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

/// Validate and return an exact persisted restart retry, without minting another allowance.
pub fn memory_restart_retry(
    history: &[SessionEvent],
    request: &super::CompactionCheckpointRestart,
) -> Result<Option<SessionEvent>, SessionError> {
    let invalid =
        || SessionError::EventSerialization(super::CompactionCheckpointRestart::invalid());
    request.validate().map_err(|_| invalid())?;
    let receipt_key = request.receipt_key();
    let mut receipt = None;
    for event in history {
        if CompactionSynthesisCheckpoint::from_event(event)
            .map_err(|_| invalid())?
            .is_some_and(|checkpoint| checkpoint.operation_key == receipt_key)
        {
            if receipt.is_some() {
                return Err(invalid());
            }
            receipt = Some(event);
        }
    }
    let Some(receipt) = receipt else {
        return Ok(None);
    };
    let replacement_sequence = request.retry_sequence(receipt).map_err(|_| invalid())?;
    let mut events = Vec::with_capacity(4);
    for offset in 0..4 {
        let sequence = receipt.sequence.checked_add(offset).ok_or_else(invalid)?;
        events.push(
            history
                .iter()
                .find(|event| event.sequence == sequence)
                .cloned()
                .ok_or_else(invalid)?,
        );
    }
    let events: [SessionEvent; 4] = events.try_into().map_err(|_| invalid())?;
    request
        .validate_retry_events(&events)
        .map_err(|_| invalid())?;
    if events[2].sequence != replacement_sequence {
        return Err(invalid());
    }
    Ok(Some(events[2].clone()))
}

/// Validate every in-memory restart fence before any event is applied.
pub fn validate_memory_restart(
    history: &[SessionEvent],
    request: &super::CompactionCheckpointRestart,
) -> Result<(), SessionError> {
    let invalid =
        || SessionError::EventSerialization(super::CompactionCheckpointRestart::invalid());
    request.validate().map_err(|_| invalid())?;
    validate_memory_commit(history, &request.comparison()).map_err(|_| invalid())?;
    if history.last().map(|event| event.sequence) != Some(request.fence()) {
        return Err(invalid());
    }
    let rejection = history
        .iter()
        .find(|event| event.sequence == request.rejection.sequence)
        .ok_or_else(invalid)?;
    if super::compaction_event_identity(rejection).map_err(|_| invalid())? != request.rejection {
        return Err(invalid());
    }
    for event in history {
        if CompactionSynthesisCheckpoint::from_event(event)
            .map_err(|_| invalid())?
            .is_some_and(|checkpoint| {
                checkpoint.operation_key == request.replacement_key
                    || checkpoint.operation_key == request.receipt_key()
            })
        {
            return Err(invalid());
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "compaction_checkpoint_memory_tests.rs"]
mod tests;
