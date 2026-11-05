//! Public canonical native-operation APIs. Every protected operation enters the session mailbox.
use crate::actor::harness::{HarnessAction as A, HarnessResult as R};
use crate::{SessionError, SessionManager, SessionOwnershipGuard, SessionOwnershipKind};
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessCapabilities, HarnessFailure, HarnessInstance,
    HarnessInteraction, HarnessObservationPage, HarnessReceipt, HarnessRequest,
    HarnessServiceResponse, HarnessServiceResponseKind, MAX_HARNESS_ID_BYTES,
    MAX_HARNESS_OBSERVATIONS,
};
use bcode_session_models::external_harness::{
    ExternalHarnessClaim, ExternalHarnessOperationRecord, ExternalHarnessOwner,
    ExternalHarnessRequest, ExternalHarnessResponse, ExternalHarnessState,
};
fn mismatch() -> SessionError {
    SessionError::EventSerialization("invalid canonical harness result".into())
}
impl SessionManager {
    /// Read exact typed native acceptance evidence without acquiring execution ownership.
    /// Current caller access authorization is required at the application boundary.
    /// # Errors
    /// Rejects invalid identities, stale indexes, conflicting evidence and concurrent changes.
    pub async fn session_external_harness_submission_acceptance(
        &self,
        session_id: bcode_session_models::SessionId,
        submission_id: &str,
        attempt_id: &str,
    ) -> Result<Option<bcode_harness_models::HarnessObservation>, SessionError> {
        if [submission_id, attempt_id].iter().any(|id| {
            id.is_empty() || id.len() > MAX_HARNESS_ID_BYTES || id.chars().any(char::is_control)
        }) {
            return Err(mismatch());
        }
        let gate = self.session_load_gate(session_id).await;
        let _guard = gate.lock().await;
        let cached = self.inner.lock().await.sessions.get(&session_id).cloned();
        if let Some(handle) = cached {
            return handle
                .harness_submission_acceptance(submission_id.into(), attempt_id.into())
                .await;
        }
        let Some(store) = self.store.as_ref() else {
            return Ok(None);
        };
        let root = store.root_path();
        if !crate::db::session_db_path(&root, session_id).exists() {
            return Ok(None);
        }
        let db = crate::db::SessionDb::open_existing_turso_in_root(session_id, &root).await?;
        Ok(db
            .external_harness_submission_acceptance(submission_id, attempt_id)
            .await?)
    }
    /// Compare exact retained mutation evidence independently of its historical daemon owner.
    ///
    /// Callers must authorize current access to this session before reading. Recorded evidence
    /// grants no native delivery authority; this method never returns `Some(New)`. None means
    /// no retained request, not permission to replay or bypass normal live-owner admission.
    /// # Errors
    /// Rejects auth/read-only requests, foreign sessions, conflicting content, incompatible or
    /// stale indexes and concurrent canonical changes. Never loads/replays history or repairs.
    pub async fn read_external_harness_retained_request(
        &self,
        session_id: bcode_session_models::SessionId,
        request: &HarnessRequest,
    ) -> Result<Option<ExternalHarnessClaim>, SessionError> {
        let retained = ExternalHarnessRequest::from_request(request)
            .map_err(SessionError::EventSerialization)?;
        if retained.target().0 != session_id.to_string() {
            return Err(mismatch());
        }
        let gate = self.session_load_gate(session_id).await;
        let _guard = gate.lock().await;
        let cached = self.inner.lock().await.sessions.get(&session_id).cloned();
        if let Some(handle) = cached {
            return handle.harness_retained_request(retained).await;
        }
        let Some(store) = self.store.as_ref() else {
            return Ok(None);
        };
        let root = store.root_path();
        if !crate::db::session_db_path(&root, session_id).exists() {
            return Ok(None);
        }
        let db = crate::db::SessionDb::open_existing_turso_in_root(session_id, &root).await?;
        Ok(db.external_harness_retained_request(&retained).await?)
    }
    /// Observe the current canonical managed-harness selection without acquiring ownership.
    /// Preserves prepared/confirmed distinction; historical readability proves no live transport.
    /// Returns None for an absent session/store or a session with no managed configuration.
    /// # Errors
    /// Rejects stale/missing indexes, incompatible storage and concurrently changed snapshots.
    /// Never invokes plugins, native processes, migrations, replay or repair.
    pub async fn session_external_harness_state(
        &self,
        session_id: bcode_session_models::SessionId,
    ) -> Result<Option<ExternalHarnessState>, SessionError> {
        let gate = self.session_load_gate(session_id).await;
        let _guard = gate.lock().await;
        let cached = self.inner.lock().await.sessions.get(&session_id).cloned();
        if let Some(handle) = cached {
            return handle.harness_snapshot().await;
        }
        let Some(store) = self.store.as_ref() else {
            return Ok(None);
        };
        let root = store.root_path();
        if !crate::db::session_db_path(&root, session_id).exists() {
            return Ok(None);
        }
        let db = crate::db::SessionDb::open_existing_turso_in_root(session_id, &root).await?;
        Ok(db.external_harness_snapshot().await?)
    }
    /// Bounded offline normalized native history. Cursor is canonical, never a native poll cursor.
    /// # Errors
    /// Rejects malformed cursors, excessive pages and stale/missing projections; never repairs.
    pub async fn external_harness_history(
        &self,
        session_id: bcode_session_models::SessionId,
        after: Option<String>,
        limit: u16,
    ) -> Result<HarnessObservationPage, SessionError> {
        if limit == 0 || usize::from(limit) > MAX_HARNESS_OBSERVATIONS {
            return Err(mismatch());
        }
        let gate = self.session_load_gate(session_id).await;
        let _guard = gate.lock().await;
        let cached = self.inner.lock().await.sessions.get(&session_id).cloned();
        if let Some(handle) = cached {
            return handle.harness_history(after, limit).await;
        }
        let store = self.store.as_ref().ok_or_else(mismatch)?;
        let db = crate::db::SessionDb::open_existing_turso_in_root(session_id, &store.root_path())
            .await?;
        Ok(db.harness_history(after, limit).await?)
    }
    /// Acquire a live canonical owner hold and compare complete daemon/artifact/generation identity.
    /// The returned guard must remain held across native I/O and completion.
    /// # Errors
    /// Rejects foreign leases, mismatched owners, stale generations and nonpersistent sessions.
    pub async fn acquire_external_harness_ownership(
        &self,
        owner: &ExternalHarnessOwner,
    ) -> Result<SessionOwnershipGuard, SessionError> {
        let handle = self.session_handle(owner.session_id).await?;
        let guard = handle
            .acquire_ownership(SessionOwnershipKind::RuntimeWork)
            .await?;
        handle
            .harness_operation(owner.clone(), A::State, self.next_activity_timestamp_ms())
            .await?;
        Ok(guard)
    }
    /// Admit immutable inspected instance/capabilities before native open. No native ID is invented.
    /// # Errors
    /// Rejects mismatched local owners and conflicting configurations.
    pub async fn configure_external_harness(
        &self,
        owner: &ExternalHarnessOwner,
        instance: HarnessInstance,
        capabilities: HarnessCapabilities,
    ) -> Result<(), SessionError> {
        match self
            .session_handle(owner.session_id)
            .await?
            .harness_operation(
                owner.clone(),
                A::Write(Box::new(ExternalHarnessOperationRecord::Configured {
                    instance,
                    capabilities,
                    open_request: None,
                })),
                self.next_activity_timestamp_ms(),
            )
            .await?
        {
            R::Committed => Ok(()),
            _ => Err(mismatch()),
        }
    }
    /// Read current configuration and latest committed binding under verified canonical ownership.
    /// # Errors
    /// Rejects missing configuration, foreign owners and degraded projections.
    pub async fn external_harness_state(
        &self,
        owner: &ExternalHarnessOwner,
    ) -> Result<ExternalHarnessState, SessionError> {
        match self
            .session_handle(owner.session_id)
            .await?
            .harness_operation(owner.clone(), A::State, self.next_activity_timestamp_ms())
            .await?
        {
            R::State(Some(state)) => Ok(*state),
            _ => Err(mismatch()),
        }
    }
    /// Atomically retain exact request content and delivery attempt before any native mutation.
    /// Only `New` permits delivery. `Uncertain` never permits automatic replay.
    /// # Errors
    /// Rejects authentication requests, conflicts, invalid owners and stale generations.
    pub async fn claim_external_harness_request(
        &self,
        owner: &ExternalHarnessOwner,
        request: &HarnessRequest,
    ) -> Result<ExternalHarnessClaim, SessionError> {
        let request = ExternalHarnessRequest::from_request(request)
            .map_err(SessionError::EventSerialization)?;
        match self
            .session_handle(owner.session_id)
            .await?
            .harness_operation(
                owner.clone(),
                A::Write(Box::new(ExternalHarnessOperationRecord::DeliveryClaimed {
                    request,
                })),
                self.next_activity_timestamp_ms(),
            )
            .await?
        {
            R::Claim(claim) => Ok(claim),
            _ => Err(mismatch()),
        }
    }
    /// Persist explicit delivery uncertainty without permitting native mutation replay.
    /// # Errors
    /// Rejects unclaimed requests, mismatched owners and conflicting recorded outcomes.
    pub async fn mark_external_harness_uncertain(
        &self,
        owner: &ExternalHarnessOwner,
        request: &HarnessRequest,
    ) -> Result<(), SessionError> {
        let response = HarnessServiceResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            operation: request.kind.operation(),
            kind: HarnessServiceResponseKind::Failure(HarnessFailure::Uncertain),
        };
        self.complete_external_harness_request(owner, request, &response)
            .await
    }
    /// Retain exact acknowledged result; never implies native settlement from a control ack.
    /// # Errors
    /// Rejects auth results, mismatched requests, conflicting retries and foreign owners.
    pub async fn complete_external_harness_request(
        &self,
        owner: &ExternalHarnessOwner,
        request: &HarnessRequest,
        response: &HarnessServiceResponse,
    ) -> Result<(), SessionError> {
        let result = ExternalHarnessResponse::from_response(request, response)
            .map_err(SessionError::EventSerialization)?;
        let request = ExternalHarnessRequest::from_request(request)
            .map_err(SessionError::EventSerialization)?;
        match self
            .session_handle(owner.session_id)
            .await?
            .harness_operation(
                owner.clone(),
                A::Write(Box::new(
                    ExternalHarnessOperationRecord::DeliveryCompleted {
                        request,
                        response: result,
                    },
                )),
                self.next_activity_timestamp_ms(),
            )
            .await?
        {
            R::Committed => Ok(()),
            _ => Err(mismatch()),
        }
    }
    /// Commit the normalized native page and its exact acknowledged predecessor cursor atomically.
    /// # Errors
    /// Rejects cursor jumps, conflicting observation identities and stale owner generations.
    pub async fn commit_external_harness_page(
        &self,
        owner: &ExternalHarnessOwner,
        after: Option<&str>,
        page: &HarnessObservationPage,
    ) -> Result<(), SessionError> {
        match self
            .session_handle(owner.session_id)
            .await?
            .harness_operation(
                owner.clone(),
                A::Page {
                    after: after.map(str::to_owned),
                    page: page.clone(),
                },
                self.next_activity_timestamp_ms(),
            )
            .await?
        {
            R::Committed => Ok(()),
            _ => Err(mismatch()),
        }
    }
    /// Exact indexed latest receipt by retained submission and attempt identities.
    /// # Errors
    /// Rejects invalid owners and degraded projections; never scans history.
    pub async fn external_harness_receipt(
        &self,
        owner: &ExternalHarnessOwner,
        submission_id: &str,
        attempt_id: &str,
    ) -> Result<Option<HarnessReceipt>, SessionError> {
        if [submission_id, attempt_id]
            .iter()
            .any(|v| v.is_empty() || v.len() > MAX_HARNESS_ID_BYTES)
        {
            return Err(mismatch());
        }
        match self
            .session_handle(owner.session_id)
            .await?
            .harness_operation(
                owner.clone(),
                A::Receipt {
                    submission_id: submission_id.into(),
                    attempt_id: attempt_id.into(),
                },
                self.next_activity_timestamp_ms(),
            )
            .await?
        {
            R::Receipt(receipt) => Ok(receipt),
            _ => Err(mismatch()),
        }
    }
    /// Exact indexed unresolved interaction; parentage grants no response authority.
    /// # Errors
    /// Rejects invalid owners and degraded projections; answered interactions return None.
    pub async fn external_harness_interaction(
        &self,
        owner: &ExternalHarnessOwner,
        request_id: &str,
    ) -> Result<Option<HarnessInteraction>, SessionError> {
        if request_id.is_empty() || request_id.len() > MAX_HARNESS_ID_BYTES {
            return Err(mismatch());
        }
        match self
            .session_handle(owner.session_id)
            .await?
            .harness_operation(
                owner.clone(),
                A::Interaction {
                    request_id: request_id.into(),
                },
                self.next_activity_timestamp_ms(),
            )
            .await?
        {
            R::Interaction(interaction) => Ok(interaction),
            _ => Err(mismatch()),
        }
    }
}
