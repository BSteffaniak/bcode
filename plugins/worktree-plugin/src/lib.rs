#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled Git worktree tool plugin for Bcode.

use bcode_command::{CommandAction, CommandContribution, CommandOwner, CommandSurface};
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolDefinition,
    ToolInvocationRequest, ToolInvocationResponse, ToolList, ToolPresentationField,
    ToolPresentationFieldKind, ToolRequestPresentationMetadata, ToolSideEffect,
};
use bcode_worktree_models::{WorktreeCreateRequest, WorktreeListRequest, WorktreeRemoveRequest};
use serde::Serialize;
use serde_json::json;
use std::path::PathBuf;

/// Bundled worktree plugin.
#[derive(Default)]
pub struct WorktreePlugin;

impl RustPlugin for WorktreePlugin {
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
        owner: CommandOwner::Plugin {
            plugin_id: "bcode.worktree".to_string(),
        },
        action: CommandAction::Host {
            route: id.to_string(),
        },
    }
}

fn invoke_tool_service(context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    match request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(request),
        OP_INVOKE_TOOL => invoke_tool(context),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported worktree tool service operation",
        ),
    }
}

fn list_tools(request: &ServiceRequest) -> ServiceResponse {
    if let Err(error) = request.payload_json::<ListToolsRequest>() {
        return invalid_request(&error);
    }
    json_response(&ToolList {
        tools: vec![list_definition(), create_definition(), remove_definition()],
    })
}

fn invoke_tool(context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    let invocation = match request.payload_json::<ToolInvocationRequest>() {
        Ok(invocation) => invocation,
        Err(error) => return invalid_request(&error),
    };
    if context.cancellation.is_cancelled() {
        return json_response(&tool_error("worktree tool cancelled".to_string()));
    }
    let response = match invocation.name.as_str() {
        "worktree.list" => invoke_list(&invocation),
        "worktree.create" => invoke_create(&invocation),
        "worktree.remove" => invoke_remove(&invocation),
        _ => ToolInvocationResponse {
            output: format!("unsupported worktree tool: {}", invocation.name),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        },
    };
    json_response(&response)
}

fn invoke_list(invocation: &ToolInvocationRequest) -> ToolInvocationResponse {
    let request = match serde_json::from_value::<WorktreeListRequest>(invocation.arguments.clone())
    {
        Ok(request) => request,
        Err(error) => return tool_error(error.to_string()),
    };
    let cwd = request
        .cwd
        .or_else(|| invocation.cwd.clone())
        .unwrap_or_else(current_dir);
    match bcode_worktree::list_worktrees(&cwd) {
        Ok(response) => json_tool_response(&response),
        Err(error) => tool_error(error.to_string()),
    }
}

fn invoke_create(invocation: &ToolInvocationRequest) -> ToolInvocationResponse {
    let mut request =
        match serde_json::from_value::<WorktreeCreateRequest>(invocation.arguments.clone()) {
            Ok(request) => request,
            Err(error) => return tool_error(error.to_string()),
        };
    if request.cwd.is_none() {
        request.cwd.clone_from(&invocation.cwd);
    }
    let cwd = request.cwd.clone().unwrap_or_else(current_dir);
    let config = match bcode_config::load_config() {
        Ok(config) => config,
        Err(error) => return tool_error(error.to_string()),
    };
    match bcode_worktree::create_worktree(&config, &request, &cwd) {
        Ok(response) => json_tool_response(&response),
        Err(error) => tool_error(error.to_string()),
    }
}

fn invoke_remove(invocation: &ToolInvocationRequest) -> ToolInvocationResponse {
    let request =
        match serde_json::from_value::<WorktreeRemoveRequest>(invocation.arguments.clone()) {
            Ok(request) => request,
            Err(error) => return tool_error(error.to_string()),
        };
    let cwd = request
        .cwd
        .clone()
        .or_else(|| invocation.cwd.clone())
        .unwrap_or_else(current_dir);
    match bcode_worktree::remove_worktree(&cwd, &request.path, request.force) {
        Ok(response) => json_tool_response(&response),
        Err(error) => tool_error(error.to_string()),
    }
}

fn tool_ui(
    activity_label: &str,
    title: &str,
    fields: Vec<ToolPresentationField>,
) -> bcode_tool::ToolUiMetadata {
    bcode_tool::ToolUiMetadata {
        activity_label: Some(activity_label.to_string()),
        live_argument_preview: None,

        request_presentation: Some(ToolRequestPresentationMetadata {
            title: title.to_string(),
            fields,
            preview: None,
        }),
    }
}

fn field(
    label: &str,
    argument: &str,
    kind: ToolPresentationFieldKind,
    optional: bool,
) -> ToolPresentationField {
    ToolPresentationField {
        label: label.to_string(),
        argument: argument.to_string(),
        kind,
        optional,
    }
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
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: bcode_tool::ToolPolicyMetadata {
            aliases: vec!["worktree.read".to_string()],
            compatibility_aliases: Vec::new(),
            capabilities: Vec::new(),
            permission_category: Some("worktree.read".to_string()),
            argument_extractors: Vec::new(),
        },
        ui: tool_ui(
            "listing worktrees",
            "List worktrees",
            vec![field(
                "Working directory",
                "cwd",
                ToolPresentationFieldKind::Path,
                true,
            )],
        ),
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
        side_effect: ToolSideEffect::ExecuteProcess,
        requires_permission: true,
        policy: bcode_tool::ToolPolicyMetadata {
            aliases: vec!["worktree.create".to_string()],
            compatibility_aliases: Vec::new(),
            capabilities: Vec::new(),
            permission_category: Some("worktree.create".to_string()),
            argument_extractors: Vec::new(),
        },
        ui: tool_ui(
            "creating worktree",
            "Create worktree",
            vec![
                field("Name", "name", ToolPresentationFieldKind::Text, false),
                field("Path", "path", ToolPresentationFieldKind::Path, true),
                field("Branch", "branch", ToolPresentationFieldKind::Text, true),
                field(
                    "New branch",
                    "new_branch",
                    ToolPresentationFieldKind::Text,
                    true,
                ),
            ],
        ),
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
        side_effect: ToolSideEffect::ExecuteProcess,
        requires_permission: true,
        policy: bcode_tool::ToolPolicyMetadata {
            aliases: vec!["worktree.remove".to_string()],
            compatibility_aliases: Vec::new(),
            capabilities: Vec::new(),
            permission_category: Some("worktree.remove".to_string()),
            argument_extractors: vec![bcode_tool::ToolArgumentExtractor {
                kind: bcode_tool::ToolArgumentKind::WritePath,
                argument: "path".to_string(),
            }],
        },
        ui: tool_ui(
            "removing worktree",
            "Remove worktree",
            vec![
                field("Path", "path", ToolPresentationFieldKind::Path, false),
                field("Force", "force", ToolPresentationFieldKind::Boolean, true),
            ],
        ),
    }
}

fn current_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
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

fn json_tool_response<T: Serialize>(value: &T) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value) {
        Ok(output) => ToolInvocationResponse {
            output,
            is_error: false,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
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
        host_action: None,
        result: None,
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(WorktreePlugin, include_str!("../bcode-plugin.toml"))
}

bcode_plugin_sdk::export_plugin!(WorktreePlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

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
                    == CommandAction::Host {
                        route: "command.work-tree.list".to_string(),
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
