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
    VimEditMode, VimEditRequest, VimEditResult, VimEditSandbox, VimEditSession,
    VimEditSessionFinishResult, VimEditSessionSnapshot, VimEditStep, run_vim_edit,
    start_vim_edit_session,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_SESSIONS: usize = 8;
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_mins(15);
const SESSION_ABSOLUTE_TIMEOUT: Duration = Duration::from_hours(1);
static SESSION_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
static SESSION_STORE: OnceLock<Mutex<BTreeMap<String, VimEditSession>>> = OnceLock::new();

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

impl Drop for VimEditPlugin {
    fn drop(&mut self) {
        clear_sessions();
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

#[derive(Debug, Deserialize)]
struct SessionStartRequest {
    path: PathBuf,
    #[serde(default)]
    sandbox: VimEditToolSandbox,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct SessionInputRequest {
    session_id: String,
    step: VimEditToolStep,
}

#[derive(Debug, Deserialize)]
struct SessionIdRequest {
    session_id: String,
}

#[derive(Debug, Deserialize)]
struct SessionFinishRequest {
    session_id: String,
    apply: bool,
}

#[derive(Debug, serde::Serialize)]
struct SessionStartOutput<'a> {
    success: bool,
    session_id: &'a str,
    cursor: bcode_vim_edit::CursorPosition,
    nvim_mode: &'a str,
    context: &'a bcode_vim_edit::TextContext,
}

#[derive(Debug, serde::Serialize)]
struct SessionInputOutput<'a> {
    success: bool,
    session_id: &'a str,
    event: &'a bcode_vim_edit::VimEditEvent,
    snapshot: &'a VimEditSessionSnapshot,
}

#[derive(Debug, serde::Serialize)]
struct SessionSnapshotOutput<'a> {
    success: bool,
    session_id: &'a str,
    snapshot: &'a VimEditSessionSnapshot,
}

#[derive(Debug, serde::Serialize)]
struct SessionFinishOutput<'a> {
    success: bool,
    session_id: &'a str,
    result: &'a VimEditSessionFinishResult,
}

#[derive(Debug, serde::Serialize)]
struct SessionCancelOutput<'a> {
    success: bool,
    session_id: &'a str,
    cancelled: bool,
}

#[derive(Debug, serde::Serialize)]
struct SessionErrorOutput<'a> {
    success: bool,
    session_id: Option<&'a str>,
    error: String,
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
        tools: vec![
            preview_tool_definition(),
            apply_tool_definition(),
            session_start_tool_definition(),
            session_input_tool_definition(),
            session_snapshot_tool_definition(),
            session_finish_tool_definition(),
            session_cancel_tool_definition(),
        ],
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
        "vim_edit.session_start" => tool_session_start(request.arguments, request.cwd.as_deref()),
        "vim_edit.session_input" => tool_session_input(request.arguments),
        "vim_edit.session_snapshot" => tool_session_snapshot(request.arguments),
        "vim_edit.session_finish" => tool_session_finish(request.arguments),
        "vim_edit.session_cancel" => tool_session_cancel(request.arguments),
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

fn tool_session_start(arguments: serde_json::Value, cwd: Option<&Path>) -> ToolInvocationResponse {
    cleanup_expired_sessions();
    let request = match serde_json::from_value::<SessionStartRequest>(arguments) {
        Ok(request) => request,
        Err(error) => return session_error_response(None, error.to_string()),
    };
    if session_count() >= MAX_SESSIONS {
        return session_error_response(
            None,
            format!("maximum interactive Vim sessions reached ({MAX_SESSIONS})"),
        );
    }
    let path = resolve_session_path(cwd, &request.path);
    let mut session = match start_vim_edit_session(VimEditRequest {
        path,
        nvim_executable: None,
        steps: Vec::new(),
        mode: VimEditMode::Preview,
        sandbox: request.sandbox.into(),
        timeout: Duration::from_millis(request.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
    }) {
        Ok(session) => session,
        Err(error) => return session_error_response(None, error.to_string()),
    };
    let session_id = next_session_id();
    let snapshot = match session.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => return session_error_response(Some(&session_id), error.to_string()),
    };
    if let Err(error) = insert_session(session_id.clone(), session) {
        return session_error_response(Some(&session_id), error);
    }
    json_tool_response(
        &SessionStartOutput {
            success: true,
            session_id: &session_id,
            cursor: snapshot.cursor,
            nvim_mode: &snapshot.nvim_mode,
            context: &snapshot.context,
        },
        false,
    )
}

fn tool_session_input(arguments: serde_json::Value) -> ToolInvocationResponse {
    cleanup_expired_sessions();
    let request = match serde_json::from_value::<SessionInputRequest>(arguments) {
        Ok(request) => request,
        Err(error) => return session_error_response(None, error.to_string()),
    };
    let Some(mut session) = remove_session(&request.session_id) else {
        return session_error_response(
            Some(&request.session_id),
            "unknown Vim edit session".to_string(),
        );
    };
    let response = match session.input(request.step.into()) {
        Ok(result) => json_tool_response(
            &SessionInputOutput {
                success: true,
                session_id: &request.session_id,
                event: &result.event,
                snapshot: &result.snapshot,
            },
            false,
        ),
        Err(error) => session_error_response(Some(&request.session_id), error.to_string()),
    };
    if let Err(error) = insert_session(request.session_id.clone(), session) {
        return session_error_response(Some(&request.session_id), error);
    }
    response
}

fn tool_session_snapshot(arguments: serde_json::Value) -> ToolInvocationResponse {
    cleanup_expired_sessions();
    let request = match serde_json::from_value::<SessionIdRequest>(arguments) {
        Ok(request) => request,
        Err(error) => return session_error_response(None, error.to_string()),
    };
    let Some(mut session) = remove_session(&request.session_id) else {
        return session_error_response(
            Some(&request.session_id),
            "unknown Vim edit session".to_string(),
        );
    };
    let response = match session.snapshot() {
        Ok(snapshot) => json_tool_response(
            &SessionSnapshotOutput {
                success: true,
                session_id: &request.session_id,
                snapshot: &snapshot,
            },
            false,
        ),
        Err(error) => session_error_response(Some(&request.session_id), error.to_string()),
    };
    if let Err(error) = insert_session(request.session_id.clone(), session) {
        return session_error_response(Some(&request.session_id), error);
    }
    response
}

fn tool_session_finish(arguments: serde_json::Value) -> ToolInvocationResponse {
    cleanup_expired_sessions();
    let request = match serde_json::from_value::<SessionFinishRequest>(arguments) {
        Ok(request) => request,
        Err(error) => return session_error_response(None, error.to_string()),
    };
    let Some(session) = remove_session(&request.session_id) else {
        return session_error_response(
            Some(&request.session_id),
            "unknown Vim edit session".to_string(),
        );
    };
    match session.finish(request.apply) {
        Ok(result) => json_tool_response(
            &SessionFinishOutput {
                success: true,
                session_id: &request.session_id,
                result: &result,
            },
            false,
        ),
        Err(error) => session_error_response(Some(&request.session_id), error.to_string()),
    }
}

fn tool_session_cancel(arguments: serde_json::Value) -> ToolInvocationResponse {
    cleanup_expired_sessions();
    let request = match serde_json::from_value::<SessionIdRequest>(arguments) {
        Ok(request) => request,
        Err(error) => return session_error_response(None, error.to_string()),
    };
    let Some(session) = remove_session(&request.session_id) else {
        return session_error_response(
            Some(&request.session_id),
            "unknown Vim edit session".to_string(),
        );
    };
    session.cancel();
    json_tool_response(
        &SessionCancelOutput {
            success: true,
            session_id: &request.session_id,
            cancelled: true,
        },
        false,
    )
}

fn session_count() -> usize {
    session_store().lock().map_or(0, |store| store.len())
}

fn insert_session(session_id: String, session: VimEditSession) -> Result<(), String> {
    match session_store().lock() {
        Ok(mut store) => {
            store.insert(session_id, session);
            Ok(())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn remove_session(session_id: &str) -> Option<VimEditSession> {
    session_store()
        .lock()
        .ok()
        .and_then(|mut store| store.remove(session_id))
}

fn session_store() -> &'static Mutex<BTreeMap<String, VimEditSession>> {
    SESSION_STORE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn next_session_id() -> String {
    let id = SESSION_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("vim-edit-session-{id}")
}

fn cleanup_expired_sessions() {
    let Ok(mut store) = session_store().lock() else {
        return;
    };
    let now = Instant::now();
    store.retain(|_, session| {
        now.duration_since(session.last_accessed_at()) <= SESSION_IDLE_TIMEOUT
            && now.duration_since(session.started_at()) <= SESSION_ABSOLUTE_TIMEOUT
    });
}

fn clear_sessions() {
    if let Ok(mut store) = session_store().lock() {
        store.clear();
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

fn session_error_response(session_id: Option<&str>, error: String) -> ToolInvocationResponse {
    json_tool_response(
        &SessionErrorOutput {
            success: false,
            session_id,
            error,
        },
        true,
    )
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

fn session_start_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "vim_edit.session_start".to_string(),
        description: "Start an interactive isolated headless Neovim RPC session on a temp copy of one UTF-8 file. Returns a session_id plus cursor, mode, and context. Sandbox is fixed for session lifetime; sandbox=\"dangerously_disabled\" is unsafe and never default.".to_string(),
        input_schema: session_start_schema(),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: path_policy("read", ToolArgumentKind::ReadPath),
        ui: ToolUiMetadata::default(),
    }
}

fn session_input_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "vim_edit.session_input".to_string(),
        description: "Apply one Vim edit step to an existing interactive Neovim RPC session. Returns cursor, mode, context, diff so far, and clear step errors.".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["session_id", "step"],
            "properties": {
                "session_id": { "type": "string" },
                "step": vim_edit_step_schema()
            }
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn session_snapshot_tool_definition() -> ToolDefinition {
    session_id_tool_definition(
        "vim_edit.session_snapshot",
        "Return cursor, mode, bounded context, and diff so far for an interactive Vim edit session.",
    )
}

fn session_finish_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "vim_edit.session_finish".to_string(),
        description: "Finish an interactive Vim edit session. If apply=true, writes only the requested file and requires write permission; if apply=false, leaves the file unchanged.".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["session_id", "apply"],
            "properties": {
                "session_id": { "type": "string" },
                "apply": { "type": "boolean" }
            }
        }),
        side_effect: ToolSideEffect::WriteFiles,
        requires_permission: true,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn session_cancel_tool_definition() -> ToolDefinition {
    session_id_tool_definition(
        "vim_edit.session_cancel",
        "Cancel an interactive Vim edit session, kill its Neovim process, clean up temp files, and leave the requested file unchanged.",
    )
}

fn session_id_tool_definition(name: &str, description: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["session_id"],
            "properties": { "session_id": { "type": "string" } }
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn session_start_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "required": ["path"],
        "properties": {
            "path": { "type": "string" },
            "sandbox": {
                "type": "string",
                "enum": ["default", "dangerously_disabled"]
            },
            "timeout_ms": { "type": "integer", "minimum": 1 }
        }
    })
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
        "required": ["path", "steps"],
        "properties": {
            "path": { "type": "string" },
            "steps": {
                "type": "array",
                "items": vim_edit_step_schema()
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
    fn tool_definitions_include_all_vim_edit_tools() {
        let tools = ToolList {
            tools: vec![
                preview_tool_definition(),
                apply_tool_definition(),
                session_start_tool_definition(),
                session_input_tool_definition(),
                session_snapshot_tool_definition(),
                session_finish_tool_definition(),
                session_cancel_tool_definition(),
            ],
        };
        let names = tools
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "vim_edit.preview",
                "vim_edit.apply",
                "vim_edit.session_start",
                "vim_edit.session_input",
                "vim_edit.session_snapshot",
                "vim_edit.session_finish",
                "vim_edit.session_cancel",
            ]
        );
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

    #[test]
    fn session_start_input_snapshot_finish_apply_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim plugin test because `nvim` is not available");
            return;
        }
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), "foo bar baz").expect("write original");
        let start = invoke_tool_request(ToolInvocationRequest {
            tool_call_id: "test".to_string(),
            name: "vim_edit.session_start".to_string(),
            arguments: json!({ "path": file.path(), "timeout_ms": 5000 }),
            cwd: None,
            artifact_dir: None,
            cancellation_path: None,
        });
        assert!(!start.is_error, "{}", start.output);
        let start_json: serde_json::Value = serde_json::from_str(&start.output).expect("json");
        let session_id = start_json["session_id"]
            .as_str()
            .expect("session id")
            .to_string();

        for step in [
            json!({ "keys": "w" }),
            json!({ "keys": "ciw" }),
            json!({ "insert": "qux" }),
            json!({ "keys": "<Esc>" }),
        ] {
            let response = invoke_tool_request(ToolInvocationRequest {
                tool_call_id: "test".to_string(),
                name: "vim_edit.session_input".to_string(),
                arguments: json!({ "session_id": session_id, "step": step }),
                cwd: None,
                artifact_dir: None,
                cancellation_path: None,
            });
            assert!(!response.is_error, "{}", response.output);
            assert!(response.output.contains("diff"));
        }

        let snapshot = invoke_tool_request(ToolInvocationRequest {
            tool_call_id: "test".to_string(),
            name: "vim_edit.session_snapshot".to_string(),
            arguments: json!({ "session_id": session_id }),
            cwd: None,
            artifact_dir: None,
            cancellation_path: None,
        });
        assert!(!snapshot.is_error, "{}", snapshot.output);
        assert!(snapshot.output.contains("foo qux baz"));

        let finish = invoke_tool_request(ToolInvocationRequest {
            tool_call_id: "test".to_string(),
            name: "vim_edit.session_finish".to_string(),
            arguments: json!({ "session_id": session_id, "apply": true }),
            cwd: None,
            artifact_dir: None,
            cancellation_path: None,
        });
        assert!(!finish.is_error, "{}", finish.output);
        assert_eq!(
            std::fs::read_to_string(file.path()).expect("read edited"),
            "foo qux baz"
        );
    }

    #[test]
    fn session_cancel_leaves_file_unchanged_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim plugin test because `nvim` is not available");
            return;
        }
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), "foo").expect("write original");
        let start = invoke_tool_request(ToolInvocationRequest {
            tool_call_id: "test".to_string(),
            name: "vim_edit.session_start".to_string(),
            arguments: json!({ "path": file.path() }),
            cwd: None,
            artifact_dir: None,
            cancellation_path: None,
        });
        assert!(!start.is_error, "{}", start.output);
        let start_json: serde_json::Value = serde_json::from_str(&start.output).expect("json");
        let session_id = start_json["session_id"]
            .as_str()
            .expect("session id")
            .to_string();
        let input = invoke_tool_request(ToolInvocationRequest {
            tool_call_id: "test".to_string(),
            name: "vim_edit.session_input".to_string(),
            arguments: json!({
                "session_id": session_id,
                "step": { "ex": "%s/foo/bar/" }
            }),
            cwd: None,
            artifact_dir: None,
            cancellation_path: None,
        });
        assert!(!input.is_error, "{}", input.output);
        let cancel = invoke_tool_request(ToolInvocationRequest {
            tool_call_id: "test".to_string(),
            name: "vim_edit.session_cancel".to_string(),
            arguments: json!({ "session_id": session_id }),
            cwd: None,
            artifact_dir: None,
            cancellation_path: None,
        });
        assert!(!cancel.is_error, "{}", cancel.output);
        assert!(cancel.output.contains("cancelled"));
        assert_eq!(
            std::fs::read_to_string(file.path()).expect("read original"),
            "foo"
        );
    }

    #[test]
    fn session_input_unknown_session_returns_clear_error() {
        let response = invoke_tool_request(ToolInvocationRequest {
            tool_call_id: "test".to_string(),
            name: "vim_edit.session_input".to_string(),
            arguments: json!({
                "session_id": "missing",
                "step": { "keys": "gg" }
            }),
            cwd: None,
            artifact_dir: None,
            cancellation_path: None,
        });
        assert!(response.is_error);
        assert!(response.output.contains("unknown Vim edit session"));
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
