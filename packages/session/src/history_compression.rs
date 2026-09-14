//! Bounded current-format canonical history recompression under offline ownership.

use crate::db::{SessionDb, SessionDbError};
use bcode_session_models::SessionId;
use std::path::Path;

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
    if !(1..=22).contains(&level) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid history compression level",
        )
        .into());
    }
    let root = root.to_path_buf();
    tokio::spawn(async move {
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
            Some(crate::artifact_storage::acquire_tracking_admission(&root).await?)
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
            .compress_history_payload_page_before(start_sequence, level, cutoff)
            .await;
        let closed = db.database().close().await;
        drop(db);
        drop(maintenance);
        closed?;
        result
    })
    .await
    .map_err(|_| std::io::Error::other("history compression task failed"))?
}
