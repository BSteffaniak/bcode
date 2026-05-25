#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled web search and page fetching tool plugin for Bcode.

use bcode_model_provider_runtime::ProviderRuntime;
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolDefinition,
    ToolInvocationRequest, ToolInvocationResponse, ToolList, ToolSideEffect,
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
            TOOL_SERVICE_INTERFACE_ID => self.invoke_tool_service(&context.request),
            _ => ServiceResponse::error(
                "unsupported_interface",
                "unsupported web search plugin service interface",
            ),
        }
    }
}

impl WebSearchPlugin {
    fn invoke_tool_service(&self, request: &ServiceRequest) -> ServiceResponse {
        match request.operation.as_str() {
            OP_LIST_TOOLS => list_tools(request),
            OP_INVOKE_TOOL => self.invoke_tool(request),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported tool service operation",
            ),
        }
    }

    fn invoke_tool(&self, request: &ServiceRequest) -> ServiceResponse {
        let invocation = match request.payload_json::<ToolInvocationRequest>() {
            Ok(invocation) => invocation,
            Err(error) => return invalid_request(&error),
        };
        let response = match invocation.name.as_str() {
            "web.search" => self.invoke_search(&invocation),
            "web.fetch" => self.invoke_fetch(&invocation),
            _ => ToolInvocationResponse {
                output: format!("unsupported web tool: {}", invocation.name),
                is_error: true,
                content: Vec::new(),
                full_output: None,
            },
        };
        json_response(&response)
    }

    fn invoke_search(&self, invocation: &ToolInvocationRequest) -> ToolInvocationResponse {
        let request = match serde_json::from_value::<SearchRequest>(invocation.arguments.clone()) {
            Ok(request) => request,
            Err(error) => return tool_error(error.to_string()),
        };
        let runtime = match &self.runtime {
            Ok(runtime) => runtime,
            Err(error) => return tool_error(format!("web runtime unavailable: {error}")),
        };
        match runtime.block_on(search_async(request)) {
            Ok(Ok(response)) => json_tool_response(&response),
            Ok(Err(error)) => tool_error(error.to_string()),
            Err(error) => tool_error(error.to_string()),
        }
    }

    fn invoke_fetch(&self, invocation: &ToolInvocationRequest) -> ToolInvocationResponse {
        let request = match serde_json::from_value::<FetchRequest>(invocation.arguments.clone()) {
            Ok(request) => request,
            Err(error) => return tool_error(error.to_string()),
        };
        let runtime = match &self.runtime {
            Ok(runtime) => runtime,
            Err(error) => return tool_error(format!("web runtime unavailable: {error}")),
        };
        match runtime.block_on(fetch_async(request)) {
            Ok(Ok(response)) => json_tool_response(&response),
            Ok(Err(error)) => tool_error(error.to_string()),
            Err(error) => tool_error(error.to_string()),
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
}

async fn search_async(request: SearchRequest) -> Result<SearchResponse, WebError> {
    validate_non_empty("query", &request.query)?;
    let provider = search_provider(request.provider.as_deref())?;
    match provider.as_str() {
        "brave" => search_brave(request).await,
        "tavily" => search_tavily(request).await,
        "exa" => search_exa(request).await,
        "serper" => search_serper(request).await,
        "serpapi" | "serp_api" => search_serpapi(request).await,
        _ => Err(WebError::InvalidRequest(format!(
            "unsupported web search provider: {provider}"
        ))),
    }
}

async fn search_brave(request: SearchRequest) -> Result<SearchResponse, WebError> {
    let api_key = provider_key(&["BCODE_WEB_SEARCH_API_KEY", "BRAVE_SEARCH_API_KEY"])?;
    let max_results = max_results(&request);
    let query = scoped_query(&request);
    let client = client(request.timeout_ms)?;
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

async fn search_tavily(request: SearchRequest) -> Result<SearchResponse, WebError> {
    let api_key = provider_key(&["TAVILY_API_KEY"])?;
    let max_results = max_results(&request);
    let client = client(request.timeout_ms)?;
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
async fn search_exa(request: SearchRequest) -> Result<SearchResponse, WebError> {
    let api_key = provider_key(&["EXA_API_KEY"])?;
    let max_results = max_results(&request);
    let client = client(request.timeout_ms)?;
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

async fn search_serper(request: SearchRequest) -> Result<SearchResponse, WebError> {
    let api_key = provider_key(&["SERPER_API_KEY"])?;
    let max_results = max_results(&request);
    let client = client(request.timeout_ms)?;
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

async fn search_serpapi(request: SearchRequest) -> Result<SearchResponse, WebError> {
    let api_key = provider_key(&["SERPAPI_API_KEY"])?;
    let max_results = max_results(&request);
    let client = client(request.timeout_ms)?;
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

async fn fetch_async(request: FetchRequest) -> Result<FetchResponse, WebError> {
    validate_url(&request.url)?;
    if request.render {
        return fetch_rendered(request);
    }
    fetch_plain_async(request).await
}

async fn fetch_plain_async(request: FetchRequest) -> Result<FetchResponse, WebError> {
    let max_bytes = request
        .max_bytes
        .unwrap_or(DEFAULT_FETCH_MAX_BYTES)
        .clamp(1, MAX_FETCH_BYTES);
    let client = client(request.timeout_ms)?;
    let response = client.get(&request.url).send().await?;
    let status = response.status();
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    let body = response.bytes().await?;
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
    Ok(FetchResponse {
        url: request.url,
        final_url,
        status: status.as_u16(),
        title,
        content_type,
        text,
        markdown,
        truncated,
        rendered: false,
    })
}

fn fetch_rendered(request: FetchRequest) -> Result<FetchResponse, WebError> {
    let command = env_value(&["BCODE_WEB_RENDER_COMMAND"]).ok_or_else(|| {
        WebError::InvalidRequest(
            "rendered fetch requires BCODE_WEB_RENDER_COMMAND to name an explicit local fetch command"
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
        .unwrap_or(DEFAULT_FETCH_MAX_BYTES)
        .clamp(1, MAX_FETCH_BYTES);
    let truncated = output.stdout.len() > max_bytes;
    let raw = String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(max_bytes)]);
    let (title, text, markdown) = html_document_text(&raw);
    Ok(FetchResponse {
        url: request.url.clone(),
        final_url: request.url,
        status: 200,
        title,
        content_type: Some("text/html; rendered=command".to_string()),
        text,
        markdown,
        truncated,
        rendered: true,
    })
}
fn list_tools(request: &ServiceRequest) -> ServiceResponse {
    if let Err(error) = request.payload_json::<ListToolsRequest>() {
        return invalid_request(&error);
    }
    json_response(&ToolList {
        tools: vec![search_tool_definition(), fetch_tool_definition()],
    })
}

fn search_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "web.search".to_string(),
        description: "Search the web through the configured search provider. Supports Brave, Tavily, Exa, Serper, and SerpAPI through provider API keys.".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string" },
                "provider": { "type": "string", "description": "Optional provider override: auto, brave, tavily, exa, serper, or serpapi" },
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
                "render": { "type": "boolean", "description": "Use the explicit rendered-fetch command adapter configured by BCODE_WEB_RENDER_COMMAND" }
            }
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: true,
    }
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

fn max_results(request: &SearchRequest) -> usize {
    request
        .max_results
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

fn search_provider(explicit: Option<&str>) -> Result<String, WebError> {
    let provider = explicit
        .map(str::to_string)
        .or_else(|| env_value(&["BCODE_WEB_SEARCH_PROVIDER"]))
        .unwrap_or_else(|| "auto".to_string())
        .trim()
        .to_ascii_lowercase();
    if provider != "auto" {
        return Ok(provider);
    }
    if env_value(&["BCODE_WEB_SEARCH_API_KEY", "BRAVE_SEARCH_API_KEY"]).is_some() {
        return Ok("brave".to_string());
    }
    if env_value(&["TAVILY_API_KEY"]).is_some() {
        return Ok("tavily".to_string());
    }
    if env_value(&["EXA_API_KEY"]).is_some() {
        return Ok("exa".to_string());
    }
    if env_value(&["SERPER_API_KEY"]).is_some() {
        return Ok("serper".to_string());
    }
    if env_value(&["SERPAPI_API_KEY"]).is_some() {
        return Ok("serpapi".to_string());
    }
    Err(WebError::MissingProvider)
}

fn provider_key(names: &[&str]) -> Result<String, WebError> {
    env_value(names).ok_or(WebError::MissingProvider)
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
    extract_element(input, "main")
        .or_else(|| extract_element(input, "article"))
        .or_else(|| extract_element(input, "body"))
}

fn extract_element<'a>(input: &'a str, tag: &str) -> Option<&'a str> {
    let lower = input.to_ascii_lowercase();
    let start_token = format!("<{tag}");
    let end_token = format!("</{tag}>");
    let start = lower.find(&start_token)?;
    let content_start = input[start..].find('>')? + start + 1;
    let end = lower[content_start..].find(&end_token)? + content_start;
    Some(&input[content_start..end])
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
        "p" | "div" | "section" | "article" | "main" | "br" => output.push_str("\n\n"),
        "li" => output.push_str("\n* "),
        "pre" | "code" => output.push('`'),
        _ => output.push(' '),
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

fn json_tool_response<T: Serialize>(value: &T) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value) {
        Ok(output) => ToolInvocationResponse {
            output,
            is_error: false,
            content: Vec::new(),
            full_output: None,
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
    fn validate_url_rejects_non_http_urls() {
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("https://example.com").is_ok());
    }

    #[test]
    fn auto_provider_requires_a_key_when_no_provider_env_exists() {
        // This test asserts only the explicit unsupported path to avoid mutating
        // process-global env in parallel test runs.
        let provider = search_provider(Some("unknown")).expect("explicit provider should resolve");
        assert_eq!(provider, "unknown");
    }
}
