#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_model_catalog::{
    OutputFormat, build_artifacts_with_live, default_source_dir, load_catalog,
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
    }
    Ok(())
}
