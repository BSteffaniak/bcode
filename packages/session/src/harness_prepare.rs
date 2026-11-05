//! Retained top-level/ordinary-child managed open preparation. No native I/O is performed.
use super::{SessionEvent, SessionEventKind, SessionId};
use bcode_harness_models::{HarnessCapabilities, HarnessRequest};
use bcode_session_models::external_harness::{
    ExternalHarnessOperation, ExternalHarnessOperationRecord, ExternalHarnessOwner,
    ExternalHarnessRequest, ExternalHarnessRequestKind,
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
        use crate::actor::harness::{HarnessAction, HarnessResult};
        let invalid =
            || crate::SessionError::EventSerialization("managed open preparation conflict".into());
        owner
            .validate()
            .map_err(crate::SessionError::EventSerialization)?;
        let retained = ExternalHarnessRequest::from_request(request)
            .map_err(crate::SessionError::EventSerialization)?;
        let ExternalHarnessRequestKind::Open(open) = &retained.kind else {
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
            record: ExternalHarnessOperationRecord::Configured {
                instance: request.instance.clone(),
                capabilities,
                open_request: Some(retained.clone()),
            },
        };
        operation
            .validate()
            .map_err(crate::SessionError::EventSerialization)?;
        let summary = if let Some(parent_id) = parent_id {
            self.create_child_session(bcode_session_models::SessionChildCreateRequest {
                child_id: owner.session_id,
                parent_id,
                name: None,
                working_directory: cwd.clone(),
            })
            .await?
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
            let configured = SessionEvent {
                sequence: 1,
                kind: SessionEventKind::ExternalHarnessOperation {
                    operation: Box::new(operation.clone()),
                },
                ..created.clone()
            };
            let result = database
                .append_event_batch(&[(created, Some(now)), (configured, Some(now))])
                .await;
            let close = database.close().await;
            result?;
            close?;
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
