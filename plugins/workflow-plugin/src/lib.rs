#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Workflow product integration plugin for Bcode.

#[cfg(feature = "static-bundled")]
mod cli;
pub mod tui;

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
};
use bcode_plugin_sdk::prelude::*;
use bcode_plugin_sdk::{OP_SESSION_STATUS, SESSION_STATUS_INTERFACE_ID};
use std::collections::{BTreeMap, BTreeSet};

const PLUGIN_ID: &str = "bcode.workflow";
const SURFACE_KIND: &str = "workflow.status";
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
            "workflow.template-start",
            "Workflow: Start Template",
            "Start a validated workflow template",
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

#[allow(clippy::too_many_lines)]
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
            format!("{} durable workflow runs", runs.len())
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
        "workflow.template-describe" => {
            let owner_plugin_id = required_arg(&request, "owner_plugin_id")?;
            let template_id = required_arg(&request, "template_id")?;
            let template_version = parse_arg::<u32>(&request, "template_version")?;
            let template = client
                .describe_workflow_template(owner_plugin_id, template_id, template_version)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "workflow template not found or disabled".to_string())?;
            options.insert(
                "template".to_string(),
                serde_json::to_value(&template).map_err(|error| error.to_string())?,
            );
            if let Some(session_id) = request.args.get("session_id") {
                options.insert("session_id".to_string(), serde_json::json!(session_id));
            }
            if let Some(configuration) = request.args.get("configuration") {
                let configuration = validate_template_configuration(
                    &template.template.configuration_schema.schema,
                    configuration,
                )?;
                options.insert("configuration".to_string(), configuration);
            }
            format!(
                "workflow template {} v{}",
                template.template.template_id, template.template.template_version
            )
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
            let configuration = validate_template_configuration(
                &described.template.configuration_schema.schema,
                &required_arg(&request, "configuration")?,
            )?;
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
                    limits: bcode_workflow_store::WorkflowRunLimits::default(),
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert("runs".to_string(), serde_json::json!([started.run]));
            "workflow template started".to_string()
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
            surface_kind: SURFACE_KIND.to_string(),
            instance_id: "workflow-status".to_string(),
            options: serde_json::Value::Object(options),
        }],
    })
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
    #[allow(clippy::too_many_lines)]
    fn manifest_contributes_bounded_reference_template_configuration() {
        let manifest: bcode_plugin::PluginManifest =
            toml::from_str(include_str!("../bcode-plugin.toml")).expect("manifest");
        let template = manifest
            .workflow_templates
            .iter()
            .find(|template| template.template_id == "implementation-verification-commit")
            .expect("reference template");
        template.validate().expect("template validates");
        assert_eq!(template.template_version, 1);
        let implementation = &template.definition.nodes["implementation"];
        assert_eq!(implementation.kind, bcode_workflow::NodeKind::Agent);
        let configuration: bcode_workflow::WorkflowAgentConfiguration =
            serde_json::from_value(implementation.configuration.clone())
                .expect("typed implementation agent configuration");
        configuration
            .validate()
            .expect("implementation agent validates");
        assert_eq!(
            configuration.execution_target,
            bcode_workflow::AgentExecutionTarget::SharedParentSequential
        );
        assert_eq!(
            configuration.tool_capability,
            bcode_workflow::WorkflowToolCapability::Mutating
        );
        let evaluation = &template.definition.nodes["evaluation"];
        assert_eq!(evaluation.kind, bcode_workflow::NodeKind::Agent);
        let evaluation_configuration: bcode_workflow::WorkflowAgentConfiguration =
            serde_json::from_value(evaluation.configuration.clone())
                .expect("typed evaluation agent configuration");
        evaluation_configuration
            .validate()
            .expect("evaluation agent validates");
        assert!(evaluation_configuration.read_only);
        assert_eq!(
            evaluation_configuration.tool_capability,
            bcode_workflow::WorkflowToolCapability::ReadOnly
        );
        assert!(template.definition.edges.iter().any(|edge| {
            edge.from == "implementation"
                && edge.to == "evaluation"
                && edge.kind == bcode_workflow::EdgeKind::Direct
        }));
        let verification = &template.definition.nodes["verification"];
        assert_eq!(verification.kind, bcode_workflow::NodeKind::PluginBlock);
        let verification_block: bcode_workflow::WorkflowBlockDefinition =
            serde_json::from_value(verification.configuration.clone())
                .expect("typed shell block configuration");
        verification_block
            .validate()
            .expect("shell block validates");
        let shell_manifest: bcode_plugin::PluginManifest =
            toml::from_str(include_str!("../../shell-plugin/bcode-plugin.toml"))
                .expect("shell manifest");
        let declared_shell = shell_manifest
            .services
            .iter()
            .flat_map(|service| &service.workflow_blocks)
            .find(|block| block.block_id == "shell.command-plan")
            .expect("declared shell block");
        assert_eq!(&verification_block, declared_shell);
        assert_eq!(verification_block.plugin_id, "bcode.shell");
        assert_eq!(verification_block.block_id, "shell.command-plan");
        assert!(verification_block.authorization.explicit_grant_required);
        let verification_edge = template
            .definition
            .edges
            .iter()
            .find(|edge| edge.from == "evaluation" && edge.to == "verification")
            .expect("verification edge");
        let verification_transform = verification_edge
            .transform
            .as_ref()
            .expect("command-plan projection");
        let state = serde_json::json!({
            "command_plan": {
                "version": 1,
                "cwd": ".",
                "commands": [{
                    "argv": ["cargo", "test"],
                    "timeout_ms": 300_000,
                    "continue_on_nonzero": false
                }],
                "environment": {"inherit": true, "set": {}},
                "output": {"preview_bytes": 4096, "artifact_spill": true}
            }
        });
        assert_eq!(
            verification_transform
                .evaluate(&[bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_STATE,
                    value: &state,
                }])
                .expect("command plan projection"),
            state["command_plan"]
        );
        let branch = &template.definition.nodes["verification_decision"];
        assert_eq!(branch.kind, bcode_workflow::NodeKind::Branch);
        assert_eq!(branch.configuration["predicate"]["path"], "passed");
        let passed = serde_json::json!({"passed": true});
        let failed = serde_json::json!({"passed": false});
        let predicate: bcode_workflow::PredicateExpression =
            serde_json::from_value(branch.configuration["predicate"].clone())
                .expect("verification predicate");
        assert!(predicate.evaluate_value(&passed).expect("passed branch"));
        assert!(!predicate.evaluate_value(&failed).expect("failed branch"));
        let git_prepare = &template.definition.nodes["git_prepare"];
        assert_eq!(git_prepare.kind, bcode_workflow::NodeKind::PluginBlock);
        let git_prepare_block: bcode_workflow::WorkflowBlockDefinition =
            serde_json::from_value(git_prepare.configuration.clone())
                .expect("typed Git prepare block");
        git_prepare_block.validate().expect("Git prepare validates");
        let git_manifest: bcode_plugin::PluginManifest =
            toml::from_str(include_str!("../../git-plugin/bcode-plugin.toml"))
                .expect("Git manifest");
        let declared_prepare = git_manifest
            .services
            .iter()
            .flat_map(|service| &service.workflow_blocks)
            .find(|block| block.block_id == "git.prepare")
            .expect("declared Git prepare block");
        assert_eq!(&git_prepare_block, declared_prepare);
        assert_eq!(git_prepare_block.block_id, "git.prepare");
        assert_eq!(
            git_prepare_block.effect,
            bcode_workflow::WorkflowBlockEffect::ReadOnly
        );
        assert!(!git_prepare_block.authorization.explicit_grant_required);
        let prepare_transform = template
            .definition
            .edges
            .iter()
            .find(|edge| edge.from == "verified" && edge.to == "git_prepare")
            .and_then(|edge| edge.transform.as_ref())
            .expect("Git prepare request transform");
        assert_eq!(
            prepare_transform.evaluate(&[]).expect("prepare request"),
            serde_json::json!({
                "include_prefixes": [],
                "exclude_prefixes": [],
                "max_paths": 10_000
            })
        );
        let git_compose = &template.definition.nodes["git_compose"];
        assert_eq!(git_compose.kind, bcode_workflow::NodeKind::PluginBlock);
        let git_compose_block: bcode_workflow::WorkflowBlockDefinition =
            serde_json::from_value(git_compose.configuration.clone())
                .expect("typed Git compose block");
        let declared_compose = git_manifest
            .services
            .iter()
            .flat_map(|service| &service.workflow_blocks)
            .find(|block| block.block_id == "git.compose-commit")
            .expect("declared Git compose block");
        assert_eq!(&git_compose_block, declared_compose);
        let compose_transform = template
            .definition
            .edges
            .iter()
            .find(|edge| edge.from == "git_prepare" && edge.to == "git_compose")
            .and_then(|edge| edge.transform.as_ref())
            .expect("Git compose request transform");
        let preparation = serde_json::json!({
            "repository_root": "/repo",
            "head": "abc",
            "changed_paths": [{"path": "src/lib.rs", "status": "modified"}]
        });
        let compose_state = serde_json::json!({"implementation_prompt": "Implement workflows"});
        let composed = compose_transform
            .evaluate(&[
                bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                    value: &preparation,
                },
                bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_STATE,
                    value: &compose_state,
                },
            ])
            .expect("compose request");
        assert_eq!(composed["preparation"], preparation);
        assert_eq!(composed["message"]["description"], "Implement workflows");
        assert_eq!(composed["no_changes"], "no_op");
        let git_commit = &template.definition.nodes["git_commit"];
        assert_eq!(git_commit.kind, bcode_workflow::NodeKind::PluginBlock);
        let git_commit_block: bcode_workflow::WorkflowBlockDefinition =
            serde_json::from_value(git_commit.configuration.clone())
                .expect("typed Git commit block");
        let declared_commit = git_manifest
            .services
            .iter()
            .flat_map(|service| &service.workflow_blocks)
            .find(|block| block.block_id == "git.commit")
            .expect("declared Git commit block");
        assert_eq!(&git_commit_block, declared_commit);
        assert!(git_commit_block.authorization.explicit_grant_required);
        assert_eq!(
            git_commit_block.reconciliation,
            bcode_workflow::WorkflowBlockReconciliation::RepairRequired
        );
        let commit_transform = template
            .definition
            .edges
            .iter()
            .find(|edge| edge.from == "commit_decision" && edge.to == "git_commit")
            .and_then(|edge| edge.transform.as_ref())
            .expect("exact commit request projection");
        let ready = serde_json::json!({
            "status": "ready",
            "request": {
                "repo_path": "/repo",
                "expected_head": "abc",
                "message": "Implement workflows",
                "paths": ["src/lib.rs"]
            }
        });
        assert_eq!(
            commit_transform
                .evaluate(&[bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                    value: &ready,
                }])
                .expect("commit projection"),
            ready["request"]
        );
        let required = template.configuration_schema.schema["required"]
            .as_array()
            .expect("required fields");
        for field in [
            "implementation_prompt",
            "stop_condition",
            "iteration_limit",
            "command_plan",
            "command_timeout_ms",
            "verification_policy",
            "commit_behavior",
            "commit_message_skill",
        ] {
            assert!(required.iter().any(|value| value == field));
        }
        assert_eq!(template.required_plugins, ["bcode.shell", "bcode.git"]);
    }

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
