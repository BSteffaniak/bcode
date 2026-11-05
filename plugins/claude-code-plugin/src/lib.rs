#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Claude Code execution backend. The host owns admission and canonical persistence.
mod auth;
mod capabilities;
mod config;
mod normalize;
mod parent_bridge;
pub use auth::{
    NATIVE_HANDOFF_INTERFACE_ID, NATIVE_HANDOFF_OPERATION, NativeLoginExecution,
    NativeLoginHandoffData, NativeLoginHandoffRequest, NativeLoginHandoffResponse,
    NativeLoginInactivity, NativeLoginMethodEvidence, native_login_handoff_data,
};
#[cfg(test)]
mod tests;

use bcode_claude_code::{ClaudeClient, ClaudeSession, Event, Settlement};
use bcode_harness_models::{
    HARNESS_INTERFACE_ID, HARNESS_SCHEMA_VERSION, HarnessAttention, HarnessAuthCustody,
    HarnessCapability, HarnessCompleteness, HarnessContractError, HarnessControlRequest,
    HarnessDeliveryState, HarnessFailure, HarnessInspection, HarnessInstance,
    HarnessInteractionKind, HarnessObservation, HarnessObservationPage, HarnessObserveRequest,
    HarnessOpenRequest, HarnessPlacement, HarnessPreparedSession, HarnessReceipt,
    HarnessReconcileRequest, HarnessReconciliation, HarnessRequest, HarnessRequestKind,
    HarnessResponse, HarnessServiceResponse, HarnessServiceResponseKind, HarnessSessionTarget,
    HarnessSubmission, HarnessSupportLevel, MAX_HARNESS_ID_BYTES, MAX_HARNESS_WIRE_BYTES,
};
use bcode_plugin_sdk::prelude::*;
use config::{Config, InstanceConfig};
use normalize::Normalizer;
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
};

const PLUGIN_ID: &str = "bcode.claude-code";
const MAX_PROCESSES: usize = 32;
const MAX_RETAINED_MUTATIONS: usize = 256;
const MAX_RETAINED_OBSERVATIONS: usize = 256;

struct StreamState {
    normalizer: Normalizer,
    observations: VecDeque<HarnessObservation>,
    gap: bool,
    acknowledged: Option<String>,
}
impl StreamState {
    fn push(&mut self, observations: Vec<HarnessObservation>) -> bool {
        if self.observations.len().saturating_add(observations.len()) > MAX_RETAINED_OBSERVATIONS {
            self.gap = true;
            return false;
        }
        if observations
            .iter()
            .any(|value| value.completeness == HarnessCompleteness::Gap)
        {
            self.gap = true;
        }
        self.observations.extend(observations);
        true
    }
}
impl StreamState {
    fn disconnected(&mut self) {
        self.gap = true;
        self.normalizer.invalidate_callbacks();
    }

    fn observation_page(
        &mut self,
        prepared: &HarnessPreparedSession,
        observe: &HarnessObserveRequest,
        settlement: &Settlement,
    ) -> Result<HarnessServiceResponseKind, HarnessFailure> {
        if !retained_target_matches(&self.normalizer.target, prepared, &observe.target) {
            return Err(HarnessFailure::StaleGeneration);
        }
        // Cursor acknowledgement only discards the exact known prefix; arbitrary cursors fail closed.
        if let Some(after) = &observe.after
            && self.acknowledged.as_ref() != Some(after)
        {
            let index = self
                .observations
                .iter()
                .position(|item| &item.observation_id == after)
                .ok_or(HarnessFailure::Conflict)?;
            self.observations.drain(..=index);
            self.acknowledged = Some(after.clone());
        }
        let mut observations = Vec::new();
        let mut bytes = 0_usize;
        for observation in self.observations.iter().take(usize::from(observe.limit)) {
            let size = serde_json::to_vec(observation)
                .map_err(|_| HarnessFailure::InvalidRequest)?
                .len();
            if bytes.saturating_add(size) > MAX_HARNESS_WIRE_BYTES / 2 {
                break;
            }
            bytes += size;
            observations.push(observation.clone());
        }
        let mut execution_status = normalize::status(settlement);
        if *settlement != Settlement::Uncertain
            && self
                .normalizer
                .pending
                .values()
                .any(|(interaction, _)| interaction.kind == HarnessInteractionKind::Assistance)
        {
            execution_status.attention = HarnessAttention::Assistance;
        }
        Ok(HarnessServiceResponseKind::Observations(
            HarnessObservationPage {
                next_after: observations.last().map(|item| item.observation_id.clone()),
                observations,
                completeness: if self.gap {
                    HarnessCompleteness::Gap
                } else {
                    HarnessCompleteness::Complete
                },
                status: execution_status,
            },
        ))
    }
}

fn retained_target_matches(
    retained: &HarnessSessionTarget,
    prepared: &HarnessPreparedSession,
    proposed: &HarnessSessionTarget,
) -> bool {
    if proposed.validate().is_err() {
        return false;
    }
    retained == proposed
        || matches!(proposed, HarnessSessionTarget::Prepared(value) if value == prepared)
}

struct Process {
    instance: HarnessInstance,
    open: HarnessOpenRequest,
    prepared: HarnessPreparedSession,
    client: ClaudeClient,
    stream: Arc<Mutex<StreamState>>,
    submissions: BTreeMap<String, (HarnessSubmission, HarnessReceipt)>,
    responses: BTreeMap<String, HarnessResponse>,
    controls: BTreeMap<String, &'static str>,
    closed: bool,
}
/// Native plugin with a persistent I/O runtime. No plugin-owned persistence or recovery journal.
#[derive(Default)]
pub struct ClaudeCodePlugin {
    runtime: Option<tokio::runtime::Runtime>,
    processes: BTreeMap<String, Process>,
    /// Failed opens are uncertain and cannot be silently repeated during this runtime.
    opens: BTreeMap<String, HarnessOpenRequest>,
    auth: auth::AuthFlows,
}
impl RustPlugin for ClaudeCodePlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.plugin_id == PLUGIN_ID
            && context.request.interface_id == NATIVE_HANDOFF_INTERFACE_ID
        {
            return prepare_handoff(&context);
        }
        if context.plugin_id != PLUGIN_ID || context.request.interface_id != HARNESS_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "Claude harness interface unavailable",
            );
        }
        if context.request.payload.len() > MAX_HARNESS_WIRE_BYTES {
            return ServiceResponse::error("invalid_request", "Harness request exceeds wire bound");
        }
        let request: HarnessRequest = match serde_json::from_slice(&context.request.payload) {
            Ok(request) => request,
            Err(_) => {
                return ServiceResponse::error("invalid_request", "Invalid typed harness request");
            }
        };
        if request.validate().is_err()
            || request.instance.plugin_id != PLUGIN_ID
            || request.kind.operation().as_str() != context.request.operation
        {
            return ServiceResponse::error(
                "invalid_request",
                "Harness contract or operation mismatch",
            );
        }
        let configuration: Config = match context.config() {
            Ok(value) => value,
            Err(_) => {
                return ServiceResponse::error(
                    "invalid_config",
                    "Invalid Claude harness configuration",
                );
            }
        };
        let Some(selected) = configuration.instances.get(&request.instance.instance_id) else {
            return reply(
                &request,
                HarnessServiceResponseKind::Failure(HarnessFailure::Unavailable),
            );
        };
        if self.runtime.is_none() {
            match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => self.runtime = Some(runtime),
                Err(_) => {
                    return reply(
                        &request,
                        HarnessServiceResponseKind::Failure(HarnessFailure::Unavailable),
                    );
                }
            }
        }
        // A long-lived runtime keeps subprocess readers alive between service invocations.
        let Some(runtime) = self.runtime.take() else {
            return reply(
                &request,
                HarnessServiceResponseKind::Failure(HarnessFailure::Unavailable),
            );
        };
        let result = runtime.block_on(self.invoke(&request, selected));
        self.runtime = Some(runtime);
        reply(
            &request,
            result.unwrap_or_else(HarnessServiceResponseKind::Failure),
        )
    }
}
impl ClaudeCodePlugin {
    async fn invoke(
        &mut self,
        request: &HarnessRequest,
        selected: &InstanceConfig,
    ) -> Result<HarnessServiceResponseKind, HarnessFailure> {
        if request.instance.executable != selected.executable.to_string_lossy()
            || request.instance.config_namespace != selected.config_directory.to_string_lossy()
            || request.instance.auth.credential_namespace != request.instance.config_namespace
            || request.instance.auth.provider_id != "claude-code"
            || request.instance.auth.custody != HarnessAuthCustody::NativeManaged
        {
            return Err(HarnessFailure::Policy);
        }
        if let Some(process) = self
            .processes
            .values()
            .find(|process| process.instance.instance_id == request.instance.instance_id)
        {
            let version = match &process
                .stream
                .lock()
                .map_err(|_| HarnessFailure::Unavailable)?
                .normalizer
                .target
            {
                HarnessSessionTarget::Prepared(value) => value.native_version.clone(),
                HarnessSessionTarget::Confirmed(value) => value.native_version.clone(),
            };
            if version != selected.exact_version {
                return Err(HarnessFailure::Conflict);
            }
        }
        match &request.kind {
            HarnessRequestKind::Inspect => {
                let options =
                    selected.options(&request.instance, selected.config_directory.clone(), None)?;
                let mut capabilities = capabilities::capabilities(options.settlement_support);
                if !selected.verified_native_auth {
                    capabilities.authentication = HarnessCapability {
                        support: HarnessSupportLevel::Unsupported,
                        constraints: vec![
                            "Exact native auth status/login support has not been verified".into(),
                        ],
                    };
                }
                Ok(HarnessServiceResponseKind::Inspection(HarnessInspection {
                    native_version: selected.exact_version.clone(),
                    executable_identity: request.instance.executable.clone(),
                    config_namespace: request.instance.config_namespace.clone(),
                    capabilities,
                }))
            }
            HarnessRequestKind::Open(open) => self.open(&request.instance, open, selected).await,
            HarnessRequestKind::Submit { target, submission } => {
                self.submit(&request.instance, target, submission).await
            }
            HarnessRequestKind::Observe(observe) => self.observe(&request.instance, observe),
            HarnessRequestKind::Respond { target, response } => {
                self.respond(&request.instance, target, response).await
            }
            HarnessRequestKind::Interrupt(control) | HarnessRequestKind::Close(control) => {
                self.control(
                    &request.instance,
                    control,
                    matches!(&request.kind, HarnessRequestKind::Close(_)),
                )
                .await
            }
            HarnessRequestKind::Reconcile(reconcile) => {
                self.reconcile(&request.instance, reconcile)
            }
            HarnessRequestKind::AuthStatus { .. }
            | HarnessRequestKind::AuthBegin(_)
            | HarnessRequestKind::AuthContinue(_)
            | HarnessRequestKind::AuthCancel(_) => {
                let options =
                    selected.options(&request.instance, selected.config_directory.clone(), None)?;
                self.auth.invoke(request, &options).await
            }
        }
    }
    async fn submit(
        &mut self,
        instance: &HarnessInstance,
        target: &HarnessSessionTarget,
        submission: &HarnessSubmission,
    ) -> Result<HarnessServiceResponseKind, HarnessFailure> {
        let process = self.process(instance, target)?;
        if let Some((original, receipt)) = process.submissions.get(&submission.submission_id) {
            return if original == submission {
                Ok(HarnessServiceResponseKind::Receipt(receipt.clone()))
            } else {
                Err(HarnessFailure::Conflict)
            };
        }
        if process.closed {
            return Err(HarnessFailure::Unavailable);
        }
        if process.submissions.len() >= MAX_RETAINED_MUTATIONS {
            if process.client.settlement() != Settlement::Settled {
                return Err(HarnessFailure::Unavailable);
            }
            // Canonical host admission owns replay fencing over the entire lifetime.
            // These caches are only recent evidence; eviction never establishes NotApplied.
            process.submissions.clear();
            process.responses.clear();
            process.controls.clear();
        }
        if matches!(target, HarnessSessionTarget::Prepared(_))
            && process
                .stream
                .lock()
                .map_err(|_| HarnessFailure::Unavailable)?
                .normalizer
                .submitted
        {
            return Err(HarnessFailure::Conflict);
        }
        if !matches!(submission.placement, HarnessPlacement::NextTurn) {
            return Err(HarnessFailure::Unsupported);
        }
        if process
            .stream
            .lock()
            .map_err(|_| HarnessFailure::Unavailable)?
            .normalizer
            .submitted
            && process.client.settlement() != Settlement::Settled
        {
            return Err(HarnessFailure::Conflict);
        }
        let mut receipt = HarnessReceipt {
            submission_id: submission.submission_id.clone(),
            attempt_id: submission.attempt_id.clone(),
            session_id: submission.session_id.clone(),
            generation: submission.generation,
            state: HarnessDeliveryState::DeliveryAttempted,
            native_request_id: None,
        };
        process.submissions.insert(
            submission.submission_id.clone(),
            (submission.clone(), receipt.clone()),
        );
        {
            let mut stream = process
                .stream
                .lock()
                .map_err(|_| HarnessFailure::Unavailable)?;
            stream.normalizer.submitted = true;
            stream.normalizer.turn_id = None;
        }
        // A completed write is not a native acceptance acknowledgement.
        let native_selection = match target {
            HarnessSessionTarget::Prepared(value) => value.requested_native_session_id.as_deref(),
            HarnessSessionTarget::Confirmed(value) => Some(value.native_session_id.as_str()),
        };
        receipt.state = if process
            .client
            .submit(&submission.text, native_selection)
            .await
            .is_ok()
        {
            HarnessDeliveryState::DeliveryAttempted
        } else {
            HarnessDeliveryState::Uncertain
        };
        process.submissions.insert(
            submission.submission_id.clone(),
            (submission.clone(), receipt.clone()),
        );
        Ok(HarnessServiceResponseKind::Receipt(receipt))
    }
    fn observe(
        &mut self,
        instance: &HarnessInstance,
        observe: &HarnessObserveRequest,
    ) -> Result<HarnessServiceResponseKind, HarnessFailure> {
        let process = self.retained_process(instance, &observe.target)?;
        let mut stream = process
            .stream
            .lock()
            .map_err(|_| HarnessFailure::Unavailable)?;
        stream.observation_page(&process.prepared, observe, &process.client.settlement())
    }
    async fn respond(
        &mut self,
        instance: &HarnessInstance,
        target: &HarnessSessionTarget,
        response: &HarnessResponse,
    ) -> Result<HarnessServiceResponseKind, HarnessFailure> {
        let process = self.process(instance, target)?;
        if process.responses.contains_key(&response.response_id) {
            return Err(HarnessFailure::Uncertain);
        }
        if process.responses.len() >= MAX_RETAINED_MUTATIONS || process.closed {
            return Err(HarnessFailure::Unavailable);
        }
        let native = {
            let stream = process
                .stream
                .lock()
                .map_err(|_| HarnessFailure::Unavailable)?;
            if stream.normalizer.assistance_grant != process.open.parent_assistance {
                return Err(HarnessFailure::StaleGeneration);
            }
            stream
                .normalizer
                .response(response)
                .map_err(|_| HarnessFailure::StaleGeneration)?
        };
        process
            .responses
            .insert(response.response_id.clone(), response.clone());
        process
            .client
            .respond(&response.request_id, native)
            .await
            .map_err(|_| HarnessFailure::Uncertain)?;
        process
            .stream
            .lock()
            .map_err(|_| HarnessFailure::Unavailable)?
            .normalizer
            .complete_callback(&response.request_id);
        Ok(HarnessServiceResponseKind::Acknowledged {
            mutation_id: response.response_id.clone(),
        })
    }
    async fn control(
        &mut self,
        instance: &HarnessInstance,
        control: &HarnessControlRequest,
        close: bool,
    ) -> Result<HarnessServiceResponseKind, HarnessFailure> {
        let operation = if close { "close" } else { "interrupt" };
        let process = self.process(instance, &control.target)?;
        if process.controls.contains_key(&control.mutation_id) {
            return Err(HarnessFailure::Uncertain);
        }
        if process.controls.len() >= MAX_RETAINED_MUTATIONS {
            return Err(HarnessFailure::Unavailable);
        }
        process
            .controls
            .insert(control.mutation_id.clone(), operation);
        if operation == "close" {
            process
                .stream
                .lock()
                .map_err(|_| HarnessFailure::Unavailable)?
                .normalizer
                .invalidate_callbacks();
            process.client.terminate();
            process.closed = true;
        } else {
            process
                .client
                .interrupt()
                .await
                .map_err(|_| HarnessFailure::Uncertain)?;
        }
        Ok(HarnessServiceResponseKind::Acknowledged {
            mutation_id: control.mutation_id.clone(),
        })
    }
    fn reconcile(
        &self,
        instance: &HarnessInstance,
        reconcile: &HarnessReconcileRequest,
    ) -> Result<HarnessServiceResponseKind, HarnessFailure> {
        let Some(process) = self.processes.get(&reconcile.session_id) else {
            return Ok(HarnessServiceResponseKind::Reconciled(
                HarnessReconciliation::Uncertain,
            ));
        };
        let target = process
            .stream
            .lock()
            .map_err(|_| HarnessFailure::Unavailable)?
            .normalizer
            .target
            .clone();
        if target.generation() != reconcile.generation || process.instance != *instance {
            return Err(HarnessFailure::StaleGeneration);
        }
        let HarnessSessionTarget::Confirmed(binding) = target else {
            return Ok(HarnessServiceResponseKind::Reconciled(
                HarnessReconciliation::Uncertain,
            ));
        };
        if reconcile
            .open_id
            .as_ref()
            .is_some_and(|id| id != &process.open.open_id)
            || reconcile
                .binding
                .as_ref()
                .is_some_and(|value| value != &binding)
        {
            return Err(HarnessFailure::Conflict);
        }
        let receipt = reconcile
            .submission_id
            .as_ref()
            .and_then(|id| process.submissions.get(id))
            .map(|(_, receipt)| receipt.clone());
        if let (Some(attempt), Some(receipt)) = (&reconcile.attempt_id, &receipt)
            && attempt != &receipt.attempt_id
        {
            return Err(HarnessFailure::Conflict);
        }
        if reconcile.submission_id.is_some() && receipt.is_none() {
            return Ok(HarnessServiceResponseKind::Reconciled(
                HarnessReconciliation::Uncertain,
            ));
        }
        Ok(HarnessServiceResponseKind::Reconciled(
            HarnessReconciliation::Known {
                binding,
                receipt,
                status: normalize::status(&process.client.settlement()),
            },
        ))
    }
    fn retained_process(
        &mut self,
        instance: &HarnessInstance,
        target: &HarnessSessionTarget,
    ) -> Result<&mut Process, HarnessFailure> {
        let process = self
            .processes
            .get_mut(target.session_id())
            .ok_or(HarnessFailure::Uncertain)?;
        if process.instance != *instance {
            return Err(HarnessFailure::Conflict);
        }
        let valid = {
            let stream = process
                .stream
                .lock()
                .map_err(|_| HarnessFailure::Unavailable)?;
            retained_target_matches(&stream.normalizer.target, &process.prepared, target)
        };
        if !valid {
            return Err(HarnessFailure::StaleGeneration);
        }
        Ok(process)
    }

    fn process(
        &mut self,
        instance: &HarnessInstance,
        target: &HarnessSessionTarget,
    ) -> Result<&mut Process, HarnessFailure> {
        let process = self.retained_process(instance, target)?;
        if process.closed || !process.client.is_control_ready() {
            return Err(HarnessFailure::Uncertain);
        }
        Ok(process)
    }
    async fn open(
        &mut self,
        instance: &HarnessInstance,
        open: &HarnessOpenRequest,
        selected: &InstanceConfig,
    ) -> Result<HarnessServiceResponseKind, HarnessFailure> {
        if let Some(process) = self.processes.get(&open.session_id) {
            if process.closed || !process.client.is_control_ready() {
                return Err(HarnessFailure::Uncertain);
            }
            if process.instance != *instance || process.open != *open {
                return Err(HarnessFailure::Conflict);
            }
            return Ok(open_response(
                &process
                    .stream
                    .lock()
                    .map_err(|_| HarnessFailure::Unavailable)?
                    .normalizer
                    .target,
            ));
        }
        // Closed instances release process capacity. Host admission must fence old canonical
        // open/submission identities across eviction and plugin restart.
        let closed: Vec<_> = self
            .processes
            .iter()
            .filter(|(_, process)| process.closed)
            .map(|(id, _)| id.clone())
            .collect();
        for id in closed {
            self.processes.remove(&id);
            self.opens.remove(&id);
        }
        if self.opens.contains_key(&open.session_id) {
            return Err(HarnessFailure::Uncertain);
        }
        if self.opens.len() >= MAX_PROCESSES {
            return Err(HarnessFailure::Unavailable);
        }
        let options = selected.options(
            instance,
            PathBuf::from(&open.working_directory),
            open.resume
                .as_ref()
                .map(|binding| binding.native_session_id.clone()),
        )?;
        if let Some(binding) = &open.resume {
            let version_matches = binding.native_version == selected.exact_version;
            let instance_matches = binding.instance_id == instance.instance_id;
            if !version_matches || !instance_matches {
                return Err(HarnessFailure::Conflict);
            }
        }
        if open.resume.is_none() {
            // Host session IDs are UUIDs. Reserve that exact UUID as the new native identity;
            // do not publish it as a binding until the native process confirms it.
            bcode_claude_code::validate_session_uuid(&open.session_id)
                .map_err(|_| HarnessFailure::InvalidRequest)?;
        }
        // This DTO is correlation, not proof of authorization: host admission must
        // replace/verify client input against both current owner scopes before dispatch.
        let admitted_assistance_grant = open.parent_assistance.clone();
        if let Some(grant) = &admitted_assistance_grant {
            grant
                .validate_for(open)
                .map_err(|_| HarnessFailure::StaleGeneration)?;
        }
        let sdk_servers = if admitted_assistance_grant.is_some() {
            vec![parent_bridge::SERVER.to_owned()]
        } else {
            Vec::new()
        };
        self.opens.insert(open.session_id.clone(), open.clone());
        let session = if open.resume.is_some() {
            ClaudeSession::spawn_with_sdk_servers(options, sdk_servers).await
        } else {
            ClaudeSession::spawn_new_with_sdk_servers(options, &open.session_id, sdk_servers).await
        }
        .map_err(|_| HarnessFailure::Uncertain)?;
        let PreparedStream {
            client,
            prepared,
            opened,
            stream,
        } = prepare_stream(
            &session,
            instance,
            open,
            selected,
            admitted_assistance_grant,
        )
        .await?;
        let _reader = tokio::spawn(drain_session(session, Arc::clone(&stream)));
        self.processes.insert(
            open.session_id.clone(),
            Process {
                instance: instance.clone(),
                open: open.clone(),
                prepared,
                client,
                stream,
                submissions: BTreeMap::new(),
                responses: BTreeMap::new(),
                controls: BTreeMap::new(),
                closed: false,
            },
        );
        Ok(opened)
    }
}
struct PreparedStream {
    client: ClaudeClient,
    prepared: HarnessPreparedSession,
    opened: HarnessServiceResponseKind,
    stream: Arc<Mutex<StreamState>>,
}

async fn prepare_stream(
    session: &ClaudeSession,
    instance: &HarnessInstance,
    open: &HarnessOpenRequest,
    selected: &InstanceConfig,
    admitted_assistance_grant: Option<bcode_harness_models::HarnessAssistanceGrant>,
) -> Result<PreparedStream, HarnessFailure> {
    let client = session.client();
    client
        .initialize()
        .await
        .map_err(|_| HarnessFailure::Uncertain)?;
    if !client.is_control_ready() {
        return Err(HarnessFailure::Uncertain);
    }
    let prepared = HarnessPreparedSession {
        open_id: open.open_id.clone(),
        session_id: open.session_id.clone(),
        instance_id: instance.instance_id.clone(),
        generation: open.generation,
        native_version: selected.exact_version.clone(),
        requested_native_session_id: Some(open.resume.as_ref().map_or_else(
            || open.session_id.clone(),
            |value| value.native_session_id.clone(),
        )),
    };
    prepared
        .validate()
        .map_err(|_| HarnessFailure::InvalidRequest)?;
    let mut normalizer = Normalizer::with_target(
        HarnessSessionTarget::Prepared(prepared.clone()),
        open.open_id.clone(),
    );
    match client.identity_state() {
        bcode_claude_code::IdentityState::Confirmed { native_session_id } => {
            // Open itself carries confirmed evidence; no duplicate promotion event.
            normalizer
                .confirmation(&native_session_id)
                .map_err(|_| HarnessFailure::Uncertain)?;
        }
        bcode_claude_code::IdentityState::Prepared {
            requested_session_id,
        } if Some(&requested_session_id) == prepared.requested_native_session_id.as_ref() => {}
        _ => return Err(HarnessFailure::Uncertain),
    }
    let opened = open_response(&normalizer.target);
    normalizer.assistance_grant = admitted_assistance_grant;
    let stream = Arc::new(Mutex::new(StreamState {
        acknowledged: None,
        normalizer,
        observations: VecDeque::new(),
        gap: false,
    }));
    Ok(PreparedStream {
        client,
        prepared,
        opened,
        stream,
    })
}

async fn drain_session(mut session: ClaudeSession, reader_state: Arc<Mutex<StreamState>>) {
    while let Ok(event) = session.next_event().await {
        if let Event::Callback {
            request_id,
            request,
        } = &event
        {
            let bridge_call = reader_state.lock().map_or(None, |stream| {
                parent_bridge::handle(
                    request_id,
                    request,
                    &stream.normalizer.target,
                    stream
                        .normalizer
                        .assistance_grant
                        .as_ref()
                        .map(|grant| grant.scope.as_str()),
                )
            });
            if let Some(parent_bridge::BridgeCall::Reply(response)) = bridge_call {
                let _ = session.client().respond(request_id, response).await;
                continue;
            }
        }
        let mut confirmation = Vec::new();
        if let bcode_claude_code::IdentityState::Confirmed { native_session_id } =
            session.client().identity_state()
        {
            let promoted = reader_state
                .lock()
                .map_or(Err(HarnessContractError::WrongOwner), |mut stream| {
                    stream.normalizer.confirmation(&native_session_id)
                });
            if let Ok(observations) = promoted {
                confirmation = observations;
            } else {
                session.client().terminate();
                break;
            }
        }
        let unsupported_callback = if let Event::Callback { request_id, .. } = &event {
            Some(request_id.clone())
        } else {
            None
        };
        let (accepted, reject_callback) =
            reader_state.lock().map_or((false, None), |mut stream| {
                let mut observations = confirmation;
                observations.extend(stream.normalizer.event(event));
                let reject =
                    unsupported_callback.filter(|id| !stream.normalizer.pending.contains_key(id));
                (stream.push(observations), reject)
            });
        if let Some(id) = reject_callback {
            let _ = session
                .client()
                .respond(&id, bcode_claude_code::CallbackResponse::Error)
                .await;
        }
        if !accepted {
            session.client().terminate();
            break;
        }
    }
    if let Ok(mut stream) = reader_state.lock() {
        stream.disconnected();
    }
    session.close().await;
}

fn open_response(target: &HarnessSessionTarget) -> HarnessServiceResponseKind {
    match target {
        HarnessSessionTarget::Prepared(value) => {
            HarnessServiceResponseKind::Prepared(value.clone())
        }
        HarnessSessionTarget::Confirmed(value) => HarnessServiceResponseKind::Opened(value.clone()),
    }
}
fn prepare_handoff(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.operation != NATIVE_HANDOFF_OPERATION
        || context.request.payload.len() > MAX_HARNESS_WIRE_BYTES
    {
        return ServiceResponse::error(
            "invalid_request",
            "Unsupported or oversized handoff request",
        );
    }
    let Ok(request) = context.request.payload_json::<NativeLoginHandoffRequest>() else {
        return ServiceResponse::error("invalid_request", "Invalid handoff request");
    };
    if request.schema_version != 1
        || request.instance.validate().is_err()
        || request.request_id.is_empty()
        || request.request_id.len() > MAX_HARNESS_ID_BYTES
        || request.request_id.trim() != request.request_id
        || request.request_id.chars().any(char::is_control)
    {
        return ServiceResponse::error("invalid_request", "Invalid handoff identity or version");
    }
    let Ok(config) = context.config::<Config>() else {
        return ServiceResponse::error("invalid_config", "Invalid native handoff configuration");
    };
    let Some(selected) = config.instances.get(&request.instance.instance_id) else {
        return ServiceResponse::error("unavailable", "Unknown native handoff instance");
    };
    let result = selected
        .options(&request.instance, selected.config_directory.clone(), None)
        .and_then(|options| native_login_handoff_data(&request.instance, &options));
    let Ok(handoff) = result else {
        return ServiceResponse::error(
            "policy",
            "Native handoff selection unsupported or mismatched",
        );
    };
    let response = NativeLoginHandoffResponse {
        schema_version: 1,
        request_id: request.request_id,
        handoff,
    };
    match serde_json::to_vec(&response) {
        Ok(payload) if payload.len() <= MAX_HARNESS_WIRE_BYTES => ServiceResponse::ok(payload),
        _ => ServiceResponse::error("bounds_exceeded", "Native handoff plan exceeds wire bounds"),
    }
}

fn reply(request: &HarnessRequest, kind: HarnessServiceResponseKind) -> ServiceResponse {
    let response = HarnessServiceResponse {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: request.request_id.clone(),
        operation: request.kind.operation(),
        kind,
    };
    if response.validate().is_err() {
        return ServiceResponse::error("invalid_response", "Harness response exceeds contract");
    }
    ServiceResponse::json(&response).unwrap_or_else(|_| {
        ServiceResponse::error("invalid_response", "Harness response encoding failed")
    })
}
/// Return the bundled registration using the same manifest as the native plugin.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(ClaudeCodePlugin, include_str!("../bcode-plugin.toml"))
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(ClaudeCodePlugin, include_str!("../bcode-plugin.toml"));
