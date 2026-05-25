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
}

#[derive(Debug, Clone, Serialize)]
struct FetchResponse {
    url: String,
    final_url: String,
    status: u16,
    title: Option<String>,
    content_type: Option<String>,
    text: String,
    truncated: bool,
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

#[derive(Debug, Error)]
enum WebError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error(
        "no web search provider configured; set BCODE_WEB_SEARCH_PROVIDER=brave and BCODE_WEB_SEARCH_API_KEY or BRAVE_SEARCH_API_KEY"
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
    let provider = search_provider()?;
    match provider.as_str() {
        "brave" => search_brave(request).await,
        _ => Err(WebError::InvalidRequest(format!(
            "unsupported web search provider: {provider}"
        ))),
    }
}

async fn search_brave(request: SearchRequest) -> Result<SearchResponse, WebError> {
    let api_key = env_value(["BCODE_WEB_SEARCH_API_KEY", "BRAVE_SEARCH_API_KEY"])
        .ok_or(WebError::MissingProvider)?;
    let max_results = request
        .max_results
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, 20);
    let mut query = request.query.trim().to_string();
    if let Some(site) = request
        .site
        .as_deref()
        .map(str::trim)
        .filter(|site| !site.is_empty())
    {
        query = format!("site:{site} {query}");
    }
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
    let response = builder.send().await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(WebError::Http {
            status: status.as_u16(),
            body: truncate_chars(&body, 1_000),
        });
    }
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
    Ok(SearchResponse {
        query: request.query,
        provider: "brave".to_string(),
        results,
        partial: false,
        message: None,
    })
}

async fn fetch_async(request: FetchRequest) -> Result<FetchResponse, WebError> {
    validate_url(&request.url)?;
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
    let text = if content_type
        .as_deref()
        .is_some_and(|value| value.to_ascii_lowercase().contains("html"))
    {
        html_text(&raw)
    } else {
        raw.into_owned()
    };
    Ok(FetchResponse {
        url: request.url,
        final_url,
        status: status.as_u16(),
        title: html_title(&text),
        content_type,
        text,
        truncated,
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
        description: "Search the web through the configured search provider. Requires a provider API key such as Brave Search.".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string" },
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
                "timeout_ms": { "type": "integer", "minimum": 1 }
            }
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: true,
    }
}

fn search_provider() -> Result<String, WebError> {
    env::var("BCODE_WEB_SEARCH_PROVIDER")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            env_value(["BCODE_WEB_SEARCH_API_KEY", "BRAVE_SEARCH_API_KEY"])
                .map(|_| "brave".to_string())
        })
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

fn env_value<const N: usize>(names: [&str; N]) -> Option<String> {
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

fn html_text(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;
    let mut in_entity = false;
    let mut entity = String::new();
    for character in input.chars() {
        if in_tag {
            if character == '>' {
                in_tag = false;
                output.push(' ');
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
    collapse_whitespace(&output)
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

fn html_title(text: &str) -> Option<String> {
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
        },
        Err(error) => tool_error(error.to_string()),
    }
}

const fn tool_error(output: String) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output,
        is_error: true,
        content: Vec::new(),
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
            "Rust & Bcode A test"
        );
    }

    #[test]
    fn validate_url_rejects_non_http_urls() {
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("https://example.com").is_ok());
    }
}
