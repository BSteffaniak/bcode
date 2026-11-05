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
    /// Print bcode configuration converted from a lazygit config.yml (found automatically
    /// when no path is given) and exit.
    #[arg(long, value_name = "CONFIG_YML")]
    #[allow(clippy::option_option)]
    import_lazygit: Option<Option<PathBuf>>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    if let Some(source) = args.import_lazygit {
        return match bcode_git_ui::lazygit::import_file(source) {
            Ok((path, import)) => {
                println!("# Source: {}", path.display());
                print!("{}", import.toml);
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("bcode-git: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let path = args
        .path
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let path = std::path::absolute(&path).unwrap_or(path);
    if args.print_config_paths {
        let loaded = bcode_git_ui::config::load(&path);
        for file in bcode_git_ui::config::config_files(&path) {
            let state = if loaded.files.contains(&file) {
                "read"
            } else {
                "missing"
            };
            println!("{state}\t{}", file.display());
        }
        for diagnostic in &loaded.diagnostics {
            eprintln!("bcode-git: {diagnostic}");
        }
        return ExitCode::SUCCESS;
    }
    let config = bcode_git_ui::config::load(&path);
    let options = bcode_git_ui::RunOptions {
        path,
        config,
        host: Arc::new(bcode_git_ui::host::StandaloneHost),
        commit_message: bcode_git_ui::commit_message_from_env(),
    };
    match bcode_git_ui::run(options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bcode-git: {error}");
            ExitCode::FAILURE
        }
    }
}
