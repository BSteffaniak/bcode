//! Caller-bound daemon application facade. No IPC caller chooses ownership identity.

use super::{
    HarnessOwner, HarnessPluginTransport, ManagedHarnessError, ManagedHarnessRuntime,
    ManagedHarnessStore, ManagedHarnessTransport, canonical_history,
};
use bcode_harness_models::{
    HarnessObservationPage, HarnessOpenRequest, HarnessReconcileRequest, HarnessResponse,
    HarnessServiceResponse, HarnessSubmission,
};
use bcode_session_models::{ClientId, SessionId};
use std::{sync::Arc, sync::atomic::Ordering};

/// Application access bound to a caller already authenticated by the daemon.
///
/// Construction is not portable authorization. The daemon request dispatcher must supply its
/// registered client ID, never deserialize one from a harness request.
pub struct ManagedHarnessApplication<'a, S, T = HarnessPluginTransport> {
    state: &'a Arc<crate::ServerState>,
    client_id: ClientId,
    runtime: &'a ManagedHarnessRuntime<S, T>,
    admitted_owner: Option<HarnessOwner>,
}

impl<'a, S: ManagedHarnessStore, T: ManagedHarnessTransport> ManagedHarnessApplication<'a, S, T> {
    /// Bind the host-authenticated client to the daemon-owned runtime.
    #[must_use]
    pub const fn new(
        state: &'a Arc<crate::ServerState>,
        client_id: ClientId,
        runtime: &'a ManagedHarnessRuntime<S, T>,
    ) -> Self {
        Self {
            state,
            client_id,
            runtime,
            admitted_owner: None,
        }
    }

    /// Retain lane-11 canonical admission, including prepared transports without native identity.
    /// Runtime independently revalidates this full owner in the canonical writer before effects.
    #[must_use]
    pub const fn with_owner(
        state: &'a Arc<crate::ServerState>,
        client_id: ClientId,
        runtime: &'a ManagedHarnessRuntime<S, T>,
        owner: HarnessOwner,
    ) -> Self {
        Self {
            state,
            client_id,
            runtime,
            admitted_owner: Some(owner),
        }
    }

    async fn scope(&self, session_id: SessionId) -> Result<(), ManagedHarnessError> {
        if self.state.shutdown_requested.load(Ordering::SeqCst) {
            return Err(ManagedHarnessError::Unavailable);
        }
        crate::session_operations::require_selected_location(self.state, session_id)
            .await
            .map_err(|_| ManagedHarnessError::StaleOwner)?;
        if self
            .state
            .active_session_namespace_mismatch(session_id, self.client_id)
            .await
            .is_some()
        {
            return Err(ManagedHarnessError::StaleOwner);
        }
        Ok(())
    }

    /// Resolve the full durable managed owner, whether pre-open, prepared or confirmed.
    /// This is not transfer or dispatch authority: runtime store acquire/claim must compare the
    /// durable full owner and retain its lease. Parentage and supplied generations are not used.
    /// # Errors
    /// Rejects missing managed state, wrong locations, namespace conflicts and unavailable projections.
    pub async fn owner(&self, session_id: SessionId) -> Result<HarnessOwner, ManagedHarnessError> {
        self.scope(session_id).await?;
        if let Some(owner) = &self.admitted_owner {
            if owner.session_id != session_id {
                return Err(ManagedHarnessError::StaleOwner);
            }
            self.runtime.verify_owner(owner).await?;
            return Ok(owner.clone());
        }
        let owner = super::CanonicalHarnessStore::new(self.state)
            .current_owner(session_id)
            .await?;
        self.runtime.verify_owner(&owner).await?;
        Ok(owner)
    }

    /// Inspect native provenance/capabilities for an already bound managed session.
    /// # Errors
    /// Rejects wrong namespace, unavailable plugins and stale canonical ownership.
    pub async fn inspect(
        &self,
        session_id: SessionId,
        request_id: String,
    ) -> Result<bcode_harness_models::HarnessInspection, ManagedHarnessError> {
        self.runtime
            .inspect(&self.owner(session_id).await?, request_id)
            .await
    }

    /// Submit against canonical ownership; input generation must match rather than being rewritten.
    /// # Errors
    /// Returns normalized scope, ownership, contract, storage or native-delivery uncertainty errors.
    pub async fn submit(
        &self,
        session_id: SessionId,
        submission: HarnessSubmission,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        self.runtime
            .submit(&self.owner(session_id).await?, submission)
            .await
    }

    /// Answer the exact outstanding native interaction after ordinary application caller admission.
    /// # Errors
    /// Rejects wrong request, stale generation, scope conflict and uncertain previous delivery.
    pub async fn respond(
        &self,
        session_id: SessionId,
        response: HarnessResponse,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        self.runtime
            .respond(&self.owner(session_id).await?, response)
            .await
    }

    /// Poll one bounded native page under canonical current ownership.
    /// # Errors
    /// Rejects invalid pages, unavailable native execution or unverifiable ownership.
    pub async fn observe(
        &self,
        session_id: SessionId,
        request_id: String,
        after: Option<String>,
        limit: u16,
        wait_millis: u32,
    ) -> Result<HarnessObservationPage, ManagedHarnessError> {
        let owner = self.owner(session_id).await.inspect_err(|error| {
            tracing::warn!(target: "bcode_server::harness", stage = "observe_owner", category = %error, "managed observation admission failed");
        })?;
        self.runtime
            .observe(&owner, request_id, after, limit, wait_millis)
            .await
    }

    /// Request interruption, never infer settlement from acknowledgment.
    /// # Errors
    /// Rejects scope conflicts, unsupported interruption or ambiguous previous delivery.
    pub async fn cancel(
        &self,
        session_id: SessionId,
        mutation_id: String,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        self.runtime
            .cancel(&self.owner(session_id).await?, mutation_id)
            .await
    }

    /// Close native transport without cancelling descendants or claiming assignment completion.
    /// # Errors
    /// Rejects stale ownership and ambiguous previous delivery.
    pub async fn close(
        &self,
        session_id: SessionId,
        mutation_id: String,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        self.runtime
            .close(&self.owner(session_id).await?, mutation_id)
            .await
    }

    /// Reconcile exact retained native identities without mutation replay.
    /// # Errors
    /// Rejects foreign identities and unavailable/unverifiable evidence.
    pub async fn reconcile(
        &self,
        session_id: SessionId,
        request_id: String,
        request: HarnessReconcileRequest,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        self.runtime
            .reconcile(&self.owner(session_id).await?, request_id, request)
            .await
    }

    /// Open an already-created ordinary Bcode session using a host-prepared canonical owner.
    /// The owner must come from durable pre-open admission, not IPC. Serving identity and client
    /// scope are checked here; runtime store acquisition verifies that admission before delivery.
    /// This method does not manufacture generation one or adopt an unrelated native session.
    /// # Errors
    /// Rejects foreign daemon/artifact identity, namespace conflicts and uncertain native delivery.
    pub async fn open(
        &self,
        owner: &HarnessOwner,
        request: HarnessOpenRequest,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        self.scope(owner.session_id).await?;
        let artifact = self
            .state
            .daemon_status
            .artifact_id
            .as_ref()
            .ok_or(ManagedHarnessError::StaleOwner)?
            .to_string();
        if owner.artifact_id != artifact || owner.daemon_id != self.state.daemon_status.instance_id
        {
            return Err(ManagedHarnessError::StaleOwner);
        }
        self.runtime.open(owner, request).await
    }

    /// Resume only a canonically bound session. Initial creation requires a durable pre-open claim;
    /// never fabricate a binding/native ID to make initial open pass this boundary.
    /// # Errors
    /// Rejects unbound sessions, mismatched resume binding and uncertain native delivery.
    pub async fn resume(
        &self,
        session_id: SessionId,
        request: HarnessOpenRequest,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        if request.resume.is_none() {
            return Err(ManagedHarnessError::Invalid);
        }
        self.runtime
            .open(&self.owner(session_id).await?, request)
            .await
    }

    /// Read bounded canonical history even with native plugin disabled or uninstalled.
    /// # Errors
    /// Rejects invalid bounds, wrong locations or degraded canonical storage.
    pub async fn history(
        &self,
        session_id: SessionId,
        after: Option<String>,
        limit: u16,
    ) -> Result<HarnessObservationPage, ManagedHarnessError> {
        self.scope(session_id).await?;
        canonical_history(self.state, session_id, after, limit).await
    }
}
