//! Current-session ownership and relocation primitives.

use bcode_session_models::SessionId;
use std::fs;
use std::path::Path;
use thiserror::Error;

/// Errors produced while coordinating current session ownership and paths.
#[derive(Debug, Error)]
pub enum SessionStorageRecoveryError {
    /// Filesystem inspection or relocation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Current session lease coordination failed.
    #[error(transparent)]
    Lease(#[from] crate::lease::SessionLeaseError),
}

/// Outcome of atomically relocating one session directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStorageRelocation {
    /// The source was atomically moved into canonical storage.
    Relocated,
    /// A live owner prevented relocation.
    BlockedByOwner,
    /// The canonical destination exists and was not overwritten.
    DestinationConflict,
}

/// Return whether a session at an explicitly supplied root currently has a live owner.
///
/// # Errors
///
/// Returns an error when lease metadata cannot be inspected.
pub fn session_has_active_owner(
    session_id: SessionId,
    root: &Path,
) -> Result<bool, SessionStorageRecoveryError> {
    crate::lease::active_session_owners(root, session_id)
        .map(|owners| !owners.is_empty())
        .map_err(Into::into)
}

/// Atomically relocate one session after coordinating both source and destination.
///
/// This current-runtime primitive has no knowledge of historical root identities or migration
/// policy. Callers supply both roots and interpret the typed outcome.
///
/// # Errors
///
/// Returns an error when lease coordination or atomic rename fails.
pub fn relocate_session(
    session_id: SessionId,
    source_root: &Path,
    destination_root: &Path,
) -> Result<SessionStorageRelocation, SessionStorageRecoveryError> {
    let source = crate::db::session_dir_path(source_root, session_id);
    let destination = crate::db::session_dir_path(destination_root, session_id);
    if destination.exists() {
        return Ok(SessionStorageRelocation::DestinationConflict);
    }
    let source_maintenance =
        match crate::lease::acquire_session_maintenance_guard(source_root, session_id) {
            Ok(guard) => guard,
            Err(crate::lease::SessionLeaseError::OwnedByOtherDaemon { .. }) => {
                return Ok(SessionStorageRelocation::BlockedByOwner);
            }
            Err(error) => return Err(error.into()),
        };
    let destination_maintenance =
        crate::lease::acquire_session_maintenance_guard(destination_root, session_id)?;
    if destination.exists() {
        return Ok(SessionStorageRelocation::DestinationConflict);
    }
    fs::create_dir_all(destination_root)?;
    fs::rename(source, destination)?;
    drop(destination_maintenance);
    drop(source_maintenance);
    Ok(SessionStorageRelocation::Relocated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relocation_moves_once_without_overwriting_a_destination() {
        let state = tempfile::tempdir().expect("state");
        let source_root = state.path().join("source");
        let destination_root = state.path().join("destination");
        let session_id = SessionId::new();
        let source = source_root.join(session_id.to_string());
        let destination = destination_root.join(session_id.to_string());
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("session.db"), b"fixture").expect("fixture");

        assert_eq!(
            relocate_session(session_id, &source_root, &destination_root).expect("relocate"),
            SessionStorageRelocation::Relocated
        );
        assert_eq!(
            fs::read(destination.join("session.db")).expect("moved"),
            b"fixture"
        );

        fs::create_dir_all(&source).expect("replacement source");
        fs::write(source.join("other"), b"other").expect("other fixture");
        assert_eq!(
            relocate_session(session_id, &source_root, &destination_root).expect("conflict"),
            SessionStorageRelocation::DestinationConflict
        );
        assert!(source.join("other").exists());
    }

    #[test]
    fn relocation_reports_live_source_owner() {
        let state = tempfile::tempdir().expect("state");
        let source_root = state.path().join("source");
        let destination_root = state.path().join("destination");
        let session_id = SessionId::new();
        let source = source_root.join(session_id.to_string());
        fs::create_dir_all(&source).expect("source");
        let _owner = crate::lease::acquire_session_lease(
            &source_root,
            session_id,
            &crate::lease::SessionLeaseOwnerContext::default(),
        )
        .expect("owner");

        assert_eq!(
            relocate_session(session_id, &source_root, &destination_root).expect("blocked"),
            SessionStorageRelocation::BlockedByOwner
        );
        assert!(source.exists());
    }
}
