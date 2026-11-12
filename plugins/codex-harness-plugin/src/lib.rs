#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Codex native app-server backend. This plugin never acts as a model provider,
//! dispatches observed native tools, or stores a recovery journal.
pub mod auth;
mod candidates;
pub mod config;
mod display;
mod execution;
mod models;
mod recovery;
mod retention;
pub mod session;
mod terminal_auth;
pub mod transport;

use auth::CodexAuth;
use bcode_harness_models::{
    HARNESS_INTERFACE_ID, HARNESS_SCHEMA_VERSION, HarnessAuthStep, HarnessCapabilities,
    HarnessCapability, HarnessConfigureRequest, HarnessControlRequest, HarnessFailure,
    HarnessInspection, HarnessInstance, HarnessObservationPage, HarnessObserveRequest,
    HarnessOpenRequest, HarnessReconciliation, HarnessRequest, HarnessRequestKind,
    HarnessServiceResponse, HarnessServiceResponseKind, HarnessSessionBinding,
    HarnessSessionTarget, HarnessSupportLevel, MAX_HARNESS_WIRE_BYTES,
};
use bcode_plugin_sdk::prelude::*;
use bcode_provider_auth_models::managed::AuthCustodyMode;
use session::CodexSession;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use transport::{CodexError, Launch, Readiness};

pub(crate) const PLUGIN_ID: &str = "bcode.codex-harness";
/// Auth provider registered by this plugin and required on every instance binding.
pub(crate) const AUTH_PROVIDER_ID: &str = "bcode.codex";
/// Upper bound on one blocking observe wait slice, so cancellation is observed promptly.
const OBSERVE_WAIT_SLICE: Duration = Duration::from_millis(100);
/// When only streaming deltas are queued, wait this long so nearby deltas share a page.
const LIVE_COALESCE: Duration = Duration::from_millis(40);
/// Retained configure replies per owner (identical retries return the recorded outcome).
const MAX_RETAINED_CONFIGURES: usize = 64;

struct Owner {
    instance: HarnessInstance,
    open: HarnessOpenRequest,
    session: Option<CodexSession>,
    page_number: u64,
    next: Option<String>,
    previous: Option<(Option<String>, HarnessObservationPage)>,
    interrupt: Option<(String, Result<(), CodexError>)>,
    configures: BTreeMap<String, (HarnessConfigureRequest, Result<(), CodexError>)>,
    /// Exact Close that dropped the native transport, and the supervisor's reap flag.
    released: Option<(HarnessControlRequest, Arc<AtomicBool>)>,
    /// Model selection evidence when opened through the model-selection extension.
    selection: Option<bcode_harness_models::HarnessModelSelection>,
    effective_model: Option<String>,
}
impl Owner {
    fn new(instance: &HarnessInstance, open: &HarnessOpenRequest) -> Self {
        Self {
            instance: instance.clone(),
            open: open.clone(),
            session: None,
            page_number: 0,
            next: None,
            previous: None,
            interrupt: None,
            configures: BTreeMap::new(),
            released: None,
            selection: None,
            effective_model: None,
        }
    }
}
struct AuthOwner {
    instance: HarnessInstance,
    auth: CodexAuth,
}

/// Retained plugin state. Operations serialize on it; only a waiting `observe` releases the
/// lock while it waits for native output.
#[derive(Default)]
pub(crate) struct PluginState {
    owners: BTreeMap<String, Owner>,
    auth: BTreeMap<String, AuthOwner>,
    /// Client-terminal `codex login` flows prepared through terminal-auth; never replayed.
    terminal_flows: BTreeMap<String, HarnessInstance>,
}

/// Native plugin service owner. Host must admit/retain mutations and suppress sensitive traces
/// before invoking this boundary. Serialization is not a grant of execution authority.
#[derive(Default)]
pub struct CodexHarnessPlugin {
    state: Mutex<PluginState>,
}

impl RustPlugin for CodexHarnessPlugin {
    fn register_auth_providers(&mut self, registrar: AuthRegistrar) -> Result<(), PluginError> {
        register_auth_providers(&registrar)
    }

    fn deactivate(&mut self) -> Result<(), PluginError> {
        self.deactivate_concurrent()
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        self.invoke_service_concurrent(context)
    }
}

impl ConcurrentRustPlugin for CodexHarnessPlugin {
    fn register_auth_providers_concurrent(
        &self,
        registrar: AuthRegistrar,
    ) -> Result<(), PluginError> {
        register_auth_providers(&registrar)
    }

    fn deactivate_concurrent(&self) -> Result<(), PluginError> {
        if let Ok(mut state) = self.state.lock() {
            state.owners.clear();
            state.auth.clear();
            state.terminal_flows.clear();
        }
        Ok(())
    }

    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
        if let Some((request, wait)) = observe_wait(&context) {
            self.wait_for_observations(&request, wait, &context.cancellation);
        }
        self.state.lock().map_or_else(
            |_| ServiceResponse::error("unavailable", "Codex harness state unavailable"),
            |mut state| state.invoke_service(&context),
        )
    }
}

fn register_auth_providers(registrar: &AuthRegistrar) -> Result<(), PluginError> {
    use bcode_provider_auth_models::{
        AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION, AuthMethodContribution,
        AuthProviderContribution, AuthSecretField, AuthSecretValidation,
    };
    // Native custody is handled by managed auth, not an empty vault enrollment.
    registrar
        .register(&AuthProviderContribution {
            external_sources: Vec::new(),
            schema_version: AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
            provider_id: AUTH_PROVIDER_ID.to_owned(),
            display_name: "Codex".to_owned(),
            methods: vec![AuthMethodContribution::SecretFields {
                method_id: "api_key".to_owned(),
                display_name: "API key (ephemeral native delivery)".to_owned(),
                fields: vec![AuthSecretField {
                    credential_id: "api_key".to_owned(),
                    storage_key: "CODEX_API_KEY".to_owned(),
                    prompt: "Codex API key".to_owned(),
                    optional: false,
                    validation: AuthSecretValidation {
                        min_bytes: Some(1),
                        max_bytes: Some(16 * 1024),
                        required_prefix: None,
                    },
                    discovery_sources: Vec::new(),
                    invocation_env: Vec::new(),
                }],
                supports_verification: false,
                supports_revocation: false,
            }],
        })
        .map_err(|_| PluginError::failed("Codex auth registration failed"))
}

/// Bounded wait requested by a valid harness `observe`; other requests never wait.
fn observe_wait(context: &NativeServiceContext) -> Option<(HarnessRequest, Duration)> {
    if context.plugin_id != PLUGIN_ID
        || context.request.interface_id != HARNESS_INTERFACE_ID
        || context.request.operation != "observe"
        || context.request.payload.len() > MAX_HARNESS_WIRE_BYTES
    {
        return None;
    }
    let request: HarnessRequest = serde_json::from_slice(&context.request.payload).ok()?;
    request.validate().ok()?;
    let HarnessRequestKind::Observe(observe) = &request.kind else {
        return None;
    };
    let wait = Duration::from_millis(u64::from(observe.wait_millis));
    (!wait.is_zero()).then_some((request, wait))
}

/// What a waiting `observe` should do next.
enum ObservationReadiness {
    /// Run the operation now.
    Run,
    /// Only transient live text is ready; a short coalescing wait is allowed.
    LiveOnly(transport::Wake, u64),
    /// Nothing is ready; wait for the native reader.
    Waiting(transport::Wake, u64),
}

impl CodexHarnessPlugin {
    /// Wait outside the state lock until the observed session has something to offer, the
    /// bounded wait elapses, or the invocation is cancelled.
    fn wait_for_observations(
        &self,
        request: &HarnessRequest,
        wait: Duration,
        cancellation: &bcode_plugin_sdk::ServiceCancellation,
    ) {
        let HarnessRequestKind::Observe(observe) = &request.kind else {
            return;
        };
        let deadline = Instant::now() + wait;
        let mut coalesce_until: Option<Instant> = None;
        loop {
            let readiness = self
                .state
                .lock()
                .ok()
                .map(|state| state.observation_readiness(&request.instance, observe));
            let (wake, seen) = match readiness {
                Some(ObservationReadiness::Waiting(wake, seen)) => (wake, seen),
                Some(ObservationReadiness::LiveOnly(wake, seen)) => {
                    let now = Instant::now();
                    let until =
                        *coalesce_until.get_or_insert_with(|| (now + LIVE_COALESCE).min(deadline));
                    if now >= until || cancellation.is_cancelled() {
                        return;
                    }
                    wake.wait_changed(seen, until - now);
                    continue;
                }
                _ => return,
            };
            loop {
                let now = Instant::now();
                if now >= deadline || cancellation.is_cancelled() {
                    return;
                }
                if wake.wait_changed(seen, (deadline - now).min(OBSERVE_WAIT_SLICE)) {
                    break;
                }
            }
        }
    }
}

impl PluginState {
    fn observation_readiness(
        &self,
        instance: &HarnessInstance,
        observe: &HarnessObserveRequest,
    ) -> ObservationReadiness {
        let HarnessSessionTarget::Confirmed(binding) = &observe.target else {
            return ObservationReadiness::Run;
        };
        let Some(owner) = self.owners.get(&binding.session_id) else {
            return ObservationReadiness::Run;
        };
        let Some(session) = owner.session.as_ref() else {
            return ObservationReadiness::Run;
        };
        if owner.instance != *instance
            || session.binding() != binding
            || observe.after != owner.next
            || observe.retention.as_ref().is_some_and(|retention| {
                retention.acknowledgment.is_some() || !retention.hydration.is_empty()
            })
            || session.locally_ready()
        {
            // Acknowledgements, hydration replies, retries and staged evidence run now.
            return ObservationReadiness::Run;
        }
        let wake = session.transport_wake();
        let seen = wake.seen();
        match session.transport_readiness() {
            Readiness::Ready => ObservationReadiness::Run,
            Readiness::DeltasOnly => ObservationReadiness::LiveOnly(wake, seen),
            Readiness::Idle if session.live_pending() => ObservationReadiness::LiveOnly(wake, seen),
            Readiness::Idle => ObservationReadiness::Waiting(wake, seen),
        }
    }

    fn invoke_service(&mut self, context: &NativeServiceContext) -> ServiceResponse {
        if context.plugin_id == PLUGIN_ID {
            match context.request.interface_id.as_str() {
                bcode_harness_models::HARNESS_CANDIDATES_INTERFACE_ID => {
                    return candidates::candidates_service(context);
                }
                bcode_harness_models::HARNESS_MODELS_INTERFACE_ID => {
                    return self.models_service(context);
                }
                bcode_harness_models::HARNESS_RECOVERY_INTERFACE_ID => {
                    return self.recovery_service(context);
                }
                bcode_harness_models::TERMINAL_AUTH_INTERFACE_ID => {
                    return terminal_auth::prepare_login_service(context);
                }
                _ => {}
            }
        }
        if context.plugin_id != PLUGIN_ID || context.request.interface_id != HARNESS_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "Codex harness interface mismatch",
            );
        }
        if context.request.payload.len() > MAX_HARNESS_WIRE_BYTES {
            return ServiceResponse::error(
                "invalid_request",
                "Codex harness request exceeds bounds",
            );
        }
        let Ok(request) = context.request.payload_json::<HarnessRequest>() else {
            return ServiceResponse::error(
                "invalid_request",
                "Invalid typed Codex harness request",
            );
        };
        if request.validate().is_err()
            || request.instance.plugin_id != PLUGIN_ID
            || context.request.operation != request.kind.operation().as_str()
            || context.cancellation.is_cancelled()
        {
            return ServiceResponse::error(
                "invalid_request",
                "Codex harness admission mismatch or cancelled invocation",
            );
        }
        let kind = self
            .handle(&request, context)
            .unwrap_or_else(|error| HarnessServiceResponseKind::Failure(failure(error)));
        let response = HarnessServiceResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            operation: request.kind.operation(),
            kind,
        };
        if response.validate_for(&request).is_err() {
            return ServiceResponse::error(
                "invalid_response",
                "Codex harness response failed contract validation",
            );
        }
        ServiceResponse::json(&response).unwrap_or_else(|_| {
            ServiceResponse::error("encode_failed", "Codex harness response encoding failed")
        })
    }

    fn handle(
        &mut self,
        request: &HarnessRequest,
        context: &NativeServiceContext,
    ) -> Result<HarnessServiceResponseKind, CodexError> {
        let instance = &request.instance;
        let configuration = config::Config::from_context(context)?;
        configuration.validate_instance(instance)?;
        match &request.kind {
            HarnessRequestKind::Inspect => {
                let launch = launch(instance, configuration.auth_working_directory(instance))?;
                let mut server = transport::AppServer::spawn(&launch)?;
                let native_version = server.initialize(&launch)?;
                Ok(HarnessServiceResponseKind::Inspection(HarnessInspection {
                    native_version,
                    executable_identity: instance.executable.clone(),
                    config_namespace: instance.config_namespace.clone(),
                    capabilities: capabilities(),
                }))
            }
            HarnessRequestKind::Open(open) => self.handle_open(instance, open, context, None),
            HarnessRequestKind::Submit { target, submission } => {
                Ok(HarnessServiceResponseKind::Receipt(
                    self.session(instance, confirmed(target)?)?
                        .submit(submission)?,
                ))
            }
            HarnessRequestKind::Respond { target, response } => {
                self.session(instance, confirmed(target)?)?
                    .respond(response)?;
                Ok(HarnessServiceResponseKind::Acknowledged {
                    mutation_id: response.response_id.clone(),
                })
            }
            HarnessRequestKind::Interrupt(control) => self.handle_interrupt(instance, control),
            HarnessRequestKind::Close(control) => self.handle_close(instance, control),
            HarnessRequestKind::Configure(configure) => self.handle_configure(instance, configure),
            HarnessRequestKind::Observe(observe) => self.handle_observe(instance, observe),
            HarnessRequestKind::Reconcile(reconcile) => {
                let Some(binding) = &reconcile.binding else {
                    return Ok(HarnessServiceResponseKind::Reconciled(
                        HarnessReconciliation::Uncertain,
                    ));
                };
                // Metadata-only read cannot establish whether an unacknowledged mutation was applied.
                if reconcile.submission_id.is_some() || reconcile.open_id.is_some() {
                    return Ok(HarnessServiceResponseKind::Reconciled(
                        HarnessReconciliation::Uncertain,
                    ));
                }
                let status = self.session(instance, binding)?.reconcile()?;
                Ok(HarnessServiceResponseKind::Reconciled(
                    HarnessReconciliation::Known {
                        binding: binding.clone(),
                        receipt: None,
                        status,
                    },
                ))
            }
            kind => self.handle_auth(instance, kind, context, &configuration),
        }
    }

    fn handle_auth(
        &mut self,
        instance: &HarnessInstance,
        kind: &HarnessRequestKind,
        context: &NativeServiceContext,
        configuration: &config::Config,
    ) -> Result<HarnessServiceResponseKind, CodexError> {
        match kind {
            HarnessRequestKind::AuthStatus { verify } => {
                if *verify {
                    return Err(CodexError::Unsupported);
                }
                Ok(HarnessServiceResponseKind::AuthStatus(
                    self.auth_owner(instance, configuration)?.status()?,
                ))
            }
            HarnessRequestKind::AuthBegin(auth)
            | HarnessRequestKind::AuthContinue(auth)
            | HarnessRequestKind::AuthCancel(auth)
                if auth.method_id == terminal_auth::NATIVE_LOGIN_METHOD =>
            {
                self.terminal_login_step(instance, kind, auth, configuration)
            }
            HarnessRequestKind::AuthBegin(auth) if auth.method_id == "api_key" => {
                let key = context
                    .credentials()
                    .get(&instance.auth.provider_id, "api_key")
                    .ok_or(CodexError::Configuration)?;
                let status = self.auth_owner(instance, configuration)?.vault_key(key)?;
                Ok(HarnessServiceResponseKind::AuthStep(HarnessAuthStep {
                    flow_id: auth.flow_id.clone(),
                    status,
                    interaction: None,
                    complete: true,
                }))
            }
            HarnessRequestKind::AuthBegin(auth) => Ok(HarnessServiceResponseKind::AuthStep(
                self.auth_owner(instance, configuration)?
                    .begin(&auth.flow_id, &auth.method_id)?,
            )),
            HarnessRequestKind::AuthContinue(auth) => {
                let owner = self
                    .auth
                    .get_mut(&instance.instance_id)
                    .ok_or(CodexError::Stale)?;
                if owner.instance != *instance || auth.input.is_some() {
                    return Err(CodexError::Unsupported);
                }
                Ok(HarnessServiceResponseKind::AuthStep(
                    owner.auth.poll(&auth.flow_id)?,
                ))
            }
            HarnessRequestKind::AuthCancel(auth) => {
                let owner = self
                    .auth
                    .get_mut(&instance.instance_id)
                    .ok_or(CodexError::Stale)?;
                if owner.instance != *instance {
                    return Err(CodexError::Stale);
                }
                Ok(HarnessServiceResponseKind::AuthStep(
                    owner.auth.cancel(&auth.flow_id)?,
                ))
            }
            _ => Err(CodexError::Unsupported),
        }
    }

    /// Client-terminal login flow: the client runs the prepared `codex login` plan and the
    /// plugin only reinspects native readiness. Exit of the native process is not readiness.
    fn terminal_login_step(
        &mut self,
        instance: &HarnessInstance,
        kind: &HarnessRequestKind,
        auth: &bcode_harness_models::HarnessAuthRequest,
        configuration: &config::Config,
    ) -> Result<HarnessServiceResponseKind, CodexError> {
        use bcode_provider_auth_models::managed::{
            AuthNamespace, AuthReadiness, ManagedAuthStatus, SensitiveAuthInteraction,
        };
        if auth.input.is_some() || instance.auth.custody != AuthCustodyMode::NativeManaged {
            return Err(CodexError::Unsupported);
        }
        match kind {
            HarnessRequestKind::AuthBegin(_) => {
                if let Some(retained) = self.terminal_flows.get(&auth.flow_id) {
                    if retained != instance {
                        return Err(CodexError::Stale);
                    }
                } else {
                    if self.terminal_flows.len() >= 32 {
                        return Err(CodexError::Bounds);
                    }
                    self.terminal_flows
                        .insert(auth.flow_id.clone(), instance.clone());
                }
            }
            HarnessRequestKind::AuthCancel(_) => {
                if self.terminal_flows.get(&auth.flow_id) != Some(instance) {
                    return Err(CodexError::Stale);
                }
                self.terminal_flows.remove(&auth.flow_id);
                // Cancels this local flow only, never shared native credentials.
                return Ok(HarnessServiceResponseKind::AuthStep(HarnessAuthStep {
                    flow_id: auth.flow_id.clone(),
                    complete: true,
                    interaction: None,
                    status: ManagedAuthStatus {
                        namespace: AuthNamespace {
                            plugin_id: PLUGIN_ID.to_owned(),
                            credential_namespace: instance.auth.credential_namespace.clone(),
                        },
                        custody: AuthCustodyMode::NativeManaged,
                        readiness: AuthReadiness::Unknown,
                    },
                }));
            }
            _ => {
                if self.terminal_flows.get(&auth.flow_id) != Some(instance) {
                    return Err(CodexError::Stale);
                }
            }
        }
        // A fresh inspection process: the native login wrote credentials outside this one.
        self.auth.remove(&instance.instance_id);
        let status = self.auth_owner(instance, configuration)?.status()?;
        let complete = status.readiness == AuthReadiness::Ready;
        if complete {
            self.terminal_flows.remove(&auth.flow_id);
        }
        Ok(HarnessServiceResponseKind::AuthStep(HarnessAuthStep {
            flow_id: auth.flow_id.clone(),
            status,
            complete,
            interaction: (!complete).then(|| SensitiveAuthInteraction::NativeHandoff {
                request_id: auth.flow_id.clone(),
            }),
        }))
    }

    pub(crate) fn handle_open(
        &mut self,
        instance: &HarnessInstance,
        open: &HarnessOpenRequest,
        context: &NativeServiceContext,
        selection: Option<&bcode_harness_models::HarnessModelSelection>,
    ) -> Result<HarnessServiceResponseKind, CodexError> {
        if let Some(owner) = self.owners.get(&open.session_id) {
            if owner.instance != *instance
                || owner.open != *open
                || owner.selection.as_ref() != selection
            {
                return Err(CodexError::Stale);
            }
            return owner
                .session
                .as_ref()
                .map(|session| HarnessServiceResponseKind::Opened(session.binding().clone()))
                .ok_or(CodexError::Uncertain);
        }
        if self.owners.len() >= 32 {
            // Fully released owners are physical history only; evict them before refusing.
            self.owners
                .retain(|_, owner| owner.session.is_some() || owner.released.is_none());
            if self.owners.len() >= 32 {
                return Err(CodexError::Bounds);
            }
        }
        let launch = launch(instance, &open.working_directory)?;
        let binding = open
            .resume
            .clone()
            .unwrap_or_else(|| HarnessSessionBinding {
                schema_version: HARNESS_SCHEMA_VERSION,
                session_id: open.session_id.clone(),
                instance_id: instance.instance_id.clone(),
                native_session_id: open.open_id.clone(),
                native_version: "unverified-app-server-v2".to_owned(),
                generation: open.generation,
            });
        let key = if instance.auth.custody == AuthCustodyMode::VaultManaged {
            Some(
                context
                    .credentials()
                    .get(&instance.auth.provider_id, "api_key")
                    .ok_or(CodexError::Configuration)?,
            )
        } else {
            None
        };
        // Retain an uncertain open before any native mutation; identical retry never spawns again.
        let mut owner = Owner::new(instance, open);
        owner.selection = selection.cloned();
        self.owners.insert(open.session_id.clone(), owner);
        let session = CodexSession::open_with_model(
            &launch,
            binding,
            open.resume.is_some(),
            key,
            open.parent_assistance.as_ref(),
            selection.map(|selection| selection.model_id.as_str()),
        )?;
        let binding = session.binding().clone();
        let owner = self
            .owners
            .get_mut(&open.session_id)
            .ok_or(CodexError::Stale)?;
        owner.effective_model = session.model().map(str::to_owned);
        owner.session = Some(session);
        Ok(HarnessServiceResponseKind::Opened(binding))
    }

    fn handle_close(
        &mut self,
        instance: &HarnessInstance,
        control: &HarnessControlRequest,
    ) -> Result<HarnessServiceResponseKind, CodexError> {
        let binding = confirmed(&control.target)?;
        let owner = self
            .owners
            .get_mut(&binding.session_id)
            .ok_or(CodexError::Uncertain)?;
        if owner.instance != *instance {
            return Err(CodexError::Stale);
        }
        if let Some((released, _)) = &owner.released {
            // An identical retry returns the recorded acknowledgement; never close twice.
            return if released == control {
                Ok(HarnessServiceResponseKind::Acknowledged {
                    mutation_id: control.mutation_id.clone(),
                })
            } else {
                Err(CodexError::Stale)
            };
        }
        let session = owner.session.as_ref().ok_or(CodexError::Uncertain)?;
        if session.binding() != binding {
            return Err(CodexError::Stale);
        }
        // Closing transport is not cancellation, settlement, or permission to repeat mutations.
        // Dropping the session fences the transport; its supervisor kills and reaps the child.
        let reaped = session.reap_flag();
        owner.session.take();
        owner.released = Some((control.clone(), reaped));
        Ok(HarnessServiceResponseKind::Acknowledged {
            mutation_id: control.mutation_id.clone(),
        })
    }

    fn handle_configure(
        &mut self,
        instance: &HarnessInstance,
        configure: &HarnessConfigureRequest,
    ) -> Result<HarnessServiceResponseKind, CodexError> {
        let binding = confirmed(&configure.target)?;
        self.session(instance, binding)?;
        let owner = self
            .owners
            .get_mut(&binding.session_id)
            .ok_or(CodexError::Stale)?;
        if let Some((retained, result)) = owner.configures.get(&configure.mutation_id) {
            if retained != configure {
                return Err(CodexError::Stale);
            }
            (*result)?;
            return Ok(HarnessServiceResponseKind::Acknowledged {
                mutation_id: configure.mutation_id.clone(),
            });
        }
        if owner.configures.len() >= MAX_RETAINED_CONFIGURES {
            owner.configures.pop_first();
        }
        // Retain uncertainty before delivery; an identical retry never re-sends.
        owner.configures.insert(
            configure.mutation_id.clone(),
            (configure.clone(), Err(CodexError::Uncertain)),
        );
        let result = owner
            .session
            .as_mut()
            .ok_or(CodexError::Uncertain)?
            .configure(&configure.setting);
        owner
            .configures
            .insert(configure.mutation_id.clone(), (configure.clone(), result));
        result?;
        Ok(HarnessServiceResponseKind::Acknowledged {
            mutation_id: configure.mutation_id.clone(),
        })
    }

    fn handle_interrupt(
        &mut self,
        instance: &HarnessInstance,
        control: &HarnessControlRequest,
    ) -> Result<HarnessServiceResponseKind, CodexError> {
        let binding = confirmed(&control.target)?;
        self.session(instance, binding)?;
        let owner = self
            .owners
            .get_mut(&binding.session_id)
            .ok_or(CodexError::Stale)?;
        if let Some((mutation_id, result)) = &owner.interrupt {
            if *mutation_id == control.mutation_id {
                (*result)?;
                return Ok(HarnessServiceResponseKind::Acknowledged {
                    mutation_id: control.mutation_id.clone(),
                });
            }
            // A later interrupt identity targets whatever turn is active now; a still-running
            // previous attempt with an uncertain outcome keeps fencing.
            if result.is_err_and(|error| error == CodexError::Uncertain) {
                return Err(CodexError::Uncertain);
            }
        }
        owner.interrupt = Some((control.mutation_id.clone(), Err(CodexError::Uncertain)));
        let result = owner
            .session
            .as_mut()
            .ok_or(CodexError::Uncertain)?
            .interrupt_active(binding.generation);
        owner.interrupt = Some((control.mutation_id.clone(), result));
        result?;
        Ok(HarnessServiceResponseKind::Acknowledged {
            mutation_id: control.mutation_id.clone(),
        })
    }

    fn handle_observe(
        &mut self,
        instance: &HarnessInstance,
        observe: &HarnessObserveRequest,
    ) -> Result<HarnessServiceResponseKind, CodexError> {
        let binding = confirmed(&observe.target)?;
        self.session(instance, binding)?;
        let owner = self
            .owners
            .get_mut(&binding.session_id)
            .ok_or(CodexError::Stale)?;
        if let Some((after, page)) = &owner.previous
            && observe.after == *after
            && observe.after != owner.next
        {
            // Exact retry of the last retained page.
            return Ok(HarnessServiceResponseKind::Observations(page.clone()));
        }
        if observe.after != owner.next {
            return Err(CodexError::Uncertain);
        }
        let session = owner.session.as_mut().ok_or(CodexError::Uncertain)?;
        if let Some(retention) = &observe.retention {
            session.apply_retention(retention)?;
        }
        let (observations, completeness) = session.observe(usize::from(observe.limit.min(8)))?;
        let status = session.status();
        let hydration_requests = session.hydration_requests();
        let inventory_page = session.take_inventory_page();
        let live = session.take_live();
        // A page with nothing to transfer (idle waits and live-only streaming pages) carries
        // no retention page and no successor cursor, so identical empty pages need no
        // canonical commit. Evidence, requests and gaps keep the exact retention protocol.
        let transfers = !observations.is_empty()
            || !hydration_requests.is_empty()
            || inventory_page.is_some()
            || completeness == bcode_harness_models::HarnessCompleteness::Gap;
        if !transfers {
            // The host's committed cursor becomes this page's (absent) successor.
            owner.next = None;
            return Ok(HarnessServiceResponseKind::Observations(
                HarnessObservationPage {
                    live,
                    retention: None,
                    hydration_requests,
                    inventory_page,
                    observations,
                    next_after: None,
                    completeness,
                    status,
                },
            ));
        }
        let retention = Some(session.retention_page(&observations)?);
        owner.page_number = owner.page_number.checked_add(1).ok_or(CodexError::Bounds)?;
        let next_after = Some(format!(
            "codex-page-{}-{}",
            binding.generation, owner.page_number
        ));
        let page = HarnessObservationPage {
            live,
            retention,
            hydration_requests,
            inventory_page,
            observations,
            next_after: next_after.clone(),
            completeness,
            status,
        };
        // Retries re-deliver durable content only; live text is offered once.
        let mut retained = page.clone();
        retained.live = Vec::new();
        owner.previous = Some((observe.after.clone(), retained));
        owner.next = next_after;
        Ok(HarnessServiceResponseKind::Observations(page))
    }

    fn session(
        &mut self,
        instance: &HarnessInstance,
        binding: &HarnessSessionBinding,
    ) -> Result<&mut CodexSession, CodexError> {
        let owner = self
            .owners
            .get_mut(&binding.session_id)
            .ok_or(CodexError::Uncertain)?;
        if owner.instance != *instance {
            return Err(CodexError::Stale);
        }
        let session = owner.session.as_mut().ok_or(CodexError::Uncertain)?;
        if session.binding() != binding {
            return Err(CodexError::Stale);
        }
        Ok(session)
    }

    fn auth_owner(
        &mut self,
        instance: &HarnessInstance,
        configuration: &config::Config,
    ) -> Result<&mut CodexAuth, CodexError> {
        if !self.auth.contains_key(&instance.instance_id) {
            if self.auth.len() >= 16 {
                return Err(CodexError::Bounds);
            }
            // Never inherit the plugin process cwd.
            let cwd = configuration.auth_working_directory(instance);
            let auth = CodexAuth::open(&launch(instance, cwd)?, instance.auth.clone())?;
            self.auth.insert(
                instance.instance_id.clone(),
                AuthOwner {
                    instance: instance.clone(),
                    auth,
                },
            );
        }
        let owner = self
            .auth
            .get_mut(&instance.instance_id)
            .ok_or(CodexError::Stale)?;
        if owner.instance != *instance {
            return Err(CodexError::Stale);
        }
        Ok(&mut owner.auth)
    }
}

const fn failure(error: CodexError) -> HarnessFailure {
    match error {
        CodexError::Stale => HarnessFailure::StaleGeneration,
        CodexError::Unsupported => HarnessFailure::Unsupported,
        CodexError::Configuration | CodexError::Bounds | CodexError::Protocol => {
            HarnessFailure::InvalidRequest
        }
        CodexError::Rejected => HarnessFailure::Policy,
        CodexError::Disconnected => HarnessFailure::Unavailable,
        CodexError::Uncertain => HarnessFailure::Uncertain,
    }
}

const fn confirmed(target: &HarnessSessionTarget) -> Result<&HarnessSessionBinding, CodexError> {
    match target {
        HarnessSessionTarget::Confirmed(binding) => Ok(binding),
        HarnessSessionTarget::Prepared(_) => Err(CodexError::Unsupported),
    }
}

pub(crate) fn launch(instance: &HarnessInstance, cwd: &str) -> Result<Launch, CodexError> {
    if instance.auth.provider_id != AUTH_PROVIDER_ID
        || instance.config_namespace != instance.auth.credential_namespace
        || instance.auth.custody == AuthCustodyMode::External
    {
        return Err(CodexError::Configuration);
    }
    Ok(Launch {
        executable: PathBuf::from(&instance.executable),
        codex_home: PathBuf::from(&instance.config_namespace),
        working_directory: PathBuf::from(cwd),
        ephemeral_credentials: instance.auth.custody == AuthCustodyMode::VaultManaged,
        timeout: Duration::from_secs(30),
    })
}

pub(crate) fn capabilities() -> HarnessCapabilities {
    let constrained = |constraint: &str| HarnessCapability {
        support: HarnessSupportLevel::Constrained,
        constraints: vec![constraint.to_owned()],
    };
    let unsupported = || HarnessCapability {
        support: HarnessSupportLevel::Unsupported,
        constraints: Vec::new(),
    };
    HarnessCapabilities {
        model_switch: constrained(
            "Native thread/settings/update model key for subsequent turns; acknowledgement is the native response, not proof a later turn used it",
        ),
        permission_mode: constrained(
            "default=untrusted approval with the opening sandbox; accept_edits=on-request approval with workspace-write sandbox; plan=native plan collaboration mode; applies to subsequent turns",
        ),
        resume: constrained(
            "Exact binding and thread identity; recorded binding version retained across native CLI upgrades; native thread/resume without history replay",
        ),
        steering: constrained(
            "Expected native turn ID required; NextTurn admitted after observed completion of the previous accepted turn with no pending callback, or on an idle resumed thread",
        ),
        approval: constrained(
            "Exact generation callback; file approval requires retained bounded native diff; root/network/additional-permission grants declined",
        ),
        questions: constrained(
            "One question per callback; multiple-question responses unsupported",
        ),
        assistance: constrained(
            "Explicit bcode_parent_assistance native dynamic tool only; host authorizes response",
        ),
        interrupt: constrained("Requires retained active turn; acknowledgment is not settlement"),
        authoritative_settlement: constrained(
            "Exact correlated successful native turn, verified multi_agent=false and no gap; never assignment acceptance",
        ),
        background_continuation: unsupported(),
        authentication: constrained(
            "Native browser/device; scoped vault key requires verified ephemeral mode; no remote verification",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::HarnessPreparedSession;
    #[test]
    fn prepared_target_is_not_promoted_to_an_invented_codex_thread() {
        let target = HarnessSessionTarget::Prepared(HarnessPreparedSession {
            open_id: "open".into(),
            session_id: "session".into(),
            instance_id: "instance".into(),
            generation: 1,
            native_version: "0.0.0".into(),
            requested_native_session_id: None,
        });
        assert_eq!(confirmed(&target), Err(CodexError::Unsupported));
    }

    #[test]
    fn configuration_capabilities_are_declared_with_constraints() {
        let declared = capabilities();
        assert_eq!(
            declared.model_switch.support,
            HarnessSupportLevel::Constrained
        );
        assert_eq!(
            declared.permission_mode.support,
            HarnessSupportLevel::Constrained
        );
        assert_eq!(
            declared.background_continuation.support,
            HarnessSupportLevel::Unsupported
        );
    }
}

/// Return this plugin's native static registration using the same manifest as dynamic loading.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_concurrent_plugin_vtable!(
        CodexHarnessPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_concurrent_plugin!(
    CodexHarnessPlugin,
    include_str!("../bcode-plugin.toml")
);
