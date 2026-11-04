#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! `bcode-git`: the bcode git client as a standalone binary.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;

/// Interactive terminal git client.
#[derive(Debug, Parser)]
#[command(name = "bcode-git", version, about)]
struct Args {
    /// Run as if started in this directory.
    #[arg(short = 'C', long = "path", value_name = "DIR")]
    path: Option<PathBuf>,
    /// Print the configuration files that would be read and exit.
    #[arg(long)]
    print_config_paths: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let path = args
        .path
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let root = bcode_git_ui::find_repo_root(&path);
    if args.print_config_paths {
        for file in bcode_git_ui::config::config_files(root.as_deref()) {
            println!("{}", file.display());
        }
        return ExitCode::SUCCESS;
    }
    let config = bcode_git_ui::config::load(root.as_deref());
    let options = bcode_git_ui::RunOptions {
        path,
        config,
        host: Arc::new(bcode_git_ui::host::StandaloneHost),
    };
    match bcode_git_ui::run(options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bcode-git: {error}");
            ExitCode::FAILURE
        }
    }
}
