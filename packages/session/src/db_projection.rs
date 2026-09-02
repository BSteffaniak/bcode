//! Current materialized-projection identities and checkpoint facts.

use crate::db::SessionDbResult;
use crate::db_event_store::{event_created_at_ms, seq_to_value};
use crate::db_row::{i64_to_u64, required_i64, required_string};
use bcode_session_models::SessionEvent;
use std::collections::BTreeMap;
use switchy::database::{Database, DatabaseValue, query::FilterableQuery};

/// One current checkpointed materialized projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializedProjection {
    /// Projected current session state.
    SessionState,
    /// User-authored input history.
    InputHistory,
    /// Transcript item spans for UI/history windows.
    Transcript,
    /// Active and completed tool-call rows.
    ToolRuns,
    /// Generic references from finalized plugin artifacts.
    ArtifactReferences,
    /// Runtime-work lifecycle rows.
    RuntimeWork,
    /// Authoritative cumulative session usage and fixed request-time cost estimates.
    SessionUsage,
    /// Authoritative current context occupancy.
    RequestContextOccupancy,
}

impl MaterializedProjection {
    const ALL: [Self; 8] = [
        Self::SessionState,
        Self::InputHistory,
        Self::Transcript,
        Self::ToolRuns,
        Self::ArtifactReferences,
        Self::RuntimeWork,
        Self::SessionUsage,
        Self::RequestContextOccupancy,
    ];

    /// Return all checkpointed materialized projections.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &Self::ALL
    }

    /// Return the schema version stored with this projection's checkpoint.
    #[must_use]
    pub const fn schema_version(self) -> u32 {
        match self {
            Self::RequestContextOccupancy => 4,
            Self::Transcript => 3,
            Self::ToolRuns => 2,
            Self::SessionState
            | Self::InputHistory
            | Self::ArtifactReferences
            | Self::RuntimeWork
            | Self::SessionUsage => 1,
        }
    }

    /// Return the stable projection checkpoint name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionState => "session_state",
            Self::InputHistory => "input_history",
            Self::Transcript => "transcript",
            Self::ToolRuns => "tool_runs",
            Self::ArtifactReferences => "artifact_references",
            Self::RuntimeWork => "runtime_work",
            Self::SessionUsage => "session_usage",
            Self::RequestContextOccupancy => "context_occupancy",
        }
    }
}

pub async fn projection_checkpoint_snapshot(
    db: &dyn Database,
) -> SessionDbResult<BTreeMap<String, ProjectionCheckpointState>> {
    let rows = db
        .select("projection_checkpoints")
        .columns(&["projection_name", "projection_version", "last_event_seq"])
        .where_in(
            "projection_name",
            MaterializedProjection::all()
                .iter()
                .map(|projection| DatabaseValue::String(projection.as_str().to_owned()))
                .collect::<Vec<_>>(),
        )
        .execute(db)
        .await?;
    rows.into_iter()
        .map(|row| {
            let name = required_string(&row, "projection_name")?;
            let state = ProjectionCheckpointState {
                version: required_i64(&row, "projection_version").map(i64_to_u64)?,
                checkpoint: required_i64(&row, "last_event_seq").map(i64_to_u64)?,
            };
            Ok((name, state))
        })
        .collect()
}

pub async fn update_projection_checkpoint(
    db: &dyn Database,
    projection: MaterializedProjection,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    db.upsert("projection_checkpoints")
        .unique(&["projection_name"])
        .value("projection_name", projection.as_str())
        .value("last_event_seq", seq_to_value(event.sequence))
        .value(
            "projection_version",
            DatabaseValue::Int64(i64::from(projection.schema_version())),
        )
        .value("updated_at_ms", seq_to_value(event_created_at_ms(event)))
        .execute(db)
        .await?;
    Ok(())
}

/// Current projections finalized together for each canonical event.
pub const BASE_MATERIALIZED_PROJECTIONS: [MaterializedProjection; 7] = [
    MaterializedProjection::SessionState,
    MaterializedProjection::InputHistory,
    MaterializedProjection::Transcript,
    MaterializedProjection::ToolRuns,
    MaterializedProjection::ArtifactReferences,
    MaterializedProjection::RuntimeWork,
    MaterializedProjection::SessionUsage,
];

/// One current projection checkpoint row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectionCheckpointState {
    pub(crate) version: u64,
    pub(crate) checkpoint: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_inventory_is_complete_and_stable() {
        assert_eq!(MaterializedProjection::all().len(), 8);
        assert_eq!(
            MaterializedProjection::RequestContextOccupancy.schema_version(),
            4
        );
        assert_eq!(MaterializedProjection::Transcript.schema_version(), 3);
        assert_eq!(MaterializedProjection::ToolRuns.schema_version(), 2);
        assert_eq!(
            MaterializedProjection::ArtifactReferences.as_str(),
            "artifact_references"
        );
    }
}
