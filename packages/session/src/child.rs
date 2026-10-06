//! Fresh-child publication and bounded non-owning relationship observation.

use crate::{SessionError, SessionManager, SessionState, db, lease, normalize_working_directory};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionChildCreateRequest, SessionChildPage,
    SessionChildQuery, SessionEvent, SessionEventKind, SessionId, SessionSummary,
};
use std::fs::{self, File};
use std::ops::Bound::{Excluded, Unbounded};

fn invalid(message: impl Into<String>) -> SessionError {
    SessionError::EventSerialization(message.into())
}

impl SessionManager {
    /// Observe exact current metadata without acquiring a session runtime lease or loading history.
    ///
    /// # Errors
    /// Returns `NotFound` for absent storage, and existing storage/projection errors for damaged,
    /// stale, incompatible, or unverifiable state. This operation never repairs or migrates.
    pub async fn session_summary_non_owning(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSummary, SessionError> {
        let Some(store) = &self.store else {
            return self
                .inner
                .lock()
                .await
                .sessions
                .get(&session_id)
                .map(|handle| handle.snapshot().summary)
                .ok_or(SessionError::NotFound(session_id));
        };
        let root = store.root_path();
        validate_session_path(&root, session_id)?;
        if !db::session_db_path(&root, session_id).exists() {
            return Err(SessionError::NotFound(session_id));
        }
        let database = db::SessionDb::open_existing_turso_in_root(session_id, &root).await?;
        let result: Result<SessionSummary, SessionError> = async {
            database.validate_write_readiness().await?;
            let summary = self
                .load_db_session_state(session_id, &database)
                .await?
                .summary();
            let prefix = database.events_range(0, 1, 2).await?;
            let relationship = prefix.iter().find_map(|event| match &event.kind {
                SessionEventKind::ChildSessionCreated { request } => Some(request.parent_id),
                _ => None,
            });
            if relationship != summary.parent_id {
                return Err(invalid(
                    "parent projection disagrees with canonical creation prefix",
                ));
            }
            if relationship.is_some() {
                validate_child_prefix(&prefix, session_id)?;
            }
            Ok(summary)
        }
        .await;
        let close = database.close().await;
        let summary = result?;
        close?;
        Ok(summary)
    }

    /// Read a bounded indexed child window without loading the catalog or owning sessions.
    ///
    /// At most `limit` candidate projections are inspected. Empty pages can have a cursor.
    /// Persistent discovery has no cross-store completeness checkpoint: its coverage is explicitly
    /// incomplete even when the loaded candidate catalog is exhausted. A cursor is pagination,
    /// not degradation. Consumers must not interpret exhausted incomplete coverage as no children.
    ///
    /// # Errors
    /// Returns an error for an invalid work budget; individual observation failures are page issues.
    pub async fn query_child_sessions(
        &self,
        query: SessionChildQuery,
    ) -> Result<SessionChildPage, SessionError> {
        if !(1..=128).contains(&query.limit) {
            return Err(invalid("child query limit must be between 1 and 128"));
        }
        self.session_summary_non_owning(query.parent_id).await?;
        let mut index_issue = None;
        let candidates = if let Some(store) = &self.store {
            let root = store.root_path();
            if root.join("catalog.db").exists() {
                if let Ok(catalog) = db::GlobalSessionDb::open_existing_turso_in_root(&root).await {
                    let result = catalog
                        .child_session_ids_page(query.parent_id, query.after, query.limit + 1)
                        .await;
                    let close = catalog.close().await;
                    if let (Ok(ids), Ok(())) = (result, close) {
                        ids
                    } else {
                        index_issue = Some("child catalog is damaged, stale, or incompatible; explicit maintenance required".to_owned());
                        Vec::new()
                    }
                } else {
                    index_issue = Some(
                        "child catalog cannot be verified; explicit maintenance required"
                            .to_owned(),
                    );
                    Vec::new()
                }
            } else {
                index_issue = Some(
                    "child catalog is missing; explicit discovery/maintenance required".to_owned(),
                );
                Vec::new()
            }
        } else {
            let inner = self.inner.lock().await;
            inner
                .sessions
                .range((query.after.map_or(Unbounded, Excluded), Unbounded))
                .take(query.limit + 1)
                .map(|(id, _)| *id)
                .collect::<Vec<_>>()
        };
        let has_more = candidates.len() > query.limit;
        let mut page = SessionChildPage {
            children: Vec::new(),
            next_after: None,
            complete: self.store.is_none() && self.catalog_loaded(),
            issues: Vec::new(),
        };
        if let Some(issue) = index_issue {
            page.issues.push(issue);
        }
        if self.store.is_some() {
            page.issues.push("persistent catalog has no verified cross-session completeness checkpoint; missing or stale entries may be omitted".into());
        }
        if !self.catalog_loaded() {
            page.complete = false;
            page.issues.push(format!(
                "catalog discovery is not complete: {:?}",
                self.catalog_status()
            ));
        }
        for id in candidates.into_iter().take(query.limit) {
            if has_more {
                page.next_after = Some(id);
            }
            match self.session_summary_non_owning(id).await {
                Ok(summary) if summary.parent_id == Some(query.parent_id) => {
                    page.children.push(summary);
                }
                Ok(_) if self.store.is_some() => {
                    page.complete = false;
                    page.issues.push(format!(
                        "candidate {id} relationship disagrees with canonical projection"
                    ));
                }
                Ok(_) => {}
                Err(_) => {
                    page.complete = false;
                    page.issues.push(format!(
                        "candidate {id} metadata is unavailable or requires maintenance"
                    ));
                }
            }
        }
        Ok(page)
    }

    /// Create or reconcile one fresh ordinary child with immutable parent and original request.
    ///
    /// The caller retains `child_id`. No parent events, context, control, or lifecycle effects are
    /// copied. Publication is a single canonical directory rename under exclusive child-ID
    /// maintenance ownership. Retry reconciliation reads only the two-event creation prefix.
    ///
    /// # Errors
    /// Returns an error for self-parenting, conflicting identity reuse, unavailable parent,
    /// unverifiable ownership, or damaged/incompatible storage. A failed derived-catalog update
    /// cannot change the already published canonical relationship.
    #[allow(clippy::too_many_lines)]
    pub async fn create_child_session(
        &self,
        mut request: SessionChildCreateRequest,
    ) -> Result<SessionSummary, SessionError> {
        request.working_directory = normalize_working_directory(&request.working_directory);
        if request.child_id == request.parent_id {
            return Err(invalid("a child session cannot be its own parent"));
        }
        let Some(store) = &self.store else {
            let lock = {
                let mut locks = self.shared_execution_locks.lock().await;
                std::sync::Arc::clone(
                    locks
                        .entry(request.child_id)
                        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(()))),
                )
            };
            let _guard = lock.lock().await;
            let existing = self
                .inner
                .lock()
                .await
                .sessions
                .get(&request.child_id)
                .cloned();
            if let Some(handle) = existing {
                let events = handle.events_range(0, 1, 2).await?;
                verify_request(&events, &request)?;
                return Ok(handle.snapshot().summary);
            }
            self.session_summary_non_owning(request.parent_id).await?;
            return self
                .create_session_record(
                    request.name.clone(),
                    request.working_directory.clone(),
                    None,
                    Some(request),
                )
                .await;
        };
        let root = store.root_path();
        validate_session_path(&root, request.child_id)?;
        validate_session_path(&root, request.parent_id)?;
        // This guard verifies all live owners and serializes both publication and crash recovery.
        let ownership = lease::acquire_session_maintenance_guard(&root, request.child_id)?;
        let canonical = db::session_dir_path(&root, request.child_id);
        if canonical.exists() {
            let database =
                db::SessionDb::open_existing_turso_in_root(request.child_id, &root).await?;
            let result: Result<SessionSummary, SessionError> = async {
                database.validate_write_readiness().await?;
                verify_request(&database.events_range(0, 1, 2).await?, &request)?;
                Ok(self
                    .load_db_session_state(request.child_id, &database)
                    .await?
                    .summary())
            }
            .await;
            let close = database.close().await;
            let summary = result?;
            close?;
            drop(ownership);
            self.register_child_summary(summary.clone()).await;
            return Ok(summary);
        }
        // Only exact local canonical storage establishes the parent; catalog metadata is not authority.
        self.session_summary_non_owning(request.parent_id).await?;
        let staging_parent = root.join(".child-staging");
        if staging_parent.exists() && !fs::symlink_metadata(&staging_parent)?.is_dir() {
            return Err(invalid("child staging root is not a regular directory"));
        }
        let staging = staging_parent.join(request.child_id.to_string());
        if staging.exists() {
            if fs::symlink_metadata(&staging)?.file_type().is_symlink() {
                return Err(invalid("child staging path is a symlink"));
            }
            let retained: SessionChildCreateRequest = serde_json::from_slice(&fs::read(staging.join("request.json"))?)
                .map_err(|_| invalid("interrupted child staging request is unverifiable; explicit maintenance required"))?;
            if retained != request {
                return Err(invalid(
                    "child identity conflicts with interrupted creation request",
                ));
            }
            // The staging session was never canonical. Exclusive verified ownership permits restart.
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;
        let request_path = staging.join("request.json");
        fs::write(
            request_path.with_extension("tmp"),
            serde_json::to_vec(&request).map_err(|error| invalid(error.to_string()))?,
        )?;
        File::open(request_path.with_extension("tmp"))?.sync_all()?;
        fs::rename(request_path.with_extension("tmp"), &request_path)?;
        File::open(&request_path)?.sync_all()?;
        File::open(&staging)?.sync_all()?;
        let database = db::SessionDb::initialize_turso_in_root(request.child_id, &staging).await?;
        let now = self.next_activity_timestamp_ms();
        let created = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
            timestamp_ms: now,
            session_id: request.child_id,
            provenance: None,
            kind: SessionEventKind::SessionCreated {
                name: request.name.clone(),
                working_directory: request.working_directory.clone(),
            },
        };
        let relationship = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: now,
            session_id: request.child_id,
            provenance: None,
            kind: SessionEventKind::ChildSessionCreated {
                request: Box::new(request.clone()),
            },
        };
        let result: Result<SessionSummary, SessionError> = async {
            database
                .append_event_batch(&[(created, Some(now)), (relationship, Some(now))])
                .await?;
            database.validate_write_readiness().await?;
            Ok(self
                .load_db_session_state(request.child_id, &database)
                .await?
                .summary())
        }
        .await;
        let close = database.close().await;
        let summary = result?;
        close?;
        crate::derivation::write_staged_manifest(&staging, &summary)?;
        fs::rename(db::session_dir_path(&staging, request.child_id), &canonical)?;
        File::open(&root)?.sync_all()?;
        // Preserve successful publication on cleanup/catalog failures; a retry verifies canonical facts.
        drop(ownership);
        self.register_child_summary(summary.clone()).await;
        Ok(summary)
    }

    async fn register_child_summary(&self, summary: SessionSummary) {
        let handle = crate::actor::SessionHandle::new(
            SessionState::from_catalog_summary(summary.clone()),
            self.store.clone(),
            None,
        );
        self.inner
            .lock()
            .await
            .sessions
            .entry(summary.id)
            .or_insert(handle);
        if let Some(store) = &self.store {
            store.schedule_catalog_summary(summary).await;
            store.flush_catalog_updates().await;
        }
    }
}

fn validate_session_path(root: &std::path::Path, id: SessionId) -> Result<(), SessionError> {
    let directory = db::session_dir_path(root, id);
    for path in [directory, db::session_db_path(root, id)] {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid("session storage path is a symlink"));
            }
            Ok(_) => {
                if !path.canonicalize()?.starts_with(root.canonicalize()?) {
                    return Err(invalid("session storage escapes verified state location"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn validate_child_prefix(events: &[SessionEvent], id: SessionId) -> Result<(), SessionError> {
    let Some(created) = events
        .iter()
        .find(|event| event.sequence == 0 && event.session_id == id)
    else {
        return Err(invalid("child creation prefix is missing"));
    };
    let Some(relationship) = events
        .iter()
        .find(|event| event.sequence == 1 && event.session_id == id)
    else {
        return Err(invalid("child relationship prefix is missing"));
    };
    match (&created.kind, &relationship.kind) {
        (
            SessionEventKind::SessionCreated {
                name,
                working_directory,
            },
            SessionEventKind::ChildSessionCreated { request },
        ) if request.child_id == id
            && request.parent_id != id
            && &request.name == name
            && &request.working_directory == working_directory =>
        {
            Ok(())
        }
        _ => Err(invalid("child creation prefix is inconsistent")),
    }
}

fn verify_request(
    events: &[SessionEvent],
    request: &SessionChildCreateRequest,
) -> Result<(), SessionError> {
    if events.len() > 1 {
        validate_child_prefix(events, request.child_id)?;
    }
    if events.iter().any(|event| event.sequence == 1 && event.session_id == request.child_id
        && matches!(&event.kind, SessionEventKind::ChildSessionCreated { request: retained } if retained.as_ref() == request)) {
        Ok(())
    } else {
        Err(invalid("child identity is already used by a different creation request"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(parent: SessionId) -> SessionChildCreateRequest {
        SessionChildCreateRequest {
            child_id: SessionId::new(),
            parent_id: parent,
            name: Some("child".into()),
            working_directory: std::env::current_dir().unwrap(),
        }
    }

    #[tokio::test]
    async fn fresh_children_are_recursive_idempotent_and_independent() {
        let manager = SessionManager::default();
        let parent = manager
            .create_session(Some("parent".into()), std::env::current_dir().unwrap())
            .await
            .unwrap();
        let child_request = request(parent.id);
        let child = manager
            .create_child_session(child_request.clone())
            .await
            .unwrap();
        assert_eq!(child.parent_id, Some(parent.id));
        assert_eq!(child.execution, None);
        assert_eq!(
            manager
                .create_child_session(child_request.clone())
                .await
                .unwrap()
                .id,
            child.id
        );
        manager
            .rename_session(child.id, Some("renamed".into()))
            .await
            .unwrap();
        assert_eq!(
            manager
                .create_child_session(child_request.clone())
                .await
                .unwrap()
                .name
                .as_deref(),
            Some("renamed")
        );
        let mut conflicting = child_request;
        conflicting.name = Some("conflict".into());
        assert!(manager.create_child_session(conflicting).await.is_err());
        let grandchild = manager
            .create_child_session(request(child.id))
            .await
            .unwrap();
        assert_eq!(grandchild.parent_id, Some(child.id));
        let page = manager
            .query_child_sessions(SessionChildQuery {
                parent_id: parent.id,
                after: None,
                limit: 128,
            })
            .await
            .unwrap();
        assert!(page.complete);
        assert_eq!(page.children.len(), 1);
        assert_eq!(page.children[0].id, child.id);
        assert!(
            manager
                .query_child_sessions(SessionChildQuery {
                    parent_id: SessionId::new(),
                    after: None,
                    limit: 1
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn durable_retry_reconciles_after_manager_restart_and_empty_index_is_not_authority() {
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::persistent(root.path()).unwrap();
        let parent = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        let child_request = request(parent.id);
        let child = manager
            .create_child_session(child_request.clone())
            .await
            .unwrap();
        assert_eq!(
            db::session_dir_path(root.path(), child.id),
            root.path().join(child.id.to_string())
        );
        manager.flush_catalog_updates().await;
        let page = manager
            .query_child_sessions(SessionChildQuery {
                parent_id: parent.id,
                after: None,
                limit: 1,
            })
            .await
            .unwrap();
        assert!(!page.complete);
        assert_eq!(page.children[0].id, child.id);
        drop(manager);
        let restored = SessionManager::persistent(root.path()).unwrap();
        assert_eq!(
            restored
                .create_child_session(child_request)
                .await
                .unwrap()
                .id,
            child.id
        );
        assert_eq!(
            restored
                .session_summary_non_owning(child.id)
                .await
                .unwrap()
                .parent_id,
            Some(parent.id)
        );
    }

    #[tokio::test]
    async fn child_non_owning_observation_preserves_ownership_and_detects_missing_storage() {
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::persistent(root.path()).unwrap();
        let parent = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        let child = manager
            .create_child_session(request(parent.id))
            .await
            .unwrap();
        let before = manager
            .inner
            .lock()
            .await
            .sessions
            .get(&child.id)
            .unwrap()
            .snapshot()
            .load_status;
        let reader = SessionManager::persistent(root.path()).unwrap();
        assert_eq!(
            reader
                .session_summary_non_owning(child.id)
                .await
                .unwrap()
                .parent_id,
            Some(parent.id)
        );
        let after = manager
            .inner
            .lock()
            .await
            .sessions
            .get(&child.id)
            .unwrap()
            .snapshot()
            .load_status;
        assert_eq!(before, after);
        let guard = lease::acquire_session_maintenance_guard(root.path(), child.id).unwrap();
        drop(guard);
        assert!(matches!(
            reader.session_summary_non_owning(SessionId::new()).await,
            Err(SessionError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn interrupted_staging_is_owned_and_request_conflicts_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::persistent(root.path()).unwrap();
        let parent = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        let child_request = request(parent.id);
        let stage = root
            .path()
            .join(".child-staging")
            .join(child_request.child_id.to_string());
        fs::create_dir_all(&stage).unwrap();
        fs::write(
            stage.join("request.json"),
            serde_json::to_vec(&child_request).unwrap(),
        )
        .unwrap();
        let mut conflict = child_request.clone();
        conflict.name = None;
        assert!(manager.create_child_session(conflict).await.is_err());
        let child = manager.create_child_session(child_request).await.unwrap();
        assert_eq!(child.parent_id, Some(parent.id));
    }
}
