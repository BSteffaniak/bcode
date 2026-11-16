#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Git worktree tool plugin for Bcode.

#[cfg(feature = "tui")]
mod cli;
#[cfg(feature = "tui")]
mod worktree_tui;
#[cfg(feature = "tui")]
pub use worktree_tui::worktree_tui_registry;

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
};
use bcode_plugin_sdk::path::display;
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolArtifact,
    ToolDefinition, ToolInvocationRequest, ToolInvocationResponse, ToolInvocationResult, ToolList,
};
use bcode_worktree_models::{
    WorktreeCreateRequest, WorktreeInfo, WorktreeListRequest, WorktreeRemoveRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

const WORKTREE_PLUGIN_ID: &str = "bcode.worktree";
const WORKTREE_REQUEST_SCHEMA: &str = "bcode.worktree.request";
const WORKTREE_LIST_SCHEMA: &str = "bcode.worktree.list";
const WORKTREE_CREATE_SCHEMA: &str = "bcode.worktree.create_result";
const WORKTREE_REMOVE_SCHEMA: &str = "bcode.worktree.remove_result";

/// worktree plugin.
#[derive(Default)]
pub struct WorktreePlugin;

impl RustPlugin for WorktreePlugin {
    fn component_registrations() -> bcode_plugin_sdk::definition::ProductRegistrations {
        crate::product_registrations()
    }

    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        for command in worktree_command_contributions() {
            registrar
                .register(&command)
                .map_err(|error| PluginError::failed(error.to_string()))?;
        }
        Ok(())
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.interface_id.as_str() {
            TOOL_SERVICE_INTERFACE_ID => invoke_tool_service(&context),
            COMMAND_INTERFACE_ID => invoke_command_service(&context.request),
            _ => ServiceResponse::error(
                "unsupported_interface",
                "unsupported worktree plugin service interface",
            ),
        }
    }
}

fn worktree_command_contributions() -> Vec<CommandContribution> {
    vec![
        worktree_command(
            "command.work-tree.list",
            "List Worktrees",
            "List Git worktrees for the current repository",
        ),
        worktree_command(
            "command.work-tree.createSession",
            "Create Session Worktree",
            "Create a worktree for the current session",
        ),
        worktree_command(
            "command.work-tree.attach",
            "Attach Worktree",
            "Attach current session to an existing worktree",
        ),
        worktree_command(
            "command.work-tree.remove",
            "Remove Worktree",
            "Remove a Git worktree",
        ),
    ]
}

fn worktree_command(id: &str, title: &str, description: &str) -> CommandContribution {
    CommandContribution {
        id: id.to_string(),
        title: title.to_string(),
        description: Some(description.to_string()),
        category: Some("worktree".to_string()),
        surfaces: std::collections::BTreeSet::from([CommandSurface::Palette]),
        slash: None,
        arguments: Vec::new(),
        session: bcode_command::CommandSessionRequirement::Optional,
        execution: bcode_command::CommandExecution::Normal,
        owner: CommandOwner::Plugin {
            plugin_id: "bcode.worktree".to_string(),
        },
        action: CommandAction::Plugin {
            plugin_id: "bcode.worktree".to_string(),
            command_id: id.to_string(),
        },
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct WorktreePreparationDescriptor {
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    remove_path: Option<PathBuf>,
}

fn worktree_workspace_root(
    request: &bcode_tool::ToolPreparationRequest,
) -> Result<Option<PathBuf>, String> {
    let mut matching = request
        .host_context
        .iter()
        .filter(|entry| entry.schema == bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA);
    let Some(entry) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() {
        return Err("duplicate Worktree workspace host context".to_owned());
    }
    if entry.schema_version != bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION {
        return Err(format!(
            "unsupported Worktree workspace host context version {}; expected {}",
            entry.schema_version,
            bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION
        ));
    }
    let root = entry
        .payload
        .get("working_directory")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Worktree workspace host context working_directory is missing".to_owned())?;
    let root = PathBuf::from(root);
    if !root.is_absolute() {
        return Err("Worktree workspace working directory must be absolute".to_owned());
    }
    Ok(Some(root))
}

/// Tool arguments with blank optional values taken as not given. None of the worktree tool
/// fields has a meaningful empty value; required fields dropped this way are refused as missing.
fn without_blank_arguments(arguments: serde_json::Value) -> serde_json::Value {
    bcode_tool::arguments::without_blank_values(arguments, &[])
}

/// Parse a tool request from invocation arguments with blank optional values taken as not given.
fn invocation_arguments<T: serde::de::DeserializeOwned>(
    invocation: &ToolInvocationRequest,
) -> Result<T, serde_json::Error> {
    serde_json::from_value(without_blank_arguments(invocation.arguments.clone()))
}

fn worktree_preparation_descriptor(
    request: &bcode_tool::ToolPreparationRequest,
) -> Result<WorktreePreparationDescriptor, String> {
    let workspace_root = worktree_workspace_root(request)?;
    let arguments = without_blank_arguments(request.invocation.arguments.clone());
    let explicit_cwd = arguments
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from);
    let cwd = match explicit_cwd {
        Some(path) if path.is_absolute() => Some(path),
        Some(path) => Some(
            workspace_root
                .as_ref()
                .ok_or_else(|| "Worktree relative cwd requires workspace host context".to_owned())?
                .join(path),
        ),
        None => workspace_root,
    };
    let remove_path = arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                Ok(path)
            } else {
                cwd.as_ref()
                    .ok_or_else(|| {
                        "Worktree relative remove path requires a prepared cwd".to_owned()
                    })
                    .map(|cwd| cwd.join(path))
            }
        })
        .transpose()?;
    Ok(WorktreePreparationDescriptor { cwd, remove_path })
}

fn worktree_policy_operation(
    request: &bcode_tool::ToolPreparationRequest,
    definition: &ToolDefinition,
) -> Result<bcode_plugin_sdk::ToolPolicyPreparation, String> {
    let descriptor = worktree_preparation_descriptor(request)?;
    let operation = match definition.name.as_str() {
        "worktree.list" => bcode_plugin_sdk::ToolPolicyOperation::ReadOnly,
        "worktree.create" => bcode_plugin_sdk::ToolPolicyOperation::Mutating,
        "worktree.remove" => bcode_plugin_sdk::ToolPolicyOperation::Write {
            paths: descriptor
                .remove_path
                .as_ref()
                .map(|path| path.display().to_string())
                .into_iter()
                .collect(),
            category: "worktree.remove".to_owned(),
        },
        name => return Err(format!("unsupported worktree policy operation: {name}")),
    };
    let category = match definition.name.as_str() {
        "worktree.list" => "worktree.read",
        "worktree.create" => "worktree.create",
        "worktree.remove" => "worktree.remove",
        name => return Err(format!("unsupported worktree policy operation: {name}")),
    };
    Ok(
        bcode_plugin_sdk::ToolPolicyPreparation::new(definition.name != "worktree.list", operation)
            .with_identity(bcode_plugin_sdk::ToolPolicyIdentity {
                aliases: Vec::new(),
                compatibility_aliases: Vec::new(),
                capabilities: vec![category.to_owned()],
                permission_category: Some(category.to_owned()),
            })
            .with_descriptor(serde_json::to_value(descriptor).map_err(|error| error.to_string())?),
    )
}

fn invoke_tool_service(context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    match request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(request),
        bcode_tool::OP_PREPARE_TOOL => prepare_tool_service_response(
            request,
            [list_definition(), create_definition(), remove_definition()],
            worktree_policy_operation,
        ),
        OP_INVOKE_TOOL => invoke_tool(context),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported worktree tool service operation",
        ),
    }
}

fn invoke_command_service(request: &ServiceRequest) -> ServiceResponse {
    if request.operation != OP_INVOKE_COMMAND {
        return ServiceResponse::error(
            "unsupported_operation",
            "unsupported worktree command operation",
        );
    }
    let Ok(request) = serde_json::from_slice::<InvokeCommandRequest>(&request.payload) else {
        return ServiceResponse::error(
            "invalid_request",
            "invalid worktree command invocation request",
        );
    };
    match request.command_id.as_str() {
        "command.work-tree.list" => list_worktrees_command(&request),
        "command.work-tree.createSession"
        | "command.work-tree.attach"
        | "command.work-tree.remove" => command_route_response(&request),
        _ => ServiceResponse::error("unknown_command", "unknown worktree command"),
    }
}

fn list_worktrees_command(request: &InvokeCommandRequest) -> ServiceResponse {
    let Some(cwd) = request
        .context
        .as_ref()
        .map(|context| context.working_directory.clone())
    else {
        return ServiceResponse::error(
            "worktree_cwd_required",
            "worktree commands require canonical working-directory context",
        );
    };
    match bcode_worktree::list_worktrees(&cwd) {
        Ok(response) => {
            let mut lines = vec![format!(
                "Worktrees for {}",
                display(&response.repo_root, &cwd)
            )];
            lines.extend(response.worktrees.into_iter().map(|worktree| {
                let marker = if worktree.is_main { "main" } else { "linked" };
                let branch = worktree.branch.unwrap_or_else(|| "<detached>".to_owned());
                format!("* {marker} {branch} — {}", display(&worktree.path, &cwd))
            }));
            json_response(&InvokeCommandResponse {
                success: true,
                message: Some("shown worktrees".to_string()),
                updated_model: None,
                updated_provider: None,
                updated_thinking: None,
                effects: vec![CommandEffect::AppendText {
                    text: lines.join("\n"),
                    format: bcode_command::CommandTextFormat::Markdown,
                }],
            })
        }
        Err(error) => ServiceResponse::error("worktree_list_failed", error.to_string()),
    }
}

fn command_route_response(request: &InvokeCommandRequest) -> ServiceResponse {
    json_response(&InvokeCommandResponse {
        success: true,
        message: None,
        updated_model: None,
        updated_provider: None,
        updated_thinking: None,
        effects: vec![CommandEffect::OpenPluginSurface {
            surface_kind: request.command_id.clone(),
            instance_id: request.command_id.clone(),
            options: serde_json::to_value(&request.args).unwrap_or(serde_json::Value::Null),
        }],
    })
}

fn list_tools(request: &ServiceRequest) -> ServiceResponse {
    if let Err(error) = request.payload_json::<ListToolsRequest>() {
        return invalid_request(&error);
    }
    json_response(&ToolList::with_runtime_stable_discovery(
        vec![list_definition(), create_definition(), remove_definition()],
        |tool| bcode_tool::ToolDiscoveryPolicy::new(tool.name == "worktree.list"),
    ))
}

fn invoke_tool(context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    let invocation = match request.payload_json::<ToolInvocationRequest>() {
        Ok(invocation) => invocation,
        Err(error) => return invalid_request(&error),
    };
    let descriptor = match serde_json::from_value::<WorktreePreparationDescriptor>(
        invocation.preparation_descriptor.clone(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            return json_response(&tool_error(format!(
                "invalid Worktree preparation descriptor: {error}"
            )));
        }
    };
    if context.cancellation.is_cancelled() {
        return json_response(&tool_error("worktree tool cancelled".to_string()));
    }
    let mut presentation = PrimaryPresentationPublisher::with_limits_and_cancellation(
        context.events.clone(),
        &invocation.tool_call_id,
        WORKTREE_PLUGIN_ID,
        WORKTREE_REQUEST_SCHEMA,
        1,
        bcode_tool::ToolPresentationRetention::RetainLatest,
        context.transient_progress_limits,
        context.cancellation.clone(),
    );
    let _ = presentation.replace(&worktree_request_payload(
        &invocation.name,
        &without_blank_arguments(invocation.arguments.clone()),
    ));
    let response = match invocation.name.as_str() {
        "worktree.list" => invoke_list(&invocation, &descriptor),
        "worktree.create" => invoke_create(&invocation, &descriptor),
        "worktree.remove" => invoke_remove(&invocation, &descriptor),
        _ => ToolInvocationResponse {
            output: format!("unsupported worktree tool: {}", invocation.name),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            result: None,
        },
    };
    json_response(&response)
}

fn invoke_list(
    invocation: &ToolInvocationRequest,
    descriptor: &WorktreePreparationDescriptor,
) -> ToolInvocationResponse {
    let _request = match invocation_arguments::<WorktreeListRequest>(invocation) {
        Ok(request) => request,
        Err(error) => return tool_error(error.to_string()),
    };
    let Some(cwd) = descriptor.cwd.as_ref() else {
        return tool_error("worktree.list preparation descriptor is missing cwd".to_string());
    };
    match bcode_worktree::list_worktrees(cwd) {
        Ok(response) => json_tool_response_with_artifact(
            &response,
            &invocation.tool_call_id,
            "list",
            WORKTREE_LIST_SCHEMA,
            "Worktrees",
        ),
        Err(error) => tool_error(error.to_string()),
    }
}

fn invoke_create(
    invocation: &ToolInvocationRequest,
    descriptor: &WorktreePreparationDescriptor,
) -> ToolInvocationResponse {
    let mut request = match invocation_arguments::<WorktreeCreateRequest>(invocation) {
        Ok(request) => request,
        Err(error) => return tool_error(error.to_string()),
    };
    let Some(cwd) = descriptor.cwd.clone() else {
        return tool_error("worktree.create preparation descriptor is missing cwd".to_string());
    };
    request.cwd = Some(cwd.clone());
    let config_paths = bcode_config::default_config_paths_from(&cwd);
    let config = match bcode_config::load_config_from_paths(&config_paths) {
        Ok(config) => config,
        Err(error) => return tool_error(error.to_string()),
    };
    match bcode_worktree::create_worktree(&config, &request, &cwd) {
        Ok(response) => json_tool_response_with_artifact(
            &response,
            &invocation.tool_call_id,
            "create",
            WORKTREE_CREATE_SCHEMA,
            "Worktree created",
        ),
        Err(error) => tool_error(error.to_string()),
    }
}

fn invoke_remove(
    invocation: &ToolInvocationRequest,
    descriptor: &WorktreePreparationDescriptor,
) -> ToolInvocationResponse {
    let mut request = match invocation_arguments::<WorktreeRemoveRequest>(invocation) {
        Ok(request) => request,
        Err(error) => return tool_error(error.to_string()),
    };
    let Some(cwd) = descriptor.cwd.clone() else {
        return tool_error("worktree.remove preparation descriptor is missing cwd".to_string());
    };
    let Some(path) = descriptor.remove_path.clone() else {
        return tool_error(
            "worktree.remove preparation descriptor is missing remove path".to_string(),
        );
    };
    request.cwd = Some(cwd.clone());
    request.path = path;
    match bcode_worktree::remove_worktree(&cwd, &request.path, request.force) {
        Ok(response) => json_tool_response_with_artifact(
            &response,
            &invocation.tool_call_id,
            "remove",
            WORKTREE_REMOVE_SCHEMA,
            "Worktree removed",
        ),
        Err(error) => tool_error(error.to_string()),
    }
}

fn worktree_request_payload(operation: &str, arguments: &serde_json::Value) -> serde_json::Value {
    arguments.as_object().map_or_else(
        || json!({"operation": operation, "arguments": arguments}),
        |arguments| {
            let mut payload = arguments.clone();
            payload.insert(
                "operation".to_owned(),
                serde_json::Value::String(operation.to_owned()),
            );
            serde_json::Value::Object(payload)
        },
    )
}

fn list_definition() -> ToolDefinition {
    ToolDefinition {
        name: "worktree.list".to_string(),
        description: "List Git worktrees for the current repository.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "cwd": { "type": "string", "description": "Optional repository discovery directory" }
            }
        }),
    }
}

fn create_definition() -> ToolDefinition {
    ToolDefinition {
        name: "worktree.create".to_string(),
        description: "Create a Git worktree using Bcode worktree configuration.".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string" },
                "cwd": { "type": "string" },
                "path": { "type": "string" },
                "branch": { "type": "string" },
                "new_branch": { "type": "string" },
                "base_ref": { "type": "string", "enum": ["auto", "default_branch", "head"] },
                "detach": { "type": "boolean" },
                "force": { "type": "boolean" },
                "no_setup": { "type": "boolean" }
            }
        }),
    }
}

fn remove_definition() -> ToolDefinition {
    ToolDefinition {
        name: "worktree.remove".to_string(),
        description: "Remove a registered Git worktree without deleting its branch.".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "cwd": { "type": "string" },
                "path": { "type": "string" },
                "force": { "type": "boolean" }
            }
        }),
    }
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

fn json_tool_response_with_artifact<T: Serialize>(
    value: &T,
    tool_call_id: &str,
    artifact_suffix: &str,
    schema: &str,
    title: &str,
) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value).and_then(|output| {
        let payload = serde_json::to_value(value)?;
        Ok((output, payload))
    }) {
        Ok((output, payload)) => ToolInvocationResponse {
            output,
            is_error: false,
            content: Vec::new(),
            full_output: None,
            result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(ToolArtifact {
                    artifact_id: format!("{tool_call_id}-worktree-{artifact_suffix}"),
                    producer_plugin_id: WORKTREE_PLUGIN_ID.to_string(),
                    schema: schema.to_string(),
                    schema_version: 1,
                    tool_call_id: Some(tool_call_id.to_string()),
                    title: Some(title.to_string()),
                    metadata: payload,
                    refs: Vec::new(),
                }),
            }),
        },
        Err(error) => tool_error(error.to_string()),
    }
}

const fn tool_error(output: String) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output,
        is_error: true,
        content: Vec::new(),
        full_output: None,
        result: None,
    }
}

/// Return the plugin-owned local CLI contribution for bundled hosts.
#[cfg(feature = "tui")]
#[must_use]
pub fn cli_registration() -> bcode_plugin_sdk::StaticCliRegistration {
    cli::registration()
}

// Identity and service declarations come from the embedded manifest only.
bcode_plugin_sdk::define_plugin!(serial WorktreePlugin, include_str!("../bcode-plugin.toml"));

fn product_registrations() -> bcode_plugin_sdk::definition::ProductRegistrations {
    let registrations = bcode_plugin_sdk::definition::ProductRegistrations::default();
    #[cfg(feature = "tui")]
    let registrations = bcode_plugin_sdk::definition::ProductRegistrations {
        frontend: Some(worktree_tui_registry),
        cli: Some(cli::registration()),
        ..registrations
    };
    registrations
}

/// Bind this implementation to the host's invocation-scoped capability resolver.
/// The resolver must retain live cancellation, caller provenance and authorized bridges.
#[must_use]
pub fn plugin_definition_with_host(
    host: impl Fn(
        bcode_plugin_sdk::definition::PluginCreateContext,
    ) -> std::sync::Arc<dyn bcode_plugin_sdk::definition::ProductInvocationHost>
    + Send
    + Sync
    + 'static,
) -> bcode_plugin_sdk::definition::PluginDefinition {
    bcode_plugin_sdk::definition::rust_plugin_definition_with_host::<WorktreePlugin>(
        plugin_definition().metadata,
        host,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn workspace_context(path: &Path) -> Vec<bcode_tool::ToolHostContextEntry> {
        vec![bcode_tool::ToolHostContextEntry {
            schema: bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA.to_owned(),
            schema_version: bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION,
            payload: serde_json::json!({"working_directory": path}),
        }]
    }

    #[test]
    fn worktree_owner_prepares_mixed_permission_policy() {
        for (definition, arguments, expected_permission) in [
            (list_definition(), serde_json::Value::Null, false),
            (
                create_definition(),
                serde_json::json!({"name": "feature"}),
                true,
            ),
            (
                remove_definition(),
                serde_json::json!({"path": "/tmp/worktree"}),
                true,
            ),
        ] {
            let request = bcode_tool::ToolPreparationRequest {
                invocation: bcode_tool::ToolInvocationDescriptor {
                    invocation_id: "call".to_owned(),
                    tool_name: definition.name.clone(),
                    arguments,
                },
                host_context: workspace_context(Path::new("/tmp/workspace")),
            };
            let policy = worktree_policy_operation(&request, &definition).expect("worktree policy");
            assert_eq!(policy.requires_permission, expected_permission);
            let descriptor =
                serde_json::from_value::<WorktreePreparationDescriptor>(policy.descriptor)
                    .expect("Worktree descriptor");
            assert_eq!(descriptor.cwd, Some(PathBuf::from("/tmp/workspace")));
            if definition.name == "worktree.remove" {
                assert_eq!(descriptor.remove_path, Some(PathBuf::from("/tmp/worktree")));
                assert_eq!(
                    policy.operation,
                    bcode_plugin_sdk::ToolPolicyOperation::Write {
                        paths: vec!["/tmp/worktree".to_owned()],
                        category: "worktree.remove".to_owned(),
                    }
                );
            }
        }
    }

    #[test]
    fn worktree_preparation_preserves_explicit_cwd_precedence_and_resolves_remove_path() {
        let definition = remove_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".to_owned(),
                tool_name: definition.name.clone(),
                arguments: serde_json::json!({
                    "cwd": "nested/repository",
                    "path": "../worktree"
                }),
            },
            host_context: workspace_context(Path::new("/tmp/workspace")),
        };

        let prepared =
            worktree_policy_operation(&request, &definition).expect("Worktree preparation");
        let descriptor =
            serde_json::from_value::<WorktreePreparationDescriptor>(prepared.descriptor)
                .expect("Worktree descriptor");

        assert_eq!(
            descriptor.cwd,
            Some(PathBuf::from("/tmp/workspace/nested/repository"))
        );
        assert_eq!(
            descriptor.remove_path,
            Some(PathBuf::from(
                "/tmp/workspace/nested/repository/../worktree"
            ))
        );
        assert_eq!(
            prepared.operation,
            bcode_plugin_sdk::ToolPolicyOperation::Write {
                paths: vec!["/tmp/workspace/nested/repository/../worktree".to_owned()],
                category: "worktree.remove".to_owned(),
            }
        );
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn committed_repository() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("repository directory");
        git(repo.path(), &["init", "--initial-branch", "main"]);
        git(repo.path(), &["config", "user.email", "bcode@example.test"]);
        git(repo.path(), &["config", "user.name", "Bcode Test"]);
        std::fs::write(repo.path().join("README.md"), "test\n").expect("readme");
        git(repo.path(), &["add", "README.md"]);
        git(repo.path(), &["commit", "-m", "initial"]);
        repo
    }

    fn create_invocation(arguments: serde_json::Value) -> ToolInvocationRequest {
        ToolInvocationRequest {
            tool_call_id: "call".to_owned(),
            name: "worktree.create".to_owned(),
            arguments,
            preparation_descriptor: serde_json::Value::Null,
        }
    }

    #[test]
    fn worktree_create_takes_blank_optional_arguments_as_not_given() {
        let repo = committed_repository();
        let workspaces = tempfile::tempdir().expect("worktree directory");
        let descriptor = WorktreePreparationDescriptor {
            cwd: Some(repo.path().to_path_buf()),
            remove_path: None,
        };
        for (index, arguments) in [
            serde_json::json!({
                "name": "new-branch",
                "new_branch": "feature-new",
                "branch": "",
                "base_ref": "head",
                "detach": false,
                "no_setup": true,
            }),
            serde_json::json!({
                "name": "detached",
                "branch": " ",
                "new_branch": null,
                "base_ref": "head",
                "detach": true,
                "no_setup": true,
            }),
        ]
        .into_iter()
        .enumerate()
        {
            let path = workspaces.path().join(format!("worktree-{index}"));
            let mut arguments = arguments;
            arguments["path"] = serde_json::json!(path);
            arguments["cwd"] = serde_json::json!("");
            let response = invoke_create(&create_invocation(arguments), &descriptor);
            assert!(!response.is_error, "{}", response.output);
            assert!(path.join("README.md").is_file());
        }
        assert_eq!(
            git(
                &workspaces.path().join("worktree-0"),
                &["branch", "--show-current"]
            ),
            "feature-new"
        );
        assert_eq!(
            git(
                &workspaces.path().join("worktree-1"),
                &["branch", "--show-current"]
            ),
            ""
        );
    }

    #[test]
    fn worktree_create_still_refuses_conflicting_or_missing_arguments() {
        let repo = committed_repository();
        let descriptor = WorktreePreparationDescriptor {
            cwd: Some(repo.path().to_path_buf()),
            remove_path: None,
        };
        let conflicting = invoke_create(
            &create_invocation(serde_json::json!({
                "name": "both",
                "branch": "main",
                "new_branch": "feature",
            })),
            &descriptor,
        );
        assert!(conflicting.is_error);
        assert!(conflicting.output.contains("either branch or new_branch"));

        let missing_name = invoke_create(
            &create_invocation(serde_json::json!({"name": "  ", "new_branch": "feature"})),
            &descriptor,
        );
        assert!(missing_name.is_error);
        assert!(missing_name.output.contains("name"));
    }

    #[test]
    fn worktree_preparation_takes_blank_cwd_and_path_as_not_given() {
        let definition = remove_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".to_owned(),
                tool_name: definition.name,
                arguments: serde_json::json!({"cwd": "", "path": "  "}),
            },
            host_context: workspace_context(Path::new("/tmp/workspace")),
        };

        let descriptor = worktree_preparation_descriptor(&request).expect("preparation");

        assert_eq!(descriptor.cwd, Some(PathBuf::from("/tmp/workspace")));
        assert_eq!(descriptor.remove_path, None);
    }

    #[test]
    fn worktree_relative_cwd_requires_workspace_context() {
        let definition = list_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".to_owned(),
                tool_name: definition.name.clone(),
                arguments: serde_json::json!({"cwd": "repository"}),
            },
            host_context: Vec::new(),
        };

        let error = worktree_policy_operation(&request, &definition)
            .expect_err("relative cwd without workspace");

        assert!(error.contains("requires workspace host context"));
    }

    #[test]
    fn worktree_plugin_registers_palette_commands_from_plugin_code() {
        extern "C" fn register_command(
            payload: *const u8,
            payload_len: usize,
            user_data: *mut std::ffi::c_void,
        ) {
            assert!(!payload.is_null());
            assert!(!user_data.is_null());
            let bytes = unsafe { std::slice::from_raw_parts(payload, payload_len) };
            let contribution = serde_json::from_slice::<CommandContribution>(bytes)
                .expect("command contribution should decode");
            let registry = unsafe { &mut *(user_data.cast::<bcode_command::CommandRegistry>()) };
            registry.register(contribution);
        }

        let mut plugin = WorktreePlugin;
        let mut registry = bcode_command::CommandRegistry::new();
        plugin
            .register_commands(CommandRegistrar::new(
                Some(register_command),
                std::ptr::from_mut(&mut registry).cast::<std::ffi::c_void>(),
            ))
            .expect("worktree plugin should register commands");

        let commands = registry.commands_for_surface(&CommandSurface::Palette);

        assert!(commands.iter().any(|command| {
            command.id == "command.work-tree.list"
                && command.action
                    == CommandAction::Plugin {
                        plugin_id: "bcode.worktree".to_string(),
                        command_id: "command.work-tree.list".to_string(),
                    }
        }));
        assert!(
            commands
                .iter()
                .any(|command| command.id == "command.work-tree.createSession")
        );
        assert!(
            commands
                .iter()
                .any(|command| command.id == "command.work-tree.attach")
        );
        assert!(
            commands
                .iter()
                .any(|command| command.id == "command.work-tree.remove")
        );
    }
}
