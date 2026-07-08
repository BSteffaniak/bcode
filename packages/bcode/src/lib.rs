#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! High-level Rust SDK facade for Bcode.
//!
//! This crate provides explicit, application-facing types for building AI applications with Bcode.
//! The facade is intentionally small and delegates reusable turn behavior to
//! `bcode_agent_runtime`.

use bcode_agent_permissions::{PermissionAskCallback, ask_callback};
use bcode_agent_policy::active_tools_for;
#[cfg(feature = "embedded-plugins")]
use bcode_agent_runtime::RuntimeFuture;
use bcode_agent_runtime::{AgentRuntime, AgentTurnRequest, AgentTurnResponse};
#[cfg(feature = "embedded-plugins")]
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, MODEL_PROVIDER_INTERFACE_ID,
    ModelTurnRequest, OP_CANCEL_TURN, OP_FINISH_TURN, OP_POLL_TURN_EVENTS, OP_START_TURN,
    PollTurnEventsRequest, PollTurnEventsResponse, StartTurnResponse,
};
use bcode_model::{ModelParameters, ProviderRequestContext};
use bcode_session_models::SessionId;
use bcode_tool::{ToolDefinition, ToolInvocationRequest, ToolInvocationResponse};
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

pub use bcode_agent_permissions::{AgentPermissionPolicy, allow_all_agent_policy};
pub use bcode_agent_policy::{Action, AgentConfig, AgentPermissionConfig, PermissionConfig};
pub use bcode_agent_profile::{AgentDecision, EvaluateToolCallResponse};
pub use bcode_agent_runtime::{
    AgentRuntimeEvent as AgentEvent, AgentRuntimeStream as AgentStream,
    AgentRuntimeStreamItem as AgentStreamItem, AllowAllPolicy, CancellationToken,
    ModelProviderInvoker, PermissionDecision, PermissionPolicy, RegisteredTool, RuntimeError,
    RuntimeFuture, RuntimePermissionContext, RuntimePermissionRequest, ToolCatalog,
    ToolExecutionOutput, ToolExecutor, ToolRoundState, ToolSource, UnifiedToolCatalog,
};
pub use bcode_model::{ContentBlock as ModelContentBlock, MessageRole, ModelMessage, ToolCall};

/// Result alias for Bcode SDK operations.
pub type Result<T> = std::result::Result<T, BcodeError>;

/// High-level SDK error.
#[derive(Debug, Error)]
pub enum BcodeError {
    /// Agent runtime failed.
    #[error("agent runtime error: {0}")]
    Runtime(#[from] RuntimeError),
    /// No provider is configured for a requested model operation.
    #[error("no provider configured")]
    MissingProvider,
    /// Embedded plugin runtime is required for this operation.
    #[error("embedded plugin runtime is not configured")]
    MissingPluginRuntime,
    /// Hook callback failed.
    #[error("hook error: {0}")]
    Hook(String),
    /// Structured output was invalid or could not be decoded.
    #[error("structured output error: {0}")]
    StructuredOutput(String),
    /// Tool execution failed.
    #[error("tool execution error: {0}")]
    ToolExecution(String),
}

/// Structured-output generation options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredOutputOptions {
    /// Human-readable object/schema name sent to providers that support native structured output.
    pub name: String,
    /// JSON schema used for provider-native structured output and local validation.
    pub schema: serde_json::Value,
    /// Request strict provider-native schema validation where supported.
    pub strict: bool,
    /// Maximum invalid-output repair attempts after the initial provider response.
    pub max_repairs: u32,
}

impl StructuredOutputOptions {
    /// Build options from a Rust type implementing [`schemars::JsonSchema`].
    ///
    /// # Panics
    ///
    /// Panics only if schemars emits a schema value that cannot be serialized to JSON.
    #[must_use]
    pub fn for_type<T>() -> Self
    where
        T: schemars::JsonSchema,
    {
        let schema = schemars::schema_for!(T);
        Self {
            name: std::any::type_name::<T>()
                .rsplit("::")
                .next()
                .unwrap_or("StructuredOutput")
                .to_string(),
            schema: serde_json::to_value(schema)
                .expect("schemars schema should serialize to JSON value"),
            strict: true,
            max_repairs: 0,
        }
    }

    /// Build options from an explicit JSON schema.
    #[must_use]
    pub fn json_schema(name: impl Into<String>, schema: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            schema,
            strict: true,
            max_repairs: 0,
        }
    }

    /// Configure whether strict provider-native schema validation should be requested.
    #[must_use]
    pub const fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Configure invalid-output repair attempts.
    #[must_use]
    pub const fn with_max_repairs(mut self, max_repairs: u32) -> Self {
        self.max_repairs = max_repairs;
        self
    }
}

/// Context supplied to model-call hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCallContext {
    /// Agent name, when configured.
    pub agent_name: Option<String>,
    /// Selected provider plugin ID, when configured.
    pub provider_plugin_id: Option<String>,
    /// Selected model ID.
    pub model_id: String,
    /// User prompt for the model call.
    pub prompt: String,
}

/// Context supplied after a successful model call.
#[derive(Debug, Clone)]
pub struct ModelCallOutcome {
    /// Generated text response and runtime metadata.
    pub response: GenerateTextResponse,
}

/// Context supplied to tool-call hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallContext {
    /// Agent name, when configured.
    pub agent_name: Option<String>,
    /// Requested tool call.
    pub call: ToolCall,
}

/// Context supplied after a successful tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallOutcome {
    /// Tool execution output including model result and normalized events.
    pub output: ToolExecutionOutput,
}

type ModelBeforeHook = Arc<dyn Fn(&ModelCallContext) -> Result<()> + Send + Sync>;
type ModelAfterHook = Arc<dyn Fn(&ModelCallContext, &ModelCallOutcome) -> Result<()> + Send + Sync>;
type ToolBeforeHook = Arc<dyn Fn(&ToolCallContext) -> Result<()> + Send + Sync>;
type ToolAfterHook = Arc<dyn Fn(&ToolCallContext, &ToolCallOutcome) -> Result<()> + Send + Sync>;

/// Typed SDK hook callbacks for logging, tracing, budgets, and safety checks.
#[derive(Clone, Default)]
pub struct AgentHooks {
    before_model: Vec<ModelBeforeHook>,
    after_model: Vec<ModelAfterHook>,
    before_tool: Vec<ToolBeforeHook>,
    after_tool: Vec<ToolAfterHook>,
}

impl fmt::Debug for AgentHooks {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentHooks")
            .field("before_model", &self.before_model.len())
            .field("after_model", &self.after_model.len())
            .field("before_tool", &self.before_tool.len())
            .field("after_tool", &self.after_tool.len())
            .finish()
    }
}

impl AgentHooks {
    /// Create an empty hook set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a hook that runs before model calls.
    #[must_use]
    pub fn on_before_model<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ModelCallContext) -> Result<()> + Send + Sync + 'static,
    {
        self.before_model.push(Arc::new(hook));
        self
    }

    /// Add a hook that runs after successful model calls.
    #[must_use]
    pub fn on_after_model<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ModelCallContext, &ModelCallOutcome) -> Result<()> + Send + Sync + 'static,
    {
        self.after_model.push(Arc::new(hook));
        self
    }

    /// Add a hook that runs before tool calls.
    #[must_use]
    pub fn on_before_tool<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ToolCallContext) -> Result<()> + Send + Sync + 'static,
    {
        self.before_tool.push(Arc::new(hook));
        self
    }

    /// Add a hook that runs after successful tool calls.
    #[must_use]
    pub fn on_after_tool<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ToolCallContext, &ToolCallOutcome) -> Result<()> + Send + Sync + 'static,
    {
        self.after_tool.push(Arc::new(hook));
        self
    }

    fn run_before_model(&self, context: &ModelCallContext) -> Result<()> {
        for hook in &self.before_model {
            hook(context)?;
        }
        Ok(())
    }

    fn run_after_model(
        &self,
        context: &ModelCallContext,
        outcome: &ModelCallOutcome,
    ) -> Result<()> {
        for hook in &self.after_model {
            hook(context, outcome)?;
        }
        Ok(())
    }

    fn run_before_tool(&self, context: &ToolCallContext) -> Result<()> {
        for hook in &self.before_tool {
            hook(context)?;
        }
        Ok(())
    }

    fn run_after_tool(&self, context: &ToolCallContext, outcome: &ToolCallOutcome) -> Result<()> {
        for hook in &self.after_tool {
            hook(context, outcome)?;
        }
        Ok(())
    }
}

type InlineToolHandler = Arc<
    dyn Fn(ToolInvocationRequest) -> std::result::Result<ToolInvocationResponse, String>
        + Send
        + Sync,
>;

fn structured_prompt(prompt: &str, options: &StructuredOutputOptions) -> String {
    format!(
        "{prompt}\n\nReturn only a JSON object that matches this JSON Schema. Do not wrap it in Markdown fences or include explanatory text.\nSchema name: {name}\nSchema:\n{schema}",
        name = options.name,
        schema = options.schema
    )
}

fn repair_prompt(
    original_prompt: &str,
    options: &StructuredOutputOptions,
    invalid_output: &str,
    error: &BcodeError,
) -> String {
    format!(
        "{base}\n\nThe previous response was invalid. Error: {error}\nPrevious response:\n{invalid_output}\n\nReturn a corrected JSON object only.",
        base = structured_prompt(original_prompt, options),
    )
}

fn decode_structured_output<T>(schema: &serde_json::Value, text: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    let value = extract_structured_json(text)?;
    validate_json_schema(schema, &value)?;
    serde_json::from_value(value).map_err(|error| {
        BcodeError::StructuredOutput(format!("failed to deserialize structured output: {error}"))
    })
}

fn extract_structured_json(text: &str) -> Result<serde_json::Value> {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => Ok(value),
        Err(error) => {
            let Some(slice) = json_object_slice(text) else {
                return Err(BcodeError::StructuredOutput(format!(
                    "model output was not valid JSON: {error}; output: {text}"
                )));
            };
            serde_json::from_str(slice).map_err(|slice_error| {
                BcodeError::StructuredOutput(format!(
                    "failed to parse JSON object from model output: {slice_error}; output: {text}"
                ))
            })
        }
    }
}

fn json_object_slice(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (start <= end).then_some(&text[start..=end])
}

fn validate_json_schema(schema: &serde_json::Value, value: &serde_json::Value) -> Result<()> {
    let validator = jsonschema::validator_for(schema).map_err(|error| {
        BcodeError::StructuredOutput(format!("invalid structured-output JSON schema: {error}"))
    })?;
    if validator.is_valid(value) {
        Ok(())
    } else {
        let errors = validator
            .iter_errors(value)
            .map(|error| error.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        Err(BcodeError::StructuredOutput(format!(
            "structured output failed JSON schema validation: {errors}"
        )))
    }
}

fn user_message(text: impl Into<String>) -> ModelMessage {
    ModelMessage {
        role: MessageRole::User,
        content: vec![ModelContentBlock::Text { text: text.into() }],
    }
}

fn assistant_message(text: impl Into<String>) -> ModelMessage {
    ModelMessage {
        role: MessageRole::Assistant,
        content: vec![ModelContentBlock::Text { text: text.into() }],
    }
}

#[derive(Clone)]
struct InlineToolExecutor {
    handlers: BTreeMap<String, InlineToolHandler>,
    #[cfg(feature = "embedded-plugins")]
    plugins: Option<bcode_plugin::PluginRuntimeHost>,
}

impl fmt::Debug for InlineToolExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("InlineToolExecutor");
        debug.field("tools", &self.handlers.keys().collect::<Vec<_>>());
        #[cfg(feature = "embedded-plugins")]
        debug.field("plugins", &self.plugins.is_some());
        #[cfg(not(feature = "embedded-plugins"))]
        debug.field("plugins", &false);
        debug.finish()
    }
}

impl ToolExecutor for InlineToolExecutor {
    fn execute_tool<'a>(
        &'a self,
        tool: &'a RegisteredTool,
        request: &'a ToolInvocationRequest,
    ) -> bcode_agent_runtime::RuntimeFuture<'a, ToolInvocationResponse> {
        Box::pin(async move {
            match &tool.source {
                ToolSource::Inline => {
                    let handler = self.handlers.get(&tool.definition.name).ok_or_else(|| {
                        RuntimeError::ToolExecution {
                            tool_name: tool.definition.name.clone(),
                            message: "inline tool handler not found".to_string(),
                        }
                    })?;
                    handler(request.clone()).map_err(|message| RuntimeError::ToolExecution {
                        tool_name: tool.definition.name.clone(),
                        message,
                    })
                }
                ToolSource::Plugin { plugin_id } => {
                    #[cfg(feature = "embedded-plugins")]
                    {
                        execute_plugin_tool(self.plugins.as_ref(), plugin_id, request).await
                    }
                    #[cfg(not(feature = "embedded-plugins"))]
                    {
                        execute_plugin_tool(plugin_id, request)
                    }
                }
            }
        })
    }
}

#[cfg(feature = "embedded-plugins")]
async fn execute_plugin_tool(
    plugins: Option<&bcode_plugin::PluginRuntimeHost>,
    plugin_id: &str,
    request: &ToolInvocationRequest,
) -> std::result::Result<ToolInvocationResponse, RuntimeError> {
    let plugins = plugins.ok_or_else(|| RuntimeError::ToolExecution {
        tool_name: request.name.clone(),
        message: "embedded plugin runtime is not configured".to_string(),
    })?;
    plugins
        .invoke_service_json_scoped(
            plugin_id,
            bcode_tool::TOOL_SERVICE_INTERFACE_ID,
            bcode_tool::OP_INVOKE_TOOL,
            request,
            bcode_plugin::PluginInvocationScope::Global,
        )
        .await
        .map_err(|error| RuntimeError::ToolExecution {
            tool_name: request.name.clone(),
            message: error.to_string(),
        })
}

#[cfg(not(feature = "embedded-plugins"))]
fn execute_plugin_tool(
    plugin_id: &str,
    request: &ToolInvocationRequest,
) -> std::result::Result<ToolInvocationResponse, RuntimeError> {
    Err(RuntimeError::ToolExecution {
        tool_name: request.name.clone(),
        message: format!(
            "plugin-backed tool routing for plugin '{plugin_id}' requires embedded-plugins"
        ),
    })
}

/// Provider invoker backed by a loaded Bcode plugin runtime.
#[cfg(feature = "embedded-plugins")]
#[derive(Debug, Clone)]
pub struct PluginModelProviderInvoker {
    plugins: bcode_plugin::PluginRuntimeHost,
}

#[cfg(feature = "embedded-plugins")]
impl PluginModelProviderInvoker {
    /// Create a provider invoker from a loaded plugin runtime host.
    #[must_use]
    pub const fn new(plugins: bcode_plugin::PluginRuntimeHost) -> Self {
        Self { plugins }
    }

    fn resolve_provider(
        &self,
        provider_plugin_id: Option<&str>,
    ) -> std::result::Result<String, RuntimeError> {
        provider_plugin_id.map_or_else(
            || {
                self.plugins
                    .registry()
                    .service_registry()
                    .unique_provider(MODEL_PROVIDER_INTERFACE_ID)
                    .map(str::to_string)
                    .map_err(|error| RuntimeError::ProviderInvocation(error.to_string()))
            },
            |provider_plugin_id| Ok(provider_plugin_id.to_string()),
        )
    }
}

#[cfg(feature = "embedded-plugins")]
impl ModelProviderInvoker for PluginModelProviderInvoker {
    fn start_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        Box::pin(async move {
            let provider_plugin_id = self.resolve_provider(provider_plugin_id)?;
            self.plugins
                .invoke_service_json_scoped(
                    &provider_plugin_id,
                    MODEL_PROVIDER_INTERFACE_ID,
                    OP_START_TURN,
                    request,
                    bcode_plugin::PluginInvocationScope::Global,
                )
                .await
                .map_err(|error| RuntimeError::ProviderInvocation(error.to_string()))
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        Box::pin(async move {
            let provider_plugin_id = self.resolve_provider(provider_plugin_id)?;
            self.plugins
                .invoke_service_json_scoped(
                    &provider_plugin_id,
                    MODEL_PROVIDER_INTERFACE_ID,
                    OP_POLL_TURN_EVENTS,
                    request,
                    bcode_plugin::PluginInvocationScope::Global,
                )
                .await
                .map_err(|error| RuntimeError::ProviderInvocation(error.to_string()))
        })
    }

    fn cancel_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a CancelTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        Box::pin(async move {
            let provider_plugin_id = self.resolve_provider(provider_plugin_id)?;
            self.plugins
                .invoke_service_json_scoped(
                    &provider_plugin_id,
                    MODEL_PROVIDER_INTERFACE_ID,
                    OP_CANCEL_TURN,
                    request,
                    bcode_plugin::PluginInvocationScope::Global,
                )
                .await
                .map_err(|error| RuntimeError::ProviderInvocation(error.to_string()))
        })
    }

    fn finish_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a FinishTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        Box::pin(async move {
            let provider_plugin_id = self.resolve_provider(provider_plugin_id)?;
            self.plugins
                .invoke_service_json_scoped(
                    &provider_plugin_id,
                    MODEL_PROVIDER_INTERFACE_ID,
                    OP_FINISH_TURN,
                    request,
                    bcode_plugin::PluginInvocationScope::Global,
                )
                .await
                .map_err(|error| RuntimeError::ProviderInvocation(error.to_string()))
        })
    }
}

/// Bcode SDK runtime mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BcodeMode {
    /// Run directly in-process without daemon IPC.
    #[default]
    Embedded,
    /// Route operations through a daemon-backed client.
    Daemon,
}

/// Top-level SDK handle.
#[derive(Debug, Clone)]
pub struct Bcode {
    mode: BcodeMode,
    runtime: AgentRuntime,
    #[cfg(feature = "embedded-plugins")]
    provider: Option<PluginModelProviderInvoker>,
    #[cfg(feature = "embedded-plugins")]
    plugins: Option<bcode_plugin::PluginRuntimeHost>,
}

impl Bcode {
    /// Start building a Bcode SDK handle.
    #[must_use]
    pub fn builder() -> BcodeBuilder {
        BcodeBuilder::default()
    }

    /// Start building an agent attached to this SDK handle.
    #[must_use]
    pub fn agent(&self) -> AgentBuilder {
        let builder = AgentBuilder::default().runtime(self.runtime.clone());
        #[cfg(feature = "embedded-plugins")]
        let builder = if let Some(provider) = self.provider.clone() {
            builder.provider_invoker(provider)
        } else {
            builder
        };
        #[cfg(feature = "embedded-plugins")]
        let builder = if let Some(plugins) = self.plugins.clone() {
            builder.plugin_runtime(plugins)
        } else {
            builder
        };
        builder
    }

    /// Return the configured runtime mode.
    #[must_use]
    pub const fn mode(&self) -> BcodeMode {
        self.mode
    }
}

/// Builder for [`Bcode`].
#[derive(Debug, Clone)]
pub struct BcodeBuilder {
    mode: BcodeMode,
    runtime: AgentRuntime,
    #[cfg(feature = "embedded-plugins")]
    provider: Option<PluginModelProviderInvoker>,
    #[cfg(feature = "embedded-plugins")]
    plugins: Option<bcode_plugin::PluginRuntimeHost>,
}

impl Default for BcodeBuilder {
    fn default() -> Self {
        Self {
            mode: BcodeMode::Embedded,
            runtime: AgentRuntime::new(),
            #[cfg(feature = "embedded-plugins")]
            provider: None,
            #[cfg(feature = "embedded-plugins")]
            plugins: None,
        }
    }
}

impl BcodeBuilder {
    /// Configure the runtime mode.
    #[must_use]
    pub const fn mode(mut self, mode: BcodeMode) -> Self {
        self.mode = mode;
        self
    }

    /// Configure the reusable agent runtime used by agents built from this handle.
    #[must_use]
    pub const fn runtime(mut self, runtime: AgentRuntime) -> Self {
        self.runtime = runtime;
        self
    }

    /// Configure a plugin-backed embedded provider invoker.
    #[cfg(feature = "embedded-plugins")]
    #[must_use]
    pub fn plugin_runtime(mut self, plugins: bcode_plugin::PluginRuntimeHost) -> Self {
        self.provider = Some(PluginModelProviderInvoker::new(plugins.clone()));
        self.plugins = Some(plugins);
        self
    }

    /// Build the SDK handle.
    #[cfg(not(feature = "embedded-plugins"))]
    #[must_use]
    pub const fn build(self) -> Bcode {
        Bcode {
            mode: self.mode,
            runtime: self.runtime,
        }
    }

    /// Build the SDK handle.
    #[cfg(feature = "embedded-plugins")]
    #[must_use]
    pub fn build(self) -> Bcode {
        Bcode {
            mode: self.mode,
            runtime: self.runtime,
            provider: self.provider,
            plugins: self.plugins,
        }
    }
}

/// In-memory SDK session state for continuing conversations without persistence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InMemorySession {
    messages: Vec<ModelMessage>,
}

impl InMemorySession {
    /// Create an empty in-memory session.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a session from caller-managed model messages.
    #[must_use]
    pub const fn from_messages(messages: Vec<ModelMessage>) -> Self {
        Self { messages }
    }

    /// Return the current session transcript.
    #[must_use]
    pub fn messages(&self) -> &[ModelMessage] {
        &self.messages
    }

    /// Export the session transcript for caller-managed persistence.
    #[must_use]
    pub fn into_messages(self) -> Vec<ModelMessage> {
        self.messages
    }

    /// Add a message to the session transcript.
    pub fn push_message(&mut self, message: ModelMessage) {
        self.messages.push(message);
    }

    /// Clear the in-memory transcript.
    pub fn clear(&mut self) {
        self.messages.clear();
    }
}

/// Stateful agent wrapper that keeps conversation history in memory.
#[derive(Debug, Clone)]
pub struct AgentSession {
    agent: Agent,
    session: InMemorySession,
}

impl AgentSession {
    /// Create a stateful wrapper around an agent and in-memory session.
    #[must_use]
    pub const fn new(agent: Agent, session: InMemorySession) -> Self {
        Self { agent, session }
    }

    /// Return the wrapped agent.
    #[must_use]
    pub const fn agent(&self) -> &Agent {
        &self.agent
    }

    /// Return the in-memory session state.
    #[must_use]
    pub const fn session(&self) -> &InMemorySession {
        &self.session
    }

    /// Export the in-memory session transcript for caller-managed persistence.
    #[must_use]
    pub fn into_messages(self) -> Vec<ModelMessage> {
        self.session.into_messages()
    }

    /// Generate text and append user/assistant messages to the in-memory transcript.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation fails, the runtime is cancelled, or the provider
    /// reports an error.
    pub async fn generate_text_with_provider<P>(
        &mut self,
        provider: &mut P,
        prompt: impl Into<String>,
    ) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        let prompt = prompt.into();
        let response = self
            .agent
            .generate_text_with_provider_and_history(
                provider,
                prompt.clone(),
                self.session.messages.clone(),
            )
            .await?;
        self.session.messages.push(user_message(prompt));
        self.session
            .messages
            .push(assistant_message(response.text.clone()));
        Ok(response)
    }
}

#[derive(Clone)]
pub struct Agent {
    runtime: AgentRuntime,
    name: Option<String>,
    profile_id: String,
    session_id: SessionId,
    cwd: Option<PathBuf>,
    provider_plugin_id: Option<String>,
    model_id: String,
    provider_context: ProviderRequestContext,
    system_prompt: Option<String>,
    parameters: ModelParameters,
    metadata: BTreeMap<String, String>,
    timeout: Duration,
    max_tool_rounds: u32,
    tool_catalog: UnifiedToolCatalog,
    inline_tool_handlers: BTreeMap<String, InlineToolHandler>,
    hooks: AgentHooks,
    policy_config: AgentConfig,
    permission_policy: Arc<dyn PermissionPolicy>,
    #[cfg(feature = "embedded-plugins")]
    provider: Option<PluginModelProviderInvoker>,
    #[cfg(feature = "embedded-plugins")]
    plugins: Option<bcode_plugin::PluginRuntimeHost>,
}

impl fmt::Debug for Agent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("Agent");
        debug
            .field("runtime", &self.runtime)
            .field("name", &self.name)
            .field("profile_id", &self.profile_id)
            .field("session_id", &self.session_id)
            .field("cwd", &self.cwd)
            .field("provider_plugin_id", &self.provider_plugin_id)
            .field("model_id", &self.model_id)
            .field("provider_context", &self.provider_context)
            .field("system_prompt", &self.system_prompt)
            .field("parameters", &self.parameters)
            .field("metadata", &self.metadata)
            .field("timeout", &self.timeout)
            .field("max_tool_rounds", &self.max_tool_rounds)
            .field("tool_catalog", &self.tool_catalog)
            .field(
                "inline_tool_handlers",
                &self.inline_tool_handlers.keys().collect::<Vec<_>>(),
            )
            .field("hooks", &self.hooks)
            .field("policy_config", &self.policy_config)
            .field("permission_policy", &"<policy>");
        #[cfg(feature = "embedded-plugins")]
        debug
            .field("provider", &self.provider)
            .field("plugins", &self.plugins.is_some());
        debug.finish()
    }
}

impl Agent {
    /// Start building an agent.
    #[must_use]
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// Generate text using the agent's configured embedded provider.
    ///
    /// # Errors
    ///
    /// Returns an error when no embedded provider is configured, provider invocation fails, the
    /// runtime is cancelled, or the provider reports an error.
    #[cfg(feature = "embedded-plugins")]
    pub async fn generate_text(&self, prompt: impl Into<String>) -> Result<GenerateTextResponse> {
        let mut provider = self.provider.clone().ok_or(BcodeError::MissingProvider)?;
        self.generate_text_with_provider(&mut provider, prompt)
            .await
    }

    /// Generate text using a caller-supplied provider invoker.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation fails, the runtime is cancelled, or the provider
    /// reports an error.
    pub async fn generate_text_with_provider<P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
    ) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        self.generate_text_with_provider_with_structured_output(provider, prompt, None)
            .await
    }

    async fn generate_text_with_provider_and_history<P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
        messages: Vec<ModelMessage>,
    ) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        self.generate_text_with_provider_with_options(provider, prompt, None, messages)
            .await
    }

    /// Generate and deserialize a structured object using the agent's embedded provider.
    ///
    /// # Errors
    ///
    /// Returns an error when no embedded provider is configured, provider invocation fails,
    /// structured JSON cannot be extracted, schema validation fails, or deserialization fails.
    #[cfg(feature = "embedded-plugins")]
    pub async fn generate_object<T>(&self, prompt: impl Into<String>) -> Result<T>
    where
        T: DeserializeOwned + schemars::JsonSchema,
    {
        let mut provider = self.provider.clone().ok_or(BcodeError::MissingProvider)?;
        self.generate_object_with_provider(&mut provider, prompt)
            .await
    }

    /// Generate and deserialize a structured object using a caller-supplied provider invoker.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation fails, structured JSON cannot be extracted,
    /// schema validation fails, or deserialization fails.
    pub async fn generate_object_with_provider<T, P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
    ) -> Result<T>
    where
        T: DeserializeOwned + schemars::JsonSchema,
        P: ModelProviderInvoker,
    {
        self.generate_object_with_provider_and_options(
            provider,
            prompt,
            StructuredOutputOptions::for_type::<T>(),
        )
        .await
    }

    /// Generate and deserialize a structured object using explicit structured-output options.
    ///
    /// # Errors
    ///
    /// Returns an error when provider invocation fails, structured JSON cannot be extracted,
    /// schema validation fails, or deserialization fails.
    pub async fn generate_object_with_provider_and_options<T, P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
        options: StructuredOutputOptions,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        P: ModelProviderInvoker,
    {
        let prompt = prompt.into();
        let schema = options.schema.clone();
        let structured_output = bcode_model::StructuredOutputRequest {
            name: options.name.clone(),
            schema: schema.clone(),
            strict: options.strict,
        };
        let mut current_prompt = structured_prompt(&prompt, &options);
        let mut last_error = None;
        for attempt in 0..=options.max_repairs {
            let response = self
                .generate_text_with_provider_with_structured_output(
                    provider,
                    current_prompt.clone(),
                    Some(structured_output.clone()),
                )
                .await?;
            match decode_structured_output(&schema, &response.text) {
                Ok(value) => return Ok(value),
                Err(error) if attempt < options.max_repairs => {
                    current_prompt = repair_prompt(&prompt, &options, &response.text, &error);
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            BcodeError::StructuredOutput("structured output repair loop did not run".to_string())
        }))
    }

    async fn generate_text_with_provider_with_structured_output<P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
        structured_output: Option<bcode_model::StructuredOutputRequest>,
    ) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        self.generate_text_with_provider_with_options(
            provider,
            prompt,
            structured_output,
            Vec::new(),
        )
        .await
    }

    async fn generate_text_with_provider_with_options<P>(
        &self,
        provider: &mut P,
        prompt: impl Into<String>,
        structured_output: Option<bcode_model::StructuredOutputRequest>,
        messages: Vec<ModelMessage>,
    ) -> Result<GenerateTextResponse>
    where
        P: ModelProviderInvoker,
    {
        let prompt = prompt.into();
        let context = self.model_call_context(prompt.clone());
        self.hooks.run_before_model(&context)?;
        let response = self
            .runtime
            .run_text_turn(
                provider,
                self.turn_request_with_structured_output_and_messages(
                    prompt,
                    structured_output,
                    messages,
                ),
            )
            .await?;
        let response = GenerateTextResponse::from(response);
        self.hooks.run_after_model(
            &context,
            &ModelCallOutcome {
                response: response.clone(),
            },
        )?;
        Ok(response)
    }

    /// Stream text using the agent's configured embedded provider.
    ///
    /// The returned stream yields normalized [`AgentStreamItem`] values and does not require the
    /// TUI or daemon when an embedded plugin provider is configured.
    ///
    /// # Errors
    ///
    /// Returns an error when no embedded provider is configured.
    #[cfg(feature = "embedded-plugins")]
    pub fn stream_text(&self, prompt: impl Into<String>) -> Result<AgentStream> {
        let provider = self.provider.clone().ok_or(BcodeError::MissingProvider)?;
        Ok(self.runtime.run_streaming_text_turn(
            provider,
            self.turn_request_with_structured_output_and_messages(prompt.into(), None, Vec::new()),
        ))
    }

    /// Stream text using the agent's configured embedded provider and cancellation token.
    ///
    /// Cancelling the token requests provider cancellation and terminates the stream with a
    /// [`RuntimeError::Cancelled`] item.
    ///
    /// # Errors
    ///
    /// Returns an error when no embedded provider is configured.
    #[cfg(feature = "embedded-plugins")]
    pub fn stream_text_with_cancellation(
        &self,
        prompt: impl Into<String>,
        cancellation: CancellationToken,
    ) -> Result<AgentStream> {
        let provider = self.provider.clone().ok_or(BcodeError::MissingProvider)?;
        Ok(self.runtime.run_streaming_text_turn(
            provider,
            self.turn_request_with_cancellation(prompt.into(), cancellation),
        ))
    }

    /// Stream text using a caller-supplied provider invoker.
    ///
    /// The returned stream yields text deltas, reasoning deltas, tool-call events, warnings, usage,
    /// a final response, or an error.
    #[must_use]
    pub fn stream_text_with_provider<P>(
        &self,
        provider: P,
        prompt: impl Into<String>,
    ) -> AgentStream
    where
        P: ModelProviderInvoker + 'static,
    {
        self.stream_text_with_provider_and_cancellation(provider, prompt, CancellationToken::new())
    }

    /// Stream text using a caller-supplied provider invoker and cancellation token.
    ///
    /// Cancelling the token requests provider cancellation and terminates the stream with a
    /// [`RuntimeError::Cancelled`] item.
    #[must_use]
    pub fn stream_text_with_provider_and_cancellation<P>(
        &self,
        provider: P,
        prompt: impl Into<String>,
        cancellation: CancellationToken,
    ) -> AgentStream
    where
        P: ModelProviderInvoker + 'static,
    {
        self.runtime.run_streaming_text_turn(
            provider,
            self.turn_request_with_cancellation(prompt.into(), cancellation),
        )
    }

    /// Create mutable tool-round state using this agent's configured maximum.
    #[must_use]
    pub const fn tool_round_state(&self) -> ToolRoundState {
        ToolRoundState::new(self.max_tool_rounds)
    }

    /// Execute a registered tool call through this agent's unified tool catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the tool is unknown, denied, or its handler fails.
    pub async fn execute_tool_call(&self, call: &ToolCall) -> Result<ToolExecutionOutput> {
        let executor = InlineToolExecutor {
            handlers: self.inline_tool_handlers.clone(),
            #[cfg(feature = "embedded-plugins")]
            plugins: self.plugins.clone(),
        };
        let context = ToolCallContext {
            agent_name: self.name.clone(),
            call: call.clone(),
        };
        self.hooks.run_before_tool(&context)?;
        let output = self
            .runtime
            .execute_tool_call_with_context(
                &self.tool_catalog,
                self.permission_policy.as_ref(),
                &executor,
                call,
                &self.permission_context(),
            )
            .await?;
        self.hooks.run_after_tool(
            &context,
            &ToolCallOutcome {
                output: output.clone(),
            },
        )?;
        Ok(output)
    }

    /// Execute a registered tool call through this agent's unified tool catalog and round budget.
    ///
    /// # Errors
    ///
    /// Returns an error when the maximum number of tool rounds is exhausted, the tool is unknown,
    /// denied, or its handler fails.
    pub async fn execute_tool_call_with_round_state(
        &self,
        call: &ToolCall,
        rounds: &mut ToolRoundState,
    ) -> Result<ToolExecutionOutput> {
        let executor = InlineToolExecutor {
            handlers: self.inline_tool_handlers.clone(),
            #[cfg(feature = "embedded-plugins")]
            plugins: self.plugins.clone(),
        };
        let context = ToolCallContext {
            agent_name: self.name.clone(),
            call: call.clone(),
        };
        self.hooks.run_before_tool(&context)?;
        let output = self
            .runtime
            .execute_tool_call_with_round_state_and_context(
                &self.tool_catalog,
                self.permission_policy.as_ref(),
                &executor,
                call,
                rounds,
                &self.permission_context(),
            )
            .await?;
        self.hooks.run_after_tool(
            &context,
            &ToolCallOutcome {
                output: output.clone(),
            },
        )?;
        Ok(output)
    }

    fn permission_context(&self) -> RuntimePermissionContext {
        RuntimePermissionContext {
            session_id: self.session_id,
            agent_id: self.profile_id.clone(),
            cwd: self.cwd.clone(),
        }
    }

    fn enabled_tool_definitions(&self) -> Vec<ToolDefinition> {
        let active_tools = active_tools_for(&self.policy_config);
        self.tool_catalog
            .definitions()
            .into_iter()
            .filter(|definition| active_tools.is_empty() || active_tools.contains(&definition.name))
            .collect()
    }

    fn model_call_context(&self, prompt: String) -> ModelCallContext {
        ModelCallContext {
            agent_name: self.name.clone(),
            provider_plugin_id: self.provider_plugin_id.clone(),
            model_id: self.model_id.clone(),
            prompt,
        }
    }

    fn turn_request_with_structured_output_and_messages(
        &self,
        prompt: String,
        structured_output: Option<bcode_model::StructuredOutputRequest>,
        messages: Vec<ModelMessage>,
    ) -> AgentTurnRequest {
        let mut request = self.turn_request_with_cancellation(prompt, CancellationToken::new());
        request.structured_output = structured_output;
        request.messages = messages;
        request
    }

    fn turn_request_with_cancellation(
        &self,
        prompt: String,
        cancellation: CancellationToken,
    ) -> AgentTurnRequest {
        AgentTurnRequest {
            provider_plugin_id: self.provider_plugin_id.clone(),
            model_id: self.model_id.clone(),
            provider_context: self.provider_context.clone(),
            system_prompt: self.system_prompt.clone(),
            messages: Vec::new(),
            prompt,
            tools: self.enabled_tool_definitions(),
            structured_output: None,
            parameters: self.parameters.clone(),
            metadata: self.metadata.clone(),
            timeout: self.timeout,
            max_tool_rounds: self.max_tool_rounds,
            cancellation,
        }
    }

    /// Create a stateful in-memory session wrapper for this agent.
    #[must_use]
    pub fn session(self) -> AgentSession {
        AgentSession::new(self, InMemorySession::new())
    }

    /// Create a stateful in-memory session wrapper from caller-managed messages.
    #[must_use]
    pub const fn session_from_messages(self, messages: Vec<ModelMessage>) -> AgentSession {
        AgentSession::new(self, InMemorySession::from_messages(messages))
    }

    /// Return the configured agent name.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Return the configured model ID.
    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.model_id
    }
}

/// Builder for [`Agent`].
#[derive(Clone)]
pub struct AgentBuilder {
    runtime: AgentRuntime,
    name: Option<String>,
    profile_id: String,
    session_id: SessionId,
    cwd: Option<PathBuf>,
    provider_plugin_id: Option<String>,
    model_id: Option<String>,
    provider_context: ProviderRequestContext,
    system_prompt: Option<String>,
    parameters: ModelParameters,
    metadata: BTreeMap<String, String>,
    timeout: Duration,
    max_tool_rounds: u32,
    tool_catalog: UnifiedToolCatalog,
    inline_tool_handlers: BTreeMap<String, InlineToolHandler>,
    hooks: AgentHooks,
    policy_config: AgentConfig,
    permission_ask_callback: Option<PermissionAskCallback>,
    custom_permission_policy: Option<Arc<dyn PermissionPolicy>>,
    #[cfg(feature = "embedded-plugins")]
    provider: Option<PluginModelProviderInvoker>,
    #[cfg(feature = "embedded-plugins")]
    plugins: Option<bcode_plugin::PluginRuntimeHost>,
}

impl fmt::Debug for AgentBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("AgentBuilder");
        debug
            .field("runtime", &self.runtime)
            .field("name", &self.name)
            .field("profile_id", &self.profile_id)
            .field("session_id", &self.session_id)
            .field("cwd", &self.cwd)
            .field("provider_plugin_id", &self.provider_plugin_id)
            .field("model_id", &self.model_id)
            .field("provider_context", &self.provider_context)
            .field("system_prompt", &self.system_prompt)
            .field("parameters", &self.parameters)
            .field("metadata", &self.metadata)
            .field("timeout", &self.timeout)
            .field("max_tool_rounds", &self.max_tool_rounds)
            .field("tool_catalog", &self.tool_catalog)
            .field(
                "inline_tool_handlers",
                &self.inline_tool_handlers.keys().collect::<Vec<_>>(),
            )
            .field("hooks", &self.hooks)
            .field("policy_config", &self.policy_config)
            .field(
                "permission_ask_callback",
                &self.permission_ask_callback.is_some(),
            )
            .field(
                "custom_permission_policy",
                &self.custom_permission_policy.is_some(),
            );
        #[cfg(feature = "embedded-plugins")]
        debug
            .field("provider", &self.provider)
            .field("plugins", &self.plugins.is_some());
        debug.finish()
    }
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self {
            runtime: AgentRuntime::new(),
            name: None,
            profile_id: bcode_agent_policy::BUILD_AGENT.to_string(),
            session_id: SessionId::default(),
            cwd: std::env::current_dir().ok(),
            provider_plugin_id: None,
            model_id: None,
            provider_context: ProviderRequestContext::default(),
            system_prompt: None,
            parameters: ModelParameters::default(),
            metadata: BTreeMap::new(),
            timeout: Duration::from_mins(2),
            max_tool_rounds: 8,
            tool_catalog: UnifiedToolCatalog::new(),
            inline_tool_handlers: BTreeMap::new(),
            hooks: AgentHooks::new(),
            policy_config: bcode_agent_policy::agent_config(
                &bcode_agent_policy::default_config(),
                bcode_agent_policy::BUILD_AGENT,
            ),
            permission_ask_callback: None,
            custom_permission_policy: None,
            #[cfg(feature = "embedded-plugins")]
            provider: None,
            #[cfg(feature = "embedded-plugins")]
            plugins: None,
        }
    }
}

impl AgentBuilder {
    /// Configure the reusable runtime used by this agent.
    #[must_use]
    pub const fn runtime(mut self, runtime: AgentRuntime) -> Self {
        self.runtime = runtime;
        self
    }

    /// Configure the embedded provider invoker for this agent.
    #[cfg(feature = "embedded-plugins")]
    #[must_use]
    pub fn provider_invoker(mut self, provider: PluginModelProviderInvoker) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Configure the embedded plugin runtime for provider and plugin-backed tool calls.
    #[cfg(feature = "embedded-plugins")]
    #[must_use]
    pub fn plugin_runtime(mut self, plugins: bcode_plugin::PluginRuntimeHost) -> Self {
        self.provider = Some(PluginModelProviderInvoker::new(plugins.clone()));
        self.plugins = Some(plugins);
        self
    }

    /// Configure a human-readable agent name.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Configure the selected model ID.
    #[must_use]
    pub fn model(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = Some(model_id.into());
        self
    }

    /// Configure a specific provider plugin ID.
    #[must_use]
    pub fn provider_plugin(mut self, provider_plugin_id: impl Into<String>) -> Self {
        self.provider_plugin_id = Some(provider_plugin_id.into());
        self
    }

    /// Configure provider request context.
    #[must_use]
    pub fn provider_context(mut self, provider_context: ProviderRequestContext) -> Self {
        self.provider_context = provider_context;
        self
    }

    /// Configure the system prompt.
    #[must_use]
    pub fn system(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(system_prompt.into());
        self
    }

    /// Configure model parameters.
    #[must_use]
    pub fn parameters(mut self, parameters: ModelParameters) -> Self {
        self.parameters = parameters;
        self
    }

    /// Add one metadata key/value pair sent to providers.
    #[must_use]
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Configure turn timeout.
    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Configure maximum tool rounds.
    #[must_use]
    pub const fn max_tool_rounds(mut self, max_tool_rounds: u32) -> Self {
        self.max_tool_rounds = max_tool_rounds;
        self
    }

    /// Register an inline SDK tool.
    ///
    /// The supplied definition is exposed to providers as a normal [`ToolDefinition`], while the
    /// handler executes in-process when the runtime routes a matching tool call.
    #[must_use]
    pub fn inline_tool<F>(mut self, definition: ToolDefinition, handler: F) -> Self
    where
        F: Fn(ToolInvocationRequest) -> std::result::Result<ToolInvocationResponse, String>
            + Send
            + Sync
            + 'static,
    {
        let name = definition.name.clone();
        self.tool_catalog.insert(RegisteredTool::inline(definition));
        self.inline_tool_handlers.insert(name, Arc::new(handler));
        self
    }

    /// Register a plugin-backed tool definition with routing metadata.
    #[must_use]
    pub fn plugin_tool(mut self, definition: ToolDefinition, plugin_id: impl Into<String>) -> Self {
        self.tool_catalog
            .insert(RegisteredTool::plugin(definition, plugin_id));
        self
    }

    /// Configure hooks for model and tool calls.
    #[must_use]
    pub fn hooks(mut self, hooks: AgentHooks) -> Self {
        self.hooks = hooks;
        self
    }

    /// Add a hook that runs before model calls.
    #[must_use]
    pub fn on_before_model<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ModelCallContext) -> Result<()> + Send + Sync + 'static,
    {
        self.hooks = self.hooks.on_before_model(hook);
        self
    }

    /// Add a hook that runs after successful model calls.
    #[must_use]
    pub fn on_after_model<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ModelCallContext, &ModelCallOutcome) -> Result<()> + Send + Sync + 'static,
    {
        self.hooks = self.hooks.on_after_model(hook);
        self
    }

    /// Add a hook that runs before tool calls.
    #[must_use]
    pub fn on_before_tool<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ToolCallContext) -> Result<()> + Send + Sync + 'static,
    {
        self.hooks = self.hooks.on_before_tool(hook);
        self
    }

    /// Add a hook that runs after successful tool calls.
    #[must_use]
    pub fn on_after_tool<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ToolCallContext, &ToolCallOutcome) -> Result<()> + Send + Sync + 'static,
    {
        self.hooks = self.hooks.on_after_tool(hook);
        self
    }

    /// Configure the active agent/profile ID used by shared policy evaluation.
    #[must_use]
    pub fn agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.profile_id = agent_id.into();
        self.policy_config = bcode_agent_policy::agent_config(
            &bcode_agent_policy::default_config(),
            &self.profile_id,
        );
        self.custom_permission_policy = None;
        self
    }

    /// Configure the session ID used by shared policy evaluation.
    #[must_use]
    pub const fn session_id(mut self, session_id: SessionId) -> Self {
        self.session_id = session_id;
        self
    }

    /// Configure the working directory used by shared path-boundary policy evaluation.
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Configure a resolved agent policy from the shared Bcode permission model.
    #[must_use]
    pub fn agent_config(mut self, config: AgentConfig) -> Self {
        self.policy_config = config;
        self.custom_permission_policy = None;
        self
    }

    /// Configure a resolved agent policy and ask callback from the shared Bcode permission model.
    #[must_use]
    pub fn agent_config_with_ask<F>(mut self, config: AgentConfig, callback: F) -> Self
    where
        F: Fn(&RuntimePermissionRequest, &EvaluateToolCallResponse) -> PermissionDecision
            + Send
            + Sync
            + 'static,
    {
        self.policy_config = config;
        self.permission_ask_callback = Some(ask_callback(callback));
        self.custom_permission_policy = None;
        self
    }

    /// Configure a multi-agent policy config from the shared Bcode permission model.
    #[must_use]
    pub fn agent_permission_config(mut self, config: &AgentPermissionConfig) -> Self {
        self.policy_config = bcode_agent_policy::agent_config(config, &self.profile_id);
        self.custom_permission_policy = None;
        self
    }

    /// Configure a multi-agent policy config and ask callback from the shared Bcode permission model.
    #[must_use]
    pub fn agent_permission_config_with_ask<F>(
        mut self,
        config: &AgentPermissionConfig,
        callback: F,
    ) -> Self
    where
        F: Fn(&RuntimePermissionRequest, &EvaluateToolCallResponse) -> PermissionDecision
            + Send
            + Sync
            + 'static,
    {
        self.policy_config = bcode_agent_policy::agent_config(config, &self.profile_id);
        self.permission_ask_callback = Some(ask_callback(callback));
        self.custom_permission_policy = None;
        self
    }

    /// Configure a callback used only when shared policy returns `ask`.
    #[must_use]
    pub fn on_permission_request<F>(mut self, callback: F) -> Self
    where
        F: Fn(&RuntimePermissionRequest, &EvaluateToolCallResponse) -> PermissionDecision
            + Send
            + Sync
            + 'static,
    {
        self.permission_ask_callback = Some(ask_callback(callback));
        self
    }

    /// Configure a custom permission policy implementation.
    #[must_use]
    pub fn custom_permission_policy<P>(mut self, policy: P) -> Self
    where
        P: PermissionPolicy + 'static,
    {
        self.custom_permission_policy = Some(Arc::new(policy));
        self
    }

    /// Discover plugin-backed tools from the configured embedded plugin runtime.
    ///
    /// # Errors
    ///
    /// Returns an error when no plugin runtime is configured or a plugin tool service fails.
    #[cfg(feature = "embedded-plugins")]
    pub async fn discover_plugin_tools(mut self) -> Result<Self> {
        let plugins = self
            .plugins
            .clone()
            .ok_or(BcodeError::MissingPluginRuntime)?;
        let registry = plugins.registry().service_registry();
        let Some(providers) = registry.providers_for(bcode_tool::TOOL_SERVICE_INTERFACE_ID) else {
            return Ok(self);
        };
        for plugin_id in providers {
            let list: bcode_tool::ToolList = plugins
                .invoke_service_json_scoped(
                    plugin_id,
                    bcode_tool::TOOL_SERVICE_INTERFACE_ID,
                    bcode_tool::OP_LIST_TOOLS,
                    &bcode_tool::ListToolsRequest::default(),
                    bcode_plugin::PluginInvocationScope::Global,
                )
                .await
                .map_err(|error| BcodeError::ToolExecution(error.to_string()))?;
            for definition in list.tools {
                self.tool_catalog
                    .insert(RegisteredTool::plugin(definition, plugin_id.clone()));
            }
        }
        Ok(self)
    }

    fn permission_policy(&self) -> Arc<dyn PermissionPolicy> {
        self.custom_permission_policy.clone().unwrap_or_else(|| {
            Arc::new(
                AgentPermissionPolicy::new(self.policy_config.clone())
                    .with_ask_callback(self.permission_ask_callback.clone()),
            )
        })
    }

    /// Build the agent.
    #[must_use]
    pub fn build(self) -> Agent {
        let permission_policy = self.permission_policy();
        Agent {
            runtime: self.runtime,
            name: self.name,
            profile_id: self.profile_id,
            session_id: self.session_id,
            cwd: self.cwd,
            provider_plugin_id: self.provider_plugin_id,
            model_id: self.model_id.unwrap_or_default(),
            provider_context: self.provider_context,
            system_prompt: self.system_prompt,
            parameters: self.parameters,
            metadata: self.metadata,
            timeout: self.timeout,
            max_tool_rounds: self.max_tool_rounds,
            tool_catalog: self.tool_catalog,
            inline_tool_handlers: self.inline_tool_handlers,
            hooks: self.hooks,
            policy_config: self.policy_config,
            permission_policy,
            #[cfg(feature = "embedded-plugins")]
            provider: self.provider,
            #[cfg(feature = "embedded-plugins")]
            plugins: self.plugins,
        }
    }
}

/// Request for text generation.
#[derive(Debug, Clone)]
pub struct GenerateTextRequest {
    /// User prompt.
    pub prompt: String,
}

/// Response from text generation.
#[derive(Debug, Clone)]
pub struct GenerateTextResponse {
    /// Generated assistant text.
    pub text: String,
    /// Runtime response containing metadata and events.
    pub runtime: AgentTurnResponse,
}

impl From<AgentTurnResponse> for GenerateTextResponse {
    fn from(runtime: AgentTurnResponse) -> Self {
        Self {
            text: runtime.text.clone(),
            runtime,
        }
    }
}
