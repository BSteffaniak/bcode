//! Parent-local relationship projection, atomically checkpointed with canonical events.

use crate::db::{SessionDb, SessionDbError, SessionDbResult};
use crate::db_row::{required_i64, required_string};
use bcode_session_models::{SessionEvent, SessionEventKind, SessionId};
use switchy::database::{
    Database,
    query::{FilterableQuery, SortDirection},
};

pub async fn project(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    match &event.kind {
        SessionEventKind::SessionCreated { .. } => {
            db.upsert("session_children_checkpoint")
                .value("singleton", 1_i64)
                .value(
                    "event_seq",
                    i64::try_from(event.sequence).unwrap_or(i64::MAX),
                )
                .value("coverage", i64::from(event.schema_version >= 50))
                .execute(db)
                .await?;
        }
        SessionEventKind::SessionChildLinked { child_id } => {
            if *child_id == event.session_id {
                return Err(invalid());
            }
            if db
                .select("session_children")
                .columns(&["child_id"])
                .where_eq("child_id", child_id.to_string())
                .execute_first(db)
                .await?
                .is_none()
            {
                db.insert("session_children")
                    .value("child_id", child_id.to_string())
                    .value("published", 0_i64)
                    .execute(db)
                    .await?;
            }
        }
        SessionEventKind::SessionChildPublished { child_id, .. } => {
            if db
                .select("session_children")
                .columns(&["child_id"])
                .where_eq("child_id", child_id.to_string())
                .execute_first(db)
                .await?
                .is_none()
            {
                return Err(invalid());
            }
            db.update("session_children")
                .value("published", 1_i64)
                .where_eq("child_id", child_id.to_string())
                .execute(db)
                .await?;
        }
        SessionEventKind::SessionChildrenReconciled { .. } => {
            db.update("session_children_checkpoint")
                .value("coverage", 1_i64)
                .where_eq("singleton", 1_i64)
                .execute(db)
                .await?;
        }
        _ => {}
    }
    db.update("session_children_checkpoint")
        .value(
            "event_seq",
            i64::try_from(event.sequence).unwrap_or(i64::MAX),
        )
        .where_eq("singleton", 1_i64)
        .execute(db)
        .await?;
    Ok(())
}

fn invalid() -> SessionDbError {
    SessionDbError::InvalidRow {
        column: "session_children".into(),
    }
}

impl SessionDb {
    /// Bounded parent-local direct-child index; returns coverage independently of pagination.
    /// # Errors
    /// Fails for absent/stale checkpoints, missing schema, or malformed relationships.
    pub async fn direct_child_page(
        &self,
        after: Option<SessionId>,
        limit: usize,
    ) -> SessionDbResult<(Vec<SessionId>, bool)> {
        let database = self.database();
        let row = database
            .select("session_children_checkpoint")
            .columns(&["event_seq", "coverage"])
            .where_eq("singleton", 1_i64)
            .execute_first(database)
            .await?
            .ok_or_else(invalid)?;
        let tail = self.last_event_sequence().await?.ok_or_else(invalid)?;
        if u64::try_from(required_i64(&row, "event_seq")?).ok() != Some(tail) {
            return Err(invalid());
        }
        let pending = database
            .select("session_children")
            .columns(&["child_id"])
            .where_eq("published", 0_i64)
            .limit(1)
            .execute_first(database)
            .await?
            .is_some();
        let ids = database
            .select("session_children")
            .columns(&["child_id"])
            .where_gt(
                "child_id",
                after.map_or_else(String::new, |id| id.to_string()),
            )
            .sort("child_id", SortDirection::Asc)
            .limit(limit.min(129))
            .execute(database)
            .await?
            .iter()
            .map(|row| {
                required_string(row, "child_id")?
                    .parse()
                    .map_err(|_| invalid())
            })
            .collect::<SessionDbResult<Vec<_>>>()?;
        if self.last_event_sequence().await? != Some(tail) {
            return Err(invalid());
        }
        Ok((ids, required_i64(&row, "coverage")? == 1 && !pending))
    }

    /// Explicit maintenance-only rebuild from bounded canonical pages; caller retains ownership.
    /// # Errors
    /// Fails if canonical history or schema cannot be read/projected.
    pub(super) async fn rebuild_child_projection_for_maintenance(&self) -> SessionDbResult<()> {
        let tail = self.last_event_sequence().await?.ok_or_else(invalid)?;
        let transaction = self.database().begin_transaction().await?;
        let target = &*transaction;
        target.delete("session_children").execute(target).await?;
        target
            .delete("session_children_checkpoint")
            .execute(target)
            .await?;
        let mut start = 0;
        loop {
            let rows = crate::db::read_canonical_payload_page(target, start, 128).await?;
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                let event = crate::persisted::decode_session_event(&row.payload)?;
                project(target, &event).await?;
            }
            start = rows.last().ok_or_else(invalid)?.sequence + 1;
            if start > tail {
                break;
            }
        }
        target
            .update("session_children_checkpoint")
            .value("coverage", 0_i64)
            .where_eq("singleton", 1_i64)
            .execute(target)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Exact bounded relationship receipt lookup for idempotent publication.
    /// # Errors
    /// Fails for absent/stale checkpoints, concurrent canonical changes, unreadable indexes,
    /// or malformed receipts. Never repairs derived state.
    pub async fn child_publication_receipt(
        &self,
        child: SessionId,
    ) -> SessionDbResult<Option<bool>> {
        let database = self.database();
        let tail = self.last_event_sequence().await?.ok_or_else(invalid)?;
        let checkpoint = database
            .select("session_children_checkpoint")
            .columns(&["event_seq", "coverage"])
            .where_eq("singleton", 1_i64)
            .execute_first(database)
            .await?
            .ok_or_else(invalid)?;
        if u64::try_from(required_i64(&checkpoint, "event_seq")?).ok() != Some(tail)
            || !matches!(required_i64(&checkpoint, "coverage")?, 0 | 1)
        {
            return Err(invalid());
        }
        let receipt = database
            .select("session_children")
            .columns(&["published"])
            .where_eq("child_id", child.to_string())
            .execute_first(database)
            .await?
            .map(|row| match required_i64(&row, "published")? {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(invalid()),
            })
            .transpose()?;
        if self.last_event_sequence().await? != Some(tail) {
            return Err(invalid());
        }
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_child_receipt_checkpoint_fails_without_repair_or_append() {
        let root = tempfile::tempdir().unwrap();
        let manager = crate::SessionManager::persistent(root.path()).unwrap();
        let parent = manager
            .create_session(None, root.path().into())
            .await
            .unwrap();
        let child = SessionId::new();
        let handle = manager.session_handle(parent.id).await.unwrap();
        handle
            .append_event(SessionEventKind::SessionChildLinked { child_id: child }, 1)
            .await
            .unwrap();
        handle
            .append_event(
                SessionEventKind::SessionChildPublished {
                    child_id: child,
                    title: None,
                },
                2,
            )
            .await
            .unwrap();
        assert!(handle.release_idle_resources().await.unwrap());
        let db = SessionDb::open_existing_turso_in_root(parent.id, root.path())
            .await
            .unwrap();
        assert_eq!(
            db.child_publication_receipt(child).await.unwrap(),
            Some(true)
        );
        let tail = db.last_event_sequence().await.unwrap();
        let database = db.database();
        database
            .update("session_children_checkpoint")
            .value("event_seq", 0_i64)
            .where_eq("singleton", 1_i64)
            .execute(database)
            .await
            .unwrap();
        for id in [child, SessionId::new()] {
            assert!(db.child_publication_receipt(id).await.is_err());
        }
        assert_eq!(db.last_event_sequence().await.unwrap(), tail);
        let checkpoint = database
            .select("session_children_checkpoint")
            .columns(&["event_seq"])
            .where_eq("singleton", 1_i64)
            .execute_first(database)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(required_i64(&checkpoint, "event_seq").unwrap(), 0);
        let receipt = database
            .select("session_children")
            .columns(&["published"])
            .where_eq("child_id", child.to_string())
            .execute_first(database)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(required_i64(&receipt, "published").unwrap(), 1);
        db.close().await.unwrap();
    }
}
