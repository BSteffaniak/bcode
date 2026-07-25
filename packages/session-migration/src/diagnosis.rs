//! Migration-owned diagnosis classification for current, released historical, future, and damaged stores.

use bcode_session_models::{SessionId, SessionOpenOperationId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Identity and coordination details for a live owner blocking migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMigrationOwnerDiagnosis {
    /// Daemon instance that currently owns the source session.
    pub daemon_instance_id: String,
    /// Owning process identifier, when available.
    pub process_id: Option<u32>,
    /// Owner start timestamp in Unix milliseconds, when available.
    pub started_at_ms: Option<u64>,
}

/// Migration-specific diagnosis facts composed with current store health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMigrationDiagnosis {
    /// Session being diagnosed.
    pub session_id: SessionId,
    /// Stable diagnosis classification.
    pub classification: SessionDiagnosisClassification,
    /// Historical source writer epoch, when applicable.
    pub source_writer_epoch: Option<u32>,
    /// Writer epoch required by this build.
    pub target_writer_epoch: u32,
    /// Ordered migration steps required to reach the target writer.
    pub migration_step_ids: Vec<String>,
    /// Operation currently waiting or running for this session.
    pub operation_id: Option<SessionOpenOperationId>,
    /// Whether migration is waiting for exclusive ownership.
    pub waiting_for_owner: bool,
    /// Live owner preventing migration, when known.
    pub owner: Option<SessionMigrationOwnerDiagnosis>,
    /// Latest retained verified backup for this source, when available.
    pub retained_backup_path: Option<PathBuf>,
    /// Actionable recovery guidance.
    pub recovery_guidance: Option<String>,
}

/// Final diagnosis classification presented by CLI and support tooling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    fn typed_diagnosis_serializes_process_boundary_fields() {
        let session_id = SessionId::new();
        let operation_id = SessionOpenOperationId::new();
        let diagnosis = SessionMigrationDiagnosis {
            session_id,
            classification: SessionDiagnosisClassification::BlockedOwner,
            source_writer_epoch: Some(4),
            target_writer_epoch: 5,
            migration_step_ids: vec!["session-writer-epoch-4-to-5".to_owned()],
            operation_id: Some(operation_id),
            waiting_for_owner: true,
            owner: Some(SessionMigrationOwnerDiagnosis {
                daemon_instance_id: "daemon-1".to_owned(),
                process_id: Some(42),
                started_at_ms: Some(10),
            }),
            retained_backup_path: Some(PathBuf::from("backup/session")),
            recovery_guidance: Some("stop the owning daemon and retry".to_owned()),
        };

        let encoded = serde_json::to_string(&diagnosis).expect("serialize diagnosis");
        let decoded = serde_json::from_str::<SessionMigrationDiagnosis>(&encoded)
            .expect("deserialize diagnosis");
        assert_eq!(decoded, diagnosis);
    }

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
