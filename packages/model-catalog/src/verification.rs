//! Model verification runner and OpenAI-compatible transports.

use crate::ModelCatalog;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};

/// Default prompt for low-cost model verification.
pub const DEFAULT_VERIFY_PROMPT: &str = "say ok";
/// Default `OpenAI` API base URL.
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// Default `ChatGPT` subscription Codex API endpoint.
pub const DEFAULT_CHATGPT_CODEX_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";

/// Auth/transport mode used for verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationAuthMode {
    /// Prefer subscription auth when available, otherwise use API-key auth.
    Auto,
    /// Use OpenAI-compatible API-key auth.
    ApiKey,
    /// Use `ChatGPT` subscription/Codex auth.
    Subscription,
}

impl std::str::FromStr for VerificationAuthMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "api-key" | "api_key" => Ok(Self::ApiKey),
            "subscription" | "chatgpt" => Ok(Self::Subscription),
            _ => Err(format!("unknown auth mode `{value}`")),
        }
    }
}

impl std::fmt::Display for VerificationAuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::ApiKey => "api-key",
            Self::Subscription => "subscription",
        })
    }
}

/// Options for a model verification run.
#[derive(Debug, Clone)]
pub struct VerificationOptions {
    /// Provider id to verify.
    pub provider: String,
    /// Prompt sent to each model.
    pub prompt: String,
    /// Maximum number of models to verify after filtering.
    pub max_models: Option<usize>,
    /// Model id wildcard filter.
    pub id_pattern: Option<String>,
    /// Only verify catalog models absent from provider discovery.
    pub catalog_only: bool,
    /// Only verify provider-discovered models.
    pub discovered_only: bool,
    /// Print candidates without sending requests.
    pub dry_run: bool,
    /// JSON report output path.
    pub output: Option<PathBuf>,
    /// OpenAI-compatible API base URL.
    pub base_url: Option<String>,
    /// Request timeout in seconds.
    pub timeout_seconds: u64,
    /// Requested concurrency. Currently reserved for future parallel execution.
    pub concurrency: usize,
    /// Auth/transport mode.
    pub auth_mode: VerificationAuthMode,
}

/// Tiny request sent to each model verifier transport.
#[derive(Debug, Clone)]
pub struct VerifyModelRequest {
    /// Prompt sent to the model.
    pub prompt: String,
}

/// Transport-independent model verifier.
pub trait ModelVerifier {
    /// Verify one model id.
    fn verify_model<'a>(
        &'a self,
        model_id: &'a str,
        request: &'a VerifyModelRequest,
    ) -> Pin<Box<dyn Future<Output = VerificationResult> + Send + 'a>>;

    /// Fetch provider-discovered model ids, when supported.
    fn discover_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + 'a>>;

    /// Transport label used in reports.
    fn transport(&self) -> &'static str;

    /// Endpoint/base URL used in reports.
    fn endpoint(&self) -> &str;
}

/// Verification report written as JSON.
#[derive(Debug, Serialize)]
pub struct VerificationReport {
    /// Provider id.
    pub provider: String,
    /// Unix timestamp for the run.
    pub verified_at: String,
    /// Prompt sent to models.
    pub prompt: String,
    /// Auth/transport mode used.
    pub auth_mode: String,
    /// Transport implementation label.
    pub transport: String,
    /// Endpoint/base URL used.
    pub endpoint: String,
    /// Whether this was a dry-run.
    pub dry_run: bool,
    /// Number of candidate models.
    pub total_models: usize,
    /// Results keyed by model id.
    pub results: BTreeMap<String, VerificationResult>,
}

/// Verification result for one model.
#[derive(Debug, Clone, Serialize)]
pub struct VerificationResult {
    /// Verification status.
    pub status: VerificationStatus,
    /// End-to-end latency in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u128>,
    /// Provider error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Provider or verifier message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Verification status for one model.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    /// Candidate was listed but not called.
    DryRun,
    /// Model accepted the request.
    Working,
    /// Auth failed or entitlement is missing.
    Unauthorized,
    /// Model id was not found.
    NotFound,
    /// Provider returned a rate limit.
    RateLimited,
    /// Request timed out.
    Timeout,
    /// Provider returned another non-success response.
    ProviderError,
    /// Network/client failure.
    NetworkError,
}

/// Run verification with the selected verifier transport.
///
/// # Errors
///
/// Returns an error when options are invalid, catalog loading fails, or report
/// writing fails.
pub async fn run_verification(
    options: VerificationOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_options(&options)?;
    if options.concurrency > 1 {
        eprintln!(
            "warning: --concurrency is accepted for workflow compatibility, but verification currently runs sequentially"
        );
    }
    let verifier = build_verifier(&options)?;
    let discovered = verifier.discover_models().await.unwrap_or_else(|error| {
        eprintln!("warning: failed to fetch provider model list: {error}");
        Vec::new()
    });
    let candidates = verification_candidates(&options, &discovered).await?;
    let request = VerifyModelRequest {
        prompt: options.prompt.clone(),
    };
    let results = verify_candidates(&options, verifier.as_ref(), &request, &candidates).await;
    write_report(&options, verifier.as_ref(), candidates.len(), results)?;
    Ok(())
}

fn validate_options(options: &VerificationOptions) -> Result<(), Box<dyn std::error::Error>> {
    if options.provider != "openai" {
        return Err(format!(
            "provider `{}` is not supported yet; use --provider openai",
            options.provider
        )
        .into());
    }
    if options.catalog_only && options.discovered_only {
        return Err("--catalog-only and --discovered-only cannot be used together".into());
    }
    Ok(())
}

fn build_verifier(
    options: &VerificationOptions,
) -> Result<Box<dyn ModelVerifier + Send + Sync>, Box<dyn std::error::Error>> {
    match options.auth_mode {
        VerificationAuthMode::ApiKey => OpenAiApiKeyVerifier::from_env(options).map(|v| Box::new(v) as _),
        VerificationAuthMode::Subscription => {
            ChatGptSubscriptionVerifier::from_env(options).map(|v| Box::new(v) as _)
        }
        VerificationAuthMode::Auto => ChatGptSubscriptionVerifier::from_env(options)
            .map(|v| Box::new(v) as _)
            .or_else(|_| OpenAiApiKeyVerifier::from_env(options).map(|v| Box::new(v) as _))
            .map_err(|_| "no verifier auth found; set BCODE_OPENAI_CODEX_ACCESS_TOKEN for subscription auth or BCODE_OPENAI_API_KEY/OPENAI_API_KEY for API-key auth".into()),
    }
}

async fn verification_candidates(
    options: &VerificationOptions,
    discovered: &[String],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let discovered_set = discovered.iter().cloned().collect::<BTreeSet<_>>();
    let catalog = ModelCatalog::load_bundled_with_remote_overlay().await?;
    let mut candidates = catalog
        .provider_models_as_model_info(&options.provider)
        .into_iter()
        .map(|model| model.model_id)
        .chain(discovered.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|model_id| {
            options
                .id_pattern
                .as_ref()
                .is_none_or(|pattern| wildcard_match(pattern, model_id))
        })
        .filter(|model_id| !options.catalog_only || !discovered_set.contains(model_id))
        .filter(|model_id| !options.discovered_only || discovered_set.contains(model_id))
        .collect::<Vec<_>>();
    if let Some(max_models) = options.max_models {
        candidates.truncate(max_models);
    }
    Ok(candidates)
}

async fn verify_candidates(
    options: &VerificationOptions,
    verifier: &(dyn ModelVerifier + Send + Sync),
    request: &VerifyModelRequest,
    candidates: &[String],
) -> BTreeMap<String, VerificationResult> {
    let mut results = BTreeMap::new();
    for model_id in candidates {
        let result = if options.dry_run {
            VerificationResult {
                status: VerificationStatus::DryRun,
                latency_ms: None,
                error_code: None,
                message: None,
            }
        } else {
            verifier.verify_model(model_id, request).await
        };
        println!(
            "{model_id}\t{:?}\t{}",
            result.status,
            result
                .latency_ms
                .map_or_else(|| "-".to_string(), |latency| format!("{latency}ms"))
        );
        results.insert(model_id.clone(), result);
    }
    results
}

fn write_report(
    options: &VerificationOptions,
    verifier: &(dyn ModelVerifier + Send + Sync),
    total_models: usize,
    results: BTreeMap<String, VerificationResult>,
) -> Result<(), Box<dyn std::error::Error>> {
    let report = VerificationReport {
        provider: options.provider.clone(),
        verified_at: unix_timestamp_string(),
        prompt: options.prompt.clone(),
        auth_mode: options.auth_mode.to_string(),
        transport: verifier.transport().to_string(),
        endpoint: verifier.endpoint().to_string(),
        dry_run: options.dry_run,
        total_models,
        results,
    };
    let body = serde_json::to_string_pretty(&report)?;
    if let Some(output) = &options.output {
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(output, body)?;
        println!("wrote {}", output.display());
    } else {
        println!("{body}");
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct OpenAiApiKeyVerifier {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl OpenAiApiKeyVerifier {
    fn from_env(options: &VerificationOptions) -> Result<Self, Box<dyn std::error::Error>> {
        let api_key =
            std::env::var("BCODE_OPENAI_API_KEY").or_else(|_| std::env::var("OPENAI_API_KEY"))?;
        let base_url = options
            .base_url
            .clone()
            .or_else(|| std::env::var("BCODE_OPENAI_BASE_URL").ok())
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string());
        Ok(Self {
            http: http_client(options.timeout_seconds)?,
            base_url,
            api_key,
        })
    }
}

impl ModelVerifier for OpenAiApiKeyVerifier {
    fn verify_model<'a>(
        &'a self,
        model_id: &'a str,
        request: &'a VerifyModelRequest,
    ) -> Pin<Box<dyn Future<Output = VerificationResult> + Send + 'a>> {
        Box::pin(async move {
            let body = serde_json::json!({
                "model": model_id,
                "input": request.prompt,
                "max_output_tokens": 16,
            });
            post_json_verify(
                &self.http,
                &format!("{}/responses", self.base_url.trim_end_matches('/')),
                &self.api_key,
                &body,
            )
            .await
        })
    }

    fn discover_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(
            async move { fetch_openai_model_ids(&self.http, &self.base_url, &self.api_key).await },
        )
    }

    fn transport(&self) -> &'static str {
        "openai_api_key"
    }

    fn endpoint(&self) -> &str {
        &self.base_url
    }
}

#[derive(Debug, Clone)]
struct ChatGptSubscriptionVerifier {
    http: reqwest::Client,
    endpoint: String,
    access_token: String,
}

impl ChatGptSubscriptionVerifier {
    fn from_env(options: &VerificationOptions) -> Result<Self, Box<dyn std::error::Error>> {
        let access_token = std::env::var("BCODE_OPENAI_CODEX_ACCESS_TOKEN")
            .or_else(|_| std::env::var("OPENAI_CODEX_ACCESS_TOKEN"))?;
        let endpoint = std::env::var("BCODE_OPENAI_CODEX_ENDPOINT")
            .or_else(|_| std::env::var("OPENAI_CODEX_ENDPOINT"))
            .unwrap_or_else(|_| DEFAULT_CHATGPT_CODEX_ENDPOINT.to_string());
        Ok(Self {
            http: http_client(options.timeout_seconds)?,
            endpoint,
            access_token,
        })
    }
}

impl ModelVerifier for ChatGptSubscriptionVerifier {
    fn verify_model<'a>(
        &'a self,
        model_id: &'a str,
        request: &'a VerifyModelRequest,
    ) -> Pin<Box<dyn Future<Output = VerificationResult> + Send + 'a>> {
        Box::pin(async move {
            let body = serde_json::json!({
                "model": model_id,
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": request.prompt }]
                }],
                "stream": false,
            });
            post_json_verify(&self.http, &self.endpoint, &self.access_token, &body).await
        })
    }

    fn discover_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn transport(&self) -> &'static str {
        "chatgpt_subscription"
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

fn http_client(timeout_seconds: u64) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_seconds))
        .user_agent(concat!("bcode-model-verifier/", env!("CARGO_PKG_VERSION")))
        .build()
}

#[derive(Debug, Clone, Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModelItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct OpenAiModelItem {
    id: String,
}

async fn fetch_openai_model_ids(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>, String> {
    let response = http
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?;
    let mut models = response
        .json::<OpenAiModelsResponse>()
        .await
        .map_err(|error| error.to_string())?
        .data;
    models.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(models.into_iter().map(|model| model.id).collect())
}

async fn post_json_verify(
    http: &reqwest::Client,
    url: &str,
    bearer_token: &str,
    body: &serde_json::Value,
) -> VerificationResult {
    let start = Instant::now();
    let response = http
        .post(url)
        .bearer_auth(bearer_token)
        .json(body)
        .send()
        .await;
    match response {
        Ok(response) => response_result(response, start).await,
        Err(error) => VerificationResult {
            status: if error.is_timeout() {
                VerificationStatus::Timeout
            } else {
                VerificationStatus::NetworkError
            },
            latency_ms: Some(start.elapsed().as_millis()),
            error_code: None,
            message: Some(error.to_string()),
        },
    }
}

async fn response_result(response: reqwest::Response, start: Instant) -> VerificationResult {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if status.is_success() {
        return VerificationResult {
            status: VerificationStatus::Working,
            latency_ms: Some(start.elapsed().as_millis()),
            error_code: None,
            message: None,
        };
    }
    let (error_code, message) = parse_openai_error(&text);
    VerificationResult {
        status: classify_status(status.as_u16(), error_code.as_deref()),
        latency_ms: Some(start.elapsed().as_millis()),
        error_code,
        message: Some(message.unwrap_or(text)),
    }
}

fn classify_status(status: u16, error_code: Option<&str>) -> VerificationStatus {
    match (status, error_code) {
        (401 | 403, _) => VerificationStatus::Unauthorized,
        (404, _) | (_, Some("model_not_found")) => VerificationStatus::NotFound,
        (429, _) => VerificationStatus::RateLimited,
        _ => VerificationStatus::ProviderError,
    }
}

fn parse_openai_error(body: &str) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return (None, None);
    };
    let error = value.get("error");
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    (code, message)
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let parts = pattern.split('*').collect::<Vec<_>>();
    if parts.len() == 1 {
        return pattern == value;
    }
    let mut remaining = value;
    if let Some(first) = parts.first()
        && !first.is_empty()
    {
        let Some(stripped) = remaining.strip_prefix(first) else {
            return false;
        };
        remaining = stripped;
    }
    for part in parts.iter().skip(1).take(parts.len().saturating_sub(2)) {
        if part.is_empty() {
            continue;
        }
        let Some(index) = remaining.find(part) else {
            return false;
        };
        remaining = &remaining[index + part.len()..];
    }
    if let Some(last) = parts.last()
        && !last.is_empty()
    {
        return remaining.ends_with(last);
    }
    true
}

fn unix_timestamp_string() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or_else(
            |_| "0".to_string(),
            |duration| duration.as_secs().to_string(),
        )
}
