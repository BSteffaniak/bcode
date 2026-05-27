#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled filesystem service plugin for Bcode.

use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ImageMetadata, ImageRefContent, ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS,
    TOOL_SERVICE_INTERFACE_ID, ToolDefinition, ToolInvocationRequest, ToolInvocationResponse,
    ToolList, ToolResultContent, ToolSideEffect,
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

/// Bundled filesystem plugin.
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
        ],
    })
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
        });
    }
    let cwd = request.cwd.clone();
    let response = match request.name.as_str() {
        "filesystem.read" => tool_read(request.arguments, cwd.as_deref()),
        "filesystem.write" => tool_write(request.arguments, cwd.as_deref()),
        "filesystem.edit" => tool_edit(request.arguments, cwd.as_deref()),
        "filesystem.exists" => tool_exists(request.arguments, cwd.as_deref()),
        "filesystem.list" => tool_list(request.arguments, cwd.as_deref(), &context.cancellation),
        "filesystem.find" => tool_find(request.arguments, cwd.as_deref(), &context.cancellation),
        "filesystem.grep" => tool_grep(request.arguments, cwd.as_deref(), &context.cancellation),
        "filesystem.stat" => tool_stat(request.arguments, cwd.as_deref()),
        _ => ToolInvocationResponse {
            output: format!("unknown filesystem tool: {}", request.name),
            is_error: true,
            content: Vec::new(),
            full_output: None,
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
    }
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

fn tool_write(arguments: serde_json::Value, cwd: Option<&Path>) -> ToolInvocationResponse {
    match serde_json::from_value::<WriteRequest>(arguments) {
        Ok(mut request) => {
            request.path = resolve_session_path(cwd, &request.path);
            write_file_inner(&request.path, &request.contents).map_or_else(
                |error| tool_io_error(&error),
                |bytes_written| ToolInvocationResponse {
                    output: format!("wrote {bytes_written} bytes"),
                    is_error: false,
                    content: Vec::new(),
                    full_output: None,
                },
            )
        }
        Err(error) => tool_json_error(&error),
    }
}

fn tool_edit(arguments: serde_json::Value, cwd: Option<&Path>) -> ToolInvocationResponse {
    match serde_json::from_value::<EditRequest>(arguments) {
        Ok(mut request) => {
            request.path = resolve_session_path(cwd, &request.path);
            edit_file_inner(&request).map_or_else(
                |error| ToolInvocationResponse {
                    output: error,
                    is_error: true,
                    content: Vec::new(),
                    full_output: None,
                },
                |replacements| ToolInvocationResponse {
                    output: format!("applied {replacements} replacement"),
                    is_error: false,
                    content: Vec::new(),
                    full_output: None,
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
        },
        Err(error) => tool_json_error(&error),
    }
}

fn tool_list(
    arguments: serde_json::Value,
    cwd: Option<&Path>,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
) -> ToolInvocationResponse {
    json_tool_response(
        serde_json::from_value::<ListRequest>(arguments)
            .map(|mut request| {
                request.path = resolve_session_path(cwd, &request.path);
                request
            })
            .and_then(|request| {
                list_directory(&request, cancellation.clone()).map_err(serde_json::Error::io)
            }),
    )
}

fn tool_find(
    arguments: serde_json::Value,
    cwd: Option<&Path>,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
) -> ToolInvocationResponse {
    json_tool_response(
        serde_json::from_value::<FindRequest>(arguments)
            .map(|mut request| {
                request.path = resolve_session_path(cwd, &request.path);
                request
            })
            .and_then(|request| {
                find_paths_with_cancellation(&request, cancellation.clone())
                    .map_err(serde_json::Error::io)
            }),
    )
}

fn tool_grep(
    arguments: serde_json::Value,
    cwd: Option<&Path>,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
) -> ToolInvocationResponse {
    json_tool_response(
        serde_json::from_value::<GrepRequest>(arguments)
            .map(|mut request| {
                request.path = resolve_session_path(cwd, &request.path);
                request
            })
            .and_then(|request| {
                grep_files_with_cancellation(&request, cancellation.clone())
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
    list_directory(&request, bcode_plugin_sdk::ServiceCancellation::default()).map_or_else(
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
    )?;
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
) -> Result<(), std::io::Error> {
    if entries.len() >= max_entries || !budget.check() {
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        if entries.len() >= max_entries || !budget.visit() {
            break;
        }
        let entry = entry?;
        let entry_path = entry.path();
        let kind = path_kind(&entry_path)?;
        entries.push(ListEntry {
            path: entry_path.display().to_string(),
            kind,
        });
        if recursive && entry_path.is_dir() {
            collect_entries(&entry_path, recursive, max_entries, budget, entries)?;
        }
    }
    Ok(())
}

fn find_paths(request: &FindRequest) -> Result<FindResponse, std::io::Error> {
    find_paths_with_cancellation(request, bcode_plugin_sdk::ServiceCancellation::default())
}

fn find_paths_with_cancellation(
    request: &FindRequest,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
) -> Result<FindResponse, std::io::Error> {
    let max_results = request.max_results.unwrap_or(DEFAULT_FIND_MAX_RESULTS);
    if let Some(response) = find_paths_with_fd(request, max_results)? {
        return Ok(response);
    }
    if let Some(response) = find_paths_with_find(request, max_results)? {
        return Ok(response);
    }
    find_paths_with_rust(request, max_results, cancellation)
}

fn find_paths_with_rust(
    request: &FindRequest,
    max_results: usize,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
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
    )?;
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
) -> Result<(), std::io::Error> {
    if paths.len() >= max_results || !budget.check() {
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        if paths.len() >= max_results || !budget.visit() {
            break;
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
            collect_find_matches(root, &entry_path, pattern, max_results, budget, paths)?;
        }
    }
    Ok(())
}

fn grep_files(request: &GrepRequest) -> Result<GrepResponse, std::io::Error> {
    grep_files_with_cancellation(request, bcode_plugin_sdk::ServiceCancellation::default())
}

fn grep_files_with_cancellation(
    request: &GrepRequest,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
) -> Result<GrepResponse, std::io::Error> {
    let max_matches = request.max_matches.unwrap_or(DEFAULT_GREP_MAX_MATCHES);
    if let Some(response) = grep_files_with_rg(request, max_matches)? {
        return Ok(response);
    }
    grep_files_with_rust(request, max_matches, cancellation)
}

fn grep_files_with_rust(
    request: &GrepRequest,
    max_matches: usize,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
) -> Result<GrepResponse, std::io::Error> {
    let mut budget = SearchBudget::new(request.timeout_ms, cancellation);
    let mut matches = Vec::new();
    collect_grep_matches(
        &request.path,
        request,
        max_matches,
        &mut budget,
        &mut matches,
    )?;
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
) -> Result<(), std::io::Error> {
    if matches.len() >= max_matches || !budget.check() {
        return Ok(());
    }
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            if matches.len() >= max_matches || !budget.visit() {
                break;
            }
            collect_grep_matches(&entry?.path(), request, max_matches, budget, matches)?;
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

fn stat_path(request: &StatRequest) -> Result<StatResponse, std::io::Error> {
    match std::fs::metadata(&request.path) {
        Ok(metadata) => Ok(StatResponse {
            exists: true,
            kind: if metadata.is_dir() {
                "directory".to_string()
            } else if metadata.is_file() {
                "file".to_string()
            } else {
                "other".to_string()
            },
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
    }
}

fn tool_json_error(error: &serde_json::Error) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output: error.to_string(),
        is_error: true,
        content: Vec::new(),
        full_output: None,
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
        },
        Err(error) => ToolInvocationResponse {
            output: error.to_string(),
            is_error: true,
            content: Vec::new(),
            full_output: None,
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
