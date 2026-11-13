//! Routing for tools that connected client applications implement.
//!
//! A client connection registers namespaced tools and claims sessions. A turn of a claimed
//! session offers the claimant's tools next to plugin tools; the offered set is pinned in the
//! turn's tool catalog, so tools that appear or disappear mid-turn take effect on the next turn.
//! Every call passes the shared permission policy before the invocation is sent to the client.
//! Registrations, claims and pending invocations are connection-scoped memory: nothing is
//! durable, and a closed connection fails its pending calls instead of replaying them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use bcode_agent_profile::client_profile::{ClientAgentProfile, validate_client_agent_profiles};
use bcode_ipc::Event;
use bcode_session_models::{ClientId, SessionId};
use bcode_tool::client_tool::{
    CLIENT_TOOL_DISCONNECTED, CLIENT_TOOL_TIMEOUT, CLIENT_TOOL_UNAVAILABLE, ClientToolDescriptor,
    ClientToolInvocation, ClientToolOutcome, ClientToolPolicy, MAX_CLIENT_TOOL_RESULT_BYTES,
    validate_client_tool_registration,
};
use tokio::sync::oneshot;

use crate::ClientEventSink;

/// Finished invocations remembered so duplicate or late results are classified, not rejected
/// as unknown.
const MAX_REMEMBERED_FINISHED_INVOCATIONS: usize = 1024;

/// A tool offered to one turn, pinned in its tool catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferedClientTool {
    /// Provider that registered the tool.
    pub provider_id: String,
    /// Model-visible definition.
    pub definition: bcode_tool::ToolDefinition,
    /// Whether every invocation is read-only.
    pub read_only: bool,
}

#[derive(Debug)]
struct ProviderConnection {
    provider_id: String,
    tools: BTreeMap<String, ClientToolDescriptor>,
    profiles: BTreeMap<String, ClientAgentProfile>,
    sink: ClientEventSink,
}

#[derive(Debug)]
struct PendingInvocation {
    client_id: ClientId,
    sender: oneshot::Sender<ClientToolOutcome>,
}

#[derive(Debug, Default)]
struct Registry {
    providers: BTreeMap<ClientId, ProviderConnection>,
    claims: BTreeMap<SessionId, ClientId>,
    pending: BTreeMap<String, PendingInvocation>,
    /// Finished invocations and the result that finished them, if any.
    finished: BTreeMap<String, Option<ClientToolOutcome>>,
    finished_order: VecDeque<String>,
}

impl Registry {
    fn remember_finished(&mut self, invocation_id: String, outcome: Option<ClientToolOutcome>) {
        if self
            .finished
            .insert(invocation_id.clone(), outcome)
            .is_none()
        {
            self.finished_order.push_back(invocation_id);
        }
        while self.finished_order.len() > MAX_REMEMBERED_FINISHED_INVOCATIONS {
            if let Some(oldest) = self.finished_order.pop_front() {
                self.finished.remove(&oldest);
            }
        }
    }
}

/// How an invocation ended, before it becomes a tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvocationEnd {
    /// The client answered.
    Answered(ClientToolOutcome),
    /// No live claimant offers the tool in the session.
    Unavailable(String),
    /// The claimant's connection closed before it answered; effects are unknown.
    Disconnected(String),
    /// The declared deadline passed.
    TimedOut(String),
    /// The turn was cancelled.
    Cancelled,
}

/// Daemon-memory registry of client tool providers, session claims and pending invocations.
#[derive(Debug, Default)]
pub struct ClientToolRegistry {
    inner: StdMutex<Registry>,
}

impl ClientToolRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.inner
            .lock()
            .expect("client tool registry lock poisoned")
    }

    /// Register (or replace) the tools of one connection.
    ///
    /// The registration is rejected whole when it is out of bounds, when the connection
    /// already registered another provider id, or when a name collides with a plugin tool or a
    /// tool of another live provider. Connections of the same provider may register the same
    /// names; the session claim decides which one is called.
    pub fn register(
        &self,
        client_id: ClientId,
        sink: ClientEventSink,
        provider_id: String,
        tools: Vec<ClientToolDescriptor>,
        plugin_tool_names: &BTreeSet<String>,
    ) -> Result<usize, String> {
        validate_client_tool_registration(&provider_id, &tools).map_err(|e| e.to_string())?;
        let mut registry = self.lock();
        if let Some(existing) = registry.providers.get(&client_id)
            && existing.provider_id != provider_id
        {
            return Err(format!(
                "this connection already provides tools as {:?}",
                existing.provider_id
            ));
        }
        let mut collisions = tools
            .iter()
            .filter(|tool| plugin_tool_names.contains(&tool.name))
            .map(|tool| tool.name.clone())
            .collect::<BTreeSet<_>>();
        for (other_client, provider) in &registry.providers {
            if *other_client == client_id || provider.provider_id == provider_id {
                continue;
            }
            collisions.extend(
                tools
                    .iter()
                    .filter(|tool| provider.tools.contains_key(&tool.name))
                    .map(|tool| tool.name.clone()),
            );
        }
        if !collisions.is_empty() {
            return Err(format!(
                "client tool names collide with existing tools: {}",
                collisions.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        let count = tools.len();
        let profiles = registry
            .providers
            .remove(&client_id)
            .map(|previous| previous.profiles)
            .unwrap_or_default();
        registry.providers.insert(
            client_id,
            ProviderConnection {
                provider_id,
                tools: tools
                    .into_iter()
                    .map(|tool| (tool.name.clone(), tool))
                    .collect(),
                profiles,
                sink,
            },
        );
        drop(registry);
        Ok(count)
    }

    /// Replace the agent profiles one provider connection contributes.
    ///
    /// The connection must have registered tools first, which fixes its provider id. The
    /// registration is rejected whole when it is out of bounds or an id collides with a plugin
    /// agent.
    pub fn register_profiles(
        &self,
        client_id: ClientId,
        profiles: Vec<ClientAgentProfile>,
        plugin_agent_ids: &BTreeSet<String>,
    ) -> Result<usize, String> {
        let mut registry = self.lock();
        let Some(provider) = registry.providers.get_mut(&client_id) else {
            return Err("register client tools before contributing agent profiles".to_owned());
        };
        validate_client_agent_profiles(&provider.provider_id, &profiles)
            .map_err(|error| error.to_string())?;
        let collisions = profiles
            .iter()
            .filter(|profile| plugin_agent_ids.contains(&profile.id))
            .map(|profile| profile.id.as_str())
            .collect::<Vec<_>>();
        if !collisions.is_empty() {
            return Err(format!(
                "agent profile ids collide with configured agents: {}",
                collisions.join(", ")
            ));
        }
        let count = profiles.len();
        provider.profiles = profiles
            .into_iter()
            .map(|profile| (profile.id.clone(), profile))
            .collect();
        drop(registry);
        Ok(count)
    }

    /// The contributed profile `agent_id`, while a connection of its provider is registered.
    pub fn agent_profile(&self, agent_id: &str) -> Option<ClientAgentProfile> {
        self.lock()
            .providers
            .values()
            .rev()
            .find_map(|provider| provider.profiles.get(agent_id).cloned())
    }

    /// Every contributed profile of the registered connections, one per id.
    pub fn agent_profiles(&self) -> Vec<ClientAgentProfile> {
        let registry = self.lock();
        let mut profiles = BTreeMap::new();
        for provider in registry.providers.values() {
            for (id, profile) in &provider.profiles {
                profiles
                    .entry(id.clone())
                    .or_insert_with(|| profile.clone());
            }
        }
        drop(registry);
        profiles.into_values().collect()
    }

    /// Route the session's client tools to this connection.
    ///
    /// Returns the sink of a different connection that lost the claim, so it can be told.
    pub fn claim(
        &self,
        client_id: ClientId,
        session_id: SessionId,
    ) -> Result<Option<ClientEventSink>, String> {
        let mut registry = self.lock();
        if !registry.providers.contains_key(&client_id) {
            return Err("register client tools before claiming a session".to_owned());
        }
        let previous = registry.claims.insert(session_id, client_id);
        let lost = previous
            .filter(|previous| *previous != client_id)
            .and_then(|previous| registry.providers.get(&previous))
            .map(|provider| provider.sink.clone());
        drop(registry);
        Ok(lost)
    }

    /// Claim the session for this connection when it provides tools as `app_id`.
    pub fn claim_if_provider(
        &self,
        client_id: ClientId,
        session_id: SessionId,
        app_id: &str,
    ) -> Option<ClientEventSink> {
        let provides = self
            .lock()
            .providers
            .get(&client_id)
            .is_some_and(|provider| provider.provider_id == app_id);
        if provides {
            self.claim(client_id, session_id).ok().flatten()
        } else {
            None
        }
    }

    /// Stop routing the session's client tools to this connection.
    pub fn release(&self, client_id: ClientId, session_id: SessionId) -> bool {
        let mut registry = self.lock();
        if registry.claims.get(&session_id) == Some(&client_id) {
            registry.claims.remove(&session_id);
            true
        } else {
            false
        }
    }

    /// Drop everything a closed connection held and fail its pending invocations.
    pub fn disconnect(&self, client_id: ClientId) {
        let mut registry = self.lock();
        if registry.providers.remove(&client_id).is_none() {
            return;
        }
        registry.claims.retain(|_, claimant| *claimant != client_id);
        let failed = registry
            .pending
            .iter()
            .filter(|(_, pending)| pending.client_id == client_id)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for invocation_id in failed {
            // Dropping the sender wakes the waiting invocation as disconnected.
            registry.pending.remove(&invocation_id);
            registry.remember_finished(invocation_id, None);
        }
    }

    /// Tools the session's live claimant offers, for pinning in a new turn's catalog.
    pub fn offered_tools(&self, session_id: SessionId) -> Vec<OfferedClientTool> {
        let registry = self.lock();
        let Some(provider) = registry
            .claims
            .get(&session_id)
            .and_then(|client_id| registry.providers.get(client_id))
        else {
            return Vec::new();
        };
        let offered = provider
            .tools
            .values()
            .map(|tool| OfferedClientTool {
                provider_id: provider.provider_id.clone(),
                definition: bcode_tool::ToolDefinition {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                },
                read_only: tool.policy == ClientToolPolicy::ReadOnly,
            })
            .collect();
        drop(registry);
        offered
    }

    /// Declared descriptor of a tool the session's live claimant offers as `provider_id`.
    pub fn descriptor(
        &self,
        session_id: SessionId,
        provider_id: &str,
        tool_name: &str,
    ) -> Option<ClientToolDescriptor> {
        let registry = self.lock();
        registry
            .claims
            .get(&session_id)
            .and_then(|client_id| registry.providers.get(client_id))
            .filter(|provider| provider.provider_id == provider_id)
            .and_then(|provider| provider.tools.get(tool_name).cloned())
    }

    /// Record a client's answer.
    ///
    /// A late answer to an invocation that already ended is accepted and dropped. A repeated
    /// identical answer is accepted; a conflicting one is rejected.
    pub fn complete(
        &self,
        client_id: ClientId,
        invocation_id: &str,
        outcome: ClientToolOutcome,
    ) -> Result<(), String> {
        let mut registry = self.lock();
        if let Some(pending) = registry.pending.get(invocation_id) {
            if pending.client_id != client_id {
                return Err("client tool invocation belongs to another connection".to_owned());
            }
            let pending = registry
                .pending
                .remove(invocation_id)
                .expect("pending invocation present");
            registry.remember_finished(invocation_id.to_owned(), Some(outcome.clone()));
            let _ = pending.sender.send(outcome);
            return Ok(());
        }
        match registry.finished.get(invocation_id) {
            Some(None) => Ok(()),
            Some(Some(recorded)) if *recorded == outcome => Ok(()),
            Some(Some(_)) => {
                Err("client tool invocation already has a different result".to_owned())
            }
            None => Err("unknown client tool invocation".to_owned()),
        }
    }

    fn begin(
        &self,
        session_id: SessionId,
        provider_id: &str,
        tool_name: &str,
        invocation_id: &str,
    ) -> Result<(ClientEventSink, oneshot::Receiver<ClientToolOutcome>), String> {
        let mut registry = self.lock();
        let Some((client_id, sink)) = registry
            .claims
            .get(&session_id)
            .and_then(|client_id| {
                registry
                    .providers
                    .get(client_id)
                    .map(|provider| (*client_id, provider))
            })
            .filter(|(_, provider)| {
                provider.provider_id == provider_id && provider.tools.contains_key(tool_name)
            })
            .map(|(client_id, provider)| (client_id, provider.sink.clone()))
        else {
            return Err(format!("{provider_id} is not connected to this session"));
        };
        let (sender, receiver) = oneshot::channel();
        registry.pending.insert(
            invocation_id.to_owned(),
            PendingInvocation { client_id, sender },
        );
        drop(registry);
        Ok((sink, receiver))
    }

    fn abandon(&self, invocation_id: &str) {
        let mut registry = self.lock();
        if registry.pending.remove(invocation_id).is_some() {
            registry.remember_finished(invocation_id.to_owned(), None);
        }
    }

    /// Send one authorized invocation to the session's claimant and wait for its end.
    ///
    /// The wait ends on the client's answer, the connection closing, the deadline, or
    /// `cancelled` completing. Ending without an answer tells the client to stop.
    pub async fn invoke(
        &self,
        invocation: ClientToolInvocation,
        session_id: SessionId,
        provider_id: &str,
        cancelled: impl std::future::Future<Output = ()>,
    ) -> InvocationEnd {
        let invocation_id = invocation.invocation_id.clone();
        let deadline = Duration::from_millis(u64::from(invocation.deadline_ms));
        let (sink, receiver) = match self.begin(
            session_id,
            provider_id,
            &invocation.tool_name,
            &invocation_id,
        ) {
            Ok(route) => route,
            Err(message) => return InvocationEnd::Unavailable(message),
        };
        if sink
            .send(Event::ClientToolInvocation(invocation))
            .await
            .is_err()
        {
            self.abandon(&invocation_id);
            return InvocationEnd::Disconnected(format!(
                "{provider_id} disconnected before the call was delivered"
            ));
        }
        let end = tokio::select! {
            answer = receiver => answer.map_or_else(
                |_| InvocationEnd::Disconnected(format!(
                    "{provider_id} disconnected while the call was running; its effects are unknown"
                )),
                InvocationEnd::Answered,
            ),
            () = tokio::time::sleep(deadline) => InvocationEnd::TimedOut(format!(
                "{provider_id} did not answer within {} ms", deadline.as_millis()
            )),
            () = cancelled => InvocationEnd::Cancelled,
        };
        if matches!(end, InvocationEnd::TimedOut(_) | InvocationEnd::Cancelled) {
            self.abandon(&invocation_id);
            let _ = sink
                .send(Event::ClientToolCancelled {
                    invocation_id: invocation_id.clone(),
                })
                .await;
        }
        end
    }
}

/// Convert an invocation's end into the model-visible tool response.
///
/// Answers are untrusted input: completed text is bounded here and then passes the runtime's
/// ordinary tool-result policy. Failures carry a stable code prefix.
pub fn tool_response(end: InvocationEnd) -> bcode_tool::ToolInvocationResponse {
    let (output, is_error) = match end {
        InvocationEnd::Answered(ClientToolOutcome::Completed { content }) => {
            if content.len() > MAX_CLIENT_TOOL_RESULT_BYTES {
                let mut end = MAX_CLIENT_TOOL_RESULT_BYTES;
                while !content.is_char_boundary(end) {
                    end -= 1;
                }
                (
                    format!(
                        "{}\n[client tool result truncated at {MAX_CLIENT_TOOL_RESULT_BYTES} bytes]",
                        &content[..end]
                    ),
                    false,
                )
            } else {
                (content, false)
            }
        }
        InvocationEnd::Answered(ClientToolOutcome::Failed { code, message }) => {
            (format!("{code}: {message}"), true)
        }
        InvocationEnd::Unavailable(message) => {
            (format!("{CLIENT_TOOL_UNAVAILABLE}: {message}"), true)
        }
        InvocationEnd::Disconnected(message) => {
            (format!("{CLIENT_TOOL_DISCONNECTED}: {message}"), true)
        }
        InvocationEnd::TimedOut(message) => (format!("{CLIENT_TOOL_TIMEOUT}: {message}"), true),
        InvocationEnd::Cancelled => ("tool cancelled".to_owned(), true),
    };
    bcode_tool::ToolInvocationResponse {
        output,
        is_error,
        content: Vec::new(),
        full_output: None,
        result: None,
    }
}

/// Standard authorization facts for a client tool, from its declared policy class.
pub fn preparation(
    request: &bcode_tool::ToolPreparationRequest,
    descriptor: &ClientToolDescriptor,
) -> Result<bcode_tool::ToolPreparationResponse, String> {
    let definition = bcode_tool::ToolDefinition {
        name: descriptor.name.clone(),
        description: descriptor.description.clone(),
        input_schema: descriptor.input_schema.clone(),
    };
    let preparation = match descriptor.policy {
        ClientToolPolicy::ReadOnly => bcode_agent_profile::ToolPolicyPreparation::read_only(),
        ClientToolPolicy::Mutating => bcode_agent_profile::ToolPolicyPreparation::new(
            true,
            bcode_agent_profile::ToolPolicyOperation::Mutating,
        ),
    };
    bcode_agent_profile::prepare_tool_policy(request, &definition, preparation)
}

/// Handle one client tool request on its connection.
pub async fn handle_request(
    request: crate::request_routing::PermissionInteractionRequest,
    request_id: u64,
    client_id: ClientId,
    state: &std::sync::Arc<crate::ServerState>,
    writer: &crate::SharedWriter,
) -> Result<(), crate::ServerError> {
    use crate::request_routing::PermissionInteractionRequest;
    use bcode_ipc::{ErrorResponse, Response, ResponsePayload};
    let response = match request {
        PermissionInteractionRequest::RegisterClientTools { provider_id, tools } => {
            let plugin_tools = crate::collect_tool_definitions(state)
                .await
                .into_iter()
                .map(|tool| tool.name)
                .collect::<BTreeSet<_>>();
            let sink = ClientEventSink::new(client_id, writer.clone(), state.metrics.clone());
            match state.client_tools.register(
                client_id,
                sink,
                provider_id.clone(),
                tools,
                &plugin_tools,
            ) {
                Ok(tool_count) => Response::Ok(ResponsePayload::ClientToolsRegistered {
                    provider_id,
                    tool_count,
                }),
                Err(message) => Response::Err(ErrorResponse::new("client_tools_rejected", message)),
            }
        }
        PermissionInteractionRequest::ClaimClientToolSession { session_id } => {
            if state.sessions.session_summary(session_id).await.is_err() {
                Response::Err(ErrorResponse::new(
                    "session_not_found",
                    "session was not found in the selected state location",
                ))
            } else {
                match state.client_tools.claim(client_id, session_id) {
                    Ok(previous) => {
                        if let Some(previous) = previous {
                            let _ = previous
                                .send(Event::ClientToolClaimLost { session_id })
                                .await;
                        }
                        Response::Ok(ResponsePayload::ClientToolSessionClaimed { session_id })
                    }
                    Err(message) => {
                        Response::Err(ErrorResponse::new("client_tools_not_registered", message))
                    }
                }
            }
        }
        PermissionInteractionRequest::ReleaseClientToolSession { session_id } => {
            state.client_tools.release(client_id, session_id);
            Response::Ok(ResponsePayload::ClientToolSessionReleased { session_id })
        }
        PermissionInteractionRequest::RegisterClientAgentProfiles { profiles } => {
            let plugin_agents = crate::plugin_agent_ids(state).await;
            match state
                .client_tools
                .register_profiles(client_id, profiles, &plugin_agents)
            {
                Ok(profile_count) => {
                    Response::Ok(ResponsePayload::ClientAgentProfilesRegistered { profile_count })
                }
                Err(message) => Response::Err(ErrorResponse::new(
                    "client_agent_profiles_rejected",
                    message,
                )),
            }
        }
        PermissionInteractionRequest::ClientToolResult {
            invocation_id,
            outcome,
        } => match state
            .client_tools
            .complete(client_id, &invocation_id, outcome)
        {
            Ok(()) => Response::Ok(ResponsePayload::ClientToolResultAccepted { invocation_id }),
            Err(message) => {
                Response::Err(ErrorResponse::new("client_tool_result_rejected", message))
            }
        },
        _ => Response::Err(ErrorResponse::new(
            "unexpected_request",
            "request is not a client tool request",
        )),
    };
    crate::send_response(writer, request_id, response).await
}

/// Claim a session the connection just created when its origin names the connection's provider.
pub async fn claim_created_session(
    state: &crate::ServerState,
    client_id: ClientId,
    session: &bcode_session_models::SessionSummary,
) {
    if let Some(origin) = &session.origin
        && let Some(previous) =
            state
                .client_tools
                .claim_if_provider(client_id, session.id, &origin.app_id)
    {
        let _ = previous
            .send(Event::ClientToolClaimLost {
                session_id: session.id,
            })
            .await;
    }
}

/// Pin the tools a session's live claimant offers into a turn's tool catalog.
pub fn pin_offered_tools(
    state: &crate::ServerState,
    session_id: SessionId,
    snapshot: std::sync::Arc<crate::tool_catalog::Snapshot>,
) -> std::sync::Arc<crate::tool_catalog::Snapshot> {
    let offered = state.client_tools.offered_tools(session_id);
    if offered.is_empty() {
        snapshot
    } else {
        std::sync::Arc::new(snapshot.with_client_tools(offered))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, policy: ClientToolPolicy) -> ClientToolDescriptor {
        ClientToolDescriptor {
            name: name.to_owned(),
            description: "test tool".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
            policy,
            max_duration_ms: 1000,
        }
    }

    #[test]
    fn answers_are_idempotent_and_conflicts_are_rejected() {
        let registry = ClientToolRegistry::default();
        let owner = ClientId::new();
        let (sender, mut receiver) = oneshot::channel();
        registry.lock().pending.insert(
            "call".to_owned(),
            PendingInvocation {
                client_id: owner,
                sender,
            },
        );
        let done = ClientToolOutcome::Completed {
            content: "rows".to_owned(),
        };
        assert!(
            registry
                .complete(ClientId::new(), "call", done.clone())
                .is_err()
        );
        registry.complete(owner, "call", done.clone()).unwrap();
        assert_eq!(receiver.try_recv().unwrap(), done);
        registry.complete(owner, "call", done).unwrap();
        assert!(
            registry
                .complete(
                    owner,
                    "call",
                    ClientToolOutcome::Failed {
                        code: "x".to_owned(),
                        message: "y".to_owned()
                    }
                )
                .is_err()
        );
        assert!(
            registry
                .complete(
                    owner,
                    "missing",
                    ClientToolOutcome::Completed {
                        content: String::new()
                    }
                )
                .is_err()
        );
        registry.abandon("call");
    }

    #[test]
    fn oversized_answers_are_truncated_on_a_character_boundary() {
        let content = "é".repeat(MAX_CLIENT_TOOL_RESULT_BYTES);
        let response = tool_response(InvocationEnd::Answered(ClientToolOutcome::Completed {
            content,
        }));
        assert!(!response.is_error);
        assert!(response.output.len() <= MAX_CLIENT_TOOL_RESULT_BYTES + 128);
        assert!(response.output.ends_with("bytes]"));
    }

    #[test]
    fn declared_policy_becomes_standard_authorization_facts() {
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".to_owned(),
                tool_name: "app.write".to_owned(),
                arguments: serde_json::json!({}),
            },
            host_context: Vec::new(),
        };
        for (policy, requires_permission) in [
            (ClientToolPolicy::Mutating, true),
            (ClientToolPolicy::ReadOnly, false),
        ] {
            let prepared = preparation(&request, &tool("app.write", policy)).unwrap();
            let metadata = bcode_agent_profile::tool_policy_authorization_metadata(
                &prepared.authorization,
                "app.write",
            )
            .unwrap();
            assert_eq!(metadata.requires_permission, requires_permission);
        }
    }
}
