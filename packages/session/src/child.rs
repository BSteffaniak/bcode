//! Fresh-child publication and bounded non-owning relationship observation.

use crate::child_creation_observation::{ChildCreationStage as Stage, timer};
use crate::{SessionError, SessionManager, SessionState, db, lease, normalize_working_directory};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionChildCreateRequest, SessionChildPage,
    SessionChildQuery, SessionEvent, SessionEventKind, SessionId, SessionSummary,
};
use std::fs::{self, File};
use std::ops::Bound::{Excluded, Included};

fn verify_summary_prefix(
    summary: &SessionSummary,
    prefix: &[SessionEvent],
    session_id: SessionId,
) -> Result<(), SessionError> {
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
        validate_child_prefix(prefix, session_id)?;
    }
    Ok(())
}

fn child_staging_path(
    root: &std::path::Path,
    request: &SessionChildCreateRequest,
) -> Result<std::path::PathBuf, SessionError> {
    let parent = root.join(".child-staging");
    if parent.exists() && !fs::symlink_metadata(&parent)?.is_dir() {
        return Err(invalid("child staging root is not a regular directory"));
    }
    let staging = parent.join(request.child_id.to_string());
    prepare_child_staging(&staging, request)?;
    Ok(staging)
}

fn prepare_child_staging(
    staging: &std::path::Path,
    request: &SessionChildCreateRequest,
) -> Result<(), SessionError> {
    if staging.exists() {
        if fs::symlink_metadata(staging)?.file_type().is_symlink() {
            return Err(invalid("child staging path is a symlink"));
        }
        let retained: SessionChildCreateRequest = serde_json::from_slice(&fs::read(
            staging.join("request.json"),
        )?)
        .map_err(|_| {
            invalid(
                "interrupted child staging request is unverifiable; explicit maintenance required",
            )
        })?;
        if &retained != request {
            return Err(invalid(
                "child identity conflicts with interrupted creation request",
            ));
        }
        // The staging session was never canonical. Exclusive verified ownership permits restart.
        fs::remove_dir_all(staging)?;
    }
    fs::create_dir_all(staging)?;
    let request_path = staging.join("request.json");
    fs::write(
        request_path.with_extension("tmp"),
        serde_json::to_vec(request).map_err(|error| invalid(error.to_string()))?,
    )?;
    File::open(request_path.with_extension("tmp"))?.sync_all()?;
    fs::rename(request_path.with_extension("tmp"), &request_path)?;
    File::open(&request_path)?.sync_all()?;
    File::open(staging)?.sync_all()?;
    Ok(())
}

fn invalid(message: impl Into<String>) -> SessionError {
    SessionError::EventSerialization(message.into())
}

fn summary_lock_retry_budget(retries: usize, elapsed: std::time::Duration) -> bool {
    retries < 10 && elapsed < std::time::Duration::from_millis(250)
}

fn summary_lock_retry_allowed(
    error: &SessionError,
    retries: usize,
    elapsed: std::time::Duration,
) -> bool {
    summary_lock_retry_budget(retries, elapsed)
        && matches!(error, SessionError::Db(error) if error.is_lock_error())
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
        let mut retries = 0;
        let mut lock_retries = 0;
        let started = std::time::Instant::now();
        loop {
            match self.session_summary_non_owning_once(session_id).await {
                Err(error)
                    if summary_lock_retry_allowed(&error, lock_retries, started.elapsed()) =>
                {
                    lock_retries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                Err(
                    SessionError::ProjectionStale { .. }
                    | SessionError::Db(db::SessionDbError::ProjectionStale { .. }),
                ) if retries < 2 && summary_lock_retry_budget(0, started.elapsed()) => {
                    retries += 1;
                    tokio::task::yield_now().await;
                }
                result => return result,
            }
        }
    }

    async fn session_summary_non_owning_once(
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
        let resident = self.inner.lock().await.sessions.get(&session_id).cloned();
        if let Some(handle) = resident {
            let (summary, prefix) = handle.observed_summary().await?;
            verify_summary_prefix(&summary, &prefix, session_id)?;
            return Ok(summary);
        }
        let database = db::SessionDb::open_existing_turso_in_root(session_id, &root).await?;
        let result: Result<SessionSummary, SessionError> = async {
            database.validate_read_readiness().await?;
            let summary = self
                .load_db_session_state(session_id, &database)
                .await?
                .summary();
            let prefix = database.events_range(0, 1, 2).await?;
            verify_summary_prefix(&summary, &prefix, session_id)?;
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
    /// At most `limit` child projections are inspected. The parent-local canonical checkpoint
    /// establishes coverage independently of pagination. Unresolved publication intents or
    /// historical/missing/damaged projection state remain incomplete until explicit maintenance.
    ///
    /// # Errors
    /// Returns an error for an invalid work budget; individual observation failures are page issues.
    async fn observed_child_index(
        &self,
        store: &crate::SessionStoreExecutor,
        query: &SessionChildQuery,
    ) -> Result<(Vec<SessionId>, bool), SessionError> {
        let resident = self
            .inner
            .lock()
            .await
            .sessions
            .get(&query.parent_id)
            .cloned();
        if let Some(handle) = resident {
            return handle
                .observed_child_page(query.after, query.limit + 1)
                .await;
        }
        let database =
            db::SessionDb::open_existing_turso_in_root(query.parent_id, &store.root_path()).await?;
        let result = database
            .direct_child_page(query.after, query.limit + 1)
            .await
            .map_err(SessionError::from);
        let close = database.close().await;
        match (result, close) {
            (Ok(page), Ok(())) => Ok(page),
            (Err(error), _) => Err(error),
            (_, Err(error)) => Err(error.into()),
        }
    }

    /// Read a bounded indexed child window without owning sessions or replaying history.
    ///
    /// Unresolved publication and damaged projection coverage remain explicit.
    /// # Errors
    /// Rejects invalid query limits and unavailable parent metadata; individual child failures are page issues.
    pub async fn query_child_sessions(
        &self,
        query: SessionChildQuery,
    ) -> Result<SessionChildPage, SessionError> {
        if !(1..=128).contains(&query.limit) {
            return Err(invalid("child query limit must be between 1 and 128"));
        }
        self.session_summary_non_owning(query.parent_id).await?;
        let mut index_issue = None;
        let coverage;
        let candidates = if let Some(store) = &self.store {
            let result = self.observed_child_index(store, &query).await;
            if let Ok((ids, complete)) = result {
                coverage = complete;
                if !complete {
                    index_issue = Some("parent child publication is pending or historical coverage requires explicit reconciliation".into());
                }
                ids
            } else {
                coverage = false;
                index_issue = Some("parent child index is missing, stale, or corrupt; explicit maintenance required".into());
                Vec::new()
            }
        } else {
            let receipts = self.child_receipts.lock().await;
            let first = SessionId(uuid::Uuid::nil());
            let last = SessionId(uuid::Uuid::from_u128(u128::MAX));
            let start = query
                .after
                .map_or(Included((query.parent_id, first)), |id| {
                    Excluded((query.parent_id, id))
                });
            let end = Included((query.parent_id, last));
            let selected = receipts
                .range((start, end))
                .take(query.limit + 1)
                .map(|((_, id), _)| *id)
                .collect::<Vec<_>>();
            coverage = self
                .pending_child_counts
                .lock()
                .await
                .get(&query.parent_id)
                .copied()
                .unwrap_or(0)
                == 0;
            if !coverage {
                index_issue = Some("parent child publication is pending".into());
            }
            selected
        };
        let has_more = candidates.len() > query.limit;
        let mut page = SessionChildPage {
            children: Vec::new(),
            next_after: None,
            complete: coverage,
            issues: Vec::new(),
        };
        if let Some(issue) = index_issue {
            page.issues.push(issue);
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
    pub async fn create_child_session(
        &self,
        request: SessionChildCreateRequest,
    ) -> Result<SessionSummary, SessionError> {
        self.create_child_session_inner(request, None).await
    }

    /// Create a fresh background execution child with explicit immediate parent.
    /// # Errors
    /// Fails for invalid provenance, conflicting retries, or publication/storage failures.
    pub async fn create_child_execution_session(
        &self,
        request: SessionChildCreateRequest,
        provenance: crate::ExecutionSessionProvenance,
    ) -> Result<SessionSummary, SessionError> {
        crate::validate_execution_session_provenance(Some(&provenance))?;
        if provenance.context_mode
            != bcode_session_models::ExecutionSessionContextMode::FreshIsolated
        {
            return Err(invalid(
                "explicit-parent execution creation currently supports fresh isolated context only",
            ));
        }
        self.create_child_session_inner(request, Some(provenance))
            .await
    }

    /// Derive a context-preserving execution child from an admitted stable prefix, including worktrees.
    /// Immediate tree parent and provenance source remain independent. Retain the child ID on retries.
    /// # Errors
    /// Fails for invalid provenance, conflicting identity, unavailable source prefix, or publication failure.
    pub async fn create_pinned_child_execution_session(
        &self,
        mut request: SessionChildCreateRequest,
        mut provenance: crate::ExecutionSessionProvenance,
        parent_generation: u64,
    ) -> Result<SessionSummary, SessionError> {
        request.working_directory = normalize_working_directory(&request.working_directory);
        if request.parent_id == request.child_id {
            return Err(invalid("child cannot parent itself"));
        }
        provenance.context_mode =
            bcode_session_models::ExecutionSessionContextMode::FixedGenerationFork;
        provenance.parent_generation = Some(parent_generation);
        crate::validate_execution_session_provenance(Some(&provenance))?;
        let root = self
            .session_store_root()
            .ok_or_else(|| invalid("pinned child requires persistent storage"))?;
        validate_session_path(&root, request.child_id)?;
        self.session_summary_non_owning(request.parent_id).await?;
        if db::session_dir_path(&root, request.child_id).exists() {
            let database =
                db::SessionDb::open_existing_turso_in_root(request.child_id, &root).await?;
            let result: Result<SessionSummary, SessionError> = async {
                verify_request(&database.events_range(0, 1, 2).await?, &request)?;
                let summary = self
                    .load_db_session_state(request.child_id, &database)
                    .await?
                    .summary();
                if summary
                    .execution
                    .as_ref()
                    .map(|execution| &execution.provenance)
                    != Some(&provenance)
                {
                    return Err(invalid(
                        "pinned execution provenance conflicts with original creation",
                    ));
                }
                Ok(summary)
            }
            .await;
            let close = database.close().await;
            let summary = result?;
            close?;
            self.link_child_session(request.parent_id, request.child_id)
                .await?;
            return Ok(summary);
        }
        let source = self
            .session_summary_non_owning(provenance.parent_session_id)
            .await?;
        let mut derivation = Self::execution_derivation_request(
            request.name.clone(),
            &provenance,
            &source,
            parent_generation,
        );
        derivation.operation_id =
            bcode_session_models::SessionDerivationOperationId(request.child_id.0);
        derivation.idempotency_key = format!("execution-child:{}", request.child_id);
        derivation.source_policy =
            bcode_session_models::SessionDerivationSourcePolicy::StablePrefix;
        derivation.destination_working_directory = Some(request.working_directory.clone());
        self.record_child_link(request.parent_id, request.child_id, false)
            .await?;
        let ownership = lease::acquire_session_maintenance_guard(&root, request.child_id)?;
        let outcome = self
            .derive_session_with_execution(derivation, Some(provenance), Some(request.clone()))
            .await;
        drop(ownership);
        let bcode_session_models::SessionDerivationTerminalOutcome::Succeeded { session } =
            outcome?
        else {
            return Err(invalid("pinned execution child derivation did not succeed"));
        };
        self.link_child_session(request.parent_id, request.child_id)
            .await?;
        Ok(*session)
    }

    async fn create_child_session_inner(
        &self,
        mut request: SessionChildCreateRequest,
        execution: Option<crate::ExecutionSessionProvenance>,
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
                let summary = handle.snapshot().summary;
                self.link_child_session(request.parent_id, request.child_id)
                    .await?;
                return Ok(summary);
            }
            self.session_summary_non_owning(request.parent_id).await?;
            self.record_child_link(request.parent_id, request.child_id, false)
                .await?;
            let summary = self
                .create_session_record(
                    request.name.clone(),
                    request.working_directory.clone(),
                    execution,
                    Some(request.clone()),
                )
                .await?;
            self.link_child_session(request.parent_id, request.child_id)
                .await?;
            return Ok(summary);
        };
        self.create_persistent_child(request, execution, store)
            .await
    }

    async fn create_persistent_child(
        &self,
        request: SessionChildCreateRequest,
        execution: Option<crate::ExecutionSessionProvenance>,
        store: &crate::store_executor::SessionStoreExecutor,
    ) -> Result<SessionSummary, SessionError> {
        let root = store.root_path();
        validate_session_path(&root, request.child_id)?;
        validate_session_path(&root, request.parent_id)?;
        if db::session_dir_path(&root, request.child_id).exists() {
            let child = self.session_summary_non_owning(request.child_id).await?;
            if child.parent_id != Some(request.parent_id) {
                return Err(invalid(
                    "child identity already belongs to a different parent",
                ));
            }
        }
        // This guard verifies all live owners and serializes both publication and crash recovery.
        let stage_timer = timer(request.child_id, Stage::ParentIntent);
        self.record_child_link(request.parent_id, request.child_id, false)
            .await?;
        drop(stage_timer);
        let stage_timer = timer(request.child_id, Stage::MaintenanceOwnership);
        let ownership = lease::acquire_session_maintenance_guard(&root, request.child_id)?;
        drop(stage_timer);
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
            if summary.execution.as_ref().map(|value| &value.provenance) != execution.as_ref() {
                return Err(invalid(
                    "child execution provenance conflicts with original creation",
                ));
            }
            drop(ownership);
            self.link_child_session(request.parent_id, request.child_id)
                .await?;
            self.register_child_summary(summary.clone()).await;
            return Ok(summary);
        }
        // Only exact local canonical storage establishes the parent; catalog metadata is not authority.
        self.session_summary_non_owning(request.parent_id).await?;
        let stage_timer = timer(request.child_id, Stage::Staging);
        let staging = child_staging_path(&root, &request)?;
        drop(stage_timer);
        let summary = self
            .prepare_child_storage(&staging, &request, execution.as_ref())
            .await?;
        let stage_timer = timer(request.child_id, Stage::RenameAndSync);
        fs::rename(db::session_dir_path(&staging, request.child_id), &canonical)?;
        File::open(&root)?.sync_all()?;
        drop(stage_timer);
        drop(ownership);
        let stage_timer = timer(request.child_id, Stage::Publication);
        self.link_child_session(request.parent_id, request.child_id)
            .await?;
        self.register_child_summary(summary.clone()).await;
        drop(stage_timer);
        Ok(summary)
    }

    async fn prepare_child_storage(
        &self,
        staging: &std::path::Path,
        request: &SessionChildCreateRequest,
        execution: Option<&crate::ExecutionSessionProvenance>,
    ) -> Result<SessionSummary, SessionError> {
        let database = db::SessionDb::initialize_turso_in_root(request.child_id, staging).await?;
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
            let stage_timer = timer(request.child_id, Stage::CreationEvents);
            database
                .append_event_batch(&[(created, Some(now)), (relationship, Some(now))])
                .await?;
            drop(stage_timer);
            if let Some(provenance) = execution {
                let event = SessionEvent {
                    schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    sequence: 2,
                    timestamp_ms: now,
                    session_id: request.child_id,
                    provenance: None,
                    kind: SessionEventKind::ExecutionSessionCreated {
                        provenance: Box::new(provenance.clone()),
                        visibility: bcode_session_models::SessionVisibility::Background,
                    },
                };
                let stage_timer = timer(request.child_id, Stage::ExecutionEvent);
                database.append_event_batch(&[(event, Some(now))]).await?;
                drop(stage_timer);
            }
            let stage_timer = timer(request.child_id, Stage::Validation);
            database.validate_write_readiness().await?;
            drop(stage_timer);
            let stage_timer = timer(request.child_id, Stage::StateLoad);
            let summary = self
                .load_db_session_state(request.child_id, &database)
                .await?
                .summary();
            drop(stage_timer);
            Ok(summary)
        }
        .await;
        let stage_timer = timer(request.child_id, Stage::DatabaseClose);
        let close = database.close().await;
        drop(stage_timer);
        let summary = result?;
        close?;
        let stage_timer = timer(request.child_id, Stage::Manifest);
        crate::derivation::write_staged_manifest(staging, &summary)?;
        drop(stage_timer);
        Ok(summary)
    }

    /// Idempotently register a verified immediate child and acknowledge publication.
    /// # Errors
    /// Fails if child identity/parent cannot be verified or the parent writer cannot append.
    pub async fn link_child_session(
        &self,
        parent_id: SessionId,
        child_id: SessionId,
    ) -> Result<(), SessionError> {
        let child = self.session_summary_non_owning(child_id).await?;
        if child.parent_id != Some(parent_id) {
            return Err(invalid("child has a different immutable parent"));
        }
        self.record_child_link(parent_id, child_id, false).await?;
        self.record_child_link(parent_id, child_id, true).await
    }

    async fn record_child_link(
        &self,
        parent: SessionId,
        child: SessionId,
        published: bool,
    ) -> Result<(), SessionError> {
        let handle = self.session_handle(parent).await?;
        for _ in 0..8 {
            let generation = self.current_session_generation(parent).await?;
            let receipt = if self.store.is_some() {
                handle.observed_child_receipt(child).await?
            } else {
                self.child_receipts
                    .lock()
                    .await
                    .get(&(parent, child))
                    .copied()
            };
            if receipt.is_some_and(|done| !published || done) {
                return Ok(());
            }
            let kind = if published {
                let summary = self.session_summary_non_owning(child).await?;
                if summary.parent_id != Some(parent) {
                    return Err(invalid("publication parent mismatch"));
                }
                SessionEventKind::SessionChildPublished {
                    child_id: child,
                    title: summary
                        .name
                        .map(|name| crate::bounded_utf8_prefix(&name, 256).0),
                }
            } else {
                SessionEventKind::SessionChildLinked { child_id: child }
            };
            match handle
                .append_event_at_generation(kind, generation, self.next_activity_timestamp_ms())
                .await
            {
                Ok(event) => {
                    if self.store.is_none() {
                        let previous = self
                            .child_receipts
                            .lock()
                            .await
                            .insert((parent, child), published);
                        let mut counts = self.pending_child_counts.lock().await;
                        let count = counts.entry(parent).or_default();
                        if !published && previous.is_none() {
                            *count += 1;
                        }
                        if published && previous == Some(false) {
                            *count = count.saturating_sub(1);
                        }
                    }
                    self.publish_committed_mutation(event, handle.snapshot().summary);
                    return Ok(());
                }
                Err(SessionError::AppendGenerationChanged { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Err(invalid(
            "parent changed during bounded child registration; retry identical request",
        ))
    }

    /// Explicitly reconcile historical or damaged relationship projections under exclusive ownership.
    /// This maintenance scans canonical directories and replays parent history; normal reads never do.
    /// # Errors
    /// Fails while any parent owner remains, on unverifiable storage, or unresolved publication intents.
    pub async fn reconcile_child_relationships(
        &self,
        parent_id: SessionId,
    ) -> Result<bcode_session_models::SessionChildReconciliation, SessionError> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| invalid("relationship maintenance requires persistent storage"))?;
        let root = store.root_path();
        validate_session_path(&root, parent_id)?;
        let maintenance = lease::acquire_session_maintenance_guard(&root, parent_id)?;
        let _write = lease::acquire_maintenance_session_write_lock(&maintenance, &root, parent_id)?;
        let database = db::SessionDb::open_existing_turso_in_root(parent_id, &root).await?;
        let result: Result<bcode_session_models::SessionChildReconciliation, SessionError> = async {
            database.rebuild_child_projection_for_maintenance().await?;
            let mut verified = std::collections::BTreeMap::new();
            for entry in fs::read_dir(&root)? {
                let entry = entry?;
                let Ok(id) = entry.file_name().to_string_lossy().parse::<SessionId>() else { continue; };
                if id == parent_id { continue; }
                let summary = self.session_summary_non_owning(id).await?;
                if summary.parent_id == Some(parent_id) { verified.insert(id, summary.name); }
            }
            let mut cursor = None;
            loop {
                let (ids, _) = database.direct_child_page(cursor, 128).await?;
                if ids.is_empty() { break; }
                for id in &ids {
                    if !verified.contains_key(id) { return Err(invalid("unresolved parent child intent requires explicit recovery of its original creation request")); }
                }
                cursor = ids.last().copied();
            }
            for (id, title) in &verified {
                if database.child_publication_receipt(*id).await?.is_none() {
                    append_maintenance_relationship(&database, parent_id, SessionEventKind::SessionChildLinked { child_id: *id }).await?;
                }
                if database.child_publication_receipt(*id).await? != Some(true) {
                    append_maintenance_relationship(&database, parent_id, SessionEventKind::SessionChildPublished {
                        child_id: *id, title: title.as_ref().map(|title| crate::bounded_utf8_prefix(title, 256).0),
                    }).await?;
                }
            }
            append_maintenance_relationship(&database, parent_id, SessionEventKind::SessionChildrenReconciled {
                verified_children: u64::try_from(verified.len()).map_err(|_| invalid("child count overflow"))?,
            }).await?;
            Ok(bcode_session_models::SessionChildReconciliation { parent_id, verified_children: verified.len() })
        }.await;
        let close = database.close().await;
        let outcome = result?;
        close?;
        Ok(outcome)
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
        let stage_timer = timer(summary.id, Stage::CatalogSchedule);
        if let Some(store) = &self.store {
            // Canonical child publication is already durable. The disposable catalog uses
            // the existing coalescing worker; only explicit flush/shutdown needs a barrier.
            store.schedule_catalog_summary(summary).await;
        }
        drop(stage_timer);
    }
}

async fn append_maintenance_relationship(
    database: &db::SessionDb,
    parent: SessionId,
    kind: SessionEventKind,
) -> Result<(), SessionError> {
    let event = SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: database
            .last_event_sequence()
            .await?
            .map_or(0, |tail| tail + 1),
        timestamp_ms: crate::current_unix_millis(),
        session_id: parent,
        provenance: None,
        kind,
    };
    database.append_event_batch(&[(event, None)]).await?;
    Ok(())
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

    #[test]
    fn lock_retry_budget_is_bounded_and_damage_is_not_retryable() {
        use std::time::Duration;
        assert!(summary_lock_retry_budget(9, Duration::from_millis(249)));
        assert!(!summary_lock_retry_budget(10, Duration::ZERO));
        assert!(!summary_lock_retry_budget(0, Duration::from_millis(250)));
        for error in [
            db::SessionDbError::InvalidData("damaged".into()),
            db::SessionDbError::WriterIncompatible {
                actual: Some(999),
                expected: 4,
            },
            db::SessionDbError::ProjectionStale {
                projection: "summary",
                checkpoint: None,
                expected: 1,
            },
        ] {
            assert!(!summary_lock_retry_allowed(
                &SessionError::Db(error),
                0,
                Duration::ZERO
            ));
        }
    }

    #[tokio::test]
    async fn concurrent_turso_writer_and_bounded_summary_observer() {
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::persistent(root.path()).unwrap();
        let parent = manager
            .create_session(None, root.path().to_path_buf())
            .await
            .unwrap();
        let canonical_before = manager
            .session_history_around(
                parent.id,
                bcode_session_models::SessionHistoryAroundQuery {
                    sequence: 0,
                    before: 0,
                    after: 0,
                },
            )
            .await
            .unwrap();
        let writer = async {
            for _ in 0..24 {
                manager
                    .append_context_compacted(parent.id, "context".into(), 0)
                    .await
                    .unwrap();
                tokio::task::yield_now().await;
            }
        };
        let reader = async {
            for _ in 0..24 {
                let summary = manager
                    .session_summary_non_owning(parent.id)
                    .await
                    .unwrap_or_else(|error| {
                        panic!("observation category: {}", error.observation_category())
                    });
                assert_eq!(summary.id, parent.id);
                tokio::task::yield_now().await;
            }
        };
        tokio::join!(writer, reader);
        let canonical_after = manager
            .session_history_around(
                parent.id,
                bcode_session_models::SessionHistoryAroundQuery {
                    sequence: 0,
                    before: 0,
                    after: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(canonical_before.events, canonical_after.events);
        assert_eq!(canonical_after.last_available_sequence, Some(24));
    }

    #[tokio::test]
    async fn live_parent_observation_and_child_publication_reuse_owned_database() {
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::persistent(root.path()).unwrap();
        let parent = manager
            .create_session(None, root.path().to_path_buf())
            .await
            .unwrap();
        let guard = manager
            .acquire_session_ownership(parent.id, crate::SessionOwnershipKind::RuntimeWork)
            .await
            .unwrap();
        manager
            .append_context_compacted(parent.id, "context".into(), 0)
            .await
            .unwrap();
        assert!(manager.session_is_owned(parent.id).await);
        assert_eq!(
            manager
                .session_summary_non_owning(parent.id)
                .await
                .unwrap()
                .id,
            parent.id
        );
        let child = manager
            .create_child_session(request(parent.id))
            .await
            .unwrap();
        let page = manager
            .query_child_sessions(SessionChildQuery {
                parent_id: parent.id,
                after: None,
                limit: 10,
            })
            .await
            .unwrap();
        assert!(page.complete);
        assert!(page.children.iter().any(|summary| summary.id == child.id));
        assert!(manager.session_is_owned(parent.id).await);
        drop(guard);
    }

    fn request(parent: SessionId) -> SessionChildCreateRequest {
        SessionChildCreateRequest {
            child_id: SessionId::new(),
            parent_id: parent,
            name: Some("child".into()),
            working_directory: std::env::current_dir().unwrap(),
        }
    }

    #[tokio::test]
    async fn fresh_active_parent_child_queries_preserve_trustworthy_empty_inventory() {
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::persistent(root.path()).unwrap();
        let parent = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        let query = SessionChildQuery {
            parent_id: parent.id,
            after: None,
            limit: 100,
        };
        let writer = async {
            for index in 0..32 {
                manager
                    .append_model_changed(
                        parent.id,
                        "provider".into(),
                        format!("model-{index}"),
                        bcode_session_models::ModelSelectionSource::UserExplicit,
                    )
                    .await
                    .unwrap();
                tokio::task::yield_now().await;
            }
        };
        let reader = async {
            for _ in 0..32 {
                let page = manager.query_child_sessions(query.clone()).await.unwrap();
                assert!(
                    page.complete,
                    "fresh canonical parent must not need reconciliation"
                );
                assert!(page.children.is_empty());
                assert!(page.next_after.is_none());
                assert!(page.issues.is_empty());
                tokio::task::yield_now().await;
            }
        };
        tokio::join!(writer, reader);
    }

    #[tokio::test]
    async fn parent_checkpoint_pending_intent_retry_and_explicit_repair() {
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::persistent(root.path()).unwrap();
        let parent = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        let query = SessionChildQuery {
            parent_id: parent.id,
            after: None,
            limit: 1,
        };
        assert!(
            manager
                .query_child_sessions(query.clone())
                .await
                .unwrap()
                .complete
        );
        let request = request(parent.id);
        manager
            .record_child_link(parent.id, request.child_id, false)
            .await
            .unwrap();
        let pending = manager.query_child_sessions(query.clone()).await.unwrap();
        assert!(!pending.complete);
        assert!(!pending.issues.is_empty());
        let child = manager.create_child_session(request.clone()).await.unwrap();
        assert!(
            manager
                .query_child_sessions(query.clone())
                .await
                .unwrap()
                .complete
        );
        manager.create_child_session(request).await.unwrap();
        let database = db::SessionDb::open_existing_turso_in_root(parent.id, root.path())
            .await
            .unwrap();
        let events = database.events_range(0, 128, 128).await.unwrap();
        assert_eq!(events.iter().filter(|event| matches!(&event.kind, SessionEventKind::SessionChildLinked { child_id } if *child_id == child.id)).count(), 1);
        assert_eq!(events.iter().filter(|event| matches!(&event.kind, SessionEventKind::SessionChildPublished { child_id, .. } if *child_id == child.id)).count(), 1);
        database
            .database()
            .delete("session_children_checkpoint")
            .execute(database.database())
            .await
            .unwrap();
        database.close().await.unwrap();
        assert!(
            !manager
                .query_child_sessions(query.clone())
                .await
                .unwrap()
                .complete
        );
        manager.release_session_ownership(parent.id).await.unwrap();
        let maintenance = SessionManager::persistent(root.path()).unwrap();
        let outcome = maintenance
            .reconcile_child_relationships(parent.id)
            .await
            .unwrap();
        assert_eq!(outcome.verified_children, 1);
        assert!(
            maintenance
                .query_child_sessions(query)
                .await
                .unwrap()
                .complete
        );
    }

    #[tokio::test]
    async fn pinned_child_preserves_context_worktree_and_distinct_tree_parent() {
        let root = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let manager = SessionManager::persistent(root.path()).unwrap();
        let source = manager
            .create_session(Some("source".into()), std::env::current_dir().unwrap())
            .await
            .unwrap();
        manager
            .append_event(
                source.id,
                SessionEventKind::SystemMessage {
                    text: "pinned source context".into(),
                },
            )
            .await
            .unwrap();
        let generation = manager.current_session_generation(source.id).await.unwrap();
        let parent = manager
            .create_child_session(request(source.id))
            .await
            .unwrap();
        let mut request = request(parent.id);
        request.working_directory = worktree.path().to_path_buf();
        let provenance = crate::ExecutionSessionProvenance {
            version: bcode_session_models::EXECUTION_SESSION_PROVENANCE_VERSION,
            owner: "workflow".into(),
            run_id: "pinned-run".into(),
            node_id: "pinned-node".into(),
            activation_id: None,
            attempt: 1,
            parent_session_id: source.id,
            workspace_snapshot: Some("worktree-snapshot".into()),
            context_mode: bcode_session_models::ExecutionSessionContextMode::FixedGenerationFork,
            parent_generation: Some(generation),
        };
        let child = manager
            .create_pinned_child_execution_session(request.clone(), provenance.clone(), generation)
            .await
            .unwrap();
        assert_eq!(child.id, request.child_id);
        assert_eq!(child.parent_id, Some(parent.id));
        assert_eq!(
            child
                .execution
                .as_ref()
                .unwrap()
                .provenance
                .parent_session_id,
            source.id
        );
        assert_eq!(
            child.working_directory,
            normalize_working_directory(worktree.path())
        );
        let history = manager.session_history(child.id).await.unwrap();
        assert!(history.iter().any(|event| matches!(&event.kind, SessionEventKind::SystemMessage { text } if text == "pinned source context")));
        assert_eq!(
            manager
                .create_pinned_child_execution_session(request, provenance, generation)
                .await
                .unwrap()
                .id,
            child.id
        );
        let page = manager
            .query_child_sessions(SessionChildQuery {
                parent_id: parent.id,
                after: None,
                limit: 1,
            })
            .await
            .unwrap();
        assert!(page.complete);
        assert_eq!(page.children[0].id, child.id);
    }

    #[tokio::test]
    async fn corrupted_child_checkpoint_requires_explicit_reconciliation() {
        use switchy::database::query::FilterableQuery;
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::persistent(root.path()).unwrap();
        let parent = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        let query = SessionChildQuery {
            parent_id: parent.id,
            after: None,
            limit: 1,
        };
        let database = db::SessionDb::open_existing_turso_in_root(parent.id, root.path())
            .await
            .unwrap();
        database
            .database()
            .update("session_children_checkpoint")
            .value("event_seq", 999_i64)
            .where_eq("singleton", 1_i64)
            .execute(database.database())
            .await
            .unwrap();
        database.close().await.unwrap();
        let before = manager.query_child_sessions(query.clone()).await.unwrap();
        assert!(!before.complete);
        assert!(!before.issues.is_empty());
        manager.release_session_ownership(parent.id).await.unwrap();
        let maintenance = SessionManager::persistent(root.path()).unwrap();
        assert_eq!(
            maintenance
                .reconcile_child_relationships(parent.id)
                .await
                .unwrap()
                .verified_children,
            0
        );
        let after = maintenance.query_child_sessions(query).await.unwrap();
        assert!(after.complete);
        assert!(after.children.is_empty());
    }

    #[tokio::test]
    async fn child_publication_queues_catalog_without_flush_and_shutdown_converges() {
        use crate::child_creation_observation::{ChildCreationObserver, observe_child_creation};
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::persistent(root.path()).unwrap();
        let parent = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        manager.flush_catalog_updates().await;
        let observations = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&observations);
        let observer: ChildCreationObserver = std::sync::Arc::new(move |observation| {
            recorded.lock().unwrap().push(observation);
        });
        let children = Box::pin(observe_child_creation(observer, async {
            let mut children = Vec::new();
            for _ in 0..3 {
                children.push(
                    manager
                        .create_child_session(request(parent.id))
                        .await
                        .unwrap(),
                );
            }
            children
        }))
        .await;
        // Relationship discovery is canonical and must not depend on a catalog barrier.
        let page = manager
            .query_child_sessions(SessionChildQuery {
                parent_id: parent.id,
                after: None,
                limit: 8,
            })
            .await
            .unwrap();
        assert!(page.complete);
        assert_eq!(page.children.len(), children.len());
        manager.shutdown_catalog_updates().await;
        let catalog = manager
            .store
            .as_ref()
            .unwrap()
            .load_catalog()
            .await
            .unwrap();
        for child in &children {
            assert_eq!(
                catalog.get(&child.id).unwrap().summary.parent_id,
                Some(parent.id)
            );
        }
        let observations = observations.lock().unwrap();
        assert_eq!(
            observations
                .iter()
                .filter(|value| value.stage == Stage::CatalogFlush)
                .count(),
            0
        );
        assert_eq!(
            observations
                .iter()
                .filter(|value| value.stage == Stage::CatalogSchedule)
                .count(),
            children.len()
        );
    }

    #[tokio::test]
    async fn child_publication_and_retry_do_not_wait_for_blocked_catalog_worker() {
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::persistent(root.path()).unwrap();
        let parent = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        manager.flush_catalog_updates().await;
        let gate = manager
            .store
            .as_ref()
            .unwrap()
            .gate_catalog_persistence()
            .await;
        // Ensure the real persistence worker is blocked before authoritative creation starts.
        manager
            .store
            .as_ref()
            .unwrap()
            .schedule_catalog_summary(parent.clone())
            .await;
        gate.entered().await;
        let request = request(parent.id);
        let creation = manager.create_child_session(request.clone());
        tokio::pin!(creation);
        let created = tokio::time::timeout(std::time::Duration::from_secs(5), &mut creation).await;
        let child = if let Ok(result) = created {
            result.unwrap()
        } else {
            drop(gate);
            let _ = creation.await;
            manager.shutdown_catalog_updates().await;
            panic!("canonical child creation waited for blocked derived catalog");
        };
        let retry = manager.create_child_session(request);
        tokio::pin!(retry);
        let retried = tokio::time::timeout(std::time::Duration::from_secs(5), &mut retry).await;
        let retried = if let Ok(result) = retried {
            result.unwrap()
        } else {
            drop(gate);
            let _ = retry.await;
            manager.shutdown_catalog_updates().await;
            panic!("canonical retry waited for blocked derived catalog");
        };
        assert_eq!(child.id, retried.id);
        let page = manager
            .query_child_sessions(SessionChildQuery {
                parent_id: parent.id,
                after: None,
                limit: 8,
            })
            .await
            .unwrap();
        assert!(page.complete);
        assert_eq!(page.children.len(), 1);
        assert_eq!(page.children[0].id, child.id);
        let flush = manager.flush_catalog_updates();
        tokio::pin!(flush);
        let flush_pending = std::future::poll_fn(|cx| {
            use std::future::Future;
            std::task::Poll::Ready(flush.as_mut().poll(cx).is_pending())
        })
        .await;
        assert!(
            flush_pending,
            "explicit flush must wait for catalog persistence"
        );
        let shutdown = manager.shutdown_catalog_updates();
        tokio::pin!(shutdown);
        let pending = std::future::poll_fn(|cx| {
            use std::future::Future;
            std::task::Poll::Ready(shutdown.as_mut().poll(cx).is_pending())
        })
        .await;
        assert!(pending, "shutdown must drain the blocked catalog worker");
        drop(gate);
        flush.await;
        shutdown.await;
        let catalog = manager
            .store
            .as_ref()
            .unwrap()
            .load_catalog()
            .await
            .unwrap();
        assert_eq!(
            catalog.get(&child.id).unwrap().summary.parent_id,
            Some(parent.id)
        );
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
        assert!(page.complete);
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
