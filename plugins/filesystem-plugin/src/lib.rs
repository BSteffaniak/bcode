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
use std::path::{Path, PathBuf};

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

#[derive(Debug, Deserialize)]
struct ListRequest {
    path: PathBuf,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    max_entries: Option<usize>,
}

#[derive(Debug, Serialize)]
struct ListEntry {
    path: String,
    kind: String,
}

#[derive(Debug, Serialize)]
struct ListResponse {
    entries: Vec<ListEntry>,
}

#[derive(Debug, Deserialize)]
struct FindRequest {
    path: PathBuf,
    pattern: String,
    #[serde(default)]
    max_results: Option<usize>,
}

#[derive(Debug, Serialize)]
struct FindResponse {
    paths: Vec<String>,
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

fn invoke_filesystem_service(request: &ServiceRequest) -> ServiceResponse {
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
        description: "Read a UTF-8 text file".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["path"],
            "properties": { "path": { "type": "string" } }
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
                "max_entries": { "type": "integer", "minimum": 1 }
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
                "max_results": { "type": "integer", "minimum": 1 }
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
                "max_matches": { "type": "integer", "minimum": 1 }
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
        "filesystem.list" => tool_list(request.arguments),
        "filesystem.find" => tool_find(request.arguments),
        "filesystem.grep" => tool_grep(request.arguments),
        "filesystem.stat" => tool_stat(request.arguments),
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

fn tool_list(arguments: serde_json::Value) -> ToolInvocationResponse {
    json_tool_response(
        serde_json::from_value::<ListRequest>(arguments)
            .and_then(|request| list_directory(&request).map_err(serde_json::Error::io)),
    )
}

fn tool_find(arguments: serde_json::Value) -> ToolInvocationResponse {
    json_tool_response(
        serde_json::from_value::<FindRequest>(arguments)
            .and_then(|request| find_paths(&request).map_err(serde_json::Error::io)),
    )
}

fn tool_grep(arguments: serde_json::Value) -> ToolInvocationResponse {
    json_tool_response(
        serde_json::from_value::<GrepRequest>(arguments)
            .and_then(|request| grep_files(&request).map_err(serde_json::Error::io)),
    )
}

fn tool_stat(arguments: serde_json::Value) -> ToolInvocationResponse {
    json_tool_response(
        serde_json::from_value::<StatRequest>(arguments)
            .and_then(|request| stat_path(&request).map_err(serde_json::Error::io)),
    )
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
    list_directory(&request).map_or_else(
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

fn list_directory(request: &ListRequest) -> Result<ListResponse, std::io::Error> {
    let mut entries = Vec::new();
    collect_entries(
        &request.path,
        request.recursive,
        request.max_entries,
        &mut entries,
    )?;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(ListResponse { entries })
}

fn collect_entries(
    path: &Path,
    recursive: bool,
    max_entries: Option<usize>,
    entries: &mut Vec<ListEntry>,
) -> Result<(), std::io::Error> {
    if max_entries.is_some_and(|max| entries.len() >= max) {
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        let kind = path_kind(&entry_path)?;
        entries.push(ListEntry {
            path: entry_path.display().to_string(),
            kind,
        });
        if recursive && entry_path.is_dir() {
            collect_entries(&entry_path, recursive, max_entries, entries)?;
        }
        if max_entries.is_some_and(|max| entries.len() >= max) {
            break;
        }
    }
    Ok(())
}

fn find_paths(request: &FindRequest) -> Result<FindResponse, std::io::Error> {
    let mut paths = Vec::new();
    collect_find_matches(
        &request.path,
        &request.path,
        &request.pattern,
        request.max_results,
        &mut paths,
    )?;
    paths.sort();
    Ok(FindResponse { paths })
}

fn collect_find_matches(
    root: &Path,
    path: &Path,
    pattern: &str,
    max_results: Option<usize>,
    paths: &mut Vec<String>,
) -> Result<(), std::io::Error> {
    if max_results.is_some_and(|max| paths.len() >= max) {
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        let relative = entry_path.strip_prefix(root).unwrap_or(&entry_path);
        let relative = relative.to_string_lossy();
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if simple_glob_matches(pattern, &relative) || simple_glob_matches(pattern, &file_name) {
            paths.push(entry_path.display().to_string());
        }
        if entry_path.is_dir() {
            collect_find_matches(root, &entry_path, pattern, max_results, paths)?;
        }
        if max_results.is_some_and(|max| paths.len() >= max) {
            break;
        }
    }
    Ok(())
}

fn grep_files(request: &GrepRequest) -> Result<GrepResponse, std::io::Error> {
    let mut matches = Vec::new();
    collect_grep_matches(&request.path, request, &mut matches)?;
    Ok(GrepResponse { matches })
}

fn collect_grep_matches(
    path: &Path,
    request: &GrepRequest,
    matches: &mut Vec<GrepMatch>,
) -> Result<(), std::io::Error> {
    if request.max_matches.is_some_and(|max| matches.len() >= max) {
        return Ok(());
    }
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            collect_grep_matches(&entry?.path(), request, matches)?;
            if request.max_matches.is_some_and(|max| matches.len() >= max) {
                break;
            }
        }
        return Ok(());
    }
    if !path.is_file() || !path_matches_optional_glob(path, request.glob.as_deref()) {
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
            if request.max_matches.is_some_and(|max| matches.len() >= max) {
                break;
            }
        }
    }
    Ok(())
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
    }
}

fn tool_json_error(error: &serde_json::Error) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output: error.to_string(),
        is_error: true,
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
        },
        Err(error) => ToolInvocationResponse {
            output: error.to_string(),
            is_error: true,
        },
    }
}

bcode_plugin_sdk::export_plugin!(FilesystemPlugin, include_str!("../bcode-plugin.toml"));
