//! Retained top-level/ordinary-child managed open preparation. No native I/O is performed.
use super::{SessionEvent, SessionEventKind, SessionId};
use bcode_harness_models::{HarnessCapabilities, HarnessRequest};
use bcode_session_models::external_harness::{
    ExternalHarnessOperation, ExternalHarnessOperationRecord, ExternalHarnessOwner,
    ExternalHarnessRequest,
};

impl crate::SessionManager {
    /// Create or verify the exact managed session identity and durable pre-open configuration.
    /// Top-level initial history/configuration publishes atomically by rename under exclusive
    /// maintenance ownership. Child publication uses ordinary immutable child-session semantics.
    /// No native creation or delivery is authorized until the separate actor claim returns New.
    /// # Errors
    /// Rejects mismatched owners, conflicting identity/workspace/parent/request, nonpersistent
    /// storage, unverifiable canonical paths and incomplete storage. Never repairs or adopts it.
    pub async fn prepare_external_harness_open(
        &self,
        owner: &ExternalHarnessOwner,
        request: &HarnessRequest,
        capabilities: HarnessCapabilities,
        parent_id: Option<SessionId>,
    ) -> Result<bcode_session_models::SessionSummary, crate::SessionError> {
        let retained = ExternalHarnessRequest::from_request(request)
            .map_err(crate::SessionError::EventSerialization)?;
        self.prepare_external_harness_retained_open(owner, retained, capabilities, parent_id, None)
            .await
    }

    /// Prepare an exact selected-model Open using canonical managed-session publication.
    /// # Errors
    /// Rejects unavailable durable fences, conflicting requests, stale owners and resume.
    pub async fn prepare_external_harness_selected_open(
        &self,
        owner: &ExternalHarnessOwner,
        request: &bcode_harness_models::HarnessModelsRequest,
        capabilities: HarnessCapabilities,
        parent_id: Option<SessionId>,
    ) -> Result<bcode_session_models::SessionSummary, crate::SessionError> {
        let retained = ExternalHarnessRequest::from_models_request(request)
            .map_err(crate::SessionError::EventSerialization)?;
        self.prepare_external_harness_retained_open(owner, retained, capabilities, parent_id, None)
            .await
    }

    /// Canonically retain the entire v2 spawn intent before preparing its exact selected Open.
    /// # Errors
    /// Rejects conflicting child intent, stale owner, mixed execution and unavailable storage fences.
    pub async fn prepare_external_harness_selected_spawn(
        &self,
        owner: &ExternalHarnessOwner,
        request: &bcode_session_models::SessionSpawnRequest,
        capabilities: HarnessCapabilities,
    ) -> Result<bcode_session_models::SessionSummary, crate::SessionError> {
        request
            .validate_storage(
                bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH,
            )
            .map_err(crate::SessionError::EventSerialization)?;
        let intent = request.managed.as_ref().ok_or_else(|| {
            crate::SessionError::EventSerialization("managed spawn intent required".into())
        })?;
        let retained = ExternalHarnessRequest::from_models_request(&intent.selected_open.request)
            .map_err(crate::SessionError::EventSerialization)?;
        if owner.session_id != request.creation.child_id
            || retained.target() != (owner.session_id.to_string().as_str(), owner.generation)
        {
            return Err(crate::SessionError::EventSerialization(
                "managed spawn owner mismatch".into(),
            ));
        }
        self.prepare_external_harness_retained_open(
            owner,
            retained,
            capabilities,
            Some(request.creation.parent_id),
            Some(request),
        )
        .await
    }

    async fn prepare_external_harness_retained_open(
        &self,
        owner: &ExternalHarnessOwner,
        retained: ExternalHarnessRequest,
        capabilities: HarnessCapabilities,
        parent_id: Option<SessionId>,
        spawning: Option<&bcode_session_models::SessionSpawnRequest>,
    ) -> Result<bcode_session_models::SessionSummary, crate::SessionError> {
        use crate::actor::harness::{HarnessAction, HarnessResult};
        let invalid =
            || crate::SessionError::EventSerialization("managed open preparation conflict".into());
        owner
            .validate()
            .map_err(crate::SessionError::EventSerialization)?;
        retained
            .validate()
            .map_err(crate::SessionError::EventSerialization)?;
        if retained.is_selected_open()
            && (bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION
                != bcode_session_models::external_harness::SELECTED_OPEN_EVENT_SCHEMA_VERSION
                || bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH
                    != bcode_session_models::external_harness::SELECTED_OPEN_STORAGE_WRITER_EPOCH)
        {
            return Err(invalid());
        }
        let Some(open) = retained.open() else {
            return Err(invalid());
        };
        if open.session_id != owner.session_id.to_string()
            || open.generation != owner.generation
            || owner.generation != 1
            || open.resume.is_some()
            || parent_id == Some(owner.session_id)
        {
            return Err(invalid());
        }
        let store = self.store.as_ref().ok_or_else(invalid)?;
        if store.lease_owner().build_fingerprint.as_deref() != Some(owner.artifact_id.as_str())
            || store.lease_owner().daemon_instance_id.as_deref() != Some(owner.daemon_id.as_str())
        {
            return Err(invalid());
        }
        let cwd = std::fs::canonicalize(&open.working_directory)?;
        if !cwd.is_dir() {
            return Err(invalid());
        }
        let operation = ExternalHarnessOperation {
            version: 1,
            owner: owner.clone(),
            inventory_commitments: Vec::new(),
            record: ExternalHarnessOperationRecord::Configured {
                instance: retained.instance.clone(),
                capabilities,
                open_request: Some(retained.clone()),
            },
        };
        operation
            .validate_storage(
                bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH,
            )
            .map_err(crate::SessionError::EventSerialization)?;
        let summary = if let Some(spawning) = spawning {
            self.create_spawning_session(spawning.clone()).await?
        } else if let Some(parent_id) = parent_id {
            self.prepare_external_child(owner, parent_id, &cwd).await?
        } else {
            self.prepare_external_top_level(owner, &cwd, &operation)
                .await?
        };
        if summary.parent_id != parent_id
            || std::fs::canonicalize(&summary.working_directory)? != cwd
        {
            return Err(invalid());
        }
        let handle = self.session_handle(owner.session_id).await?;
        let result = handle
            .harness_operation(
                owner.clone(),
                HarnessAction::Write(Box::new(operation.record)),
                self.next_activity_timestamp_ms(),
            )
            .await?;
        if !matches!(result, HarnessResult::Committed) {
            return Err(invalid());
        }
        Ok(summary)
    }
    async fn prepare_external_child(
        &self,
        owner: &ExternalHarnessOwner,
        parent_id: SessionId,
        cwd: &std::path::Path,
    ) -> Result<bcode_session_models::SessionSummary, crate::SessionError> {
        let invalid =
            || crate::SessionError::EventSerialization("managed open preparation conflict".into());
        let creation = bcode_session_models::SessionChildCreateRequest {
            child_id: owner.session_id,
            parent_id,
            name: None,
            working_directory: cwd.to_path_buf(),
        };
        Ok(
            match self.session_summary_non_owning(owner.session_id).await {
                Ok(existing) if existing.execution.is_some() => {
                    let events = self
                        .session_handle(owner.session_id)
                        .await?
                        .events_range(0, 1, 2)
                        .await?;
                    let retained_creation = events
                        .iter()
                        .find_map(|event| match &event.kind {
                            SessionEventKind::ChildSessionCreated { request, .. }
                                if event.sequence == 1 && event.session_id == owner.session_id =>
                            {
                                Some(request.as_ref().clone())
                            }
                            _ => None,
                        })
                        .ok_or_else(invalid)?;
                    if retained_creation.child_id != creation.child_id
                        || retained_creation.parent_id != creation.parent_id
                        || std::fs::canonicalize(&retained_creation.working_directory)? != cwd
                    {
                        return Err(invalid());
                    }
                    let provenance = existing.execution.ok_or_else(invalid)?.provenance;
                    self.create_child_execution_session(retained_creation, provenance)
                        .await?
                }
                Ok(_) | Err(crate::SessionError::NotFound(_)) => {
                    self.create_child_session(creation).await?
                }
                Err(error) => return Err(error),
            },
        )
    }

    async fn prepare_external_top_level(
        &self,
        owner: &ExternalHarnessOwner,
        cwd: &std::path::Path,
        operation: &ExternalHarnessOperation,
    ) -> Result<bcode_session_models::SessionSummary, crate::SessionError> {
        let invalid =
            || crate::SessionError::EventSerialization("managed open preparation conflict".into());
        let store = self.store.as_ref().ok_or_else(invalid)?;
        let gate = self.session_load_gate(owner.session_id).await;
        let _load = gate.lock().await;
        let root = store.root_path();
        let canonical = crate::db::session_dir_path(&root, owner.session_id);
        Ok(if canonical.exists() {
            if std::fs::symlink_metadata(&canonical)?
                .file_type()
                .is_symlink()
            {
                return Err(invalid());
            }
            let cached = self
                .inner
                .lock()
                .await
                .sessions
                .get(&owner.session_id)
                .cloned();
            if let Some(handle) = cached {
                handle.summary().await?
            } else {
                self.session_summary_non_owning(owner.session_id).await?
            }
        } else {
            // Exclusive maintenance fencing is cross-process; the load gate is merely
            // local registry coordination. Retained creation is never protected by memory alone.
            let maintenance =
                crate::lease::acquire_session_maintenance_guard(&root, owner.session_id)?;
            if canonical.exists() {
                return Err(invalid());
            }
            let staging_parent = root.join(".managed-staging");
            std::fs::create_dir_all(&staging_parent)?;
            if std::fs::symlink_metadata(&staging_parent)?
                .file_type()
                .is_symlink()
            {
                return Err(invalid());
            }
            let staging = staging_parent.join(uuid::Uuid::new_v4().to_string());
            std::fs::create_dir(&staging)?;
            let database =
                crate::db::SessionDb::initialize_turso_in_root(owner.session_id, &staging).await?;
            let now = self.next_activity_timestamp_ms();
            let created = SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 0,
                timestamp_ms: now,
                session_id: owner.session_id,
                provenance: None,
                kind: SessionEventKind::SessionCreated {
                    name: None,
                    working_directory: cwd.to_path_buf(),
                },
            };
            let mut prepared_operation = operation.clone();
            database
                .prepare_harness_inventory_operation(&mut prepared_operation, 1)
                .await?;
            let configured = SessionEvent {
                sequence: 1,
                kind: SessionEventKind::ExternalHarnessOperation {
                    operation: Box::new(prepared_operation),
                },
                ..created.clone()
            };
            let result = database
                .append_event_batch(&[(created, Some(now)), (configured, Some(now))])
                .await;
            let summary = async {
                result?;
                Ok::<_, crate::SessionError>(
                    self.load_db_session_state(owner.session_id, &database)
                        .await?
                        .summary(),
                )
            }
            .await;
            let close = database.close().await;
            let summary = summary?;
            close?;
            crate::derivation::write_staged_manifest(&staging, &summary)?;
            let staged = crate::db::session_dir_path(&staging, owner.session_id);
            std::fs::File::open(&staged)?.sync_all()?;
            std::fs::rename(&staged, &canonical)?;
            std::fs::File::open(&root)?.sync_all()?;
            // Interrupted unpublished staging is never canonical and never authorizes
            // native effects. Leave it for explicit maintenance rather than deleting data.
            drop(maintenance);
            self.session_summary_non_owning(owner.session_id).await?
        })
    }
}
