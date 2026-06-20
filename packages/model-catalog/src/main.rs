#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_model_catalog::{
    OutputFormat, RemoteCatalogClient, RemoteCatalogOptions, build_artifacts_with_live,
    default_source_dir, load_catalog,
};
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
