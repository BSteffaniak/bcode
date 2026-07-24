use crate::execute_command;
use bcode_command::InvokeCommandRequest;
use bcode_plugin_sdk::{StaticCliFuture, StaticCliOutcome, StaticCliRegistration};
use clap::{CommandFactory, FromArgMatches, Parser};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "workflow",
    about = "Discover, register, run, and inspect durable workflows"
)]
struct WorkflowCli {
    /// Workflow action: list, register, run, status, pause, resume, cancel, inspect, or provide-input.
    #[arg(default_value = "status")]
    action: String,
    /// Definition or run identity for the selected action.
    #[arg(long)]
    id: Option<String>,
    /// Exact positive definition version.
    #[arg(long)]
    version: Option<u32>,
    /// Path to a compiled workflow definition JSON document for registration.
    #[arg(long)]
    definition: Option<PathBuf>,
    /// Repository path used to derive the immutable workspace snapshot identity.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// JSON workflow input for `run`, or JSON gate value for `provide-input`.
    #[arg(long)]
    input: Option<String>,
    /// Exact waiting node identity for `provide-input`.
    #[arg(long)]
    node: Option<String>,
    /// Exact waiting activation identity for `provide-input`.
    #[arg(long)]
    activation: Option<String>,
    /// Parent session identity required by `run`.
    #[arg(long)]
    session: Option<String>,
}

pub fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: true,
        command: WorkflowCli::command,
        invoke,
    }
}

fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let cli = WorkflowCli::from_arg_matches(&matches).map_err(|error| error.to_string())?;
        let command_id = match cli.action.as_str() {
            "status" => "workflow.status".to_string(),
            action if action.starts_with("workflow.") => action.to_string(),
            action => format!("workflow.{action}"),
        };
        let mut args = BTreeMap::new();
        if let Some(id) = cli.id {
            let key = if matches!(cli.action.as_str(), "run" | "register") {
                "definition_id"
            } else {
                "run_id"
            };
            args.insert(key.to_string(), id);
        }
        if let Some(version) = cli.version {
            let key = if cli.action == "register" {
                "version"
            } else {
                "definition_version"
            };
            args.insert(key.to_string(), version.to_string());
        }
        if let Some(path) = cli.definition {
            let definition = std::fs::read_to_string(&path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            args.insert("definition".to_string(), definition);
        }
        if let Some(input) = cli.input {
            let key = if cli.action == "provide-input" {
                "value"
            } else {
                "input"
            };
            args.insert(key.to_string(), input);
        }
        if let Some(node) = cli.node {
            args.insert("node_id".to_string(), node);
        }
        if let Some(activation) = cli.activation {
            args.insert("activation_id".to_string(), activation);
        }
        if let Some(session) = cli.session {
            args.insert("session_id".to_string(), session);
        }
        let workspace_snapshot = std::fs::canonicalize(&cli.repo)
            .unwrap_or(cli.repo)
            .to_string_lossy()
            .into_owned();
        args.insert("workspace_snapshot".to_string(), workspace_snapshot);
        let response = execute_command(InvokeCommandRequest { command_id, args }).await?;
        if let Some(message) = response.message {
            println!("{message}");
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&response.effects).map_err(|error| error.to_string())?
        );
        Ok(StaticCliOutcome::default())
    })
}
