#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled Git worktree tool plugin for Bcode.

use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolDefinition,
    ToolInvocationRequest, ToolInvocationResponse, ToolList, ToolSideEffect,
};
use bcode_worktree_models::{WorktreeCreateRequest, WorktreeListRequest, WorktreeRemoveRequest};
use serde::Serialize;
use serde_json::json;
use std::path::PathBuf;

/// Bundled worktree plugin.
#[derive(Default)]
pub struct WorktreePlugin;

impl RustPlugin for WorktreePlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.interface_id.as_str() {
            TOOL_SERVICE_INTERFACE_ID => invoke_tool_service(&context.request),
            _ => ServiceResponse::error(
                "unsupported_interface",
                "unsupported worktree plugin service interface",
            ),
        }
    }
}

fn invoke_tool_service(request: &ServiceRequest) -> ServiceResponse {
    match request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(request),
        OP_INVOKE_TOOL => invoke_tool(request),
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

fn invoke_tool(request: &ServiceRequest) -> ServiceResponse {
    let invocation = match request.payload_json::<ToolInvocationRequest>() {
        Ok(invocation) => invocation,
        Err(error) => return invalid_request(&error),
    };
    let response = match invocation.name.as_str() {
        "worktree.list" => invoke_list(&invocation),
        "worktree.create" => invoke_create(&invocation),
        "worktree.remove" => invoke_remove(&invocation),
        _ => ToolInvocationResponse {
            output: format!("unsupported worktree tool: {}", invocation.name),
            is_error: true,
            content: Vec::new(),
            full_output: None,
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
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(WorktreePlugin, include_str!("../bcode-plugin.toml"))
}

bcode_plugin_sdk::export_plugin!(WorktreePlugin, include_str!("../bcode-plugin.toml"));
