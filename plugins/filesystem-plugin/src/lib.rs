#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled filesystem service plugin for Bcode.

use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolDefinition,
    ToolInvocationRequest, ToolInvocationResponse, ToolList, ToolSideEffect,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

const FILESYSTEM_INTERFACE_ID: &str = "bcode.filesystem/v1";

/// Bundled filesystem plugin.
#[derive(Default)]
pub struct FilesystemPlugin;

impl RustPlugin for FilesystemPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.interface_id.as_str() {
            FILESYSTEM_INTERFACE_ID => invoke_filesystem_service(&context.request),
            TOOL_SERVICE_INTERFACE_ID => invoke_tool_service(&context.request),
            _ => ServiceResponse::error(
                "unsupported_interface",
                "unsupported filesystem plugin service interface",
            ),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReadRequest {
    path: PathBuf,
}

#[derive(Debug, Serialize)]
struct ReadResponse {
    contents: String,
}

#[derive(Debug, Deserialize)]
struct WriteRequest {
    path: PathBuf,
    contents: String,
}

#[derive(Debug, Serialize)]
struct WriteResponse {
    bytes_written: usize,
}

#[derive(Debug, Deserialize)]
struct EditRequest {
    path: PathBuf,
    old_text: String,
    new_text: String,
}

#[derive(Debug, Serialize)]
struct EditResponse {
    replacements: usize,
}

#[derive(Debug, Deserialize)]
struct ExistsRequest {
    path: PathBuf,
}

#[derive(Debug, Serialize)]
struct ExistsResponse {
    exists: bool,
}

fn invoke_filesystem_service(request: &ServiceRequest) -> ServiceResponse {
    match request.operation.as_str() {
        "read" => read_file(request),
        "write" => write_file(request),
        "edit" => edit_file(request),
        "exists" => path_exists(request),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported filesystem service operation",
        ),
    }
}

fn invoke_tool_service(request: &ServiceRequest) -> ServiceResponse {
    match request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(request),
        OP_INVOKE_TOOL => invoke_tool(request),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported tool service operation",
        ),
    }
}

fn list_tools(request: &ServiceRequest) -> ServiceResponse {
    if let Err(error) = request.payload_json::<ListToolsRequest>() {
        return invalid_request(&error);
    }
    json_response(&ToolList {
        tools: vec![
            ToolDefinition {
                name: "filesystem.read".to_string(),
                description: "Read a UTF-8 text file".to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": { "path": { "type": "string" } }
                }),
                side_effect: ToolSideEffect::ReadOnly,
                requires_permission: false,
            },
            ToolDefinition {
                name: "filesystem.write".to_string(),
                description: "Write a UTF-8 text file, creating parent directories".to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["path", "contents"],
                    "properties": {
                        "path": { "type": "string" },
                        "contents": { "type": "string" }
                    }
                }),
                side_effect: ToolSideEffect::WriteFiles,
                requires_permission: true,
            },
            ToolDefinition {
                name: "filesystem.edit".to_string(),
                description: "Replace one unique text occurrence in a UTF-8 text file".to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["path", "old_text", "new_text"],
                    "properties": {
                        "path": { "type": "string" },
                        "old_text": { "type": "string" },
                        "new_text": { "type": "string" }
                    }
                }),
                side_effect: ToolSideEffect::WriteFiles,
                requires_permission: true,
            },
            ToolDefinition {
                name: "filesystem.exists".to_string(),
                description: "Check whether a path exists".to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": { "path": { "type": "string" } }
                }),
                side_effect: ToolSideEffect::ReadOnly,
                requires_permission: false,
            },
        ],
    })
}

fn invoke_tool(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ToolInvocationRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let response = match request.name.as_str() {
        "filesystem.read" => tool_read(request.arguments),
        "filesystem.write" => tool_write(request.arguments),
        "filesystem.edit" => tool_edit(request.arguments),
        "filesystem.exists" => tool_exists(request.arguments),
        _ => ToolInvocationResponse {
            output: format!("unknown filesystem tool: {}", request.name),
            is_error: true,
        },
    };
    json_response(&response)
}

fn tool_read(arguments: serde_json::Value) -> ToolInvocationResponse {
    match serde_json::from_value::<ReadRequest>(arguments) {
        Ok(request) => match std::fs::read_to_string(&request.path) {
            Ok(contents) => ToolInvocationResponse {
                output: contents,
                is_error: false,
            },
            Err(error) => tool_io_error(&error),
        },
        Err(error) => tool_json_error(&error),
    }
}

fn tool_write(arguments: serde_json::Value) -> ToolInvocationResponse {
    match serde_json::from_value::<WriteRequest>(arguments) {
        Ok(request) => write_file_inner(&request.path, &request.contents).map_or_else(
            |error| tool_io_error(&error),
            |bytes_written| ToolInvocationResponse {
                output: format!("wrote {bytes_written} bytes"),
                is_error: false,
            },
        ),
        Err(error) => tool_json_error(&error),
    }
}

fn tool_edit(arguments: serde_json::Value) -> ToolInvocationResponse {
    match serde_json::from_value::<EditRequest>(arguments) {
        Ok(request) => edit_file_inner(&request).map_or_else(
            |error| ToolInvocationResponse {
                output: error,
                is_error: true,
            },
            |replacements| ToolInvocationResponse {
                output: format!("applied {replacements} replacement"),
                is_error: false,
            },
        ),
        Err(error) => tool_json_error(&error),
    }
}

fn tool_exists(arguments: serde_json::Value) -> ToolInvocationResponse {
    match serde_json::from_value::<ExistsRequest>(arguments) {
        Ok(request) => ToolInvocationResponse {
            output: request.path.exists().to_string(),
            is_error: false,
        },
        Err(error) => tool_json_error(&error),
    }
}

fn read_file(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ReadRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match std::fs::read_to_string(&request.path) {
        Ok(contents) => json_response(&ReadResponse { contents }),
        Err(error) => io_error(&error),
    }
}

fn write_file(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WriteRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match write_file_inner(&request.path, &request.contents) {
        Ok(bytes_written) => json_response(&WriteResponse { bytes_written }),
        Err(error) => io_error(&error),
    }
}

fn edit_file(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<EditRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match edit_file_inner(&request) {
        Ok(replacements) => json_response(&EditResponse { replacements }),
        Err(error) => ServiceResponse::error("edit_error", error),
    }
}

fn path_exists(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ExistsRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    json_response(&ExistsResponse {
        exists: request.path.exists(),
    })
}

fn write_file_inner(path: &std::path::Path, contents: &str) -> Result<usize, std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents.as_bytes())?;
    Ok(contents.len())
}

fn edit_file_inner(request: &EditRequest) -> Result<usize, String> {
    let contents = std::fs::read_to_string(&request.path).map_err(|error| error.to_string())?;
    let matches = contents.matches(&request.old_text).count();
    if matches != 1 {
        return Err(format!(
            "old_text must match exactly once, found {matches} matches"
        ));
    }
    let updated = contents.replacen(&request.old_text, &request.new_text, 1);
    std::fs::write(&request.path, updated.as_bytes()).map_err(|error| error.to_string())?;
    Ok(1)
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

fn io_error(error: &std::io::Error) -> ServiceResponse {
    ServiceResponse::error("io_error", error.to_string())
}

fn tool_io_error(error: &std::io::Error) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output: error.to_string(),
        is_error: true,
    }
}

fn tool_json_error(error: &serde_json::Error) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output: error.to_string(),
        is_error: true,
    }
}

bcode_plugin_sdk::export_plugin!(FilesystemPlugin, include_str!("../bcode-plugin.toml"));
