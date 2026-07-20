//! Recovery for the removed historical writer-epoch session root.
//!
//! This module is the only production code allowed to recognize the abandoned
//! `session-storage/writer-epoch-2` layout. Normal session access always uses the canonical
//! `sessions/<session-id>/session.db` location.

use crate::{SessionStoreError, db, lease};
use bcode_session_models::SessionId;
use std::fs;
use std::path::Path;

/// Outcome of scanning the removed historical writer-epoch root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacyStorageRecoveryReport {
    /// Sessions atomically relocated into the canonical store.
    pub relocated: Vec<SessionId>,
    /// Sessions left untouched because a live owner still uses the historical root.
    pub blocked_by_owner: Vec<SessionId>,
    /// Sessions left untouched because the canonical destination already exists.
    pub destination_conflicts: Vec<SessionId>,
}

/// Non-mutating diagnosis of the removed historical writer-epoch root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacyStorageInspectionReport {
    /// Sessions that can be relocated when recovery next runs.
    pub pending_relocation: Vec<SessionId>,
    /// Sessions currently protected by a live owner.
    pub blocked_by_owner: Vec<SessionId>,
    /// Sessions with both historical and canonical directories.
    pub destination_conflicts: Vec<SessionId>,
}

/// Inspect the removed writer-epoch root without modifying files or owner metadata.
///
/// # Errors
///
/// Returns an error when directory or owner inspection fails.
pub fn inspect_accidental_epoch_session_root(
    state_dir: &Path,
) -> Result<LegacyStorageInspectionReport, SessionStoreError> {
    let historical_root = historical_root(state_dir);
    let canonical_root = state_dir.join("sessions");
    let mut report = LegacyStorageInspectionReport::default();
    for session_id in historical_session_ids(&historical_root)? {
        if db::session_dir_path(&canonical_root, session_id).exists() {
            report.destination_conflicts.push(session_id);
        } else if lease::active_session_owners(&historical_root, session_id)?.is_empty() {
            report.pending_relocation.push(session_id);
        } else {
            report.blocked_by_owner.push(session_id);
        }
    }
    Ok(report)
}

/// Relocate unambiguous sessions from the removed writer-epoch root into canonical storage.
///
/// The function never opens session databases, merges directories, overwrites canonical data, or
/// selects the historical root for normal access. Conflicts and live ownership are reported for
/// diagnosis. Empty historical directories are removed after successful relocation.
///
/// # Errors
///
/// Returns an error when directory inspection, coordination, or atomic relocation fails.
pub fn recover_accidental_epoch_session_root(
    state_dir: &Path,
) -> Result<LegacyStorageRecoveryReport, SessionStoreError> {
    let historical_root = historical_root(state_dir);
    if !historical_root.exists() {
        return Ok(LegacyStorageRecoveryReport::default());
    }
    let canonical_root = state_dir.join("sessions");
    let session_ids = historical_session_ids(&historical_root)?;
    let mut report = LegacyStorageRecoveryReport::default();
    for session_id in session_ids {
        let source = db::session_dir_path(&historical_root, session_id);
        let destination = db::session_dir_path(&canonical_root, session_id);
        if destination.exists() {
            report.destination_conflicts.push(session_id);
            continue;
        }
        let source_maintenance =
            match lease::acquire_session_maintenance_guard(&historical_root, session_id) {
                Ok(guard) => guard,
                Err(lease::SessionLeaseError::OwnedByOtherDaemon { .. }) => {
                    report.blocked_by_owner.push(session_id);
                    continue;
                }
                Err(error) => return Err(SessionStoreError::Lease(error)),
            };
        let destination_maintenance =
            lease::acquire_session_maintenance_guard(&canonical_root, session_id)?;
        if destination.exists() {
            report.destination_conflicts.push(session_id);
            continue;
        }
        fs::create_dir_all(&canonical_root)?;
        fs::rename(&source, &destination)?;
        report.relocated.push(session_id);
        drop(destination_maintenance);
        drop(source_maintenance);
    }
    remove_empty_dir(historical_root.join("leases"));
    remove_empty_dir(historical_root.join("locks"));
    remove_empty_dir(historical_root.clone());
    if let Some(parent) = historical_root.parent() {
        remove_empty_dir(parent.to_path_buf());
    }
    Ok(report)
}

/// Return the exact removed writer-epoch root for read-only diagnosis.
#[must_use]
pub fn accidental_epoch_session_root(state_dir: &Path) -> std::path::PathBuf {
    historical_root(state_dir)
}

fn historical_root(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("session-storage").join("writer-epoch-2")
}

fn historical_session_ids(root: &Path) -> Result<Vec<SessionId>, SessionStoreError> {
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

fn remove_empty_dir(path: std::path::PathBuf) {
    let _ = fs::remove_dir(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_reports_conflict_without_mutation() {
        let state = tempfile::tempdir().expect("state");
        let session_id = SessionId::new();
        let source = state
            .path()
            .join("session-storage/writer-epoch-2")
            .join(session_id.to_string());
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
        let source = state
            .path()
            .join("session-storage/writer-epoch-2")
            .join(session_id.to_string());
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("session.db"), b"fixture").expect("fixture");

        let report = recover_accidental_epoch_session_root(state.path()).expect("recover");
        assert_eq!(report.relocated, vec![session_id]);
        assert!(
            state
                .path()
                .join("sessions")
                .join(session_id.to_string())
                .exists()
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
        let source = state
            .path()
            .join("session-storage/writer-epoch-2")
            .join(session_id.to_string());
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
        let historical = state.path().join("session-storage/writer-epoch-2");
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
