//! Serve tools that run in this application's process to sessions of the daemon.
//!
//! A [`ClientToolProvider`] owns one daemon connection. It registers the application's tools,
//! claims the sessions where they are offered, and answers the invocations the daemon sends
//! after the call passed the shared permission policy. Registrations and claims end with the
//! connection; after a daemon restart, connect again, register again and re-claim.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use bcode_ipc::{Event, Request, ResponsePayload};
use bcode_session_models::{SessionId, SessionOrigin, SessionSummary};
use bcode_tool::client_tool::{ClientToolDescriptor, ClientToolInvocation, ClientToolOutcome};

use crate::{BcodeClient, ClientConnection, ClientError};

/// What the daemon asked of a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientToolRequest {
    /// Run a tool; answer with [`ClientToolProvider::respond`].
    Invoke(ClientToolInvocation),
    /// Stop the invocation; the daemon drops any later answer.
    Cancel {
        /// The cancelled invocation.
        invocation_id: String,
    },
    /// Another connection of the same provider claimed the session.
    ClaimLost {
        /// The session no longer routed to this connection.
        session_id: SessionId,
    },
}

/// Boxed future of a tool handler.
pub type ClientToolFuture = Pin<Box<dyn Future<Output = ClientToolOutcome> + Send>>;

/// Daemon connection that provides client tools.
#[derive(Debug)]
pub struct ClientToolProvider {
    connection: ClientConnection,
    provider_id: String,
}

impl BcodeClient {
    /// Open a connection that provides `tools` under `provider_id`.
    ///
    /// Tool names must be `<provider_id>.<tool>`. Sessions this connection creates with an
    /// origin whose app id is `provider_id` are claimed automatically.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the registration: names
    /// outside the namespace, out-of-bounds descriptors, or names that collide with plugin tools
    /// or another provider's tools (`client_tools_rejected`).
    pub async fn client_tool_provider(
        &self,
        provider_id: impl Into<String>,
        tools: Vec<ClientToolDescriptor>,
    ) -> Result<ClientToolProvider, ClientError> {
        let provider_id = provider_id.into();
        let mut connection = self.connect("client-tool-provider").await?;
        match connection
            .send_request(Request::RegisterClientTools {
                provider_id: provider_id.clone(),
                tools,
            })
            .await?
        {
            ResponsePayload::ClientToolsRegistered { .. } => Ok(ClientToolProvider {
                connection,
                provider_id,
            }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }
}

impl ClientToolProvider {
    /// Provider id the tools are registered under.
    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// Replace the registered tools.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport fails or the daemon rejects the registration.
    pub async fn register(&mut self, tools: Vec<ClientToolDescriptor>) -> Result<(), ClientError> {
        match self
            .connection
            .send_request(Request::RegisterClientTools {
                provider_id: self.provider_id.clone(),
                tools,
            })
            .await?
        {
            ResponsePayload::ClientToolsRegistered { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Contribute agent profiles under this provider id, replacing earlier ones.
    ///
    /// A profile narrows a base agent: it adds instructions and may only remove tools, and every
    /// call is still decided by the base agent's permission policy. Profiles exist while this
    /// connection is open; turns of sessions that selected one fail closed with
    /// `agent_profile_unavailable` after it closes.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport fails or the daemon rejects the profiles: ids outside
    /// `<provider_id>.`, out-of-bounds fields, or ids that collide with configured agents
    /// (`client_agent_profiles_rejected`).
    pub async fn register_agent_profiles(
        &mut self,
        profiles: Vec<bcode_agent_profile::client_profile::ClientAgentProfile>,
    ) -> Result<(), ClientError> {
        match self
            .connection
            .send_request(Request::RegisterClientAgentProfiles { profiles })
            .await?
        {
            ResponsePayload::ClientAgentProfilesRegistered { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Offer this provider's tools in `session_id` from the next turn on.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport fails or the session does not exist.
    pub async fn claim_session(&mut self, session_id: SessionId) -> Result<(), ClientError> {
        match self
            .connection
            .send_request(Request::ClaimClientToolSession { session_id })
            .await?
        {
            ResponsePayload::ClientToolSessionClaimed { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Stop offering this provider's tools in `session_id` from the next turn on.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport fails.
    pub async fn release_session(&mut self, session_id: SessionId) -> Result<(), ClientError> {
        match self
            .connection
            .send_request(Request::ReleaseClientToolSession { session_id })
            .await?
        {
            ResponsePayload::ClientToolSessionReleased { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Create a session on this connection; it is claimed when `origin` names this provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport fails or the daemon rejects the session, including an
    /// out-of-bounds origin.
    pub async fn create_session(
        &mut self,
        name: Option<String>,
        working_directory: PathBuf,
        origin: Option<SessionOrigin>,
    ) -> Result<SessionSummary, ClientError> {
        match self
            .connection
            .send_request(Request::CreateSession {
                name,
                working_directory,
                origin,
            })
            .await?
        {
            ResponsePayload::SessionCreated { session } => Ok(session),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Wait for the daemon's next request to this provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the connection closes. The daemon then fails this connection's
    /// running calls; reconnect and register again to keep serving.
    pub async fn next_request(&mut self) -> Result<ClientToolRequest, ClientError> {
        loop {
            match self.connection.recv_event_without_reconnect().await? {
                Event::ClientToolInvocation(invocation) => {
                    return Ok(ClientToolRequest::Invoke(invocation));
                }
                Event::ClientToolCancelled { invocation_id } => {
                    return Ok(ClientToolRequest::Cancel { invocation_id });
                }
                Event::ClientToolClaimLost { session_id } => {
                    return Ok(ClientToolRequest::ClaimLost { session_id });
                }
                Event::Session(_)
                | Event::SessionLive(_)
                | Event::RuntimeWork(_)
                | Event::Workflow(_)
                | Event::SessionViewResyncRequired { .. }
                | Event::SessionCatalogUpdated { .. } => {}
            }
        }
    }

    /// Answer one invocation. Answering an invocation that already ended is accepted and
    /// ignored; answering twice with different outcomes is rejected.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport fails or the daemon rejects the answer.
    pub async fn respond(
        &mut self,
        invocation_id: String,
        outcome: ClientToolOutcome,
    ) -> Result<(), ClientError> {
        match self
            .connection
            .send_request(Request::ClientToolResult {
                invocation_id,
                outcome,
            })
            .await?
        {
            ResponsePayload::ClientToolResultAccepted { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Answer invocations with `handler` until the connection closes.
    ///
    /// Invocations are handled one at a time, in arrival order. Cancellations and claim losses
    /// are skipped; a handler that must stop early should watch for them through
    /// [`Self::next_request`] instead.
    ///
    /// # Errors
    ///
    /// Returns the transport error that ended the connection.
    pub async fn serve<H>(&mut self, mut handler: H) -> Result<(), ClientError>
    where
        H: FnMut(ClientToolInvocation) -> ClientToolFuture + Send,
    {
        loop {
            if let ClientToolRequest::Invoke(invocation) = self.next_request().await? {
                let invocation_id = invocation.invocation_id.clone();
                let outcome = handler(invocation).await;
                self.respond(invocation_id, outcome).await?;
            }
        }
    }
}

/// Daemon requests to a [`ClientToolHandle`]'s provider, in arrival order.
///
/// The stream ends when the provider's daemon connection closes; the daemon then fails the
/// connection's running calls and drops its registrations and claims.
#[derive(Debug)]
pub struct ClientToolRequests {
    receiver: tokio::sync::mpsc::Receiver<ClientToolRequest>,
}

impl ClientToolRequests {
    /// The next request, or `None` once the connection closed.
    pub async fn recv(&mut self) -> Option<ClientToolRequest> {
        self.receiver.recv().await
    }
}

type PendingResponses = std::sync::Arc<
    std::sync::Mutex<
        std::collections::BTreeMap<u64, tokio::sync::oneshot::Sender<bcode_ipc::Response>>,
    >,
>;

/// Cloneable handle of a provider connection that serves invocations concurrently.
///
/// A plain [`ClientToolProvider`] answers one request at a time on its connection. The handle
/// splits the connection: a reader task delivers daemon requests to [`ClientToolRequests`] and
/// routes responses, so answering one invocation never waits for another to finish and claims
/// or registrations can be sent while invocations run.
#[derive(Debug, Clone)]
pub struct ClientToolHandle {
    provider_id: String,
    writer: std::sync::Arc<tokio::sync::Mutex<tokio::io::WriteHalf<bcode_ipc::LocalIpcStream>>>,
    pending: PendingResponses,
    next_request_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    request_timeout: std::time::Duration,
    reader: std::sync::Arc<ReaderTask>,
}

/// Stops the reader task when the last handle is dropped, which closes the connection.
#[derive(Debug)]
struct ReaderTask(tokio::task::JoinHandle<()>);

impl Drop for ReaderTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl ClientToolProvider {
    /// Split this provider into a cloneable handle and the stream of daemon requests, so
    /// invocations can be answered concurrently.
    ///
    /// Must be called from a Tokio runtime: the reader runs as a task until the connection
    /// closes or every handle is dropped.
    #[must_use]
    pub fn into_concurrent(self) -> (ClientToolHandle, ClientToolRequests) {
        let ClientConnection {
            stream,
            next_request_id,
            pending_events,
            request_timeout,
            ..
        } = self.connection;
        let (read, write) = tokio::io::split(stream);
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        let pending = PendingResponses::default();
        let routes = pending.clone();
        let reader = tokio::spawn(read_provider_frames(read, pending_events, sender, routes));
        (
            ClientToolHandle {
                provider_id: self.provider_id,
                writer: std::sync::Arc::new(tokio::sync::Mutex::new(write)),
                pending,
                next_request_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                    next_request_id,
                )),
                request_timeout,
                reader: std::sync::Arc::new(ReaderTask(reader)),
            },
            ClientToolRequests { receiver },
        )
    }
}

/// Deliver the provider's daemon requests and route responses to their waiting requests until
/// the connection closes.
async fn read_provider_frames(
    mut read: tokio::io::ReadHalf<bcode_ipc::LocalIpcStream>,
    pending_events: std::collections::VecDeque<Event>,
    sender: tokio::sync::mpsc::Sender<ClientToolRequest>,
    routes: PendingResponses,
) {
    for event in pending_events {
        if let Some(request) = provider_request(event)
            && sender.send(request).await.is_err()
        {
            return;
        }
    }
    while let Ok(envelope) = bcode_ipc::recv_envelope(&mut read).await {
        match envelope.kind {
            bcode_ipc::EnvelopeKind::Event => {
                if let Ok(event) = bcode_ipc::decode_event(&envelope.payload)
                    && let Some(request) = provider_request(event)
                    && sender.send(request).await.is_err()
                {
                    return;
                }
            }
            bcode_ipc::EnvelopeKind::Response => {
                let Ok(response) = bcode_ipc::decode_response(&envelope.payload) else {
                    continue;
                };
                let route = routes
                    .lock()
                    .expect("client tool response routes poisoned")
                    .remove(&envelope.request_id);
                if let Some(route) = route {
                    let _ = route.send(response);
                }
            }
            _ => {}
        }
    }
    // The connection closed: waiting requests fail with a dropped route.
    routes
        .lock()
        .expect("client tool response routes poisoned")
        .clear();
}

fn provider_request(event: Event) -> Option<ClientToolRequest> {
    match event {
        Event::ClientToolInvocation(invocation) => Some(ClientToolRequest::Invoke(invocation)),
        Event::ClientToolCancelled { invocation_id } => {
            Some(ClientToolRequest::Cancel { invocation_id })
        }
        Event::ClientToolClaimLost { session_id } => {
            Some(ClientToolRequest::ClaimLost { session_id })
        }
        Event::Session(_)
        | Event::SessionLive(_)
        | Event::RuntimeWork(_)
        | Event::Workflow(_)
        | Event::SessionViewResyncRequired { .. }
        | Event::SessionCatalogUpdated { .. } => None,
    }
}

impl ClientToolHandle {
    /// Provider id the tools are registered under.
    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    async fn send(&self, request: Request) -> Result<ResponsePayload, ClientError> {
        if self.reader.0.is_finished() {
            return Err(ClientError::Protocol(
                "client tool provider connection closed".to_owned(),
            ));
        }
        let request_id = self
            .next_request_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let envelope = bcode_ipc::request_envelope(request_id, &request)?;
        let (route, response) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .expect("client tool response routes poisoned")
            .insert(request_id, route);
        let sent = bcode_ipc::send_envelope(&mut *self.writer.lock().await, &envelope).await;
        if let Err(error) = sent {
            self.pending
                .lock()
                .expect("client tool response routes poisoned")
                .remove(&request_id);
            return Err(error.into());
        }
        let response = tokio::time::timeout(self.request_timeout, response)
            .await
            .map_err(|_| {
                self.pending
                    .lock()
                    .expect("client tool response routes poisoned")
                    .remove(&request_id);
                ClientError::RequestTimeout {
                    timeout: self.request_timeout,
                }
            })?
            .map_err(|_| {
                ClientError::Protocol("client tool provider connection closed".to_owned())
            })?;
        match response {
            bcode_ipc::Response::Ok(payload) => Ok(payload),
            bcode_ipc::Response::Err(error) => Err(error.into()),
        }
    }

    /// Replace the registered tools.
    ///
    /// # Errors
    ///
    /// Returns an error when the connection closed or the daemon rejects the registration.
    pub async fn register(&self, tools: Vec<ClientToolDescriptor>) -> Result<(), ClientError> {
        match self
            .send(Request::RegisterClientTools {
                provider_id: self.provider_id.clone(),
                tools,
            })
            .await?
        {
            ResponsePayload::ClientToolsRegistered { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Contribute agent profiles; see [`ClientToolProvider::register_agent_profiles`].
    ///
    /// # Errors
    ///
    /// Returns an error when the connection closed or the daemon rejects the profiles.
    pub async fn register_agent_profiles(
        &self,
        profiles: Vec<bcode_agent_profile::client_profile::ClientAgentProfile>,
    ) -> Result<(), ClientError> {
        match self
            .send(Request::RegisterClientAgentProfiles { profiles })
            .await?
        {
            ResponsePayload::ClientAgentProfilesRegistered { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Offer this provider's tools in `session_id` from the next turn on.
    ///
    /// # Errors
    ///
    /// Returns an error when the connection closed or the session does not exist.
    pub async fn claim_session(&self, session_id: SessionId) -> Result<(), ClientError> {
        match self
            .send(Request::ClaimClientToolSession { session_id })
            .await?
        {
            ResponsePayload::ClientToolSessionClaimed { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Stop offering this provider's tools in `session_id` from the next turn on.
    ///
    /// # Errors
    ///
    /// Returns an error when the connection closed.
    pub async fn release_session(&self, session_id: SessionId) -> Result<(), ClientError> {
        match self
            .send(Request::ReleaseClientToolSession { session_id })
            .await?
        {
            ResponsePayload::ClientToolSessionReleased { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Create a session on this connection; it is claimed when `origin` names this provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the connection closed or the daemon rejects the session.
    pub async fn create_session(
        &self,
        name: Option<String>,
        working_directory: PathBuf,
        origin: Option<SessionOrigin>,
    ) -> Result<SessionSummary, ClientError> {
        match self
            .send(Request::CreateSession {
                name,
                working_directory,
                origin,
            })
            .await?
        {
            ResponsePayload::SessionCreated { session } => Ok(session),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Answer one invocation; see [`ClientToolProvider::respond`].
    ///
    /// # Errors
    ///
    /// Returns an error when the connection closed or the daemon rejects the answer.
    pub async fn respond(
        &self,
        invocation_id: String,
        outcome: ClientToolOutcome,
    ) -> Result<(), ClientError> {
        match self
            .send(Request::ClientToolResult {
                invocation_id,
                outcome,
            })
            .await?
        {
            ResponsePayload::ClientToolResultAccepted { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }
}
