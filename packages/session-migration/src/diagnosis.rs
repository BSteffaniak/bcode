//! Migration-owned diagnosis classification for current, released historical, future, and damaged stores.

/// Final diagnosis classification presented by CLI and support tooling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDiagnosisClassification {
    /// Store is current and writable.
    CurrentReady,
    /// Store was produced by a released writer and can migrate to current.
    Migratable,
    /// Released migration is temporarily blocked by a live owner.
    BlockedOwner,
    /// Store was produced by an unknown future writer.
    UnsupportedFuture,
    /// Store structure or canonical history is malformed.
    StructurallyCorrupt,
    /// Store is current but requires explicit repair or reindexing.
    RepairRequired,
}

impl SessionDiagnosisClassification {
    /// Stable serialized/display name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CurrentReady => "current_ready",
            Self::Migratable => "migratable",
            Self::BlockedOwner => "blocked_owner",
            Self::UnsupportedFuture => "unsupported_future",
            Self::StructurallyCorrupt => "structurally_corrupt",
            Self::RepairRequired => "repair_required",
        }
    }
}

/// Inputs required to classify one store without embedding database implementation types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDiagnosisCompatibility {
    /// Current writer/ledger format.
    Current,
    /// Released historical format.
    ReleasedHistorical,
    /// Unknown future writer format.
    UnknownFuture,
    /// Storage classification failed structurally.
    StructurallyCorrupt,
}

/// Classify one session diagnosis from typed, transport-independent facts.
#[must_use]
pub const fn classify_session_diagnosis(
    compatibility: SessionDiagnosisCompatibility,
    write_ready: bool,
    strict_history_failed: bool,
    waiting_for_owner: bool,
) -> SessionDiagnosisClassification {
    match compatibility {
        SessionDiagnosisCompatibility::ReleasedHistorical if waiting_for_owner => {
            SessionDiagnosisClassification::BlockedOwner
        }
        SessionDiagnosisCompatibility::ReleasedHistorical => {
            SessionDiagnosisClassification::Migratable
        }
        SessionDiagnosisCompatibility::UnknownFuture => {
            SessionDiagnosisClassification::UnsupportedFuture
        }
        SessionDiagnosisCompatibility::StructurallyCorrupt => {
            SessionDiagnosisClassification::StructurallyCorrupt
        }
        SessionDiagnosisCompatibility::Current if strict_history_failed => {
            SessionDiagnosisClassification::StructurallyCorrupt
        }
        SessionDiagnosisCompatibility::Current if write_ready => {
            SessionDiagnosisClassification::CurrentReady
        }
        SessionDiagnosisCompatibility::Current => SessionDiagnosisClassification::RepairRequired,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_distinguishes_every_required_state() {
        assert_eq!(
            classify_session_diagnosis(SessionDiagnosisCompatibility::Current, true, false, false),
            SessionDiagnosisClassification::CurrentReady
        );
        assert_eq!(
            classify_session_diagnosis(
                SessionDiagnosisCompatibility::ReleasedHistorical,
                false,
                false,
                false,
            ),
            SessionDiagnosisClassification::Migratable
        );
        assert_eq!(
            classify_session_diagnosis(
                SessionDiagnosisCompatibility::ReleasedHistorical,
                false,
                false,
                true,
            ),
            SessionDiagnosisClassification::BlockedOwner
        );
        assert_eq!(
            classify_session_diagnosis(
                SessionDiagnosisCompatibility::UnknownFuture,
                false,
                false,
                false,
            ),
            SessionDiagnosisClassification::UnsupportedFuture
        );
        assert_eq!(
            classify_session_diagnosis(
                SessionDiagnosisCompatibility::StructurallyCorrupt,
                false,
                false,
                false,
            ),
            SessionDiagnosisClassification::StructurallyCorrupt
        );
        assert_eq!(
            classify_session_diagnosis(SessionDiagnosisCompatibility::Current, false, true, false),
            SessionDiagnosisClassification::StructurallyCorrupt
        );
        assert_eq!(
            classify_session_diagnosis(SessionDiagnosisCompatibility::Current, false, false, false),
            SessionDiagnosisClassification::RepairRequired
        );
    }
}
