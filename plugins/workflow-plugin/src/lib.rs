#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Workflow product integration plugin for Bcode.

mod authoring_tui;
#[cfg(feature = "static-bundled")]
mod cli;
pub mod tui;

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
    SlashCommandContribution,
};
use bcode_plugin_sdk::prelude::*;
use bcode_plugin_sdk::{OP_SESSION_STATUS, SESSION_STATUS_INTERFACE_ID};
use std::collections::{BTreeMap, BTreeSet};

const PLUGIN_ID: &str = "bcode.workflow";
const STATUS_SURFACE_KIND: &str = "workflow.status";
const AUTHOR_SURFACE_KIND: &str = "workflow.author";
const QUERY_LIMIT: usize = 100;

#[derive(Default)]
pub struct WorkflowPlugin;

impl RustPlugin for WorkflowPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        for command in command_contributions() {
            registrar
                .register(&command)
                .map_err(|error| PluginError::failed(error.to_string()))?;
        }
        Ok(())
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match (
            context.request.interface_id.as_str(),
            context.request.operation.as_str(),
        ) {
            (COMMAND_INTERFACE_ID, OP_INVOKE_COMMAND) => invoke_command(&context.request),
            (SESSION_STATUS_INTERFACE_ID, OP_SESSION_STATUS) => session_status(&context.request),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported workflow plugin operation",
            ),
        }
    }
}

#[allow(clippy::too_many_lines)]
fn command_contributions() -> Vec<CommandContribution> {
    [
        ("workflow", "Workflow", "Open workflow status"),
        (
            "workflow.list",
            "Workflow: List",
            "List registered workflows",
        ),
        (
            "workflow.templates",
            "Workflow: Templates",
            "List available workflow templates",
        ),
        (
            "workflow.template-describe",
            "Workflow: Configure Template",
            "Describe and configure an exact workflow template",
        ),
        (
            "workflow.template-instantiate",
            "Workflow: Edit Template",
            "Instantiate an exact maintainable template and open the graph editor",
        ),
        (
            "workflow.template-start",
            "Workflow: Start Template",
            "Start a validated workflow template",
        ),
        (
            "workflow.author-apply",
            "Workflow: Apply Source",
            "Create or optimistically update the canonical source draft",
        ),
        (
            "workflow.author-check",
            "Workflow: Check Source",
            "Validate and compile-preview one authored workflow source",
        ),
        (
            "workflow.package-check",
            "Workflow: Check Package",
            "Validate and compile-preview one bounded workflow package",
        ),
        (
            "workflow.package-start",
            "Workflow: Start Package Export",
            "Start one exact published workflow package export",
        ),
        (
            "workflow.author-create",
            "Workflow: Create From Source",
            "Create one durable workflow draft from authored source",
        ),
        (
            "workflow.author-update",
            "Workflow: Update From Source",
            "Replace one exact durable draft generation from authored source",
        ),
        (
            "workflow.author-publish",
            "Workflow: Publish Authored Draft",
            "Publish one exact authored draft generation",
        ),
        (
            "workflow.author-export",
            "Workflow: Export Authored Revision",
            "Export one exact authored revision and dependency manifest",
        ),
        (
            "workflow.author-import",
            "Workflow: Import Authored Workflow",
            "Import one exact portable authored workflow bundle",
        ),
        (
            "workflow.register",
            "Workflow: Register",
            "Register a compiled workflow definition",
        ),
        (
            "workflow.run",
            "Workflow: Run",
            "Start a registered workflow",
        ),
        (
            "workflow.status",
            "Workflow: Status",
            "Inspect workflow status",
        ),
        ("workflow.pause", "Workflow: Pause", "Pause a workflow run"),
        (
            "workflow.resume",
            "Workflow: Resume",
            "Resume a workflow run",
        ),
        (
            "workflow.cancel",
            "Workflow: Cancel",
            "Cancel a workflow run",
        ),
        (
            "workflow.inspect",
            "Workflow: Inspect",
            "Inspect workflow graph and history",
        ),
        (
            "workflow.doctor",
            "Workflow: Doctor",
            "Diagnose one workflow run without mutation",
        ),
        (
            "workflow.repair",
            "Workflow: Repair",
            "Apply one explicit typed attempt repair",
        ),
        (
            "workflow.retry-node",
            "Workflow: Retry Node",
            "Retry one exact latest failed node attempt",
        ),
        (
            "workflow.approve",
            "Workflow: Approve",
            "Resolve one waiting typed approval",
        ),
        (
            "workflow.deny",
            "Workflow: Deny",
            "Deny one waiting typed approval",
        ),
        (
            "workflow.provide-input",
            "Workflow: Provide Input",
            "Resolve a waiting input",
        ),
        (
            "workflow.approve-mutation",
            "Workflow: Approve Mutation",
            "Approve one exact pending mutation dispatch",
        ),
        (
            "workflow.deny-mutation",
            "Workflow: Deny Mutation",
            "Deny one exact pending mutation dispatch",
        ),
    ]
    .into_iter()
    .map(|(id, title, description)| CommandContribution {
        id: id.to_string(),
        title: title.to_string(),
        description: Some(description.to_string()),
        category: Some("workflow".to_string()),
        surfaces: BTreeSet::from([CommandSurface::Palette, CommandSurface::Slash]),
        slash: Some(SlashCommandContribution {
            name: id.to_string(),
            aliases: BTreeSet::new(),
        }),
        execution: bcode_command::CommandExecution::Immediate,
        owner: CommandOwner::Plugin {
            plugin_id: PLUGIN_ID.to_string(),
        },
        action: CommandAction::Plugin {
            plugin_id: PLUGIN_ID.to_string(),
            command_id: id.to_string(),
        },
    })
    .collect()
}

fn invoke_command(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<InvokeCommandRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match run_async(async move { Box::pin(execute_command(request)).await }) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("workflow_command_failed", error),
    }
}

#[allow(clippy::too_many_lines, clippy::large_stack_frames)]
pub(crate) async fn execute_command(
    mut request: InvokeCommandRequest,
) -> Result<InvokeCommandResponse, String> {
    if let Some(arguments) = request.args.get("arguments").cloned() {
        for (key, value) in parse_arguments(&arguments) {
            request.args.entry(key).or_insert(value);
        }
    }
    let client = bcode_client::BcodeClient::default_endpoint();
    let mut options = serde_json::Map::from_iter([
        (
            "command_id".to_string(),
            serde_json::Value::String(request.command_id.clone()),
        ),
        (
            "arguments".to_string(),
            serde_json::to_value(&request.args).map_err(|error| error.to_string())?,
        ),
    ]);
    let message = match request.command_id.as_str() {
        "workflow" | "workflow.status" => {
            let runs = client
                .list_workflow_runs(QUERY_LIMIT)
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "runs".to_string(),
                serde_json::to_value(&runs).map_err(|error| error.to_string())?,
            );
            let mutation_approvals = client
                .list_all_workflow_mutation_approvals(QUERY_LIMIT)
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "mutation_approvals".to_string(),
                serde_json::to_value(&mutation_approvals).map_err(|error| error.to_string())?,
            );
            format!(
                "{} durable workflow runs · {} pending mutation approvals",
                runs.len(),
                mutation_approvals.len()
            )
        }
        "workflow.list" => {
            let definitions = client
                .list_workflow_definitions(QUERY_LIMIT)
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "definitions".to_string(),
                serde_json::to_value(&definitions).map_err(|error| error.to_string())?,
            );
            let services = client
                .plugin_services()
                .await
                .map_err(|error| error.to_string())?;
            let blocks = services
                .into_iter()
                .flat_map(|service| service.workflow_blocks)
                .collect::<Vec<_>>();
            options.insert(
                "blocks".to_string(),
                serde_json::to_value(&blocks).map_err(|error| error.to_string())?,
            );
            format!(
                "{} registered workflow definitions · {} available plugin blocks",
                definitions.len(),
                blocks.len()
            )
        }
        "workflow.templates" => {
            let templates = client
                .list_workflow_templates(QUERY_LIMIT)
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "templates".to_string(),
                serde_json::to_value(&templates).map_err(|error| error.to_string())?,
            );
            format!("{} available workflow templates", templates.len())
        }
        "workflow.template-describe" | "workflow.template-instantiate" => {
            describe_or_instantiate_template(&request, &client, &mut options).await?
        }
        "workflow.template-start" => {
            let owner_plugin_id = required_arg(&request, "owner_plugin_id")?;
            let template_id = required_arg(&request, "template_id")?;
            let template_version = parse_arg::<u32>(&request, "template_version")?;
            let described = client
                .describe_workflow_template(
                    owner_plugin_id.clone(),
                    template_id.clone(),
                    template_version,
                )
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "workflow template not found or disabled".to_string())?;
            if !described.diagnostics.is_empty() {
                return Err(described
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; "));
            }
            let configuration_schema = described
                .authoring_document
                .as_ref()
                .map_or_else(
                    || described.template.configuration_schema(),
                    |document| &document.configuration_schema,
                )
                .schema
                .clone();
            let configuration = validate_template_configuration(
                &configuration_schema,
                &required_arg(&request, "configuration")?,
            )?;
            let limits = reference_template_run_limits(&template_id, &configuration)?;
            let parent_session_id = parse_arg(&request, "session_id")?;
            let started = client
                .start_workflow_template(bcode_ipc::WorkflowTemplateStartRequest {
                    owner_plugin_id,
                    template_id,
                    template_version,
                    run_id: request.args.get("run_id").cloned(),
                    workspace_snapshot: request.args.get("workspace_snapshot").cloned(),
                    parent_session_id,
                    configuration,
                    limits,
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert("runs".to_string(), serde_json::json!([started.run]));
            "workflow template started".to_string()
        }
        "workflow.author-apply" => {
            let source_format: bcode_workflow::WorkflowSourceFormat =
                serde_json::from_str(&required_arg(&request, "source_format")?)
                    .map_err(|error| error.to_string())?;
            let source = required_arg(&request, "source")?;
            let draft_id =
                request.args.get("draft_id").cloned().unwrap_or_else(|| {
                    bcode_workflow::DEFAULT_WORKFLOW_SOURCE_DRAFT_ID.to_string()
                });
            let applied = client
                .apply_workflow_source(source_format, source, draft_id)
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "source_apply".to_string(),
                serde_json::to_value(applied).map_err(|error| error.to_string())?,
            );
            "workflow source apply resolved".to_string()
        }
        "workflow.author-check" => {
            let source_format: bcode_workflow::WorkflowSourceFormat =
                serde_json::from_str(&required_arg(&request, "source_format")?)
                    .map_err(|error| error.to_string())?;
            let source = required_arg(&request, "source")?;
            let validation = client
                .validate_workflow_source(bcode_ipc::WorkflowSourceComputationRequest {
                    source_format,
                    source: source.clone(),
                    control: bcode_ipc::WorkflowComputationControl::default(),
                })
                .await
                .map_err(|error| error.to_string())?;
            let preview = client
                .preview_workflow_source(bcode_ipc::WorkflowSourcePreviewRequest {
                    source_format,
                    source,
                    configuration: None,
                    control: bcode_ipc::WorkflowComputationControl::default(),
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "source_validation".to_string(),
                serde_json::to_value(validation).map_err(|error| error.to_string())?,
            );
            options.insert(
                "source_preview".to_string(),
                serde_json::to_value(preview).map_err(|error| error.to_string())?,
            );
            "workflow authoring source validated".to_string()
        }
        "workflow.package-check" => {
            let manifest: bcode_workflow::WorkflowPackageManifest =
                serde_json::from_str(&required_arg(&request, "package_manifest")?)
                    .map_err(|error| error.to_string())?;
            let closure = bcode_workflow::WorkflowPackageClosure {
                version: bcode_workflow::WORKFLOW_PACKAGE_CLOSURE_VERSION,
                entry_package_id: manifest.package_id.clone(),
                packages: vec![bcode_workflow::WorkflowPackageClosureSource {
                    package_id: manifest.package_id.clone(),
                    source_name: None,
                    manifest,
                }],
            };
            let validation = client
                .validate_workflow_package(bcode_ipc::WorkflowPackageComputationRequest {
                    closure,
                    control: bcode_ipc::WorkflowComputationControl::default(),
                })
                .await
                .map_err(|error| error.to_string())?;
            let entry_index = validation
                .plan
                .packages
                .iter()
                .position(|package| package.package_id == validation.plan.entry_package_id)
                .ok_or_else(|| "planned package closure has no entry package".to_string())?;
            let entry_plan = validation.plan.packages[entry_index].plan.clone();
            let dependency_plans = validation.plan.packages[..entry_index]
                .iter()
                .map(|package| package.plan.clone())
                .collect();
            let preview = client
                .preview_workflow_package(bcode_ipc::WorkflowPackagePreviewRequest {
                    plan: entry_plan,
                    dependency_plans,
                    configurations: BTreeMap::new(),
                    control: bcode_ipc::WorkflowComputationControl::default(),
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "package_validation".to_string(),
                serde_json::to_value(validation).map_err(|error| error.to_string())?,
            );
            options.insert(
                "package_preview".to_string(),
                serde_json::to_value(preview).map_err(|error| error.to_string())?,
            );
            "workflow package validated".to_string()
        }
        "workflow.package-start" => {
            let started = client
                .start_workflow_package_export(package_export_start_request(&request)?)
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "package_export_start".to_string(),
                serde_json::to_value(started).map_err(|error| error.to_string())?,
            );
            "workflow package export started".to_string()
        }
        "workflow.author-create" => {
            let source_format: bcode_workflow::WorkflowSourceFormat =
                serde_json::from_str(&required_arg(&request, "source_format")?)
                    .map_err(|error| error.to_string())?;
            let document: bcode_workflow::WorkflowAuthoringDocument =
                serde_json::from_str(&required_arg(&request, "source_document")?)
                    .map_err(|error| error.to_string())?;
            let draft_id = required_arg(&request, "draft_id")?;
            let created = client
                .create_authored_workflow(bcode_ipc::CreateAuthoredWorkflowRequest {
                    document,
                    draft_id,
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "source_format".to_string(),
                serde_json::to_value(source_format).map_err(|error| error.to_string())?,
            );
            options.insert(
                "workflow".to_string(),
                serde_json::to_value(&created.0).map_err(|error| error.to_string())?,
            );
            options.insert(
                "draft".to_string(),
                serde_json::to_value(&created.1).map_err(|error| error.to_string())?,
            );
            "workflow draft created from source".to_string()
        }
        "workflow.author-update" => {
            let source_format: bcode_workflow::WorkflowSourceFormat =
                serde_json::from_str(&required_arg(&request, "source_format")?)
                    .map_err(|error| error.to_string())?;
            let document: bcode_workflow::WorkflowAuthoringDocument =
                serde_json::from_str(&required_arg(&request, "source_document")?)
                    .map_err(|error| error.to_string())?;
            let workflow_id = required_arg(&request, "workflow_id")?;
            let draft_id = required_arg(&request, "draft_id")?;
            let expected_generation = parse_arg::<u64>(&request, "expected_generation")?;
            let producer = document.producer.clone();
            let updated = client
                .update_workflow_draft(bcode_ipc::UpdateWorkflowDraftRequest {
                    workflow_id,
                    draft_id,
                    expected_generation,
                    document,
                    producer,
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "source_format".to_string(),
                serde_json::to_value(source_format).map_err(|error| error.to_string())?,
            );
            options.insert(
                "draft_update".to_string(),
                serde_json::to_value(&updated).map_err(|error| error.to_string())?,
            );
            "workflow draft update resolved".to_string()
        }
        "workflow.author-publish" => {
            let workflow_id = required_arg(&request, "workflow_id")?;
            let draft_id = required_arg(&request, "draft_id")?;
            let expected_generation = parse_arg::<u64>(&request, "expected_generation")?;
            let result = client
                .publish_workflow_draft(bcode_ipc::PublishWorkflowDraftRequest {
                    workflow_id,
                    draft_id,
                    expected_generation,
                    configuration: None,
                    activate: false,
                    expected_active_revision: None,
                    control: bcode_ipc::WorkflowComputationControl::default(),
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "publication".to_string(),
                serde_json::to_value(result).map_err(|error| error.to_string())?,
            );
            "workflow draft published".to_string()
        }
        "workflow.author-export" => {
            let workflow_id = required_arg(&request, "workflow_id")?;
            let revision = parse_arg::<u64>(&request, "revision")?;
            let bundle = client
                .export_workflow_revision(bcode_ipc::ExportWorkflowRevisionRequest {
                    workflow_id,
                    revision,
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "export_bundle".to_string(),
                serde_json::to_value(bundle).map_err(|error| error.to_string())?,
            );
            "workflow revision exported".to_string()
        }
        "workflow.author-import" => {
            let bundle = serde_json::from_str(&required_arg(&request, "bundle")?)
                .map_err(|error| error.to_string())?;
            let target_workflow_id = required_arg(&request, "target_workflow_id")?;
            let draft_id = required_arg(&request, "draft_id")?;
            let imported = client
                .import_workflow(bcode_ipc::ImportWorkflowRequest {
                    bundle,
                    target_workflow_id,
                    draft_id,
                    collision_policy: bcode_ipc::WorkflowImportCollisionPolicy::RequireNewWorkflow,
                    control: bcode_ipc::WorkflowComputationControl::default(),
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "import_result".to_string(),
                serde_json::to_value(imported).map_err(|error| error.to_string())?,
            );
            "workflow bundle imported".to_string()
        }
        "workflow.register" => {
            let definition_id = required_arg(&request, "definition_id")?;
            let version = parse_arg::<u32>(&request, "version")?;
            let definition = serde_json::from_str(&required_arg(&request, "definition")?)
                .map_err(|error| error.to_string())?;
            let registered = client
                .register_workflow_definition(bcode_ipc::WorkflowDefinitionRegistrationRequest {
                    definition_id,
                    version,
                    definition,
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert("definitions".to_string(), serde_json::json!([registered]));
            "workflow definition registered".to_string()
        }
        "workflow.run" => {
            let definition_id = required_arg(&request, "definition_id")?;
            let definition_version = parse_arg::<u32>(&request, "definition_version")?;
            let parent_session_id = parse_arg(&request, "session_id")?;
            let workspace_snapshot = required_arg(&request, "workspace_snapshot")?;
            let input = request
                .args
                .get("input")
                .map(|value| serde_json::from_str(value).map_err(|error| error.to_string()))
                .transpose()?;
            let started = client
                .start_workflow_run(bcode_ipc::WorkflowRunStartRequest {
                    definition_id,
                    definition_version,
                    run_id: None,
                    workspace_snapshot,
                    parent_session_id,
                    parent_session_generation: None,
                    binding: None,
                    input,
                    limits: bcode_workflow_store::WorkflowRunLimits::default(),
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert("run".to_string(), serde_json::json!(started.run));
            format!("started workflow run {}", started.run.run_id)
        }
        "workflow.pause" | "workflow.resume" | "workflow.cancel" => {
            let run_id = required_arg(&request, "run_id")?;
            let changed = match request.command_id.as_str() {
                "workflow.pause" => client.pause_workflow_run(run_id.clone()).await,
                "workflow.resume" => client.resume_workflow_run(run_id.clone()).await,
                _ => client.cancel_workflow_run(run_id.clone()).await,
            }
            .map_err(|error| error.to_string())?;
            options.insert("run_id".to_string(), serde_json::json!(run_id));
            format!("{} changed={changed}", request.command_id)
        }
        "workflow.inspect" => {
            let run_id = required_arg(&request, "run_id")?;
            let inspection = client
                .inspect_workflow_run(run_id.clone(), QUERY_LIMIT)
                .await
                .map_err(|error| error.to_string())?;
            options.extend([
                ("run".to_string(), serde_json::json!(inspection.run)),
                (
                    "definition".to_string(),
                    serde_json::json!(inspection.definition),
                ),
                (
                    "terminal_output".to_string(),
                    serde_json::json!(inspection.terminal_output),
                ),
                (
                    "activations".to_string(),
                    serde_json::json!(inspection.activations),
                ),
                ("waits".to_string(), serde_json::json!(inspection.waits)),
                (
                    "mutation_approvals".to_string(),
                    serde_json::json!(inspection.mutation_approvals),
                ),
                (
                    "attempts".to_string(),
                    serde_json::json!(inspection.attempts),
                ),
                ("events".to_string(), serde_json::json!(inspection.events)),
                (
                    "decisions".to_string(),
                    serde_json::json!(inspection.decisions),
                ),
                ("grants".to_string(), serde_json::json!(inspection.grants)),
                (
                    "resource_leases".to_string(),
                    serde_json::json!(inspection.resource_leases),
                ),
                ("outputs".to_string(), serde_json::json!(inspection.outputs)),
                (
                    "child_run_links".to_string(),
                    serde_json::json!(inspection.child_run_links),
                ),
                (
                    "descendant_runs".to_string(),
                    serde_json::json!(inspection.descendant_runs),
                ),
                (
                    "repeat_outcomes".to_string(),
                    serde_json::json!(inspection.repeat_outcomes),
                ),
                (
                    "child_sessions".to_string(),
                    serde_json::json!(inspection.child_sessions),
                ),
            ]);
            format!("workflow run {run_id}")
        }
        "workflow.doctor" => {
            let run_id = required_arg(&request, "run_id")?;
            let report = client
                .doctor_workflow_run(run_id, QUERY_LIMIT)
                .await
                .map_err(|error| error.to_string())?;
            options.insert("doctor".to_string(), serde_json::json!(report));
            "workflow doctor completed without mutation".to_string()
        }
        "workflow.repair" => {
            let dispatch_identity = required_arg(&request, "dispatch_identity")?;
            let resolution = serde_json::from_str(&required_arg(&request, "resolution")?)
                .map_err(|error| format!("invalid typed repair resolution: {error}"))?;
            let result = client
                .repair_workflow_attempt(dispatch_identity, resolution)
                .await
                .map_err(|error| error.to_string())?;
            options.insert("repair".to_string(), serde_json::json!(result));
            "workflow attempt repaired explicitly".to_string()
        }
        "workflow.retry-node" => {
            let run_id = required_arg(&request, "run_id")?;
            let node_id = required_arg(&request, "node_id")?;
            let activation_id = required_arg(&request, "activation_id")?;
            let failed_attempt = parse_arg::<u32>(&request, "failed_attempt")?;
            let result = client
                .retry_workflow_node(run_id, node_id, activation_id, failed_attempt)
                .await
                .map_err(|error| error.to_string())?;
            options.insert("retry".to_string(), serde_json::json!(result));
            "workflow node retry admitted".to_string()
        }
        "workflow.approve" | "workflow.deny" => {
            let run_id = required_arg(&request, "run_id")?;
            let node_id = required_arg(&request, "node_id")?;
            let activation_id = required_arg(&request, "activation_id")?;
            let approved = request.command_id == "workflow.approve";
            let result = client
                .resolve_workflow_approval(run_id, node_id, activation_id, approved)
                .await
                .map_err(|error| error.to_string())?;
            options.insert("approval_resolution".to_string(), serde_json::json!(result));
            if approved {
                "workflow approval recorded".to_string()
            } else {
                "workflow approval denied".to_string()
            }
        }
        "workflow.provide-input" => {
            let run_id = required_arg(&request, "run_id")?;
            let node_id = required_arg(&request, "node_id")?;
            let activation_id = required_arg(&request, "activation_id")?;
            let value = serde_json::from_str(&required_arg(&request, "value")?)
                .map_err(|error| error.to_string())?;
            let result = client
                .provide_workflow_input(run_id, node_id, activation_id, value)
                .await
                .map_err(|error| error.to_string())?;
            options.insert("resolution".to_string(), serde_json::json!(result));
            "workflow input recorded".to_string()
        }
        "workflow.approve-mutation" | "workflow.deny-mutation" => {
            let approval_id = required_arg(&request, "approval_id")?;
            let decision = if request.command_id == "workflow.approve-mutation" {
                bcode_workflow_store::WorkflowMutationApprovalDecision::Approve
            } else {
                bcode_workflow_store::WorkflowMutationApprovalDecision::Deny
            };
            let result = client
                .resolve_workflow_mutation_approval(approval_id, decision)
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "mutation_approval_resolution".to_string(),
                serde_json::json!(result),
            );
            format!("workflow mutation approval {decision:?}")
        }
        command => return Err(format!("unsupported workflow command '{command}'")),
    };
    Ok(InvokeCommandResponse {
        success: true,
        message: Some(message),
        updated_model: None,
        updated_provider: None,
        updated_thinking: None,
        effects: vec![CommandEffect::OpenPluginSurface {
            surface_kind: if matches!(
                request.command_id.as_str(),
                "workflow.template-describe" | "workflow.template-instantiate"
            ) {
                AUTHOR_SURFACE_KIND.to_string()
            } else {
                STATUS_SURFACE_KIND.to_string()
            },
            instance_id: if matches!(
                request.command_id.as_str(),
                "workflow.template-describe" | "workflow.template-instantiate"
            ) {
                "workflow-author".to_string()
            } else {
                "workflow-status".to_string()
            },
            options: serde_json::Value::Object(options),
        }],
    })
}

#[allow(clippy::too_many_lines)]
async fn describe_or_instantiate_template(
    request: &InvokeCommandRequest,
    client: &bcode_client::BcodeClient,
    options: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<String, String> {
    let owner_plugin_id = required_arg(request, "owner_plugin_id")?;
    let template_id = required_arg(request, "template_id")?;
    let template_version = parse_arg::<u32>(request, "template_version")?;
    let template = client
        .describe_workflow_template(
            owner_plugin_id.clone(),
            template_id.clone(),
            template_version,
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "workflow template not found or disabled".to_string())?;
    options.insert(
        "template".to_string(),
        serde_json::to_value(&template).map_err(|error| error.to_string())?,
    );
    match (
        request.args.get("workflow_id"),
        request.args.get("draft_id"),
    ) {
        (Some(workflow_id), Some(draft_id))
            if request.command_id == "workflow.template-instantiate" =>
        {
            let (_, draft) = client
                .instantiate_workflow_template(bcode_ipc::WorkflowTemplateInstantiationRequest {
                    owner_plugin_id,
                    template_id,
                    template_version,
                    workflow_id: workflow_id.clone(),
                    draft_id: draft_id.clone(),
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "draft".to_string(),
                serde_json::to_value(draft).map_err(|error| error.to_string())?,
            );
        }
        (Some(workflow_id), Some(draft_id)) => {
            let draft = client
                .workflow_draft(workflow_id.clone(), draft_id.clone())
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("workflow draft not found: {workflow_id}/{draft_id}"))?;
            options.insert(
                "draft".to_string(),
                serde_json::to_value(draft).map_err(|error| error.to_string())?,
            );
        }
        (None, None) if request.command_id == "workflow.template-instantiate" => {
            let workflow_id = request
                .args
                .get("new_workflow_id")
                .cloned()
                .unwrap_or_else(|| generated_authored_workflow_id(&template_id));
            let draft_id = request
                .args
                .get("new_draft_id")
                .cloned()
                .unwrap_or_else(|| "draft-1".to_string());
            let (_, draft) = client
                .instantiate_workflow_template(bcode_ipc::WorkflowTemplateInstantiationRequest {
                    owner_plugin_id,
                    template_id,
                    template_version,
                    workflow_id,
                    draft_id,
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "draft".to_string(),
                serde_json::to_value(draft).map_err(|error| error.to_string())?,
            );
        }
        (None, None) => {}
        _ => {
            return Err(
                "workflow_id and draft_id must be provided together for mutable editing"
                    .to_string(),
            );
        }
    }
    if let Some(session_id) = request.args.get("session_id") {
        options.insert("session_id".to_string(), serde_json::json!(session_id));
    }
    if let Some(configuration) = request.args.get("configuration") {
        let configuration_schema = template.authoring_document.as_ref().map_or_else(
            || template.template.configuration_schema(),
            |document| &document.configuration_schema,
        );
        let configuration =
            validate_template_configuration(&configuration_schema.schema, configuration)?;
        options.insert("configuration".to_string(), configuration);
    }
    if request.command_id == "workflow.template-instantiate" {
        Ok("workflow template instantiated as a mutable graph draft".to_string())
    } else {
        Ok(format!(
            "workflow template {} v{}",
            template.template.template_id, template.template.template_version
        ))
    }
}

fn generated_authored_workflow_id(template_id: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{template_id}-{timestamp:x}-{sequence:x}")
}

fn session_status(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<bcode_plugin_sdk::SessionStatusRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let contribution = match run_async(active_session_status(request.session_id)) {
        Ok(contribution) => contribution,
        Err(error) => return ServiceResponse::error("workflow_status_failed", error),
    };
    json_response(&bcode_plugin_sdk::SessionStatusResponse { contribution })
}

async fn active_session_status(
    session_id: bcode_session_models::SessionId,
) -> Result<Option<bcode_plugin_sdk::SessionStatusContribution>, String> {
    let runs = bcode_client::BcodeClient::default_endpoint()
        .list_workflow_runs(QUERY_LIMIT)
        .await
        .map_err(|error| error.to_string())?;
    let session_id = session_id.to_string();
    let active = runs
        .into_iter()
        .filter(|run| {
            run.parent_session_id.as_deref() == Some(session_id.as_str())
                && matches!(
                    run.status,
                    bcode_workflow_store::RunStatus::Running
                        | bcode_workflow_store::RunStatus::Paused
                )
        })
        .collect::<Vec<_>>();
    if active.is_empty() {
        return Ok(None);
    }
    let paused = active
        .iter()
        .filter(|run| run.status == bcode_workflow_store::RunStatus::Paused)
        .count();
    let paused_suffix = if paused > 0 {
        format!(" · {paused} paused")
    } else {
        String::new()
    };
    Ok(Some(bcode_plugin_sdk::SessionStatusContribution {
        contribution_id: "workflow".to_string(),
        text: format!("Workflows · {} active{paused_suffix}", active.len()),
        priority: 40,
        metadata: BTreeMap::from([("runs".to_string(), serde_json::json!(active))]),
    }))
}

fn reference_template_run_limits(
    template_id: &str,
    configuration: &serde_json::Value,
) -> Result<bcode_workflow_store::WorkflowRunLimits, String> {
    let mut limits = bcode_workflow_store::WorkflowRunLimits::default();
    if template_id != "implementation-verification-commit" {
        return Ok(limits);
    }
    let iteration_limit = configuration
        .get("iteration_limit")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "reference template iteration_limit is required".to_string())?;
    limits.cycle_cap = u32::try_from(iteration_limit)
        .map_err(|_| "reference template iteration_limit exceeds u32".to_string())?;
    limits.node_execution_cap = limits
        .cycle_cap
        .checked_mul(16)
        .ok_or_else(|| "reference template node execution limit overflow".to_string())?;
    Ok(limits)
}

fn parse_arguments(arguments: &str) -> BTreeMap<String, String> {
    let mut parsed = BTreeMap::new();
    let bytes = arguments.as_bytes();
    let mut start = 0;
    let mut depth = 0_u32;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b'{' | b'[' => depth = depth.saturating_add(1),
            b'}' | b']' => depth = depth.saturating_sub(1),
            byte if byte.is_ascii_whitespace() && depth == 0 => {
                insert_argument(&mut parsed, &arguments[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    insert_argument(&mut parsed, &arguments[start..]);
    parsed
}

fn insert_argument(parsed: &mut BTreeMap<String, String>, argument: &str) {
    let argument = argument.trim();
    if let Some((key, value)) = argument.split_once('=')
        && !key.is_empty()
        && !value.is_empty()
    {
        parsed.insert(key.to_string(), value.to_string());
    }
}

fn validate_template_configuration(
    schema: &serde_json::Value,
    configuration: &str,
) -> Result<serde_json::Value, String> {
    if configuration.len() > 1_048_576 {
        return Err("template configuration exceeds 1048576 bytes".to_string());
    }
    let configuration = serde_json::from_str(configuration)
        .map_err(|error| format!("invalid template configuration JSON: {error}"))?;
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| format!("invalid template configuration schema: {error}"))?;
    if let Err(error) = validator.validate(&configuration) {
        return Err(format!("template configuration is invalid: {error}"));
    }
    Ok(configuration)
}

fn package_export_start_request(
    request: &InvokeCommandRequest,
) -> Result<bcode_ipc::StartWorkflowPackageExportRequest, String> {
    let package_id = required_arg(request, "package_id")?;
    let export = required_arg(request, "export")?;
    let parent_session_id = required_arg(request, "parent_session_id")?
        .parse()
        .map_err(|error| format!("invalid 'parent_session_id': {error}"))?;
    Ok(bcode_ipc::StartWorkflowPackageExportRequest {
        package_export: bcode_workflow::WorkflowPackageExportIdentity {
            package_id,
            export,
            package_lock_digest_sha256: request
                .args
                .get("package_lock_digest_sha256")
                .filter(|value| !value.is_empty())
                .cloned(),
        },
        run_id: request
            .args
            .get("run_id")
            .filter(|value| !value.is_empty())
            .cloned(),
        parent_session_id,
        workspace_snapshot: request
            .args
            .get("workspace_snapshot")
            .filter(|value| !value.is_empty())
            .cloned(),
        parent_session_generation: request
            .args
            .get("parent_session_generation")
            .filter(|value| !value.is_empty())
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|error| format!("invalid 'parent_session_generation': {error}"))
            })
            .transpose()?,
        configuration: optional_json_arg(request, "configuration")?,
        input: optional_json_arg(request, "input")?,
    })
}

fn optional_json_arg(
    request: &InvokeCommandRequest,
    name: &str,
) -> Result<Option<serde_json::Value>, String> {
    request
        .args
        .get(name)
        .filter(|value| !value.is_empty())
        .map(|value| {
            if value.len() > bcode_workflow::MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES {
                return Err(format!("'{name}' exceeds the workflow input byte limit"));
            }
            serde_json::from_str(value).map_err(|error| format!("invalid '{name}' JSON: {error}"))
        })
        .transpose()
}

fn required_arg(request: &InvokeCommandRequest, name: &str) -> Result<String, String> {
    request
        .args
        .get(name)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| format!("{} requires '{name}'", request.command_id))
}

fn parse_arg<T>(request: &InvokeCommandRequest, name: &str) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    required_arg(request, name)?
        .parse()
        .map_err(|error| format!("invalid '{name}': {error}"))
}

fn run_async<F, T>(future: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?
            .block_on(future)
    })
    .join()
    .map_err(|_| "workflow plugin async worker panicked".to_string())?
}

fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("encode_failed", error.to_string()))
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    let mut vtable = bcode_plugin_sdk::static_plugin_vtable!(
        WorkflowPlugin,
        include_str!("../bcode-plugin.toml")
    );
    vtable.cli_registration = Some(cli::registration);
    vtable
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(WorkflowPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_cover_supported_lifecycle_surface() {
        let ids = command_contributions()
            .into_iter()
            .map(|command| command.id)
            .collect::<BTreeSet<_>>();
        for expected in [
            "workflow.list",
            "workflow.templates",
            "workflow.template-describe",
            "workflow.template-start",
            "workflow.package-check",
            "workflow.package-start",
            "workflow.register",
            "workflow.run",
            "workflow.status",
            "workflow.pause",
            "workflow.resume",
            "workflow.cancel",
            "workflow.inspect",
            "workflow.doctor",
            "workflow.repair",
            "workflow.retry-node",
            "workflow.provide-input",
            "workflow.approve-mutation",
            "workflow.deny-mutation",
        ] {
            assert!(ids.contains(expected));
        }
    }

    #[test]
    fn slash_arguments_parse_named_values() {
        assert_eq!(
            parse_arguments("run_id=run-1 definition_version=2"),
            BTreeMap::from([
                ("definition_version".to_string(), "2".to_string()),
                ("run_id".to_string(), "run-1".to_string()),
            ])
        );
    }

    #[test]
    fn slash_arguments_preserve_structured_json_with_spaces() {
        assert_eq!(
            parse_arguments(
                r#"template_id=reference configuration={"prompt":"implement safely", "commands":[["cargo","test"]]} session_id=session-1"#
            ),
            BTreeMap::from([
                (
                    "configuration".to_string(),
                    r#"{"prompt":"implement safely", "commands":[["cargo","test"]]}"#.to_string(),
                ),
                ("session_id".to_string(), "session-1".to_string()),
                ("template_id".to_string(), "reference".to_string()),
            ])
        );
    }

    #[test]
    fn reference_template_maps_iteration_limit_into_persisted_run_limits() {
        let limits = reference_template_run_limits(
            "implementation-verification-commit",
            &serde_json::json!({"iteration_limit": 7}),
        )
        .expect("limits");
        assert_eq!(limits.cycle_cap, 7);
        assert_eq!(limits.node_execution_cap, 112);
        assert!(
            reference_template_run_limits(
                "implementation-verification-commit",
                &serde_json::json!({})
            )
            .is_err()
        );
        assert_eq!(
            reference_template_run_limits("another-template", &serde_json::json!({}))
                .expect("default"),
            bcode_workflow_store::WorkflowRunLimits::default()
        );
    }

    #[test]
    fn template_configuration_is_locally_schema_validated_and_bounded() {
        let schema = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["prompt", "max_iterations"],
            "properties": {
                "prompt": {"type": "string", "minLength": 1},
                "max_iterations": {"type": "integer", "minimum": 1, "maximum": 100}
            }
        });
        assert_eq!(
            validate_template_configuration(
                &schema,
                r#"{"prompt":"implement","max_iterations":3}"#
            )
            .expect("valid")["max_iterations"],
            3
        );
        assert!(
            validate_template_configuration(&schema, r#"{"prompt":"","max_iterations":0}"#)
                .is_err()
        );
        assert!(validate_template_configuration(&schema, "not-json").is_err());
        assert!(validate_template_configuration(&schema, &"x".repeat(1_048_577)).is_err());
    }

    #[test]
    fn package_start_builds_exact_portable_request() {
        let parent_session_id = bcode_session_models::SessionId::new();
        let request = InvokeCommandRequest {
            command_id: "workflow.package-start".to_string(),
            args: BTreeMap::from([
                ("package_id".to_string(), "example/package".to_string()),
                ("export".to_string(), "main".to_string()),
                (
                    "parent_session_id".to_string(),
                    parent_session_id.to_string(),
                ),
                ("package_lock_digest_sha256".to_string(), "a".repeat(64)),
                ("run_id".to_string(), "run-1".to_string()),
                ("parent_session_generation".to_string(), "7".to_string()),
                ("input".to_string(), r#"{"subject":"change"}"#.to_string()),
            ]),
        };
        let start = package_export_start_request(&request).expect("start request");
        assert_eq!(start.package_export.package_id, "example/package");
        assert_eq!(start.package_export.export, "main");
        assert_eq!(start.parent_session_id, parent_session_id);
        assert_eq!(start.parent_session_generation, Some(7));
        assert_eq!(start.input.expect("input")["subject"], "change");
    }

    #[test]
    fn package_start_json_arguments_are_bounded_and_typed() {
        let request = InvokeCommandRequest {
            command_id: "workflow.package-start".to_string(),
            args: BTreeMap::from([
                (
                    "configuration".to_string(),
                    r#"{"mode":"safe"}"#.to_string(),
                ),
                ("input".to_string(), r#"{"subject":"change"}"#.to_string()),
            ]),
        };
        assert_eq!(
            optional_json_arg(&request, "configuration")
                .expect("configuration")
                .expect("value")["mode"],
            "safe"
        );
        assert_eq!(
            optional_json_arg(&request, "input")
                .expect("input")
                .expect("value")["subject"],
            "change"
        );
        let invalid = InvokeCommandRequest {
            command_id: "workflow.package-start".to_string(),
            args: BTreeMap::from([("input".to_string(), "not-json".to_string())]),
        };
        assert!(optional_json_arg(&invalid, "input").is_err());
        let oversized = InvokeCommandRequest {
            command_id: "workflow.package-start".to_string(),
            args: BTreeMap::from([(
                "input".to_string(),
                "x".repeat(bcode_workflow::MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES + 1),
            )]),
        };
        assert!(optional_json_arg(&oversized, "input").is_err());
    }

    #[test]
    fn required_arguments_fail_closed() {
        let request = InvokeCommandRequest {
            command_id: "workflow.run".to_string(),
            args: BTreeMap::new(),
        };
        assert_eq!(
            required_arg(&request, "definition_id").expect_err("missing argument"),
            "workflow.run requires 'definition_id'"
        );
    }
}
