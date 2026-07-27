//! Current canonical identity and projection-checkpoint validation.

use crate::db::{MaterializedProjection, SessionDbError, SessionDbResult};
use crate::db_projection::ProjectionCheckpointState;
use crate::db_row::{i64_to_u64, required_i64};
use bcode_session_models::{SessionEvent, SessionId};
use std::collections::BTreeMap;
use switchy::database::{Database, query::FilterableQuery};

pub async fn validate_model_context_precondition(
    db: &dyn Database,
    event: &SessionEvent,
    projection_id: i32,
    schema_version: u32,
) -> SessionDbResult<()> {
    let state = db
        .select("model_context_projection_state")
        .columns(&["schema_version", "last_event_seq"])
        .where_eq("projection_id", projection_id)
        .execute_first(db)
        .await?;
    let Some(state) = state.as_ref() else {
        if event.sequence == 0 {
            return Ok(());
        }
        return Err(SessionDbError::ProjectionStale {
            projection: "model_context",
            checkpoint: None,
            expected: event.sequence.saturating_sub(1),
        });
    };
    let actual = required_i64(state, "schema_version").map(i64_to_u64)?;
    if actual != u64::from(schema_version) {
        return Err(SessionDbError::ModelContextProjectionVersion {
            actual,
            expected: u64::from(schema_version),
        });
    }
    let checkpoint = required_i64(state, "last_event_seq").map(i64_to_u64)?;
    let expected = event.sequence.saturating_sub(1);
    if event.sequence == 0 || checkpoint != expected {
        return Err(SessionDbError::ModelContextProjectionStale {
            checkpoint,
            expected,
        });
    }
    Ok(())
}

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

pub fn validate_append_identity(
    event: &SessionEvent,
    canonical_tail: Option<u64>,
) -> SessionDbResult<()> {
    if let bcode_session_models::SessionEventKind::ToolContribution { event } = &event.kind
        && event.persistence == bcode_session_models::ToolContributionPersistence::Transient
    {
        return Err(SessionDbError::TransientContribution {
            contribution_id: event.contribution_id.clone(),
        });
    }
    let expected_sequence = canonical_tail.map_or(0, |tail| tail.saturating_add(1));
    if event.sequence != expected_sequence {
        return Err(SessionDbError::InvalidCanonicalAppendSequence {
            expected: expected_sequence,
            actual: event.sequence,
        });
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
