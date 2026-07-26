//! CLI composition adapters joining migration diagnosis to current ownership facts.

use bcode_session::ownership::{SessionStorageRecoveryError, session_has_active_owner};
use bcode_session_migration::HistoricalStorageError;
use bcode_session_models::SessionId;
use std::path::Path;

fn historical_session_has_active_owner(
    session_id: SessionId,
    historical_root: &Path,
) -> Result<bool, SessionStorageRecoveryError> {
    session_has_active_owner(session_id, historical_root)
}

fn map_historical_storage_error(
    error: HistoricalStorageError<SessionStorageRecoveryError>,
) -> SessionStorageRecoveryError {
    match error {
        HistoricalStorageError::Io(error) => SessionStorageRecoveryError::Io(error),
        HistoricalStorageError::Coordination(error) => error,
    }
}

pub fn diagnose_historical_session_storage(
    state_dir: &Path,
) -> Result<bcode_session_migration::HistoricalStorageDiagnosis, SessionStorageRecoveryError> {
    bcode_session_migration::diagnose_accidental_epoch_session_root(
        state_dir,
        historical_session_has_active_owner,
    )
    .map_err(map_historical_storage_error)
}
