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
    /// Retain a physically reaped native transport under its exact current owner.
    /// The trusted native integration must supply actual reap evidence; an acknowledgement
    /// or caller-provided boolean is not evidence. This never dispatches native operations.
    /// # Errors
    /// Rejects unpublished schema support, stale owners, missing Close completion or changed binding.
    pub async fn release_external_harness_transport<F>(
        &self,
        owner: &ExternalHarnessOwner,
        release: bcode_session_models::external_harness::ExternalHarnessTransportRelease,
        verify_reap: F,
    ) -> Result<(), SessionError>
    where
        F: FnOnce(
                &bcode_session_models::external_harness::ExternalHarnessTransportRelease,
            ) -> Result<(), String>
            + Send
            + 'static,
    {
        if !bcode_session_models::external_harness::recovery_storage_available() {
            return Err(mismatch());
        }
        release
            .validate_for(owner)
            .map_err(SessionError::EventSerialization)?;
        match self
            .session_handle(owner.session_id)
            .await?
            .harness_operation(
                owner.clone(),
                A::ReleaseTransport {
                    release: Box::new(release),
                    verify_reap: Box::new(verify_reap),
                },
                self.next_activity_timestamp_ms(),
            )
            .await?
        {
            R::Committed => Ok(()),
            _ => Err(mismatch()),
        }
    }

    /// Atomically fence an explicitly authorized recovery intent to the next generation.
    /// The host verifier must check exact retained lifetime provenance is released and the
    /// native transport's reap evidence is valid. It runs inside the writer mailbox before
    /// the transactional CAS. This API neither resumes native work nor replays prior input.
    /// # Errors
    /// Rejects unpublished schema support, failed host proof, stale CAS and conflicting intent.
    pub async fn prepare_external_harness_recovery<F>(
        &self,
        owner: &ExternalHarnessOwner,
        recovery: bcode_session_models::external_harness::ExternalHarnessRecovery,
        verify_release: F,
    ) -> Result<(), SessionError>
    where
        F: FnOnce(
                &bcode_session_models::external_harness::ExternalHarnessTransportRelease,
            ) -> Result<(), String>
            + Send
            + 'static,
    {
        if !bcode_session_models::external_harness::recovery_storage_available() {
            return Err(mismatch());
        }
        recovery
            .validate_for(owner)
            .map_err(SessionError::EventSerialization)?;
        match self
            .session_handle(owner.session_id)
            .await?
            .harness_operation(
                owner.clone(),
                A::Recover {
                    recovery: Box::new(recovery),
                    verify_release: Box::new(verify_release),
                },
                self.next_activity_timestamp_ms(),
            )
            .await?
        {
            R::Committed => Ok(()),
            _ => Err(mismatch()),
        }
    }

    /// Inspect retained transport-release provenance without acquiring native authority.
    /// Current application access authorization is required separately. No lifetime proof,
    /// native process creation, mutation replay, repair or history scan is performed.
    /// # Errors
    /// Rejects stale owners, damaged locators, missing storage and concurrent changes.
    pub async fn session_external_harness_transport_release(
        &self,
        owner: &ExternalHarnessOwner,
    ) -> Result<
        Option<(
            u64,
            bcode_session_models::external_harness::ExternalHarnessTransportRelease,
        )>,
        SessionError,
    > {
        owner.validate().map_err(SessionError::EventSerialization)?;
        let gate = self.session_load_gate(owner.session_id).await;
        let _guard = gate.lock().await;
        let store = self.store.as_ref().ok_or_else(mismatch)?;
        let db =
            crate::db::SessionDb::open_existing_turso_in_root(owner.session_id, &store.root_path())
                .await?;
        Ok(db.harness_transport_release(owner).await?)
    }

    /// The canonical tail an abandoned transport's recovery would follow, when the exact
    /// current owner never released it. No lifetime proof, native effect or history scan.
    /// # Errors
    /// Rejects stale owners, damaged locators, missing storage and concurrent changes.
    pub async fn session_external_harness_abandonment_point(
        &self,
        owner: &ExternalHarnessOwner,
    ) -> Result<Option<u64>, SessionError> {
        owner.validate().map_err(SessionError::EventSerialization)?;
        let gate = self.session_load_gate(owner.session_id).await;
        let _guard = gate.lock().await;
        let store = self.store.as_ref().ok_or_else(mismatch)?;
        let db =
            crate::db::SessionDb::open_existing_turso_in_root(owner.session_id, &store.root_path())
                .await?;
        Ok(db.harness_abandonment_point(owner).await?)
    }

    /// Read one bounded, revision-pinned lifecycle inventory page without native execution.
    /// The caller supplies the exact inspected owner; no ownership or completion is inferred.
    /// # Errors
    /// Rejects foreign owners, missing proofs, malformed cursors and unavailable session stores.
    pub async fn session_harness_inventory_page(
        &self,
        owner: &ExternalHarnessOwner,
        scope: &bcode_session_models::HarnessInventoryScope,
        kind: bcode_session_models::HarnessInventoryKind,
        cursor: Option<&bcode_session_models::HarnessInventoryCursor>,
    ) -> Result<bcode_session_models::HarnessInventoryPage, SessionError> {
        let gate = self.session_load_gate(owner.session_id).await;
        let _guard = gate.lock().await;
        let store = self.store.as_ref().ok_or_else(mismatch)?;
        let db =
            crate::db::SessionDb::open_existing_turso_in_root(owner.session_id, &store.root_path())
                .await?;
        Ok(db
            .harness_inventory_page(owner, scope, kind, cursor)
            .await?)
    }

    /// Read canonical lifecycle summary truth without collecting an inventory.
    /// # Errors
    /// Rejects foreign owners, missing stores, damaged accessed proofs and concurrent changes.
    pub async fn session_harness_inventory_summary(
        &self,
        owner: &ExternalHarnessOwner,
        scope: &bcode_session_models::HarnessInventoryScope,
        kind: bcode_session_models::HarnessInventoryKind,
    ) -> Result<bcode_session_models::external_harness::HarnessInventorySummaryRead, SessionError>
    {
        let gate = self.session_load_gate(owner.session_id).await;
        let _guard = gate.lock().await;
        let store = self.store.as_ref().ok_or_else(mismatch)?;
        let db =
            crate::db::SessionDb::open_existing_turso_in_root(owner.session_id, &store.root_path())
                .await?;
        Ok(db.harness_inventory_summary(owner, scope, kind).await?)
    }

    /// Read exact previous-page acknowledgment and bounded canonical hydration for a target.
    /// None means retention metadata is unknown, not that native effects are absent.
    /// # Errors
    /// Rejects foreign/stale targets, missing canonical pages, mismatched cursors,
    /// damaged pages and concurrent changes.
    pub async fn session_harness_retention_context(
        &self,
        owner: &ExternalHarnessOwner,
        target: &bcode_harness_models::HarnessSessionTarget,
        after: Option<&str>,
    ) -> Result<Option<bcode_harness_models::HarnessRetentionContext>, SessionError> {
        let gate = self.session_load_gate(owner.session_id).await;
        let _guard = gate.lock().await;
        let store = self.store.as_ref().ok_or_else(mismatch)?;
        let db =
            crate::db::SessionDb::open_existing_turso_in_root(owner.session_id, &store.root_path())
                .await?;
        Ok(db.harness_retention_context(owner, target, after).await?)
    }

    /// Hydrate a bounded native-reference batch from exact canonical locators, never a scan.
    /// # Errors
    /// Rejects malformed/foreign requests, missing stores and concurrent canonical changes.
    pub async fn session_harness_native_hydration(
        &self,
        owner: &ExternalHarnessOwner,
        requests: &[bcode_harness_models::HarnessHydrationRequest],
    ) -> Result<Vec<bcode_harness_models::HarnessHydrationReply>, SessionError> {
        let gate = self.session_load_gate(owner.session_id).await;
        let _guard = gate.lock().await;
        let store = self.store.as_ref().ok_or_else(mismatch)?;
        let db =
            crate::db::SessionDb::open_existing_turso_in_root(owner.session_id, &store.root_path())
                .await?;
        Ok(db.harness_native_hydration(owner, requests).await?)
    }

    /// Recover an exact canonical page acknowledgment; not an execution authorization.
    /// # Errors
    /// Rejects foreign owner/page, digest mismatches and missing canonical recording.
    pub async fn session_harness_canonical_page_ack(
        &self,
        owner: &ExternalHarnessOwner,
        page: &bcode_harness_models::HarnessRetentionPage,
    ) -> Result<bcode_harness_models::HarnessCanonicalAck, SessionError> {
        let gate = self.session_load_gate(owner.session_id).await;
        let _guard = gate.lock().await;
        let store = self.store.as_ref().ok_or_else(mismatch)?;
        let db =
            crate::db::SessionDb::open_existing_turso_in_root(owner.session_id, &store.root_path())
                .await?;
        Ok(db.harness_canonical_page_ack(owner, page).await?)
    }

    /// Resolve a stable lifecycle identity at an explicit owner/scope/kind commitment.
    /// Returns retained evidence only; acknowledgement and adapter hydration are separate.
    /// # Errors
    /// Rejects unavailable stores, foreign identities and missing or damaged proof state.
    pub async fn session_harness_inventory_item(
        &self,
        owner: &ExternalHarnessOwner,
        scope: &bcode_session_models::HarnessInventoryScope,
        kind: bcode_session_models::HarnessInventoryKind,
        identity: &str,
        commitment: &bcode_session_models::HarnessInventoryCommitment,
    ) -> Result<bcode_session_models::HarnessInventoryItemRead, SessionError> {
        let gate = self.session_load_gate(owner.session_id).await;
        let _guard = gate.lock().await;
        let store = self.store.as_ref().ok_or_else(mismatch)?;
        let db =
            crate::db::SessionDb::open_existing_turso_in_root(owner.session_id, &store.root_path())
                .await?;
        Ok(db
            .harness_inventory_item(owner, scope, kind, identity, commitment)
            .await?)
    }

    /// Read bounded indexed legacy and v3 execution evidence under one canonical snapshot.
    /// The tuple includes current owner. V3 retains the latest lifecycle entry for each exact
    /// attempt identity plus uncorrelated session-wide work/continuations, independently of
    /// history-prefix length. Overflow and missing coverage fail closed; reads never reindex.
    /// Legacy acceptance/result/native-terminal/settlement remains the strict fallback.
    /// Evidence completeness is not successful disposition, work settlement, or workflow policy.
    /// # Errors
    /// Rejects stale/missing indexes, foreign/conflicting evidence and concurrent changes.
    pub async fn session_external_harness_submission_evidence(
        &self,
        session_id: bcode_session_models::SessionId,
        submission_id: &str,
        attempt_id: &str,
    ) -> Result<Option<(ExternalHarnessOwner, HarnessObservationPage)>, SessionError> {
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
                .harness_submission_evidence(submission_id.into(), attempt_id.into())
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
            .external_harness_submission_evidence(submission_id, attempt_id)
            .await?)
    }

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
        self.read_external_harness_retained_envelope(session_id, retained)
            .await
    }

    /// Read the exact selected-open claim/result without reconstructing legacy Open.
    /// This is historical evidence, never delivery or ownership authority.
    /// # Errors
    /// Rejects conflicting envelopes, unavailable fences, damaged indexes and wrong sessions.
    pub async fn read_external_harness_retained_selected_request(
        &self,
        session_id: bcode_session_models::SessionId,
        request: &bcode_harness_models::HarnessModelsRequest,
    ) -> Result<Option<ExternalHarnessClaim>, SessionError> {
        let retained = ExternalHarnessRequest::from_models_request(request)
            .map_err(SessionError::EventSerialization)?;
        self.read_external_harness_retained_envelope(session_id, retained)
            .await
    }

    /// Read canonical selected-open receipt locators without adopting native ownership.
    /// # Errors
    /// Rejects unavailable storage, conflicting requests and damaged canonical projections.
    pub async fn read_external_harness_selected_open_receipt(
        &self,
        session_id: bcode_session_models::SessionId,
        request: &bcode_harness_models::HarnessModelsRequest,
    ) -> Result<Option<bcode_session_models::ManagedSessionSpawnOpenReceipt>, SessionError> {
        let retained = ExternalHarnessRequest::from_models_request(request)
            .map_err(SessionError::EventSerialization)?;
        if retained.target().0 != session_id.to_string() {
            return Err(mismatch());
        }
        let gate = self.session_load_gate(session_id).await;
        let _guard = gate.lock().await;
        let Some(store) = &self.store else {
            return Ok(None);
        };
        let root = store.root_path();
        if !crate::db::session_db_path(&root, session_id).exists() {
            return Ok(None);
        }
        let db = crate::db::SessionDb::open_existing_turso_in_root(session_id, &root).await?;
        let result = db.external_harness_selected_open_receipt(&retained).await;
        let close = db.close().await;
        let result = result?;
        close?;
        Ok(result)
    }

    async fn read_external_harness_retained_envelope(
        &self,
        session_id: bcode_session_models::SessionId,
        retained: ExternalHarnessRequest,
    ) -> Result<Option<ExternalHarnessClaim>, SessionError> {
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
        let result = db.external_harness_retained_request(&retained).await;
        let close = db.close().await;
        let result = result?;
        close?;
        Ok(result)
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
            let result = handle.harness_snapshot().await;
            return result;
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
            let result = handle.harness_history(after, limit).await;
            return result;
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
                    capabilities: Box::new(capabilities),
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
    /// Claim an exact selected-open request before native delivery.
    /// Only `New` grants delivery; retained uncertainty must never be redispatched.
    /// # Errors
    /// Rejects conflicting envelopes, unavailable storage fences and stale owners.
    pub async fn claim_external_harness_selected_open(
        &self,
        owner: &ExternalHarnessOwner,
        request: &bcode_harness_models::HarnessModelsRequest,
    ) -> Result<ExternalHarnessClaim, SessionError> {
        let request = ExternalHarnessRequest::from_models_request(request)
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

    /// Complete selected open with exact model evidence or explicit failure/uncertainty.
    /// # Errors
    /// Rejects unclaimed, mismatched, conflicting or stale-owner results.
    pub async fn complete_external_harness_selected_open(
        &self,
        owner: &ExternalHarnessOwner,
        request: &bcode_harness_models::HarnessModelsRequest,
        response: &bcode_harness_models::HarnessModelsResponse,
    ) -> Result<(), SessionError> {
        let result = ExternalHarnessResponse::from_models_response(request, response)
            .map_err(SessionError::EventSerialization)?;
        let request = ExternalHarnessRequest::from_models_request(request)
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
                    page: Box::new(page.clone()),
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
