//! Current ownership adapter for historical session-root inspection and recovery.
//!
//! Historical layout discovery and recovery policy live in `bcode_session_migration`. This module
//! supplies only current lease coordination and the canonical atomic rename primitive.

use crate::{SessionStoreError, db, lease};
use bcode_session_migration::{
    HistoricalStorageError, HistoricalStorageRelocation,
    diagnose_accidental_epoch_session_root as diagnose_historical_root,
    inspect_accidental_epoch_session_root as inspect_historical_root,
    recover_accidental_epoch_session_root as recover_historical_root,
};
use bcode_session_models::SessionId;
use std::fs;
use std::path::Path;

pub use bcode_session_migration::{
    HistoricalStorageDiagnosis, HistoricalStorageDiagnosisStatus,
    HistoricalStorageInspectionReport as LegacyStorageInspectionReport,
    HistoricalStorageRecoveryReport as LegacyStorageRecoveryReport, accidental_epoch_session_root,
};

/// Diagnose and classify the removed writer-epoch root without mutation.
///
/// # Errors
///
/// Returns an error when directory or owner inspection fails.
pub fn diagnose_accidental_epoch_session_root(
    state_dir: &Path,
) -> Result<HistoricalStorageDiagnosis, SessionStoreError> {
    diagnose_historical_root(state_dir, |session_id, historical_root| {
        lease::active_session_owners(historical_root, session_id).map(|owners| !owners.is_empty())
    })
    .map_err(|error| match error {
        HistoricalStorageError::Io(error) => SessionStoreError::Io(error),
        HistoricalStorageError::Coordination(error) => SessionStoreError::Lease(error),
    })
}

/// Inspect the removed writer-epoch root without modifying files or owner metadata.
///
/// # Errors
///
/// Returns an error when directory or owner inspection fails.
pub fn inspect_accidental_epoch_session_root(
    state_dir: &Path,
) -> Result<LegacyStorageInspectionReport, SessionStoreError> {
    inspect_historical_root(state_dir, |session_id, historical_root| {
        lease::active_session_owners(historical_root, session_id).map(|owners| !owners.is_empty())
    })
    .map_err(|error| match error {
        HistoricalStorageError::Io(error) => SessionStoreError::Io(error),
        HistoricalStorageError::Coordination(error) => SessionStoreError::Lease(error),
    })
}

/// Relocate unambiguous sessions from the removed writer-epoch root into canonical storage.
///
/// Current maintenance guards prevent moving a live source or racing a canonical destination.
/// Existing canonical data is never overwritten or merged.
///
/// # Errors
///
/// Returns an error when directory inspection, coordination, or atomic relocation fails.
pub fn recover_accidental_epoch_session_root(
    state_dir: &Path,
) -> Result<LegacyStorageRecoveryReport, SessionStoreError> {
    recover_historical_root(state_dir, |session_id, historical_root, canonical_root| {
        relocate_historical_session(session_id, historical_root, canonical_root)
    })
    .map_err(map_historical_storage_error)
}

fn relocate_historical_session(
    session_id: SessionId,
    historical_root: &Path,
    canonical_root: &Path,
) -> Result<HistoricalStorageRelocation, SessionStoreError> {
    let source = db::session_dir_path(historical_root, session_id);
    let destination = db::session_dir_path(canonical_root, session_id);
    if destination.exists() {
        return Ok(HistoricalStorageRelocation::DestinationConflict);
    }
    let source_maintenance =
        match lease::acquire_session_maintenance_guard(historical_root, session_id) {
            Ok(guard) => guard,
            Err(lease::SessionLeaseError::OwnedByOtherDaemon { .. }) => {
                return Ok(HistoricalStorageRelocation::BlockedByOwner);
            }
            Err(error) => return Err(SessionStoreError::Lease(error)),
        };
    let destination_maintenance =
        lease::acquire_session_maintenance_guard(canonical_root, session_id)?;
    if destination.exists() {
        return Ok(HistoricalStorageRelocation::DestinationConflict);
    }
    fs::create_dir_all(canonical_root)?;
    fs::rename(source, destination)?;
    drop(destination_maintenance);
    drop(source_maintenance);
    Ok(HistoricalStorageRelocation::Relocated)
}

fn map_historical_storage_error(
    error: HistoricalStorageError<SessionStoreError>,
) -> SessionStoreError {
    match error {
        HistoricalStorageError::Io(error) => SessionStoreError::Io(error),
        HistoricalStorageError::Coordination(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_reports_conflict_without_mutation() {
        let state = tempfile::tempdir().expect("state");
        let session_id = SessionId::new();
        let source = accidental_epoch_session_root(state.path()).join(session_id.to_string());
        let destination = state.path().join("sessions").join(session_id.to_string());
        fs::create_dir_all(&source).expect("source");
        fs::create_dir_all(&destination).expect("destination");

        let report = inspect_accidental_epoch_session_root(state.path()).expect("inspect");
        assert_eq!(report.destination_conflicts, vec![session_id]);
        assert!(source.exists());
        assert!(destination.exists());
    }

    #[test]
    fn recovery_relocates_once_and_is_idempotent() {
        let state = tempfile::tempdir().expect("state");
        let session_id = SessionId::new();
        let source = accidental_epoch_session_root(state.path()).join(session_id.to_string());
        let destination = state.path().join("sessions").join(session_id.to_string());
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("session.db"), b"fixture").expect("source fixture");

        let report = recover_accidental_epoch_session_root(state.path()).expect("recover");
        assert_eq!(report.relocated, vec![session_id]);
        assert!(!source.exists());
        assert_eq!(
            fs::read(destination.join("session.db")).expect("moved"),
            b"fixture"
        );
        assert_eq!(
            recover_accidental_epoch_session_root(state.path()).expect("repeat"),
            LegacyStorageRecoveryReport::default()
        );
    }

    #[test]
    fn recovery_reports_destination_conflict_without_merging() {
        let state = tempfile::tempdir().expect("state");
        let session_id = SessionId::new();
        let source = accidental_epoch_session_root(state.path()).join(session_id.to_string());
        let destination = state.path().join("sessions").join(session_id.to_string());
        fs::create_dir_all(&source).expect("source");
        fs::create_dir_all(&destination).expect("destination");
        fs::write(source.join("source"), b"source").expect("source fixture");
        fs::write(destination.join("destination"), b"destination").expect("destination fixture");

        let report = recover_accidental_epoch_session_root(state.path()).expect("recover");
        assert_eq!(report.destination_conflicts, vec![session_id]);
        assert!(source.join("source").exists());
        assert!(destination.join("destination").exists());
    }

    #[test]
    fn recovery_reports_live_owner_without_moving_session() {
        let state = tempfile::tempdir().expect("state");
        let historical = accidental_epoch_session_root(state.path());
        let session_id = SessionId::new();
        let source = historical.join(session_id.to_string());
        fs::create_dir_all(&source).expect("source");
        let _owner = lease::acquire_session_lease(
            &historical,
            session_id,
            &lease::SessionLeaseOwnerContext::default(),
        )
        .expect("owner");

        let report = recover_accidental_epoch_session_root(state.path()).expect("recover");
        assert_eq!(report.blocked_by_owner, vec![session_id]);
        assert!(source.exists());
    }
}
