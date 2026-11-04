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
            Some(path) => path,
            None => std::env::current_dir().map_err(|error| error.to_string())?,
        };
        let root = bcode_git_ui::find_repo_root(&path);
        let config = bcode_git_ui::config::load(root.as_deref());
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
        })
        .await
        .map_err(|error| error.to_string())?;
        Ok(StaticCliOutcome::default())
    })
}
