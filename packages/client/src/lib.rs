#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Programmatic client API for Bcode.

use bcode_agent_profile::{AgentInfo, PolicyStatusResponse};
use bcode_ipc::{
    CodecError, EnvelopeKind, ErrorResponse, Event, IpcEndpoint, LocalIpcStream, PermissionSummary,
    PluginServiceResponse, PluginServiceSummary, Request, Response, ResponsePayload, decode,
    default_endpoint, recv_envelope, request_envelope, send_envelope,
};
use bcode_session_models::{
    ClientId, SessionEvent, SessionHistoryPage, SessionHistoryQuery, SessionId,
    SessionInputHistoryEntry, SessionSummary,
};
use bcode_skill_models::{SkillId, SkillList, SkillManifest};
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

/// History returned when attaching to a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedSessionHistory {
    pub history: Vec<SessionEvent>,
    pub input_history: Vec<SessionInputHistoryEntry>,
}

impl From<ErrorResponse> for ClientError {
    fn from(value: ErrorResponse) -> Self {
        Self::Server {
            code: value.code,
            message: value.message,
        }
    }
}

/// Client configured for a local Bcode server endpoint.
#[derive(Debug, Clone)]
pub struct BcodeClient {
    endpoint: IpcEndpoint,
}

impl BcodeClient {
    /// Create a client that connects to the default endpoint.
    #[must_use]
    pub fn default_endpoint() -> Self {
        Self {
            endpoint: default_endpoint(),
        }
    }

    /// Create a client for a specific endpoint.
    #[must_use]
    pub const fn new(endpoint: IpcEndpoint) -> Self {
        Self { endpoint }
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection.send_request(Request::ServerStop).await? {
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
            .send_request(Request::CreateSession { name })
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
        let mut connection = self.connect("bcode-cli").await?;
        match connection.send_request(Request::ListSessions).await? {
            ResponsePayload::SessionList { sessions } => Ok(sessions),
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
    ) -> Result<(), ClientError> {
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
    ) -> Result<(), ClientError> {
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
            ResponsePayload::MessageSent => Ok(()),
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
                client_name: client_name.to_string(),
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
                ..
            } => Ok(AttachedSessionHistory {
                history,
                input_history,
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
                ..
            } => Ok(AttachedSessionHistory {
                history,
                input_history,
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
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::SendUserMessage { session_id, text })
            .await?
        {
            ResponsePayload::MessageSent => Ok(()),
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
