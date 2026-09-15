//! Transactional, disposable usage snapshots stored through Switchy/Turso.

use bcode_session_models::{SessionId, SessionUsageGeneration, SessionUsagePage};
use bcode_usage_models::{UsageQuery, UsageReport, UsageRequestRow};
use std::path::{Path, PathBuf};
use switchy::database::{
    Database, DatabaseValue, Row,
    query::{FilterableQuery, SortDirection},
};

const SCHEMA: i64 = 1;

/// State-location-local reporting storage. Opening a dashboard never initializes or repairs it.
#[derive(Debug)]
pub struct UsageIndex {
    path: PathBuf,
}

fn error(_: impl std::fmt::Display) -> String {
    "usage index unavailable or incompatible; explicit collection/maintenance required".into()
}
fn text(row: &Row, name: &str) -> Result<String, String> {
    row.get(name)
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| error(name))
}
fn integer(row: &Row, name: &str) -> Result<i64, String> {
    row.get(name)
        .and_then(|value| value.as_i64())
        .ok_or_else(|| error(name))
}

/// Bounded decision for a source whose current accounting generation was verified by its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectionProgress {
    /// No published or staged collection exists.
    Missing,
    /// Published contributions already represent the verified generation.
    Current,
    /// Continue durable staging at this exclusive source key.
    Continue(String),
    /// Recorded source generation differs; explicit restart is required.
    Changed,
}

impl UsageIndex {
    /// Inspect one source checkpoint without creating storage or reading request contributions.
    /// # Errors
    /// Rejects corrupt checkpoints or incompatible storage; a missing index is reported separately.
    pub async fn collection_progress(
        &self,
        session_id: SessionId,
        generation: SessionUsageGeneration,
    ) -> Result<CollectionProgress, String> {
        match std::fs::symlink_metadata(&self.path) {
            Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CollectionProgress::Missing);
            }
            Err(cause) => return Err(error(cause)),
            Ok(_) => {}
        }
        let (_lock, db) = self.open(false).await?;
        let Some(row) = db
            .select("usage_sessions")
            .where_eq("session_id", session_id.to_string())
            .execute_first(&*db)
            .await
            .map_err(error)?
        else {
            return Ok(CollectionProgress::Missing);
        };
        let recorded: SessionUsageGeneration =
            serde_json::from_str(&text(&row, "generation")?).map_err(error)?;
        if recorded != generation {
            return Ok(CollectionProgress::Changed);
        }
        match integer(&row, "collecting")? {
            0 => Ok(CollectionProgress::Current),
            1 => {
                let next = text(&row, "next_key")?;
                if next.is_empty() || next.len() > 4096 {
                    return Err(error("invalid collection cursor"));
                }
                Ok(CollectionProgress::Continue(next))
            }
            _ => Err(error("invalid collection status")),
        }
    }
    /// Resolve reporting storage beneath an already resolved state location.
    #[must_use]
    pub fn in_state_root(root: &Path) -> Self {
        Self {
            path: root.join("usage.db"),
        }
    }

    async fn open(&self, initialize: bool) -> Result<(std::fs::File, Box<dyn Database>), String> {
        // Reject symlinks before access, including the supplied state directory.
        let parent = self.path.parent().ok_or_else(|| error("parent"))?;
        let canonical_parent = parent.canonicalize().map_err(error)?;
        if canonical_parent != parent {
            return Err(error("noncanonical state root"));
        }
        match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
                return Err(error("unsafe index path"));
            }
            Ok(_) => {}
            Err(cause) if cause.kind() == std::io::ErrorKind::NotFound && initialize => {}
            Err(cause) => return Err(error(cause)),
        }
        // Fence every access across daemon artifact versions before opening Turso.
        let lock_path = self.path.with_extension("lock");
        if let Ok(metadata) = std::fs::symlink_metadata(&lock_path)
            && (!metadata.is_file() || metadata.file_type().is_symlink())
        {
            return Err(error("unsafe lock"));
        }
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(initialize)
            .truncate(false)
            .open(lock_path)
            .map_err(error)?;
        lock.try_lock().map_err(error)?;
        let db = switchy::database_connection::builder()
            .turso()
            .with_path(&self.path)
            .with_busy_timeout(std::time::Duration::from_secs(5))
            .with_multiprocess_wal(false)
            .build()
            .await
            .map_err(error)?;
        let tx = db.begin_transaction().await.map_err(error)?;
        let tables = tx.query_raw("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' LIMIT 8").await.map_err(error)?;
        if tables.is_empty() && initialize {
            for statement in [
                "CREATE TABLE usage_meta (id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL, revision INTEGER NOT NULL, ordinal INTEGER NOT NULL)",
                "INSERT INTO usage_meta VALUES (1,1,0,0)",
                "CREATE TABLE usage_sessions (session_id TEXT PRIMARY KEY, generation TEXT NOT NULL, next_key TEXT, collecting INTEGER NOT NULL)",
                "CREATE TABLE usage_rows (ordinal INTEGER PRIMARY KEY, session_id TEXT NOT NULL, request_key TEXT NOT NULL, entry_json TEXT NOT NULL, staged INTEGER NOT NULL, UNIQUE(session_id,request_key,staged))",
                "CREATE INDEX usage_session_rows ON usage_rows(session_id,staged,ordinal)",
                "CREATE INDEX usage_published_rows ON usage_rows(staged,ordinal)",
            ] {
                tx.query_raw(statement).await.map_err(error)?;
            }
        }
        let meta = tx
            .select("usage_meta")
            .where_eq("id", 1)
            .execute_first(&*tx)
            .await
            .map_err(error)?
            .ok_or_else(|| error("missing schema"))?;
        if integer(&meta, "version")? != SCHEMA {
            return Err(error("future schema"));
        }
        tx.commit().await.map_err(error)?;
        Ok((lock, db))
    }

    /// Stage one bounded complete-range source page and atomically publish at end-of-source.
    /// Incomplete staging survives restart. Concurrent conflicting collectors fail closed.
    /// # Errors
    /// Rejects invalid pages, changed generations, duplicate contributions, or incompatible storage.
    pub async fn collect(
        &self,
        session_id: SessionId,
        after: Option<&str>,
        page: SessionUsagePage,
    ) -> Result<bool, String> {
        validate_page(after, &page)?;
        let (_lock, db) = self.open(true).await?;
        let tx = db.begin_transaction().await.map_err(error)?;
        let id = session_id.to_string();
        let generation = serde_json::to_string(&page.generation).map_err(error)?;
        let current = tx
            .select("usage_sessions")
            .where_eq("session_id", id.clone())
            .execute_first(&*tx)
            .await
            .map_err(error)?;
        if let Some(after) = after {
            let current = current.ok_or_else(|| error("collection missing"))?;
            if integer(&current, "collecting")? != 1
                || text(&current, "generation")? != generation
                || text(&current, "next_key")? != after
            {
                return Err(error("collection changed"));
            }
        } else {
            validate_restart(current.as_ref(), &generation)?;
            tx.delete("usage_rows")
                .where_eq("session_id", id.clone())
                .where_eq("staged", 1)
                .execute(&*tx)
                .await
                .map_err(error)?;
        }
        let meta = tx
            .select("usage_meta")
            .where_eq("id", 1)
            .execute_first(&*tx)
            .await
            .map_err(error)?
            .ok_or_else(|| error("meta"))?;
        let mut ordinal = integer(&meta, "ordinal")?;
        for entry in &page.entries {
            ordinal = ordinal
                .checked_add(1)
                .ok_or_else(|| error("ordinal overflow"))?;
            tx.insert("usage_rows")
                .value("ordinal", ordinal)
                .value("session_id", id.clone())
                .value("request_key", entry.key.clone())
                .value("entry_json", serde_json::to_string(entry).map_err(error)?)
                .value("staged", 1)
                .execute(&*tx)
                .await
                .map_err(error)?;
        }
        let complete = page.next_after.is_none();
        tx.upsert("usage_sessions")
            .unique(&["session_id"])
            .value("session_id", id.clone())
            .value("generation", generation)
            .value(
                "next_key",
                page.next_after
                    .map_or(DatabaseValue::Null, DatabaseValue::String),
            )
            .value("collecting", i32::from(!complete))
            .execute(&*tx)
            .await
            .map_err(error)?;
        let mut revision = integer(&meta, "revision")?;
        if complete {
            tx.delete("usage_rows")
                .where_eq("session_id", id.clone())
                .where_eq("staged", 0)
                .execute(&*tx)
                .await
                .map_err(error)?;
            tx.update("usage_rows")
                .value("staged", 0)
                .where_eq("session_id", id)
                .where_eq("staged", 1)
                .execute(&*tx)
                .await
                .map_err(error)?;
            revision = revision
                .checked_add(1)
                .ok_or_else(|| error("revision overflow"))?;
        }
        tx.update("usage_meta")
            .value("ordinal", ordinal)
            .value("revision", revision)
            .where_eq("id", 1)
            .execute(&*tx)
            .await
            .map_err(error)?;
        tx.commit().await.map_err(error)?;
        Ok(complete)
    }

    /// Read a bounded generation-fenced report page without initializing missing storage.
    /// # Errors
    /// Rejects invalid queries, incompatible storage, corrupt contributions, and changed revisions.
    pub async fn query(&self, query: &UsageQuery) -> Result<UsageReport, String> {
        query.validate()?;
        let (_lock, db) = self.open(false).await?;
        let tx = db.begin_transaction().await.map_err(error)?;
        let meta = tx
            .select("usage_meta")
            .where_eq("id", 1)
            .execute_first(&*tx)
            .await
            .map_err(error)?
            .ok_or_else(|| error("meta"))?;
        let revision = u64::try_from(integer(&meta, "revision")?).map_err(error)?;
        if query.revision.is_some_and(|expected| expected != revision) {
            return Err(error("revision changed"));
        }
        let after = i64::try_from(query.after.unwrap_or_default()).map_err(error)?;
        let rows = tx
            .select("usage_rows")
            .where_eq("staged", 0)
            .where_gt("ordinal", after)
            .sort("ordinal", SortDirection::Asc)
            .limit(query.limit as usize)
            .execute(&*tx)
            .await
            .map_err(error)?;
        let mut requests = Vec::with_capacity(rows.len());
        for row in rows {
            requests.push(UsageRequestRow {
                ordinal: u64::try_from(integer(&row, "ordinal")?).map_err(error)?,
                session_id: text(&row, "session_id")?.parse().map_err(error)?,
                entry: serde_json::from_str(&text(&row, "entry_json")?).map_err(error)?,
            });
        }
        let next = if requests.len() == query.limit as usize {
            requests.last().map(|row| row.ordinal)
        } else {
            None
        };
        let mut query = query.clone();
        query.revision = Some(revision);
        let report = crate::report_page(&query, requests, next,
            "Page subtotal of durable explicitly collected snapshots. Source freshness unverified; recollect before relying on totals.".into())?;
        tx.commit().await.map_err(error)?;
        Ok(report)
    }

    /// Remove unverifiable source contributions and invalidate report cursors atomically.
    /// # Errors
    /// Returns an error for unavailable storage or revision overflow.
    pub async fn invalidate(&self, session_id: SessionId) -> Result<(), String> {
        let (_lock, db) = self.open(false).await?;
        let tx = db.begin_transaction().await.map_err(error)?;
        tx.delete("usage_rows")
            .where_eq("session_id", session_id.to_string())
            .execute(&*tx)
            .await
            .map_err(error)?;
        tx.delete("usage_sessions")
            .where_eq("session_id", session_id.to_string())
            .execute(&*tx)
            .await
            .map_err(error)?;
        let current = tx
            .select("usage_meta")
            .where_eq("id", 1)
            .execute_first(&*tx)
            .await
            .map_err(error)?
            .ok_or_else(|| error("meta"))?;
        let revision = integer(&current, "revision")?
            .checked_add(1)
            .ok_or_else(|| error("revision overflow"))?;
        tx.update("usage_meta")
            .value("revision", revision)
            .where_eq("id", 1)
            .execute(&*tx)
            .await
            .map_err(error)?;
        tx.commit().await.map_err(error)?;
        Ok(())
    }
}

fn validate_restart(current: Option<&Row>, generation: &str) -> Result<(), String> {
    if let Some(current) = current {
        match integer(current, "collecting")? {
            1 if text(current, "generation")? == generation => {
                return Err(error("collection already active; continue its cursor"));
            }
            0 | 1 => {}
            _ => return Err(error("invalid collection status")),
        }
    }
    Ok(())
}

fn validate_page(after: Option<&str>, page: &SessionUsagePage) -> Result<(), String> {
    if page.scanned > 256 || page.entries.len() != page.scanned as usize {
        return Err(error("incomplete range"));
    }
    let mut previous = after.unwrap_or_default();
    for entry in &page.entries {
        if entry.key.as_str() <= previous {
            return Err(error("unordered page"));
        }
        entry.usage.validate()?;
        previous = &entry.key;
    }
    if page
        .next_after
        .as_deref()
        .is_some_and(|next| next <= after.unwrap_or_default() || next != previous)
    {
        return Err(error("continuation"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        SessionCostRange, SessionTokenUsage, SessionUsageEntry, SessionUsageGeneration,
    };
    use bcode_usage_models::USAGE_VERSION;
    fn query() -> UsageQuery {
        UsageQuery {
            version: USAGE_VERSION,
            range: SessionCostRange {
                from_timestamp_ms: 0,
                to_timestamp_ms: 100,
            },
            models: std::collections::BTreeSet::new(),
            providers: std::collections::BTreeSet::new(),
            session_id: None,
            bucket_ms: 100,
            after: None,
            revision: None,
            limit: 256,
        }
    }
    fn page(key: &str, next: Option<&str>) -> SessionUsagePage {
        SessionUsagePage {
            generation: SessionUsageGeneration {
                through_sequence: Some(2),
                cost_revision: 0,
            },
            scanned: 1,
            entries: vec![SessionUsageEntry {
                key: key.into(),
                first_observed_at_ms: 1,
                usage: SessionTokenUsage::default(),
            }],
            next_after: next.map(str::to_owned),
        }
    }
    #[tokio::test]
    async fn restart_preserves_staging_and_atomic_publication() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let index = UsageIndex::in_state_root(&root);
        assert!(index.query(&query()).await.is_err());
        assert!(!root.join("usage.db").exists());
        let session = SessionId::new();
        assert!(
            !index
                .collect(session, None, page("a", Some("a")))
                .await
                .unwrap()
        );
        assert_eq!(index.query(&query()).await.unwrap().totals.requests, 0);
        drop(index);
        let index = UsageIndex::in_state_root(&root);
        assert!(
            index
                .collect(session, Some("a"), page("b", None))
                .await
                .unwrap()
        );
        let report = index.query(&query()).await.unwrap();
        assert_eq!(report.totals.requests, 2);
        assert!(index.collect(session, None, page("a", None)).await.unwrap());
        assert_eq!(index.query(&query()).await.unwrap().totals.requests, 1);
        let mut stale = query();
        stale.revision = Some(report.revision);
        assert!(index.query(&stale).await.is_err());
        index.invalidate(session).await.unwrap();
        assert_eq!(index.query(&query()).await.unwrap().totals.requests, 0);
    }
    #[tokio::test]
    async fn progress_distinguishes_missing_staged_current_and_repriced() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let index = UsageIndex::in_state_root(&root);
        let session = SessionId::new();
        let generation = page("a", None).generation;
        assert_eq!(
            index
                .collection_progress(session, generation)
                .await
                .unwrap(),
            CollectionProgress::Missing
        );
        assert!(!root.join("usage.db").exists());
        index
            .collect(session, None, page("a", Some("a")))
            .await
            .unwrap();
        assert_eq!(
            index
                .collection_progress(session, generation)
                .await
                .unwrap(),
            CollectionProgress::Continue("a".into())
        );
        index
            .collect(session, Some("a"), page("b", None))
            .await
            .unwrap();
        assert_eq!(
            index
                .collection_progress(session, generation)
                .await
                .unwrap(),
            CollectionProgress::Current
        );
        let mut repriced = generation;
        repriced.cost_revision += 1;
        assert_eq!(
            index.collection_progress(session, repriced).await.unwrap(),
            CollectionProgress::Changed
        );
    }

    #[tokio::test]
    async fn changed_source_restarts_staging_and_rejects_old_continuation() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let index = UsageIndex::in_state_root(&root);
        let session = SessionId::new();
        index
            .collect(session, None, page("a", Some("a")))
            .await
            .unwrap();
        let mut changed = page("a", Some("a"));
        changed.generation.cost_revision = 1;
        index.collect(session, None, changed).await.unwrap();
        assert!(
            index
                .collect(session, Some("a"), page("b", None))
                .await
                .is_err()
        );
        let mut final_page = page("b", None);
        final_page.generation.cost_revision = 1;
        assert!(index.collect(session, Some("a"), final_page).await.unwrap());
        assert_eq!(index.query(&query()).await.unwrap().totals.requests, 2);
    }

    #[tokio::test]
    async fn future_schema_is_preserved_and_rejected() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let index = UsageIndex::in_state_root(&root);
        index
            .collect(SessionId::new(), None, page("a", None))
            .await
            .unwrap();
        let (lock, db) = index.open(false).await.unwrap();
        db.query_raw("UPDATE usage_meta SET version=999 WHERE id=1")
            .await
            .unwrap();
        drop(db);
        drop(lock);
        assert!(index.query(&query()).await.is_err());
        assert!(
            index
                .collect(SessionId::new(), None, page("b", None))
                .await
                .is_err()
        );
    }
}
