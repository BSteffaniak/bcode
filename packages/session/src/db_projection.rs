//! Current materialized-projection identities and checkpoint facts.

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
    /// Authoritative current context occupancy.
    RequestContextOccupancy,
}

impl MaterializedProjection {
    const ALL: [Self; 7] = [
        Self::SessionState,
        Self::InputHistory,
        Self::Transcript,
        Self::ToolRuns,
        Self::ArtifactReferences,
        Self::RuntimeWork,
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
            Self::Transcript | Self::ToolRuns => 2,
            Self::SessionState
            | Self::InputHistory
            | Self::ArtifactReferences
            | Self::RuntimeWork => 1,
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
            Self::RequestContextOccupancy => "context_occupancy",
        }
    }
}

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
        assert_eq!(MaterializedProjection::all().len(), 7);
        assert_eq!(
            MaterializedProjection::RequestContextOccupancy.schema_version(),
            4
        );
        assert_eq!(MaterializedProjection::Transcript.schema_version(), 2);
        assert_eq!(MaterializedProjection::ToolRuns.schema_version(), 2);
        assert_eq!(
            MaterializedProjection::ArtifactReferences.as_str(),
            "artifact_references"
        );
    }
}
