#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Vim edit tool plugin for Bcode.
//!
//! This plugin exposes model-callable tools that drive the reusable
//! `bcode_vim_edit` Neovim RPC editing engine.

#[cfg(feature = "static-bundled")]
mod vim_edit_interaction;
#[cfg(feature = "static-bundled")]
mod vim_edit_playback_tui;

use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    InteractiveToolRequest, InteractiveToolResumeRequest, ListToolsRequest, OP_INVOKE_TOOL,
    OP_LIST_TOOLS, OP_RESUME_INTERACTIVE_TOOL, TOOL_SERVICE_INTERFACE_ID, ToolArgumentExtractor,
    ToolArgumentKind, ToolArtifact, ToolDefinition, ToolInvocationRequest, ToolInvocationResponse,
    ToolInvocationResult, ToolInvocationStreamEvent, ToolList, ToolPluginVisualMetadata,
    ToolPolicyMetadata, ToolSideEffect, ToolStreamVisualUpdate, ToolUiMetadata,
    ToolVisualPayloadSelector,
};
use bcode_vim_edit::{
    VimEditFrame, VimEditMode, VimEditMultiFileEntry, VimEditMultiFileRequest, VimEditRequest,
    VimEditResult, VimEditSandbox, VimEditStep, run_vim_edit_observed,
    run_vim_multi_file_edit_observed,
};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const VIM_EDIT_PLUGIN_ID: &str = "bcode.vim-edit";
const VIM_EDIT_REQUEST_PREVIEW_SCHEMA: &str = "bcode.vim-edit.request.preview";
const VIM_EDIT_REQUEST_APPLY_SCHEMA: &str = "bcode.vim-edit.request.apply";
const VIM_EDIT_LIVE_SCHEMA: &str = "bcode.vim-edit.live";
const VIM_EDIT_PLAYBACK_SCHEMA: &str = "bcode.vim-edit.playback";
const VIM_EDIT_PLAYBACK_INTERACTION_KIND: &str = "bcode.vim-edit.playback";
const VIM_EDIT_PLAYBACK_SURFACE: &str = "tool.vim-edit.playback";

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
#[serde(untagged)]
enum VimEditToolRequest {
    Single {
        path: PathBuf,
        #[serde(default)]
        steps: Vec<VimEditToolStep>,
        #[serde(default)]
        sandbox: VimEditToolSandbox,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    Multi {
        files: Vec<VimEditMultiFileToolEntry>,
        #[serde(default)]
        sandbox: VimEditToolSandbox,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
}

#[derive(Debug, Deserialize)]
struct VimEditMultiFileToolEntry {
    path: PathBuf,
    #[serde(default)]
    steps: Vec<VimEditToolStep>,
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
        OP_RESUME_INTERACTIVE_TOOL => resume_interactive_tool(request),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported vim edit tool service operation",
        ),
    }
}

fn resume_interactive_tool(request: &ServiceRequest) -> ServiceResponse {
    let resume = match request.payload_json::<InteractiveToolResumeRequest>() {
        Ok(resume) => resume,
        Err(error) => return invalid_request(&error),
    };
    json_response(&ToolInvocationResponse {
        output: format!("Vim edit playback closed: {}", resume.interaction_id),
        is_error: false,
        content: Vec::new(),
        full_output: None,
        host_action: None,
        result: None,
    })
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
    let response = invoke_tool_request_with_events(request, context.events);
    json_response(&response)
}

#[cfg(test)]
fn invoke_tool_request(request: ToolInvocationRequest) -> ToolInvocationResponse {
    invoke_tool_request_with_events(request, ServiceEventEmitter::default())
}

fn invoke_tool_request_with_events(
    request: ToolInvocationRequest,
    events: ServiceEventEmitter,
) -> ToolInvocationResponse {
    match request.name.as_str() {
        "vim_edit.preview" => tool_vim_edit_with_nvim_executable(
            request.arguments,
            request.cwd.as_deref(),
            VimEditMode::Preview,
            &request.tool_call_id,
            "vim_edit.preview",
            None,
            events,
        ),
        "vim_edit.apply" => tool_vim_edit_with_nvim_executable(
            request.arguments,
            request.cwd.as_deref(),
            VimEditMode::Apply,
            &request.tool_call_id,
            "vim_edit.apply",
            None,
            events,
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

#[cfg(test)]
fn tool_vim_edit_with_nvim_executable_for_test(
    arguments: serde_json::Value,
    cwd: Option<&Path>,
    mode: VimEditMode,
    tool_call_id: &str,
    tool_name: &str,
    nvim_executable: Option<PathBuf>,
) -> ToolInvocationResponse {
    tool_vim_edit_with_nvim_executable(
        arguments,
        cwd,
        mode,
        tool_call_id,
        tool_name,
        nvim_executable,
        ServiceEventEmitter::default(),
    )
}

fn tool_vim_edit_with_nvim_executable(
    arguments: serde_json::Value,
    cwd: Option<&Path>,
    mode: VimEditMode,
    tool_call_id: &str,
    tool_name: &str,
    nvim_executable: Option<PathBuf>,
    events: ServiceEventEmitter,
) -> ToolInvocationResponse {
    let request = match serde_json::from_value::<VimEditToolRequest>(arguments) {
        Ok(request) => request,
        Err(error) => return tool_json_error(&error),
    };

    match request {
        VimEditToolRequest::Single {
            path,
            steps,
            sandbox,
            timeout_ms,
        } => {
            let path = resolve_path(cwd, &path);
            let display_path = path.display().to_string();
            let edit_request = VimEditRequest {
                path,
                nvim_executable,
                steps: steps.into_iter().map(Into::into).collect(),
                mode,
                sandbox: sandbox.into(),
                timeout: Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
            };
            let mut sequence = 0u64;
            let mut observer = |frame: VimEditFrame| {
                sequence = sequence.saturating_add(1);
                emit_vim_live_frame(&events, tool_call_id, sequence, tool_name, &frame);
            };
            match run_vim_edit_observed(edit_request, Some(&mut observer)) {
                Ok(result) => {
                    vim_edit_success_response(&display_path, &result, tool_call_id, tool_name)
                }
                Err(error) => vim_edit_error_response(Some(&display_path), error.to_string()),
            }
        }
        VimEditToolRequest::Multi {
            files,
            sandbox,
            timeout_ms,
        } => {
            let entries = files
                .into_iter()
                .map(|file| VimEditMultiFileEntry {
                    path: resolve_path(cwd, &file.path),
                    steps: file.steps.into_iter().map(Into::into).collect(),
                })
                .collect::<Vec<_>>();
            let mut sequence = 0u64;
            let mut observer = |frame: VimEditFrame| {
                sequence = sequence.saturating_add(1);
                emit_vim_live_frame(&events, tool_call_id, sequence, tool_name, &frame);
            };
            match run_vim_multi_file_edit_observed(
                &VimEditMultiFileRequest {
                    files: entries,
                    nvim_executable,
                    mode,
                    sandbox: sandbox.into(),
                    timeout: Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
                },
                Some(&mut observer),
            ) {
                Ok(result) => {
                    vim_edit_multi_file_success_response(&result, tool_call_id, tool_name)
                }
                Err(error) => vim_edit_error_response(None, error.to_string()),
            }
        }
    }
}

fn emit_vim_live_frame(
    events: &ServiceEventEmitter,
    tool_call_id: &str,
    sequence: u64,
    tool_name: &str,
    frame: &VimEditFrame,
) {
    let event = ToolInvocationStreamEvent::VisualUpdate {
        tool_call_id: tool_call_id.to_string(),
        sequence,
        streaming: true,
        visual: ToolStreamVisualUpdate {
            producer_plugin_id: Some(VIM_EDIT_PLUGIN_ID.to_string()),
            schema: VIM_EDIT_LIVE_SCHEMA.to_string(),
            schema_version: 1,
            title: Some("Vim edit live".to_string()),
            subtitle: Some(format!(
                "{} · step {}/{}",
                frame.path.display(),
                frame.step_index.saturating_add(1),
                frame.step_total
            )),
            payload: json!({
                "tool_name": tool_name,
                "phase": "running",
                "path": frame.path.display().to_string(),
                "file_index": frame.file_index,
                "file_total": frame.file_total,
                "step_index": frame.step_index,
                "step_total": frame.step_total,
                "step": frame.step.clone(),
                "before_cursor": frame.before_cursor,
                "after_cursor": frame.after_cursor,
                "cursor": frame.after_cursor,
                "nvim_mode": frame.nvim_mode.clone(),
                "context": frame.context.clone(),
                "changed": frame.changed,
                "message": frame.message.clone(),
            }),
        },
    };
    if let Ok(payload) = serde_json::to_vec(&event) {
        events.emit(&payload);
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
    let playback = vim_edit_change_artifact(tool_call_id, tool_name, path, result);
    let ToolInvocationResult::Artifact { artifact } = &playback else {
        unreachable!("vim edit artifact is always an artifact")
    };
    response.host_action = Some(vim_edit_playback_host_action(
        tool_call_id,
        &artifact.metadata,
    ));
    response.result = Some(playback);
    response
}

fn vim_edit_playback_host_action(
    tool_call_id: &str,
    playback: &serde_json::Value,
) -> bcode_tool::ToolInvocationHostAction {
    bcode_tool::ToolInvocationHostAction::InteractiveToolRequest(InteractiveToolRequest {
        interaction_id: format!("{tool_call_id}-vim-edit-playback"),
        interaction_kind: Some(VIM_EDIT_PLAYBACK_INTERACTION_KIND.to_string()),
        surface_kind: VIM_EDIT_PLAYBACK_SURFACE.to_string(),
        request: json!({ "playback": playback }),
        required: false,
        turn_behavior: bcode_tool::InteractiveToolTurnBehavior::CompleteTurnWithPendingInteraction,
        render_target: bcode_tool::InteractiveToolRenderTarget::TranscriptToolCall,
    })
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
            artifact_id: format!("{tool_call_id}-vim-edit-playback"),
            producer_plugin_id: VIM_EDIT_PLUGIN_ID.to_string(),
            schema: VIM_EDIT_PLAYBACK_SCHEMA.to_string(),
            schema_version: 1,
            tool_call_id: Some(tool_call_id.to_string()),
            title: Some("Vim edit playback".to_string()),
            metadata: json!({
                "success": true,
                "error": null,
                "tool_name": tool_name,
                "summary": summary,
                "path": path,
                "changed": result.changed,
                "diff": result.diff,
                "cursor": result.cursor,
                "nvim_mode": result.nvim_mode,
                "final_context": result.final_context,
                "events": result.events,
                "changed_ranges": [],
                "selected_ranges": [],
                "playback_controls": {
                    "available": ["first", "previous", "next", "last"],
                    "default_index": result.events.len()
                },
            }),
            refs: Vec::new(),
        }),
    }
}

fn vim_edit_multi_file_success_response(
    result: &bcode_vim_edit::VimEditMultiFileEditResult,
    tool_call_id: &str,
    tool_name: &str,
) -> ToolInvocationResponse {
    let output = json!({
        "success": true,
        "error": null,
        "tool_name": tool_name,
        "changed": result.changed,
        "diff": result.diff,
        "files": result.files,
    });
    let mut response = json_tool_response(&output, false);
    response.host_action = Some(vim_edit_playback_host_action(tool_call_id, &output));
    response.result = Some(ToolInvocationResult::Artifact {
        artifact: Box::new(ToolArtifact {
            artifact_id: format!("{tool_call_id}-vim-edit-playback"),
            producer_plugin_id: VIM_EDIT_PLUGIN_ID.to_string(),
            schema: VIM_EDIT_PLAYBACK_SCHEMA.to_string(),
            schema_version: 1,
            tool_call_id: Some(tool_call_id.to_string()),
            title: Some("Vim edit playback".to_string()),
            metadata: output,
            refs: Vec::new(),
        }),
    });
    response
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
        description: "Preview ordered Vim/Neovim edits using isolated headless Neovim over RPC. Accepts either single-file path+steps or an ordered files array where each entry switches to that file and runs its steps. Does not modify requested files. Optional sandbox=\"dangerously_disabled\" is unsafe and explicitly bypasses default command filtering.".to_string(),
        input_schema: vim_edit_input_schema(),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: path_policy("read", ToolArgumentKind::ReadPath),
        ui: ToolUiMetadata {
            activity_label: Some("previewing Vim edit".to_string()),
            request_visual: Some(vim_edit_request_visual(
                VIM_EDIT_REQUEST_PREVIEW_SCHEMA,
                "Vim edit preview",
            )),
        },
    }
}

fn apply_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "vim_edit.apply".to_string(),
        description: "Apply ordered Vim/Neovim edits using isolated headless Neovim over RPC. Accepts either single-file path+steps or an ordered files array where each entry switches to that file and runs its steps. Requires write permission and writes only after the full workflow succeeds. Optional sandbox=\"dangerously_disabled\" is unsafe and explicitly bypasses default command filtering.".to_string(),
        input_schema: vim_edit_input_schema(),
        side_effect: ToolSideEffect::WriteFiles,
        requires_permission: true,
        policy: path_policy("edit", ToolArgumentKind::WritePath),
        ui: ToolUiMetadata {
            activity_label: Some("applying Vim edit".to_string()),
            request_visual: Some(vim_edit_request_visual(
                VIM_EDIT_REQUEST_APPLY_SCHEMA,
                "Vim edit apply",
            )),
        },
    }
}

fn vim_edit_request_visual(schema: &str, title: &str) -> ToolPluginVisualMetadata {
    let mut payload = std::collections::BTreeMap::new();
    for field in ["path", "files", "steps", "sandbox", "timeout_ms"] {
        payload.insert(
            field.to_string(),
            ToolVisualPayloadSelector {
                fields: vec![field.to_string()],
                literal: None,
                required: matches!(field, "path" | "files"),
            },
        );
    }
    ToolPluginVisualMetadata {
        producer_plugin_id: Some(VIM_EDIT_PLUGIN_ID.to_string()),
        schema: schema.to_string(),
        schema_version: 1,
        title: Some(title.to_string()),
        subtitle: Some("vim edit · {bytes}".to_string()),
        payload,
    }
}

fn vim_edit_step_schema() -> serde_json::Value {
    json!({
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
    })
}

fn vim_edit_input_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "oneOf": [
            {
                "required": ["path", "steps"],
                "properties": {
                    "path": { "type": "string" },
                    "steps": {
                        "type": "array",
                        "items": vim_edit_step_schema()
                    }
                }
            },
            {
                "required": ["files"],
                "properties": {
                    "files": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "required": ["path", "steps"],
                            "properties": {
                                "path": { "type": "string" },
                                "steps": {
                                    "type": "array",
                                    "items": vim_edit_step_schema()
                                }
                            }
                        }
                    }
                }
            }
        ],
        "properties": {
            "path": { "type": "string" },
            "steps": {
                "type": "array",
                "items": vim_edit_step_schema()
            },
            "files": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["path", "steps"],
                    "properties": {
                        "path": { "type": "string" },
                        "steps": {
                            "type": "array",
                            "items": vim_edit_step_schema()
                        }
                    }
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
        argument_extractors: vec![
            ToolArgumentExtractor {
                kind,
                argument: "path".to_string(),
            },
            ToolArgumentExtractor {
                kind,
                argument: "files".to_string(),
            },
        ],
    }
}

fn resolve_path(cwd: Option<&Path>, path: &Path) -> PathBuf {
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
    vim_edit_error_response(None, format!("invalid vim edit request: {error}"))
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
    let mut vtable = static_plugin_vtable!(VimEditPlugin, include_str!("../bcode-plugin.toml"));
    vtable.tui_registry = Some(vim_edit_tui_registry);
    vtable.interaction_registry = Some(vim_edit_interaction_registry);
    vtable
}

#[cfg(feature = "static-bundled")]
fn vim_edit_interaction_registry() -> bcode_plugin_sdk::interaction::PluginInteractionRegistry {
    let mut registry = bcode_plugin_sdk::interaction::PluginInteractionRegistry::default();
    registry.register_interaction::<vim_edit_interaction::VimEditPlaybackInteractionController>();
    registry
}

#[cfg(feature = "static-bundled")]
fn vim_edit_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    registry.register_visual_adapter(Box::new(
        vim_edit_playback_tui::VimEditPlaybackTuiVisualAdapter,
    ));
    registry.register_interactive_surface::<
        vim_edit_interaction::VimEditPlaybackInteractionController,
        vim_edit_playback_tui::VimEditPlaybackTerminalRenderer,
    >();
    registry
}

export_plugin!(VimEditPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_include_only_preview_and_apply() {
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
        assert_eq!(tool.policy.argument_extractors.len(), 2);
    }

    #[test]
    fn apply_tool_writes_and_requires_permission() {
        let tool = apply_tool_definition();
        assert_eq!(tool.side_effect, ToolSideEffect::WriteFiles);
        assert!(tool.requires_permission);
        assert_eq!(tool.policy.argument_extractors[0].argument, "path");
        assert_eq!(tool.policy.argument_extractors[1].argument, "files");
        assert_eq!(
            tool.policy.argument_extractors[0].kind,
            ToolArgumentKind::WritePath
        );
        assert_eq!(
            tool.policy.argument_extractors[1].kind,
            ToolArgumentKind::WritePath
        );
    }

    #[test]
    fn parses_dangerous_sandbox_explicitly() {
        let request = serde_json::from_value::<VimEditToolRequest>(json!({
            "path": "src/lib.rs",
            "steps": [{ "keys": "gg" }],
            "sandbox": "dangerously_disabled"
        }))
        .expect("request parses");
        let VimEditToolRequest::Single { sandbox, .. } = request else {
            panic!("expected single request");
        };
        assert!(matches!(sandbox, VimEditToolSandbox::DangerouslyDisabled));
    }

    #[test]
    fn parses_ordered_multi_file_shape() {
        let request = serde_json::from_value::<VimEditToolRequest>(json!({
            "files": [
                { "path": "a.txt", "steps": [{ "keys": "gg" }] },
                { "path": "b.txt", "steps": [{ "ex": "%s/a/b/" }] },
                { "path": "a.txt", "steps": [{ "insert": "again" }] }
            ]
        }))
        .expect("request parses");
        let VimEditToolRequest::Multi { files, .. } = request else {
            panic!("expected multi request");
        };
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, PathBuf::from("a.txt"));
        assert_eq!(files[2].path, PathBuf::from("a.txt"));
    }

    #[test]
    fn invalid_tool_request_returns_clear_error() {
        let response = tool_vim_edit_with_nvim_executable_for_test(
            json!({ "path": 123 }),
            None,
            VimEditMode::Preview,
            "call-1",
            "vim_edit.preview",
            None,
        );
        assert!(response.is_error);
        assert!(response.output.contains("invalid vim edit request"));
    }

    #[test]
    fn missing_nvim_returns_clear_tool_error() {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), "foo").expect("write temp file");
        let response = tool_vim_edit_with_nvim_executable_for_test(
            json!({ "path": file.path(), "steps": [] }),
            None,
            VimEditMode::Preview,
            "call-1",
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
    fn success_response_contains_vim_edit_playback_artifact() {
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
        assert_eq!(artifact.schema, "bcode.vim-edit.playback");
        assert_eq!(artifact.producer_plugin_id, "bcode.vim-edit");
        assert_eq!(artifact.metadata["tool_name"], "vim_edit.preview");
        assert_eq!(artifact.metadata["path"], "src/lib.rs");
        assert_eq!(artifact.metadata["summary"], "vim edit changed file");
        assert_eq!(artifact.metadata["success"], true);
        assert!(artifact.metadata.get("events").is_some());
        assert!(artifact.metadata.get("final_context").is_some());
    }

    #[test]
    fn preview_tool_invocation_returns_success_and_does_not_modify_file_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim plugin test because `nvim` is not available");
            return;
        }
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), "foo").expect("write original");
        let response = invoke_tool_request(ToolInvocationRequest {
            tool_call_id: "test".to_string(),
            name: "vim_edit.preview".to_string(),
            arguments: json!({
                "path": file.path(),
                "steps": [{ "ex": "%s/foo/bar/" }]
            }),
            cwd: None,
            artifact_dir: None,
            cancellation_path: None,
        });
        assert!(!response.is_error, "{}", response.output);
        assert!(response.output.contains("bar"));
        assert_eq!(
            std::fs::read_to_string(file.path()).expect("read original"),
            "foo"
        );
    }

    #[test]
    fn apply_tool_invocation_returns_success_and_modifies_file_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim plugin test because `nvim` is not available");
            return;
        }
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), "foo").expect("write original");
        let response = invoke_tool_request(ToolInvocationRequest {
            tool_call_id: "test".to_string(),
            name: "vim_edit.apply".to_string(),
            arguments: json!({
                "path": file.path(),
                "steps": [{ "ex": "%s/foo/bar/" }]
            }),
            cwd: None,
            artifact_dir: None,
            cancellation_path: None,
        });
        assert!(!response.is_error, "{}", response.output);
        assert_eq!(
            std::fs::read_to_string(file.path()).expect("read edited"),
            "bar"
        );
    }

    #[test]
    fn multi_file_preview_uses_ordered_files_and_preserves_real_files_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim plugin test because `nvim` is not available");
            return;
        }
        let first = tempfile::NamedTempFile::new().expect("first temp file");
        let second = tempfile::NamedTempFile::new().expect("second temp file");
        std::fs::write(first.path(), "alpha beta").expect("write first");
        std::fs::write(second.path(), "target ").expect("write second");
        let response = invoke_tool_request(ToolInvocationRequest {
            tool_call_id: "test".to_string(),
            name: "vim_edit.preview".to_string(),
            arguments: json!({
                "files": [
                    { "path": first.path(), "steps": [{ "keys": "yiw" }] },
                    { "path": second.path(), "steps": [{ "keys": "A" }, { "keys": "<Esc>" }, { "keys": "p" }] },
                    { "path": first.path(), "steps": [{ "ex": "%s/beta/gamma/" }] }
                ]
            }),
            cwd: None,
            artifact_dir: None,
            cancellation_path: None,
        });
        assert!(!response.is_error, "{}", response.output);
        assert_eq!(
            std::fs::read_to_string(first.path()).expect("read first"),
            "alpha beta"
        );
        assert_eq!(
            std::fs::read_to_string(second.path()).expect("read second"),
            "target "
        );
        assert!(response.output.contains("gamma"), "{}", response.output);
        assert!(response.output.contains("alpha"), "{}", response.output);
    }

    #[test]
    fn multi_file_apply_runs_ordered_entries_and_writes_changed_files_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim plugin test because `nvim` is not available");
            return;
        }
        let first = tempfile::NamedTempFile::new().expect("first temp file");
        let second = tempfile::NamedTempFile::new().expect("second temp file");
        std::fs::write(first.path(), "alpha beta").expect("write first");
        std::fs::write(second.path(), "target ").expect("write second");
        let response = invoke_tool_request(ToolInvocationRequest {
            tool_call_id: "test".to_string(),
            name: "vim_edit.apply".to_string(),
            arguments: json!({
                "files": [
                    { "path": first.path(), "steps": [{ "keys": "yiw" }] },
                    { "path": second.path(), "steps": [{ "keys": "A" }, { "keys": "<Esc>" }, { "keys": "p" }] },
                    { "path": first.path(), "steps": [{ "ex": "%s/beta/gamma/" }] }
                ]
            }),
            cwd: None,
            artifact_dir: None,
            cancellation_path: None,
        });
        assert!(!response.is_error, "{}", response.output);
        assert_eq!(
            std::fs::read_to_string(first.path()).expect("read first"),
            "alpha gamma"
        );
        assert_eq!(
            std::fs::read_to_string(second.path()).expect("read second"),
            "target alpha"
        );
    }

    #[test]
    fn multi_file_apply_does_not_partially_write_when_later_entry_fails_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim plugin test because `nvim` is not available");
            return;
        }
        let first = tempfile::NamedTempFile::new().expect("first temp file");
        let second = tempfile::NamedTempFile::new().expect("second temp file");
        std::fs::write(first.path(), "foo").expect("write first");
        std::fs::write(second.path(), "bar").expect("write second");
        let response = invoke_tool_request(ToolInvocationRequest {
            tool_call_id: "test".to_string(),
            name: "vim_edit.apply".to_string(),
            arguments: json!({
                "files": [
                    { "path": first.path(), "steps": [{ "ex": "%s/foo/one/" }] },
                    { "path": second.path(), "steps": [{ "keys": "/missing<CR>" }] }
                ]
            }),
            cwd: None,
            artifact_dir: None,
            cancellation_path: None,
        });
        assert!(response.is_error);
        assert_eq!(
            std::fs::read_to_string(first.path()).expect("read first"),
            "foo"
        );
        assert_eq!(
            std::fs::read_to_string(second.path()).expect("read second"),
            "bar"
        );
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
