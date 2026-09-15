//! Bounded current-format canonical history recompression under offline ownership.

#[cfg(test)]
mod tests;

use crate::db::{SessionDb, SessionDbError};
use bcode_session_models::SessionId;
use std::path::Path;

struct CancelOnAbandon(Option<crate::artifact_storage::ArtifactMaintenanceCancellation>);
impl Drop for CancelOnAbandon {
    fn drop(&mut self) {
        if let Some(token) = &self.0 {
            token.cancel();
        }
    }
}

/// Result of one atomic history-maintenance page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryCompressionPage {
    /// Number of canonical rows inspected.
    pub inspected: usize,
    /// Rows whose physical payload representation became smaller.
    pub compressed: usize,
    /// Payload-column bytes saved, excluding database free-page reclamation.
    pub saved_bytes: u64,
    /// Exclusive continuation position. None means no more canonical rows were found.
    pub next_sequence: Option<u64>,
    /// Captured canonical tail for a finite sweep; retain it on subsequent page requests.
    pub through_sequence: Option<u64>,
}

/// Recompress one bounded page without changing logical events or derived projections.
///
/// The current writer contract fences older writers. Each page is atomic and retry-safe: already
/// smaller representations remain unchanged. Private JSON fields are preserved byte-for-byte.
/// Separate explicit reclamation releases free database pages after this operation completes.
///
/// # Errors
/// Rejects invalid compression levels, missing/unsafe storage, active ownership, incompatible
/// contracts, corrupt canonical events, codec failures or database errors. Dropping the waiter
/// does not release ownership before the owned task closes its database.
pub async fn compress_history_page(
    root: &Path,
    id: SessionId,
    start_sequence: u64,
    level: i32,
) -> Result<HistoryCompressionPage, SessionDbError> {
    compress_history_page_with_age(root, id, start_sequence, level, None).await
}

/// Compress a bounded history page with access age and per-event age checked under ownership.
///
/// # Errors
/// Rejects dirty registry state, unknown access evidence and the same failures as explicit
/// compression. Recent access returns an empty page without mutation. Events newer than the cutoff
/// are skipped, not removed; pagination still advances over them.
pub async fn compress_history_page_with_age(
    root: &Path,
    id: SessionId,
    start_sequence: u64,
    level: i32,
    age: Option<(u64, u64)>,
) -> Result<HistoryCompressionPage, SessionDbError> {
    compress_history_page_cancellable(
        root,
        id,
        start_sequence,
        level,
        age,
        crate::artifact_storage::ArtifactMaintenanceCancellation::default(),
    )
    .await
}

/// Compress an atomic history page with cooperative cancellation through commit.
///
/// # Errors
/// Returns age-checked compression failures or Interrupted on cancellation before commit. The
/// caller should await completion after cancellation; dropping the waiter also requests cancellation.
pub async fn compress_history_page_cancellable(
    root: &Path,
    id: SessionId,
    start_sequence: u64,
    level: i32,
    age: Option<(u64, u64)>,
    cancellation: crate::artifact_storage::ArtifactMaintenanceCancellation,
) -> Result<HistoryCompressionPage, SessionDbError> {
    compress_history_page_through(root, id, start_sequence, level, age, cancellation, None).await
}

/// Recompress a page within a captured finite history sweep.
///
/// Pass the returned `through_sequence` on subsequent pages. Newer canonical events are left
/// for the next sweep, so ongoing sessions do not monopolize background maintenance.
///
/// # Errors
/// Returns the same validation, ownership, cancellation and storage errors as cancellable pages.
pub async fn compress_history_page_through(
    root: &Path,
    id: SessionId,
    start_sequence: u64,
    level: i32,
    age: Option<(u64, u64)>,
    cancellation: crate::artifact_storage::ArtifactMaintenanceCancellation,
    through_sequence: Option<u64>,
) -> Result<HistoryCompressionPage, SessionDbError> {
    cancellation.check()?;
    if !(1..=22).contains(&level) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid history compression level",
        )
        .into());
    }
    let root = root.to_path_buf();
    let mut waiter = CancelOnAbandon(Some(cancellation.clone()));
    let task = tokio::spawn(async move {
        cancellation.check()?;
        let root = root.canonicalize()?;
        let directory = root.join(id.to_string());
        if !std::fs::symlink_metadata(&directory)?.is_dir()
            || !std::fs::symlink_metadata(directory.join("session.db"))?.is_file()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "unsafe canonical history path",
            )
            .into());
        }
        let _admission = if age.is_some() {
            Some(cancellation.admit_tracking(&root).await?)
        } else {
            None
        };
        let lock_root = root.clone();
        let maintenance = tokio::task::spawn_blocking(move || {
            crate::lease::acquire_session_maintenance_guard(&lock_root, id)
        })
        .await
        .map_err(|_| std::io::Error::other("history maintenance acquisition failed"))?
        .map_err(std::io::Error::other)?;
        let cutoff = if let Some((now, minimum_age)) = age {
            let crate::storage_access::StorageAccessObservation::Recorded(record) =
                crate::storage_access::observe_session_access(&root, id)?
            else {
                return Err(std::io::Error::other("unknown history access age").into());
            };
            if now
                .checked_sub(record.observed_at_ms)
                .is_none_or(|elapsed| elapsed < minimum_age)
            {
                return Ok(HistoryCompressionPage::default());
            }
            Some(
                now.checked_sub(minimum_age)
                    .ok_or_else(|| std::io::Error::other("invalid history age"))?,
            )
        } else {
            None
        };
        let db = SessionDb::open_existing_turso_in_root(id, &root).await?;
        let result = db
            .compress_history_payload_page_through(
                start_sequence,
                level,
                cutoff,
                through_sequence,
                || cancellation.check(),
            )
            .await;
        let closed = db.database().close().await;
        drop(db);
        drop(maintenance);
        closed?;
        result
    });
    let result = task.await;
    waiter.0 = None;
    result.map_err(|_| std::io::Error::other("history compression task failed"))?
}
