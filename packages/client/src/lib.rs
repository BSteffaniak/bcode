#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Programmatic client API for Bcode.

use bcode_agent_profile::{AgentInfo, PolicyStatusResponse};
use bcode_ipc::{
    ClientRuntimeContext, CodecError, EnvelopeKind, ErrorResponse, Event, IpcEndpoint,
    LocalIpcStream, PermissionSummary, PluginServiceResponse, PluginServiceSummary, Request,
    Response, ResponsePayload, ServerStopMode, SessionCatalogSourceStatus, SessionCatalogStatus,
    SessionImportWarning, WorktreeCreateRequest, WorktreeCreateResponse, WorktreeListRequest,
    WorktreeListResponse, WorktreeRemoveRequest, WorktreeRemoveResponse, current_working_directory,
    decode, default_endpoint, recv_envelope, request_envelope, send_envelope,
};
use bcode_session_models::{
    ClientId, SessionEvent, SessionHistoryPage, SessionHistoryQuery, SessionId,
    SessionInputHistoryEntry, SessionSummary,
};
use bcode_skill_models::{SkillId, SkillList, SkillManifest};
use std::collections::BTreeMap;
use thiserror::Error;

/// Errors returned by the Bcode client.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("IPC transport error: {0}")]
    Transport(#[from] bcode_ipc::IpcTransportError),
    #[error("IPC codec error: {0}")]
    Codec(#[from] CodecError),
    #[error("server returned error {code}: {message}")]
    Server { code: String, message: String },
    #[error("unexpected response payload")]
    UnexpectedResponse,
    #[error("unexpected IPC envelope kind")]
    UnexpectedEnvelope,
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

fn current_runtime_context() -> Option<ClientRuntimeContext> {
    let config = bcode_config::load_config().ok()?;
    let mut env = CLIENT_RUNTIME_ENV_VARS
        .iter()
        .filter_map(|name| match std::env::var(name) {
            Ok(value) if !value.trim().is_empty() => Some(((*name).to_string(), value)),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let mut resolved = config.resolved_model_selection();
    resolved.auth_profile = selected_auth_profile(&resolved);
    let auth = merge_selected_auth_profile_env(&config, resolved.auth_profile.as_deref(), &mut env);
    let env_keys = env.keys().cloned().map(|key| (key, true)).collect();
    Some(ClientRuntimeContext {
        selected_provider_plugin_id: resolved.provider_plugin_id,
        selected_model_id: resolved.model_id,
        provider_context: bcode_model::ProviderRequestContext {
            model_profile: resolved.model_profile,
            auth_profile: resolved.auth_profile,
            settings: resolved.settings,
            auth,
            request: resolved.request,
            env,
        },
        env_keys,
    })
}

fn selected_auth_profile(resolved: &bcode_config::ResolvedModelSelection) -> Option<String> {
    std::env::var(bcode_config::BCODE_AUTH_PROFILE_ENV)
        .ok()
        .filter(|profile| !profile.trim().is_empty())
        .or_else(|| resolved.auth_profile.clone())
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
    let store = sshenv_vault::SshenvStore::new(sshenv_vault::SshenvStoreConfig::new(vault));
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
}

impl MessageAcceptance {
    /// Acceptance for legacy servers that only report message delivery.
    #[must_use]
    pub const fn sent() -> Self {
        Self {
            queued: false,
            queue_position: None,
        }
    }
}

/// Client configured for a local Bcode server endpoint.
#[derive(Debug, Clone)]
pub struct BcodeClient {
    endpoint: IpcEndpoint,
    runtime_context: Option<ClientRuntimeContext>,
}

impl BcodeClient {
    /// Create a client that connects to the default endpoint.
    #[must_use]
    pub fn default_endpoint() -> Self {
        Self {
            endpoint: default_endpoint(),
            runtime_context: current_runtime_context(),
        }
    }

    /// Create a client for a specific endpoint.
    #[must_use]
    pub const fn new(endpoint: IpcEndpoint) -> Self {
        Self {
            endpoint,
            runtime_context: None,
        }
    }

    /// Attach a client-supplied runtime context to future connections.
    #[must_use]
    pub fn with_runtime_context(mut self, runtime_context: Option<ClientRuntimeContext>) -> Self {
        self.runtime_context = runtime_context;
        self
    }

    /// Check whether the local server accepts requests.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn ping(&self) -> Result<(), ClientError> {
        let mut connection = self.connect("bcode-cli").await?;
        match connection.send_request(Request::Ping).await? {
            ResponsePayload::Pong => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Query local server status.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn server_status(&self) -> Result<bcode_ipc::ServerStatus, ClientError> {
        let mut connection = self.connect("bcode-cli").await?;
        match connection.send_request(Request::ServerStatus).await? {
            ResponsePayload::ServerStatus { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
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

    async fn server_stop_with_mode(&self, mode: ServerStopMode) -> Result<(), ClientError> {
        let mut connection = self.connect("bcode-cli").await?;
        match connection
            .send_request(Request::ServerStop { mode })
            .await?
        {
            ResponsePayload::ServerStopping => Ok(()),
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
            .send_request(Request::CreateSession {
                name,
                working_directory: current_working_directory(),
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
            .send_request(Request::ImportExternalSession {
                source_id: source_id.into(),
                external_session_id: external_session_id.into(),
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        request: WorktreeListRequest,
    ) -> Result<WorktreeListResponse, ClientError> {
        let mut connection = self.connect("bcode-cli").await?;
        match connection
            .send_request(Request::ListWorktrees(request))
            .await?
        {
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
        request: WorktreeCreateRequest,
    ) -> Result<WorktreeCreateResponse, ClientError> {
        let mut connection = self.connect("bcode-cli").await?;
        match connection
            .send_request(Request::CreateWorktree(request))
            .await?
        {
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
        request: WorktreeRemoveRequest,
    ) -> Result<WorktreeRemoveResponse, ClientError> {
        let mut connection = self.connect("bcode-cli").await?;
        match connection
            .send_request(Request::RemoveWorktree(request))
            .await?
        {
            ResponsePayload::WorktreeRemoved(response) => Ok(response),
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
            .send_request(Request::DeleteSession { session_id })
            .await?
        {
            ResponsePayload::SessionDeleted { session } => Ok(session),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return replayable session history.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, ClientError> {
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
    ) -> Result<MessageAcceptance, ClientError> {
        let mut connection = self.connect("bcode-cli").await?;
        connection.send_user_message(session_id, text).await
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
            .send_request(Request::SessionModelStatus { session_id })
            .await?
        {
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
            .send_request(Request::CancelSessionTurn { session_id })
            .await?
        {
            ResponsePayload::TurnCancellationRequested { cancelled } => Ok(cancelled),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Compact the model-visible context for a session while preserving append-only history.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn compact_session(&self, session_id: SessionId) -> Result<String, ClientError> {
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection.send_request(Request::ListAgents).await? {
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection.send_request(Request::ListSkills).await? {
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection.send_request(Request::AgentPolicyStatus).await? {
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection.send_request(Request::ListPermissions).await? {
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
            .send_request(Request::ResolvePermission {
                permission_id,
                approved,
            })
            .await?
        {
            ResponsePayload::PermissionResolved { resolved } => Ok(resolved),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Persist and activate a permission policy rule under `[agent.<agent_id>.permission.<category>]`.
    ///
    /// `category` must be one of `bash`, `read`, `write`, or `edit`.
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection.send_request(Request::ListPluginServices).await? {
            ResponsePayload::PluginServices { services } => Ok(services),
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection
            .send_request(Request::PublishPluginEvent { topic, payload })
            .await?
        {
            ResponsePayload::PluginEventPublished { delivered } => Ok(delivered),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Open a long-lived connection to the daemon.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the handshake.
    pub async fn connect(&self, client_name: &str) -> Result<ClientConnection, ClientError> {
        let stream = LocalIpcStream::connect(&self.endpoint).await?;
        let mut connection = ClientConnection {
            stream,
            next_request_id: 1,
            client_id: None,
        };
        match connection
            .send_request(Request::Hello {
                client_name: format!("{client_name};cap=message_accepted"),
                runtime_context: self.runtime_context.clone(),
                daemon_namespace: bcode_ipc::daemon_namespace(),
            })
            .await?
        {
            ResponsePayload::Hello { client_id, .. } => {
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
        self.update_runtime_context(current_runtime_context()).await
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
                session,
                ..
            } => Ok(AttachedSessionHistory {
                session,
                history,
                input_history,
                import_warnings,
            }),
            _ => Err(ClientError::UnexpectedResponse),
        }
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
                session,
                ..
            } => Ok(AttachedSessionHistory {
                session,
                history,
                input_history,
                import_warnings,
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
    ) -> Result<MessageAcceptance, ClientError> {
        match self
            .send_request(Request::SendUserMessage { session_id, text })
            .await?
        {
            ResponsePayload::MessageAccepted {
                queued,
                queue_position,
            } => Ok(MessageAcceptance {
                queued,
                queue_position,
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
        loop {
            let envelope = recv_envelope(&mut self.stream).await?;
            if envelope.kind != EnvelopeKind::Event {
                continue;
            }
            return decode(&envelope.payload).map_err(ClientError::from);
        }
    }

    async fn send_request(&mut self, request: Request) -> Result<ResponsePayload, ClientError> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        let envelope = request_envelope(request_id, &request)?;
        send_envelope(&mut self.stream, &envelope).await?;

        loop {
            let envelope = recv_envelope(&mut self.stream).await?;
            if envelope.kind != EnvelopeKind::Response || envelope.request_id != request_id {
                continue;
            }
            let response: Response = decode(&envelope.payload)?;
            return match response {
                Response::Ok(payload) => Ok(payload),
                Response::Err(error) => Err(error.into()),
            };
        }
    }
}
