//! Client-owned, invocation-bound generic plugin service plumbing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bcode_plugin_sdk::workspace_access::{
    OP_WORKSPACE_ACCESS_AUTHORIZE, OP_WORKSPACE_RESOURCE_AUTHORIZE, WORKSPACE_ACCESS_INTERFACE_ID,
    WorkspaceAccessGrant, WorkspaceAccessRequest, WorkspaceResourceGrant, WorkspaceResourceRequest,
};
use bcode_plugin_sdk::{ServiceBridgeRequest, ServiceBridgeResponse, ServiceCancellation};
use bcode_session_models::{ClientId, SessionId};
use bcode_tool::ToolInvocationServiceResolution;

use super::ServerState;
use super::plugin_operations::{PluginServiceOperationResult, PublicPluginError};

#[derive(Debug, Default)]
pub struct InvocationRegistry {
    // Settled IDs remain tombstoned until disconnect: stale cancel cannot select new work.
    clients: BTreeMap<ClientId, BTreeMap<String, Option<ServiceCancellation>>>,
    reserved: BTreeMap<(ClientId, String), tokio::time::Instant>,
}

impl InvocationRegistry {
    fn admit(&mut self, client: ClientId, id: &str) -> Option<ServiceCancellation> {
        let entries = self.clients.entry(client).or_default();
        if entries.len() >= 4096 || entries.contains_key(id) {
            return None;
        }
        let cancellation = ServiceCancellation::default();
        entries.insert(id.into(), Some(cancellation.clone()));
        Some(cancellation)
    }

    fn reserve(&mut self, client: ClientId, id: &str, duration: Duration) -> bool {
        if self.admit(client, id).is_none() {
            return false;
        }
        self.reserved.insert(
            (client, id.to_owned()),
            tokio::time::Instant::now() + duration,
        );
        true
    }

    fn claim(&mut self, client: ClientId, id: &str) -> Option<ServiceCancellation> {
        if self.reserved.remove(&(client, id.to_owned())).is_some() {
            self.clients.get(&client)?.get(id)?.clone()
        } else {
            self.admit(client, id)
        }
    }

    fn cancel(&self, client: ClientId, id: &str) -> bool {
        let Some(Some(cancellation)) = self.clients.get(&client).and_then(|ids| ids.get(id)) else {
            return false;
        };
        cancellation.cancel();
        true
    }

    fn settle(&mut self, client: ClientId, id: &str) {
        if let Some(entry) = self
            .clients
            .get_mut(&client)
            .and_then(|ids| ids.get_mut(id))
        {
            *entry = None;
        }
    }

    fn disconnect(&mut self, client: ClientId) {
        self.reserved.retain(|(owner, _), _| *owner != client);
        if let Some(entries) = self.clients.remove(&client) {
            for cancellation in entries.into_values().flatten() {
                cancellation.cancel();
            }
        }
    }
}

pub fn reserve(
    state: &ServerState,
    client: ClientId,
    context: &bcode_ipc::PluginServiceContext,
) -> bool {
    valid_lifetime(context)
        && state
            .contextual_plugin_invocations
            .lock()
            .is_ok_and(|mut registry| {
                registry.reserve(
                    client,
                    &context.invocation_id,
                    Duration::from_millis(context.deadline_ms),
                )
            })
}

pub fn cancel(state: &ServerState, client: ClientId, id: &str) -> bool {
    state
        .contextual_plugin_invocations
        .lock()
        .is_ok_and(|registry| registry.cancel(client, id))
}

pub fn disconnect(state: &ServerState, client: ClientId) {
    if let Ok(mut registry) = state.contextual_plugin_invocations.lock() {
        registry.disconnect(client);
    }
}

struct Registration<'a> {
    state: &'a ServerState,
    client: ClientId,
    id: String,
    cancellation: ServiceCancellation,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Ok(mut registry) = self.state.contextual_plugin_invocations.lock() {
            registry.settle(self.client, &self.id);
        }
    }
}

fn rejected_stage(stage: &'static str) -> PublicPluginError {
    tracing::warn!(stage, "plugin invocation context rejected");
    rejected()
}

const fn rejected() -> PublicPluginError {
    PublicPluginError {
        code: "plugin_context_denied",
        message: "plugin invocation context or workspace access denied",
    }
}

fn valid_lifetime(context: &bcode_ipc::PluginServiceContext) -> bool {
    !context.invocation_id.is_empty()
        && context.invocation_id.len() <= 256
        && !context.invocation_id.chars().any(char::is_control)
        && (1..=900_000).contains(&context.deadline_ms)
}

async fn canonical_directory(path: &Path) -> Result<PathBuf, PublicPluginError> {
    if !path.is_absolute() {
        return Err(rejected());
    }
    let path = tokio::fs::canonicalize(path)
        .await
        .map_err(|_| rejected())?;
    if !tokio::fs::metadata(&path)
        .await
        .map_err(|_| rejected())?
        .is_dir()
    {
        return Err(rejected());
    }
    Ok(path)
}

struct BoundContext {
    id: String,
    root: PathBuf,
    session: Option<SessionId>,
    config: bcode_config::BcodeConfig,
    agent: String,
    scope: bcode_plugin::PluginInvocationScope,
}

async fn validated_agent(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
    hint: Option<&str>,
) -> Result<String, PublicPluginError> {
    if state
        .plugins
        .registry()
        .service_registry()
        .unique_provider(super::AGENT_PROFILE_INTERFACE_ID)
        .is_err()
    {
        return Err(rejected_stage("agent_provider_or_identity"));
    }
    let request = super::AgentListRequest {
        effective_config_toml: Some(Box::new(
            bcode_config::encode_effective_config(config)
                .map_err(|_| rejected_stage("agent_catalog_or_config"))?,
        )),
    };
    let profiles = state
        .plugins
        .invoke_service_by_interface_json::<_, super::AgentList>(
            super::AGENT_PROFILE_INTERFACE_ID,
            super::OP_LIST_AGENTS,
            &request,
        )
        .await
        .map_err(|_| rejected_stage("agent_catalog_or_config"))?
        .agents;
    let id = hint.map_or_else(|| super::default_agent_id(&profiles), str::to_owned);
    if profiles.iter().filter(|profile| profile.id == id).count() != 1 {
        return Err(rejected_stage("agent_provider_or_identity"));
    }
    Ok(id)
}

async fn bind(
    state: &ServerState,
    client: ClientId,
    context: &bcode_ipc::PluginServiceContext,
) -> Result<BoundContext, PublicPluginError> {
    let runtime = state
        .client_runtime_context(client)
        .await
        .ok_or_else(|| rejected_stage("caller_runtime_or_workspace_missing"))?;
    let session = context
        .workspace
        .session_id
        .as_deref()
        .map(str::parse::<SessionId>)
        .transpose()
        .map_err(|_| rejected_stage("effective_config_decode"))?;
    let (root, config, agent, scope) = if let Some(session) = session {
        if state.attached_client_sessions.lock().await.get(&client) != Some(&session)
            || !state.sessions.session_is_owned(session).await
        {
            return Err(rejected_stage("canonical_workspace_mismatch"));
        }
        let cwd = state
            .sessions
            .session_working_directory(session)
            .await
            .map_err(|_| rejected_stage("effective_config_decode"))?;
        let root = canonical_directory(&cwd).await?;
        if let Some(hint) = &context.workspace.launch_root
            && canonical_directory(hint).await? != root
        {
            return Err(rejected_stage("canonical_workspace_mismatch"));
        }
        let config = state.session_config(session).await;
        let selected = super::session_agent_selection(state, session).await;
        if context
            .agent_id
            .as_ref()
            .is_some_and(|hint| hint != &selected)
        {
            return Err(rejected_stage("canonical_workspace_mismatch"));
        }
        let agent = validated_agent(state, &config, Some(&selected)).await?;
        (
            root,
            config,
            agent,
            bcode_plugin::PluginInvocationScope::session(session.to_string())
                .with_client_id(client.to_string()),
        )
    } else {
        let launch = context
            .workspace
            .launch_root
            .as_ref()
            .ok_or_else(|| rejected_stage("caller_runtime_or_workspace_missing"))?;
        let root = canonical_directory(launch).await?;
        let client_root = runtime
            .working_directory
            .as_ref()
            .ok_or_else(|| rejected_stage("caller_runtime_or_workspace_missing"))?;
        if canonical_directory(client_root).await? != root {
            return Err(rejected_stage("canonical_workspace_mismatch"));
        }
        let config = runtime
            .effective_config_toml
            .as_deref()
            .map(|text| bcode_config::decode_effective_config(text))
            .transpose()
            .map_err(|_| rejected_stage("effective_config_decode"))?
            .unwrap_or_else(|| state.startup_config.clone());
        let agent = validated_agent(state, &config, context.agent_id.as_deref()).await?;
        (
            root,
            config,
            agent,
            bcode_plugin::PluginInvocationScope::Global,
        )
    };
    Ok(BoundContext {
        id: context.invocation_id.clone(),
        root,
        session,
        config,
        agent,
        scope,
    })
}

/// Invoke with one validated client context; nested providers inherit its bridge.
#[allow(clippy::too_many_arguments)]
fn decode_command_payload(
    payload: &[u8],
) -> Result<bcode_command::InvokeCommandRequest, PublicPluginError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Context {
        #[serde(default)]
        session_id: Option<SessionId>,
        working_directory: PathBuf,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        command_id: String,
        #[serde(default)]
        args: BTreeMap<String, String>,
        #[serde(default)]
        context: Option<Context>,
    }
    let input: Input = serde_json::from_slice(payload).map_err(|_| rejected())?;
    if input.command_id.is_empty()
        || input.command_id.len() > 256
        || input.command_id.chars().any(char::is_control)
        || input.args.len() > 1024
    {
        return Err(rejected());
    }
    Ok(bcode_command::InvokeCommandRequest {
        command_id: input.command_id,
        args: input.args,
        context: input
            .context
            .map(|context| bcode_command::CommandInvocationContext {
                session_id: context.session_id,
                working_directory: context.working_directory,
            }),
    })
}

fn canonical_command_payload(
    mut command: bcode_command::InvokeCommandRequest,
    bound: &BoundContext,
) -> Result<Vec<u8>, PublicPluginError> {
    if command
        .context
        .as_ref()
        .and_then(|context| context.session_id)
        != bound.session
    {
        return Err(rejected());
    }
    command.context = Some(bcode_command::CommandInvocationContext {
        session_id: bound.session,
        working_directory: bound.root.clone(),
    });
    let payload = serde_json::to_vec(&command).map_err(|_| rejected())?;
    if payload.len() > 256 * 1024 {
        return Err(rejected());
    }
    Ok(payload)
}

fn admitted_command(
    state: &ServerState,
    plugin: &str,
    interface: &str,
    operation: &str,
    payload: &[u8],
) -> Result<Option<bcode_command::InvokeCommandRequest>, PublicPluginError> {
    let registry = state.plugins.registry();
    if interface == bcode_command::COMMAND_INTERFACE_ID
        && operation == bcode_command::OP_INVOKE_COMMAND
    {
        let command = decode_command_payload(payload)?;
        let service_declared = declared_route(state, plugin, interface, operation);
        let legacy_declared = state
            .plugins
            .command_contributions()
            .iter()
            .any(|owned| owned.plugin_id == plugin && owned.command.id == command.command_id);
        let registered_declared = [
            bcode_command::CommandSurface::Palette,
            bcode_command::CommandSurface::Slash,
        ]
        .iter()
        .flat_map(|surface| state.plugins.registered_command_contributions(surface))
        .any(|contribution| {
            matches!(contribution.action,
                bcode_command::CommandAction::Plugin { plugin_id, command_id }
                if plugin_id == plugin && command_id == command.command_id)
        });
        if !service_declared {
            tracing::warn!(
                plugin_id = plugin,
                interface_id = interface,
                operation,
                stage = "command_route_declaration",
                "command invocation rejected"
            );
            return Err(PublicPluginError {
                code: "plugin_command_route_unavailable",
                message: "plugin command service must explicitly declare the invoke operation; check the plugin manifest",
            });
        }
        if !(legacy_declared || registered_declared) {
            return Err(rejected_stage("command_not_registered"));
        }
        Ok(Some(command))
    } else {
        if operation != bcode_plugin_sdk::composer::OP_COMPOSER_SEARCH {
            return Err(rejected());
        }
        let request: bcode_plugin_sdk::composer::ComposerSearchRequest =
            serde_json::from_slice(payload).map_err(|_| rejected())?;
        let contributions: Vec<_> = registry
            .composer_contributions()
            .into_iter()
            .filter(|owned| {
                owned.plugin_id == plugin && owned.contribution.service_interface_id == interface
            })
            .map(|owned| owned.contribution)
            .collect();
        request
            .validate_for_contributions(&contributions)
            .map_err(|_| rejected())?;
        Ok(None)
    }
}

pub async fn invoke(
    state: &ServerState,
    client: ClientId,
    context: bcode_ipc::PluginServiceContext,
    plugin: String,
    interface: String,
    operation: String,
    payload: Vec<u8>,
) -> Result<PluginServiceOperationResult, PublicPluginError> {
    if !valid_lifetime(&context) || payload.len() > 256 * 1024 {
        return Err(rejected_stage("lifetime_or_payload_bound"));
    }
    let command = admitted_command(state, &plugin, &interface, &operation, &payload)?;
    let (cancellation, deadline) = {
        let mut registry = state
            .contextual_plugin_invocations
            .lock()
            .map_err(|_| rejected())?;
        let deadline = registry
            .reserved
            .get(&(client, context.invocation_id.clone()))
            .copied()
            .unwrap_or_else(|| {
                tokio::time::Instant::now() + Duration::from_millis(context.deadline_ms)
            });
        let cancellation = registry
            .claim(client, &context.invocation_id)
            .ok_or_else(|| rejected_stage("invocation_receipt_claim"))?;
        drop(registry);
        (cancellation, deadline)
    };
    let registration = Registration {
        state,
        client,
        id: context.invocation_id.clone(),
        cancellation,
    };
    let work = async {
        let bound = bind(state, client, &context).await.inspect_err(|_| {
            tracing::warn!(
                plugin_id = plugin,
                interface_id = interface,
                operation,
                stage = "caller_context",
                "plugin invocation rejected"
            );
        })?;
        if !route_policy_allows(&bound, &plugin, &interface, &operation) {
            tracing::warn!(
                plugin_id = plugin,
                interface_id = interface,
                operation,
                stage = "route_policy",
                "plugin invocation rejected"
            );
            return Err(rejected());
        }
        let payload = if let Some(command) = command {
            canonical_command_payload(command, &bound)?
        } else {
            payload
        };
        let response = route(state, &bound, &plugin, interface, operation, payload, 0).await?;
        if response.payload.len() > 2 * 1024 * 1024 {
            return Err(rejected());
        }
        Ok(PluginServiceOperationResult {
            payload: response.payload,
            error: response.error.map(|error| {
                super::plugin_operations::PluginServiceOperationError {
                    code: error.code,
                    message: error.message,
                }
            }),
        })
    };
    let result = tokio::select! {
        biased;
        () = wait_cancel(&registration.cancellation) => Err(PublicPluginError { code: "plugin_cancelled", message: "plugin invocation cancelled" }),
        result = tokio::time::timeout_at(deadline, work) =>
            result.unwrap_or(Err(PublicPluginError { code: "plugin_deadline", message: "plugin invocation deadline exceeded" })),
    };
    // Settle and cancel only after authorization/execution or its terminal fence.
    drop(registration);
    result
}

async fn wait_cancel(cancellation: &ServiceCancellation) {
    while !cancellation.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn bound_identity(id: &str, bound: &str) -> bool {
    id.is_empty() || id == bound
}

fn command_application_request_allowed(
    command_id: Option<&str>,
    bound_id: &str,
    request: &bcode_tool::ToolInvocationServiceRequest,
) -> bool {
    command_id.is_some_and(|command_id| {
        bound_identity(&request.invocation_id, bound_id) || request.invocation_id == command_id
    }) && request.route_id.is_none()
        && (request.interface_id == bcode_plugin_sdk::SESSION_DERIVATION_INTERFACE_ID
            || request.interface_id == bcode_plugin_sdk::milestones::MILESTONES_HOST_INTERFACE_ID)
}

fn route<'a>(
    state: &'a ServerState,
    bound: &'a BoundContext,
    plugin: &'a str,
    interface: String,
    operation: String,
    payload: Vec<u8>,
    depth: usize,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<bcode_plugin::ServiceResponse, PublicPluginError>>
            + Send
            + 'a,
    >,
> {
    Box::pin(async move {
        if depth >= 8 {
            return Err(rejected());
        }
        let command_id = (depth == 0
            && interface == bcode_command::COMMAND_INTERFACE_ID
            && operation == bcode_command::OP_INVOKE_COMMAND)
            .then(|| serde_json::from_slice::<bcode_command::InvokeCommandRequest>(&payload).ok())
            .flatten()
            .map(|request| request.command_id);
        let _model_owner_guard = super::plugin_operations::ModelOwnerGuard {
            host: std::sync::Arc::clone(&state.model_host),
            owner: command_id
                .as_ref()
                .map(|_| (plugin.to_owned(), bound.id.clone())),
        };
        let (bridge, mut requests) = super::server_plugin_bridge();
        let invocation = state.plugins.invoke_service_with_bridge_scoped(
            plugin,
            interface,
            operation,
            payload,
            bound.scope.clone(),
            Some(bridge),
        );
        tokio::pin!(invocation);
        loop {
            tokio::select! {
                result = &mut invocation => return result.map_err(|error| super::plugin_operations::normalize_error(&error)),
                Some(call) = requests.recv() => {
                    let result = match call.request {
                        ServiceBridgeRequest::InvokeService(mut request)
                            if depth == 0 && command_id.is_some()
                                && request.interface_id == bcode_model::host_operation::INTERFACE_ID
                                && request.route_id.is_none()
                                && (request.invocation_id == bound.id || command_id.as_deref() == Some(request.invocation_id.as_str())) => {
                            // Command clients retain their semantic correlation ID; execution,
                            // idempotency and cancellation are fenced by this unique receipt.
                            request.invocation_id.clone_from(&bound.id);
                            if call.cancellation.is_cancelled() { ToolInvocationServiceResolution::Cancelled }
                            else { state.model_host.clone().dispatch(plugin, request).await }
                        }
                        ServiceBridgeRequest::InvokeService(request)
                            if command_application_request_allowed(command_id.as_deref(), &bound.id, &request) => {
                            // Only the admitted top-level command receives the existing session
                            // application bridge. Nested providers and composers cannot inherit it.
                            match super::resolve_command_plugin_bridge_request(
                                state,
                                bound.session,
                                ServiceBridgeRequest::InvokeService(request),
                                &call.cancellation,
                            ).await {
                                Ok(ServiceBridgeResponse::Service(result)) => result,
                                _ => denied(),
                            }
                        }
                        ServiceBridgeRequest::InvokeService(request)
                            if bound_identity(&request.invocation_id, &bound.id) && request.route_id.is_none() => {
                            if request.interface_id == WORKSPACE_ACCESS_INTERFACE_ID {
                                if request.operation == OP_WORKSPACE_ACCESS_AUTHORIZE {
                                    authorize_workspace(state, bound, request.payload).await
                                } else if request.operation == OP_WORKSPACE_RESOURCE_AUTHORIZE {
                                    authorize_resource(state, bound, request.payload).await
                                } else if request.operation == bcode_plugin_sdk::workspace_access::OP_EXTERNAL_RESOURCE_AUTHORIZE {
                                    authorize_external_resource(state, bound, request.payload).await
                                } else { ToolInvocationServiceResolution::Unsupported }
                            } else {
                                route_nested(state, bound, request, depth + 1).await
                            }
                        }
                        _ => denied(),
                    };
                    let _sent = call.response.send(Ok(ServiceBridgeResponse::Service(result)));
                }
            }
        }
    })
}

fn denied() -> ToolInvocationServiceResolution {
    ToolInvocationServiceResolution::Failed {
        code: "workspace_access_denied".into(),
        message: "invocation workspace discovery denied".into(),
    }
}

fn declared_route(state: &ServerState, plugin: &str, interface: &str, operation: &str) -> bool {
    state
        .plugins
        .registry()
        .manifests()
        .get(plugin)
        .is_some_and(|manifest| {
            let services: Vec<_> = manifest
                .services
                .iter()
                .filter(|service| service.interface_id == interface)
                .collect();
            services.len() == 1 && services[0].permits_invocation(operation)
        })
}

fn route_policy_key(plugin: &str, interface: &str, operation: &str) -> String {
    format!("plugin.service.{plugin}.{interface}.{operation}")
}

fn route_policy_allows(
    bound: &BoundContext,
    plugin: &str,
    interface: &str,
    operation: &str,
) -> bool {
    let key = route_policy_key(plugin, interface, operation);
    // Manifest opt-in grants routing only. Explicit generic agent disables/asks/denies
    // win; no tool execution, exchanges or artifact writes exist on this bridge.
    let agent = bound
        .config
        .agent
        .get(&bound.agent)
        .or_else(|| bound.config.agent.get("build"));
    agent.is_none_or(|agent| {
        agent.tools.get(&key) != Some(&false)
            && agent
                .permission
                .tools
                .get(&key)
                .or_else(|| agent.permission.tools.get("*"))
                .is_none_or(|action| {
                    serde_json::to_value(action).ok() == Some(serde_json::json!("allow"))
                })
    })
}

async fn route_nested(
    state: &ServerState,
    bound: &BoundContext,
    request: bcode_tool::ToolInvocationServiceRequest,
    depth: usize,
) -> ToolInvocationServiceResolution {
    // Only registered plugin service providers participate. Application-only host routes
    // (workflow, derivation, inference, etc.) are deliberately not granted here.
    let Ok(plugin) = state
        .plugins
        .registry()
        .service_registry()
        .unique_provider(&request.interface_id)
    else {
        return ToolInvocationServiceResolution::Unsupported;
    };
    if !declared_route(state, plugin, &request.interface_id, &request.operation)
        || !route_policy_allows(bound, plugin, &request.interface_id, &request.operation)
    {
        return denied();
    }
    let Ok(payload) = serde_json::to_vec(&request.payload) else {
        return denied();
    };
    if payload.len() > 256 * 1024 {
        return denied();
    }
    match route(
        state,
        bound,
        plugin,
        request.interface_id,
        request.operation,
        payload,
        depth,
    )
    .await
    {
        Ok(response) if response.error.is_none() => serde_json::from_slice(&response.payload)
            .map_or_else(
                |_| denied(),
                |payload| ToolInvocationServiceResolution::Responded { payload },
            ),
        _ => denied(),
    }
}

/// Authorize one exact workspace resource read, never discovery or attachment.
/// The plugin must still confine its opened descriptor and bound any content read.
async fn authorize_resource(
    state: &ServerState,
    bound: &BoundContext,
    payload: serde_json::Value,
) -> ToolInvocationServiceResolution {
    let Ok(request) = serde_json::from_value::<WorkspaceResourceRequest>(payload) else {
        return denied();
    };
    if !request.is_normalized() {
        return denied();
    }
    let requested = bound.root.join(&request.path);
    authorize_resource_path(state, bound, requested, bound.root.clone(), request.path).await
}

async fn authorize_external_resource(
    state: &ServerState,
    bound: &BoundContext,
    payload: serde_json::Value,
) -> ToolInvocationServiceResolution {
    let Ok(request) = serde_json::from_value::<
        bcode_plugin_sdk::workspace_access::ExternalResourceRequest,
    >(payload) else {
        return denied();
    };
    if !request.is_normalized() {
        return denied();
    }
    let Some(parent) = request.path.parent() else {
        return denied();
    };
    let Some(name) = request.path.file_name().and_then(|name| name.to_str()) else {
        return denied();
    };
    let root = parent.to_path_buf();
    let name = name.to_owned();
    authorize_resource_path(state, bound, request.path, root, name).await
}

async fn authorize_resource_path(
    state: &ServerState,
    bound: &BoundContext,
    requested: std::path::PathBuf,
    authorized_root: std::path::PathBuf,
    relative_path: String,
) -> ToolInvocationServiceResolution {
    let Ok(path) = tokio::fs::canonicalize(&requested).await else {
        return denied();
    };
    // Do not broaden a request for one lexical resource into a symlink target.
    if path != requested || !path.starts_with(&authorized_root) {
        return denied();
    }
    if !tokio::fs::metadata(&path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
    {
        return denied();
    }
    let evaluation = bcode_agent_profile::EvaluateToolCallRequest {
        session_id: bound.session.unwrap_or_default(),
        agent_id: bound.agent.clone(),
        tool_name: "filesystem.read".into(),
        operation: bcode_agent_profile::ToolPolicyOperation::Read {
            paths: vec![path.to_string_lossy().into_owned()],
        },
        aliases: vec!["filesystem.read".into(), "read".into()],
        requires_permission: false,
        policy_profile: None,
        cwd: Some(bound.root.to_string_lossy().into_owned()),
        effective_config_toml: match bcode_config::encode_effective_config(&bound.config) {
            Ok(config) => Some(Box::new(config)),
            Err(_) => return denied(),
        },
    };
    let decision = state
        .plugins
        .invoke_service_by_interface_json::<_, bcode_agent_profile::EvaluateToolCallResponse>(
            super::AGENT_PROFILE_INTERFACE_ID,
            super::OP_EVALUATE_TOOL_CALL,
            &evaluation,
        )
        .await;
    if !decision.is_ok_and(|decision| decision.decision == super::AgentDecision::Allow) {
        return denied();
    }
    if let Some(session) = bound.session {
        let metadata = super::ToolPolicyAuthorizationMetadata {
            requires_permission: false,
            aliases: evaluation.aliases,
            compatibility_aliases: Vec::new(),
            capabilities: Vec::new(),
            permission_category: Some("read".into()),
            operation: evaluation.operation,
        };
        if !matches!(
            super::evaluate_active_skill_tool_policy_with_metadata(
                state,
                session,
                "filesystem.read",
                &metadata
            )
            .await,
            super::SkillToolPolicyOutcome::NoOpinion
                | super::SkillToolPolicyOutcome::Allow { .. }
                | super::SkillToolPolicyOutcome::Warn { .. }
        ) {
            return denied();
        }
    }
    let grant = WorkspaceResourceGrant {
        workspace_root: authorized_root,
        path: relative_path,
    };
    serde_json::to_value(grant).map_or_else(
        |_| denied(),
        |payload| ToolInvocationServiceResolution::Responded { payload },
    )
}

async fn recursive_skills_unrestricted(state: &ServerState, session: SessionId) -> bool {
    let ids = state
        .active_skills
        .lock()
        .await
        .get(&session)
        .cloned()
        .unwrap_or_default();
    if ids.is_empty() {
        return true;
    }
    let Some(registry) = state.session_skills(session).await else {
        return false;
    };
    ids.iter().all(|id| {
        registry
            .describe(id)
            .is_ok_and(|manifest| skill_has_no_resource_restrictions(&manifest.permission_policy))
    })
}

fn skill_has_no_resource_restrictions(policy: &bcode_skill_models::SkillPermissionPolicy) -> bool {
    policy.mode == bcode_skill_models::SkillPermissionMode::Disabled
        || (policy.mode == bcode_skill_models::SkillPermissionMode::Inherit
            && policy.tools.is_empty()
            && policy.categories.is_empty())
}

async fn authorize_workspace(
    state: &ServerState,
    bound: &BoundContext,
    payload: serde_json::Value,
) -> ToolInvocationServiceResolution {
    let Ok(request) = serde_json::from_value::<WorkspaceAccessRequest>(payload) else {
        return denied();
    };
    if !request.is_normalized() {
        return denied();
    }
    let Ok(path) = canonical_directory(&bound.root.join(&request.directory)).await else {
        return denied();
    };
    if !path.starts_with(&bound.root) {
        return denied();
    }
    // A root-only policy decision cannot certify arbitrary descendant paths. Until
    // restrictions can be represented as literal exclusions, reject configured read
    // rules other than unconditional allows rather than overlooking denied children.
    let effective = bound
        .config
        .agent
        .get(&bound.agent)
        .or_else(|| bound.config.agent.get("build"));
    if effective.is_some_and(|agent| {
        agent
            .permission
            .read
            .values()
            .any(|action| serde_json::to_value(action).ok() != Some(serde_json::json!("allow")))
    }) {
        return denied();
    }
    let evaluation = bcode_agent_profile::EvaluateToolCallRequest {
        session_id: bound.session.unwrap_or_default(),
        agent_id: bound.agent.clone(),
        tool_name: "filesystem.list".into(),
        operation: bcode_agent_profile::ToolPolicyOperation::Read {
            paths: vec![path.to_string_lossy().into_owned()],
        },
        aliases: vec!["filesystem.list".into(), "ls".into(), "read".into()],
        requires_permission: false,
        policy_profile: None,
        cwd: Some(bound.root.to_string_lossy().into_owned()),
        effective_config_toml: match bcode_config::encode_effective_config(&bound.config) {
            Ok(config) => Some(Box::new(config)),
            Err(_) => return denied(),
        },
    };
    let decision = state
        .plugins
        .invoke_service_by_interface_json::<_, bcode_agent_profile::EvaluateToolCallResponse>(
            super::AGENT_PROFILE_INTERFACE_ID,
            super::OP_EVALUATE_TOOL_CALL,
            &evaluation,
        )
        .await;
    if !decision.is_ok_and(|decision| decision.decision == super::AgentDecision::Allow) {
        return denied();
    }
    if let Some(session) = bound.session {
        // Skill path rules may constrain descendants independently of the requested
        // root. Do not certify recursive enumeration from a root-only evaluation.
        if request.recursive && !recursive_skills_unrestricted(state, session).await {
            return denied();
        }
        let metadata = super::ToolPolicyAuthorizationMetadata {
            requires_permission: false,
            aliases: evaluation.aliases,
            compatibility_aliases: Vec::new(),
            capabilities: Vec::new(),
            permission_category: Some("read".into()),
            operation: evaluation.operation,
        };
        if !matches!(
            super::evaluate_active_skill_tool_policy_with_metadata(
                state,
                session,
                "filesystem.list",
                &metadata
            )
            .await,
            super::SkillToolPolicyOutcome::NoOpinion
                | super::SkillToolPolicyOutcome::Allow { .. }
                | super::SkillToolPolicyOutcome::Warn { .. }
        ) {
            return denied();
        }
    }
    let grant = WorkspaceAccessGrant {
        workspace_root: bound.root.clone(),
        excluded_prefixes: Vec::new(),
    };
    serde_json::to_value(grant).map_or_else(
        |_| denied(),
        |payload| ToolInvocationServiceResolution::Responded { payload },
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn command_payload_is_typed_and_rejects_unknown_or_invalid_input() {
        for payload in [
            br#"{"command_id":"ok","unexpected":true}"#.as_slice(),
            br#"{"command_id":""}"#.as_slice(),
            br#"{"command_id":"bad\nroute"}"#.as_slice(),
            br#"{"command_id":"ok","args":{"n":42}}"#.as_slice(),
            br#"{"command_id":"ok","context":{"working_directory":"/","authority":true}}"#
                .as_slice(),
            br#"{"operation":"arbitrary","payload":[]}"#.as_slice(),
        ] {
            assert!(decode_command_payload(payload).is_err());
        }
        let command = decode_command_payload(
            r#"{"command_id":"declared","args":{"literal":"λ"}}"#.as_bytes(),
        )
        .unwrap();
        assert_eq!(command.command_id, "declared");
        assert_eq!(command.args["literal"], "λ");
    }

    #[test]
    fn command_context_uses_verified_workspace_and_rejects_session_substitution() {
        let temp = tempfile::tempdir().unwrap();
        let bound = bound_fixture(temp.path().canonicalize().unwrap());
        let request = bcode_command::InvokeCommandRequest {
            command_id: "declared".to_owned(),
            args: BTreeMap::new(),
            context: Some(bcode_command::CommandInvocationContext {
                session_id: bound.session,
                working_directory: PathBuf::from("/untrusted/private"),
            }),
        };
        let canonical: bcode_command::InvokeCommandRequest =
            serde_json::from_slice(&canonical_command_payload(request.clone(), &bound).unwrap())
                .unwrap();
        assert_eq!(canonical.context.unwrap().working_directory, bound.root);
        let mut foreign = request;
        foreign.context.as_mut().unwrap().session_id = Some(SessionId::new());
        assert!(canonical_command_payload(foreign, &bound).is_err());
        let mut denied_bound = bound;
        denied_bound
            .config
            .agent
            .entry(denied_bound.agent.clone())
            .or_default()
            .tools
            .insert(
                route_policy_key(
                    "owner",
                    bcode_command::COMMAND_INTERFACE_ID,
                    bcode_command::OP_INVOKE_COMMAND,
                ),
                false,
            );
        assert!(!route_policy_allows(
            &denied_bound,
            "owner",
            bcode_command::COMMAND_INTERFACE_ID,
            bcode_command::OP_INVOKE_COMMAND
        ));
    }

    #[test]
    fn reserved_cancel_precedes_claim_without_changing_ownership() {
        let client = ClientId::new();
        let foreign = ClientId::new();
        let mut registry = InvocationRegistry::default();
        assert!(registry.reserve(client, "queued", Duration::from_secs(1)));
        assert!(!registry.cancel(foreign, "queued"));
        assert!(registry.cancel(client, "queued"));
        assert!(registry.claim(client, "queued").unwrap().is_cancelled());
        registry.settle(client, "queued");
        assert!(registry.claim(client, "queued").is_none());
    }

    #[test]
    fn unrelated_inherited_skills_do_not_disable_recursive_discovery() {
        let mut policy = bcode_skill_models::SkillPermissionPolicy::default();
        assert!(skill_has_no_resource_restrictions(&policy));
        policy.categories.push("read".into());
        assert!(!skill_has_no_resource_restrictions(&policy));
        policy.mode = bcode_skill_models::SkillPermissionMode::Disabled;
        assert!(skill_has_no_resource_restrictions(&policy));
        policy.mode = bcode_skill_models::SkillPermissionMode::Strict;
        assert!(!skill_has_no_resource_restrictions(&policy));
    }

    #[test]
    fn invocation_ownership_cancellation_and_tombstones() {
        let client = ClientId::new();
        let other = ClientId::new();
        let mut registry = InvocationRegistry::default();
        let token = registry.admit(client, "search").unwrap();
        assert!(!registry.cancel(other, "search"));
        assert!(!registry.cancel(client, "missing"));
        assert!(!token.is_cancelled());
        assert!(registry.cancel(client, "search"));
        assert!(token.is_cancelled());
        registry.settle(client, "search");
        assert!(!registry.cancel(client, "search"));
        assert!(registry.admit(client, "search").is_none());
        let token = registry.admit(client, "next").unwrap();
        registry.disconnect(client);
        assert!(token.is_cancelled());
    }

    fn wait_search_payload() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "contribution_id": "wait",
            "request_id": "wait-search",
            "document": { "document_id": "wait-input", "revision": 1, "text": "x", "cursor": 1 },
            "activation": { "range": { "start": 0, "end": 1 }, "query": "x" },
            "max_results": 1
        }))
        .unwrap()
    }

    const WAIT_MANIFEST: &str = r#"
id = "test.composer-wait"
name = "Cancellation fixture"
version = "0.0.1"
[[services]]
interface_id = "bcode.composer/v1"
[[composer_contributions]]
id = "wait"
activation_kind = "wait"
service_interface_id = "bcode.composer/v1"
[runtime]
type = "native"
abi_version = 5
library = "libtest_composer_wait.dylib"
event_symbol = "bcode_plugin_handle_event_v1"
"#;

    const COMMAND_MANIFEST: &str = r#"
id = "test.command-context"
name = "Typed command fixture"
version = "0.0.1"
[[services]]
interface_id = "bcode.command/v1"
invocation_operations = ["invoke"]
[[command_contributions]]
id = "literal"
title = "Literal fixture"
[runtime]
type = "native"
abi_version = 5
library = "libtest_command_context.dylib"
"#;

    #[derive(Default)]
    struct CommandPlugin;

    impl bcode_plugin_sdk::RustPlugin for CommandPlugin {
        fn invoke_service(
            &mut self,
            context: bcode_plugin_sdk::NativeServiceContext,
        ) -> bcode_plugin_sdk::ServiceResponse {
            let request: bcode_command::InvokeCommandRequest =
                serde_json::from_slice(&context.request.payload).unwrap();
            let response = bcode_command::InvokeCommandResponse {
                success: true,
                message: None,
                updated_model: None,
                updated_provider: None,
                updated_thinking: None,
                effects: vec![bcode_command::CommandEffect::AppendText {
                    text: request
                        .context
                        .unwrap()
                        .working_directory
                        .display()
                        .to_string(),
                    format: bcode_command::CommandTextFormat::PlainText,
                }],
            };
            bcode_plugin_sdk::ServiceResponse::ok(serde_json::to_vec(&response).unwrap())
        }
    }

    fn command_plugins() -> [bcode_plugin::StaticBundledPlugin; 2] {
        [
            bcode_plugin::StaticBundledPlugin::new(
                COMMAND_MANIFEST,
                bcode_plugin_sdk::static_plugin_vtable!(CommandPlugin, COMMAND_MANIFEST),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../plugins/default-agents-plugin/bcode-plugin.toml"),
                bcode_default_agents_plugin::static_plugin(),
            ),
        ]
    }

    fn command_state() -> ServerState {
        let plugins = command_plugins();
        super::super::tests::test_server_state_with_plugins(
            bcode_session::SessionManager::default(),
            bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
                &bcode_plugin::PluginSelection {
                    mode: bcode_plugin::PluginSelectionMode::Explicit,
                    enabled: std::collections::BTreeSet::from([
                        "test.command-context".into(),
                        "bcode.default-agents".into(),
                    ]),
                    disabled: std::collections::BTreeSet::new(),
                },
                &plugins,
            )
            .unwrap(),
        )
    }

    fn command_payload(id: &str) -> Vec<u8> {
        serde_json::to_vec(&bcode_command::InvokeCommandRequest {
            command_id: id.to_owned(),
            args: BTreeMap::new(),
            context: Some(bcode_command::CommandInvocationContext {
                session_id: None,
                working_directory: PathBuf::from("/untrusted"),
            }),
        })
        .unwrap()
    }

    fn command_context(root: &Path, id: &str) -> bcode_ipc::PluginServiceContext {
        bcode_ipc::PluginServiceContext {
            agent_id: Some("build".into()),
            invocation_id: id.to_owned(),
            workspace: bcode_plugin_sdk::WorkspaceContext {
                session_id: None,
                launch_root: Some(root.to_path_buf()),
            },
            deadline_ms: 1000,
        }
    }

    async fn assert_command_context(state: &ServerState, client: ClientId, root: &Path) {
        let result = invoke(
            state,
            client,
            command_context(root, "command-1"),
            "test.command-context".into(),
            bcode_command::COMMAND_INTERFACE_ID.into(),
            bcode_command::OP_INVOKE_COMMAND.into(),
            command_payload("literal"),
        )
        .await
        .unwrap();
        let response: bcode_command::InvokeCommandResponse =
            serde_json::from_slice(&result.payload).unwrap();
        assert!(response.success);
        assert_eq!(
            response.effects,
            vec![bcode_command::CommandEffect::AppendText {
                text: root.display().to_string(),
                format: bcode_command::CommandTextFormat::PlainText,
            }]
        );
        assert!(
            !cancel(state, client, "command-1"),
            "settled invocation is not active cancellation"
        );
    }

    #[test]
    fn missing_command_route_has_actionable_secret_safe_error() {
        let state = command_state();
        let error = admitted_command(
            &state,
            "not.loaded",
            bcode_command::COMMAND_INTERFACE_ID,
            bcode_command::OP_INVOKE_COMMAND,
            &command_payload("literal"),
        )
        .unwrap_err();
        drop(state);
        assert_eq!(error.code, "plugin_command_route_unavailable");
        assert_eq!(
            error.message,
            "plugin command service must explicitly declare the invoke operation; check the plugin manifest"
        );
    }

    #[test]
    fn command_application_bridge_does_not_inherit_or_accept_forged_identity() {
        let allowed = |depth, invocation_id: &str, route_id: Option<String>, interface: &str| {
            let command_id = (depth == 0).then_some("registered.command");
            command_application_request_allowed(
                command_id,
                "bound-invocation",
                &bcode_tool::ToolInvocationServiceRequest {
                    invocation_id: invocation_id.into(),
                    request_id: "request".into(),
                    route_id,
                    interface_id: interface.into(),
                    operation: "request".into(),
                    payload: serde_json::json!({}),
                },
            )
        };
        let interface = bcode_plugin_sdk::SESSION_DERIVATION_INTERFACE_ID;
        assert!(allowed(0, "bound-invocation", None, interface));
        assert!(allowed(0, "registered.command", None, interface));
        assert!(allowed(0, "", None, interface));
        assert!(!allowed(1, "bound-invocation", None, interface));
        assert!(!allowed(0, "forged", None, interface));
        assert!(!allowed(
            0,
            "bound-invocation",
            Some("route".into()),
            interface
        ));
        assert!(!allowed(0, "bound-invocation", None, "unrelated/v1"));
    }

    #[tokio::test]
    async fn typed_command_admission_preserves_declared_route_and_caller_lifetime() {
        let temp = tempfile::tempdir().unwrap();
        let state = Arc::new(command_state());
        assert!(declared_route(
            &state,
            "test.command-context",
            bcode_command::COMMAND_INTERFACE_ID,
            "invoke"
        ));
        assert!(!declared_route(
            &state,
            "test.command-context",
            bcode_command::COMMAND_INTERFACE_ID,
            "list"
        ));
        let root = temp.path().canonicalize().unwrap();
        let client = ClientId::new();
        state
            .set_client_runtime_context(
                client,
                Some(bcode_ipc::ClientRuntimeContext {
                    working_directory: Some(root.clone()),
                    ..Default::default()
                }),
            )
            .await;
        assert_command_context(&state, client, &root).await;
        assert!(
            invoke(
                &state,
                client,
                command_context(&root, "command-1"),
                "test.command-context".into(),
                bcode_command::COMMAND_INTERFACE_ID.into(),
                bcode_command::OP_INVOKE_COMMAND.into(),
                command_payload("literal")
            )
            .await
            .is_err(),
            "settled identity must not dispatch again"
        );
        for (id, interface, operation, command) in [
            (
                "undeclared",
                bcode_command::COMMAND_INTERFACE_ID,
                "invoke",
                "not-declared",
            ),
            (
                "wrong-operation",
                bcode_command::COMMAND_INTERFACE_ID,
                "arbitrary",
                "literal",
            ),
            ("wrong-interface", "test.arbitrary/v1", "invoke", "literal"),
        ] {
            assert!(
                invoke(
                    &state,
                    client,
                    command_context(&root, id),
                    "test.command-context".into(),
                    interface.into(),
                    operation.into(),
                    command_payload(command)
                )
                .await
                .is_err()
            );
        }
        {
            let mut registry = state.contextual_plugin_invocations.lock().unwrap();
            assert!(registry.reserve(client, "cancelled-command", Duration::from_secs(1)));
            assert!(!registry.cancel(ClientId::new(), "cancelled-command"));
            assert!(registry.cancel(client, "cancelled-command"));
            drop(registry);
        }

        assert!(matches!(
            invoke(
                &state,
                client,
                command_context(&root, "cancelled-command"),
                "test.command-context".into(),
                bcode_command::COMMAND_INTERFACE_ID.into(),
                bcode_command::OP_INVOKE_COMMAND.into(),
                command_payload("literal")
            )
            .await,
            Err(PublicPluginError {
                code: "plugin_cancelled",
                ..
            })
        ));
        assert!(!cancel(&state, client, "cancelled-command"));
        drop(state);
    }

    #[tokio::test]
    async fn command_application_executes_same_owner_directly_and_over_real_ipc() {
        use bcode_command::application::{
            CommandApplication as _, CommandInvocationOutcome, CommandInvocationRequest,
        };
        let temp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let plugins = command_plugins();
        let state = std::sync::Arc::new(super::super::tests::test_server_state_with_plugins(
            bcode_session::SessionManager::default(),
            bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
                &bcode_plugin::PluginSelection {
                    mode: bcode_plugin::PluginSelectionMode::Explicit,
                    enabled: std::collections::BTreeSet::from([
                        "test.command-context".into(),
                        "bcode.default-agents".into(),
                    ]),
                    disabled: std::collections::BTreeSet::new(),
                },
                &plugins,
            )
            .unwrap(),
        ));
        let root = temp.path().canonicalize().unwrap();
        let direct_client = ClientId::new();
        let runtime = bcode_ipc::ClientRuntimeContext {
            working_directory: Some(root.clone()),
            ..Default::default()
        };
        state
            .set_client_runtime_context(direct_client, Some(runtime.clone()))
            .await;
        let request = CommandInvocationRequest {
            plugin_id: "test.command-context".to_owned(),
            invocation_id: "execute-parity".to_owned(),
            agent_id: None,
            launch_root: None,
            deadline_ms: 1000,
            command: bcode_command::InvokeCommandRequest {
                command_id: "literal".to_owned(),
                args: BTreeMap::new(),
                context: None,
            },
        };
        let direct = crate::CommandApplicationHandle::new(&state, direct_client)
            .invoke_command(request.clone())
            .await;
        assert!(
            matches!(&direct.outcome, CommandInvocationOutcome::Completed(response) if response.success && response.effects.len() == 1)
        );
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(temp.path().join("command-parity.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            tokio::pin!(stopped);
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    stream = listener.accept() => {
                        let state = server_state.clone();
                        let stream = stream.unwrap();
                        tasks.spawn(async move { super::super::handle_client(stream, state).await.unwrap(); });
                    }
                }
            }
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        });
        let client = bcode_client::BcodeClient::new(endpoint).with_runtime_context(Some(runtime));
        let ipc = tokio::time::timeout(Duration::from_secs(5), client.invoke_command(request))
            .await
            .unwrap();
        assert_eq!(
            direct, ipc,
            "same declared command owner must execute with identical typed effects and identity"
        );
        let ack = client
            .cancel_command("execute-parity".to_owned())
            .await
            .unwrap();
        assert_eq!(ack.invocation_id, "execute-parity");
        assert!(
            !ack.accepted,
            "terminal execution is not an active cancellation"
        );
        stop.send(()).unwrap();
        server.await.unwrap();
        drop(state);
    }

    #[tokio::test]
    async fn bundled_copy_session_id_executes_over_real_ipc() {
        use bcode_command::application::{
            CommandApplication as _, CommandInvocationOutcome, CommandInvocationRequest,
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let bundles = [
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../plugins/session-derivation-plugin/bcode-plugin.toml"),
                bcode_session_derivation_plugin::static_plugin(),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../plugins/default-agents-plugin/bcode-plugin.toml"),
                bcode_default_agents_plugin::static_plugin(),
            ),
        ];
        let state = std::sync::Arc::new(super::super::tests::test_server_state_with_plugins(
            bcode_session::SessionManager::persistent(root.join("sessions")).unwrap(),
            bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
                &bcode_plugin::PluginSelection::all_enabled(),
                &bundles,
            )
            .unwrap(),
        ));
        let session = state
            .sessions
            .create_session(None, root.clone())
            .await
            .unwrap();
        let (endpoint, stop, server) = serve_fixture(std::sync::Arc::clone(&state), temp.path());
        let client = bcode_client::BcodeClient::new(endpoint).with_runtime_context(Some(
            bcode_ipc::ClientRuntimeContext {
                working_directory: Some(root.clone()),
                ..Default::default()
            },
        ));
        let result = client
            .invoke_command(CommandInvocationRequest {
                plugin_id: "bcode.session-derivation".into(),
                invocation_id: "copy-session-id".into(),
                agent_id: None,
                launch_root: Some(root),
                deadline_ms: 5000,
                command: bcode_command::InvokeCommandRequest {
                    command_id: "session-derivation.copy-session-id".into(),
                    args: BTreeMap::new(),
                    context: Some(bcode_command::CommandInvocationContext {
                        session_id: Some(session.id),
                        working_directory: temp.path().to_path_buf(),
                    }),
                },
            })
            .await;
        let CommandInvocationOutcome::Completed(response) = result.outcome else {
            panic!("copy command failed: {result:?}");
        };
        assert!(response.success);
        assert_eq!(
            response.effects,
            vec![
                bcode_command::CommandEffect::CopyText {
                    text: session.id.to_string()
                },
                bcode_command::CommandEffect::AppendText {
                    text: format!("Session ID: {}", session.id),
                    format: bcode_command::CommandTextFormat::PlainText,
                },
            ]
        );
        assert_bundled_derivation_commands(&client, &state, session.id, temp.path()).await;
        stop.send(()).unwrap();
        server.await.unwrap();
        drop(state);
    }

    async fn assert_bundled_derivation_commands(
        client: &bcode_client::BcodeClient,
        state: &ServerState,
        session_id: SessionId,
        root: &Path,
    ) {
        use bcode_command::application::{
            CommandApplication as _, CommandInvocationOutcome, CommandInvocationRequest,
        };
        let prompt = state
            .sessions
            .append_user_message(session_id, ClientId::new(), "authoritative prompt".into())
            .await
            .unwrap();
        let prompt_sequence = prompt
            .iter()
            .find_map(|event| {
                matches!(
                    event.kind,
                    bcode_session_models::SessionEventKind::UserMessage { .. }
                )
                .then_some(event.sequence)
            })
            .expect("canonical user message");
        for (suffix, arguments) in [
            ("fork", None),
            ("clone", Some("name=clone-fixture".to_owned())),
            (
                "fork",
                Some(format!("sequence={prompt_sequence} name=fork-fixture")),
            ),
        ] {
            let result = client
                .invoke_command(CommandInvocationRequest {
                    plugin_id: "bcode.session-derivation".into(),
                    invocation_id: uuid::Uuid::new_v4().to_string(),
                    agent_id: None,
                    launch_root: Some(root.to_path_buf()),
                    deadline_ms: 30_000,
                    command: bcode_command::InvokeCommandRequest {
                        command_id: format!("session-derivation.{suffix}"),
                        args: arguments
                            .clone()
                            .map(|args| BTreeMap::from([("arguments".into(), args)]))
                            .unwrap_or_default(),
                        context: Some(bcode_command::CommandInvocationContext {
                            session_id: Some(session_id),
                            working_directory: root.to_path_buf(),
                        }),
                    },
                })
                .await;
            let CommandInvocationOutcome::Completed(response) = result.outcome else {
                panic!("{suffix} failed: {result:?}");
            };
            assert!(response.success, "{response:?}");
            if arguments.is_none() {
                assert!(
                    !response.effects.is_empty(),
                    "prompt selection must be presented"
                );
                continue;
            }
            let destination = response
                .effects
                .iter()
                .find_map(|effect| match effect {
                    bcode_command::CommandEffect::OpenSession { session_id, .. } => {
                        Some(*session_id)
                    }
                    _ => None,
                })
                .expect("derived session navigation");
            assert_ne!(destination, session_id);
            assert_derived_prompt(state, destination, suffix == "fork").await;
        }
    }

    async fn assert_derived_prompt(state: &ServerState, destination: SessionId, fork: bool) {
        let snapshot = state
            .sessions
            .session_derivation_snapshot(destination)
            .await
            .unwrap();
        let prompts = state
            .sessions
            .session_derivation_prompt_candidates(
                destination,
                bcode_session_models::SessionDerivationPromptQuery {
                    generation: snapshot.generation,
                    before_sequence: None,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        if fork {
            assert_eq!(
                state
                    .sessions
                    .session_composer_draft(destination)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("authoritative prompt")
            );
            assert!(prompts.candidates.is_empty());
        } else {
            assert!(
                prompts
                    .candidates
                    .iter()
                    .any(|candidate| candidate.preview == "authoritative prompt")
            );
        }
    }

    const DISPATCH_MANIFEST: &str = r#"
id = "test.composer-dispatch"
name = "Generic composer dispatch fixture"
version = "0.0.1"
[[services]]
interface_id = "bcode.composer/v1"
[[composer_contributions]]
id = "first"
activation_kind = "fixture"
service_interface_id = "bcode.composer/v1"
[[composer_contributions]]
id = "second"
activation_kind = "fixture"
service_interface_id = "bcode.composer/v1"
[runtime]
type = "native"
abi_version = 5
library = "libtest_composer_dispatch.dylib"
"#;

    #[derive(Default)]
    struct DispatchPlugin;

    impl bcode_plugin_sdk::RustPlugin for DispatchPlugin {
        fn invoke_service(
            &mut self,
            context: bcode_plugin_sdk::NativeServiceContext,
        ) -> bcode_plugin_sdk::ServiceResponse {
            bcode_plugin_sdk::ServiceResponse::ok(context.request.payload)
        }
    }

    #[tokio::test]
    async fn outer_dispatch_selects_exact_contribution_and_rejects_ambiguous_legacy() {
        let temp = tempfile::tempdir().unwrap();
        let plugins = [
            bcode_plugin::StaticBundledPlugin::new(
                DISPATCH_MANIFEST,
                bcode_plugin_sdk::static_plugin_vtable!(DispatchPlugin, DISPATCH_MANIFEST),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../plugins/default-agents-plugin/bcode-plugin.toml"),
                bcode_default_agents_plugin::static_plugin(),
            ),
        ];
        let state = Arc::new(super::super::tests::test_server_state_with_plugins(
            bcode_session::SessionManager::default(),
            bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
                &bcode_plugin::PluginSelection {
                    mode: bcode_plugin::PluginSelectionMode::Explicit,
                    enabled: std::collections::BTreeSet::from([
                        "test.composer-dispatch".into(),
                        "bcode.default-agents".into(),
                    ]),
                    disabled: std::collections::BTreeSet::new(),
                },
                &plugins,
            )
            .unwrap(),
        ));
        let root = temp.path().canonicalize().unwrap();
        let client = ClientId::new();
        state
            .set_client_runtime_context(
                client,
                Some(bcode_ipc::ClientRuntimeContext {
                    working_directory: Some(root.clone()),
                    ..Default::default()
                }),
            )
            .await;
        for (index, selected) in ["first", "second", "unknown", "", "first\n"]
            .iter()
            .enumerate()
        {
            let mut request: serde_json::Value =
                serde_json::from_slice(&wait_search_payload()).unwrap();
            request["contribution_id"] = serde_json::json!(selected);
            let result = invoke(
                &state,
                client,
                bcode_ipc::PluginServiceContext {
                    agent_id: Some("build".into()),
                    invocation_id: format!("dispatch-{index}"),
                    workspace: bcode_plugin_sdk::WorkspaceContext {
                        session_id: None,
                        launch_root: Some(root.clone()),
                    },
                    deadline_ms: 1000,
                },
                "test.composer-dispatch".into(),
                "bcode.composer/v1".into(),
                "search".into(),
                serde_json::to_vec(&request).unwrap(),
            )
            .await;
            if ["first", "second"].contains(selected) {
                let response = result.unwrap();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&response.payload).unwrap()["contribution_id"],
                    serde_json::json!(selected)
                );
            } else {
                assert!(matches!(
                    result,
                    Err(PublicPluginError {
                        code: "plugin_context_denied",
                        ..
                    })
                ));
            }
        }
        drop(state);
    }

    const WAIT_LEAF_MANIFEST: &str = r#"
id = "test.wait-leaf"
name = "Nested wait fixture"
version = "0.0.1"
[[services]]
interface_id = "test.wait/v1"
invocation_operations = ["wait"]
[runtime]
type = "native"
abi_version = 5
library = "libtest_wait_leaf.dylib"
"#;

    const SECOND_WAIT_MANIFEST: &str = r#"
id = "test.wait-second"
name = "Ambiguous nested fixture"
version = "0.0.1"
[[services]]
interface_id = "test.wait/v1"
invocation_operations = ["wait"]
[runtime]
type = "native"
abi_version = 5
library = "libtest_wait_second.dylib"
"#;

    #[derive(Default)]
    struct WaitPlugin;
    impl bcode_plugin_sdk::RustPlugin for WaitPlugin {
        fn invoke_service(
            &mut self,
            context: bcode_plugin_sdk::NativeServiceContext,
        ) -> bcode_plugin_sdk::ServiceResponse {
            if context.request.interface_id == "bcode.composer/v1" {
                let request = bcode_plugin_sdk::ServiceBridgeRequest::InvokeService(
                    bcode_tool::ToolInvocationServiceRequest {
                        invocation_id: String::new(),
                        request_id: "nested-wait".into(),
                        route_id: None,
                        interface_id: "test.wait/v1".into(),
                        operation: "wait".into(),
                        payload: serde_json::json!({}),
                    },
                );
                return match context.bridge.request(&request) {
                    Ok(response) => bcode_plugin_sdk::ServiceResponse::json(&response)
                        .unwrap_or_else(|error| {
                            bcode_plugin_sdk::ServiceResponse::error("encode", error.to_string())
                        }),
                    Err(bcode_plugin_sdk::ServiceBridgeError::Cancelled) => {
                        bcode_plugin_sdk::ServiceResponse::error("cancelled", "cancelled")
                    }
                    Err(error) => {
                        bcode_plugin_sdk::ServiceResponse::error("bridge", error.to_string())
                    }
                };
            }
            while !context.cancellation.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            bcode_plugin_sdk::ServiceResponse::error("cancelled", "cancelled")
        }
    }

    #[tokio::test]
    async fn cancelled_nested_bridge_does_not_poison_static_wait_fixture() {
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        cancellation.cancel();
        let context = bcode_plugin_sdk::NativeServiceContext {
            diagnostics: None,
            plugin_id: "test.composer-wait".into(),
            config: bcode_plugin_sdk::PluginConfigContext::default(),
            events: bcode_plugin_sdk::ServiceEventEmitter::default(),
            transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
            request: bcode_plugin_sdk::ServiceRequest {
                interface_id: "bcode.composer/v1".into(),
                operation: "search".into(),
                payload: Vec::new(),
            },
            bridge: bcode_plugin_sdk::ServiceBridge::new(
                None,
                std::ptr::null_mut(),
                cancellation.clone(),
            ),
            cancellation,
        };
        let fixture = std::sync::Mutex::new(WaitPlugin);
        let response =
            bcode_plugin_sdk::RustPlugin::invoke_service(&mut *fixture.lock().unwrap(), context);
        assert_eq!(response.error.unwrap().code, "cancelled");
        assert!(
            fixture.lock().is_ok(),
            "bridge cancellation must not poison plugin lifecycle state"
        );
        let temp = tempfile::tempdir().unwrap();
        let (state, _, stop, server) = start_wait_service(temp.path());
        stop.send(()).unwrap();
        server.await.unwrap();
        drop(state);
    }

    fn start_wait_service(
        root: &Path,
    ) -> (
        std::sync::Arc<ServerState>,
        bcode_ipc::IpcEndpoint,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let plugin = bcode_plugin::StaticBundledPlugin::new(
            WAIT_MANIFEST,
            bcode_plugin_sdk::static_plugin_vtable!(WaitPlugin, WAIT_MANIFEST),
        );
        let runtime = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection {
                mode: bcode_plugin::PluginSelectionMode::Explicit,
                enabled: std::collections::BTreeSet::from([
                    "test.composer-wait".into(),
                    "test.wait-leaf".into(),
                    "bcode.default-agents".into(),
                ]),
                disabled: std::collections::BTreeSet::new(),
            },
            &[
                plugin,
                bcode_plugin::StaticBundledPlugin::new(
                    WAIT_LEAF_MANIFEST,
                    bcode_plugin_sdk::static_plugin_vtable!(WaitPlugin, WAIT_LEAF_MANIFEST),
                ),
                bcode_plugin::StaticBundledPlugin::new(
                    include_str!("../../../plugins/default-agents-plugin/bcode-plugin.toml"),
                    bcode_default_agents_plugin::static_plugin(),
                ),
            ],
        )
        .unwrap();
        let state = std::sync::Arc::new(super::super::tests::test_server_state_with_plugins(
            bcode_session::SessionManager::default(),
            runtime,
        ));
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(root.join("context.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server_state = std::sync::Arc::clone(&state);
        let server = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            tokio::pin!(stopped);
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    stream = listener.accept() => {
                        let state = std::sync::Arc::clone(&server_state);
                        let stream = stream.unwrap();
                        tasks.spawn(async move { super::super::handle_client(stream, state).await.unwrap(); });
                    }
                }
            }
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        });
        (state, endpoint, stop, server)
    }

    #[tokio::test]
    async fn contextual_client_cancellation_reaches_real_ipc_invocation() {
        let temp = tempfile::tempdir().unwrap();
        let (state, endpoint, stop, server) = start_wait_service(temp.path());
        let root = temp.path().canonicalize().unwrap();
        let client = bcode_client::BcodeClient::new(endpoint).with_runtime_context(Some(
            bcode_ipc::ClientRuntimeContext {
                working_directory: Some(root.clone()),
                ..Default::default()
            },
        ));
        let active_client = client.clone();
        let invocation = tokio::spawn(async move {
            active_client
                .invoke_plugin_service_contextual(
                    bcode_ipc::PluginServiceContext {
                        agent_id: None,
                        invocation_id: "wait".into(),
                        workspace: bcode_plugin_sdk::WorkspaceContext {
                            session_id: None,
                            launch_root: Some(root),
                        },
                        deadline_ms: 5000,
                    },
                    "test.composer-wait".into(),
                    "bcode.composer/v1".into(),
                    "search".into(),
                    wait_search_payload(),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let active = state
                    .contextual_plugin_invocations
                    .lock()
                    .unwrap()
                    .clients
                    .values()
                    .any(|ids| ids.get("wait").is_some_and(Option::is_some));
                if active {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !client
                .cancel_plugin_service_invocation("forged".into())
                .await
                .unwrap()
        );
        assert!(
            client
                .cancel_plugin_service_invocation("wait".into())
                .await
                .unwrap()
        );
        let result = tokio::time::timeout(Duration::from_secs(2), invocation)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(result, Err(bcode_client::ClientError::Server { code, .. }) if code == "plugin_cancelled")
        );
        assert!(
            !client
                .cancel_plugin_service_invocation("wait".into())
                .await
                .unwrap()
        );
        stop.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn early_client_cancellation_survives_connect_and_admission() {
        let temp = tempfile::tempdir().unwrap();
        let (state, endpoint, stop, server) = start_wait_service(temp.path());
        let root = temp.path().canonicalize().unwrap();
        let client = bcode_client::BcodeClient::new(endpoint).with_runtime_context(Some(
            bcode_ipc::ClientRuntimeContext {
                working_directory: Some(root.clone()),
                ..bcode_ipc::ClientRuntimeContext::default()
            },
        ));
        let owner = client.clone();
        let invocation = tokio::spawn(async move {
            owner
                .invoke_plugin_service_contextual(
                    bcode_ipc::PluginServiceContext {
                        agent_id: Some("build".into()),
                        invocation_id: "early".into(),
                        workspace: bcode_plugin_sdk::WorkspaceContext {
                            session_id: None,
                            launch_root: Some(root),
                        },
                        deadline_ms: 5000,
                    },
                    "test.composer-wait".into(),
                    "bcode.composer/v1".into(),
                    "search".into(),
                    wait_search_payload(),
                )
                .await
        });
        // Cancellation is queued at the client during connection/handshake, before
        // the server necessarily admits the contextual request.
        let accepted = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if client
                    .cancel_plugin_service_invocation("early".into())
                    .await
                    .unwrap()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        let terminal = tokio::time::timeout(Duration::from_secs(2), invocation).await;
        stop.send(()).unwrap();
        server.await.unwrap();
        drop(state);
        accepted.unwrap();
        let terminal_result = terminal
            .expect("early cancellation invocation must settle")
            .unwrap();
        assert!(
            matches!(&terminal_result, Err(bcode_client::ClientError::Server { code, .. }) if code == "plugin_cancelled"),
            "early cancellation returned {terminal_result:?}"
        );
    }

    #[tokio::test]
    async fn nested_invocation_inherits_outer_deadline() {
        let temp = tempfile::tempdir().unwrap();
        let (state, endpoint, stop, server) = start_wait_service(temp.path());
        let root = temp.path().canonicalize().unwrap();
        let client = bcode_client::BcodeClient::new(endpoint).with_runtime_context(Some(
            bcode_ipc::ClientRuntimeContext {
                working_directory: Some(root.clone()),
                ..bcode_ipc::ClientRuntimeContext::default()
            },
        ));
        let result = client
            .invoke_plugin_service_contextual(
                bcode_ipc::PluginServiceContext {
                    agent_id: Some("build".into()),
                    invocation_id: "deadline".into(),
                    workspace: bcode_plugin_sdk::WorkspaceContext {
                        session_id: None,
                        launch_root: Some(root),
                    },
                    deadline_ms: 200,
                },
                "test.composer-wait".into(),
                "bcode.composer/v1".into(),
                "search".into(),
                wait_search_payload(),
            )
            .await;
        stop.send(()).unwrap();
        server.await.unwrap();
        drop(state);
        assert!(
            matches!(&result, Err(bcode_client::ClientError::Server { code, .. }) if code == "plugin_deadline"),
            "nested deadline returned {result:?}"
        );
    }

    #[tokio::test]
    async fn ambiguous_nested_provider_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let plugins = [
            bcode_plugin::StaticBundledPlugin::new(
                WAIT_LEAF_MANIFEST,
                bcode_plugin_sdk::static_plugin_vtable!(WaitPlugin, WAIT_LEAF_MANIFEST),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                SECOND_WAIT_MANIFEST,
                bcode_plugin_sdk::static_plugin_vtable!(WaitPlugin, SECOND_WAIT_MANIFEST),
            ),
        ];
        let state = super::super::tests::test_server_state_with_plugins(
            bcode_session::SessionManager::default(),
            bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
                &bcode_plugin::PluginSelection {
                    mode: bcode_plugin::PluginSelectionMode::Explicit,
                    enabled: std::collections::BTreeSet::from([
                        "test.wait-leaf".into(),
                        "test.wait-second".into(),
                    ]),
                    disabled: std::collections::BTreeSet::new(),
                },
                &plugins,
            )
            .unwrap(),
        );
        let bound = bound_fixture(temp.path().canonicalize().unwrap());
        let request = bcode_tool::ToolInvocationServiceRequest {
            invocation_id: String::new(),
            request_id: "ambiguous".into(),
            route_id: None,
            interface_id: "test.wait/v1".into(),
            operation: "wait".into(),
            payload: serde_json::json!({}),
        };
        let result = route_nested(&state, &bound, request, 1).await;
        drop(state);
        assert_eq!(result, ToolInvocationServiceResolution::Unsupported);
    }

    fn bound_fixture(root: PathBuf) -> BoundContext {
        BoundContext {
            id: "outer".into(),
            root,
            session: None,
            config: bcode_config::BcodeConfig::default(),
            agent: "build".into(),
            scope: bcode_plugin::PluginInvocationScope::Global,
        }
    }

    fn bridge_state(root: &Path) -> ServerState {
        let plugins = [
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../plugins/filesystem-plugin/bcode-plugin.toml"),
                bcode_filesystem_plugin::static_plugin(),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../plugins/default-agents-plugin/bcode-plugin.toml"),
                bcode_default_agents_plugin::static_plugin(),
            ),
        ];
        let runtime = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection {
                mode: bcode_plugin::PluginSelectionMode::Explicit,
                enabled: std::collections::BTreeSet::from([
                    "bcode.filesystem".into(),
                    "bcode.default-agents".into(),
                ]),
                disabled: std::collections::BTreeSet::new(),
            },
            &plugins,
        )
        .unwrap();
        let sessions = bcode_session::SessionManager::persistent(root).unwrap();
        super::super::tests::test_server_state_with_plugins(sessions, runtime)
    }

    // The real filesystem plugin evaluates Git's ambient ignore sources. Run these
    // integration fixtures in a separate process with ordinary, isolated sources;
    // changing process-global environment inside parallel async tests is unsafe.
    fn isolated_git_fixture(test: &str) -> bool {
        const MARKER: &str = "BCODE_SERVER_ISOLATED_GIT_FIXTURE";
        if std::env::var(MARKER).as_deref() == Ok(test) {
            return false;
        }
        let home = tempfile::tempdir().unwrap();
        // macOS temporary roots can contain /var or /tmp symlink aliases. The
        // filesystem plugin intentionally requires no-follow metadata paths.
        let home_root = home.path().canonicalize().unwrap();
        std::fs::create_dir_all(home_root.join(".config/git")).unwrap();
        let config = home_root.join("gitconfig");
        std::fs::write(&config, "").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("GIT_CONFIG") {
                child.env_remove(name);
            }
        }
        let output = child
            .args(["--exact", test, "--nocapture"])
            .env(MARKER, test)
            .env("HOME", &home_root)
            .env("XDG_CONFIG_HOME", home_root.join(".config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", config)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    fn stage_fixture_paths(root: &Path, paths: &[&str]) {
        for args in [
            vec!["init", "--quiet"],
            [vec!["add", "--"], paths.to_vec()].concat(),
        ] {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    fn real_composer_state(root: &Path, enabled: bool) -> ServerState {
        let plugins = [
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../plugins/file-references-plugin/bcode-plugin.toml"),
                bcode_file_references_plugin::static_plugin(),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../plugins/filesystem-plugin/bcode-plugin.toml"),
                bcode_filesystem_plugin::static_plugin(),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../plugins/default-agents-plugin/bcode-plugin.toml"),
                bcode_default_agents_plugin::static_plugin(),
            ),
        ];
        let mut selected = std::collections::BTreeSet::from([
            "bcode.filesystem".into(),
            "bcode.default-agents".into(),
        ]);
        if enabled {
            selected.insert("bcode.file-references".into());
        }
        let runtime =
            bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled_and_config(
                &bcode_plugin::PluginSelection {
                    mode: bcode_plugin::PluginSelectionMode::Explicit,
                    enabled: selected,
                    disabled: std::collections::BTreeSet::new(),
                },
                &plugins,
                BTreeMap::from([(
                    "bcode.file-references".into(),
                    bcode_plugin::ResolvedPluginConfig::new(
                        serde_json::json!({"timeout_ms": 1000}),
                        serde_json::json!({"timeout_ms": 1000}),
                    ),
                )]),
            )
            .unwrap();
        super::super::tests::test_server_state_with_plugins(
            bcode_session::SessionManager::persistent(root).unwrap(),
            runtime,
        )
    }

    async fn search_real_composer(
        state: &Arc<ServerState>,
        root: &Path,
    ) -> Result<PluginServiceOperationResult, PublicPluginError> {
        use bcode_plugin_sdk::composer::{
            ComposerActivation, ComposerDocument, ComposerSearchRequest, ComposerTextRange,
        };
        let client = ClientId::new();
        state
            .set_client_runtime_context(
                client,
                Some(bcode_ipc::ClientRuntimeContext {
                    working_directory: Some(root.to_path_buf()),
                    ..bcode_ipc::ClientRuntimeContext::default()
                }),
            )
            .await;
        let document = ComposerDocument {
            document_id: "composer".into(),
            revision: 4,
            text: "Inspect @".into(),
            cursor: 9,
            selection: None,
        };
        let active =
            bcode_file_references_plugin::grammar::activate(&document.text, document.cursor)
                .unwrap();
        let request = ComposerSearchRequest {
            contribution_id: "paths".into(),
            request_id: "real-search".into(),
            document,
            activation: ComposerActivation {
                range: ComposerTextRange {
                    start: active.range.start,
                    end: active.range.end,
                },
                query: active.query,
            },
            max_results: 64,
            interaction_state: std::collections::BTreeMap::new(),
            action_id: None,
        };
        invoke(
            state,
            client,
            bcode_ipc::PluginServiceContext {
                agent_id: None,
                invocation_id: request.request_id.clone(),
                workspace: bcode_plugin_sdk::WorkspaceContext {
                    session_id: None,
                    launch_root: Some(root.to_path_buf()),
                },
                deadline_ms: 5000,
            },
            "bcode.file-references".into(),
            "bcode.composer/v1".into(),
            "search".into(),
            serde_json::to_vec(&request).unwrap(),
        )
        .await
    }

    fn serve_fixture(
        state: std::sync::Arc<ServerState>,
        root: &Path,
    ) -> (
        bcode_ipc::IpcEndpoint,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(root.join("fixture.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            tokio::pin!(stopped);
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    stream = listener.accept() => {
                        let state = std::sync::Arc::clone(&state);
                        let stream = stream.unwrap();
                        clients.spawn(async move { super::super::handle_client(stream, state).await.unwrap(); });
                    }
                }
            }
            clients.abort_all();
            while clients.join_next().await.is_some() {}
        });
        (endpoint, stop, task)
    }

    #[tokio::test]
    async fn real_session_owned_composer_search_over_ipc() {
        use bcode_plugin_sdk::composer::{
            ComposerActivation, ComposerDocument, ComposerSearchRequest, ComposerSnapshot,
            ComposerTextRange,
        };
        if isolated_git_fixture(
            "plugin_context::tests::real_session_owned_composer_search_over_ipc",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("visible.rs"), "NO_CONTENT_IN_RESPONSE").unwrap();
        stage_fixture_paths(&root, &["visible.rs"]);
        let state = std::sync::Arc::new(real_composer_state(&temp.path().join("sessions"), true));
        let session = state
            .sessions
            .create_session(None, root.clone())
            .await
            .unwrap();
        let (endpoint, stop, server) = serve_fixture(std::sync::Arc::clone(&state), temp.path());
        let client = bcode_client::BcodeClient::new(endpoint).with_runtime_context(Some(
            bcode_ipc::ClientRuntimeContext {
                working_directory: Some(root.clone()),
                ..bcode_ipc::ClientRuntimeContext::default()
            },
        ));
        let request = ComposerSearchRequest {
            contribution_id: "paths".into(),
            request_id: "session-search".into(),
            document: ComposerDocument {
                document_id: "input".into(),
                revision: 1,
                text: "@visible".into(),
                cursor: 8,
                selection: None,
            },
            activation: ComposerActivation {
                range: ComposerTextRange { start: 0, end: 8 },
                query: "visible".into(),
            },
            max_results: 8,
            interaction_state: std::collections::BTreeMap::new(),
            action_id: None,
        };
        let response = client
            .invoke_plugin_service_contextual(
                bcode_ipc::PluginServiceContext {
                    agent_id: Some("build".into()),
                    invocation_id: request.request_id.clone(),
                    workspace: bcode_plugin_sdk::WorkspaceContext {
                        session_id: Some(session.id.to_string()),
                        launch_root: Some(root),
                    },
                    deadline_ms: 5000,
                },
                "bcode.file-references".into(),
                "bcode.composer/v1".into(),
                "search".into(),
                serde_json::to_vec(&request).unwrap(),
            )
            .await
            .unwrap();
        stop.send(()).unwrap();
        server.await.unwrap();
        drop(state);
        assert!(response.error.is_none());
        let snapshot: ComposerSnapshot = serde_json::from_slice(&response.payload).unwrap();
        snapshot.validate_for(&request).unwrap();
        let candidate = snapshot
            .candidates
            .iter()
            .find(|candidate| candidate.label == "visible.rs")
            .unwrap();
        assert_eq!(
            candidate.edit.apply(&request.document).unwrap().text,
            "@visible.rs"
        );
        assert!(
            !String::from_utf8(response.payload)
                .unwrap()
                .contains("NO_CONTENT_IN_RESPONSE")
        );
    }

    #[tokio::test]
    async fn launch_selected_agent_is_verified_and_policy_is_not_defaulted() {
        let temp = tempfile::tempdir().unwrap();
        let state = bridge_state(&temp.path().join("sessions"));
        let config = bcode_config::BcodeConfig::default();
        assert_eq!(
            validated_agent(&state, &config, Some("plan"))
                .await
                .unwrap(),
            "plan"
        );
        assert!(
            validated_agent(&state, &config, Some("unknown-agent"))
                .await
                .is_err()
        );
        drop(state);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_composer_invalid_byte_names_are_partial_without_lossy_edits() {
        use bcode_plugin_sdk::composer::{ComposerSnapshot, ComposerSnapshotState};
        use std::os::unix::ffi::OsStringExt;
        if isolated_git_fixture(
            "plugin_context::tests::real_composer_invalid_byte_names_are_partial_without_lossy_edits",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("visible.rs"), "content").unwrap();
        let invalid_path = root.join(std::ffi::OsString::from_vec(vec![b'x', 255]));
        if let Err(error) = std::fs::write(&invalid_path, "content") {
            // APFS can reject this fixture before traversal. Do not mistake
            // unrelated I/O failures for unsupported filename encoding.
            #[cfg(target_os = "macos")]
            {
                assert_eq!(error.raw_os_error(), Some(libc::EILSEQ));
                assert!(!invalid_path.exists());
                eprintln!(
                    "non-UTF-8 traversal not exercised: filesystem rejected fixture with EILSEQ"
                );
                return;
            }
            #[cfg(not(target_os = "macos"))]
            panic!("non-UTF-8 fixture creation failed: {error}");
        }
        let state = Arc::new(real_composer_state(&temp.path().join("sessions"), true));
        for _ in 0..2 {
            let response = search_real_composer(&state, &root).await.unwrap();
            let snapshot: ComposerSnapshot = serde_json::from_slice(&response.payload).unwrap();
            assert_eq!(snapshot.state, ComposerSnapshotState::Truncated);
            assert!(
                snapshot
                    .candidates
                    .iter()
                    .any(|candidate| candidate.label == "visible.rs")
            );
            assert!(
                snapshot
                    .candidates
                    .iter()
                    .all(|candidate| !candidate.edit.replacement.contains('\u{fffd}'))
            );
        }
        drop(state);
    }

    async fn real_search_diagnostics(
        state: &ServerState,
        root: &Path,
    ) -> Vec<(u64, ToolInvocationServiceResolution)> {
        let bound = bound_fixture(root.to_path_buf());
        let mut results = Vec::new();
        for timeout_ms in [100, 1000] {
            let request = bcode_tool::ToolInvocationServiceRequest {
                invocation_id: String::new(),
                request_id: format!("diagnostic-{timeout_ms}"),
                route_id: None,
                interface_id: "bcode.filesystem/v1".into(),
                operation: "search_paths".into(),
                payload: serde_json::json!({"query":"", "directory":"", "recursive":true, "timeout_ms":timeout_ms}),
            };
            results.push((timeout_ms, route_nested(state, &bound, request, 1).await));
        }
        results
    }

    #[tokio::test]
    async fn real_composer_filesystem_resource_ignore_chain_and_disabled_route() {
        use bcode_plugin_sdk::composer::{
            ComposerContinuation, ComposerDocument, ComposerSnapshot, ComposerSnapshotState,
        };
        if isolated_git_fixture(
            "plugin_context::tests::real_composer_filesystem_resource_ignore_chain_and_disabled_route",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(".gitignore"), "excluded.rs\n").unwrap();
        std::fs::write(root.join("excluded.rs"), "EXCLUDED_CONTENT_MARKER").unwrap();
        std::fs::write(root.join("文件 space.rs"), "VISIBLE_CONTENT_MARKER").unwrap();
        stage_fixture_paths(&root, &[".gitignore", "文件 space.rs"]);
        let root = root.canonicalize().unwrap();
        let state = Arc::new(real_composer_state(&temp.path().join("sessions"), true));
        let response = search_real_composer(&state, &root).await.unwrap();
        assert!(response.error.is_none(), "{:?}", response.error);
        let snapshot: ComposerSnapshot = serde_json::from_slice(&response.payload).unwrap();
        if snapshot.state != ComposerSnapshotState::Ready {
            let diagnostics = real_search_diagnostics(&state, &root).await;
            panic!(
                "real composer incomplete: {:?}; actual filesystem diagnostics: {diagnostics:?}",
                snapshot.state
            );
        }
        drop(state);
        assert_eq!(snapshot.state, ComposerSnapshotState::Ready);
        assert!(
            !snapshot
                .candidates
                .iter()
                .any(|candidate| candidate.label.contains("excluded.rs"))
        );
        let candidate = snapshot
            .candidates
            .iter()
            .find(|candidate| candidate.label.contains("文件 space.rs"))
            .unwrap();
        assert_eq!(candidate.continuation, ComposerContinuation::Finish);
        let document = ComposerDocument {
            document_id: "composer".into(),
            revision: 4,
            text: "Inspect @".into(),
            cursor: 9,
            selection: None,
        };
        let applied = candidate.edit.apply(&document).unwrap();
        assert_eq!(applied.text, "Inspect @\"文件 space.rs\"");
        let wire = String::from_utf8(response.payload).unwrap();
        assert!(!wire.contains("VISIBLE_CONTENT_MARKER"));
        assert!(!wire.contains("EXCLUDED_CONTENT_MARKER"));
        let disabled = Arc::new(real_composer_state(
            &temp.path().join("disabled-sessions"),
            false,
        ));
        let result = search_real_composer(&disabled, &root).await;
        drop(disabled);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn nested_declared_discovery_routes_through_bound_workspace_grant() {
        if isolated_git_fixture(
            "plugin_context::tests::nested_declared_discovery_routes_through_bound_workspace_grant",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("visible.rs"), "not read by discovery").unwrap();
        std::fs::write(workspace.join("visible-tracked.rs"), "also not read").unwrap();
        std::fs::write(workspace.join("visible-excluded.rs"), "also not read").unwrap();
        stage_fixture_paths(&workspace, &["visible.rs", "visible-tracked.rs"]);
        let xdg = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap());
        // Ignore rules suppress untracked paths, not paths already in the index.
        std::fs::write(xdg.join("git/ignore"), "visible-*.rs\n").unwrap();
        let state = bridge_state(&temp.path().join("sessions"));
        let bound = bound_fixture(workspace.canonicalize().unwrap());
        let request = bcode_tool::ToolInvocationServiceRequest {
            invocation_id: String::new(),
            request_id: "nested".into(),
            route_id: None,
            interface_id: "bcode.filesystem/v1".into(),
            operation: "search_paths".into(),
            payload: serde_json::json!({"query":"visible", "directory":"", "recursive":true, "scope":"workspace", "timeout_ms":1000}),
        };
        let result = route_nested(&state, &bound, request.clone(), 1).await;
        let ToolInvocationServiceResolution::Responded { payload } = result else {
            panic!("nested discovery did not respond: {result:?}");
        };
        assert_eq!(payload["state"], "complete", "{payload}");
        assert!(payload.to_string().contains("visible.rs"), "{payload}");
        assert!(
            payload.to_string().contains("visible-tracked.rs"),
            "{payload}"
        );
        assert!(
            !payload.to_string().contains("visible-excluded.rs"),
            "{payload}"
        );
        let mut undeclared = request;
        undeclared.operation = "write".into();
        assert!(matches!(
            route_nested(&state, &bound, undeclared, 1).await,
            ToolInvocationServiceResolution::Failed { .. }
        ));
        assert!(matches!(
            authorize_workspace(
                &state,
                &bound,
                serde_json::json!({"directory":"../", "recursive":true, "include_hidden":false})
            )
            .await,
            ToolInvocationServiceResolution::Failed { .. }
        ));
        drop(state);
    }

    #[tokio::test]
    async fn resource_read_is_exact_confined_and_independently_authorized() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("resource.txt"), "must not be returned by host").unwrap();
        let state = bridge_state(&temp.path().join("sessions"));
        let mut bound = bound_fixture(root.canonicalize().unwrap());
        let payload = serde_json::json!({"path":"resource.txt"});
        let result = authorize_resource(&state, &bound, payload.clone()).await;
        let ToolInvocationServiceResolution::Responded { payload: grant } = result else {
            panic!("resource authorization failed: {result:?}");
        };
        let grant: WorkspaceResourceGrant = serde_json::from_value(grant).unwrap();
        assert_eq!(grant.workspace_root, bound.root);
        assert_eq!(grant.path, "resource.txt");
        for path in ["", "../resource.txt", "/resource.txt", "missing", "."] {
            assert!(matches!(
                authorize_resource(&state, &bound, serde_json::json!({"path":path})).await,
                ToolInvocationServiceResolution::Failed { .. }
            ));
        }
        // A per-resource denial is evaluated, not discarded because discovery is allowed.
        bound
            .config
            .agent
            .entry("build".into())
            .or_default()
            .permission
            .read
            .insert(
                "*".into(),
                serde_json::from_value(serde_json::json!("deny")).unwrap(),
            );
        assert!(matches!(
            authorize_resource(&state, &bound, payload).await,
            ToolInvocationServiceResolution::Failed { .. }
        ));
        drop(state);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resource_symlink_aliases_are_denied_even_inside_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("target"), "contents").unwrap();
        std::os::unix::fs::symlink(root.join("target"), root.join("alias")).unwrap();
        let state = bridge_state(&temp.path().join("sessions"));
        let bound = bound_fixture(root.canonicalize().unwrap());
        assert!(matches!(
            authorize_resource(&state, &bound, serde_json::json!({"path":"alias"})).await,
            ToolInvocationServiceResolution::Failed { .. }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_discovery_directory_cannot_escape_bound_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let state = bridge_state(&temp.path().join("sessions"));
        let bound = bound_fixture(root.canonicalize().unwrap());
        assert!(matches!(
            authorize_workspace(
                &state,
                &bound,
                serde_json::json!({"directory":"escape", "recursive":true, "include_hidden":false})
            )
            .await,
            ToolInvocationServiceResolution::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn external_resource_grants_are_exact_and_do_not_change_workspace_authority() {
        use bcode_plugin_sdk::workspace_access::ExternalResourceRequest;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let external = temp.path().join("index");
        std::fs::write(&external, "metadata").unwrap();
        let external = external.canonicalize().unwrap();
        let state = bridge_state(&temp.path().join("sessions"));
        let bound = bound_fixture(root.canonicalize().unwrap());
        let result = authorize_external_resource(
            &state,
            &bound,
            serde_json::to_value(ExternalResourceRequest {
                path: external.clone(),
            })
            .unwrap(),
        )
        .await;
        let ToolInvocationServiceResolution::Responded { payload } = result else {
            panic!("authorized exact read")
        };
        let grant: WorkspaceResourceGrant = serde_json::from_value(payload).unwrap();
        assert_eq!(grant.workspace_root.join(&grant.path), external);
        assert_eq!(bound.root, root.canonicalize().unwrap());
        assert!(matches!(
            authorize_resource(&state, &bound, serde_json::json!({"path":"../index"})).await,
            ToolInvocationServiceResolution::Failed { .. }
        ));
        for path in ["relative/index", "/tmp/../index"] {
            assert!(matches!(
                authorize_external_resource(&state, &bound, serde_json::json!({"path":path})).await,
                ToolInvocationServiceResolution::Failed { .. }
            ));
        }
        #[cfg(unix)]
        {
            let alias = temp.path().join("alias");
            std::os::unix::fs::symlink(&external, &alias).unwrap();
            assert!(matches!(
                authorize_external_resource(&state, &bound, serde_json::json!({"path":alias}))
                    .await,
                ToolInvocationServiceResolution::Failed { .. }
            ));
        }
        drop(state);
    }

    #[tokio::test]
    async fn dropping_context_registration_cancels_and_rejects_stale_reuse() {
        let temp = tempfile::tempdir().unwrap();
        let state = bridge_state(&temp.path().join("sessions"));
        let client = ClientId::new();
        let token = state
            .contextual_plugin_invocations
            .lock()
            .unwrap()
            .admit(client, "outer")
            .unwrap();
        let registration = Registration {
            state: &state,
            client,
            id: "outer".into(),
            cancellation: token.clone(),
        };
        drop(registration);
        assert!(token.is_cancelled());
        assert!(!cancel(&state, client, "outer"));
        assert!(
            state
                .contextual_plugin_invocations
                .lock()
                .unwrap()
                .admit(client, "outer")
                .is_none()
        );
        tokio::time::timeout(Duration::from_millis(100), wait_cancel(&token))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn forged_session_and_launch_roots_are_denied() {
        let temp = tempfile::tempdir().unwrap();
        let state = Arc::new(bridge_state(&temp.path().join("sessions")));
        let client = ClientId::new();
        state
            .set_client_runtime_context(
                client,
                Some(bcode_ipc::ClientRuntimeContext {
                    working_directory: Some(temp.path().canonicalize().unwrap()),
                    ..Default::default()
                }),
            )
            .await;
        let mut context = bcode_ipc::PluginServiceContext {
            agent_id: None,
            invocation_id: "outer".into(),
            workspace: bcode_plugin_sdk::WorkspaceContext {
                session_id: Some(SessionId::new().to_string()),
                launch_root: None,
            },
            deadline_ms: 1000,
        };
        assert!(bind(&state, client, &context).await.is_err());
        context.workspace.session_id = None;
        context.workspace.launch_root = Some(PathBuf::from("/"));
        assert!(bind(&state, client, &context).await.is_err());
        context.workspace.launch_root = Some(temp.path().canonicalize().unwrap());
        assert!(bind(&state, client, &context).await.is_ok());
        drop(state);
    }

    #[test]
    fn declared_nested_operations_fail_closed() {
        let valid_operations = |operations: &[String], operation: &str| {
            let mut service: bcode_plugin::PluginService =
                serde_json::from_value(serde_json::json!({"interface_id": "test/v1"})).unwrap();
            service.invocation_operations = operations.to_vec();
            service.permits_invocation(operation)
        };
        assert!(valid_operations(&["discover".into()], "discover"));
        assert!(!valid_operations(&[], "discover"));
        assert!(!valid_operations(&["discover".into()], "write"));
        assert!(!valid_operations(
            &["discover".into(), "discover".into()],
            "discover"
        ));
        assert!(!valid_operations(
            &[String::new(), "discover".into()],
            "discover"
        ));
        assert!(!valid_operations(&["discover\n".into()], "discover\n"));
    }

    #[test]
    fn explicit_route_denial_overrides_manifest_opt_in() {
        let key = route_policy_key("producer", "service/v1", "discover");
        let mut bound = BoundContext {
            id: "outer".into(),
            root: PathBuf::from("/workspace"),
            session: None,
            config: bcode_config::BcodeConfig::default(),
            agent: "build".into(),
            scope: bcode_plugin::PluginInvocationScope::Global,
        };
        assert!(route_policy_allows(
            &bound,
            "producer",
            "service/v1",
            "discover"
        ));
        bound
            .config
            .agent
            .entry("build".into())
            .or_default()
            .tools
            .insert(key, false);
        assert!(!route_policy_allows(
            &bound,
            "producer",
            "service/v1",
            "discover"
        ));
    }

    #[tokio::test]
    async fn roots_must_be_absolute_existing_directories() {
        assert!(canonical_directory(Path::new("relative")).await.is_err());
        assert!(
            canonical_directory(Path::new("/definitely-not-a-real-bcode-workspace"))
                .await
                .is_err()
        );
        let root = canonical_directory(&std::env::temp_dir()).await.unwrap();
        assert!(root.is_absolute());
        assert!(root.is_dir());
    }

    #[test]
    fn forged_nested_identity_denied_and_empty_identity_bound() {
        assert!(bound_identity("", "outer"));
        assert!(bound_identity("outer", "outer"));
        assert!(!bound_identity("another", "outer"));
    }

    #[test]
    fn contextual_deadlines_and_ids_are_bounded() {
        let mut context = bcode_ipc::PluginServiceContext {
            agent_id: None,
            invocation_id: "search".into(),
            workspace: bcode_plugin_sdk::WorkspaceContext::default(),
            deadline_ms: 1000,
        };
        assert!(valid_lifetime(&context));
        context.deadline_ms = 0;
        assert!(!valid_lifetime(&context));
        context.deadline_ms = 30_001;
        assert!(valid_lifetime(&context));
        context.deadline_ms = 900_000;
        assert!(valid_lifetime(&context));
        context.deadline_ms = 900_001;
        assert!(!valid_lifetime(&context));
        context.deadline_ms = 1;
        context.invocation_id = "forged\n".into();
        assert!(!valid_lifetime(&context));
    }
}
