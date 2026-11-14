//! Native session application. It is deliberately separate from model-turn scheduling.

use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessControlRequest, HarnessFailure, HarnessInspection,
    HarnessObservationPage, HarnessObserveRequest, HarnessOpenRequest, HarnessOperation,
    HarnessReconcileRequest, HarnessReconciliation, HarnessRequest, HarnessRequestKind,
    HarnessResponse, HarnessServiceResponse, HarnessServiceResponseKind, HarnessSessionSetting,
    HarnessSessionTarget, HarnessSubmission, HarnessSupportLevel, MAX_HARNESS_ID_BYTES,
    MAX_HARNESS_OBSERVATIONS,
};
use bcode_session_models::SessionId;

use super::{
    ManagedHarnessError, policy,
    store::{DeliveryClaim, HarnessOwner, ManagedHarnessStore, ManagedSession},
    transport::{HarnessPluginTransport, ManagedHarnessTransport},
};

/// Preserve normalized native refusal semantics on the read-only observation path.
///
/// A lost native process is not evidence that an enabled plugin disappeared, nor permission
/// to reconstruct a prepared transport. Mutation failure handling remains uncertainty-fenced.
const fn observation_failure(failure: HarnessFailure) -> ManagedHarnessError {
    match failure {
        HarnessFailure::Unsupported | HarnessFailure::Authentication | HarnessFailure::Policy => {
            ManagedHarnessError::Unsupported
        }
        HarnessFailure::InvalidRequest => ManagedHarnessError::Invalid,
        HarnessFailure::Unavailable => ManagedHarnessError::Unavailable,
        HarnessFailure::StaleGeneration => ManagedHarnessError::StaleOwner,
        HarnessFailure::Conflict => ManagedHarnessError::Conflict,
        HarnessFailure::Uncertain => ManagedHarnessError::Uncertain,
    }
}

/// One application boundary shared by interactive, delegated and workflow callers.
///
/// Construct only after ordinary caller admission; the store independently verifies durable owner.
pub struct ManagedHarnessRuntime<S, T = HarnessPluginTransport> {
    store: S,
    transport: T,
    activity: super::activity::NativeActivity,
    approvals: super::native_policy::NativeApprovals,
    /// Status of the last page committed per session, so an idle poll that changes nothing
    /// is not written to history.
    committed_status: std::sync::Mutex<
        std::collections::BTreeMap<SessionId, bcode_harness_models::HarnessExecutionStatus>,
    >,
}

impl<S: ManagedHarnessStore, T: ManagedHarnessTransport> ManagedHarnessRuntime<S, T> {
    /// Bind canonical persistence and selected-plugin transport. No native process is started.
    #[must_use]
    pub const fn new(store: S, transport: T) -> Self {
        Self {
            store,
            transport,
            activity: super::activity::NativeActivity::new(),
            approvals: super::native_policy::NativeApprovals::new(),
            committed_status: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Managed sessions with live native foreground or unsettled background work, derived
    /// from committed canonical evidence. Idle shutdown must not stop such a session.
    #[must_use]
    pub fn busy_native_sessions(&self) -> usize {
        self.activity.busy_sessions()
    }

    /// Native approvals committed for `session` and not yet put to its agent's policy.
    pub fn take_native_approvals(
        &self,
        session: SessionId,
    ) -> Vec<bcode_harness_models::HarnessInteraction> {
        self.approvals.take(session)
    }

    /// Whether a native foreground turn is running for `session`.
    pub fn native_turn_running(&self, session: SessionId) -> bool {
        self.activity.foreground(session)
    }

    /// Verify an admitted full owner while retaining a real canonical lease for the check.
    /// Every operation reacquires and holds that lease through native I/O and commit.
    /// # Errors
    /// Rejects stale/foreign owner identity and unavailable canonical selection.
    pub async fn verify_owner(&self, owner: &HarnessOwner) -> Result<(), ManagedHarnessError> {
        let _ownership = self.store.acquire(owner).await?;
        self.store.load(owner).await.map(|_| ())
    }

    async fn request(
        &self,
        owner: &HarnessOwner,
        request_id: String,
        kind: HarnessRequestKind,
    ) -> Result<(ManagedSession, HarnessRequest), ManagedHarnessError> {
        let session = self.store.load(owner).await?;
        session
            .instance
            .validate()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        if owner.generation == 0
            || [&owner.artifact_id, &owner.daemon_id].iter().any(|id| {
                id.is_empty() || id.len() > MAX_HARNESS_ID_BYTES || id.chars().any(char::is_control)
            })
        {
            return Err(ManagedHarnessError::StaleOwner);
        }
        if let Some(target) = &session.target {
            target
                .validate_owner(&owner.session_id.to_string(), owner.generation)
                .map_err(|_| ManagedHarnessError::StaleOwner)?;
        }
        if let Some(binding) = &session.binding {
            binding
                .validate_owner(&owner.session_id.to_string(), owner.generation)
                .map_err(|_| ManagedHarnessError::StaleOwner)?;
        }
        // Recheck the binding after assembling an operation. No caller may race a changed
        // association and deliver against the stale native identity from its earlier load.
        let supplied_target = match &kind {
            HarnessRequestKind::Submit { target, .. }
            | HarnessRequestKind::Respond { target, .. } => Some(target),
            HarnessRequestKind::Observe(value) => Some(&value.target),
            HarnessRequestKind::Interrupt(value) | HarnessRequestKind::Close(value) => {
                Some(&value.target)
            }
            HarnessRequestKind::Configure(value) => Some(&value.target),
            _ => None,
        };
        if supplied_target.is_some_and(|target| session.target.as_ref() != Some(target)) {
            return Err(ManagedHarnessError::Conflict);
        }
        let request = HarnessRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id,
            instance: session.instance.clone(),
            kind,
        };
        request
            .validate()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        Ok((session, request))
    }

    async fn native(
        &self,
        request: &HarnessRequest,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        let response = self.transport.invoke(request).await?;
        response
            .validate_for(request)
            .map_err(|error| {
                tracing::warn!(target: "bcode_server::harness", stage = "native_response_contract", operation = request.kind.operation().as_str(), category = ?error, "managed native response rejected");
                ManagedHarnessError::Invalid
            })?;
        Ok(response)
    }

    async fn mutate(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        let assistance = self.store.assistance_guard(owner, request).await?;
        self.mutate_guarded(owner, request, assistance).await
    }

    async fn mutate_guarded(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
        _assistance: Box<dyn Send + Sync>,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        match self.store.claim(owner, request).await? {
            DeliveryClaim::Recorded(response) => {
                response
                    .validate_for(request)
                    .map_err(|_| ManagedHarnessError::Conflict)?;
                return Ok(*response);
            }
            DeliveryClaim::Uncertain => return Err(ManagedHarnessError::Uncertain),
            DeliveryClaim::New => {}
        }
        // A cancelled/dropped future still leaves the durable claim. It never grants retry.
        let Ok(response) = self.native(request).await else {
            let _ = self.store.uncertain(owner, request).await;
            return Err(ManagedHarnessError::Uncertain);
        };
        if let (
            HarnessRequestKind::Submit { submission, .. },
            HarnessServiceResponseKind::Receipt(receipt),
        ) = (&request.kind, &response.kind)
            && policy::receipt(submission, receipt).is_err()
        {
            let _ = self.store.uncertain(owner, request).await;
            return Err(ManagedHarnessError::Uncertain);
        }
        if self.store.commit(owner, request, &response).await.is_err() {
            let _ = self.store.uncertain(owner, request).await;
            return Err(ManagedHarnessError::Uncertain);
        }
        match (&request.kind, &response.kind) {
            (HarnessRequestKind::Submit { .. }, HarnessServiceResponseKind::Receipt(_)) => {
                self.activity.submitted(owner.session_id);
            }
            (HarnessRequestKind::Close(_), HarnessServiceResponseKind::Acknowledged { .. }) => {
                self.activity.closed(owner.session_id);
            }
            _ => {}
        }
        Ok(response)
    }

    /// Inspect selected executable/config provenance under verified ownership without starting a turn.
    /// # Errors
    /// Rejects unavailable plugins, wrong namespace, malformed capabilities and stale ownership.
    pub async fn inspect(
        &self,
        owner: &HarnessOwner,
        request_id: String,
    ) -> Result<HarnessInspection, ManagedHarnessError> {
        let _ownership = self.store.acquire(owner).await?;
        let (_, request) = self
            .request(owner, request_id, HarnessRequestKind::Inspect)
            .await?;
        let response = self.native(&request).await?;
        let HarnessServiceResponseKind::Inspection(inspection) = response.kind else {
            return Err(ManagedHarnessError::Unavailable);
        };
        Ok(inspection)
    }

    /// Deliver an exact selected Open under a selected canonical claim, never a legacy claim.
    /// # Errors
    /// Rejects invalid ownership, unsupported persistence, conflicts and ambiguous delivery.
    pub async fn open_selected(
        &self,
        owner: &HarnessOwner,
        request: &bcode_harness_models::HarnessModelsRequest,
    ) -> Result<bcode_harness_models::HarnessModelsResponse, ManagedHarnessError> {
        use super::store::SelectedDeliveryClaim;
        use bcode_harness_models::{HarnessModelsRequestKind, HarnessModelsResponseKind};
        request
            .validate()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        let HarnessModelsRequestKind::OpenSelected { open, .. } = &request.kind else {
            return Err(ManagedHarnessError::Invalid);
        };
        if open.resume.is_some() || open.parent_assistance.is_some() {
            return Err(ManagedHarnessError::Unsupported);
        }
        if open.session_id != owner.session_id.to_string() || open.generation != owner.generation {
            return Err(ManagedHarnessError::StaleOwner);
        }
        let _ownership = self.store.acquire(owner).await?;
        let selected = self.store.load(owner).await?;
        if selected.instance != request.instance {
            return Err(ManagedHarnessError::Conflict);
        }
        match self.store.claim_selected(owner, request).await? {
            SelectedDeliveryClaim::Recorded(response) => {
                response
                    .validate_for(request)
                    .map_err(|_| ManagedHarnessError::Conflict)?;
                return Ok(*response);
            }
            SelectedDeliveryClaim::Uncertain => return Err(ManagedHarnessError::Uncertain),
            SelectedDeliveryClaim::New => {}
        }
        // Only the exact selected claim above authorizes this extension. Cancellation or a
        // dropped future leaves the claim non-replayable even if uncertainty append fails.
        let response = self.transport.invoke_selected(request).await;
        let response = match response {
            Ok(response)
                if response.validate_for(request).is_ok()
                    && matches!(
                        &response.kind,
                        HarnessModelsResponseKind::Selected { .. }
                            | HarnessModelsResponseKind::Failure(_)
                    ) =>
            {
                response
            }
            _ => {
                let _ = self.store.uncertain_selected(owner, request).await;
                return Err(ManagedHarnessError::Uncertain);
            }
        };
        if self
            .store
            .commit_selected(owner, request, &response)
            .await
            .is_err()
        {
            let _ = self.store.uncertain_selected(owner, request).await;
            return Err(ManagedHarnessError::Uncertain);
        }
        Ok(response)
    }

    /// Open native execution for an already-created ordinary top-level or child Bcode session.
    /// Caller creation must use the existing session application and immutable parent contract.
    /// # Errors
    /// Rejects foreign generations, conflicting open IDs and ambiguous prior creation delivery.
    pub async fn open(
        &self,
        owner: &HarnessOwner,
        open: HarnessOpenRequest,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        self.open_admitted(owner, open, false).await
    }

    // Only explicit application recovery may use a per-intent native constraint receipt.
    // The canonical store requires this exact CAS-published prepared_open; this is not
    // a blanket promotion of Constrained resume capability for ordinary Open calls.
    pub(crate) async fn open_validated_recovery(
        &self,
        owner: &HarnessOwner,
        open: HarnessOpenRequest,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        if open.resume.is_none() || !crate::harness::recovery_published() {
            return Err(ManagedHarnessError::Unsupported);
        }
        self.open_admitted(owner, open, true).await
    }

    async fn open_admitted(
        &self,
        owner: &HarnessOwner,
        open: HarnessOpenRequest,
        constraints_validated: bool,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        let _ownership = self.store.acquire(owner).await?;
        if open.session_id != owner.session_id.to_string() || open.generation != owner.generation {
            return Err(ManagedHarnessError::StaleOwner);
        }
        let (session, request) = self
            .request(owner, open.open_id.clone(), HarnessRequestKind::Open(open))
            .await?;
        let HarnessRequestKind::Open(open) = &request.kind else {
            return Err(ManagedHarnessError::Invalid);
        };
        if let Some(resume) = &open.resume
            && ((!constraints_validated
                && session.target.as_ref()
                    != Some(&HarnessSessionTarget::Confirmed(resume.clone())))
                || (constraints_validated && session.target.is_some())
                || session.binding.as_ref() != Some(resume))
        {
            return Err(ManagedHarnessError::Conflict);
        }
        if open.resume.is_some()
            && session.capabilities.resume.support != HarnessSupportLevel::Supported
            && !(constraints_validated
                && session.capabilities.resume.support == HarnessSupportLevel::Constrained)
        {
            return Err(ManagedHarnessError::Unsupported);
        }
        self.mutate(owner, &request).await
    }

    /// Submit retained text or steer an exact native turn; never enters Bcode's model loop.
    /// # Errors
    /// Rejects stale ownership, unsupported steering, conflicting IDs or uncertain delivery.
    pub async fn submit(
        &self,
        owner: &HarnessOwner,
        submission: HarnessSubmission,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        let _ownership = self.store.acquire(owner).await?;
        let session = self.store.load(owner).await?;
        let target = session.target.ok_or(ManagedHarnessError::Conflict)?;
        policy::submission(&target, &session.capabilities, &submission)?;
        let (_, request) = self
            .request(
                owner,
                submission.submission_id.clone(),
                HarnessRequestKind::Submit { target, submission },
            )
            .await?;
        self.mutate(owner, &request).await
    }

    /// Respond to the exact outstanding question, approval or assistance request.
    /// Parentage is not admission; store claim atomically consumes the interaction.
    /// # Errors
    /// Rejects stale/resolved requests, wrong response types and uncertain prior delivery.
    pub async fn respond(
        &self,
        owner: &HarnessOwner,
        response: HarnessResponse,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        let _ownership = self.store.acquire(owner).await?;
        let session = self.store.load(owner).await?;
        let target = session.target.ok_or(ManagedHarnessError::Conflict)?;
        let (_, request) = self
            .request(
                owner,
                response.response_id.clone(),
                HarnessRequestKind::Respond {
                    target: target.clone(),
                    response,
                },
            )
            .await?;
        let HarnessRequestKind::Respond { response, .. } = &request.kind else {
            return Err(ManagedHarnessError::Invalid);
        };
        let interaction = match self.store.interaction(owner, &response.request_id).await {
            Ok(interaction) => interaction,
            Err(ManagedHarnessError::Conflict) => {
                // A response claim consumes the pending interaction, including when native
                // delivery or its acknowledgement is uncertain. Compare the exact retained
                // request before interpreting its absence as a conflicting new response.
                // The actor checks existing claims BEFORE new-claim interaction validation.
                // This branch never dispatches, even if a noncanonical store returns New.
                return match self.store.claim(owner, &request).await? {
                    DeliveryClaim::Recorded(recorded) => {
                        recorded
                            .validate_for(&request)
                            .map_err(|_| ManagedHarnessError::Conflict)?;
                        Ok(*recorded)
                    }
                    DeliveryClaim::Uncertain | DeliveryClaim::New => {
                        Err(ManagedHarnessError::Uncertain)
                    }
                };
            }
            Err(error) => return Err(error),
        };
        policy::response(&target, &interaction, response)?;
        self.mutate(owner, &request).await
    }

    /// Observe one bounded live page, persisting normalized observations before returning them.
    /// Native tools/subagents are inert evidence; idle/results never complete an assignment.
    /// # Errors
    /// Rejects gaps in authority, malformed pages, capability violations or unavailable plugins.
    pub async fn observe(
        &self,
        owner: &HarnessOwner,
        request_id: String,
        after: Option<String>,
        limit: u16,
        wait_millis: u32,
    ) -> Result<HarnessObservationPage, ManagedHarnessError> {
        let _ownership = self.store.acquire(owner).await?;
        let session = self.store.load(owner).await?;
        let target = session
            .target
            .clone()
            .ok_or(ManagedHarnessError::Conflict)?;
        let previous_after = after.clone();
        let retention = self
            .store
            .retention_context(owner, &target, previous_after.as_deref())
            .await?;
        let (_, request) = self
            .request(
                owner,
                request_id,
                HarnessRequestKind::Observe(HarnessObserveRequest {
                    target: target.clone(),
                    retention,
                    after,
                    limit,
                    wait_millis,
                }),
            )
            .await?;
        let response = self.native(&request).await.inspect_err(|error| {
            tracing::warn!(target: "bcode_server::harness", stage = "observe_native", category = %error, "managed observation stage failed");
        })?;
        if let HarnessServiceResponseKind::Failure(failure) = &response.kind {
            tracing::warn!(target: "bcode_server::harness", stage = "observe_native_failure", category = ?failure, "managed observation stage failed");
            return Err(observation_failure(*failure));
        }
        let HarnessServiceResponseKind::Observations(mut page) = response.kind else {
            tracing::warn!(target: "bcode_server::harness", stage = "observe_response_family", "managed observation stage failed");
            return Err(ManagedHarnessError::Unavailable);
        };
        for observation in &page.observations {
            policy::observation(&target, &session.capabilities, observation).inspect_err(|error| {
                tracing::warn!(target: "bcode_server::harness", stage = "observe_policy", category = %error, "managed observation stage failed");
            })?;
        }
        // The live sidecar is transient presentation: it is never persisted and is not part of
        // the retention digest, so strip it before canonical commit. It is reattached only to
        // the in-memory result, where daemon ingestion publishes it as session live events.
        let live = std::mem::take(&mut page.live);
        if self.unchanged_idle_page(owner.session_id, &page) {
            // Nothing to record: the canonical cursor stays where it was, and so does the
            // caller's.
            page.next_after.clone_from(&previous_after);
        } else {
            self.store
                .observation_page(owner, previous_after.as_deref(), &page)
                .await?;
            if let Ok(mut committed) = self.committed_status.lock() {
                committed.insert(owner.session_id, page.status.clone());
            }
        }
        page.live = live;
        self.activity
            .observed(owner.session_id, &session.capabilities, &page.observations);
        self.approvals
            .observed(owner.session_id, &page.observations);
        if !page.live.is_empty() {
            self.store.publish_live(owner, &page).await;
        }
        Ok(page)
    }

    /// Whether `page` carries nothing durable and reports the status last committed: an idle
    /// poll result, which history does not need.
    fn unchanged_idle_page(
        &self,
        session: SessionId,
        page: &bcode_harness_models::HarnessObservationPage,
    ) -> bool {
        page.observations.is_empty()
            && page.hydration_requests.is_empty()
            && page.retention.is_none()
            && page.inventory_page.is_none()
            && self
                .committed_status
                .lock()
                .is_ok_and(|committed| committed.get(&session) == Some(&page.status))
    }

    /// Durably request native interruption; acknowledgement does not prove settlement/no effects.
    ///
    /// Native active-versus-settled control semantics belong to the selected plugin. Results and
    /// idle hints never justify a host-generated acknowledgement. Revoke assistance even when
    /// interruption is unsupported; revocation alone does not interrupt native execution.
    /// # Errors
    /// Rejects stale ownership, unsupported interruption and ambiguous control delivery.
    pub async fn cancel(
        &self,
        owner: &HarnessOwner,
        mutation_id: String,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        let _ownership = self.store.acquire(owner).await?;
        let session = self.store.load(owner).await?;
        let interruption_supported =
            session.capabilities.interrupt.support == HarnessSupportLevel::Supported;
        let target = session.target.ok_or(ManagedHarnessError::Conflict)?;
        let (_, request) = self
            .request(
                owner,
                mutation_id.clone(),
                HarnessRequestKind::Interrupt(HarnessControlRequest {
                    target,
                    mutation_id,
                }),
            )
            .await?;
        // Cancellation revokes parent assistance independently of whether this native
        // backend supports interrupt. Keep the revocation drain guard until return, and
        // do not reacquire it in mutate (canonical revocation takes an exclusive gate).
        let assistance = self.store.assistance_guard(owner, &request).await?;
        if !interruption_supported {
            return Err(ManagedHarnessError::Unsupported);
        }
        self.mutate_guarded(owner, &request, assistance).await
    }

    /// Durably request one mid-session setting change for subsequent native turns.
    ///
    /// The canonical claim precedes native delivery exactly like interruption. Acknowledgement
    /// is not proof of native effect, and an ambiguous outcome is never replayed.
    /// # Errors
    /// Rejects stale ownership, undeclared capabilities, conflicting identities and ambiguous
    /// delivery.
    pub async fn configure(
        &self,
        owner: &HarnessOwner,
        mutation_id: String,
        setting: HarnessSessionSetting,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        let _ownership = self.store.acquire(owner).await?;
        let session = self.store.load(owner).await?;
        let target = session.target.ok_or(ManagedHarnessError::Conflict)?;
        policy::configuration(&target, &session.capabilities, &setting)?;
        let (_, request) = self
            .request(
                owner,
                mutation_id.clone(),
                HarnessRequestKind::Configure(bcode_harness_models::HarnessConfigureRequest {
                    target,
                    mutation_id,
                    setting,
                }),
            )
            .await?;
        self.mutate(owner, &request).await
    }

    /// Close native transport explicitly. It is not assignment completion or descendant cancellation.
    /// # Errors
    /// Rejects stale ownership and ambiguous prior delivery.
    pub async fn close(
        &self,
        owner: &HarnessOwner,
        mutation_id: String,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        let _ownership = self.store.acquire(owner).await?;
        let target = self
            .store
            .load(owner)
            .await?
            .target
            .ok_or(ManagedHarnessError::Conflict)?;
        let (_, request) = self
            .request(
                owner,
                mutation_id.clone(),
                HarnessRequestKind::Close(HarnessControlRequest {
                    target,
                    mutation_id,
                }),
            )
            .await?;
        self.mutate(owner, &request).await
    }

    /// Read-only native reconciliation of exact retained identities. Never calls submit/open again.
    /// Daemon loss alone neither terminalizes work nor establishes that native effects were absent.
    /// # Errors
    /// Rejects unverifiable ownership, foreign binding/correlations and unavailable native evidence.
    pub async fn reconcile(
        &self,
        owner: &HarnessOwner,
        request_id: String,
        reconciliation: HarnessReconcileRequest,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        let _ownership = self.store.acquire(owner).await?;
        if reconciliation.session_id != owner.session_id.to_string()
            || reconciliation.generation != owner.generation
        {
            return Err(ManagedHarnessError::StaleOwner);
        }
        let (session, request) = self
            .request(
                owner,
                request_id,
                HarnessRequestKind::Reconcile(reconciliation),
            )
            .await?;
        let HarnessRequestKind::Reconcile(reconciliation) = &request.kind else {
            return Err(ManagedHarnessError::Invalid);
        };
        if reconciliation.binding != session.binding {
            return Err(ManagedHarnessError::Conflict);
        }
        if let (Some(submission), Some(attempt)) =
            (&reconciliation.submission_id, &reconciliation.attempt_id)
            && self
                .store
                .receipt(owner, submission, attempt)
                .await?
                .is_none()
        {
            return Err(ManagedHarnessError::Conflict);
        }
        // Reconciliation itself is read-only. Retain its exact query before recording evidence;
        // a lost read reply may be queried again, unlike open/submit/respond/control mutations.
        match self.store.claim(owner, &request).await? {
            DeliveryClaim::Recorded(response) => {
                response
                    .validate_for(&request)
                    .map_err(|_| ManagedHarnessError::Conflict)?;
                return Ok(*response);
            }
            DeliveryClaim::New | DeliveryClaim::Uncertain => {}
        }
        let response = self.native(&request).await?;
        if let HarnessServiceResponseKind::Reconciled(
            HarnessReconciliation::Known {
                receipt: Some(receipt),
                ..
            }
            | HarnessReconciliation::KnownPrepared {
                receipt: Some(receipt),
                ..
            },
        ) = &response.kind
        {
            if let Some(prior) = self
                .store
                .receipt(owner, &receipt.submission_id, &receipt.attempt_id)
                .await?
            {
                policy::transition(prior.state, receipt.state)?;
            } else {
                return Err(ManagedHarnessError::Conflict);
            }
        }
        // Commit only evidence; even NotApplied does not authorize automatic replay.
        self.store.commit(owner, &request, &response).await?;
        Ok(response)
    }

    /// Read canonical observations without requiring a running/enabled native plugin.
    /// The outer application verifies selected-location read authority.
    /// # Errors
    /// Rejects oversized pages and damaged/stale canonical projections.
    pub async fn history(
        &self,
        session_id: SessionId,
        after: Option<String>,
        limit: u16,
    ) -> Result<HarnessObservationPage, ManagedHarnessError> {
        if limit == 0
            || usize::from(limit) > MAX_HARNESS_OBSERVATIONS
            || after
                .as_ref()
                .is_some_and(|value| value.is_empty() || value.len() > MAX_HARNESS_ID_BYTES)
        {
            return Err(ManagedHarnessError::Invalid);
        }
        let page = self.store.history(session_id, after, limit).await?;
        if page.observations.len() > usize::from(limit)
            || page
                .observations
                .iter()
                .any(|event| event.session_id != session_id.to_string())
        {
            return Err(ManagedHarnessError::Invalid);
        }
        let envelope = HarnessServiceResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: "canonical-history".into(),
            operation: HarnessOperation::Observe,
            kind: HarnessServiceResponseKind::Observations(page),
        };
        envelope
            .validate()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        let HarnessServiceResponseKind::Observations(page) = envelope.kind else {
            return Err(ManagedHarnessError::Invalid);
        };
        Ok(page)
    }
}

#[cfg(test)]
mod observation_failure_tests {
    use super::*;

    #[test]
    fn observation_process_loss_never_becomes_plugin_discovery_failure() {
        assert_eq!(
            observation_failure(HarnessFailure::Uncertain),
            ManagedHarnessError::Uncertain
        );
        assert_eq!(
            observation_failure(HarnessFailure::StaleGeneration),
            ManagedHarnessError::StaleOwner
        );
        assert_eq!(
            observation_failure(HarnessFailure::Conflict),
            ManagedHarnessError::Conflict
        );
    }
}
