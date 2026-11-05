//! `bcode git`: the interactive git client, contributed by the git plugin.

use std::path::PathBuf;
use std::sync::Arc;

use bcode_plugin_sdk::{StaticCliFuture, StaticCliOutcome, StaticCliRegistration};
use clap::{CommandFactory, FromArgMatches, Parser};

#[derive(Debug, Parser)]
#[command(name = "git", about = "Interactive git client (lazygit-style)")]
struct GitCli {
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

pub fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: |_| false,
        command: GitCli::command,
        invoke,
    }
}

fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let cli = GitCli::from_arg_matches(&matches).map_err(|error| error.to_string())?;
        let path = match cli.path {
            Some(path) => std::path::absolute(&path).unwrap_or(path),
            None => std::env::current_dir().map_err(|error| error.to_string())?,
        };
        let options = bcode_git_ui::config::ConfigOptions {
            file: cli.config,
            toml: cli.config_toml,
        };
        let command = if let Some(source) = cli.import_lazygit {
            Some(bcode_git_ui::config::ConfigCommand::ImportLazygit {
                source,
                nested: cli.nested,
            })
        } else if cli.print_config {
            Some(bcode_git_ui::config::ConfigCommand::Print { nested: cli.nested })
        } else if cli.print_config_paths {
            Some(bcode_git_ui::config::ConfigCommand::PrintPaths)
        } else {
            None
        };
        if let Some(command) = command {
            print!(
                "{}",
                bcode_git_ui::config::run_command(&path, &options, command)?
            );
            return Ok(StaticCliOutcome::default());
        }
        let config = bcode_git_ui::config::load_with(&path, &options);
        if !config.ui_enabled() {
            return Err(
                "bcode git is disabled by configuration ([plugins] disabled or \
                 [plugins.config.\"bcode.git\".ui] enabled = false)"
                    .to_owned(),
            );
        }
        bcode_git_ui::run(bcode_git_ui::RunOptions {
            path,
            config,
            host: Arc::new(bcode_git_ui::host::StandaloneHost),
            commit_message: bcode_git_ui::commit_message_from_env(),
            state_dir: None,
        })
        .await
        .map_err(|error| error.to_string())?;
        Ok(StaticCliOutcome::default())
    })
}
