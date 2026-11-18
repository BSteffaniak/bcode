//! Local-only CLI adapters; no daemon or model invocation during OIDC setup.
use bcode_command::cli::{CliCommandPolicy, CliInvocation, CliOutcome};
use bcode_plugin_sdk::{StaticCliFuture, StaticCliOutcome, StaticCliRegistration};
use bmux_plugin_sdk::{
    CommandExecutionKind, PluginCommand, PluginCommandArgument, PluginCommandArgumentKind,
};
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

/// Compatibility registration for callers not yet using portable declarations.
#[must_use]
pub fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: |_| false,
        command: Cli::command,
        invoke,
        invoke_with_execution: None,
    }
}
fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let cli = Cli::from_arg_matches(&matches)
            .map_err(|_| "invalid_broker_setup_arguments".to_owned())?;
        prepare(cli).await?;
        Ok(StaticCliOutcome { host_action: None })
    })
}

/// Structured local handler used by the shared `bcode.cli/v1` adapter.
/// Parsing stays caller-local; output crosses the extension as a value.
pub fn invoke_structured(
    matches: clap::ArgMatches,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<CliOutcome, String>> + Send + 'static>>
{
    Box::pin(async move {
        let cli = Cli::from_arg_matches(&matches)
            .map_err(|_| "invalid_broker_setup_arguments".to_owned())?;
        Ok(match prepare(cli).await {
            Ok(()) => CliOutcome::default(),
            Err(error) => failure(&error),
        })
    })
}

async fn prepare(cli: Cli) -> Result<(), String> {
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
    Ok(())
}

/// Portable grammar consumed by caller-local clap composition in either packaging mode.
#[must_use]
pub fn command() -> PluginCommand {
    let mut command = PluginCommand::new(
        "auth-broker.prepare",
        "Prepare a private GitHub OIDC broker lease",
    );
    command.path = vec!["auth-broker".into(), "prepare".into()];
    command.execution = CommandExecutionKind::CallerProcess;
    command.expose_in_cli = true;
    command.arguments = [
        ("directory", PluginCommandArgumentKind::Path),
        ("broker-url", PluginCommandArgumentKind::String),
        ("audience", PluginCommandArgumentKind::String),
        ("timeout-secs", PluginCommandArgumentKind::Integer),
    ]
    .into_iter()
    .map(|(name, kind)| PluginCommandArgument {
        name: name.into(),
        kind,
        choice_values: Vec::new(),
        position: None,
        long: Some(name.into()),
        short: None,
        required: true,
        multiple: false,
        trailing_var_arg: false,
        allow_hyphen_values: false,
        summary: None,
        value_name: None,
    })
    .collect();
    command
}

/// Bootstrap never requests daemon startup, stdin or host actions.
#[must_use]
pub fn policy() -> CliCommandPolicy {
    CliCommandPolicy {
        command_name: "auth-broker.prepare".into(),
        requires_daemon: false,
        accepts_stdin: false,
        allows_host_action: false,
    }
}

/// Invoke the portable CLI contract without transporting clap values.
pub async fn invoke_portable(invocation: CliInvocation) -> CliOutcome {
    if invocation.command_name != "auth-broker.prepare"
        || invocation.command_path != ["auth-broker", "prepare"]
        || invocation.stdin.is_some()
    {
        return failure("invalid_broker_setup_invocation");
    }
    // The caller supplies only the selected leaf's original tokens. Revalidate
    // untrusted input locally, never deserialize clap or executable callbacks.
    let args = ["auth-broker".to_owned(), "prepare".to_owned()]
        .into_iter()
        .chain(invocation.arguments);
    let Ok(cli) = Cli::try_parse_from(args) else {
        return failure("invalid_broker_setup_arguments");
    };
    match prepare(cli).await {
        Ok(()) => CliOutcome::default(),
        Err(error) => failure(&error),
    }
}

fn failure(code: &str) -> CliOutcome {
    CliOutcome {
        exit_code: 1,
        stderr: format!("{code}\n").into_bytes(),
        ..CliOutcome::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_policy_is_local_and_has_no_input_or_host_action() {
        let declaration = command();
        assert_eq!(declaration.path, ["auth-broker", "prepare"]);
        assert_eq!(declaration.execution, CommandExecutionKind::CallerProcess);
        assert!(
            declaration
                .arguments
                .iter()
                .all(|argument| argument.required)
        );
        assert!(!policy().requires_daemon);
        assert!(!policy().accepts_stdin);
        assert!(!policy().allows_host_action);
    }

    #[test]
    fn structured_bootstrap_returns_failure_without_printing_or_network() {
        let matches = Cli::command()
            .try_get_matches_from([
                "auth-broker",
                "prepare",
                "--directory",
                "relative-invalid-directory",
                "--broker-url",
                "https://broker.example",
                "--audience",
                "bcode",
                "--timeout-secs",
                "600",
            ])
            .expect("valid CLI grammar");
        let runtime = crate::ProviderRuntime::new().expect("runtime");
        let outcome = runtime
            .block_on(invoke_structured(matches))
            .expect("runtime")
            .expect("structured outcome");
        assert_eq!(outcome.exit_code, 1);
        assert_eq!(outcome.stderr, b"invalid_private_directory\n");
        assert_eq!(outcome.stdout, b"");
        assert!(outcome.host_action.is_none());
        runtime
            .shutdown(std::time::Duration::from_secs(5))
            .expect("shutdown");
    }

    #[test]
    fn portable_bootstrap_rejects_input_before_effects() {
        let runtime = crate::ProviderRuntime::new().expect("runtime");
        let outcome = runtime
            .block_on(invoke_portable(CliInvocation {
                command_name: "auth-broker.prepare".into(),
                command_path: vec!["auth-broker".into(), "prepare".into()],
                arguments: Vec::new(),
                stdin: Some(vec![1]),
            }))
            .expect("local invocation");
        assert_eq!(outcome.exit_code, 1);
        assert_eq!(outcome.stdout, b"");
        assert!(outcome.host_action.is_none());
        runtime
            .shutdown(std::time::Duration::from_secs(5))
            .expect("shutdown");
    }
}
