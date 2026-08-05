use crate::execute_command;
use bcode_command::InvokeCommandRequest;
use bcode_plugin_sdk::{StaticCliFuture, StaticCliOutcome, StaticCliRegistration};
use clap::{CommandFactory, FromArgMatches, Parser};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "workflow-ui",
    about = "Discover templates and operate durable workflow runtime/UI surfaces"
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
    /// Path to an editable JSON, YAML, or TOML `WorkflowAuthoringDocument` source file.
    #[arg(long)]
    source: Option<PathBuf>,
    /// Path to a bounded package manifest JSON payload for `package-check`.
    #[arg(long)]
    package_manifest: Option<PathBuf>,
    /// Explicit source format (`json`, `yaml`, or `toml`); otherwise inferred from the file name.
    #[arg(long)]
    source_format: Option<String>,
    /// Path to a compiled workflow definition JSON document for registration.
    #[arg(long)]
    definition: Option<PathBuf>,
    /// Repository path used to derive the immutable workspace snapshot identity.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// JSON workflow input for `run`, JSON template configuration, or gate value.
    #[arg(long)]
    input: Option<String>,
    /// Exact template owner plugin for template describe/instantiate/start.
    #[arg(long)]
    owner: Option<String>,
    /// Exact owner-local template ID for template describe/instantiate/start.
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
    /// Exact mutable draft generation for authored publication.
    #[arg(long)]
    generation: Option<u64>,
    /// Exact immutable revision for authored export.
    #[arg(long)]
    revision: Option<u64>,
    /// Portable export bundle JSON for authored import.
    #[arg(long)]
    bundle: Option<String>,
    /// New logical workflow identity for authored import.
    #[arg(long)]
    target_workflow: Option<String>,
    /// Parent session identity required by `run`.
    #[arg(long)]
    session: Option<String>,
}

fn load_workflow_source_text(
    path: &std::path::Path,
    explicit_format: Option<&str>,
) -> Result<(String, bcode_workflow::WorkflowSourceFormat), String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    if source.len() > bcode_workflow::MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES {
        return Err("workflow source exceeds the portable byte bound".to_string());
    }
    let format = match explicit_format {
        Some("json") => bcode_workflow::WorkflowSourceFormat::Json,
        Some("yaml" | "yml") => bcode_workflow::WorkflowSourceFormat::Yaml,
        Some("toml") => bcode_workflow::WorkflowSourceFormat::Toml,
        Some(format) => return Err(format!("unsupported workflow source format '{format}'")),
        None => bcode_workflow::WorkflowSourceFormat::from_file_name(
            path.file_name()
                .and_then(std::ffi::OsStr::to_str)
                .ok_or_else(|| "workflow source file name is not valid UTF-8".to_string())?,
        )
        .map_err(|error| error.to_string())?,
    };
    Ok((source, format))
}

fn load_workflow_source(
    path: &std::path::Path,
    explicit_format: Option<&str>,
) -> Result<
    (
        bcode_workflow::WorkflowAuthoringDocument,
        bcode_workflow::WorkflowSourceFormat,
    ),
    String,
> {
    let (source, format) = load_workflow_source_text(path, explicit_format)?;
    let document = bcode_workflow::decode_workflow_authoring_source(&source, format)
        .map_err(|error| error.to_string())?;
    Ok((document, format))
}

pub fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: true,
        command: WorkflowCli::command,
        invoke,
    }
}

#[allow(clippy::too_many_lines)]
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
                "template-describe" | "template-instantiate" | "template-start" => {
                    "template_version"
                }
                _ => "definition_version",
            };
            args.insert(key.to_string(), version.to_string());
        }
        if let Some(path) = cli.source {
            if matches!(
                cli.action.as_str(),
                "author-apply" | "workflow.author-apply" | "author-check" | "workflow.author-check"
            ) {
                let (source, format) =
                    load_workflow_source_text(&path, cli.source_format.as_deref())?;
                args.insert("source".to_string(), source);
                args.insert(
                    "source_format".to_string(),
                    serde_json::to_string(&format).map_err(|error| error.to_string())?,
                );
                args.entry("draft_id".to_string()).or_insert_with(|| {
                    bcode_workflow::DEFAULT_WORKFLOW_SOURCE_DRAFT_ID.to_string()
                });
            } else {
                let (document, format) = load_workflow_source(&path, cli.source_format.as_deref())?;
                args.insert(
                    "source_document".to_string(),
                    serde_json::to_string(&document).map_err(|error| error.to_string())?,
                );
                args.insert(
                    "source_format".to_string(),
                    serde_json::to_string(&format).map_err(|error| error.to_string())?,
                );
            }
        }
        if let Some(path) = cli.definition {
            let definition = std::fs::read_to_string(&path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            args.insert("definition".to_string(), definition);
        }
        if let Some(path) = cli.package_manifest {
            let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
            if bytes.len() > bcode_workflow::MAX_WORKFLOW_PACKAGE_SOURCE_BYTES {
                return Err("workflow package manifest exceeds the package byte bound".to_string());
            }
            let source = std::str::from_utf8(&bytes).map_err(|error| error.to_string())?;
            let manifest: bcode_workflow::WorkflowPackageManifest =
                serde_json::from_str(source).map_err(|error| error.to_string())?;
            manifest.validate().map_err(|error| error.to_string())?;
            args.insert(
                "package_manifest".to_string(),
                serde_json::to_string(&manifest).map_err(|error| error.to_string())?,
            );
        }
        if let Some(input) = cli.input {
            let key = match cli.action.as_str() {
                "provide-input" => "value",
                "template-describe" | "template-instantiate" | "template-start" => "configuration",
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
        if let Some(revision) = cli.revision {
            args.insert("revision".to_string(), revision.to_string());
        }
        if let Some(generation) = cli.generation {
            args.insert("expected_generation".to_string(), generation.to_string());
        }
        if let Some(bundle) = cli.bundle {
            args.insert("bundle".to_string(), bundle);
        }
        if let Some(target_workflow) = cli.target_workflow {
            args.insert("target_workflow_id".to_string(), target_workflow);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_loaders_support_concise_yaml_without_retaining_paths() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let json = root.join("fixtures/workflows/source-defined-input.workflow.json");
        let toml = root.join("fixtures/workflows/source-defined-input.workflow.toml");
        let yaml = root.join("fixtures/workflows/source-defined-input.workflow.yaml");
        let concise = root.join("fixtures/workflows/concise-run.workflow.yaml");
        let (json_document, json_format) = load_workflow_source(&json, None).expect("JSON source");
        let (toml_document, toml_format) =
            load_workflow_source(&toml, Some("toml")).expect("explicit TOML source");
        let (yaml_document, yaml_format) = load_workflow_source(&yaml, None).expect("YAML source");
        assert_eq!(json_format, bcode_workflow::WorkflowSourceFormat::Json);
        assert_eq!(toml_format, bcode_workflow::WorkflowSourceFormat::Toml);
        assert_eq!(yaml_format, bcode_workflow::WorkflowSourceFormat::Yaml);
        assert_eq!(json_document, toml_document);
        assert_eq!(json_document, yaml_document);
        let encoded = serde_json::to_string(&json_document).expect("portable document");
        assert!(!encoded.contains("fixtures/workflows"));
        let (_, overridden_format) =
            load_workflow_source(&json, Some("yaml")).expect("JSON is valid YAML");
        assert_eq!(
            overridden_format,
            bcode_workflow::WorkflowSourceFormat::Yaml
        );

        let (source, format) =
            load_workflow_source_text(&concise, None).expect("concise YAML source text");
        assert_eq!(format, bcode_workflow::WorkflowSourceFormat::Yaml);
        assert!(source.contains("workflow_source_version: 1"));
        assert!(!source.contains("fixtures/workflows"));
        assert!(load_workflow_source(&concise, None).is_err());
    }
}
