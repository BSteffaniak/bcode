//! Current-session coordination adapters used by migration-owned storage recovery.

use bcode_session_migration::{HistoricalStorageError, HistoricalStorageRelocation};
use bcode_session_models::SessionId;
use std::fs;
use std::path::Path;
use thiserror::Error;

/// Errors produced while adapting migration-owned recovery to current lease/path primitives.
#[derive(Debug, Error)]
pub enum SessionStorageRecoveryError {
    /// Filesystem inspection or relocation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Current session lease coordination failed.
    #[error(transparent)]
    Lease(#[from] crate::lease::SessionLeaseError),
}

/// Return whether a historical-root session currently has a live owner.
///
/// # Errors
///
/// Returns an error when lease metadata cannot be inspected.
pub fn historical_session_has_active_owner(
    session_id: SessionId,
    historical_root: &Path,
) -> Result<bool, SessionStorageRecoveryError> {
    crate::lease::active_session_owners(historical_root, session_id)
        .map(|owners| !owners.is_empty())
        .map_err(Into::into)
}

/// Atomically relocate one historical session after coordinating both source and destination.
///
/// # Errors
///
/// Returns an error when lease coordination or atomic rename fails.
pub fn relocate_historical_session(
    session_id: SessionId,
    historical_root: &Path,
    canonical_root: &Path,
) -> Result<HistoricalStorageRelocation, SessionStorageRecoveryError> {
    let source = crate::db::session_dir_path(historical_root, session_id);
    let destination = crate::db::session_dir_path(canonical_root, session_id);
    if destination.exists() {
        return Ok(HistoricalStorageRelocation::DestinationConflict);
    }
    let source_maintenance =
        match crate::lease::acquire_session_maintenance_guard(historical_root, session_id) {
            Ok(guard) => guard,
            Err(crate::lease::SessionLeaseError::OwnedByOtherDaemon { .. }) => {
                return Ok(HistoricalStorageRelocation::BlockedByOwner);
            }
            Err(error) => return Err(error.into()),
        };
    let destination_maintenance =
        crate::lease::acquire_session_maintenance_guard(canonical_root, session_id)?;
    if destination.exists() {
        return Ok(HistoricalStorageRelocation::DestinationConflict);
    }
    fs::create_dir_all(canonical_root)?;
    fs::rename(source, destination)?;
    drop(destination_maintenance);
    drop(source_maintenance);
    Ok(HistoricalStorageRelocation::Relocated)
}

/// Map migration-owned storage recovery errors into the current adapter error.
#[must_use]
pub fn map_historical_storage_error(
    error: HistoricalStorageError<SessionStorageRecoveryError>,
) -> SessionStorageRecoveryError {
    match error {
        HistoricalStorageError::Io(error) => SessionStorageRecoveryError::Io(error),
        HistoricalStorageError::Coordination(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_relocates_once_and_is_idempotent() {
        let state = tempfile::tempdir().expect("state");
        let session_id = SessionId::new();
        let historical = bcode_session_migration::accidental_epoch_session_root(state.path());
        let source = historical.join(session_id.to_string());
        let canonical = state.path().join("sessions");
        let destination = canonical.join(session_id.to_string());
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("session.db"), b"fixture").expect("fixture");

        let report = bcode_session_migration::recover_accidental_epoch_session_root(
            state.path(),
            relocate_historical_session,
        )
        .map_err(map_historical_storage_error)
        .expect("recover");
        assert_eq!(report.relocated, vec![session_id]);
        assert_eq!(
            fs::read(destination.join("session.db")).expect("moved"),
            b"fixture"
        );
        assert_eq!(
            bcode_session_migration::recover_accidental_epoch_session_root(
                state.path(),
                relocate_historical_session,
            )
            .map_err(map_historical_storage_error)
            .expect("repeat"),
            bcode_session_migration::HistoricalStorageRecoveryReport::default()
        );
    }

    #[test]
    fn recovery_reports_destination_conflict_without_merging() {
        let state = tempfile::tempdir().expect("state");
        let session_id = SessionId::new();
        let historical = bcode_session_migration::accidental_epoch_session_root(state.path());
        let source = historical.join(session_id.to_string());
        let destination = state.path().join("sessions").join(session_id.to_string());
        fs::create_dir_all(&source).expect("source");
        fs::create_dir_all(&destination).expect("destination");
        fs::write(source.join("source"), b"source").expect("source fixture");
        fs::write(destination.join("destination"), b"destination").expect("destination fixture");

        let report = bcode_session_migration::recover_accidental_epoch_session_root(
            state.path(),
            relocate_historical_session,
        )
        .map_err(map_historical_storage_error)
        .expect("recover");
        assert_eq!(report.destination_conflicts, vec![session_id]);
        assert!(source.join("source").exists());
        assert!(destination.join("destination").exists());
    }

    #[test]
    fn recovery_reports_live_owner_without_moving_session() {
        let state = tempfile::tempdir().expect("state");
        let historical = bcode_session_migration::accidental_epoch_session_root(state.path());
        let session_id = SessionId::new();
        let source = historical.join(session_id.to_string());
        fs::create_dir_all(&source).expect("source");
        let _owner = crate::lease::acquire_session_lease(
            &historical,
            session_id,
            &crate::lease::SessionLeaseOwnerContext::default(),
        )
        .expect("owner");

        let report = bcode_session_migration::recover_accidental_epoch_session_root(
            state.path(),
            relocate_historical_session,
        )
        .map_err(map_historical_storage_error)
        .expect("recover");
        assert_eq!(report.blocked_by_owner, vec![session_id]);
        assert!(source.exists());
    }
}
