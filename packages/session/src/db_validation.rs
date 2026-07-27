//! Current canonical identity and projection-checkpoint validation.

use crate::db::{MaterializedProjection, SessionDbError, SessionDbResult};
use crate::db_projection::ProjectionCheckpointState;
use bcode_session_models::{SessionEvent, SessionId};
use std::collections::BTreeMap;

pub fn validate_canonical_event_identity(
    event: &SessionEvent,
    expected: u64,
    session_id: SessionId,
) -> SessionDbResult<()> {
    if event.sequence != expected {
        return Err(SessionDbError::InvalidCanonicalSequence {
            expected,
            actual: event.sequence,
        });
    }
    if event.session_id != session_id {
        return Err(SessionDbError::InvalidRow {
            column: "events.session_id".to_owned(),
        });
    }
    Ok(())
}

pub fn validate_projection_checkpoint_snapshot(
    snapshot: &BTreeMap<String, ProjectionCheckpointState>,
    expected: u64,
) -> SessionDbResult<()> {
    for projection in MaterializedProjection::all() {
        let Some(state) = snapshot.get(projection.as_str()) else {
            return Err(SessionDbError::ProjectionStale {
                projection: projection.as_str(),
                checkpoint: None,
                expected,
            });
        };
        let expected_version = u64::from(projection.schema_version());
        if state.version != expected_version {
            return Err(SessionDbError::ProjectionIncompatible {
                projection: projection.as_str(),
                actual: state.version,
                expected: expected_version,
            });
        }
        if state.checkpoint != expected {
            return Err(SessionDbError::ProjectionStale {
                projection: projection.as_str(),
                checkpoint: Some(state.checkpoint),
                expected,
            });
        }
    }
    Ok(())
}

pub fn compaction_boundary(event: &SessionEvent) -> SessionDbResult<Option<u64>> {
    let boundary = match &event.kind {
        bcode_session_models::SessionEventKind::ContextCompacted {
            compacted_through_sequence,
            ..
        }
        | bcode_session_models::SessionEventKind::ProviderContextCompacted {
            compacted_through_sequence,
            ..
        } => *compacted_through_sequence,
        _ => return Ok(None),
    };
    if boundary > event.sequence {
        return Err(SessionDbError::InvalidCompactionMarker {
            sequence: event.sequence,
            message: format!("compacted boundary #{boundary} is later than its marker"),
        });
    }
    Ok(Some(boundary))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_projection_is_stale() {
        assert!(matches!(
            validate_projection_checkpoint_snapshot(&BTreeMap::new(), 7),
            Err(SessionDbError::ProjectionStale {
                checkpoint: None,
                expected: 7,
                ..
            })
        ));
    }
}
