#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Vim edit tool plugin for Bcode.
//!
//! This plugin exposes model-callable tools that drive the reusable
//! `bcode_vim_edit` Neovim RPC editing engine.

#[cfg(feature = "static-bundled")]
mod vim_edit_playback_tui;

use bcode_plugin_sdk::path::display;
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolArtifact,
    ToolContributionEnvelope, ToolContributionEvent, ToolContributionOperation,
    ToolContributionPersistence, ToolContributionPlacement, ToolDefinition, ToolInvocationRequest,
    ToolInvocationResponse, ToolInvocationResult, ToolList,
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
#[cfg(feature = "static-bundled")]
const VIM_EDIT_REQUEST_DRAFT_PREVIEW_SCHEMA: &str = "bcode.vim-edit.request-draft.preview";
#[cfg(feature = "static-bundled")]
const VIM_EDIT_REQUEST_DRAFT_APPLY_SCHEMA: &str = "bcode.vim-edit.request-draft.apply";
const VIM_EDIT_LIVE_SCHEMA: &str = "bcode.vim-edit.live";
const VIM_EDIT_PLAYBACK_SCHEMA: &str = "bcode.vim-edit.playback";
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

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct VimEditPreparationDescriptor {
    #[serde(default)]
    workspace_root: Option<PathBuf>,
    #[serde(default)]
    paths: Vec<PathBuf>,
}

fn vim_edit_workspace_root(
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
        return Err("duplicate Vim edit workspace host context".to_owned());
    }
    if entry.schema_version != bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION {
        return Err(format!(
            "unsupported Vim edit workspace host context version {}; expected {}",
            entry.schema_version,
            bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION
        ));
    }
    let root = entry
        .payload
        .get("working_directory")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Vim edit workspace host context working_directory is missing".to_owned())?;
    let root = PathBuf::from(root);
    if !root.is_absolute() {
        return Err("Vim edit workspace working directory must be absolute".to_owned());
    }
    Ok(Some(root))
}

fn vim_edit_argument_paths(arguments: &serde_json::Value) -> Vec<PathBuf> {
    arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .into_iter()
        .chain(
            arguments
                .get("files")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|file| {
                    file.get("path")
                        .and_then(serde_json::Value::as_str)
                        .map(PathBuf::from)
                }),
        )
        .collect()
}

fn vim_edit_policy_operation(
    request: &bcode_tool::ToolPreparationRequest,
    definition: &ToolDefinition,
) -> Result<bcode_plugin_sdk::ToolPolicyPreparation, String> {
    let workspace_root = vim_edit_workspace_root(request)?;
    let paths = vim_edit_argument_paths(&request.invocation.arguments)
        .into_iter()
        .map(|path| {
            if path.is_absolute() {
                Ok(path)
            } else {
                workspace_root
                    .as_ref()
                    .ok_or_else(|| {
                        "Vim edit relative path requires workspace host context".to_owned()
                    })
                    .map(|root| root.join(path))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let policy_paths = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    let operation = match definition.name.as_str() {
        "vim_edit.preview" => bcode_plugin_sdk::ToolPolicyOperation::Read {
            paths: policy_paths,
        },
        "vim_edit.apply" => bcode_plugin_sdk::ToolPolicyOperation::Write {
            paths: policy_paths,
            category: "write".to_owned(),
        },
        name => return Err(format!("unsupported Vim edit policy operation: {name}")),
    };
    let category = match definition.name.as_str() {
        "vim_edit.preview" => "read",
        "vim_edit.apply" => "edit",
        name => return Err(format!("unsupported Vim edit policy operation: {name}")),
    };
    Ok(
        bcode_plugin_sdk::ToolPolicyPreparation::new(
            definition.name == "vim_edit.apply",
            operation,
        )
        .with_identity(path_policy(category))
        .with_descriptor(
            serde_json::to_value(VimEditPreparationDescriptor {
                workspace_root,
                paths,
            })
            .map_err(|error| error.to_string())?,
        ),
    )
}

fn apply_vim_edit_preparation(
    arguments: &mut serde_json::Value,
    descriptor: &VimEditPreparationDescriptor,
) -> Result<(), String> {
    let object = arguments
        .as_object_mut()
        .ok_or_else(|| "Vim edit arguments must be an object".to_owned())?;
    if let Some(path) = object.get_mut("path") {
        if descriptor.paths.len() != 1 {
            return Err("Vim edit single-file descriptor must contain exactly one path".to_owned());
        }
        *path = serde_json::Value::String(descriptor.paths[0].display().to_string());
        return Ok(());
    }
    if let Some(files) = object
        .get_mut("files")
        .and_then(serde_json::Value::as_array_mut)
    {
        if files.len() != descriptor.paths.len() {
            return Err(
                "Vim edit multi-file descriptor path count does not match request".to_owned(),
            );
        }
        for (file, path) in files.iter_mut().zip(&descriptor.paths) {
            let path_value = file
                .as_object_mut()
                .and_then(|file| file.get_mut("path"))
                .ok_or_else(|| "Vim edit multi-file entry is missing path".to_owned())?;
            *path_value = serde_json::Value::String(path.display().to_string());
        }
        return Ok(());
    }
    if descriptor.paths.is_empty() {
        Ok(())
    } else {
        Err("Vim edit descriptor contains paths for a pathless request".to_owned())
    }
}

fn invoke_tool_service(context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    match request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(request),
        bcode_tool::OP_PREPARE_TOOL => prepare_tool_service_response(
            request,
            [preview_tool_definition(), apply_tool_definition()],
            vim_edit_policy_operation,
        ),
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

#[derive(Debug, Clone)]
struct VimProgressContext {
    events: ServiceEventEmitter,
    limits: bcode_plugin_sdk::TransientProgressLimits,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
}

fn invoke_tool(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<ToolInvocationRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let response = invoke_tool_request_with_events(
        request,
        VimProgressContext {
            events: context.events,
            limits: context.transient_progress_limits,
            cancellation: context.cancellation.clone(),
        },
    );
    json_response(&response)
}

#[cfg(test)]
fn invoke_tool_request(mut request: ToolInvocationRequest) -> ToolInvocationResponse {
    let definition = match request.name.as_str() {
        "vim_edit.preview" => preview_tool_definition(),
        "vim_edit.apply" => apply_tool_definition(),
        _ => return vim_edit_error_response(None, "unknown vim edit tool".to_owned()),
    };
    let host_context = Vec::new();
    let preparation = bcode_tool::ToolPreparationRequest {
        invocation: bcode_tool::ToolInvocationDescriptor {
            invocation_id: request.tool_call_id.clone(),
            tool_name: request.name.clone(),
            arguments: request.arguments.clone(),
        },
        host_context,
    };
    request.preparation_descriptor = match vim_edit_policy_operation(&preparation, &definition) {
        Ok(prepared) => prepared.descriptor,
        Err(error) => return vim_edit_error_response(None, error),
    };
    invoke_tool_request_with_events(
        request,
        VimProgressContext {
            events: ServiceEventEmitter::default(),
            limits: bcode_plugin_sdk::TransientProgressLimits::default(),
            cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
        },
    )
}

fn invoke_tool_request_with_events(
    request: ToolInvocationRequest,
    progress: VimProgressContext,
) -> ToolInvocationResponse {
    let descriptor = match serde_json::from_value::<VimEditPreparationDescriptor>(
        request.preparation_descriptor.clone(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            return vim_edit_error_response(
                None,
                format!("invalid Vim edit preparation descriptor: {error}"),
            );
        }
    };

    emit_request_contribution(
        progress.events,
        &request.tool_call_id,
        &request.name,
        &request.arguments,
    );
    let mut arguments = request.arguments;
    if let Err(error) = apply_vim_edit_preparation(&mut arguments, &descriptor) {
        return vim_edit_error_response(None, error);
    }
    match request.name.as_str() {
        "vim_edit.preview" => tool_vim_edit_with_nvim_executable(
            arguments,
            descriptor.workspace_root.as_deref(),
            VimEditMode::Preview,
            &request.tool_call_id,
            "vim_edit.preview",
            None,
            progress,
        ),
        "vim_edit.apply" => tool_vim_edit_with_nvim_executable(
            arguments,
            descriptor.workspace_root.as_deref(),
            VimEditMode::Apply,
            &request.tool_call_id,
            "vim_edit.apply",
            None,
            progress,
        ),
        _ => ToolInvocationResponse {
            output: "unknown vim edit tool".to_string(),
            is_error: true,
            content: Vec::new(),
            full_output: None,
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
        VimProgressContext {
            events: ServiceEventEmitter::default(),
            limits: bcode_plugin_sdk::TransientProgressLimits::default(),
            cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
        },
    )
}

fn tool_vim_edit_with_nvim_executable(
    arguments: serde_json::Value,
    cwd: Option<&Path>,
    mode: VimEditMode,
    tool_call_id: &str,
    tool_name: &str,
    nvim_executable: Option<PathBuf>,
    progress: VimProgressContext,
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
            progress,
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
            progress,
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
    progress_context: VimProgressContext,
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
    let events = progress_context.events;
    let mut progress = TransientProgressPublisher::with_limits_and_cancellation(
        events,
        run.tool_call_id,
        "vim-live",
        VIM_EDIT_PLUGIN_ID,
        VIM_EDIT_LIVE_SCHEMA,
        1,
        progress_context.limits,
        progress_context.cancellation,
    );
    emit_vim_live_phase(
        &mut progress,
        run.tool_name,
        "started",
        Some(&display_path),
        None,
    );
    let run_result = {
        let mut observer = |frame: VimEditFrame| {
            emit_vim_live_frame(&mut progress, run.tool_name, "running", &frame);
        };
        run_vim_edit_observed(edit_request, Some(&mut observer))
    };
    match run_result {
        Ok(result) => {
            let _ = progress.finish();
            vim_edit_success_response(
                &display_path,
                &result,
                run.tool_call_id,
                run.tool_name,
                run.mode,
                &run.original_arguments,
                &events,
            )
        }
        Err(error) => {
            let error = error.to_string();
            let _ = progress.finish();
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
    progress_context: VimProgressContext,
) -> ToolInvocationResponse {
    let entries = run
        .files
        .into_iter()
        .map(|file| VimEditMultiFileEntry {
            path: resolve_path(run.cwd, &file.path),
            steps: file.steps.into_iter().map(Into::into).collect(),
        })
        .collect::<Vec<_>>();
    let events = progress_context.events;
    let mut progress = TransientProgressPublisher::with_limits_and_cancellation(
        events,
        run.tool_call_id,
        "vim-live",
        VIM_EDIT_PLUGIN_ID,
        VIM_EDIT_LIVE_SCHEMA,
        1,
        progress_context.limits,
        progress_context.cancellation,
    );
    emit_vim_live_phase(&mut progress, run.tool_name, "started", None, None);
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
            emit_vim_live_frame(&mut progress, run.tool_name, "running", &frame);
        };
        run_vim_multi_file_edit_observed(&request, Some(&mut observer))
    };
    match run_result {
        Ok(result) => {
            let _ = progress.finish();
            vim_edit_multi_file_success_response(
                &result,
                run.tool_call_id,
                run.tool_name,
                run.mode,
                &run.original_arguments,
                &events,
            )
        }
        Err(error) => {
            let error = error.to_string();
            let _ = progress.finish();
            vim_edit_error_response(None, error)
        }
    }
}

fn emit_vim_live_phase(
    progress: &mut TransientProgressPublisher,
    tool_name: &str,
    phase: &str,
    path: Option<&str>,
    error: Option<&str>,
) {
    let _ = progress.upsert(&json!({
        "tool_name": tool_name,
        "phase": phase,
        "path": path,
        "error": error,
    }));
}

fn emit_vim_live_frame(
    progress: &mut TransientProgressPublisher,
    tool_name: &str,
    phase: &str,
    frame: &VimEditFrame,
) {
    let _ = progress.upsert_if_ready(&json!({
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
    }));
}

fn vim_edit_success_response(
    path: &str,
    result: &VimEditResult,
    tool_call_id: &str,
    tool_name: &str,
    mode: VimEditMode,
    original_arguments: &serde_json::Value,
    events: &ServiceEventEmitter,
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
    let playback = vim_edit_playback_payload(tool_name, path, result, mode, original_arguments);
    let response = playback_tool_response(&output, tool_call_id, &playback);
    emit_playback_contribution(events, tool_call_id, &playback);
    response
}

fn vim_edit_playback_payload(
    tool_name: &str,
    path: &str,
    result: &VimEditResult,
    mode: VimEditMode,
    original_arguments: &serde_json::Value,
) -> serde_json::Value {
    let summary = if result.changed {
        "vim edit changed file"
    } else {
        "vim edit produced no changes"
    };
    let diff = truncated_text(&result.diff, MAX_DIFF_BYTES);
    let frames = single_file_playback_frames(path, result);
    let frames_truncated = result.events.len() > frames.len();
    json!({
        "success": true,
        "error": null,
        "tool_name": tool_name,
        "tool_mode": mode,
        "original_arguments": original_arguments,
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
    })
}

fn vim_edit_multi_file_success_response(
    result: &bcode_vim_edit::VimEditMultiFileEditResult,
    tool_call_id: &str,
    tool_name: &str,
    mode: VimEditMode,
    original_arguments: &serde_json::Value,
    events: &ServiceEventEmitter,
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
        "changed": result.changed,
        "diff": diff.text,
        "diff_truncated": diff.truncated,
        "files": result.files,
        "frames": frames,
        "frame_count": frame_count,
        "frames_truncated": frames_truncated,
    });
    let response = playback_tool_response(&output, tool_call_id, &output);
    emit_playback_contribution(events, tool_call_id, &output);
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
    }
}

fn apply_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "vim_edit.apply".to_string(),
        description: "Apply ordered Vim/Neovim edits using isolated headless Neovim over RPC. Accepts either single-file path+steps or an ordered files array where each entry switches to that file and runs its steps. Requires write permission and writes only after the full workflow succeeds. Optional sandbox=\"dangerously_disabled\" is unsafe and explicitly bypasses default command filtering.".to_string(),
        input_schema: vim_edit_input_schema(),
    }
}

fn vim_edit_request_schema(operation: &str) -> Option<&'static str> {
    match operation {
        "vim_edit.preview" => Some(VIM_EDIT_REQUEST_PREVIEW_SCHEMA),
        "vim_edit.apply" => Some(VIM_EDIT_REQUEST_APPLY_SCHEMA),
        _ => None,
    }
}

fn emit_request_contribution(
    events: ServiceEventEmitter,
    invocation_id: &str,
    operation: &str,
    arguments: &serde_json::Value,
) {
    let Some(schema) = vim_edit_request_schema(operation) else {
        return;
    };
    let mut payload = arguments.as_object().cloned().unwrap_or_default();
    payload.insert(
        "operation".to_owned(),
        serde_json::Value::String(operation.to_owned()),
    );
    let event = ToolContributionEvent {
        invocation_id: invocation_id.to_owned(),
        contribution_id: "request".to_owned(),
        sequence: 1,
        producer_id: VIM_EDIT_PLUGIN_ID.to_owned(),
        schema: schema.to_owned(),
        schema_version: 1,
        operation: ToolContributionOperation::Upsert,
        persistence: ToolContributionPersistence::Durable,
        artifact: None,
        payload: serde_json::Value::Object(payload),
    };
    let envelope = ToolContributionEnvelope::new(ToolContributionPlacement::Request, event);
    if let Ok(payload) = serde_json::to_vec(&envelope) {
        events.emit(&payload);
    }
}

fn emit_playback_contribution(
    events: &ServiceEventEmitter,
    invocation_id: &str,
    payload: &serde_json::Value,
) {
    let event = ToolContributionEvent {
        invocation_id: invocation_id.to_owned(),
        contribution_id: "playback".to_owned(),
        sequence: 1,
        producer_id: VIM_EDIT_PLUGIN_ID.to_owned(),
        schema: VIM_EDIT_PLAYBACK_SCHEMA.to_owned(),
        schema_version: 1,
        operation: ToolContributionOperation::Upsert,
        persistence: ToolContributionPersistence::Durable,
        artifact: None,
        payload: payload.clone(),
    };
    let envelope = ToolContributionEnvelope::new(ToolContributionPlacement::Result, event);
    if let Ok(payload) = serde_json::to_vec(&envelope) {
        events.emit(&payload);
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

fn path_policy(category: &str) -> bcode_plugin_sdk::ToolPolicyIdentity {
    bcode_plugin_sdk::ToolPolicyIdentity {
        aliases: vec![category.to_string()],
        compatibility_aliases: Vec::new(),
        capabilities: vec![format!("vim_edit.{category}")],
        permission_category: Some(category.to_string()),
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

fn playback_tool_response<T: serde::Serialize>(
    value: &T,
    tool_call_id: &str,
    playback: &serde_json::Value,
) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value) {
        Ok(output) => ToolInvocationResponse {
            output,
            is_error: false,
            content: Vec::new(),
            full_output: None,
            result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(ToolArtifact {
                    artifact_id: format!("{tool_call_id}-vim-edit-playback"),
                    producer_plugin_id: VIM_EDIT_PLUGIN_ID.to_owned(),
                    schema: VIM_EDIT_PLAYBACK_SCHEMA.to_owned(),
                    schema_version: 1,
                    tool_call_id: Some(tool_call_id.to_owned()),
                    title: Some("Vim edit".to_owned()),
                    metadata: playback.clone(),
                    refs: Vec::new(),
                }),
            }),
        },
        Err(error) => ToolInvocationResponse {
            output: error.to_string(),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            result: None,
        },
    }
}

fn json_tool_response<T: serde::Serialize>(value: &T, is_error: bool) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value) {
        Ok(output) => ToolInvocationResponse {
            output,
            is_error,
            content: Vec::new(),
            full_output: None,
            result: None,
        },
        Err(error) => ToolInvocationResponse {
            output: error.to_string(),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            result: None,
        },
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> StaticPluginVtable {
    static_plugin_vtable!(VimEditPlugin, include_str!("../bcode-plugin.toml"))
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn vim_edit_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    registry.register_visual_adapter(Box::new(
        vim_edit_playback_tui::VimEditPlaybackTuiVisualAdapter,
    ));
    registry
}

#[cfg(not(feature = "static-bundled"))]
export_plugin!(VimEditPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vim_edit_requests_remove_legacy_visuals_and_map_contribution_schemas() {
        assert_eq!(
            vim_edit_request_schema("vim_edit.preview"),
            Some(VIM_EDIT_REQUEST_PREVIEW_SCHEMA)
        );
        assert_eq!(
            vim_edit_request_schema("vim_edit.apply"),
            Some(VIM_EDIT_REQUEST_APPLY_SCHEMA)
        );
        assert_eq!(vim_edit_request_schema("unknown"), None);
    }
    use std::ffi::c_void;

    extern "C" fn collect_event(payload: *const u8, len: usize, user_data: *mut c_void) {
        let events = unsafe { &mut *(user_data.cast::<Vec<ToolContributionEnvelope>>()) };
        let payload = unsafe { std::slice::from_raw_parts(payload, len) };
        events.push(serde_json::from_slice(payload).expect("contribution event"));
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
    fn vim_edit_catalog_preparation_accepts_missing_paths() {
        for definition in [preview_tool_definition(), apply_tool_definition()] {
            let request = bcode_tool::ToolPreparationRequest {
                invocation: bcode_tool::ToolInvocationDescriptor {
                    invocation_id: "catalog".to_owned(),
                    tool_name: definition.name.clone(),
                    arguments: serde_json::Value::Null,
                },
                host_context: Vec::new(),
            };
            vim_edit_policy_operation(&request, &definition)
                .expect("catalog Vim policy preparation should remain total");
        }
    }

    fn workspace_context(path: &Path) -> Vec<bcode_tool::ToolHostContextEntry> {
        vec![bcode_tool::ToolHostContextEntry {
            schema: bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA.to_owned(),
            schema_version: bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION,
            payload: serde_json::json!({"working_directory": path}),
        }]
    }

    #[test]
    fn preview_tool_is_read_only_without_permission() {
        let tool = preview_tool_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "preview".to_owned(),
                tool_name: tool.name.clone(),
                arguments: json!({"path": "src/lib.rs", "steps": []}),
            },
            host_context: workspace_context(Path::new("/tmp/workspace")),
        };
        let policy = vim_edit_policy_operation(&request, &tool).expect("preview policy");
        assert!(!policy.requires_permission);
        assert_eq!(
            policy.operation,
            bcode_plugin_sdk::ToolPolicyOperation::Read {
                paths: vec!["/tmp/workspace/src/lib.rs".to_owned()],
            }
        );
    }

    #[test]
    fn apply_tool_writes_and_requires_permission() {
        let tool = apply_tool_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "apply".to_owned(),
                tool_name: tool.name.clone(),
                arguments: json!({
                    "files": [
                        {"path": "src/lib.rs", "steps": []},
                        {"path": "src/main.rs", "steps": []}
                    ]
                }),
            },
            host_context: workspace_context(Path::new("/tmp/workspace")),
        };
        let policy = vim_edit_policy_operation(&request, &tool).expect("apply policy");
        assert!(policy.requires_permission);
        assert_eq!(
            policy.operation,
            bcode_plugin_sdk::ToolPolicyOperation::Write {
                paths: vec![
                    "/tmp/workspace/src/lib.rs".to_owned(),
                    "/tmp/workspace/src/main.rs".to_owned(),
                ],
                category: "write".to_owned(),
            }
        );
        assert_eq!(
            serde_json::from_value::<VimEditPreparationDescriptor>(policy.descriptor)
                .expect("Vim edit descriptor"),
            VimEditPreparationDescriptor {
                workspace_root: Some(PathBuf::from("/tmp/workspace")),
                paths: vec![
                    PathBuf::from("/tmp/workspace/src/lib.rs"),
                    PathBuf::from("/tmp/workspace/src/main.rs"),
                ],
            }
        );
    }

    #[test]
    fn vim_edit_relative_path_requires_workspace_context() {
        let tool = preview_tool_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "preview".to_owned(),
                tool_name: tool.name.clone(),
                arguments: json!({"path": "src/lib.rs", "steps": []}),
            },
            host_context: Vec::new(),
        };

        let error = vim_edit_policy_operation(&request, &tool)
            .expect_err("relative path without workspace");

        assert!(error.contains("requires workspace host context"));
    }

    #[test]
    fn vim_edit_invocation_applies_ordered_prepared_paths() {
        let mut arguments = json!({
            "files": [
                {"path": "src/lib.rs", "steps": []},
                {"path": "src/main.rs", "steps": []}
            ]
        });
        let descriptor = VimEditPreparationDescriptor {
            workspace_root: Some(PathBuf::from("/tmp/workspace")),
            paths: vec![
                PathBuf::from("/tmp/workspace/src/lib.rs"),
                PathBuf::from("/tmp/workspace/src/main.rs"),
            ],
        };

        apply_vim_edit_preparation(&mut arguments, &descriptor)
            .expect("apply Vim edit preparation");

        assert_eq!(arguments["files"][0]["path"], "/tmp/workspace/src/lib.rs");
        assert_eq!(arguments["files"][1]["path"], "/tmp/workspace/src/main.rs");
    }

    #[test]
    fn vim_edit_invocation_rejects_descriptor_path_count_mismatch() {
        let mut arguments = json!({
            "files": [
                {"path": "src/lib.rs", "steps": []},
                {"path": "src/main.rs", "steps": []}
            ]
        });
        let descriptor = VimEditPreparationDescriptor {
            workspace_root: Some(PathBuf::from("/tmp/workspace")),
            paths: vec![PathBuf::from("/tmp/workspace/src/lib.rs")],
        };

        let error = apply_vim_edit_preparation(&mut arguments, &descriptor)
            .expect_err("descriptor mismatch");

        assert!(error.contains("path count does not match"));
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
    fn live_event_stream_emits_started_running_finished_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), "foo bar").expect("write temp file");
        let mut events = Vec::<ToolContributionEnvelope>::new();
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
            VimProgressContext {
                events: emitter,
                limits: bcode_plugin_sdk::TransientProgressLimits {
                    max_encoded_bytes: 256 * 1024,
                    min_interval_ms: 0,
                },
                cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            },
        );
        assert!(!response.is_error, "{}", response.output);
        let live_events = events
            .iter()
            .filter(|event| event.contribution.schema == VIM_EDIT_LIVE_SCHEMA)
            .collect::<Vec<_>>();
        assert!(live_events.len() >= 3, "{events:#?}");
        assert_eq!(live_events[0].contribution.contribution_id, "vim-live");
        assert_eq!(live_events[0].contribution.payload["phase"], "started");
        assert!(live_events.iter().any(|event| {
            event.contribution.payload["phase"] == "running"
                && event.contribution.payload.get("context").is_some()
        }));
        assert_eq!(
            live_events.last().map(|event| event.contribution.operation),
            Some(ToolContributionOperation::Remove)
        );
        assert!(events.iter().any(|event| {
            event.contribution.schema == VIM_EDIT_PLAYBACK_SCHEMA
                && event.contribution.persistence == ToolContributionPersistence::Durable
        }));
    }

    #[test]
    fn live_event_stream_emits_error_for_missing_nvim() {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), "foo").expect("write temp file");
        let mut events = Vec::<ToolContributionEnvelope>::new();
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
            VimProgressContext {
                events: emitter,
                limits: bcode_plugin_sdk::TransientProgressLimits {
                    max_encoded_bytes: 256 * 1024,
                    min_interval_ms: 0,
                },
                cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            },
        );
        assert!(response.is_error);
        assert_eq!(
            events
                .first()
                .map(|event| &event.contribution.payload["phase"]),
            Some(&json!("started"))
        );
        assert_eq!(
            events.last().map(|event| event.contribution.operation),
            Some(ToolContributionOperation::Remove)
        );
    }

    #[test]
    fn success_response_emits_only_durable_vim_edit_playback_contribution() {
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
        let mut events = Vec::<ToolContributionEnvelope>::new();
        let emitter = ServiceEventEmitter::new(
            Some(collect_event),
            std::ptr::addr_of_mut!(events).cast::<c_void>(),
        );
        let response = vim_edit_success_response(
            "src/lib.rs",
            &result,
            "call-1",
            "vim_edit.preview",
            VimEditMode::Preview,
            &json!({ "path": "src/lib.rs", "steps": [] }),
            &emitter,
        );
        let artifact = match response.result.as_ref() {
            Some(ToolInvocationResult::Artifact { artifact }) => artifact,
            result => panic!("expected canonical playback artifact, got {result:?}"),
        };
        assert_eq!(artifact.schema, VIM_EDIT_PLAYBACK_SCHEMA);
        assert_eq!(artifact.producer_plugin_id, VIM_EDIT_PLUGIN_ID);
        assert_eq!(artifact.metadata["path"], "src/lib.rs");
        let playback = events
            .iter()
            .find(|event| event.contribution.contribution_id == "playback")
            .expect("durable playback contribution");
        assert_eq!(playback.contribution.schema, VIM_EDIT_PLAYBACK_SCHEMA);
        assert_eq!(playback.contribution.producer_id, VIM_EDIT_PLUGIN_ID);
        assert_eq!(
            playback.contribution.persistence,
            ToolContributionPersistence::Durable
        );
        assert_eq!(
            playback.contribution.payload["tool_name"],
            "vim_edit.preview"
        );
        assert_eq!(playback.contribution.payload["path"], "src/lib.rs");
        assert_eq!(
            playback.contribution.payload["summary"],
            "vim edit changed file"
        );
        assert_eq!(playback.contribution.payload["success"], true);
        assert!(playback.contribution.payload.get("events").is_some());
        assert!(playback.contribution.payload.get("frames").is_some());
        assert_eq!(playback.contribution.payload["frame_count"], 0);
        assert_eq!(playback.contribution.payload["frames_truncated"], false);
        assert_eq!(playback.contribution.payload["diff_truncated"], false);
        assert!(playback.contribution.payload.get("final_context").is_some());
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
            preparation_descriptor: serde_json::Value::Null,
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
            preparation_descriptor: serde_json::Value::Null,
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
            preparation_descriptor: serde_json::Value::Null,
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
            preparation_descriptor: serde_json::Value::Null,
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
            preparation_descriptor: serde_json::Value::Null,
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
