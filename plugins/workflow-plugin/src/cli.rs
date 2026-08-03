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
    /// Workflow action: list, register, run, status, pause, resume, cancel, inspect,
    /// retry-node, provide-input, approve-mutation, or deny-mutation.
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
    /// JSON workflow input for `run`, JSON template configuration, or gate value.
    #[arg(long)]
    input: Option<String>,
    /// Exact template owner plugin for template describe/start.
    #[arg(long)]
    owner: Option<String>,
    /// Exact owner-local template ID for template describe/start.
    #[arg(long)]
    template: Option<String>,
    /// Exact logical authored-workflow identity for mutable template describe.
    #[arg(long)]
    workflow: Option<String>,
    /// Exact mutable draft identity for mutable template describe.
    #[arg(long)]
    draft: Option<String>,
    /// Exact waiting node identity for `provide-input`.
    #[arg(long)]
    node: Option<String>,
    /// Exact waiting activation or failed node activation identity.
    #[arg(long)]
    activation: Option<String>,
    /// Exact latest failed attempt number for `retry-node`.
    #[arg(long)]
    attempt: Option<u32>,
    /// Exact pending mutation approval identity for `approve-mutation` or `deny-mutation`.
    #[arg(long)]
    approval: Option<String>,
    /// Caller-stable run identity for `template-start`.
    #[arg(long)]
    run: Option<String>,
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
            let key = match cli.action.as_str() {
                "register" => "version",
                "template-describe" | "template-start" => "template_version",
                _ => "definition_version",
            };
            args.insert(key.to_string(), version.to_string());
        }
        if let Some(path) = cli.definition {
            let definition = std::fs::read_to_string(&path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            args.insert("definition".to_string(), definition);
        }
        if let Some(input) = cli.input {
            let key = match cli.action.as_str() {
                "provide-input" => "value",
                "template-describe" | "template-start" => "configuration",
                _ => "input",
            };
            args.insert(key.to_string(), input);
        }
        if let Some(owner) = cli.owner {
            args.insert("owner_plugin_id".to_string(), owner);
        }
        if let Some(template) = cli.template {
            args.insert("template_id".to_string(), template);
        }
        if let Some(workflow) = cli.workflow {
            args.insert("workflow_id".to_string(), workflow);
        }
        if let Some(draft) = cli.draft {
            args.insert("draft_id".to_string(), draft);
        }
        if let Some(node) = cli.node {
            args.insert("node_id".to_string(), node);
        }
        if let Some(activation) = cli.activation {
            args.insert("activation_id".to_string(), activation);
        }
        if let Some(attempt) = cli.attempt {
            args.insert("failed_attempt".to_string(), attempt.to_string());
        }
        if let Some(approval) = cli.approval {
            args.insert("approval_id".to_string(), approval);
        }
        if let Some(run) = cli.run {
            args.insert("run_id".to_string(), run);
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
