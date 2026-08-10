use crate::RELEASED_HISTORICAL_ROOTS;
use bcode_session_models::SessionId;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Outcome of scanning and recovering the removed historical writer-epoch root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HistoricalStorageRecoveryReport {
    /// Sessions atomically relocated into canonical storage.
    pub relocated: Vec<SessionId>,
    /// Sessions left untouched because a live owner still uses the historical root.
    pub blocked_by_owner: Vec<SessionId>,
    /// Sessions left untouched because the canonical destination already exists.
    pub destination_conflicts: Vec<SessionId>,
}

/// Non-mutating diagnosis of the removed historical writer-epoch root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HistoricalStorageInspectionReport {
    /// Sessions that can be relocated when recovery next runs.
    pub pending_relocation: Vec<SessionId>,
    /// Sessions currently protected by a live owner.
    pub blocked_by_owner: Vec<SessionId>,
    /// Sessions with both historical and canonical directories.
    pub destination_conflicts: Vec<SessionId>,
}

/// Classification of the removed historical storage root for doctor output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoricalStorageDiagnosisStatus {
    /// No historical sessions require action.
    Ok,
    /// Unambiguous historical sessions can be relocated.
    WouldRecover,
    /// A live owner prevents recovery.
    BlockedByOwner,
    /// Historical and canonical directories conflict and require manual intervention.
    ManualRequired,
}

/// Migration-owned diagnosis of the removed historical storage root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoricalStorageDiagnosis {
    /// Exact removed historical root.
    pub root: PathBuf,
    /// Classified doctor status.
    pub status: HistoricalStorageDiagnosisStatus,
    /// Detailed non-mutating inspection.
    pub inspection: HistoricalStorageInspectionReport,
    /// Human-readable migration-domain findings.
    pub notes: Vec<String>,
}

/// Result returned by the coordination adapter for one relocation attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoricalStorageRelocation {
    /// The source was atomically moved into canonical storage.
    Relocated,
    /// A live owner prevented relocation.
    BlockedByOwner,
    /// The canonical destination exists and was not overwritten.
    DestinationConflict,
}

/// Historical-root inspection or recovery failure.
#[derive(Debug, Error)]
pub enum HistoricalStorageError<E> {
    /// Filesystem discovery or cleanup failed.
    #[error("historical session storage I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// Owner coordination or atomic relocation failed.
    #[error("historical session storage coordination failed: {0}")]
    Coordination(E),
}

/// Return the exact removed writer-epoch root for explicit diagnosis and recovery.
#[must_use]
pub fn accidental_epoch_session_root(state_dir: &Path) -> PathBuf {
    RELEASED_HISTORICAL_ROOTS[0]
        .path
        .split('/')
        .fold(state_dir.to_path_buf(), |root, segment| root.join(segment))
}

/// Diagnose and classify the removed writer-epoch root without mutation.
///
/// # Errors
///
/// Returns an error when directory discovery or owner inspection fails.
pub fn diagnose_accidental_epoch_session_root<E>(
    state_dir: &Path,
    has_active_owner: impl FnMut(SessionId, &Path) -> Result<bool, E>,
) -> Result<HistoricalStorageDiagnosis, HistoricalStorageError<E>> {
    let root = accidental_epoch_session_root(state_dir);
    let inspection = inspect_accidental_epoch_session_root(state_dir, has_active_owner)?;
    let status = if !inspection.destination_conflicts.is_empty() {
        HistoricalStorageDiagnosisStatus::ManualRequired
    } else if !inspection.blocked_by_owner.is_empty() {
        HistoricalStorageDiagnosisStatus::BlockedByOwner
    } else if !inspection.pending_relocation.is_empty() {
        HistoricalStorageDiagnosisStatus::WouldRecover
    } else {
        HistoricalStorageDiagnosisStatus::Ok
    };
    let mut notes = Vec::new();
    for session_id in &inspection.pending_relocation {
        notes.push(format!(
            "historical session {session_id} can be relocated to canonical storage"
        ));
    }
    for session_id in &inspection.blocked_by_owner {
        notes.push(format!(
            "historical session {session_id} is still owned by a live process"
        ));
    }
    for session_id in &inspection.destination_conflicts {
        notes.push(format!(
            "historical session {session_id} conflicts with an existing canonical session; no merge or overwrite is safe"
        ));
    }
    Ok(HistoricalStorageDiagnosis {
        root,
        status,
        inspection,
        notes,
    })
}

/// Inspect the removed writer-epoch root without modifying files or owner metadata.
///
/// `has_active_owner` is supplied by the current session ownership implementation so this
/// historical module does not depend on runtime lease types.
///
/// # Errors
///
/// Returns an error when directory discovery or owner inspection fails.
pub fn inspect_accidental_epoch_session_root<E>(
    state_dir: &Path,
    mut has_active_owner: impl FnMut(SessionId, &Path) -> Result<bool, E>,
) -> Result<HistoricalStorageInspectionReport, HistoricalStorageError<E>> {
    let historical_root = accidental_epoch_session_root(state_dir);
    let canonical_root = state_dir.join("sessions");
    let mut report = HistoricalStorageInspectionReport::default();
    for session_id in historical_session_ids(&historical_root)? {
        if canonical_root.join(session_id.to_string()).exists() {
            report.destination_conflicts.push(session_id);
        } else if has_active_owner(session_id, &historical_root)
            .map_err(HistoricalStorageError::Coordination)?
        {
            report.blocked_by_owner.push(session_id);
        } else {
            report.pending_relocation.push(session_id);
        }
    }
    Ok(report)
}

/// Recover unambiguous sessions from the removed writer-epoch root.
///
/// The supplied relocation adapter must acquire current ownership coordination, recheck the
/// destination, and perform the atomic move. This function owns historical-root discovery,
/// iteration, classification, reporting, and empty-root cleanup.
///
/// # Errors
///
/// Returns an error when directory discovery, coordination, relocation, or cleanup fails.
pub fn recover_accidental_epoch_session_root<E>(
    state_dir: &Path,
    mut relocate: impl FnMut(SessionId, &Path, &Path) -> Result<HistoricalStorageRelocation, E>,
) -> Result<HistoricalStorageRecoveryReport, HistoricalStorageError<E>> {
    let historical_root = accidental_epoch_session_root(state_dir);
    if !historical_root.exists() {
        return Ok(HistoricalStorageRecoveryReport::default());
    }
    let canonical_root = state_dir.join("sessions");
    let mut report = HistoricalStorageRecoveryReport::default();
    for session_id in historical_session_ids(&historical_root)? {
        match relocate(session_id, &historical_root, &canonical_root)
            .map_err(HistoricalStorageError::Coordination)?
        {
            HistoricalStorageRelocation::Relocated => report.relocated.push(session_id),
            HistoricalStorageRelocation::BlockedByOwner => {
                report.blocked_by_owner.push(session_id);
            }
            HistoricalStorageRelocation::DestinationConflict => {
                report.destination_conflicts.push(session_id);
            }
        }
    }
    remove_empty_dir(historical_root.join("leases"));
    remove_empty_dir(historical_root.join("locks"));
    remove_empty_dir(historical_root.clone());
    if let Some(parent) = historical_root.parent() {
        remove_empty_dir(parent.to_path_buf());
    }
    Ok(report)
}

fn historical_session_ids(root: &Path) -> Result<Vec<SessionId>, std::io::Error> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut ids = fs::read_dir(root)?
        .flatten()
        .filter_map(|entry| {
            entry.file_type().ok().filter(std::fs::FileType::is_dir)?;
            entry.file_name().to_str()?.parse::<SessionId>().ok()
        })
        .collect::<Vec<_>>();
    ids.sort_unstable();
    Ok(ids)
}

fn remove_empty_dir(path: PathBuf) {
    let _ = fs::remove_dir(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    #[test]
    fn historical_root_path_is_driven_by_released_inventory() {
        let state = tempfile::tempdir().expect("state");
        assert_eq!(
            accidental_epoch_session_root(state.path()),
            state.path().join("session-storage").join("writer-epoch-2")
        );
        assert_eq!(RELEASED_HISTORICAL_ROOTS.len(), 1);
        assert_eq!(
            RELEASED_HISTORICAL_ROOTS[0].treatment,
            crate::ReleasedRootTreatment::RelocateToCanonical
        );
    }

    #[test]
    fn diagnosis_prioritizes_conflicts_then_owners_then_pending_recovery() {
        let state = tempfile::tempdir().expect("state");
        let historical = accidental_epoch_session_root(state.path());
        let pending = SessionId::new();
        let owned = SessionId::new();
        let conflict = SessionId::new();
        for session_id in [pending, owned, conflict] {
            fs::create_dir_all(historical.join(session_id.to_string())).expect("source");
        }
        fs::create_dir_all(state.path().join("sessions").join(conflict.to_string()))
            .expect("destination");

        let diagnosis = diagnose_accidental_epoch_session_root(state.path(), |session_id, _| {
            Ok::<_, Infallible>(session_id == owned)
        })
        .expect("diagnosis");
        assert_eq!(
            diagnosis.status,
            HistoricalStorageDiagnosisStatus::ManualRequired
        );
        assert_eq!(diagnosis.notes.len(), 3);
        assert_eq!(diagnosis.inspection.pending_relocation, vec![pending]);
        assert_eq!(diagnosis.inspection.blocked_by_owner, vec![owned]);
        assert_eq!(diagnosis.inspection.destination_conflicts, vec![conflict]);
    }

    #[test]
    fn inspection_classifies_pending_owned_and_conflicting_sessions_without_mutation() {
        let state = tempfile::tempdir().expect("state");
        let historical = accidental_epoch_session_root(state.path());
        let pending = SessionId::new();
        let owned = SessionId::new();
        let conflict = SessionId::new();
        for session_id in [pending, owned, conflict] {
            fs::create_dir_all(historical.join(session_id.to_string())).expect("source");
        }
        fs::create_dir_all(state.path().join("sessions").join(conflict.to_string()))
            .expect("destination");

        let report = inspect_accidental_epoch_session_root(state.path(), |session_id, _| {
            Ok::<_, Infallible>(session_id == owned)
        })
        .expect("inspection");
        assert_eq!(report.pending_relocation, vec![pending]);
        assert_eq!(report.blocked_by_owner, vec![owned]);
        assert_eq!(report.destination_conflicts, vec![conflict]);
        assert!(historical.join(pending.to_string()).exists());
    }

    #[test]
    fn recovery_classifies_adapter_outcomes_and_cleans_empty_historical_root() {
        let state = tempfile::tempdir().expect("state");
        let historical = accidental_epoch_session_root(state.path());
        let relocated = SessionId::new();
        let blocked = SessionId::new();
        let conflict = SessionId::new();
        for session_id in [relocated, blocked, conflict] {
            fs::create_dir_all(historical.join(session_id.to_string())).expect("source");
        }

        let report = recover_accidental_epoch_session_root(
            state.path(),
            |session_id, source_root, destination_root| {
                if session_id == blocked {
                    return Ok::<_, Infallible>(HistoricalStorageRelocation::BlockedByOwner);
                }
                if session_id == conflict {
                    fs::remove_dir(source_root.join(session_id.to_string())).expect("test cleanup");
                    return Ok(HistoricalStorageRelocation::DestinationConflict);
                }
                fs::create_dir_all(destination_root).expect("canonical root");
                fs::rename(
                    source_root.join(session_id.to_string()),
                    destination_root.join(session_id.to_string()),
                )
                .expect("relocate");
                Ok(HistoricalStorageRelocation::Relocated)
            },
        )
        .expect("recovery");
        assert_eq!(report.relocated, vec![relocated]);
        assert_eq!(report.blocked_by_owner, vec![blocked]);
        assert_eq!(report.destination_conflicts, vec![conflict]);
        assert!(historical.exists(), "blocked source keeps historical root");
    }
}
