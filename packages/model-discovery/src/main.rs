#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "bcode-model-discovery")]
#[command(about = "Generate live model catalog snapshots")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate an Amazon Bedrock live model snapshot.
    Bedrock {
        /// Comma-separated AWS regions to query.
        #[arg(long, value_delimiter = ',')]
        regions: Vec<String>,
        /// Output JSON path.
        #[arg(long)]
        output: PathBuf,
    },
    /// Generate an xAI live model snapshot.
    Xai {
        /// xAI API key (falls back to `XAI_API_KEY` env var).
        #[arg(long)]
        api_key: Option<String>,
        /// Output JSON path.
        #[arg(long)]
        output: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::Bedrock { regions, output } => {
            let regions = if regions.is_empty() {
                regions_from_env()
            } else {
                regions
            };
            if regions.is_empty() {
                return Err("pass --regions or set BCODE_MODEL_DISCOVERY_BEDROCK_REGIONS".into());
            }
            let snapshot = bcode_model_discovery::bedrock::discover(&regions).await?;
            bcode_model_discovery::write_snapshot(&output, &snapshot)?;
            println!(
                "wrote {} Bedrock live models to {}",
                snapshot.models.len(),
                output.display()
            );
        }
        Command::Xai { api_key, output } => {
            let snapshot = bcode_model_discovery::xai::discover(api_key).await?;
            bcode_model_discovery::write_snapshot(&output, &snapshot)?;
            println!(
                "wrote {} xAI live models to {}",
                snapshot.models.len(),
                output.display()
            );
        }
    }
    Ok(())
}

fn regions_from_env() -> Vec<String> {
    std::env::var("BCODE_MODEL_DISCOVERY_BEDROCK_REGIONS")
        .map(|regions| {
            regions
                .split(',')
                .map(str::trim)
                .filter(|region| !region.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}
