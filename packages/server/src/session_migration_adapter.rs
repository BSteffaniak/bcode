//! Composition adapters joining migration-owned historical policy to current ownership primitives.

use bcode_session::ownership::{
    SessionStorageRecoveryError, SessionStorageRelocation, relocate_session,
};
use bcode_session_migration::{HistoricalStorageError, HistoricalStorageRelocation};
use bcode_session_models::SessionId;
use std::path::Path;

fn relocate_historical_session(
    session_id: SessionId,
    historical_root: &Path,
    canonical_root: &Path,
) -> Result<HistoricalStorageRelocation, SessionStorageRecoveryError> {
    relocate_session(session_id, historical_root, canonical_root).map(|outcome| match outcome {
        SessionStorageRelocation::Relocated => HistoricalStorageRelocation::Relocated,
        SessionStorageRelocation::BlockedByOwner => HistoricalStorageRelocation::BlockedByOwner,
        SessionStorageRelocation::DestinationConflict => {
            HistoricalStorageRelocation::DestinationConflict
        }
    })
}

fn map_historical_storage_error(
    error: HistoricalStorageError<SessionStorageRecoveryError>,
) -> SessionStorageRecoveryError {
    match error {
        HistoricalStorageError::Io(error) => SessionStorageRecoveryError::Io(error),
        HistoricalStorageError::Coordination(error) => error,
    }
}

pub fn recover_historical_session_storage(
    state_dir: &Path,
) -> Result<bcode_session_migration::HistoricalStorageRecoveryReport, SessionStorageRecoveryError> {
    bcode_session_migration::recover_accidental_epoch_session_root(
        state_dir,
        relocate_historical_session,
    )
    .map_err(map_historical_storage_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn recovery_relocates_once_and_is_idempotent() {
        let state = tempfile::tempdir().expect("state");
        let session_id = SessionId::new();
        let historical = bcode_session_migration::accidental_epoch_session_root(state.path());
        let source = historical.join(session_id.to_string());
        let destination = state.path().join("sessions").join(session_id.to_string());
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("session.db"), b"fixture").expect("fixture");

        let report = recover_historical_session_storage(state.path()).expect("recover");
        assert_eq!(report.relocated, vec![session_id]);
        assert_eq!(
            fs::read(destination.join("session.db")).expect("moved"),
            b"fixture"
        );
        assert_eq!(
            recover_historical_session_storage(state.path()).expect("repeat"),
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

        let report = recover_historical_session_storage(state.path()).expect("recover");
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
        let _owner = bcode_session::lease::acquire_session_lease(
            &historical,
            session_id,
            &bcode_session::lease::SessionLeaseOwnerContext::default(),
        )
        .expect("owner");

        let report = recover_historical_session_storage(state.path()).expect("recover");
        assert_eq!(report.blocked_by_owner, vec![session_id]);
        assert!(source.exists());
    }
}
