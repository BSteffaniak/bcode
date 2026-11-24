#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Antigravity native ACP (`agy acp --stdio`) backend. This plugin never acts as a model
//! provider, dispatches observed native tools, or stores a recovery journal.
pub mod auth;
mod candidates;
pub mod config;
mod display;
mod models;
mod recovery;
mod retention;
pub mod session;
mod terminal_auth;
pub mod transport;

use bcode_harness_models::{
    HARNESS_INTERFACE_ID, HARNESS_SCHEMA_VERSION, HarnessAuthStep, HarnessCapabilities,
    HarnessCapability, HarnessConfigureRequest, HarnessControlRequest, HarnessFailure,
    HarnessInspection, HarnessInstance, HarnessObservationPage, HarnessObserveRequest,
    HarnessOpenRequest, HarnessReconciliation, HarnessRequest, HarnessRequestKind,
    HarnessServiceResponse, HarnessServiceResponseKind, HarnessSessionBinding,
    HarnessSessionTarget, HarnessSupportLevel, MAX_HARNESS_WIRE_BYTES,
};
use bcode_plugin_sdk::prelude::*;
use bcode_provider_auth_models::managed::{
    AuthCustodyMode, AuthNamespace, AuthReadiness, ManagedAuthStatus, SensitiveAuthInteraction,
};
use session::{AcpSession, OpenFailure, PendingConfigure};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use transport::{AcpError, Launch, Readiness};

pub(crate) const PLUGIN_ID: &str = "bcode.antigravity-harness";
/// Auth provider registered by this plugin and required on every instance binding.
pub(crate) const AUTH_PROVIDER_ID: &str = "bcode.antigravity";
/// Upper bound on one blocking observe wait slice, so cancellation is observed promptly.
const OBSERVE_WAIT_SLICE: Duration = Duration::from_millis(100);
/// When only streaming deltas are queued, wait this long so nearby deltas share a page.
const LIVE_COALESCE: Duration = Duration::from_millis(40);
/// Retained configure replies per owner (identical retries return the recorded outcome).
const MAX_RETAINED_CONFIGURES: usize = 64;
/// Retained owners (live, opening, or released) before released ones are evicted.
const MAX_OWNERS: usize = 32;
/// Concurrent client-terminal login flows.
const MAX_TERMINAL_FLOWS: usize = 32;

struct Owner {
    instance: HarnessInstance,
    open: HarnessOpenRequest,
    session: Option<AcpSession>,
    /// A native open for this owner is in progress outside the state lock.
    opening: bool,
    page_number: u64,
    next: Option<String>,
    previous: Option<(Option<String>, HarnessObservationPage)>,
    interrupt: Option<(String, Result<(), AcpError>)>,
    configures: BTreeMap<String, (HarnessConfigureRequest, Result<(), AcpError>)>,
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
            opening: false,
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

/// Retained plugin state. Bookkeeping serializes on it; native process launches, account
/// inspection, model discovery, configuration acknowledgements, reap waits and waiting
/// `observe` calls all run with the lock released.
#[derive(Default)]
pub(crate) struct PluginState {
    owners: BTreeMap<String, Owner>,
    /// Client-terminal `agy acp --login` flows prepared through terminal-auth; never replayed.
    terminal_flows: BTreeMap<String, HarnessInstance>,
}

/// What an open request needs after the state lock is released.
enum OpenPlan {
    /// The exact open already completed.
    Existing(HarnessSessionBinding, Option<String>),
    /// Launch the native session, then call [`PluginState::finish_open`].
    Spawn(Launch, HarnessSessionBinding),
}

/// Native plugin service owner. Host must admit/retain mutations and suppress sensitive traces
/// before invoking this boundary. Serialization is not a grant of execution authority.
#[derive(Default)]
pub struct AntigravityHarnessPlugin {
    state: Mutex<PluginState>,
}

impl RustPlugin for AntigravityHarnessPlugin {
    fn deactivate(&mut self) -> Result<(), PluginError> {
        self.deactivate_concurrent()
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        self.invoke_service_concurrent(context)
    }
}

impl ConcurrentRustPlugin for AntigravityHarnessPlugin {
    fn deactivate_concurrent(&self) -> Result<(), PluginError> {
        if let Ok(mut state) = self.state.lock() {
            state.owners.clear();
            state.terminal_flows.clear();
        }
        Ok(())
    }

    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
        if let Some((request, wait)) = observe_wait(&context) {
            self.wait_for_observations(&request, wait, &context.cancellation);
        }
        self.invoke(&context)
    }
}

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

/// Decode and admit one `bcode.harness/v3` request, or the service error to return.
fn decode_harness_request(
    context: &NativeServiceContext,
) -> Result<HarnessRequest, ServiceResponse> {
    if context.plugin_id != PLUGIN_ID || context.request.interface_id != HARNESS_INTERFACE_ID {
        return Err(ServiceResponse::error(
            "unsupported_interface",
            "Antigravity harness interface mismatch",
        ));
    }
    if context.request.payload.len() > MAX_HARNESS_WIRE_BYTES {
        return Err(ServiceResponse::error(
            "invalid_request",
            "Antigravity harness request exceeds bounds",
        ));
    }
    let Ok(request) = context.request.payload_json::<HarnessRequest>() else {
        return Err(ServiceResponse::error(
            "invalid_request",
            "Invalid typed Antigravity harness request",
        ));
    };
    if request.validate().is_err()
        || request.instance.plugin_id != PLUGIN_ID
        || context.request.operation != request.kind.operation().as_str()
        || context.cancellation.is_cancelled()
    {
        return Err(ServiceResponse::error(
            "invalid_request",
            "Antigravity harness admission mismatch or cancelled invocation",
        ));
    }
    Ok(request)
}

fn encode_harness_response(
    request: &HarnessRequest,
    kind: HarnessServiceResponseKind,
) -> ServiceResponse {
    let response = HarnessServiceResponse {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: request.request_id.clone(),
        operation: request.kind.operation(),
        kind,
    };
    if response.validate_for(request).is_err() {
        return ServiceResponse::error(
            "invalid_response",
            "Antigravity harness response failed contract validation",
        );
    }
    ServiceResponse::json(&response).unwrap_or_else(|_| {
        ServiceResponse::error(
            "encode_failed",
            "Antigravity harness response encoding failed",
        )
    })
}

enum ObservationReadiness {
    Run,
    LiveOnly(transport::Wake, u64),
    Waiting(transport::Wake, u64),
}

impl AntigravityHarnessPlugin {
    fn lock(&self) -> Result<MutexGuard<'_, PluginState>, AcpError> {
        self.state.lock().map_err(|_| AcpError::Disconnected)
    }

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
                    let _ = wake.wait_changed(seen, until - now);
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

    fn invoke(&self, context: &NativeServiceContext) -> ServiceResponse {
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
        let request = match decode_harness_request(context) {
            Ok(request) => request,
            Err(response) => return response,
        };
        let kind = self
            .handle(&request, context)
            .unwrap_or_else(|error| HarnessServiceResponseKind::Failure(failure(error)));
        encode_harness_response(&request, kind)
    }

    fn handle(
        &self,
        request: &HarnessRequest,
        context: &NativeServiceContext,
    ) -> Result<HarnessServiceResponseKind, AcpError> {
        let instance = &request.instance;
        let configuration = config::Config::from_context(context)?;
        configuration.validate_instance(instance)?;
        match &request.kind {
            HarnessRequestKind::Inspect => {
                let launch = launch(instance, configuration.auth_working_directory(instance))?;
                let mut server = transport::AcpServer::spawn(&launch)?;
                let native_version = server.initialize(&launch)?;
                Ok(HarnessServiceResponseKind::Inspection(HarnessInspection {
                    native_version,
                    executable_identity: instance.executable.clone(),
                    config_namespace: instance.config_namespace.clone(),
                    capabilities: capabilities(),
                }))
            }
            HarnessRequestKind::Open(open) => self
                .open(instance, open, None)
                .map(|(binding, _)| HarnessServiceResponseKind::Opened(binding)),
            HarnessRequestKind::Configure(configure) => self.configure(instance, configure),
            HarnessRequestKind::AuthStatus { verify } => {
                if *verify {
                    return Err(AcpError::Unsupported);
                }
                let launch = launch(instance, configuration.auth_working_directory(instance))?;
                Ok(HarnessServiceResponseKind::AuthStatus(
                    auth::inspect_status(&launch, &instance.auth)?,
                ))
            }
            HarnessRequestKind::AuthBegin(auth_req)
            | HarnessRequestKind::AuthContinue(auth_req)
            | HarnessRequestKind::AuthCancel(auth_req)
                if auth_req.method_id == terminal_auth::NATIVE_LOGIN_METHOD =>
            {
                self.terminal_login_step(instance, &request.kind, auth_req, &configuration)
            }
            kind => self.lock()?.handle_locked(instance, kind),
        }
    }

    /// Open (or return the completed exact open of) a native session. The native launch runs
    /// with the state lock released; the owner is retained as `opening` meanwhile so identical
    /// retries never spawn twice.
    pub(crate) fn open(
        &self,
        instance: &HarnessInstance,
        open: &HarnessOpenRequest,
        selection: Option<&bcode_harness_models::HarnessModelSelection>,
    ) -> Result<(HarnessSessionBinding, Option<String>), AcpError> {
        let plan = self.lock()?.prepare_open(instance, open, selection)?;
        let (launch, binding) = match plan {
            OpenPlan::Existing(binding, model) => return Ok((binding, model)),
            OpenPlan::Spawn(launch, binding) => (launch, binding),
        };
        let result = AcpSession::open_tracked(
            &launch,
            binding,
            open.resume.is_some(),
            selection.map(|selected| selected.model_id.as_str()),
        );
        self.lock()?.finish_open(open, result)
    }

    /// Apply a configuration change, waiting for the native acknowledgement unlocked.
    fn configure(
        &self,
        instance: &HarnessInstance,
        configure: &HarnessConfigureRequest,
    ) -> Result<HarnessServiceResponseKind, AcpError> {
        let acknowledged = HarnessServiceResponseKind::Acknowledged {
            mutation_id: configure.mutation_id.clone(),
        };
        let Some(pending) = self.lock()?.begin_configure(instance, configure)? else {
            return Ok(acknowledged);
        };
        let result = pending.wait();
        self.lock()?.finish_configure(configure, &result)?;
        Ok(acknowledged)
    }

    fn terminal_login_step(
        &self,
        instance: &HarnessInstance,
        kind: &HarnessRequestKind,
        auth_req: &bcode_harness_models::HarnessAuthRequest,
        configuration: &config::Config,
    ) -> Result<HarnessServiceResponseKind, AcpError> {
        if auth_req.input.is_some() || instance.auth.custody != AuthCustodyMode::NativeManaged {
            return Err(AcpError::Unsupported);
        }
        {
            let mut state = self.lock()?;
            match kind {
                HarnessRequestKind::AuthBegin(_) => {
                    if let Some(retained) = state.terminal_flows.get(&auth_req.flow_id) {
                        if retained != instance {
                            return Err(AcpError::Stale);
                        }
                    } else {
                        if state.terminal_flows.len() >= MAX_TERMINAL_FLOWS {
                            return Err(AcpError::Bounds);
                        }
                        state
                            .terminal_flows
                            .insert(auth_req.flow_id.clone(), instance.clone());
                    }
                }
                HarnessRequestKind::AuthCancel(_) => {
                    if state.terminal_flows.get(&auth_req.flow_id) != Some(instance) {
                        return Err(AcpError::Stale);
                    }
                    state.terminal_flows.remove(&auth_req.flow_id);
                    return Ok(HarnessServiceResponseKind::AuthStep(HarnessAuthStep {
                        flow_id: auth_req.flow_id.clone(),
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
                    if state.terminal_flows.get(&auth_req.flow_id) != Some(instance) {
                        return Err(AcpError::Stale);
                    }
                }
            }
        }
        let launch = launch(instance, configuration.auth_working_directory(instance))?;
        let status = auth::inspect_status(&launch, &instance.auth)?;
        let complete = status.readiness == AuthReadiness::Ready;
        if complete {
            let mut state = self.lock()?;
            if state.terminal_flows.get(&auth_req.flow_id) == Some(instance) {
                state.terminal_flows.remove(&auth_req.flow_id);
            }
        }
        Ok(HarnessServiceResponseKind::AuthStep(HarnessAuthStep {
            flow_id: auth_req.flow_id.clone(),
            status,
            complete,
            interaction: (!complete).then(|| SensitiveAuthInteraction::NativeHandoff {
                request_id: auth_req.flow_id.clone(),
            }),
        }))
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

    /// Operations that only touch retained state and write to an already-running transport.
    fn handle_locked(
        &mut self,
        instance: &HarnessInstance,
        kind: &HarnessRequestKind,
    ) -> Result<HarnessServiceResponseKind, AcpError> {
        match kind {
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
            HarnessRequestKind::Observe(observe) => self.handle_observe(instance, observe),
            HarnessRequestKind::Reconcile(reconcile) => {
                let Some(binding) = &reconcile.binding else {
                    return Ok(HarnessServiceResponseKind::Reconciled(
                        HarnessReconciliation::Uncertain,
                    ));
                };
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
            _ => Err(AcpError::Unsupported),
        }
    }

    fn prepare_open(
        &mut self,
        instance: &HarnessInstance,
        open: &HarnessOpenRequest,
        selection: Option<&bcode_harness_models::HarnessModelSelection>,
    ) -> Result<OpenPlan, AcpError> {
        if let Some(owner) = self.owners.get(&open.session_id) {
            // A later generation may replace an owner without a live or in-progress native
            // session (released by Close, or left uncertain by a failed open).
            let superseded = owner.instance == *instance
                && owner.session.is_none()
                && !owner.opening
                && open.generation > owner.open.generation;
            if !superseded {
                if owner.instance != *instance
                    || owner.open != *open
                    || owner.selection.as_ref() != selection
                {
                    return Err(AcpError::Stale);
                }
                return owner
                    .session
                    .as_ref()
                    .map(|session| {
                        OpenPlan::Existing(session.binding().clone(), owner.effective_model.clone())
                    })
                    .ok_or(AcpError::Uncertain);
            }
            self.owners.remove(&open.session_id);
        }
        if self.owners.len() >= MAX_OWNERS {
            self.owners
                .retain(|_, owner| owner.session.is_some() || owner.released.is_none());
            if self.owners.len() >= MAX_OWNERS {
                return Err(AcpError::Bounds);
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
                native_version: "unverified-acp-v1".to_owned(),
                generation: open.generation,
            });
        // Retain the open before any native mutation; identical retries never spawn again.
        let mut owner = Owner::new(instance, open);
        owner.selection = selection.cloned();
        owner.opening = true;
        self.owners.insert(open.session_id.clone(), owner);
        Ok(OpenPlan::Spawn(launch, binding))
    }

    fn finish_open(
        &mut self,
        open: &HarnessOpenRequest,
        result: Result<AcpSession, OpenFailure>,
    ) -> Result<(HarnessSessionBinding, Option<String>), AcpError> {
        let Some(owner) = self
            .owners
            .get_mut(&open.session_id)
            .filter(|owner| owner.opening && owner.open == *open)
        else {
            // Deactivated or replaced meanwhile; a launched session is dropped and reaped.
            return Err(AcpError::Stale);
        };
        owner.opening = false;
        match result {
            Ok(session) => {
                let binding = session.binding().clone();
                let model = session.model().map(str::to_owned);
                owner.effective_model.clone_from(&model);
                owner.session = Some(session);
                Ok((binding, model))
            }
            Err(failure) => {
                // Only failures that provably created nothing release the owner for a retry;
                // otherwise the open stays uncertain so a retry cannot duplicate a session.
                if !failure.native_session_possible {
                    self.owners.remove(&open.session_id);
                }
                Err(failure.error)
            }
        }
    }

    fn handle_close(
        &mut self,
        instance: &HarnessInstance,
        control: &HarnessControlRequest,
    ) -> Result<HarnessServiceResponseKind, AcpError> {
        let binding = confirmed(&control.target)?;
        let owner = self
            .owners
            .get_mut(&binding.session_id)
            .ok_or(AcpError::Uncertain)?;
        if owner.instance != *instance {
            return Err(AcpError::Stale);
        }
        if let Some((released, _)) = &owner.released {
            return if released == control {
                Ok(HarnessServiceResponseKind::Acknowledged {
                    mutation_id: control.mutation_id.clone(),
                })
            } else {
                Err(AcpError::Stale)
            };
        }
        let session = owner.session.as_mut().ok_or(AcpError::Uncertain)?;
        if session.binding() != binding {
            return Err(AcpError::Stale);
        }
        let reaped = session.reap_flag();
        session.send_close();
        owner.session.take();
        owner.released = Some((control.clone(), reaped));
        Ok(HarnessServiceResponseKind::Acknowledged {
            mutation_id: control.mutation_id.clone(),
        })
    }

    /// Record and write a configure mutation. `None` means an identical retry already
    /// succeeded.
    fn begin_configure(
        &mut self,
        instance: &HarnessInstance,
        configure: &HarnessConfigureRequest,
    ) -> Result<Option<PendingConfigure>, AcpError> {
        let binding = confirmed(&configure.target)?;
        self.session(instance, binding)?;
        let owner = self
            .owners
            .get_mut(&binding.session_id)
            .ok_or(AcpError::Stale)?;
        if let Some((retained, result)) = owner.configures.get(&configure.mutation_id) {
            if retained != configure {
                return Err(AcpError::Stale);
            }
            (*result)?;
            return Ok(None);
        }
        if owner.configures.len() >= MAX_RETAINED_CONFIGURES {
            owner.configures.pop_first();
        }
        let begun = owner
            .session
            .as_mut()
            .ok_or(AcpError::Uncertain)?
            .begin_configure(&configure.setting);
        // In flight until the acknowledgement is recorded; a retry meanwhile is uncertain.
        let recorded = Err(begun.as_ref().err().copied().unwrap_or(AcpError::Uncertain));
        owner
            .configures
            .insert(configure.mutation_id.clone(), (configure.clone(), recorded));
        begun.map(Some)
    }

    fn finish_configure(
        &mut self,
        configure: &HarnessConfigureRequest,
        result: &Result<Option<String>, AcpError>,
    ) -> Result<(), AcpError> {
        let binding = confirmed(&configure.target)?;
        let outcome = result.as_ref().map(|_| ()).map_err(|error| *error);
        if let Some(owner) = self.owners.get_mut(&binding.session_id) {
            if let Some(entry) = owner.configures.get_mut(&configure.mutation_id)
                && entry.0 == *configure
            {
                entry.1 = outcome;
            }
            if let (Ok(model), Some(session)) = (result, owner.session.as_mut())
                && session.binding() == binding
            {
                session.finish_configure(model.as_deref());
            }
        }
        outcome
    }

    fn handle_interrupt(
        &mut self,
        instance: &HarnessInstance,
        control: &HarnessControlRequest,
    ) -> Result<HarnessServiceResponseKind, AcpError> {
        let binding = confirmed(&control.target)?;
        self.session(instance, binding)?;
        let owner = self
            .owners
            .get_mut(&binding.session_id)
            .ok_or(AcpError::Stale)?;
        if let Some((mutation_id, result)) = &owner.interrupt {
            if *mutation_id == control.mutation_id {
                (*result)?;
                return Ok(HarnessServiceResponseKind::Acknowledged {
                    mutation_id: control.mutation_id.clone(),
                });
            }
            if result.is_err_and(|error| error == AcpError::Uncertain) {
                return Err(AcpError::Uncertain);
            }
        }
        owner.interrupt = Some((control.mutation_id.clone(), Err(AcpError::Uncertain)));
        let result = owner
            .session
            .as_mut()
            .ok_or(AcpError::Uncertain)?
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
    ) -> Result<HarnessServiceResponseKind, AcpError> {
        let binding = confirmed(&observe.target)?;
        self.session(instance, binding)?;
        let owner = self
            .owners
            .get_mut(&binding.session_id)
            .ok_or(AcpError::Stale)?;
        if let Some((after, page)) = &owner.previous
            && observe.after == *after
            && observe.after != owner.next
        {
            return Ok(HarnessServiceResponseKind::Observations(page.clone()));
        }
        if observe.after != owner.next {
            return Err(AcpError::Uncertain);
        }
        let session = owner.session.as_mut().ok_or(AcpError::Uncertain)?;
        if let Some(retention) = &observe.retention {
            session.apply_retention(retention)?;
        }
        let (observations, completeness) = session.observe(usize::from(observe.limit.min(8)))?;
        let status = session.status();
        let hydration_requests = session.hydration_requests();
        let inventory_page = session.take_inventory_page();
        let live = session.take_live();
        let transfers = !observations.is_empty()
            || !hydration_requests.is_empty()
            || inventory_page.is_some()
            || completeness == bcode_harness_models::HarnessCompleteness::Gap;
        if !transfers {
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
        owner.page_number = owner.page_number.checked_add(1).ok_or(AcpError::Bounds)?;
        let next_after = Some(format!(
            "antigravity-page-{}-{}",
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
    ) -> Result<&mut AcpSession, AcpError> {
        let owner = self
            .owners
            .get_mut(&binding.session_id)
            .ok_or(AcpError::Uncertain)?;
        if owner.instance != *instance {
            return Err(AcpError::Stale);
        }
        let session = owner.session.as_mut().ok_or(AcpError::Uncertain)?;
        if session.binding() != binding {
            return Err(AcpError::Stale);
        }
        Ok(session)
    }
}

const fn failure(error: AcpError) -> HarnessFailure {
    match error {
        AcpError::Stale => HarnessFailure::StaleGeneration,
        AcpError::Unsupported => HarnessFailure::Unsupported,
        AcpError::Configuration | AcpError::Bounds | AcpError::Protocol => {
            HarnessFailure::InvalidRequest
        }
        AcpError::Rejected => HarnessFailure::Policy,
        AcpError::Disconnected => HarnessFailure::Unavailable,
        AcpError::Uncertain => HarnessFailure::Uncertain,
    }
}

const fn confirmed(target: &HarnessSessionTarget) -> Result<&HarnessSessionBinding, AcpError> {
    match target {
        HarnessSessionTarget::Confirmed(binding) => Ok(binding),
        HarnessSessionTarget::Prepared(_) => Err(AcpError::Unsupported),
    }
}

pub(crate) fn launch(instance: &HarnessInstance, cwd: &str) -> Result<Launch, AcpError> {
    if instance.auth.provider_id != AUTH_PROVIDER_ID
        || instance.config_namespace != instance.auth.credential_namespace
        || instance.auth.custody != AuthCustodyMode::NativeManaged
    {
        return Err(AcpError::Configuration);
    }
    Ok(Launch {
        executable: PathBuf::from(&instance.executable),
        gemini_dir: PathBuf::from(&instance.config_namespace),
        working_directory: PathBuf::from(cwd),
        timeout: Duration::from_secs(30),
    })
}

/// Capabilities declared by the Antigravity ACP harness backend.
#[must_use]
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
            "Native ACP session/set_model for subsequent turns; acknowledgement is the native response, not proof a later turn used it",
        ),
        permission_mode: constrained(
            "default=default, accept_edits=accept-edits, plan=plan via ACP session/set_mode; applies to subsequent turns",
        ),
        resume: constrained(
            "Exact binding and ACP sessionId via session/resume without history replay; requires matching native CLI version",
        ),
        steering: constrained(
            "NextTurn admitted only after the previous foreground prompt completes with no pending permission callback; mid-turn steer unsupported",
        ),
        approval: constrained("Exact generation ACP session/request_permission callback"),
        questions: unsupported(),
        assistance: unsupported(),
        interrupt: constrained(
            "ACP session/cancel notification; acknowledgement is not authoritative settlement",
        ),
        authoritative_settlement: unsupported(),
        background_continuation: unsupported(),
        authentication: constrained(
            "Native Google OAuth via client-terminal agy acp --login; no remote verification",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::HarnessPreparedSession;

    #[test]
    fn prepared_target_is_not_promoted_to_an_invented_acp_session() {
        let target = HarnessSessionTarget::Prepared(HarnessPreparedSession {
            open_id: "open".into(),
            session_id: "session".into(),
            instance_id: "instance".into(),
            generation: 1,
            native_version: "1.5.15".into(),
            requested_native_session_id: None,
        });
        assert_eq!(confirmed(&target), Err(AcpError::Unsupported));
    }

    fn instance() -> HarnessInstance {
        HarnessInstance {
            instance_id: "antigravity".into(),
            plugin_id: PLUGIN_ID.into(),
            executable: "/bin/agy".into(),
            config_namespace: "/home/user/.gemini".into(),
            auth: bcode_harness_models::HarnessAuthBinding {
                provider_id: AUTH_PROVIDER_ID.into(),
                profile: "native".into(),
                credential_namespace: "/home/user/.gemini".into(),
                custody: AuthCustodyMode::NativeManaged,
            },
        }
    }

    fn open_request(generation: u64) -> HarnessOpenRequest {
        HarnessOpenRequest {
            open_id: format!("open-{generation}"),
            session_id: "session".into(),
            generation,
            working_directory: "/work".into(),
            resume: None,
            parent_assistance: None,
        }
    }

    #[test]
    fn only_definite_open_failures_release_the_owner_for_a_retry() {
        let mut state = PluginState::default();
        let instance = instance();
        let first = open_request(1);
        assert!(matches!(
            state.prepare_open(&instance, &first, None),
            Ok(OpenPlan::Spawn(..))
        ));
        // An identical retry while the native open runs never spawns a second process.
        assert!(matches!(
            state.prepare_open(&instance, &first, None),
            Err(AcpError::Uncertain)
        ));

        // A rejected `session/new` created nothing: the same open may be retried.
        let rejected = OpenFailure {
            error: AcpError::Rejected,
            native_session_possible: false,
        };
        assert_eq!(
            state.finish_open(&first, Err(rejected)),
            Err(AcpError::Rejected)
        );
        assert!(matches!(
            state.prepare_open(&instance, &first, None),
            Ok(OpenPlan::Spawn(..))
        ));

        // A timeout may have created a native session: retries stay uncertain...
        let timed_out = OpenFailure {
            error: AcpError::Uncertain,
            native_session_possible: true,
        };
        assert_eq!(
            state.finish_open(&first, Err(timed_out)),
            Err(AcpError::Uncertain)
        );
        assert!(matches!(
            state.prepare_open(&instance, &first, None),
            Err(AcpError::Uncertain)
        ));
        // ...until a later generation supersedes the abandoned open.
        assert!(matches!(
            state.prepare_open(&instance, &open_request(2), None),
            Ok(OpenPlan::Spawn(..))
        ));
        // A stale generation cannot replace the newer owner.
        assert!(matches!(
            state.prepare_open(&instance, &first, None),
            Err(AcpError::Stale)
        ));
    }

    #[test]
    fn capabilities_are_explicit_and_valid() {
        let declared = capabilities();
        assert!(declared.validate().is_ok());
        assert_eq!(
            declared.model_switch.support,
            HarnessSupportLevel::Constrained
        );
        assert_eq!(
            declared.permission_mode.support,
            HarnessSupportLevel::Constrained
        );
        assert_eq!(
            declared.authoritative_settlement.support,
            HarnessSupportLevel::Unsupported
        );
        assert_eq!(
            declared.background_continuation.support,
            HarnessSupportLevel::Unsupported
        );
    }
}

// One declaration serves bundled and native packaging; implementations export no ABI symbols.
bcode_plugin_sdk::define_plugin!(
    concurrent AntigravityHarnessPlugin,
    include_str!("../bcode-plugin.toml")
);
