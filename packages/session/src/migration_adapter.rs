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
pub fn map_historical_storage_error(
    error: HistoricalStorageError<SessionStorageRecoveryError>,
) -> SessionStorageRecoveryError {
    match error {
        HistoricalStorageError::Io(error) => SessionStorageRecoveryError::Io(error),
        HistoricalStorageError::Coordination(error) => error,
    }
}
