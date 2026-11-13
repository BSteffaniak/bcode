//! Central managed-session dispatch.
//!
//! All entry points retain authenticated caller context.
//! Canonical mutation storage/admission is injected together; absence is a hard failure,
//! never an in-memory substitute or a direct native-service fallback.
use crate::harness::{
    HarnessOwner, HarnessPluginTransport, ManagedHarnessError, ManagedHarnessRuntime,
    ManagedHarnessStore,
};
use bcode_harness_models::{HarnessRequestKind, HarnessServiceResponse};
use bcode_ipc::harness::{
    MANAGED_SESSION_INTERFACE_ID, MANAGED_SESSION_VERSION, ManagedSessionCall,
    ManagedSessionOperation,
};
use bcode_ipc::{ErrorResponse, ManagedSessionRequest, ManagedSessionResponse};
use bcode_session_models::{ClientId, SessionId};
use std::{future::Future, pin::Pin, sync::Arc};

/// Sendable application operation tied to the caller/admission borrow.
pub type ManagedSessionTask<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ManagedHarnessError>> + Send + 'a>>;
type Task<'a, T> = ManagedSessionTask<'a, T>;

/// Identity retained by the daemon bridge, never decoded from a plugin payload.
#[derive(Debug, Clone)]
pub enum ManagedCaller {
    /// Authenticated IPC connection.
    Client(ClientId),
    /// Tool/command/workflow invocation with its exact host-authorized session.
    Invocation {
        session_id: SessionId,
        plugin_id: String,
        invocation_id: String,
    },
    /// Workflow-owned application call; exact grant is checked against the retained
    /// dispatch receipt and current workflow authority, not inferred from session parentage.
    Workflow {
        run_id: String,
        dispatch_identity: String,
        session_id: SessionId,
        plugin_id: String,
    },
}

/// A current canonical lease retained through native auth dispatch and response handling.
/// Admission must supply its real ownership guard, not a process-local lock.
pub struct ManagedAuthAdmission {
    /// Host-verified auth namespace, waiter, custody and method.
    pub scope: crate::harness_auth::VerifiedHarnessAuthScope,
    /// Exact enabled plugin/executable/config binding.
    pub instance: bcode_harness_models::HarnessInstance,
    /// Canonical ownership guard retained until this operation finishes.
    pub ownership_hold: Box<dyn Send + Sync>,
    /// Positive host evidence that this credential namespace has no unresolved native login
    /// from an earlier daemon. Ready/login-required status alone is not inactivity proof.
    pub fresh_begin_authorized: bool,
}

/// Canonical admission paired with the runtime store.
///
/// Implementations must verify active authorization, workspace and exact plugin/config
/// selection on every call. Live delivery additionally requires the current durable lease;
/// reading exact retained outcomes never transfers that lease.
/// Parent or assistance hints are never execution/response/interrupt authority. The only
/// parent-relative control is delegated control: the active authenticated tool invocation of
/// a child's immutable canonical parent session may address that existing managed child,
/// subject to the same per-operation policy, ownership, generation and retained-target checks.
pub trait ManagedSessionAdmission: Send + Sync {
    /// Read exact retained mutation evidence after current caller, workspace, plugin/config,
    /// parent and target-association authorization, but without acquiring a live owner.
    /// Returns only a canonical response or explicit uncertainty; this grants no execution
    /// authority. Conflicting request content and unverifiable canonical storage must fail.
    /// `None` means normal live-owner admission is still required, never permission to replay.
    fn retained_result<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        call: &'a ManagedSessionCall,
    ) -> Task<'a, Option<HarnessServiceResponse>>;
    /// Authorize an existing operation and resolve its current durable owner. The supplied
    /// instance and any native binding must equal canonical state, not replace it.
    fn existing<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        call: &'a ManagedSessionCall,
    ) -> Task<'a, HarnessOwner>;
    /// Create an ordinary canonical top-level/child session and durable pre-open claim.
    /// Must compare the retained open ID before creating; never replay a lost native open.
    fn create<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        call: &'a ManagedSessionCall,
    ) -> Task<'a, HarnessOwner>;
    /// Prepare exact selected creation with canonical conflict detection before native I/O.
    fn create_selected<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        request: &'a bcode_ipc::harness::ManagedSelectedStartRequest,
    ) -> Task<'a, HarnessOwner> {
        let _ = (caller, request);
        Box::pin(async { Err(ManagedHarnessError::Unsupported) })
    }

    /// Resolve current auth scope from canonical bindings, not a client profile or generation.
    fn auth_scope<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        session_id: SessionId,
        instance_id: &'a str,
    ) -> Task<'a, ManagedAuthAdmission>;

    /// Authorize an invocation-scoped read of a delegated child's retained routing evidence
    /// and canonical history. Grants no mutation, replay or transport authority.
    fn delegated_read<'a>(&'a self, caller: &'a ManagedCaller, child: SessionId) -> Task<'a, ()> {
        let _ = (caller, child);
        Box::pin(async { Err(ManagedHarnessError::Unsupported) })
    }
}

/// Object-safe application adapter. Only typed managed operations cross this boundary.
pub trait ManagedSessionBackend: Send + Sync {
    /// Close only transports admitted by this daemon, preserving ambiguous delivery evidence.
    fn shutdown(&self) -> Task<'_, ()>;
    /// Dispatch through the canonical runtime, after caller admission.
    fn execute<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        call: ManagedSessionCall,
    ) -> Task<'a, HarnessServiceResponse>;
    /// Deliver a resolved selected creation through canonical admission and selected claim.
    fn execute_selected<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        request: bcode_ipc::harness::ManagedSelectedStartRequest,
        selection: bcode_model::execution::ResolvedExecutionModel,
    ) -> Task<'a, bcode_ipc::harness::ManagedSelectedStartResponse> {
        let _ = (caller, request, selection);
        Box::pin(async { Err(ManagedHarnessError::Unsupported) })
    }

    /// Send the oldest input queued for `session_id` if its native agent is idle. Returns
    /// whether one was sent (or retired), so the caller can look again promptly.
    fn release_queued_input<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        session_id: SessionId,
    ) -> Task<'a, bool> {
        let _ = (caller, session_id);
        Box::pin(async { Ok(false) })
    }

    /// Retain an explicitly recovered current owner for normal shutdown cleanup.
    /// Implementations without canonical runtime ownership must reject this operation.
    fn retain_recovery_owner(&self, owner: HarnessOwner) -> Task<'_, ()> {
        let _ = owner;
        Box::pin(async { Err(ManagedHarnessError::Unsupported) })
    }
    /// Deliver an already-CAS-published exact resume under renewed caller admission.
    fn resume_recovery<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        owner: HarnessOwner,
        call: ManagedSessionCall,
    ) -> Task<'a, HarnessServiceResponse> {
        let _ = (caller, owner, call);
        Box::pin(async { Err(ManagedHarnessError::Unsupported) })
    }
    /// Read a delegated child's exact retained routing target for its parent's invocation.
    /// Returns `None` only when no canonical session with that identity exists yet.
    fn delegated_binding<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        request: &'a bcode_harness_models::ManagedDelegationBindingRequest,
    ) -> Task<'a, bcode_harness_models::ManagedDelegationBindingResponse> {
        let _ = (caller, request);
        Box::pin(async { Err(ManagedHarnessError::Unsupported) })
    }
    /// Read one bounded canonical history page of a delegated child for its parent's
    /// invocation, after a best-effort bounded native observation drain.
    fn delegated_history<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        request: &'a bcode_harness_models::ManagedDelegationHistoryRequest,
    ) -> Task<'a, bcode_harness_models::HarnessObservationPage> {
        let _ = (caller, request);
        Box::pin(async { Err(ManagedHarnessError::Unsupported) })
    }
    /// Poll canonical native observations for an admitted interactive attachment, letting the
    /// adapter hold the page for up to `wait_millis`. Returns the committed page's progress,
    /// counting durable observations, hydration requests and transient live updates.
    fn poll_observations<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        query: &'a bcode_ipc::harness::ManagedSessionPollRequest,
        wait_millis: u32,
    ) -> Task<'a, usize> {
        let _ = (caller, query, wait_millis);
        Box::pin(async { Err(ManagedHarnessError::Unsupported) })
    }
    /// Admit an interactive attachment, drain once, then keep ingesting native observations in
    /// the daemon for as long as that client stays admitted. Committed pages reach the client
    /// through its ordinary session event subscription.
    fn watch_observations<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        query: &'a bcode_ipc::harness::ManagedSessionPollRequest,
    ) -> Task<'a, ()> {
        let _ = (caller, query);
        Box::pin(async { Err(ManagedHarnessError::Unsupported) })
    }
    /// Managed sessions whose committed evidence shows live native foreground work or
    /// unsettled background work. Idle shutdown is blocked while this is nonzero.
    fn busy_native_sessions(&self) -> usize {
        0
    }
    /// Resolve auth from the same admitted canonical runtime.
    fn auth_scope<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        session_id: SessionId,
        instance_id: &'a str,
    ) -> Task<'a, ManagedAuthAdmission>;
}

impl std::fmt::Debug for dyn ManagedSessionBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedSessionBackend { .. }")
    }
}

use bcode_harness_models::{
    HARNESS_RECOVERY_INTERFACE_ID as NATIVE_RECOVERY_INTERFACE, HARNESS_RECOVERY_SCHEMA_VERSION,
    HarnessRecoveryRequest as NativeRecoveryRequest,
    HarnessRecoveryRequestKind as NativeRecoveryKind,
    HarnessRecoveryResponse as NativeRecoveryResponse,
    HarnessRecoveryResponseKind as NativeRecoveryResult,
};

fn validate_native_recovery_request(
    request: &NativeRecoveryRequest,
) -> Result<(), ManagedHarnessError> {
    if request.schema_version != HARNESS_RECOVERY_SCHEMA_VERSION
        || request.request_id.is_empty()
        || request.request_id.len() > bcode_harness_models::MAX_HARNESS_ID_BYTES
        || request.request_id.chars().any(char::is_control)
    {
        return Err(ManagedHarnessError::Invalid);
    }
    let kind = match &request.kind {
        NativeRecoveryKind::PrepareResume { request: open } if open.resume.is_some() => {
            HarnessRequestKind::Open(open.clone())
        }
        NativeRecoveryKind::PrepareResume { .. } => return Err(ManagedHarnessError::Invalid),
        NativeRecoveryKind::ReleaseTransport { request: control } => {
            HarnessRequestKind::Close(control.clone())
        }
    };
    bcode_harness_models::HarnessRequest {
        schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
        request_id: request.request_id.clone(),
        instance: request.instance.clone(),
        kind,
    }
    .validate()
    .map_err(|_| ManagedHarnessError::Invalid)
}

fn validate_native_recovery_result(
    request: &NativeRecoveryRequest,
    response: NativeRecoveryResponse,
) -> Result<NativeRecoveryResult, ManagedHarnessError> {
    validate_native_recovery_request(request)?;
    if response.schema_version != HARNESS_RECOVERY_SCHEMA_VERSION
        || response.request_id != request.request_id
    {
        return Err(ManagedHarnessError::Invalid);
    }
    match (&request.kind, &response.kind) {
        (
            NativeRecoveryKind::PrepareResume { request: expected },
            NativeRecoveryResult::ResumeValidated { request: actual },
        ) if expected == actual => {}
        (
            NativeRecoveryKind::ReleaseTransport { request: expected },
            NativeRecoveryResult::TransportReleased {
                target,
                mutation_id,
                reap_id,
            },
        ) if target == &expected.target
            && mutation_id == &expected.mutation_id
            && !reap_id.is_empty()
            && reap_id.len() <= bcode_harness_models::MAX_HARNESS_ID_BYTES
            && !reap_id.chars().any(char::is_control) => {}
        _ => return Err(ManagedHarnessError::Invalid),
    }
    Ok(response.kind)
}

async fn native_recovery_service(
    state: &Arc<crate::ServerState>,
    request: &NativeRecoveryRequest,
) -> Result<NativeRecoveryResult, ManagedHarnessError> {
    validate_native_recovery_request(request)?;
    let operation = match &request.kind {
        NativeRecoveryKind::PrepareResume { .. } => "prepare_resume",
        NativeRecoveryKind::ReleaseTransport { .. } => "release_transport",
    };
    // No vendor fallback or generic arbitrary service route. Exactly one declared
    // service on the canonically selected enabled plugin is required.
    if state
        .plugins
        .service_summaries()
        .iter()
        .filter(|(plugin, service)| {
            plugin == &request.instance.plugin_id
                && service.interface_id == NATIVE_RECOVERY_INTERFACE
        })
        .count()
        != 1
    {
        return Err(ManagedHarnessError::Unsupported);
    }
    let payload = serde_json::to_vec(request).map_err(|_| ManagedHarnessError::Invalid)?;
    if payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES {
        return Err(ManagedHarnessError::Invalid);
    }
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        state.plugins.invoke_service_with_bridge_scoped(
            &request.instance.plugin_id,
            NATIVE_RECOVERY_INTERFACE,
            operation,
            payload,
            bcode_plugin::PluginInvocationScope::session(match &request.kind {
                NativeRecoveryKind::PrepareResume { request: open } => open.session_id.clone(),
                NativeRecoveryKind::ReleaseTransport { request: control } => {
                    control.target.session_id().to_owned()
                }
            }),
            None,
        ),
    )
    .await
    .map_err(|_| ManagedHarnessError::Uncertain)?
    .map_err(|_| ManagedHarnessError::Unavailable)?;
    if response.error.is_some()
        || response.payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES
    {
        return Err(ManagedHarnessError::Unavailable);
    }
    let response: NativeRecoveryResponse =
        serde_json::from_slice(&response.payload).map_err(|_| ManagedHarnessError::Invalid)?;
    validate_native_recovery_result(request, response)
}

async fn record_shutdown_transport_release(
    state: &Arc<crate::ServerState>,
    owner: &HarnessOwner,
    mutation_id: String,
    response: &HarnessServiceResponse,
) -> Result<(), ManagedHarnessError> {
    if !matches!(&response.kind, bcode_harness_models::HarnessServiceResponseKind::Acknowledged { mutation_id: acknowledged }
        if acknowledged == &mutation_id)
    {
        return Err(ManagedHarnessError::Uncertain);
    }
    let store = crate::harness::CanonicalHarnessStore::new(state);
    let _hold = store.acquire(owner).await?;
    let selected = store.selected(owner).await?;
    let Some(bcode_harness_models::HarnessSessionTarget::Confirmed(binding)) = selected.target
    else {
        // Prepared-only sessions cannot be resumed and receive no fabricated native identity.
        return Ok(());
    };
    let target = bcode_harness_models::HarnessSessionTarget::Confirmed(binding.clone());
    let request = NativeRecoveryRequest {
        schema_version: 1,
        request_id: mutation_id.clone(),
        instance: selected.instance.clone(),
        kind: NativeRecoveryKind::ReleaseTransport {
            request: bcode_harness_models::HarnessControlRequest {
                target: target.clone(),
                mutation_id: mutation_id.clone(),
            },
        },
    };
    let native = native_recovery_service(state, &request).await?;
    let NativeRecoveryResult::TransportReleased {
        target: released,
        mutation_id: completed,
        reap_id,
    } = native
    else {
        return Err(ManagedHarnessError::Invalid);
    };
    if released != target || completed != mutation_id {
        return Err(ManagedHarnessError::Invalid);
    }
    let release = bcode_session_models::external_harness::ExternalHarnessTransportRelease {
        close_request_id: mutation_id,
        instance: selected.instance,
        binding,
        lifetime: store.release_lifetime_provenance()?,
        reap_id,
    };
    let expected = release.clone();
    drop(store);
    state
        .sessions
        .release_external_harness_transport(&selected.owner, release, move |actual| {
            if actual == &expected {
                Ok(())
            } else {
                Err("physical release proof changed".into())
            }
        })
        .await
        .map_err(|_| ManagedHarnessError::Storage)
}

struct DaemonTransport(std::sync::Weak<crate::ServerState>);
impl crate::harness::ManagedHarnessTransport for DaemonTransport {
    async fn invoke_selected(
        &self,
        request: &bcode_harness_models::HarnessModelsRequest,
    ) -> Result<bcode_harness_models::HarnessModelsResponse, ManagedHarnessError> {
        let state = self.0.upgrade().ok_or(ManagedHarnessError::Unavailable)?;
        HarnessPluginTransport::new(state)
            .invoke_selected(request)
            .await
    }

    async fn invoke(
        &self,
        request: &bcode_harness_models::HarnessRequest,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        let state = self.0.upgrade().ok_or(ManagedHarnessError::Unavailable)?;
        HarnessPluginTransport::new(state).invoke(request).await
    }
}

struct CanonicalBackend<S, A> {
    state: std::sync::Weak<crate::ServerState>,
    runtime: ManagedHarnessRuntime<S, DaemonTransport>,
    admission: A,
    owners: tokio::sync::Mutex<std::collections::BTreeMap<SessionId, HarnessOwner>>,
    tool_queues: crate::harness::ToolQueueHost,
    watches: crate::harness::ObservationWatches,
    /// Queued inputs being sent now; their own delivery must not be held again.
    releasing: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// Sessions whose stored queue was last seen empty; any hold clears the mark. Absent
    /// sessions (including every session after a restart) are read from storage.
    empty_queues: std::sync::Mutex<std::collections::BTreeSet<SessionId>>,
}
impl<S: ManagedHarnessStore, A: ManagedSessionAdmission> ManagedSessionBackend
    for CanonicalBackend<S, A>
{
    fn release_queued_input<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        session_id: SessionId,
    ) -> Task<'a, bool> {
        Box::pin(self.release_queued(caller, session_id))
    }

    fn execute_selected<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        request: bcode_ipc::harness::ManagedSelectedStartRequest,
        selection: bcode_model::execution::ResolvedExecutionModel,
    ) -> Task<'a, bcode_ipc::harness::ManagedSelectedStartResponse> {
        Box::pin(async move {
            request
                .validate()
                .map_err(|_| ManagedHarnessError::Invalid)?;
            if selection.requested.model_id != request.selection.model_id
                || selection.target.model.model_id != request.selection.model_id
                || Some(&selection.target.route) != request.selection.route.as_ref()
                || selection.target.native_model_id != request.selection.model_id
            {
                return Err(ManagedHarnessError::Conflict);
            }
            if request
                .selection
                .account_reference
                .as_ref()
                .is_some_and(|account| selection.target.account_reference.as_ref() != Some(account))
                || selection.requested.route != request.selection.route
                || selection.requested.account_reference != request.selection.account_reference
            {
                return Err(ManagedHarnessError::Conflict);
            }
            let owner = self.admission.create_selected(caller, &request).await?;
            let state = self
                .state
                .upgrade()
                .ok_or(ManagedHarnessError::Unavailable)?;
            {
                let mut owners = self.owners.lock().await;
                if state
                    .shutdown_requested
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    return Err(ManagedHarnessError::Unavailable);
                }
                owners.insert(owner.session_id, owner.clone());
            }
            let credential_call = ManagedSessionCall {
                session_id: None,
                harness: bcode_harness_models::HarnessRequest {
                    schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
                    request_id: request.open.open_id.clone(),
                    instance: request.instance.clone(),
                    kind: HarnessRequestKind::Open(request.open.clone()),
                },
                parent_session_id: request.parent_session_id.map(|id| id.to_string()),
                assistance_destination_session_id: None,
            };
            // Used only for existing tool/credential admission, never legacy Open delivery.
            self.admit_tool_queue(&state, caller, &owner, &credential_call)
                .await?;
            drop(state);
            let native = bcode_harness_models::HarnessModelsRequest {
                schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
                request_id: request.open.open_id.clone(),
                instance: request.instance,
                kind: bcode_harness_models::HarnessModelsRequestKind::OpenSelected {
                    open: Box::new(request.open),
                    selection: bcode_harness_models::HarnessModelSelection {
                        model_id: selection.target.native_model_id.clone(),
                    },
                },
            };
            let response = crate::harness::with_credential_admission(
                caller.clone(),
                owner.clone(),
                credential_call,
                self.runtime.open_selected(&owner, &native),
            )
            .await?;
            let mut selection = selection;
            if let bcode_harness_models::HarnessModelsResponseKind::Selected { evidence, .. } =
                &response.kind
            {
                selection
                    .target
                    .confirmed_model_id
                    .clone_from(&evidence.effective_model_id);
            }
            Ok(bcode_ipc::harness::ManagedSelectedStartResponse {
                selection,
                response,
            })
        })
    }

    fn retain_recovery_owner(&self, owner: HarnessOwner) -> Task<'_, ()> {
        Box::pin(async move {
            self.runtime.verify_owner(&owner).await?;
            self.owners.lock().await.insert(owner.session_id, owner);
            Ok(())
        })
    }

    fn resume_recovery<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        owner: HarnessOwner,
        call: ManagedSessionCall,
    ) -> Task<'a, HarnessServiceResponse> {
        Box::pin(async move {
            if !matches!(caller, ManagedCaller::Client(_))
                || self.admission.existing(caller, &call).await? != owner
            {
                return Err(ManagedHarnessError::StaleOwner);
            }
            let state = self
                .state
                .upgrade()
                .ok_or(ManagedHarnessError::Unavailable)?;
            self.retain_recovery_owner(owner.clone()).await?;
            let credential_call = serde_json::from_value(
                serde_json::to_value(&call).map_err(|_| ManagedHarnessError::Invalid)?,
            )
            .map_err(|_| ManagedHarnessError::Invalid)?;
            self.admit_tool_queue(&state, caller, &owner, &call).await?;
            let HarnessRequestKind::Open(open) = call.harness.kind else {
                return Err(ManagedHarnessError::Invalid);
            };
            crate::harness::with_credential_admission(
                caller.clone(),
                owner.clone(),
                credential_call,
                Box::pin(self.runtime.open_validated_recovery(&owner, open)),
            )
            .await
        })
    }

    fn watch_observations<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        query: &'a bcode_ipc::harness::ManagedSessionPollRequest,
    ) -> Task<'a, ()> {
        Box::pin(async move {
            let ManagedCaller::Client(client) = caller else {
                return Err(ManagedHarnessError::Unsupported);
            };
            // Admission and the first drain run on the caller's request so failures surface.
            self.poll_observations(caller, query, 0).await?;
            self.watches
                .start(self.state.clone(), *client, query.clone());
            Ok(())
        })
    }

    fn poll_observations<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        query: &'a bcode_ipc::harness::ManagedSessionPollRequest,
        wait_millis: u32,
    ) -> Task<'a, usize> {
        Box::pin(async move {
            let ManagedCaller::Client(client) = caller else {
                return Err(ManagedHarnessError::Unsupported);
            };
            let state = self
                .state
                .upgrade()
                .ok_or(ManagedHarnessError::Unavailable)?;
            query.validate().map_err(|_| ManagedHarnessError::Invalid)?;
            let snapshot = state
                .sessions
                .session_external_harness_state(query.session_id)
                .await
                .map_err(|_| ManagedHarnessError::Storage)?
                .ok_or(ManagedHarnessError::Conflict)?;
            if snapshot.owner.generation != query.generation
                || snapshot.instance.instance_id != query.instance_id
            {
                return Err(ManagedHarnessError::StaleOwner);
            }
            let call = ManagedSessionCall {
                session_id: Some(query.session_id),
                harness: bcode_harness_models::HarnessRequest {
                    schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
                    request_id: format!("attachment-admission:{}", uuid::Uuid::new_v4()),
                    instance: snapshot.instance,
                    kind: HarnessRequestKind::Observe(
                        bcode_harness_models::HarnessObserveRequest {
                            target: snapshot.target.ok_or(ManagedHarnessError::Conflict)?,
                            // Admission shape only: the application reads the real actor cursor
                            // under its observer guard before constructing any native request.
                            retention: None,
                            after: None,
                            limit: query.limit,
                            wait_millis: 0,
                        },
                    ),
                },
                parent_session_id: None,
                assistance_destination_session_id: None,
            };
            let owner = self.admission.existing(caller, &call).await?;
            // Native observe must use the configuration view pinned for this owner, exactly as
            // the admitted execute path does; without it the transport would fall back to the
            // daemon's startup baseline, which may predate the instance's setup.
            let application = crate::harness::ManagedHarnessApplication::with_owner(
                &state,
                *client,
                &self.runtime,
                owner.clone(),
            );
            let result = Box::pin(crate::harness::with_credential_admission(
                caller.clone(),
                owner,
                call,
                application.poll_observations(query, wait_millis),
            ))
            .await;
            drop(application);
            drop(state);
            result
        })
    }

    fn delegated_binding<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        request: &'a bcode_harness_models::ManagedDelegationBindingRequest,
    ) -> Task<'a, bcode_harness_models::ManagedDelegationBindingResponse> {
        Box::pin(async move {
            request
                .validate()
                .map_err(|_| ManagedHarnessError::Invalid)?;
            let child = request
                .session_id
                .parse::<SessionId>()
                .map_err(|_| ManagedHarnessError::Invalid)?;
            if !matches!(caller, ManagedCaller::Invocation { .. }) {
                return Err(ManagedHarnessError::Unsupported);
            }
            let state = self
                .state
                .upgrade()
                .ok_or(ManagedHarnessError::Unavailable)?;
            // An exact creation retry derives the same identity before the session exists.
            // Absence reveals nothing about other sessions and grants no authority.
            match state.sessions.session_summary(child).await {
                Err(bcode_session::SessionError::NotFound(_)) => {
                    return Ok(bcode_harness_models::ManagedDelegationBindingResponse {
                        binding: None,
                    });
                }
                Err(_) => return Err(ManagedHarnessError::Storage),
                Ok(_) => {}
            }
            self.admission.delegated_read(caller, child).await?;
            let snapshot = state
                .sessions
                .session_external_harness_state(child)
                .await
                .map_err(|_| ManagedHarnessError::Storage)?
                .ok_or(ManagedHarnessError::Conflict)?;
            drop(state);
            if snapshot.owner.session_id != child {
                return Err(ManagedHarnessError::StaleOwner);
            }
            let generation = snapshot.owner.generation;
            let binding =
                snapshot
                    .target
                    .map(|target| bcode_harness_models::ManagedDelegationBinding {
                        session_id: child.to_string(),
                        instance: snapshot.instance,
                        target,
                    });
            let response = bcode_harness_models::ManagedDelegationBindingResponse { binding };
            response
                .validate_for(request)
                .map_err(|_| ManagedHarnessError::Storage)?;
            if response
                .binding
                .as_ref()
                .is_some_and(|binding| binding.target.generation() != generation)
            {
                return Err(ManagedHarnessError::StaleOwner);
            }
            Ok(response)
        })
    }

    fn delegated_history<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        request: &'a bcode_harness_models::ManagedDelegationHistoryRequest,
    ) -> Task<'a, bcode_harness_models::HarnessObservationPage> {
        Box::pin(async move {
            request
                .validate()
                .map_err(|_| ManagedHarnessError::Invalid)?;
            let child = request
                .session_id
                .parse::<SessionId>()
                .map_err(|_| ManagedHarnessError::Invalid)?;
            if !matches!(caller, ManagedCaller::Invocation { .. }) {
                return Err(ManagedHarnessError::Unsupported);
            }
            let state = self
                .state
                .upgrade()
                .ok_or(ManagedHarnessError::Unavailable)?;
            self.admission.delegated_read(caller, child).await?;
            // Best-effort observation drain: a released or unavailable native transport
            // still leaves bounded canonical history readable. Never a mutation replay.
            if let Err(error) = self
                .drain_delegated(&state, caller, child, request.limit)
                .await
            {
                tracing::debug!(target: "bcode_server::harness", category = %error, "delegated observation drain unavailable");
            }
            let page = crate::harness::canonical_history(
                &state,
                child,
                request.after.clone(),
                request.limit,
            )
            .await?;
            drop(state);
            request
                .validate_page(&page)
                .map_err(|_| ManagedHarnessError::Storage)?;
            Ok(page)
        })
    }

    fn shutdown(&self) -> Task<'_, ()> {
        Box::pin(async move {
            self.watches.shutdown();
            self.tool_queues.shutdown().await;
            let owners = std::mem::take(&mut *self.owners.lock().await);
            let closes = owners.into_values().map(|owner| async move {
                let mutation_id = format!("shutdown:{}", uuid::Uuid::new_v4());
                let response = self.runtime.close(&owner, mutation_id.clone()).await?;
                // Release evidence makes the session recoverable after restart. Its absence
                // (unsupported plugin, prepared-only target, unproven reap) never turns an
                // acknowledged Close into a shutdown failure.
                if crate::harness::recovery_published()
                    && let Some(state) = self.state.upgrade()
                    && let Err(error) =
                        record_shutdown_transport_release(&state, &owner, mutation_id, &response)
                            .await
                {
                    tracing::info!(target: "bcode_server::harness", category = %error, "managed transport release evidence unavailable; session is not recoverable");
                }
                Ok::<_, ManagedHarnessError>(response)
            });
            let results = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                futures::future::join_all(closes),
            )
            .await
            .map_err(|_| ManagedHarnessError::Uncertain)?;
            for result in results {
                result?;
            }
            Ok(())
        })
    }

    fn execute<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        call: ManagedSessionCall,
    ) -> Task<'a, HarnessServiceResponse> {
        Box::pin(async move {
            call.harness
                .validate()
                .map_err(|_| ManagedHarnessError::Invalid)?;
            let supplied_target = call_target(&call);
            let state = self
                .state
                .upgrade()
                .ok_or(ManagedHarnessError::Unavailable)?;
            let workflow_hold = if let ManagedCaller::Workflow {
                run_id,
                dispatch_identity,
                session_id,
                plugin_id,
            } = caller
            {
                Some(
                    crate::harness_workflow::authorize_call(
                        &state,
                        run_id,
                        dispatch_identity,
                        *session_id,
                        plugin_id,
                        &call,
                    )
                    .await
                    .map_err(|_| ManagedHarnessError::StaleOwner)?,
                )
            } else {
                None
            };
            // Retained mutation evidence is an authorized canonical read, not an owner
            // transfer. Keep workflow authority pinned, but do not require this daemon's
            // live native lease merely to return an identical historical outcome.
            if matches!(
                &call.harness.kind,
                HarnessRequestKind::Open(_)
                    | HarnessRequestKind::Submit { .. }
                    | HarnessRequestKind::Respond { .. }
                    | HarnessRequestKind::Interrupt(_)
                    | HarnessRequestKind::Close(_)
                    | HarnessRequestKind::Configure(_)
                    | HarnessRequestKind::Reconcile(_)
            ) && let Some(response) = self
                .admission
                .retained_result(caller, &call)
                .await
                .inspect_err(|error| reject_stage("retained_result", *error))?
            {
                response
                    .validate_for(&call.harness)
                    .map_err(|_| ManagedHarnessError::Storage)?;
                return Ok(response);
            }
            let owner = self.admit_owner(caller, &call).await?;
            if let Some(target) = supplied_target {
                target
                    .validate_owner(&owner.session_id.to_string(), owner.generation)
                    .map_err(|_| ManagedHarnessError::StaleOwner)
                    .inspect_err(|error| reject_stage("target_owner", *error))?;
            }
            {
                let mut owners = self.owners.lock().await;
                if state
                    .shutdown_requested
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    return Err(ManagedHarnessError::Unavailable);
                }
                owners.insert(owner.session_id, owner.clone());
            }
            let credential_call = serde_json::from_value(
                serde_json::to_value(&call).map_err(|_| ManagedHarnessError::Invalid)?,
            )
            .map_err(|_| ManagedHarnessError::Invalid)?;
            let failed_submit_scope = self.admit_tool_queue(&state, caller, &owner, &call).await?;
            let operation = call.harness.kind.operation();
            let release_mutation = match &call.harness.kind {
                HarnessRequestKind::Close(control) => Some(control.mutation_id.clone()),
                _ => None,
            };
            let result = crate::harness::with_credential_admission(
                caller.clone(),
                owner.clone(),
                credential_call,
                Box::pin(self.execute_admitted(&state, caller, &owner, call)),
            )
            .await;
            self.record_explicit_release(&state, &owner, release_mutation, &result)
                .await;
            if let Err(error) = &result {
                log_admitted_failure(operation, *error, failed_submit_scope.is_some());
                if let Some(scope) = failed_submit_scope {
                    self.tool_queues.revoke_exact(&state, &scope).await;
                }
            }
            drop(workflow_hold);
            drop(state);
            result
        })
    }
    fn busy_native_sessions(&self) -> usize {
        self.runtime.busy_native_sessions()
    }
    fn auth_scope<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        session_id: SessionId,
        instance_id: &'a str,
    ) -> Task<'a, ManagedAuthAdmission> {
        self.admission.auth_scope(caller, session_id, instance_id)
    }
}

const fn call_target(
    call: &ManagedSessionCall,
) -> Option<&bcode_harness_models::HarnessSessionTarget> {
    match &call.harness.kind {
        HarnessRequestKind::Submit { target, .. } | HarnessRequestKind::Respond { target, .. } => {
            Some(target)
        }
        HarnessRequestKind::Observe(value) => Some(&value.target),
        HarnessRequestKind::Interrupt(value) | HarnessRequestKind::Close(value) => {
            Some(&value.target)
        }
        HarnessRequestKind::Configure(value) => Some(&value.target),
        _ => None,
    }
}

/// Debug-level trace of the pre-admission stage that refused a managed call.
fn reject_stage(stage: &'static str, error: ManagedHarnessError) {
    tracing::debug!(target: "bcode_server::harness", stage, category = %error, "managed call refused before admission");
}

fn log_admitted_failure(
    operation: bcode_harness_models::HarnessOperation,
    error: ManagedHarnessError,
    revoke_submit_scope: bool,
) {
    let category = match error {
        ManagedHarnessError::Invalid => "invalid",
        ManagedHarnessError::Storage => "storage",
        ManagedHarnessError::StaleOwner => "stale_owner",
        ManagedHarnessError::Conflict => "conflict",
        ManagedHarnessError::Uncertain => "uncertain",
        ManagedHarnessError::Unavailable => "unavailable",
        ManagedHarnessError::Deadline => "deadline",
        ManagedHarnessError::Unsupported => "unsupported",
    };
    tracing::warn!(target: "bcode_server::harness", operation = operation.as_str(), category,
                    revoke_submit_scope, "managed admitted operation failed");
}

/// Times one queued input may be refused as busy before it is given up.
const MAX_QUEUED_INPUT_ATTEMPTS: usize = 3;

impl<S: ManagedHarnessStore, A: ManagedSessionAdmission> CanonicalBackend<S, A> {
    /// Hold next-turn input while the native agent is mid-turn, or while earlier input still
    /// waits, so input is taken in order instead of being refused as a conflict. A held
    /// submission claims nothing yet: its later delivery claims exactly these identities.
    async fn hold_while_busy(
        &self,
        state: &Arc<crate::ServerState>,
        owner: &HarnessOwner,
        request_id: &str,
        submission: &bcode_harness_models::HarnessSubmission,
    ) -> Result<Option<HarnessServiceResponse>, ManagedHarnessError> {
        if !matches!(
            submission.placement,
            bcode_harness_models::HarnessPlacement::NextTurn
        ) || self.releasing.lock().map_or(true, |releasing| {
            releasing.contains(&submission.submission_id)
        }) {
            return Ok(None);
        }
        let held = state
            .sessions
            .native_input_queue(
                owner.session_id,
                bcode_session::NativeInputQueueChange::List,
            )
            .await
            .map_err(|_| ManagedHarnessError::Storage)?;
        if !held
            .iter()
            .any(|input| input.submission_id == submission.submission_id)
        {
            if held.is_empty() && !self.runtime.native_turn_running(owner.session_id) {
                return Ok(None);
            }
            if let Ok(mut empty) = self.empty_queues.lock() {
                empty.remove(&owner.session_id);
            }
            let queued_at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| {
                    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
                });
            state
                .sessions
                .native_input_queue(
                    owner.session_id,
                    bcode_session::NativeInputQueueChange::Queue(
                        bcode_session_models::external_harness::QueuedNativeInput {
                            submission_id: submission.submission_id.clone(),
                            attempt_id: submission.attempt_id.clone(),
                            text: submission.text.clone(),
                            queued_at_ms,
                        },
                    ),
                )
                .await
                .map_err(|_| ManagedHarnessError::Storage)?;
        }
        Ok(Some(HarnessServiceResponse {
            schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
            request_id: request_id.to_owned(),
            operation: bcode_harness_models::HarnessOperation::Submit,
            kind: bcode_harness_models::HarnessServiceResponseKind::Receipt(
                bcode_harness_models::HarnessReceipt {
                    submission_id: submission.submission_id.clone(),
                    attempt_id: submission.attempt_id.clone(),
                    session_id: submission.session_id.clone(),
                    generation: submission.generation,
                    state: bcode_harness_models::HarnessDeliveryState::Accepted,
                    native_request_id: None,
                },
            ),
        }))
    }

    /// Retire a released input, re-queuing it under fresh identities if it was refused as busy.
    async fn settle_released(
        &self,
        session_id: SessionId,
        next: bcode_session_models::external_harness::QueuedNativeInput,
        retry: bool,
    ) -> Result<(), ManagedHarnessError> {
        let state = self
            .state
            .upgrade()
            .ok_or(ManagedHarnessError::Unavailable)?;
        state
            .sessions
            .native_input_queue(
                session_id,
                bcode_session::NativeInputQueueChange::Remove(next.submission_id.clone()),
            )
            .await
            .map_err(|_| ManagedHarnessError::Storage)?;
        let attempts = next.submission_id.matches(".retry").count() + 1;
        if retry && attempts < MAX_QUEUED_INPUT_ATTEMPTS {
            state
                .sessions
                .native_input_queue(
                    session_id,
                    bcode_session::NativeInputQueueChange::Queue(
                        bcode_session_models::external_harness::QueuedNativeInput {
                            submission_id: format!("{}.retry", next.submission_id),
                            attempt_id: uuid::Uuid::new_v4().to_string(),
                            text: next.text,
                            queued_at_ms: next.queued_at_ms,
                        },
                    ),
                )
                .await
                .map_err(|_| ManagedHarnessError::Storage)?;
        }
        drop(state);
        Ok(())
    }

    /// The submit call that delivers `next` to the session's current native target.
    async fn queued_submit_call(
        state: &Arc<crate::ServerState>,
        session_id: SessionId,
        next: &bcode_session_models::external_harness::QueuedNativeInput,
    ) -> Result<ManagedSessionCall, ManagedHarnessError> {
        let canonical = state
            .sessions
            .session_external_harness_state(session_id)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?
            .ok_or(ManagedHarnessError::StaleOwner)?;
        let target = canonical.target.ok_or(ManagedHarnessError::Unavailable)?;
        let submission = bcode_harness_models::HarnessSubmission {
            submission_id: next.submission_id.clone(),
            attempt_id: next.attempt_id.clone(),
            session_id: session_id.to_string(),
            generation: canonical.owner.generation,
            text: next.text.clone(),
            placement: bcode_harness_models::HarnessPlacement::NextTurn,
        };
        let call = ManagedSessionCall {
            session_id: Some(session_id),
            parent_session_id: None,
            assistance_destination_session_id: None,
            harness: bcode_harness_models::HarnessRequest {
                schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
                request_id: next.submission_id.clone(),
                instance: canonical.instance,
                kind: HarnessRequestKind::Submit { target, submission },
            },
        };
        Ok(call)
    }

    /// Send the oldest queued input once the native agent is idle, through the same admission
    /// and canonical claim as any submit. A refusal as busy re-queues it under fresh
    /// identities (the refused ones are already recorded); an uncertain outcome is never
    /// re-sent.
    async fn release_queued(
        &self,
        caller: &ManagedCaller,
        session_id: SessionId,
    ) -> Result<bool, ManagedHarnessError> {
        if self.runtime.native_turn_running(session_id)
            || self
                .empty_queues
                .lock()
                .is_ok_and(|empty| empty.contains(&session_id))
        {
            return Ok(false);
        }
        let state = self
            .state
            .upgrade()
            .ok_or(ManagedHarnessError::Unavailable)?;
        let held = state
            .sessions
            .native_input_queue(session_id, bcode_session::NativeInputQueueChange::List)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?;
        let Some(next) = held.into_iter().next() else {
            if let Ok(mut empty) = self.empty_queues.lock() {
                empty.insert(session_id);
            }
            return Ok(false);
        };
        let call = Self::queued_submit_call(&state, session_id, &next).await?;
        drop(state);
        if let Ok(mut releasing) = self.releasing.lock() {
            releasing.insert(next.submission_id.clone());
        }
        let outcome = self.execute(caller, call).await;
        if let Ok(mut releasing) = self.releasing.lock() {
            releasing.remove(&next.submission_id);
        }
        let retry = match &outcome {
            // Admission failed before any claim: keep it queued and try again later.
            Err(
                ManagedHarnessError::StaleOwner
                | ManagedHarnessError::Unavailable
                | ManagedHarnessError::Storage
                | ManagedHarnessError::Deadline,
            ) => return Ok(false),
            Ok(response) => matches!(
                response.kind,
                bcode_harness_models::HarnessServiceResponseKind::Failure(
                    bcode_harness_models::HarnessFailure::Conflict
                )
            ),
            Err(_) => false,
        };
        self.settle_released(session_id, next, retry).await?;
        Ok(true)
    }
    /// Admit the delegated child's current owner exactly as an Observe call, then drain one
    /// bounded native page through the observer guard under credential admission.
    async fn drain_delegated(
        &self,
        state: &Arc<crate::ServerState>,
        caller: &ManagedCaller,
        child: SessionId,
        limit: u16,
    ) -> Result<usize, ManagedHarnessError> {
        let snapshot = state
            .sessions
            .session_external_harness_state(child)
            .await
            .map_err(|_| ManagedHarnessError::Storage)?
            .ok_or(ManagedHarnessError::Conflict)?;
        let call = ManagedSessionCall {
            session_id: Some(child),
            harness: bcode_harness_models::HarnessRequest {
                schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
                request_id: format!("delegated-admission:{}", uuid::Uuid::new_v4()),
                instance: snapshot.instance,
                kind: HarnessRequestKind::Observe(bcode_harness_models::HarnessObserveRequest {
                    target: snapshot.target.ok_or(ManagedHarnessError::Conflict)?,
                    // Admission shape only: the drain reads the real committed actor cursor.
                    retention: None,
                    after: None,
                    limit,
                    wait_millis: 0,
                }),
            },
            parent_session_id: None,
            assistance_destination_session_id: None,
        };
        let owner = self.admission.existing(caller, &call).await?;
        Box::pin(crate::harness::with_credential_admission(
            caller.clone(),
            owner.clone(),
            call,
            crate::harness::drain_delegated_observations(state, &self.runtime, &owner, limit),
        ))
        .await
    }

    /// Admit the owner a call creates (fresh open) or addresses (every other operation).
    async fn admit_owner(
        &self,
        caller: &ManagedCaller,
        call: &ManagedSessionCall,
    ) -> Result<HarnessOwner, ManagedHarnessError> {
        if matches!(&call.harness.kind, HarnessRequestKind::Open(value) if value.resume.is_none()) {
            self.admission
                .create(caller, call)
                .await
                .inspect_err(|error| reject_stage("create", *error))
        } else {
            self.admission
                .existing(caller, call)
                .await
                .inspect_err(|error| reject_stage("existing_owner", *error))
        }
    }

    async fn inspect_admitted(
        &self,
        application: Option<&crate::harness::ManagedHarnessApplication<'_, S, DaemonTransport>>,
        owner: &HarnessOwner,
        request_id: String,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        let value = if let Some(app) = application {
            app.inspect(owner.session_id, request_id.clone()).await?
        } else {
            self.runtime.inspect(owner, request_id.clone()).await?
        };
        Ok(HarnessServiceResponse {
            schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
            request_id,
            operation: bcode_harness_models::HarnessOperation::Inspect,
            kind: bcode_harness_models::HarnessServiceResponseKind::Inspection(value),
        })
    }

    /// Best-effort release evidence after an explicit Close. The Close outcome itself is
    /// already canonical; missing release evidence only means the session is not recoverable.
    async fn record_explicit_release(
        &self,
        state: &Arc<crate::ServerState>,
        owner: &HarnessOwner,
        mutation: Option<String>,
        result: &Result<HarnessServiceResponse, ManagedHarnessError>,
    ) {
        let (Some(mutation), Ok(response)) = (mutation, result) else {
            return;
        };
        if !matches!(
            &response.kind,
            bcode_harness_models::HarnessServiceResponseKind::Acknowledged { .. }
        ) {
            return;
        }
        if crate::harness::recovery_published()
            && let Err(error) =
                record_shutdown_transport_release(state, owner, mutation, response).await
        {
            tracing::info!(target: "bcode_server::harness", category = %error, "managed transport release evidence unavailable; session is not recoverable");
        }
        // An acknowledged Close leaves no transport for shutdown cleanup to close again.
        self.owners.lock().await.remove(&owner.session_id);
    }

    async fn observe_admitted(
        &self,
        application: Option<&crate::harness::ManagedHarnessApplication<'_, S, DaemonTransport>>,
        owner: &HarnessOwner,
        request_id: String,
        value: bcode_harness_models::HarnessObserveRequest,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        let value = if let Some(app) = application {
            app.observe(
                owner.session_id,
                request_id.clone(),
                value.after,
                value.limit,
                value.wait_millis,
            )
            .await?
        } else {
            self.runtime
                .observe(
                    owner,
                    request_id.clone(),
                    value.after,
                    value.limit,
                    value.wait_millis,
                )
                .await?
        };
        Ok(HarnessServiceResponse {
            schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
            request_id,
            operation: bcode_harness_models::HarnessOperation::Observe,
            kind: bcode_harness_models::HarnessServiceResponseKind::Observations(value),
        })
    }

    async fn admit_tool_queue(
        &self,
        state: &Arc<crate::ServerState>,
        caller: &ManagedCaller,
        owner: &HarnessOwner,
        call: &ManagedSessionCall,
    ) -> Result<Option<bcode_harness_models::ManagedToolQueueScope>, ManagedHarnessError> {
        // Retained-result early return must precede this admission; replay never issues grants.
        if matches!(
            call.harness.kind,
            HarnessRequestKind::Interrupt(_) | HarnessRequestKind::Close(_)
        ) {
            self.tool_queues.revoke(state, owner.session_id).await;
            Ok(None)
        } else if matches!(call.harness.kind, HarnessRequestKind::Configure(_)) {
            // Configuration neither issues nor revokes native tool grants.
            Ok(None)
        } else {
            self.tool_queues.install(state, caller, call).await
        }
    }

    async fn execute_admitted(
        &self,
        state: &Arc<crate::ServerState>,
        caller: &ManagedCaller,
        owner: &HarnessOwner,
        call: ManagedSessionCall,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        let check = |session: &str, generation: u64| {
            if session == owner.session_id.to_string() && generation == owner.generation {
                Ok(())
            } else {
                Err(ManagedHarnessError::StaleOwner)
            }
        };
        let application =
            match caller {
                ManagedCaller::Client(client) => Some(
                    crate::harness::ManagedHarnessApplication::new(state, *client, &self.runtime),
                ),
                ManagedCaller::Invocation { .. } | ManagedCaller::Workflow { .. } => None,
            };
        let request_id = call.harness.request_id;
        let result = match call.harness.kind {
            HarnessRequestKind::Inspect => {
                self.inspect_admitted(application.as_ref(), owner, request_id)
                    .await
            }
            HarnessRequestKind::Open(value) => {
                check(&value.session_id, value.generation)?;
                if let Some(app) = &application {
                    app.open(owner, value).await
                } else {
                    self.runtime.open(owner, value).await
                }
            }
            HarnessRequestKind::Submit { submission, .. } => {
                check(&submission.session_id, submission.generation)?;
                if application.is_some()
                    && let Some(held) = self
                        .hold_while_busy(state, owner, &request_id, &submission)
                        .await?
                {
                    return Ok(held);
                }
                if let Some(app) = &application {
                    app.submit(owner.session_id, submission).await
                } else {
                    self.runtime.submit(owner, submission).await
                }
            }
            HarnessRequestKind::Respond { response, .. } => {
                check(&response.session_id, response.generation)?;
                if let Some(app) = &application {
                    app.respond(owner.session_id, response).await
                } else {
                    self.runtime.respond(owner, response).await
                }
            }
            HarnessRequestKind::Observe(value) => {
                check(value.target.session_id(), value.target.generation())?;
                self.observe_admitted(application.as_ref(), owner, request_id, value)
                    .await
            }
            HarnessRequestKind::Interrupt(value) => {
                check(value.target.session_id(), value.target.generation())?;
                if let Some(app) = &application {
                    app.cancel(owner.session_id, value.mutation_id).await
                } else {
                    self.runtime.cancel(owner, value.mutation_id).await
                }
            }
            HarnessRequestKind::Close(value) => {
                check(value.target.session_id(), value.target.generation())?;
                if let Some(app) = &application {
                    app.close(owner.session_id, value.mutation_id).await
                } else {
                    self.runtime.close(owner, value.mutation_id).await
                }
            }
            HarnessRequestKind::Reconcile(value) => {
                check(&value.session_id, value.generation)?;
                if let Some(app) = &application {
                    app.reconcile(owner.session_id, request_id, value).await
                } else {
                    self.runtime.reconcile(owner, request_id, value).await
                }
            }
            HarnessRequestKind::Configure(value) => {
                check(value.target.session_id(), value.target.generation())?;
                // Mid-session configuration is an interactive client control only.
                let Some(app) = &application else {
                    return Err(ManagedHarnessError::Unsupported);
                };
                app.configure(owner.session_id, value.mutation_id, value.setting)
                    .await
            }
            // Native auth envelopes are never accepted from ordinary/model callers.
            HarnessRequestKind::AuthStatus { .. }
            | HarnessRequestKind::AuthBegin(_)
            | HarnessRequestKind::AuthContinue(_)
            | HarnessRequestKind::AuthCancel(_) => Err(ManagedHarnessError::Invalid),
        };
        drop(application);
        result
    }
}

impl crate::ServerState {
    /// Install canonical persistence and admission atomically for this daemon. There is no
    /// default/fake store. Must run during startup before admitting managed operations.
    /// # Errors
    /// Rejects replacing the runtime after installation.
    pub fn install_managed_sessions<
        S: ManagedHarnessStore + 'static,
        A: ManagedSessionAdmission + 'static,
    >(
        self: &Arc<Self>,
        store: S,
        admission: A,
    ) -> Result<(), ManagedHarnessError> {
        if self
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(ManagedHarnessError::Unavailable);
        }
        let backend = CanonicalBackend {
            state: Arc::downgrade(self),
            runtime: ManagedHarnessRuntime::new(store, DaemonTransport(Arc::downgrade(self))),
            admission,
            owners: tokio::sync::Mutex::new(std::collections::BTreeMap::new()),
            tool_queues: crate::harness::ToolQueueHost::default(),
            watches: crate::harness::ObservationWatches::default(),
            releasing: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            empty_queues: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        };
        self.managed_sessions
            .set(Arc::new(backend))
            .map_err(|_| ManagedHarnessError::Conflict)
    }
}

fn failure(error: ManagedHarnessError) -> ErrorResponse {
    let code = match error {
        ManagedHarnessError::Invalid => "managed_session_invalid",
        ManagedHarnessError::Storage => "managed_session_storage_unavailable",
        ManagedHarnessError::StaleOwner => "managed_session_stale_owner",
        ManagedHarnessError::Conflict => "managed_session_conflict",
        ManagedHarnessError::Uncertain => "managed_session_uncertain",
        ManagedHarnessError::Unavailable => "managed_session_unavailable",
        ManagedHarnessError::Deadline => "managed_session_deadline",
        ManagedHarnessError::Unsupported => "managed_session_unsupported",
    };
    ErrorResponse::new(code, error.to_string())
}

async fn dispatch_binding(
    state: &Arc<crate::ServerState>,
    caller: ManagedCaller,
    request: &ManagedSessionRequest,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Query {
        session_id: SessionId,
    }
    let query: Query = serde_json::from_str(&request.request_json)
        .map_err(|_| failure(ManagedHarnessError::Invalid))?;
    let ManagedCaller::Client(client) = caller else {
        return Err(failure(ManagedHarnessError::Unsupported));
    };
    crate::session_operations::require_selected_location(state, query.session_id)
        .await
        .map_err(|_| failure(ManagedHarnessError::StaleOwner))?;
    if state
        .active_session_namespace_mismatch(query.session_id, client)
        .await
        .is_some()
    {
        return Err(failure(ManagedHarnessError::StaleOwner));
    }
    let canonical = state
        .sessions
        .session_external_harness_state(query.session_id)
        .await
        .map_err(|_| failure(ManagedHarnessError::Storage))?;
    if let Some(value) = &canonical {
        if value.owner.session_id != query.session_id {
            return Err(failure(ManagedHarnessError::StaleOwner));
        }
        if let Some(target) = &value.target {
            target
                .validate_owner(&query.session_id.to_string(), value.owner.generation)
                .map_err(|_| failure(ManagedHarnessError::Storage))?;
        }
    }
    let snapshot = canonical.and_then(|value| {
        value
            .target
            .map(|target| bcode_ipc::harness::ManagedSessionTargetSnapshot {
                session_id: value.owner.session_id,
                instance: value.instance,
                target,
            })
    });
    if let Some(value) = &snapshot {
        value
            .validate()
            .map_err(|_| failure(ManagedHarnessError::Storage))?;
        if value.session_id != query.session_id {
            return Err(failure(ManagedHarnessError::StaleOwner));
        }
    }
    auth_response(&snapshot)
}

async fn dispatch_attachment_poll(
    state: &Arc<crate::ServerState>,
    caller: &ManagedCaller,
    query: &bcode_ipc::harness::ManagedSessionPollRequest,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    query
        .validate()
        .map_err(|_| failure(ManagedHarnessError::Invalid))?;
    state
        .managed_sessions
        .get()
        .ok_or_else(|| failure(ManagedHarnessError::Unavailable))?
        .poll_observations(caller, query, 0)
        .await
        .map_err(failure)?;
    Ok(ManagedSessionResponse {
        schema_version: MANAGED_SESSION_VERSION,
        response_json: "{\"committed\":true}".into(),
    })
}

async fn dispatch_attachment_watch(
    state: &Arc<crate::ServerState>,
    caller: &ManagedCaller,
    query: &bcode_ipc::harness::ManagedSessionPollRequest,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    query
        .validate()
        .map_err(|_| failure(ManagedHarnessError::Invalid))?;
    state
        .managed_sessions
        .get()
        .ok_or_else(|| failure(ManagedHarnessError::Unavailable))?
        .watch_observations(caller, query)
        .await
        .map_err(failure)?;
    Ok(ManagedSessionResponse {
        schema_version: MANAGED_SESSION_VERSION,
        response_json: "{\"committed\":true,\"watching\":true}".into(),
    })
}

/// Reconstructed end evidence for a transport whose daemon ended without releasing it (for
/// example it was killed). Requires the exact previous owner to be another daemon whose
/// published execution lifetime is released; the lifetime is re-verified at preparation.
async fn abandoned_transport(
    state: &Arc<crate::ServerState>,
    snapshot: &bcode_session_models::external_harness::ExternalHarnessState,
) -> Result<
    (
        u64,
        bcode_session_models::external_harness::ExternalHarnessTransportRelease,
    ),
    ManagedHarnessError,
> {
    use bcode_session_models::external_harness::ABANDONED_TRANSPORT_EVIDENCE;
    let previous = &snapshot.owner;
    let location = state
        .daemon_status
        .state_location_id
        .clone()
        .ok_or(ManagedHarnessError::Conflict)?;
    if previous.daemon_id == state.daemon_status.instance_id {
        return Err(ManagedHarnessError::Conflict);
    }
    let binding = snapshot
        .binding
        .clone()
        .ok_or(ManagedHarnessError::Conflict)?;
    let artifact = bcode_daemon_lifecycle::released_execution_lifetime_artifact(
        &state.state_root,
        &previous.daemon_id,
        &location,
    )
    .ok_or(ManagedHarnessError::Conflict)?;
    let sequence = state
        .sessions
        .session_external_harness_abandonment_point(previous)
        .await
        .map_err(|_| ManagedHarnessError::Storage)?
        .ok_or(ManagedHarnessError::Conflict)?;
    Ok((
        sequence,
        bcode_session_models::external_harness::ExternalHarnessTransportRelease {
            close_request_id: ABANDONED_TRANSPORT_EVIDENCE.into(),
            instance: snapshot.instance.clone(),
            binding,
            lifetime: bcode_session_models::external_harness::ExternalHarnessLifetime {
                daemon_instance_id: previous.daemon_id.clone(),
                artifact_id: artifact,
                state_location_id: location,
            },
            reap_id: ABANDONED_TRANSPORT_EVIDENCE.into(),
        },
    ))
}

fn recovery_intent(
    query: &bcode_ipc::harness::ManagedSessionRecoveryRequest,
    snapshot: bcode_session_models::external_harness::ExternalHarnessState,
    sequence: u64,
    release: bcode_session_models::external_harness::ExternalHarnessTransportRelease,
    abandoned: bool,
    directory: &std::path::Path,
) -> Result<
    (
        bcode_harness_models::HarnessRequest,
        bcode_session_models::external_harness::ExternalHarnessRecovery,
    ),
    ManagedHarnessError,
> {
    let generation = query
        .expected_generation
        .checked_add(1)
        .ok_or(ManagedHarnessError::Conflict)?;
    let mut binding = release.binding.clone();
    binding.generation = generation;
    let open = bcode_harness_models::HarnessOpenRequest {
        open_id: query.recovery_id.clone(),
        session_id: query.session_id.to_string(),
        generation,
        working_directory: directory
            .to_str()
            .ok_or(ManagedHarnessError::Invalid)?
            .to_owned(),
        resume: Some(binding),
        parent_assistance: None,
    };
    let request = bcode_harness_models::HarnessRequest {
        schema_version: 1,
        request_id: query.recovery_id.clone(),
        instance: snapshot.instance,
        kind: HarnessRequestKind::Open(open),
    };
    let recovery = bcode_session_models::external_harness::ExternalHarnessRecovery {
        previous_owner: snapshot.owner,
        release_event_sequence: sequence,
        release,
        abandoned,
        capabilities: snapshot.capabilities,
        resume_request:
            bcode_session_models::external_harness::ExternalHarnessRequest::from_request(&request)
                .map_err(|_| ManagedHarnessError::Invalid)?,
    };
    Ok((request, recovery))
}

async fn preflight_recovery_resume(
    state: &Arc<crate::ServerState>,
    request: bcode_harness_models::HarnessRequest,
) -> Result<(), ManagedHarnessError> {
    let HarnessRequestKind::Open(open) = request.kind else {
        return Err(ManagedHarnessError::Invalid);
    };
    let native = NativeRecoveryRequest {
        schema_version: 1,
        request_id: request.request_id,
        instance: request.instance,
        kind: NativeRecoveryKind::PrepareResume { request: open },
    };
    match native_recovery_service(state, &native).await? {
        NativeRecoveryResult::ResumeValidated { .. } => Ok(()),
        NativeRecoveryResult::TransportReleased { .. } => Err(ManagedHarnessError::Invalid),
    }
}

async fn dispatch_recovery(
    state: &Arc<crate::ServerState>,
    caller: &ManagedCaller,
    query: bcode_ipc::harness::ManagedSessionRecoveryRequest,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    query
        .validate()
        .map_err(|_| failure(ManagedHarnessError::Invalid))?;
    if !crate::harness::recovery_published() {
        return Err(failure(ManagedHarnessError::Unsupported));
    }
    let store = crate::harness::CanonicalHarnessStore::new(state);
    let admission = crate::harness::CanonicalManagedAdmission::new(store.clone());
    // Caller authorization precedes release discovery and all plugin preflight/native effects.
    admission
        .authorize_recovery_request(caller, query.session_id)
        .await
        .map_err(failure)?;
    let snapshot = state
        .sessions
        .session_external_harness_state(query.session_id)
        .await
        .map_err(|_| failure(ManagedHarnessError::Storage))?
        .ok_or_else(|| failure(ManagedHarnessError::Conflict))?;
    if snapshot.instance.instance_id != query.instance_id
        || snapshot.owner.generation != query.expected_generation
    {
        return Err(failure(ManagedHarnessError::StaleOwner));
    }
    let released = state
        .sessions
        .session_external_harness_transport_release(&snapshot.owner)
        .await
        .map_err(|_| failure(ManagedHarnessError::Storage))?;
    let (sequence, release, abandoned) = if let Some((sequence, release)) = released {
        (sequence, release, false)
    } else {
        let (sequence, release) = abandoned_transport(state, &snapshot)
            .await
            .map_err(failure)?;
        (sequence, release, true)
    };
    let summary = state
        .sessions
        .session_summary(query.session_id)
        .await
        .map_err(|_| failure(ManagedHarnessError::Storage))?;
    let (request, recovery) = recovery_intent(
        &query,
        snapshot,
        sequence,
        release,
        abandoned,
        &summary.working_directory,
    )
    .map_err(failure)?;
    let plugin_state = Arc::clone(state);
    let owner = admission
        .prepare_recovery(caller, recovery, move |request| async move {
            preflight_recovery_resume(&plugin_state, request).await
        })
        .await
        .map_err(failure)?;
    drop(admission);
    let backend = state
        .managed_sessions
        .get()
        .ok_or_else(|| failure(ManagedHarnessError::Unavailable))?;
    let call = ManagedSessionCall {
        session_id: Some(query.session_id),
        harness: request,
        parent_session_id: None,
        assistance_destination_session_id: None,
    };
    // Canonical intent was CAS-published first. The retained claim authorizes one resume;
    // dropped acknowledgements remain uncertain and never trigger input/native replay.
    let response = backend
        .resume_recovery(caller, owner, call)
        .await
        .map_err(failure)?;
    Ok(ManagedSessionResponse {
        schema_version: MANAGED_SESSION_VERSION,
        response_json: serde_json::to_string(&response)
            .map_err(|_| failure(ManagedHarnessError::Invalid))?,
    })
}

#[cfg(test)]
mod recovery_route_tests {
    use super::*;

    fn release_request() -> NativeRecoveryRequest {
        let session = SessionId::new().to_string();
        let instance = serde_json::from_value(serde_json::json!({
            "instance_id":"fixture","plugin_id":"fixture","executable":"/not-invoked","config_namespace":"fixture",
            "auth":{"provider_id":"fixture","profile":"native","credential_namespace":"fixture","custody":"native_managed"}
        })).unwrap();
        NativeRecoveryRequest {
            schema_version: 1,
            request_id: "close".into(),
            instance,
            kind: NativeRecoveryKind::ReleaseTransport {
                request: bcode_harness_models::HarnessControlRequest {
                    target: bcode_harness_models::HarnessSessionTarget::Confirmed(
                        bcode_harness_models::HarnessSessionBinding {
                            schema_version: 1,
                            session_id: session,
                            instance_id: "fixture".into(),
                            native_session_id: "exact-native".into(),
                            native_version: "exact-version".into(),
                            generation: 3,
                        },
                    ),
                    mutation_id: "close".into(),
                },
            },
        }
    }

    #[test]
    fn native_recovery_rejects_unknown_versions_unbounded_ids_and_nonresume_open() {
        let mut request = release_request();
        validate_native_recovery_request(&request).unwrap();
        request.schema_version = HARNESS_RECOVERY_SCHEMA_VERSION + 1;
        assert!(validate_native_recovery_request(&request).is_err());
        request.schema_version = HARNESS_RECOVERY_SCHEMA_VERSION;
        request.request_id = "x".repeat(bcode_harness_models::MAX_HARNESS_ID_BYTES + 1);
        assert!(validate_native_recovery_request(&request).is_err());
        request.request_id = "close".into();
        request.kind = NativeRecoveryKind::PrepareResume {
            request: bcode_harness_models::HarnessOpenRequest {
                open_id: "close".into(),
                session_id: SessionId::new().to_string(),
                generation: 1,
                working_directory: "/workspace".into(),
                resume: None,
                parent_assistance: None,
            },
        };
        assert!(
            validate_native_recovery_request(&request).is_err(),
            "new native creation is not recovery"
        );
    }

    #[test]
    fn physical_release_requires_exact_target_mutation_and_nonempty_reap() {
        let request = release_request();
        let NativeRecoveryKind::ReleaseTransport { request: control } = &request.kind else {
            unreachable!()
        };
        let response = |target, mutation_id, reap_id| NativeRecoveryResponse {
            schema_version: 1,
            request_id: "close".into(),
            kind: NativeRecoveryResult::TransportReleased {
                target,
                mutation_id,
                reap_id,
            },
        };
        assert!(
            validate_native_recovery_result(
                &request,
                response(control.target.clone(), "close".into(), "reaped".into())
            )
            .is_ok()
        );
        assert!(
            validate_native_recovery_result(
                &request,
                response(control.target.clone(), "other".into(), "reaped".into())
            )
            .is_err()
        );
        assert!(
            validate_native_recovery_result(
                &request,
                response(control.target.clone(), "close".into(), String::new())
            )
            .is_err()
        );
        let mut foreign = control.target.clone();
        if let bcode_harness_models::HarnessSessionTarget::Confirmed(binding) = &mut foreign {
            binding.generation += 1;
        }
        assert!(
            validate_native_recovery_result(
                &request,
                response(foreign, "close".into(), "reaped".into())
            )
            .is_err()
        );
        let acknowledged = serde_json::json!({"schema_version":1,"request_id":"close","kind":{"kind":"acknowledged","mutation_id":"close"}});
        assert!(
            serde_json::from_value::<NativeRecoveryResponse>(acknowledged).is_err(),
            "Ack cannot deserialize as physical release"
        );
    }

    #[test]
    fn resume_preflight_must_preserve_exact_complete_intent() {
        let release = release_request();
        let NativeRecoveryKind::ReleaseTransport { request: control } = release.kind else {
            unreachable!()
        };
        let bcode_harness_models::HarnessSessionTarget::Confirmed(binding) = control.target else {
            unreachable!()
        };
        let open = bcode_harness_models::HarnessOpenRequest {
            open_id: "resume".into(),
            session_id: binding.session_id.clone(),
            generation: binding.generation,
            working_directory: "/workspace".into(),
            resume: Some(binding),
            parent_assistance: None,
        };
        let request = NativeRecoveryRequest {
            schema_version: 1,
            request_id: "resume".into(),
            instance: release.instance,
            kind: NativeRecoveryKind::PrepareResume {
                request: open.clone(),
            },
        };
        let response = |open| NativeRecoveryResponse {
            schema_version: 1,
            request_id: "resume".into(),
            kind: NativeRecoveryResult::ResumeValidated { request: open },
        };
        assert!(validate_native_recovery_result(&request, response(open.clone())).is_ok());
        let mut changed = open;
        changed.working_directory = "/foreign-workspace".into();
        assert!(validate_native_recovery_result(&request, response(changed)).is_err());
    }

    #[tokio::test]
    async fn unauthorized_recovery_route_has_no_effects() {
        let state = Arc::new(crate::tests::test_server_state(
            crate::SessionManager::default(),
        ));
        let query = bcode_ipc::harness::ManagedSessionRecoveryRequest {
            session_id: SessionId::new(),
            instance_id: "fixture".into(),
            expected_generation: 1,
            recovery_id: "explicit".into(),
        };
        let session_id = query.session_id;
        assert!(
            dispatch_recovery(
                &state,
                &ManagedCaller::Client(crate::ClientId::new()),
                query
            )
            .await
            .is_err()
        );
        assert!(state.sessions.session_summary(session_id).await.is_err());
        drop(state);
    }
}

async fn dispatch_registration_request(
    state: &Arc<crate::ServerState>,
    caller: &ManagedCaller,
    request: &ManagedSessionRequest,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    let ManagedCaller::Client(client_id) = caller else {
        return Err(failure(ManagedHarnessError::Unsupported));
    };
    let value = match request.operation {
        ManagedSessionOperation::CommitRegistration => {
            let query: bcode_ipc::harness::ManagedRegistrationCommitRequest =
                serde_json::from_str(&request.request_json)
                    .map_err(|_| failure(ManagedHarnessError::Invalid))?;
            serde_json::to_string(
                &crate::harness::commit_registration(state, *client_id, query)
                    .await
                    .map_err(failure)?,
            )
        }
        ManagedSessionOperation::RegistrationSnapshot => {
            let view =
                crate::harness::configuration_views::declarative_for_client(state, *client_id)
                    .await
                    .map_err(failure)?;
            serde_json::to_string(
                &crate::harness::registration_snapshot(&view)
                    .await
                    .map_err(failure)?,
            )
        }
        ManagedSessionOperation::RegistrationResolution => {
            let view =
                crate::harness::configuration_views::declarative_for_client(state, *client_id)
                    .await
                    .map_err(failure)?;
            serde_json::to_string(
                &crate::harness::registration_resolution(&view)
                    .await
                    .map_err(failure)?,
            )
        }
        _ => return Err(failure(ManagedHarnessError::Invalid)),
    }
    .map_err(|_| failure(ManagedHarnessError::Invalid))?;
    Ok(ManagedSessionResponse {
        schema_version: MANAGED_SESSION_VERSION,
        response_json: value,
    })
}

async fn dispatch_execution_catalog_request(
    state: &Arc<crate::ServerState>,
    caller: &ManagedCaller,
    request: &ManagedSessionRequest,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    if matches!(
        request.operation,
        ManagedSessionOperation::CommitRegistration
            | ManagedSessionOperation::RegistrationSnapshot
            | ManagedSessionOperation::RegistrationResolution
    ) {
        return dispatch_registration_request(state, caller, request).await;
    }
    if request.operation == ManagedSessionOperation::PrepareCandidate {
        let ManagedCaller::Client(client_id) = caller else {
            return Err(failure(ManagedHarnessError::Unsupported));
        };
        if state.client_runtime_context(*client_id).await.is_none() {
            return Err(failure(ManagedHarnessError::StaleOwner));
        }
        let query: bcode_harness_models::HarnessPrepareCandidateRequest =
            serde_json::from_str(&request.request_json)
                .map_err(|_| failure(ManagedHarnessError::Invalid))?;
        let response = HarnessPluginTransport::with_view(
            Arc::clone(state),
            crate::harness::configuration_views::for_client(state, *client_id)
                .await
                .map_err(failure)?,
        )
        .prepare_candidate(&query)
        .await
        .map_err(failure)?;
        let response_json =
            serde_json::to_string(&response).map_err(|_| failure(ManagedHarnessError::Invalid))?;
        if response_json.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES {
            return Err(failure(ManagedHarnessError::Invalid));
        }
        return Ok(ManagedSessionResponse {
            schema_version: MANAGED_SESSION_VERSION,
            response_json,
        });
    }
    if request.operation == ManagedSessionOperation::DiscoverCandidates {
        let query: bcode_ipc::harness::ManagedCandidatesRequest =
            serde_json::from_str(&request.request_json)
                .map_err(|_| failure(ManagedHarnessError::Invalid))?;
        let response = crate::harness::execution_catalog::discover_candidates(state, caller, query)
            .await
            .map_err(failure)?;
        let response_json =
            serde_json::to_string(&response).map_err(|_| failure(ManagedHarnessError::Invalid))?;
        if response_json.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES {
            return Err(failure(ManagedHarnessError::Invalid));
        }
        return Ok(ManagedSessionResponse {
            schema_version: MANAGED_SESSION_VERSION,
            response_json,
        });
    }
    if matches!(
        request.operation,
        ManagedSessionOperation::ExecutionCatalog | ManagedSessionOperation::StartSelected
    ) {
        let json = if request.operation == ManagedSessionOperation::ExecutionCatalog {
            let query = request.decode_execution_catalog()
                .map_err(|_| failure(ManagedHarnessError::Invalid))?;
            let catalog = crate::harness::execution_catalog::discover(state, caller, query)
                .await.map_err(failure)?;
            serde_json::to_string(&catalog)
        } else {
            let query = request.decode_selected_start()
                .map_err(|_| failure(ManagedHarnessError::Invalid))?;
            let response = crate::harness::execution_catalog::start_selected(state, caller, query)
                .await.map_err(|error| {
                    if error == ManagedHarnessError::Unsupported {
                        ErrorResponse::new(
                            "managed_selected_start_persistence_required",
                            "selected start requires canonical model/open correlation; no native mutation was attempted",
                        )
                    } else {
                        failure(error)
                    }
                })?;
            serde_json::to_string(&response)
        }.map_err(|_| failure(ManagedHarnessError::Invalid))?;
        if json.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES {
            return Err(failure(ManagedHarnessError::Invalid));
        }
        return Ok(ManagedSessionResponse {
            schema_version: MANAGED_SESSION_VERSION,
            response_json: json,
        });
    }
    Err(failure(ManagedHarnessError::Invalid))
}

async fn dispatch_auth_operation(
    state: &Arc<crate::ServerState>,
    caller: ManagedCaller,
    request: ManagedSessionRequest,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    if request.operation == ManagedSessionOperation::AuthPrepare {
        return prepare_client_terminal(state, caller, request).await;
    }
    if matches!(
        request.operation,
        ManagedSessionOperation::AuthHandoff | ManagedSessionOperation::AuthReaped
    ) {
        return prepare_handoff(state, caller, request).await;
    }
    if matches!(
        request.operation,
        ManagedSessionOperation::AuthContinue
            | ManagedSessionOperation::AuthStatus
            | ManagedSessionOperation::AuthAcknowledge
    ) && serde_json::from_str::<bcode_ipc::harness::ManagedSessionHandoffRequest>(
        &request.request_json,
    )
    .is_ok()
    {
        return prepare_handoff(state, caller, request).await;
    }
    return Box::pin(dispatch_auth(state, caller, request)).await;
}

pub(crate) async fn dispatch(
    state: &Arc<crate::ServerState>,
    caller: ManagedCaller,
    request: ManagedSessionRequest,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    let operation = request.operation;
    let result = Box::pin(dispatch_request(state, caller, request)).await;
    if let Err(error) = &result {
        tracing::debug!(target: "bcode_server::harness", operation = operation.as_str(), code = %error.code, "managed session request refused");
    }
    result
}

const fn execution_catalog_operation(operation: ManagedSessionOperation) -> bool {
    matches!(
        operation,
        ManagedSessionOperation::CommitRegistration
            | ManagedSessionOperation::RegistrationSnapshot
            | ManagedSessionOperation::RegistrationResolution
            | ManagedSessionOperation::PrepareCandidate
            | ManagedSessionOperation::DiscoverCandidates
            | ManagedSessionOperation::ExecutionCatalog
            | ManagedSessionOperation::StartSelected
    )
}

async fn dispatch_request(
    state: &Arc<crate::ServerState>,
    caller: ManagedCaller,
    request: ManagedSessionRequest,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    request
        .validate_envelope()
        .map_err(|message| ErrorResponse::new("managed_session_invalid", message))?;
    if state
        .shutdown_requested
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return Err(failure(ManagedHarnessError::Unavailable));
    }
    if execution_catalog_operation(request.operation) {
        return dispatch_execution_catalog_request(state, &caller, &request).await;
    }
    if request.operation == ManagedSessionOperation::Recover {
        let query = request
            .decode_recovery()
            .map_err(|_| failure(ManagedHarnessError::Invalid))?;
        return dispatch_recovery(state, &caller, query).await;
    }
    if request.operation.is_auth() {
        return Box::pin(dispatch_auth_operation(state, caller, request)).await;
    }
    if request.operation == ManagedSessionOperation::Observe
        && let Ok(query) = serde_json::from_str::<bcode_ipc::harness::ManagedSessionPollRequest>(
            &request.request_json,
        )
    {
        return dispatch_attachment_poll(state, &caller, &query).await;
    }
    if request.operation == ManagedSessionOperation::Watch {
        let query = serde_json::from_str::<bcode_ipc::harness::ManagedSessionPollRequest>(
            &request.request_json,
        )
        .map_err(|_| failure(ManagedHarnessError::Invalid))?;
        return dispatch_attachment_watch(state, &caller, &query).await;
    }
    if request.operation == ManagedSessionOperation::Binding {
        return dispatch_binding(state, caller, &request).await;
    }
    let json = if request.operation == ManagedSessionOperation::History {
        let query = request
            .decode_history()
            .map_err(|message| ErrorResponse::new("managed_session_invalid", message))?;
        match caller {
            ManagedCaller::Client(client) => {
                crate::session_operations::require_selected_location(state, query.session_id)
                    .await
                    .map_err(|_| failure(ManagedHarnessError::StaleOwner))?;
                if state
                    .active_session_namespace_mismatch(query.session_id, client)
                    .await
                    .is_some()
                {
                    return Err(failure(ManagedHarnessError::StaleOwner));
                }
            }
            ManagedCaller::Invocation { session_id, .. } if session_id == query.session_id => {}
            ManagedCaller::Invocation { .. } | ManagedCaller::Workflow { .. } => {
                return Err(failure(ManagedHarnessError::StaleOwner));
            }
        }
        let page =
            crate::harness::canonical_history(state, query.session_id, query.after, query.limit)
                .await
                .map_err(failure)?;
        serde_json::to_string(&page)
    } else {
        let call = request
            .decode_call()
            .map_err(|message| ErrorResponse::new("managed_session_invalid", message))?;
        // Configuration is an interactive client control; others never reach a claim.
        if matches!(call.harness.kind, HarnessRequestKind::Configure(_))
            && !matches!(caller, ManagedCaller::Client(_))
        {
            return Err(failure(ManagedHarnessError::Unsupported));
        }
        let backend = state.managed_sessions.get().ok_or_else(|| {
            ErrorResponse::new(
                "managed_session_storage_unavailable",
                "canonical managed-session admission/store bridge is not installed",
            )
        })?;
        let response = backend.execute(&caller, call).await.map_err(failure)?;
        serde_json::to_string(&response)
    }
    .map_err(|_| failure(ManagedHarnessError::Invalid))?;
    if json.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES {
        return Err(failure(ManagedHarnessError::Invalid));
    }
    Ok(ManagedSessionResponse {
        schema_version: MANAGED_SESSION_VERSION,
        response_json: json,
    })
}

/// Compatibility adapter for existing public service clients. This reserves the application
/// interface in the host; it must never resolve a vendor plugin through service discovery.
pub(crate) async fn service(
    state: &Arc<crate::ServerState>,
    client_id: ClientId,
    operation: String,
    payload: Vec<u8>,
) -> Result<bcode_ipc::PluginServiceResponse, ErrorResponse> {
    if payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES {
        return Err(failure(ManagedHarnessError::Invalid));
    }
    let request: ManagedSessionRequest =
        serde_json::from_slice(&payload).map_err(|_| failure(ManagedHarnessError::Invalid))?;
    if request.operation.as_str() != operation {
        return Err(failure(ManagedHarnessError::Invalid));
    }
    if matches!(
        request.operation,
        ManagedSessionOperation::AuthPrepare
            | ManagedSessionOperation::AuthHandoff
            | ManagedSessionOperation::AuthReaped
    ) {
        return Err(ErrorResponse::new(
            "managed_session_sensitive_transport_required",
            "client terminal lifecycle requires typed managed-session IPC",
        ));
    }
    if request.operation == ManagedSessionOperation::AuthStatus {
        let auth: bcode_ipc::harness::ManagedSessionAuthRequest =
            serde_json::from_str(&request.request_json)
                .map_err(|_| failure(ManagedHarnessError::Invalid))?;
        if auth.sensitive_interaction {
            return Err(ErrorResponse::new(
                "managed_session_sensitive_transport_required",
                "sensitive auth interactions require typed managed-session IPC",
            ));
        }
    }
    let mut response = dispatch(state, ManagedCaller::Client(client_id), request).await?;
    Ok(bcode_ipc::PluginServiceResponse {
        payload: std::mem::take(&mut response.response_json).into_bytes(),
        error: None,
    })
}

/// Resolve workflow caller identity from the retained dispatch link, never its parent
/// session or request JSON. The backend subsequently holds `authorize_call`'s verified
/// authority guard across canonical admission and native delivery.
pub(crate) async fn workflow_invocation(
    state: &crate::ServerState,
    run_id: &str,
    dispatch_identity: &str,
    plugin_id: &str,
    request: bcode_tool::ToolInvocationServiceRequest,
) -> bcode_tool::ToolInvocationServiceResolution {
    use bcode_tool::ToolInvocationServiceResolution as R;
    if request.invocation_id != dispatch_identity {
        return R::Failed {
            code: "invocation_id_mismatch".into(),
            message: "managed operation does not belong to this workflow dispatch".into(),
        };
    }
    let session = async {
        let stored = crate::workflow_representation::owner::execution_receipt(
            state,
            run_id,
            dispatch_identity,
        )
        .await
        .ok()??;
        let link = crate::workflow_representation::owner::execution_session_link(
            state,
            run_id,
            &stored.node_id,
            &stored.activation_id,
            stored.attempt,
        )
        .await
        .ok()??;
        link.session_id.parse::<SessionId>().ok()
    }
    .await;
    let Some(session_id) = session else {
        return R::Failed {
            code: "managed_session_scope_mismatch".into(),
            message: "managed workflow requires its canonical dispatch receipt and session link"
                .into(),
        };
    };
    invocation(
        state,
        ManagedCaller::Workflow {
            run_id: run_id.into(),
            dispatch_identity: dispatch_identity.into(),
            session_id,
            plugin_id: plugin_id.into(),
        },
        request,
    )
    .await
}

fn invocation_operation(operation: &str, workflow: bool) -> Option<ManagedSessionOperation> {
    Some(match operation {
        "start" => ManagedSessionOperation::Start,
        "inspect" => ManagedSessionOperation::Inspect,
        "followup" => ManagedSessionOperation::Followup,
        "respond" => ManagedSessionOperation::Respond,
        "interrupt" => ManagedSessionOperation::Interrupt,
        "observe" if workflow => ManagedSessionOperation::Observe,
        "reconcile" if workflow => ManagedSessionOperation::Reconcile,
        _ => return None,
    })
}

pub(crate) async fn invocation(
    state: &crate::ServerState,
    caller: ManagedCaller,
    request: bcode_tool::ToolInvocationServiceRequest,
) -> bcode_tool::ToolInvocationServiceResolution {
    use bcode_tool::ToolInvocationServiceResolution as R;
    if request.interface_id != MANAGED_SESSION_INTERFACE_ID {
        return R::Unsupported;
    }
    let (session_id, plugin_id, invocation_id) = match &caller {
        ManagedCaller::Invocation {
            session_id,
            plugin_id,
            invocation_id,
        } => (session_id, plugin_id, invocation_id),
        ManagedCaller::Workflow {
            session_id,
            plugin_id,
            dispatch_identity,
            ..
        } => (session_id, plugin_id, dispatch_identity),
        ManagedCaller::Client(_) => return R::Unsupported,
    };
    if &request.invocation_id != invocation_id {
        return R::Failed {
            code: "invocation_id_mismatch".into(),
            message: "managed operation does not belong to this invocation".into(),
        };
    }
    if plugin_id.is_empty()
        || !state.plugins.registry().manifests().contains_key(plugin_id)
        || request
            .route_id
            .as_deref()
            .is_some_and(|route| route != MANAGED_SESSION_INTERFACE_ID)
        || crate::session_operations::require_selected_location(state, *session_id)
            .await
            .is_err()
    {
        return R::Failed {
            code: "managed_session_scope_mismatch".into(),
            message: "managed session requires the current authenticated plugin/session scope"
                .into(),
        };
    }
    if matches!(&caller, ManagedCaller::Invocation { .. })
        && matches!(
            request.operation.as_str(),
            bcode_harness_models::MANAGED_DELEGATION_BINDING_OPERATION
                | bcode_harness_models::MANAGED_DELEGATION_HISTORY_OPERATION
        )
    {
        return delegated_read_invocation(state, &caller, request).await;
    }
    let Some(operation) = invocation_operation(
        &request.operation,
        matches!(&caller, ManagedCaller::Workflow { .. }),
    ) else {
        return R::Unsupported;
    };
    let Ok(request_json) = serde_json::to_string(&request.payload) else {
        return R::Unsupported;
    };
    let envelope = ManagedSessionRequest {
        schema_version: MANAGED_SESSION_VERSION,
        operation,
        request_json,
    };
    let Ok(call) = envelope.decode_call() else {
        return R::Failed {
            code: "managed_session_invalid".into(),
            message: "invalid managed session contract".into(),
        };
    };
    let Some(backend) = state.managed_sessions.get() else {
        return R::Failed {
            code: "managed_session_storage_unavailable".into(),
            message: "canonical managed-session admission/store bridge is not installed".into(),
        };
    };
    match backend.execute(&caller, call).await {
        Ok(response) => match serde_json::to_value(response) {
            Ok(payload)
                if serde_json::to_vec(&payload).is_ok_and(|bytes| {
                    bytes.len() <= bcode_harness_models::MAX_HARNESS_WIRE_BYTES
                }) =>
            {
                R::Responded { payload }
            }
            _ => R::Failed {
                code: "managed_session_invalid".into(),
                message: "invalid managed session response".into(),
            },
        },
        Err(error) => {
            let error = failure(error);
            R::Failed {
                code: error.code,
                message: error.message,
            }
        }
    }
}

/// Invocation-scoped delegated reads. Responses are typed, bounded and carry no authority.
async fn delegated_read_invocation(
    state: &crate::ServerState,
    caller: &ManagedCaller,
    request: bcode_tool::ToolInvocationServiceRequest,
) -> bcode_tool::ToolInvocationServiceResolution {
    use bcode_tool::ToolInvocationServiceResolution as R;
    let Some(backend) = state.managed_sessions.get() else {
        return R::Failed {
            code: "managed_session_storage_unavailable".into(),
            message: "canonical managed-session admission/store bridge is not installed".into(),
        };
    };
    let result = if request.operation == bcode_harness_models::MANAGED_DELEGATION_BINDING_OPERATION
    {
        match serde_json::from_value::<bcode_harness_models::ManagedDelegationBindingRequest>(
            request.payload,
        ) {
            Ok(query) => backend
                .delegated_binding(caller, &query)
                .await
                .and_then(|value| {
                    serde_json::to_value(value).map_err(|_| ManagedHarnessError::Invalid)
                }),
            Err(_) => Err(ManagedHarnessError::Invalid),
        }
    } else {
        match serde_json::from_value::<bcode_harness_models::ManagedDelegationHistoryRequest>(
            request.payload,
        ) {
            Ok(query) => backend
                .delegated_history(caller, &query)
                .await
                .and_then(|value| {
                    serde_json::to_value(value).map_err(|_| ManagedHarnessError::Invalid)
                }),
            Err(_) => Err(ManagedHarnessError::Invalid),
        }
    };
    match result {
        Ok(payload)
            if serde_json::to_vec(&payload)
                .is_ok_and(|bytes| bytes.len() <= bcode_harness_models::MAX_HARNESS_WIRE_BYTES) =>
        {
            R::Responded { payload }
        }
        Ok(_) => R::Failed {
            code: "managed_session_invalid".into(),
            message: "invalid managed session response".into(),
        },
        Err(error) => {
            let error = failure(error);
            R::Failed {
                code: error.code,
                message: error.message,
            }
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeHandoffReply {
    schema_version: u16,
    request_id: String,
    handoff: bcode_ipc::harness::ManagedNativeLoginPlan,
}

async fn selected_handoff_plan(
    state: &Arc<crate::ServerState>,
    session_id: SessionId,
    instance: &bcode_harness_models::HarnessInstance,
    interface_id: &str,
    namespace: &bcode_provider_auth_models::managed::AuthNamespace,
) -> Result<bcode_ipc::harness::ManagedNativeLoginPlan, ErrorResponse> {
    let declared = state
        .plugins
        .registry()
        .manifests()
        .get(&instance.plugin_id)
        .is_some_and(|manifest| {
            manifest
                .services
                .iter()
                .any(|service| service.interface_id == interface_id)
        });
    if !declared {
        return Err(failure(ManagedHarnessError::Unsupported));
    }
    // Only the selected enabled plugin can prepare this read-only extension. There is no
    // daemon subprocess launch, arbitrary method forwarding, or inferred vendor argument list.
    let request_id = format!("handoff:{}", uuid::Uuid::new_v4());
    let payload = serde_json::to_vec(
        &serde_json::json!({"schema_version":1,"request_id":request_id,"instance":instance}),
    )
    .map_err(|_| failure(ManagedHarnessError::Invalid))?;
    let view = crate::harness::configuration_views::for_session(state, session_id)
        .await
        .map_err(failure)?;
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        view.invoke_service_with_bridge_scoped(
            &instance.plugin_id,
            interface_id,
            "prepare_login",
            payload,
            bcode_plugin::PluginInvocationScope::Global,
            None,
        ),
    )
    .await
    .map_err(|_| failure(ManagedHarnessError::Deadline))?
    .map_err(|_| failure(ManagedHarnessError::Unavailable))?;
    drop(view);
    if response.error.is_some()
        || response.payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES
    {
        return Err(failure(ManagedHarnessError::Invalid));
    }
    let reply: NativeHandoffReply = serde_json::from_slice(&response.payload)
        .map_err(|_| failure(ManagedHarnessError::Invalid))?;
    reply
        .handoff
        .validate()
        .map_err(|_| failure(ManagedHarnessError::Invalid))?;
    if reply.schema_version != 1
        || reply.request_id != request_id
        || reply.handoff.namespace != *namespace
        || reply.handoff.executable != std::path::Path::new(&instance.executable)
    {
        return Err(failure(ManagedHarnessError::Conflict));
    }
    Ok(reply.handoff)
}

fn terminal_fence_path(
    state: &crate::ServerState,
    namespace: &bcode_provider_auth_models::managed::AuthNamespace,
) -> Result<std::path::PathBuf, ErrorResponse> {
    use sha2::{Digest as _, Sha256};
    let bytes = serde_json::to_vec(namespace).map_err(|_| failure(ManagedHarnessError::Invalid))?;
    let key = hex::encode(Sha256::digest(bytes));
    Ok(state
        .state_root
        .join("managed-auth-terminal-claims")
        .join(key))
}

fn require_unissued_terminal(
    state: &crate::ServerState,
    namespace: &bcode_provider_auth_models::managed::AuthNamespace,
) -> Result<(), ErrorResponse> {
    match std::fs::symlink_metadata(terminal_fence_path(state, namespace)?) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(failure(ManagedHarnessError::Uncertain)),
        Err(_) => Err(failure(ManagedHarnessError::Storage)),
    }
}

fn issue_terminal_fence(
    state: &crate::ServerState,
    namespace: &bcode_provider_auth_models::managed::AuthNamespace,
    claim: &bcode_provider_auth::managed::ClientTerminalClaim,
) -> Result<(), ErrorResponse> {
    use std::io::Write as _;
    let path = terminal_fence_path(state, namespace)?;
    let directory = path
        .parent()
        .ok_or_else(|| failure(ManagedHarnessError::Storage))?;
    std::fs::create_dir_all(directory).map_err(|_| failure(ManagedHarnessError::Storage))?;
    std::fs::File::open(&state.state_root)
        .and_then(|file| file.sync_all())
        .map_err(|_| failure(ManagedHarnessError::Storage))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|_| failure(ManagedHarnessError::Uncertain))?;
    file.write_all(claim.claim_id.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|_| failure(ManagedHarnessError::Storage))?;
    std::fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|_| failure(ManagedHarnessError::Storage))?;
    Ok(())
}

fn retire_terminal_fence(
    state: &crate::ServerState,
    namespace: &bcode_provider_auth_models::managed::AuthNamespace,
    claim: &bcode_provider_auth::managed::ClientTerminalClaim,
) -> Result<(), ErrorResponse> {
    let path = terminal_fence_path(state, namespace)?;
    let retained =
        std::fs::read_to_string(&path).map_err(|_| failure(ManagedHarnessError::Storage))?;
    if retained != claim.claim_id {
        return Err(failure(ManagedHarnessError::Conflict));
    }
    std::fs::remove_file(&path).map_err(|_| failure(ManagedHarnessError::Storage))?;
    std::fs::File::open(
        path.parent()
            .ok_or_else(|| failure(ManagedHarnessError::Storage))?,
    )
    .and_then(|file| file.sync_all())
    .map_err(|_| failure(ManagedHarnessError::Storage))?;
    Ok(())
}

async fn terminal_admission(
    state: &Arc<crate::ServerState>,
    caller: &ManagedCaller,
    session_id: SessionId,
    instance_id: &str,
    interface_id: &str,
) -> Result<
    (
        ManagedAuthAdmission,
        crate::harness::HarnessAuthMethodEvidence,
        bcode_ipc::harness::ManagedNativeLoginPlan,
    ),
    ErrorResponse,
> {
    if interface_id.is_empty() || interface_id.len() > 256 {
        return Err(failure(ManagedHarnessError::Invalid));
    }
    let backend = state
        .managed_sessions
        .get()
        .ok_or_else(|| failure(ManagedHarnessError::Storage))?;
    let initial = backend
        .auth_scope(caller, session_id, instance_id)
        .await
        .map_err(failure)?;
    let plan = selected_handoff_plan(
        state,
        session_id,
        &initial.instance,
        interface_id,
        &initial.scope.namespace,
    )
    .await?;
    // Plugin preparation awaits external I/O; reauthorize without recursively holding a lease.
    let previous_instance = initial.instance.clone();
    let previous_namespace = initial.scope.namespace.clone();
    let previous_waiter = initial.scope.waiter.clone();
    let previous_custody = initial.scope.custody;
    drop(initial);
    let mut current = backend
        .auth_scope(caller, session_id, instance_id)
        .await
        .map_err(failure)?;
    if previous_instance != current.instance
        || previous_namespace != current.scope.namespace
        || previous_waiter != current.scope.waiter
        || previous_custody != current.scope.custody
    {
        return Err(failure(ManagedHarnessError::StaleOwner));
    }
    let method = serde_json::from_value(
        serde_json::to_value(&plan.method).map_err(|_| failure(ManagedHarnessError::Invalid))?,
    )
    .map_err(|_| failure(ManagedHarnessError::Invalid))?;
    crate::harness::CanonicalManagedAdmission::bind_client_terminal_preparation(
        &mut current,
        &method,
    )
    .map_err(failure)?;
    Ok((current, method, plan))
}

async fn prepare_client_terminal(
    state: &Arc<crate::ServerState>,
    caller: ManagedCaller,
    request: ManagedSessionRequest,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    use bcode_provider_auth::managed::ManagedAuthCoordinator;
    let ManagedCaller::Client(client) = &caller else {
        return Err(failure(ManagedHarnessError::Unsupported));
    };
    let input: bcode_ipc::harness::ManagedSessionPrepareHandoffRequest =
        serde_json::from_str(&request.request_json)
            .map_err(|_| failure(ManagedHarnessError::Invalid))?;
    let (admission, method, _plan) = terminal_admission(
        state,
        &caller,
        input.session_id,
        &input.instance_id,
        &input.interface_id,
    )
    .await?;
    require_unissued_terminal(state, &admission.scope.namespace)?;
    let authority = AuthAuthority {
        scope: crate::harness_auth::VerifiedHarnessAuthScope {
            waiter: admission.scope.waiter.clone(),
            namespace: admission.scope.namespace.clone(),
            custody: admission.scope.custody,
            method: admission.scope.method.clone(),
        },
        caller_id: client.to_string(),
        instance_id: input.instance_id.clone(),
    };
    let auth = state
        .managed_auth
        .get_or_try_init(|| async {
            ManagedAuthCoordinator::new(
                state.daemon_status.instance_id.clone(),
                64,
                128,
                std::time::Duration::from_secs(600),
            )
            .map(crate::harness_auth::HarnessAuthService::new)
        })
        .await
        .map_err(|_| failure(ManagedHarnessError::Unavailable))?;
    let joined = auth
        .begin_client_terminal(
            &authority,
            &authority.caller_id,
            &input.session_id.to_string(),
            &input.instance_id,
        )
        .await
        .map_err(|_| failure(ManagedHarnessError::Uncertain))?;
    let Some(command) = joined.command else {
        return auth_response(&joined.snapshot);
    };
    let ticket = command.ticket.clone();
    let (outgoing, invocation) =
        crate::harness::SensitiveHarnessAuthRequest::from_client_terminal_invocation(
            command,
            admission.instance.clone(),
            &method,
        )
        .map_err(failure)?;
    let response = crate::harness::HarnessPluginTransport::new(state.clone())
        .invoke_sensitive_auth(&admission, outgoing)
        .await;
    let Ok(response) = response else {
        let _ = auth.delivery_uncertain(&ticket).await;
        return Err(failure(ManagedHarnessError::Uncertain));
    };
    let (snapshot, pending) = auth
        .accept_harness_response(&admission.instance.plugin_id, &invocation, response)
        .await
        .map_err(|_| failure(ManagedHarnessError::Uncertain))?;
    if let Some(command) = pending {
        let _ = auth.delivery_uncertain(&command.ticket).await;
        return Err(failure(ManagedHarnessError::Uncertain));
    }
    auth_response(&snapshot)
}

async fn acknowledge_terminal_reap(
    state: &crate::ServerState,
    auth: &crate::harness_auth::HarnessAuthService,
    authority: &AuthAuthority,
    input: &bcode_ipc::harness::ManagedSessionHandoffRequest,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    auth.status(
        authority,
        &authority.caller_id,
        &input.session_id.to_string(),
        &input.instance_id,
        &input.handle,
    )
    .await
    .map_err(|_| failure(ManagedHarnessError::StaleOwner))?;
    let claim = auth
        .client_terminal_claim(&input.handle)
        .await
        .map_err(|_| failure(ManagedHarnessError::StaleOwner))?
        .ok_or_else(|| failure(ManagedHarnessError::Conflict))?;
    auth.client_terminal_reaped(&authority.caller_id, &claim)
        .await
        .map_err(|_| failure(ManagedHarnessError::StaleOwner))?;
    retire_terminal_fence(state, &authority.scope.namespace, &claim)?;
    auth_response(&serde_json::json!({"reaped": true}))
}

async fn issue_terminal_plan(
    state: &crate::ServerState,
    auth: &crate::harness_auth::HarnessAuthService,
    authority: &AuthAuthority,
    input: &bcode_ipc::harness::ManagedSessionHandoffRequest,
    handoff: &bcode_ipc::harness::ManagedNativeLoginPlan,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    let interaction = auth
        .sensitive_interaction(
            authority,
            &authority.caller_id,
            &input.session_id.to_string(),
            &input.instance_id,
            &input.handle,
        )
        .await
        .map_err(|_| failure(ManagedHarnessError::StaleOwner))?;
    if !matches!(interaction, Some((_, bcode_provider_auth_models::managed::SensitiveAuthInteraction::NativeHandoff { ref request_id })) if request_id == &input.interaction_id)
    {
        return Err(failure(ManagedHarnessError::Conflict));
    }
    let claim = auth
        .claim_client_terminal(
            authority,
            &authority.caller_id,
            &input.session_id.to_string(),
            &input.instance_id,
            crate::harness_auth::ClientTerminalClaimRequest {
                handle: &input.handle,
                interaction_id: &input.interaction_id,
                claim_id: uuid::Uuid::new_v4().to_string(),
            },
        )
        .await
        .map_err(|_| failure(ManagedHarnessError::Uncertain))?;
    issue_terminal_fence(state, &authority.scope.namespace, &claim)?;
    // The client's explicit action launches this exact plan. Result is not credential readiness;
    // client must continue the same live auth flow after native exit and let the plugin inspect.
    auth_response(handoff)
}

async fn continue_terminal_auth(
    state: &Arc<crate::ServerState>,
    auth: &crate::harness_auth::HarnessAuthService,
    authority: &AuthAuthority,
    input: &bcode_ipc::harness::ManagedSessionHandoffRequest,
    instance: bcode_harness_models::HarnessInstance,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    let command = auth
        .continue_flow(
            authority,
            &authority.caller_id,
            &input.session_id.to_string(),
            &input.instance_id,
            &input.handle,
            None,
        )
        .await
        .map_err(|_| {
            ErrorResponse::new(
                "managed_session_continue_issue_uncertain",
                "The retained auth flow cannot issue this continuation; reconcile without replay.",
            )
        })?;
    let snapshot = deliver_auth(state, input.session_id, auth, instance, command).await.map_err(|error| {
        if error.code == "managed_session_uncertain" {
            ErrorResponse::new("managed_session_continue_delivery_uncertain",
                "Auth continuation delivery or response acceptance is uncertain; do not replay.")
        } else { error }
    })?;
    auth_response(&snapshot)
}

fn terminal_ack_failure(cause: bcode_provider_auth::managed::CoordinationError) -> ErrorResponse {
    use bcode_provider_auth::managed::CoordinationError as E;
    let code = match cause {
        E::Invalid => "managed_session_ack_invalid",
        E::NotFound => "managed_session_ack_not_found",
        E::Stale => "managed_session_ack_stale",
        E::Conflict => "managed_session_ack_conflict",
        E::Capacity => "managed_session_ack_capacity",
        E::Busy => "managed_session_ack_not_settled",
        E::Terminal => "managed_session_ack_terminal",
    };
    ErrorResponse::new(
        code,
        "Terminal authentication acknowledgment was rejected; readiness and replay authority are unchanged.",
    )
}

async fn prepare_handoff(
    state: &Arc<crate::ServerState>,
    caller: ManagedCaller,
    request: ManagedSessionRequest,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    use bcode_ipc::harness::ManagedSessionHandoffRequest;
    let ManagedCaller::Client(client) = &caller else {
        return Err(failure(ManagedHarnessError::Unsupported));
    };
    let input: ManagedSessionHandoffRequest = serde_json::from_str(&request.request_json)
        .map_err(|_| failure(ManagedHarnessError::Invalid))?;
    if (input.interaction_id.is_empty() && request.operation != ManagedSessionOperation::AuthStatus)
        || input.interaction_id.len() > 256
        || input.interface_id.is_empty()
        || input.interface_id.len() > 256
    {
        return Err(failure(ManagedHarnessError::Invalid));
    }
    let (admission, _method, handoff) = terminal_admission(
        state,
        &caller,
        input.session_id,
        &input.instance_id,
        &input.interface_id,
    )
    .await?;
    let ManagedAuthAdmission {
        scope,
        instance,
        ownership_hold: _hold,
        ..
    } = admission;
    if instance.instance_id != input.instance_id
        || scope.waiter.session_id != input.session_id.to_string()
        || scope.namespace.plugin_id != instance.plugin_id
        || scope.namespace.credential_namespace != instance.auth.credential_namespace
        || scope.custody != instance.auth.custody
    {
        return Err(failure(ManagedHarnessError::StaleOwner));
    }
    let caller_id = client.to_string();
    let authority = AuthAuthority {
        scope,
        caller_id: caller_id.clone(),
        instance_id: input.instance_id.clone(),
    };
    let auth = state
        .managed_auth
        .get()
        .ok_or_else(|| failure(ManagedHarnessError::StaleOwner))?;
    if request.operation == ManagedSessionOperation::AuthStatus {
        let interaction = auth
            .sensitive_interaction(
                &authority,
                &authority.caller_id,
                &input.session_id.to_string(),
                &input.instance_id,
                &input.handle,
            )
            .await
            .map_err(|_| failure(ManagedHarnessError::StaleOwner))?;
        return auth_response(&interaction);
    }
    if request.operation == ManagedSessionOperation::AuthReaped {
        return acknowledge_terminal_reap(state, auth, &authority, &input).await;
    }
    if request.operation == ManagedSessionOperation::AuthAcknowledge {
        auth.acknowledge_terminal(
            &authority,
            &authority.caller_id,
            &input.session_id.to_string(),
            &input.instance_id,
            &input.handle,
        )
        .await
        .map_err(terminal_ack_failure)?;
        return auth_response(&serde_json::json!({"acknowledged": true}));
    }
    if request.operation == ManagedSessionOperation::AuthContinue {
        return continue_terminal_auth(state, auth, &authority, &input, instance).await;
    }
    issue_terminal_plan(state, auth, &authority, &input, &handoff).await
}

struct AuthAuthority {
    scope: crate::harness_auth::VerifiedHarnessAuthScope,
    caller_id: String,
    instance_id: String,
}
impl crate::harness_auth::HarnessAuthAuthority for AuthAuthority {
    fn resolve(
        &self,
        caller: &str,
        session_id: &str,
        instance_id: &str,
    ) -> Result<
        crate::harness_auth::VerifiedHarnessAuthScope,
        bcode_provider_auth::managed::CoordinationError,
    > {
        // The enclosing route has just resolved this scope for one authenticated connection;
        // none of these caller/lease facts came from the portable auth payload.
        if caller != self.caller_id
            || session_id != self.scope.waiter.session_id
            || instance_id != self.instance_id
        {
            return Err(bcode_provider_auth::managed::CoordinationError::Stale);
        }
        Ok(crate::harness_auth::VerifiedHarnessAuthScope {
            waiter: self.scope.waiter.clone(),
            namespace: self.scope.namespace.clone(),
            custody: self.scope.custody,
            method: self.scope.method.clone(),
        })
    }
}

async fn dispatch_auth_status(
    state: &Arc<crate::ServerState>,
    auth: &crate::harness_auth::HarnessAuthService,
    authority: &AuthAuthority,
    caller_id: &str,
    session: &str,
    input: &bcode_ipc::harness::ManagedSessionAuthRequest,
    instance: &bcode_harness_models::HarnessInstance,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    let rejected = |_| failure(ManagedHarnessError::Invalid);
    if input.input.is_some() {
        return Err(failure(ManagedHarnessError::Invalid));
    }
    if input.handle.is_none() {
        if input.sensitive_interaction {
            return Err(failure(ManagedHarnessError::Invalid));
        }
        let outgoing = bcode_harness_models::HarnessRequest {
            schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
            request_id: format!("auth-status:{}", uuid::Uuid::new_v4()),
            instance: instance.clone(),
            kind: HarnessRequestKind::AuthStatus { verify: false },
        };
        let response = invoke_sensitive_auth(state, input.session_id, &outgoing)
            .await
            .map_err(failure)?;
        response
            .validate_for(&outgoing)
            .map_err(|_| failure(ManagedHarnessError::Invalid))?;
        let bcode_harness_models::HarnessServiceResponseKind::AuthStatus(status) = response.kind
        else {
            return Err(failure(ManagedHarnessError::Invalid));
        };
        return auth_response(&status);
    }
    let handle = input
        .handle
        .as_ref()
        .ok_or_else(|| failure(ManagedHarnessError::Invalid))?;
    if input.sensitive_interaction {
        let interaction = auth
            .sensitive_interaction(authority, caller_id, session, &input.instance_id, handle)
            .await
            .map_err(rejected)?;
        return auth_response(&interaction);
    }
    let snapshot = auth
        .status(authority, caller_id, session, &input.instance_id, handle)
        .await
        .map_err(rejected)?;
    auth_response(&snapshot)
}

async fn acknowledge_auth(
    state: &Arc<crate::ServerState>,
    auth: &crate::harness_auth::HarnessAuthService,
    authority: &AuthAuthority,
    caller_id: &str,
    session: &str,
    input: &bcode_ipc::harness::ManagedSessionAuthRequest,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    let rejected = |_| failure(ManagedHarnessError::Invalid);
    if input.input.is_some() {
        return Err(failure(ManagedHarnessError::Invalid));
    }
    let handle = input
        .handle
        .as_ref()
        .ok_or_else(|| failure(ManagedHarnessError::Invalid))?;
    auth.acknowledge_terminal(authority, caller_id, session, &input.instance_id, handle)
        .await
        .map_err(rejected)?;
    // Other waiters retain their terminal state. Retirement succeeds only when the
    // domain confirms all terminal waiters acknowledged or expired.
    let _ = auth.retire(handle).await;
    state
        .managed_auth_handoffs
        .lock()
        .await
        .remove(&handle.flow_id);
    auth_response(&serde_json::json!({"acknowledged":true}))
}

async fn begin_auth(
    state: &Arc<crate::ServerState>,
    auth: &crate::harness_auth::HarnessAuthService,
    authority: &AuthAuthority,
    input: &bcode_ipc::harness::ManagedSessionAuthRequest,
    instance: bcode_harness_models::HarnessInstance,
    fresh_begin_authorized: bool,
) -> Result<bcode_provider_auth::managed::ManagedFlowSnapshot, ErrorResponse> {
    let rejected = |_| failure(ManagedHarnessError::Invalid);
    require_unissued_terminal(state, &authority.scope.namespace)?;
    if !fresh_begin_authorized {
        return Err(failure(ManagedHarnessError::Uncertain));
    }
    if input.handle.is_some() || input.input.is_some() || input.sensitive_interaction {
        return Err(failure(ManagedHarnessError::Invalid));
    }
    let joined = auth
        .begin(
            authority,
            &authority.caller_id,
            &authority.scope.waiter.session_id,
            &input.instance_id,
        )
        .await
        .map_err(rejected)?;
    if let Some(command) = joined.command {
        deliver_auth(state, input.session_id, auth, instance, command).await
    } else {
        Ok(joined.snapshot)
    }
}

fn decode_auth_input(
    request: &ManagedSessionRequest,
) -> Result<bcode_ipc::harness::ManagedSessionAuthRequest, ErrorResponse> {
    let input: bcode_ipc::harness::ManagedSessionAuthRequest =
        serde_json::from_str(&request.request_json)
            .map_err(|_| failure(ManagedHarnessError::Invalid))?;
    if input.instance_id.is_empty()
        || input.instance_id.len() > bcode_harness_models::MAX_HARNESS_ID_BYTES
        || input.input.as_ref().is_some_and(|value| !value.is_valid())
    {
        return Err(failure(ManagedHarnessError::Invalid));
    }
    if input.sensitive_interaction && request.operation != ManagedSessionOperation::AuthStatus {
        return Err(failure(ManagedHarnessError::Invalid));
    }
    Ok(input)
}

async fn dispatch_readonly_auth_status(
    state: &Arc<crate::ServerState>,
    caller: &ManagedCaller,
    input: &bcode_ipc::harness::ManagedSessionAuthRequest,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    if input.input.is_some() || input.sensitive_interaction {
        return Err(failure(ManagedHarnessError::Invalid));
    }
    state
        .managed_sessions
        .get()
        .ok_or_else(|| failure(ManagedHarnessError::Storage))?;
    let admission = crate::harness::CanonicalManagedAdmission::new(
        crate::harness::CanonicalHarnessStore::new(state),
    );
    let inspected =
        Box::pin(admission.authorize_auth_status(caller, input.session_id, &input.instance_id))
            .await
            .inspect_err(|error| reject_stage("readonly_auth_status", *error))
            .map_err(failure)?;
    drop(admission);
    auth_response(&inspected.status)
}

async fn auth_coordinator(
    state: &crate::ServerState,
) -> Result<&crate::harness_auth::HarnessAuthService, ErrorResponse> {
    use bcode_provider_auth::managed::ManagedAuthCoordinator;
    state
        .managed_auth
        .get_or_try_init(|| async {
            ManagedAuthCoordinator::new(
                state.daemon_status.instance_id.clone(),
                64,
                128,
                std::time::Duration::from_secs(600),
            )
            .map(crate::harness_auth::HarnessAuthService::new)
        })
        .await
        .map_err(|_| failure(ManagedHarnessError::Unavailable))
}

fn validate_auth_admission(
    scope: &crate::harness_auth::VerifiedHarnessAuthScope,
    instance: &bcode_harness_models::HarnessInstance,
    input: &bcode_ipc::harness::ManagedSessionAuthRequest,
) -> Result<(), ErrorResponse> {
    if instance.instance_id != input.instance_id
        || scope.waiter.session_id != input.session_id.to_string()
        || instance.plugin_id != scope.namespace.plugin_id
        || instance.auth.credential_namespace != scope.namespace.credential_namespace
        || instance.auth.custody != scope.custody
        || instance.validate().is_err()
        || !scope.namespace.is_valid()
    {
        return Err(failure(ManagedHarnessError::StaleOwner));
    }
    Ok(())
}

async fn dispatch_auth(
    state: &Arc<crate::ServerState>,
    caller: ManagedCaller,
    request: ManagedSessionRequest,
) -> Result<ManagedSessionResponse, ErrorResponse> {
    use bcode_provider_auth::managed::ManagedFlowSnapshot;
    let ManagedCaller::Client(client_id) = &caller else {
        return Err(failure(ManagedHarnessError::Unsupported));
    };
    let caller_id = client_id.to_string();
    let input = decode_auth_input(&request)?;
    if request.operation == ManagedSessionOperation::AuthStatus && input.handle.is_none() {
        return Box::pin(dispatch_readonly_auth_status(state, &caller, &input)).await;
    }
    let backend = state
        .managed_sessions
        .get()
        .ok_or_else(|| failure(ManagedHarnessError::Storage))?;
    let ManagedAuthAdmission {
        scope,
        instance,
        ownership_hold: _ownership_hold,
        fresh_begin_authorized,
    } = backend
        .auth_scope(&caller, input.session_id, &input.instance_id)
        .await
        .inspect_err(|error| reject_stage("auth_scope", *error))
        .map_err(failure)?;
    validate_auth_admission(&scope, &instance, &input)?;
    let authority = AuthAuthority {
        scope,
        caller_id: caller_id.clone(),
        instance_id: input.instance_id.clone(),
    };
    let auth = auth_coordinator(state).await?;
    let session = input.session_id.to_string();
    let rejected = |_| failure(ManagedHarnessError::StaleOwner);
    let snapshot: ManagedFlowSnapshot = match request.operation {
        ManagedSessionOperation::AuthAcknowledge => {
            return acknowledge_auth(state, auth, &authority, &caller_id, &session, &input).await;
        }
        ManagedSessionOperation::AuthBegin => {
            begin_auth(
                state,
                auth,
                &authority,
                &input,
                instance,
                fresh_begin_authorized,
            )
            .await?
        }
        ManagedSessionOperation::AuthStatus => {
            return dispatch_auth_status(
                state, auth, &authority, &caller_id, &session, &input, &instance,
            )
            .await;
        }
        ManagedSessionOperation::AuthContinue => {
            let handle = input
                .handle
                .as_ref()
                .ok_or_else(|| failure(ManagedHarnessError::Invalid))?;
            let command = auth
                .continue_flow(
                    &authority,
                    &caller_id,
                    &session,
                    &input.instance_id,
                    handle,
                    input.input,
                )
                .await
                .map_err(rejected)?;
            deliver_auth(state, input.session_id, auth, instance, command).await?
        }
        ManagedSessionOperation::AuthCancel => {
            if input.input.is_some() {
                return Err(failure(ManagedHarnessError::Invalid));
            }
            let handle = input
                .handle
                .as_ref()
                .ok_or_else(|| failure(ManagedHarnessError::Invalid))?;
            if let Some(command) = auth
                .cancel(&authority, &caller_id, &session, &input.instance_id, handle)
                .await
                .map_err(rejected)?
            {
                deliver_auth(state, input.session_id, auth, instance, command).await?
            } else {
                return auth_response(&serde_json::json!({"detached": true}));
            }
        }
        _ => return Err(failure(ManagedHarnessError::Invalid)),
    };
    auth_response(&snapshot)
}

fn auth_response(value: &impl serde::Serialize) -> Result<ManagedSessionResponse, ErrorResponse> {
    let response_json =
        serde_json::to_string(value).map_err(|_| failure(ManagedHarnessError::Invalid))?;
    if response_json.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES {
        return Err(failure(ManagedHarnessError::Invalid));
    }
    Ok(ManagedSessionResponse {
        schema_version: MANAGED_SESSION_VERSION,
        response_json,
    })
}

/// Auth-only selected-plugin channel. Inputs are daemon-issued and ephemeral; no ordinary
/// runtime store, observation broadcast, raw plugin errors, or model callback sees them.
async fn invoke_sensitive_auth(
    state: &Arc<crate::ServerState>,
    session_id: SessionId,
    request: &bcode_harness_models::HarnessRequest,
) -> Result<HarnessServiceResponse, ManagedHarnessError> {
    if !matches!(
        request.kind,
        HarnessRequestKind::AuthStatus { .. }
            | HarnessRequestKind::AuthBegin(_)
            | HarnessRequestKind::AuthContinue(_)
            | HarnessRequestKind::AuthCancel(_)
    ) {
        return Err(ManagedHarnessError::Invalid);
    }
    request
        .validate()
        .map_err(|_| ManagedHarnessError::Invalid)?;
    let manifest = state
        .plugins
        .registry()
        .manifests()
        .get(&request.instance.plugin_id)
        .ok_or(ManagedHarnessError::Unavailable)?;
    bcode_plugin::harness::harness_contribution(manifest)
        .map_err(|_| ManagedHarnessError::Unavailable)?;
    let bytes = serde_json::to_vec(request).map_err(|_| ManagedHarnessError::Invalid)?;
    if bytes.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES {
        return Err(ManagedHarnessError::Invalid);
    }
    let view = crate::harness::configuration_views::for_session(state, session_id).await?;
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        view.invoke_service_with_bridge_scoped(
            &request.instance.plugin_id,
            bcode_harness_models::HARNESS_INTERFACE_ID,
            request.kind.operation().as_str(),
            bytes,
            bcode_plugin::PluginInvocationScope::Global,
            None,
        ),
    )
    .await
    .map_err(|_| ManagedHarnessError::Deadline)?
    .map_err(|_| ManagedHarnessError::Unavailable)?;
    drop(view);
    if response.error.is_some()
        || response.payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES
    {
        return Err(ManagedHarnessError::Invalid);
    }
    let decoded: HarnessServiceResponse =
        serde_json::from_slice(&response.payload).map_err(|_| ManagedHarnessError::Invalid)?;
    decoded
        .validate_for(request)
        .map_err(|_| ManagedHarnessError::Invalid)?;
    Ok(decoded)
}

async fn deliver_auth(
    state: &Arc<crate::ServerState>,
    session_id: SessionId,
    auth: &crate::harness_auth::HarnessAuthService,
    instance: bcode_harness_models::HarnessInstance,
    command: bcode_provider_auth::managed::ManagedAuthCommand,
) -> Result<bcode_provider_auth::managed::ManagedFlowSnapshot, ErrorResponse> {
    let ticket = command.ticket.clone();
    let plugin_id = instance.plugin_id.clone();
    let Ok(mut invocation) =
        crate::harness_auth::HarnessAuthInvocation::prepare(command, instance.clone())
    else {
        let _ = auth.delivery_uncertain(&ticket).await;
        return Err(failure(ManagedHarnessError::Uncertain));
    };
    let outgoing = invocation
        .take_request()
        .ok_or_else(|| failure(ManagedHarnessError::Conflict))?;
    let Ok(response) = invoke_sensitive_auth(state, session_id, &outgoing).await else {
        let _ = auth.delivery_uncertain(&ticket).await;
        return Err(failure(ManagedHarnessError::Uncertain));
    };
    let (snapshot, cancellation) = auth
        .accept_harness_response(&plugin_id, &invocation, response)
        .await
        .map_err(|_| failure(ManagedHarnessError::Uncertain))?;
    if let Some(command) = cancellation {
        // Bounded follow-up is issued by the coordinator, never reconstructed from client data.
        let next_ticket = command.ticket.clone();
        let Ok(mut cancel) = crate::harness_auth::HarnessAuthInvocation::prepare(command, instance)
        else {
            let _ = auth.delivery_uncertain(&next_ticket).await;
            return Err(failure(ManagedHarnessError::Uncertain));
        };
        let outgoing = cancel
            .take_request()
            .ok_or_else(|| failure(ManagedHarnessError::Conflict))?;
        let Ok(response) = invoke_sensitive_auth(state, session_id, &outgoing).await else {
            let _ = auth.delivery_uncertain(&next_ticket).await;
            return Err(failure(ManagedHarnessError::Uncertain));
        };
        let (snapshot, pending) = auth
            .accept_harness_response(&plugin_id, &cancel, response)
            .await
            .map_err(|_| failure(ManagedHarnessError::Uncertain))?;
        if let Some(pending) = pending {
            let _ = auth.delivery_uncertain(&pending.ticket).await;
        }
        return Ok(snapshot);
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::{
        HARNESS_SCHEMA_VERSION, HarnessAuthBinding, HarnessAuthCustody, HarnessInstance,
        HarnessRequest,
    };

    fn request(operation: ManagedSessionOperation) -> ManagedSessionRequest {
        let call = ManagedSessionCall {
            session_id: Some(SessionId::new()),
            harness: HarnessRequest {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: "inspection".into(),
                instance: HarnessInstance {
                    instance_id: "instance".into(),
                    plugin_id: "native".into(),
                    executable: "/explicit/native".into(),
                    config_namespace: "/explicit/config".into(),
                    auth: HarnessAuthBinding {
                        provider_id: "auth".into(),
                        profile: "native".into(),
                        credential_namespace: "credentials".into(),
                        custody: HarnessAuthCustody::NativeManaged,
                    },
                },
                kind: HarnessRequestKind::Inspect,
            },
            parent_session_id: None,
            assistance_destination_session_id: None,
        };
        ManagedSessionRequest {
            schema_version: MANAGED_SESSION_VERSION,
            operation,
            request_json: serde_json::to_string(&call).unwrap(),
        }
    }

    #[tokio::test]
    async fn prior_terminal_issue_is_fenced_independently_of_daemon_memory() {
        let root = tempfile::tempdir().unwrap();
        let mut state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        state.state_root = root.path().to_path_buf();
        let namespace = bcode_provider_auth_models::managed::AuthNamespace {
            plugin_id: "native".into(),
            credential_namespace: "/explicit/config".into(),
        };
        assert!(require_unissued_terminal(&state, &namespace).is_ok());
        let path = terminal_fence_path(&state, &namespace).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Even a partially written prior issue must not become another launch permit.
        std::fs::write(&path, []).unwrap();
        assert_eq!(
            require_unissued_terminal(&state, &namespace)
                .unwrap_err()
                .code,
            "managed_session_uncertain"
        );
        assert!(path.exists());
        drop(state);
    }

    #[tokio::test]
    async fn missing_canonical_bridge_fails_closed_without_native_dispatch() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let result = dispatch(
            &state,
            ManagedCaller::Client(ClientId::new()),
            request(ManagedSessionOperation::Inspect),
        )
        .await;
        assert_eq!(
            result.unwrap_err().code,
            "managed_session_storage_unavailable"
        );
        assert!(state.managed_sessions.get().is_none());
        drop(state);
    }

    #[tokio::test]
    async fn workflow_callback_rejects_forged_invocation_before_receipt_lookup() {
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        let result = workflow_invocation(
            &state,
            "run",
            "dispatch",
            "plugin",
            bcode_tool::ToolInvocationServiceRequest {
                invocation_id: "different-dispatch".into(),
                request_id: "callback".into(),
                route_id: None,
                interface_id: MANAGED_SESSION_INTERFACE_ID.into(),
                operation: "followup".into(),
                payload: serde_json::json!({}),
            },
        )
        .await;
        assert!(
            matches!(result, bcode_tool::ToolInvocationServiceResolution::Failed { code, .. } if code == "invocation_id_mismatch")
        );
        assert!(state.managed_sessions.get().is_none());
        drop(state);
    }

    #[tokio::test]
    async fn workflow_caller_cannot_use_native_service_callback() {
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        let result = invocation(
            &state,
            ManagedCaller::Workflow {
                run_id: "run".into(),
                dispatch_identity: "dispatch".into(),
                session_id: SessionId::new(),
                plugin_id: "plugin".into(),
            },
            bcode_tool::ToolInvocationServiceRequest {
                invocation_id: "dispatch".into(),
                request_id: "callback".into(),
                route_id: None,
                interface_id: bcode_harness_models::HARNESS_INTERFACE_ID.into(),
                operation: "submit".into(),
                payload: serde_json::json!({}),
            },
        )
        .await;
        assert!(matches!(
            result,
            bcode_tool::ToolInvocationServiceResolution::Unsupported
        ));
        assert!(state.managed_sessions.get().is_none());
        drop(state);
    }

    #[tokio::test]
    async fn generic_service_route_never_returns_sensitive_interaction_material() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let request = ManagedSessionRequest {
            schema_version: MANAGED_SESSION_VERSION,
            operation: ManagedSessionOperation::AuthStatus,
            request_json:
                serde_json::json!({"session_id":SessionId::new(), "instance_id":"instance",
                "handle":null, "input":null, "sensitive_interaction":true})
                .to_string(),
        };
        let error = service(
            &state,
            ClientId::new(),
            "auth_status".into(),
            serde_json::to_vec(&request).unwrap(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "managed_session_sensitive_transport_required");
        assert!(state.managed_auth.get().is_none());
        drop(state);
    }

    #[tokio::test]
    async fn auth_only_channel_rejects_non_auth_before_plugin_dispatch() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let request = request(ManagedSessionOperation::Inspect)
            .decode_call()
            .unwrap();
        let error = invoke_sensitive_auth(&state, SessionId::new(), &request.harness)
            .await
            .unwrap_err();
        drop(state);
        assert_eq!(error, ManagedHarnessError::Invalid);
    }

    #[tokio::test]
    async fn operation_mismatch_rejects_before_admission() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let result = dispatch(
            &state,
            ManagedCaller::Client(ClientId::new()),
            request(ManagedSessionOperation::Respond),
        )
        .await;
        assert_eq!(result.unwrap_err().code, "managed_session_invalid");
        assert!(state.managed_sessions.get().is_none());
        drop(state);
    }

    #[tokio::test]
    async fn model_invocations_cannot_enter_sensitive_auth_route() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let result = dispatch(
            &state,
            ManagedCaller::Invocation {
                session_id: SessionId::new(),
                plugin_id: "native".into(),
                invocation_id: "invocation".into(),
            },
            request(ManagedSessionOperation::AuthBegin),
        )
        .await;
        assert_eq!(result.unwrap_err().code, "managed_session_unsupported");
        assert!(state.managed_auth.get().is_none());
        drop(state);
    }

    #[test]
    fn fresh_start_or_callback_material_is_not_ordinary_runtime_authority() {
        let value = request(ManagedSessionOperation::AuthContinue);
        assert!(value.decode_call().is_err());
        assert!(!format!("{value:?}").contains("credentials"));
    }

    #[tokio::test]
    async fn history_cannot_inherit_cross_session_authority_from_parentage() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let query = bcode_ipc::harness::ManagedSessionHistoryRequest {
            session_id: SessionId::new(),
            after: None,
            limit: 1,
        };
        let result = dispatch(
            &state,
            ManagedCaller::Invocation {
                session_id: SessionId::new(),
                plugin_id: "native".into(),
                invocation_id: "invocation".into(),
            },
            ManagedSessionRequest {
                schema_version: MANAGED_SESSION_VERSION,
                operation: ManagedSessionOperation::History,
                request_json: serde_json::to_string(&query).unwrap(),
            },
        )
        .await;
        drop(state);
        assert_eq!(result.unwrap_err().code, "managed_session_stale_owner");
    }
}
