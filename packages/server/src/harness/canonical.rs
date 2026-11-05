//! Canonical session-store implementation. Every protected operation enters the session mailbox.

use super::{
    DeliveryClaim, HarnessOwner, ManagedHarnessError, ManagedHarnessStore, ManagedSession,
};
use bcode_harness_models::{
    HarnessAssistanceGrant, HarnessInspection, HarnessInstance, HarnessInteraction,
    HarnessInteractionKind, HarnessObservationPage, HarnessReceipt, HarnessRequest,
    HarnessRequestKind, HarnessServiceResponse, HarnessSessionBinding, HarnessSessionTarget,
};
use bcode_session_models::{
    SessionId,
    external_harness::{ExternalHarnessClaim, ExternalHarnessOwner},
};
use std::sync::{Arc, Weak};

/// Log only a fixed stage and an error category; never DB messages, paths or native payloads.
fn storage_failure(
    stage: &'static str,
    error: &bcode_session::SessionError,
) -> ManagedHarnessError {
    use bcode_session::SessionError;
    let category = match error {
        SessionError::NotFound(_) => "not_found",
        SessionError::Store(_) => "session_store",
        SessionError::Db(_) => "session_database",
        SessionError::EventSerialization(_) => "canonical_contract_or_owner",
        SessionError::DbUnavailable(_) => "database_unavailable",
        _ => "session_operation",
    };
    tracing::warn!(target: "bcode_server::harness", stage, category, "managed canonical stage failed");
    ManagedHarnessError::Storage
}

/// Production adapter; weak state avoids a backend/ServerState reference cycle.
#[derive(Clone)]
pub struct CanonicalHarnessStore {
    pub(super) state: Weak<crate::ServerState>,
    pub(super) assistance: Arc<super::assistance::AssistanceRegistry>,
}

impl CanonicalHarnessStore {
    pub(super) async fn admitted_assistance(
        &self,
        owner: &HarnessOwner,
        grant: &HarnessAssistanceGrant,
    ) -> Result<Box<dyn Send + Sync>, ManagedHarnessError> {
        let consent = self.assistance.exact(owner, grant).await?;
        let live = super::assistance::AssistanceRegistry::read(&consent).await?;
        let result = async {
            let state = self.state()?;
            Self::local_owner(&state, &consent.parent)?;
            drop(state);
            let child_hold = self.acquire(owner).await?;
            let parent_hold = self.acquire(&consent.parent).await?;
            // Parent must have an actual current canonical generation, not merely a lease
            // for an unconfigured ordinary session. No fallback to an inferred generation.
            self.selected(&consent.parent).await?;
            let state = self.state()?;
            let child = state
                .sessions
                .session_summary(owner.session_id)
                .await
                .map_err(|_| ManagedHarnessError::Storage)?;
            drop(state);
            if child.parent_id != Some(consent.parent.session_id) {
                return Err(ManagedHarnessError::StaleOwner);
            }
            consent
                .authorization
                .authorize(owner, &consent.parent)
                .await?;
            Ok(Box::new((live, child_hold, parent_hold)) as Box<dyn Send + Sync>)
        }
        .await;
        if result.is_err() {
            drop(self.assistance.revoke(owner.session_id).await);
        }
        result
    }
    /// Bind the owning daemon without retaining its lifetime.
    #[must_use]
    pub fn new(state: &Arc<crate::ServerState>) -> Self {
        Self {
            state: Arc::downgrade(state),
            assistance: Arc::default(),
        }
    }
    pub(super) fn state(&self) -> Result<Arc<crate::ServerState>, ManagedHarnessError> {
        self.state.upgrade().ok_or(ManagedHarnessError::Unavailable)
    }
    pub(super) fn local_owner(
        state: &crate::ServerState,
        owner: &HarnessOwner,
    ) -> Result<(), ManagedHarnessError> {
        if owner.generation == 0
            || owner.daemon_id != state.daemon_status.instance_id
            || state
                .daemon_status
                .artifact_id
                .as_ref()
                .map(ToString::to_string)
                .as_deref()
                != Some(owner.artifact_id.as_str())
        {
            return Err(ManagedHarnessError::StaleOwner);
        }
        Ok(())
    }
    pub(super) fn durable(
        &self,
        owner: &HarnessOwner,
    ) -> Result<ExternalHarnessOwner, ManagedHarnessError> {
        let state = self.state()?;
        Self::local_owner(&state, owner)?;
        // Session ownership uses the lease's build fingerprint, not the separately
        // hashed executable ArtifactId. Translate only after checking the actual local
        // artifact AND daemon. Never translate a foreign persisted daemon into ours.
        Ok(ExternalHarnessOwner {
            session_id: owner.session_id,
            generation: owner.generation,
            artifact_id: state.daemon_status.build_fingerprint.clone(),
            daemon_id: owner.daemon_id.clone(),
        })
    }

    pub(super) async fn prepared_open(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
        parent_id: Option<SessionId>,
    ) -> Result<bool, ManagedHarnessError> {
        let state = self.state()?;
        Self::local_owner(&state, owner)?;
        match state.sessions.session_summary(owner.session_id).await {
            Err(bcode_session::SessionError::NotFound(_)) => return Ok(false),
            Err(error) => return Err(storage_failure("prepared_lookup", &error)),
            Ok(summary) => {
                if summary.parent_id != parent_id {
                    return Err(ManagedHarnessError::Conflict);
                }
                let HarnessRequestKind::Open(open) = &request.kind else {
                    return Err(ManagedHarnessError::Invalid);
                };
                if super::admission::canonical_directory(&summary.working_directory).await?
                    != super::admission::canonical_directory(std::path::Path::new(
                        &open.working_directory,
                    ))
                    .await?
                {
                    return Err(ManagedHarnessError::Conflict);
                }
            }
        }
        let selected = self.selected(owner).await?;
        if selected.instance != request.instance
            || selected.prepared_open.as_ref()
                != Some(
                    &bcode_session_models::external_harness::ExternalHarnessRequest::from_request(
                        request,
                    )
                    .map_err(|_| ManagedHarnessError::Invalid)?,
                )
        {
            return Err(ManagedHarnessError::Conflict);
        }
        // Verify the retained open/workspace/parent atomically again in the owning domain.
        // Returning true does not authorize native delivery; actor claim remains separate.
        state
            .sessions
            .prepare_external_harness_open(
                &self.durable(owner)?,
                request,
                selected.capabilities,
                parent_id,
            )
            .await
            .map_err(|_| ManagedHarnessError::Conflict)?;
        drop(state);
        Ok(true)
    }

    pub(super) async fn prepare_open(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
        inspection: &HarnessInspection,
        parent_id: Option<SessionId>,
    ) -> Result<(), ManagedHarnessError> {
        let state = self.state()?;
        Self::local_owner(&state, owner)?;
        let summary = state
            .sessions
            .prepare_external_harness_open(
                &self.durable(owner)?,
                request,
                inspection.capabilities.clone(),
                parent_id,
            )
            .await
            .map_err(|error| storage_failure("prepare_canonical_open", &error))?;
        state.session_catalog.upsert_native_session(summary).await;
        drop(state);
        Ok(())
    }

    /// Discover the full durable owner, including sessions whose native target is only prepared.
    /// Snapshot discovery is not live-transport evidence: acquire and re-read canonical state
    /// before returning. A foreign daemon's recorded owner is never rewritten into local authority.
    /// # Errors
    /// Rejects absent/degraded selection, wrong location, foreign leases and changed snapshots.
    pub async fn current_owner(
        &self,
        session_id: SessionId,
    ) -> Result<HarnessOwner, ManagedHarnessError> {
        let state = self.state()?;
        crate::session_operations::require_selected_location(&state, session_id)
            .await
            .map_err(|_| ManagedHarnessError::StaleOwner)?;
        let snapshot = state
            .sessions
            .session_external_harness_state(session_id)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?
            .ok_or(ManagedHarnessError::Conflict)?;
        if snapshot.owner.daemon_id != state.daemon_status.instance_id
            || snapshot.owner.artifact_id != state.daemon_status.build_fingerprint
        {
            return Err(ManagedHarnessError::StaleOwner);
        }
        let mut owner = Self::owner_from_snapshot(session_id, &snapshot)?;
        owner.artifact_id = state
            .daemon_status
            .artifact_id
            .as_ref()
            .ok_or(ManagedHarnessError::StaleOwner)?
            .to_string();
        Self::local_owner(&state, &owner)?;
        drop(state);
        let _hold = self.acquire(&owner).await?;
        if self.selected(&owner).await? != snapshot {
            return Err(ManagedHarnessError::StaleOwner);
        }
        Ok(owner)
    }

    pub(super) fn owner_from_snapshot(
        session_id: SessionId,
        snapshot: &bcode_session_models::external_harness::ExternalHarnessState,
    ) -> Result<HarnessOwner, ManagedHarnessError> {
        snapshot
            .owner
            .validate()
            .map_err(|_| ManagedHarnessError::Storage)?;
        if snapshot.owner.session_id != session_id {
            return Err(ManagedHarnessError::StaleOwner);
        }
        if let Some(target) = &snapshot.target {
            target
                .validate_owner(&session_id.to_string(), snapshot.owner.generation)
                .map_err(|_| ManagedHarnessError::StaleOwner)?;
            if target.instance_id() != snapshot.instance.instance_id {
                return Err(ManagedHarnessError::Conflict);
            }
        }
        if matches!(snapshot.target, Some(HarnessSessionTarget::Prepared(_)))
            && snapshot.binding.is_some()
        {
            return Err(ManagedHarnessError::Conflict);
        }
        if let Some(HarnessSessionTarget::Confirmed(binding)) = &snapshot.target
            && snapshot.binding.as_ref() != Some(binding)
        {
            return Err(ManagedHarnessError::Conflict);
        }
        Ok(HarnessOwner {
            session_id,
            generation: snapshot.owner.generation,
            artifact_id: snapshot.owner.artifact_id.clone(),
            daemon_id: snapshot.owner.daemon_id.clone(),
        })
    }

    /// Current canonical native binding for legacy/application owner discovery.
    /// # Errors
    /// Rejects missing/degraded storage. This does not transfer execution ownership.
    pub async fn binding(
        &self,
        session_id: SessionId,
    ) -> Result<(HarnessInstance, HarnessSessionBinding), ManagedHarnessError> {
        use bcode_session_models::external_backend::{
            ExternalBackendEnvelope, ExternalBackendRecord,
        };
        let state = self.state()?;
        let event = state
            .sessions
            .session_external_backend_binding(session_id)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?
            .ok_or(ManagedHarnessError::Conflict)?;
        drop(state);
        match ExternalBackendEnvelope::from_event(&event)
            .map_err(|_| ManagedHarnessError::Storage)?
            .ok_or(ManagedHarnessError::Storage)?
            .record
        {
            ExternalBackendRecord::Binding { instance, binding } => Ok((instance, binding)),
            _ => Err(ManagedHarnessError::Storage),
        }
    }

    /// Read canonical selected state under live lease verification. No plugin is required.
    /// # Errors
    /// Rejects foreign/stale owners and unavailable canonical state without fallback.
    pub async fn selected(
        &self,
        owner: &HarnessOwner,
    ) -> Result<bcode_session_models::external_harness::ExternalHarnessState, ManagedHarnessError>
    {
        let state = self.state()?;
        Self::local_owner(&state, owner)?;
        crate::session_operations::require_selected_location(&state, owner.session_id)
            .await
            .map_err(|_| ManagedHarnessError::StaleOwner)?;
        state
            .sessions
            .external_harness_state(&self.durable(owner)?)
            .await
            .map_err(|_| ManagedHarnessError::Storage)
    }
}

impl ManagedHarnessStore for CanonicalHarnessStore {
    type OwnershipGuard = bcode_session::SessionOwnershipGuard;

    async fn acquire(
        &self,
        owner: &HarnessOwner,
    ) -> Result<Self::OwnershipGuard, ManagedHarnessError> {
        let state = self.state()?;
        Self::local_owner(&state, owner)?;
        crate::session_operations::require_selected_location(&state, owner.session_id)
            .await
            .map_err(|_| ManagedHarnessError::StaleOwner)?;
        state
            .sessions
            .acquire_external_harness_ownership(&self.durable(owner)?)
            .await
            .map_err(|_| ManagedHarnessError::StaleOwner)
    }
    async fn load(&self, owner: &HarnessOwner) -> Result<ManagedSession, ManagedHarnessError> {
        let selected = self.selected(owner).await?;
        Ok(ManagedSession {
            instance: selected.instance,
            capabilities: selected.capabilities,
            binding: selected.binding,
            target: selected.target,
        })
    }
    async fn assistance_guard(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
    ) -> Result<Box<dyn Send + Sync>, ManagedHarnessError> {
        if matches!(
            &request.kind,
            HarnessRequestKind::Interrupt(_) | HarnessRequestKind::Close(_)
        ) {
            return Ok(Box::new(self.assistance.revoke(owner.session_id).await));
        }
        let selected = self.selected(owner).await?;
        let retained = selected
            .prepared_open
            .as_ref()
            .and_then(|request| match &request.kind {
                bcode_session_models::external_harness::ExternalHarnessRequestKind::Open(open) => {
                    open.parent_assistance.as_ref()
                }
                _ => None,
            });
        let assistance_response =
            if let HarnessRequestKind::Respond { response, .. } = &request.kind {
                let interaction = self.interaction(owner, &response.request_id).await?;
                matches!(interaction.kind, HarnessInteractionKind::Assistance)
            } else {
                false
            };
        let proposed = match &request.kind {
            HarnessRequestKind::Open(open) => open.parent_assistance.as_ref(),
            HarnessRequestKind::Respond { .. } if assistance_response => {
                Some(retained.ok_or(ManagedHarnessError::StaleOwner)?)
            }
            _ => None,
        };
        if let Some(grant) = proposed {
            if Some(grant) != retained {
                return Err(ManagedHarnessError::StaleOwner);
            }
            return self.admitted_assistance(owner, grant).await;
        }
        Ok(Box::new(()))
    }

    async fn claim(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
    ) -> Result<DeliveryClaim, ManagedHarnessError> {
        let state = self.state()?;
        Self::local_owner(&state, owner)?;
        match state
            .sessions
            .claim_external_harness_request(&self.durable(owner)?, request)
            .await
            .map_err(|_| ManagedHarnessError::Conflict)?
        {
            ExternalHarnessClaim::New => Ok(DeliveryClaim::New),
            ExternalHarnessClaim::Recorded(response) => {
                Ok(DeliveryClaim::Recorded(Box::new(response.response())))
            }
            ExternalHarnessClaim::Uncertain => Ok(DeliveryClaim::Uncertain),
        }
    }
    async fn commit(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
        response: &HarnessServiceResponse,
    ) -> Result<(), ManagedHarnessError> {
        let state = self.state()?;
        Self::local_owner(&state, owner)?;
        state
            .sessions
            .complete_external_harness_request(&self.durable(owner)?, request, response)
            .await
            .map_err(|_| ManagedHarnessError::Storage)
    }
    async fn uncertain(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
    ) -> Result<(), ManagedHarnessError> {
        let state = self.state()?;
        Self::local_owner(&state, owner)?;
        state
            .sessions
            .mark_external_harness_uncertain(&self.durable(owner)?, request)
            .await
            .map_err(|_| ManagedHarnessError::Storage)
    }
    async fn interaction(
        &self,
        owner: &HarnessOwner,
        request_id: &str,
    ) -> Result<HarnessInteraction, ManagedHarnessError> {
        let state = self.state()?;
        Self::local_owner(&state, owner)?;
        state
            .sessions
            .external_harness_interaction(&self.durable(owner)?, request_id)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?
            .ok_or(ManagedHarnessError::Conflict)
    }
    async fn receipt(
        &self,
        owner: &HarnessOwner,
        submission_id: &str,
        attempt_id: &str,
    ) -> Result<Option<HarnessReceipt>, ManagedHarnessError> {
        let state = self.state()?;
        Self::local_owner(&state, owner)?;
        state
            .sessions
            .external_harness_receipt(&self.durable(owner)?, submission_id, attempt_id)
            .await
            .map_err(|_| ManagedHarnessError::Storage)
    }
    fn observations(
        &self,
        _: &HarnessOwner,
        _: &HarnessObservationPage,
    ) -> impl std::future::Future<Output = Result<(), ManagedHarnessError>> + Send {
        // The production path must retain its acknowledged predecessor; refusing this older
        // convenience method prevents an omitted cursor from masquerading as a first page.
        std::future::ready(Err(ManagedHarnessError::Invalid))
    }
    async fn observation_page(
        &self,
        owner: &HarnessOwner,
        after: Option<&str>,
        page: &HarnessObservationPage,
    ) -> Result<(), ManagedHarnessError> {
        let state = self.state()?;
        Self::local_owner(&state, owner)?;
        state
            .sessions
            .commit_external_harness_page(&self.durable(owner)?, after, page)
            .await
            .map_err(|error| {
                let confirmations = page.observations.iter().filter(|item| matches!(item.kind, bcode_harness_models::HarnessObservationKind::BindingConfirmed { .. })).count();
                let interactions = page.observations.iter().filter(|item| matches!(item.kind, bcode_harness_models::HarnessObservationKind::Interaction { .. })).count();
                let settlements = page.observations.iter().filter(|item| matches!(item.kind, bcode_harness_models::HarnessObservationKind::Settlement { .. })).count();
                tracing::warn!(target: "bcode_server::harness", stage = "observe_commit_shape", has_predecessor = after.is_some(), observations = page.observations.len(), confirmations, interactions, settlements, "managed canonical observation commit rejected");
                storage_failure("observe_canonical_commit", &error)
            })
    }
    async fn history(
        &self,
        session_id: SessionId,
        after: Option<String>,
        limit: u16,
    ) -> Result<HarnessObservationPage, ManagedHarnessError> {
        super::canonical_history(&self.state()?, session_id, after, limit).await
    }
}
