//! Caller, workspace, enabled-plugin and canonical owner admission for daemon entrypoints.

use super::{
    CanonicalHarnessStore, HarnessOwner, ManagedHarnessError, ManagedHarnessStore,
    ManagedHarnessTransport,
};
use crate::harness_dispatch::{ManagedCaller, ManagedSessionAdmission};
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessAssistanceGrant, HarnessInstance, HarnessRequest,
    HarnessRequestKind, HarnessServiceResponse, HarnessServiceResponseKind, HarnessSupportLevel,
};
use bcode_ipc::harness::ManagedSessionCall;
use bcode_session_models::SessionId;
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

type Task<'a, T> = Pin<Box<dyn Future<Output = Result<T, ManagedHarnessError>> + Send + 'a>>;

/// Canonical admission for the same store installed with the managed runtime.
#[derive(Clone)]
pub struct CanonicalManagedAdmission {
    store: CanonicalHarnessStore,
}

impl CanonicalManagedAdmission {
    /// Bind the existing daemon's canonical store. This performs no native work.
    #[must_use]
    pub const fn new(store: CanonicalHarnessStore) -> Self {
        Self { store }
    }

    /// Recheck current authenticated interactive caller before recovery evidence discovery.
    /// # Errors
    /// Rejects non-client callers and ordinary workspace/location/bootstrap admission failures.
    pub async fn authorize_recovery_request(
        &self,
        caller: &ManagedCaller,
        session_id: SessionId,
    ) -> Result<(), ManagedHarnessError> {
        if !matches!(caller, ManagedCaller::Client(_)) {
            return Err(ManagedHarnessError::Unsupported);
        }
        self.authorize(caller, session_id, false).await.map(|_| ())
    }

    /// Authorize and fence one explicit graceful-restart recovery before native resume.
    /// The supplied host-owned validator must inspect the exact resume constraints without
    /// spawning native transport or replaying input. Constrained support is never promoted
    /// globally. A subsequent native dispatch still needs the ordinary durable claim gate.
    /// # Errors
    /// Rejects unpublished storage support, non-client callers, changed bootstrap/workspace,
    /// stale release evidence, unverifiable old lifetimes and failed exact native constraints.
    pub async fn prepare_recovery<F, Fut>(
        &self,
        caller: &ManagedCaller,
        recovery: bcode_session_models::external_harness::ExternalHarnessRecovery,
        validate_resume: F,
    ) -> Result<HarnessOwner, ManagedHarnessError>
    where
        F: FnOnce(bcode_harness_models::HarnessRequest) -> Fut + Send,
        Fut: Future<Output = Result<(), ManagedHarnessError>> + Send,
    {
        if !crate::harness::recovery_published() {
            return Err(ManagedHarnessError::Unsupported);
        }
        let ManagedCaller::Client(client_id) = caller else {
            return Err(ManagedHarnessError::Unsupported);
        };
        let session_id = recovery.previous_owner.session_id;
        let cwd = self.authorize(caller, session_id, false).await?;
        let state = self.store.state()?;
        let bootstrap = state
            .client_runtime_context(*client_id)
            .await
            .ok_or(ManagedHarnessError::StaleOwner)?;
        let snapshot = state
            .sessions
            .session_external_harness_state(session_id)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?
            .ok_or(ManagedHarnessError::Conflict)?;
        let owner = Self::daemon_owner(
            &state,
            session_id,
            snapshot
                .owner
                .generation
                .checked_add(1)
                .ok_or(ManagedHarnessError::Conflict)?,
        )?;
        let durable = self.store.durable(&owner)?;
        recovery
            .validate_for(&durable)
            .map_err(|_| ManagedHarnessError::Invalid)?;
        if snapshot.owner != recovery.previous_owner
            || snapshot.instance != recovery.release.instance
            || snapshot.binding.as_ref() != Some(&recovery.release.binding)
            || snapshot.capabilities != recovery.capabilities
        {
            return Err(ManagedHarnessError::Conflict);
        }
        Self::enabled(&state, &snapshot.instance)?;
        let summary = state
            .sessions
            .session_summary(session_id)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?;
        if summary.execution.is_some() {
            // Interactive recovery cannot take over a workflow-owned execution. Its durable
            // receipt/authority owner must implement its own explicit recovery journey.
            return Err(ManagedHarnessError::Unsupported);
        }
        let resume_request = recovery
            .resume_request
            .request()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        let HarnessRequestKind::Open(open) = &resume_request.kind else {
            return Err(ManagedHarnessError::Invalid);
        };
        if canonical_directory(&summary.working_directory).await? != cwd
            || canonical_directory(Path::new(&open.working_directory)).await? != cwd
        {
            return Err(ManagedHarnessError::StaleOwner);
        }
        let retained = state
            .sessions
            .session_external_harness_transport_release(&snapshot.owner)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?
            .ok_or(ManagedHarnessError::Conflict)?;
        if retained != (recovery.release_event_sequence, recovery.release.clone()) {
            return Err(ManagedHarnessError::Conflict);
        }
        self.store.verify_recovery_lifetime(&recovery.release)?;
        validate_resume(
            recovery
                .resume_request
                .request()
                .map_err(|_| ManagedHarnessError::Invalid)?,
        )
        .await?;
        // Recheck current caller admission after plugin preflight; authority is not a
        // property of a previously selected session or retained recovery payload.
        if self.authorize(caller, session_id, false).await? != cwd
            || state.client_runtime_context(*client_id).await.as_ref() != Some(&bootstrap)
        {
            return Err(ManagedHarnessError::StaleOwner);
        }
        Self::enabled(&state, &snapshot.instance)?;
        let store = self.store.clone();
        state
            .sessions
            .prepare_external_harness_recovery(&durable, recovery, move |release| {
                store
                    .verify_recovery_lifetime(release)
                    .map_err(|_| "managed recovery lifetime proof rejected".into())
            })
            .await
            .map_err(|_| ManagedHarnessError::Conflict)?;
        drop(state);
        Ok(owner)
    }

    /// Resolve a single API credential only under current managed caller/owner authority.
    /// No native-login readiness inspection or plugin-wide credential binding is consulted.
    /// # Errors
    /// Rejects missing/revoked caller authority, stale owner, mismatched selection,
    /// undeclared credential access or unavailable exact vault material.
    pub(super) async fn credential(
        &self,
        caller: &ManagedCaller,
        owner: &HarnessOwner,
        call: &ManagedSessionCall,
        selection: &bcode_plugin_sdk::PluginCredentialSelection,
    ) -> Result<bcode_plugin_sdk::SensitiveCredential, ManagedHarnessError> {
        selection
            .validate()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        if selection.instance_id != call.harness.instance.instance_id
            || selection.provider_id != call.harness.instance.auth.provider_id
            || selection.profile != call.harness.instance.auth.profile
            || selection.credential_namespace != call.harness.instance.auth.credential_namespace
            || selection.custody != call.harness.instance.auth.custody
            || selection.custody != bcode_harness_models::HarnessAuthCustody::VaultManaged
        {
            return Err(ManagedHarnessError::StaleOwner);
        }
        let (_cwd, _workflow) = self
            .authorize_existing_call(caller, call, owner.session_id)
            .await?;
        let _ownership = self.store.acquire(owner).await?;
        let selected = self.store.selected(owner).await?;
        if selected.instance != call.harness.instance {
            return Err(ManagedHarnessError::StaleOwner);
        }
        let state = self.store.state()?;
        Self::enabled(&state, &selected.instance)?;
        let config = state.session_config(owner.session_id).await;
        let runtime = bcode_config::load_runtime_auth_subscriptions();
        let resolved_profile = bcode_provider_auth::resolve_auth_provider_profile(
            &config,
            &selection.provider_id,
            &selected.instance.plugin_id,
            Some(&selection.profile),
            &runtime,
        )
        .map_err(|_| ManagedHarnessError::Unavailable)?;
        let profile = &resolved_profile.profile;
        // Namespace is an explicit non-secret profile binding, not inferred from its name.
        let declared = state.plugins.auth_provider(&selection.provider_id)
            .filter(|registered| registered.plugin_id == selected.instance.plugin_id)
            .is_some_and(|registered| registered.contribution.methods.iter().any(|method| {
                matches!(method,
                    bcode_provider_auth_models::AuthMethodContribution::SecretFields { method_id, fields, .. }
                    if profile.scheme.as_deref() == Some(method_id.as_str())
                        && fields.iter().any(|field| {
                            let mapped = profile.map.get(&selection.credential_id)
                                .and_then(|mapping| mapping.key.as_ref().or(mapping.env.as_ref()));
                            field.credential_id == selection.credential_id
                                && mapped == Some(&field.storage_key)
                        }))
            }));
        if !declared
            || profile.owner_plugin_id.as_deref() != Some(selected.instance.plugin_id.as_str())
            || profile.provider_id.as_deref() != Some(selection.provider_id.as_str())
        {
            return Err(ManagedHarnessError::StaleOwner);
        }
        let secret = bcode_plugin_sdk::SensitiveCredential::new(
            bcode_provider_auth::resolve_exact_vault_credential(
                &selection.profile,
                profile,
                &selection.credential_id,
                &selection.credential_namespace,
            )
            .map_err(|_| ManagedHarnessError::Unavailable)?,
        )
        .map_err(|_| ManagedHarnessError::Unavailable)?;
        // Resolution may touch the vault. Recheck ordinary caller/workflow grants
        // and the exact canonical generation before releasing the material.
        self.authorize_existing_call(caller, call, owner.session_id)
            .await?;
        if self.store.selected(owner).await?.instance != call.harness.instance {
            return Err(ManagedHarnessError::StaleOwner);
        }
        let current_config = state.session_config(owner.session_id).await;
        drop(state);
        let current_runtime = bcode_config::load_runtime_auth_subscriptions();
        let current = bcode_provider_auth::resolve_auth_provider_profile(
            &current_config,
            &selection.provider_id,
            &selected.instance.plugin_id,
            Some(&selection.profile),
            &current_runtime,
        )
        .map_err(|_| ManagedHarnessError::StaleOwner)?;
        if current.profile != *profile {
            return Err(ManagedHarnessError::StaleOwner);
        }
        Ok(secret)
    }

    /// Issue live assistance consent only after host policy and authenticated caller checks.
    /// Call before retaining the exact returned grant in the child's canonical open. The child
    /// must already be an ordinary canonical child with a current daemon lease; this API never
    /// creates sessions or infers authority from the requested parent relationship.
    /// # Errors
    /// Rejects unregistered/unauthorized callers, ineligible parents, foreign full owners,
    /// workspace/parent mismatches, revoked consent and exhausted bounded admission storage.
    pub async fn issue_parent_assistance(
        &self,
        caller: &ManagedCaller,
        child: HarnessOwner,
        parent: HarnessOwner,
        scope: String,
        authorization: Arc<dyn super::ParentAssistanceAuthorization>,
    ) -> Result<HarnessAssistanceGrant, ManagedHarnessError> {
        let child_cwd = self.authorize(caller, child.session_id, false).await?;
        let parent_cwd = self.authorize(caller, parent.session_id, false).await?;
        let state = self.store.state()?;
        CanonicalHarnessStore::local_owner(&state, &child)?;
        CanonicalHarnessStore::local_owner(&state, &parent)?;
        let _child_hold = self.store.acquire(&child).await?;
        let _parent_hold = self.store.acquire(&parent).await?;
        self.store.selected(&parent).await?;
        let child_summary = state
            .sessions
            .session_summary(child.session_id)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?;
        let parent_summary = state
            .sessions
            .session_summary(parent.session_id)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?;
        drop(state);
        if child_summary.parent_id != Some(parent.session_id)
            || canonical_directory(&child_summary.working_directory).await? != child_cwd
            || canonical_directory(&parent_summary.working_directory).await? != parent_cwd
        {
            return Err(ManagedHarnessError::StaleOwner);
        }
        authorization.authorize(&child, &parent).await?;
        let grant = HarnessAssistanceGrant {
            admission_id: uuid::Uuid::new_v4().to_string(),
            child_session_id: child.session_id.to_string(),
            child_generation: child.generation,
            parent_session_id: parent.session_id.to_string(),
            parent_generation: parent.generation,
            scope,
        };
        self.store
            .assistance
            .issue(child, parent, grant, authorization)
            .await
    }

    /// Revoke live assistance for a cancelled child or parent before returning to its caller.
    /// The host cancellation path must call this even when it does not send a native interrupt.
    /// Already-running admitted responses drain before this call returns; later responses fail.
    /// This does not cancel native execution or assert settlement.
    /// # Errors
    /// Rejects caller/location/owner ambiguity without granting any native control authority.
    pub async fn revoke_parent_assistance(
        &self,
        caller: &ManagedCaller,
        owner: &HarnessOwner,
    ) -> Result<(), ManagedHarnessError> {
        self.authorize(caller, owner.session_id, false).await?;
        let state = self.store.state()?;
        CanonicalHarnessStore::local_owner(&state, owner)?;
        drop(state);
        // Consent revocation is fail-closed even if the canonical owner became stale. It
        // grants no execution authority and deliberately precedes the owner recheck.
        drop(self.store.assistance.revoke(owner.session_id).await);
        let _hold = self.store.acquire(owner).await?;
        Ok(())
    }

    /// Acquire the real canonical lease for an authenticated caller's selected native instance.
    /// The returned guard must be retained through native I/O and response processing.
    /// # Errors
    /// Rejects unavailable bindings, wrong locations/workspaces, disabled plugins and stale owners.
    pub async fn ownership_guard(
        &self,
        caller: &ManagedCaller,
        session_id: SessionId,
        instance_id: &str,
    ) -> Result<
        (
            HarnessOwner,
            HarnessInstance,
            bcode_session::SessionOwnershipGuard,
        ),
        ManagedHarnessError,
    > {
        let cwd = self.authorize(caller, session_id, false).await?;
        let state = self.store.state()?;
        let summary = state
            .sessions
            .session_summary(session_id)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?;
        if canonical_directory(&summary.working_directory).await? != cwd {
            return Err(ManagedHarnessError::StaleOwner);
        }
        let owner = self.store.current_owner(session_id).await?;
        let hold = self.store.acquire(&owner).await?;
        let selected = self.store.selected(&owner).await?;
        let instance = selected.instance;
        if instance.instance_id != instance_id {
            return Err(ManagedHarnessError::Conflict);
        }
        Self::enabled(&state, &instance)?;
        drop(state);
        Ok((owner, instance, hold))
    }

    /// Inspect credential readiness under current read authority, including after daemon restart.
    ///
    /// This does not acquire/adopt the historical owner or authorize any auth mutation. The
    /// returned inspection deliberately contains no begin-admission evidence.
    /// # Errors
    /// Rejects unauthorized callers, changed canonical selection, invalid namespaces or plugins.
    pub async fn authorize_auth_status(
        &self,
        caller: &ManagedCaller,
        session_id: SessionId,
        instance_id: &str,
    ) -> Result<super::InspectedHarnessAuth, ManagedHarnessError> {
        if !matches!(caller, ManagedCaller::Client(_)) {
            return Err(ManagedHarnessError::Unsupported);
        }
        let selected = self
            .auth_read_selection(caller, session_id, instance_id)
            .await?;
        let transport = {
            let state = self.store.state()?;
            super::HarnessPluginTransport::with_view(
                Arc::clone(&state),
                super::configuration_views::for_session(&state, session_id).await?,
            )
        };
        let inspected = if selected.instance.auth.custody
            == bcode_harness_models::HarnessAuthCustody::VaultManaged
        {
            let (owner, _, _hold) = self
                .ownership_guard(caller, session_id, instance_id)
                .await?;
            let call = ManagedSessionCall {
                session_id: Some(session_id),
                parent_session_id: None,
                assistance_destination_session_id: None,
                harness: HarnessRequest {
                    schema_version: HARNESS_SCHEMA_VERSION,
                    request_id: format!("credential-status:{}", uuid::Uuid::new_v4()),
                    instance: selected.instance.clone(),
                    kind: HarnessRequestKind::AuthStatus { verify: false },
                },
            };
            super::transport::with_credential_admission(
                caller.clone(),
                owner,
                call,
                transport.inspect_auth_readonly(&selected.instance),
            )
            .await?
        } else {
            transport.inspect_auth_readonly(&selected.instance).await?
        };
        let current = self
            .auth_read_selection(caller, session_id, instance_id)
            .await?;
        if current != selected {
            return Err(ManagedHarnessError::StaleOwner);
        }
        Ok(inspected)
    }

    async fn auth_read_selection(
        &self,
        caller: &ManagedCaller,
        session_id: SessionId,
        instance_id: &str,
    ) -> Result<bcode_session_models::external_harness::ExternalHarnessState, ManagedHarnessError>
    {
        let cwd = self.authorize(caller, session_id, false).await?;
        let state = self.store.state()?;
        let summary = state
            .sessions
            .session_summary(session_id)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?;
        if canonical_directory(&summary.working_directory).await? != cwd {
            return Err(ManagedHarnessError::StaleOwner);
        }
        let selected = state
            .sessions
            .session_external_harness_state(session_id)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?
            .ok_or(ManagedHarnessError::Conflict)?;
        CanonicalHarnessStore::owner_from_snapshot(session_id, &selected)?;
        if selected.target.is_none() {
            return Err(ManagedHarnessError::Conflict);
        }
        if selected.instance.instance_id != instance_id {
            return Err(ManagedHarnessError::Conflict);
        }
        // Cached launch capabilities may predate a status-only plugin correction.
        // Read-only readiness asks the enabled selected plugin for current evidence;
        // it grants no login method, inactivity proof or native mutation authority.
        Self::enabled(&state, &selected.instance)?;
        drop(state);
        Ok(selected)
    }

    /// Bind typed terminal-plan evidence without granting daemon-native begin authority.
    /// The dispatcher must first obtain this evidence from the selected namespace-bound plan,
    /// reserve that namespace and reject every prior issued/uncertain handoff.
    /// # Errors
    /// Rejects wrong execution mode, unsupported method, custody or positive active-login evidence.
    pub fn bind_client_terminal_preparation(
        admission: &mut crate::harness_dispatch::ManagedAuthAdmission,
        method: &super::HarnessAuthMethodEvidence,
    ) -> Result<(), ManagedHarnessError> {
        admission.fresh_begin_authorized = false;
        admission.scope.method.clear();
        method.validate_client_terminal(&admission.instance)?;
        if admission.scope.namespace.plugin_id != admission.instance.plugin_id
            || admission.scope.namespace.credential_namespace
                != admission.instance.auth.credential_namespace
            || admission.scope.custody != method.custody
        {
            return Err(ManagedHarnessError::StaleOwner);
        }
        admission.scope.method.clone_from(&method.method_id);
        Ok(())
    }

    /// Resolve auth against the actual selected credential namespace, retaining its canonical lease.
    /// Status and existing-flow cleanup do not imply permission to start a new native login.
    /// # Errors
    /// Rejects unauthorized callers, foreign owners and inconsistent native selection.
    pub async fn authorize_auth(
        &self,
        caller: &ManagedCaller,
        session_id: SessionId,
        instance_id: &str,
    ) -> Result<crate::harness_dispatch::ManagedAuthAdmission, ManagedHarnessError> {
        self.authorize_auth_inspected(caller, session_id, instance_id)
            .await
            .map(|(admission, _)| admission)
    }

    /// Inspect authorized native readiness without discarding the reason begin is blocked.
    /// The returned admission retains the real canonical lease through caller response handling.
    /// Only positive plugin method/inactivity evidence can enable fresh begin; readiness itself
    /// is available even for plugins that do not yet declare login admission evidence.
    /// # Errors
    /// Rejects unsupported canonical auth capability, caller scope, stale ownership and invalid
    /// plugin evidence. Never guesses a method or interprets an empty flow registry as inactivity.
    pub async fn authorize_auth_inspected(
        &self,
        caller: &ManagedCaller,
        session_id: SessionId,
        instance_id: &str,
    ) -> Result<
        (
            crate::harness_dispatch::ManagedAuthAdmission,
            super::InspectedHarnessAuth,
        ),
        ManagedHarnessError,
    > {
        // Sensitive frontend authentication is not delegated by parentage or workflow linkage.
        if !matches!(caller, ManagedCaller::Client(_)) {
            return Err(ManagedHarnessError::Unsupported);
        }
        let (owner, instance, hold) = self
            .ownership_guard(caller, session_id, instance_id)
            .await?;
        let selected = self.store.selected(&owner).await?;
        if selected.capabilities.authentication.support == HarnessSupportLevel::Unsupported {
            return Err(ManagedHarnessError::Unsupported);
        }
        let namespace = bcode_provider_auth_models::managed::AuthNamespace {
            plugin_id: instance.plugin_id.clone(),
            credential_namespace: instance.auth.credential_namespace.clone(),
        };
        if !namespace.is_valid() {
            return Err(ManagedHarnessError::Invalid);
        }
        let ownership_fence = serde_json::to_string(&(owner.artifact_id, owner.daemon_id))
            .map_err(|_| ManagedHarnessError::Invalid)?;
        let mut admission = crate::harness_dispatch::ManagedAuthAdmission {
            scope: crate::harness_auth::VerifiedHarnessAuthScope {
                waiter: bcode_provider_auth::managed::AuthWaiter {
                    session_id: session_id.to_string(),
                    generation: owner.generation,
                    ownership_fence,
                },
                namespace,
                custody: instance.auth.custody,
                // Bootstrap inspection carries no begin authority. Only a validated native
                // status extension below can supply the selected method and inactivity proof.
                method: String::new(),
            },
            instance,
            ownership_hold: Box::new(hold),
            fresh_begin_authorized: false,
        };
        let inspected = super::HarnessPluginTransport::new(self.store.state()?)
            .inspect_auth(&admission)
            .await?;
        // Inspection performs native I/O. Recheck the retained caller after that await as
        // well as keeping the canonical guard, so revoked client scope cannot enable begin.
        self.authorize(caller, session_id, false).await?;
        apply_auth_inspection(&mut admission, &inspected)?;
        Ok((admission, inspected))
    }

    /// Admit only an exact workflow operation backed by the retained dispatch receipt.
    /// Caller strings and session association alone never authorize canonical access. The
    /// backend additionally retains its workflow guard across native delivery/completion.
    async fn authorize_existing_call(
        &self,
        caller: &ManagedCaller,
        call: &ManagedSessionCall,
        session_id: SessionId,
    ) -> Result<(PathBuf, Option<crate::workflow_operations::AuthorityGuard>), ManagedHarnessError>
    {
        let ManagedCaller::Workflow {
            run_id,
            dispatch_identity,
            session_id: granted_session,
            plugin_id,
        } = caller
        else {
            return self
                .authorize(caller, session_id, false)
                .await
                .map(|cwd| (cwd, None));
        };
        if *granted_session != session_id {
            return Err(ManagedHarnessError::StaleOwner);
        }
        let state = self.store.state()?;
        if state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(ManagedHarnessError::Unavailable);
        }
        let guard = crate::harness_workflow::authorize_call(
            &state, run_id, dispatch_identity, session_id, plugin_id, call,
        ).await.map_err(|_| ManagedHarnessError::StaleOwner).inspect_err(|error| {
            tracing::warn!(target: "bcode_server::harness", stage = "workflow_call_authority", category = %error, "managed workflow admission failed");
        })?;
        crate::session_operations::require_selected_location(&state, session_id)
            .await
            .map_err(|_| ManagedHarnessError::StaleOwner)?;
        let summary = state
            .sessions
            .session_summary(session_id)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?;
        drop(state);
        let cwd = canonical_directory(&summary.working_directory).await?;
        Ok((cwd, Some(guard)))
    }

    async fn authorize(
        &self,
        caller: &ManagedCaller,
        target: SessionId,
        creating: bool,
    ) -> Result<PathBuf, ManagedHarnessError> {
        let state = self.store.state()?;
        if state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(ManagedHarnessError::Unavailable);
        }
        // A fresh target has no canonical directory yet. Creation performs the same
        // location check after authenticated workspace admission and primary-root setup.
        if !creating {
            crate::session_operations::require_selected_location(&state, target)
                .await
                .map_err(|_failure| {
                    tracing::warn!(target: "bcode_server::harness", category = "caller_selected_location", "managed harness admission rejected");
                    ManagedHarnessError::StaleOwner
                })?;
        }
        let cwd = match caller {
            ManagedCaller::Client(client) => {
                if state.client_runtime_context(*client).await.is_none() {
                    tracing::debug!(
                        stage = "caller_context",
                        "managed harness admission rejected"
                    );
                    return Err(ManagedHarnessError::StaleOwner);
                }
                if state
                    .active_session_namespace_mismatch(target, *client)
                    .await
                    .is_some()
                {
                    tracing::debug!(
                        stage = "caller_namespace",
                        "managed harness admission rejected"
                    );
                    return Err(ManagedHarnessError::StaleOwner);
                }
                state
                    .client_working_directory(*client)
                    .await
                    .ok_or_else(|| {
                        tracing::debug!(
                            stage = "caller_workspace_missing",
                            "managed harness admission rejected"
                        );
                        ManagedHarnessError::StaleOwner
                    })?
            }
            ManagedCaller::Workflow { .. } => {
                // Workflow execution admission belongs to its durable receipt/authority bridge;
                // an auth namespace is never granted merely from that session association.
                return Err(ManagedHarnessError::Unsupported);
            }
            ManagedCaller::Invocation {
                session_id,
                plugin_id,
                invocation_id,
            } => {
                // The authenticated invocation authorizes its source only, not an arbitrary
                // descendant. Creation still passes ordinary child publication admission.
                let active = state
                    .active_plugin_invocations
                    .lock()
                    .map_err(|_| ManagedHarnessError::StaleOwner)?
                    .get(&(*session_id, invocation_id.clone()))
                    .is_some_and(|entry| entry.producer_plugin_id == *plugin_id);
                if !active || (!creating && *session_id != target) {
                    return Err(ManagedHarnessError::StaleOwner);
                }
                state
                    .sessions
                    .session_summary(*session_id)
                    .await
                    .map_err(|_| ManagedHarnessError::Storage)?
                    .working_directory
            }
        };
        canonical_directory(&cwd).await
    }

    fn daemon_owner(
        state: &crate::ServerState,
        session_id: SessionId,
        generation: u64,
    ) -> Result<HarnessOwner, ManagedHarnessError> {
        let artifact_id = state
            .daemon_status
            .artifact_id
            .as_ref()
            .ok_or(ManagedHarnessError::StaleOwner)?
            .to_string();
        if generation == 0 || state.daemon_status.instance_id.is_empty() {
            return Err(ManagedHarnessError::StaleOwner);
        }
        Ok(HarnessOwner {
            session_id,
            generation,
            artifact_id,
            daemon_id: state.daemon_status.instance_id.clone(),
        })
    }

    async fn selected_workspace(
        &self,
        caller: &ManagedCaller,
        request: &bcode_ipc::harness::ManagedSelectedStartRequest,
        creating: bool,
    ) -> Result<(PathBuf, Option<crate::workflow_operations::AuthorityGuard>), ManagedHarnessError>
    {
        let target = request
            .open
            .session_id
            .parse::<SessionId>()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        if let ManagedCaller::Workflow {
            run_id,
            dispatch_identity,
            session_id,
            plugin_id,
        } = caller
        {
            let state = self.store.state()?;
            let guard = crate::harness_workflow::authorize_selected_start(
                &state,
                run_id,
                dispatch_identity,
                *session_id,
                plugin_id,
                request,
            )
            .await
            .map_err(|_| ManagedHarnessError::StaleOwner)?;
            crate::session_operations::require_selected_location(&state, target)
                .await
                .map_err(|_| ManagedHarnessError::StaleOwner)?;
            let summary = state
                .sessions
                .session_summary(target)
                .await
                .map_err(|_| ManagedHarnessError::Storage)?;
            drop(state);
            return Ok((
                canonical_directory(&summary.working_directory).await?,
                Some(guard),
            ));
        }
        Ok((self.authorize(caller, target, creating).await?, None))
    }

    /// Authorize a retained selected result without granting transport or replay authority.
    pub(super) async fn authorize_selected_history(
        &self,
        caller: &ManagedCaller,
        request: &bcode_ipc::harness::ManagedSelectedStartRequest,
    ) -> Result<(), ManagedHarnessError> {
        request
            .validate()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        let session_id = request
            .open
            .session_id
            .parse::<SessionId>()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        let (cwd, _workflow_guard) = self.selected_workspace(caller, request, false).await?;
        let state = self.store.state()?;
        let summary = state
            .sessions
            .session_summary(session_id)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?;
        if summary.parent_id != request.parent_session_id
            || canonical_directory(&summary.working_directory).await? != cwd
            || canonical_directory(Path::new(&request.open.working_directory)).await? != cwd
        {
            return Err(ManagedHarnessError::Conflict);
        }
        let selected = state
            .sessions
            .session_external_harness_state(session_id)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?;
        match selected {
            Some(selected) if selected.instance == request.instance => {}
            None if matches!(caller, ManagedCaller::Workflow { .. })
                && summary.execution.is_some() => {}
            _ => return Err(ManagedHarnessError::Conflict),
        }
        Self::enabled(&state, &request.instance)?;
        drop(state);
        Ok(())
    }

    /// Admit the immutable selected intent without performing discovery or native mutation.
    /// The selected canonical prepare step must follow before any native inspection.
    pub(super) async fn selected_open_owner(
        &self,
        caller: &ManagedCaller,
        request: &bcode_ipc::harness::ManagedSelectedStartRequest,
    ) -> Result<HarnessOwner, ManagedHarnessError> {
        request
            .validate()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        if request.open.parent_assistance.is_some() {
            return Err(ManagedHarnessError::Unsupported);
        }
        let session_id = request
            .open
            .session_id
            .parse::<SessionId>()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        let (cwd, _workflow_guard) = self.selected_workspace(caller, request, true).await?;
        if canonical_directory(Path::new(&request.open.working_directory)).await? != cwd {
            return Err(ManagedHarnessError::StaleOwner);
        }
        match caller {
            ManagedCaller::Invocation {
                session_id: source, ..
            } if request.parent_session_id != Some(*source) => {
                return Err(ManagedHarnessError::StaleOwner);
            }
            ManagedCaller::Client(_) => {
                if let Some(parent) = request.parent_session_id {
                    self.authorize(caller, parent, false).await?;
                }
            }
            ManagedCaller::Workflow { .. } | ManagedCaller::Invocation { .. } => {}
        }
        let state = self.store.state()?;
        Self::enabled(&state, &request.instance)?;
        let owner = Self::daemon_owner(&state, session_id, 1)?;
        super::configuration_views::pin_creation(&state, caller, &owner, &request.open.open_id)
            .await?;
        let root = state
            .sessions
            .session_store_root()
            .ok_or(ManagedHarnessError::Storage)?;
        initialize_primary_sessions_root(&root).await?;
        crate::session_operations::require_selected_location(&state, session_id)
            .await
            .map_err(|_| ManagedHarnessError::StaleOwner)?;
        drop(state);
        Ok(owner)
    }

    async fn prepare_open(
        &self,
        caller: &ManagedCaller,
        call: &ManagedSessionCall,
    ) -> Result<HarnessOwner, ManagedHarnessError> {
        call.harness
            .validate()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        let HarnessRequestKind::Open(open) = &call.harness.kind else {
            return Err(ManagedHarnessError::Invalid);
        };
        if open.resume.is_some()
            || call.assistance_destination_session_id.is_some()
            || call.harness.request_id != open.open_id
            || open.generation != 1
        {
            return Err(ManagedHarnessError::Invalid);
        }
        let session_id = target(call)?;
        let cwd = self.authorize(caller, session_id, true).await?;
        if canonical_directory(Path::new(&open.working_directory)).await? != cwd {
            tracing::debug!(
                stage = "create_workspace_mismatch",
                "managed harness admission rejected"
            );
            return Err(ManagedHarnessError::StaleOwner);
        }
        let parent_id = call
            .parent_session_id
            .as_deref()
            .map(str::parse::<SessionId>)
            .transpose()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        if parent_id == Some(session_id) {
            return Err(ManagedHarnessError::Invalid);
        }
        match caller {
            ManagedCaller::Invocation {
                session_id: source, ..
            } if parent_id != Some(*source) => {
                return Err(ManagedHarnessError::StaleOwner);
            }
            ManagedCaller::Client(_) => {
                if let Some(parent) = parent_id {
                    let parent_cwd = self.authorize(caller, parent, false).await?;
                    let state = self.store.state()?;
                    let summary = state
                        .sessions
                        .session_summary(parent)
                        .await
                        .map_err(|_| ManagedHarnessError::Storage)?;
                    drop(state);
                    if canonical_directory(&summary.working_directory).await? != parent_cwd {
                        return Err(ManagedHarnessError::StaleOwner);
                    }
                }
            }
            _ => {}
        }
        let state = self.store.state()?;
        Self::enabled(&state, &call.harness.instance)?;
        let owner = Self::daemon_owner(&state, session_id, 1)?;
        super::configuration_views::pin_creation(&state, caller, &owner, &open.open_id).await?;
        // Bootstrap only the selected primary sessions leaf, never readable locations or
        // missing ancestors. exact_location_claims intentionally rejects unavailable roots;
        // on a new installation the primary leaf has simply never been published yet.
        let root = state
            .sessions
            .session_store_root()
            .ok_or(ManagedHarnessError::Storage)?;
        initialize_primary_sessions_root(&root).await?;
        crate::session_operations::require_selected_location(&state, session_id)
            .await
            .map_err(|failure| {
                tracing::debug!(stage = "create_location", code = %failure.code, "managed harness admission rejected");
                ManagedHarnessError::StaleOwner
            })?;
        drop(state);
        if let Some(grant) = &open.parent_assistance {
            let _assistance = self.store.admitted_assistance(&owner, grant).await?;
        }
        // Exact retries are resolved from retained selection before reinspection. Do not change
        // capabilities/config selection after native delivery may have happened.
        if Box::pin(self.store.prepared_open(&owner, &call.harness, parent_id)).await? {
            return Ok(owner);
        }
        let inspection = self.inspect_creation(call).await?;
        Box::pin(
            self.store
                .prepare_open(&owner, &call.harness, &inspection, parent_id),
        )
        .await?;
        Ok(owner)
    }

    pub(super) async fn inspect_selected_creation(
        &self,
        request: &bcode_ipc::harness::ManagedSelectedStartRequest,
    ) -> Result<bcode_harness_models::HarnessInspection, ManagedHarnessError> {
        let call = ManagedSessionCall {
            session_id: None,
            harness: HarnessRequest {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: request.open.open_id.clone(),
                instance: request.instance.clone(),
                kind: HarnessRequestKind::Open(request.open.clone()),
            },
            parent_session_id: request.parent_session_id.map(|id| id.to_string()),
            assistance_destination_session_id: None,
        };
        // This legacy-shaped object is used only for read-only inspection. It is never
        // prepared, claimed or delivered as Open; selected persistence owns those steps.
        self.inspect_creation(&call).await
    }

    async fn inspect_creation(
        &self,
        call: &ManagedSessionCall,
    ) -> Result<bcode_harness_models::HarnessInspection, ManagedHarnessError> {
        let inspection_request = HarnessRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: call.harness.request_id.clone(),
            instance: call.harness.instance.clone(),
            kind: HarnessRequestKind::Inspect,
        };
        let state = self.store.state()?;
        let session_id = match &call.harness.kind {
            HarnessRequestKind::Open(open) => open
                .session_id
                .parse()
                .map_err(|_| ManagedHarnessError::Invalid)?,
            _ => return Err(ManagedHarnessError::Invalid),
        };
        let owner = Self::daemon_owner(&state, session_id, 1)?;
        let inspection: HarnessServiceResponse = super::HarnessPluginTransport::with_view(
            Arc::clone(&state),
            super::configuration_views::for_owner(&state, &owner).await?,
        )
        .invoke(&inspection_request)
        .await?;
        drop(state);
        inspection
            .validate_for(&inspection_request)
            .map_err(|_| ManagedHarnessError::Invalid)?;
        let HarnessServiceResponseKind::Inspection(inspection) = inspection.kind else {
            return Err(ManagedHarnessError::Unavailable);
        };
        // The plugin, not the host, interprets executable/config/account namespace semantics.
        // It must return the exact admitted executable provenance and namespace.
        if inspection.executable_identity != call.harness.instance.executable {
            return Err(ManagedHarnessError::Conflict);
        }
        Ok(inspection)
    }

    fn enabled(
        state: &crate::ServerState,
        instance: &HarnessInstance,
    ) -> Result<(), ManagedHarnessError> {
        instance
            .validate()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        let manifest = state
            .plugins
            .registry()
            .manifests()
            .get(&instance.plugin_id)
            .ok_or(ManagedHarnessError::Unavailable)?;
        bcode_plugin::harness::harness_contribution(manifest)
            .map_err(|_| ManagedHarnessError::Unavailable)?;
        Ok(())
    }
}

pub(super) fn validate_retained_target(
    selected: &bcode_session_models::external_harness::ExternalHarnessState,
    supplied: &bcode_harness_models::HarnessSessionTarget,
) -> Result<(), ManagedHarnessError> {
    use bcode_harness_models::HarnessSessionTarget;
    if selected.target.as_ref() == Some(supplied) {
        return Ok(());
    }
    // Historical Prepared requests remain readable after the same open is confirmed.
    // This permits no dispatch: the complete retained request must still match afterward.
    let (HarnessSessionTarget::Prepared(prepared), Some(HarnessSessionTarget::Confirmed(binding))) =
        (supplied, selected.target.as_ref())
    else {
        return Err(ManagedHarnessError::Conflict);
    };
    supplied
        .validate_confirmation(&prepared.open_id, binding)
        .map_err(|_| ManagedHarnessError::Conflict)?;
    if !matches!(selected.prepared_open.as_ref().map(|request| &request.kind),
        Some(bcode_session_models::external_harness::ExternalHarnessRequestKind::Open(open))
            if open.open_id == prepared.open_id)
    {
        return Err(ManagedHarnessError::Conflict);
    }
    Ok(())
}

fn apply_auth_inspection(
    admission: &mut crate::harness_dispatch::ManagedAuthAdmission,
    inspected: &super::InspectedHarnessAuth,
) -> Result<(), ManagedHarnessError> {
    // Reset first: negative/missing observations cannot preserve an older begin approval.
    admission.fresh_begin_authorized = false;
    admission.scope.method.clear();
    if inspected.status.namespace != admission.scope.namespace
        || inspected.status.custody != admission.scope.custody
    {
        return Err(ManagedHarnessError::StaleOwner);
    }
    if let Some(evidence) = &inspected.admission {
        evidence.validate_for(&admission.instance, &inspected.status)?;
        admission.scope.method.clone_from(&evidence.method_id);
        admission.fresh_begin_authorized = evidence.permits_fresh_begin(&inspected.status);
    }
    Ok(())
}

/// Initialize only a new primary sessions leaf under an already existing state root.
/// No ancestor creation: an unavailable selected volume must not become a replacement store.
async fn initialize_primary_sessions_root(root: &Path) -> Result<(), ManagedHarnessError> {
    match tokio::fs::metadata(root).await {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) | Err(_) => return Err(ManagedHarnessError::Storage),
    }
    match tokio::fs::create_dir(root).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another admitted creator may have published the same leaf concurrently.
            if tokio::fs::metadata(root)
                .await
                .map_err(|_| ManagedHarnessError::Storage)?
                .is_dir()
            {
                Ok(())
            } else {
                Err(ManagedHarnessError::Storage)
            }
        }
        Err(_) => Err(ManagedHarnessError::Storage),
    }
}

pub(super) fn target(call: &ManagedSessionCall) -> Result<SessionId, ManagedHarnessError> {
    let identity: &str = match &call.harness.kind {
        HarnessRequestKind::Open(value) => &value.session_id,
        HarnessRequestKind::Submit { submission, .. } => &submission.session_id,
        HarnessRequestKind::Respond { response, .. } => &response.session_id,
        HarnessRequestKind::Observe(value) => value.target.session_id(),
        HarnessRequestKind::Interrupt(value) | HarnessRequestKind::Close(value) => {
            value.target.session_id()
        }
        HarnessRequestKind::Configure(value) => value.target.session_id(),
        HarnessRequestKind::Reconcile(value) => &value.session_id,
        HarnessRequestKind::Inspect => return call.session_id.ok_or(ManagedHarnessError::Invalid),
        _ => return Err(ManagedHarnessError::Unsupported),
    };
    let id = identity.parse().map_err(|_| ManagedHarnessError::Invalid)?;
    if call.session_id.is_some_and(|supplied| supplied != id) {
        return Err(ManagedHarnessError::Conflict);
    }
    Ok(id)
}

pub(super) async fn canonical_directory(path: &Path) -> Result<PathBuf, ManagedHarnessError> {
    if !path.is_absolute() {
        return Err(ManagedHarnessError::Invalid);
    }
    let canonical = tokio::fs::canonicalize(path)
        .await
        .map_err(|_| ManagedHarnessError::Invalid)?;
    if !tokio::fs::metadata(&canonical)
        .await
        .map_err(|_| ManagedHarnessError::Invalid)?
        .is_dir()
    {
        return Err(ManagedHarnessError::Invalid);
    }
    Ok(canonical)
}

/// Install the concrete canonical store/admission pair once during daemon startup.
/// Does not inspect native executables, log in, or spend provider tokens.
/// # Errors
/// Rejects repeated installation; optional-domain storage errors remain operation-local.
pub fn install_canonical_backend(
    state: &Arc<crate::ServerState>,
) -> Result<(), ManagedHarnessError> {
    let store = CanonicalHarnessStore::new(state);
    state.install_managed_sessions(store.clone(), CanonicalManagedAdmission::new(store))
}

include!("admission_impl.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::{HarnessAuthBinding, HarnessAuthCustody, HarnessOpenRequest};

    #[test]
    fn plugin_inspection_enables_begin_only_for_exact_positive_evidence() {
        use bcode_provider_auth_models::managed::{
            AuthNamespace, AuthReadiness, ManagedAuthStatus,
        };
        let instance = call().harness.instance;
        let namespace = AuthNamespace {
            plugin_id: instance.plugin_id.clone(),
            credential_namespace: instance.auth.credential_namespace.clone(),
        };
        let mut admission = crate::harness_dispatch::ManagedAuthAdmission {
            scope: crate::harness_auth::VerifiedHarnessAuthScope {
                waiter: bcode_provider_auth::managed::AuthWaiter {
                    session_id: SessionId::new().to_string(),
                    generation: 1,
                    ownership_fence: "fixture".into(),
                },
                namespace: namespace.clone(),
                custody: instance.auth.custody,
                method: String::new(),
            },
            instance: instance.clone(),
            ownership_hold: Box::new(()),
            fresh_begin_authorized: false,
        };
        let mut inspected = super::super::InspectedHarnessAuth {
            status: ManagedAuthStatus {
                namespace,
                custody: instance.auth.custody,
                readiness: AuthReadiness::LoginRequired,
            },
            admission: Some(super::super::HarnessAuthAdmissionEvidence {
                schema_version: 1,
                instance,
                method_id: "plugin-selected-method".into(),
                native_login: super::super::HarnessNativeLoginState::Inactive,
            }),
        };
        apply_auth_inspection(&mut admission, &inspected).unwrap();
        assert!(admission.fresh_begin_authorized);
        assert_eq!(admission.scope.method, "plugin-selected-method");
        inspected.admission.as_mut().unwrap().native_login =
            super::super::HarnessNativeLoginState::Active;
        apply_auth_inspection(&mut admission, &inspected).unwrap();
        assert!(!admission.fresh_begin_authorized);
        assert_eq!(admission.scope.method, "plugin-selected-method");
        inspected.admission = None;
        apply_auth_inspection(&mut admission, &inspected).unwrap();
        assert!(!admission.fresh_begin_authorized);
        assert_eq!(admission.scope.method, "");
        inspected.status.namespace.credential_namespace = "foreign".into();
        assert_eq!(
            apply_auth_inspection(&mut admission, &inspected),
            Err(ManagedHarnessError::StaleOwner)
        );
    }

    fn call() -> ManagedSessionCall {
        ManagedSessionCall {
            session_id: None,
            parent_session_id: Some(SessionId::new().to_string()),
            assistance_destination_session_id: None,
            harness: HarnessRequest {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: "inspect".into(),
                kind: HarnessRequestKind::Inspect,
                instance: HarnessInstance {
                    instance_id: "instance".into(),
                    plugin_id: "plugin".into(),
                    executable: "/native".into(),
                    config_namespace: "/config".into(),
                    auth: HarnessAuthBinding {
                        provider_id: "auth".into(),
                        profile: "profile".into(),
                        credential_namespace: "credentials".into(),
                        custody: HarnessAuthCustody::NativeManaged,
                    },
                },
            },
        }
    }

    #[test]
    fn retained_target_allows_only_confirmation_of_the_same_prepared_open() {
        use bcode_harness_models::{
            HarnessPreparedSession, HarnessSessionBinding, HarnessSessionTarget,
        };
        let session_id = SessionId::new();
        let mut call = call();
        call.harness.kind = HarnessRequestKind::Open(HarnessOpenRequest {
            open_id: "open".into(),
            session_id: session_id.to_string(),
            generation: 1,
            working_directory: "/workspace".into(),
            resume: None,
            parent_assistance: None,
        });
        let prepared = HarnessSessionTarget::Prepared(HarnessPreparedSession {
            open_id: "open".into(),
            session_id: session_id.to_string(),
            generation: 1,
            instance_id: call.harness.instance.instance_id.clone(),
            native_version: "v1".into(),
            requested_native_session_id: Some("native".into()),
        });
        let cap = bcode_harness_models::HarnessCapability {
            support: HarnessSupportLevel::Supported,
            constraints: vec![],
        };
        let capabilities = bcode_harness_models::HarnessCapabilities {
            model_switch: bcode_harness_models::HarnessCapability::unsupported(),
            permission_mode: bcode_harness_models::HarnessCapability::unsupported(),
            resume: cap.clone(),
            steering: cap.clone(),
            approval: cap.clone(),
            questions: cap.clone(),
            assistance: cap.clone(),
            interrupt: cap.clone(),
            authoritative_settlement: cap.clone(),
            background_continuation: cap.clone(),
            authentication: cap,
        };
        let binding = HarnessSessionBinding {
            schema_version: HARNESS_SCHEMA_VERSION,
            session_id: session_id.to_string(),
            generation: 1,
            instance_id: call.harness.instance.instance_id.clone(),
            native_version: "v1".into(),
            native_session_id: "native".into(),
        };
        let mut selected = bcode_session_models::external_harness::ExternalHarnessState {
            owner: bcode_session_models::ExternalHarnessOwner {
                session_id,
                generation: 1,
                artifact_id: "old-artifact".into(),
                daemon_id: "old-daemon".into(),
            },
            instance: call.harness.instance.clone(),
            capabilities,
            binding: Some(binding.clone()),
            target: Some(HarnessSessionTarget::Confirmed(binding)),
            observation_after: None,
            prepared_open: Some(
                bcode_session_models::external_harness::ExternalHarnessRequest::from_request(
                    &call.harness,
                )
                .unwrap(),
            ),
        };
        assert_eq!(validate_retained_target(&selected, &prepared), Ok(()));
        let original = selected.target.clone();
        for changed in 0..4 {
            let Some(HarnessSessionTarget::Confirmed(mut binding)) = original.clone() else {
                unreachable!()
            };
            match changed {
                0 => binding.native_session_id = "different-native".into(),
                1 => binding.native_version = "different-version".into(),
                2 => binding.generation += 1,
                _ => binding.session_id = SessionId::new().to_string(),
            }
            selected.target = Some(HarnessSessionTarget::Confirmed(binding));
            assert!(validate_retained_target(&selected, &prepared).is_err());
        }
        selected.target = original;
        selected.prepared_open = None;
        assert_eq!(
            validate_retained_target(&selected, &prepared),
            Err(ManagedHarnessError::Conflict)
        );
    }

    #[test]
    fn explicit_inspection_target_is_not_inferred_from_parent() {
        let mut request = call();
        assert_eq!(target(&request), Err(ManagedHarnessError::Invalid));
        let id = SessionId::new();
        request.session_id = Some(id);
        assert_eq!(target(&request), Ok(id));
        request.harness.kind = HarnessRequestKind::Open(HarnessOpenRequest {
            open_id: "open".into(),
            session_id: SessionId::new().to_string(),
            generation: 1,
            working_directory: "/workspace".into(),
            resume: None,
            parent_assistance: None,
        });
        assert_eq!(target(&request), Err(ManagedHarnessError::Conflict));
    }

    #[tokio::test]
    async fn fresh_primary_sessions_leaf_is_initialized_without_creating_a_target() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("sessions");
        initialize_primary_sessions_root(&root).await.unwrap();
        initialize_primary_sessions_root(&root).await.unwrap();
        assert!(root.is_dir());
        assert!(!root.join(SessionId::new().to_string()).exists());
    }

    #[tokio::test]
    async fn primary_initialization_does_not_recreate_missing_ancestors_or_replace_files() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("unavailable-volume").join("sessions");
        assert_eq!(
            initialize_primary_sessions_root(&missing).await,
            Err(ManagedHarnessError::Storage)
        );
        assert!(!directory.path().join("unavailable-volume").exists());
        let file = directory.path().join("sessions");
        tokio::fs::write(&file, b"not a directory").await.unwrap();
        assert_eq!(
            initialize_primary_sessions_root(&file).await,
            Err(ManagedHarnessError::Storage)
        );
        assert_eq!(tokio::fs::read(file).await.unwrap(), b"not a directory");
    }

    #[tokio::test]
    async fn workspace_must_be_an_existing_absolute_directory() {
        assert_eq!(
            canonical_directory(Path::new("relative")).await,
            Err(ManagedHarnessError::Invalid)
        );
        let temporary = tempfile::tempdir().unwrap();
        let canonical = canonical_directory(temporary.path()).await.unwrap();
        assert!(canonical.is_absolute());
        let file = temporary.path().join("not-a-directory");
        std::fs::write(&file, b"fixture").unwrap();
        assert_eq!(
            canonical_directory(&file).await,
            Err(ManagedHarnessError::Invalid)
        );
    }
}

#[cfg(test)]
mod credential_admission_tests {
    use super::*;

    #[tokio::test]
    async fn mismatched_credential_selection_is_rejected_before_owner_or_vault_access() {
        let state = Arc::new(crate::tests::test_server_state(
            crate::SessionManager::default(),
        ));
        let admission = CanonicalManagedAdmission::new(CanonicalHarnessStore::new(&state));
        let session_id = SessionId::new();
        let owner = HarnessOwner {
            session_id,
            generation: 1,
            artifact_id: "fixture".into(),
            daemon_id: "fixture".into(),
        };
        let call = ManagedSessionCall {
            session_id: Some(session_id),
            parent_session_id: None,
            assistance_destination_session_id: None,
            harness: HarnessRequest {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: "inspect".into(),
                kind: HarnessRequestKind::Inspect,
                instance: HarnessInstance {
                    instance_id: "api".into(),
                    plugin_id: "test.api".into(),
                    executable: "/native".into(),
                    config_namespace: "/config".into(),
                    auth: bcode_harness_models::HarnessAuthBinding {
                        provider_id: "test-api".into(),
                        profile: "profile".into(),
                        credential_namespace: "namespace".into(),
                        custody: bcode_harness_models::HarnessAuthCustody::VaultManaged,
                    },
                },
            },
        };
        let caller = ManagedCaller::Client(bcode_session_models::ClientId::new());
        let selection = bcode_plugin_sdk::PluginCredentialSelection {
            instance_id: "api".into(),
            provider_id: "test-api".into(),
            profile: "profile".into(),
            credential_namespace: "namespace".into(),
            custody: bcode_harness_models::HarnessAuthCustody::VaultManaged,
            credential_id: "api_key".into(),
        };
        for field in 0..5 {
            let mut wrong = selection.clone();
            match field {
                0 => wrong.instance_id = "other".into(),
                1 => wrong.provider_id = "other".into(),
                2 => wrong.profile = "other".into(),
                3 => wrong.credential_namespace = "other".into(),
                _ => wrong.custody = bcode_harness_models::HarnessAuthCustody::NativeManaged,
            }
            assert!(matches!(
                admission.credential(&caller, &owner, &call, &wrong).await,
                Err(ManagedHarnessError::StaleOwner)
            ));
        }
        // Exact strings still grant nothing when the caller/current owner is absent.
        for caller in [
            caller,
            ManagedCaller::Invocation {
                session_id,
                plugin_id: "test.api".into(),
                invocation_id: "unregistered".into(),
            },
            ManagedCaller::Workflow {
                run_id: "unadmitted".into(),
                dispatch_identity: "unadmitted".into(),
                session_id,
                plugin_id: "test.api".into(),
            },
        ] {
            assert!(
                admission
                    .credential(&caller, &owner, &call, &selection)
                    .await
                    .is_err()
            );
        }
        drop(admission);
        drop(state);
    }
}
