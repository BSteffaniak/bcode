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
    ResponsePayload, ServerStopMode, SessionCatalogSourceStatus, SessionCatalogStatus,
    SessionImportWarning, WorktreeCreateRequest, WorktreeCreateResponse, WorktreeListRequest,
    WorktreeListResponse, WorktreeRemoveRequest, WorktreeRemoveResponse, current_working_directory,
    decode_event, decode_response, default_endpoint, recv_envelope, request_envelope,
    send_envelope,
};
use bcode_session_models::{
    ClientId, ProjectionWindowRequest, RuntimeWorkStatus, SessionEvent, SessionEventKind,
    SessionForkResult, SessionHistoryPage, SessionHistoryQuery, SessionId,
    SessionInputHistoryEntry, SessionSummary, WorkId,
};
use bcode_skill_models::{SkillId, SkillList, SkillManifest};
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;
use thiserror::Error;

const DEFAULT_CLIENT_IPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CLIENT_DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_DAEMON_RETRY_DELAY: Duration = Duration::from_millis(50);

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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    #[error("client request timed out after {timeout:?}")]
    RequestTimeout { timeout: Duration },
    #[error("daemon executable identity mismatch: {message}")]
    IncompatibleDaemon { message: String },
    #[error("unexpected response payload")]
    UnexpectedResponse,
    #[error("unexpected IPC envelope kind")]
    UnexpectedEnvelope,
}

impl ClientError {
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
            Self::RequestTimeout { .. } | Self::DaemonStart(_) => true,
            Self::Transport(_)
            | Self::Codec(_)
            | Self::Server { .. }
            | Self::IncompatibleDaemon { .. }
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
    let mut candidates = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    if let Some(primary_auth_profile) = primary_auth_profile {
        push_config_auth_candidate(
            config,
            primary_auth_profile,
            env,
            &mut candidates,
            &mut seen,
        );
    }
    if let Some(auth_pool) = config.auth.pools.get(auth_pool_name) {
        for auth_profile_name in &auth_pool.profiles {
            push_config_auth_candidate(config, auth_profile_name, env, &mut candidates, &mut seen);
        }
    }
    let registry = bcode_config::load_runtime_auth_subscriptions();
    if let Some(pool) = registry.pools.get(auth_pool_name) {
        for profile in &pool.profiles {
            if seen.contains(&profile.auth_profile) {
                continue;
            }
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
                Event::SessionCatalogUpdated { .. } | Event::SessionViewResyncRequired { .. } => {}
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
                | Event::SessionViewResyncRequired { .. }
                | Event::SessionCatalogUpdated { .. } => {}
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
            request_timeout: configured_request_timeout(),
        }
    }

    /// Create a client for a specific endpoint.
    #[must_use]
    pub const fn new(endpoint: IpcEndpoint) -> Self {
        Self {
            endpoint,
            runtime_context: None,
            daemon_availability: DaemonAvailability::RequireRunning,
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

    /// Configure the maximum wait for connection handshakes and IPC responses.
    #[must_use]
    pub const fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Return the configured IPC request timeout.
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
        tokio::time::timeout(
            CLIENT_DAEMON_START_TIMEOUT,
            ensure_daemon_running(&EnsureDaemonOptions {
                endpoint: self.endpoint.clone(),
                quiet: true,
                log_path: bcode_daemon_lifecycle::default_daemon_log_path(),
            }),
        )
        .await
        .map_err(|_| ClientError::RequestTimeout {
            timeout: CLIENT_DAEMON_START_TIMEOUT,
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
        let (_path, digest) = bcode_daemon_lifecycle::current_executable_identity()
            .map_err(DaemonStartError::from)?;
        let expected_namespace = bcode_ipc::daemon_namespace();
        let expected_protocol = u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION);
        let expected_writer_epoch = bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH;
        let expected_event_schema = bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION;
        if status.namespace == expected_namespace
            && status.protocol_version == expected_protocol
            && status.build_fingerprint == bcode_ipc::BUILD_FINGERPRINT
            && status.executable_digest.as_deref() == Some(digest.as_str())
            && status.storage_writer_epoch == Some(expected_writer_epoch)
            && status.session_event_schema_version == Some(expected_event_schema)
        {
            return Ok(());
        }
        Err(ClientError::IncompatibleDaemon {
            message: format!(
                "client expects namespace={expected_namespace} protocol={expected_protocol} build={} executable={digest} session_event_schema={expected_event_schema} storage_writer_epoch={expected_writer_epoch}; daemon reported namespace={} protocol={} build={} executable={} session_event_schema={} storage_writer_epoch={}",
                bcode_ipc::BUILD_FINGERPRINT,
                status.namespace,
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

    /// Ask the connected daemon to close one session database without detaching clients.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, rejects the request, or returns an
    /// unexpected response.
    pub async fn release_session_database(
        &self,
        session_id: bcode_session_models::SessionId,
    ) -> Result<bool, ClientError> {
        match self
            .send_request(Request::ReleaseSessionDatabase { session_id })
            .await?
        {
            ResponsePayload::SessionDatabaseReleased {
                session_id: released_session_id,
                released,
            } if released_session_id == session_id => Ok(released),
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

    /// Fork a session from a selected user prompt.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn fork_session(
        &self,
        source_session_id: SessionId,
        prompt_sequence: u64,
        name: Option<String>,
    ) -> Result<SessionForkResult, ClientError> {
        match self
            .send_request(Request::ForkSession {
                source_session_id,
                prompt_sequence,
                name,
            })
            .await?
        {
            ResponsePayload::SessionForked { session, draft } => {
                Ok(SessionForkResult { session, draft })
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Clone a session's full history.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn clone_session(
        &self,
        source_session_id: SessionId,
        name: Option<String>,
    ) -> Result<SessionForkResult, ClientError> {
        match self
            .send_request(Request::CloneSession {
                source_session_id,
                name,
                expected_generation: None,
            })
            .await?
        {
            ResponsePayload::SessionForked { session, draft } => {
                Ok(SessionForkResult { session, draft })
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Clone a session's history only when its current generation matches `expected_generation`.
    ///
    /// The generation comparison and history snapshot are performed by the session actor as one
    /// serialized read, so accepted clones cannot contain a different source generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, rejects the request, or the source
    /// generation changed before the snapshot was captured.
    pub async fn clone_session_at_generation(
        &self,
        source_session_id: SessionId,
        expected_generation: u64,
        name: Option<String>,
    ) -> Result<SessionForkResult, ClientError> {
        match self
            .send_request(Request::CloneSession {
                source_session_id,
                name,
                expected_generation: Some(expected_generation),
            })
            .await?
        {
            ResponsePayload::SessionForked { session, draft } => {
                Ok(SessionForkResult { session, draft })
            }
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
        match self
            .send_request(Request::ChangeSessionWorkingDirectory {
                session_id,
                working_directory: working_directory.into(),
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

    /// Create a Git worktree.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn create_worktree(
        &self,
        mut request: WorktreeCreateRequest,
    ) -> Result<WorktreeCreateResponse, ClientError> {
        request.cwd = Some(resolve_caller_path(request.cwd));
        match self.send_request(Request::CreateWorktree(request)).await? {
            ResponsePayload::WorktreeCreated(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
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
        match self
            .send_request(Request::InvokeSkill {
                session_id,
                skill_id,
                arguments,
                display_text,
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
        let mut last_error = None;
        for _ in 0..3 {
            match self.send_request_once(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(error)
                    if self.daemon_availability == DaemonAvailability::AutoStart
                        && error.is_daemon_unavailable() =>
                {
                    last_error = Some(error);
                    self.ensure_daemon_available().await?;
                    tokio::time::sleep(CLIENT_DAEMON_RETRY_DELAY).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or(ClientError::UnexpectedResponse))
    }

    async fn send_request_once(&self, request: Request) -> Result<ResponsePayload, ClientError> {
        let mut connection = self.connect("bcode-cli").await?;
        connection.send_request(request).await
    }

    /// Open a long-lived connection to the daemon.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the handshake.
    pub async fn connect(&self, client_name: &str) -> Result<ClientConnection, ClientError> {
        let mut last_error = None;
        for _ in 0..3 {
            let result = tokio::time::timeout(self.request_timeout, self.connect_once(client_name))
                .await
                .map_err(|_| ClientError::RequestTimeout {
                    timeout: self.request_timeout,
                })
                .and_then(std::convert::identity);
            match result {
                Ok(connection) => return Ok(connection),
                Err(error)
                    if self.daemon_availability == DaemonAvailability::AutoStart
                        && error.is_daemon_unavailable() =>
                {
                    last_error = Some(error);
                    self.ensure_daemon_available().await?;
                    tokio::time::sleep(CLIENT_DAEMON_RETRY_DELAY).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or(ClientError::UnexpectedResponse))
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
    use std::path::Path;
    use std::time::Duration;

    fn matching_daemon_status() -> bcode_ipc::DaemonStatus {
        let (_path, digest) = bcode_daemon_lifecycle::current_executable_identity()
            .expect("current executable identity");
        bcode_ipc::DaemonStatus {
            namespace: bcode_ipc::daemon_namespace(),
            protocol_version: u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION),
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
    fn daemon_identity_matrix_rejects_every_incompatible_capability() {
        let matching = matching_daemon_status();
        BcodeClient::verify_daemon_identity(&matching).expect("matching daemon");

        let cases = [
            bcode_ipc::DaemonStatus {
                protocol_version: matching.protocol_version.saturating_add(1),
                ..matching.clone()
            },
            bcode_ipc::DaemonStatus {
                build_fingerprint: "other-build".to_owned(),
                ..matching.clone()
            },
            bcode_ipc::DaemonStatus {
                executable_digest: Some("other-digest".to_owned()),
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
            assert!(message.contains("session_event_schema="));
            assert!(message.contains("storage_writer_epoch="));
            assert!(message.contains("protocol="));
            assert!(message.contains("build="));
        }
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
    fn request_timeout_can_be_overridden() {
        let client = BcodeClient::default_endpoint().with_request_timeout(Duration::from_secs(17));

        assert_eq!(client.request_timeout(), Duration::from_secs(17));
    }
}
