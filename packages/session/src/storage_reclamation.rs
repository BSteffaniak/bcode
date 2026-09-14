//! Explicit database space reclamation under verified session maintenance ownership.
//!
//! This operation never runs from history/catalog reads. Canonical events stay in the same
//! database; database-engine VACUUM performs physical compaction without replay or reindexing.

use crate::db::{SessionDb, SessionDbError};
use bcode_session_models::SessionId;
use std::path::Path;

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
/// The backend owns journaling and interruption recovery. Unsupported compaction is an error,
/// never a fallback to copying history, rebuilding projections, or manipulating WAL files. This
/// can perform work proportional to database size and is not an interactive read operation.
/// Dropping the async waiter does not release maintenance ownership: the owned operation finishes
/// database close first. This backend operation has no supported mid-VACUUM cancellation primitive.
///
/// # Errors
/// Refuses missing/unconfined canonical storage, active/unverifiable ownership, incompatible
/// storage, backend VACUUM errors, close errors, and filesystem failures. No substitute root is used.
pub async fn reclaim_session_storage(
    root: &Path,
    id: SessionId,
) -> Result<SessionReclamation, SessionDbError> {
    let root = root.to_path_buf();
    run_owned_reclamation(async move { reclaim_session_storage_owned(&root, id).await }).await
}

// Dropping a JoinHandle does not abort the task. The task, not its optional waiter, owns the lease
// and database connection through terminal close. This is not a durable resumable operation.
async fn run_owned_reclamation<T: Send + 'static>(
    operation: impl std::future::Future<Output = Result<T, SessionDbError>> + Send + 'static,
) -> Result<T, SessionDbError> {
    tokio::spawn(operation)
        .await
        .map_err(|_| SessionDbError::Io(std::io::Error::other("session reclamation task failed")))?
}

async fn reclaim_session_storage_owned(
    root: &Path,
    id: SessionId,
) -> Result<SessionReclamation, SessionDbError> {
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
    let lock_root = root.clone();
    let maintenance = tokio::task::spawn_blocking(move || {
        crate::lease::acquire_session_maintenance_guard(&lock_root, id)
    })
    .await
    .map_err(|_| std::io::Error::other("maintenance acquisition failed"))?
    .map_err(std::io::Error::other)?;
    let db = SessionDb::open_existing_turso_in_root(id, &root).await?;
    let before_bytes = std::fs::metadata(&path)?.len();
    let result = db.reclaim_free_pages().await;
    let closed = db.database().close().await;
    drop(db);
    // Both success and failure close the maintenance connection before releasing its lease.
    let after_bytes = std::fs::metadata(path).map(|metadata| metadata.len());
    drop(maintenance);
    result?;
    closed?;
    Ok(SessionReclamation {
        before_bytes,
        after_bytes: after_bytes?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let waiter = tokio::spawn(run_owned_reclamation(async move {
            let guard = crate::lease::acquire_session_maintenance_guard(&owned_root, id)
                .map_err(std::io::Error::other)?;
            started.send(()).expect("ready");
            finished.await.expect("finish signal");
            drop(guard);
            released.send(()).expect("released");
            Ok(())
        }));
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
    async fn unsupported_reclamation_preserves_storage_and_releases_ownership() {
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
        let before = std::fs::read(&path).expect("before");
        // The locked backend disables experimental VACUUM. Preserve this refusal instead of
        // enabling an experimental storage feature implicitly or rewriting with another engine.
        assert!(reclaim_session_storage(root.path(), id).await.is_err());
        assert_eq!(std::fs::read(&path).expect("after"), before);
        drop(crate::lease::acquire_session_maintenance_guard(root.path(), id).expect("released"));
        let db = SessionDb::open_existing_turso_in_root(id, root.path())
            .await
            .expect("reopen");
        assert!(db.all_events_strict().await.expect("history").is_empty());
    }
}
