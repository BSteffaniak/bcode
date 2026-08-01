#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

#[cfg(feature = "static-bundled")]
mod web_search_tui;

#[path = "providers/exa.rs"]
pub(crate) mod exa;

use bcode_model_provider_runtime::ProviderRuntime;
use bcode_plugin_sdk::prelude::*;
use bcode_provider_auth_models::{
    AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION, AuthMethodContribution, AuthProviderContribution,
    AuthSecretField, AuthSecretValidation,
};
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolArtifact,
    ToolDefinition, ToolInvocationLifecycleEvent, ToolInvocationLifecycleStage,
    ToolInvocationRequest, ToolInvocationResponse, ToolInvocationResult,
    ToolInvocationServiceRequest, ToolInvocationServiceResolution, ToolList,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::time::Duration;
use thiserror::Error;

const MODEL_PROVIDER_SERVICE_INTERFACE: &str = "bcode.model-provider/v1";
const MODEL_NATIVE_WEB_SEARCH_OPERATION: &str = "native_web_search";
const DEFAULT_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_MAX_RESULTS: usize = 8;
const DEFAULT_FETCH_MAX_BYTES: usize = 256 * 1024;
const MAX_FETCH_BYTES: usize = 2 * 1024 * 1024;
const USER_AGENT: &str = concat!("Bcode/", env!("CARGO_PKG_VERSION"));
const WEB_SEARCH_PLUGIN_ID: &str = "bcode.web-search";
const EXA_PROVIDER_ID: &str = "exa";
const EXA_CREDENTIAL_ID: &str = "api_key";
const EXA_STORAGE_KEY: &str = "EXA_API_KEY";
const EXA_CANONICAL_SECRET_ID: &str = "bcode.web-search/exa/api_key";
const WEB_SEARCH_REQUEST_SCHEMA: &str = "bcode.web-search.search_request";
const WEB_FETCH_REQUEST_SCHEMA: &str = "bcode.web-search.fetch_request";
const WEB_STATUS_REQUEST_SCHEMA: &str = "bcode.web-search.status_request";
const WEB_INSPECT_REQUEST_SCHEMA: &str = "bcode.web-search.inspect_request";
const WEB_SEARCH_RESULTS_SCHEMA: &str = "bcode.web-search.search_results";
const WEB_FETCH_RESULT_SCHEMA: &str = "bcode.web-search.fetch_result";
const WEB_STATUS_SCHEMA: &str = "bcode.web-search.status";
const WEB_INSPECT_RESULT_SCHEMA: &str = "bcode.web-search.inspect_result";

#[derive(Clone)]
struct ProgressReporter {
    events: ServiceEventEmitter,
    tool_call_id: String,
    sequence: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ProgressReporter {
    fn new(events: ServiceEventEmitter, tool_call_id: String) -> Self {
        Self {
            events,
            tool_call_id,
            sequence: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    fn emit(&self, message: impl Into<String>) {
        let sequence = self
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(1);
        let event = progress_lifecycle_event(&self.tool_call_id, sequence, message.into());
        if let Ok(payload) = serde_json::to_vec(&event) {
            self.events.emit(&payload);
        }
    }
}

fn progress_lifecycle_event(
    invocation_id: &str,
    sequence: u64,
    message: String,
) -> ToolInvocationLifecycleEvent {
    ToolInvocationLifecycleEvent {
        invocation_id: invocation_id.to_owned(),
        sequence,
        stage: ToolInvocationLifecycleStage::Progress,
        message: Some(message),
        metadata: serde_json::Value::Null,
    }
}

/// web search plugin.
pub struct WebSearchPlugin {
    runtime: Result<ProviderRuntime, String>,
}

impl Default for WebSearchPlugin {
    fn default() -> Self {
        Self {
            runtime: ProviderRuntime::new().map_err(|error| error.to_string()),
        }
    }
}

impl RustPlugin for WebSearchPlugin {
    fn register_auth_providers(&mut self, registrar: AuthRegistrar) -> Result<(), PluginError> {
        registrar
            .register(&exa_auth_provider_contribution())
            .map_err(|error| PluginError::failed(format!("failed to register Exa auth: {error}")))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.interface_id.as_str() {
            TOOL_SERVICE_INTERFACE_ID => self.invoke_tool_service(&context),
            _ => ServiceResponse::error(
                "unsupported_interface",
                "unsupported web search plugin service interface",
            ),
        }
    }
}

fn exa_auth_provider_contribution() -> AuthProviderContribution {
    AuthProviderContribution {
        schema_version: AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
        provider_id: EXA_PROVIDER_ID.to_owned(),
        display_name: "Exa".to_owned(),
        methods: vec![AuthMethodContribution::SecretFields {
            method_id: "api_key".to_owned(),
            display_name: "API key".to_owned(),
            fields: vec![AuthSecretField {
                credential_id: EXA_CREDENTIAL_ID.to_owned(),
                storage_key: EXA_STORAGE_KEY.to_owned(),
                prompt: "Exa API key".to_owned(),
                optional: false,
                validation: AuthSecretValidation {
                    min_bytes: Some(1),
                    max_bytes: Some(512),
                    required_prefix: None,
                },
            }],
            supports_verification: false,
            supports_revocation: false,
        }],
    }
}

impl WebSearchPlugin {
    fn invoke_tool_service(&self, context: &NativeServiceContext) -> ServiceResponse {
        match context.request.operation.as_str() {
            OP_LIST_TOOLS => list_tools(&context.request, &context.config),
            bcode_tool::OP_PREPARE_TOOL => {
                prepare_web_tool_service_response(&context.request, &context.config)
            }
            OP_INVOKE_TOOL => self.invoke_tool(context),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported tool service operation",
            ),
        }
    }

    fn invoke_tool(&self, context: &NativeServiceContext) -> ServiceResponse {
        let invocation = match context.request.payload_json::<ToolInvocationRequest>() {
            Ok(invocation) => invocation,
            Err(error) => return invalid_request(&error),
        };
        if context.cancellation.is_cancelled() {
            return json_response(&tool_error("web tool cancelled".to_string()));
        }
        if let Some(schema) = web_request_schema(&invocation.name) {
            let mut presentation = PrimaryPresentationPublisher::with_limits_and_cancellation(
                context.events,
                &invocation.tool_call_id,
                WEB_SEARCH_PLUGIN_ID,
                schema,
                1,
                bcode_tool::ToolPresentationRetention::RetainLatest,
                context.transient_progress_limits,
                context.cancellation.clone(),
            );
            let _ = presentation.replace(&web_request_visual_payload(
                &invocation.name,
                &invocation.arguments,
            ));
        }
        let response = match invocation.name.as_str() {
            "web.search" => self.invoke_search(
                &context.config,
                &context.cancellation,
                &invocation,
                context.events,
                context.bridge.clone(),
            ),
            "web.fetch" => self.invoke_fetch(
                &context.config,
                &context.cancellation,
                &invocation,
                context.events,
            ),
            "web.status" => invoke_status(&context.config),
            "web.inspect" => invoke_inspect(&invocation),
            _ => ToolInvocationResponse {
                output: format!("unsupported web tool: {}", invocation.name),
                is_error: true,
                content: Vec::new(),
                full_output: None,
                result: None,
            },
        };
        json_response(&response)
    }

    fn invoke_search(
        &self,
        config: &bcode_plugin_sdk::PluginConfigContext,
        cancellation: &bcode_plugin_sdk::ServiceCancellation,
        invocation: &ToolInvocationRequest,
        events: ServiceEventEmitter,
        bridge: ServiceBridge,
    ) -> ToolInvocationResponse {
        let request = match serde_json::from_value::<SearchRequest>(invocation.arguments.clone()) {
            Ok(request) => request,
            Err(error) => return tool_error(error.to_string()),
        };
        let plugin_config = match config.typed_or_default::<WebSearchConfig>() {
            Ok(config) => config,
            Err(error) => return tool_error(error.to_string()),
        };
        let runtime = match &self.runtime {
            Ok(runtime) => runtime,
            Err(error) => return tool_error(format!("web runtime unavailable: {error}")),
        };
        let credentials = ProviderCredentials::new(config);
        let progress = ProgressReporter::new(events, invocation.tool_call_id.clone());
        progress.emit(format!("search: query {}", request.query));
        match runtime.block_on(run_cancellable(
            search_async(
                request,
                plugin_config,
                credentials,
                Some(progress),
                bridge,
                invocation.tool_call_id.clone(),
                invocation.preparation_descriptor.clone(),
            ),
            cancellation.clone(),
        )) {
            Ok(Ok(response)) => search_tool_response(&response, &invocation.tool_call_id),
            Ok(Err(error)) => tool_error(error.to_string()),
            Err(error) => tool_error(error.to_string()),
        }
    }

    fn invoke_fetch(
        &self,
        config: &bcode_plugin_sdk::PluginConfigContext,
        cancellation: &bcode_plugin_sdk::ServiceCancellation,
        invocation: &ToolInvocationRequest,
        events: ServiceEventEmitter,
    ) -> ToolInvocationResponse {
        let request = match serde_json::from_value::<FetchRequest>(invocation.arguments.clone()) {
            Ok(request) => request,
            Err(error) => return tool_error(error.to_string()),
        };
        let plugin_config = match config.typed_or_default::<WebSearchConfig>() {
            Ok(config) => config,
            Err(error) => return tool_error(error.to_string()),
        };
        let runtime = match &self.runtime {
            Ok(runtime) => runtime,
            Err(error) => return tool_error(format!("web runtime unavailable: {error}")),
        };
        let progress = ProgressReporter::new(events, invocation.tool_call_id.clone());
        progress.emit(format!("fetch: requesting {}", request.url));
        match runtime.block_on(run_cancellable(
            fetch_async(request, plugin_config, Some(progress)),
            cancellation.clone(),
        )) {
            Ok(Ok(response)) => json_tool_response_with_artifact(
                &response,
                &invocation.tool_call_id,
                "fetch",
                WEB_FETCH_RESULT_SCHEMA,
                "Fetched page",
            ),
            Ok(Err(error)) => tool_error(error.to_string()),
            Err(error) => tool_error(error.to_string()),
        }
    }
}

async fn run_cancellable<T>(
    future: impl std::future::Future<Output = Result<T, WebError>>,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
) -> Result<T, WebError> {
    tokio::select! {
        result = future => result,
        () = wait_for_cancellation(cancellation) => Err(WebError::Cancelled),
    }
}

async fn wait_for_cancellation(cancellation: bcode_plugin_sdk::ServiceCancellation) {
    while !cancellation.is_cancelled() {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

fn invoke_status(config: &bcode_plugin_sdk::PluginConfigContext) -> ToolInvocationResponse {
    let plugin_config = match config.typed_or_default::<WebSearchConfig>() {
        Ok(config) => config,
        Err(error) => return tool_error(error.to_string()),
    };
    let credentials = ProviderCredentials::new(config);
    json_tool_response_with_artifact(
        &status_response(&plugin_config, &credentials),
        "web-status",
        "status",
        WEB_STATUS_SCHEMA,
        "Web capabilities",
    )
}

fn invoke_inspect(invocation: &ToolInvocationRequest) -> ToolInvocationResponse {
    let request = match serde_json::from_value::<InspectRequest>(invocation.arguments.clone()) {
        Ok(request) => request,
        Err(error) => return tool_error(error.to_string()),
    };
    match inspect_url(&request.url) {
        Ok(response) => json_tool_response_with_artifact(
            &response,
            &invocation.tool_call_id,
            "inspect",
            WEB_INSPECT_RESULT_SCHEMA,
            "URL inspection",
        ),
        Err(error) => tool_error(error.to_string()),
    }
}

#[derive(Debug, Clone, Deserialize)]
struct WebSearchConfig {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default = "default_allow_best_effort_no_key")]
    allow_best_effort_no_key: bool,
    #[serde(default)]
    fetch: Option<WebFetchConfig>,
    #[serde(default)]
    model_native_available: bool,
    #[serde(default)]
    providers: WebSearchProviderConfig,
}

const fn default_allow_best_effort_no_key() -> bool {
    true
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: None,
            max_results: None,
            timeout_ms: None,
            allow_best_effort_no_key: default_allow_best_effort_no_key(),
            fetch: None,
            model_native_available: false,
            providers: WebSearchProviderConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WebFetchConfig {
    #[serde(default)]
    max_bytes: Option<usize>,
    #[serde(default)]
    fallbacks: Vec<FetchFallback>,
    #[serde(default)]
    rendered: Option<RenderedFetchConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FetchFallback {
    Plain,
    JinaReader,
    RenderedCommand,
}

impl FetchFallback {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::JinaReader => "jina_reader",
            Self::RenderedCommand => "rendered_command",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RenderedFetchConfig {
    #[serde(default)]
    command: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WebSearchProviderConfig {
    #[serde(default)]
    brave: ProviderConfig,
    #[serde(default)]
    tavily: ProviderConfig,
    #[serde(default)]
    exa: ProviderConfig,
    #[serde(default)]
    serper: ProviderConfig,
    #[serde(default)]
    serpapi: ProviderConfig,
    #[serde(default)]
    perplexity: ProviderConfig,
    #[serde(default)]
    gemini: ProviderConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProviderConfig {
    #[serde(default)]
    api_key: Option<SecretRef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
enum SecretRef {
    Env { name: String },
    Sshenv,
}

impl SecretRef {
    fn resolve_legacy(&self) -> Option<String> {
        if let Self::Env { name } = self {
            return env_value(&[name.as_str()]);
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialSource {
    ExplicitReference,
    IntegratedAuth,
    EnvironmentFallback,
}

impl CredentialSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitReference => "explicit_reference",
            Self::IntegratedAuth => "integrated_auth",
            Self::EnvironmentFallback => "environment_fallback",
        }
    }
}

#[derive(Clone)]
struct ProviderCredentials {
    secrets: std::collections::BTreeMap<String, String>,
}

impl ProviderCredentials {
    fn new(config: &bcode_plugin_sdk::PluginConfigContext) -> Self {
        Self {
            secrets: config.secrets.clone(),
        }
    }

    fn exa(&self, config: &ProviderConfig) -> Option<(&str, CredentialSource)> {
        if let Some(value) = self.secrets.get(EXA_CANONICAL_SECRET_ID) {
            let source = match config.api_key.as_ref() {
                Some(SecretRef::Env { name }) if env_value(&[name.as_str()]).is_some() => {
                    CredentialSource::ExplicitReference
                }
                Some(SecretRef::Sshenv) => CredentialSource::ExplicitReference,
                _ => CredentialSource::IntegratedAuth,
            };
            return Some((value, source));
        }
        env_value(&[EXA_STORAGE_KEY]).map(|_| ("", CredentialSource::EnvironmentFallback))
    }

    fn exa_key(&self, _config: &ProviderConfig) -> Result<String, WebError> {
        if let Some(value) = self.secrets.get(EXA_CANONICAL_SECRET_ID) {
            return Ok(value.clone());
        }
        env_value(&[EXA_STORAGE_KEY]).ok_or(WebError::MissingProvider)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct SearchRequest {
    query: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    site: Option<String>,
    #[serde(default)]
    freshness: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    safe_search: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    provider_options: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
struct SearchResponse {
    query: String,
    provider: String,
    results: Vec<SearchResult>,
    partial: bool,
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
    published: Option<String>,
    source: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct FetchRequest {
    url: String,
    #[serde(default)]
    max_bytes: Option<usize>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    render: bool,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    provider: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct FetchResponse {
    url: String,
    final_url: String,
    status: u16,
    title: Option<String>,
    content_type: Option<String>,
    text: String,
    markdown: Option<String>,
    truncated: bool,
    rendered: bool,
    fallback_used: String,
    content_format: String,
    extraction: String,
    prompt: Option<String>,
    prompt_response: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct WebStatusResponse {
    search: SearchStatus,
    fetch: FetchStatus,
}

#[derive(Debug, Clone, Serialize)]
struct InspectResponse {
    url: String,
    kind: String,
    recommended_tool: Option<String>,
    recommended_action: String,
    notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SearchStatus {
    available: bool,
    provider: Option<String>,
    quality: String,
    credential_source: Option<String>,
    credential_owner: Option<String>,
    configured_providers: Vec<String>,
    recommended: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct FetchStatus {
    available: bool,
    fallbacks: Vec<String>,
    rendered_fetch: bool,
    max_bytes: usize,
}
#[derive(Debug, Clone, Deserialize)]
struct InspectRequest {
    url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct BraveSearchResponse {
    #[serde(default)]
    web: Option<BraveWebResults>,
}

#[derive(Debug, Clone, Deserialize)]
struct BraveWebResults {
    #[serde(default)]
    results: Vec<BraveWebResult>,
}

#[derive(Debug, Clone, Deserialize)]
struct BraveWebResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    age: Option<String>,
    #[serde(default)]
    profile: Option<BraveProfile>,
}

#[derive(Debug, Clone, Deserialize)]
struct BraveProfile {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TavilySearchResponse {
    #[serde(default)]
    results: Vec<TavilyResult>,
}

#[derive(Debug, Clone, Deserialize)]
struct TavilyResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SerperSearchResponse {
    #[serde(default)]
    organic: Vec<SerperResult>,
}

#[derive(Debug, Clone, Deserialize)]
struct SerperResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SerpApiSearchResponse {
    #[serde(default)]
    organic_results: Vec<SerpApiResult>,
}

#[derive(Debug, Clone, Deserialize)]
struct SerpApiResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct PerplexitySearchResponse {
    #[serde(default)]
    choices: Vec<PerplexityChoice>,
    #[serde(default)]
    citations: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct PerplexityChoice {
    #[serde(default)]
    message: PerplexityMessage,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PerplexityMessage {
    #[serde(default)]
    content: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GeminiGenerateResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
}

#[derive(Debug, Clone, Deserialize)]
struct GeminiCandidate {
    #[serde(default)]
    content: GeminiContent,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GeminiContent {
    #[serde(default)]
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Clone, Deserialize)]
struct GeminiPart {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Error)]
enum WebError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error(
        "no web search provider configured; set BCODE_WEB_SEARCH_PROVIDER or a supported provider API key"
    )]
    MissingProvider,
    #[error("network request failed: {0}")]
    Network(#[from] reqwest::Error),
    #[error("provider returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("response decode failed: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("tool cancelled")]
    Cancelled,
}

async fn search_async(
    request: SearchRequest,
    config: WebSearchConfig,
    credentials: ProviderCredentials,
    progress: Option<ProgressReporter>,
    bridge: ServiceBridge,
    invocation_id: String,
    preparation_descriptor: serde_json::Value,
) -> Result<SearchResponse, WebError> {
    validate_non_empty("query", &request.query)?;
    let provider = search_provider(request.provider.as_deref(), &config, &credentials)?;
    if request.provider_options.is_some() && provider != "exa" {
        return Err(WebError::InvalidRequest(
            "provider_options are currently supported only when provider is exa".to_string(),
        ));
    }
    if let Some(progress) = &progress {
        progress.emit(format!("search: provider selected: {provider}"));
    }
    let response = match provider.as_str() {
        "brave" => search_brave(request, &config).await,
        "tavily" => search_tavily(request, &config).await,
        "exa" => search_exa(request, &config, &credentials).await,
        "perplexity" | "pplx" => search_perplexity(request, &config).await,
        "gemini" | "google_gemini" => search_gemini(request, &config).await,
        "serper" => search_serper(request, &config).await,
        "serpapi" | "serp_api" => search_serpapi(request, &config).await,
        "model_native" => {
            search_model_native(&request, &bridge, &invocation_id, &preparation_descriptor)
        }
        "duckduckgo_html" | "duckduckgo" | "ddg" => search_duckduckgo_html(request, &config).await,
        _ => Err(WebError::InvalidRequest(format!(
            "unsupported web search provider: {provider}"
        ))),
    }?;
    if let Some(progress) = &progress {
        progress.emit(format!(
            "search: provider {provider} returned {} results",
            response.results.len()
        ));
    }
    Ok(response)
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct WebToolPreparationDescriptor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_provider_route_id: Option<String>,
}

fn web_policy_operation(
    request: &bcode_tool::ToolPreparationRequest,
    definition: &ToolDefinition,
) -> Result<bcode_plugin_sdk::ToolPolicyPreparation, String> {
    let operation = match definition.name.as_str() {
        "web.fetch" => bcode_plugin_sdk::ToolPolicyOperation::Web {
            url: request
                .invocation
                .arguments
                .get("url")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string),
        },
        "web.search" | "web.status" | "web.inspect" => {
            bcode_plugin_sdk::ToolPolicyOperation::ReadOnly
        }
        name => return Err(format!("unsupported web policy operation: {name}")),
    };
    let identity = if definition.name == "web.fetch" {
        bcode_plugin_sdk::ToolPolicyIdentity {
            aliases: vec!["web".to_string()],
            compatibility_aliases: Vec::new(),
            capabilities: Vec::new(),
            permission_category: Some("web".to_string()),
        }
    } else {
        bcode_plugin_sdk::ToolPolicyIdentity::default()
    };
    Ok(
        bcode_plugin_sdk::ToolPolicyPreparation::new(definition.name == "web.fetch", operation)
            .with_identity(identity),
    )
}

fn prepare_web_tool_service_response(
    request: &ServiceRequest,
    config: &bcode_plugin_sdk::PluginConfigContext,
) -> ServiceResponse {
    let preparation_request = match request.payload_json::<bcode_tool::ToolPreparationRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let mut response = match prepare_tool_from_definitions(
        request,
        web_tool_definitions(config),
        web_policy_operation,
    ) {
        Ok(response) => response,
        Err(message) => return ServiceResponse::error("invalid_preparation", message),
    };
    if preparation_request.invocation.tool_name == "web.search" {
        let route_id = preparation_request
            .host_context
            .iter()
            .filter(|entry| {
                entry.schema == bcode_tool::TOOL_INVOCATION_SERVICE_ROUTES_SCHEMA
                    && entry.schema_version == 1
            })
            .find_map(|entry| {
                serde_json::from_value::<Vec<bcode_tool::ToolInvocationServiceRoute>>(
                    entry.payload.clone(),
                )
                .ok()
            })
            .and_then(|routes| {
                routes.into_iter().find(|route| {
                    route.interface_id == MODEL_PROVIDER_SERVICE_INTERFACE
                        && route
                            .operations
                            .iter()
                            .any(|operation| operation == MODEL_NATIVE_WEB_SEARCH_OPERATION)
                })
            })
            .map(|route| route.route_id);
        response.descriptor = serde_json::to_value(WebToolPreparationDescriptor {
            model_provider_route_id: route_id,
        })
        .unwrap_or(serde_json::Value::Null);
    }
    json_response(&response)
}

fn search_model_native(
    request: &SearchRequest,
    bridge: &ServiceBridge,
    invocation_id: &str,
    preparation_descriptor: &serde_json::Value,
) -> Result<SearchResponse, WebError> {
    let route_id =
        serde_json::from_value::<WebToolPreparationDescriptor>(preparation_descriptor.clone())?
            .model_provider_route_id
            .ok_or_else(|| {
                WebError::InvalidRequest(
                    "model-native search is not supported by this host".to_owned(),
                )
            })?;
    let query = request.query.clone();
    let response = bridge
        .request(&ServiceBridgeRequest::InvokeService(
            ToolInvocationServiceRequest {
                invocation_id: invocation_id.to_string(),
                request_id: format!("{invocation_id}-model-native-search"),
                route_id: Some(route_id),
                interface_id: MODEL_PROVIDER_SERVICE_INTERFACE.to_string(),
                operation: MODEL_NATIVE_WEB_SEARCH_OPERATION.to_string(),
                payload: serde_json::json!({
                    "query": request.query,
                    "max_results": request.max_results,
                    "site": request.site,
                    "freshness": request.freshness,
                    "region": request.region,
                    "safe_search": request.safe_search,
                }),
            },
        ))
        .map_err(|error| WebError::InvalidRequest(error.to_string()))?;
    let ServiceBridgeResponse::Service(response) = response else {
        return Err(WebError::InvalidRequest(
            "model-native search returned unexpected bridge response".to_string(),
        ));
    };
    let payload = match response {
        ToolInvocationServiceResolution::Responded { payload } => payload,
        ToolInvocationServiceResolution::Cancelled => return Err(WebError::Cancelled),
        ToolInvocationServiceResolution::Unsupported => {
            return Err(WebError::InvalidRequest(
                "model-native search is not supported by this host".to_string(),
            ));
        }
        ToolInvocationServiceResolution::Failed { code, message } => {
            return Err(WebError::InvalidRequest(format!(
                "model-native search failed ({code}): {message}"
            )));
        }
    };
    let response = serde_json::from_value::<ModelNativeSearchResponse>(payload)?;
    Ok(SearchResponse {
        query,
        provider: response.provider,
        results: response.results,
        partial: response.partial,
        message: response.message,
    })
}

#[derive(Debug, Deserialize)]
struct ModelNativeSearchResponse {
    provider: String,
    #[serde(default)]
    results: Vec<SearchResult>,
    #[serde(default)]
    partial: bool,
    #[serde(default)]
    message: Option<String>,
}

async fn search_brave(
    request: SearchRequest,
    config: &WebSearchConfig,
) -> Result<SearchResponse, WebError> {
    let api_key = provider_key(
        &config.providers.brave,
        &["BCODE_WEB_SEARCH_API_KEY", "BRAVE_SEARCH_API_KEY"],
    )?;
    let max_results = max_results(&request, config);
    let query = scoped_query(&request);
    let client = client(request.timeout_ms.or(config.timeout_ms))?;
    let mut builder = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("Accept", "application/json")
        .header("X-Subscription-Token", api_key)
        .query(&[("q", query.as_str()), ("count", &max_results.to_string())]);
    if let Some(region) = request.region.as_deref() {
        builder = builder.query(&[("country", region)]);
    }
    if let Some(freshness) = request.freshness.as_deref() {
        builder = builder.query(&[("freshness", freshness)]);
    }
    if let Some(safe_search) = request.safe_search.as_deref() {
        builder = builder.query(&[("safesearch", safe_search)]);
    }
    let body = checked_text(builder.send().await?).await?;
    let decoded = serde_json::from_str::<BraveSearchResponse>(&body)?;
    let results = decoded
        .web
        .map(|web| web.results)
        .unwrap_or_default()
        .into_iter()
        .filter(|result| !result.url.is_empty())
        .take(max_results)
        .map(|result| SearchResult {
            title: html_text(&result.title),
            url: result.url,
            snippet: html_text(&result.description),
            published: result.age,
            source: result.profile.and_then(|profile| profile.name),
        })
        .collect();
    Ok(search_response(request.query, "brave", results))
}

async fn search_tavily(
    request: SearchRequest,
    config: &WebSearchConfig,
) -> Result<SearchResponse, WebError> {
    let api_key = provider_key(&config.providers.tavily, &["TAVILY_API_KEY"])?;
    let max_results = max_results(&request, config);
    let client = client(request.timeout_ms.or(config.timeout_ms))?;
    let body = json!({
        "api_key": api_key,
        "query": scoped_query(&request),
        "max_results": max_results,
        "search_depth": "basic"
    });
    let text = checked_text(
        client
            .post("https://api.tavily.com/search")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await?,
    )
    .await?;
    let decoded = serde_json::from_str::<TavilySearchResponse>(&text)?;
    let results = decoded
        .results
        .into_iter()
        .filter(|result| !result.url.is_empty())
        .take(max_results)
        .map(|result| SearchResult {
            title: html_text(&result.title),
            url: result.url,
            snippet: html_text(&result.content),
            published: None,
            source: Some("tavily".to_string()),
        })
        .collect();
    Ok(search_response(request.query, "tavily", results))
}
async fn search_exa(
    request: SearchRequest,
    config: &WebSearchConfig,
    credentials: &ProviderCredentials,
) -> Result<SearchResponse, WebError> {
    let api_key = credentials.exa_key(&config.providers.exa)?;
    let max_results = max_results(&request, config);
    let client = client(request.timeout_ms.or(config.timeout_ms))?;
    let results = exa::search(
        &client,
        &api_key,
        exa::SearchInput {
            query: &request.query,
            max_results,
            site: request.site.as_deref(),
            freshness: request.freshness.as_deref(),
            region: request.region.as_deref(),
            safe_search: request.safe_search.as_deref(),
            provider_options: request.provider_options,
        },
    )
    .await?
    .into_iter()
    .map(|result| SearchResult {
        title: result.title,
        url: result.url,
        snippet: result.snippet,
        published: result.published,
        source: Some("exa".to_string()),
    })
    .collect();
    Ok(search_response(request.query, "exa", results))
}

async fn search_perplexity(
    request: SearchRequest,
    config: &WebSearchConfig,
) -> Result<SearchResponse, WebError> {
    let api_key = provider_key(
        &config.providers.perplexity,
        &["PERPLEXITY_API_KEY", "PPLX_API_KEY"],
    )?;
    let max_results = max_results(&request, config);
    let client = client(request.timeout_ms.or(config.timeout_ms))?;
    let body = json!({
        "model": "sonar",
        "messages": [
            { "role": "system", "content": "Search the web and return concise cited results." },
            { "role": "user", "content": scoped_query(&request) }
        ],
        "return_citations": true
    });
    let text = checked_text(
        client
            .post("https://api.perplexity.ai/chat/completions")
            .header("Accept", "application/json")
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await?,
    )
    .await?;
    let decoded = serde_json::from_str::<PerplexitySearchResponse>(&text)?;
    let content = decoded
        .choices
        .first()
        .map(|choice| choice.message.content.clone())
        .unwrap_or_default();
    let mut results: Vec<SearchResult> = decoded
        .citations
        .into_iter()
        .filter(|url| !url.is_empty())
        .take(max_results)
        .map(|url| SearchResult {
            title: url.clone(),
            url,
            snippet: content.clone(),
            published: None,
            source: Some("perplexity".to_string()),
        })
        .collect();
    if results.is_empty() && !content.trim().is_empty() {
        results.push(SearchResult {
            title: format!("Perplexity answer for {}", request.query),
            url: String::new(),
            snippet: content,
            published: None,
            source: Some("perplexity".to_string()),
        });
    }
    Ok(search_response(request.query, "perplexity", results))
}

async fn search_gemini(
    request: SearchRequest,
    config: &WebSearchConfig,
) -> Result<SearchResponse, WebError> {
    let api_key = provider_key(
        &config.providers.gemini,
        &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
    )?;
    let max_results = max_results(&request, config);
    let client = client(request.timeout_ms.or(config.timeout_ms))?;
    let prompt = format!(
        "Search the web for this query and return up to {max_results} concise results with URLs and snippets:\n{}",
        scoped_query(&request)
    );
    let body = json!({
        "contents": [{ "parts": [{ "text": prompt }] }],
        "tools": [{ "google_search": {} }]
    });
    let text = checked_text(
        client
            .post("https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent")
            .query(&[("key", api_key.as_str())])
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await?,
    )
    .await?;
    let decoded = serde_json::from_str::<GeminiGenerateResponse>(&text)?;
    let content = gemini_text(&decoded);
    let results = urls_from_text(&content)
        .into_iter()
        .take(max_results)
        .map(|url| SearchResult {
            title: url.clone(),
            url,
            snippet: content.clone(),
            published: None,
            source: Some("gemini".to_string()),
        })
        .collect::<Vec<_>>();
    let results = if results.is_empty() && !content.trim().is_empty() {
        vec![SearchResult {
            title: format!("Gemini answer for {}", request.query),
            url: String::new(),
            snippet: content,
            published: None,
            source: Some("gemini".to_string()),
        }]
    } else {
        results
    };
    Ok(search_response(request.query, "gemini", results))
}

async fn search_serper(
    request: SearchRequest,
    config: &WebSearchConfig,
) -> Result<SearchResponse, WebError> {
    let api_key = provider_key(&config.providers.serper, &["SERPER_API_KEY"])?;
    let max_results = max_results(&request, config);
    let client = client(request.timeout_ms.or(config.timeout_ms))?;
    let body = json!({ "q": scoped_query(&request), "num": max_results });
    let text = checked_text(
        client
            .post("https://google.serper.dev/search")
            .header("Accept", "application/json")
            .header("X-API-KEY", api_key)
            .json(&body)
            .send()
            .await?,
    )
    .await?;
    let decoded = serde_json::from_str::<SerperSearchResponse>(&text)?;
    let results = decoded
        .organic
        .into_iter()
        .filter(|result| !result.link.is_empty())
        .take(max_results)
        .map(|result| SearchResult {
            title: html_text(&result.title),
            url: result.link,
            snippet: html_text(&result.snippet),
            published: result.date,
            source: result.source,
        })
        .collect();
    Ok(search_response(request.query, "serper", results))
}

async fn search_serpapi(
    request: SearchRequest,
    config: &WebSearchConfig,
) -> Result<SearchResponse, WebError> {
    let api_key = provider_key(&config.providers.serpapi, &["SERPAPI_API_KEY"])?;
    let max_results = max_results(&request, config);
    let client = client(request.timeout_ms.or(config.timeout_ms))?;
    let text = checked_text(
        client
            .get("https://serpapi.com/search.json")
            .query(&[
                ("engine", "google"),
                ("q", scoped_query(&request).as_str()),
                ("api_key", api_key.as_str()),
                ("num", max_results.to_string().as_str()),
            ])
            .send()
            .await?,
    )
    .await?;
    let decoded = serde_json::from_str::<SerpApiSearchResponse>(&text)?;
    let results = decoded
        .organic_results
        .into_iter()
        .filter(|result| !result.link.is_empty())
        .take(max_results)
        .map(|result| SearchResult {
            title: html_text(&result.title),
            url: result.link,
            snippet: html_text(&result.snippet),
            published: result.date,
            source: result.source,
        })
        .collect();
    Ok(search_response(request.query, "serpapi", results))
}

async fn search_duckduckgo_html(
    request: SearchRequest,
    config: &WebSearchConfig,
) -> Result<SearchResponse, WebError> {
    let max_results = max_results(&request, config);
    let client = client(request.timeout_ms.or(config.timeout_ms))?;
    let body = checked_text(
        client
            .get("https://html.duckduckgo.com/html/")
            .query(&[("q", scoped_query(&request).as_str())])
            .send()
            .await?,
    )
    .await?;
    let results = parse_duckduckgo_html_results(&body)
        .into_iter()
        .take(max_results)
        .collect();
    let mut response = search_response(request.query, "duckduckgo_html", results);
    response.message = Some(
        "No configured API search provider was found; using best-effort DuckDuckGo HTML search."
            .to_string(),
    );
    Ok(response)
}

fn parse_duckduckgo_html_results(body: &str) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut remaining = body;
    while let Some(anchor_start) = remaining.find("result__a") {
        remaining = &remaining[anchor_start..];
        let Some(href_key) = remaining.find("href=\"") else {
            break;
        };
        let href_start = href_key + "href=\"".len();
        let Some(href_end) = remaining[href_start..].find('"') else {
            break;
        };
        let url = html_text(&remaining[href_start..href_start + href_end]);
        let Some(title_start) = remaining[href_start + href_end..].find('>') else {
            break;
        };
        let title_start = href_start + href_end + title_start + 1;
        let Some(title_end) = remaining[title_start..].find("</a>") else {
            break;
        };
        let title = html_text(&remaining[title_start..title_start + title_end]);
        let snippet = extract_duckduckgo_snippet(remaining).unwrap_or_default();
        if !url.is_empty() && !title.is_empty() {
            results.push(SearchResult {
                title,
                url: decode_duckduckgo_redirect(&url),
                snippet,
                published: None,
                source: Some("DuckDuckGo HTML".to_string()),
            });
        }
        remaining = &remaining[title_start + title_end..];
    }
    results
}

fn extract_duckduckgo_snippet(block: &str) -> Option<String> {
    let start = block.find("result__snippet")?;
    let block = &block[start..];
    let text_start = block.find('>')? + 1;
    let text_end = block[text_start..]
        .find("</a>")
        .or_else(|| block[text_start..].find("</div>"))?;
    Some(html_text(&block[text_start..text_start + text_end]))
}

fn decode_duckduckgo_redirect(url: &str) -> String {
    let Some(query_start) = url.find("uddg=") else {
        return url.to_string();
    };
    let encoded = &url[query_start + "uddg=".len()..];
    let encoded = encoded.split('&').next().unwrap_or(encoded);
    percent_decode(encoded)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[index + 1..index + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            output.push(byte);
            index += 3;
        } else if bytes[index] == b'+' {
            output.push(b' ');
            index += 1;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).to_string()
}

async fn fetch_async(
    request: FetchRequest,
    config: WebSearchConfig,
    progress: Option<ProgressReporter>,
) -> Result<FetchResponse, WebError> {
    validate_url(&request.url)?;
    if request.render {
        if let Some(progress) = &progress {
            progress.emit("fetch: using rendered fetch adapter");
        }
        let mut response = fetch_rendered(&request, &config)?;
        apply_prompt_extraction(&mut response, &request, &config).await?;
        return Ok(response);
    }
    let fallbacks = fetch_fallbacks(&config);
    let plain_result = fetch_plain_async(&request, &config, progress.clone()).await;
    let mut response = if should_try_jina(&fallbacks, &plain_result) {
        if let Some(progress) = &progress {
            progress.emit("fetch: trying Jina reader fallback");
        }
        match fetch_jina_reader_async(&request, &config, progress.clone()).await {
            Ok(response) => response,
            Err(_) => plain_result?,
        }
    } else {
        plain_result?
    };
    apply_prompt_extraction(&mut response, &request, &config).await?;
    if let Some(progress) = &progress {
        progress.emit(format!(
            "fetch: extracted {} bytes via {}",
            response.text.len(),
            response.fallback_used
        ));
    }
    Ok(response)
}

async fn apply_prompt_extraction(
    response: &mut FetchResponse,
    request: &FetchRequest,
    config: &WebSearchConfig,
) -> Result<(), WebError> {
    let Some(prompt) = request
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    let provider = fetch_extraction_provider(request, config);
    let extracted = match provider.as_str() {
        "perplexity" | "pplx" => extract_with_perplexity(prompt, response, request, config).await?,
        "gemini" | "google_gemini" => {
            extract_with_gemini(prompt, response, request, config).await?
        }
        "none" | "content" => prompt_response(request, &response.text).unwrap_or_default(),
        _ => {
            return Err(WebError::InvalidRequest(format!(
                "unsupported prompted fetch provider: {provider}"
            )));
        }
    };
    response.prompt_response = Some(extracted);
    response.extraction = format!("{}+prompt_{provider}", response.extraction);
    Ok(())
}

fn fetch_extraction_provider(request: &FetchRequest, config: &WebSearchConfig) -> String {
    let provider = request
        .provider
        .clone()
        .or_else(|| env_value(&["BCODE_WEB_FETCH_PROVIDER"]))
        .unwrap_or_else(|| "auto".to_string())
        .trim()
        .to_ascii_lowercase();
    if provider != "auto" {
        return provider;
    }
    if provider_key(
        &config.providers.gemini,
        &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
    )
    .is_ok()
    {
        return "gemini".to_string();
    }
    if provider_key(
        &config.providers.perplexity,
        &["PERPLEXITY_API_KEY", "PPLX_API_KEY"],
    )
    .is_ok()
    {
        return "perplexity".to_string();
    }
    "content".to_string()
}

async fn extract_with_perplexity(
    prompt: &str,
    response: &FetchResponse,
    request: &FetchRequest,
    config: &WebSearchConfig,
) -> Result<String, WebError> {
    let api_key = provider_key(
        &config.providers.perplexity,
        &["PERPLEXITY_API_KEY", "PPLX_API_KEY"],
    )?;
    let client = client(request.timeout_ms.or(config.timeout_ms))?;
    let content = bounded_prompt_content(&response.text);
    let body = json!({
        "model": "sonar",
        "messages": [
            { "role": "system", "content": "Answer the user's extraction prompt using only the provided fetched web content. Cite the source URL when useful." },
            { "role": "user", "content": format!("URL: {}\nTitle: {}\n\nPrompt: {}\n\nFetched content:\n{}", response.final_url, response.title.as_deref().unwrap_or(""), prompt, content) }
        ]
    });
    let text = checked_text(
        client
            .post("https://api.perplexity.ai/chat/completions")
            .header("Accept", "application/json")
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await?,
    )
    .await?;
    let decoded = serde_json::from_str::<PerplexitySearchResponse>(&text)?;
    Ok(decoded
        .choices
        .first()
        .map(|choice| choice.message.content.clone())
        .unwrap_or_default())
}

async fn extract_with_gemini(
    prompt: &str,
    response: &FetchResponse,
    request: &FetchRequest,
    config: &WebSearchConfig,
) -> Result<String, WebError> {
    let api_key = provider_key(
        &config.providers.gemini,
        &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
    )?;
    let client = client(request.timeout_ms.or(config.timeout_ms))?;
    let content = bounded_prompt_content(&response.text);
    let body = json!({
        "contents": [{
            "parts": [{
                "text": format!("Use only the fetched content below to answer the extraction prompt.\n\nURL: {}\nTitle: {}\nPrompt: {}\n\nFetched content:\n{}", response.final_url, response.title.as_deref().unwrap_or(""), prompt, content)
            }]
        }]
    });
    let text = checked_text(
        client
            .post("https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent")
            .query(&[("key", api_key.as_str())])
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await?,
    )
    .await?;
    let decoded = serde_json::from_str::<GeminiGenerateResponse>(&text)?;
    Ok(gemini_text(&decoded))
}

fn bounded_prompt_content(text: &str) -> String {
    const MAX_PROMPT_CONTENT_CHARS: usize = 40_000;
    if text.chars().count() <= MAX_PROMPT_CONTENT_CHARS {
        return text.to_string();
    }
    let mut output = truncate_chars(text, MAX_PROMPT_CONTENT_CHARS);
    output.push_str("\n\n[truncated]");
    output
}

async fn fetch_plain_async(
    request: &FetchRequest,
    config: &WebSearchConfig,
    progress: Option<ProgressReporter>,
) -> Result<FetchResponse, WebError> {
    let max_bytes = request
        .max_bytes
        .or_else(|| config.fetch.as_ref().and_then(|fetch| fetch.max_bytes))
        .unwrap_or(DEFAULT_FETCH_MAX_BYTES)
        .clamp(1, MAX_FETCH_BYTES);
    let client = client(request.timeout_ms.or(config.timeout_ms))?;
    let response = client.get(&request.url).send().await?;
    let status = response.status();
    let final_url = response.url().to_string();
    if let Some(progress) = &progress {
        progress.emit(format!("fetch: response {status} from {final_url}"));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    let body = response.bytes().await?;
    if let Some(progress) = &progress {
        progress.emit(format!("fetch: received {} bytes", body.len()));
    }
    let truncated = body.len() > max_bytes;
    let bytes = &body[..body.len().min(max_bytes)];
    let raw = String::from_utf8_lossy(bytes);
    let (title, text, markdown) = if content_type
        .as_deref()
        .is_some_and(|value| value.to_ascii_lowercase().contains("html"))
    {
        html_document_text(&raw)
    } else {
        let text = raw.into_owned();
        (plain_title(&text), text, None)
    };
    let prompt_response = prompt_response(request, &text);
    Ok(FetchResponse {
        url: request.url.clone(),
        final_url,
        status: status.as_u16(),
        title,
        content_type,
        text,
        markdown,
        truncated,
        rendered: false,
        fallback_used: "plain".to_string(),
        content_format: "markdown".to_string(),
        extraction: "bcode_html".to_string(),
        prompt: request.prompt.clone(),
        prompt_response,
    })
}

async fn fetch_jina_reader_async(
    request: &FetchRequest,
    config: &WebSearchConfig,
    progress: Option<ProgressReporter>,
) -> Result<FetchResponse, WebError> {
    let max_bytes = request
        .max_bytes
        .or_else(|| config.fetch.as_ref().and_then(|fetch| fetch.max_bytes))
        .unwrap_or(DEFAULT_FETCH_MAX_BYTES)
        .clamp(1, MAX_FETCH_BYTES);
    let jina_url = jina_reader_url(&request.url);
    let client = client(request.timeout_ms.or(config.timeout_ms))?;
    let response = client.get(&jina_url).send().await?;
    let status = response.status();
    let final_url = response.url().to_string();
    if let Some(progress) = &progress {
        progress.emit(format!("fetch: Jina response {status} from {final_url}"));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    let body = response.bytes().await?;
    if let Some(progress) = &progress {
        progress.emit(format!("fetch: received {} bytes", body.len()));
    }
    let truncated = body.len() > max_bytes;
    let text = String::from_utf8_lossy(&body[..body.len().min(max_bytes)]).to_string();
    let prompt_response = prompt_response(request, &text);
    Ok(FetchResponse {
        url: request.url.clone(),
        final_url,
        status: status.as_u16(),
        title: plain_title(&text),
        content_type,
        markdown: Some(text.clone()),
        text,
        truncated,
        rendered: false,
        fallback_used: "jina_reader".to_string(),
        content_format: "markdown".to_string(),
        extraction: "jina_reader".to_string(),
        prompt: request.prompt.clone(),
        prompt_response,
    })
}

fn jina_reader_url(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.starts_with("https://r.jina.ai/http://") {
        return trimmed.to_string();
    }
    format!("https://r.jina.ai/http://{trimmed}")
}

fn should_try_jina(
    fallbacks: &[FetchFallback],
    plain_result: &Result<FetchResponse, WebError>,
) -> bool {
    if !fallbacks.contains(&FetchFallback::JinaReader) {
        return false;
    }
    plain_result.as_ref().map_or(true, |response| {
        response.status == 401
            || response.status == 403
            || response.status == 429
            || response.text.len() < 200
    })
}

fn prompt_response(request: &FetchRequest, _text: &str) -> Option<String> {
    let prompt = request.prompt.as_deref()?.trim();
    if prompt.is_empty() {
        return None;
    }
    let provider = request.provider.as_deref().unwrap_or("auto");
    Some(format!(
        "Prompted extraction requested via provider '{provider}': {prompt}\n\nNo configured provider-backed extraction is available in web.fetch yet; use the returned page text/markdown to answer the prompt."
    ))
}

fn fetch_fallbacks(config: &WebSearchConfig) -> Vec<FetchFallback> {
    let configured = config
        .fetch
        .as_ref()
        .map(|fetch| fetch.fallbacks.clone())
        .unwrap_or_default();
    if configured.is_empty() {
        vec![FetchFallback::Plain, FetchFallback::JinaReader]
    } else {
        configured
    }
}

fn fetch_rendered(
    request: &FetchRequest,
    config: &WebSearchConfig,
) -> Result<FetchResponse, WebError> {
    let command = config
        .fetch
        .as_ref()
        .and_then(|fetch| fetch.rendered.as_ref())
        .and_then(|rendered| rendered.command.clone())
        .or_else(|| env_value(&["BCODE_WEB_RENDER_COMMAND"]))
        .ok_or_else(|| {
            WebError::InvalidRequest(
                "rendered fetch requires BCODE_WEB_RENDER_COMMAND or web_search.fetch.rendered.command"
                    .to_string(),
            )
        })?;
    let output = std::process::Command::new(command)
        .arg(&request.url)
        .output()
        .map_err(|error| WebError::InvalidRequest(error.to_string()))?;
    if !output.status.success() {
        return Err(WebError::InvalidRequest(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }
    let max_bytes = request
        .max_bytes
        .or_else(|| config.fetch.as_ref().and_then(|fetch| fetch.max_bytes))
        .unwrap_or(DEFAULT_FETCH_MAX_BYTES)
        .clamp(1, MAX_FETCH_BYTES);
    let truncated = output.stdout.len() > max_bytes;
    let raw = String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(max_bytes)]);
    let (title, text, markdown) = html_document_text(&raw);
    let prompt_response = prompt_response(request, &text);
    Ok(FetchResponse {
        url: request.url.clone(),
        final_url: request.url.clone(),
        status: 200,
        title,
        content_type: Some("text/html; rendered=command".to_string()),
        text,
        markdown,
        truncated,
        rendered: true,
        fallback_used: "rendered_command".to_string(),
        content_format: "markdown".to_string(),
        extraction: "rendered_command".to_string(),
        prompt: request.prompt.clone(),
        prompt_response,
    })
}
fn web_tool_definitions(config: &bcode_plugin_sdk::PluginConfigContext) -> Vec<ToolDefinition> {
    let plugin_config = config
        .typed_or_default::<WebSearchConfig>()
        .unwrap_or_else(|_| WebSearchConfig::default());
    let credentials = ProviderCredentials::new(config);
    let mut tools = Vec::new();
    if search_provider(None, &plugin_config, &credentials).is_ok() {
        tools.push(search_tool_definition());
    }
    tools.push(fetch_tool_definition());
    tools.push(status_tool_definition());
    tools.push(inspect_tool_definition());
    tools
}

fn list_tools(
    request: &ServiceRequest,
    config: &bcode_plugin_sdk::PluginConfigContext,
) -> ServiceResponse {
    if let Err(error) = request.payload_json::<ListToolsRequest>() {
        return invalid_request(&error);
    }
    json_response(&ToolList {
        tools: web_tool_definitions(config),
    })
}

fn web_request_schema(operation: &str) -> Option<&'static str> {
    match operation {
        "web.search" => Some(WEB_SEARCH_REQUEST_SCHEMA),
        "web.fetch" => Some(WEB_FETCH_REQUEST_SCHEMA),
        "web.status" => Some(WEB_STATUS_REQUEST_SCHEMA),
        "web.inspect" => Some(WEB_INSPECT_REQUEST_SCHEMA),
        _ => None,
    }
}

fn web_request_visual_payload(operation: &str, arguments: &serde_json::Value) -> serde_json::Value {
    let mut payload = arguments.as_object().cloned().unwrap_or_default();
    payload.insert(
        "operation".to_owned(),
        serde_json::Value::String(operation.to_owned()),
    );
    serde_json::Value::Object(payload)
}

fn search_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "web.search".to_string(),
        description: "Search the web through the configured search provider. Supports Brave, Tavily, Exa, Serper, SerpAPI, model-native capability detection, and best-effort DuckDuckGo HTML fallback.".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string" },
                "provider": { "type": "string", "description": "Optional provider override: auto, model_native, brave, tavily, exa, perplexity, gemini, serper, serpapi, or duckduckgo_html" },
                "max_results": { "type": "integer", "minimum": 1, "maximum": 20 },
                "site": { "type": "string", "description": "Optional domain restriction; provider adapters translate this to native filtering when supported" },
                "freshness": { "type": "string", "description": "Provider freshness filter; Exa accepts day, week, month, or year" },
                "region": { "type": "string", "description": "Provider-specific country/region code; Exa accepts a two-letter country code" },
                "safe_search": { "type": "string", "description": "Provider-specific safe-search setting" },
                "timeout_ms": { "type": "integer", "minimum": 1 },
                "provider_options": {
                    "type": "object",
                    "description": "Exa-only options: search_type, category, include_domains, exclude_domains, publication/crawl date ranges, include_text, exclude_text, content, max_characters, and max_age_hours. Unknown fields are rejected by the Exa adapter.",
                    "properties": {
                        "search_type": { "type": "string", "enum": ["auto", "fast", "instant", "deep-lite", "deep", "deep-reasoning"] },
                        "category": { "type": "string", "enum": ["company", "people", "publication", "news", "personal_site", "financial_report"] },
                        "include_domains": { "type": "array", "maxItems": 1200, "items": { "type": "string" } },
                        "exclude_domains": { "type": "array", "maxItems": 1200, "items": { "type": "string" } },
                        "start_published_date": { "type": "string" },
                        "end_published_date": { "type": "string" },
                        "start_crawl_date": { "type": "string" },
                        "end_crawl_date": { "type": "string" },
                        "include_text": { "type": "array", "maxItems": 1, "items": { "type": "string" } },
                        "exclude_text": { "type": "array", "maxItems": 1, "items": { "type": "string" } },
                        "content": { "type": "string", "enum": ["highlights", "text", "summary"] },
                        "max_characters": { "type": "integer", "minimum": 1, "maximum": 20000 },
                        "max_age_hours": { "type": "integer", "minimum": -1 }
                    },
                    "additionalProperties": false
                }
            }
        }),
    }
}

fn fetch_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "web.fetch".to_string(),
        description:
            "Fetch a URL over HTTP(S) and return bounded model-visible text plus response metadata."
                .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["url"],
            "properties": {
                "url": { "type": "string" },
                "max_bytes": { "type": "integer", "minimum": 1, "maximum": MAX_FETCH_BYTES },
                "timeout_ms": { "type": "integer", "minimum": 1 },
                "render": { "type": "boolean", "description": "Use the explicit rendered-fetch command adapter configured by BCODE_WEB_RENDER_COMMAND" },
                "prompt": { "type": "string", "description": "Optional question or extraction prompt to carry alongside fetched content" },
                "provider": { "type": "string", "description": "Reserved provider override for prompted extraction; plain fetch currently returns content plus prompt metadata" }
            }
        }),
    }
}

fn status_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "web.status".to_string(),
        description: "Report configured and fallback web search/fetch capabilities.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {}
        }),
    }
}

fn inspect_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "web.inspect".to_string(),
        description: "Classify a URL and recommend the most agent-appropriate Bcode tool/action before fetching.".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["url"],
            "properties": {
                "url": { "type": "string" }
            }
        }),
    }
}

fn inspect_url(url: &str) -> Result<InspectResponse, WebError> {
    validate_url(url)?;
    let lower = url.to_ascii_lowercase();
    let (kind, recommended_tool, recommended_action, notes) = if is_git_repo_url(&lower) {
        (
            "git_repository",
            Some("git.clone".to_string()),
            "Use git.clone when available so the agent can inspect real repository files instead of rendered forge HTML.".to_string(),
            vec!["Git repository web pages are poor fetch targets for code understanding.".to_string()],
        )
    } else if std::path::Path::new(&lower)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
    {
        (
            "pdf",
            Some("document.extract".to_string()),
            "Use document.extract when available; web.fetch can only return raw or fallback text for PDFs.".to_string(),
            vec!["PDF extraction should preserve page text and metadata.".to_string()],
        )
    } else if is_youtube_url(&lower) {
        (
            "youtube_video",
            Some("media.transcript".to_string()),
            "Use media.transcript when available before attempting video analysis.".to_string(),
            vec![
                "Transcripts are cheaper and more agent-friendly than visual analysis.".to_string(),
            ],
        )
    } else {
        (
            "web_page",
            Some("web.fetch".to_string()),
            "Use web.fetch for a bounded Markdown-oriented page read.".to_string(),
            Vec::new(),
        )
    };
    Ok(InspectResponse {
        url: url.to_string(),
        kind: kind.to_string(),
        recommended_tool,
        recommended_action,
        notes,
    })
}

fn is_git_repo_url(lower_url: &str) -> bool {
    for host in ["github.com", "gitlab.com", "codeberg.org", "bitbucket.org"] {
        if forge_repo_url(lower_url, host) {
            return true;
        }
    }
    has_git_extension(lower_url)
}

fn has_git_extension(value: &str) -> bool {
    std::path::Path::new(value)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("git"))
}

fn forge_repo_url(lower_url: &str, host: &str) -> bool {
    let secure_prefix = format!("https://{host}/");
    let plain_prefix = format!("http://{host}/");
    if !(lower_url.starts_with(&secure_prefix) || lower_url.starts_with(&plain_prefix)) {
        return false;
    }
    let path = lower_url
        .trim_start_matches(&secure_prefix)
        .trim_start_matches(&plain_prefix);
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    let Some(owner) = segments.next() else {
        return false;
    };
    let Some(repo) = segments.next() else {
        return false;
    };
    !matches!(owner, "features" | "topics" | "trending" | "marketplace") && !repo.is_empty()
}

fn is_youtube_url(lower_url: &str) -> bool {
    lower_url.contains("youtube.com/watch") || lower_url.contains("youtu.be/")
}

fn status_response(
    config: &WebSearchConfig,
    credentials: &ProviderCredentials,
) -> WebStatusResponse {
    let provider = search_provider(None, config, credentials).ok();
    let configured = configured_search_providers(config, credentials);
    let available = provider
        .as_deref()
        .is_some_and(|provider| search_provider_available(provider, config, credentials));
    let credential = provider
        .as_deref()
        .filter(|provider| *provider == EXA_PROVIDER_ID)
        .and_then(|_| credentials.exa(&config.providers.exa));
    let quality = provider
        .as_deref()
        .filter(|_| available)
        .map_or("unavailable", provider_quality)
        .to_string();
    let mut recommended = Vec::new();
    if matches!(provider.as_deref(), Some(EXA_PROVIDER_ID)) && !available {
        recommended.push(
            "Run `bcode auth login exa`, configure an explicit environment reference, or set EXA_API_KEY."
                .to_owned(),
        );
    } else if let Some(provider) = provider.as_deref().filter(|_| !available) {
        recommended.push(format!(
            "The selected {provider} search provider is missing required credentials or host capability."
        ));
    } else if matches!(provider.as_deref(), Some("duckduckgo_html") | None) {
        recommended.push(
            "Configure Brave, Tavily, Exa, Perplexity, Gemini, Serper, SerpAPI, or model-native search for more stable results."
                .to_string(),
        );
    }
    let fallbacks = fetch_fallbacks(config);
    WebStatusResponse {
        search: SearchStatus {
            available,
            provider,
            quality,
            credential_source: credential.map(|(_, source)| source.as_str().to_owned()),
            credential_owner: credential.map(|_| WEB_SEARCH_PLUGIN_ID.to_owned()),
            configured_providers: configured,
            recommended,
        },
        fetch: FetchStatus {
            available: true,
            rendered_fetch: rendered_fetch_available(config),
            max_bytes: config
                .fetch
                .as_ref()
                .and_then(|fetch| fetch.max_bytes)
                .unwrap_or(DEFAULT_FETCH_MAX_BYTES),
            fallbacks: fallbacks
                .into_iter()
                .map(FetchFallback::as_str)
                .map(ToString::to_string)
                .collect(),
        },
    }
}

fn search_provider_available(
    provider: &str,
    config: &WebSearchConfig,
    credentials: &ProviderCredentials,
) -> bool {
    match provider {
        "brave" => provider_key(
            &config.providers.brave,
            &["BCODE_WEB_SEARCH_API_KEY", "BRAVE_SEARCH_API_KEY"],
        )
        .is_ok(),
        "tavily" => provider_key(&config.providers.tavily, &["TAVILY_API_KEY"]).is_ok(),
        "exa" => credentials.exa(&config.providers.exa).is_some(),
        "perplexity" | "pplx" => provider_key(
            &config.providers.perplexity,
            &["PERPLEXITY_API_KEY", "PPLX_API_KEY"],
        )
        .is_ok(),
        "gemini" | "google_gemini" => provider_key(
            &config.providers.gemini,
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        )
        .is_ok(),
        "serper" => provider_key(&config.providers.serper, &["SERPER_API_KEY"]).is_ok(),
        "serpapi" | "serp_api" => {
            provider_key(&config.providers.serpapi, &["SERPAPI_API_KEY"]).is_ok()
        }
        "model_native" => config.model_native_available,
        "duckduckgo_html" => config.allow_best_effort_no_key,
        _ => false,
    }
}

fn provider_quality(provider: &str) -> &'static str {
    match provider {
        "duckduckgo_html" => "best_effort",
        "model_native" => "model_provider_native",
        _ => "configured_api",
    }
}

fn configured_search_providers(
    config: &WebSearchConfig,
    credentials: &ProviderCredentials,
) -> Vec<String> {
    let mut providers = Vec::new();
    if config
        .providers
        .brave
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve_legacy)
        .is_some()
        || env_value(&["BCODE_WEB_SEARCH_API_KEY", "BRAVE_SEARCH_API_KEY"]).is_some()
    {
        providers.push("brave".to_string());
    }
    if config
        .providers
        .tavily
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve_legacy)
        .is_some()
        || env_value(&["TAVILY_API_KEY"]).is_some()
    {
        providers.push("tavily".to_string());
    }
    if credentials.exa(&config.providers.exa).is_some() {
        providers.push("exa".to_string());
    }
    if config
        .providers
        .perplexity
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve_legacy)
        .is_some()
        || env_value(&["PERPLEXITY_API_KEY", "PPLX_API_KEY"]).is_some()
    {
        providers.push("perplexity".to_string());
    }
    if config
        .providers
        .gemini
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve_legacy)
        .is_some()
        || env_value(&["GEMINI_API_KEY", "GOOGLE_API_KEY"]).is_some()
    {
        providers.push("gemini".to_string());
    }
    if config
        .providers
        .serper
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve_legacy)
        .is_some()
        || env_value(&["SERPER_API_KEY"]).is_some()
    {
        providers.push("serper".to_string());
    }
    if config
        .providers
        .serpapi
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve_legacy)
        .is_some()
        || env_value(&["SERPAPI_API_KEY"]).is_some()
    {
        providers.push("serpapi".to_string());
    }
    if config.model_native_available {
        providers.push("model_native".to_string());
    }
    if config.allow_best_effort_no_key {
        providers.push("duckduckgo_html".to_string());
    }
    providers
}

fn rendered_fetch_available(config: &WebSearchConfig) -> bool {
    config
        .fetch
        .as_ref()
        .and_then(|fetch| fetch.rendered.as_ref())
        .and_then(|rendered| rendered.command.as_ref())
        .is_some()
        || env_value(&["BCODE_WEB_RENDER_COMMAND"]).is_some()
}

fn search_response(query: String, provider: &str, results: Vec<SearchResult>) -> SearchResponse {
    SearchResponse {
        query,
        provider: provider.to_string(),
        results,
        partial: false,
        message: None,
    }
}

fn max_results(request: &SearchRequest, config: &WebSearchConfig) -> usize {
    request
        .max_results
        .or(config.max_results)
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, 20)
}

fn scoped_query(request: &SearchRequest) -> String {
    let mut query = request.query.trim().to_string();
    if let Some(site) = request
        .site
        .as_deref()
        .map(str::trim)
        .filter(|site| !site.is_empty())
    {
        query = format!("site:{site} {query}");
    }
    query
}

fn explicit_search_provider(provider: &str) -> Result<String, WebError> {
    const SUPPORTED: &[&str] = &[
        "brave",
        "tavily",
        "exa",
        "perplexity",
        "pplx",
        "gemini",
        "google_gemini",
        "serper",
        "serpapi",
        "serp_api",
        "model_native",
        "duckduckgo_html",
    ];
    if SUPPORTED.contains(&provider) {
        Ok(provider.to_string())
    } else {
        Err(WebError::InvalidRequest(format!(
            "unsupported web search provider: {provider}"
        )))
    }
}

fn search_provider(
    explicit: Option<&str>,
    config: &WebSearchConfig,
    credentials: &ProviderCredentials,
) -> Result<String, WebError> {
    let provider = explicit
        .map(str::to_string)
        .or_else(|| config.provider.clone())
        .or_else(|| env_value(&["BCODE_WEB_SEARCH_PROVIDER"]))
        .unwrap_or_else(|| "auto".to_string())
        .trim()
        .to_ascii_lowercase();
    if provider != "auto" {
        return explicit_search_provider(&provider);
    }
    if config
        .providers
        .brave
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve_legacy)
        .is_some()
        || env_value(&["BCODE_WEB_SEARCH_API_KEY", "BRAVE_SEARCH_API_KEY"]).is_some()
    {
        return Ok("brave".to_string());
    }
    if config
        .providers
        .tavily
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve_legacy)
        .is_some()
        || env_value(&["TAVILY_API_KEY"]).is_some()
    {
        return Ok("tavily".to_string());
    }
    if credentials.exa(&config.providers.exa).is_some() {
        return Ok("exa".to_string());
    }
    if config
        .providers
        .perplexity
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve_legacy)
        .is_some()
        || env_value(&["PERPLEXITY_API_KEY", "PPLX_API_KEY"]).is_some()
    {
        return Ok("perplexity".to_string());
    }
    if config
        .providers
        .gemini
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve_legacy)
        .is_some()
        || env_value(&["GEMINI_API_KEY", "GOOGLE_API_KEY"]).is_some()
    {
        return Ok("gemini".to_string());
    }
    if config
        .providers
        .serper
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve_legacy)
        .is_some()
        || env_value(&["SERPER_API_KEY"]).is_some()
    {
        return Ok("serper".to_string());
    }
    if config
        .providers
        .serpapi
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve_legacy)
        .is_some()
        || env_value(&["SERPAPI_API_KEY"]).is_some()
    {
        return Ok("serpapi".to_string());
    }
    if config.model_native_available {
        return Ok("model_native".to_string());
    }
    if config.allow_best_effort_no_key {
        return Ok("duckduckgo_html".to_string());
    }
    Err(WebError::MissingProvider)
}

fn gemini_text(response: &GeminiGenerateResponse) -> String {
    response
        .candidates
        .iter()
        .flat_map(|candidate| candidate.content.parts.iter())
        .map(|part| part.text.trim())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn urls_from_text(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|token| {
            let token = token.trim_matches(|character: char| {
                matches!(
                    character,
                    '(' | ')' | '[' | ']' | ',' | '.' | ';' | '"' | '\''
                )
            });
            (token.starts_with("http://") || token.starts_with("https://"))
                .then(|| token.to_string())
        })
        .collect()
}

fn provider_key(config: &ProviderConfig, names: &[&str]) -> Result<String, WebError> {
    config
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve_legacy)
        .or_else(|| env_value(names))
        .ok_or(WebError::MissingProvider)
}

fn client(timeout_ms: Option<u64>) -> Result<Client, WebError> {
    Client::builder()
        .timeout(Duration::from_millis(
            timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).max(1),
        ))
        .user_agent(USER_AGENT)
        .build()
        .map_err(WebError::Network)
}

async fn checked_text(response: reqwest::Response) -> Result<String, WebError> {
    let status = response.status();
    let body = response.text().await?;
    if status.is_success() {
        Ok(body)
    } else {
        Err(WebError::Http {
            status: status.as_u16(),
            body: truncate_chars(&body, 1_000),
        })
    }
}

fn env_value(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    })
}

fn validate_non_empty(field: &str, value: &str) -> Result<(), WebError> {
    if value.trim().is_empty() {
        Err(WebError::InvalidRequest(format!(
            "{field} must not be empty"
        )))
    } else {
        Ok(())
    }
}

fn validate_url(url: &str) -> Result<(), WebError> {
    validate_non_empty("url", url)?;
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        Ok(())
    } else {
        Err(WebError::InvalidRequest(
            "url must start with http:// or https://".to_string(),
        ))
    }
}
fn html_document_text(input: &str) -> (Option<String>, String, Option<String>) {
    let title = extract_between_case_insensitive(input, "<title", "</title>")
        .and_then(|raw| raw.split_once('>').map(|(_, text)| html_text(text)))
        .filter(|text| !text.is_empty());
    let body = extract_preferred_body(input).unwrap_or(input);
    let markdown = html_to_markdown(body);
    let text = collapse_blank_lines(&markdown);
    (title.or_else(|| plain_title(&text)), text, Some(markdown))
}

fn extract_preferred_body(input: &str) -> Option<&str> {
    select_longest_element(input, &["main", "article"])
        .or_else(|| select_longest_element(input, &["body"]))
}

fn select_longest_element<'a>(input: &'a str, tags: &[&str]) -> Option<&'a str> {
    tags.iter()
        .flat_map(|tag| extract_elements(input, tag))
        .max_by_key(|candidate| readable_score(candidate))
}

fn extract_elements<'a>(input: &'a str, tag: &str) -> Vec<&'a str> {
    let mut elements = Vec::new();
    let lower = input.to_ascii_lowercase();
    let start_token = format!("<{tag}");
    let end_token = format!("</{tag}>");
    let mut search_from = 0;
    while let Some(relative_start) = lower[search_from..].find(&start_token) {
        let start = search_from + relative_start;
        let Some(relative_content_start) = input[start..].find('>') else {
            break;
        };
        let content_start = start + relative_content_start + 1;
        let Some(relative_end) = lower[content_start..].find(&end_token) else {
            break;
        };
        let end = content_start + relative_end;
        elements.push(&input[content_start..end]);
        search_from = end + end_token.len();
    }
    elements
}

fn readable_score(input: &str) -> usize {
    let text = html_text(input);
    text.chars()
        .filter(|character| !character.is_whitespace())
        .count()
}

fn extract_between_case_insensitive<'a>(input: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let lower = input.to_ascii_lowercase();
    let range_start = lower.find(&start.to_ascii_lowercase())?;
    let range_end = lower[range_start..].find(&end.to_ascii_lowercase())? + range_start + end.len();
    Some(&input[range_start..range_end])
}

fn html_to_markdown(input: &str) -> String {
    let without_noise = remove_noise_elements(input);
    let mut output = String::with_capacity(without_noise.len());
    let mut tag = String::new();
    let mut in_tag = false;
    let mut in_entity = false;
    let mut entity = String::new();
    for character in without_noise.chars() {
        if in_tag {
            if character == '>' {
                push_tag_marker(&mut output, &tag);
                tag.clear();
                in_tag = false;
            } else {
                tag.push(character);
            }
            continue;
        }
        if in_entity {
            if character == ';' {
                output.push_str(decode_entity(&entity));
                entity.clear();
                in_entity = false;
            } else if entity.len() < 16 {
                entity.push(character);
            } else {
                output.push('&');
                output.push_str(&entity);
                entity.clear();
                in_entity = false;
            }
            continue;
        }
        match character {
            '<' => in_tag = true,
            '&' => in_entity = true,
            _ => output.push(character),
        }
    }
    collapse_blank_lines(&output)
}

fn remove_noise_elements(input: &str) -> String {
    let mut output = input.to_string();
    for tag in ["script", "style", "nav", "footer", "aside", "svg"] {
        output = remove_element_case_insensitive(&output, tag);
    }
    output
}

fn remove_element_case_insensitive(input: &str, tag: &str) -> String {
    let mut output = String::new();
    let mut remaining = input;
    let start_token = format!("<{tag}");
    let end_token = format!("</{tag}>");
    loop {
        let lower = remaining.to_ascii_lowercase();
        let Some(start) = lower.find(&start_token) else {
            output.push_str(remaining);
            break;
        };
        output.push_str(&remaining[..start]);
        let Some(relative_end) = lower[start..].find(&end_token) else {
            break;
        };
        let end = start + relative_end + end_token.len();
        remaining = &remaining[end..];
    }
    output
}

fn push_tag_marker(output: &mut String, tag: &str) {
    let normalized = tag
        .trim_start_matches('/')
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match normalized.as_str() {
        "h1" => output.push_str("\n\n# "),
        "h2" => output.push_str("\n\n## "),
        "h3" => output.push_str("\n\n### "),
        "h4" | "h5" | "h6" => output.push_str("\n\n#### "),
        "p" | "div" | "section" | "article" | "main" | "br" | "tr" => output.push_str("\n\n"),
        "li" => output.push_str("\n* "),
        "td" | "th" => output.push_str(" | "),
        "pre" => output.push_str("\n\n```\n"),
        "code" => output.push('`'),
        _ if normalized == "a" => {
            if let Some(href) = tag_attribute(tag, "href") {
                output.push_str(" [");
                output.push_str(&href);
                output.push_str("] ");
            } else {
                output.push(' ');
            }
        }
        _ => output.push(' '),
    }
}

fn tag_attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let key = format!("{name}=");
    let start = lower.find(&key)? + key.len();
    let quote = tag[start..].chars().next()?;
    if quote == '"' || quote == '\'' {
        let value_start = start + quote.len_utf8();
        let end = tag[value_start..].find(quote)? + value_start;
        Some(html_text(&tag[value_start..end]))
    } else {
        let end = tag[start..]
            .find(char::is_whitespace)
            .map_or(tag.len(), |end| start + end);
        Some(html_text(&tag[start..end]))
    }
}

fn html_text(input: &str) -> String {
    collapse_whitespace(&html_to_markdown(input))
}

fn decode_entity(entity: &str) -> &str {
    match entity {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" | "#39" => "'",
        _ => " ",
    }
}

fn collapse_whitespace(input: &str) -> String {
    let mut output = String::new();
    let mut last_was_space = false;
    for character in input.chars() {
        if character.is_whitespace() {
            if !last_was_space {
                output.push(' ');
                last_was_space = true;
            }
        } else {
            output.push(character);
            last_was_space = false;
        }
    }
    output.trim().to_string()
}

fn collapse_blank_lines(input: &str) -> String {
    let mut output = String::new();
    let mut blank_lines = 0_u8;
    for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if blank_lines < 2 && !output.is_empty() {
            output.push_str("\n\n");
            blank_lines += 1;
        }
        output.push_str(line);
    }
    output.trim().to_string()
}

fn plain_title(text: &str) -> Option<String> {
    text.lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| truncate_chars(line, 120))
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    input.chars().take(max_chars).collect()
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

fn search_tool_response(value: &SearchResponse, tool_call_id: &str) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value).and_then(|output| {
        let payload = serde_json::to_value(value)?;
        Ok((output, payload))
    }) {
        Ok((output, payload)) => ToolInvocationResponse {
            output,
            is_error: false,
            content: Vec::new(),
            full_output: None,
            result: Some(web_artifact_result(
                tool_call_id,
                "search",
                WEB_SEARCH_RESULTS_SCHEMA,
                "Search results",
                payload,
            )),
        },
        Err(error) => tool_error(error.to_string()),
    }
}

fn json_tool_response_with_artifact<T: Serialize>(
    value: &T,
    tool_call_id: &str,
    artifact_suffix: &str,
    schema: &str,
    title: &str,
) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value).and_then(|output| {
        let payload = serde_json::to_value(value)?;
        Ok((output, payload))
    }) {
        Ok((output, payload)) => ToolInvocationResponse {
            output,
            is_error: false,
            content: Vec::new(),
            full_output: None,
            result: Some(web_artifact_result(
                tool_call_id,
                artifact_suffix,
                schema,
                title,
                payload,
            )),
        },
        Err(error) => tool_error(error.to_string()),
    }
}

fn web_artifact_result(
    tool_call_id: &str,
    artifact_suffix: &str,
    schema: &str,
    title: &str,
    payload: serde_json::Value,
) -> ToolInvocationResult {
    ToolInvocationResult::Artifact {
        artifact: Box::new(ToolArtifact {
            artifact_id: format!("{tool_call_id}-web-{artifact_suffix}"),
            producer_plugin_id: WEB_SEARCH_PLUGIN_ID.to_string(),
            schema: schema.to_string(),
            schema_version: 1,
            tool_call_id: Some(tool_call_id.to_string()),
            title: Some(title.to_string()),
            metadata: payload,
            refs: Vec::new(),
        }),
    }
}

const fn tool_error(output: String) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output,
        is_error: true,
        content: Vec::new(),
        full_output: None,
        result: None,
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(WebSearchPlugin, include_str!("../bcode-plugin.toml"))
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn web_search_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    registry.register_visual_adapter(
        [
            "web-search-request-card",
            "web-fetch-request-card",
            "web-status-request-card",
            "web-inspect-request-card",
            "web-search-results-card",
            "web-fetch-result-card",
            "web-status-card",
            "web-inspect-result-card",
        ],
        Box::new(web_search_tui::WebSearchTuiVisualAdapter),
    );
    registry
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(WebSearchPlugin, include_str!("../bcode-plugin.toml"));
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static AUTH_REGISTRATIONS: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
    static EXA_ENV: Mutex<()> = Mutex::new(());

    struct ExaEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
    }

    impl ExaEnvGuard {
        fn set(value: Option<&str>) -> Self {
            let lock = EXA_ENV.lock().expect("Exa environment lock");
            let previous = std::env::var_os(EXA_STORAGE_KEY);
            unsafe {
                if let Some(value) = value {
                    std::env::set_var(EXA_STORAGE_KEY, value);
                } else {
                    std::env::remove_var(EXA_STORAGE_KEY);
                }
            }
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for ExaEnvGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var(EXA_STORAGE_KEY, previous);
                } else {
                    std::env::remove_var(EXA_STORAGE_KEY);
                }
            }
        }
    }

    extern "C" fn collect_auth_registration(
        payload: *const u8,
        payload_len: usize,
        _user_data: *mut std::ffi::c_void,
    ) {
        let payload = unsafe { std::slice::from_raw_parts(payload, payload_len) };
        AUTH_REGISTRATIONS
            .lock()
            .expect("auth registration collector")
            .push(payload.to_vec());
    }

    fn plugin_context_with_exa(secret: Option<&str>) -> bcode_plugin_sdk::PluginConfigContext {
        let mut context = bcode_plugin_sdk::PluginConfigContext::default();
        if let Some(secret) = secret {
            context
                .secrets
                .insert(EXA_CANONICAL_SECRET_ID.to_owned(), secret.to_owned());
        }
        context
    }

    #[test]
    fn plugin_registers_exa_dynamically() {
        AUTH_REGISTRATIONS
            .lock()
            .expect("auth registration collector")
            .clear();
        let mut plugin = WebSearchPlugin::default();
        plugin
            .register_auth_providers(AuthRegistrar::new(
                Some(collect_auth_registration),
                std::ptr::null_mut(),
            ))
            .expect("Exa registration succeeds");
        let registrations = AUTH_REGISTRATIONS
            .lock()
            .expect("auth registration collector");
        assert_eq!(registrations.len(), 1);
        let contribution: AuthProviderContribution =
            serde_json::from_slice(&registrations[0]).expect("registration decodes");
        drop(registrations);
        assert_eq!(contribution, exa_auth_provider_contribution());
    }

    #[test]
    fn auth_contribution_declares_exa_api_key_contract() {
        let contribution = exa_auth_provider_contribution();
        contribution.validate().expect("valid Exa contribution");
        assert_eq!(contribution.provider_id, EXA_PROVIDER_ID);
        assert_eq!(contribution.methods.len(), 1);
        let AuthMethodContribution::SecretFields {
            method_id,
            fields,
            supports_verification,
            supports_revocation,
            ..
        } = &contribution.methods[0]
        else {
            panic!("Exa must use generic secret-field enrollment");
        };
        assert_eq!(method_id, "api_key");
        assert!(!supports_verification);
        assert!(!supports_revocation);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].credential_id, EXA_CREDENTIAL_ID);
        assert_eq!(fields[0].storage_key, EXA_STORAGE_KEY);
    }

    #[test]
    fn integrated_exa_credential_has_canonical_owner_and_redacted_status() {
        let config = WebSearchConfig {
            provider: Some(EXA_PROVIDER_ID.to_owned()),
            ..WebSearchConfig::default()
        };
        let context = plugin_context_with_exa(Some("integrated-test-secret"));
        let credentials = ProviderCredentials::new(&context);
        assert_eq!(
            credentials.exa_key(&config.providers.exa).expect("Exa key"),
            "integrated-test-secret"
        );
        let status = status_response(&config, &credentials);
        assert!(status.search.available);
        assert_eq!(
            status.search.credential_source.as_deref(),
            Some("integrated_auth")
        );
        assert_eq!(
            status.search.credential_owner.as_deref(),
            Some(WEB_SEARCH_PLUGIN_ID)
        );
        let encoded = serde_json::to_string(&status).expect("status encodes");
        assert!(!encoded.contains("integrated-test-secret"));
    }

    #[test]
    fn explicit_reference_is_reported_without_exposing_value() {
        let _env = ExaEnvGuard::set(None);
        unsafe {
            std::env::set_var(
                "BCODE_TEST_EXA_EXPLICIT_REFERENCE",
                "explicit-reference-secret",
            );
        }
        let config = WebSearchConfig {
            provider: Some(EXA_PROVIDER_ID.to_owned()),
            providers: WebSearchProviderConfig {
                exa: ProviderConfig {
                    api_key: Some(SecretRef::Env {
                        name: "BCODE_TEST_EXA_EXPLICIT_REFERENCE".to_owned(),
                    }),
                },
                ..WebSearchProviderConfig::default()
            },
            ..WebSearchConfig::default()
        };
        let context = plugin_context_with_exa(Some("explicit-reference-secret"));
        let credentials = ProviderCredentials::new(&context);
        let status = status_response(&config, &credentials);
        unsafe {
            std::env::remove_var("BCODE_TEST_EXA_EXPLICIT_REFERENCE");
        }
        assert!(status.search.available);
        assert_eq!(
            status.search.credential_source.as_deref(),
            Some("explicit_reference")
        );
        assert!(
            !serde_json::to_string(&status)
                .expect("status encodes")
                .contains("explicit-reference-secret")
        );
    }

    #[test]
    fn conventional_exa_environment_fallback_is_available_and_redacted() {
        let _env = ExaEnvGuard::set(Some("environment-fallback-secret"));
        let config = WebSearchConfig {
            provider: Some(EXA_PROVIDER_ID.to_owned()),
            ..WebSearchConfig::default()
        };
        let context = plugin_context_with_exa(None);
        let credentials = ProviderCredentials::new(&context);
        let status = status_response(&config, &credentials);
        assert!(status.search.available);
        assert_eq!(
            status.search.credential_source.as_deref(),
            Some("environment_fallback")
        );
        let encoded = serde_json::to_string(&status).expect("status encodes");
        assert!(!encoded.contains("environment-fallback-secret"));
    }

    #[test]
    fn web_tools_emit_mapped_request_contributions() {
        assert_eq!(
            web_request_schema("web.search"),
            Some(WEB_SEARCH_REQUEST_SCHEMA)
        );
        assert_eq!(
            web_request_schema("web.fetch"),
            Some(WEB_FETCH_REQUEST_SCHEMA)
        );
        assert_eq!(
            web_request_schema("web.status"),
            Some(WEB_STATUS_REQUEST_SCHEMA)
        );
        assert_eq!(
            web_request_schema("web.inspect"),
            Some(WEB_INSPECT_REQUEST_SCHEMA)
        );
        assert_eq!(web_request_schema("unknown"), None);
    }

    #[test]
    fn web_catalog_preparation_accepts_missing_url_and_preserves_read_only_tools() {
        for definition in [
            search_tool_definition(),
            fetch_tool_definition(),
            status_tool_definition(),
            inspect_tool_definition(),
        ] {
            let request = bcode_tool::ToolPreparationRequest {
                invocation: bcode_tool::ToolInvocationDescriptor {
                    invocation_id: "catalog".to_owned(),
                    tool_name: definition.name.clone(),
                    arguments: serde_json::Value::Null,
                },
                host_context: Vec::new(),
            };
            let policy = web_policy_operation(&request, &definition).expect("catalog web policy");
            assert_eq!(policy.requires_permission, definition.name == "web.fetch");
            if definition.name != "web.fetch" {
                assert_eq!(
                    policy.operation,
                    bcode_plugin_sdk::ToolPolicyOperation::ReadOnly
                );
            }
        }
    }

    #[test]
    fn web_owner_prepares_fetch_url_without_generic_extractors() {
        let definition = fetch_tool_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".to_owned(),
                tool_name: definition.name.clone(),
                arguments: serde_json::json!({"url": "https://example.com/page"}),
            },
            host_context: Vec::new(),
        };
        let policy = web_policy_operation(&request, &definition).expect("web policy");
        assert!(policy.requires_permission);
        assert_eq!(
            policy.operation,
            bcode_plugin_sdk::ToolPolicyOperation::Web {
                url: Some("https://example.com/page".to_owned()),
            }
        );
    }

    #[test]
    fn progress_uses_neutral_invocation_lifecycle_contract() {
        let event = progress_lifecycle_event("web-call", 6, "searching".to_owned());
        let encoded = serde_json::to_vec(&event).expect("lifecycle should encode");
        let decoded: ToolInvocationLifecycleEvent =
            serde_json::from_slice(&encoded).expect("lifecycle should decode");
        assert_eq!(decoded.invocation_id, "web-call");
        assert_eq!(decoded.sequence, 6);
        assert_eq!(decoded.stage, ToolInvocationLifecycleStage::Progress);
        assert_eq!(decoded.message.as_deref(), Some("searching"));
    }

    #[test]
    fn web_search_output_is_usable_without_presentation_events() {
        let response = search_tool_response(
            &SearchResponse {
                query: "rust".to_string(),
                provider: "test".to_string(),
                results: vec![SearchResult {
                    title: "Rust".to_string(),
                    url: "https://www.rust-lang.org/".to_string(),
                    snippet: "A language empowering everyone".to_string(),
                    published: None,
                    source: Some("example".to_string()),
                }],
                partial: false,
                message: Some("ok".to_string()),
            },
            "test-search",
        );

        assert!(!response.is_error);
        assert!(response.output.contains("\"query\": \"rust\""));
        assert!(response.output.contains("Rust"));
        assert!(response.output.contains("https://www.rust-lang.org/"));
        assert!(response.result.is_some());
    }

    #[test]
    fn web_fetch_output_is_usable_without_presentation_events() {
        let response = json_tool_response_with_artifact(
            &FetchResponse {
                url: "https://example.com".to_string(),
                final_url: "https://example.com/".to_string(),
                status: 200,
                title: Some("Example".to_string()),
                content_type: Some("text/html".to_string()),
                text: "Example Domain".to_string(),
                markdown: None,
                truncated: false,
                rendered: false,
                fallback_used: "none".to_string(),
                content_format: "text".to_string(),
                extraction: "plain".to_string(),
                prompt: None,
                prompt_response: None,
            },
            "test-fetch",
            "fetch",
            WEB_FETCH_RESULT_SCHEMA,
            "Fetched page",
        );

        assert!(!response.is_error);
        assert!(response.output.contains("https://example.com"));
        assert!(response.output.contains("Example Domain"));
        assert!(response.result.is_some());
    }

    #[test]
    fn html_text_removes_tags_and_decodes_common_entities() {
        assert_eq!(
            html_text("<h1>Rust &amp; Bcode</h1><p>A&nbsp;test</p>"),
            "# Rust & Bcode # A test"
        );
    }

    #[test]
    fn html_document_prefers_article_and_removes_noise() {
        let html = r"
            <html><head><title>Doc title</title></head>
            <body><nav>menu</nav><article><h1>Heading</h1><p>Body &amp; text</p></article></body></html>
        ";
        let (title, text, markdown) = html_document_text(html);
        assert_eq!(title.as_deref(), Some("Doc title"));
        assert!(text.contains("# Heading"));
        assert!(text.contains("Body & text"));
        assert!(!text.contains("menu"));
        assert!(markdown.is_some());
    }

    #[test]
    fn html_document_chooses_longer_article_content() {
        let html = r"
            <body>
                <article><p>short</p></article>
                <article><h2>Useful</h2><p>This is the detailed content agents need.</p></article>
            </body>
        ";
        let (_title, text, _markdown) = html_document_text(html);
        assert!(text.contains("Useful"));
        assert!(text.contains("detailed content"));
    }

    #[test]
    fn html_markdown_preserves_links_and_table_cells() {
        let markdown = html_to_markdown(
            "<p>See <a href='https://example.com'>docs</a></p><table><tr><td>A</td><td>B</td></tr></table>",
        );
        assert!(markdown.contains("https://example.com"));
        assert!(markdown.contains('A'));
        assert!(markdown.contains('B'));
    }

    #[test]
    fn validate_url_rejects_non_http_urls() {
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("https://example.com").is_ok());
    }

    #[test]
    fn auto_provider_uses_best_effort_fallback_by_default() {
        let _env = ExaEnvGuard::set(None);
        let context = plugin_context_with_exa(None);
        let credentials = ProviderCredentials::new(&context);
        let provider = search_provider(None, &WebSearchConfig::default(), &credentials)
            .expect("best-effort fallback should resolve");
        assert_eq!(provider, "duckduckgo_html");
    }

    #[test]
    fn auto_provider_can_disable_best_effort_fallback() {
        let _env = ExaEnvGuard::set(None);
        let config = WebSearchConfig {
            allow_best_effort_no_key: false,
            ..WebSearchConfig::default()
        };
        let context = plugin_context_with_exa(None);
        let credentials = ProviderCredentials::new(&context);
        assert!(search_provider(None, &config, &credentials).is_err());
    }

    #[test]
    fn explicit_provider_names_are_validated() {
        assert_eq!(
            search_provider(
                Some("exa"),
                &WebSearchConfig::default(),
                &ProviderCredentials::new(&plugin_context_with_exa(None)),
            )
            .expect("exa"),
            "exa"
        );
        assert!(
            search_provider(
                Some("unknown-provider"),
                &WebSearchConfig::default(),
                &ProviderCredentials::new(&plugin_context_with_exa(None)),
            )
            .is_err()
        );
    }

    #[test]
    fn auto_provider_prefers_configured_exa() {
        let config = WebSearchConfig::default();
        let context = plugin_context_with_exa(Some("test-only"));
        let credentials = ProviderCredentials::new(&context);
        assert_eq!(
            search_provider(None, &config, &credentials).expect("exa"),
            "exa"
        );
        let status = status_response(&config, &credentials);
        assert_eq!(status.search.provider.as_deref(), Some("exa"));
        assert!(
            status
                .search
                .configured_providers
                .contains(&"exa".to_string())
        );
    }

    #[test]
    fn status_reports_configured_exa_without_exposing_key() {
        let config = WebSearchConfig {
            provider: Some("exa".to_string()),
            ..WebSearchConfig::default()
        };
        let context = plugin_context_with_exa(Some("status-test-secret"));
        let credentials = ProviderCredentials::new(&context);
        let status = status_response(&config, &credentials);
        assert_eq!(status.search.provider.as_deref(), Some("exa"));
        assert!(status.search.available);
        assert_eq!(status.search.quality, "configured_api");
        let encoded = serde_json::to_string(&status).expect("status encodes");
        assert!(!encoded.contains("status-test-secret"));
    }

    #[test]
    fn status_reports_explicit_exa_missing_key_without_claiming_availability() {
        let config = WebSearchConfig {
            provider: Some("exa".to_string()),
            ..WebSearchConfig::default()
        };
        let context = plugin_context_with_exa(None);
        let credentials = ProviderCredentials::new(&context);
        let status = status_response(&config, &credentials);
        assert_eq!(status.search.provider.as_deref(), Some("exa"));
        assert!(!status.search.available);
        assert_eq!(status.search.quality, "unavailable");
        assert!(
            status
                .search
                .configured_providers
                .iter()
                .all(|provider| provider != "exa")
        );
        assert!(
            status
                .search
                .recommended
                .iter()
                .any(|message| { message.contains("bcode auth login exa") })
        );
    }

    #[test]
    fn explicit_perplexity_provider_is_accepted_without_credentials() {
        let context = plugin_context_with_exa(None);
        let credentials = ProviderCredentials::new(&context);
        let provider = search_provider(
            Some("perplexity"),
            &WebSearchConfig::default(),
            &credentials,
        )
        .expect("perplexity provider");
        assert_eq!(provider, "perplexity");
    }

    #[test]
    fn duckduckgo_html_parser_extracts_results() {
        let html = r#"
            <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs&amp;rut=x">Example &amp; Docs</a>
            <a class="result__snippet">Useful <b>snippet</b></a>
        "#;
        let results = parse_duckduckgo_html_results(html);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.com/docs");
        assert_eq!(results[0].title, "Example & Docs");
        assert_eq!(results[0].snippet, "Useful snippet");
    }

    #[test]
    fn status_reports_default_search_and_fetch_capabilities() {
        let context = plugin_context_with_exa(None);
        let status = status_response(
            &WebSearchConfig::default(),
            &ProviderCredentials::new(&context),
        );
        assert!(status.search.available);
        assert_eq!(status.search.provider.as_deref(), Some("duckduckgo_html"));
        assert_eq!(status.search.quality, "best_effort");
        assert!(status.fetch.fallbacks.contains(&"jina_reader".to_string()));
    }

    #[test]
    fn jina_reader_url_wraps_original_url() {
        assert_eq!(
            jina_reader_url("https://example.com/docs"),
            "https://r.jina.ai/http://https://example.com/docs"
        );
    }

    #[test]
    fn inspect_recommends_specialized_tools_for_developer_resources() {
        let github = inspect_url("https://github.com/bmorphism/bcode").expect("github url");
        assert_eq!(github.kind, "git_repository");
        assert_eq!(github.recommended_tool.as_deref(), Some("git.clone"));

        let pdf = inspect_url("https://example.com/paper.pdf").expect("pdf url");
        assert_eq!(pdf.kind, "pdf");
        assert_eq!(pdf.recommended_tool.as_deref(), Some("document.extract"));

        let youtube = inspect_url("https://youtu.be/example").expect("youtube url");
        assert_eq!(youtube.kind, "youtube_video");
        assert_eq!(
            youtube.recommended_tool.as_deref(),
            Some("media.transcript")
        );
    }
    #[test]
    fn model_native_search_uses_nested_service_and_decodes_response() {
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        let bridge = ServiceBridge::new(
            Some(test_model_native_bridge),
            std::ptr::null_mut(),
            cancellation,
        );
        let response = search_model_native(
            &SearchRequest {
                query: "rust".to_string(),
                provider: Some("model_native".to_string()),
                max_results: Some(3),
                site: None,
                freshness: None,
                region: None,
                safe_search: None,
                timeout_ms: None,
                provider_options: None,
            },
            &bridge,
            "call-native",
            &serde_json::json!({
                "model_provider_route_id": "test-provider-route"
            }),
        )
        .expect("model-native nested service succeeds");

        assert_eq!(response.provider, "provider-native");
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].title, "Rust");
    }

    extern "C" fn test_model_native_bridge(
        request_ptr: *const u8,
        request_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        let request = unsafe { std::slice::from_raw_parts(request_ptr, request_len) };
        let request: ServiceBridgeRequest =
            serde_json::from_slice(request).expect("bridge request decodes");
        let ServiceBridgeRequest::InvokeService(request) = request else {
            panic!("expected nested service request");
        };
        assert_eq!(request.invocation_id, "call-native");
        assert_eq!(request.route_id.as_deref(), Some("test-provider-route"));
        assert_eq!(request.interface_id, MODEL_PROVIDER_SERVICE_INTERFACE);
        assert_eq!(request.operation, MODEL_NATIVE_WEB_SEARCH_OPERATION);
        let response = ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded {
            payload: serde_json::json!({
                "provider": "provider-native",
                "results": [{
                    "title": "Rust",
                    "url": "https://www.rust-lang.org/",
                    "snippet": "Rust language"
                }],
                "partial": false
            }),
        });
        let encoded = serde_json::to_vec(&response).expect("bridge response encodes");
        assert!(encoded.len() <= output_capacity);
        unsafe {
            std::ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
            *output_len = encoded.len();
        }
        bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_OK
    }
}
