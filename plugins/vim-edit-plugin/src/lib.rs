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

use bcode_plugin_sdk::path::{display, display_from_current_dir};
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
    VimEditFrame, VimEditMode, VimEditMultiFileEntry, VimEditMultiFileRequest,
    VimEditObservationGranularity, VimEditRequest, VimEditResult, VimEditSandbox, VimEditStep,
    run_vim_edit_observed, run_vim_multi_file_edit_observed,
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
const MAX_PLAYBACK_FRAMES: usize = 500;
const MAX_CONTEXT_LINES: usize = 15;
const MAX_CONTEXT_LINE_CHARS: usize = 240;
const MAX_DIFF_BYTES: usize = 256 * 1024;

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
        OP_RESUME_INTERACTIVE_TOOL => resume_interactive_tool(context),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported vim edit tool service operation",
        ),
    }
}

fn resume_interactive_tool(context: &NativeServiceContext) -> ServiceResponse {
    let resume = match context
        .request
        .payload_json::<InteractiveToolResumeRequest>()
    {
        Ok(resume) => resume,
        Err(error) => return invalid_request(&error),
    };
    if let bcode_tool::InteractiveToolResolution::Submitted { payload } = &resume.resolution
        && payload.get("action").and_then(serde_json::Value::as_str) == Some("apply_requested")
    {
        let arguments = payload
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| resume.original_arguments.clone());
        let response = tool_vim_edit_with_nvim_executable(
            arguments,
            None,
            VimEditMode::Apply,
            &resume.tool_call_id,
            "vim_edit.apply",
            None,
            context.events,
        );
        return json_response(&response);
    }
    let output = format!("Vim edit playback closed: {}", resume.interaction_id);
    json_response(&ToolInvocationResponse {
        output,
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
    let request = match serde_json::from_value::<VimEditToolRequest>(arguments.clone()) {
        Ok(request) => request,
        Err(error) => return tool_json_error(&error),
    };

    match request {
        VimEditToolRequest::Single {
            path,
            steps,
            sandbox,
            timeout_ms,
        } => run_single_vim_edit_tool(
            SingleVimEditToolRun {
                path,
                steps,
                sandbox,
                timeout_ms,
                cwd,
                mode,
                tool_call_id,
                tool_name,
                nvim_executable,
                original_arguments: arguments,
            },
            events,
        ),
        VimEditToolRequest::Multi {
            files,
            sandbox,
            timeout_ms,
        } => run_multi_file_vim_edit_tool(
            MultiFileVimEditToolRun {
                files,
                sandbox,
                timeout_ms,
                cwd,
                mode,
                tool_call_id,
                tool_name,
                nvim_executable,
                original_arguments: arguments,
            },
            events,
        ),
    }
}

struct SingleVimEditToolRun<'a> {
    path: PathBuf,
    steps: Vec<VimEditToolStep>,
    sandbox: VimEditToolSandbox,
    timeout_ms: Option<u64>,
    cwd: Option<&'a Path>,
    mode: VimEditMode,
    tool_call_id: &'a str,
    tool_name: &'a str,
    nvim_executable: Option<PathBuf>,
    original_arguments: serde_json::Value,
}

fn run_single_vim_edit_tool(
    run: SingleVimEditToolRun<'_>,
    events: ServiceEventEmitter,
) -> ToolInvocationResponse {
    let path = resolve_path(run.cwd, &run.path);
    let display_path = display(&path, run.cwd.unwrap_or_else(|| Path::new("."))).to_string();
    let edit_request = VimEditRequest {
        path,
        nvim_executable: run.nvim_executable,
        steps: run.steps.into_iter().map(Into::into).collect(),
        mode: run.mode,
        sandbox: run.sandbox.into(),
        timeout: Duration::from_millis(run.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
        observation_granularity: VimEditObservationGranularity::Key,
    };
    let mut sequence = 0u64;
    emit_vim_live_phase(
        &events,
        run.tool_call_id,
        sequence,
        run.tool_name,
        "started",
        Some(&display_path),
        None,
    );
    let run_result = {
        let mut observer = |frame: VimEditFrame| {
            sequence = sequence.saturating_add(1);
            emit_vim_live_frame(
                &events,
                run.tool_call_id,
                sequence,
                run.tool_name,
                "running",
                &frame,
            );
        };
        run_vim_edit_observed(edit_request, Some(&mut observer))
    };
    match run_result {
        Ok(result) => {
            sequence = sequence.saturating_add(1);
            emit_vim_live_finished_result(
                &events,
                run.tool_call_id,
                sequence,
                run.tool_name,
                &display_path,
                &result,
            );
            vim_edit_success_response(
                &display_path,
                &result,
                run.tool_call_id,
                run.tool_name,
                run.mode,
                &run.original_arguments,
            )
        }
        Err(error) => {
            let error = error.to_string();
            sequence = sequence.saturating_add(1);
            emit_vim_live_phase(
                &events,
                run.tool_call_id,
                sequence,
                run.tool_name,
                "error",
                Some(&display_path),
                Some(&error),
            );
            vim_edit_error_response(Some(&display_path), error)
        }
    }
}

struct MultiFileVimEditToolRun<'a> {
    files: Vec<VimEditMultiFileToolEntry>,
    sandbox: VimEditToolSandbox,
    timeout_ms: Option<u64>,
    cwd: Option<&'a Path>,
    mode: VimEditMode,
    tool_call_id: &'a str,
    tool_name: &'a str,
    nvim_executable: Option<PathBuf>,
    original_arguments: serde_json::Value,
}

fn run_multi_file_vim_edit_tool(
    run: MultiFileVimEditToolRun<'_>,
    events: ServiceEventEmitter,
) -> ToolInvocationResponse {
    let entries = run
        .files
        .into_iter()
        .map(|file| VimEditMultiFileEntry {
            path: resolve_path(run.cwd, &file.path),
            steps: file.steps.into_iter().map(Into::into).collect(),
        })
        .collect::<Vec<_>>();
    let mut sequence = 0u64;
    emit_vim_live_phase(
        &events,
        run.tool_call_id,
        sequence,
        run.tool_name,
        "started",
        None,
        None,
    );
    let request = VimEditMultiFileRequest {
        files: entries,
        nvim_executable: run.nvim_executable,
        mode: run.mode,
        sandbox: run.sandbox.into(),
        timeout: Duration::from_millis(run.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
        observation_granularity: VimEditObservationGranularity::Key,
    };
    let run_result = {
        let mut observer = |frame: VimEditFrame| {
            sequence = sequence.saturating_add(1);
            emit_vim_live_frame(
                &events,
                run.tool_call_id,
                sequence,
                run.tool_name,
                "running",
                &frame,
            );
        };
        run_vim_multi_file_edit_observed(&request, Some(&mut observer))
    };
    match run_result {
        Ok(result) => {
            sequence = sequence.saturating_add(1);
            emit_vim_live_finished_multi_result(
                &events,
                run.tool_call_id,
                sequence,
                run.tool_name,
                &result,
            );
            vim_edit_multi_file_success_response(
                &result,
                run.tool_call_id,
                run.tool_name,
                run.mode,
                &run.original_arguments,
            )
        }
        Err(error) => {
            let error = error.to_string();
            sequence = sequence.saturating_add(1);
            emit_vim_live_phase(
                &events,
                run.tool_call_id,
                sequence,
                run.tool_name,
                "error",
                None,
                Some(&error),
            );
            vim_edit_error_response(None, error)
        }
    }
}

fn emit_vim_live_phase(
    events: &ServiceEventEmitter,
    tool_call_id: &str,
    sequence: u64,
    tool_name: &str,
    phase: &str,
    path: Option<&str>,
    error: Option<&str>,
) {
    let event = ToolInvocationStreamEvent::VisualUpdate {
        tool_call_id: tool_call_id.to_string(),
        sequence,
        streaming: !matches!(phase, "finished" | "error"),
        visual: ToolStreamVisualUpdate {
            visual_id: Some(format!("{tool_call_id}-vim-live")),
            producer_plugin_id: Some(VIM_EDIT_PLUGIN_ID.to_string()),
            schema: VIM_EDIT_LIVE_SCHEMA.to_string(),
            schema_version: 1,
            title: Some("Vim edit live".to_string()),
            subtitle: path.map(ToOwned::to_owned),
            payload: json!({
                "tool_name": tool_name,
                "phase": phase,
                "path": path,
                "error": error,
            }),
        },
    };
    if let Ok(payload) = serde_json::to_vec(&event) {
        events.emit(&payload);
    }
}

fn emit_vim_live_payload(
    events: &ServiceEventEmitter,
    tool_call_id: &str,
    sequence: u64,
    tool_name: &str,
    path: &str,
    payload: serde_json::Value,
    streaming: bool,
) {
    let event = ToolInvocationStreamEvent::VisualUpdate {
        tool_call_id: tool_call_id.to_string(),
        sequence,
        streaming,
        visual: ToolStreamVisualUpdate {
            visual_id: Some(format!("{tool_call_id}-vim-live")),
            producer_plugin_id: Some(VIM_EDIT_PLUGIN_ID.to_string()),
            schema: VIM_EDIT_LIVE_SCHEMA.to_string(),
            schema_version: 1,
            title: Some("Vim edit live".to_string()),
            subtitle: Some(format!("{path} · {tool_name}")),
            payload,
        },
    };
    if let Ok(payload) = serde_json::to_vec(&event) {
        events.emit(&payload);
    }
}

fn emit_vim_live_finished_result(
    events: &ServiceEventEmitter,
    tool_call_id: &str,
    sequence: u64,
    tool_name: &str,
    path: &str,
    result: &VimEditResult,
) {
    let last_event = result.events.last();
    let payload = json!({
        "tool_name": tool_name,
        "phase": "finished",
        "path": path,
        "file_index": 0,
        "file_total": 1,
        "step_index": last_event.map_or(0, |event| event.step_index),
        "step_total": result.events.len(),
        "step": last_event.map(|event| event.step.clone()),
        "substep_index": null,
        "substep_total": null,
        "input_token": null,
        "before_cursor": last_event.map_or(result.cursor, |event| event.before_cursor),
        "after_cursor": result.cursor,
        "cursor": result.cursor,
        "nvim_mode": result.nvim_mode,
        "context": result.final_context,
        "changed": result.changed,
        "message": "vim edit finished",
        "error": null,
    });
    emit_vim_live_payload(
        events,
        tool_call_id,
        sequence,
        tool_name,
        path,
        payload,
        false,
    );
}

fn emit_vim_live_finished_multi_result(
    events: &ServiceEventEmitter,
    tool_call_id: &str,
    sequence: u64,
    tool_name: &str,
    result: &bcode_vim_edit::VimEditMultiFileEditResult,
) {
    let Some((file_index, file)) = result
        .files
        .iter()
        .enumerate()
        .rev()
        .find(|(_, file)| !file.events.is_empty())
        .or_else(|| result.files.iter().enumerate().next_back())
    else {
        emit_vim_live_phase(
            events,
            tool_call_id,
            sequence,
            tool_name,
            "finished",
            None,
            None,
        );
        return;
    };
    let last_event = file.events.last();
    let path = file.path.display().to_string();
    let step_total = result
        .files
        .iter()
        .map(|file| file.events.len())
        .sum::<usize>();
    let payload = json!({
        "tool_name": tool_name,
        "phase": "finished",
        "path": path,
        "file_index": file_index,
        "file_total": result.files.len(),
        "step_index": last_event.map_or(0, |event| event.step_index),
        "step_total": step_total,
        "step": last_event.map(|event| event.step.clone()),
        "substep_index": null,
        "substep_total": null,
        "input_token": null,
        "before_cursor": last_event.map_or(file.cursor, |event| event.before_cursor),
        "after_cursor": file.cursor,
        "cursor": file.cursor,
        "nvim_mode": file.nvim_mode,
        "context": file.final_context,
        "changed": result.changed,
        "message": "vim edit finished",
        "error": null,
    });
    emit_vim_live_payload(
        events,
        tool_call_id,
        sequence,
        tool_name,
        &path,
        payload,
        false,
    );
}

fn emit_vim_live_frame(
    events: &ServiceEventEmitter,
    tool_call_id: &str,
    sequence: u64,
    tool_name: &str,
    phase: &str,
    frame: &VimEditFrame,
) {
    let event = ToolInvocationStreamEvent::VisualUpdate {
        tool_call_id: tool_call_id.to_string(),
        sequence,
        streaming: true,
        visual: ToolStreamVisualUpdate {
            visual_id: Some(format!("{tool_call_id}-vim-live")),
            producer_plugin_id: Some(VIM_EDIT_PLUGIN_ID.to_string()),
            schema: VIM_EDIT_LIVE_SCHEMA.to_string(),
            schema_version: 1,
            title: Some("Vim edit live".to_string()),
            subtitle: Some(format!(
                "{} · step {}/{}",
                display_from_current_dir(&frame.path),
                frame.step_index.saturating_add(1),
                frame.step_total
            )),
            payload: json!({
                "tool_name": tool_name,
                "phase": phase,
                "path": frame.path.display().to_string(),
                "file_index": frame.file_index,
                "file_total": frame.file_total,
                "step_index": frame.step_index,
                "step_total": frame.step_total,
                "step": frame.step.clone(),
                "substep_index": frame.substep_index,
                "substep_total": frame.substep_total,
                "input_token": frame.input_token.clone(),
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
    mode: VimEditMode,
    original_arguments: &serde_json::Value,
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
    let playback = vim_edit_change_artifact(
        tool_call_id,
        tool_name,
        path,
        result,
        mode,
        original_arguments,
    );
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
    mode: VimEditMode,
    original_arguments: &serde_json::Value,
) -> ToolInvocationResult {
    let summary = if result.changed {
        "vim edit changed file"
    } else {
        "vim edit produced no changes"
    };
    let diff = truncated_text(&result.diff, MAX_DIFF_BYTES);
    let frames = single_file_playback_frames(path, result);
    let frames_truncated = result.events.len() > frames.len();
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
                "tool_mode": mode,
                "original_arguments": original_arguments,
                "preview_actions": if mode == VimEditMode::Preview { json!(["apply_requested", "dismiss"]) } else { json!(["dismiss"]) },
                "summary": summary,
                "path": path,
                "changed": result.changed,
                "diff": diff.text,
                "diff_truncated": diff.truncated,
                "cursor": result.cursor,
                "nvim_mode": result.nvim_mode,
                "final_context": bounded_context(&result.final_context),
                "events": result.events,
                "frames": frames,
                "frame_count": result.events.len(),
                "frames_truncated": frames_truncated,
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
    mode: VimEditMode,
    original_arguments: &serde_json::Value,
) -> ToolInvocationResponse {
    let diff = truncated_text(&result.diff, MAX_DIFF_BYTES);
    let frames = multi_file_playback_frames(result);
    let frame_count = result
        .files
        .iter()
        .map(|file| file.events.len())
        .sum::<usize>();
    let frames_truncated = frame_count > frames.len();
    let output = json!({
        "success": true,
        "error": null,
        "tool_name": tool_name,
        "tool_mode": mode,
        "original_arguments": original_arguments,
        "preview_actions": if mode == VimEditMode::Preview { json!(["apply_requested", "dismiss"]) } else { json!(["dismiss"]) },
        "changed": result.changed,
        "diff": diff.text,
        "diff_truncated": diff.truncated,
        "files": result.files,
        "frames": frames,
        "frame_count": frame_count,
        "frames_truncated": frames_truncated,
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

#[derive(Debug, Clone)]
struct TruncatedText {
    text: String,
    truncated: bool,
}

fn truncated_text(value: &str, max_bytes: usize) -> TruncatedText {
    if value.len() <= max_bytes {
        return TruncatedText {
            text: value.to_string(),
            truncated: false,
        };
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut text = value[..end].to_string();
    text.push_str("\n… truncated …");
    TruncatedText {
        text,
        truncated: true,
    }
}

fn bounded_context(context: &bcode_vim_edit::TextContext) -> serde_json::Value {
    let lines = context
        .lines
        .iter()
        .take(MAX_CONTEXT_LINES)
        .map(|line| truncated_text(line, MAX_CONTEXT_LINE_CHARS).text)
        .collect::<Vec<_>>();
    json!({
        "start_line": context.start_line,
        "lines": lines,
    })
}

fn single_file_playback_frames(path: &str, result: &VimEditResult) -> Vec<serde_json::Value> {
    let total = result.events.len();
    bounded_event_indexes(total)
        .into_iter()
        .enumerate()
        .map(|(frame_index, event_index)| {
            let event = &result.events[event_index];
            json!({
                "frame_index": frame_index,
                "file_index": 0,
                "file_total": 1,
                "path": path,
                "step_index": event.step_index,
                "step_total": total,
                "step": event.step,
                "before_cursor": event.before_cursor,
                "after_cursor": event.after_cursor,
                "cursor": event.after_cursor,
                "nvim_mode": event.nvim_mode,
                "context": bounded_context(&event.context),
                "changed": event.changed,
                "message": event.message,
            })
        })
        .collect()
}

fn multi_file_playback_frames(
    result: &bcode_vim_edit::VimEditMultiFileEditResult,
) -> Vec<serde_json::Value> {
    let mut events = result
        .files
        .iter()
        .enumerate()
        .flat_map(|(file_index, file)| {
            file.events
                .iter()
                .map(move |event| (file_index, file, event))
        })
        .collect::<Vec<_>>();
    events.sort_by_key(|(_, _, event)| event.step_index);
    let total = events.len();
    bounded_event_indexes(total)
        .into_iter()
        .enumerate()
        .filter_map(|(frame_index, event_index)| {
            let (file_index, file, event) = events.get(event_index)?;
            Some(json!({
                "frame_index": frame_index,
                "file_index": file_index,
                "file_total": result.files.len(),
                "path": file.path,
                "step_index": event.step_index,
                "step_total": total,
                "step": event.step,
                "before_cursor": event.before_cursor,
                "after_cursor": event.after_cursor,
                "cursor": event.after_cursor,
                "nvim_mode": event.nvim_mode,
                "context": bounded_context(&event.context),
                "changed": event.changed,
                "message": event.message,
            }))
        })
        .collect()
}

fn bounded_event_indexes(total: usize) -> Vec<usize> {
    if total <= MAX_PLAYBACK_FRAMES {
        return (0..total).collect();
    }
    let head = MAX_PLAYBACK_FRAMES / 5;
    let tail = MAX_PLAYBACK_FRAMES.saturating_sub(head);
    (0..head).chain(total.saturating_sub(tail)..total).collect()
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
    use std::ffi::c_void;

    extern "C" fn collect_event(payload: *const u8, len: usize, user_data: *mut c_void) {
        let events = unsafe { &mut *(user_data.cast::<Vec<ToolInvocationStreamEvent>>()) };
        let payload = unsafe { std::slice::from_raw_parts(payload, len) };
        events.push(serde_json::from_slice(payload).expect("stream event"));
    }

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
    fn resume_apply_request_runs_apply_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), "foo").expect("write temp file");
        let original_arguments = json!({
            "path": file.path(),
            "steps": [{ "ex": "%s/foo/bar/" }]
        });
        let resume = InteractiveToolResumeRequest {
            tool_call_id: "call-apply".to_string(),
            tool_name: "vim_edit.preview".to_string(),
            interaction_id: "call-apply-vim-edit-playback".to_string(),
            original_arguments: original_arguments.clone(),
            interactive_request: InteractiveToolRequest {
                interaction_id: "call-apply-vim-edit-playback".to_string(),
                interaction_kind: Some(VIM_EDIT_PLAYBACK_INTERACTION_KIND.to_string()),
                surface_kind: VIM_EDIT_PLAYBACK_SURFACE.to_string(),
                request: json!({ "playback": { "original_arguments": original_arguments } }),
                required: false,
                turn_behavior:
                    bcode_tool::InteractiveToolTurnBehavior::CompleteTurnWithPendingInteraction,
                render_target: bcode_tool::InteractiveToolRenderTarget::TranscriptToolCall,
            },
            resolution: bcode_tool::InteractiveToolResolution::Submitted {
                payload: json!({ "action": "apply_requested" }),
            },
        };
        let context = NativeServiceContext {
            plugin_id: VIM_EDIT_PLUGIN_ID.to_string(),
            request: ServiceRequest {
                interface_id: TOOL_SERVICE_INTERFACE_ID.to_string(),
                operation: OP_RESUME_INTERACTIVE_TOOL.to_string(),
                payload: serde_json::to_vec(&resume).expect("resume payload"),
            },
            config: bcode_plugin_sdk::PluginConfigContext::default(),
            events: ServiceEventEmitter::default(),
            cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            bridge: bcode_plugin_sdk::ServiceBridge::default(),
        };
        let service_response = resume_interactive_tool(&context);
        assert!(service_response.error.is_none(), "{service_response:?}");
        let response: ToolInvocationResponse =
            serde_json::from_slice(&service_response.payload).expect("tool response");
        assert!(!response.is_error, "{}", response.output);
        assert_eq!(
            std::fs::read_to_string(file.path()).expect("read file"),
            "bar"
        );
    }

    #[test]
    fn live_event_stream_emits_started_running_finished_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), "foo bar").expect("write temp file");
        let mut events = Vec::<ToolInvocationStreamEvent>::new();
        let emitter = ServiceEventEmitter::new(
            Some(collect_event),
            std::ptr::addr_of_mut!(events).cast::<c_void>(),
        );
        let response = tool_vim_edit_with_nvim_executable(
            json!({ "path": file.path(), "steps": [{ "keys": "w" }, { "keys": "b" }] }),
            None,
            VimEditMode::Preview,
            "call-live",
            "vim_edit.preview",
            None,
            emitter,
        );
        assert!(!response.is_error, "{}", response.output);
        let visual_events = events
            .iter()
            .filter_map(|event| match event {
                ToolInvocationStreamEvent::VisualUpdate {
                    visual, streaming, ..
                } => Some((visual, streaming)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(visual_events.len() >= 3, "{visual_events:#?}");
        assert_eq!(visual_events[0].0.schema, VIM_EDIT_LIVE_SCHEMA);
        assert_eq!(
            visual_events[0].0.visual_id.as_deref(),
            Some("call-live-vim-live")
        );
        assert_eq!(visual_events[0].0.payload["phase"], "started");
        assert!(visual_events.iter().any(|(visual, _)| {
            visual.payload["phase"] == "running" && visual.payload.get("context").is_some()
        }));
        let Some((last, streaming)) = visual_events.last() else {
            panic!("expected final event");
        };
        assert_eq!(last.payload["phase"], "finished");
        assert!(last.payload.get("context").is_some(), "{last:#?}");
        assert!(last.payload.get("cursor").is_some(), "{last:#?}");
        assert!(last.payload.get("step").is_some(), "{last:#?}");
        assert_eq!(last.payload["nvim_mode"], "n");
        assert!(!**streaming);
    }

    #[test]
    fn live_event_stream_emits_error_for_missing_nvim() {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), "foo").expect("write temp file");
        let mut events = Vec::<ToolInvocationStreamEvent>::new();
        let emitter = ServiceEventEmitter::new(
            Some(collect_event),
            std::ptr::addr_of_mut!(events).cast::<c_void>(),
        );
        let response = tool_vim_edit_with_nvim_executable(
            json!({ "path": file.path(), "steps": [{ "keys": "w" }] }),
            None,
            VimEditMode::Preview,
            "call-error",
            "vim_edit.preview",
            Some(PathBuf::from("definitely-missing-bcode-plugin-nvim")),
            emitter,
        );
        assert!(response.is_error);
        let phases = events
            .iter()
            .filter_map(|event| match event {
                ToolInvocationStreamEvent::VisualUpdate {
                    visual, streaming, ..
                } => Some((visual.payload["phase"].clone(), *streaming)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            phases.first().map(|(phase, _)| phase),
            Some(&json!("started"))
        );
        assert_eq!(phases.last(), Some(&(json!("error"), false)));
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
        let response = vim_edit_success_response(
            "src/lib.rs",
            &result,
            "call-1",
            "vim_edit.preview",
            VimEditMode::Preview,
            &json!({ "path": "src/lib.rs", "steps": [] }),
        );
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
        assert!(artifact.metadata.get("frames").is_some());
        assert_eq!(artifact.metadata["frame_count"], 0);
        assert_eq!(artifact.metadata["frames_truncated"], false);
        assert_eq!(artifact.metadata["diff_truncated"], false);
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
            invocation_action_path: None,
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
            invocation_action_path: None,
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
            invocation_action_path: None,
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
            invocation_action_path: None,
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
            invocation_action_path: None,
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
