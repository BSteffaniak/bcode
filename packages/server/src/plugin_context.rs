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
        && (1..=30_000).contains(&context.deadline_ms)
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
        return Err(rejected());
    }
    let request = super::AgentListRequest {
        effective_config_toml: Some(Box::new(
            bcode_config::encode_effective_config(config).map_err(|_| rejected())?,
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
        .map_err(|_| rejected())?
        .agents;
    let id = hint.map_or_else(|| super::default_agent_id(&profiles), str::to_owned);
    if profiles.iter().filter(|profile| profile.id == id).count() != 1 {
        return Err(rejected());
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
        .ok_or_else(rejected)?;
    let session = context
        .workspace
        .session_id
        .as_deref()
        .map(str::parse::<SessionId>)
        .transpose()
        .map_err(|_| rejected())?;
    let (root, config, agent, scope) = if let Some(session) = session {
        if state.attached_client_sessions.lock().await.get(&client) != Some(&session)
            || !state.sessions.session_is_owned(session).await
        {
            return Err(rejected());
        }
        let cwd = state
            .sessions
            .session_working_directory(session)
            .await
            .map_err(|_| rejected())?;
        let root = canonical_directory(&cwd).await?;
        if let Some(hint) = &context.workspace.launch_root
            && canonical_directory(hint).await? != root
        {
            return Err(rejected());
        }
        let config = state.session_config(session).await;
        let selected = super::session_agent_selection(state, session).await;
        if context
            .agent_id
            .as_ref()
            .is_some_and(|hint| hint != &selected)
        {
            return Err(rejected());
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
            .ok_or_else(rejected)?;
        let root = canonical_directory(launch).await?;
        let client_root = runtime.working_directory.as_ref().ok_or_else(rejected)?;
        if canonical_directory(client_root).await? != root {
            return Err(rejected());
        }
        let config = runtime
            .effective_config_toml
            .as_deref()
            .map(|text| bcode_config::decode_effective_config(text))
            .transpose()
            .map_err(|_| rejected())?
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
        return Err(rejected());
    }
    let registry = state.plugins.registry();
    if operation != bcode_plugin_sdk::composer::OP_COMPOSER_SEARCH {
        return Err(rejected());
    }
    let request: bcode_plugin_sdk::composer::ComposerSearchRequest =
        serde_json::from_slice(&payload).map_err(|_| rejected())?;
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
            .ok_or_else(rejected)?;
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
        let bound = bind(state, client, &context).await?;
        if !route_policy_allows(&bound, &plugin, &interface, &operation) {
            return Err(rejected());
        }
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

fn valid_operations(operations: &[String], operation: &str) -> bool {
    !operations.is_empty()
        && operations.len() <= 64
        && operations.iter().all(|entry| {
            !entry.trim().is_empty() && entry.len() <= 256 && !entry.chars().any(char::is_control)
        })
        && operations
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == operations.len()
        && operations.iter().any(|entry| entry == operation)
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
            services.len() == 1 && valid_operations(&services[0].invocation_operations, operation)
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
    use super::*;

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
        let state = super::super::tests::test_server_state_with_plugins(
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
        );
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
                    Ok(reply) => {
                        bcode_plugin_sdk::ServiceResponse::json(&reply).unwrap_or_else(|error| {
                            bcode_plugin_sdk::ServiceResponse::error(
                                "encode_failed",
                                error.to_string(),
                            )
                        })
                    }
                    Err(error) => {
                        bcode_plugin_sdk::ServiceResponse::error("bridge_failed", error.to_string())
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
        assert_eq!(response.error.unwrap().code, "bridge_failed");
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
        state: &ServerState,
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
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("visible.rs"), "NO_CONTENT_IN_RESPONSE").unwrap();
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
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("visible.rs"), "content").unwrap();
        if std::fs::write(
            root.join(std::ffi::OsString::from_vec(vec![b'x', 255])),
            "content",
        )
        .is_err()
        {
            eprintln!("filesystem does not support invalid-byte names; fixture skipped");
            return;
        }
        let state = real_composer_state(&temp.path().join("sessions"), true);
        let response = search_real_composer(&state, &root).await.unwrap();
        drop(state);
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
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(".gitignore"), "excluded.rs\n").unwrap();
        std::fs::write(root.join("excluded.rs"), "EXCLUDED_CONTENT_MARKER").unwrap();
        std::fs::write(root.join("文件 space.rs"), "VISIBLE_CONTENT_MARKER").unwrap();
        let root = root.canonicalize().unwrap();
        let state = real_composer_state(&temp.path().join("sessions"), true);
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
        let disabled = real_composer_state(&temp.path().join("disabled-sessions"), false);
        let result = search_real_composer(&disabled, &root).await;
        drop(disabled);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn nested_declared_discovery_routes_through_bound_workspace_grant() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("visible.rs"), "not read by discovery").unwrap();
        let state = bridge_state(&temp.path().join("sessions"));
        let bound = bound_fixture(workspace.canonicalize().unwrap());
        let request = bcode_tool::ToolInvocationServiceRequest {
            invocation_id: String::new(),
            request_id: "nested".into(),
            route_id: None,
            interface_id: "bcode.filesystem/v1".into(),
            operation: "search_paths".into(),
            payload: serde_json::json!({"query":"visible", "directory":"", "recursive":true, "scope":"workspace"}),
        };
        let result = route_nested(&state, &bound, request.clone(), 1).await;
        let ToolInvocationServiceResolution::Responded { payload } = result else {
            panic!("nested discovery did not respond: {result:?}");
        };
        assert!(payload.to_string().contains("visible.rs"), "{payload}");
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
        let state = bridge_state(&temp.path().join("sessions"));
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
        assert!(!valid_lifetime(&context));
        context.deadline_ms = 1;
        context.invocation_id = "forged\n".into();
        assert!(!valid_lifetime(&context));
    }
}
