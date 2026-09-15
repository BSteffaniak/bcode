//! Explicit database space reclamation under verified session maintenance ownership.
//!
//! This operation never runs from history/catalog reads. Canonical events stay in the same
//! database; database-engine VACUUM performs physical compaction without replay or reindexing.

use crate::artifact_storage::ArtifactMaintenanceCancellation;
use crate::db::{SessionDb, SessionDbError};
use bcode_session_models::SessionId;
use std::path::Path;

/// Outcome of an explicit reclamation attempt. No outcome implies history repair or migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionReclamationOutcome {
    /// The database has no whole free pages; no compaction was attempted.
    NotNeeded,
    /// The engine completed physical compaction.
    Reclaimed(SessionReclamation),
    /// The configured engine refuses compaction without an unsupported/experimental option.
    BackendUnsupported {
        /// Measured free-page capacity, not bytes already reclaimed.
        reclaimable_bytes: u64,
    },
}

/// Physical database lengths observed before and after explicit reclamation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionReclamation {
    /// Main database file length before reclamation; excludes engine sidecars.
    pub before_bytes: u64,
    /// Main database file length after engine compaction and close.
    pub after_bytes: u64,
}

impl SessionReclamation {
    /// Observed reduction; zero if checkpointing caused the main database to grow.
    #[must_use]
    pub const fn reclaimed_bytes(self) -> u64 {
        self.before_bytes.saturating_sub(self.after_bytes)
    }
}

/// Compact one current-format, idle canonical session database explicitly.
///
/// The exact locked Turso engine owns journaling and interruption recovery. Only this offline
/// maintenance connection enables its experimental VACUUM option; normal connections do not.
/// There is no fallback to copying history, rebuilding projections, or manipulating WAL files. This
/// can perform work proportional to database size and is not an interactive read operation.
/// Dropping the async waiter requests cancellation before VACUUM starts, but does not release
/// maintenance ownership: the owned operation finishes database close first. This backend operation
/// has no supported mid-VACUUM cancellation primitive.
///
/// # Errors
/// Refuses missing/unconfined canonical storage, active/unverifiable ownership, incompatible
/// storage, backend VACUUM errors, close errors, and filesystem failures. No substitute root is used.
pub async fn reclaim_session_storage(
    root: &Path,
    id: SessionId,
) -> Result<SessionReclamation, SessionDbError> {
    match try_reclaim_session_storage(root, id).await? {
        SessionReclamationOutcome::Reclaimed(report) => Ok(report),
        SessionReclamationOutcome::NotNeeded => {
            let length = std::fs::metadata(root.join(id.to_string()).join("session.db"))?.len();
            Ok(SessionReclamation {
                before_bytes: length,
                after_bytes: length,
            })
        }
        SessionReclamationOutcome::BackendUnsupported { .. } => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "configured session backend does not support space reclamation",
        )
        .into()),
    }
}

/// Attempt reclamation while preserving a typed backend-capability result.
///
/// # Errors
/// Returns ownership, compatibility, IO, or unexpected backend errors. Known disabled VACUUM
/// capability is reported as `BackendUnsupported`, never as reclaimed space or a repair attempt.
pub async fn try_reclaim_session_storage(
    root: &Path,
    id: SessionId,
) -> Result<SessionReclamationOutcome, SessionDbError> {
    let root = root.to_path_buf();
    let cancellation = ArtifactMaintenanceCancellation::default();
    run_owned_reclamation(cancellation.clone(), async move {
        reclaim_session_storage_owned(&root, id, None, cancellation).await
    })
    .await
}

/// Reclaim only when tracking remains old enough and the registry grants automatic admission.
///
/// # Errors
/// Returns errors for unknown/dirty tracking, unavailable ownership, incompatible storage, or IO.
/// The final age check and compaction retain one maintenance lease; no canonical replay occurs.
pub async fn reclaim_idle_session_storage(
    root: &Path,
    id: SessionId,
    now_ms: u64,
    minimum_age_ms: u64,
    minimum_free_bytes: u64,
) -> Result<SessionReclamationOutcome, SessionDbError> {
    reclaim_idle_session_storage_admitted(
        root,
        id,
        now_ms,
        minimum_age_ms,
        minimum_free_bytes,
        ArtifactMaintenanceCancellation::default(),
    )
    .await
}

/// Reclaim eligible storage using the live-daemon acknowledgement carried by the operation context.
///
/// Cancellation and health are checked before starting VACUUM. Once the engine operation starts,
/// it is drained to completion while admission and ownership remain held; it is not rolled back by
/// dropping a waiter. This prevents cancellation from releasing a live compaction's authority.
///
/// # Errors
/// Returns age, registry, ownership, compatibility, cancellation or backend failures.
pub async fn reclaim_idle_session_storage_admitted(
    root: &Path,
    id: SessionId,
    now_ms: u64,
    minimum_age_ms: u64,
    minimum_free_bytes: u64,
    cancellation: ArtifactMaintenanceCancellation,
) -> Result<SessionReclamationOutcome, SessionDbError> {
    cancellation.check()?;
    let root = root.to_path_buf();
    run_owned_reclamation(cancellation.clone(), async move {
        reclaim_session_storage_owned(
            &root,
            id,
            Some((now_ms, minimum_age_ms, minimum_free_bytes)),
            cancellation,
        )
        .await
    })
    .await
}

struct CancelOnAbandon(Option<ArtifactMaintenanceCancellation>);

impl Drop for CancelOnAbandon {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.0 {
            cancellation.cancel();
        }
    }
}

// Dropping a JoinHandle does not abort the task. Request cancellation, but retain ownership in
// the task until it observes cancellation or finishes an already-started engine operation.
async fn run_owned_reclamation<T: Send + 'static>(
    cancellation: ArtifactMaintenanceCancellation,
    operation: impl std::future::Future<Output = Result<T, SessionDbError>> + Send + 'static,
) -> Result<T, SessionDbError> {
    let mut abandoned = CancelOnAbandon(Some(cancellation));
    let result = tokio::spawn(operation).await.map_err(|_| {
        SessionDbError::Io(std::io::Error::other("session reclamation task failed"))
    })?;
    abandoned.0 = None;
    result
}

async fn reclaim_session_storage_owned(
    root: &Path,
    id: SessionId,
    eligibility: Option<(u64, u64, u64)>,
    cancellation: ArtifactMaintenanceCancellation,
) -> Result<SessionReclamationOutcome, SessionDbError> {
    cancellation.check()?;
    let root = root.canonicalize()?;
    let directory = root.join(id.to_string());
    if !std::fs::symlink_metadata(&directory)?.is_dir()
        || !directory.canonicalize()?.starts_with(&root)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unsafe session directory",
        )
        .into());
    }
    let path = directory.join("session.db");
    if !std::fs::symlink_metadata(&path)?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unsafe canonical database",
        )
        .into());
    }
    let _admission = if eligibility.is_some() {
        Some(cancellation.admit_tracking(&root).await?)
    } else {
        None
    };
    let lock_root = root.clone();
    let maintenance = tokio::task::spawn_blocking(move || {
        crate::lease::acquire_session_maintenance_guard(&lock_root, id)
    })
    .await
    .map_err(|_| std::io::Error::other("maintenance acquisition failed"))?
    .map_err(std::io::Error::other)?;
    if let Some((now_ms, minimum_age_ms, _)) = eligibility {
        let crate::storage_access::StorageAccessObservation::Recorded(record) =
            crate::storage_access::observe_session_access(&root, id)?
        else {
            return Err(std::io::Error::other("unknown session access age").into());
        };
        if now_ms
            .checked_sub(record.observed_at_ms)
            .is_none_or(|age| age < minimum_age_ms)
        {
            return Ok(SessionReclamationOutcome::NotNeeded);
        }
    }
    let db = SessionDb::open_existing_turso_in_root(id, &root).await?;
    let before_bytes = std::fs::metadata(&path)?.len();
    let capacity = db.reclaimable_bytes().await;
    let closed = db.database().close().await;
    drop(db);
    closed?;
    let capacity = capacity?;
    if capacity == 0 || eligibility.is_some_and(|(_, _, minimum)| capacity < minimum) {
        return Ok(SessionReclamationOutcome::NotNeeded);
    }
    // Same locked engine as normal sessions; only this exclusively owned maintenance connection
    // opts into VACUUM. The engine owns atomicity, temporary files, and WAL recovery.
    cancellation.check()?;
    let result = vacuum_with_maintenance_engine(&path).await;
    let after_bytes = std::fs::metadata(&path).map(|metadata| metadata.len());
    drop(maintenance);
    result?;
    Ok(SessionReclamationOutcome::Reclaimed(SessionReclamation {
        before_bytes,
        after_bytes: after_bytes?,
    }))
}

async fn vacuum_with_maintenance_engine(path: &Path) -> Result<(), SessionDbError> {
    let path = path.to_str().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-UTF8 database path")
    })?;
    let database = turso::Builder::new_local(path)
        .experimental_vacuum(true)
        .experimental_multiprocess_wal(false)
        .build()
        .await
        .map_err(std::io::Error::other)?;
    let connection = database.connect().map_err(std::io::Error::other)?;
    let result = connection.execute("VACUUM", ()).await;
    drop(connection);
    drop(database);
    result
        .map(|_| ())
        .map_err(|error| std::io::Error::other(error).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn abandoning_waiter_cancels_but_drains_owned_operation() {
        let cancellation = ArtifactMaintenanceCancellation::default();
        let observed = cancellation.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let (finished, drained) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(run_owned_reclamation(cancellation, async move {
            started.send(()).expect("started");
            released.await.expect("release");
            assert!(observed.check().is_err());
            finished.send(()).expect("drained");
            Ok(())
        }));
        ready.await.expect("running");
        waiter.abort();
        assert!(waiter.await.expect_err("aborted").is_cancelled());
        release.send(()).expect("operation still alive");
        drained.await.expect("operation drained after cancellation");
    }

    #[tokio::test]
    async fn completed_waiter_does_not_cancel_shared_token() {
        let cancellation = ArtifactMaintenanceCancellation::default();
        run_owned_reclamation(cancellation.clone(), async { Ok(()) })
            .await
            .expect("complete");
        cancellation.check().expect("still healthy");
    }

    #[tokio::test]
    async fn cancelled_admitted_reclamation_does_not_create_storage() {
        let root = tempfile::tempdir().expect("root");
        let cancellation = ArtifactMaintenanceCancellation::default();
        cancellation.cancel();
        assert!(
            reclaim_idle_session_storage_admitted(
                root.path(),
                SessionId::new(),
                100,
                10,
                1,
                cancellation
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read_dir(root.path()).expect("entries").count(), 0);
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_release_running_maintenance() {
        let root = tempfile::tempdir().expect("root");
        let id = SessionId::new();
        let db = SessionDb::open_turso_in_root(id, root.path())
            .await
            .expect("db");
        db.database().close().await.expect("close");
        drop(db);
        let owned_root = root.path().to_path_buf();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (finish, finished) = tokio::sync::oneshot::channel();
        let (released, release_done) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(run_owned_reclamation(
            ArtifactMaintenanceCancellation::default(),
            async move {
                let guard = crate::lease::acquire_session_maintenance_guard(&owned_root, id)
                    .map_err(std::io::Error::other)?;
                started.send(()).expect("ready");
                finished.await.expect("finish signal");
                drop(guard);
                released.send(()).expect("released");
                Ok(())
            },
        ));
        ready.await.expect("started");
        waiter.abort();
        assert!(waiter.await.expect_err("cancelled").is_cancelled());
        // This probe uses the same underlying exclusive coordinator and must remain refused.
        let probe_root = root.path().to_path_buf();
        let blocked = tokio::task::spawn_blocking(move || {
            crate::lease::acquire_session_lease(
                &probe_root,
                id,
                &crate::lease::SessionLeaseOwnerContext::default(),
            )
            .is_err()
        })
        .await
        .expect("probe");
        assert!(blocked);
        finish.send(()).expect("finish");
        release_done.await.expect("terminal release");
        drop(crate::lease::acquire_session_maintenance_guard(root.path(), id).expect("new owner"));
    }

    #[tokio::test]
    async fn free_page_accounting_detects_reclaimable_capacity_without_vacuum() {
        let root = tempfile::tempdir().expect("root");
        let id = SessionId::new();
        let db = SessionDb::open_turso_in_root(id, root.path())
            .await
            .expect("db");
        let before = db.reclaimable_bytes().await.expect("before");
        db.database()
            .exec_raw("CREATE TABLE space_fixture (content BLOB)")
            .await
            .expect("table");
        db.database()
            .exec_raw("INSERT INTO space_fixture VALUES (zeroblob(1048576))")
            .await
            .expect("grow");
        db.database()
            .exec_raw("DROP TABLE space_fixture")
            .await
            .expect("free");
        let available = db.reclaimable_bytes().await.expect("available");
        assert!(available > before);
        assert!(available >= 1024 * 1024);
        db.database().close().await.expect("close");
    }

    #[tokio::test]
    async fn reclamation_refuses_live_ownership_and_missing_sessions() {
        let root = tempfile::tempdir().expect("root");
        let id = SessionId::new();
        assert!(reclaim_session_storage(root.path(), id).await.is_err());
        assert!(!root.path().join(id.to_string()).exists());
        let db = SessionDb::open_turso_in_root(id, root.path())
            .await
            .expect("db");
        db.database().close().await.expect("close");
        drop(db);
        let _owner = crate::lease::acquire_session_lease(
            root.path(),
            id,
            &crate::lease::SessionLeaseOwnerContext::default(),
        )
        .expect("owner");
        assert!(reclaim_session_storage(root.path(), id).await.is_err());
    }

    #[tokio::test]
    async fn vacuum_preserves_nonempty_history_and_current_writer_contract() {
        let root = tempfile::tempdir().expect("root");
        let manager = crate::SessionManager::persistent(root.path()).expect("manager");
        let session = manager
            .create_session(Some("reclamation".into()), root.path().to_path_buf())
            .await
            .expect("session");
        let id = session.id;
        let history = manager.session_history(id).await.expect("history");
        manager
            .release_session_ownership(id)
            .await
            .expect("release");
        drop(manager);
        let db = SessionDb::open_existing_turso_in_root(id, root.path())
            .await
            .expect("db");
        let epoch = db.storage_writer_epoch().await.expect("epoch");
        db.database()
            .exec_raw("CREATE TABLE free_fixture (content BLOB)")
            .await
            .expect("fixture");
        db.database()
            .exec_raw("INSERT INTO free_fixture VALUES (zeroblob(2097152))")
            .await
            .expect("grow");
        db.database()
            .exec_raw("DROP TABLE free_fixture")
            .await
            .expect("free");
        db.database().close().await.expect("close");
        drop(db);
        let report = reclaim_session_storage(root.path(), id)
            .await
            .expect("vacuum");
        assert!(report.reclaimed_bytes() > 0);
        let db = SessionDb::open_existing_turso_in_root(id, root.path())
            .await
            .expect("reopen");
        assert_eq!(db.all_events_strict().await.expect("same history"), history);
        assert_eq!(db.storage_writer_epoch().await.expect("same epoch"), epoch);
        db.storage_compatibility()
            .await
            .expect("current compatibility");
    }

    #[tokio::test]
    async fn reclamation_reduces_file_size_and_releases_ownership() {
        let root = tempfile::tempdir().expect("root");
        let id = SessionId::new();
        let db = SessionDb::open_turso_in_root(id, root.path())
            .await
            .expect("db");
        db.database()
            .exec_raw("CREATE TABLE reclamation_fixture (content BLOB)")
            .await
            .expect("fixture");
        db.database()
            .exec_raw("INSERT INTO reclamation_fixture VALUES (zeroblob(4194304))")
            .await
            .expect("grow");
        db.database()
            .exec_raw("DELETE FROM reclamation_fixture")
            .await
            .expect("free pages");
        db.database()
            .exec_raw("DROP TABLE reclamation_fixture")
            .await
            .expect("drop fixture");
        db.database().close().await.expect("close");
        drop(db);
        let path = root.path().join(id.to_string()).join("session.db");
        let before = std::fs::metadata(&path).expect("before").len();
        let outcome = try_reclaim_session_storage(root.path(), id)
            .await
            .expect("reclaimed");
        let SessionReclamationOutcome::Reclaimed(report) = outcome else {
            panic!("expected reclamation: {outcome:?}");
        };
        assert_eq!(report.before_bytes, before);
        assert!(report.reclaimed_bytes() >= 4 * 1024 * 1024);
        assert_eq!(
            std::fs::metadata(&path).expect("after").len(),
            report.after_bytes
        );
        drop(crate::lease::acquire_session_maintenance_guard(root.path(), id).expect("released"));
        let db = SessionDb::open_existing_turso_in_root(id, root.path())
            .await
            .expect("reopen");
        assert!(db.all_events_strict().await.expect("history").is_empty());
    }
}
