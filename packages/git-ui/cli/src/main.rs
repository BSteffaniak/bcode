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
    /// Extra configuration file, layered above every other bcode-git file.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Inline configuration TOML, the topmost layer.
    #[arg(long, value_name = "TOML")]
    config_toml: Option<String>,
    /// Print which configuration files are read, in order, and exit.
    #[arg(long)]
    print_config_paths: bool,
    /// Print the effective configuration as a bcode-git config file and exit.
    #[arg(long)]
    print_config: bool,
    /// Print settings converted from a lazygit config.yml (found automatically when no path
    /// is given) and exit.
    #[arg(long, value_name = "CONFIG_YML")]
    #[allow(clippy::option_option)]
    import_lazygit: Option<Option<PathBuf>>,
    /// With --print-config or --import-lazygit, emit the bcode.toml form instead.
    #[arg(long)]
    nested: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let path = args
        .path
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let path = std::path::absolute(&path).unwrap_or(path);
    let config_options = bcode_git_ui::config::ConfigOptions {
        file: args.config,
        toml: args.config_toml,
    };
    let command = if let Some(source) = args.import_lazygit {
        Some(bcode_git_ui::config::ConfigCommand::ImportLazygit {
            source,
            nested: args.nested,
        })
    } else if args.print_config {
        Some(bcode_git_ui::config::ConfigCommand::Print {
            nested: args.nested,
        })
    } else if args.print_config_paths {
        Some(bcode_git_ui::config::ConfigCommand::PrintPaths)
    } else {
        None
    };
    if let Some(command) = command {
        return match bcode_git_ui::config::run_command(&path, &config_options, command) {
            Ok(text) => {
                print!("{text}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("bcode-git: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let config = bcode_git_ui::config::load_with(&path, &config_options);
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
