//! Typed workflow adapters for the high-level Bcode SDK.
//!
//! The host-neutral composition types live in [`bcode_workflow`]. This module adds an ergonomic
//! [`AgentStep`] that executes structured agent turns using a caller-supplied provider factory.

use crate::{
    Agent, AgentBuilder, BcodeError, CancellationToken, ModelProviderInvoker,
    StructuredOutputOptions,
};
pub use bcode_workflow::{
    AbortTaskOnDrop, ArtifactReference, EdgeDefinition, EdgeKind, Field, NodeDefinition, NodeKind,
    NodeRunState, ParallelFailurePolicy, Predicate, PredicateExpression, ResourceAccess,
    ResourceClaim, RetryPolicy, Step, StepContext, ValueSchema, Workflow, WorkflowApprovalResolver,
    WorkflowBuilder, WorkflowCancellation, WorkflowDefinition, WorkflowError, WorkflowEvent,
    WorkflowEventReceiver, WorkflowEventSender, WorkflowGrantScope, WorkflowOutcome, WorkflowPlan,
    WorkflowPolicyGrant, WorkflowPolicyPreflight, WorkflowPolicyRequest, WorkflowRunObserver,
    WorkflowRunSnapshot, WorkflowToolCapability, authorize_workflow_policy, fan_out, field,
    parallel, parallel_named, parallel_named_with_policy, preflight_workflow_policy,
    workflow_event_channel,
};
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

/// Factory that creates an independent provider for each agent-step execution.
pub type WorkflowProviderFactory =
    Arc<dyn Fn() -> Box<dyn ModelProviderInvoker> + Send + Sync + 'static>;

/// Typed builder for one structured Bcode agent workflow step.
pub struct AgentStep<I, O> {
    name: String,
    prompt: Arc<dyn Fn(&I) -> String + Send + Sync>,
    agent: AgentBuilder,
    provider: WorkflowProviderFactory,
    strict: bool,
    max_repairs: u32,
    agent_profile_configured: bool,
    tool_restriction: Option<Vec<String>>,
    read_only_tools: bool,
    resources: Vec<ResourceClaim>,
    timeout: Option<Duration>,
    _types: PhantomData<fn(I) -> O>,
}

impl<I, O> std::fmt::Debug for AgentStep<I, O> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentStep")
            .field("name", &self.name)
            .field("strict", &self.strict)
            .field("max_repairs", &self.max_repairs)
            .field("agent_profile_configured", &self.agent_profile_configured)
            .field("tool_restriction", &self.tool_restriction)
            .field("read_only_tools", &self.read_only_tools)
            .field("resources", &self.resources)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl<I, O> AgentStep<I, O>
where
    I: Serialize + JsonSchema + Send + 'static,
    O: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
{
    /// Create a typed agent step.
    ///
    /// The provider factory is called once per execution, which makes parallel agent steps
    /// independent by construction. The default prompt serializes the complete typed input as
    /// JSON and requests an output matching `O`.
    ///
    /// # Panics
    ///
    /// Panics only if a future input value cannot be serialized despite implementing `Serialize`.
    #[must_use]
    pub fn new<F, P>(name: impl Into<String>, provider: F) -> Self
    where
        F: Fn() -> P + Send + Sync + 'static,
        P: ModelProviderInvoker + 'static,
    {
        Self {
            name: name.into(),
            prompt: Arc::new(|input| {
                serde_json::to_string_pretty(input)
                    .expect("workflow agent input should serialize to JSON")
            }),
            agent: Agent::builder(),
            provider: Arc::new(move || Box::new(provider())),
            strict: true,
            max_repairs: 0,
            agent_profile_configured: false,
            tool_restriction: None,
            read_only_tools: false,
            resources: Vec::new(),
            timeout: None,
            _types: PhantomData,
        }
    }

    /// Configure the Bcode agent profile used by this step.
    #[must_use]
    pub fn agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent = self.agent.agent_id(agent_id);
        self.agent_profile_configured = true;
        self
    }

    /// Configure the model used by this step.
    #[must_use]
    pub fn model(mut self, model_id: impl Into<String>) -> Self {
        self.agent = self.agent.model(model_id);
        self
    }

    /// Configure the provider plugin identity forwarded to each generated provider request.
    #[must_use]
    pub fn provider_plugin(mut self, plugin_id: impl Into<String>) -> Self {
        self.agent = self.agent.provider_plugin(plugin_id);
        self
    }

    /// Configure the step system prompt.
    #[must_use]
    pub fn system(mut self, prompt: impl Into<String>) -> Self {
        self.agent = self.agent.system(prompt);
        self
    }

    /// Configure a typed prompt function.
    #[must_use]
    pub fn prompt_with<F>(mut self, prompt: F) -> Self
    where
        F: Fn(&I) -> String + Send + Sync + 'static,
    {
        self.prompt = Arc::new(prompt);
        self
    }

    /// Restrict this step to an exact set of tool names.
    ///
    /// This composes by intersection with the selected agent profile; it never enables tools that
    /// the profile disabled.
    #[must_use]
    pub fn restrict_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tool_restriction = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    /// Restrict this step to tools declared read-only by their owners.
    #[must_use]
    pub const fn read_only(mut self) -> Self {
        self.read_only_tools = true;
        self
    }

    /// Declare resources acquired atomically before this agent step executes.
    #[must_use]
    pub fn resources(mut self, claims: impl IntoIterator<Item = ResourceClaim>) -> Self {
        self.resources = claims.into_iter().collect();
        self
    }

    /// Configure strict provider-native schema output where supported.
    #[must_use]
    pub const fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Configure bounded structured-output repair attempts.
    #[must_use]
    pub const fn max_repairs(mut self, max_repairs: u32) -> Self {
        self.max_repairs = max_repairs;
        self
    }

    /// Configure an execution timeout for this workflow step.
    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Configure the complete underlying Bcode agent builder.
    ///
    /// Prefer [`Self::agent_id`] when the workflow step requires a configured profile for
    /// mutating authority; this escape hatch intentionally does not assert that a profile was
    /// explicitly selected.
    #[must_use]
    pub fn configure_agent<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(AgentBuilder) -> AgentBuilder,
    {
        self.agent = configure(self.agent);
        self
    }

    /// Build the policy request for this agent step from immutable execution context.
    ///
    /// # Errors
    ///
    /// Returns an error when the step requests mutating capability without an explicitly
    /// configured agent profile or its compiled policy metadata is malformed.
    pub fn policy_request(
        &self,
        initiating: WorkflowToolCapability,
        requested: WorkflowToolCapability,
        scope: WorkflowGrantScope,
        grant: Option<WorkflowPolicyGrant>,
    ) -> Result<WorkflowPolicyRequest, WorkflowError> {
        if requested == WorkflowToolCapability::Mutating && !self.agent_profile_configured {
            return Err(WorkflowError::Build {
                path: self.name.clone(),
                message: "mutating workflow nodes require an explicitly configured agent profile"
                    .to_string(),
            });
        }
        let profile = if self.read_only_tools {
            WorkflowToolCapability::ReadOnly
        } else {
            WorkflowToolCapability::Mutating
        };
        Ok(WorkflowPolicyRequest {
            initiating,
            profile,
            node: requested,
            scope,
            grant,
        })
    }

    /// Finish this agent step so it can be composed with other typed steps.
    #[must_use]
    pub fn build(self) -> Step<I, O> {
        let name = self.name;
        let prompt = self.prompt;
        let provider = self.provider;
        let strict = self.strict;
        let max_repairs = self.max_repairs;
        let agent_profile_configured = self.agent_profile_configured;
        let tool_restriction = self.tool_restriction;
        let read_only_tools = self.read_only_tools;
        let resources = self.resources;
        let timeout = self.timeout;
        let agent = {
            let mut agent = self.agent;
            if let Some(tools) = &tool_restriction {
                agent = agent.restrict_tools(tools.iter().cloned());
            }
            if read_only_tools {
                agent = agent.read_only_tools();
            }
            agent.build()
        };
        let configuration = json!({
            "agent_id": agent.profile_id(),
            "agent_profile_configured": agent_profile_configured,
            "strict": strict,
            "max_repairs": max_repairs,
            "tools": tool_restriction,
            "read_only": read_only_tools,
            "timeout_ms": timeout.map(|value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX)),
        });
        let step_name = name.clone();
        let step = Step::configured_task(
            name,
            NodeKind::Agent,
            configuration,
            move |input: I, context: StepContext| {
                let prompt = prompt(&input);
                let agent = agent.clone();
                let mut provider = provider();
                let step_name = step_name.clone();
                async move {
                    context.ensure_active(step_name.clone())?;
                    let cancellation = CancellationToken::new();
                    let workflow_cancellation = context.cancellation();
                    let cancellation_signal = cancellation.clone();
                    let _cancellation_task = AbortTaskOnDrop::new(tokio::spawn(async move {
                        workflow_cancellation.cancelled().await;
                        cancellation_signal.cancel();
                    }));
                    let options = StructuredOutputOptions::for_type::<O>()
                        .with_strict(strict)
                        .with_max_repairs(max_repairs);
                    agent
                        .generate_object_with_provider_and_request_options(
                            &mut provider,
                            prompt,
                            options,
                            Vec::new(),
                            cancellation,
                        )
                        .await
                        .map_err(|error| map_agent_error(&step_name, &error))
                }
            },
        )
        .resources(resources);
        if let Some(duration) = timeout {
            step.timeout(duration)
        } else {
            step
        }
    }
}

fn map_agent_error(step: &str, error: &BcodeError) -> WorkflowError {
    WorkflowError::step(step, error.to_string())
}

/// Create a typed structured agent workflow step.
#[must_use]
pub fn agent<I, O, F, P>(name: impl Into<String>, provider: F) -> AgentStep<I, O>
where
    I: Serialize + JsonSchema + Send + 'static,
    O: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
    F: Fn() -> P + Send + Sync + 'static,
    P: ModelProviderInvoker + 'static,
{
    AgentStep::new(name, provider)
}
