#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! `bcode-feedback`: the `bcode feedback` commands as a standalone executable, for
//! applications that collect feedback by running a process instead of linking Bcode.

use std::process::ExitCode;

use bcode_feedback_cli::FeedbackCli;
use clap::{CommandFactory as _, FromArgMatches as _};

#[tokio::main]
async fn main() -> ExitCode {
    let matches = FeedbackCli::command()
        .name("bcode-feedback")
        .version(env!("CARGO_PKG_VERSION"))
        .get_matches();
    let cli = match FeedbackCli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };
    let installed = match bcode_client::BcodeClient::for_installed_bcode(
        bcode_client::InstalledDaemonStart::IfNeeded,
    )
    .await
    {
        Ok(installed) => installed,
        Err(error) => {
            eprintln!("bcode-feedback: {error}");
            return ExitCode::FAILURE;
        }
    };
    match bcode_feedback_cli::run(cli, installed.client()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bcode-feedback: {error}");
            ExitCode::FAILURE
        }
    }
}
