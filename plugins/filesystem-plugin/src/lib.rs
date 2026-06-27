#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! filesystem service plugin for Bcode.

use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    FileChangeResult, ImageMetadata, ImageRefContent, ListToolsRequest, OP_INVOKE_TOOL,
    OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolDefinition, ToolInvocationRequest,
    ToolInvocationResponse, ToolInvocationResult, ToolInvocationStreamEvent, ToolList,
    ToolLiveArgumentPreviewMetadata, ToolPresentationEvent, ToolPresentationField,
    ToolPresentationFieldKind, ToolPresentationFieldValue, ToolPresentationSection,
    ToolPresentationTarget, ToolRequestPresentationMetadata, ToolResultContent, ToolSideEffect,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const FILESYSTEM_INTERFACE_ID: &str = "bcode.filesystem/v1";
const DEFAULT_SEARCH_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_GREP_MAX_MATCHES: usize = 100;
const DEFAULT_FIND_MAX_RESULTS: usize = 1_000;
const DEFAULT_LIST_MAX_ENTRIES: usize = 1_000;
const MAX_EXTERNAL_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_RUST_GREP_FILE_BYTES: u64 = 4 * 1024 * 1024;
const TERMINATION_GRACE_MS: u64 = 500;
const DEFAULT_READ_MAX_LINES: usize = 1_000;
const DEFAULT_READ_MAX_BYTES: usize = 256 * 1024;
const DEFAULT_ARTIFACT_READ_MAX_BYTES: usize = 64 * 1024;
const DEFAULT_ARTIFACT_GREP_MAX_MATCHES: usize = 100;

/// filesystem plugin.
#[derive(Default)]
pub struct FilesystemPlugin;

impl RustPlugin for FilesystemPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.interface_id.as_str() {
            FILESYSTEM_INTERFACE_ID => invoke_filesystem_service(&context),
            TOOL_SERVICE_INTERFACE_ID => invoke_tool_service(&context),
            _ => ServiceResponse::error(
                "unsupported_interface",
                "unsupported filesystem plugin service interface",
            ),
        }
    }
}

struct ProgressReporter {
    events: ServiceEventEmitter,
    tool_call_id: String,
    sequence: u64,
    next_visited_report: usize,
}

impl ProgressReporter {
    const fn new(events: ServiceEventEmitter, tool_call_id: String) -> Self {
        Self {
            events,
            tool_call_id,
            sequence: 0,
            next_visited_report: 250,
        }
    }

    fn emit(&mut self, message: impl Into<String>) {
        self.sequence = self.sequence.saturating_add(1);
        let event = ToolInvocationStreamEvent::Status {
            tool_call_id: self.tool_call_id.clone(),
            sequence: self.sequence,
            message: message.into(),
        };
        if let Ok(payload) = serde_json::to_vec(&event) {
            self.events.emit(&payload);
        }
    }

    fn maybe_visited(&mut self, kind: &str, visited_entries: usize) {
        if visited_entries >= self.next_visited_report {
            self.emit(format!("{kind}: visited {visited_entries} entries"));
            self.next_visited_report = self.next_visited_report.saturating_add(250);
        }
    }
}

#[derive(Default)]
struct ProgressSummary {
    matches: usize,
    bytes_scanned: u64,
}

#[derive(Debug, Deserialize)]
struct ReadRequest {
    path: PathBuf,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
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

#[derive(Debug, Deserialize)]
struct ListRequest {
    path: PathBuf,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    max_entries: Option<usize>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct ListEntry {
    path: String,
    kind: String,
}

#[derive(Debug, Serialize)]
struct ListResponse {
    entries: Vec<ListEntry>,
    backend: String,
    timed_out: bool,
    partial: bool,
    visited_entries: usize,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FindRequest {
    path: PathBuf,
    pattern: String,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct FindResponse {
    paths: Vec<String>,
    backend: String,
    timed_out: bool,
    partial: bool,
    visited_entries: usize,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GrepRequest {
    path: PathBuf,
    pattern: String,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    max_matches: Option<usize>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct GrepMatch {
    path: String,
    line_number: usize,
    line: String,
}

#[derive(Debug, Serialize)]
struct GrepResponse {
    matches: Vec<GrepMatch>,
    backend: String,
    timed_out: bool,
    partial: bool,
    visited_entries: usize,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ArtifactMetadataRequest {
    path: PathBuf,
}

#[derive(Debug, Serialize)]
struct ArtifactMetadataResponse {
    path: String,
    exists: bool,
    kind: String,
    byte_len: Option<u64>,
    content_type: Option<String>,
    complete: Option<bool>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ArtifactReadRequest {
    path: PathBuf,
    #[serde(default)]
    offset_bytes: Option<u64>,
    #[serde(default)]
    max_bytes: Option<usize>,
    #[serde(default)]
    from_end: bool,
}

#[derive(Debug, Serialize)]
struct ArtifactReadResponse {
    path: String,
    offset_bytes: u64,
    returned_bytes: usize,
    total_bytes: u64,
    from_end: bool,
    truncated: bool,
    contents: String,
}

#[derive(Debug, Deserialize)]
struct ArtifactGrepRequest {
    path: PathBuf,
    pattern: String,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    max_matches: Option<usize>,
}

#[derive(Debug, Serialize)]
struct ArtifactGrepResponse {
    path: String,
    matches: Vec<GrepMatch>,
    total_bytes: u64,
    partial: bool,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StatRequest {
    path: PathBuf,
}

#[derive(Debug, Serialize)]
struct StatResponse {
    exists: bool,
    kind: String,
    len: Option<u64>,
}

fn invoke_filesystem_service(context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    match request.operation.as_str() {
        "read" => read_file(request),
        "write" => write_file(request),
        "edit" => edit_file(request),
        "exists" => path_exists(request),
        "list" => list_directory_service(request),
        "find" => find_paths_service(request),
        "grep" => grep_files_service(request),
        "stat" => stat_path_service(request),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported filesystem service operation",
        ),
    }
}

fn invoke_tool_service(context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    match request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(request),
        OP_INVOKE_TOOL => invoke_tool(context),
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
            read_tool_definition(),
            write_tool_definition(),
            edit_tool_definition(),
            exists_tool_definition(),
            list_tool_definition(),
            find_tool_definition(),
            grep_tool_definition(),
            stat_tool_definition(),
            artifact_metadata_tool_definition(),
            artifact_read_tool_definition(),
            artifact_grep_tool_definition(),
        ],
    })
}

fn path_policy(
    aliases: &[&str],
    category: &str,
    kind: bcode_tool::ToolArgumentKind,
) -> bcode_tool::ToolPolicyMetadata {
    bcode_tool::ToolPolicyMetadata {
        aliases: aliases.iter().map(ToString::to_string).collect(),
        compatibility_aliases: compatibility_aliases_for(aliases),
        capabilities: aliases
            .iter()
            .map(|alias| format!("filesystem.{alias}"))
            .collect(),
        permission_category: Some(category.to_string()),
        argument_extractors: vec![bcode_tool::ToolArgumentExtractor {
            kind,
            argument: "path".to_string(),
        }],
    }
}

fn compatibility_aliases_for(aliases: &[&str]) -> Vec<bcode_tool::ToolCompatibilityAlias> {
    aliases
        .iter()
        .filter_map(|alias| match *alias {
            "read" => Some("Read"),
            "write" => Some("Write"),
            "edit" => Some("Edit"),
            "grep" => Some("Grep"),
            "find" => Some("Glob"),
            "ls" => Some("LS"),
            _ => None,
        })
        .map(|name| bcode_tool::ToolCompatibilityAlias::new("claude", name))
        .collect()
}

fn path_tool_ui(activity_label: &str, title: &str) -> bcode_tool::ToolUiMetadata {
    bcode_tool::ToolUiMetadata {
        activity_label: Some(activity_label.to_string()),
        live_argument_preview: None,

        request_presentation: Some(ToolRequestPresentationMetadata {
            title: title.to_string(),
            fields: vec![ToolPresentationField {
                label: "Path".to_string(),
                argument: "path".to_string(),
                kind: ToolPresentationFieldKind::Path,
                optional: false,
            }],
            preview: None,
        }),
    }
}

fn write_tool_ui(
    activity_label: &str,
    title: &str,
    preview_title: &str,
) -> bcode_tool::ToolUiMetadata {
    bcode_tool::ToolUiMetadata {
        activity_label: Some(activity_label.to_string()),
        live_argument_preview: Some(ToolLiveArgumentPreviewMetadata::FileEdit {
            path_fields: vec!["path".to_string()],
            old_text_fields: Vec::new(),
            new_text_fields: vec!["contents".to_string(), "new_text".to_string()],
            preview_title: Some(preview_title.to_string()),
            streaming_status: Some(format!("{activity_label} {{path}} · {{bytes}}")),
        }),

        request_presentation: Some(ToolRequestPresentationMetadata {
            title: title.to_string(),
            fields: vec![
                ToolPresentationField {
                    label: "Path".to_string(),
                    argument: "path".to_string(),
                    kind: ToolPresentationFieldKind::Path,
                    optional: false,
                },
                ToolPresentationField {
                    label: "Contents".to_string(),
                    argument: "contents".to_string(),
                    kind: ToolPresentationFieldKind::Text,
                    optional: false,
                },
            ],
            preview: Some(bcode_tool::ToolRequestPreviewMetadata::FileEdit {
                path_fields: vec!["path".to_string()],
                old_text_fields: Vec::new(),
                new_text_fields: vec!["contents".to_string(), "new_text".to_string()],
            }),
        }),
    }
}

fn read_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "filesystem.read".to_string(),
        description: "Read a file. Supports UTF-8 text files and images (PNG, JPEG, GIF, WebP). Image files are returned as model-visible image attachments with metadata.".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" },
                "offset": { "type": "integer", "minimum": 1, "description": "1-indexed line number to start reading from for text files" },
                "limit": { "type": "integer", "minimum": 1, "description": "Maximum number of text lines to return" }
            }
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: path_policy(&["read"], "read", bcode_tool::ToolArgumentKind::ReadPath),
        ui: path_tool_ui("reading", "Read file"),
    }
}

fn write_tool_definition() -> ToolDefinition {
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
        policy: path_policy(&["write"], "write", bcode_tool::ToolArgumentKind::WritePath),
        ui: write_tool_ui("writing", "Write file", "Write preview"),
    }
}

fn edit_tool_definition() -> ToolDefinition {
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
        policy: path_policy(&["edit"], "edit", bcode_tool::ToolArgumentKind::WritePath),
        ui: write_tool_ui("editing", "Edit file", "Edit preview"),
    }
}

fn exists_tool_definition() -> ToolDefinition {
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
        policy: path_policy(&["read"], "read", bcode_tool::ToolArgumentKind::ReadPath),
        ui: path_tool_ui("checking", "Check path"),
    }
}

fn list_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "filesystem.list".to_string(),
        description: "List directory entries".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" },
                "recursive": { "type": "boolean" },
                "max_entries": { "type": "integer", "minimum": 1 },
                "timeout_ms": { "type": "integer", "minimum": 1 }
            }
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: path_policy(
            &["ls", "read"],
            "read",
            bcode_tool::ToolArgumentKind::ReadPath,
        ),
        ui: path_tool_ui("listing", "List directory"),
    }
}

fn find_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "filesystem.find".to_string(),
        description: "Find paths by simple glob pattern".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["path", "pattern"],
            "properties": {
                "path": { "type": "string" },
                "pattern": { "type": "string" },
                "max_results": { "type": "integer", "minimum": 1 },
                "timeout_ms": { "type": "integer", "minimum": 1 }
            }
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: path_policy(
            &["find", "read"],
            "read",
            bcode_tool::ToolArgumentKind::ReadPath,
        ),
        ui: path_tool_ui("finding", "Find paths"),
    }
}

fn grep_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "filesystem.grep".to_string(),
        description: "Search UTF-8 files for a literal text pattern".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["path", "pattern"],
            "properties": {
                "path": { "type": "string" },
                "pattern": { "type": "string" },
                "glob": { "type": "string" },
                "ignore_case": { "type": "boolean" },
                "max_matches": { "type": "integer", "minimum": 1 },
                "timeout_ms": { "type": "integer", "minimum": 1 }
            }
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: path_policy(
            &["grep", "read"],
            "read",
            bcode_tool::ToolArgumentKind::ReadPath,
        ),
        ui: path_tool_ui("searching", "Search files"),
    }
}

fn stat_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "filesystem.stat".to_string(),
        description: "Read filesystem metadata for a path".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["path"],
            "properties": { "path": { "type": "string" } }
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: path_policy(
            &["stat", "read"],
            "read",
            bcode_tool::ToolArgumentKind::ReadPath,
        ),
        ui: path_tool_ui("stat", "Inspect path"),
    }
}

fn artifact_metadata_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "artifact.metadata".to_string(),
        description: "Read metadata for a saved tool-output artifact or trace blob".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["path"],
            "properties": { "path": { "type": "string" } }
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: bcode_tool::ToolPolicyMetadata::default(),
        ui: bcode_tool::ToolUiMetadata::default(),
    }
}

fn artifact_read_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "artifact.read".to_string(),
        description: "Read a bounded UTF-8 slice from a saved tool-output artifact. Supports byte offsets and tail reads without loading the whole artifact into model context.".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" },
                "offset_bytes": { "type": "integer", "minimum": 0, "description": "0-indexed byte offset to start reading from" },
                "max_bytes": { "type": "integer", "minimum": 1, "description": "Maximum bytes to return" },
                "from_end": { "type": "boolean", "description": "Read the last max_bytes bytes of the artifact" }
            }
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: bcode_tool::ToolPolicyMetadata::default(),
        ui: bcode_tool::ToolUiMetadata::default(),
    }
}

fn artifact_grep_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "artifact.grep".to_string(),
        description: "Search a saved UTF-8 tool-output artifact for a literal text pattern"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["path", "pattern"],
            "properties": {
                "path": { "type": "string" },
                "pattern": { "type": "string" },
                "ignore_case": { "type": "boolean" },
                "max_matches": { "type": "integer", "minimum": 1 }
            }
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: bcode_tool::ToolPolicyMetadata::default(),
        ui: bcode_tool::ToolUiMetadata::default(),
    }
}

fn invoke_tool(context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    let request = match request.payload_json::<ToolInvocationRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    if context.cancellation.is_cancelled() {
        return json_response(&ToolInvocationResponse {
            output: "filesystem tool cancelled".to_string(),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        });
    }
    let cwd = request.cwd.clone();
    let response = match request.name.as_str() {
        "filesystem.read" => tool_read(request.arguments, cwd.as_deref()),
        "filesystem.write" => tool_write(
            request.arguments,
            cwd.as_deref(),
            Some(context.events),
            &request.tool_call_id,
        ),
        "filesystem.edit" => tool_edit(
            request.arguments,
            cwd.as_deref(),
            Some(context.events),
            &request.tool_call_id,
        ),
        "filesystem.exists" => tool_exists(request.arguments, cwd.as_deref()),
        "filesystem.list" => tool_list(
            request.arguments,
            cwd.as_deref(),
            &context.cancellation,
            context.events,
            &request.tool_call_id,
        ),
        "filesystem.find" => tool_find(
            request.arguments,
            cwd.as_deref(),
            &context.cancellation,
            context.events,
            &request.tool_call_id,
        ),
        "filesystem.grep" => tool_grep(
            request.arguments,
            cwd.as_deref(),
            &context.cancellation,
            context.events,
            &request.tool_call_id,
        ),
        "filesystem.stat" => tool_stat(request.arguments, cwd.as_deref()),
        "artifact.metadata" => tool_artifact_metadata(request.arguments, cwd.as_deref()),
        "artifact.read" => tool_artifact_read(request.arguments, cwd.as_deref()),
        "artifact.grep" => tool_artifact_grep(request.arguments, cwd.as_deref()),
        _ => ToolInvocationResponse {
            output: format!("unknown filesystem tool: {}", request.name),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        },
    };
    json_response(&response)
}

fn tool_read(arguments: serde_json::Value, cwd: Option<&Path>) -> ToolInvocationResponse {
    match serde_json::from_value::<ReadRequest>(arguments) {
        Ok(request) => read_path_for_tool(&resolve_session_path(cwd, &request.path), &request),
        Err(error) => tool_json_error(&error),
    }
}

fn read_path_for_tool(path: &Path, request: &ReadRequest) -> ToolInvocationResponse {
    match image_file_metadata(path) {
        Ok(Some(image)) => image_tool_response(path, image),
        Ok(None) => match std::fs::read(path) {
            Ok(bytes) => text_tool_response(path, request, &bytes),
            Err(error) => tool_io_error(&error),
        },
        Err(error) => tool_io_error(&error),
    }
}

fn text_tool_response(path: &Path, request: &ReadRequest, bytes: &[u8]) -> ToolInvocationResponse {
    let Ok(contents) = std::str::from_utf8(bytes) else {
        return ToolInvocationResponse {
            output: format!(
                "Binary file could not be decoded as UTF-8.\nPath: {}\nSize: {} bytes\nUse a specialized tool to inspect this file type.",
                path.display(),
                bytes.len()
            ),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        };
    };
    let lines = contents.lines().collect::<Vec<_>>();
    let total_lines = lines.len();
    let start_line = request
        .offset
        .unwrap_or(1)
        .saturating_sub(1)
        .min(total_lines);
    let max_lines = request.limit.unwrap_or(DEFAULT_READ_MAX_LINES);
    let mut selected = Vec::new();
    let mut retained_bytes = 0usize;
    let mut byte_truncated = false;
    for line in lines.iter().skip(start_line).take(max_lines) {
        let line_bytes = line.len().saturating_add(1);
        if retained_bytes.saturating_add(line_bytes) > DEFAULT_READ_MAX_BYTES {
            byte_truncated = true;
            break;
        }
        selected.push(*line);
        retained_bytes = retained_bytes.saturating_add(line_bytes);
    }
    let mut output = selected.join("\n");
    let next_line = start_line.saturating_add(selected.len()).saturating_add(1);
    if next_line <= total_lines || byte_truncated {
        let _ = write!(
            output,
            "\n\n[Showing lines {}-{} of {total_lines}. Use offset={next_line} to continue.]",
            start_line.saturating_add(1),
            start_line.saturating_add(selected.len())
        );
    }
    ToolInvocationResponse {
        output,
        is_error: false,
        content: Vec::new(),
        full_output: None,
        host_action: None,
        result: None,
    }
}

fn tool_artifact_metadata(
    arguments: serde_json::Value,
    cwd: Option<&Path>,
) -> ToolInvocationResponse {
    match serde_json::from_value::<ArtifactMetadataRequest>(arguments) {
        Ok(request) => {
            let path = resolve_session_path(cwd, &request.path);
            json_tool_response(serde_json::to_value(artifact_metadata_response(&path)))
        }
        Err(error) => tool_json_error(&error),
    }
}

fn artifact_metadata_response(path: &Path) -> ArtifactMetadataResponse {
    match std::fs::metadata(path) {
        Ok(metadata) => ArtifactMetadataResponse {
            path: path.display().to_string(),
            exists: true,
            kind: metadata_kind(&metadata),
            byte_len: Some(metadata.len()),
            content_type: artifact_content_type(path),
            complete: Some(true),
            message: Some(
                "Artifact byte length reflects retained bytes on disk; upstream tool capture may have been bounded."
                    .to_string(),
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ArtifactMetadataResponse {
            path: path.display().to_string(),
            exists: false,
            kind: "missing".to_string(),
            byte_len: None,
            content_type: None,
            complete: None,
            message: Some(error.to_string()),
        },
        Err(error) => ArtifactMetadataResponse {
            path: path.display().to_string(),
            exists: false,
            kind: "error".to_string(),
            byte_len: None,
            content_type: None,
            complete: None,
            message: Some(error.to_string()),
        },
    }
}

fn tool_artifact_read(arguments: serde_json::Value, cwd: Option<&Path>) -> ToolInvocationResponse {
    match serde_json::from_value::<ArtifactReadRequest>(arguments) {
        Ok(request) => match read_artifact(&resolve_session_path(cwd, &request.path), &request) {
            Ok(response) => json_tool_response(serde_json::to_value(response)),
            Err(error) => ToolInvocationResponse {
                output: error,
                is_error: true,
                content: Vec::new(),
                full_output: None,
                host_action: None,
                result: None,
            },
        },
        Err(error) => tool_json_error(&error),
    }
}

fn read_artifact(
    path: &Path,
    request: &ArtifactReadRequest,
) -> Result<ArtifactReadResponse, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    let total_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let max_bytes = request.max_bytes.unwrap_or(DEFAULT_ARTIFACT_READ_MAX_BYTES);
    let requested_offset = if request.from_end {
        bytes.len().saturating_sub(max_bytes)
    } else {
        usize::try_from(request.offset_bytes.unwrap_or(0)).unwrap_or(usize::MAX)
    };
    let offset = requested_offset.min(bytes.len());
    let end = offset.saturating_add(max_bytes).min(bytes.len());
    let start = utf8_boundary_at_or_after_bytes(&bytes, offset);
    let end = utf8_boundary_at_or_before_bytes(&bytes, end);
    let contents = std::str::from_utf8(&bytes[start..end])
        .map_err(|error| format!("artifact slice is not valid UTF-8: {error}"))?
        .to_string();
    Ok(ArtifactReadResponse {
        path: path.display().to_string(),
        offset_bytes: u64::try_from(start).unwrap_or(u64::MAX),
        returned_bytes: end.saturating_sub(start),
        total_bytes,
        from_end: request.from_end,
        truncated: start > 0 || end < bytes.len(),
        contents,
    })
}

fn tool_artifact_grep(arguments: serde_json::Value, cwd: Option<&Path>) -> ToolInvocationResponse {
    match serde_json::from_value::<ArtifactGrepRequest>(arguments) {
        Ok(request) => match grep_artifact(&resolve_session_path(cwd, &request.path), &request) {
            Ok(response) => json_tool_response(serde_json::to_value(response)),
            Err(error) => ToolInvocationResponse {
                output: error,
                is_error: true,
                content: Vec::new(),
                full_output: None,
                host_action: None,
                result: None,
            },
        },
        Err(error) => tool_json_error(&error),
    }
}

fn grep_artifact(
    path: &Path,
    request: &ArtifactGrepRequest,
) -> Result<ArtifactGrepResponse, String> {
    let contents = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let pattern = if request.ignore_case {
        request.pattern.to_lowercase()
    } else {
        request.pattern.clone()
    };
    let max_matches = request
        .max_matches
        .unwrap_or(DEFAULT_ARTIFACT_GREP_MAX_MATCHES);
    let mut matches = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        let haystack = if request.ignore_case {
            line.to_lowercase()
        } else {
            line.to_string()
        };
        if haystack.contains(&pattern) {
            matches.push(GrepMatch {
                path: path.display().to_string(),
                line_number: index.saturating_add(1),
                line: line.to_string(),
            });
            if matches.len() >= max_matches {
                break;
            }
        }
    }
    let partial = matches.len() >= max_matches;
    Ok(ArtifactGrepResponse {
        path: path.display().to_string(),
        matches,
        total_bytes: u64::try_from(contents.len()).unwrap_or(u64::MAX),
        partial,
        message: partial.then(|| "maximum match count reached".to_string()),
    })
}

fn artifact_content_type(path: &Path) -> Option<String> {
    match path.extension().and_then(std::ffi::OsStr::to_str) {
        Some("json") => Some("application/json".to_string()),
        Some("txt") => Some("text/plain".to_string()),
        _ => None,
    }
}

fn utf8_boundary_at_or_after_bytes(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && std::str::from_utf8(&bytes[..index]).is_err() {
        index = index.saturating_add(1);
    }
    index
}

fn utf8_boundary_at_or_before_bytes(bytes: &[u8], mut index: usize) -> usize {
    while index > 0 && std::str::from_utf8(&bytes[..index]).is_err() {
        index = index.saturating_sub(1);
    }
    index
}

fn image_tool_response(path: &Path, image: ImageFileMetadata) -> ToolInvocationResponse {
    let metadata = std::fs::metadata(path);
    let byte_len = metadata.as_ref().map_or(0, std::fs::Metadata::len);
    let output = format!(
        "Read image file [{}]\nPath: {}\nDimensions: {}x{}\nSize: {} bytes\nReturned image reference for visual inspection.",
        image.mime_type,
        path.display(),
        image.width,
        image.height,
        byte_len
    );
    ToolInvocationResponse {
        output,
        is_error: false,
        content: vec![ToolResultContent::ImageRef {
            image: ImageRefContent {
                path: path.display().to_string(),
                mime_type: image.mime_type,
                metadata: ImageMetadata {
                    width: Some(image.width),
                    height: Some(image.height),
                    byte_len: Some(byte_len),
                    source_path: Some(path.display().to_string()),
                },
            },
        }],
        full_output: None,
        host_action: None,
        result: None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageFileMetadata {
    mime_type: String,
    width: u32,
    height: u32,
}

fn image_file_metadata(path: &Path) -> std::io::Result<Option<ImageFileMetadata>> {
    let mut file = std::fs::File::open(path)?;
    let mut header = [0_u8; 32];
    let read = file.read(&mut header)?;
    let Some(mime_type) = sniff_supported_image_mime(&header[..read]) else {
        return Ok(None);
    };
    let Ok((width, height)) = image::image_dimensions(path) else {
        return Ok(None);
    };
    Ok(Some(ImageFileMetadata {
        mime_type: mime_type.to_string(),
        width,
        height,
    }))
}

fn sniff_supported_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn emit_presentation(
    events: Option<ServiceEventEmitter>,
    tool_call_id: &str,
    sequence: u64,
    presentation: ToolPresentationEvent,
) {
    let event = ToolInvocationStreamEvent::Presentation {
        tool_call_id: tool_call_id.to_string(),
        sequence,
        presentation,
    };
    if let Ok(payload) = serde_json::to_vec(&event)
        && let Some(events) = events
    {
        events.emit(&payload);
    }
}

fn file_change_fields(path: &Path, summary: &str) -> ToolPresentationSection {
    ToolPresentationSection::Fields {
        fields: vec![
            ToolPresentationFieldValue {
                label: "Path".to_string(),
                value: path.display().to_string(),
            },
            ToolPresentationFieldValue {
                label: "Summary".to_string(),
                value: summary.to_string(),
            },
        ],
    }
}

fn tool_write(
    arguments: serde_json::Value,
    cwd: Option<&Path>,
    events: Option<ServiceEventEmitter>,
    tool_call_id: &str,
) -> ToolInvocationResponse {
    match serde_json::from_value::<WriteRequest>(arguments) {
        Ok(mut request) => {
            request.path = resolve_session_path(cwd, &request.path);
            emit_presentation(
                events,
                tool_call_id,
                1,
                ToolPresentationEvent::Status(bcode_tool::ToolStatusPresentation {
                    target: ToolPresentationTarget::Activity,
                    text: format!("applying file change {}", request.path.display()),
                    level: bcode_tool::ToolPresentationLevel::Info,
                }),
            );
            emit_presentation(
                events,
                tool_call_id,
                2,
                ToolPresentationEvent::Card(bcode_tool::ToolCardPresentation {
                    target: ToolPresentationTarget::Preview,
                    title: "Write preview".to_string(),
                    subtitle: Some("Applying".to_string()),
                    sections: vec![
                        file_change_fields(
                            &request.path,
                            &format!("{} bytes", request.contents.len()),
                        ),
                        ToolPresentationSection::Diff {
                            path: Some(request.path.display().to_string()),
                            old_text: String::new(),
                            new_text: request.contents.clone(),
                        },
                    ],
                }),
            );
            write_file_inner(&request.path, &request.contents).map_or_else(
                |error| {
                    emit_presentation(
                        events,
                        tool_call_id,
                        3,
                        ToolPresentationEvent::Card(bcode_tool::ToolCardPresentation {
                            target: ToolPresentationTarget::Result,
                            title: "File change failed".to_string(),
                            subtitle: None,
                            sections: vec![file_change_fields(&request.path, &error.to_string())],
                        }),
                    );
                    tool_io_error(&error)
                },
                |bytes_written| {
                    let summary = format!("wrote {bytes_written} bytes");
                    emit_presentation(
                        events,
                        tool_call_id,
                        3,
                        ToolPresentationEvent::Card(bcode_tool::ToolCardPresentation {
                            target: ToolPresentationTarget::Result,
                            title: "Applied file change".to_string(),
                            subtitle: None,
                            sections: vec![file_change_fields(&request.path, &summary)],
                        }),
                    );
                    ToolInvocationResponse {
                        output: summary.clone(),
                        is_error: false,
                        content: Vec::new(),
                        full_output: None,
                        host_action: None,
                        result: Some(ToolInvocationResult::FileChange {
                            result: FileChangeResult {
                                tool_name: "filesystem.write".to_owned(),
                                summary,
                                path: Some(request.path.display().to_string()),
                            },
                        }),
                    }
                },
            )
        }
        Err(error) => tool_json_error(&error),
    }
}

fn tool_edit(
    arguments: serde_json::Value,
    cwd: Option<&Path>,
    events: Option<ServiceEventEmitter>,
    tool_call_id: &str,
) -> ToolInvocationResponse {
    match serde_json::from_value::<EditRequest>(arguments) {
        Ok(mut request) => {
            request.path = resolve_session_path(cwd, &request.path);
            emit_presentation(
                events,
                tool_call_id,
                1,
                ToolPresentationEvent::Status(bcode_tool::ToolStatusPresentation {
                    target: ToolPresentationTarget::Activity,
                    text: format!("applying file change {}", request.path.display()),
                    level: bcode_tool::ToolPresentationLevel::Info,
                }),
            );
            emit_presentation(
                events,
                tool_call_id,
                2,
                ToolPresentationEvent::Card(bcode_tool::ToolCardPresentation {
                    target: ToolPresentationTarget::Preview,
                    title: "Edit preview".to_string(),
                    subtitle: Some("Applying".to_string()),
                    sections: vec![
                        file_change_fields(&request.path, "replacement"),
                        ToolPresentationSection::Diff {
                            path: Some(request.path.display().to_string()),
                            old_text: request.old_text.clone(),
                            new_text: request.new_text.clone(),
                        },
                    ],
                }),
            );
            edit_file_inner(&request).map_or_else(
                |error| {
                    emit_presentation(
                        events,
                        tool_call_id,
                        3,
                        ToolPresentationEvent::Card(bcode_tool::ToolCardPresentation {
                            target: ToolPresentationTarget::Result,
                            title: "File change failed".to_string(),
                            subtitle: None,
                            sections: vec![file_change_fields(&request.path, &error)],
                        }),
                    );
                    ToolInvocationResponse {
                        output: error,
                        is_error: true,
                        content: Vec::new(),
                        full_output: None,
                        host_action: None,
                        result: None,
                    }
                },
                |replacements| {
                    let summary = format!("applied {replacements} replacement");
                    emit_presentation(
                        events,
                        tool_call_id,
                        3,
                        ToolPresentationEvent::Card(bcode_tool::ToolCardPresentation {
                            target: ToolPresentationTarget::Result,
                            title: "Applied file change".to_string(),
                            subtitle: None,
                            sections: vec![file_change_fields(&request.path, &summary)],
                        }),
                    );
                    ToolInvocationResponse {
                        output: summary.clone(),
                        is_error: false,
                        content: Vec::new(),
                        full_output: None,
                        host_action: None,
                        result: Some(ToolInvocationResult::FileChange {
                            result: FileChangeResult {
                                tool_name: "filesystem.edit".to_owned(),
                                summary,
                                path: Some(request.path.display().to_string()),
                            },
                        }),
                    }
                },
            )
        }
        Err(error) => tool_json_error(&error),
    }
}

fn tool_exists(arguments: serde_json::Value, cwd: Option<&Path>) -> ToolInvocationResponse {
    match serde_json::from_value::<ExistsRequest>(arguments) {
        Ok(request) => ToolInvocationResponse {
            output: resolve_session_path(cwd, &request.path)
                .exists()
                .to_string(),
            is_error: false,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        },
        Err(error) => tool_json_error(&error),
    }
}

fn tool_list(
    arguments: serde_json::Value,
    cwd: Option<&Path>,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    events: ServiceEventEmitter,
    tool_call_id: &str,
) -> ToolInvocationResponse {
    json_tool_response(
        serde_json::from_value::<ListRequest>(arguments)
            .map(|mut request| {
                request.path = resolve_session_path(cwd, &request.path);
                request
            })
            .and_then(|request| {
                let mut progress = ProgressReporter::new(events, tool_call_id.to_string());
                progress.emit("list: scanning entries");
                list_directory(&request, cancellation.clone(), Some(&mut progress))
                    .map_err(serde_json::Error::io)
            }),
    )
}

fn tool_find(
    arguments: serde_json::Value,
    cwd: Option<&Path>,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    events: ServiceEventEmitter,
    tool_call_id: &str,
) -> ToolInvocationResponse {
    json_tool_response(
        serde_json::from_value::<FindRequest>(arguments)
            .map(|mut request| {
                request.path = resolve_session_path(cwd, &request.path);
                request
            })
            .and_then(|request| {
                let mut progress = ProgressReporter::new(events, tool_call_id.to_string());
                progress.emit(format!("find: searching for {}", request.pattern));
                find_paths_with_cancellation(&request, cancellation.clone(), Some(&mut progress))
                    .map_err(serde_json::Error::io)
            }),
    )
}

fn tool_grep(
    arguments: serde_json::Value,
    cwd: Option<&Path>,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    events: ServiceEventEmitter,
    tool_call_id: &str,
) -> ToolInvocationResponse {
    json_tool_response(
        serde_json::from_value::<GrepRequest>(arguments)
            .map(|mut request| {
                request.path = resolve_session_path(cwd, &request.path);
                request
            })
            .and_then(|request| {
                let mut progress = ProgressReporter::new(events, tool_call_id.to_string());
                progress.emit(format!("grep: searching for {}", request.pattern));
                grep_files_with_cancellation(&request, cancellation.clone(), Some(&mut progress))
                    .map_err(serde_json::Error::io)
            }),
    )
}

fn tool_stat(arguments: serde_json::Value, cwd: Option<&Path>) -> ToolInvocationResponse {
    json_tool_response(
        serde_json::from_value::<StatRequest>(arguments)
            .map(|mut request| {
                request.path = resolve_session_path(cwd, &request.path);
                request
            })
            .and_then(|request| stat_path(&request).map_err(serde_json::Error::io)),
    )
}

fn resolve_session_path(cwd: Option<&Path>, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.map_or_else(|| path.to_path_buf(), |cwd| cwd.join(path))
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

fn list_directory_service(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ListRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    list_directory(
        &request,
        bcode_plugin_sdk::ServiceCancellation::default(),
        None,
    )
    .map_or_else(
        |error| io_error(&error),
        |response| json_response(&response),
    )
}

fn find_paths_service(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<FindRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    find_paths(&request).map_or_else(
        |error| io_error(&error),
        |response| json_response(&response),
    )
}

fn grep_files_service(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<GrepRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    grep_files(&request).map_or_else(
        |error| io_error(&error),
        |response| json_response(&response),
    )
}

fn stat_path_service(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<StatRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    stat_path(&request).map_or_else(
        |error| io_error(&error),
        |response| json_response(&response),
    )
}

#[derive(Debug)]
struct SearchBudget {
    started: Instant,
    timeout: Duration,
    visited_entries: usize,
    timed_out: bool,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
}

impl SearchBudget {
    fn new(timeout_ms: Option<u64>, cancellation: bcode_plugin_sdk::ServiceCancellation) -> Self {
        Self {
            started: Instant::now(),
            timeout: Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_SEARCH_TIMEOUT_MS)),
            visited_entries: 0,
            timed_out: false,
            cancellation,
        }
    }

    fn check(&mut self) -> bool {
        if self.cancellation.is_cancelled() {
            self.timed_out = true;
            return false;
        }
        if self.started.elapsed() >= self.timeout {
            self.timed_out = true;
            return false;
        }
        true
    }

    fn visit(&mut self) -> bool {
        self.visited_entries = self.visited_entries.saturating_add(1);
        self.check()
    }

    fn elapsed_timeout_ms(&self) -> u64 {
        u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX)
    }
}

fn timeout_message(kind: &str, timeout_ms: u64) -> String {
    format!("{kind} timed out after {timeout_ms}ms; results are partial")
}

fn list_directory(
    request: &ListRequest,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
    mut progress: Option<&mut ProgressReporter>,
) -> Result<ListResponse, std::io::Error> {
    let mut budget = SearchBudget::new(request.timeout_ms, cancellation);
    let max_entries = request.max_entries.unwrap_or(DEFAULT_LIST_MAX_ENTRIES);
    let mut entries = Vec::new();
    collect_entries(
        &request.path,
        request.recursive,
        max_entries,
        &mut budget,
        &mut entries,
        progress.as_deref_mut(),
    )?;
    if let Some(progress) = progress {
        progress.emit(format!(
            "list: visited {} entries; retained {}",
            budget.visited_entries,
            entries.len()
        ));
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let partial = budget.timed_out || entries.len() >= max_entries;
    Ok(ListResponse {
        entries,
        backend: "rust".to_string(),
        timed_out: budget.timed_out,
        partial,
        visited_entries: budget.visited_entries,
        message: budget
            .timed_out
            .then(|| timeout_message("list", budget.elapsed_timeout_ms())),
    })
}

fn collect_entries(
    path: &Path,
    recursive: bool,
    max_entries: usize,
    budget: &mut SearchBudget,
    entries: &mut Vec<ListEntry>,
    mut progress: Option<&mut ProgressReporter>,
) -> Result<(), std::io::Error> {
    if entries.len() >= max_entries || !budget.check() {
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        if entries.len() >= max_entries || !budget.visit() {
            break;
        }
        if let Some(progress) = progress.as_deref_mut() {
            progress.maybe_visited("list", budget.visited_entries);
        }
        let entry = entry?;
        let entry_path = entry.path();
        let kind = path_kind(&entry_path)?;
        entries.push(ListEntry {
            path: entry_path.display().to_string(),
            kind,
        });
        if recursive && entry_path.is_dir() {
            collect_entries(
                &entry_path,
                recursive,
                max_entries,
                budget,
                entries,
                progress.as_deref_mut(),
            )?;
        }
    }
    Ok(())
}

fn find_paths(request: &FindRequest) -> Result<FindResponse, std::io::Error> {
    find_paths_with_cancellation(
        request,
        bcode_plugin_sdk::ServiceCancellation::default(),
        None,
    )
}

fn find_paths_with_cancellation(
    request: &FindRequest,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
    progress: Option<&mut ProgressReporter>,
) -> Result<FindResponse, std::io::Error> {
    let max_results = request.max_results.unwrap_or(DEFAULT_FIND_MAX_RESULTS);
    if let Some(response) = find_paths_with_fd(request, max_results)? {
        return Ok(response);
    }
    if let Some(response) = find_paths_with_find(request, max_results)? {
        return Ok(response);
    }
    find_paths_with_rust(request, max_results, cancellation, progress)
}

fn find_paths_with_rust(
    request: &FindRequest,
    max_results: usize,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
    mut progress: Option<&mut ProgressReporter>,
) -> Result<FindResponse, std::io::Error> {
    let mut budget = SearchBudget::new(request.timeout_ms, cancellation);
    let mut paths = Vec::new();
    collect_find_matches(
        &request.path,
        &request.path,
        &request.pattern,
        max_results,
        &mut budget,
        &mut paths,
        progress.as_deref_mut(),
    )?;
    if let Some(progress) = progress {
        progress.emit(format!(
            "find: visited {} entries; matched {}",
            budget.visited_entries,
            paths.len()
        ));
    }
    paths.sort();
    let partial = budget.timed_out || paths.len() >= max_results;
    Ok(FindResponse {
        paths,
        backend: "rust".to_string(),
        timed_out: budget.timed_out,
        partial,
        visited_entries: budget.visited_entries,
        message: budget
            .timed_out
            .then(|| timeout_message("find", budget.elapsed_timeout_ms())),
    })
}

fn collect_find_matches(
    root: &Path,
    path: &Path,
    pattern: &str,
    max_results: usize,
    budget: &mut SearchBudget,
    paths: &mut Vec<String>,
    mut progress: Option<&mut ProgressReporter>,
) -> Result<(), std::io::Error> {
    if paths.len() >= max_results || !budget.check() {
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        if paths.len() >= max_results || !budget.visit() {
            break;
        }
        if let Some(progress) = progress.as_deref_mut() {
            progress.maybe_visited("find", budget.visited_entries);
        }
        let entry = entry?;
        let entry_path = entry.path();
        let relative = entry_path.strip_prefix(root).unwrap_or(&entry_path);
        let relative = relative.to_string_lossy();
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if simple_glob_matches(pattern, &relative) || simple_glob_matches(pattern, &file_name) {
            paths.push(entry_path.display().to_string());
        }
        if entry_path.is_dir() {
            collect_find_matches(
                root,
                &entry_path,
                pattern,
                max_results,
                budget,
                paths,
                progress.as_deref_mut(),
            )?;
        }
    }
    Ok(())
}

fn grep_files(request: &GrepRequest) -> Result<GrepResponse, std::io::Error> {
    grep_files_with_cancellation(
        request,
        bcode_plugin_sdk::ServiceCancellation::default(),
        None,
    )
}

fn grep_files_with_cancellation(
    request: &GrepRequest,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
    progress: Option<&mut ProgressReporter>,
) -> Result<GrepResponse, std::io::Error> {
    let max_matches = request.max_matches.unwrap_or(DEFAULT_GREP_MAX_MATCHES);
    if let Some(response) = grep_files_with_rg(request, max_matches)? {
        return Ok(response);
    }
    grep_files_with_rust(request, max_matches, cancellation, progress)
}

fn grep_files_with_rust(
    request: &GrepRequest,
    max_matches: usize,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
    mut progress: Option<&mut ProgressReporter>,
) -> Result<GrepResponse, std::io::Error> {
    let mut budget = SearchBudget::new(request.timeout_ms, cancellation);
    let mut matches = Vec::new();
    let mut summary = ProgressSummary::default();
    collect_grep_matches(
        &request.path,
        request,
        max_matches,
        &mut budget,
        &mut matches,
        &mut summary,
        progress.as_deref_mut(),
    )?;
    if let Some(progress) = progress {
        progress.emit(format!(
            "grep: visited {} entries; matched {}; scanned {} bytes",
            budget.visited_entries, summary.matches, summary.bytes_scanned
        ));
    }
    let partial = budget.timed_out || matches.len() >= max_matches;
    Ok(GrepResponse {
        matches,
        backend: "rust".to_string(),
        timed_out: budget.timed_out,
        partial,
        visited_entries: budget.visited_entries,
        message: budget
            .timed_out
            .then(|| timeout_message("grep", budget.elapsed_timeout_ms())),
    })
}

fn collect_grep_matches(
    path: &Path,
    request: &GrepRequest,
    max_matches: usize,
    budget: &mut SearchBudget,
    matches: &mut Vec<GrepMatch>,
    summary: &mut ProgressSummary,
    mut progress: Option<&mut ProgressReporter>,
) -> Result<(), std::io::Error> {
    if matches.len() >= max_matches || !budget.check() {
        return Ok(());
    }
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            if matches.len() >= max_matches || !budget.visit() {
                break;
            }
            if let Some(progress) = progress.as_deref_mut() {
                progress.maybe_visited("grep", budget.visited_entries);
            }
            collect_grep_matches(
                &entry?.path(),
                request,
                max_matches,
                budget,
                matches,
                summary,
                progress.as_deref_mut(),
            )?;
        }
        return Ok(());
    }
    if !path.is_file() || !path_matches_optional_glob(path, request.glob.as_deref()) {
        return Ok(());
    }
    let metadata = std::fs::metadata(path)?;
    if metadata.len() > MAX_RUST_GREP_FILE_BYTES {
        return Ok(());
    }
    summary.bytes_scanned = summary.bytes_scanned.saturating_add(metadata.len());
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    let needle = if request.ignore_case {
        request.pattern.to_lowercase()
    } else {
        request.pattern.clone()
    };
    for (line_index, line) in contents.lines().enumerate() {
        if matches.len() >= max_matches || !budget.check() {
            break;
        }
        let haystack = if request.ignore_case {
            line.to_lowercase()
        } else {
            line.to_string()
        };
        if haystack.contains(&needle) {
            matches.push(GrepMatch {
                path: path.display().to_string(),
                line_number: line_index.saturating_add(1),
                line: line.to_string(),
            });
            summary.matches = summary.matches.saturating_add(1);
            if let Some(progress) = progress.as_deref_mut()
                && summary.matches.is_multiple_of(25)
            {
                progress.emit(format!("grep: matched {} lines", summary.matches));
            }
        }
    }
    Ok(())
}

struct ExternalCommandOutput {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    timed_out: bool,
}

fn grep_files_with_rg(
    request: &GrepRequest,
    max_matches: usize,
) -> Result<Option<GrepResponse>, std::io::Error> {
    if !command_exists("rg") {
        return Ok(None);
    }
    let mut command = Command::new("rg");
    configure_command_for_timeout(&mut command);
    command
        .arg("--json")
        .arg("--color")
        .arg("never")
        .arg("--fixed-strings")
        .arg("--line-number");
    if request.ignore_case {
        command.arg("--ignore-case");
    }
    if let Some(glob) = &request.glob {
        command.arg("--glob").arg(glob);
    }
    command.arg(&request.pattern).arg(&request.path);
    let output = run_external_command(command, request.timeout_ms)?;
    if output.exit_code == Some(127) {
        return Ok(None);
    }
    let mut matches = parse_rg_json_matches(&output.stdout, max_matches);
    let limit_reached = matches.len() >= max_matches;
    if matches.len() > max_matches {
        matches.truncate(max_matches);
    }
    let partial = output.timed_out || limit_reached;
    Ok(Some(GrepResponse {
        matches,
        backend: "rg".to_string(),
        timed_out: output.timed_out,
        partial,
        visited_entries: 0,
        message: external_message("grep", &output, partial, max_matches, request.timeout_ms),
    }))
}

fn parse_rg_json_matches(output: &str, max_matches: usize) -> Vec<GrepMatch> {
    let mut matches = Vec::new();
    for line in output.lines() {
        if matches.len() >= max_matches {
            break;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) != Some("match") {
            continue;
        }
        let Some(data) = value.get("data") else {
            continue;
        };
        let path = data
            .get("path")
            .and_then(|path| path.get("text"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let line_number = data
            .get("line_number")
            .and_then(serde_json::Value::as_u64)
            .and_then(|line| usize::try_from(line).ok())
            .unwrap_or_default();
        let line = data
            .get("lines")
            .and_then(|lines| lines.get("text"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim_end_matches(['\r', '\n'])
            .to_string();
        matches.push(GrepMatch {
            path,
            line_number,
            line,
        });
    }
    matches
}

fn find_paths_with_fd(
    request: &FindRequest,
    max_results: usize,
) -> Result<Option<FindResponse>, std::io::Error> {
    if !command_exists("fd") {
        return Ok(None);
    }
    let mut command = Command::new("fd");
    configure_command_for_timeout(&mut command);
    command
        .arg("--color")
        .arg("never")
        .arg("--glob")
        .arg("--max-results")
        .arg(max_results.to_string())
        .arg(&request.pattern)
        .arg(&request.path);
    let output = run_external_command(command, request.timeout_ms)?;
    let paths = output
        .stdout
        .lines()
        .take(max_results)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let partial = output.timed_out || paths.len() >= max_results;
    Ok(Some(FindResponse {
        paths,
        backend: "fd".to_string(),
        timed_out: output.timed_out,
        partial,
        visited_entries: 0,
        message: external_message("find", &output, partial, max_results, request.timeout_ms),
    }))
}

fn find_paths_with_find(
    request: &FindRequest,
    max_results: usize,
) -> Result<Option<FindResponse>, std::io::Error> {
    if !command_exists("find") {
        return Ok(None);
    }
    let mut command = Command::new("find");
    configure_command_for_timeout(&mut command);
    command
        .arg(&request.path)
        .arg("(")
        .arg("-name")
        .arg(&request.pattern)
        .arg("-o")
        .arg("-path")
        .arg(&request.pattern)
        .arg(")")
        .arg("-print");
    let output = run_external_command(command, request.timeout_ms)?;
    let mut paths = output
        .stdout
        .lines()
        .take(max_results)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    paths.sort();
    let partial = output.timed_out || paths.len() >= max_results;
    Ok(Some(FindResponse {
        paths,
        backend: "find".to_string(),
        timed_out: output.timed_out,
        partial,
        visited_entries: 0,
        message: external_message("find", &output, partial, max_results, request.timeout_ms),
    }))
}

fn external_message(
    kind: &str,
    output: &ExternalCommandOutput,
    partial: bool,
    max_results: usize,
    timeout_ms: Option<u64>,
) -> Option<String> {
    if output.timed_out {
        return Some(timeout_message(
            kind,
            timeout_ms.unwrap_or(DEFAULT_SEARCH_TIMEOUT_MS),
        ));
    }
    if partial {
        return Some(format!(
            "{kind} stopped after reaching limit {max_results}; results are partial"
        ));
    }
    if !output.stderr.trim().is_empty() && !matches!(output.exit_code, Some(0 | 1)) {
        return Some(output.stderr.trim().to_string());
    }
    None
}

fn command_exists(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn run_external_command(
    mut command: Command,
    timeout_ms: Option<u64>,
) -> Result<ExternalCommandOutput, std::io::Error> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("failed to capture stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("failed to capture stderr"))?;
    let stdout_reader = std::thread::spawn(move || read_limited(stdout, MAX_EXTERNAL_OUTPUT_BYTES));
    let stderr_reader = std::thread::spawn(move || read_limited(stderr, MAX_EXTERNAL_OUTPUT_BYTES));
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_SEARCH_TIMEOUT_MS));
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            break terminate_child_after_timeout(&mut child)?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(ExternalCommandOutput {
        stdout: join_reader(stdout_reader)?,
        stderr: join_reader(stderr_reader)?,
        exit_code: status.code(),
        timed_out,
    })
}

#[cfg(unix)]
fn configure_command_for_timeout(command: &mut Command) {
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_command_for_timeout(_command: &mut Command) {}

#[cfg(unix)]
fn terminate_child_after_timeout(child: &mut Child) -> Result<ExitStatus, std::io::Error> {
    let process_group_id = i32::try_from(child.id()).map_err(std::io::Error::other)?;
    send_signal_to_process_group(process_group_id, SIGTERM)?;
    let grace_started = Instant::now();
    let mut status = None;
    loop {
        if status.is_none() {
            status = child.try_wait()?;
        }
        if status.is_some()
            || grace_started.elapsed() >= Duration::from_millis(TERMINATION_GRACE_MS)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    send_signal_to_process_group(process_group_id, SIGKILL)?;
    status.map_or_else(|| child.wait(), Ok)
}

#[cfg(not(unix))]
fn terminate_child_after_timeout(child: &mut Child) -> Result<ExitStatus, std::io::Error> {
    child.kill()?;
    child.wait()
}

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;
#[cfg(unix)]
const ESRCH: i32 = 3;

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
fn send_signal_to_process_group(process_group_id: i32, signal: i32) -> Result<(), std::io::Error> {
    let target = process_group_id
        .checked_neg()
        .ok_or_else(|| std::io::Error::other("process group id cannot be negated"))?;
    // SAFETY: `kill` targets the process group created by `CommandExt::process_group(0)`.
    let result = unsafe { kill(target, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ESRCH) {
        return Ok(());
    }
    Err(error)
}

fn read_limited<R>(mut reader: R, max_bytes: usize) -> Result<String, std::io::Error>
where
    R: Read,
{
    let mut bytes = Vec::new();
    let limit = u64::try_from(max_bytes).map_err(std::io::Error::other)?;
    reader.by_ref().take(limit).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn join_reader(
    handle: std::thread::JoinHandle<Result<String, std::io::Error>>,
) -> Result<String, std::io::Error> {
    handle
        .join()
        .map_err(|_| std::io::Error::other("output reader thread panicked"))?
}

fn metadata_kind(metadata: &std::fs::Metadata) -> String {
    if metadata.is_dir() {
        "directory".to_string()
    } else if metadata.is_file() {
        "file".to_string()
    } else {
        "other".to_string()
    }
}

fn stat_path(request: &StatRequest) -> Result<StatResponse, std::io::Error> {
    match std::fs::metadata(&request.path) {
        Ok(metadata) => Ok(StatResponse {
            exists: true,
            kind: metadata_kind(&metadata),
            len: metadata.is_file().then_some(metadata.len()),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(StatResponse {
            exists: false,
            kind: "missing".to_string(),
            len: None,
        }),
        Err(error) => Err(error),
    }
}

fn path_kind(path: &Path) -> Result<String, std::io::Error> {
    let metadata = std::fs::metadata(path)?;
    Ok(if metadata.is_dir() {
        "directory".to_string()
    } else if metadata.is_file() {
        "file".to_string()
    } else {
        "other".to_string()
    })
}

fn path_matches_optional_glob(path: &Path, glob: Option<&str>) -> bool {
    glob.is_none_or(|glob| {
        path.file_name()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|file_name| simple_glob_matches(glob, file_name))
            || simple_glob_matches(glob, &path.to_string_lossy())
    })
}

fn simple_glob_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let mut remainder = value;
    let mut first = true;
    for part in pattern.split('*') {
        if part.is_empty() {
            continue;
        }
        if first && !pattern.starts_with('*') {
            let Some(next) = remainder.strip_prefix(part) else {
                return false;
            };
            remainder = next;
        } else if let Some(index) = remainder.find(part) {
            remainder = &remainder[index + part.len()..];
        } else {
            return false;
        }
        first = false;
    }
    pattern.ends_with('*') || remainder.is_empty()
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
        content: Vec::new(),
        full_output: None,
        host_action: None,
        result: None,
    }
}

fn tool_json_error(error: &serde_json::Error) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output: error.to_string(),
        is_error: true,
        content: Vec::new(),
        full_output: None,
        host_action: None,
        result: None,
    }
}

fn json_tool_response<T>(result: Result<T, serde_json::Error>) -> ToolInvocationResponse
where
    T: Serialize,
{
    match result.and_then(|value| serde_json::to_string_pretty(&value)) {
        Ok(output) => ToolInvocationResponse {
            output,
            is_error: false,
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
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(FilesystemPlugin, include_str!("../bcode-plugin.toml"))
}

bcode_plugin_sdk::export_plugin!(FilesystemPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bcode-filesystem-plugin-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn write_and_edit_responses_include_file_change_result() {
        let root = temp_dir("presentation");
        let write_response = tool_write(
            serde_json::json!({
                "path": root.join("file.txt"),
                "contents": "hello",
            }),
            None,
            None,
            "test",
        );
        assert!(matches!(
            write_response.result,
            Some(ToolInvocationResult::FileChange {
                result: FileChangeResult {
                    tool_name,
                    summary,
                    path: Some(_),
                },
            }) if tool_name == "filesystem.write" && summary == "wrote 5 bytes"
        ));

        let edit_response = tool_edit(
            serde_json::json!({
                "path": root.join("file.txt"),
                "old_text": "hello",
                "new_text": "hello world",
            }),
            None,
            None,
            "test",
        );
        assert!(matches!(
            edit_response.result,
            Some(ToolInvocationResult::FileChange {
                result: FileChangeResult {
                    tool_name,
                    summary,
                    path: Some(_),
                },
            }) if tool_name == "filesystem.edit" && summary == "applied 1 replacement"
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rust_grep_enforces_timeout() {
        let root = temp_dir("grep-timeout");
        std::fs::write(root.join("file.txt"), "needle\n").expect("write file");

        let response = grep_files_with_rust(
            &GrepRequest {
                path: root.clone(),
                pattern: "needle".to_string(),
                glob: None,
                ignore_case: false,
                max_matches: Some(10),
                timeout_ms: Some(0),
            },
            10,
            bcode_plugin_sdk::ServiceCancellation::default(),
            None,
        )
        .expect("grep response");

        assert_eq!(response.backend, "rust");
        assert!(response.timed_out);
        assert!(response.partial);
        assert!(
            response
                .message
                .as_deref()
                .is_some_and(|message| message.contains("timed out"))
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rust_find_uses_default_result_limit() {
        let root = temp_dir("find-limit");
        for index in 0..3 {
            std::fs::write(root.join(format!("file-{index}.txt")), "x").expect("write file");
        }

        let response = find_paths_with_rust(
            &FindRequest {
                path: root.clone(),
                pattern: "*.txt".to_string(),
                max_results: Some(1),
                timeout_ms: Some(30_000),
            },
            1,
            bcode_plugin_sdk::ServiceCancellation::default(),
            None,
        )
        .expect("find response");

        assert_eq!(response.backend, "rust");
        assert_eq!(response.paths.len(), 1);
        assert!(response.partial);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn read_text_supports_offset_and_limit() {
        let root = temp_dir("read-offset-limit");
        let file = root.join("file.txt");
        std::fs::write(&file, "one\ntwo\nthree\nfour\n").expect("write file");

        let response = read_path_for_tool(
            &file,
            &ReadRequest {
                path: file.clone(),
                offset: Some(2),
                limit: Some(2),
            },
        );

        assert!(!response.is_error);
        assert!(response.output.starts_with("two\nthree"));
        assert!(response.output.contains("Use offset=4 to continue"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn artifact_read_can_return_tail_without_full_read() {
        let root = temp_dir("artifact-read-tail");
        let file = root.join("artifact.txt");
        std::fs::write(&file, "head-middle-tail").expect("write artifact");

        let response = read_artifact(
            &file,
            &ArtifactReadRequest {
                path: file.clone(),
                offset_bytes: None,
                max_bytes: Some(4),
                from_end: true,
            },
        )
        .expect("read artifact tail");

        assert_eq!(response.contents, "tail");
        assert_eq!(response.total_bytes, 16);
        assert!(response.truncated);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn artifact_grep_searches_literal_matches() {
        let root = temp_dir("artifact-grep");
        let file = root.join("artifact.txt");
        std::fs::write(&file, "alpha\nbeta\nALPHA\n").expect("write artifact");

        let response = grep_artifact(
            &file,
            &ArtifactGrepRequest {
                path: file.clone(),
                pattern: "alpha".to_string(),
                ignore_case: true,
                max_matches: Some(10),
            },
        )
        .expect("grep artifact");

        assert_eq!(response.matches.len(), 2);
        assert_eq!(response.matches[0].line_number, 1);
        assert_eq!(response.matches[1].line_number, 3);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn read_image_returns_structured_image_content() {
        let root = temp_dir("read-image");
        let file = root.join("image.png");
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("write header");
            writer
                .write_image_data(&[255, 0, 0, 255])
                .expect("write image");
        }
        std::fs::write(&file, bytes).expect("write image file");

        let response = read_path_for_tool(
            &file,
            &ReadRequest {
                path: file.clone(),
                offset: None,
                limit: None,
            },
        );

        assert!(!response.is_error);
        assert!(response.output.contains("Read image file [image/png]"));
        assert_eq!(response.content.len(), 1);
        let ToolResultContent::ImageRef { image } = &response.content[0] else {
            panic!("expected image reference content");
        };
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.path, file.display().to_string());
        assert_eq!(image.metadata.width, Some(1));
        assert_eq!(image.metadata.height, Some(1));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn external_command_timeout_kills_process_group() {
        let mut command = Command::new("sh");
        configure_command_for_timeout(&mut command);
        command
            .arg("-c")
            .arg("sh -c 'trap \"\" HUP TERM; sleep 5' | cat");
        let started = Instant::now();

        let output = run_external_command(command, Some(100)).expect("external output");

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(output.timed_out);
    }
}
