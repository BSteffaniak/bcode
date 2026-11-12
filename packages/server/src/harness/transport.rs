//! Bounded, producer-selected plugin transport. No model/provider fallback is allowed.

use std::{sync::Arc, time::Duration};

use bcode_harness_models::{HarnessRequest, HarnessServiceResponse};

tokio::task_local! {
    static CREDENTIAL_CALLER: (crate::harness_dispatch::ManagedCaller, super::HarnessOwner, Arc<bcode_ipc::harness::ManagedSessionCall>);
}

/// Retain authenticated admission independently of native request bytes.
pub async fn with_credential_admission<T>(
    caller: crate::harness_dispatch::ManagedCaller,
    owner: super::HarnessOwner,
    call: bcode_ipc::harness::ManagedSessionCall,
    operation: impl std::future::Future<Output = T>,
) -> T {
    CREDENTIAL_CALLER
        .scope((caller, owner, Arc::new(call)), operation)
        .await
}

/// Selected-plugin semantic transport. Implementations never dispatch Bcode tools or model turns.
pub trait ManagedHarnessTransport: Send + Sync {
    /// Deliver one validated envelope. Failure after invocation may have native effects.
    /// Deliver a model-selected Open only after its independent canonical delivery claim.
    fn invoke_selected(
        &self,
        request: &bcode_harness_models::HarnessModelsRequest,
    ) -> impl std::future::Future<
        Output = Result<bcode_harness_models::HarnessModelsResponse, ManagedHarnessError>,
    > + Send {
        let _ = request;
        async { Err(ManagedHarnessError::Unsupported) }
    }

    fn invoke(
        &self,
        request: &HarnessRequest,
    ) -> impl std::future::Future<Output = Result<HarnessServiceResponse, ManagedHarnessError>> + Send;
}

impl ManagedHarnessTransport for HarnessPluginTransport {
    async fn invoke_selected(
        &self,
        request: &bcode_harness_models::HarnessModelsRequest,
    ) -> Result<bcode_harness_models::HarnessModelsResponse, ManagedHarnessError> {
        if !matches!(
            &request.kind,
            bcode_harness_models::HarnessModelsRequestKind::OpenSelected { .. }
        ) {
            return Err(ManagedHarnessError::Invalid);
        }
        // Selected mutations are inaccessible without an admitted canonical owner scope.
        let admitted = CREDENTIAL_CALLER
            .try_with(|(_, owner, call)| {
                let bcode_harness_models::HarnessModelsRequestKind::OpenSelected { open, .. } =
                    &request.kind
                else {
                    return false;
                };
                call.harness.instance == request.instance
                    && owner.session_id.to_string() == open.session_id
                    && owner.generation == open.generation
                    && call.harness.request_id == request.request_id
            })
            .unwrap_or(false);
        if !admitted {
            return Err(ManagedHarnessError::StaleOwner);
        }
        self.call_models(request).await
    }

    async fn invoke(
        &self,
        request: &HarnessRequest,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        self.call(
            &request.instance.plugin_id,
            request.kind.operation().as_str(),
            request,
        )
        .await
    }
}

/// Secret-safe failures at the managed session application boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ManagedHarnessError {
    /// Request or plugin response violates the portable contract.
    #[error("invalid managed harness contract")]
    Invalid,
    /// Canonical session storage could not commit or inspect trustworthy state.
    #[error("managed harness canonical storage unavailable")]
    Storage,
    /// The current durable generation does not authorize this operation.
    #[error("managed harness ownership generation changed")]
    StaleOwner,
    /// A retained mutation ID was reused with different content.
    #[error("conflicting managed harness mutation")]
    Conflict,
    /// Prior delivery has an unresolved outcome and must not be repeated.
    #[error("managed harness outcome uncertain; reconcile without replay")]
    Uncertain,
    /// An enabled matching plugin was not available or failed to answer.
    #[error("managed harness plugin unavailable")]
    Unavailable,
    /// A bounded operation exceeded its configured deadline.
    #[error("managed harness operation deadline exceeded")]
    Deadline,
    /// The operation is unsupported by this instance's declared capabilities.
    #[error("managed harness operation unsupported")]
    Unsupported,
}

fn validate_auth_selection(
    admission: &crate::harness_dispatch::ManagedAuthAdmission,
    request: &HarnessRequest,
) -> Result<(), ManagedHarnessError> {
    if request.instance != admission.instance
        || admission.scope.namespace.plugin_id != request.instance.plugin_id
        || admission.scope.namespace.credential_namespace
            != request.instance.auth.credential_namespace
        || admission.scope.custody != request.instance.auth.custody
        || !admission.scope.namespace.is_valid()
    {
        return Err(ManagedHarnessError::StaleOwner);
    }
    Ok(())
}

/// Validate ephemeral auth output without releasing the caller's ownership guard or retaining
/// secret-bearing transport bytes. Namespace/custody and positive admission evidence must agree.
fn validate_auth_result(
    admission: &crate::harness_dispatch::ManagedAuthAdmission,
    request: &HarnessRequest,
    payload: &[u8],
) -> Result<
    (
        HarnessServiceResponse,
        Option<super::HarnessAuthAdmissionEvidence>,
    ),
    ManagedHarnessError,
> {
    use bcode_harness_models::{HarnessRequestKind, HarnessServiceResponseKind};
    let evidence = if matches!(&request.kind, HarnessRequestKind::AuthStatus { .. }) {
        super::auth_inspection::decode_inspection(payload)?
    } else {
        None
    };
    let response: HarnessServiceResponse =
        serde_json::from_slice(payload).map_err(|_| ManagedHarnessError::Invalid)?;
    response
        .validate_for(request)
        .map_err(|_| ManagedHarnessError::Invalid)?;
    let status = match &response.kind {
        HarnessServiceResponseKind::AuthStatus(status) => Some(status),
        HarnessServiceResponseKind::AuthStep(step) => Some(&step.status),
        _ => None,
    };
    if let Some(status) = status
        && (status.namespace != admission.scope.namespace
            || status.custody != admission.scope.custody)
    {
        return Err(ManagedHarnessError::StaleOwner);
    }
    if let Some(value) = &evidence {
        let HarnessServiceResponseKind::AuthStatus(status) = &response.kind else {
            return Err(ManagedHarnessError::Invalid);
        };
        value.validate_for(&admission.instance, status)?;
    }
    Ok((response, evidence))
}

/// Decode a read-only status without exporting method/login evidence as mutation authority.
fn decode_readonly_auth(
    request: &HarnessRequest,
    payload: &[u8],
) -> Result<super::InspectedHarnessAuth, ManagedHarnessError> {
    if !matches!(
        &request.kind,
        bcode_harness_models::HarnessRequestKind::AuthStatus { verify: false }
    ) {
        return Err(ManagedHarnessError::Unsupported);
    }
    let response: HarnessServiceResponse =
        serde_json::from_slice(payload).map_err(|_| ManagedHarnessError::Invalid)?;
    response
        .validate_for(request)
        .map_err(|_| ManagedHarnessError::Invalid)?;
    let bcode_harness_models::HarnessServiceResponseKind::AuthStatus(status) = response.kind else {
        return Err(ManagedHarnessError::Invalid);
    };
    if status.namespace.plugin_id != request.instance.plugin_id
        || status.namespace.credential_namespace != request.instance.auth.credential_namespace
        || status.custody != request.instance.auth.custody
        || !status.namespace.is_valid()
    {
        return Err(ManagedHarnessError::StaleOwner);
    }
    if let Some(evidence) = super::auth_inspection::decode_inspection(payload)? {
        evidence.validate_for(&request.instance, &status)?;
    }
    Ok(super::InspectedHarnessAuth {
        status,
        admission: None,
    })
}

/// Deliberate per-call budget. Native readers must drain independently of these polls.
pub(super) const MAX_CALL_BYTES: usize = 1_048_576;
pub(super) const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Uses the selected plugin, not unique-provider discovery or provider-model dispatch.
#[derive(Clone)]
pub struct HarnessPluginTransport {
    state: Arc<crate::ServerState>,
    view: Option<bcode_plugin::PluginServiceConfigView>,
}

/// Ephemeral auth-only envelope. No serialization/debug implementation is exposed to callers.
/// Construct flow steps only from a daemon-issued auth invocation, never an ordinary harness call.
pub struct SensitiveHarnessAuthRequest {
    request: HarnessRequest,
    terminal_preparation: bool,
}

impl SensitiveHarnessAuthRequest {
    /// Build read-only status for the exact admitted instance. Remote verification is not implied.
    #[must_use]
    pub fn status(admission: &crate::harness_dispatch::ManagedAuthAdmission) -> Self {
        Self {
            terminal_preparation: false,
            request: HarnessRequest {
                schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
                request_id: format!("auth-status:{}", uuid::Uuid::new_v4()),
                instance: admission.instance.clone(),
                kind: bcode_harness_models::HarnessRequestKind::AuthStatus { verify: false },
            },
        }
    }

    /// Take a single daemon-issued login step without copying its sensitive input.
    /// # Errors
    /// Rejects an already consumed invocation. This does not itself authorize delivery.
    pub fn from_invocation(
        invocation: &mut crate::harness_auth::HarnessAuthInvocation,
    ) -> Result<Self, ManagedHarnessError> {
        invocation
            .take_request()
            .map(|request| Self {
                request,
                terminal_preparation: false,
            })
            .ok_or(ManagedHarnessError::Conflict)
    }

    /// Consume one coordinator-issued terminal-preparation begin, never a native login begin.
    /// Caller must reserve the namespace via `begin_client_terminal` before constructing the
    /// invocation, and retain prior-issued claim fencing through plan delivery/client reap.
    /// # Errors
    /// Rejects non-begin commands, wrong method/custody or unsupported execution evidence.
    pub fn from_client_terminal_invocation(
        command: bcode_provider_auth::managed::ManagedAuthCommand,
        instance: bcode_harness_models::HarnessInstance,
        method: &super::HarnessAuthMethodEvidence,
    ) -> Result<(Self, crate::harness_auth::HarnessAuthInvocation), ManagedHarnessError> {
        if command.preparation
            != bcode_provider_auth_models::managed::AuthPreparationMode::ClientTerminal
        {
            return Err(ManagedHarnessError::Unsupported);
        }
        let mut invocation = crate::harness_auth::HarnessAuthInvocation::prepare(command, instance)
            .map_err(|_| ManagedHarnessError::Invalid)?;
        let request = invocation
            .take_request()
            .ok_or(ManagedHarnessError::Conflict)?;
        method.validate_client_terminal(&request.instance)?;
        let bcode_harness_models::HarnessRequestKind::AuthBegin(flow) = &request.kind else {
            return Err(ManagedHarnessError::Unsupported);
        };
        if flow.method_id != method.method_id || flow.input.is_some() {
            return Err(ManagedHarnessError::Conflict);
        }
        Ok((
            Self {
                request,
                terminal_preparation: true,
            },
            invocation,
        ))
    }
}

impl HarnessPluginTransport {
    /// Auth-only ephemeral channel, separate from ordinary runtime dispatch and canonical events.
    /// Admission is borrowed across the entire await, retaining its real ownership hold. Request,
    /// response and plugin errors never enter host request traces or observation/model broadcasts.
    /// Only plugin routing metadata is instrumented by the scoped plugin service API.
    /// # Errors
    /// Rejects foreign instance/namespace, unsupported custody/method, non-auth requests,
    /// malformed responses, disabled plugins, oversized payloads and native transport failures.
    pub async fn invoke_sensitive_auth(
        &self,
        admission: &crate::harness_dispatch::ManagedAuthAdmission,
        input: SensitiveHarnessAuthRequest,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        self.invoke_auth_inner(admission, input)
            .await
            .map(|(response, _)| response)
    }

    /// Inspect native readiness and optional plugin-declared method/login activity under ownership.
    /// The inspection is auth-only and never persists or broadcasts the native response.
    /// # Errors
    /// Rejects invalid evidence, stale ownership, namespace contradictions and native failures.
    pub async fn inspect_auth(
        &self,
        admission: &crate::harness_dispatch::ManagedAuthAdmission,
    ) -> Result<super::InspectedHarnessAuth, ManagedHarnessError> {
        let (response, evidence) = self
            .invoke_auth_inner(admission, SensitiveHarnessAuthRequest::status(admission))
            .await?;
        let bcode_harness_models::HarnessServiceResponseKind::AuthStatus(status) = response.kind
        else {
            return Err(ManagedHarnessError::Invalid);
        };
        if let Some(value) = &evidence {
            value.validate_for(&admission.instance, &status)?;
        }
        Ok(super::InspectedHarnessAuth {
            status,
            admission: evidence,
        })
    }

    async fn invoke_auth_inner(
        &self,
        admission: &crate::harness_dispatch::ManagedAuthAdmission,
        input: SensitiveHarnessAuthRequest,
    ) -> Result<
        (
            HarnessServiceResponse,
            Option<super::HarnessAuthAdmissionEvidence>,
        ),
        ManagedHarnessError,
    > {
        use bcode_harness_models::HarnessRequestKind;
        let terminal_preparation = input.terminal_preparation;
        let request = input.request;
        request
            .validate()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        validate_auth_selection(admission, &request)?;
        match &request.kind {
            HarnessRequestKind::AuthStatus { verify: false } => {}
            HarnessRequestKind::AuthBegin(flow)
            | HarnessRequestKind::AuthContinue(flow)
            | HarnessRequestKind::AuthCancel(flow) => {
                if admission.scope.method.is_empty() || flow.method_id != admission.scope.method {
                    return Err(ManagedHarnessError::Unsupported);
                }
                if matches!(&request.kind, HarnessRequestKind::AuthBegin(_))
                    && !admission.fresh_begin_authorized
                    && !terminal_preparation
                {
                    return Err(ManagedHarnessError::Uncertain);
                }
            }
            _ => return Err(ManagedHarnessError::Unsupported),
        }
        let (artifact_id, daemon_id): (String, String) =
            serde_json::from_str(&admission.scope.waiter.ownership_fence)
                .map_err(|_| ManagedHarnessError::StaleOwner)?;
        let owner = super::HarnessOwner {
            session_id: admission
                .scope
                .waiter
                .session_id
                .parse()
                .map_err(|_| ManagedHarnessError::StaleOwner)?,
            generation: admission.scope.waiter.generation,
            artifact_id,
            daemon_id,
        };
        let store = super::CanonicalHarnessStore::new(&self.state);
        let _ownership = super::ManagedHarnessStore::acquire(&store, &owner).await?;
        if store.selected(&owner).await?.instance != request.instance {
            return Err(ManagedHarnessError::StaleOwner);
        }
        drop(store);
        let payload = Self::with_view(
            Arc::clone(&self.state),
            super::configuration_views::for_owner(&self.state, &owner).await?,
        )
        .auth_payload(&request)
        .await?;
        validate_auth_result(admission, &request, &payload)
    }

    /// Read readiness only; callers must validate canonical selection before and after this call.
    /// No ownership, flow method, login command or native session transport is constructed.
    pub(super) async fn inspect_auth_readonly(
        &self,
        instance: &bcode_harness_models::HarnessInstance,
    ) -> Result<super::InspectedHarnessAuth, ManagedHarnessError> {
        let request = HarnessRequest {
            schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
            request_id: format!("auth-status:{}", uuid::Uuid::new_v4()),
            instance: instance.clone(),
            kind: bcode_harness_models::HarnessRequestKind::AuthStatus { verify: false },
        };
        let payload = self.auth_payload(&request).await?;
        decode_readonly_auth(&request, &payload)
    }

    fn credential_bridge(&self, request: &HarnessRequest) -> bcode_plugin::PluginInvocationBridge {
        // Native publication lookup is independent of vault/auth-provider registration.
        let native_bridge = crate::native_connections::bridge_for_instance(
            Arc::clone(&self.state),
            request.instance.clone(),
        );
        // Only an owning auth declaration opts into exact semantic credential delivery.
        let Some(registered) = self
            .state
            .plugins
            .auth_provider(&request.instance.auth.provider_id)
        else {
            return native_bridge;
        };
        if registered.plugin_id != request.instance.plugin_id {
            return native_bridge;
        }
        let admitted = CREDENTIAL_CALLER.try_with(Clone::clone).ok();
        if let Some((caller, owner, call)) = admitted {
            let state = Arc::clone(&self.state);
            let runtime = tokio::runtime::Handle::current();
            let resolver: bcode_plugin::SensitiveCredentialResolver =
                Arc::new(move |selection, cancellation| {
                    if cancellation.is_cancelled() {
                        return Err(());
                    }
                    let admission = crate::harness::CanonicalManagedAdmission::new(
                        crate::harness::CanonicalHarnessStore::new(&state),
                    );
                    // The SDK callback may run inside the plugin's own Tokio runtime.
                    // Enter the host runtime on a bounded scoped thread, never nested block_on.
                    let secret = std::thread::scope(|scope| {
                        scope
                            .spawn(|| {
                                runtime.block_on(async {
                                    tokio::time::timeout(
                                        CALL_TIMEOUT,
                                        admission.credential(&caller, &owner, &call, selection),
                                    )
                                    .await
                                    .map_err(|_| ())?
                                    .map_err(|_| ())
                                })
                            })
                            .join()
                            .map_err(|_| ())?
                    })?;
                    if cancellation.is_cancelled() {
                        return Err(());
                    }
                    Ok(secret)
                });
            native_bridge.with_credentials(resolver)
        } else {
            // Suppress legacy serialized secret maps even when no credential admission exists.
            native_bridge.with_credentials(Arc::new(|_, _| Err(())))
        }
    }

    async fn auth_payload(&self, request: &HarnessRequest) -> Result<Vec<u8>, ManagedHarnessError> {
        request
            .validate()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        if !matches!(
            &request.kind,
            bcode_harness_models::HarnessRequestKind::AuthStatus { verify: false }
                | bcode_harness_models::HarnessRequestKind::AuthBegin(_)
                | bcode_harness_models::HarnessRequestKind::AuthContinue(_)
                | bcode_harness_models::HarnessRequestKind::AuthCancel(_)
        ) {
            return Err(ManagedHarnessError::Unsupported);
        }
        let plugin_id = &request.instance.plugin_id;
        let manifest = self
            .state
            .plugins
            .registry()
            .manifests()
            .get(plugin_id)
            .ok_or(ManagedHarnessError::Unavailable)?;
        bcode_plugin::harness::harness_contribution(manifest)
            .map_err(|_| ManagedHarnessError::Unavailable)?;
        let bytes = serde_json::to_vec(request).map_err(|_| ManagedHarnessError::Invalid)?;
        if bytes.len() > MAX_CALL_BYTES {
            return Err(ManagedHarnessError::Invalid);
        }
        // Direct selected-plugin service plumbing has routing-only instrumentation. Do not
        // route through public plugin-service IPC, tool invocation, or request/event tracing.
        let response = tokio::time::timeout(
            CALL_TIMEOUT,
            self.configuration_view()
                .await?
                .invoke_service_with_bridge_scoped(
                    plugin_id,
                    bcode_harness_models::HARNESS_INTERFACE_ID,
                    request.kind.operation().as_str(),
                    bytes,
                    bcode_plugin::PluginInvocationScope::Global,
                    self.credential_bridge(request),
                ),
        )
        .await
        .map_err(|_| ManagedHarnessError::Deadline)?
        .map_err(|_| ManagedHarnessError::Unavailable)?;
        if response.payload.len() > MAX_CALL_BYTES || response.error.is_some() {
            return Err(ManagedHarnessError::Invalid);
        }
        Ok(response.payload)
    }

    /// Bind to the owning daemon. Caller admission is performed by the application layer.
    #[must_use]
    pub const fn new(state: Arc<crate::ServerState>) -> Self {
        Self { state, view: None }
    }

    pub(crate) const fn with_view(
        state: Arc<crate::ServerState>,
        view: bcode_plugin::PluginServiceConfigView,
    ) -> Self {
        Self {
            state,
            view: Some(view),
        }
    }

    async fn configuration_view(
        &self,
    ) -> Result<bcode_plugin::PluginServiceConfigView, ManagedHarnessError> {
        if let Ok(owner) = CREDENTIAL_CALLER.try_with(|(_, owner, _)| owner.clone()) {
            return super::configuration_views::for_owner(&self.state, &owner).await;
        }
        let view = match &self.view {
            Some(view) => view.clone(),
            None => super::configuration_views::baseline(&self.state)?,
        };
        if view.runtime_generation() != self.state.plugins.runtime_generation() {
            return Err(ManagedHarnessError::StaleOwner);
        }
        Ok(view)
    }

    /// Discover native installation candidates with manifest ownership and no credential bridge.
    pub(super) async fn discover_candidates(
        &self,
        plugin_id: &str,
        request: &bcode_harness_models::HarnessCandidatesRequest,
    ) -> Result<bcode_harness_models::HarnessCandidatesResponse, ManagedHarnessError> {
        use bcode_harness_models::{
            HARNESS_CANDIDATES_INTERFACE_ID, HARNESS_DISCOVER_CANDIDATES_OPERATION,
        };
        request
            .validate()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        let manifest = self
            .state
            .plugins
            .registry()
            .manifests()
            .get(plugin_id)
            .ok_or(ManagedHarnessError::Unavailable)?;
        bcode_plugin::harness::harness_contribution(manifest)
            .map_err(|_| ManagedHarnessError::Unavailable)?;
        if manifest
            .services
            .iter()
            .filter(|service| service.interface_id == HARNESS_CANDIDATES_INTERFACE_ID)
            .count()
            != 1
        {
            return Err(ManagedHarnessError::Unsupported);
        }
        let bytes = serde_json::to_vec(request).map_err(|_| ManagedHarnessError::Invalid)?;
        let response = tokio::time::timeout(
            CALL_TIMEOUT,
            self.configuration_view()
                .await?
                .invoke_service_with_bridge_scoped(
                    plugin_id,
                    HARNESS_CANDIDATES_INTERFACE_ID,
                    HARNESS_DISCOVER_CANDIDATES_OPERATION,
                    bytes,
                    bcode_plugin::PluginInvocationScope::Global,
                    None,
                ),
        )
        .await
        .map_err(|_| ManagedHarnessError::Deadline)?
        .map_err(|_| ManagedHarnessError::Unavailable)?;
        if response.error.is_some()
            || response.payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES
        {
            return Err(ManagedHarnessError::Invalid);
        }
        let response: bcode_harness_models::HarnessCandidatesResponse =
            serde_json::from_slice(&response.payload).map_err(|_| ManagedHarnessError::Invalid)?;
        response
            .validate_for(request, plugin_id)
            .map_err(|_| ManagedHarnessError::Invalid)?;
        Ok(response)
    }

    pub(crate) async fn prepare_candidate(
        &self,
        request: &bcode_harness_models::HarnessPrepareCandidateRequest,
    ) -> Result<bcode_harness_models::HarnessPrepareCandidateResponse, ManagedHarnessError> {
        use bcode_harness_models::{
            HARNESS_CANDIDATES_INTERFACE_ID, HARNESS_PREPARE_CANDIDATE_OPERATION,
        };
        request
            .validate()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        let plugin_id = &request.candidate.plugin_id;
        let manifest = self
            .state
            .plugins
            .registry()
            .manifests()
            .get(plugin_id)
            .ok_or(ManagedHarnessError::Unavailable)?;
        bcode_plugin::harness::harness_contribution(manifest)
            .map_err(|_| ManagedHarnessError::Unavailable)?;
        if manifest
            .services
            .iter()
            .filter(|service| service.interface_id == HARNESS_CANDIDATES_INTERFACE_ID)
            .count()
            != 1
        {
            return Err(ManagedHarnessError::Unsupported);
        }
        let bytes = serde_json::to_vec(request).map_err(|_| ManagedHarnessError::Invalid)?;
        let response = tokio::time::timeout(
            CALL_TIMEOUT,
            self.configuration_view()
                .await?
                .invoke_service_with_bridge_scoped(
                    plugin_id,
                    HARNESS_CANDIDATES_INTERFACE_ID,
                    HARNESS_PREPARE_CANDIDATE_OPERATION,
                    bytes,
                    bcode_plugin::PluginInvocationScope::Global,
                    None,
                ),
        )
        .await
        .map_err(|_| ManagedHarnessError::Deadline)?
        .map_err(|_| ManagedHarnessError::Unavailable)?;
        if response.error.is_some()
            || response.payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES
        {
            return Err(ManagedHarnessError::Invalid);
        }
        let response: bcode_harness_models::HarnessPrepareCandidateResponse =
            serde_json::from_slice(&response.payload).map_err(|_| ManagedHarnessError::Invalid)?;
        response
            .validate_for(request, plugin_id)
            .map_err(|_| ManagedHarnessError::Conflict)?;
        Ok(response)
    }

    /// Discover models without opening a conversation or authorizing a selected mutation.
    /// # Errors
    /// Rejects selected opens, disabled plugins and uncorrelated or excessive responses.
    pub(super) async fn discover_models(
        &self,
        request: &bcode_harness_models::HarnessModelsRequest,
    ) -> Result<bcode_harness_models::HarnessModelsResponse, ManagedHarnessError> {
        if !matches!(
            &request.kind,
            bcode_harness_models::HarnessModelsRequestKind::DiscoverModels
        ) {
            return Err(ManagedHarnessError::Unsupported);
        }
        self.call_models(request).await
    }

    async fn call_models(
        &self,
        request: &bcode_harness_models::HarnessModelsRequest,
    ) -> Result<bcode_harness_models::HarnessModelsResponse, ManagedHarnessError> {
        use bcode_harness_models::HARNESS_MODELS_INTERFACE_ID;
        request
            .validate()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        let plugin_id = &request.instance.plugin_id;
        let manifest = self
            .state
            .plugins
            .registry()
            .manifests()
            .get(plugin_id)
            .ok_or(ManagedHarnessError::Unavailable)?;
        bcode_plugin::harness::harness_contribution(manifest)
            .map_err(|_| ManagedHarnessError::Unavailable)?;
        let bytes = serde_json::to_vec(request).map_err(|_| ManagedHarnessError::Invalid)?;
        let inspect = HarnessRequest {
            schema_version: request.schema_version,
            request_id: request.request_id.clone(),
            instance: request.instance.clone(),
            kind: bcode_harness_models::HarnessRequestKind::Inspect,
        };
        // No canonical credential admission is minted by discovery. Native subscription
        // custody stays native; API-key resolution without an admitted owner fails closed.
        let response = tokio::time::timeout(
            CALL_TIMEOUT,
            self.configuration_view()
                .await?
                .invoke_service_with_bridge_scoped(
                    plugin_id,
                    HARNESS_MODELS_INTERFACE_ID,
                    request.kind.operation(),
                    bytes,
                    bcode_plugin::PluginInvocationScope::Global,
                    self.credential_bridge(&inspect),
                ),
        )
        .await
        .map_err(|_| ManagedHarnessError::Deadline)?
        .map_err(|_| ManagedHarnessError::Unavailable)?;
        if response.payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES
            || response.error.is_some()
        {
            return Err(ManagedHarnessError::Invalid);
        }
        let response: bcode_harness_models::HarnessModelsResponse =
            serde_json::from_slice(&response.payload).map_err(|_| ManagedHarnessError::Invalid)?;
        response
            .validate_for(request)
            .map_err(|_| ManagedHarnessError::Invalid)?;
        Ok(response)
    }

    pub(super) async fn call(
        &self,
        plugin_id: &str,
        operation: &str,
        request: &HarnessRequest,
    ) -> Result<HarnessServiceResponse, ManagedHarnessError> {
        request
            .validate()
            .map_err(|_| ManagedHarnessError::Invalid)?;
        if request.kind.operation().is_sensitive()
            || matches!(
                request.kind,
                bcode_harness_models::HarnessRequestKind::AuthStatus { .. }
            )
        {
            // Authentication has its own ephemeral, trace-exempt custody boundary.
            return Err(ManagedHarnessError::Unsupported);
        }
        if plugin_id != request.instance.plugin_id || operation != request.kind.operation().as_str()
        {
            return Err(ManagedHarnessError::Invalid);
        }
        let manifest = self
            .state
            .plugins
            .registry()
            .manifests()
            .get(plugin_id)
            .ok_or(ManagedHarnessError::Unavailable)?;
        bcode_plugin::harness::harness_contribution(manifest)
            .map_err(|_| ManagedHarnessError::Unavailable)?;
        if plugin_id.is_empty() || plugin_id.len() > 512 {
            return Err(ManagedHarnessError::Invalid);
        }
        let bytes = serde_json::to_vec(request).map_err(|_| ManagedHarnessError::Invalid)?;
        if bytes.len() > MAX_CALL_BYTES {
            return Err(ManagedHarnessError::Invalid);
        }
        // Error payloads may contain native diagnostics or secret-bearing protocol material.
        // Never forward them to public errors, logs or canonical events.
        let bridge = Some(self.credential_bridge(request));
        let response = tokio::time::timeout(
            CALL_TIMEOUT,
            self.configuration_view()
                .await?
                .invoke_service_with_bridge_scoped(
                    plugin_id,
                    bcode_harness_models::HARNESS_INTERFACE_ID,
                    operation,
                    bytes,
                    bcode_plugin::PluginInvocationScope::Global,
                    bridge,
                ),
        )
        .await
        .map_err(|_| ManagedHarnessError::Deadline)?
        .map_err(|_| ManagedHarnessError::Unavailable)?;
        if response.payload.len() > MAX_CALL_BYTES || response.error.is_some() {
            return Err(ManagedHarnessError::Invalid);
        }
        serde_json::from_slice(&response.payload).map_err(|_| ManagedHarnessError::Invalid)
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use bcode_harness_models::{HarnessAuthBinding, HarnessInstance};
    use bcode_provider_auth_models::managed::{AuthCustodyMode, AuthNamespace};

    fn admission() -> crate::harness_dispatch::ManagedAuthAdmission {
        let instance = HarnessInstance {
            instance_id: "selected".into(),
            plugin_id: "native".into(),
            executable: "/native".into(),
            config_namespace: "/account-a".into(),
            auth: HarnessAuthBinding {
                provider_id: "native-auth".into(),
                profile: "account-a".into(),
                credential_namespace: "/account-a".into(),
                custody: AuthCustodyMode::NativeManaged,
            },
        };
        crate::harness_dispatch::ManagedAuthAdmission {
            scope: crate::harness_auth::VerifiedHarnessAuthScope {
                waiter: bcode_provider_auth::managed::AuthWaiter {
                    session_id: bcode_session_models::SessionId::new().to_string(),
                    generation: 1,
                    ownership_fence: "not-a-real-lease".into(),
                },
                namespace: AuthNamespace {
                    plugin_id: instance.plugin_id.clone(),
                    credential_namespace: instance.auth.credential_namespace.clone(),
                },
                custody: instance.auth.custody,
                method: String::new(),
            },
            instance,
            ownership_hold: Box::new(()),
            fresh_begin_authorized: false,
        }
    }

    #[test]
    fn status_keeps_selected_namespace_and_never_requests_remote_verification() {
        let admission = admission();
        let input = SensitiveHarnessAuthRequest::status(&admission);
        assert!(matches!(
            input.request.kind,
            bcode_harness_models::HarnessRequestKind::AuthStatus { verify: false }
        ));
        assert_eq!(validate_auth_selection(&admission, &input.request), Ok(()));
        assert!(!admission.fresh_begin_authorized);
    }

    #[test]
    fn auth_result_validation_preserves_correlation_and_namespace_fencing() {
        use bcode_harness_models::{
            HARNESS_SCHEMA_VERSION, HarnessOperation, HarnessServiceResponseKind,
        };
        use bcode_provider_auth_models::managed::{AuthReadiness, ManagedAuthStatus};
        let admission = admission();
        let request = SensitiveHarnessAuthRequest::status(&admission).request;
        let mut response = HarnessServiceResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            operation: HarnessOperation::AuthStatus,
            kind: HarnessServiceResponseKind::AuthStatus(ManagedAuthStatus {
                namespace: admission.scope.namespace.clone(),
                custody: admission.scope.custody,
                readiness: AuthReadiness::LoginRequired,
            }),
        };
        let payload = serde_json::to_vec(&response).unwrap();
        let (_, evidence) = validate_auth_result(&admission, &request, &payload).unwrap();
        assert!(evidence.is_none());
        let readonly = decode_readonly_auth(&request, &payload).unwrap();
        assert_eq!(readonly.status.namespace, admission.scope.namespace);
        assert!(readonly.admission.is_none());
        let mut with_evidence: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        with_evidence["kind"]["response"]["admission"] = serde_json::json!({
            "schema_version": 1, "instance": request.instance,
            "method_id": "native-login", "native_login": "inactive"
        });
        let readonly =
            decode_readonly_auth(&request, &serde_json::to_vec(&with_evidence).unwrap()).unwrap();
        assert!(readonly.admission.is_none());
        assert_eq!(readonly.status.namespace, admission.scope.namespace);
        response.request_id = "different-request".into();
        assert!(matches!(
            validate_auth_result(
                &admission,
                &request,
                &serde_json::to_vec(&response).unwrap()
            ),
            Err(ManagedHarnessError::Invalid)
        ));
        response.request_id.clone_from(&request.request_id);
        let HarnessServiceResponseKind::AuthStatus(status) = &mut response.kind else {
            panic!("fixture")
        };
        status.namespace.credential_namespace = "/different-account".into();
        assert!(
            validate_auth_result(
                &admission,
                &request,
                &serde_json::to_vec(&response).unwrap()
            )
            .is_err()
        );
        // The shared response contract rejects WrongOwner before the transport-local
        // namespace check; this remains a fail-closed invalid response, not readable status.
        assert!(matches!(
            decode_readonly_auth(&request, &serde_json::to_vec(&response).unwrap()),
            Err(ManagedHarnessError::Invalid)
        ));
    }

    #[test]
    fn changed_native_config_or_credential_namespace_is_not_the_same_admission() {
        let admission = admission();
        let mut input = SensitiveHarnessAuthRequest::status(&admission);
        input.request.instance.config_namespace = "/account-b".into();
        assert_eq!(
            validate_auth_selection(&admission, &input.request),
            Err(ManagedHarnessError::StaleOwner)
        );
        let mut input = SensitiveHarnessAuthRequest::status(&admission);
        input.request.instance.auth.credential_namespace = "/account-b".into();
        assert_eq!(
            validate_auth_selection(&admission, &input.request),
            Err(ManagedHarnessError::StaleOwner)
        );
    }
}
