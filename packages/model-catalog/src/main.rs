#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_model_catalog::{
    DEFAULT_VERIFY_PROMPT, OutputFormat, RemoteCatalogClient, RemoteCatalogOptions,
    VerificationAuthMode, VerificationOptions, build_artifacts_with_live, default_source_dir,
    load_catalog, run_verification,
};
use bcode_plugin_sdk::path::display_from_current_dir;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

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
        /// Only verify models present in the catalog but not returned by provider `/models` API.
        #[arg(long)]
        catalog_only: bool,
        /// Only verify models returned by provider `/models` API.
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
        /// Auth/transport mode: auto, api-key, or subscription.
        #[arg(long, default_value = "auto")]
        auth_mode: VerificationAuthMode,
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::Validate { source } => validate_catalog(source)?,
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
            auth_mode,
        } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(run_verification(VerificationOptions {
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
                auth_mode,
            }))?;
        }
    }
    Ok(())
}

fn validate_catalog(source: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let source = if source.as_os_str().is_empty() {
        default_source_dir()
    } else {
        source
    };
    let catalog = load_catalog(&source)?;
    println!(
        "validated {} providers from {}",
        catalog.providers.len(),
        display_from_current_dir(&source)
    );
    Ok(())
}

fn print_status() -> Result<(), Box<dyn std::error::Error>> {
    let bundled = load_catalog(&default_source_dir())?;
    let options = RemoteCatalogOptions::default();
    println!("bundled_revision={}", bundled.catalog_revision);
    println!("bundled_generated_at={}", bundled.generated_at);
    println!("bundled_providers={}", bundled.providers.len());
    println!("remote_url={}", options.base_url);
    println!(
        "remote_cache_dir={}",
        display_from_current_dir(&options.cache_dir)
    );
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
