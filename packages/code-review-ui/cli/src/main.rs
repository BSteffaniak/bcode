#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! `bcode-review`: the bcode code review app as a standalone binary.
//!
//! It takes the same subcommands as `bcode review` and opens sessions with `bcode` from the
//! path.

use std::path::PathBuf;
use std::process::ExitCode;

use bcode_code_review_ui::cli::ReviewCommand;
use clap::Parser;

/// Interactive terminal code review.
#[derive(Debug, Parser)]
#[command(name = "bcode-review", version, about)]
struct Args {
    /// Run as if started in this directory.
    #[arg(short = 'C', long = "path", value_name = "DIR")]
    path: Option<PathBuf>,
    /// Print which configuration files are read, in order, and exit.
    #[arg(long)]
    print_config_paths: bool,
    /// What to open or do; the review home when absent.
    #[command(subcommand)]
    command: Option<ReviewCommand>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    if let Some(path) = &args.path
        && let Err(error) = std::env::set_current_dir(path)
    {
        eprintln!("bcode-review: {}: {error}", path.display());
        return ExitCode::FAILURE;
    }
    if args.print_config_paths {
        let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let config = bcode_code_review_ui::config::load(&start);
        for file in &config.files {
            println!("{}", file.display());
        }
        for diagnostic in &config.diagnostics {
            eprintln!("bcode-review: {diagnostic}");
        }
        return ExitCode::SUCCESS;
    }
    match bcode_code_review_ui::cli::run(args.command, None).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bcode-review: {error}");
            ExitCode::FAILURE
        }
    }
}
