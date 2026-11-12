#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Claude Code execution backend. The host owns admission and canonical persistence.
mod attribution;
mod auth;
mod candidates;
mod capabilities;
mod config;
mod diagnostics;
mod discovery;
mod discovery_service;
mod execution;
mod live;
mod model_selection;
mod native_connection;
mod normalize;
mod observation_wake;
mod parent_bridge;
mod recovery;
mod retention;
mod tool_bridge;
pub use attribution::RetainedAttribution;
pub use auth::{
    NATIVE_HANDOFF_INTERFACE_ID, NATIVE_HANDOFF_OPERATION, NativeLoginExecution,
    NativeLoginHandoffData, NativeLoginHandoffRequest, NativeLoginHandoffResponse,
    NativeLoginInactivity, NativeLoginMethodEvidence, native_login_handoff_data,
};
#[cfg(test)]
mod tests;

use bcode_claude_code::{ClaudeClient, ClaudeSession, Event, Settlement};
#[cfg(test)]
use bcode_harness_models::HarnessAuthCustody;
use bcode_harness_models::{
    HARNESS_INTERFACE_ID, HARNESS_SCHEMA_VERSION, HarnessAttemptIdentity, HarnessAttention,
    HarnessCapability, HarnessCompleteness, HarnessContractError, HarnessControlRequest,
    HarnessDeliveryState, HarnessFailure, HarnessInspection, HarnessInstance,
    HarnessInteractionKind, HarnessObservation, HarnessObservationKind, HarnessObservationPage,
    HarnessObserveRequest, HarnessOpenRequest, HarnessPlacement, HarnessPreparedSession,
    HarnessReceipt, HarnessReconcileRequest, HarnessReconciliation, HarnessRequest,
    HarnessRequestKind, HarnessResponse, HarnessServiceResponse, HarnessServiceResponseKind,
    HarnessSessionTarget, HarnessSubmission, HarnessSupportLevel, MAX_HARNESS_ID_BYTES,
    MAX_HARNESS_WIRE_BYTES,
};
use bcode_plugin_sdk::prelude::*;
pub use config::{ApiBilling, Config, InstanceConfig};
pub use discovery::{
    ClaudeCodeCandidate, ClaudeCodeCompatibility, ClaudeCodeDiscovery, ClaudeCodePreparationError,
    discover_candidates, discover_candidates_in,
};
pub use discovery_service::{
    CLAUDE_DISCOVERY_INTERFACE_ID, CLAUDE_DISCOVERY_OPERATION, ClaudeDiscoveryCandidate,
    ClaudeDiscoveryEvidence, ClaudeDiscoveryRequest, ClaudeDiscoveryResponse, ClaudeDiscoverySetup,
    MAX_DISCOVERY_CANDIDATES, MAX_DISCOVERY_PATH_BYTES,
};
use normalize::Normalizer;
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
};

const PLUGIN_ID: &str = "bcode.claude-code";
const MAX_PROCESSES: usize = 32;
const MAX_RETAINED_MUTATIONS: usize = 256;
const MAX_RETAINED_OBSERVATIONS: usize = 256;
/// Staged native records pumped per durable page cycle; execution evidence still
/// stops the batch until the host acknowledges it.
const MAX_PUMPED_RECORDS_PER_PAGE: usize = 256;
/// Upper bound on one blocking wait slice, so cancellation is observed promptly.
const OBSERVE_WAIT_SLICE: std::time::Duration = std::time::Duration::from_millis(100);
/// When only transient live text is ready, wait this long so nearby deltas share a page.
const LIVE_COALESCE: std::time::Duration = std::time::Duration::from_millis(40);
/// Vendor namespace for closed-vocabulary host-tool bridge telemetry observations.
const HOST_TOOLS_VENDOR_NAMESPACE: &str = "claude_code.host_tools";

struct StreamState {
    /// Signalled whenever native evidence arrives, so a waiting `observe` can return.
    wake: Arc<observation_wake::ObservationWake>,
    normalizer: Normalizer,
    observations: VecDeque<HarnessObservation>,
    gap: bool,
    acknowledged: Option<String>,
    retention: retention::PageRetention,
    retired_attempts: Vec<HarnessAttemptIdentity>,
}
impl StreamState {
    fn tool_diagnostic(&mut self, _identity: &str, diagnostic: diagnostics::Diagnostic) -> bool {
        // A bridge/request identity can recur across turns (bind, revoke, refresh).
        // Each emitted diagnostic is a distinct observation, not a mutation replay.
        // Host-tool bridge telemetry is operational evidence, not conversation content: it is
        // retained as closed-vocabulary vendor detail so it never becomes a transcript notice.
        let Some(observation) = self.normalizer.observation(HarnessObservationKind::Vendor {
            namespace: HOST_TOOLS_VENDOR_NAMESPACE.into(),
            detail: serde_json::json!({
                "code": diagnostic.code(),
                "message": diagnostic.message(),
            }),
        }) else {
            self.gap = true;
            return false;
        };
        self.push(vec![observation])
    }

    fn push(&mut self, observations: Vec<HarnessObservation>) -> bool {
        // Native evidence may be staged without producing observations yet; either way
        // the next page can differ, so wake any waiting observer.
        self.wake.notify();
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
        !self.gap
    }
}
impl StreamState {
    fn disconnected(&mut self) {
        self.gap = true;
        let evidence = self.normalizer.connection_lost();
        let _ = self.push(evidence);
    }

    /// Apply the request's acknowledgement and hydration, then pump staged native records into
    /// the unoffered queue. Repeating the same request is idempotent and offers nothing.
    fn advance(
        &mut self,
        prepared: &HarnessPreparedSession,
        observe: &HarnessObserveRequest,
    ) -> Result<(), HarnessFailure> {
        if !retained_target_matches(&self.normalizer.target, prepared, &observe.target) {
            return Err(HarnessFailure::StaleGeneration);
        }
        let acknowledged = self.retention.apply(observe.retention.as_ref())?;
        for observation in &acknowledged {
            if self.observations.front().map(|item| &item.observation_id)
                != Some(&observation.observation_id)
            {
                return Err(HarnessFailure::Conflict);
            }
            self.observations.pop_front();
            self.normalizer
                .acknowledge_execution(&observation.observation_id);
            self.normalizer.acknowledge_record(observation);
            self.acknowledged = Some(observation.observation_id.clone());
        }
        if !acknowledged.is_empty() {
            self.retired_attempts
                .extend(self.normalizer.retire_acknowledged());
        }
        if let Some(retention) = &observe.retention {
            self.normalizer
                .apply_hydration(&retention.hydration)
                .map_err(|_| HarnessFailure::Conflict)?;
        }
        if !self.retention.pending() {
            // Leave queue headroom: one native record can expand into several observations.
            let budget = usize::from(observe.limit)
                .min(MAX_RETAINED_OBSERVATIONS / 2)
                .saturating_sub(self.observations.len())
                .max(1);
            let staged = self
                .normalizer
                .pump_batch(budget, MAX_PUMPED_RECORDS_PER_PAGE);
            if !staged.is_empty() && !self.push(staged) {
                return Err(HarnessFailure::Uncertain);
            }
        }
        let (staged, unacknowledged, hydrating) = self.normalizer.pump_state();
        tracing::trace!(target: "bcode_claude_code_plugin", staged, unacknowledged, hydrating,
            retention_pending = self.retention.pending(), queued = self.observations.len(),
            acknowledged = acknowledged.len(), gap = self.gap, "observe advanced");
        Ok(())
    }

    /// Whether the next page carries evidence or a request the host must receive now.
    fn ready_to_offer(&self) -> bool {
        self.durable_ready() || self.normalizer.live_pending()
    }

    /// Ready for reasons other than transient live text.
    fn durable_ready(&self) -> bool {
        self.gap
            || self.retention.pending()
            || !self.observations.is_empty()
            || !self.normalizer.hydration_requests().is_empty()
    }

    fn observation_page(
        &mut self,
        prepared: &HarnessPreparedSession,
        observe: &HarnessObserveRequest,
        settlement: &Settlement,
    ) -> Result<HarnessServiceResponseKind, HarnessFailure> {
        self.advance(prepared, observe)?;
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
        let hydration_requests = self.normalizer.hydration_requests();
        // A page with nothing to transfer (idle waits and live-only streaming pages) carries
        // no retention page. Retaining it would force one canonical page commit per
        // coalesced live window; without retention identical empty pages commit as exact
        // retries. Any observation, pending page, gap or hydration request keeps the
        // exact post-commit retention protocol.
        let retain = !observations.is_empty()
            || self.retention.pending()
            || self.gap
            || !hydration_requests.is_empty();
        let (retention, observations) = if retain {
            let (retention, observations) = self.retention.offer(observations)?;
            (Some(retention), observations)
        } else {
            (None, observations)
        };
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
                // Transient: offered once, never retained and not covered by the digest.
                live: self.normalizer.take_live(),
                retention,
                hydration_requests,
                inventory_page: self.normalizer.inventory_page(),
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
    /// Configured expected version at launch; a later configuration change conflicts.
    /// The prepared target carries the actual probed native version.
    configured_version: String,
    /// Init-advertised native models; the only keys accepted by mid-session switching.
    models: Vec<bcode_claude_code::NativeModel>,
    client: ClaudeClient,
    stream: Arc<Mutex<StreamState>>,
    submissions: BTreeMap<String, (HarnessSubmission, HarnessReceipt)>,
    responses: BTreeMap<String, HarnessResponse>,
    controls: BTreeMap<String, &'static str>,
    released_close: Option<HarnessControlRequest>,
    closed: bool,
}
/// Native plugin with a persistent I/O runtime. No plugin-owned persistence or recovery journal.
///
/// Operations serialize on the retained state exactly as an exclusive plugin would; only an
/// `observe` with `wait_millis` waits for native output outside that lock.
#[derive(Default)]
pub struct ClaudeCodePlugin {
    state: Mutex<PluginState>,
}

#[derive(Default)]
pub(crate) struct PluginState {
    runtime: Option<tokio::runtime::Runtime>,
    processes: BTreeMap<String, Process>,
    /// Failed opens are uncertain and cannot be silently repeated during this runtime.
    opens: BTreeMap<String, HarnessOpenRequest>,
    /// Native-auth-required opens reserve identity without launching a conversation.
    auth_prepared: BTreeMap<String, (HarnessInstance, HarnessPreparedSession)>,
    model_selections: BTreeMap<String, model_selection::RetainedSelection>,
    auth: auth::AuthFlows,
    tool_queues: BTreeMap<String, Arc<Mutex<tool_bridge::Queue>>>,
}
impl PluginState {
    fn record_host_tools_status(&self, process: &Process, identity: String) -> Option<()> {
        let client = process.client.clone();
        let stream = Arc::clone(&process.stream);
        self.runtime.as_ref()?.spawn(async move {
            let status = client.host_tools_status().await.unwrap_or("unavailable");
            if let Ok(mut stream) = stream.lock() {
                let _ = stream.tool_diagnostic(
                    &identity,
                    diagnostics::Diagnostic::NativeStatus(diagnostics::Connection::from_label(
                        status,
                    )),
                );
                drop(stream);
            }
        });
        Some(())
    }

    fn refresh_host_tools(&self, process: &Process, identity: String) -> Option<()> {
        let client = process.client.clone();
        let stream = Arc::clone(&process.stream);
        let counts = client.sdk_metadata_counts();
        if let Ok(mut state) = stream.lock() {
            let _ = state.tool_diagnostic(
                &format!("{identity}-metadata-before-refresh"),
                diagnostics::Diagnostic::Metadata { counts },
            );
            let _ = state.tool_diagnostic(
                &format!("{identity}-metadata-unavailable"),
                diagnostics::Diagnostic::MetadataUnavailable,
            );
        }
        self.runtime.as_ref()?.spawn(async move {
            let outcome = client.refresh_sdk_catalogs_acknowledged().await;
            let counts = client.sdk_metadata_counts();
            if let Ok(mut state) = stream.lock() {
                let _ = state.tool_diagnostic(
                    &format!("{identity}-refresh"),
                    diagnostics::Diagnostic::Refresh(diagnostics::Refresh::classify(&outcome)),
                );
                let _ = state.tool_diagnostic(
                    &format!("{identity}-metadata"),
                    diagnostics::Diagnostic::Metadata { counts },
                );
            }
        });
        Some(())
    }

    fn complete_tool_queue(
        &self,
        scope: &bcode_harness_models::ManagedToolQueueScope,
        response: bcode_harness_models::ManagedToolResponse,
    ) -> Option<bcode_harness_models::ManagedToolQueueResponse> {
        let catalog_diagnostic = diagnostics::CatalogResponse::classify(&response.outcome);
        let diagnostic_id = response.request_id.clone();
        let completed = self
            .tool_queues
            .get(&scope.session_id)?
            .lock()
            .ok()?
            .complete(scope, response)
            .ok()?;
        if let Some((callback_id, response)) = completed {
            let process = self.processes.get(&scope.session_id)?;
            let client = process.client.clone();
            let stream = Arc::clone(&process.stream);
            self.runtime.as_ref()?.spawn(async move {
                let delivered = client.respond(&callback_id, response).await.is_ok();
                if let Some(message) = catalog_diagnostic {
                    let identity = format!("{diagnostic_id}-catalog");
                    if let Ok(mut stream) = stream.lock() {
                        let _ = stream.tool_diagnostic(
                            &identity,
                            diagnostics::Diagnostic::CatalogDelivery {
                                response: message,
                                written: delivered,
                            },
                        );
                        drop(stream);
                    }
                }
            });
        }
        Some(bcode_harness_models::ManagedToolQueueResponse::Completed)
    }

    fn tool_queue_service(&mut self, context: &NativeServiceContext) -> ServiceResponse {
        use bcode_harness_models::{
            ManagedToolQueueRequest as Request, ManagedToolQueueResponse as Response,
        };
        if context.request.payload.len() > bcode_harness_models::MAX_MANAGED_TOOL_MESSAGE_BYTES {
            return ServiceResponse::json(&Response::Rejected)
                .unwrap_or_else(|_| ServiceResponse::error("invalid_request", "Queue rejected"));
        }
        let result = (|| {
            let request: Request = context.request.payload_json().ok()?;
            if request.validate().is_err() || request.operation() != context.request.operation {
                return None;
            }
            let scope = request.scope().clone();
            match request {
                Request::Bind { .. } => {
                    // Only the host queue route admits this Bind. Existing inert SDK
                    // registration can activate after ordinary Open has prepared its target.
                    if let Some(process) = self.processes.get(&scope.session_id) {
                        let current = self.tool_queues.get(&scope.session_id)?;
                        if process.closed || process.open.generation != scope.generation {
                            return None;
                        }
                        let mut queue = current.lock().ok()?;
                        if queue.scope() == Some(&scope) {
                            return Some(Response::Bound);
                        }
                        // The host must have revoked the prior principal. This queue existed
                        // at process launch; a process restored without it cannot gain tools.
                        if queue.scope().is_some() || !queue.bind(scope.clone()) {
                            return None;
                        }
                        drop(queue);
                        // The task waits outside this exclusive service call and queue lock.
                        // Until catalog delivery, observations explicitly mark metadata unavailable.
                        self.refresh_host_tools(process, scope.bridge_id.clone())?;
                        self.record_host_tools_status(process, scope.bridge_id.clone())?;
                        return Some(Response::Bound);
                    }
                    self.tool_queues.retain(|id, queue| {
                        self.processes.contains_key(id)
                            || queue.lock().is_ok_and(|queue| queue.scope().is_some())
                    });
                    if self.tool_queues.len() >= 128 {
                        return None;
                    }
                    let queue = self
                        .tool_queues
                        .entry(scope.session_id.clone())
                        .or_default();
                    if !queue.lock().ok()?.bind(scope) {
                        return None;
                    }
                    Some(Response::Bound)
                }
                Request::Drain { .. } => {
                    if let Some(process) = self.processes.get(&scope.session_id)
                        && (process.closed || process.open.generation != scope.generation)
                    {
                        return None;
                    }
                    let requests = self
                        .tool_queues
                        .get(&scope.session_id)?
                        .lock()
                        .ok()?
                        .drain(&scope)?;
                    Some(Response::Requests { requests })
                }
                Request::Complete { response, .. } => self.complete_tool_queue(&scope, response),
                Request::Revoke { .. } => {
                    let replies = self
                        .tool_queues
                        .get(&scope.session_id)?
                        .lock()
                        .ok()?
                        .revoke(&scope)?;
                    if let Some(process) = self.processes.get(&scope.session_id) {
                        if let Ok(mut stream) = process.stream.lock() {
                            let _ = stream.tool_diagnostic(
                                &format!("{}-revoked", scope.bridge_id),
                                diagnostics::Diagnostic::QueueRevoked,
                            );
                        }
                        let client = process.client.clone();
                        self.runtime.as_ref()?.spawn(async move {
                            for (id, response) in replies {
                                let _ = client.respond(&id, response).await;
                            }
                        });
                    }
                    Some(Response::Revoked)
                }
            }
        })()
        .unwrap_or(Response::Rejected);
        ServiceResponse::json(&result)
            .unwrap_or_else(|_| ServiceResponse::error("invalid_request", "Queue rejected"))
    }
}

impl RustPlugin for ClaudeCodePlugin {
    fn register_auth_providers(&mut self, registrar: AuthRegistrar) -> Result<(), PluginError> {
        self.register_auth_providers_concurrent(registrar)
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        self.invoke_service_concurrent(context)
    }
}

impl ConcurrentRustPlugin for ClaudeCodePlugin {
    fn register_auth_providers_concurrent(
        &self,
        registrar: AuthRegistrar,
    ) -> Result<(), PluginError> {
        PluginState::register_auth_providers(registrar)
    }

    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
        if let Some((request, wait)) = observe_wait(&context) {
            self.wait_for_observations(&request, wait, &context.cancellation);
        }
        self.state.lock().map_or_else(
            |_| ServiceResponse::error("unavailable", "Claude harness state unavailable"),
            |mut state| state.invoke_service(&context),
        )
    }
}

impl ClaudeCodePlugin {
    /// Wait outside the state lock until the observed session has something to offer, the
    /// bounded wait elapses, or the invocation is cancelled. Native records that produce no
    /// observations are absorbed while waiting, so the page is not released early by them.
    fn wait_for_observations(
        &self,
        request: &HarnessRequest,
        wait: std::time::Duration,
        cancellation: &bcode_plugin_sdk::ServiceCancellation,
    ) {
        let HarnessRequestKind::Observe(observe) = &request.kind else {
            return;
        };
        let deadline = std::time::Instant::now() + wait;
        // End of the live coalescing window, fixed when live text first becomes ready.
        let mut coalesce_until: Option<std::time::Instant> = None;
        loop {
            let readiness = self
                .state
                .lock()
                .ok()
                .map(|state| state.advance_observation(&request.instance, observe));
            let (wake, seen) = match readiness {
                Some(ObservationReadiness::Waiting(wake, seen)) => (wake, seen),
                Some(ObservationReadiness::LiveOnly(wake, seen)) => {
                    // Let nearby deltas share one page for the whole window. Every native
                    // frame wakes this wait; the readiness check above returns early once
                    // durable evidence is ready or the live buffer nears its bounds.
                    let now = std::time::Instant::now();
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
                let now = std::time::Instant::now();
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

/// What a waiting `observe` should do next.
enum ObservationReadiness {
    /// Run the operation now: durable evidence is ready or a failure must be reported.
    Run,
    /// Only transient live text is ready; a short coalescing wait is allowed.
    LiveOnly(Arc<observation_wake::ObservationWake>, u64),
    /// Nothing is ready; wait for the native reader.
    Waiting(Arc<observation_wake::ObservationWake>, u64),
}

/// Bounded wait requested by a valid harness `observe`; other requests never wait.
fn observe_wait(context: &NativeServiceContext) -> Option<(HarnessRequest, std::time::Duration)> {
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
    let wait = std::time::Duration::from_millis(u64::from(observe.wait_millis));
    (!wait.is_zero()).then_some((request, wait))
}

impl PluginState {
    fn register_auth_providers(registrar: AuthRegistrar) -> Result<(), PluginError> {
        use bcode_provider_auth_models::{
            AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION, AuthMethodContribution,
            AuthProviderContribution, AuthSecretField,
        };
        registrar
            .register(&AuthProviderContribution {
                schema_version: AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
                provider_id: "claude-code".into(),
                display_name: "Claude Code".into(),
                methods: vec![AuthMethodContribution::SecretFields {
                    method_id: "api_key".into(),
                    display_name: "Anthropic API key".into(),
                    fields: vec![AuthSecretField {
                        credential_id: "api_key".into(),
                        storage_key: "ANTHROPIC_API_KEY".into(),
                        prompt: "Anthropic API key".into(),
                        optional: false,
                        validation: bcode_provider_auth_models::AuthSecretValidation::default(),
                        discovery_sources: Vec::new(),
                        invocation_env: Vec::new(),
                    }],
                    supports_verification: false,
                    supports_revocation: false,
                }],
            })
            .map_err(|_| PluginError::failed("Claude auth registration failed"))
    }

    pub(crate) fn invoke_service(&mut self, context: &NativeServiceContext) -> ServiceResponse {
        if context.plugin_id == PLUGIN_ID
            && context.request.interface_id == bcode_harness_models::HARNESS_CANDIDATES_INTERFACE_ID
        {
            return self.candidates_service(context);
        }
        if context.plugin_id == PLUGIN_ID
            && context.request.interface_id == bcode_harness_models::HARNESS_MODELS_INTERFACE_ID
        {
            return self.invoke_models_service(context);
        }
        if context.plugin_id == PLUGIN_ID
            && context.request.interface_id == bcode_harness_models::HARNESS_RECOVERY_INTERFACE_ID
        {
            return self.recovery_service(context);
        }
        if context.plugin_id == PLUGIN_ID
            && context.request.interface_id == bcode_harness_models::MANAGED_TOOL_QUEUE_INTERFACE_ID
        {
            return self.tool_queue_service(context);
        }
        if context.plugin_id == PLUGIN_ID
            && matches!(
                context.request.interface_id.as_str(),
                NATIVE_HANDOFF_INTERFACE_ID | bcode_harness_models::TERMINAL_AUTH_INTERFACE_ID
            )
        {
            return prepare_handoff(context);
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
        let configuration: Config = match context.config_or_default() {
            Ok(value) => value,
            Err(_) => {
                return ServiceResponse::error("invalid_config", "Invalid Claude configuration");
            }
        };
        let resolved;
        let selected = if let Some(selected) =
            configuration.instances.get(&request.instance.instance_id)
        {
            selected
        } else {
            resolved = match native_connection::resolve(&context, &request.instance) {
                Ok(value) => value,
                Err(error) => return reply(&request, HarnessServiceResponseKind::Failure(error)),
            };
            &resolved
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
        let result = runtime.block_on(self.invoke(&request, selected, context));
        self.runtime = Some(runtime);
        reply(
            &request,
            result.unwrap_or_else(HarnessServiceResponseKind::Failure),
        )
    }
}
impl ClaudeCodePlugin {
    /// Inspect bounded, sanitized producer evidence on an exact retained target.
    /// This is not canonical publication, native turn acceptance, settlement, or
    /// authority to execute/replay. Portable consumed/result observations are also
    /// emitted through normal observation delivery. Raw native payloads and
    /// prompt/result text are never returned by this inspection method.
    /// # Errors
    /// Unknown transport, instance mismatch, stale generation, or poisoned state.
    pub fn retained_attribution(
        &self,
        instance: &HarnessInstance,
        target: &HarnessSessionTarget,
    ) -> Result<Vec<RetainedAttribution>, HarnessFailure> {
        self.state
            .lock()
            .map_err(|_| HarnessFailure::Unavailable)?
            .retained_attribution(instance, target)
    }
}
impl PluginState {
    fn retained_attribution(
        &mut self,
        instance: &HarnessInstance,
        target: &HarnessSessionTarget,
    ) -> Result<Vec<RetainedAttribution>, HarnessFailure> {
        let process = self.retained_process(instance, target)?;
        let stream = process
            .stream
            .lock()
            .map_err(|_| HarnessFailure::Unavailable)?;
        Ok(stream.normalizer.retained_attribution().to_vec())
    }
    async fn invoke(
        &mut self,
        request: &HarnessRequest,
        selected: &InstanceConfig,
        context: &NativeServiceContext,
    ) -> Result<HarnessServiceResponseKind, HarnessFailure> {
        selected.credential_selection(&request.instance)?;
        if request.instance.executable != selected.executable.to_string_lossy()
            || request.instance.config_namespace != selected.config_directory.to_string_lossy()
        {
            return Err(HarnessFailure::Policy);
        }
        // A running process launched under a different configured version conflicts.
        let configured = selected.exact_version.as_str();
        if self
            .processes
            .values()
            .filter(|process| process.instance.instance_id == request.instance.instance_id)
            .any(|process| process.configured_version != configured)
        {
            return Err(HarnessFailure::Conflict);
        }
        match &request.kind {
            HarnessRequestKind::Inspect => {
                let options = selected.inspection_options(&request.instance)?;
                let mut capabilities = capabilities::capabilities(options.settlement_support);
                capabilities.authentication =
                    if options.billing_route == bcode_claude_code::BillingRoute::ApiKey {
                        capabilities::api_auth_capability()
                    } else {
                        native_auth_capability(options.auth_support)
                    };
                Ok(HarnessServiceResponseKind::Inspection(HarnessInspection {
                    native_version: selected.exact_version.clone(),
                    executable_identity: request.instance.executable.clone(),
                    config_namespace: request.instance.config_namespace.clone(),
                    capabilities,
                }))
            }
            HarnessRequestKind::Open(open) => {
                self.open(&request.instance, open, selected, context).await
            }
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
            HarnessRequestKind::Configure(configure) => {
                self.configure(&request.instance, configure).await
            }
            HarnessRequestKind::AuthStatus { .. }
            | HarnessRequestKind::AuthBegin(_)
            | HarnessRequestKind::AuthContinue(_)
            | HarnessRequestKind::AuthCancel(_) => {
                let options = selected.options_with_context(
                    &request.instance,
                    selected.config_directory.clone(),
                    None,
                    context,
                )?;
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
            // Never evict exact UUID/attempt ownership while this process can emit frames.
            return Err(HarnessFailure::Unavailable);
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
            && !process.client.can_submit_next_turn()
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
        let native_uuid = {
            let mut stream = process
                .stream
                .lock()
                .map_err(|_| HarnessFailure::Unavailable)?;
            let uuid = stream
                .normalizer
                .retain_submission(submission)
                .map_err(|_| HarnessFailure::Conflict)?;
            stream.normalizer.submitted = true;
            stream.normalizer.turn_id = None;
            uuid
        };
        process.submissions.insert(
            submission.submission_id.clone(),
            (submission.clone(), receipt.clone()),
        );
        // A completed write is not a native acceptance acknowledgement.
        let native_selection = match target {
            HarnessSessionTarget::Prepared(value) => value.requested_native_session_id.as_deref(),
            HarnessSessionTarget::Confirmed(value) => Some(value.native_session_id.as_str()),
        };
        receipt.state = if process
            .client
            .submit_with_uuid(&submission.text, native_selection, native_uuid)
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
        let page =
            stream.observation_page(&process.prepared, observe, &process.client.settlement())?;
        let retired = std::mem::take(&mut stream.retired_attempts);
        drop(stream);
        for attempt in retired {
            let uuid = bcode_claude_code::SubmissionUuid::new(attempt.attempt_id.clone())
                .map_err(|_| HarnessFailure::Uncertain)?;
            process.client.retire_acknowledged_submission(&uuid);
            if process
                .submissions
                .get(&attempt.submission_id)
                .is_some_and(|(submission, _)| submission.attempt_id == attempt.attempt_id)
            {
                process.submissions.remove(&attempt.submission_id);
            }
        }
        Ok(page)
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
            {
                let mut stream = process
                    .stream
                    .lock()
                    .map_err(|_| HarnessFailure::Unavailable)?;
                let withdrawn = stream.normalizer.invalidate_callbacks();
                // Offered on a later page if one is drained; overflow already marks a gap.
                let _ = stream.push(withdrawn);
            }
            process.client.terminate();
            // Signal/acknowledgement is not release. Only a successful supervisor OS
            // wait allows this process to become eligible for local capacity release.
            process
                .client
                .wait_reaped()
                .await
                .map_err(|_| HarnessFailure::Uncertain)?;
            process.released_close = Some(control.clone());
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
    /// Apply one mid-session native setting. Acknowledged only after the correlated native
    /// control response; a retained mutation identity is never re-sent.
    async fn configure(
        &mut self,
        instance: &HarnessInstance,
        configure: &bcode_harness_models::HarnessConfigureRequest,
    ) -> Result<HarnessServiceResponseKind, HarnessFailure> {
        use bcode_harness_models::{HarnessPermissionMode, HarnessSessionSetting};
        let process = self.process(instance, &configure.target)?;
        if process.controls.contains_key(&configure.mutation_id) {
            return Err(HarnessFailure::Uncertain);
        }
        if process.controls.len() >= MAX_RETAINED_MUTATIONS {
            return Err(HarnessFailure::Unavailable);
        }
        let result = match &configure.setting {
            HarnessSessionSetting::Model { model_id } => {
                if !process.models.iter().any(|model| &model.value == model_id) {
                    return Err(HarnessFailure::InvalidRequest);
                }
                if !process.client.can_submit_next_turn() {
                    // Never queue a switch behind an active foreground turn.
                    return Err(HarnessFailure::Conflict);
                }
                process
                    .controls
                    .insert(configure.mutation_id.clone(), "configure");
                process
                    .client
                    .select_model(&process.models, model_id)
                    .await
                    .map(|_| ())
            }
            HarnessSessionSetting::PermissionMode { mode } => {
                process
                    .controls
                    .insert(configure.mutation_id.clone(), "configure");
                process
                    .client
                    .set_permission_mode(match mode {
                        HarnessPermissionMode::Default => {
                            bcode_claude_code::PermissionMode::Default
                        }
                        HarnessPermissionMode::Plan => bcode_claude_code::PermissionMode::Plan,
                        HarnessPermissionMode::AcceptEdits => {
                            bcode_claude_code::PermissionMode::AcceptEdits
                        }
                    })
                    .await
            }
        };
        result.map_err(|error| configure_failure(&error))?;
        Ok(HarnessServiceResponseKind::Acknowledged {
            mutation_id: configure.mutation_id.clone(),
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
    /// Advance a retained session's observation state for a waiting `observe`. Returns the
    /// wake handle and current sequence while nothing durable is ready to offer; `Run`
    /// means the ordinary operation should run now (ready, or a failure it will report).
    fn advance_observation(
        &self,
        instance: &HarnessInstance,
        observe: &HarnessObserveRequest,
    ) -> ObservationReadiness {
        let Some(process) = self.processes.get(observe.target.session_id()) else {
            return ObservationReadiness::Run;
        };
        if process.instance != *instance || process.closed {
            return ObservationReadiness::Run;
        }
        let Ok(mut stream) = process.stream.lock() else {
            return ObservationReadiness::Run;
        };
        if stream.advance(&process.prepared, observe).is_err()
            || stream.durable_ready()
            || stream.normalizer.live_near_capacity()
        {
            return ObservationReadiness::Run;
        }
        let wake = Arc::clone(&stream.wake);
        let seen = wake.sequence();
        if stream.ready_to_offer() {
            ObservationReadiness::LiveOnly(wake, seen)
        } else {
            ObservationReadiness::Waiting(wake, seen)
        }
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
    fn release_closed_processes(&mut self) {
        let closed: Vec<_> = self
            .processes
            .iter()
            .filter(|(_, process)| process.closed)
            .map(|(id, _)| id.clone())
            .collect();
        for id in closed {
            self.processes.remove(&id);
            self.opens.remove(&id);
            self.model_selections.remove(&id);
            self.tool_queues.remove(&id);
        }
    }

    async fn open(
        &mut self,
        instance: &HarnessInstance,
        open: &HarnessOpenRequest,
        selected: &InstanceConfig,
        context: &NativeServiceContext,
    ) -> Result<HarnessServiceResponseKind, HarnessFailure> {
        self.open_with_model(instance, open, selected, context, None)
            .await
    }

    fn retained_open_response(
        &self,
        instance: &HarnessInstance,
        open: &HarnessOpenRequest,
    ) -> Result<Option<HarnessServiceResponseKind>, HarnessFailure> {
        if let Some(process) = self.processes.get(&open.session_id) {
            if process.closed || !process.client.is_control_ready() {
                return Err(HarnessFailure::Uncertain);
            }
            if process.instance != *instance || process.open != *open {
                return Err(HarnessFailure::Conflict);
            }
            return Ok(Some(open_response(
                &process
                    .stream
                    .lock()
                    .map_err(|_| HarnessFailure::Unavailable)?
                    .normalizer
                    .target,
            )));
        }
        if let Some((retained_instance, prepared)) = self.auth_prepared.get(&open.session_id) {
            if retained_instance != instance || self.opens.get(&open.session_id) != Some(open) {
                return Err(HarnessFailure::Conflict);
            }
            return Ok(Some(HarnessServiceResponseKind::Prepared(prepared.clone())));
        }
        Ok(None)
    }

    // Reserve a host identity for managed auth without claiming a native conversation exists.
    async fn prepare_auth_only(
        &mut self,
        instance: &HarnessInstance,
        open: &HarnessOpenRequest,
        options: &bcode_claude_code::LaunchOptions,
    ) -> Result<Option<HarnessPreparedSession>, HarnessFailure> {
        let status = bcode_claude_code::auth_status(options)
            .await
            .map_err(|_| HarnessFailure::Unavailable)?;
        if status.logged_in {
            return Ok(None);
        }
        let native_version = bcode_claude_code::installed_version(
            &options.executable,
            &options.cwd,
            &options.environment,
        )
        .await
        .map_err(|_| HarnessFailure::Unavailable)?;
        let prepared = HarnessPreparedSession {
            open_id: open.open_id.clone(),
            session_id: open.session_id.clone(),
            instance_id: instance.instance_id.clone(),
            generation: open.generation,
            native_version,
            requested_native_session_id: Some(open.session_id.clone()),
        };
        prepared
            .validate()
            .map_err(|_| HarnessFailure::InvalidRequest)?;
        self.opens.insert(open.session_id.clone(), open.clone());
        self.auth_prepared.insert(
            open.session_id.clone(),
            (instance.clone(), prepared.clone()),
        );
        Ok(Some(prepared))
    }

    fn retain_open_selection(
        &mut self,
        open: &HarnessOpenRequest,
        model: Option<&bcode_harness_models::HarnessModelSelection>,
    ) {
        self.opens.insert(open.session_id.clone(), open.clone());
        if let Some(model) = model {
            self.model_selections.insert(
                open.session_id.clone(),
                model_selection::RetainedSelection {
                    selection: model.clone(),
                    evidence: None,
                },
            );
        }
    }

    async fn open_with_model(
        &mut self,
        instance: &HarnessInstance,
        open: &HarnessOpenRequest,
        selected: &InstanceConfig,
        context: &NativeServiceContext,
        model: Option<&bcode_harness_models::HarnessModelSelection>,
    ) -> Result<HarnessServiceResponseKind, HarnessFailure> {
        if (self.opens.contains_key(&open.session_id)
            || self.processes.contains_key(&open.session_id))
            && !model_selection::same_selection(self.model_selections.get(&open.session_id), model)
        {
            return Err(HarnessFailure::Conflict);
        }
        if let Some(response) = self.retained_open_response(instance, open)? {
            return Ok(response);
        }
        // Closed instances release process capacity. Host admission must fence old canonical
        // open/submission identities across eviction and plugin restart.
        self.release_closed_processes();
        if self.opens.contains_key(&open.session_id) {
            return Err(HarnessFailure::Uncertain);
        }
        if self.opens.len() >= MAX_PROCESSES {
            return Err(HarnessFailure::Unavailable);
        }
        validate_open_selection(instance, open, selected)?;
        let options = selected.options_with_context(
            instance,
            PathBuf::from(&open.working_directory),
            open.resume
                .as_ref()
                .map(|binding| binding.native_session_id.clone()),
            context,
        )?;
        if model.is_none()
            && open.resume.is_none()
            && let Some(prepared) = self.prepare_auth_only(instance, open, &options).await?
        {
            return Ok(HarnessServiceResponseKind::Prepared(prepared));
        }
        // This DTO is correlation, not proof of authorization: host admission must
        // replace/verify client input against both current owner scopes before dispatch.
        let admitted_assistance_grant = open.parent_assistance.clone();
        if let Some(grant) = &admitted_assistance_grant {
            grant
                .validate_for(open)
                .map_err(|_| HarnessFailure::StaleGeneration)?;
        }
        // The SDK server is inert until an explicit host Bind. Keeping the queue object
        // from launch permits a later authorized workflow receipt to bootstrap it without
        // recreating the native process or inferring authority from its arguments.
        let tool_queue = Some(Arc::clone(
            self.tool_queues.entry(open.session_id.clone()).or_default(),
        ));
        let mut sdk_servers = if admitted_assistance_grant.is_some() {
            vec![parent_bridge::SERVER.to_owned()]
        } else {
            Vec::new()
        };
        if tool_queue.is_some() {
            sdk_servers.push(tool_bridge::SERVER.to_owned());
        }
        self.retain_open_selection(open, model);
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
            model_evidence,
            models,
        } = prepare_stream(
            &session,
            instance,
            open,
            selected,
            admitted_assistance_grant,
            model,
        )
        .await?;
        if let Some(retained) = self.model_selections.get_mut(&open.session_id) {
            retained.evidence = model_evidence;
        }
        let _reader = tokio::spawn(drain_session(session, Arc::clone(&stream), tool_queue));
        self.processes.insert(
            open.session_id.clone(),
            Process {
                instance: instance.clone(),
                open: open.clone(),
                prepared,
                configured_version: selected.exact_version.clone(),
                models,
                client,
                stream,
                submissions: BTreeMap::new(),
                responses: BTreeMap::new(),
                controls: BTreeMap::new(),
                released_close: None,
                closed: false,
            },
        );
        Ok(opened)
    }
}
fn validate_open_selection(
    instance: &HarnessInstance,
    open: &HarnessOpenRequest,
    selected: &InstanceConfig,
) -> Result<(), HarnessFailure> {
    if let Some(binding) = &open.resume {
        recovery::validate_resume(instance, open, selected)?;
        if !recovery::resumable_version(&binding.native_version, selected)
            || binding.instance_id != instance.instance_id
        {
            return Err(HarnessFailure::Conflict);
        }
    } else {
        // Reserve the exact host UUID; only native confirmation establishes binding.
        bcode_claude_code::validate_session_uuid(&open.session_id)
            .map_err(|_| HarnessFailure::InvalidRequest)?;
    }
    Ok(())
}

fn native_auth_capability(support: bcode_claude_code::NativeAuthSupport) -> HarnessCapability {
    use bcode_claude_code::NativeAuthSupport;
    match support {
        NativeAuthSupport::VerifiedStatusAndLogin => HarnessCapability {
            support: HarnessSupportLevel::Constrained,
            constraints: vec![
                "Native account namespace only; explicit supported CLI login handoff, no invented auth RPC or API-key extraction".into(),
            ],
        },
        NativeAuthSupport::VerifiedStatus => HarnessCapability {
            support: HarnessSupportLevel::Constrained,
            constraints: vec![
                "Exact-version local auth status only; native login handoff is unsupported".into(),
            ],
        },
        NativeAuthSupport::Unknown => HarnessCapability {
            support: HarnessSupportLevel::Unsupported,
            constraints: vec![
                "Exact native auth status/login support has not been verified".into(),
            ],
        },
    }
}

struct PreparedStream {
    model_evidence: Option<bcode_harness_models::HarnessModelEvidence>,
    models: Vec<bcode_claude_code::NativeModel>,
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
    model: Option<&bcode_harness_models::HarnessModelSelection>,
) -> Result<PreparedStream, HarnessFailure> {
    let client = session.client();
    let initialize = client
        .initialize()
        .await
        .map_err(|_| HarnessFailure::Uncertain)?;
    let model_evidence = if let Some(model) = model {
        let evidence = client
            .select_model_before_prompt(&initialize, &model.model_id)
            .await
            .map_err(|_| HarnessFailure::Uncertain)?;
        Some(bcode_harness_models::HarnessModelEvidence {
            requested_model_id: evidence.requested,
            effective_model_id: evidence.effective,
        })
    } else {
        None
    };
    if !client.is_control_ready() {
        return Err(HarnessFailure::Uncertain);
    }
    // An unusable model snapshot only disables mid-session switching; it never blocks open.
    let models = bcode_claude_code::available_models(&initialize).unwrap_or_default();
    // Bindings carry the actual probed executable version, which may drift from the
    // configured expected version. Tested-version membership gates verified features.
    let native_version = client.native_version().to_owned();
    let prepared = HarnessPreparedSession {
        open_id: open.open_id.clone(),
        session_id: open.session_id.clone(),
        instance_id: instance.instance_id.clone(),
        generation: open.generation,
        native_version: native_version.clone(),
        requested_native_session_id: Some(open.resume.as_ref().map_or_else(
            || open.session_id.clone(),
            |value| value.native_session_id.clone(),
        )),
    };
    prepared
        .validate()
        .map_err(|_| HarnessFailure::InvalidRequest)?;
    // An exact resume reopens the host-verified binding: `--resume=<id>` pins the native
    // session, so the open answers with that binding (at the new generation) instead of a
    // prepared target. Later native init must then name the same session or fail closed.
    // The binding keeps its recorded native version; the probed version only gates
    // verified features below.
    let initial_target = open.resume.as_ref().map_or_else(
        || HarnessSessionTarget::Prepared(prepared.clone()),
        |binding| HarnessSessionTarget::Confirmed(binding.clone()),
    );
    let mut normalizer = Normalizer::with_target(initial_target, open.open_id.clone());
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
    normalizer.configure_execution(
        selected.verified_session_state && bcode_claude_code::is_tested_version(&native_version),
        open.resume.is_none(),
    );
    normalizer.enable_retention();
    normalizer.assistance_grant = admitted_assistance_grant;
    let stream = Arc::new(Mutex::new(StreamState {
        wake: Arc::default(),
        retention: retention::PageRetention::new(format!(
            "claude:{}:{}:{}",
            std::process::id(),
            open.generation,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| HarnessFailure::Unavailable)?
                .as_nanos()
        )),
        acknowledged: None,
        retired_attempts: Vec::new(),
        normalizer,
        observations: VecDeque::new(),
        gap: false,
    }));
    Ok(PreparedStream {
        model_evidence,
        models,
        client,
        prepared,
        opened,
        stream,
    })
}

fn native_init_host_tools_status(value: &Value) -> &'static str {
    let Some(servers) = value.get("mcp_servers").and_then(Value::as_array) else {
        return "unknown";
    };
    if servers.len() > 256 {
        return "unknown";
    }
    let mut selected = servers
        .iter()
        .filter(|server| server["name"] == tool_bridge::SERVER);
    let Some(server) = selected.next() else {
        return "missing";
    };
    if selected.next().is_some() {
        return "unknown";
    }
    match server["status"].as_str() {
        Some("connected") => "connected",
        Some("failed") => "failed",
        Some("pending") => "pending",
        Some("disabled") => "disabled",
        Some("needs-auth") => "needs_auth",
        _ => "unknown",
    }
}

#[cfg(test)]
mod host_tools_diagnostic_tests {
    use super::*;

    #[test]
    fn recurring_bridge_diagnostics_have_distinct_canonical_observation_ids() {
        let prepared = HarnessPreparedSession {
            open_id: "open".into(),
            session_id: "session".into(),
            instance_id: "instance".into(),
            generation: 1,
            native_version: "tested-version".into(),
            requested_native_session_id: None,
        };
        let mut stream = StreamState {
            wake: Arc::default(),
            normalizer: Normalizer::with_target(
                HarnessSessionTarget::Prepared(prepared),
                "open".into(),
            ),
            observations: VecDeque::new(),
            gap: false,
            acknowledged: None,
            retention: retention::PageRetention::new("diagnostic-test".into()),
            retired_attempts: Vec::new(),
        };
        for _ in 0..2 {
            assert!(stream.tool_diagnostic("same-bridge", diagnostics::Diagnostic::QueueRevoked));
        }
        assert_ne!(
            stream.observations[0].observation_id,
            stream.observations[1].observation_id
        );
        assert_eq!(stream.observations[0].kind, stream.observations[1].kind);
        // Bridge telemetry is retained evidence, never a user-facing diagnostic notice.
        assert!(matches!(
            &stream.observations[0].kind,
            HarnessObservationKind::Vendor { detail, .. }
                if detail["code"] == "host_tools_queue_revoked"
        ));
    }

    #[test]
    fn system_init_status_is_sanitized_and_does_not_guess_unknowns() {
        assert_eq!(
            native_init_host_tools_status(&serde_json::json!({"mcp_servers":[{
                "name":"bcode-tools", "status":"connected", "error":"SECRET", "config":{"url":"SECRET"},
            }]})),
            "connected"
        );
        assert_eq!(
            native_init_host_tools_status(&serde_json::json!({"mcp_servers":[]})),
            "missing"
        );
        assert_eq!(
            native_init_host_tools_status(&serde_json::json!({"mcp_servers":[{
                "name":"bcode-tools", "status":"future-status",
            }]})),
            "unknown"
        );
        assert_eq!(
            native_init_host_tools_status(&serde_json::json!({})),
            "unknown"
        );
    }
}

async fn handle_tool_callback(
    client: &ClaudeClient,
    reader_state: &Mutex<StreamState>,
    queue: Option<&Arc<Mutex<tool_bridge::Queue>>>,
    request_id: &str,
    request: &bcode_claude_code::CallbackRequest,
) -> bool {
    let Some(queue) = queue else {
        return false;
    };
    let (handled, disposition) = queue.lock().map_or_else(
        |_| (None, diagnostics::CallbackDisposition::Unavailable),
        |mut queue| {
            let bound = queue.scope().is_some();
            let action = queue.callback(request_id, request);
            let disposition = diagnostics::CallbackDisposition::classify(bound, action.as_ref());
            (action, disposition)
        },
    );
    if matches!(request, bcode_claude_code::CallbackRequest::McpMessage { server_name, message }
                    if server_name == "bcode-tools" && message["method"] == "tools/list")
        && let Ok(mut stream) = reader_state.lock()
    {
        let _ = stream.tool_diagnostic(
            &format!("{request_id}-disposition"),
            diagnostics::Diagnostic::Callback(disposition),
        );
    }
    let Some(action) = handled else {
        return false;
    };
    if let tool_bridge::CallbackAction::Reply(response) = action {
        let _ = client.respond(request_id, response).await;
    }
    true
}

async fn normalize_session_event(
    client: &bcode_claude_code::ClaudeClient,
    reader_state: &Arc<Mutex<StreamState>>,
    event: Event,
) -> bool {
    let mut confirmation = Vec::new();
    if let bcode_claude_code::IdentityState::Confirmed { native_session_id } =
        client.identity_state()
    {
        let promoted = reader_state
            .lock()
            .map_or(Err(HarnessContractError::WrongOwner), |mut stream| {
                stream.normalizer.confirmation(&native_session_id)
            });
        if let Ok(observations) = promoted {
            confirmation = observations;
        } else {
            client.terminate();
            return false;
        }
    }
    let unsupported_callback = if let Event::Callback { request_id, .. } = &event {
        Some(request_id.clone())
    } else {
        None
    };
    let (accepted, reject_callback) = reader_state.lock().map_or((false, None), |mut stream| {
        let mut observations = confirmation;
        observations.extend(stream.normalizer.event(event));
        let reject = unsupported_callback.filter(|id| !stream.normalizer.pending.contains_key(id));
        (stream.push(observations), reject)
    });
    if let Some(id) = reject_callback {
        let _ = client
            .respond(&id, bcode_claude_code::CallbackResponse::Error)
            .await;
    }
    if !accepted {
        client.terminate();
        return false;
    }
    true
}

async fn drain_session(
    mut session: ClaudeSession,
    reader_state: Arc<Mutex<StreamState>>,
    tool_queue: Option<Arc<Mutex<tool_bridge::Queue>>>,
) {
    while let Ok(event) = session.next_event().await {
        if let Event::Message(bcode_claude_code::NativeMessage::System(value)) = &event
            && value["subtype"] == "init"
            && let Ok(mut stream) = reader_state.lock()
        {
            let status = native_init_host_tools_status(value);
            let _ = stream.tool_diagnostic(
                "native-init",
                diagnostics::Diagnostic::SystemInit(diagnostics::Connection::from_label(status)),
            );
            drop(stream);
        }
        if let Event::Callback {
            request_id,
            request,
        } = &event
        {
            if matches!(request, bcode_claude_code::CallbackRequest::McpMessage { server_name, message }
                if server_name == "bcode-tools" && message["method"] == "tools/list")
                && let Ok(mut stream) = reader_state.lock()
            {
                let _ = stream.tool_diagnostic(
                    &format!("{request_id}-catalog-request"),
                    diagnostics::Diagnostic::Metadata {
                        counts: session.client().sdk_metadata_counts(),
                    },
                );
            }
            if handle_tool_callback(
                &session.client(),
                &reader_state,
                tool_queue.as_ref(),
                request_id,
                request,
            )
            .await
            {
                continue;
            }
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
        if !normalize_session_event(&session.client(), &reader_state, event).await {
            break;
        }
    }
    if let Some(queue) = &tool_queue
        && let Ok(mut queue) = queue.lock()
        && let Some(scope) = queue.scope().cloned()
    {
        let _ = queue.revoke(&scope);
    }
    if let Ok(mut stream) = reader_state.lock() {
        stream.disconnected();
    }
    session.close().await;
}

/// Map a native control failure. Admission refusals happen before any native write; a
/// native error or mismatched acknowledgment is reported as uncertain and never retried.
const fn configure_failure(error: &bcode_claude_code::Error) -> HarnessFailure {
    match error {
        bcode_claude_code::Error::InvalidConfiguration(_) => HarnessFailure::Conflict,
        bcode_claude_code::Error::Capacity => HarnessFailure::Unavailable,
        _ => HarnessFailure::Uncertain,
    }
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
    let Ok(config) = context.config_or_default::<Config>() else {
        return ServiceResponse::error("invalid_config", "Invalid native handoff configuration");
    };
    let resolved;
    let selected = if let Some(selected) = config.instances.get(&request.instance.instance_id) {
        selected
    } else {
        resolved = match native_connection::resolve(context, &request.instance) {
            Ok(value) => value,
            Err(_) => {
                return ServiceResponse::error("unavailable", "Native connection unavailable");
            }
        };
        &resolved
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
    bcode_plugin_sdk::static_concurrent_plugin_vtable!(
        ClaudeCodePlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_concurrent_plugin!(ClaudeCodePlugin, include_str!("../bcode-plugin.toml"));
