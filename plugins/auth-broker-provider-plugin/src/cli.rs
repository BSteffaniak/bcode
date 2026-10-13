//! Local-only static CLI; no daemon or model invocation during OIDC setup.
use bcode_plugin_sdk::{StaticCliFuture, StaticCliOutcome, StaticCliRegistration};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "auth-broker",
    about = "Prepare a private GitHub OIDC broker lease"
)]
struct Cli {
    #[command(subcommand)]
    command: Operation,
}
#[derive(Subcommand)]
enum Operation {
    /// Exchange OIDC once; write private lease and typed trusted configuration.
    Prepare {
        #[arg(long)]
        directory: PathBuf,
        #[arg(long)]
        broker_url: String,
        #[arg(long)]
        audience: String,
        #[arg(long)]
        timeout_secs: u64,
    },
}
pub(crate) fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: false,
        command: Cli::command,
        invoke,
    }
}
fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let cli = Cli::from_arg_matches(&matches)
            .map_err(|_| "invalid_broker_setup_arguments".to_owned())?;
        let Operation::Prepare {
            directory,
            broker_url,
            audience,
            timeout_secs,
        } = cli.command;
        crate::setup::prepare(&crate::setup::PrepareRequest { directory, broker_url, audience, timeout_secs })
            .await.map_err(|code| {
                if code == "execution_budget_exceeds_lease" {
                    "execution_budget_exceeds_lease: execution budget must fit actual remaining lease minus ten seconds; no model turn started".to_owned()
                } else { code.to_owned() }
            })?;
        Ok(StaticCliOutcome { host_action: None })
    })
}
