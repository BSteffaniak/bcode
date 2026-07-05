#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Vim edit tool plugin for Bcode.
//!
//! This plugin exposes model-callable tools that drive the reusable
//! `bcode_vim_edit` Neovim RPC editing engine.

use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID,
    ToolArgumentExtractor, ToolArgumentKind, ToolArtifact, ToolDefinition, ToolInvocationRequest,
    ToolInvocationResponse, ToolInvocationResult, ToolList, ToolPolicyMetadata, ToolSideEffect,
    ToolUiMetadata,
};
use bcode_vim_edit::{
    VimEditMode, VimEditRequest, VimEditResult, VimEditSandbox, VimEditStep, run_vim_edit,
};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_TIMEOUT_MS: u64 = 5_000;

/// Vim edit plugin.
#[derive(Default)]
pub struct VimEditPlugin;

impl RustPlugin for VimEditPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.interface_id.as_str() {
            TOOL_SERVICE_INTERFACE_ID => invoke_tool_service(&context),
            _ => ServiceResponse::error(
                "unsupported_interface",
                "unsupported vim edit plugin service interface",
            ),
        }
    }
}

#[derive(Debug, Deserialize)]
struct VimEditToolRequest {
    path: PathBuf,
    #[serde(default)]
    steps: Vec<VimEditToolStep>,
    #[serde(default)]
    sandbox: VimEditToolSandbox,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum VimEditToolStep {
    Keys { keys: String },
    Insert { insert: String },
    Ex { ex: String },
}

impl From<VimEditToolStep> for VimEditStep {
    fn from(step: VimEditToolStep) -> Self {
        match step {
            VimEditToolStep::Keys { keys } => Self::Keys { input: keys },
            VimEditToolStep::Insert { insert } => Self::Insert { text: insert },
            VimEditToolStep::Ex { ex } => Self::Ex { command: ex },
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VimEditToolSandbox {
    #[default]
    Default,
    DangerouslyDisabled,
}

impl From<VimEditToolSandbox> for VimEditSandbox {
    fn from(sandbox: VimEditToolSandbox) -> Self {
        match sandbox {
            VimEditToolSandbox::Default => Self::Default,
            VimEditToolSandbox::DangerouslyDisabled => Self::DangerouslyDisabled,
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct VimEditToolOutput<'a> {
    success: bool,
    path: &'a str,
    changed: bool,
    diff: &'a str,
    cursor: bcode_vim_edit::CursorPosition,
    nvim_mode: &'a str,
    final_context: &'a bcode_vim_edit::TextContext,
    events: &'a [bcode_vim_edit::VimEditEvent],
}

#[derive(Debug, serde::Serialize)]
struct VimEditToolError<'a> {
    success: bool,
    path: Option<&'a str>,
    error: String,
}

fn invoke_tool_service(context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    match request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(request),
        OP_INVOKE_TOOL => invoke_tool(context),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported vim edit tool service operation",
        ),
    }
}

fn list_tools(request: &ServiceRequest) -> ServiceResponse {
    if let Err(error) = request.payload_json::<ListToolsRequest>() {
        return invalid_request(&error);
    }
    json_response(&ToolList {
        tools: vec![preview_tool_definition(), apply_tool_definition()],
    })
}

fn invoke_tool(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<ToolInvocationRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let response = invoke_tool_request(request);
    json_response(&response)
}

fn invoke_tool_request(request: ToolInvocationRequest) -> ToolInvocationResponse {
    match request.name.as_str() {
        "vim_edit.preview" => tool_vim_edit_with_nvim_executable(
            request.arguments,
            request.cwd.as_deref(),
            VimEditMode::Preview,
            &request.tool_call_id,
            "vim_edit.preview",
            None,
        ),
        "vim_edit.apply" => tool_vim_edit_with_nvim_executable(
            request.arguments,
            request.cwd.as_deref(),
            VimEditMode::Apply,
            &request.tool_call_id,
            "vim_edit.apply",
            None,
        ),
        _ => ToolInvocationResponse {
            output: "unknown vim edit tool".to_string(),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        },
    }
}

fn tool_vim_edit_with_nvim_executable(
    arguments: serde_json::Value,
    cwd: Option<&Path>,
    mode: VimEditMode,
    tool_call_id: &str,
    tool_name: &str,
    nvim_executable: Option<PathBuf>,
) -> ToolInvocationResponse {
    let request = match serde_json::from_value::<VimEditToolRequest>(arguments) {
        Ok(request) => request,
        Err(error) => return tool_json_error(&error),
    };
    let path = resolve_session_path(cwd, &request.path);
    let display_path = path.display().to_string();
    let edit_request = VimEditRequest {
        path,
        nvim_executable,
        steps: request.steps.into_iter().map(Into::into).collect(),
        mode,
        sandbox: request.sandbox.into(),
        timeout: Duration::from_millis(request.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
    };

    match run_vim_edit(edit_request) {
        Ok(result) => vim_edit_success_response(&display_path, &result, tool_call_id, tool_name),
        Err(error) => vim_edit_error_response(Some(&display_path), error.to_string()),
    }
}

fn vim_edit_success_response(
    path: &str,
    result: &VimEditResult,
    tool_call_id: &str,
    tool_name: &str,
) -> ToolInvocationResponse {
    let output = VimEditToolOutput {
        success: true,
        path,
        changed: result.changed,
        diff: &result.diff,
        cursor: result.cursor,
        nvim_mode: &result.nvim_mode,
        final_context: &result.final_context,
        events: &result.events,
    };
    let mut response = json_tool_response(&output, false);
    response.result = Some(vim_edit_change_artifact(
        tool_call_id,
        tool_name,
        path,
        result,
    ));
    response
}

fn vim_edit_change_artifact(
    tool_call_id: &str,
    tool_name: &str,
    path: &str,
    result: &VimEditResult,
) -> ToolInvocationResult {
    let summary = if result.changed {
        "vim edit changed file"
    } else {
        "vim edit produced no changes"
    };
    ToolInvocationResult::Artifact {
        artifact: Box::new(ToolArtifact {
            artifact_id: format!("{tool_call_id}-vim-edit-change"),
            producer_plugin_id: "bcode.vim-edit".to_string(),
            schema: "bcode.vim-edit.change".to_string(),
            schema_version: 1,
            tool_call_id: Some(tool_call_id.to_string()),
            title: Some("Vim edit change".to_string()),
            metadata: json!({
                "tool_name": tool_name,
                "summary": summary,
                "path": path,
                "changed": result.changed,
                "diff": result.diff,
            }),
            refs: Vec::new(),
        }),
    }
}

fn vim_edit_error_response(path: Option<&str>, error: String) -> ToolInvocationResponse {
    let output = VimEditToolError {
        success: false,
        path,
        error,
    };
    json_tool_response(&output, true)
}

fn preview_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "vim_edit.preview".to_string(),
        description: "Preview Vim/Neovim edits to a single UTF-8 file using isolated headless Neovim over RPC. Does not modify the target file. Steps preserve real Vim command concepts via keys/insert/ex entries. Optional sandbox=\"dangerously_disabled\" is unsafe and explicitly bypasses default command filtering.".to_string(),
        input_schema: vim_edit_input_schema(),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: path_policy("read", ToolArgumentKind::ReadPath),
        ui: ToolUiMetadata::default(),
    }
}

fn apply_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "vim_edit.apply".to_string(),
        description: "Apply Vim/Neovim edits to a single UTF-8 file using isolated headless Neovim over RPC. Requires write permission and writes only the requested path. Steps preserve real Vim command concepts via keys/insert/ex entries. Optional sandbox=\"dangerously_disabled\" is unsafe and explicitly bypasses default command filtering.".to_string(),
        input_schema: vim_edit_input_schema(),
        side_effect: ToolSideEffect::WriteFiles,
        requires_permission: true,
        policy: path_policy("edit", ToolArgumentKind::WritePath),
        ui: ToolUiMetadata::default(),
    }
}

fn vim_edit_input_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "required": ["path", "steps"],
        "properties": {
            "path": { "type": "string" },
            "steps": {
                "type": "array",
                "items": {
                    "type": "object",
                    "oneOf": [
                        {
                            "required": ["keys"],
                            "properties": { "keys": { "type": "string" } }
                        },
                        {
                            "required": ["insert"],
                            "properties": { "insert": { "type": "string" } }
                        },
                        {
                            "required": ["ex"],
                            "properties": { "ex": { "type": "string" } }
                        }
                    ]
                }
            },
            "sandbox": {
                "type": "string",
                "enum": ["default", "dangerously_disabled"]
            },
            "timeout_ms": { "type": "integer", "minimum": 1 }
        }
    })
}

fn path_policy(category: &str, kind: ToolArgumentKind) -> ToolPolicyMetadata {
    ToolPolicyMetadata {
        aliases: vec![category.to_string()],
        compatibility_aliases: Vec::new(),
        capabilities: vec![format!("vim_edit.{category}")],
        permission_category: Some(category.to_string()),
        argument_extractors: vec![ToolArgumentExtractor {
            kind,
            argument: "path".to_string(),
        }],
    }
}

fn resolve_session_path(cwd: Option<&Path>, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.map_or_else(|| path.to_path_buf(), |cwd| cwd.join(path))
    }
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("serialization_failed", error.to_string()))
}

fn tool_json_error(error: &serde_json::Error) -> ToolInvocationResponse {
    vim_edit_error_response(None, error.to_string())
}

fn json_tool_response<T: serde::Serialize>(value: &T, is_error: bool) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value) {
        Ok(output) => ToolInvocationResponse {
            output,
            is_error,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        },
        Err(error) => ToolInvocationResponse {
            output: error.to_string(),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        },
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> StaticPluginVtable {
    static_plugin_vtable!(VimEditPlugin, include_str!("../bcode-plugin.toml"))
}

export_plugin!(VimEditPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_include_preview_and_apply() {
        let tools = ToolList {
            tools: vec![preview_tool_definition(), apply_tool_definition()],
        };
        let names = tools
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["vim_edit.preview", "vim_edit.apply"]);
    }

    #[test]
    fn preview_tool_is_read_only_without_permission() {
        let tool = preview_tool_definition();
        assert_eq!(tool.side_effect, ToolSideEffect::ReadOnly);
        assert!(!tool.requires_permission);
    }

    #[test]
    fn apply_tool_writes_and_requires_permission() {
        let tool = apply_tool_definition();
        assert_eq!(tool.side_effect, ToolSideEffect::WriteFiles);
        assert!(tool.requires_permission);
    }

    #[test]
    fn parses_dangerous_sandbox_explicitly() {
        let request = serde_json::from_value::<VimEditToolRequest>(json!({
            "path": "src/lib.rs",
            "steps": [{ "keys": "gg" }],
            "sandbox": "dangerously_disabled"
        }))
        .expect("request parses");
        assert!(matches!(
            request.sandbox,
            VimEditToolSandbox::DangerouslyDisabled
        ));
    }

    #[test]
    fn preview_tool_invocation_returns_success_and_does_not_modify_file_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim plugin test because `nvim` is not available");
            return;
        }
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), "foo bar baz").expect("write original");
        let response = invoke_tool_request(ToolInvocationRequest {
            tool_call_id: "test".to_string(),
            name: "vim_edit.preview".to_string(),
            arguments: json!({
                "path": file.path(),
                "steps": [
                    { "keys": "w" },
                    { "keys": "ciw" },
                    { "insert": "qux" },
                    { "keys": "<Esc>" }
                ]
            }),
            cwd: None,
            artifact_dir: None,
            cancellation_path: None,
        });
        assert!(!response.is_error, "{}", response.output);
        assert!(matches!(
            response.result,
            Some(ToolInvocationResult::Artifact { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(file.path()).expect("read original"),
            "foo bar baz"
        );
        assert!(response.output.contains("\"success\": true"));
        assert!(response.output.contains("foo qux baz"));
    }

    #[test]
    fn apply_tool_invocation_returns_success_and_modifies_file_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim plugin test because `nvim` is not available");
            return;
        }
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), "foo bar baz").expect("write original");
        let response = invoke_tool_request(ToolInvocationRequest {
            tool_call_id: "test".to_string(),
            name: "vim_edit.apply".to_string(),
            arguments: json!({
                "path": file.path(),
                "steps": [
                    { "keys": "w" },
                    { "keys": "ciw" },
                    { "insert": "qux" },
                    { "keys": "<Esc>" }
                ]
            }),
            cwd: None,
            artifact_dir: None,
            cancellation_path: None,
        });
        assert!(!response.is_error, "{}", response.output);
        assert!(matches!(
            response.result,
            Some(ToolInvocationResult::Artifact { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(file.path()).expect("read edited"),
            "foo qux baz"
        );
    }

    #[test]
    fn invalid_tool_request_returns_clear_error() {
        let response = invoke_tool_request(ToolInvocationRequest {
            tool_call_id: "test".to_string(),
            name: "vim_edit.preview".to_string(),
            arguments: json!({ "steps": [] }),
            cwd: None,
            artifact_dir: None,
            cancellation_path: None,
        });
        assert!(response.is_error);
        assert!(response.output.contains("success"));
        assert!(response.output.contains("error"));
    }

    #[test]
    fn missing_nvim_returns_clear_tool_error() {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), "foo").expect("write original");
        let response = tool_vim_edit_with_nvim_executable(
            json!({
                "path": file.path(),
                "steps": []
            }),
            None,
            VimEditMode::Preview,
            "test",
            "vim_edit.preview",
            Some(PathBuf::from("definitely-missing-bcode-plugin-nvim")),
        );
        assert!(response.is_error);
        assert!(response.output.contains("success"));
        assert!(response.output.contains("error"));
        assert!(
            response
                .output
                .contains("definitely-missing-bcode-plugin-nvim")
        );
    }

    #[test]
    fn success_response_contains_vim_edit_change_artifact() {
        let result = VimEditResult {
            changed: true,
            diff: "+new".to_string(),
            cursor: bcode_vim_edit::CursorPosition { line: 1, column: 1 },
            nvim_mode: "normal".to_string(),
            final_context: bcode_vim_edit::TextContext {
                start_line: 1,
                lines: vec!["new".to_string()],
            },
            events: Vec::new(),
        };
        let response =
            vim_edit_success_response("src/lib.rs", &result, "call-1", "vim_edit.preview");
        let Some(ToolInvocationResult::Artifact { artifact }) = response.result else {
            panic!("expected artifact result");
        };
        assert_eq!(artifact.schema, "bcode.vim-edit.change");
        assert_eq!(artifact.producer_plugin_id, "bcode.vim-edit");
        assert_eq!(artifact.metadata["tool_name"], "vim_edit.preview");
        assert_eq!(artifact.metadata["path"], "src/lib.rs");
        assert_eq!(artifact.metadata["summary"], "vim edit changed file");
    }

    fn nvim_available() -> bool {
        std::process::Command::new("nvim")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}
