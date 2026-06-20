#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_model_catalog::{
    ModelCatalog, OutputFormat, RemoteCatalogClient, RemoteCatalogOptions,
    build_artifacts_with_live, default_source_dir, load_catalog,
};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

const DEFAULT_VERIFY_PROMPT: &str = "say ok";
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Debug, Parser)]
#[command(name = "bcode-model-catalog")]
#[command(about = "Validate and build Bcode model catalog artifacts")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate catalog source files.
    Validate {
        /// Source directory containing providers/*.toml.
        #[arg(long, default_value = "catalog/models")]
        source: PathBuf,
    },
    /// Build static catalog artifacts.
    Build {
        /// Source directory containing providers/*.toml.
        #[arg(long, default_value = "catalog/models")]
        source: PathBuf,
        /// Output directory for generated artifacts.
        #[arg(long)]
        output: PathBuf,
        /// Directory containing generated live snapshot JSON files.
        #[arg(long)]
        live: Option<PathBuf>,
        /// Output format.
        #[arg(long, default_value = "pretty-json")]
        format: CliOutputFormat,
    },
    /// Show bundled/remote catalog status.
    Status,
    /// Verify catalog/provider models with a tiny live completion request.
    Verify {
        /// Provider id to verify. Currently supports `openai`.
        #[arg(long, default_value = "openai")]
        provider: String,
        /// Prompt sent to each model.
        #[arg(long, default_value = DEFAULT_VERIFY_PROMPT)]
        prompt: String,
        /// Maximum number of models to verify after filtering.
        #[arg(long)]
        max_models: Option<usize>,
        /// Model id wildcard filter. Supports `*` globs.
        #[arg(long)]
        id_pattern: Option<String>,
        /// Only verify models present in the catalog but not returned by the provider `/models` API.
        #[arg(long)]
        catalog_only: bool,
        /// Only verify models returned by the provider `/models` API.
        #[arg(long)]
        discovered_only: bool,
        /// Print candidate models without sending verification requests.
        #[arg(long)]
        dry_run: bool,
        /// Output JSON report path.
        #[arg(long)]
        output: Option<PathBuf>,
        /// OpenAI-compatible API base URL.
        #[arg(long)]
        base_url: Option<String>,
        /// Request timeout in seconds.
        #[arg(long, default_value_t = 20)]
        timeout_seconds: u64,
        /// Accepted for workflow/local UX; requests currently run sequentially to avoid rate-limit spikes.
        #[arg(long, default_value_t = 1)]
        concurrency: usize,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliOutputFormat {
    Json,
    PrettyJson,
}

impl From<CliOutputFormat> for OutputFormat {
    fn from(value: CliOutputFormat) -> Self {
        match value {
            CliOutputFormat::Json => Self::Json,
            CliOutputFormat::PrettyJson => Self::PrettyJson,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModelItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct OpenAiModelItem {
    id: String,
}

#[derive(Debug, Serialize)]
struct VerificationReport {
    provider: String,
    verified_at: String,
    prompt: String,
    base_url: String,
    dry_run: bool,
    total_models: usize,
    results: BTreeMap<String, VerificationResult>,
}

#[derive(Debug, Serialize)]
struct VerificationResult {
    status: VerificationStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    latency_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum VerificationStatus {
    DryRun,
    Working,
    Unauthorized,
    NotFound,
    RateLimited,
    Timeout,
    ProviderError,
    NetworkError,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::Validate { source } => {
            let source = if source.as_os_str().is_empty() {
                default_source_dir()
            } else {
                source
            };
            let catalog = load_catalog(&source)?;
            println!(
                "validated {} providers from {}",
                catalog.providers.len(),
                source.display()
            );
        }
        Command::Build {
            source,
            live,
            output,
            format,
        } => build_artifacts_with_live(&source, live.as_deref(), &output, format.into())?,
        Command::Status => print_status()?,
        Command::Verify {
            provider,
            prompt,
            max_models,
            id_pattern,
            catalog_only,
            discovered_only,
            dry_run,
            output,
            base_url,
            timeout_seconds,
            concurrency,
        } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(verify_models(VerifyOptions {
                provider,
                prompt,
                max_models,
                id_pattern,
                catalog_only,
                discovered_only,
                dry_run,
                output,
                base_url,
                timeout_seconds,
                concurrency,
            }))?;
        }
    }
    Ok(())
}

fn print_status() -> Result<(), Box<dyn std::error::Error>> {
    let bundled = load_catalog(&default_source_dir())?;
    let options = RemoteCatalogOptions::default();
    println!("bundled_revision={}", bundled.catalog_revision);
    println!("bundled_generated_at={}", bundled.generated_at);
    println!("bundled_providers={}", bundled.providers.len());
    println!("remote_url={}", options.base_url);
    println!("remote_cache_dir={}", options.cache_dir.display());
    println!("remote_disabled={}", options.disabled);
    if options.disabled {
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let client = RemoteCatalogClient::new(options)?;
    match runtime.block_on(client.fetch_catalog()) {
        Ok(remote) => {
            println!("remote_status=available");
            println!("remote_revision={}", remote.catalog_revision);
            println!("remote_generated_at={}", remote.generated_at);
            println!("remote_providers={}", remote.providers.len());
        }
        Err(error) => {
            println!("remote_status=unavailable");
            println!("remote_error={error}");
        }
    }
    Ok(())
}

struct VerifyOptions {
    provider: String,
    prompt: String,
    max_models: Option<usize>,
    id_pattern: Option<String>,
    catalog_only: bool,
    discovered_only: bool,
    dry_run: bool,
    output: Option<PathBuf>,
    base_url: Option<String>,
    timeout_seconds: u64,
    concurrency: usize,
}

async fn verify_models(options: VerifyOptions) -> Result<(), Box<dyn std::error::Error>> {
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
    if options.concurrency > 1 {
        eprintln!(
            "warning: --concurrency is accepted for workflow compatibility, but verification currently runs sequentially"
        );
    }

    let base_url = options
        .base_url
        .clone()
        .or_else(|| std::env::var("BCODE_OPENAI_BASE_URL").ok())
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
        .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string());
    let api_key = std::env::var("BCODE_OPENAI_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .ok();
    if api_key.is_none() && !options.dry_run {
        return Err(
            "BCODE_OPENAI_API_KEY or OPENAI_API_KEY is required unless --dry-run is used".into(),
        );
    }

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(options.timeout_seconds))
        .user_agent(concat!("bcode-model-verifier/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let discovered = discover_openai_models(&http, &base_url, api_key.as_deref()).await;
    let candidates = verification_candidates(&options, &discovered).await?;
    let results =
        verify_candidates(&options, &http, &base_url, api_key.as_deref(), &candidates).await;

    write_report(&options, base_url, candidates.len(), results)?;
    Ok(())
}

async fn discover_openai_models(
    http: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
) -> Vec<String> {
    if let Some(api_key) = api_key {
        fetch_openai_model_ids(http, base_url, api_key)
            .await
            .unwrap_or_else(|error| {
                eprintln!("warning: failed to fetch provider model list: {error}");
                Vec::new()
            })
    } else {
        Vec::new()
    }
}

async fn verification_candidates(
    options: &VerifyOptions,
    discovered: &[String],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let discovered_set = discovered
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let catalog = ModelCatalog::load_bundled_with_remote_overlay().await?;
    let mut candidates = catalog
        .provider_models_as_model_info(&options.provider)
        .into_iter()
        .map(|model| model.model_id)
        .chain(discovered.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>()
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
    options: &VerifyOptions,
    http: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
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
            verify_openai_model(
                http,
                base_url,
                api_key.unwrap_or_default(),
                model_id,
                &options.prompt,
            )
            .await
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
    options: &VerifyOptions,
    base_url: String,
    total_models: usize,
    results: BTreeMap<String, VerificationResult>,
) -> Result<(), Box<dyn std::error::Error>> {
    let report = VerificationReport {
        provider: options.provider.clone(),
        verified_at: unix_timestamp_string(),
        prompt: options.prompt.clone(),
        base_url,
        dry_run: options.dry_run,
        total_models,
        results,
    };
    let body = serde_json::to_string_pretty(&report)?;
    if let Some(output) = &options.output {
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(output, body)?;
        println!("wrote {}", output.display());
    } else {
        println!("{body}");
    }
    Ok(())
}

async fn fetch_openai_model_ids(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let response = http
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .bearer_auth(api_key)
        .send()
        .await?
        .error_for_status()?;
    let mut models = response.json::<OpenAiModelsResponse>().await?.data;
    models.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(models.into_iter().map(|model| model.id).collect())
}

async fn verify_openai_model(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model_id: &str,
    prompt: &str,
) -> VerificationResult {
    let start = Instant::now();
    let body = serde_json::json!({
        "model": model_id,
        "input": prompt,
        "max_output_tokens": 16,
    });
    let response = http
        .post(format!("{}/responses", base_url.trim_end_matches('/')))
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await;
    match response {
        Ok(response) => {
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
