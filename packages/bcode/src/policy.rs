//! SDK adapters for Bcode's shared agent permission policy model.

use bcode_agent_policy::{
    Action, AgentConfig, AgentPermissionConfig, PermissionConfig, evaluate_tool_call,
};
use bcode_agent_profile::{AgentDecision, EvaluateToolCallRequest, EvaluateToolCallResponse};
use bcode_agent_runtime::{PermissionDecision, PermissionPolicy, RuntimePermissionRequest};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

/// Callback used to resolve `ask` decisions from the shared agent policy model.
pub type PermissionAskCallback = Arc<
    dyn Fn(&RuntimePermissionRequest, &EvaluateToolCallResponse) -> PermissionDecision
        + Send
        + Sync,
>;

/// SDK permission policy backed by Bcode's shared agent policy evaluator.
#[derive(Clone)]
pub struct AgentPermissionPolicy {
    config: AgentConfig,
    ask_callback: Option<PermissionAskCallback>,
}

impl fmt::Debug for AgentPermissionPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentPermissionPolicy")
            .field("config", &self.config)
            .field("ask_callback", &self.ask_callback.is_some())
            .finish()
    }
}

impl AgentPermissionPolicy {
    /// Create a permission policy from one resolved agent configuration.
    #[must_use]
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            ask_callback: None,
        }
    }

    /// Create a permission policy from a multi-agent configuration and selected agent ID.
    #[must_use]
    pub fn from_permission_config(config: &AgentPermissionConfig, agent_id: &str) -> Self {
        Self::new(bcode_agent_policy::agent_config(config, agent_id))
    }

    /// Attach a callback used only when core policy returns [`AgentDecision::Ask`].
    #[must_use]
    pub fn on_ask<F>(mut self, callback: F) -> Self
    where
        F: Fn(&RuntimePermissionRequest, &EvaluateToolCallResponse) -> PermissionDecision
            + Send
            + Sync
            + 'static,
    {
        self.ask_callback = Some(Arc::new(callback));
        self
    }

    /// Attach a pre-built ask callback.
    #[must_use]
    pub(crate) fn with_ask_callback(mut self, callback: Option<PermissionAskCallback>) -> Self {
        self.ask_callback = callback;
        self
    }
}

impl PermissionPolicy for AgentPermissionPolicy {
    fn evaluate_tool_call<'a>(
        &'a self,
        request: &'a RuntimePermissionRequest,
    ) -> bcode_agent_runtime::RuntimeFuture<'a, PermissionDecision> {
        Box::pin(async move {
            let cwd = request
                .context
                .cwd
                .clone()
                .unwrap_or_else(|| PathBuf::from("."));
            let profile_request = EvaluateToolCallRequest {
                session_id: request.context.session_id,
                agent_id: request.context.agent_id.clone(),
                tool_name: request.call.name.clone(),
                side_effect: request.tool.definition.side_effect,
                policy: request.tool.definition.policy.clone(),
                arguments: request.call.arguments.clone(),
                cwd: request
                    .context
                    .cwd
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
            };
            let evaluation = evaluate_tool_call(&self.config, &profile_request, &cwd).response;
            let decision = match evaluation.decision {
                AgentDecision::Allow => PermissionDecision::Allow,
                AgentDecision::Deny => PermissionDecision::Deny(
                    evaluation
                        .reason
                        .unwrap_or_else(|| "tool call denied by agent policy".to_string()),
                ),
                AgentDecision::Ask => {
                    if let Some(callback) = self.ask_callback.as_ref() {
                        callback(request, &evaluation)
                    } else {
                        PermissionDecision::Ask(evaluation.reason.unwrap_or_else(|| {
                            "tool call requires permission but no ask handler is configured"
                                .to_string()
                        }))
                    }
                }
            };
            Ok(decision)
        })
    }
}

/// Build an agent policy that allows all tool calls without prompting.
#[must_use]
pub fn allow_all_agent_policy() -> AgentPermissionPolicy {
    let config = AgentConfig {
        permission: PermissionConfig {
            command: BTreeMap::from([("*".to_string(), Action::Allow)]),
            read: BTreeMap::from([("*".to_string(), Action::Allow)]),
            write: BTreeMap::from([("*".to_string(), Action::Allow)]),
            edit: BTreeMap::from([("*".to_string(), Action::Allow)]),
            web: BTreeMap::from([("*".to_string(), Action::Allow)]),
            external_directory: Action::Allow,
        },
        ..AgentConfig::default()
    };
    AgentPermissionPolicy::new(config)
}

/// Box an ask callback for builder storage.
pub fn ask_callback<F>(callback: F) -> PermissionAskCallback
where
    F: Fn(&RuntimePermissionRequest, &EvaluateToolCallResponse) -> PermissionDecision
        + Send
        + Sync
        + 'static,
{
    Arc::new(callback)
}
