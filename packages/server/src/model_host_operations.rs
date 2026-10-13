//! Admitted plugin model-host adapter. This module owns provider routing and process-local
//! lifecycle retention, not OIDC, subscriptions, or caller policy interpretation.
use super::*;
use bcode_model::host_operation as wire;
#[cfg(target_os = "linux")]
use bcode_provider_auth::operations::AuthRequestCustody;
use serde::Deserialize;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// Explicit admission for one integration and one administrator-selected model profile.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admission {
    /// Canonical enabled plugin owner admitted to this capability.
    pub allowed_plugin_id: String,
    /// Fixed configured profile; request payloads cannot override it.
    pub model_profile: String,
    /// Operator confirmation that the selected account supports this use.
    pub supported_account_use: bool,
    /// Maximum retained turns, including idempotency tombstones.
    pub max_turns: usize,
    /// Maximum scope lifetime in seconds (default 900, hard maximum 900).
    #[serde(default = "default_scope_seconds")]
    pub max_scope_seconds: u64,
    /// Optional retained Linux age custody. No fallback to native/ambient custody.
    #[serde(default)]
    pub custody: Option<CustodyConfig>,
}

/// Non-secret retained custody locators, owned by host administration.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustodyConfig {
    /// Registered auth provider identity, checked against the fixed model's plugin owner.
    #[cfg(target_os = "linux")]
    pub provider_id: String,
    /// Existing exclusively owned custody directory.
    #[cfg(target_os = "linux")]
    pub directory: PathBuf,
    /// Owner-only age identity file, never delivered to plugins.
    #[cfg(target_os = "linux")]
    pub identity_file: PathBuf,
    /// Preserve configured locators solely to reject custody on unsupported platforms.
    #[cfg(not(target_os = "linux"))]
    #[serde(flatten)]
    _unsupported: BTreeMap<String, serde_json::Value>,
}

struct PreparedAuth {
    context: bcode_model::ProviderRequestContext,
    request_custody: Option<Arc<dyn bcode_provider_auth::operations::AuthRequestCustody>>,
    credential_custody: Option<Arc<dyn bcode_provider_auth::operations::AuthCredentialCustody>>,
}

struct PreparedTurn {
    request: ModelTurnRequest,
    route: String,
    credential_custody: Option<Arc<dyn bcode_provider_auth::operations::AuthCredentialCustody>>,
}

struct Turn {
    caller: String,
    invocation: String,
    scope: String,
    expires_at: u64,
    request: wire::StartRequest,
    cancel: AtomicBool,
    inner: Mutex<TurnState>,
}

#[derive(Default)]
struct TurnState {
    events: VecDeque<wire::SequencedEvent>,
    bytes: usize,
    sequence: u64,
    terminal: bool,
    cleaned: bool,
    completed: bool,
    failure: Option<&'static str>,
}

/// Daemon-owned, bounded lifecycle store. Construct once and share across bridge calls.
pub struct ModelHostOperations {
    config: bcode_config::BcodeConfig,
    admission: Admission,
    turns: Mutex<BTreeMap<String, Arc<Turn>>>,
    stopping: AtomicBool,
    server: OnceLock<Weak<ServerState>>,
    #[cfg(target_os = "linux")]
    custody: StdMutex<Option<Arc<bcode_provider_auth::operations::RetainedAuthRequestCustody>>>,
}

impl std::fmt::Debug for ModelHostOperations {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelHostOperations")
            .finish_non_exhaustive()
    }
}

const fn default_scope_seconds() -> u64 {
    900
}

const MAX_EVENTS: usize = 256;
const MAX_EVENT_BYTES: usize = 262_144;
const RETENTION_SECONDS: u64 = 60;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs())
}

fn failed(code: &str) -> ToolInvocationServiceResolution {
    ToolInvocationServiceResolution::Failed {
        code: code.to_owned(),
        message: "model host operation was not admitted or could not complete".to_owned(),
    }
}

fn responded(value: &impl serde::Serialize) -> ToolInvocationServiceResolution {
    serde_json::to_value(value).map_or_else(
        |_| failed("response_encoding_failed"),
        |payload| ToolInvocationServiceResolution::Responded { payload },
    )
}

impl ModelHostOperations {
    /// Construct a bounded adapter using the same configuration used for auth materialization.
    #[must_use]
    pub fn new(config: bcode_config::BcodeConfig, admission: Admission) -> Self {
        Self {
            config,
            admission,
            turns: Mutex::new(BTreeMap::new()),
            stopping: AtomicBool::new(false),
            server: OnceLock::new(),
            #[cfg(target_os = "linux")]
            custody: StdMutex::new(None),
        }
    }

    /// Bind the daemon after its Arc is constructed, without retaining the daemon.
    pub fn bind_server(&self, state: &Arc<ServerState>) {
        let _ = self.server.set(Arc::downgrade(state));
    }

    /// Read opt-in admission from existing plugin configuration; malformed/missing disables it.
    #[must_use]
    pub fn from_config(config: &bcode_config::BcodeConfig) -> Self {
        let admission = config
            .plugins
            .config
            .get("bcode.auth-broker")
            .and_then(|table| table.get("model_host"))
            .and_then(|value| value.clone().try_into::<Admission>().ok())
            .unwrap_or(Admission {
                allowed_plugin_id: String::new(),
                model_profile: String::new(),
                supported_account_use: false,
                max_turns: 0,
                max_scope_seconds: 0,
                custody: None,
            });
        Self::new(config.clone(), admission)
    }

    /// Whether this explicit caller may own a global model-host bridge.
    #[must_use]
    pub fn admits(&self, caller: &str) -> bool {
        self.admission.supported_account_use
            && self.admission.allowed_plugin_id == caller
            && !self.admission.model_profile.is_empty()
            && self.admission.model_profile.len() <= 128
            && (1..=1024).contains(&self.admission.max_turns)
            && (1..=900).contains(&self.admission.max_scope_seconds)
            && self
                .config
                .resolved_model_profile(&self.admission.model_profile)
                .is_some()
            && !self.stopping.load(Ordering::Acquire)
    }

    /// Dispatch a normalized request. `caller` MUST come from the authenticated outer invocation.
    /// The caller's outer invocation ID must be validated before entering this operation.
    pub async fn dispatch(
        self: Arc<Self>,
        caller: &str,
        request: bcode_tool::ToolInvocationServiceRequest,
    ) -> ToolInvocationServiceResolution {
        if request.interface_id != wire::INTERFACE_ID {
            return ToolInvocationServiceResolution::Unsupported;
        }
        if caller != self.admission.allowed_plugin_id
            || !self.admission.supported_account_use
            || self.admission.model_profile.is_empty()
            || self.admission.model_profile.len() > 128
            || !(1..=1024).contains(&self.admission.max_turns)
            || !(1..=900).contains(&self.admission.max_scope_seconds)
        {
            return failed("model_host_not_admitted");
        }
        match request.operation.as_str() {
            wire::OP_START => {
                let Ok(input) = serde_json::from_value::<wire::StartRequest>(request.payload)
                else {
                    return failed("invalid_request");
                };
                let Some(state) = self.server.get().and_then(Weak::upgrade) else {
                    return failed("model_host_unavailable");
                };
                self.start(state, caller, &request.invocation_id, input)
                    .await
            }
            wire::OP_POLL => {
                let Ok(input) = serde_json::from_value::<wire::PollRequest>(request.payload) else {
                    return failed("invalid_request");
                };
                self.poll(caller, &request.invocation_id, input).await
            }
            wire::OP_CANCEL | wire::OP_FINISH => {
                let Ok(input) = serde_json::from_value::<wire::TurnRequest>(request.payload) else {
                    return failed("invalid_request");
                };
                let Some(turn) = self
                    .owned(
                        caller,
                        &request.invocation_id,
                        &input.scope_id,
                        &input.turn_id,
                    )
                    .await
                else {
                    return failed("turn_unavailable");
                };
                turn.cancel.store(true, Ordering::Release);
                if request.operation == wire::OP_FINISH && !turn.inner.lock().await.cleaned {
                    return failed("cleanup_pending");
                }
                responded(&bcode_model::AckResponse::default())
            }
            _ => ToolInvocationServiceResolution::Unsupported,
        }
    }

    async fn start(
        self: Arc<Self>,
        state: Arc<ServerState>,
        caller: &str,
        invocation: &str,
        input: wire::StartRequest,
    ) -> ToolInvocationServiceResolution {
        if self.stopping.load(Ordering::Acquire) || !valid_start(&input, &self.admission, now()) {
            return failed("invalid_request");
        }
        let mut turns = self.turns.lock().await;
        // Serialize shutdown admission with reservation. An invocation that waited for
        // this lock must not spawn work after shutdown took its worker snapshot.
        if self.stopping.load(Ordering::Acquire) {
            return failed("model_host_stopping");
        }
        for (id, turn) in turns.iter() {
            if let Some(result) = reserve_match(turn, caller, invocation, &input) {
                return match result {
                    Ok(()) => responded(&wire::StartResponse {
                        turn_id: id.clone(),
                    }),
                    Err(code) => failed(code),
                };
            }
        }
        if turns.len() >= self.admission.max_turns {
            return failed("capacity_exhausted");
        }
        let id = uuid::Uuid::new_v4().to_string();
        let turn = Arc::new(Turn {
            caller: caller.to_owned(),
            invocation: invocation.to_owned(),
            scope: input.scope_id.clone(),
            expires_at: input.expires_at,
            request: input,
            cancel: AtomicBool::new(false),
            inner: Mutex::new(TurnState::default()),
        });
        turns.insert(id.clone(), Arc::clone(&turn));
        drop(turns);
        let host = Arc::clone(&self);
        let task_id = id.clone();
        // Reservation is committed before any provider work. Disconnects cannot replay start.
        tokio::spawn(async move {
            host.run(state, &task_id, turn).await;
        });
        responded(&wire::StartResponse { turn_id: id })
    }

    async fn owned(
        &self,
        caller: &str,
        invocation: &str,
        scope: &str,
        id: &str,
    ) -> Option<Arc<Turn>> {
        self.turns
            .lock()
            .await
            .get(id)
            .filter(|turn| owned_by(turn, caller, invocation, scope, now()))
            .cloned()
    }

    async fn poll(
        &self,
        caller: &str,
        invocation: &str,
        input: wire::PollRequest,
    ) -> ToolInvocationServiceResolution {
        let Some(turn) = self
            .owned(caller, invocation, &input.scope_id, &input.turn_id)
            .await
        else {
            return failed("turn_unavailable");
        };
        let inner = turn.inner.lock().await;
        if let Some(code) = inner.failure {
            return failed(code);
        }
        if input.after_sequence > inner.sequence
            || inner
                .events
                .front()
                .is_some_and(|event| input.after_sequence.saturating_add(1) < event.sequence)
        {
            return failed("cursor_gap");
        }
        let events: Vec<_> = inner
            .events
            .iter()
            .filter(|event| event.sequence > input.after_sequence)
            .take(64)
            .cloned()
            .collect();
        let next_sequence = events
            .last()
            .map_or(input.after_sequence, |event| event.sequence);
        responded(&wire::PollResponse {
            events,
            next_sequence,
            terminal: inner.terminal && next_sequence == inner.sequence,
        })
    }

    async fn run(&self, state: Arc<ServerState>, id: &str, turn: Arc<Turn>) {
        let result = self.run_provider(&state, id, &turn).await;
        let mut inner = turn.inner.lock().await;
        if let Err(code) = result {
            if code == "turn_cancelled" {
                retain_event(&mut inner, bcode_model::ProviderTurnEvent::Cancelled);
            } else {
                inner.failure = Some(code);
            }
        }
        inner.terminal = true;
        inner.completed = true;
        // A failed finish is retained as unresolved rather than evicted or retried as a start.
        if !matches!(
            inner.failure,
            Some("cleanup_failed" | "start_failed_no_retry")
        ) {
            inner.cleaned = true;
        }
    }

    #[cfg(target_os = "linux")]
    fn retained_custody(
        &self,
        state: &ServerState,
        selection: &bcode_config::ResolvedModelSelection,
    ) -> Result<
        Option<Arc<bcode_provider_auth::operations::RetainedAuthRequestCustody>>,
        &'static str,
    > {
        let Some(config) = &self.admission.custody else {
            return Ok(None);
        };
        let mut retained = self.custody.lock().map_err(|_| "custody_unavailable")?;
        if let Some(custody) = retained.as_ref() {
            return Ok(Some(Arc::clone(custody)));
        }
        if selection.auth_pool.is_some() {
            return Err("custody_pool_unsupported");
        }
        let owner = selection
            .provider_plugin_id
            .as_deref()
            .ok_or("custody_owner_missing")?;
        let profile = selection
            .auth_profile
            .as_deref()
            .ok_or("custody_profile_missing")?;
        let provider = state
            .plugins
            .auth_provider(&config.provider_id)
            .ok_or("custody_provider_missing")?;
        if provider.plugin_id != owner {
            return Err("custody_owner_mismatch");
        }
        let runtime = bcode_config::try_load_runtime_auth_subscriptions()
            .map_err(|_| "custody_profile_missing")?;
        let resolved = bcode_provider_auth::resolve_auth_provider_profile(
            &self.config,
            &provider.contribution.provider_id,
            owner,
            Some(profile),
            &runtime,
        )
        .map_err(|_| "custody_profile_missing")?;
        if resolved.profile.backend != "sshenv" {
            return Err("custody_backend_unsupported");
        }
        if bcode_provider_auth::security::device_seal_options_for_auth_profile(&resolved.profile)
            .policy
            != bcode_provider_auth::security::AuthDeviceSealPolicy::Off
        {
            return Err("custody_device_policy_unsupported");
        }
        let method = provider
            .contribution
            .methods
            .iter()
            .find(|method| Some(method.method_id()) == resolved.profile.scheme.as_deref())
            .cloned()
            .ok_or("custody_method_missing")?;
        let identity = read_identity(&config.identity_file)?;
        let storage =
            bcode_provider_auth::custody_storage::CredentialCustodyStorage::open(&config.directory)
                .map_err(|_| "custody_unavailable")?;
        let custody = bcode_provider_auth::operations::RetainedAuthRequestCustody::new(
            storage,
            resolved,
            &provider.contribution.provider_id,
            owner,
            method,
            vec![identity],
            None,
        )
        .map_err(|_| "custody_unavailable")?
        .key_source(Arc::new(FreshCustodyKey));
        let custody = Arc::new(custody);
        *retained = Some(Arc::clone(&custody));
        Ok(Some(custody))
    }

    fn prepare_auth(
        &self,
        state: &ServerState,
        selection: &bcode_config::ResolvedModelSelection,
    ) -> Result<PreparedAuth, &'static str> {
        #[cfg(target_os = "linux")]
        if let Some(custody) = self.retained_custody(state, selection)? {
            let runtime = bcode_config::try_load_runtime_auth_subscriptions()
                .map_err(|_| "auth_unavailable")?;
            let mut context = bcode_provider_auth::resolve_provider_request_context_with_resolver(
                bcode_provider_auth::ProviderRequestContextResolution {
                    config: &self.config,
                    selection: selection.clone(),
                },
                &runtime,
                |_, _| bcode_provider_auth::ResolvedProviderAuth::default(),
            );
            let owner = selection
                .provider_plugin_id
                .as_deref()
                .ok_or("model_profile_incomplete")?;
            let material = custody
                .materialize(owner, &context)
                .map_err(|_| "custody_materialization_failed")?;
            context.auth = Some(material.auth);
            context.env = material.env;
            let request_custody: Arc<dyn bcode_provider_auth::operations::AuthRequestCustody> =
                Arc::<bcode_provider_auth::operations::RetainedAuthRequestCustody>::clone(&custody);
            return Ok(PreparedAuth {
                context,
                request_custody: Some(request_custody),
                credential_custody: Some(custody),
            });
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = state;
            if self.admission.custody.is_some() {
                return Err("custody_platform_unsupported");
            }
        }
        let context = bcode_provider_auth::try_resolve_provider_request_context(
            bcode_provider_auth::ProviderRequestContextResolution {
                config: &self.config,
                selection: selection.clone(),
            },
        )
        .map_err(|_| "auth_unavailable")?;
        Ok(PreparedAuth {
            context,
            request_custody: None,
            credential_custody: None,
        })
    }

    async fn prepare_turn(
        &self,
        state: &ServerState,
        id: &str,
        turn: &Turn,
    ) -> Result<PreparedTurn, &'static str> {
        let selection = self
            .config
            .resolved_model_profile(&self.admission.model_profile)
            .ok_or("model_profile_missing")?;
        let requested_model = selection
            .model_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or("model_profile_incomplete")?;
        let requested_provider = selection
            .provider_plugin_id
            .as_deref()
            .ok_or("model_profile_incomplete")?;
        let PreparedAuth {
            context,
            request_custody,
            credential_custody,
        } = self.prepare_auth(state, &selection)?;
        let target = tokio::time::timeout(
            Duration::from_secs(30),
            Box::pin(resolve_model_request_target(
                state,
                ModelRequestTargetInput {
                    provider_plugin_id: Some(requested_provider),
                    selected_model_id: Some(requested_model),
                    provider_context: &context,
                },
            )),
        )
        .await
        .map_err(|_| "model_unavailable")?
        .map_err(|_| "model_unavailable")?;
        drop(context);
        if target.model_id != requested_model
            || target.provider_plugin_id.as_deref() != Some(requested_provider)
        {
            return Err("fixed_model_resolution_mismatch");
        }
        if turn.cancel.load(Ordering::Acquire) || turn.expires_at <= now() {
            return Err("turn_cancelled");
        }
        let max_output_tokens = target
            .effective_max_output_tokens(&target.provider_context)
            .unwrap_or(4096);
        let mut provider_context = target.provider_context;
        if let Some(custody) = request_custody {
            // Rematerialize after discovery using the same fenced owner, never an earlier snapshot.
            let material = custody
                .materialize(requested_provider, &provider_context)
                .map_err(|_| "custody_materialization_failed")?;
            provider_context.auth = Some(material.auth);
            provider_context.env = material.env;
        }
        let request = build_turn_request(
            id,
            &turn.request,
            target.model_id,
            provider_context,
            max_output_tokens,
        );
        let route = target.provider_plugin_id.ok_or("provider_unavailable")?;
        validate_tool_capabilities(state, &request, &route).await?;
        for tool in &request.tools {
            jsonschema::validator_for(&tool.input_schema).map_err(|_| "invalid_tool_schema")?;
        }
        Ok(PreparedTurn {
            request,
            route,
            credential_custody,
        })
    }

    async fn run_provider(
        &self,
        state: &ServerState,
        id: &str,
        turn: &Turn,
    ) -> Result<(), &'static str> {
        let PreparedTurn {
            request,
            route,
            credential_custody,
        } = self.prepare_turn(state, id, turn).await?;
        // Resolution/authentication can await services. Do not start provider work
        // when the lease or owning invocation ended during preparation.
        if turn.cancel.load(Ordering::Acquire) || turn.expires_at <= now() {
            retain_event(
                &mut *turn.inner.lock().await,
                bcode_model::ProviderTurnEvent::Cancelled,
            );
            return Ok(());
        }
        let scope = PluginInvocationScope::Global;
        let started = tokio::time::timeout(
            Duration::from_mins(1),
            invoke_model_provider_start_turn_with_custody(
                state,
                Some(route.clone()),
                &request,
                scope.clone(),
                &self.config,
                credential_custody,
            ),
        )
        .await
        .map_err(|_| "start_failed_no_retry")?
        .map_err(|_| "start_failed_no_retry")?;
        drop(request);
        poll_provider_turn(state, turn, route, started, scope).await
    }

    /// Signal cleanup when an owning service invocation ends. Invoke even on disconnect/failure.
    pub async fn cancel_owner(&self, caller: &str, invocation: &str) {
        for turn in self.turns.lock().await.values() {
            if turn.caller == caller && turn.invocation == invocation {
                turn.cancel.store(true, Ordering::Release);
            }
        }
    }

    /// Signal expired turns and prune only cleaned, expired idempotency records.
    pub async fn reap(&self) {
        let mut turns = self.turns.lock().await;
        let time = now();
        let mut remove = Vec::new();
        for (id, turn) in turns.iter() {
            if turn.expires_at <= time {
                turn.cancel.store(true, Ordering::Release);
                if time > turn.expires_at.saturating_add(RETENTION_SECONDS)
                    && turn.inner.lock().await.cleaned
                {
                    remove.push(id.clone());
                }
            }
        }
        for id in remove {
            turns.remove(&id);
        }
    }

    /// Stop admission and signal cleanup of every retained turn. Must run before runtime teardown.
    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        let turns: Vec<_> = self.turns.lock().await.values().cloned().collect();
        for turn in &turns {
            turn.cancel.store(true, Ordering::Release);
        }
        loop {
            let mut pending = false;
            for turn in &turns {
                let inner = turn.inner.lock().await;
                if !inner.completed {
                    pending = true;
                }
            }
            if !pending {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[cfg(target_os = "linux")]
struct FreshCustodyKey;

#[cfg(target_os = "linux")]
impl bcode_provider_auth::operations::AuthCustodyKeySource for FreshCustodyKey {
    fn generate(
        &self,
    ) -> Result<zeroize::Zeroizing<[u8; 32]>, bcode_provider_auth::lifecycle::AuthVaultLifecycleError>
    {
        Ok(zeroize::Zeroizing::new(
            *sshenv_vault::crypto::generate_data_key(),
        ))
    }
}

#[cfg(target_os = "linux")]
fn read_identity(path: &std::path::Path) -> Result<zeroize::Zeroizing<String>, &'static str> {
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
    if !path.is_absolute() {
        return Err("custody_identity_unavailable");
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "custody_identity_unavailable")?;
    let metadata = file
        .metadata()
        .map_err(|_| "custody_identity_unavailable")?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 8192
    {
        return Err("custody_identity_unavailable");
    }
    let mut identity = zeroize::Zeroizing::new(String::new());
    file.by_ref()
        .take(8193)
        .read_to_string(&mut identity)
        .map_err(|_| "custody_identity_unavailable")?;
    if identity.len() > 8192 {
        return Err("custody_identity_unavailable");
    }
    // One age identity only. No ambient agent, SSH key, device factor or passphrase fallback.
    let value = identity.trim();
    if !value.starts_with("AGE-SECRET-KEY-1") || value.contains('\n') || value.contains('\r') {
        return Err("custody_identity_unsupported");
    }
    Ok(zeroize::Zeroizing::new(value.to_owned()))
}

fn build_turn_request(
    id: &str,
    input: &wire::StartRequest,
    model_id: String,
    provider_context: bcode_model::ProviderRequestContext,
    max_output_tokens: u32,
) -> ModelTurnRequest {
    ModelTurnRequest {
        session_id: SessionId::new(),
        turn_id: id.to_owned(),
        model_id,
        provider_context,
        system_prompt: None,
        messages: input.messages.clone(),
        tools: input.tools.clone(),
        tool_call_policy: input.tool_call_policy.clone(),
        tool_schema_mode: input.tool_schema_mode,
        parameters: bcode_model::ModelParameters {
            max_output_tokens: Some(
                input
                    .max_output_tokens
                    .unwrap_or(4096)
                    .min(max_output_tokens),
            ),
            ..bcode_model::ModelParameters::default()
        },
        structured_output: None,
        context_management: bcode_model::ContextManagementRequest::default(),
        prompt_cache: bcode_model::PromptCacheHints::default(),
        conversation_reuse: bcode_model::ConversationReuseHints::default(),
        metadata: BTreeMap::new(),
    }
}

async fn poll_provider_turn(
    state: &ServerState,
    turn: &Turn,
    route: String,
    started: StartTurnResponse,
    scope: PluginInvocationScope,
) -> Result<(), &'static str> {
    let mut cancelled = false;
    let mut polling_failed = false;
    loop {
        if turn.cancel.load(Ordering::Acquire) || turn.expires_at <= now() {
            cancelled = true;
            break;
        }
        let batch: Result<bcode_model::PollTurnEventsResponse, String> = provider_call(
            state,
            Some(route.clone()),
            OP_POLL_TURN_EVENTS,
            bcode_model::PollTurnEventsRequest {
                provider_turn_id: started.provider_turn_id.clone(),
            },
            scope.clone(),
        )
        .await;
        let Ok(batch) = batch else {
            polling_failed = true;
            break;
        };
        let mut inner = turn.inner.lock().await;
        for event in batch.events {
            if matches!(
                event,
                bcode_model::ProviderTurnEvent::TurnFinished { .. }
                    | bcode_model::ProviderTurnEvent::Cancelled
                    | bcode_model::ProviderTurnEvent::Error { .. }
            ) {
                inner.terminal = true;
            }
            if let Some(event) = project_event(event) {
                retain_event(&mut inner, event);
            }
        }
        let terminal = inner.terminal;
        drop(inner);
        if terminal {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let cancellation = if cancelled || polling_failed {
        let cancellation: Result<bcode_model::AckResponse, String> = provider_call(
            state,
            Some(route.clone()),
            OP_CANCEL_TURN,
            bcode_model::CancelTurnRequest {
                provider_turn_id: started.provider_turn_id.clone(),
            },
            scope.clone(),
        )
        .await;
        cancellation
    } else {
        Ok(bcode_model::AckResponse::default())
    };
    let finish: Result<bcode_model::AckResponse, String> = provider_call(
        state,
        Some(route),
        OP_FINISH_TURN,
        bcode_model::FinishTurnRequest {
            provider_turn_id: started.provider_turn_id,
        },
        scope,
    )
    .await;
    // Finish acknowledgement alone does not prove a failed cancellation stopped
    // provider work. Keep unresolved cleanup capacity-accounted on either failure.
    acknowledge_cleanup(cancellation, finish)?;
    if polling_failed {
        return Err("provider_poll_failed");
    }
    if cancelled {
        let mut inner = turn.inner.lock().await;
        retain_event(&mut inner, bcode_model::ProviderTurnEvent::Cancelled);
    }
    Ok(())
}

fn acknowledge_cleanup(
    cancellation: Result<bcode_model::AckResponse, String>,
    finish: Result<bcode_model::AckResponse, String>,
) -> Result<(), &'static str> {
    cancellation.map_err(|_| "cleanup_failed")?;
    finish.map_err(|_| "cleanup_failed")?;
    Ok(())
}

async fn provider_call<Q, R>(
    state: &ServerState,
    route: Option<String>,
    operation: &'static str,
    request: Q,
    scope: PluginInvocationScope,
) -> Result<R, String>
where
    Q: serde::Serialize + Send + Sync + 'static,
    R: serde::de::DeserializeOwned + Send + 'static,
{
    tokio::time::timeout(
        Duration::from_secs(30),
        invoke_model_provider_json_blocking_scoped(state, route, operation, request, scope),
    )
    .await
    .map_err(|_| "provider lifecycle deadline exceeded".to_owned())?
}

fn owned_by(turn: &Turn, caller: &str, invocation: &str, scope: &str, time: u64) -> bool {
    turn.caller == caller
        && turn.invocation == invocation
        && turn.scope == scope
        && turn.expires_at > time
}

fn reserve_match(
    turn: &Turn,
    caller: &str,
    invocation: &str,
    input: &wire::StartRequest,
) -> Option<Result<(), &'static str>> {
    if turn.caller != caller || turn.scope != input.scope_id {
        return None;
    }
    if turn.invocation != invocation || turn.expires_at != input.expires_at {
        return Some(Err("scope_conflict"));
    }
    if turn.request.request_id != input.request_id {
        return None;
    }
    Some(if turn.request == *input {
        Ok(())
    } else {
        Err("request_conflict")
    })
}

fn valid_start(input: &wire::StartRequest, admission: &Admission, time: u64) -> bool {
    let bounded_id = |id: &str| {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    };
    bounded_id(&input.scope_id)
        && bounded_id(&input.request_id)
        && input.expires_at > time
        && input.expires_at <= time.saturating_add(admission.max_scope_seconds)
        && input.validate_public().is_ok()
        && input
            .max_output_tokens
            .is_none_or(|tokens| (1..=16384).contains(&tokens))
}

async fn validate_tool_capabilities(
    state: &ServerState,
    request: &ModelTurnRequest,
    route: &str,
) -> Result<(), &'static str> {
    use bcode_model::{RequestedModelFeature as Feature, ToolChoice, ToolChoiceMode};
    if request.tools.is_empty() {
        return Ok(());
    }
    let negotiation = tokio::time::timeout(
        Duration::from_secs(30),
        resolve_model_feature_negotiation(
            state,
            Some(route),
            &request.model_id,
            &request.provider_context,
        ),
    )
    .await
    .map_err(|_| "tool_capabilities_unavailable")?
    .ok_or("tool_capabilities_unavailable")?;
    let choice = match &request.tool_call_policy.choice {
        ToolChoice::Auto => ToolChoiceMode::Auto,
        ToolChoice::None => ToolChoiceMode::None,
        ToolChoice::Required => ToolChoiceMode::Required,
        ToolChoice::Tool { .. } => ToolChoiceMode::Named,
    };
    let mut features = vec![Feature::ToolChoice(choice)];
    if request.tool_call_policy.parallel == Some(true) {
        features.push(Feature::ToolChoice(ToolChoiceMode::Parallel));
    }
    if let Some(mode) = request.tool_schema_mode {
        features.push(Feature::ToolSchema(mode));
    }
    for feature in features {
        if !matches!(
            negotiation
                .provider
                .feature_support
                .negotiate(&negotiation.model.feature_support, feature,),
            bcode_model::NegotiatedFeatureSupport::Guaranteed { .. }
        ) {
            return Err("tool_capability_not_guaranteed");
        }
    }
    Ok(())
}

fn project_event(event: bcode_model::ProviderTurnEvent) -> Option<bcode_model::ProviderTurnEvent> {
    use bcode_model::{ProviderOutputEvent as Output, ProviderTurnEvent as Event};
    match event {
        Event::TurnStarted
        | Event::TextDelta { .. }
        | Event::ToolCallStarted { .. }
        | Event::ToolCallDelta { .. }
        | Event::ToolCallFinished { .. }
        | Event::Usage { .. }
        | Event::TurnFinished { .. }
        | Event::Cancelled
        | Event::Output {
            event:
                Output::TextDelta { .. }
                | Output::ToolCallStarted { .. }
                | Output::ToolCallDelta { .. }
                | Output::ToolCallFinished { .. },
            ..
        } => Some(event),
        Event::Error { error } => Some(Event::Error {
            error: bcode_model::ProviderError {
                code: "provider_turn_failed".into(),
                category: error.category,
                message: "The selected model provider could not complete the turn.".into(),
                retryable: error.retryable,
                provider_message: None,
                failure: None,
                request_id: None,
                diagnostic_context: Box::default(),
                sources: Box::default(),
                retry: None,
            },
        }),
        _ => None,
    }
}

fn retain_event(inner: &mut TurnState, event: bcode_model::ProviderTurnEvent) {
    let bytes = serde_json::to_vec(&event).map_or(MAX_EVENT_BYTES + 1, |encoded| encoded.len());
    inner.sequence += 1;
    if bytes > MAX_EVENT_BYTES {
        inner.failure = Some("event_too_large");
        return;
    }
    inner.bytes += bytes;
    inner.events.push_back(wire::SequencedEvent {
        sequence: inner.sequence,
        event,
    });
    while inner.events.len() > MAX_EVENTS || inner.bytes > MAX_EVENT_BYTES {
        if let Some(event) = inner.events.pop_front() {
            inner.bytes = inner.bytes.saturating_sub(
                serde_json::to_vec(&event.event).map_or(0, |encoded| encoded.len()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fake_host(
        settings: BTreeMap<String, String>,
    ) -> (Arc<ModelHostOperations>, Arc<ServerState>) {
        let state = Arc::new(crate::tests::test_server_state_with_fake_provider(
            bcode_session::SessionManager::default(),
        ));
        let mut config = bcode_config::BcodeConfig::default();
        config.model.profiles.insert(
            "fixed".into(),
            bcode_config::ModelProfileConfig {
                provider_plugin_id: "bcode.fake-provider".into(),
                model_id: Some("fake-echo".into()),
                settings,
                ..Default::default()
            },
        );
        let host = Arc::new(ModelHostOperations::new(
            config,
            Admission {
                allowed_plugin_id: "plugin".into(),
                model_profile: "fixed".into(),
                supported_account_use: true,
                max_turns: 8,
                max_scope_seconds: 900,
                custody: None,
            },
        ));
        host.bind_server(&state);
        (host, state)
    }

    async fn call_host(
        host: &Arc<ModelHostOperations>,
        caller: &str,
        invocation: &str,
        operation: &str,
        payload: impl serde::Serialize,
    ) -> ToolInvocationServiceResolution {
        Arc::clone(host)
            .dispatch(
                caller,
                bcode_tool::ToolInvocationServiceRequest {
                    invocation_id: invocation.into(),
                    request_id: uuid::Uuid::new_v4().to_string(),
                    route_id: None,
                    interface_id: wire::INTERFACE_ID.into(),
                    operation: operation.into(),
                    payload: serde_json::to_value(payload).unwrap(),
                },
            )
            .await
    }

    async fn start_host(host: &Arc<ModelHostOperations>, request: &wire::StartRequest) -> String {
        let ToolInvocationServiceResolution::Responded { payload } =
            call_host(host, "plugin", "serve", wire::OP_START, request).await
        else {
            panic!("start reservation must succeed");
        };
        serde_json::from_value::<wire::StartResponse>(payload)
            .unwrap()
            .turn_id
    }

    async fn settled(host: &ModelHostOperations, id: &str) -> Arc<Turn> {
        let turn = host.turns.lock().await.get(id).unwrap().clone();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if turn.inner.lock().await.completed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("host lifecycle must settle");
        turn
    }

    async fn host_page(
        host: &Arc<ModelHostOperations>,
        scope: &str,
        id: &str,
    ) -> wire::PollResponse {
        let ToolInvocationServiceResolution::Responded { payload } = call_host(
            host,
            "plugin",
            "serve",
            wire::OP_POLL,
            wire::PollRequest {
                scope_id: scope.into(),
                turn_id: id.into(),
                after_sequence: 0,
            },
        )
        .await
        else {
            panic!("settled normalized host poll must succeed");
        };
        serde_json::from_value(payload).unwrap()
    }

    #[tokio::test]
    async fn real_host_authorization_and_reservation() {
        let (host, _state) = fake_host(BTreeMap::new());
        let request = retained_test_turn().request.clone();
        let mut injected = serde_json::to_value(&request).unwrap();
        injected["provider_context"] = serde_json::json!({"auth":{"token":"secret"},"settings":{}});
        assert!(
            matches!(call_host(&host, "plugin", "serve", wire::OP_START, injected).await,
            ToolInvocationServiceResolution::Failed { code, .. } if code == "invalid_request")
        );
        let id = start_host(&host, &request).await;
        assert_eq!(start_host(&host, &request).await, id);
        let mut conflict = request.clone();
        conflict.max_output_tokens = Some(42);
        assert!(
            matches!(call_host(&host, "plugin", "serve", wire::OP_START, &conflict).await,
            ToolInvocationServiceResolution::Failed { code, .. } if code == "request_conflict")
        );
        for (caller, invocation, scope) in [
            ("other", "serve", "scope"),
            ("plugin", "other", "scope"),
            ("plugin", "serve", "other-lease"),
        ] {
            for operation in [wire::OP_CANCEL, wire::OP_FINISH] {
                let forbidden = wire::TurnRequest {
                    scope_id: scope.into(),
                    turn_id: id.clone(),
                };
                assert!(matches!(
                    call_host(&host, caller, invocation, operation, forbidden).await,
                    ToolInvocationServiceResolution::Failed { .. }
                ));
            }
            assert!(matches!(
                call_host(
                    &host,
                    caller,
                    invocation,
                    wire::OP_POLL,
                    wire::PollRequest {
                        scope_id: scope.into(),
                        turn_id: id.clone(),
                        after_sequence: 0
                    }
                )
                .await,
                ToolInvocationServiceResolution::Failed { .. }
            ));
        }
        let turn = settled(&host, &id).await;
        assert!(turn.inner.lock().await.cleaned);
        host.shutdown().await;
    }

    #[tokio::test]
    async fn real_host_normalized_tool_continuation() {
        use bcode_model::{ContentBlock, MessageRole, ModelMessage, ProviderTurnEvent, ToolChoice};
        let (host, _state) = fake_host(BTreeMap::new());
        let mut request = retained_test_turn().request.clone();
        request.tools = vec![bcode_model::ToolDefinition {
            name: "read_file".into(),
            description: "Read a file".into(),
            input_schema: serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        }];
        request.tool_call_policy.choice = ToolChoice::Required;
        let id = start_host(&host, &request).await;
        let turn_request = wire::TurnRequest {
            scope_id: request.scope_id.clone(),
            turn_id: id.clone(),
        };
        let first = settled(&host, &id).await;
        let inner = first.inner.lock().await;
        assert!(inner.cleaned);
        assert_eq!(inner.failure, None);
        drop(inner);
        let page = host_page(&host, &request.scope_id, &id).await;
        assert!(page.terminal);
        let call = page
            .events
            .iter()
            .find_map(|event| match &event.event {
                ProviderTurnEvent::ToolCallFinished { call }
                | ProviderTurnEvent::Output {
                    event: bcode_model::ProviderOutputEvent::ToolCallFinished { call },
                    ..
                } => Some(call.clone()),
                _ => None,
            })
            .expect("actual fake provider must emit a normalized tool call");
        assert_eq!(call.name, "read_file");
        assert!(matches!(
            call_host(&host, "plugin", "serve", wire::OP_FINISH, turn_request).await,
            ToolInvocationServiceResolution::Responded { .. }
        ));
        request.request_id = "continuation".into();
        request.tool_call_policy.choice = ToolChoice::Auto;
        request.messages.push(ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolCall { call: call.clone() }],
        });
        request.messages.push(ModelMessage {
            role: MessageRole::Tool,
            content: vec![ContentBlock::ToolResult {
                result: bcode_model::ToolResult {
                    call_id: call.id,
                    output: "local tool output".into(),
                    is_error: false,
                    content: vec![],
                },
            }],
        });
        let continuation = start_host(&host, &request).await;
        assert_ne!(continuation, id);
        let second = settled(&host, &continuation).await;
        let inner = second.inner.lock().await;
        assert!(inner.cleaned);
        assert_eq!(inner.failure, None);
        drop(inner);
        let page = host_page(&host, &request.scope_id, &continuation).await;
        assert!(page.terminal);
        let text: String = page
            .events
            .iter()
            .filter_map(|event| match &event.event {
                ProviderTurnEvent::TextDelta { text }
                | ProviderTurnEvent::Output {
                    event: bcode_model::ProviderOutputEvent::TextDelta { text },
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "fake tool result: local tool output");
        host.shutdown().await;
    }

    #[tokio::test]
    async fn real_host_disconnect_expiry_and_shutdown_settle_cleanup() {
        for ending in ["cancel", "disconnect", "expiry", "shutdown"] {
            let (host, _state) = fake_host(BTreeMap::from([(
                "fake_stream_delta_delay_ms".into(),
                "2000".into(),
            )]));
            let mut request = retained_test_turn().request.clone();
            request.messages[0].content = vec![bcode_model::ContentBlock::Text {
                text: format!("stream-text {}", "long output ".repeat(1000)),
            }];
            if ending == "expiry" {
                request.expires_at = now() + 2;
            }
            let id = start_host(&host, &request).await;
            // Wait for real provider start/poll, not just the local reservation.
            let turn = host.turns.lock().await.get(&id).unwrap().clone();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if !turn.inner.lock().await.events.is_empty() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            match ending {
                "cancel" => {
                    let input = wire::TurnRequest {
                        scope_id: request.scope_id.clone(),
                        turn_id: id.clone(),
                    };
                    assert!(
                        matches!(call_host(&host, "plugin", "serve", wire::OP_FINISH, &input).await,
                        ToolInvocationServiceResolution::Failed { code, .. } if code == "cleanup_pending")
                    );
                    assert!(matches!(
                        call_host(&host, "plugin", "serve", wire::OP_CANCEL, &input).await,
                        ToolInvocationServiceResolution::Responded { .. }
                    ));
                }
                "disconnect" => host.cancel_owner("plugin", "serve").await,
                "expiry" => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    host.reap().await;
                    assert!(host.owned("plugin", "serve", "scope", &id).await.is_none());
                }
                _ => host.shutdown().await,
            }
            let turn = settled(&host, &id).await;
            let inner = turn.inner.lock().await;
            assert!(inner.cleaned, "{ending}: cleanup must acknowledge");
            assert_eq!(inner.failure, None);
            assert!(
                inner
                    .events
                    .iter()
                    .any(|event| matches!(event.event, bcode_model::ProviderTurnEvent::Cancelled))
            );
            drop(inner);
            if ending != "expiry" {
                assert!(matches!(
                    call_host(
                        &host,
                        "plugin",
                        "serve",
                        wire::OP_FINISH,
                        wire::TurnRequest {
                            scope_id: request.scope_id.clone(),
                            turn_id: id.clone()
                        }
                    )
                    .await,
                    ToolInvocationServiceResolution::Responded { .. }
                ));
            }
            host.shutdown().await;
            assert!(matches!(
                call_host(&host, "plugin", "serve", wire::OP_START, request).await,
                ToolInvocationServiceResolution::Failed { .. }
            ));
        }
    }

    #[tokio::test]
    async fn real_host_ambiguous_provider_start_keeps_same_reservation() {
        let (host, _state) = fake_host(BTreeMap::from([(
            "fake_event_script".into(),
            "not-json".into(),
        )]));
        let request = retained_test_turn().request.clone();
        let id = start_host(&host, &request).await;
        let turn = settled(&host, &id).await;
        let inner = turn.inner.lock().await;
        assert_eq!(inner.failure, Some("start_failed_no_retry"));
        assert!(!inner.cleaned);
        drop(inner);
        assert_eq!(start_host(&host, &request).await, id);
        assert_eq!(host.turns.lock().await.len(), 1);
        assert!(
            matches!(call_host(&host, "plugin", "serve", wire::OP_FINISH,
            wire::TurnRequest { scope_id: request.scope_id, turn_id: id }).await,
            ToolInvocationServiceResolution::Failed { code, .. } if code == "cleanup_pending")
        );
        host.shutdown().await;
    }

    #[tokio::test]
    async fn ambiguous_reservation_and_failed_cleanup_are_never_evicted() {
        let host = ModelHostOperations::new(
            bcode_config::BcodeConfig::default(),
            Admission {
                allowed_plugin_id: "plugin".into(),
                model_profile: "fixed".into(),
                supported_account_use: true,
                max_turns: 2,
                max_scope_seconds: 900,
                custody: None,
            },
        );
        for failure in ["start_failed_no_retry", "cleanup_failed"] {
            let mut turn = retained_test_turn();
            {
                let turn = Arc::get_mut(&mut turn).unwrap();
                turn.expires_at = now().saturating_sub(RETENTION_SECONDS + 1);
                turn.request.expires_at = turn.expires_at;
            }
            {
                let mut inner = turn.inner.lock().await;
                inner.failure = Some(failure);
                inner.completed = true;
                inner.terminal = true;
            }
            assert_eq!(
                reserve_match(&turn, "plugin", "serve", &turn.request),
                Some(Ok(()))
            );
            host.turns.lock().await.insert(failure.into(), turn);
        }
        host.reap().await;
        assert_eq!(host.turns.lock().await.len(), 2);
        host.shutdown().await;
        drop(host);
    }

    #[test]
    fn cleanup_requires_both_provider_acknowledgements() {
        let ack = || Ok(bcode_model::AckResponse::default());
        assert!(acknowledge_cleanup(ack(), ack()).is_ok());
        assert_eq!(
            acknowledge_cleanup(Err("private cancel failure".into()), ack()),
            Err("cleanup_failed")
        );
        assert_eq!(
            acknowledge_cleanup(ack(), Err("private finish failure".into())),
            Err("cleanup_failed")
        );
    }

    #[test]
    fn admission_is_opt_in_and_never_inherits_active_selection() {
        let config = bcode_config::BcodeConfig::default();
        let host = ModelHostOperations::from_config(&config);
        assert!(!host.admits("bcode.auth-broker"));
        assert!(host.admission.model_profile.is_empty());
        drop(host);
        let mut config = config;
        config.plugins.config.insert("bcode.auth-broker".into(), toml::from_str(
            "[model_host]\nallowed_plugin_id = 'bcode.auth-broker'\nmodel_profile = 'fixed'\nsupported_account_use = true\nmax_turns = 1\nmax_scope_seconds = 60\n"
        ).unwrap());
        let host = ModelHostOperations::from_config(&config);
        // Configured admission cannot silently substitute a missing fixed profile.
        assert!(!host.admits("bcode.auth-broker"));
        assert!(!host.admits("other"));
        assert_eq!(host.admission.model_profile, "fixed");
        drop(host);
    }

    #[test]
    fn scope_default_is_bounded_without_renewal() {
        let admission: Admission = serde_json::from_value(serde_json::json!({
            "allowed_plugin_id":"plugin", "model_profile":"fixed", "supported_account_use":true,
            "max_turns":1,
        }))
        .unwrap();
        assert_eq!(admission.max_scope_seconds, 900);
        let mut input = retained_test_turn().request.clone();
        input.expires_at = 1000;
        assert!(valid_start(&input, &admission, 100));
        input.expires_at = 1001;
        assert!(!valid_start(&input, &admission, 100));
        input.expires_at = 86500;
        assert!(!valid_start(&input, &admission, 100));
    }

    #[test]
    fn bounds_and_expiry_are_not_advisory() {
        let admission = Admission {
            allowed_plugin_id: "p".into(),
            model_profile: "m".into(),
            supported_account_use: true,
            max_turns: 2,
            max_scope_seconds: 60,
            custody: None,
        };
        let mut input = wire::StartRequest {
            scope_id: "s".into(),
            expires_at: 150,
            request_id: "r".into(),
            messages: vec![bcode_model::ModelMessage {
                role: bcode_model::MessageRole::User,
                content: vec![bcode_model::ContentBlock::Text { text: "hi".into() }],
            }],
            tools: Vec::new(),
            tool_call_policy: bcode_model::ToolCallRequestPolicy::default(),
            tool_schema_mode: None,
            max_output_tokens: None,
        };
        assert!(valid_start(&input, &admission, 100));
        input.expires_at = 161;
        assert!(!valid_start(&input, &admission, 100));
        input.expires_at = 100;
        assert!(!valid_start(&input, &admission, 100));
    }
    fn retained_test_turn() -> Arc<Turn> {
        let request = wire::StartRequest {
            scope_id: "scope".into(),
            expires_at: now() + 60,
            request_id: "request".into(),
            messages: vec![bcode_model::ModelMessage {
                role: bcode_model::MessageRole::User,
                content: vec![bcode_model::ContentBlock::Text {
                    text: "hello".into(),
                }],
            }],
            tools: Vec::new(),
            tool_call_policy: bcode_model::ToolCallRequestPolicy::default(),
            tool_schema_mode: None,
            max_output_tokens: None,
        };
        Arc::new(Turn {
            caller: "plugin".into(),
            invocation: "serve".into(),
            scope: request.scope_id.clone(),
            expires_at: request.expires_at,
            request,
            cancel: AtomicBool::new(false),
            inner: Mutex::new(TurnState::default()),
        })
    }

    #[test]
    fn request_identity_and_expired_ownership_are_enforced() {
        let turn = retained_test_turn();
        let request = &turn.request;
        assert!(!owned_by(
            &turn,
            "plugin",
            "serve",
            "scope",
            request.expires_at
        ));
        assert_eq!(
            reserve_match(&turn, "plugin", "serve", request),
            Some(Ok(()))
        );
        let mut conflict = request.clone();
        conflict.messages[0].content = vec![bcode_model::ContentBlock::Text {
            text: "changed".into(),
        }];
        assert_eq!(
            reserve_match(&turn, "plugin", "serve", &conflict),
            Some(Err("request_conflict"))
        );
        assert_eq!(
            reserve_match(&turn, "plugin", "other", request),
            Some(Err("scope_conflict"))
        );
    }

    #[tokio::test]
    async fn ownership_cursor_and_request_identity_are_enforced() {
        let turn = retained_test_turn();
        let host = ModelHostOperations::new(
            bcode_config::BcodeConfig::default(),
            Admission {
                allowed_plugin_id: "plugin".into(),
                model_profile: "fixed".into(),
                supported_account_use: true,
                max_turns: 1,
                max_scope_seconds: 60,
                custody: None,
            },
        );
        host.turns
            .lock()
            .await
            .insert("turn".into(), Arc::clone(&turn));
        assert!(
            host.owned("other", "serve", "scope", "turn")
                .await
                .is_none()
        );
        assert!(
            host.owned("plugin", "serve", "different", "turn")
                .await
                .is_none()
        );
        assert!(
            host.owned("plugin", "different", "scope", "turn")
                .await
                .is_none()
        );
        {
            let mut inner = turn.inner.lock().await;
            for _ in 0..300 {
                retain_event(&mut inner, bcode_model::ProviderTurnEvent::TurnStarted);
            }
            inner.terminal = true;
            drop(inner);
        }
        let result = host
            .poll(
                "plugin",
                "serve",
                wire::PollRequest {
                    scope_id: "scope".into(),
                    turn_id: "turn".into(),
                    after_sequence: 0,
                },
            )
            .await;
        assert!(
            matches!(result, ToolInvocationServiceResolution::Failed { code, .. } if code == "cursor_gap")
        );
        let result = host
            .poll(
                "plugin",
                "serve",
                wire::PollRequest {
                    scope_id: "scope".into(),
                    turn_id: "turn".into(),
                    after_sequence: 44,
                },
            )
            .await;
        let ToolInvocationServiceResolution::Responded { payload } = result else {
            panic!("retained cursor must succeed");
        };
        let page: wire::PollResponse = serde_json::from_value(payload).unwrap();
        assert_eq!(page.events.len(), 64);
        assert_eq!(page.events[0].sequence, 45);
        assert_eq!(page.next_sequence, 108);
        assert!(
            !page.terminal,
            "terminal must not hide retained events on later pages"
        );
        assert!(matches!(host.poll("plugin", "serve", wire::PollRequest {
            scope_id: "scope".into(), turn_id: "turn".into(), after_sequence: 301,
        }).await, ToolInvocationServiceResolution::Failed { code, .. } if code == "cursor_gap"));
        let ToolInvocationServiceResolution::Responded { payload } = host
            .poll(
                "plugin",
                "serve",
                wire::PollRequest {
                    scope_id: "scope".into(),
                    turn_id: "turn".into(),
                    after_sequence: 299,
                },
            )
            .await
        else {
            panic!("final retained page must succeed");
        };
        let final_page: wire::PollResponse = serde_json::from_value(payload).unwrap();
        assert!(final_page.terminal);
        assert_eq!(final_page.next_sequence, 300);
        host.cancel_owner("plugin", "serve").await;
        drop(host);
        assert!(turn.cancel.load(Ordering::Acquire));
    }

    #[test]
    fn projects_tool_calls_and_removes_error_private_details() {
        let call = bcode_model::ToolCall {
            id: "c".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path":"x"}),
        };
        let event = bcode_model::ProviderTurnEvent::ToolCallFinished { call };
        assert_eq!(project_event(event.clone()), Some(event));
        let error = bcode_model::ProviderError {
            code: "upstream-secret".into(),
            category: bcode_model::ProviderErrorCategory::ProviderInternal,
            message: "unsafe upstream payload".into(),
            retryable: false,
            provider_message: Some("secret".into()),
            failure: None,
            request_id: Some("private-id".into()),
            diagnostic_context: Box::new(BTreeMap::from([("private".into(), "secret".into())])),
            sources: Box::default(),
            retry: None,
        };
        let projected = project_event(bcode_model::ProviderTurnEvent::Error { error }).unwrap();
        let bcode_model::ProviderTurnEvent::Error { error } = projected else {
            panic!("normalized error required");
        };
        assert_eq!(error.code, "provider_turn_failed");
        assert!(error.provider_message.is_none());
        assert!(error.request_id.is_none());
        assert!(error.diagnostic_context.is_empty());
    }

    #[test]
    fn retention_has_explicit_source_sequences() {
        let mut state = TurnState::default();
        for _ in 0..300 {
            retain_event(&mut state, bcode_model::ProviderTurnEvent::TurnStarted);
        }
        assert_eq!(state.events.len(), MAX_EVENTS);
        assert_eq!(state.events.front().unwrap().sequence, 45);
        assert!(
            project_event(bcode_model::ProviderTurnEvent::ProviderMetadata {
                key: "secret".into(),
                value: "opaque".into()
            })
            .is_none()
        );
    }
}
