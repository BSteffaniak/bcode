#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled web search and page fetching tool plugin for Bcode.

use bcode_model_provider_runtime::ProviderRuntime;
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolDefinition,
    ToolInvocationHostAction, ToolInvocationRequest, ToolInvocationResponse,
    ToolInvocationStreamEvent, ToolList, ToolPresentationField, ToolPresentationFieldKind,
    ToolRequestPresentationMetadata, ToolSideEffect,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::time::Duration;
use thiserror::Error;

const DEFAULT_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_MAX_RESULTS: usize = 8;
const DEFAULT_FETCH_MAX_BYTES: usize = 256 * 1024;
const MAX_FETCH_BYTES: usize = 2 * 1024 * 1024;
const USER_AGENT: &str = concat!("Bcode/", env!("CARGO_PKG_VERSION"));

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
        let event = ToolInvocationStreamEvent::Status {
            tool_call_id: self.tool_call_id.clone(),
            sequence,
            message: message.into(),
        };
        if let Ok(payload) = serde_json::to_vec(&event) {
            self.events.emit(&payload);
        }
    }
}

/// Bundled web search plugin.
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

impl WebSearchPlugin {
    fn invoke_tool_service(&self, context: &NativeServiceContext) -> ServiceResponse {
        match context.request.operation.as_str() {
            OP_LIST_TOOLS => list_tools(&context.request, &context.config),
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
        let response = match invocation.name.as_str() {
            "web.search" => self.invoke_search(
                &context.config,
                &context.cancellation,
                &invocation,
                context.events,
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
                host_action: None,
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
        let progress = ProgressReporter::new(events, invocation.tool_call_id.clone());
        progress.emit(format!("search: query {}", request.query));
        match runtime.block_on(run_cancellable(
            search_async(request, plugin_config, Some(progress)),
            cancellation.clone(),
        )) {
            Ok(Ok(response)) => search_tool_response(&response),
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
            Ok(Ok(response)) => json_tool_response(&response),
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
    json_tool_response(&status_response(&plugin_config))
}

fn invoke_inspect(invocation: &ToolInvocationRequest) -> ToolInvocationResponse {
    let request = match serde_json::from_value::<InspectRequest>(invocation.arguments.clone()) {
        Ok(request) => request,
        Err(error) => return tool_error(error.to_string()),
    };
    match inspect_url(&request.url) {
        Ok(response) => json_tool_response(&response),
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
    Value { value: String },
}

impl SecretRef {
    fn resolve(&self) -> Option<String> {
        match self {
            Self::Env { name } => env_value(&[name.as_str()]),
            Self::Sshenv => None,
            Self::Value { value } => Some(value.clone()),
        }
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
}

#[derive(Debug, Clone, Serialize)]
struct SearchResponse {
    query: String,
    provider: String,
    results: Vec<SearchResult>,
    partial: bool,
    message: Option<String>,
    #[serde(skip)]
    host_action: Option<ToolInvocationHostAction>,
}

#[derive(Debug, Clone, Serialize)]
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
struct ExaSearchResponse {
    #[serde(default)]
    results: Vec<ExaResult>,
}

#[derive(Debug, Clone, Deserialize)]
struct ExaResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    published_date: Option<String>,
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
    progress: Option<ProgressReporter>,
) -> Result<SearchResponse, WebError> {
    validate_non_empty("query", &request.query)?;
    let provider = search_provider(request.provider.as_deref(), &config)?;
    if let Some(progress) = &progress {
        progress.emit(format!("search: provider selected: {provider}"));
    }
    let response = match provider.as_str() {
        "brave" => search_brave(request, &config).await,
        "tavily" => search_tavily(request, &config).await,
        "exa" => search_exa(request, &config).await,
        "perplexity" | "pplx" => search_perplexity(request, &config).await,
        "gemini" | "google_gemini" => search_gemini(request, &config).await,
        "serper" => search_serper(request, &config).await,
        "serpapi" | "serp_api" => search_serpapi(request, &config).await,
        "model_native" => Ok(SearchResponse {
            query: request.query.clone(),
            provider: "model_native".to_string(),
            results: Vec::new(),
            partial: true,
            message: Some(
                "model-native web search requested through host provider bridge".to_string(),
            ),
            host_action: Some(ToolInvocationHostAction::HostModelNativeWebSearch(
                bcode_tool::HostModelNativeWebSearchRequest {
                    query: request.query,
                    max_results: request.max_results,
                    site: request.site,
                    freshness: request.freshness,
                    region: request.region,
                    safe_search: request.safe_search,
                },
            )),
        }),
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
) -> Result<SearchResponse, WebError> {
    let api_key = provider_key(&config.providers.exa, &["EXA_API_KEY"])?;
    let max_results = max_results(&request, config);
    let client = client(request.timeout_ms.or(config.timeout_ms))?;
    let body = json!({
        "query": scoped_query(&request),
        "numResults": max_results,
        "contents": { "text": { "maxCharacters": 500 } }
    });
    let text = checked_text(
        client
            .post("https://api.exa.ai/search")
            .header("Accept", "application/json")
            .header("x-api-key", api_key)
            .json(&body)
            .send()
            .await?,
    )
    .await?;
    let decoded = serde_json::from_str::<ExaSearchResponse>(&text)?;
    let results = decoded
        .results
        .into_iter()
        .filter(|result| !result.url.is_empty())
        .take(max_results)
        .map(|result| SearchResult {
            title: result
                .title
                .map_or_else(String::new, |title| html_text(&title)),
            url: result.url,
            snippet: result
                .text
                .map_or_else(String::new, |text| html_text(&text)),
            published: result.published_date,
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
fn list_tools(
    request: &ServiceRequest,
    config: &bcode_plugin_sdk::PluginConfigContext,
) -> ServiceResponse {
    if let Err(error) = request.payload_json::<ListToolsRequest>() {
        return invalid_request(&error);
    }
    let plugin_config = config
        .typed_or_default::<WebSearchConfig>()
        .unwrap_or_else(|_| WebSearchConfig::default());
    let mut tools = Vec::new();
    if search_provider(None, &plugin_config).is_ok() {
        tools.push(search_tool_definition());
    }
    tools.push(fetch_tool_definition());
    tools.push(status_tool_definition());
    tools.push(inspect_tool_definition());
    json_response(&ToolList { tools })
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
                "site": { "type": "string", "description": "Optional domain to restrict results with a site: query" },
                "freshness": { "type": "string", "description": "Provider-specific freshness filter such as day, week, month, or year" },
                "region": { "type": "string", "description": "Provider-specific country/region code" },
                "safe_search": { "type": "string", "description": "Provider-specific safe-search setting" },
                "timeout_ms": { "type": "integer", "minimum": 1 }
            }
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: bcode_tool::ToolPolicyMetadata::default(),
        ui: bcode_tool::ToolUiMetadata {
            activity_label: Some("searching".to_string()),
            request_presentation: Some(ToolRequestPresentationMetadata {
                title: "Search web".to_string(),
                fields: vec![
                    ToolPresentationField {
                        label: "Query".to_string(),
                        argument: "query".to_string(),
                        kind: ToolPresentationFieldKind::Text,
                        optional: false,
                    },
                    ToolPresentationField {
                        label: "Provider".to_string(),
                        argument: "provider".to_string(),
                        kind: ToolPresentationFieldKind::Text,
                        optional: true,
                    },
                    ToolPresentationField {
                        label: "Max results".to_string(),
                        argument: "max_results".to_string(),
                        kind: ToolPresentationFieldKind::Count,
                        optional: true,
                    },
                ],
            }),
        },
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
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: true,
        policy: bcode_tool::ToolPolicyMetadata {
            aliases: vec!["web".to_string()],
            permission_category: Some("web".to_string()),
            argument_extractors: vec![bcode_tool::ToolArgumentExtractor {
                kind: bcode_tool::ToolArgumentKind::Url,
                argument: "url".to_string(),
            }],
        },
        ui: bcode_tool::ToolUiMetadata {
            activity_label: Some("fetching".to_string()),
            request_presentation: Some(ToolRequestPresentationMetadata {
                title: "Fetch URL".to_string(),
                fields: vec![
                    ToolPresentationField {
                        label: "URL".to_string(),
                        argument: "url".to_string(),
                        kind: ToolPresentationFieldKind::Url,
                        optional: false,
                    },
                    ToolPresentationField {
                        label: "Max bytes".to_string(),
                        argument: "max_bytes".to_string(),
                        kind: ToolPresentationFieldKind::Count,
                        optional: true,
                    },
                    ToolPresentationField {
                        label: "Rendered".to_string(),
                        argument: "render".to_string(),
                        kind: ToolPresentationFieldKind::Boolean,
                        optional: true,
                    },
                ],
            }),
        },
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
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: bcode_tool::ToolPolicyMetadata::default(),
        ui: bcode_tool::ToolUiMetadata::default(),
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
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: bcode_tool::ToolPolicyMetadata::default(),
        ui: bcode_tool::ToolUiMetadata::default(),
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

fn status_response(config: &WebSearchConfig) -> WebStatusResponse {
    let provider = search_provider(None, config).ok();
    let configured = configured_search_providers(config);
    let quality = provider
        .as_deref()
        .map_or("unavailable", provider_quality)
        .to_string();
    let mut recommended = Vec::new();
    if matches!(provider.as_deref(), Some("duckduckgo_html") | None) {
        recommended.push(
            "Configure Brave, Tavily, Exa, Perplexity, Gemini, Serper, SerpAPI, or model-native search for more stable results."
                .to_string(),
        );
    }
    let fallbacks = fetch_fallbacks(config);
    WebStatusResponse {
        search: SearchStatus {
            available: provider.is_some(),
            provider,
            quality,
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

fn provider_quality(provider: &str) -> &'static str {
    match provider {
        "duckduckgo_html" => "best_effort",
        "model_native" => "model_provider_native",
        _ => "configured_api",
    }
}

fn configured_search_providers(config: &WebSearchConfig) -> Vec<String> {
    let mut providers = Vec::new();
    if config
        .providers
        .brave
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve)
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
        .and_then(SecretRef::resolve)
        .is_some()
        || env_value(&["TAVILY_API_KEY"]).is_some()
    {
        providers.push("tavily".to_string());
    }
    if config
        .providers
        .exa
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve)
        .is_some()
        || env_value(&["EXA_API_KEY"]).is_some()
    {
        providers.push("exa".to_string());
    }
    if config
        .providers
        .perplexity
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve)
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
        .and_then(SecretRef::resolve)
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
        .and_then(SecretRef::resolve)
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
        .and_then(SecretRef::resolve)
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
        host_action: None,
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

fn search_provider(explicit: Option<&str>, config: &WebSearchConfig) -> Result<String, WebError> {
    let provider = explicit
        .map(str::to_string)
        .or_else(|| config.provider.clone())
        .or_else(|| env_value(&["BCODE_WEB_SEARCH_PROVIDER"]))
        .unwrap_or_else(|| "auto".to_string())
        .trim()
        .to_ascii_lowercase();
    if provider != "auto" {
        return Ok(provider);
    }
    if config
        .providers
        .brave
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve)
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
        .and_then(SecretRef::resolve)
        .is_some()
        || env_value(&["TAVILY_API_KEY"]).is_some()
    {
        return Ok("tavily".to_string());
    }
    if config
        .providers
        .exa
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve)
        .is_some()
        || env_value(&["EXA_API_KEY"]).is_some()
    {
        return Ok("exa".to_string());
    }
    if config
        .providers
        .perplexity
        .api_key
        .as_ref()
        .and_then(SecretRef::resolve)
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
        .and_then(SecretRef::resolve)
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
        .and_then(SecretRef::resolve)
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
        .and_then(SecretRef::resolve)
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
        .and_then(SecretRef::resolve)
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

fn search_tool_response(value: &SearchResponse) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value) {
        Ok(output) => ToolInvocationResponse {
            output,
            is_error: false,
            content: Vec::new(),
            full_output: None,
            host_action: value.host_action.clone(),
            result: None,
        },
        Err(error) => tool_error(error.to_string()),
    }
}

fn json_tool_response<T: Serialize>(value: &T) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value) {
        Ok(output) => ToolInvocationResponse {
            output,
            is_error: false,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        },
        Err(error) => tool_error(error.to_string()),
    }
}

const fn tool_error(output: String) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output,
        is_error: true,
        content: Vec::new(),
        full_output: None,
        host_action: None,
        result: None,
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(WebSearchPlugin, include_str!("../bcode-plugin.toml"))
}

bcode_plugin_sdk::export_plugin!(WebSearchPlugin, include_str!("../bcode-plugin.toml"));
#[cfg(test)]
mod tests {
    use super::*;

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
        let provider = search_provider(None, &WebSearchConfig::default())
            .expect("best-effort fallback should resolve");
        assert_eq!(provider, "duckduckgo_html");
    }

    #[test]
    fn auto_provider_can_disable_best_effort_fallback() {
        let config = WebSearchConfig {
            allow_best_effort_no_key: false,
            ..WebSearchConfig::default()
        };
        assert!(search_provider(None, &config).is_err());
    }

    #[test]
    fn auto_provider_prefers_perplexity_when_keyed() {
        let config = WebSearchConfig {
            allow_best_effort_no_key: true,
            providers: WebSearchProviderConfig {
                perplexity: ProviderConfig {
                    api_key: Some(SecretRef::Value {
                        value: "pplx-test".to_string(),
                    }),
                },
                ..WebSearchProviderConfig::default()
            },
            ..WebSearchConfig::default()
        };
        let provider = search_provider(None, &config).expect("perplexity provider");
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
        let status = status_response(&WebSearchConfig::default());
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
}
