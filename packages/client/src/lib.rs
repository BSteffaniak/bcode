#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Programmatic client API for Bcode.

use bcode_agent_profile::{AgentInfo, PolicyStatusResponse};
use bcode_daemon_lifecycle::{DaemonStartError, EnsureDaemonOptions, ensure_daemon_running};
use bcode_ipc::{
    ClientRuntimeContext, CodecError, EnvelopeKind, ErrorResponse, Event, IpcEndpoint,
    LocalIpcStream, PendingToolExchangeSummary, PermissionSummary, PluginContributions,
    PluginServiceResponse, PluginServiceSummary, RalphApproveRequest, RalphCancelRequest,
    RalphCancelResponse, RalphLifecycleRequest, RalphListIterationsRequest,
    RalphListIterationsResponse, RalphListRunsRequest, RalphListRunsResponse, RalphResumeRequest,
    RalphResumeResponse, RalphRunRequest, RalphRunResponse, RalphRunStatusRequest,
    RalphRunStatusResponse, RalphStatusRequest, RalphStatusResponse, Request, Response,
    ResponsePayload, ServerStopMode, SessionBulkMigrationOperationStatus,
    SessionBulkMigrationStartRequest, SessionCatalogSourceStatus, SessionCatalogStatus,
    SessionCompatibilityInventoryRequest, SessionCompatibilityInventoryResponse,
    SessionImportWarning, WorktreeCreateOperationStatus, WorktreeCreateRequest,
    WorktreeCreateResponse, WorktreeListRequest, WorktreeListResponse, WorktreeRemoveRequest,
    WorktreeRemoveResponse, current_working_directory, decode_event, decode_response,
    default_endpoint, recv_envelope, request_envelope, send_envelope,
};
use bcode_session_models::{
    ClientId, ProjectionWindowRequest, RuntimeWorkStatus, SessionDerivationPromptPage,
    SessionDerivationPromptQuery, SessionDerivationRequest, SessionDerivationSourceSnapshot,
    SessionDerivationTerminalOutcome, SessionEvent, SessionEventKind, SessionHistoryAroundQuery,
    SessionHistoryPage, SessionHistoryQuery, SessionHistoryWindow, SessionId,
    SessionInputHistoryEntry, SessionInspectionPage, SessionInspectionQuery, SessionSummary,
    WorkId,
};
use bcode_skill_models::{SkillId, SkillList, SkillManifest};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

const DEFAULT_CLIENT_IPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_CLIENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_CLIENT_DAEMON_START_TIMEOUT: Duration = Duration::from_secs(30);
const LONG_POLL_TRANSPORT_GRACE: Duration = Duration::from_secs(5);

/// Bounded generic session artifact byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionArtifactRange {
    pub artifact_id: String,
    pub reference_key: String,
    pub content_type: Option<String>,
    pub offset: u64,
    pub total_bytes: u64,
    pub reference_bytes: Option<u64>,
    pub reference_revision: u64,
    pub finalized: bool,
    pub finalized_event_seq: Option<u64>,
    pub availability: Option<String>,
    pub complete: Option<bool>,
    pub checksum_sha256: Option<String>,
    pub bytes: Vec<u8>,
}

impl SessionArtifactRange {
    /// Return the offset immediately after this response.
    #[must_use]
    pub fn next_offset(&self) -> u64 {
        self.offset
            .saturating_add(u64::try_from(self.bytes.len()).unwrap_or(u64::MAX))
    }

    /// Return whether this response reaches the current artifact EOF.
    #[must_use]
    pub fn is_eof(&self) -> bool {
        self.next_offset() >= self.total_bytes
    }
}

#[cfg(test)]
mod artifact_range_tests {
    use super::SessionArtifactRange;

    #[test]
    fn range_metadata_supports_eof_and_replacement_detection() {
        let range = SessionArtifactRange {
            artifact_id: "artifact".to_owned(),
            reference_key: "recording".to_owned(),
            content_type: Some("application/octet-stream".to_owned()),
            offset: 8,
            total_bytes: 10,
            reference_bytes: Some(10),
            reference_revision: 42,
            finalized: true,
            finalized_event_seq: Some(42),
            availability: Some("complete".to_owned()),
            complete: Some(true),
            checksum_sha256: Some("abc".to_owned()),
            bytes: b"89".to_vec(),
        };
        assert_eq!(range.next_offset(), 10);
        assert!(range.is_eof());
        assert_eq!(range.finalized_event_seq, Some(42));
        assert_eq!(range.checksum_sha256.as_deref(), Some("abc"));
    }
}

/// Grouped runtime-work lifecycle span.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RuntimeWorkSpan {
    pub work_id: WorkId,
    pub parent_work_id: Option<WorkId>,
    pub label: String,
    pub status: Option<RuntimeWorkStatus>,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub cancelled: bool,
    pub message: Option<String>,
}

impl RuntimeWorkSpan {
    #[must_use]
    pub fn duration_ms(&self) -> Option<u64> {
        Some(self.finished_at_ms?.saturating_sub(self.started_at_ms?))
    }
}

fn runtime_work_spans(events: Vec<SessionEvent>) -> Vec<RuntimeWorkSpan> {
    let mut spans = BTreeMap::new();
    for event in events {
        match event.kind {
            SessionEventKind::RuntimeWorkStarted {
                work_id,
                label,
                parent_work_id,
                started_at_ms,
                ..
            } => {
                spans.insert(
                    work_id.clone(),
                    RuntimeWorkSpan {
                        work_id,
                        parent_work_id,
                        label,
                        status: None,
                        started_at_ms,
                        finished_at_ms: None,
                        cancelled: false,
                        message: None,
                    },
                );
            }
            SessionEventKind::RuntimeWorkCancelRequested { work_id, .. } => {
                if let Some(span) = spans.get_mut(&work_id) {
                    span.cancelled = true;
                }
            }
            SessionEventKind::RuntimeWorkProgress {
                work_id, message, ..
            } => {
                if let Some(span) = spans.get_mut(&work_id) {
                    span.message = Some(message);
                }
            }
            SessionEventKind::RuntimeWorkFinished {
                work_id,
                status,
                finished_at_ms,
                message,
            } => {
                if let Some(span) = spans.get_mut(&work_id) {
                    span.status = Some(status);
                    span.finished_at_ms = finished_at_ms;
                    if message.is_some() {
                        span.message = message;
                    }
                }
            }
            _ => {}
        }
    }
    spans.into_values().collect()
}

/// Errors returned by the Bcode client.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("IPC transport error: {0}")]
    Transport(#[from] bcode_ipc::IpcTransportError),
    #[error("IPC codec error: {0}")]
    Codec(#[from] CodecError),
    #[error("daemon start error: {0}")]
    DaemonStart(#[from] DaemonStartError),
    #[error("server returned error {code}: {message}")]
    Server { code: String, message: String },
    #[error("daemon connection and handshake timed out after {timeout:?}")]
    ConnectTimeout { timeout: Duration },
    #[error("daemon startup timed out after {timeout:?}")]
    DaemonStartupTimeout { timeout: Duration },
    #[error("client request timed out after {timeout:?}")]
    RequestTimeout { timeout: Duration },
    #[error("incompatible daemon: {message}")]
    IncompatibleDaemon { message: String },
    #[error("client protocol error: {0}")]
    Protocol(String),
    #[error("worktree creation failed ({code}): {message}")]
    WorktreeCreate {
        code: String,
        message: String,
        created_path: Option<std::path::PathBuf>,
    },
    #[error("unexpected response payload")]
    UnexpectedResponse,
    #[error("unexpected IPC envelope kind")]
    UnexpectedEnvelope,
}

impl ClientError {
    /// Return true when an optional domain is unavailable while unrelated daemon capabilities
    /// remain usable.
    #[must_use]
    pub fn is_optional_domain_unavailable(&self) -> bool {
        matches!(
            self,
            Self::Server { code, .. } if code == "workflow_capability_unavailable"
        )
    }

    /// Return true when the error means the local daemon transport is unavailable.
    #[must_use]
    pub fn is_daemon_unavailable(&self) -> bool {
        match self {
            Self::Transport(bcode_ipc::IpcTransportError::Io(error)) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ),
            Self::Codec(CodecError::Io(error)) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ),
            Self::DaemonStartupTimeout { .. } | Self::DaemonStart(_) => true,
            Self::ConnectTimeout { .. }
            | Self::RequestTimeout { .. }
            | Self::Transport(_)
            | Self::Codec(_)
            | Self::Server { .. }
            | Self::WorktreeCreate { .. }
            | Self::IncompatibleDaemon { .. }
            | Self::Protocol(_)
            | Self::UnexpectedResponse
            | Self::UnexpectedEnvelope => false,
        }
    }
}

/// Receiver and task for cancellable client-side observation of detached session preparation.
pub struct SessionOpenProgressObserver {
    /// Progress snapshots in operation revision order.
    pub receiver:
        tokio::sync::mpsc::UnboundedReceiver<bcode_session_models::SessionOpenOperationSnapshot>,
    /// Client observation task. Dropping the receiver ends this task but not server migration.
    pub task: tokio::task::JoinHandle<Result<(), ClientError>>,
}

/// Session list response with persistent catalog status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionList {
    pub sessions: Vec<SessionSummary>,
    pub catalog_status: SessionCatalogStatus,
    pub catalog_sources: Vec<SessionCatalogSourceStatus>,
    pub catalog_revision: u64,
}

/// History returned when attaching to a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedSessionHistory {
    pub session: SessionSummary,
    pub history: Vec<SessionEvent>,
    pub input_history: Vec<SessionInputHistoryEntry>,
    pub import_warnings: Vec<SessionImportWarning>,
    pub draft: Option<String>,
    pub runtime_selection: bcode_ipc::SessionRuntimeSelection,
    /// Projection-window metadata when the attach used a semantic projection request.
    pub projection_window: Option<bcode_session_models::ProjectionWindow>,
}

const CLIENT_RUNTIME_ENV_VARS: &[&str] = &[
    "BCODE_OPENAI_API_KEY",
    "OPENAI_API_KEY",
    "BCODE_OPENAI_AUTH_MODE",
    "BCODE_OPENAI_AUTH_PROFILE",
    "BCODE_OPENAI_AUTH_VAULT",
    "BCODE_OPENAI_BASE_URL",
    "OPENAI_BASE_URL",
    "BCODE_OPENAI_MODEL",
    "OPENAI_MODEL",
    "BCODE_OPENAI_MODELS",
    "OPENAI_MODELS",
    "BCODE_OPENAI_DIALECT",
    "OPENAI_DIALECT",
    "BCODE_OPENAI_CODEX_ACCESS_TOKEN",
    "BCODE_OPENAI_CODEX_REFRESH_TOKEN",
    "BCODE_OPENAI_CODEX_ID_TOKEN",
    "BCODE_OPENAI_CODEX_EXPIRES_AT",
    "BCODE_OPENAI_CODEX_ACCOUNT_ID",
    "BCODE_XAI_AUTH_MODE",
    "BCODE_XAI_AUTH_PROFILE",
    "BCODE_XAI_AUTH_VAULT",
    "BCODE_XAI_API_KEY",
    "XAI_API_KEY",
    "BCODE_XAI_BASE_URL",
    "XAI_BASE_URL",
    "BCODE_XAI_MODEL",
    "XAI_MODEL",
    "BCODE_XAI_MODELS",
    "XAI_MODELS",
    "BCODE_BEDROCK_MODEL",
    "BEDROCK_MODEL",
    "BCODE_BEDROCK_MODELS",
    "BEDROCK_MODELS",
    "BCODE_BEDROCK_REGION",
    "BEDROCK_REGION",
    "BCODE_BEDROCK_AWS_PROFILE",
    "AWS_PROFILE",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "BCODE_BEDROCK_ENDPOINT_URL",
    "BEDROCK_ENDPOINT_URL",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
];

fn resolve_caller_path(path: Option<std::path::PathBuf>) -> std::path::PathBuf {
    resolve_path_from(path, &current_working_directory())
}

fn resolve_path_from(
    path: Option<std::path::PathBuf>,
    caller_cwd: &std::path::Path,
) -> std::path::PathBuf {
    let path = path.map_or_else(
        || caller_cwd.to_path_buf(),
        |path| {
            if path.is_absolute() {
                path
            } else {
                caller_cwd.join(path)
            }
        },
    );
    path.canonicalize().unwrap_or(path)
}

fn current_runtime_context() -> ClientRuntimeContext {
    let working_directory = current_working_directory();
    let Ok(config) = bcode_config::load_config() else {
        return ClientRuntimeContext {
            working_directory: Some(working_directory),
            ..ClientRuntimeContext::default()
        };
    };
    let effective_config_toml = bcode_config::encode_effective_config(&config)
        .ok()
        .map(Box::new);
    let mut env = CLIENT_RUNTIME_ENV_VARS
        .iter()
        .filter_map(|name| match std::env::var(name) {
            Ok(value) if !value.trim().is_empty() => Some(((*name).to_string(), value)),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let mut resolved = config.resolved_model_selection();
    resolved.auth_profile = selected_auth_profile(&resolved);
    resolved.auth_pool = selected_auth_pool(&config, &resolved);
    let auth = merge_selected_auth_profile_env(&config, resolved.auth_profile.as_deref(), &mut env);
    let auth_pool_routing = selected_auth_pool_routing(&config, resolved.auth_pool.as_deref());
    let auth_candidates = merge_selected_auth_pool_env(
        &config,
        resolved.auth_pool.as_deref(),
        resolved.auth_profile.as_deref(),
        &mut env,
    );
    let env_keys = env.keys().cloned().map(|key| (key, true)).collect();
    ClientRuntimeContext {
        working_directory: Some(working_directory),
        effective_config_toml,
        selected_provider_plugin_id: resolved.provider_plugin_id,
        selected_model_id: resolved.model_id,
        requested_model_id: resolved.selected_model_id,
        provider_context: bcode_model::ProviderRequestContext {
            model_profile: resolved.model_profile,
            auth_profile: resolved.auth_profile,
            auth_pool: resolved.auth_pool,
            auth_pool_routing,
            auth_pool_selection_reason: None,
            settings: resolved.settings,
            auth,
            auth_candidates,
            request: resolved.request,
            env,
            api_surface: None,
        },
        interaction_adapters: Vec::new(),
        env_keys,
    }
}

fn selected_auth_profile(resolved: &bcode_config::ResolvedModelSelection) -> Option<String> {
    std::env::var(bcode_config::BCODE_AUTH_PROFILE_ENV)
        .ok()
        .filter(|profile| !profile.trim().is_empty())
        .or_else(|| resolved.auth_profile.clone())
}

fn selected_auth_pool(
    config: &bcode_config::BcodeConfig,
    resolved: &bcode_config::ResolvedModelSelection,
) -> Option<String> {
    resolved.auth_pool.clone().or_else(|| {
        resolved
            .auth_profile
            .as_deref()
            .filter(|auth_profile| is_openai_chatgpt_auth_profile(config, auth_profile))
            .map(|_| "openai".to_string())
    })
}

fn is_openai_chatgpt_auth_profile(config: &bcode_config::BcodeConfig, auth_profile: &str) -> bool {
    let Some(profile) = config.auth.profiles.get(auth_profile) else {
        return false;
    };
    profile.settings.get("provider").map(String::as_str) == Some("openai")
        && (profile.scheme.as_deref() == Some("chatgpt")
            || profile.settings.get("mode").map(String::as_str) == Some("chatgpt"))
}

fn selected_auth_pool_routing(
    config: &bcode_config::BcodeConfig,
    auth_pool: Option<&str>,
) -> bcode_model::ProviderAuthPoolRouting {
    let Some(auth_pool) = auth_pool else {
        return bcode_model::ProviderAuthPoolRouting::default();
    };
    let Some(pool) = config.auth.pools.get(auth_pool) else {
        return bcode_model::ProviderAuthPoolRouting::default();
    };
    bcode_model::ProviderAuthPoolRouting {
        strategy: Some(match pool.strategy {
            bcode_config::AuthPoolStrategy::Failover => "failover".to_string(),
            bcode_config::AuthPoolStrategy::RoundRobin => "round_robin".to_string(),
        }),
        priming_enabled: pool.priming.enabled,
        priming_include_primary: pool.priming.include_primary,
        priming_reprime_after: pool.priming.reprime_after.clone(),
        priming_provider_windows: pool.priming.provider_windows,
        priming_fallback_reprime_after: pool.priming.fallback_reprime_after.clone(),
        priming_required_windows: pool.priming.required_windows.clone(),
    }
}

fn merge_selected_auth_profile_env(
    config: &bcode_config::BcodeConfig,
    auth_profile: Option<&str>,
    env: &mut BTreeMap<String, String>,
) -> Option<bcode_model::ProviderAuthContext> {
    if let Some(auth_profile_name) = auth_profile {
        if let Some(auth_profile) = config.auth.profiles.get(auth_profile_name) {
            let resolved =
                bcode_provider_auth::resolve_auth_profile(auth_profile_name, auth_profile);
            for (key, value) in resolved.env {
                env.entry(key).or_insert(value);
            }
            return Some(resolved.auth);
        }
        return None;
    }
    merge_legacy_openai_auth_profile_env(config, env);
    None
}

fn merge_selected_auth_pool_env(
    config: &bcode_config::BcodeConfig,
    auth_pool: Option<&str>,
    primary_auth_profile: Option<&str>,
    env: &mut BTreeMap<String, String>,
) -> Vec<bcode_model::ProviderAuthCandidate> {
    let Some(auth_pool_name) = auth_pool else {
        return Vec::new();
    };
    let registry = bcode_config::load_runtime_auth_subscriptions();
    let order = bcode_config::effective_auth_pool_order(
        config,
        &registry,
        auth_pool_name,
        primary_auth_profile,
    );
    let mut candidates = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for auth_profile_name in &order.profiles {
        if config.auth.profiles.contains_key(auth_profile_name) {
            push_config_auth_candidate(config, auth_profile_name, env, &mut candidates, &mut seen);
            continue;
        }
        if let Some(profile) = registry
            .pools
            .get(auth_pool_name)
            .into_iter()
            .flat_map(|pool| pool.profiles.iter())
            .find(|profile| profile.auth_profile == *auth_profile_name)
        {
            let auth_profile = runtime_subscription_auth_profile(profile);
            let resolved =
                bcode_provider_auth::resolve_auth_profile(&profile.auth_profile, &auth_profile);
            for (key, value) in &resolved.env {
                env.entry(key.clone()).or_insert_with(|| value.clone());
            }
            candidates.push(bcode_model::ProviderAuthCandidate {
                profile: Some(profile.auth_profile.clone()),
                auth: resolved.auth,
                env: resolved.env,
            });
            seen.insert(profile.auth_profile.clone());
        }
    }
    candidates
}

fn push_config_auth_candidate(
    config: &bcode_config::BcodeConfig,
    auth_profile_name: &str,
    env: &mut BTreeMap<String, String>,
    candidates: &mut Vec<bcode_model::ProviderAuthCandidate>,
    seen: &mut std::collections::BTreeSet<String>,
) {
    if !seen.insert(auth_profile_name.to_string()) {
        return;
    }
    if let Some(auth_profile) = config.auth.profiles.get(auth_profile_name) {
        let resolved = bcode_provider_auth::resolve_auth_profile(auth_profile_name, auth_profile);
        for (key, value) in &resolved.env {
            env.entry(key.clone()).or_insert_with(|| value.clone());
        }
        candidates.push(bcode_model::ProviderAuthCandidate {
            profile: Some(auth_profile_name.to_string()),
            auth: resolved.auth,
            env: resolved.env,
        });
    }
}

fn runtime_subscription_auth_profile(
    profile: &bcode_config::RuntimeAuthSubscriptionProfile,
) -> bcode_config::AuthProfileConfig {
    bcode_config::AuthProfileConfig {
        backend: "sshenv".to_string(),
        provider_id: None,
        owner_plugin_id: None,
        scheme: Some(profile.scheme.clone()),
        settings: BTreeMap::from([
            ("provider".to_string(), profile.provider.clone()),
            ("profile".to_string(), profile.storage_profile.clone()),
            ("vault".to_string(), profile.vault.display().to_string()),
            ("mode".to_string(), profile.scheme.clone()),
        ]),
        map: BTreeMap::from([
            (
                "access_token".to_string(),
                bcode_config::AuthCredentialMapping {
                    env: Some("BCODE_OPENAI_CODEX_ACCESS_TOKEN".to_string()),
                    key: None,
                },
            ),
            (
                "refresh_token".to_string(),
                bcode_config::AuthCredentialMapping {
                    env: Some("BCODE_OPENAI_CODEX_REFRESH_TOKEN".to_string()),
                    key: None,
                },
            ),
            (
                "expires_at".to_string(),
                bcode_config::AuthCredentialMapping {
                    env: Some("BCODE_OPENAI_CODEX_EXPIRES_AT".to_string()),
                    key: None,
                },
            ),
        ]),
    }
}

fn merge_legacy_openai_auth_profile_env(
    config: &bcode_config::BcodeConfig,
    env: &mut BTreeMap<String, String>,
) {
    let Some(auth) = &config.auth.openai else {
        return;
    };
    if auth.backend != "sshenv" {
        return;
    }
    let vault = auth
        .vault
        .clone()
        .unwrap_or_else(bcode_config::default_auth_vault_path);
    let options = bcode_provider_auth::security::AuthDeviceSealOptions::from_policy(
        bcode_provider_auth::security::AuthDeviceSealPolicy::Preferred,
    );
    let _report = bcode_provider_auth::security::reconcile_auth_vault_security_report_with_options(
        &vault,
        &auth.profile,
        options,
        None,
    );
    let store = sshenv_vault::SshenvStore::new(
        sshenv_vault::SshenvStoreConfig::new(vault.clone()).with_private_key_paths(
            bcode_provider_auth::security::vault_private_key_paths(&vault),
        ),
    );
    let Ok(Some(profile)) = store.get_profile(&auth.profile) else {
        return;
    };
    for (key, value) in profile {
        env.entry(key).or_insert_with(|| value.to_string());
    }
}

impl From<ErrorResponse> for ClientError {
    fn from(value: ErrorResponse) -> Self {
        Self::Server {
            code: value.code,
            message: value.message,
        }
    }
}

/// Result returned after a user message or skill invocation is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageAcceptance {
    pub queued: bool,
    pub queue_position: Option<u32>,
    pub disposition: bcode_ipc::MessageAcceptanceDisposition,
}

impl MessageAcceptance {
    /// Acceptance for legacy servers that only report message delivery.
    #[must_use]
    pub const fn sent() -> Self {
        Self {
            queued: false,
            queue_position: None,
            disposition: bcode_ipc::MessageAcceptanceDisposition::StartedTurn,
        }
    }
}

/// Client configured for a local Bcode server endpoint.
#[derive(Debug, Clone)]
pub struct BcodeClient {
    endpoint: IpcEndpoint,
    runtime_context: Option<ClientRuntimeContext>,
    daemon_availability: DaemonAvailability,
    connect_timeout: Duration,
    startup_timeout: Duration,
    startup_gate: Arc<tokio::sync::Mutex<()>>,
    request_timeout: Duration,
}

/// Daemon availability policy used by client connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonAvailability {
    /// Require an already-running daemon and return transport errors directly.
    RequireRunning,
    /// Start the daemon when recoverable IPC failures indicate it is unavailable.
    AutoStart,
}

/// Event-driven session catalog watcher.
#[derive(Debug)]
pub struct SessionCatalogWatcher {
    connection: ClientConnection,
    last_revision: u64,
}

impl SessionCatalogWatcher {
    /// Return the initial catalog snapshot after subscribing to updates.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn initial_snapshot(&mut self) -> Result<SessionList, ClientError> {
        let snapshot = self.connection.list_sessions_with_status().await?;
        self.last_revision = snapshot.catalog_revision;
        Ok(snapshot)
    }

    /// Wait for the next catalog revision and fetch its snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon connection fails or listing fails.
    pub async fn next_snapshot(&mut self) -> Result<SessionList, ClientError> {
        loop {
            match self.connection.recv_event().await? {
                Event::SessionCatalogUpdated { revision } if revision > self.last_revision => {
                    let snapshot = self.connection.list_sessions_with_status().await?;
                    self.last_revision = snapshot.catalog_revision.max(revision);
                    return Ok(snapshot);
                }
                Event::SessionCatalogUpdated { .. }
                | Event::Session(_)
                | Event::SessionLive(_)
                | Event::RuntimeWork(_)
                | Event::Workflow(_)
                | Event::SessionViewResyncRequired { .. } => {}
            }
        }
    }
}

/// Session update received by a long-lived watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionWatchEvent {
    /// Durable session event.
    Durable(Box<SessionEvent>),
    /// Ephemeral live session event.
    Live(Box<bcode_session_models::SessionLiveEvent>),
    /// The daemon requires this watcher to replace its view from bounded state.
    ResyncRequired,
}

/// Event-driven session watcher initialized with bounded recent history.
#[derive(Debug)]
pub struct SessionWatcher {
    connection: ClientConnection,
    session_id: SessionId,
    initial: Option<AttachedSessionHistory>,
}

impl SessionWatcher {
    const fn initial_session_id(&self) -> SessionId {
        self.session_id
    }

    /// Take the bounded initial session state captured while subscribing.
    #[must_use]
    pub const fn take_initial(&mut self) -> Option<AttachedSessionHistory> {
        self.initial.take()
    }

    /// Wait for the next durable or live session event.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon connection closes or the event cannot be decoded.
    pub async fn next_event(&mut self) -> Result<SessionWatchEvent, ClientError> {
        loop {
            match self.connection.recv_event().await? {
                Event::Session(event) | Event::RuntimeWork(event) => {
                    return Ok(SessionWatchEvent::Durable(Box::new(event)));
                }
                Event::SessionLive(event) => {
                    return Ok(SessionWatchEvent::Live(Box::new(event)));
                }
                Event::SessionViewResyncRequired {
                    session_id: required,
                } if required == self.initial_session_id() => {
                    return Ok(SessionWatchEvent::ResyncRequired);
                }
                Event::SessionCatalogUpdated { .. }
                | Event::Workflow(_)
                | Event::SessionViewResyncRequired { .. } => {}
            }
        }
    }
}

/// Event-driven runtime-work watcher.
#[derive(Debug)]
pub struct RuntimeWorkWatcher {
    connection: ClientConnection,
}

impl RuntimeWorkWatcher {
    /// Wait for the next runtime-work lifecycle event.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon connection closes or the event cannot be decoded.
    pub async fn next_event(&mut self) -> Result<SessionEvent, ClientError> {
        loop {
            match self.connection.recv_event().await? {
                Event::RuntimeWork(event) => return Ok(event),
                Event::Session(_)
                | Event::SessionLive(_)
                | Event::Workflow(_)
                | Event::SessionViewResyncRequired { .. }
                | Event::SessionCatalogUpdated { .. } => {}
            }
        }
    }
}

/// Event-driven workflow-run watcher.
#[derive(Debug)]
pub struct WorkflowRunWatcher {
    connection: ClientConnection,
    sequence: bcode_workflow_view_models::WorkflowLiveSequence,
}

/// Outcome of receiving one workflow live notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowRunWatchEvent {
    /// Canonical state changed; refetch this run's bounded projection.
    Changed(bcode_workflow_view_models::WorkflowLiveEvent),
    /// Delivery skipped beyond the bounded catch-up window; replace bounded snapshots.
    ResyncRequired,
    /// A future event contract was received and cannot be interpreted.
    UnsupportedVersion { version: u32 },
}

impl WorkflowRunWatcher {
    /// Wait for the next workflow canonical-state notification.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon connection closes or the event cannot be decoded.
    pub async fn next_event(&mut self) -> Result<WorkflowRunWatchEvent, ClientError> {
        loop {
            let event = match self.connection.recv_event().await? {
                Event::Workflow(event) => event,
                Event::Session(_)
                | Event::SessionLive(_)
                | Event::RuntimeWork(_)
                | Event::SessionViewResyncRequired { .. }
                | Event::SessionCatalogUpdated { .. } => continue,
            };
            match self.sequence.observe(&event) {
                bcode_workflow_view_models::WorkflowLiveEventDisposition::Refetch => {
                    return Ok(WorkflowRunWatchEvent::Changed(event));
                }
                bcode_workflow_view_models::WorkflowLiveEventDisposition::Duplicate => {}
                bcode_workflow_view_models::WorkflowLiveEventDisposition::UnsupportedVersion => {
                    return Ok(WorkflowRunWatchEvent::UnsupportedVersion {
                        version: event.version,
                    });
                }
                bcode_workflow_view_models::WorkflowLiveEventDisposition::Gap => {
                    let after_sequence = self.sequence.last_observed().unwrap_or(0);
                    let page = self
                        .connection
                        .workflow_live_event_catch_up(after_sequence, 256)
                        .await?;
                    if page.resync_required {
                        return Ok(WorkflowRunWatchEvent::ResyncRequired);
                    }
                    for caught_up in page.events {
                        match self.sequence.observe(&caught_up) {
                            bcode_workflow_view_models::WorkflowLiveEventDisposition::Refetch
                            | bcode_workflow_view_models::WorkflowLiveEventDisposition::Duplicate => {}
                            bcode_workflow_view_models::WorkflowLiveEventDisposition::Gap => {
                                return Ok(WorkflowRunWatchEvent::ResyncRequired);
                            }
                            bcode_workflow_view_models::WorkflowLiveEventDisposition::UnsupportedVersion => {
                                return Ok(WorkflowRunWatchEvent::UnsupportedVersion {
                                    version: caught_up.version,
                                });
                            }
                        }
                    }
                    return Ok(WorkflowRunWatchEvent::Changed(event));
                }
            }
        }
    }
}

fn configured_request_timeout() -> Duration {
    bcode_config::load_config().map_or(DEFAULT_CLIENT_IPC_REQUEST_TIMEOUT, |config| {
        Duration::from_secs(config.client.request_timeout_secs)
    })
}

impl BcodeClient {
    /// Create a client that connects to the default endpoint.
    #[must_use]
    pub fn default_endpoint() -> Self {
        Self {
            endpoint: default_endpoint(),
            runtime_context: Some(current_runtime_context()),
            daemon_availability: DaemonAvailability::AutoStart,
            connect_timeout: DEFAULT_CLIENT_CONNECT_TIMEOUT,
            startup_timeout: DEFAULT_CLIENT_DAEMON_START_TIMEOUT,
            startup_gate: Arc::new(tokio::sync::Mutex::new(())),
            request_timeout: configured_request_timeout(),
        }
    }

    /// Create a client for a specific endpoint.
    #[must_use]
    pub fn new(endpoint: IpcEndpoint) -> Self {
        Self {
            endpoint,
            runtime_context: None,
            daemon_availability: DaemonAvailability::RequireRunning,
            connect_timeout: DEFAULT_CLIENT_CONNECT_TIMEOUT,
            startup_timeout: DEFAULT_CLIENT_DAEMON_START_TIMEOUT,
            startup_gate: Arc::new(tokio::sync::Mutex::new(())),
            request_timeout: DEFAULT_CLIENT_IPC_REQUEST_TIMEOUT,
        }
    }

    /// Attach a client-supplied runtime context to future connections.
    #[must_use]
    pub fn with_runtime_context(mut self, runtime_context: Option<ClientRuntimeContext>) -> Self {
        self.runtime_context = runtime_context;
        self
    }

    /// Attach renderer interaction adapters to future connections.
    #[must_use]
    pub fn with_interaction_adapters(
        mut self,
        interaction_adapters: Vec<
            bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability,
        >,
    ) -> Self {
        let context = self.runtime_context.get_or_insert_default();
        context.interaction_adapters = interaction_adapters;
        self
    }

    /// Add an interaction adapter to future connections while retaining existing runtime context.
    #[must_use]
    pub fn with_interaction_adapter(
        mut self,
        interaction_adapter: bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability,
    ) -> Self {
        self.runtime_context
            .get_or_insert_default()
            .interaction_adapters
            .push(interaction_adapter);
        self
    }

    /// Configure the maximum wait for one transport connection and verified handshake.
    #[must_use]
    pub const fn with_connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

    /// Configure the maximum wait for daemon lifecycle startup.
    #[must_use]
    pub const fn with_startup_timeout(mut self, startup_timeout: Duration) -> Self {
        self.startup_timeout = startup_timeout;
        self
    }

    /// Configure the maximum wait for application IPC responses.
    #[must_use]
    pub const fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Return the configured connection and handshake timeout.
    #[must_use]
    pub const fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// Return the configured daemon startup timeout.
    #[must_use]
    pub const fn startup_timeout(&self) -> Duration {
        self.startup_timeout
    }

    /// Return the configured application request timeout.
    #[must_use]
    pub const fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    /// Configure daemon availability behavior for future connections.
    #[must_use]
    pub const fn with_daemon_availability(
        mut self,
        daemon_availability: DaemonAvailability,
    ) -> Self {
        self.daemon_availability = daemon_availability;
        self
    }

    /// Ensure a compatible local daemon is available when auto-start is enabled.
    ///
    /// # Errors
    ///
    /// Returns an error when daemon acquisition fails or this client is configured
    /// to require an already-running daemon.
    pub async fn ensure_daemon_available(&self) -> Result<(), ClientError> {
        if self.daemon_availability == DaemonAvailability::RequireRunning {
            return Ok(());
        }
        let _startup_guard = self.startup_gate.lock().await;
        if self
            .connect_with_deadline("bcode-daemon-availability")
            .await
            .is_ok()
        {
            return Ok(());
        }
        tokio::time::timeout(
            self.startup_timeout,
            ensure_daemon_running(&EnsureDaemonOptions {
                endpoint: self.endpoint.clone(),
                quiet: true,
                log_path: bcode_daemon_lifecycle::default_daemon_log_path(),
            }),
        )
        .await
        .map_err(|_| ClientError::DaemonStartupTimeout {
            timeout: self.startup_timeout,
        })??;
        Ok(())
    }

    /// Create an event-driven session catalog watcher.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the subscription.
    pub async fn watch_session_catalog(&self) -> Result<SessionCatalogWatcher, ClientError> {
        let mut connection = self.connect("bcode-session-catalog").await?;
        connection.subscribe_catalog_updates().await?;
        Ok(SessionCatalogWatcher {
            connection,
            last_revision: 0,
        })
    }

    /// Create an event-driven session watcher with bounded recent history.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the attachment.
    pub async fn watch_session(
        &self,
        session_id: SessionId,
        history_limit: usize,
    ) -> Result<SessionWatcher, ClientError> {
        let mut connection = self.connect("bcode-session-view").await?;
        let initial = connection
            .attach_session_recent_with_input_history(session_id, history_limit)
            .await?;
        Ok(SessionWatcher {
            connection,
            session_id,
            initial: Some(initial),
        })
    }

    /// Create an event-driven session watcher with a bounded semantic projection window.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the attachment.
    pub async fn watch_session_projection_window(
        &self,
        session_id: SessionId,
        request: ProjectionWindowRequest,
    ) -> Result<SessionWatcher, ClientError> {
        let mut connection = self.connect("bcode-session-view").await?;
        let initial = connection
            .attach_session_projection_window_with_input_history(session_id, request)
            .await?;
        Ok(SessionWatcher {
            connection,
            session_id,
            initial: Some(initial),
        })
    }

    /// Create an event-driven runtime-work watcher for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the subscription.
    pub async fn watch_runtime_work(
        &self,
        session_id: SessionId,
    ) -> Result<RuntimeWorkWatcher, ClientError> {
        let mut connection = self.connect("bcode-runtime-work").await?;
        connection.subscribe_runtime_work(session_id).await?;
        Ok(RuntimeWorkWatcher { connection })
    }

    /// Create an event-driven watcher for workflow-run canonical-state notifications.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the subscription.
    pub async fn watch_workflow_runs(&self) -> Result<WorkflowRunWatcher, ClientError> {
        let mut connection = self.connect("bcode-workflow-runs").await?;
        let after_sequence = connection.subscribe_workflow_runs().await?;
        Ok(WorkflowRunWatcher {
            connection,
            sequence: bcode_workflow_view_models::WorkflowLiveSequence::from_last_observed(
                after_sequence,
            ),
        })
    }

    /// Check whether the local server accepts requests.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn ping(&self) -> Result<(), ClientError> {
        match self.send_request(Request::Ping).await? {
            ResponsePayload::Pong => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Submit a bounded client-side metrics batch to the daemon-owned registry.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the batch.
    pub async fn ingest_client_metrics(
        &self,
        batch: bcode_metrics::ClientMetricBatch,
    ) -> Result<usize, ClientError> {
        match self
            .send_request(Request::IngestClientMetrics { batch })
            .await?
        {
            ResponsePayload::ClientMetricsIngested { accepted } => Ok(accepted),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Query local server status.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn server_status(&self) -> Result<bcode_ipc::ServerStatus, ClientError> {
        match self
            .send_request(Request::ServerStatus {
                working_directory: Some(current_working_directory()),
            })
            .await?
        {
            ResponsePayload::ServerStatus { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    fn verify_daemon_identity(status: &bcode_ipc::DaemonStatus) -> Result<(), ClientError> {
        let expected_namespace = bcode_ipc::daemon_namespace();
        let expected_protocol = u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION);
        let expected_artifact_id = bcode_ipc::ArtifactId::current();
        let expected_writer_epoch = bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH;
        let expected_event_schema = bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION;
        if status.namespace == expected_namespace
            && status.protocol_version == expected_protocol
            && status.artifact_id.as_ref() == Some(&expected_artifact_id)
            && status.build_fingerprint == bcode_ipc::BUILD_FINGERPRINT
            && status.storage_writer_epoch == Some(expected_writer_epoch)
            && status.session_event_schema_version == Some(expected_event_schema)
        {
            return Ok(());
        }
        Err(ClientError::IncompatibleDaemon {
            message: format!(
                "client expects namespace={expected_namespace} artifact={expected_artifact_id} protocol={expected_protocol} build={} session_event_schema={expected_event_schema} storage_writer_epoch={expected_writer_epoch}; daemon reported namespace={} artifact={} protocol={} build={} executable={} session_event_schema={} storage_writer_epoch={}",
                bcode_ipc::BUILD_FINGERPRINT,
                status.namespace,
                status
                    .artifact_id
                    .as_ref()
                    .map_or("<unknown>", bcode_ipc::ArtifactId::as_str),
                status.protocol_version,
                status.build_fingerprint,
                status.executable_digest.as_deref().unwrap_or("<unknown>"),
                status
                    .session_event_schema_version
                    .map_or_else(|| "<unknown>".to_owned(), |value| value.to_string()),
                status
                    .storage_writer_epoch
                    .map_or_else(|| "<unknown>".to_owned(), |value| value.to_string()),
            ),
        })
    }

    /// Return server status after verifying daemon executable identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, rejects the request, or does not match
    /// this client's executable identity.
    pub async fn verified_server_status(&self) -> Result<bcode_ipc::ServerStatus, ClientError> {
        let status = self.server_status().await?;
        Self::verify_daemon_identity(&status.daemon)?;
        Ok(status)
    }

    /// Request graceful local server shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn server_stop(&self) -> Result<(), ClientError> {
        self.server_stop_with_mode(ServerStopMode::Force).await
    }

    /// Request graceful local server shutdown only if the daemon is idle.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, rejects the request,
    /// or is not idle.
    pub async fn server_stop_if_idle(&self) -> Result<(), ClientError> {
        self.server_stop_with_mode(ServerStopMode::IfIdle).await
    }

    /// Ask the connected daemon to release one quiescent session's runtime ownership.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, rejects the request, or returns an
    /// unexpected response.
    pub async fn release_session_ownership(
        &self,
        session_id: bcode_session_models::SessionId,
    ) -> Result<bcode_ipc::SessionOwnershipReleaseOutcome, ClientError> {
        match self
            .send_request(Request::ReleaseSessionOwnership { session_id })
            .await?
        {
            ResponsePayload::SessionOwnershipReleased {
                session_id: released_session_id,
                outcome,
            } if released_session_id == session_id => Ok(outcome),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    async fn server_stop_with_mode(&self, mode: ServerStopMode) -> Result<(), ClientError> {
        match self.send_request(Request::ServerStop { mode }).await? {
            ResponsePayload::ServerStopping => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return the persisted composer draft for a scope.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn composer_draft(
        &self,
        scope: bcode_ipc::ComposerDraftScope,
    ) -> Result<Option<String>, ClientError> {
        match self.send_request(Request::ComposerDraft { scope }).await? {
            ResponsePayload::ComposerDraft { draft } => Ok(draft),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Set or clear the persisted composer draft for a scope.
    ///
    /// Empty text clears the draft.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn set_composer_draft(
        &self,
        scope: bcode_ipc::ComposerDraftScope,
        text: String,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::SetComposerDraft { scope, text })
            .await?
        {
            ResponsePayload::ComposerDraftSet => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Create a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn create_session(
        &self,
        name: Option<String>,
    ) -> Result<SessionSummary, ClientError> {
        self.create_session_in_working_directory(name, current_working_directory())
            .await
    }

    /// Create a session in a specific working directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn create_session_in_working_directory(
        &self,
        name: Option<String>,
        working_directory: std::path::PathBuf,
    ) -> Result<SessionSummary, ClientError> {
        let working_directory =
            resolve_path_from(Some(working_directory), &current_working_directory());
        match self
            .send_request(Request::CreateSession {
                name,
                working_directory,
            })
            .await?
        {
            ResponsePayload::SessionCreated { session } => Ok(session),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List sessions.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, ClientError> {
        Ok(self.list_sessions_with_status().await?.sessions)
    }

    /// List sessions and return the persistent catalog status observed by the server.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_sessions_with_status(&self) -> Result<SessionList, ClientError> {
        match self
            .send_request(Request::ListSessions {
                working_directory: current_working_directory(),
            })
            .await?
        {
            ResponsePayload::SessionList {
                sessions,
                catalog_status,
                catalog_sources,
                catalog_revision,
            } => Ok(SessionList {
                sessions,
                catalog_status,
                catalog_sources,
                catalog_revision,
            }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read one bounded, non-mutating session compatibility inventory page.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_compatibility_inventory(
        &self,
        request: SessionCompatibilityInventoryRequest,
    ) -> Result<SessionCompatibilityInventoryResponse, ClientError> {
        match self
            .send_request(Request::SessionCompatibilityInventory { request })
            .await?
        {
            ResponsePayload::SessionCompatibilityInventory { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Start explicit bounded bulk canonical migration or its inventory mode.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn start_session_bulk_migration(
        &self,
        request: SessionBulkMigrationStartRequest,
    ) -> Result<SessionBulkMigrationOperationStatus, ClientError> {
        match self
            .send_request(Request::SessionBulkMigrationStart { request })
            .await?
        {
            ResponsePayload::SessionBulkMigrationOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read transient bulk migration operation status.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unavailable.
    pub async fn session_bulk_migration_status(
        &self,
        operation_id: String,
    ) -> Result<SessionBulkMigrationOperationStatus, ClientError> {
        match self
            .send_request(Request::SessionBulkMigrationStatus { operation_id })
            .await?
        {
            ResponsePayload::SessionBulkMigrationOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Wait for a newer transient bulk migration operation revision.
    ///
    /// Aggregate operation state is daemon-local and is not durable across restart.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unavailable.
    pub async fn wait_session_bulk_migration(
        &self,
        operation_id: String,
        after_revision: u64,
        timeout_ms: u64,
    ) -> Result<SessionBulkMigrationOperationStatus, ClientError> {
        validate_session_bulk_migration_wait_timeout(timeout_ms)?;
        let server_wait = Duration::from_millis(timeout_ms);
        let response_timeout = self
            .request_timeout
            .max(server_wait.saturating_add(LONG_POLL_TRANSPORT_GRACE));
        match self
            .send_request_with_timeout(
                Request::SessionBulkMigrationWait {
                    operation_id,
                    after_revision,
                    timeout_ms,
                },
                response_timeout,
            )
            .await?
        {
            ResponsePayload::SessionBulkMigrationOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Request cooperative bulk migration cancellation between sessions.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unavailable.
    pub async fn cancel_session_bulk_migration(
        &self,
        operation_id: String,
    ) -> Result<SessionBulkMigrationOperationStatus, ClientError> {
        match self
            .send_request(Request::SessionBulkMigrationCancel { operation_id })
            .await?
        {
            ResponsePayload::SessionBulkMigrationOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Import an external session and return the native Bcode session plus one-time warnings.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the import request.
    pub async fn import_external_session(
        &self,
        source_id: impl Into<String>,
        external_session_id: impl Into<String>,
    ) -> Result<(SessionSummary, Vec<SessionImportWarning>), ClientError> {
        match self
            .send_request(Request::ImportExternalSession {
                source_id: source_id.into(),
                external_session_id: external_session_id.into(),
                working_directory: Some(current_working_directory()),
            })
            .await?
        {
            ResponsePayload::ExternalSessionImported { session, warnings } => {
                Ok((session, warnings))
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Refresh the session catalog and return the refreshed snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn refresh_session_catalog(
        &self,
        sources: Option<Vec<String>>,
    ) -> Result<SessionList, ClientError> {
        match self
            .send_request(Request::RefreshSessionCatalog {
                working_directory: Some(current_working_directory()),
                sources,
            })
            .await?
        {
            ResponsePayload::SessionCatalogRefreshed {
                sessions,
                catalog_status,
                catalog_sources,
                catalog_revision,
            } => Ok(SessionList {
                sessions,
                catalog_status,
                catalog_sources,
                catalog_revision,
            }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Change a session's canonical working directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn change_session_working_directory(
        &self,
        session_id: SessionId,
        working_directory: impl Into<std::path::PathBuf>,
    ) -> Result<SessionSummary, ClientError> {
        let working_directory =
            resolve_path_from(Some(working_directory.into()), &current_working_directory());
        match self
            .send_request(Request::ChangeSessionWorkingDirectory {
                session_id,
                working_directory,
            })
            .await?
        {
            ResponsePayload::SessionWorkingDirectoryChanged { session, .. } => Ok(session),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List Git worktrees for the current repository.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_worktrees(
        &self,
        mut request: WorktreeListRequest,
    ) -> Result<WorktreeListResponse, ClientError> {
        request.cwd = Some(resolve_caller_path(request.cwd));
        match self.send_request(Request::ListWorktrees(request)).await? {
            ResponsePayload::WorktreeList(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Start an idempotent daemon-owned worktree creation operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the operation.
    pub async fn start_worktree_create(
        &self,
        operation_id: String,
        mut request: WorktreeCreateRequest,
    ) -> Result<WorktreeCreateOperationStatus, ClientError> {
        request.cwd = Some(resolve_caller_path(request.cwd));
        match self
            .send_request(Request::WorktreeCreateStart {
                operation_id,
                request,
            })
            .await?
        {
            ResponsePayload::WorktreeCreateOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read one transient worktree creation operation snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unavailable.
    pub async fn worktree_create_status(
        &self,
        operation_id: String,
    ) -> Result<WorktreeCreateOperationStatus, ClientError> {
        match self
            .send_request(Request::WorktreeCreateStatus { operation_id })
            .await?
        {
            ResponsePayload::WorktreeCreateOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Wait for a newer worktree creation operation revision.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unavailable.
    pub async fn wait_worktree_create(
        &self,
        operation_id: String,
        after_revision: u64,
        timeout_ms: u64,
    ) -> Result<WorktreeCreateOperationStatus, ClientError> {
        let server_wait = Duration::from_millis(timeout_ms);
        let response_timeout = self
            .request_timeout
            .max(server_wait.saturating_add(LONG_POLL_TRANSPORT_GRACE));
        match self
            .send_request_with_timeout(
                Request::WorktreeCreateWait {
                    operation_id,
                    after_revision,
                    timeout_ms,
                },
                response_timeout,
            )
            .await?
        {
            ResponsePayload::WorktreeCreateOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Create a Git worktree, waiting through bounded long-poll requests until completion.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or creation reaches a failed terminal
    /// state.
    pub async fn create_worktree(
        &self,
        request: WorktreeCreateRequest,
    ) -> Result<WorktreeCreateResponse, ClientError> {
        let operation_id = format!(
            "worktree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let mut status = self
            .start_worktree_create(operation_id.clone(), request)
            .await?;
        loop {
            if let Some(response) = status.response {
                return Ok(response);
            }
            if let Some(error) = status.error {
                return Err(ClientError::WorktreeCreate {
                    code: error.code,
                    message: error.message,
                    created_path: error.created_path,
                });
            }
            status = self
                .wait_worktree_create(operation_id.clone(), status.revision, 30_000)
                .await?;
        }
    }

    /// Remove a Git worktree.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn remove_worktree(
        &self,
        mut request: WorktreeRemoveRequest,
    ) -> Result<WorktreeRemoveResponse, ClientError> {
        request.cwd = Some(resolve_caller_path(request.cwd));
        match self.send_request(Request::RemoveWorktree(request)).await? {
            ResponsePayload::WorktreeRemoved(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return Ralph loop status for a repository.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn ralph_status(
        &self,
        request: RalphStatusRequest,
    ) -> Result<RalphStatusResponse, ClientError> {
        match self.send_request(Request::RalphStatus(request)).await? {
            ResponsePayload::RalphStatus(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Start a bounded Ralph autonomous run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn run_ralph_loop(
        &self,
        request: RalphRunRequest,
    ) -> Result<RalphRunResponse, ClientError> {
        match self.send_request(Request::RunRalphLoop(request)).await? {
            ResponsePayload::RalphRunStarted(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Approve and start an approval-gated Ralph autonomous run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn approve_ralph_run(
        &self,
        request: RalphApproveRequest,
    ) -> Result<RalphRunResponse, ClientError> {
        match self.send_request(Request::ApproveRalphRun(request)).await? {
            ResponsePayload::RalphRunApproved(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Cancel a Ralph autonomous run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn cancel_ralph_loop(
        &self,
        request: RalphCancelRequest,
    ) -> Result<RalphCancelResponse, ClientError> {
        match self.send_request(Request::CancelRalphLoop(request)).await? {
            ResponsePayload::RalphRunCancelled(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List recent Ralph runs for a repository.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_ralph_runs(
        &self,
        request: RalphListRunsRequest,
    ) -> Result<RalphListRunsResponse, ClientError> {
        match self
            .send_request(Request::ListRalphRuns(Box::new(request)))
            .await?
        {
            ResponsePayload::RalphRunsListed(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List recent Ralph iterations for a repository.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_ralph_iterations(
        &self,
        request: RalphListIterationsRequest,
    ) -> Result<RalphListIterationsResponse, ClientError> {
        match self
            .send_request(Request::ListRalphIterations(Box::new(request)))
            .await?
        {
            ResponsePayload::RalphIterationsListed(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Prepare a Ralph resume run for an interrupted run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn resume_ralph_run(
        &self,
        request: RalphResumeRequest,
    ) -> Result<RalphResumeResponse, ClientError> {
        match self.send_request(Request::ResumeRalphRun(request)).await? {
            ResponsePayload::RalphRunResumed(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return Ralph autonomous run status for a repository.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn ralph_run_status(
        &self,
        request: RalphRunStatusRequest,
    ) -> Result<RalphRunStatusResponse, ClientError> {
        match self.send_request(Request::RalphRunStatus(request)).await? {
            ResponsePayload::RalphRunStatus(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Record a Ralph lifecycle marker in session history.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn record_ralph_lifecycle(
        &self,
        request: RalphLifecycleRequest,
    ) -> Result<SessionEvent, ClientError> {
        match self
            .send_request(Request::RecordRalphLifecycle(request))
            .await?
        {
            ResponsePayload::RalphLifecycleRecorded { event } => Ok(event),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Rename a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn rename_session(
        &self,
        session_id: SessionId,
        name: Option<String>,
    ) -> Result<SessionSummary, ClientError> {
        match self
            .send_request(Request::RenameSession { session_id, name })
            .await?
        {
            ResponsePayload::SessionRenamed { session } => Ok(session),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Delete a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn delete_session(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSummary, ClientError> {
        match self
            .send_request(Request::DeleteSession { session_id })
            .await?
        {
            ResponsePayload::SessionDeleted { session } => Ok(session),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Deliver opaque schema-versioned input to an active invocation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the input.
    pub async fn send_invocation_input(
        &self,
        session_id: SessionId,
        input: bcode_tool::ToolInvocationInput,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::InvocationInput { session_id, input })
            .await?
        {
            ResponsePayload::InvocationInputAccepted => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read a bounded generic artifact range from canonical session metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, rejects the reference/range, or returns
    /// an unexpected payload.
    pub async fn session_artifact_range(
        &self,
        session_id: SessionId,
        artifact_id: String,
        reference_key: String,
        offset: u64,
        length: u32,
    ) -> Result<SessionArtifactRange, ClientError> {
        match self
            .send_request(Request::ReadSessionArtifact {
                session_id,
                artifact_id,
                reference_key,
                offset,
                length,
            })
            .await?
        {
            ResponsePayload::SessionArtifactRange {
                artifact_id,
                reference_key,
                content_type,
                offset,
                total_bytes,
                reference_bytes,
                reference_revision,
                finalized,
                finalized_event_seq,
                availability,
                complete,
                checksum_sha256,
                bytes,
            } => Ok(SessionArtifactRange {
                artifact_id,
                reference_key,
                content_type,
                offset,
                total_bytes,
                reference_bytes,
                reference_revision,
                finalized,
                finalized_event_seq,
                availability,
                complete,
                checksum_sha256,
                bytes,
            }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Submit an ordinary turn with generic admission metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn submit_turn(
        &self,
        session_id: SessionId,
        text: String,
        admission: bcode_session_models::TurnAdmissionMetadata,
    ) -> Result<bcode_session_models::TurnAdmission, ClientError> {
        match self
            .send_request(Request::SubmitTurn {
                session_id,
                text,
                admission,
            })
            .await?
        {
            ResponsePayload::TurnAdmission { admission } => Ok(admission),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return complete replayable session history for explicit export/debug/history commands.
    ///
    /// This request performs a full canonical event read on the daemon. Do not use it for
    /// normal UI, attach, prompt/model-context, catalog, or background maintenance flows; use
    /// [`Self::session_history_page`] or projection-specific APIs instead.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, ClientError> {
        match self
            .send_request(Request::SessionHistory { session_id })
            .await?
        {
            ResponsePayload::SessionHistory { history, .. } => Ok(history),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return a bounded page of session history.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_history_page(
        &self,
        session_id: SessionId,
        query: SessionHistoryQuery,
    ) -> Result<SessionHistoryPage, ClientError> {
        match self
            .send_request(Request::SessionHistoryPage { session_id, query })
            .await?
        {
            ResponsePayload::SessionHistoryPage { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return a bounded canonical history window around one event sequence.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_history_around(
        &self,
        session_id: SessionId,
        query: SessionHistoryAroundQuery,
    ) -> Result<SessionHistoryWindow, ClientError> {
        match self
            .send_request(Request::SessionHistoryAround { session_id, query })
            .await?
        {
            ResponsePayload::SessionHistoryAround { window } => Ok(window),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one bounded structured session investigation page.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_inspection(
        &self,
        session_id: SessionId,
        query: SessionInspectionQuery,
    ) -> Result<SessionInspectionPage, ClientError> {
        match self
            .send_request(Request::SessionInspection { session_id, query })
            .await?
        {
            ResponsePayload::SessionInspection { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Run one bounded terminal federated session search and optionally hydrate exact locators.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_search(
        &self,
        request: bcode_session_search::SessionSearchRequest,
        policy: bcode_session_search::SessionSearchPlanPolicy,
        routes: Vec<bcode_session_search::SessionSearchContentRoute>,
        hydrate: bool,
    ) -> Result<
        (
            bcode_session_search::FederatedSessionSearchResponse,
            Vec<bcode_session_search::HydratedSessionSearchHit>,
        ),
        ClientError,
    > {
        match self
            .send_request(Request::SessionSearch {
                request,
                policy,
                routes,
                hydrate,
            })
            .await?
        {
            ResponsePayload::SessionSearch {
                response,
                hydrated_hits,
            } => Ok((response, hydrated_hits)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return discovered session-search provider capabilities, status, coverage, and failures.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_search_providers(
        &self,
    ) -> Result<bcode_session_search::ListSessionSearchProvidersResponse, ClientError> {
        match self.send_request(Request::SessionSearchProviders).await? {
            ResponsePayload::SessionSearchProviders { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Explain deterministic provider selection without invoking provider searches.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_search_explain(
        &self,
        request: bcode_session_search::SessionSearchRequest,
        policy: bcode_session_search::SessionSearchPlanPolicy,
        routes: Vec<bcode_session_search::SessionSearchContentRoute>,
    ) -> Result<bcode_session_search::SessionSearchPlan, ClientError> {
        match self
            .send_request(Request::SessionSearchExplain {
                request,
                policy,
                routes,
            })
            .await?
        {
            ResponsePayload::SessionSearchPlan { plan } => Ok(plan),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Explicitly purge one provider's derived session-search state.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the provider rejects the operation.
    pub async fn session_search_purge(
        &self,
        provider_id: String,
        confirmation: String,
    ) -> Result<bcode_session_search::SessionSearchMaintenanceResponse, ClientError> {
        match self
            .send_request(Request::SessionSearchPurge {
                provider_id,
                confirmation,
            })
            .await?
        {
            ResponsePayload::SessionSearchMaintenance { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Explicitly recreate one provider's empty derived session-search state.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the provider rejects the operation.
    pub async fn session_search_rebuild(
        &self,
        provider_id: String,
        confirmation: String,
    ) -> Result<bcode_session_search::SessionSearchMaintenanceResponse, ClientError> {
        match self
            .send_request(Request::SessionSearchRebuild {
                provider_id,
                confirmation,
            })
            .await?
        {
            ResponsePayload::SessionSearchMaintenance { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Start an addressable complete historical backfill operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_search_complete_backfill_start(
        &self,
        request: bcode_session_search::CompleteSessionSearchBackfillRequest,
    ) -> Result<bcode_session_search::StartSessionSearchBackfillResponse, ClientError> {
        match self
            .send_request(Request::SessionSearchCompleteBackfillStart { request })
            .await?
        {
            ResponsePayload::SessionSearchCompleteBackfillStarted { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Start explicit bounded indexing across every enabled session-search provider.
    ///
    /// The server owns canonical traversal and provider coordination; this client call only starts
    /// the addressable operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_search_index_all_start(
        &self,
        request: bcode_session_search::CompleteSessionSearchBackfillRequest,
    ) -> Result<bcode_session_search::StartSessionSearchBackfillResponse, ClientError> {
        self.session_search_complete_backfill_start(request).await
    }

    /// Start an addressable bounded single-provider historical backfill operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_search_backfill_start(
        &self,
        request: bcode_session_search::BackfillSessionSearchRequest,
    ) -> Result<bcode_session_search::StartSessionSearchBackfillResponse, ClientError> {
        match self
            .send_request(Request::SessionSearchBackfillStart { request })
            .await?
        {
            ResponsePayload::SessionSearchBackfillStarted { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read bounded status for an addressable historical backfill operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unknown.
    pub async fn session_search_backfill_status(
        &self,
        operation_id: String,
    ) -> Result<bcode_session_search::SessionSearchBackfillOperationStatus, ClientError> {
        match self
            .send_request(Request::SessionSearchBackfillStatus { operation_id })
            .await?
        {
            ResponsePayload::SessionSearchBackfillOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Wait for a newer addressable historical backfill revision or timeout.
    ///
    /// The revision is in-process notification state and does not imply durable resume.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, the operation is unknown, or the wait
    /// bound is invalid.
    pub async fn session_search_backfill_wait(
        &self,
        operation_id: String,
        after_revision: u64,
        timeout_ms: u64,
    ) -> Result<bcode_session_search::SessionSearchBackfillOperationStatus, ClientError> {
        validate_session_search_backfill_wait_timeout(timeout_ms)?;
        let server_wait = Duration::from_millis(timeout_ms);
        let response_timeout = self
            .request_timeout
            .max(server_wait.saturating_add(LONG_POLL_TRANSPORT_GRACE));
        match self
            .send_request_with_timeout(
                Request::SessionSearchBackfillWait {
                    operation_id,
                    after_revision,
                    timeout_ms,
                },
                response_timeout,
            )
            .await?
        {
            ResponsePayload::SessionSearchBackfillOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Request cancellation of an addressable historical backfill operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unknown.
    pub async fn session_search_backfill_cancel(
        &self,
        operation_id: String,
    ) -> Result<bcode_session_search::SessionSearchBackfillOperationStatus, ClientError> {
        match self
            .send_request(Request::SessionSearchBackfillCancel { operation_id })
            .await?
        {
            ResponsePayload::SessionSearchBackfillOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Explicitly backfill selected or bounded catalog sessions into one provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded maintenance
    /// request.
    pub async fn session_search_backfill(
        &self,
        request: bcode_session_search::BackfillSessionSearchRequest,
    ) -> Result<bcode_session_search::SessionSearchBackfillResponse, ClientError> {
        match self
            .send_request(Request::SessionSearchBackfill { request })
            .await?
        {
            ResponsePayload::SessionSearchBackfill { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Send a user message to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn send_user_message(
        &self,
        session_id: SessionId,
        text: String,
        placement: bcode_ipc::PromptPlacement,
    ) -> Result<MessageAcceptance, ClientError> {
        match self
            .send_request(Request::SendUserMessageWithPlacement {
                session_id,
                text,
                placement,
            })
            .await?
        {
            ResponsePayload::MessageAccepted {
                queued,
                queue_position,
            } => Ok(MessageAcceptance {
                queued,
                queue_position,
                disposition: bcode_ipc::MessageAcceptanceDisposition::StartedTurn,
            }),
            ResponsePayload::MessageAcceptedWithDisposition {
                queued,
                queue_position,
                disposition,
            } => Ok(MessageAcceptance {
                queued,
                queue_position,
                disposition,
            }),
            ResponsePayload::MessageSent => Ok(MessageAcceptance::sent()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Send a user message with immutable execution options for its admitted turn.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn send_user_message_with_execution(
        &self,
        session_id: SessionId,
        text: String,
        placement: bcode_ipc::PromptPlacement,
        execution: bcode_session_models::TurnExecutionOptions,
    ) -> Result<MessageAcceptance, ClientError> {
        match self
            .send_request(Request::SendUserMessageWithExecution {
                session_id,
                text,
                placement,
                execution,
            })
            .await?
        {
            ResponsePayload::MessageAccepted {
                queued,
                queue_position,
            } => Ok(MessageAcceptance {
                queued,
                queue_position,
                disposition: bcode_ipc::MessageAcceptanceDisposition::StartedTurn,
            }),
            ResponsePayload::MessageAcceptedWithDisposition {
                queued,
                queue_position,
                disposition,
            } => Ok(MessageAcceptance {
                queued,
                queue_position,
                disposition,
            }),
            ResponsePayload::MessageSent => Ok(MessageAcceptance::sent()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Set a session-specific model selection.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn set_session_model(
        &self,
        session_id: SessionId,
        provider_plugin_id: Option<String>,
        model_id: String,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::SetSessionModel {
                session_id,
                provider_plugin_id,
                model_id,
            })
            .await?
        {
            ResponsePayload::SessionModelSet => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List portable, secret-free auth-pool status.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or returns an unexpected response.
    pub async fn auth_pool_list(
        &self,
    ) -> Result<Vec<bcode_provider_auth_models::AuthPoolSummary>, ClientError> {
        match self.send_request(Request::AuthPoolList).await? {
            ResponsePayload::AuthPoolList { pools } => Ok(pools),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Persist or clear an interactive preferred profile for an auth pool.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the pool/profile.
    pub async fn set_auth_pool_preference(
        &self,
        pool: String,
        profile: Option<String>,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::SetAuthPoolPreference { pool, profile })
            .await?
        {
            ResponsePayload::AuthPoolPreferenceSet => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Set a session-specific reasoning selection.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn set_session_reasoning(
        &self,
        session_id: SessionId,
        effort: Option<String>,
        summary: Option<String>,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::SetSessionReasoning {
                session_id,
                effort,
                summary,
            })
            .await?
        {
            ResponsePayload::SessionModelSet => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Append a durable presentation-only note to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the note.
    pub async fn append_presentation_note(
        &self,
        session_id: SessionId,
        source_id: String,
        note_id: String,
        text: String,
        format: bcode_command::CommandTextFormat,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::AppendPresentationNote {
                session_id,
                source_id,
                note_id,
                text,
                format,
            })
            .await?
        {
            ResponsePayload::PresentationNoteAppended => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return active model metadata for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_model_status(
        &self,
        session_id: SessionId,
    ) -> Result<bcode_ipc::SessionModelStatus, ClientError> {
        match self
            .send_request(Request::SessionModelStatus { session_id })
            .await?
        {
            ResponsePayload::SessionModelStatus { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return active model metadata for a new draft session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn default_model_status(&self) -> Result<bcode_ipc::SessionModelStatus, ClientError> {
        match self.send_request(Request::DefaultModelStatus).await? {
            ResponsePayload::SessionModelStatus { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return available models for a provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_model_list(
        &self,
        provider_plugin_id: Option<String>,
    ) -> Result<bcode_model::ModelList, ClientError> {
        match self
            .send_request(Request::SessionModelList { provider_plugin_id })
            .await?
        {
            ResponsePayload::SessionModelList { models, .. } => Ok(models),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Request cancellation of the active model turn for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn cancel_session_turn(&self, session_id: SessionId) -> Result<bool, ClientError> {
        self.cancel_session_turn_with_options(session_id, false)
            .await
    }

    /// Request cancellation of the active model turn and optionally clear queued commands.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn cancel_session_turn_with_options(
        &self,
        session_id: SessionId,
        clear_queue: bool,
    ) -> Result<bool, ClientError> {
        match self
            .send_request(Request::CancelSessionTurn {
                session_id,
                clear_queue,
            })
            .await?
        {
            ResponsePayload::TurnCancellationRequested { cancelled } => Ok(cancelled),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Atomically create one logical workflow and its initial draft.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the authored document.
    pub async fn create_authored_workflow(
        &self,
        request: bcode_ipc::CreateAuthoredWorkflowRequest,
    ) -> Result<
        (
            bcode_ipc::AuthoredWorkflowSnapshot,
            bcode_ipc::WorkflowDraftSnapshot,
        ),
        ClientError,
    > {
        match self
            .send_request(Request::CreateAuthoredWorkflow(request))
            .await?
        {
            ResponsePayload::AuthoredWorkflowCreated { workflow, draft } => Ok((workflow, *draft)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Apply one already-lowered portable source through the canonical authored-workflow lifecycle.
    ///
    /// The operation creates an absent logical workflow or performs at most one optimistic
    /// replacement of an existing source draft. It never publishes, activates, starts, or retries
    /// a conflict.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, rejects canonical state, or the logical
    /// workflow exists without the selected source-draft identity.
    pub async fn apply_workflow_source(
        &self,
        source_format: bcode_workflow::WorkflowSourceFormat,
        source: String,
        draft_id: String,
    ) -> Result<bcode_workflow::WorkflowSourceApplyResult, ClientError> {
        match self
            .send_request(Request::ApplyWorkflowSource(
                bcode_ipc::ApplyWorkflowSourceRequest {
                    source_format,
                    source,
                    draft_id,
                },
            ))
            .await?
        {
            ResponsePayload::WorkflowSourceApplied { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Apply one bounded renderer-neutral semantic edit batch.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the operation boundary.
    pub async fn apply_workflow_draft_edits(
        &self,
        request: bcode_ipc::ApplyWorkflowDraftEditsRequest,
    ) -> Result<bcode_ipc::WorkflowDraftEditResult, ClientError> {
        match self
            .send_request(Request::ApplyWorkflowDraftEdits(request))
            .await?
        {
            ResponsePayload::WorkflowDraftEditResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Replace one exact authored-workflow draft generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the authored document.
    pub async fn update_workflow_draft(
        &self,
        request: bcode_ipc::UpdateWorkflowDraftRequest,
    ) -> Result<bcode_ipc::WorkflowDraftUpdateResult, ClientError> {
        match self
            .send_request(Request::UpdateWorkflowDraft(request))
            .await?
        {
            ResponsePayload::WorkflowDraftUpdateResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Publish one exact draft generation, optionally activating the new immutable revision.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects publication.
    pub async fn publish_workflow_draft(
        &self,
        request: bcode_ipc::PublishWorkflowDraftRequest,
    ) -> Result<bcode_ipc::WorkflowPublicationResult, ClientError> {
        match self
            .send_request(Request::PublishWorkflowDraft(request))
            .await?
        {
            ResponsePayload::WorkflowPublicationResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Publish one exact draft and then attempt separately reported durable run admission.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the operation before its
    /// publication outcome can be produced.
    pub async fn publish_and_start_workflow(
        &self,
        request: bcode_ipc::PublishAndStartWorkflowRequest,
    ) -> Result<bcode_ipc::WorkflowPublishAndStartResult, ClientError> {
        match self
            .send_request(Request::PublishAndStartWorkflow(Box::new(request)))
            .await?
        {
            ResponsePayload::WorkflowPublishAndStartResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Compare-and-set one immutable revision as active.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects activation.
    pub async fn activate_workflow_revision(
        &self,
        request: bcode_ipc::ActivateWorkflowRevisionRequest,
    ) -> Result<bcode_ipc::WorkflowAuthoringMutationResult, ClientError> {
        match self
            .send_request(Request::ActivateWorkflowRevision(request))
            .await?
        {
            ResponsePayload::WorkflowActivationResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Archive or unarchive one logical authored workflow.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the mutation.
    pub async fn set_authored_workflow_archived(
        &self,
        request: bcode_ipc::SetAuthoredWorkflowArchivedRequest,
    ) -> Result<bcode_ipc::AuthoredWorkflowSnapshot, ClientError> {
        match self
            .send_request(Request::SetAuthoredWorkflowArchived(request))
            .await?
        {
            ResponsePayload::AuthoredWorkflowArchived { workflow } => Ok(workflow),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Discard one exact mutable draft generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the mutation.
    pub async fn discard_workflow_draft(
        &self,
        request: bcode_ipc::DiscardWorkflowDraftRequest,
    ) -> Result<bcode_ipc::WorkflowAuthoringMutationResult, ClientError> {
        match self
            .send_request(Request::DiscardWorkflowDraft(request))
            .await?
        {
            ResponsePayload::WorkflowDraftDiscardResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Fork one exact draft or immutable revision into a new generation-1 draft.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the source/identity.
    pub async fn fork_workflow_draft(
        &self,
        request: bcode_ipc::ForkWorkflowDraftRequest,
    ) -> Result<bcode_ipc::WorkflowDraftSnapshot, ClientError> {
        match self
            .send_request(Request::ForkWorkflowDraft(request))
            .await?
        {
            ResponsePayload::WorkflowDraftForked { draft } => Ok(*draft),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Create one revision-bound workflow preset.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the preset.
    pub async fn create_workflow_preset(
        &self,
        request: bcode_ipc::CreateWorkflowPresetRequest,
    ) -> Result<bcode_ipc::WorkflowPresetSnapshot, ClientError> {
        match self
            .send_request(Request::CreateWorkflowPreset(request))
            .await?
        {
            ResponsePayload::WorkflowPresetCreated { preset } => Ok(preset),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Replace one exact workflow preset generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the preset.
    pub async fn update_workflow_preset(
        &self,
        request: bcode_ipc::UpdateWorkflowPresetRequest,
    ) -> Result<bcode_ipc::WorkflowPresetUpdateResult, ClientError> {
        match self
            .send_request(Request::UpdateWorkflowPreset(request))
            .await?
        {
            ResponsePayload::WorkflowPresetUpdateResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Delete one exact workflow preset generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects deletion.
    pub async fn delete_workflow_preset(
        &self,
        request: bcode_ipc::DeleteWorkflowPresetRequest,
    ) -> Result<bcode_ipc::WorkflowAuthoringMutationResult, ClientError> {
        match self
            .send_request(Request::DeleteWorkflowPreset(request))
            .await?
        {
            ResponsePayload::WorkflowPresetDeleteResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Export one exact immutable authored revision.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the revision is unavailable.
    pub async fn export_workflow_revision(
        &self,
        request: bcode_ipc::ExportWorkflowRevisionRequest,
    ) -> Result<bcode_workflow::WorkflowExportBundle, ClientError> {
        match self
            .send_request(Request::ExportWorkflowRevision(request))
            .await?
        {
            ResponsePayload::WorkflowRevisionExported { bundle } => Ok(*bundle),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Preview one portable import without mutation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the bundle is incompatible.
    pub async fn preview_workflow_import(
        &self,
        request: bcode_ipc::PreviewWorkflowImportRequest,
    ) -> Result<bcode_workflow::WorkflowImportPreview, ClientError> {
        match self
            .send_request(Request::PreviewWorkflowImport(request))
            .await?
        {
            ResponsePayload::WorkflowImportPreview { preview } => Ok(*preview),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Import one portable bundle as a new logical workflow and initial draft.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or import is incompatible/unauthorized.
    pub async fn import_workflow(
        &self,
        request: bcode_ipc::ImportWorkflowRequest,
    ) -> Result<
        (
            bcode_ipc::AuthoredWorkflowSnapshot,
            bcode_ipc::WorkflowDraftSnapshot,
        ),
        ClientError,
    > {
        match self.send_request(Request::ImportWorkflow(request)).await? {
            ResponsePayload::WorkflowImported { workflow, draft } => Ok((workflow, *draft)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Import one portable bundle as a generation-1 draft in an existing logical workflow.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or import is incompatible/unauthorized.
    pub async fn import_workflow_draft(
        &self,
        request: bcode_ipc::ImportWorkflowDraftRequest,
    ) -> Result<bcode_ipc::WorkflowDraftImportResult, ClientError> {
        match self
            .send_request(Request::ImportWorkflowDraft(request))
            .await?
        {
            ResponsePayload::WorkflowDraftImported { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Import one portable bundle as the exact next immutable revision of an existing workflow.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or import is incompatible/unauthorized.
    pub async fn import_workflow_revision(
        &self,
        request: bcode_ipc::ImportWorkflowRevisionRequest,
    ) -> Result<bcode_ipc::WorkflowRevisionImportResult, ClientError> {
        match self
            .send_request(Request::ImportWorkflowRevision(request))
            .await?
        {
            ResponsePayload::WorkflowRevisionImported { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resolve and start one immutable authored-workflow revision.
    ///
    /// # Errors
    ///
    /// Returns an error when resolution, configuration, authorization, or durable run admission
    /// fails.
    pub async fn start_authored_workflow(
        &self,
        request: bcode_ipc::StartAuthoredWorkflowRequest,
    ) -> Result<bcode_ipc::AuthoredWorkflowRunStartResponse, ClientError> {
        match self
            .send_request(Request::StartAuthoredWorkflow(request))
            .await?
        {
            ResponsePayload::AuthoredWorkflowRunStarted(started) => Ok(started),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded logical authored workflows.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bound.
    pub async fn list_authored_workflows(
        &self,
        cursor: Option<bcode_workflow::WorkflowAuthoringListCursor>,
        limit: usize,
    ) -> Result<
        bcode_ipc::WorkflowAuthoringPage<
            bcode_ipc::AuthoredWorkflowSnapshot,
            bcode_workflow::WorkflowAuthoringListCursor,
        >,
        ClientError,
    > {
        match self
            .send_request(Request::ListAuthoredWorkflows { cursor, limit })
            .await?
        {
            ResponsePayload::AuthoredWorkflowList { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Get one logical authored workflow.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity.
    pub async fn authored_workflow(
        &self,
        workflow_id: String,
    ) -> Result<Option<bcode_ipc::AuthoredWorkflowSnapshot>, ClientError> {
        match self
            .send_request(Request::GetAuthoredWorkflow { workflow_id })
            .await?
        {
            ResponsePayload::AuthoredWorkflowDescription { workflow } => Ok(workflow),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one bounded aggregate authored-workflow inspection snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity/bound.
    pub async fn inspect_authored_workflow(
        &self,
        workflow_id: String,
        limit: usize,
    ) -> Result<Option<bcode_ipc::AuthoredWorkflowInspection>, ClientError> {
        match self
            .send_request(Request::InspectAuthoredWorkflow { workflow_id, limit })
            .await?
        {
            ResponsePayload::AuthoredWorkflowInspection { inspection } => {
                Ok(inspection.map(|inspection| *inspection))
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded mutable drafts for one logical workflow.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity/bound.
    pub async fn list_workflow_drafts(
        &self,
        workflow_id: String,
        cursor: Option<bcode_workflow::WorkflowAuthoringListCursor>,
        limit: usize,
    ) -> Result<
        bcode_ipc::WorkflowAuthoringPage<
            bcode_ipc::WorkflowDraftSnapshot,
            bcode_workflow::WorkflowAuthoringListCursor,
        >,
        ClientError,
    > {
        match self
            .send_request(Request::ListWorkflowDrafts {
                workflow_id,
                cursor,
                limit,
            })
            .await?
        {
            ResponsePayload::WorkflowDraftList { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Get one exact mutable workflow draft.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity.
    pub async fn workflow_draft(
        &self,
        workflow_id: String,
        draft_id: String,
    ) -> Result<Option<bcode_ipc::WorkflowDraftSnapshot>, ClientError> {
        match self
            .send_request(Request::GetWorkflowDraft {
                workflow_id,
                draft_id,
            })
            .await?
        {
            ResponsePayload::WorkflowDraftDescription { draft } => Ok(draft.map(|draft| *draft)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded immutable published revisions.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity/bound.
    pub async fn list_workflow_revisions(
        &self,
        workflow_id: String,
        cursor: Option<bcode_workflow::WorkflowRevisionListCursor>,
        limit: usize,
    ) -> Result<
        bcode_ipc::WorkflowAuthoringPage<
            bcode_ipc::WorkflowRevisionSnapshot,
            bcode_workflow::WorkflowRevisionListCursor,
        >,
        ClientError,
    > {
        match self
            .send_request(Request::ListWorkflowRevisions {
                workflow_id,
                cursor,
                limit,
            })
            .await?
        {
            ResponsePayload::WorkflowRevisionList { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Get one exact immutable published revision.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity.
    pub async fn workflow_revision(
        &self,
        workflow_id: String,
        revision: u64,
    ) -> Result<Option<bcode_ipc::WorkflowRevisionSnapshot>, ClientError> {
        match self
            .send_request(Request::GetWorkflowRevision {
                workflow_id,
                revision,
            })
            .await?
        {
            ResponsePayload::WorkflowRevisionDescription { revision } => {
                Ok(revision.map(|revision| *revision))
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Inspect immutable revision facts with current derived requirement availability.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity.
    pub async fn workflow_revision_requirement_inspection(
        &self,
        workflow_id: String,
        revision: u64,
    ) -> Result<Option<bcode_ipc::WorkflowRevisionRequirementInspection>, ClientError> {
        match self
            .send_request(Request::InspectWorkflowRevisionRequirements {
                workflow_id,
                revision,
            })
            .await?
        {
            ResponsePayload::WorkflowRevisionRequirementInspection { inspection } => Ok(inspection),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded revision-bound presets.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity/bound.
    pub async fn list_workflow_presets(
        &self,
        workflow_id: String,
        cursor: Option<bcode_workflow::WorkflowAuthoringListCursor>,
        limit: usize,
    ) -> Result<
        bcode_ipc::WorkflowAuthoringPage<
            bcode_ipc::WorkflowPresetSnapshot,
            bcode_workflow::WorkflowAuthoringListCursor,
        >,
        ClientError,
    > {
        match self
            .send_request(Request::ListWorkflowPresets {
                workflow_id,
                cursor,
                limit,
            })
            .await?
        {
            ResponsePayload::WorkflowPresetList { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Get one exact revision-bound preset.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity.
    pub async fn workflow_preset(
        &self,
        workflow_id: String,
        preset_id: String,
    ) -> Result<Option<bcode_ipc::WorkflowPresetSnapshot>, ClientError> {
        match self
            .send_request(Request::GetWorkflowPreset {
                workflow_id,
                preset_id,
            })
            .await?
        {
            ResponsePayload::WorkflowPresetDescription { preset } => Ok(preset),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return the portable runtime-workflow authoring catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or catalog construction fails.
    pub async fn workflow_authoring_catalog(
        &self,
    ) -> Result<bcode_workflow::WorkflowAuthoringCatalogSnapshot, ClientError> {
        match self.send_request(Request::WorkflowAuthoringCatalog).await? {
            ResponsePayload::WorkflowAuthoringCatalog { catalog } => Ok(catalog),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read one bounded derived package publication receipt without mutation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the package identity is invalid.
    pub async fn workflow_package_publication(
        &self,
        package_id: String,
    ) -> Result<Option<bcode_workflow::WorkflowPackagePublicationReceipt>, ClientError> {
        match self
            .send_request(Request::GetWorkflowPackagePublication { package_id })
            .await?
        {
            ResponsePayload::WorkflowPackagePublication { receipt } => Ok(receipt),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Atomically apply one validated package plan through the daemon-owned workflow store.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, optimistic generations conflict, or
    /// the complete transaction cannot commit.
    pub async fn apply_workflow_package(
        &self,
        request: bcode_ipc::ApplyWorkflowPackageRequest,
    ) -> Result<bcode_workflow::WorkflowPackageMutationResult, ClientError> {
        match self
            .send_request(Request::ApplyWorkflowPackage(request))
            .await?
        {
            ResponsePayload::WorkflowPackageApplied { result } => Ok(*result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Atomically publish every exact package draft generation through the daemon-owned store.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, package facts drift, optimistic
    /// generations conflict, or the complete transaction cannot commit.
    pub async fn publish_workflow_package(
        &self,
        request: bcode_ipc::PublishWorkflowPackageRequest,
    ) -> Result<bcode_workflow::WorkflowPackageMutationResult, ClientError> {
        match self
            .send_request(Request::PublishWorkflowPackage(request))
            .await?
        {
            ResponsePayload::WorkflowPackagePublished { result } => Ok(*result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Validate and plan one bounded workflow package through the daemon-owned catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or package planning fails.
    pub async fn validate_workflow_package(
        &self,
        request: bcode_ipc::WorkflowPackageComputationRequest,
    ) -> Result<bcode_ipc::WorkflowPackageValidationResult, ClientError> {
        match self
            .send_request(Request::ValidateWorkflowPackage(request))
            .await?
        {
            ResponsePayload::WorkflowPackageValidated { result } => Ok(*result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Compile-preview one complete package plan through the daemon-owned catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or any package member cannot compile.
    pub async fn preview_workflow_package(
        &self,
        request: bcode_ipc::WorkflowPackagePreviewRequest,
    ) -> Result<bcode_workflow::WorkflowPackagePreview, ClientError> {
        match self
            .send_request(Request::PreviewWorkflowPackage(request))
            .await?
        {
            ResponsePayload::WorkflowPackagePreviewed { preview } => Ok(*preview),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Validate and lower one raw source through the daemon-owned catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or source lowering fails.
    pub async fn validate_workflow_source(
        &self,
        request: bcode_ipc::WorkflowSourceComputationRequest,
    ) -> Result<bcode_ipc::WorkflowSourceValidationResult, ClientError> {
        match self
            .send_request(Request::ValidateWorkflowSource(request))
            .await?
        {
            ResponsePayload::WorkflowSourceValidated { result } => Ok(*result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Lower and compile-preview one raw source through the daemon-owned catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or source compilation fails.
    pub async fn preview_workflow_source(
        &self,
        request: bcode_ipc::WorkflowSourcePreviewRequest,
    ) -> Result<bcode_ipc::WorkflowSourcePreviewResult, ClientError> {
        match self
            .send_request(Request::PreviewWorkflowSource(request))
            .await?
        {
            ResponsePayload::WorkflowSourcePreviewed { result } => Ok(*result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Validate one portable authoring document without durable mutation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or returns an incompatible response.
    pub async fn validate_workflow_authoring(
        &self,
        document: bcode_workflow::WorkflowAuthoringDocument,
    ) -> Result<bcode_workflow::WorkflowValidationReport, ClientError> {
        self.validate_workflow_authoring_with_control(
            document,
            bcode_ipc::WorkflowComputationControl::default(),
        )
        .await
    }

    /// Validate one document with an explicit server-side deadline/cancellation identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, computation is cancelled/times out, or
    /// the response is incompatible.
    pub async fn validate_workflow_authoring_with_control(
        &self,
        document: bcode_workflow::WorkflowAuthoringDocument,
        control: bcode_ipc::WorkflowComputationControl,
    ) -> Result<bcode_workflow::WorkflowValidationReport, ClientError> {
        match self
            .send_request(Request::ValidateWorkflowAuthoring { document, control })
            .await?
        {
            ResponsePayload::WorkflowAuthoringValidated { report } => Ok(report),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Compile and preview one authored workflow without persistence or dispatch.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or returns an incompatible response.
    pub async fn preview_workflow_compilation(
        &self,
        document: bcode_workflow::WorkflowAuthoringDocument,
        configuration: Option<serde_json::Value>,
    ) -> Result<bcode_workflow::WorkflowCompilationPreview, ClientError> {
        self.preview_workflow_compilation_with_control(
            document,
            configuration,
            bcode_ipc::WorkflowComputationControl::default(),
        )
        .await
    }

    /// Compile and preview with an explicit server-side deadline/cancellation identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, computation is cancelled/times out, or
    /// the response is incompatible.
    pub async fn preview_workflow_compilation_with_control(
        &self,
        document: bcode_workflow::WorkflowAuthoringDocument,
        configuration: Option<serde_json::Value>,
        control: bcode_ipc::WorkflowComputationControl,
    ) -> Result<bcode_workflow::WorkflowCompilationPreview, ClientError> {
        match self
            .send_request(Request::PreviewWorkflowCompilation {
                document,
                configuration,
                control,
            })
            .await?
        {
            ResponsePayload::WorkflowCompilationPreview { preview } => Ok(*preview),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Request cancellation of one exact authored-workflow validation or compilation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the operation identity.
    pub async fn cancel_workflow_computation(
        &self,
        operation_id: String,
    ) -> Result<bool, ClientError> {
        match self
            .send_request(Request::CancelWorkflowComputation { operation_id })
            .await?
        {
            ResponsePayload::WorkflowComputationCancellationRequested { cancelled } => {
                Ok(cancelled)
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Explicitly request incompatible workflow-store reset.
    ///
    /// The running daemon refuses this request because it owns the store; clients use the typed
    /// error to direct operators to the offline maintenance entry point.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or refuses online reset.
    pub async fn reset_incompatible_workflow_store(
        &self,
        confirm: String,
    ) -> Result<bcode_workflow_store::WorkflowStoreResetReceipt, ClientError> {
        match self
            .send_request(Request::ResetIncompatibleWorkflowStore { confirm })
            .await?
        {
            ResponsePayload::WorkflowStoreReset { receipt } => Ok(receipt),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded plugin-owned workflow templates with availability diagnostics.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bound.
    pub async fn list_workflow_templates(
        &self,
        limit: usize,
    ) -> Result<Vec<bcode_ipc::WorkflowTemplateDescription>, ClientError> {
        match self
            .send_request(Request::ListWorkflowTemplates { limit })
            .await?
        {
            ResponsePayload::WorkflowTemplateList { templates } => Ok(templates),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Describe one exact loaded plugin-owned workflow template.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity.
    pub async fn describe_workflow_template(
        &self,
        owner_plugin_id: String,
        template_id: String,
        template_version: u32,
    ) -> Result<Option<bcode_ipc::WorkflowTemplateDescription>, ClientError> {
        match self
            .send_request(Request::DescribeWorkflowTemplate {
                owner_plugin_id,
                template_id,
                template_version,
            })
            .await?
        {
            ResponsePayload::WorkflowTemplateDescription { template } => {
                Ok(template.map(|template| *template))
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Start one exact loaded plugin-owned workflow template.
    ///
    /// # Errors
    ///
    /// Returns an error when requirements are unavailable, configuration is invalid, or the
    /// daemon cannot register and start the exact compiled definition.
    pub async fn start_workflow_template(
        &self,
        request: bcode_ipc::WorkflowTemplateStartRequest,
    ) -> Result<bcode_ipc::WorkflowRunStartResponse, ClientError> {
        match self
            .send_request(Request::StartWorkflowTemplate(request))
            .await?
        {
            ResponsePayload::WorkflowTemplateStarted(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Instantiate a maintainable plugin template as a normal mutable authored draft.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, the template is unavailable, or
    /// authored-state creation is rejected.
    pub async fn instantiate_workflow_template(
        &self,
        request: bcode_ipc::WorkflowTemplateInstantiationRequest,
    ) -> Result<
        (
            bcode_ipc::AuthoredWorkflowSnapshot,
            bcode_ipc::WorkflowDraftSnapshot,
        ),
        ClientError,
    > {
        match self
            .send_request(Request::InstantiateWorkflowTemplate(request))
            .await?
        {
            ResponsePayload::AuthoredWorkflowCreated { workflow, draft } => Ok((workflow, *draft)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Durably register one structurally validated compiled workflow definition.
    ///
    /// Re-registering byte-identical content is idempotent. Reusing an exact identity/version for
    /// different content fails closed.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the definition contract.
    pub async fn register_workflow_definition(
        &self,
        request: bcode_ipc::WorkflowDefinitionRegistrationRequest,
    ) -> Result<bcode_workflow_store::StoredWorkflowDefinition, ClientError> {
        match self
            .send_request(Request::RegisterWorkflowDefinition(request))
            .await?
        {
            ResponsePayload::WorkflowDefinitionRegistered { definition } => Ok(definition),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Register an exact typed definition and start one associated durable workflow through one
    /// retry-safe daemon operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects identity, definition, input,
    /// binding, or execution context.
    pub async fn start_workflow(
        &self,
        request: bcode_ipc::WorkflowStartRequest,
    ) -> Result<bcode_ipc::WorkflowRunStartResponse, ClientError> {
        match self.send_request(Request::StartWorkflow(request)).await? {
            ResponsePayload::WorkflowRunStarted(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resolve and start one exact published package export.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, publication facts drift, or the exact
    /// authored revision cannot start.
    pub async fn start_workflow_package_export(
        &self,
        request: bcode_ipc::StartWorkflowPackageExportRequest,
    ) -> Result<bcode_ipc::WorkflowPackageExportRunStartResponse, ClientError> {
        match self
            .send_request(Request::StartWorkflowPackageExport(request))
            .await?
        {
            ResponsePayload::WorkflowPackageExportRunStarted(response) => Ok(*response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Start one durable workflow from a registered exact definition.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the immutable execution
    /// context, definition identity, or run limits.
    pub async fn start_workflow_run(
        &self,
        request: bcode_ipc::WorkflowRunStartRequest,
    ) -> Result<bcode_ipc::WorkflowRunStartResponse, ClientError> {
        match self
            .send_request(Request::StartWorkflowRun(request))
            .await?
        {
            ResponsePayload::WorkflowRunStarted(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Run one bounded non-mutating workflow doctor inspection.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bound.
    pub async fn doctor_workflow_run(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<bcode_workflow_store::WorkflowDoctorReport, ClientError> {
        match self
            .send_request(Request::DoctorWorkflowRun { run_id, limit })
            .await?
        {
            ResponsePayload::WorkflowDoctorReport { report } => Ok(report),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Apply one explicit typed repair resolution.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the attempt/resolution is invalid.
    pub async fn repair_workflow_attempt(
        &self,
        dispatch_identity: String,
        resolution: bcode_workflow_store::RepairResolution,
    ) -> Result<bcode_workflow_store::RepairResult, ClientError> {
        match self
            .send_request(Request::RepairWorkflowAttempt {
                dispatch_identity,
                resolution,
            })
            .await?
        {
            ResponsePayload::WorkflowAttemptRepaired { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded, checksum-verified durable workflow definitions.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn list_workflow_definitions(
        &self,
        limit: usize,
    ) -> Result<Vec<bcode_workflow_store::StoredWorkflowDefinition>, ClientError> {
        match self
            .send_request(Request::ListWorkflowDefinitions { limit })
            .await?
        {
            ResponsePayload::WorkflowDefinitionList { definitions } => Ok(definitions),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Describe one exact durable workflow definition version.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn describe_workflow_definition(
        &self,
        definition_id: String,
        version: u32,
    ) -> Result<Option<bcode_workflow_store::StoredWorkflowDefinition>, ClientError> {
        match self
            .send_request(Request::DescribeWorkflowDefinition {
                definition_id,
                version,
            })
            .await?
        {
            ResponsePayload::WorkflowDefinitionDescription { definition } => Ok(definition),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one bounded aggregate workflow inspection snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, the run is absent, or a bounded
    /// canonical query fails.
    pub async fn inspect_workflow_run(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<bcode_ipc::WorkflowRunInspection, ClientError> {
        match self
            .send_request(Request::InspectWorkflowRun { run_id, limit })
            .await?
        {
            ResponsePayload::WorkflowRunInspection { inspection } => Ok(*inspection),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one durable workflow run summary.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn workflow_run_status(
        &self,
        run_id: String,
    ) -> Result<Option<bcode_workflow_store::WorkflowRunSummary>, ClientError> {
        match self
            .send_request(Request::WorkflowRunStatus { run_id })
            .await?
        {
            ResponsePayload::WorkflowRunStatus { run } => Ok(run),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return the newest workflow run associated with one exact generic binding key.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded lookup.
    pub async fn associated_workflow_run(
        &self,
        key: bcode_ipc::WorkflowRunBindingLookup,
    ) -> Result<Option<bcode_workflow_store::WorkflowRunSummary>, ClientError> {
        match self
            .send_request(Request::AssociatedWorkflowRun { key })
            .await?
        {
            ResponsePayload::AssociatedWorkflowRun { run } => Ok(run),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Inspect the newest workflow run associated with one exact generic binding key.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or a bounded canonical query fails.
    pub async fn inspect_associated_workflow_run(
        &self,
        key: bcode_ipc::WorkflowRunBindingLookup,
        limit: usize,
    ) -> Result<Option<bcode_ipc::WorkflowRunInspection>, ClientError> {
        match self
            .send_request(Request::InspectAssociatedWorkflowRun { key, limit })
            .await?
        {
            ResponsePayload::AssociatedWorkflowRunInspection { inspection } => {
                Ok(inspection.map(|inspection| *inspection))
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Apply one lifecycle transition to the newest run for one generic binding key.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, lookup fails, or the transition is not
    /// valid for the associated run.
    pub async fn control_associated_workflow_run(
        &self,
        key: bcode_ipc::WorkflowRunBindingLookup,
        action: bcode_ipc::WorkflowRunControlAction,
    ) -> Result<(Option<bcode_workflow_store::WorkflowRunSummary>, bool), ClientError> {
        match self
            .send_request(Request::ControlAssociatedWorkflowRun { key, action })
            .await?
        {
            ResponsePayload::AssociatedWorkflowRunControlled { run, changed } => Ok((run, changed)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one bounded renderer-neutral workflow run projection.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn workflow_run_view(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<bcode_workflow_view_models::WorkflowRunView, ClientError> {
        match self
            .send_request(Request::WorkflowRunView { run_id, limit })
            .await?
        {
            ResponsePayload::WorkflowRunView { view } => Ok(*view),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one bounded renderer-neutral workflow run catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn workflow_catalog_view(
        &self,
        request: bcode_workflow_view_models::WorkflowCatalogRequest,
    ) -> Result<bcode_workflow_view_models::WorkflowCatalogView, ClientError> {
        match self
            .send_request(Request::WorkflowCatalogView { request })
            .await?
        {
            ResponsePayload::WorkflowCatalogView { view } => Ok(view),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded durable workflow run summaries.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn list_workflow_runs(
        &self,
        limit: usize,
    ) -> Result<Vec<bcode_workflow_store::WorkflowRunSummary>, ClientError> {
        match self
            .send_request(Request::ListWorkflowRuns { limit })
            .await?
        {
            ResponsePayload::WorkflowRunList { runs } => Ok(runs),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return bounded canonical validated output values for one workflow run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn workflow_run_outputs(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<Vec<bcode_ipc::WorkflowOutputInspection>, ClientError> {
        match self
            .send_request(Request::WorkflowRunOutputs { run_id, limit })
            .await?
        {
            ResponsePayload::WorkflowRunOutputs { outputs } => Ok(outputs),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Request durable cancellation for one workflow run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn cancel_workflow_run(&self, run_id: String) -> Result<bool, ClientError> {
        match self
            .send_request(Request::CancelWorkflowRun { run_id })
            .await?
        {
            ResponsePayload::WorkflowRunCancellationRequested { recorded } => Ok(recorded),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Pause one running workflow before further scheduler admission.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the transition.
    pub async fn pause_workflow_run(&self, run_id: String) -> Result<bool, ClientError> {
        match self
            .send_request(Request::PauseWorkflowRun { run_id })
            .await?
        {
            ResponsePayload::WorkflowRunPaused { changed } => Ok(changed),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resume one paused workflow for subsequent scheduler admission.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the transition.
    pub async fn resume_workflow_run(&self, run_id: String) -> Result<bool, ClientError> {
        match self
            .send_request(Request::ResumeWorkflowRun { run_id })
            .await?
        {
            ResponsePayload::WorkflowRunResumed { changed } => Ok(changed),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Explicitly retry one exact latest failed workflow node attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the exact activation/attempt is stale,
    /// unsafe, cancelled, or outside its retry budget.
    pub async fn retry_workflow_node(
        &self,
        run_id: String,
        node_id: String,
        activation_id: String,
        failed_attempt: u32,
    ) -> Result<bcode_workflow_store::WorkflowNodeRetryResult, ClientError> {
        match self
            .send_request(Request::RetryWorkflowNode {
                run_id,
                node_id,
                activation_id,
                failed_attempt,
            })
            .await?
        {
            ResponsePayload::WorkflowNodeRetried { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded durable input/approval waits for one run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn list_workflow_waits(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<Vec<bcode_workflow_store::WaitingActivation>, ClientError> {
        match self
            .send_request(Request::ListWorkflowWaits { run_id, limit })
            .await?
        {
            ResponsePayload::WorkflowWaitList { waits } => Ok(waits),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resolve one exact durable input wait with schema-validated JSON.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity, state, or value.
    pub async fn provide_workflow_input(
        &self,
        run_id: String,
        node_id: String,
        activation_id: String,
        value: serde_json::Value,
    ) -> Result<bcode_workflow_store::WaitingResolutionResult, ClientError> {
        match self
            .send_request(Request::ProvideWorkflowInput {
                run_id,
                node_id,
                activation_id,
                value,
            })
            .await?
        {
            ResponsePayload::WorkflowWaitResolved { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resolve one exact durable approval wait.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity or state.
    pub async fn resolve_workflow_approval(
        &self,
        run_id: String,
        node_id: String,
        activation_id: String,
        approved: bool,
    ) -> Result<bcode_workflow_store::WaitingResolutionResult, ClientError> {
        match self
            .send_request(Request::ResolveWorkflowApproval {
                run_id,
                node_id,
                activation_id,
                approved,
            })
            .await?
        {
            ResponsePayload::WorkflowWaitResolved { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded pending mutation approvals across all workflow runs.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn list_all_workflow_mutation_approvals(
        &self,
        limit: usize,
    ) -> Result<Vec<bcode_workflow_store::WorkflowMutationApproval>, ClientError> {
        match self
            .send_request(Request::ListWorkflowMutationApprovalsAll { limit })
            .await?
        {
            ResponsePayload::WorkflowMutationApprovalList { approvals } => Ok(approvals),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded pending mutation approvals for one run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn list_workflow_mutation_approvals(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<Vec<bcode_workflow_store::WorkflowMutationApproval>, ClientError> {
        match self
            .send_request(Request::ListWorkflowMutationApprovals { run_id, limit })
            .await?
        {
            ResponsePayload::WorkflowMutationApprovalList { approvals } => Ok(approvals),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resolve one exact durable mutation approval.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity or decision.
    pub async fn resolve_workflow_mutation_approval(
        &self,
        approval_id: String,
        decision: bcode_workflow_store::WorkflowMutationApprovalDecision,
    ) -> Result<bcode_workflow_store::WorkflowMutationApprovalResolution, ClientError> {
        match self
            .send_request(Request::ResolveWorkflowMutationApproval {
                approval_id,
                decision,
            })
            .await?
        {
            ResponsePayload::WorkflowMutationApprovalResolved { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one bounded page of workflow attempts.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn workflow_attempt_history(
        &self,
        run_id: String,
        cursor: Option<bcode_workflow_store::AttemptCursor>,
        limit: usize,
    ) -> Result<Vec<bcode_workflow_store::AttemptSummary>, ClientError> {
        match self
            .send_request(Request::WorkflowAttemptHistory {
                run_id,
                cursor,
                limit,
            })
            .await?
        {
            ResponsePayload::WorkflowAttemptHistory { attempts } => Ok(attempts),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one bounded page of workflow events.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn workflow_event_history(
        &self,
        run_id: String,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<bcode_workflow_store::WorkflowEventRow>, ClientError> {
        match self
            .send_request(Request::WorkflowEventHistory {
                run_id,
                after_sequence,
                limit,
            })
            .await?
        {
            ResponsePayload::WorkflowEventHistory { events } => Ok(events),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Request cancellation of a specific active runtime-work item.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn cancel_runtime_work(
        &self,
        session_id: SessionId,
        work_id: bcode_session_models::WorkId,
    ) -> Result<bool, ClientError> {
        match self
            .send_request(Request::CancelRuntimeWork {
                session_id,
                work_id,
            })
            .await?
        {
            ResponsePayload::RuntimeWorkCancellationRequested { cancelled } => Ok(cancelled),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List active runtime work for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_runtime_work(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<bcode_ipc::RuntimeWorkSnapshot>, ClientError> {
        match self
            .send_request(Request::ListRuntimeWork { session_id })
            .await?
        {
            ResponsePayload::RuntimeWorkList { work } => Ok(work),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return recent durable runtime-work lifecycle events for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn runtime_work_history(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<Vec<bcode_session_models::SessionEvent>, ClientError> {
        match self
            .send_request(Request::RuntimeWorkHistory { session_id, limit })
            .await?
        {
            ResponsePayload::RuntimeWorkHistory { events } => Ok(events),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return grouped runtime-work lifecycle spans for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the history request.
    pub async fn runtime_work_spans(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<Vec<RuntimeWorkSpan>, ClientError> {
        Ok(runtime_work_spans(
            self.runtime_work_history(session_id, limit).await?,
        ))
    }

    /// Compact the model-visible context for a session while preserving append-only history.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn compact_session(&self, session_id: SessionId) -> Result<String, ClientError> {
        match self
            .send_request(Request::CompactSession { session_id })
            .await?
        {
            ResponsePayload::SessionCompacted { message, .. } => Ok(message),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List available agent profiles.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_agents(&self) -> Result<Vec<AgentInfo>, ClientError> {
        match self.send_request(Request::ListAgents).await? {
            ResponsePayload::AgentList { agents } => Ok(agents),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List available skills.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_skills(&self) -> Result<SkillList, ClientError> {
        match self.send_request(Request::ListSkills).await? {
            ResponsePayload::SkillList { skills } => Ok(*skills),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Describe a skill.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn describe_skill(&self, skill_id: SkillId) -> Result<SkillManifest, ClientError> {
        match self
            .send_request(Request::DescribeSkill { skill_id })
            .await?
        {
            ResponsePayload::SkillManifest { skill } => Ok(*skill),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Invoke a skill for one model turn.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn invoke_skill(
        &self,
        session_id: SessionId,
        skill_id: SkillId,
        arguments: String,
        display_text: String,
    ) -> Result<MessageAcceptance, ClientError> {
        self.invoke_skill_request(Request::InvokeSkill {
            session_id,
            skill_id,
            arguments,
            display_text,
        })
        .await
    }

    /// Invoke a skill for one model turn with immutable execution options.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn invoke_skill_with_execution(
        &self,
        session_id: SessionId,
        skill_id: SkillId,
        arguments: String,
        display_text: String,
        execution: bcode_session_models::TurnExecutionOptions,
    ) -> Result<MessageAcceptance, ClientError> {
        self.invoke_skill_request(Request::InvokeSkillWithExecution {
            session_id,
            skill_id,
            arguments,
            display_text,
            execution,
        })
        .await
    }

    async fn invoke_skill_request(
        &self,
        request: Request,
    ) -> Result<MessageAcceptance, ClientError> {
        match self.send_request(request).await? {
            ResponsePayload::MessageAccepted {
                queued,
                queue_position,
            } => Ok(MessageAcceptance {
                queued,
                queue_position,
                disposition: bcode_ipc::MessageAcceptanceDisposition::StartedTurn,
            }),
            ResponsePayload::MessageAcceptedWithDisposition {
                queued,
                queue_position,
                disposition,
            } => Ok(MessageAcceptance {
                queued,
                queue_position,
                disposition,
            }),
            ResponsePayload::MessageSent => Ok(MessageAcceptance::sent()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Activate a skill for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn activate_skill(
        &self,
        session_id: SessionId,
        skill_id: SkillId,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::ActivateSkill {
                session_id,
                skill_id,
            })
            .await?
        {
            ResponsePayload::SessionAgentSet => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Deactivate a skill for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn deactivate_skill(
        &self,
        session_id: SessionId,
        skill_id: SkillId,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::DeactivateSkill {
                session_id,
                skill_id,
            })
            .await?
        {
            ResponsePayload::SessionAgentSet => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return active skills for a session as loaded contexts.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn active_skills(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<bcode_skill_models::SkillContextResponse>, ClientError> {
        match self
            .send_request(Request::ActiveSkills { session_id })
            .await?
        {
            ResponsePayload::ActiveSkills { skills } => Ok(skills),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return agent policy provider status.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn agent_policy_status(&self) -> Result<PolicyStatusResponse, ClientError> {
        match self.send_request(Request::AgentPolicyStatus).await? {
            ResponsePayload::AgentPolicyStatus { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Set a session-specific active agent profile.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn set_session_agent(
        &self,
        session_id: SessionId,
        agent_id: String,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::SetSessionAgent {
                session_id,
                agent_id,
            })
            .await?
        {
            ResponsePayload::SessionAgentSet => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List pending permission checkpoints.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_permissions(&self) -> Result<Vec<PermissionSummary>, ClientError> {
        match self.send_request(Request::ListPermissions).await? {
            ResponsePayload::PermissionList { permissions } => Ok(permissions),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resolve a pending permission checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn resolve_permission(
        &self,
        permission_id: String,
        approved: bool,
    ) -> Result<bool, ClientError> {
        self.resolve_permission_with_remember(permission_id, approved, false)
            .await
    }

    /// Resolve a pending permission checkpoint and optionally remember the policy decision.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn resolve_permission_with_remember(
        &self,
        permission_id: String,
        approved: bool,
        remember: bool,
    ) -> Result<bool, ClientError> {
        match self
            .send_request(Request::ResolvePermission {
                permission_id,
                approved,
                remember,
            })
            .await?
        {
            ResponsePayload::PermissionResolved { resolved } => Ok(resolved),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resolve all currently pending checkpoints in one authorization batch.
    ///
    /// Batch decisions never persist a remembered policy rule; each targeted checkpoint receives
    /// the same one-time decision. Returns the number of checkpoints resolved.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn resolve_permission_batch(
        &self,
        batch_id: String,
        approved: bool,
    ) -> Result<usize, ClientError> {
        match self
            .send_request(Request::ResolvePermissionBatch { batch_id, approved })
            .await?
        {
            ResponsePayload::PermissionBatchResolved { resolved } => Ok(resolved),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List pending renderer-neutral tool exchanges.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_pending_tool_exchanges(
        &self,
    ) -> Result<Vec<PendingToolExchangeSummary>, ClientError> {
        match self.send_request(Request::ListPendingToolExchanges).await? {
            ResponsePayload::PendingToolExchangeList { exchanges } => Ok(exchanges),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resolve a pending renderer-neutral tool exchange.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn resolve_tool_exchange(
        &self,
        exchange_id: String,
        resolution: bcode_session_models::ToolExchangeResolution,
    ) -> Result<bool, ClientError> {
        match self
            .send_request(Request::ResolveToolExchange {
                exchange_id,
                resolution_json: serde_json::to_value(resolution).unwrap_or_else(|error| {
                    serde_json::json!({
                        "status": "failed",
                        "code": "resolution_encode_failed",
                        "message": error.to_string(),
                    })
                }),
            })
            .await?
        {
            ResponsePayload::ToolExchangeResolved { resolved } => Ok(resolved),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Persist and activate a permission policy rule under `[agent.<agent_id>.permission.<category>]`.
    ///
    /// `category` must be one of `command`, `read`, `write`, `edit`, or `web`.
    /// `action` must be one of `allow`, `ask`, or `deny`.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn add_permission_rule(
        &self,
        agent_id: String,
        category: String,
        pattern: String,
        action: String,
    ) -> Result<String, ClientError> {
        match self
            .send_request(Request::AddPermissionRule {
                agent_id,
                category,
                pattern,
                action,
            })
            .await?
        {
            ResponsePayload::PermissionRuleAdded { config_path } => Ok(config_path),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List services provided by loaded daemon plugins.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn plugin_services(&self) -> Result<Vec<PluginServiceSummary>, ClientError> {
        match self.send_request(Request::ListPluginServices).await? {
            ResponsePayload::PluginServices { services } => Ok(services),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List manifest-declared plugin contributions without executing plugin code.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn plugin_contributions(&self) -> Result<PluginContributions, ClientError> {
        match self.send_request(Request::ListPluginContributions).await? {
            ResponsePayload::PluginContributions { contributions } => Ok(contributions),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Capture a bounded stable source snapshot for generic derivation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the source cannot be read boundedly.
    pub async fn session_derivation_snapshot(
        &self,
        session_id: SessionId,
    ) -> Result<SessionDerivationSourceSnapshot, ClientError> {
        match self
            .send_request(Request::SessionDerivationSnapshot { session_id })
            .await?
        {
            ResponsePayload::SessionDerivationSnapshot { snapshot } => Ok(snapshot),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read one bounded generation-pinned page of derivation prompt candidates.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, the generation changed, or the query is
    /// invalid.
    pub async fn session_derivation_prompts(
        &self,
        session_id: SessionId,
        query: SessionDerivationPromptQuery,
    ) -> Result<SessionDerivationPromptPage, ClientError> {
        match self
            .send_request(Request::SessionDerivationPrompts { session_id, query })
            .await?
        {
            ResponsePayload::SessionDerivationPrompts { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Execute one generic session derivation request.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or derivation fails.
    pub async fn derive_session(
        &self,
        request: SessionDerivationRequest,
    ) -> Result<SessionDerivationTerminalOutcome, ClientError> {
        match self
            .send_request(Request::DeriveSession {
                request: Box::new(request),
            })
            .await?
        {
            ResponsePayload::SessionDerived { outcome } => Ok(outcome),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return the latest derivation operation status.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unknown.
    pub async fn session_derivation_status(
        &self,
        operation_id: bcode_session_models::SessionDerivationOperationId,
    ) -> Result<bcode_session_models::SessionDerivationOperationSnapshot, ClientError> {
        match self
            .send_request(Request::SessionDerivationStatus { operation_id })
            .await?
        {
            ResponsePayload::SessionDerivationStatus { snapshot } => Ok(snapshot),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Request cancellation of one running derivation operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn cancel_session_derivation(
        &self,
        operation_id: bcode_session_models::SessionDerivationOperationId,
    ) -> Result<bool, ClientError> {
        match self
            .send_request(Request::CancelSessionDerivation { operation_id })
            .await?
        {
            ResponsePayload::SessionDerivationCancellationRequested { accepted } => Ok(accepted),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Invoke a loaded daemon plugin service by explicit plugin ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn invoke_plugin_service(
        &self,
        plugin_id: String,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> Result<PluginServiceResponse, ClientError> {
        match self
            .send_request(Request::InvokePluginService {
                plugin_id,
                interface_id,
                operation,
                payload,
            })
            .await?
        {
            ResponsePayload::PluginServiceResult { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Invoke a loaded daemon plugin service by interface ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn call_plugin_service(
        &self,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> Result<PluginServiceResponse, ClientError> {
        match self
            .send_request(Request::CallPluginService {
                interface_id,
                operation,
                payload,
            })
            .await?
        {
            ResponsePayload::PluginServiceResult { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Publish an event to matching daemon plugin subscriptions.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn publish_plugin_event(
        &self,
        topic: String,
        payload: Vec<u8>,
    ) -> Result<usize, ClientError> {
        match self
            .send_request(Request::PublishPluginEvent { topic, payload })
            .await?
        {
            ResponsePayload::PluginEventPublished { delivered } => Ok(delivered),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    async fn send_request(&self, request: Request) -> Result<ResponsePayload, ClientError> {
        self.send_request_once(request).await
    }

    async fn send_request_with_timeout(
        &self,
        request: Request,
        request_timeout: Duration,
    ) -> Result<ResponsePayload, ClientError> {
        let mut connection = self.connect("bcode-cli").await?;
        connection.request_timeout = request_timeout;
        connection.send_request(request).await
    }

    async fn send_request_once(&self, request: Request) -> Result<ResponsePayload, ClientError> {
        let mut connection = self.connect("bcode-cli").await?;
        connection.send_request(request).await
    }

    /// Open a long-lived connection to the daemon.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, rejects the handshake, or reports a
    /// different build fingerprint.
    pub async fn connect(&self, client_name: &str) -> Result<ClientConnection, ClientError> {
        match self.connect_with_deadline(client_name).await {
            Ok(connection) => Ok(connection),
            Err(error)
                if self.daemon_availability == DaemonAvailability::AutoStart
                    && error.is_daemon_unavailable() =>
            {
                self.ensure_daemon_available().await?;
                self.connect_with_deadline(client_name).await
            }
            Err(error) => Err(error),
        }
    }

    async fn connect_with_deadline(
        &self,
        client_name: &str,
    ) -> Result<ClientConnection, ClientError> {
        tokio::time::timeout(self.connect_timeout, self.connect_once(client_name))
            .await
            .map_err(|_| ClientError::ConnectTimeout {
                timeout: self.connect_timeout,
            })?
    }

    /// Observe detached session-open preparation until terminal state or receiver drop.
    ///
    /// Dropping the returned receiver stops only this client observer. The server-owned migration
    /// continues independently.
    #[must_use]
    pub fn observe_session_open(&self, session_id: SessionId) -> SessionOpenProgressObserver {
        let client = self.clone();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut connection = client.connect("bcode-session-open-observer").await?;
            let close_sender = sender.clone();
            let observation = connection.prepare_session_open_while(session_id, |snapshot| {
                sender.send(snapshot.clone()).is_ok()
            });
            tokio::pin!(observation);
            tokio::select! {
                result = &mut observation => {
                    result?;
                }
                () = close_sender.closed() => {}
            }
            Ok(())
        });
        SessionOpenProgressObserver { receiver, task }
    }

    async fn connect_once(&self, client_name: &str) -> Result<ClientConnection, ClientError> {
        let stream = LocalIpcStream::connect(&self.endpoint).await?;
        let mut connection = ClientConnection {
            stream,
            next_request_id: 1,
            client_id: None,
            pending_events: VecDeque::new(),
            request_timeout: self.request_timeout,
            reconnect_client: Some(std::sync::Arc::new(self.clone())),
            reconnect_name: std::sync::Arc::from(client_name),
        };
        match connection
            .send_request(Request::Hello {
                client_name: format!("{client_name};cap=message_accepted"),
                runtime_context: self.runtime_context.clone(),
                daemon_namespace: bcode_ipc::daemon_namespace(),
                artifact_id: Some(bcode_ipc::ArtifactId::current()),
                build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.to_owned(),
            })
            .await?
        {
            ResponsePayload::Hello {
                client_id, daemon, ..
            } => {
                Self::verify_daemon_identity(&daemon)?;
                connection.client_id = Some(client_id);
                Ok(connection)
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }
}

/// Long-lived client connection.
#[derive(Debug)]
pub struct ClientConnection {
    stream: LocalIpcStream,
    next_request_id: u64,
    client_id: Option<ClientId>,
    pending_events: VecDeque<Event>,
    request_timeout: Duration,
    reconnect_client: Option<std::sync::Arc<BcodeClient>>,
    reconnect_name: std::sync::Arc<str>,
}

impl ClientConnection {
    /// Return the server-assigned client identifier.
    #[must_use]
    pub const fn client_id(&self) -> Option<ClientId> {
        self.client_id
    }

    /// Replace the runtime context attached to this long-lived connection.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn update_runtime_context(
        &mut self,
        runtime_context: Option<ClientRuntimeContext>,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::UpdateClientRuntimeContext { runtime_context })
            .await?
        {
            ResponsePayload::ClientRuntimeContextUpdated => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Refresh this long-lived connection's runtime context from the current process.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn refresh_runtime_context(&mut self) -> Result<(), ClientError> {
        self.update_runtime_context(Some(current_runtime_context()))
            .await
    }

    /// Subscribe this connection to catalog update events.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn subscribe_catalog_updates(&mut self) -> Result<(), ClientError> {
        match self.send_request(Request::SubscribeCatalogUpdates).await? {
            ResponsePayload::CatalogUpdatesSubscribed => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return a bounded page of workflow live notifications after one global sequence.
    ///
    /// A page marked `resync_required` must be replaced with bounded catalog/run snapshots rather
    /// than repeatedly paging and claiming durable stream resume.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn workflow_live_event_catch_up(
        &mut self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<bcode_workflow_view_models::WorkflowLiveEventPage, ClientError> {
        match self
            .send_request(Request::WorkflowLiveEventCatchUp {
                after_sequence,
                limit,
            })
            .await?
        {
            ResponsePayload::WorkflowLiveEventCatchUp { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Subscribe this connection to workflow-run canonical-state notifications.
    ///
    /// The stream is live-only and does not imply durable resume. Obtain bounded snapshots through
    /// [`Self::workflow_catalog_view`] and [`Self::workflow_run_view`].
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn subscribe_workflow_runs(&mut self) -> Result<u64, ClientError> {
        match self.send_request(Request::SubscribeWorkflowRuns).await? {
            ResponsePayload::WorkflowRunsSubscribed { after_sequence } => Ok(after_sequence),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Subscribe this connection to runtime-work events for one session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn subscribe_runtime_work(
        &mut self,
        session_id: SessionId,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::SubscribeRuntimeWork { session_id })
            .await?
        {
            ResponsePayload::RuntimeWorkSubscribed => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List sessions for the current working directory on this connection.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_sessions_with_status(&mut self) -> Result<SessionList, ClientError> {
        match self
            .send_request(Request::ListSessions {
                working_directory: current_working_directory(),
            })
            .await?
        {
            ResponsePayload::SessionList {
                sessions,
                catalog_status,
                catalog_sources,
                catalog_revision,
            } => Ok(SessionList {
                sessions,
                catalog_status,
                catalog_sources,
                catalog_revision,
            }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read one bounded, non-mutating session compatibility inventory page.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_compatibility_inventory(
        &mut self,
        request: SessionCompatibilityInventoryRequest,
    ) -> Result<SessionCompatibilityInventoryResponse, ClientError> {
        match self
            .send_request(Request::SessionCompatibilityInventory { request })
            .await?
        {
            ResponsePayload::SessionCompatibilityInventory { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Attach to a session and return replayed history.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn attach_session(
        &mut self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, ClientError> {
        self.attach_session_with_input_history(session_id)
            .await
            .map(|attached| attached.history)
    }

    /// Attach to a session and return replayed history plus input-history entries.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn attach_session_with_input_history(
        &mut self,
        session_id: SessionId,
    ) -> Result<AttachedSessionHistory, ClientError> {
        match self
            .send_request(Request::AttachSession { session_id })
            .await?
        {
            ResponsePayload::Attached {
                history,
                input_history,
                import_warnings,
                draft,
                runtime_selection,
                projection_window,
                session,
                ..
            } => Ok(AttachedSessionHistory {
                session,
                history,
                input_history,
                import_warnings,
                draft,
                runtime_selection,
                projection_window,
            }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Classify session storage and start or join legacy migration when required.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects preparation.
    pub async fn prepare_session_open(
        &mut self,
        session_id: SessionId,
    ) -> Result<bcode_session_models::SessionOpenOperationSnapshot, ClientError> {
        match self
            .send_request(Request::PrepareSessionOpen { session_id })
            .await?
        {
            ResponsePayload::SessionOpenPrepared { snapshot } => Ok(snapshot),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Wait for a newer session-open snapshot or a bounded server timeout.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, the operation identity is stale, or
    /// the request is rejected.
    pub async fn wait_session_open_progress(
        &mut self,
        session_id: SessionId,
        operation_id: bcode_session_models::SessionOpenOperationId,
        after_revision: u64,
        timeout: Duration,
    ) -> Result<bcode_session_models::SessionOpenOperationSnapshot, ClientError> {
        let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        match self
            .send_request(Request::WaitSessionOpenProgress {
                session_id,
                operation_id,
                after_revision,
                timeout_ms,
            })
            .await?
        {
            ResponsePayload::SessionOpenPrepared { snapshot } => Ok(snapshot),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Prepare a session until it reaches a terminal state, invoking `on_progress` for every
    /// observed snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when preparation or progress observation fails.
    pub async fn prepare_session_open_until_terminal<F>(
        &mut self,
        session_id: SessionId,
        mut on_progress: F,
    ) -> Result<bcode_session_models::SessionOpenOperationSnapshot, ClientError>
    where
        F: FnMut(&bcode_session_models::SessionOpenOperationSnapshot),
    {
        self.prepare_session_open_while(session_id, |snapshot| {
            on_progress(snapshot);
            true
        })
        .await
    }

    async fn prepare_session_open_while<F>(
        &mut self,
        session_id: SessionId,
        mut on_progress: F,
    ) -> Result<bcode_session_models::SessionOpenOperationSnapshot, ClientError>
    where
        F: FnMut(&bcode_session_models::SessionOpenOperationSnapshot) -> bool,
    {
        let mut snapshot = self.prepare_session_open(session_id).await?;
        if !on_progress(&snapshot) {
            return Ok(snapshot);
        }
        let mut reconnect_attempts = 0_u8;
        while snapshot.outcome.is_none() {
            match self
                .wait_session_open_progress(
                    session_id,
                    snapshot.operation_id,
                    snapshot.revision,
                    Duration::from_secs(5),
                )
                .await
            {
                Ok(next) => {
                    snapshot = next;
                    if !on_progress(&snapshot) {
                        return Ok(snapshot);
                    }
                }
                Err(error)
                    if error.is_daemon_unavailable()
                        && reconnect_attempts < 3
                        && self.reconnect_client.is_some() =>
                {
                    reconnect_attempts = reconnect_attempts.saturating_add(1);
                    self.reconnect_for_session_open().await?;
                    snapshot = match self
                        .wait_session_open_progress(
                            session_id,
                            snapshot.operation_id,
                            snapshot.revision,
                            Duration::ZERO,
                        )
                        .await
                    {
                        Ok(recovered) => recovered,
                        Err(ClientError::Server { code, .. })
                            if code == "session_open_operation_not_found" =>
                        {
                            self.prepare_session_open(session_id).await?
                        }
                        Err(error) => return Err(error),
                    };
                    if !on_progress(&snapshot) {
                        return Ok(snapshot);
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(snapshot)
    }

    async fn reconnect_for_session_open(&mut self) -> Result<(), ClientError> {
        let client = self
            .reconnect_client
            .clone()
            .ok_or(ClientError::UnexpectedResponse)?;
        let mut replacement = client.connect(&self.reconnect_name).await?;
        let mut pending_events = std::mem::take(&mut self.pending_events);
        pending_events.append(&mut replacement.pending_events);
        replacement.pending_events = pending_events;
        *self = replacement;
        Ok(())
    }

    /// Attach to a session and return a recent history window.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn attach_session_recent(
        &mut self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, ClientError> {
        self.attach_session_recent_with_input_history(session_id, limit)
            .await
            .map(|attached| attached.history)
    }

    /// Attach to a session and return a recent history window plus input-history entries.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn attach_session_recent_with_input_history(
        &mut self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<AttachedSessionHistory, ClientError> {
        match self
            .send_request(Request::AttachSessionRecent { session_id, limit })
            .await?
        {
            ResponsePayload::Attached {
                history,
                input_history,
                import_warnings,
                draft,
                runtime_selection,
                projection_window,
                session,
                ..
            } => Ok(AttachedSessionHistory {
                session,
                history,
                input_history,
                import_warnings,
                draft,
                runtime_selection,
                projection_window,
            }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Prepare a session to a terminal state, then attach with a bounded projection window.
    ///
    /// # Errors
    ///
    /// Returns an error when preparation fails, reaches a terminal state that cannot be attached,
    /// or attach fails. Ready states use the bounded attach path; degraded/read-only and all other
    /// non-ready terminal states return without attaching.
    pub async fn prepare_then_attach_session_projection_window<F>(
        &mut self,
        session_id: SessionId,
        request: bcode_session_models::ProjectionWindowRequest,
        on_progress: F,
    ) -> Result<AttachedSessionHistory, ClientError>
    where
        F: FnMut(&bcode_session_models::SessionOpenOperationSnapshot),
    {
        let snapshot = self
            .prepare_session_open_until_terminal(session_id, on_progress)
            .await?;
        session_open_attach_readiness(&snapshot)?;
        self.attach_session_projection_window_with_input_history(session_id, request)
            .await
    }

    /// Attach to a session and return a projection-sized history window plus input-history entries.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn attach_session_projection_window_with_input_history(
        &mut self,
        session_id: SessionId,
        request: ProjectionWindowRequest,
    ) -> Result<AttachedSessionHistory, ClientError> {
        match self
            .send_request(Request::AttachSessionProjectionWindow {
                session_id,
                request,
            })
            .await?
        {
            ResponsePayload::Attached {
                history,
                input_history,
                import_warnings,
                draft,
                runtime_selection,
                projection_window,
                session,
                ..
            } => Ok(AttachedSessionHistory {
                session,
                history,
                input_history,
                import_warnings,
                draft,
                runtime_selection,
                projection_window,
            }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Send a user message to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn send_user_message(
        &mut self,
        session_id: SessionId,
        text: String,
        placement: bcode_ipc::PromptPlacement,
    ) -> Result<MessageAcceptance, ClientError> {
        match self
            .send_request(Request::SendUserMessageWithPlacement {
                session_id,
                text,
                placement,
            })
            .await?
        {
            ResponsePayload::MessageAccepted {
                queued,
                queue_position,
            } => Ok(MessageAcceptance {
                queued,
                queue_position,
                disposition: bcode_ipc::MessageAcceptanceDisposition::StartedTurn,
            }),
            ResponsePayload::MessageAcceptedWithDisposition {
                queued,
                queue_position,
                disposition,
            } => Ok(MessageAcceptance {
                queued,
                queue_position,
                disposition,
            }),
            ResponsePayload::MessageSent => Ok(MessageAcceptance::sent()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Receive the next server event.
    ///
    /// # Errors
    ///
    /// Returns an error when the connection closes or the event cannot be decoded.
    pub async fn recv_event(&mut self) -> Result<Event, ClientError> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(event);
        }
        loop {
            let envelope = recv_envelope(&mut self.stream).await?;
            if envelope.kind != EnvelopeKind::Event {
                continue;
            }
            return decode_event(&envelope.payload).map_err(ClientError::from);
        }
    }

    async fn send_request(&mut self, request: Request) -> Result<ResponsePayload, ClientError> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        let envelope = request_envelope(request_id, &request)?;
        send_envelope(&mut self.stream, &envelope).await?;

        loop {
            let envelope =
                tokio::time::timeout(self.request_timeout, recv_envelope(&mut self.stream))
                    .await
                    .map_err(|_| ClientError::RequestTimeout {
                        timeout: self.request_timeout,
                    })??;
            if envelope.kind == EnvelopeKind::Event {
                self.pending_events
                    .push_back(decode_event(&envelope.payload).map_err(ClientError::from)?);
                continue;
            }
            if envelope.kind != EnvelopeKind::Response || envelope.request_id != request_id {
                continue;
            }
            let response: Response = decode_response(&envelope.payload)?;
            return match response {
                Response::Ok(payload) => Ok(payload),
                Response::Err(error) => Err(error.into()),
            };
        }
    }
}

fn session_open_attach_readiness(
    snapshot: &bcode_session_models::SessionOpenOperationSnapshot,
) -> Result<(), ClientError> {
    let session_id = snapshot.session_id;
    let stage_message = &snapshot.progress.message;
    let verified_backup_path = snapshot.backup_path.as_deref();
    match &snapshot.outcome {
        Some(bcode_session_models::SessionOpenTerminalOutcome::Ready) => Ok(()),
        Some(bcode_session_models::SessionOpenTerminalOutcome::DegradedReadOnly {
            issue_count,
        }) => Err(ClientError::Server {
            code: "session_degraded_read_only".to_owned(),
            message: format!(
                "session contains {issue_count} unsupported persisted event(s); bounded history remains inspectable but writable attach is disabled"
            ),
        }),
        Some(bcode_session_models::SessionOpenTerminalOutcome::WriterIncompatible {
            actual,
            expected,
        }) => Err(ClientError::Server {
            code: "session_writer_incompatible".to_owned(),
            message: terminal_session_open_error_message(
                session_id,
                stage_message,
                &format!(
                    "session writer epoch {actual:?} is incompatible with expected epoch {expected}"
                ),
                verified_backup_path,
            ),
        }),
        Some(bcode_session_models::SessionOpenTerminalOutcome::RepairRequired { reason }) => {
            Err(ClientError::Server {
                code: "session_repair_required".to_owned(),
                message: terminal_session_open_error_message(
                    session_id,
                    stage_message,
                    reason,
                    verified_backup_path,
                ),
            })
        }
        Some(bcode_session_models::SessionOpenTerminalOutcome::Failed {
            kind,
            message,
            backup_path,
        }) => Err(ClientError::Server {
            code: session_open_failure_code(*kind).to_owned(),
            message: terminal_session_open_error_message(
                session_id,
                stage_message,
                message,
                backup_path.as_deref().or(verified_backup_path),
            ),
        }),
        None => Err(ClientError::UnexpectedResponse),
    }
}

fn validate_bounded_long_poll_timeout(
    operation: &'static str,
    timeout_ms: u64,
) -> Result<(), ClientError> {
    if timeout_ms == 0 || timeout_ms > 30_000 {
        return Err(ClientError::Protocol(format!(
            "{operation} wait timeout must be between 1 and 30000 milliseconds"
        )));
    }
    Ok(())
}

fn validate_session_search_backfill_wait_timeout(timeout_ms: u64) -> Result<(), ClientError> {
    validate_bounded_long_poll_timeout("backfill", timeout_ms)
}

fn validate_session_bulk_migration_wait_timeout(timeout_ms: u64) -> Result<(), ClientError> {
    validate_bounded_long_poll_timeout("bulk migration", timeout_ms)
}

fn terminal_session_open_error_message(
    session_id: SessionId,
    stage_message: &str,
    reason: &str,
    backup_path: Option<&std::path::Path>,
) -> String {
    let backup = backup_path.map_or_else(String::new, |path| {
        format!(" Retained backup: {}.", path.display())
    });
    format!(
        "session preparation failed during {stage_message}: {reason}.{backup} Diagnose with `bcode session diagnose {session_id}`."
    )
}

const fn session_open_failure_code(
    kind: bcode_session_models::SessionOpenFailureKind,
) -> &'static str {
    match kind {
        bcode_session_models::SessionOpenFailureKind::OwnedByOtherDaemon => {
            "session_active_elsewhere"
        }
        bcode_session_models::SessionOpenFailureKind::WriterIncompatible => {
            "session_writer_incompatible"
        }
        bcode_session_models::SessionOpenFailureKind::ProjectionStale => "projection_stale",
        bcode_session_models::SessionOpenFailureKind::RepairRequired => "session_repair_required",
        bcode_session_models::SessionOpenFailureKind::BackupFailed => {
            "session_migration_backup_failed"
        }
        bcode_session_models::SessionOpenFailureKind::MigrationFailed => "session_migration_failed",
        bcode_session_models::SessionOpenFailureKind::NotFound => "session_not_found",
    }
}

#[cfg(test)]
mod client_timeout_tests {
    use super::{
        BcodeClient, ClientError, resolve_path_from, session_open_attach_readiness,
        terminal_session_open_error_message,
    };
    use bcode_session_models::{
        SessionId, SessionMigrationProgress, SessionMigrationStage, SessionOpenOperationId,
        SessionOpenOperationSnapshot, SessionOpenTerminalOutcome,
    };
    use bcode_session_search::{
        SessionSearchBackfillOperationState, SessionSearchBackfillOperationStatus,
    };
    use std::path::Path;
    use std::time::Duration;

    fn matching_daemon_status() -> bcode_ipc::DaemonStatus {
        let (_path, digest) = bcode_daemon_lifecycle::current_executable_identity()
            .expect("current executable identity");
        bcode_ipc::DaemonStatus {
            namespace: bcode_ipc::daemon_namespace(),
            protocol_version: u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION),
            artifact_id: Some(bcode_ipc::ArtifactId::current()),
            build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.to_owned(),
            executable_digest: Some(digest),
            storage_writer_epoch: Some(bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
            session_event_schema_version: Some(
                bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            ),
            ..bcode_ipc::DaemonStatus::default()
        }
    }

    #[test]
    fn daemon_identity_accepts_same_artifact_with_different_executable_digest() {
        let matching = matching_daemon_status();
        let resigned = bcode_ipc::DaemonStatus {
            executable_digest: Some("different-signed-executable-digest".to_owned()),
            ..matching
        };

        BcodeClient::verify_daemon_identity(&resigned)
            .expect("executable digest is diagnostic, not a compatibility boundary");
    }

    #[test]
    fn daemon_identity_matrix_rejects_every_incompatible_capability() {
        let matching = matching_daemon_status();
        BcodeClient::verify_daemon_identity(&matching).expect("matching daemon");

        let cases = [
            bcode_ipc::DaemonStatus {
                artifact_id: Some(
                    bcode_ipc::ArtifactId::parse("other-artifact")
                        .expect("other artifact identity"),
                ),
                ..matching.clone()
            },
            bcode_ipc::DaemonStatus {
                protocol_version: matching.protocol_version.saturating_add(1),
                ..matching.clone()
            },
            bcode_ipc::DaemonStatus {
                build_fingerprint: "other-build".to_owned(),
                ..matching.clone()
            },
            bcode_ipc::DaemonStatus {
                storage_writer_epoch: matching.storage_writer_epoch.map(|value| value + 1),
                ..matching.clone()
            },
            bcode_ipc::DaemonStatus {
                session_event_schema_version: matching
                    .session_event_schema_version
                    .map(|value| value + 1),
                ..matching.clone()
            },
            bcode_ipc::DaemonStatus {
                storage_writer_epoch: None,
                session_event_schema_version: None,
                ..matching
            },
        ];
        for daemon in cases {
            let error = BcodeClient::verify_daemon_identity(&daemon)
                .expect_err("incompatible capability must fail before requests");
            let ClientError::IncompatibleDaemon { message } = error else {
                panic!("expected incompatible daemon");
            };
            assert!(message.contains("artifact="));
            assert!(message.contains("session_event_schema="));
            assert!(message.contains("storage_writer_epoch="));
            assert!(message.contains("protocol="));
            assert!(message.contains("build="));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn custom_endpoint_warm_handshake_uses_one_connection_without_startup() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcw-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("warm.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let expected_client_id = bcode_session_models::ClientId::new();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let hello = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("receive hello");
            assert!(matches!(
                bcode_ipc::decode_request(&hello.payload).expect("decode hello"),
                bcode_ipc::Request::Hello { artifact_id: Some(artifact_id), .. }
                    if artifact_id == bcode_ipc::ArtifactId::current()
            ));
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion::current(),
                client_id: expected_client_id,
                daemon: matching_daemon_status(),
            });
            let envelope =
                bcode_ipc::response_envelope(hello.request_id, &response).expect("hello response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send hello");
        });
        let client = BcodeClient::new(endpoint);

        let connection = client
            .connect("custom-warm-test")
            .await
            .expect("warm connect");
        assert_eq!(connection.client_id(), Some(expected_client_id));

        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_custom_endpoint_requires_running_without_auto_start() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcm-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let socket_path = socket_dir.join("missing.sock");
        let client = BcodeClient::new(bcode_ipc::IpcEndpoint::unix_socket(socket_path.clone()));

        let error = client
            .connect("custom-missing-test")
            .await
            .expect_err("custom endpoint must require a running daemon");
        assert!(error.is_daemon_unavailable());
        assert!(!socket_path.exists());

        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connection_timeout_is_distinct_and_does_not_trigger_auto_start() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcc-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("connect-timeout.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let _hello = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("receive hello");
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let client = BcodeClient::new(endpoint)
            .with_daemon_availability(super::DaemonAvailability::AutoStart)
            .with_connect_timeout(Duration::from_millis(10))
            .with_request_timeout(Duration::from_secs(1));

        let error = client
            .connect("connect-timeout-test")
            .await
            .expect_err("unresponsive reachable endpoint must time out");
        assert!(matches!(
            error,
            ClientError::ConnectTimeout { timeout } if timeout == Duration::from_millis(10)
        ));
        assert!(!error.is_daemon_unavailable());

        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mismatched_artifact_hello_is_rejected_explicitly() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bci-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("artifact.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let hello = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("hello request");
            let daemon = bcode_ipc::DaemonStatus {
                artifact_id: Some(
                    bcode_ipc::ArtifactId::parse("foreign-artifact")
                        .expect("foreign artifact identity"),
                ),
                ..matching_daemon_status()
            };
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion(bcode_ipc::CURRENT_PROTOCOL_VERSION),
                client_id: bcode_session_models::ClientId::new(),
                daemon,
            });
            let envelope = bcode_ipc::response_envelope(hello.request_id, &response)
                .expect("hello response envelope");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send hello response");
        });

        let client = BcodeClient::new(endpoint)
            .with_daemon_availability(super::DaemonAvailability::RequireRunning);
        let error = client
            .connect("artifact-mismatch-test")
            .await
            .expect_err("foreign artifact must be rejected");
        assert!(matches!(error, ClientError::IncompatibleDaemon { .. }));
        assert!(error.to_string().contains("foreign-artifact"));

        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unrelated_events_remain_buffered_in_fifo_order_during_requests() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bce-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("client.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let request = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("request envelope");
            for revision in [11, 12] {
                let event = bcode_ipc::Event::SessionCatalogUpdated { revision };
                let envelope = bcode_ipc::event_envelope(&event).expect("event envelope");
                bcode_ipc::send_envelope(&mut stream, &envelope)
                    .await
                    .expect("send event");
            }
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Pong);
            let envelope = bcode_ipc::response_envelope(request.request_id, &response)
                .expect("response envelope");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send response");
        });
        let stream = bcode_ipc::LocalIpcStream::connect(&endpoint)
            .await
            .expect("connect");
        let mut connection = super::ClientConnection {
            stream,
            next_request_id: 1,
            client_id: None,
            pending_events: std::collections::VecDeque::new(),
            request_timeout: Duration::from_secs(1),
            reconnect_client: None,
            reconnect_name: std::sync::Arc::from(""),
        };

        assert!(matches!(
            connection.send_request(bcode_ipc::Request::Ping).await,
            Ok(bcode_ipc::ResponsePayload::Pong)
        ));
        for expected in [11, 12] {
            assert_eq!(
                connection.recv_event().await.expect("buffered event"),
                bcode_ipc::Event::SessionCatalogUpdated { revision: expected }
            );
        }
        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("event socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn long_poll_transport_timeout_is_distinct_from_operation_failure() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bct-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("timeout.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let _request = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("wait request");
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let stream = bcode_ipc::LocalIpcStream::connect(&endpoint)
            .await
            .expect("connect");
        let mut connection = super::ClientConnection {
            stream,
            next_request_id: 1,
            client_id: None,
            pending_events: std::collections::VecDeque::new(),
            request_timeout: Duration::from_millis(10),
            reconnect_client: None,
            reconnect_name: std::sync::Arc::from(""),
        };
        let session_id = SessionId::new();

        assert!(matches!(
            connection
                .wait_session_open_progress(
                    session_id,
                    SessionOpenOperationId::new(),
                    0,
                    Duration::from_secs(5),
                )
                .await,
            Err(ClientError::RequestTimeout { timeout })
                if timeout == Duration::from_millis(10)
        ));
        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("timeout socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preparation_recovers_retained_operation_after_transport_interruption() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcr-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("reconnect.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let session_id = SessionId::new();
        let operation_id = SessionOpenOperationId::new();
        let snapshot = |revision, terminal| SessionOpenOperationSnapshot {
            operation_id,
            revision,
            session_id,
            source_writer_epoch: Some(3),
            target_writer_epoch: 4,
            progress: SessionMigrationProgress {
                stage: if terminal {
                    SessionMigrationStage::Complete
                } else {
                    SessionMigrationStage::CopyingBackup
                },
                completed_units: Some(revision),
                total_units: Some(2),
                unit: Some(bcode_session_models::SessionMigrationProgressUnit::Files),
                message: "migration".to_owned(),
            },
            outcome: terminal.then_some(SessionOpenTerminalOutcome::Ready),
            backup_path: None,
        };
        let initial = snapshot(1, false);
        let terminal = snapshot(2, true);
        let server_terminal = terminal.clone();
        let daemon = matching_daemon_status();
        let server = tokio::spawn(async move {
            for (connection_index, prepared) in [initial, server_terminal].into_iter().enumerate() {
                let mut stream = listener.accept().await.expect("accept client");
                let hello = bcode_ipc::recv_envelope(&mut stream).await.expect("hello");
                let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                    protocol_version: bcode_ipc::ProtocolVersion(
                        bcode_ipc::CURRENT_PROTOCOL_VERSION,
                    ),
                    client_id: bcode_session_models::ClientId::new(),
                    daemon: daemon.clone(),
                });
                let envelope = bcode_ipc::response_envelope(hello.request_id, &response)
                    .expect("hello response");
                bcode_ipc::send_envelope(&mut stream, &envelope)
                    .await
                    .expect("send hello");

                let request = bcode_ipc::recv_envelope(&mut stream)
                    .await
                    .expect("preparation request");
                let response =
                    bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::SessionOpenPrepared {
                        snapshot: prepared,
                    });
                let envelope = bcode_ipc::response_envelope(request.request_id, &response)
                    .expect("preparation response");
                bcode_ipc::send_envelope(&mut stream, &envelope)
                    .await
                    .expect("send preparation");
                if connection_index == 0 {
                    let _wait = bcode_ipc::recv_envelope(&mut stream)
                        .await
                        .expect("wait request before disconnect");
                }
            }
        });
        let client = BcodeClient::new(endpoint).with_request_timeout(Duration::from_secs(1));
        let mut connection = client.connect("reconnect-test").await.expect("connect");
        let mut revisions = Vec::new();

        let recovered = connection
            .prepare_session_open_until_terminal(session_id, |snapshot| {
                revisions.push(snapshot.revision);
            })
            .await
            .expect("recover preparation");

        assert_eq!(recovered, terminal);
        assert_eq!(revisions, vec![1, 2]);
        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_progress_receiver_stops_client_observation_cleanly() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcd-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("drop.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let session_id = SessionId::new();
        let snapshot = SessionOpenOperationSnapshot {
            operation_id: SessionOpenOperationId::new(),
            revision: 1,
            session_id,
            source_writer_epoch: Some(3),
            target_writer_epoch: 4,
            progress: SessionMigrationProgress {
                stage: SessionMigrationStage::CopyingBackup,
                completed_units: Some(1),
                total_units: Some(2),
                unit: Some(bcode_session_models::SessionMigrationProgressUnit::Files),
                message: "migration".to_owned(),
            },
            outcome: None,
            backup_path: None,
        };
        let daemon = matching_daemon_status();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let hello = bcode_ipc::recv_envelope(&mut stream).await.expect("hello");
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion(bcode_ipc::CURRENT_PROTOCOL_VERSION),
                client_id: bcode_session_models::ClientId::new(),
                daemon,
            });
            let envelope =
                bcode_ipc::response_envelope(hello.request_id, &response).expect("hello response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send hello");
            let request = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("prepare request");
            let response =
                bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::SessionOpenPrepared {
                    snapshot,
                });
            let envelope = bcode_ipc::response_envelope(request.request_id, &response)
                .expect("prepare response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send prepare");
            let first = tokio::time::timeout(
                Duration::from_millis(250),
                bcode_ipc::recv_envelope(&mut stream),
            )
            .await;
            if first.as_ref().is_ok_and(Result::is_ok) {
                tokio::time::timeout(
                    Duration::from_millis(250),
                    bcode_ipc::recv_envelope(&mut stream),
                )
                .await
            } else {
                first
            }
        });
        let client = BcodeClient::new(endpoint).with_request_timeout(Duration::from_secs(1));
        let mut observer = client.observe_session_open(session_id);
        let first = observer.receiver.recv().await.expect("initial progress");
        assert_eq!(first.revision, 1);
        drop(observer.receiver);
        assert!(observer.task.await.expect("observer task").is_ok());
        let next_request = server.await.expect("server task");
        assert!(
            next_request.is_ok_and(|request| request.is_err()),
            "observer sent another wait request after receiver drop"
        );
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn application_request_is_not_replayed_after_response_eof() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcd-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("single-send.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let daemon = matching_daemon_status();
        let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_accepted = std::sync::Arc::clone(&accepted);
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            server_accepted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let hello = bcode_ipc::recv_envelope(&mut stream).await.expect("hello");
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion(bcode_ipc::CURRENT_PROTOCOL_VERSION),
                client_id: bcode_session_models::ClientId::new(),
                daemon,
            });
            let envelope =
                bcode_ipc::response_envelope(hello.request_id, &response).expect("hello response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send hello");
            let request = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("application request");
            assert!(matches!(
                bcode_ipc::decode_request(&request.payload).expect("decode request"),
                bcode_ipc::Request::Ping
            ));
            drop(stream);
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        let client = BcodeClient::new(endpoint)
            .with_daemon_availability(super::DaemonAvailability::AutoStart)
            .with_request_timeout(Duration::from_secs(1));
        let error = client.ping().await.expect_err("response EOF must fail");
        assert!(matches!(error, ClientError::Codec(_)));
        server.await.expect("server task");
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1);
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[test]
    fn only_ready_terminal_outcome_allows_writable_attach() {
        let session_id = SessionId::new();
        let snapshot = |outcome| SessionOpenOperationSnapshot {
            operation_id: SessionOpenOperationId::new(),
            revision: 1,
            session_id,
            source_writer_epoch: Some(3),
            target_writer_epoch: 4,
            progress: SessionMigrationProgress {
                stage: SessionMigrationStage::Failed,
                completed_units: None,
                total_units: None,
                unit: None,
                message: "Classifying session".to_owned(),
            },
            outcome: Some(outcome),
            backup_path: Some("/tmp/backup".into()),
        };

        assert!(
            session_open_attach_readiness(&snapshot(SessionOpenTerminalOutcome::Ready)).is_ok()
        );
        for (outcome, expected_code) in [
            (
                SessionOpenTerminalOutcome::DegradedReadOnly { issue_count: 1 },
                "session_degraded_read_only",
            ),
            (
                SessionOpenTerminalOutcome::WriterIncompatible {
                    actual: Some(5),
                    expected: 4,
                },
                "session_writer_incompatible",
            ),
            (
                SessionOpenTerminalOutcome::RepairRequired {
                    reason: "damaged tail".to_owned(),
                },
                "session_repair_required",
            ),
            (
                SessionOpenTerminalOutcome::Failed {
                    kind: bcode_session_models::SessionOpenFailureKind::BackupFailed,
                    message: "backup failed".to_owned(),
                    backup_path: Some("/tmp/failed-backup".into()),
                },
                "session_migration_backup_failed",
            ),
        ] {
            assert!(matches!(
                session_open_attach_readiness(&snapshot(outcome)),
                Err(ClientError::Server { code, .. }) if code == expected_code
            ));
        }
    }

    #[test]
    fn terminal_session_open_error_preserves_recovery_context() {
        let session_id = SessionId::new();
        let message = terminal_session_open_error_message(
            session_id,
            "Verifying retained backup",
            "hash mismatch",
            Some(std::path::Path::new("/tmp/session-backup")),
        );

        assert!(message.contains("Verifying retained backup"));
        assert!(message.contains("hash mismatch"));
        assert!(message.contains("/tmp/session-backup"));
        assert!(message.contains(&format!("bcode session diagnose {session_id}")));
    }

    #[test]
    fn caller_paths_are_absolute_and_relative_paths_use_the_caller_cwd() {
        let caller_cwd = Path::new("/tmp/bcode-client-cwd");

        assert_eq!(
            resolve_path_from(None, caller_cwd),
            caller_cwd.to_path_buf()
        );
        assert_eq!(
            resolve_path_from(Some("nested".into()), caller_cwd),
            caller_cwd.join("nested")
        );
        assert_eq!(
            resolve_path_from(Some("/tmp/explicit".into()), caller_cwd),
            Path::new("/tmp/explicit")
        );
    }

    #[test]
    fn default_endpoint_honors_process_config_override() {
        let guard = bcode_config::push_process_config_overrides(
            bcode_config::ConfigLoadOverrides::from_env_with_cli(
                None,
                Some("[client]\nrequest_timeout_secs = 23\n".to_owned()),
            ),
        );

        let client = BcodeClient::default_endpoint();

        assert_eq!(client.request_timeout(), Duration::from_secs(23));
        drop(guard);
    }

    #[test]
    fn bounded_long_poll_timeouts_use_server_bound_plus_transport_grace() {
        let client =
            BcodeClient::default_endpoint().with_request_timeout(Duration::from_millis(10));
        let response_timeout = client
            .request_timeout()
            .max(Duration::from_secs(30).saturating_add(super::LONG_POLL_TRANSPORT_GRACE));

        assert_eq!(response_timeout, Duration::from_secs(35));
        assert!(super::validate_session_search_backfill_wait_timeout(1).is_ok());
        assert!(super::validate_session_search_backfill_wait_timeout(30_000).is_ok());
        assert!(matches!(
            super::validate_session_search_backfill_wait_timeout(30_001),
            Err(ClientError::Protocol(_))
        ));
        assert!(super::validate_session_bulk_migration_wait_timeout(1).is_ok());
        assert!(super::validate_session_bulk_migration_wait_timeout(30_000).is_ok());
        assert!(matches!(
            super::validate_session_bulk_migration_wait_timeout(30_001),
            Err(ClientError::Protocol(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backfill_wait_longer_than_default_request_timeout_receives_quiet_response() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcl-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("long-poll.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let operation_id = "quiet-backfill".to_owned();
        let expected_operation_id = operation_id.clone();
        let daemon = matching_daemon_status();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let hello = bcode_ipc::recv_envelope(&mut stream).await.expect("hello");
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion::current(),
                client_id: bcode_session_models::ClientId::new(),
                daemon,
            });
            let envelope =
                bcode_ipc::response_envelope(hello.request_id, &response).expect("hello response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send hello");

            let request = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("wait request");
            assert!(matches!(
                bcode_ipc::decode_request(&request.payload).expect("decode wait request"),
                bcode_ipc::Request::SessionSearchBackfillWait {
                    operation_id,
                    after_revision: 1,
                    timeout_ms: 80,
                } if operation_id == expected_operation_id
            ));
            tokio::time::sleep(Duration::from_millis(40)).await;
            let response = bcode_ipc::Response::Ok(
                bcode_ipc::ResponsePayload::SessionSearchBackfillOperation {
                    status: SessionSearchBackfillOperationStatus {
                        operation_id: expected_operation_id,
                        provider_id: "bcode.test-search".to_owned(),
                        revision: 1,
                        state: SessionSearchBackfillOperationState::Running,
                        response: None,
                        complete_progress: None,
                        complete_response: None,
                        error: None,
                    },
                },
            );
            let envelope =
                bcode_ipc::response_envelope(request.request_id, &response).expect("wait response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send wait response");
        });
        let client = BcodeClient::new(endpoint)
            .with_daemon_availability(super::DaemonAvailability::RequireRunning)
            .with_request_timeout(Duration::from_millis(10));

        let status = client
            .session_search_backfill_wait(operation_id, 1, 80)
            .await
            .expect("long poll must outlive generic request timeout");

        assert_eq!(status.revision, 1);
        assert_eq!(status.state, SessionSearchBackfillOperationState::Running);
        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bulk_migration_wait_longer_than_default_request_timeout_receives_quiet_response() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcmw-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("migration-wait.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let operation_id = "quiet-migration".to_owned();
        let expected_operation_id = operation_id.clone();
        let daemon = matching_daemon_status();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let hello = bcode_ipc::recv_envelope(&mut stream).await.expect("hello");
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion::current(),
                client_id: bcode_session_models::ClientId::new(),
                daemon,
            });
            let envelope =
                bcode_ipc::response_envelope(hello.request_id, &response).expect("hello response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send hello");

            let request = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("wait request");
            assert!(matches!(
                bcode_ipc::decode_request(&request.payload).expect("decode wait request"),
                bcode_ipc::Request::SessionBulkMigrationWait {
                    operation_id,
                    after_revision: 2,
                    timeout_ms: 80,
                } if operation_id == expected_operation_id
            ));
            tokio::time::sleep(Duration::from_millis(40)).await;
            let response = bcode_ipc::Response::Ok(
                bcode_ipc::ResponsePayload::SessionBulkMigrationOperation {
                    status: bcode_ipc::SessionBulkMigrationOperationStatus {
                        operation_id: expected_operation_id,
                        revision: 2,
                        state: bcode_ipc::SessionBulkMigrationState::Running,
                        mode: bcode_ipc::SessionBulkMigrationMode::Inventory,
                        selected: 0,
                        visited: 0,
                        migrated: 0,
                        blocked: 0,
                        failed: 0,
                        current_session_id: None,
                        outcomes: Vec::new(),
                    },
                },
            );
            let envelope =
                bcode_ipc::response_envelope(request.request_id, &response).expect("wait response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send wait response");
        });
        let client = BcodeClient::new(endpoint)
            .with_daemon_availability(super::DaemonAvailability::RequireRunning)
            .with_request_timeout(Duration::from_millis(10));

        let status = client
            .wait_session_bulk_migration(operation_id, 2, 80)
            .await
            .expect("long poll must outlive generic request timeout");

        assert_eq!(status.revision, 2);
        assert_eq!(status.state, bcode_ipc::SessionBulkMigrationState::Running);
        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[test]
    fn request_timeout_can_be_overridden() {
        let client = BcodeClient::default_endpoint().with_request_timeout(Duration::from_secs(17));

        assert_eq!(client.request_timeout(), Duration::from_secs(17));
    }
}
