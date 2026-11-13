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
