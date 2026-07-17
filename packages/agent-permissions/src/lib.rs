#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared agent permission orchestration for SDK, daemon, TUI, and plugin surfaces.
//!
//! This crate adapts runtime tool-permission requests to Bcode's shared agent policy evaluator
//! without making the pure `bcode_agent_policy` crate depend on runtime execution types.

use bcode_agent_policy::{
    Action, AgentConfig, AgentPermissionConfig, PermissionConfig, evaluate_tool_call,
};
use bcode_agent_profile::tool_policy_authorization_metadata;
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

/// Runtime permission policy backed by Bcode's shared agent policy evaluator.
#[derive(Clone)]
pub struct AgentPermissionPolicy {
    config: AgentConfig,
    cwd: PathBuf,
    ask_callback: Option<PermissionAskCallback>,
}

impl fmt::Debug for AgentPermissionPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentPermissionPolicy")
            .field("config", &self.config)
            .field("cwd", &self.cwd)
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
            cwd: PathBuf::from("."),
            ask_callback: None,
        }
    }

    /// Create a permission policy from a multi-agent configuration and selected agent ID.
    #[must_use]
    pub fn from_permission_config(config: &AgentPermissionConfig, agent_id: &str) -> Self {
        Self::new(bcode_agent_policy::agent_config(config, agent_id))
    }

    /// Configure the workspace used by this domain policy adapter.
    #[must_use]
    pub fn with_working_directory(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
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
    pub fn with_ask_callback(mut self, callback: Option<PermissionAskCallback>) -> Self {
        self.ask_callback = callback;
        self
    }

    /// Return the resolved agent config used by this policy.
    #[must_use]
    pub const fn config(&self) -> &AgentConfig {
        &self.config
    }
}

impl PermissionPolicy for AgentPermissionPolicy {
    fn evaluate_tool_call<'a>(
        &'a self,
        request: &'a RuntimePermissionRequest,
    ) -> bcode_agent_runtime::RuntimeFuture<'a, PermissionDecision> {
        Box::pin(async move {
            let profile_request =
                runtime_permission_request_to_profile_request(request, &self.cwd)?;
            let evaluation = evaluate_profile_tool_call(&self.config, &profile_request, &self.cwd);
            Ok(self.runtime_decision(request, evaluation))
        })
    }
}

impl AgentPermissionPolicy {
    fn runtime_decision(
        &self,
        request: &RuntimePermissionRequest,
        evaluation: EvaluateToolCallResponse,
    ) -> PermissionDecision {
        match evaluation.decision {
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
                        "tool call requires permission but no ask handler is configured".to_string()
                    }))
                }
            }
        }
    }
}

/// Convert owner-produced authorization facts into the shared agent-profile policy request.
///
/// # Errors
///
/// Returns an error when the tool owner omitted, duplicated, or malformed the standard policy
/// authorization fact, or when its identity does not match the correlated runtime call.
pub fn runtime_permission_request_to_profile_request(
    request: &RuntimePermissionRequest,
    cwd: &std::path::Path,
) -> bcode_agent_runtime::Result<EvaluateToolCallRequest> {
    let metadata = tool_policy_authorization_metadata(&request.facts, &request.call.name)
        .map_err(bcode_agent_runtime::RuntimeError::HostExtension)?;
    let aliases = std::iter::once(request.call.name.clone())
        .chain(metadata.permission_category.iter().cloned())
        .chain(metadata.aliases)
        .collect();
    Ok(EvaluateToolCallRequest {
        session_id: request.context.session_id,
        agent_id: request.context.agent_id.clone(),
        tool_name: request.call.name.clone(),
        operation: metadata.operation,
        aliases,
        requires_permission: metadata.requires_permission,
        cwd: Some(cwd.to_string_lossy().into_owned()),
    })
}

/// Evaluate an agent-profile tool-call request against a resolved agent config.
#[must_use]
pub fn evaluate_profile_tool_call(
    config: &AgentConfig,
    request: &EvaluateToolCallRequest,
    cwd: &std::path::Path,
) -> EvaluateToolCallResponse {
    evaluate_tool_call(config, request, cwd).response
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
#[must_use]
pub fn ask_callback<F>(callback: F) -> PermissionAskCallback
where
    F: Fn(&RuntimePermissionRequest, &EvaluateToolCallResponse) -> PermissionDecision
        + Send
        + Sync
        + 'static,
{
    Arc::new(callback)
}
