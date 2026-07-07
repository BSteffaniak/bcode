#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! High-level Rust SDK facade for Bcode.
//!
//! This crate provides explicit, application-facing types for building AI applications with Bcode.
//! The facade is intentionally small and delegates reusable turn behavior to
//! `bcode_agent_runtime`.

#[cfg(feature = "embedded-plugins")]
use bcode_agent_runtime::RuntimeFuture;
use bcode_agent_runtime::{
    AgentRuntime, AgentTurnRequest, AgentTurnResponse, ModelProviderInvoker,
};
#[cfg(feature = "embedded-plugins")]
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, MODEL_PROVIDER_INTERFACE_ID,
    ModelTurnRequest, OP_CANCEL_TURN, OP_FINISH_TURN, OP_POLL_TURN_EVENTS, OP_START_TURN,
    PollTurnEventsRequest, PollTurnEventsResponse, StartTurnResponse,
};
use bcode_model::{ModelParameters, ProviderRequestContext};
use bcode_tool::{ToolDefinition, ToolInvocationRequest, ToolInvocationResponse};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

pub use bcode_agent_runtime::{
    AgentRuntimeEvent as AgentEvent, AgentRuntimeStream as AgentStream,
    AgentRuntimeStreamItem as AgentStreamItem, AllowAllPolicy, CancellationToken,
    PermissionDecision, PermissionPolicy, RegisteredTool, RuntimeError, ToolExecutionOutput,
    ToolExecutor, ToolRoundState, ToolSource, UnifiedToolCatalog,
};
pub use bcode_model::ToolCall;

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
    /// Tool execution failed.
    #[error("tool execution error: {0}")]
    ToolExecution(String),
}

type InlineToolHandler = Arc<
    dyn Fn(ToolInvocationRequest) -> std::result::Result<ToolInvocationResponse, String>
        + Send
        + Sync,
>;

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

/// Configured agent facade.
#[derive(Clone)]
pub struct Agent {
    runtime: AgentRuntime,
    name: Option<String>,
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
            );
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
        let response = self
            .runtime
            .run_text_turn(provider, self.turn_request(prompt.into()))
            .await?;
        Ok(response.into())
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
        Ok(self
            .runtime
            .run_streaming_text_turn(provider, self.turn_request(prompt.into())))
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
        Ok(self
            .runtime
            .execute_tool_call(&self.tool_catalog, &AllowAllPolicy, &executor, call)
            .await?)
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
        Ok(self
            .runtime
            .execute_tool_call_with_round_state(
                &self.tool_catalog,
                &AllowAllPolicy,
                &executor,
                call,
                rounds,
            )
            .await?)
    }

    fn turn_request(&self, prompt: String) -> AgentTurnRequest {
        self.turn_request_with_cancellation(prompt, CancellationToken::new())
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
            prompt,
            parameters: self.parameters.clone(),
            metadata: self.metadata.clone(),
            timeout: self.timeout,
            max_tool_rounds: self.max_tool_rounds,
            cancellation,
        }
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

    /// Build the agent.
    #[must_use]
    pub fn build(self) -> Agent {
        Agent {
            runtime: self.runtime,
            name: self.name,
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
