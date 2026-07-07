#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! shell execution tool plugin for Bcode.

#[cfg(feature = "static-bundled")]
mod shell_run_tui;
mod terminal_clean;

use bcode_config::{
    ShellToolConfig, ShellToolEnvAutoFallback, ShellToolEnvConfig, ShellToolEnvMode,
    default_config_paths_from_with_environment, load_config_from_paths_with_environment,
};
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolArtifact,
    ToolArtifactRef, ToolDefinition, ToolInvocationRequest, ToolInvocationResponse,
    ToolInvocationResult, ToolInvocationStreamEvent, ToolList, ToolOutputStream,
    ToolPluginVisualMetadata, ToolSideEffect, ToolStreamVisualUpdate,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum ShellRunResult {
    Terminal {
        exit_code: Option<i32>,
        timed_out: bool,
        cancelled: bool,
        #[serde(default)]
        duration_ms: Option<u64>,
        output_tail: String,
        output_truncated: bool,
        output_bytes: Option<u64>,
        retained_output_bytes: Option<u64>,
        columns: u16,
        rows: u16,
    },
    Captured {
        exit_code: Option<i32>,
        timed_out: bool,
        cancelled: bool,
        #[serde(default)]
        duration_ms: Option<u64>,
        stdout: String,
        stderr: String,
        stdout_truncated: bool,
        stderr_truncated: bool,
        stdout_bytes: Option<u64>,
        stderr_bytes: Option<u64>,
    },
}

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_TERMINAL_COLUMNS: u16 = 120;
const DEFAULT_TERMINAL_ROWS: u16 = 30;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024;
const MAX_INLINE_TERMINAL_OUTPUT_BYTES: usize = 16 * 1024;
const TERMINAL_PTY_STREAM_REF_KEY: &str = "terminal_pty_stream";
const TERMINAL_PTY_STREAM_CONTENT_TYPE: &str =
    "application/x-bcode-terminal-pty-stream; charset=utf-8";

/// shell plugin.
#[derive(Default)]
pub struct ShellPlugin;

impl ConcurrentRustPlugin for ShellPlugin {
    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
        invoke_shell_service(&context)
    }
}

impl RustPlugin for ShellPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        invoke_shell_service(&context)
    }
}

fn invoke_shell_service(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.interface_id != TOOL_SERVICE_INTERFACE_ID {
        return ServiceResponse::error(
            "unsupported_interface",
            "unsupported shell plugin service interface",
        );
    }

    match context.request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(&context.request),
        OP_INVOKE_TOOL => invoke_tool(context),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported tool service operation",
        ),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ShellRunArguments {
    command: String,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    columns: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
}

impl ShellRunArguments {
    const fn terminal_columns(&self) -> u16 {
        match self.columns {
            Some(columns) if columns > 0 => columns,
            _ => DEFAULT_TERMINAL_COLUMNS,
        }
    }

    const fn terminal_rows(&self) -> u16 {
        match self.rows {
            Some(rows) if rows > 0 => rows,
            _ => DEFAULT_TERMINAL_ROWS,
        }
    }
}

fn list_tools(request: &ServiceRequest) -> ServiceResponse {
    if let Err(error) = request.payload_json::<ListToolsRequest>() {
        return invalid_request(&error);
    }
    json_response(&ToolList {
        tools: vec![ToolDefinition {
            name: "shell.run".to_string(),
            description: "Run a shell command in pseudo-terminal mode so output streams live with human-like CLI colors and formatting.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 1 },
                    "columns": { "type": "integer", "minimum": 1 },
                    "rows": { "type": "integer", "minimum": 1 }
                }
            }),
            side_effect: ToolSideEffect::ExecuteProcess,
            requires_permission: true,
            policy: bcode_tool::ToolPolicyMetadata {
                aliases: Vec::new(),
                compatibility_aliases: vec![bcode_tool::ToolCompatibilityAlias::new("claude", "Bash")],
                capabilities: vec!["shell.run".to_string(), "process.execute".to_string()],
                permission_category: Some("command".to_string()),
                argument_extractors: vec![bcode_tool::ToolArgumentExtractor {
                    kind: bcode_tool::ToolArgumentKind::Command,
                    argument: "command".to_string(),
                }],
            },
            ui: bcode_tool::ToolUiMetadata {
                activity_label: Some("running".to_string()),
                request_visual: Some(ToolPluginVisualMetadata {
                    producer_plugin_id: Some("bcode.shell".to_string()),
                    schema: "bcode.tool.request.shell.run".to_string(),
                    schema_version: 1,
                    title: Some("Shell command".to_string()),
                    subtitle: Some("shell command · {bytes}".to_string()),
                    payload: BTreeMap::from([
                        (
                            "command".to_string(),
                            bcode_tool::ToolVisualPayloadSelector {
                                fields: vec!["command".to_string()],
                                literal: None,
                                required: true,
                            },
                        ),
                        (
                            "cwd".to_string(),
                            bcode_tool::ToolVisualPayloadSelector {
                                fields: vec!["cwd".to_string()],
                                literal: None,
                                required: false,
                            },
                        ),
                    ]),
                }),

            },
        }],
    })
}

fn invoke_tool(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<ToolInvocationRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let response = match request.name.as_str() {
        "shell.run" => run_shell_tool(
            context,
            context.events,
            &request.tool_call_id,
            request.name.as_str(),
            request.arguments,
            request.cwd.as_deref(),
            TerminalRunPaths {
                session_cwd: request.cwd.as_deref(),
                artifact_dir: request.artifact_dir.as_deref(),
                cancellation_path: request.cancellation_path.as_deref(),
            },
        ),
        _ => ToolInvocationResponse {
            output: format!("unknown shell tool: {}", request.name),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        },
    };
    json_response(&response)
}

fn run_shell_tool(
    context: &NativeServiceContext,
    events: ServiceEventEmitter,
    tool_call_id: &str,
    tool_name: &str,
    arguments: serde_json::Value,
    session_cwd: Option<&std::path::Path>,
    paths: TerminalRunPaths<'_>,
) -> ToolInvocationResponse {
    let arguments = match serde_json::from_value::<ShellRunArguments>(arguments) {
        Ok(arguments) => arguments,
        Err(error) => {
            return ToolInvocationResponse {
                output: error.to_string(),
                is_error: true,
                content: Vec::new(),
                full_output: None,
                host_action: None,
                result: None,
            };
        }
    };
    if arguments.command.trim().is_empty() {
        return ToolInvocationResponse {
            output: "command must not be empty".to_string(),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        };
    }
    let arguments_json = serde_json::to_value(&arguments).unwrap_or_else(|_| json!({}));
    let now_ms = current_unix_millis();
    emit_tool_stream_event(
        events,
        &ToolInvocationStreamEvent::Started {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            sequence: 0,
            terminal: true,
            columns: Some(arguments.terminal_columns()),
            rows: Some(arguments.terminal_rows()),
            started_at_ms: Some(now_ms),
        },
    );
    emit_tool_status(
        events,
        tool_call_id,
        0,
        format!("starting command: {}", arguments.command),
    );
    let response = run_terminal_shell_command(
        events,
        &context.cancellation,
        tool_call_id,
        &arguments,
        arguments_json,
        TerminalRunPaths {
            session_cwd,
            ..paths
        },
    );
    emit_tool_stream_event(
        events,
        &ToolInvocationStreamEvent::Finished {
            tool_call_id: tool_call_id.to_owned(),
            sequence: 0,
            is_error: response.is_error,
            finished_at_ms: Some(current_unix_millis()),
        },
    );
    response
}

#[derive(Debug, Serialize)]
struct TerminalCommandOutput {
    mode: &'static str,
    exit_code: Option<i32>,
    timed_out: bool,
    cancelled: bool,
    command: String,
    cwd: Option<String>,
    output: String,
    output_truncated: bool,
    output_bytes: u64,
    retained_output_bytes: u64,
    columns: u16,
    rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LimitedOutput {
    text: String,
    original_bytes: usize,
    retained_bytes: usize,
    truncated: bool,
}

#[derive(Debug, Clone)]
struct TerminalStreamOutput {
    raw: LimitedOutput,
    clean: LimitedOutput,
    raw_artifact_path: Option<PathBuf>,
    clean_artifact_path: Option<PathBuf>,
}

fn resolve_effective_cwd(
    arguments: &ShellRunArguments,
    session_cwd: Option<&Path>,
) -> Option<PathBuf> {
    arguments.cwd.as_deref().map_or_else(
        || session_cwd.map(Path::to_path_buf),
        |cwd| {
            if cwd.is_absolute() {
                Some(cwd.to_path_buf())
            } else {
                session_cwd
                    .map(|base| base.join(cwd))
                    .or_else(|| Some(cwd.to_path_buf()))
            }
        },
    )
}

fn shell_config_with_environment(
    cwd: Option<&Path>,
    environment: &impl bcode_config::ConfigEnvironment,
) -> Result<ShellToolConfig, String> {
    let paths = cwd.map_or_else(
        || bcode_config::default_config_paths_with_environment(environment),
        |cwd| default_config_paths_from_with_environment(cwd, environment),
    );
    load_config_from_paths_with_environment(&paths, environment)
        .map(|config| config.tools.shell)
        .map_err(|error| error.to_string())
}

fn direnv_file_for(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd.to_path_buf();
    loop {
        let envrc = current.join(".envrc");
        if envrc.exists() {
            return Some(envrc);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn direnv_available() -> bool {
    Command::new("direnv")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn should_use_direnv(cwd: Option<&Path>, config: ShellToolEnvConfig) -> Result<bool, String> {
    match config.mode {
        ShellToolEnvMode::Inherit => Ok(false),
        ShellToolEnvMode::Direnv => {
            if direnv_available() {
                Ok(true)
            } else {
                Err("shell env mode is direnv, but `direnv` is not available on PATH".to_owned())
            }
        }
        ShellToolEnvMode::Auto => {
            let Some(cwd) = cwd else {
                return Ok(false);
            };
            let Some(envrc) = direnv_file_for(cwd) else {
                return Ok(false);
            };
            if direnv_available() {
                Ok(true)
            } else if config.auto_fallback == ShellToolEnvAutoFallback::Inherit {
                Ok(false)
            } else {
                Err(format!(
                    "found {}, but `direnv` is not available on PATH; install direnv or set `[tools.shell.env] auto_fallback = \"inherit\"`",
                    envrc.display()
                ))
            }
        }
    }
}

fn shell_program_and_args(
    command: &str,
    cwd: Option<&Path>,
    env_config: ShellToolEnvConfig,
) -> Result<(String, Vec<String>), String> {
    if should_use_direnv(cwd, env_config)? {
        let cwd = cwd.ok_or_else(|| "direnv shell mode requires a working directory".to_owned())?;
        Ok((
            "direnv".to_owned(),
            vec![
                "exec".to_owned(),
                cwd.display().to_string(),
                shell_program().to_owned(),
                "-o".to_owned(),
                "pipefail".to_owned(),
                "-c".to_owned(),
                command.to_owned(),
            ],
        ))
    } else {
        Ok((shell_program().to_owned(), shell_args(command)))
    }
}

#[derive(Debug, Clone, Copy)]
struct TerminalRunPaths<'a> {
    session_cwd: Option<&'a Path>,
    artifact_dir: Option<&'a Path>,
    cancellation_path: Option<&'a Path>,
}

fn run_terminal_shell_command(
    events: ServiceEventEmitter,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    tool_call_id: &str,
    arguments: &ShellRunArguments,
    arguments_json: serde_json::Value,
    paths: TerminalRunPaths<'_>,
) -> ToolInvocationResponse {
    run_terminal_shell_command_with_environment(
        events,
        cancellation,
        tool_call_id,
        arguments,
        arguments_json,
        paths,
        &bcode_config::ProcessConfigEnvironment,
    )
}

fn run_terminal_shell_command_with_environment(
    events: ServiceEventEmitter,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    tool_call_id: &str,
    arguments: &ShellRunArguments,
    arguments_json: serde_json::Value,
    paths: TerminalRunPaths<'_>,
    environment: &impl bcode_config::ConfigEnvironment,
) -> ToolInvocationResponse {
    match run_terminal_shell_command_inner(
        events,
        cancellation,
        tool_call_id,
        arguments,
        arguments_json,
        paths,
        environment,
    ) {
        Ok(response) => response,
        Err(error) => ToolInvocationResponse {
            output: error,
            is_error: true,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        },
    }
}

#[derive(Debug, Clone, Copy)]
struct TerminalShellStatus {
    exit_code: i32,
    success: bool,
    timed_out: bool,
    cancelled: bool,
}

fn wait_for_terminal_shell_status(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    cancellation_path: Option<&Path>,
    timeout: Duration,
    tool_call_id: &str,
    events: ServiceEventEmitter,
) -> Result<TerminalShellStatus, String> {
    let started = Instant::now();
    let mut timed_out = false;
    let mut cancelled = false;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break status;
        }
        if cancellation.is_cancelled() || cancellation_path.is_some_and(Path::exists) {
            cancelled = true;
            emit_tool_status(
                events,
                tool_call_id,
                1,
                "cancellation requested; killing terminal process",
            );
            child.kill().map_err(|error| error.to_string())?;
            break child.wait().map_err(|error| error.to_string())?;
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            emit_tool_status(
                events,
                tool_call_id,
                1,
                "timeout reached; killing terminal process",
            );
            child.kill().map_err(|error| error.to_string())?;
            break child.wait().map_err(|error| error.to_string())?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(TerminalShellStatus {
        exit_code: i32::try_from(status.exit_code()).unwrap_or(i32::MAX),
        success: status.success(),
        timed_out,
        cancelled,
    })
}

fn encode_terminal_output(
    command: &str,
    cwd: Option<&Path>,
    status: TerminalShellStatus,
    output: &LimitedOutput,
    columns: u16,
    rows: u16,
) -> Result<(String, String, LimitedOutput), String> {
    let inline_output = limit_terminal_inline_output(output);
    let terminal_output = TerminalCommandOutput {
        mode: "terminal",
        exit_code: Some(status.exit_code),
        timed_out: status.timed_out,
        cancelled: status.cancelled,
        command: command.to_owned(),
        cwd: cwd.map(|cwd| cwd.display().to_string()),
        output: inline_output.text.clone(),
        output_truncated: inline_output.truncated,
        output_bytes: u64::try_from(inline_output.original_bytes).unwrap_or(u64::MAX),
        retained_output_bytes: u64::try_from(inline_output.retained_bytes).unwrap_or(u64::MAX),
        columns,
        rows,
    };
    let full_terminal_output = TerminalCommandOutput {
        mode: "terminal",
        exit_code: Some(status.exit_code),
        timed_out: status.timed_out,
        cancelled: status.cancelled,
        command: command.to_owned(),
        cwd: cwd.map(|cwd| cwd.display().to_string()),
        output: output.text.clone(),
        output_truncated: output.truncated,
        output_bytes: u64::try_from(output.original_bytes).unwrap_or(u64::MAX),
        retained_output_bytes: u64::try_from(output.retained_bytes).unwrap_or(u64::MAX),
        columns,
        rows,
    };
    let encoded = serde_json::to_string(&terminal_output).map_err(|error| error.to_string())?;
    let full_encoded =
        serde_json::to_string(&full_terminal_output).map_err(|error| error.to_string())?;
    Ok((encoded, full_encoded, inline_output))
}

fn run_terminal_shell_command_inner(
    events: ServiceEventEmitter,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    tool_call_id: &str,
    arguments: &ShellRunArguments,
    arguments_json: serde_json::Value,
    paths: TerminalRunPaths<'_>,
    environment: &impl bcode_config::ConfigEnvironment,
) -> Result<ToolInvocationResponse, String> {
    let timeout = Duration::from_millis(arguments.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
    let cwd = resolve_effective_cwd(arguments, paths.session_cwd);
    let env_config = shell_config_with_environment(cwd.as_deref(), environment)?.env;
    let columns = arguments.terminal_columns();
    let rows = arguments.terminal_rows();
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows,
            cols: columns,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| error.to_string())?;

    let (program, args) = shell_program_and_args(&arguments.command, cwd.as_deref(), env_config)?;
    let mut command = portable_pty::CommandBuilder::new(program);
    for arg in args {
        command.arg(arg);
    }
    if let Some(cwd) = cwd.as_deref() {
        command.cwd(cwd);
    }
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");

    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| error.to_string())?;
    drop(pair.slave);
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| error.to_string())?;
    let clean_artifact_path = clean_artifact_path(paths.artifact_dir, tool_call_id)?;
    let raw_artifact_path = raw_artifact_path(paths.artifact_dir, tool_call_id)?;
    let timeout_at_ms = current_unix_millis()
        .saturating_add(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX));
    let reader_thread = std::thread::spawn({
        let tool_call_id = tool_call_id.to_owned();
        move || {
            read_limited_streaming(
                &mut reader,
                events,
                &tool_call_id,
                ShellVisualStreamContext {
                    arguments: &arguments_json,
                    stream: ToolOutputStream::Pty,
                    columns,
                    rows,
                    timeout_at_ms: Some(timeout_at_ms),
                },
                TerminalStreamPaths {
                    clean_artifact_path,
                    raw_artifact_path,
                },
            )
        }
    });

    let started = Instant::now();
    let status = wait_for_terminal_shell_status(
        &mut child,
        cancellation,
        paths.cancellation_path,
        timeout,
        tool_call_id,
        events,
    )?;
    drop(pair.master);
    let stream_output = join_reader(reader_thread)?;
    terminal_shell_response(
        tool_call_id,
        TerminalShellResponseInput {
            arguments,
            cwd: cwd.as_deref(),
            status,
            started,
            stream_output: &stream_output,
            columns,
            rows,
        },
    )
}

#[derive(Clone, Copy)]
struct TerminalShellResponseInput<'a> {
    arguments: &'a ShellRunArguments,
    cwd: Option<&'a Path>,
    status: TerminalShellStatus,
    started: Instant,
    stream_output: &'a TerminalStreamOutput,
    columns: u16,
    rows: u16,
}

fn terminal_shell_response(
    tool_call_id: &str,
    input: TerminalShellResponseInput<'_>,
) -> Result<ToolInvocationResponse, String> {
    let (encoded, full_encoded, _clean_inline_output) = encode_terminal_output(
        &input.arguments.command,
        input.cwd,
        input.status,
        &input.stream_output.clean,
        input.columns,
        input.rows,
    )?;
    let raw_inline_output = limit_terminal_inline_output(&input.stream_output.raw);
    Ok(ToolInvocationResponse {
        output: encoded,
        is_error: input.status.timed_out || input.status.cancelled || !input.status.success,
        content: Vec::new(),
        full_output: Some(full_encoded),
        host_action: None,
        result: Some(shell_run_artifact(
            tool_call_id,
            &ShellRunResult::Terminal {
                exit_code: Some(input.status.exit_code),
                timed_out: input.status.timed_out,
                cancelled: input.status.cancelled,
                duration_ms: Some(
                    u64::try_from(input.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                ),
                output_tail: raw_inline_output.text,
                output_truncated: raw_inline_output.truncated,
                output_bytes: Some(
                    u64::try_from(raw_inline_output.original_bytes).unwrap_or(u64::MAX),
                ),
                retained_output_bytes: Some(
                    u64::try_from(raw_inline_output.retained_bytes).unwrap_or(u64::MAX),
                ),
                columns: input.columns,
                rows: input.rows,
            },
            input
                .stream_output
                .clean_artifact_path
                .as_deref()
                .map(|path| clean_artifact_ref(path, &input.stream_output.clean)),
            input
                .stream_output
                .raw_artifact_path
                .as_deref()
                .map(|path| {
                    raw_artifact_ref(path, &input.stream_output.raw, input.columns, input.rows)
                }),
        )),
    })
}

fn limit_terminal_inline_output(output: &LimitedOutput) -> LimitedOutput {
    let bytes = output.text.as_bytes();
    let limit = MAX_INLINE_TERMINAL_OUTPUT_BYTES.min(bytes.len());
    let start = bytes.len().saturating_sub(limit);
    let start = utf8_boundary_at_or_after(&output.text, start);
    let text = output.text[start..].to_owned();
    LimitedOutput {
        text,
        original_bytes: output.original_bytes,
        retained_bytes: bytes.len().saturating_sub(start),
        truncated: output.truncated || start > 0,
    }
}

const fn utf8_boundary_at_or_after(value: &str, mut index: usize) -> usize {
    while index < value.len() && !value.is_char_boundary(index) {
        index = index.saturating_add(1);
    }
    index
}

#[cfg(unix)]
const fn shell_program() -> &'static str {
    "sh"
}

#[cfg(windows)]
const fn shell_program() -> &'static str {
    "cmd"
}

#[cfg(unix)]
fn shell_args(command: &str) -> Vec<String> {
    vec![
        "-o".to_string(),
        "pipefail".to_string(),
        "-c".to_string(),
        command.to_string(),
    ]
}

#[cfg(windows)]
fn shell_args(command: &str) -> Vec<String> {
    vec!["/C".to_string(), command.to_string()]
}

struct TerminalStreamPaths {
    clean_artifact_path: Option<PathBuf>,
    raw_artifact_path: Option<PathBuf>,
}

#[derive(Clone, Copy)]
struct ShellVisualStreamContext<'a> {
    arguments: &'a serde_json::Value,
    stream: ToolOutputStream,
    columns: u16,
    rows: u16,
    timeout_at_ms: Option<u64>,
}

fn read_limited_streaming<R>(
    mut reader: R,
    events: ServiceEventEmitter,
    tool_call_id: &str,
    visual_context: ShellVisualStreamContext<'_>,
    paths: TerminalStreamPaths,
) -> Result<TerminalStreamOutput, String>
where
    R: Read,
{
    let mut raw_bytes = Vec::new();
    let mut raw_writer = raw_artifact_writer(paths.raw_artifact_path.as_deref())?;
    let mut clean_writer = clean_artifact_writer(paths.clean_artifact_path.as_deref())?;
    let mut cleaner = terminal_clean::TerminalCleanWriter::new(
        &mut clean_writer,
        visual_context.columns,
        visual_context.rows,
        MAX_INLINE_TERMINAL_OUTPUT_BYTES,
    );
    let mut buffer = [0_u8; 4096];
    let mut sequence = 0_u64;
    let mut raw_original_bytes = 0_usize;
    let mut raw_truncated = false;
    let mut visual_output = String::new();
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        sequence = sequence.saturating_add(1);
        raw_original_bytes = raw_original_bytes.saturating_add(read);
        visual_output.push_str(&String::from_utf8_lossy(&buffer[..read]));
        emit_tool_output_delta(
            events,
            tool_call_id,
            visual_context,
            sequence,
            visual_output.as_bytes(),
        );
        cleaner
            .write_chunk(&buffer[..read])
            .map_err(|error| error.to_string())?;

        let remaining = DEFAULT_MAX_OUTPUT_BYTES.saturating_sub(raw_bytes.len());
        if remaining == 0 {
            raw_truncated = true;
            continue;
        }
        let retained = read.min(remaining);
        raw_writer
            .write_all(&buffer[..retained])
            .map_err(|error| error.to_string())?;
        raw_bytes.extend_from_slice(&buffer[..retained]);
        raw_truncated = raw_truncated || retained < read;
    }
    raw_writer.flush().map_err(|error| error.to_string())?;
    let clean_summary = cleaner.finish().map_err(|error| error.to_string())?;
    let clean_bytes = clean_summary.tail.into_bytes();
    Ok(TerminalStreamOutput {
        raw: limit_output_bytes_with_original(
            &raw_bytes,
            raw_original_bytes,
            DEFAULT_MAX_OUTPUT_BYTES,
            raw_truncated,
        ),
        clean: limit_output_bytes_with_original(
            &clean_bytes,
            usize::try_from(clean_summary.bytes_written).unwrap_or(usize::MAX),
            MAX_INLINE_TERMINAL_OUTPUT_BYTES,
            clean_summary.tail_truncated,
        ),
        raw_artifact_path: paths.raw_artifact_path,
        clean_artifact_path: paths.clean_artifact_path,
    })
}

fn raw_artifact_path(
    artifact_dir: Option<&Path>,
    tool_call_id: &str,
) -> Result<Option<PathBuf>, String> {
    artifact_path(artifact_dir, tool_call_id, |safe_tool_call_id| {
        format!("tool-output-{safe_tool_call_id}-pty.txt")
    })
}

fn clean_artifact_path(
    artifact_dir: Option<&Path>,
    tool_call_id: &str,
) -> Result<Option<PathBuf>, String> {
    artifact_path(artifact_dir, tool_call_id, |safe_tool_call_id| {
        format!("tool-output-{safe_tool_call_id}-clean.txt")
    })
}

fn artifact_path(
    artifact_dir: Option<&Path>,
    tool_call_id: &str,
    name: impl FnOnce(&str) -> String,
) -> Result<Option<PathBuf>, String> {
    let Some(artifact_dir) = artifact_dir else {
        return Ok(None);
    };
    std::fs::create_dir_all(artifact_dir).map_err(|error| error.to_string())?;
    let safe_tool_call_id = tool_call_id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>();
    Ok(Some(artifact_dir.join(name(&safe_tool_call_id))))
}

fn raw_artifact_writer(path: Option<&Path>) -> Result<Box<dyn Write + Send>, String> {
    artifact_writer(path)
}

fn clean_artifact_writer(path: Option<&Path>) -> Result<Box<dyn Write + Send>, String> {
    artifact_writer(path)
}

fn artifact_writer(path: Option<&Path>) -> Result<Box<dyn Write + Send>, String> {
    path.map_or_else(
        || Ok(Box::new(Vec::<u8>::new()) as Box<dyn Write + Send>),
        |path| {
            File::create(path)
                .map(|file| Box::new(file) as Box<dyn Write + Send>)
                .map_err(|error| error.to_string())
        },
    )
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn emit_tool_output_delta(
    events: ServiceEventEmitter,
    tool_call_id: &str,
    visual_context: ShellVisualStreamContext<'_>,
    sequence: u64,
    bytes: &[u8],
) {
    let text = String::from_utf8_lossy(bytes).into_owned();
    emit_tool_stream_event(
        events,
        &ToolInvocationStreamEvent::VisualUpdate {
            tool_call_id: tool_call_id.to_owned(),
            sequence,
            visual: ToolStreamVisualUpdate {
                producer_plugin_id: Some("bcode.shell".to_owned()),
                schema: "bcode.tool.request.shell.run".to_owned(),
                schema_version: 1,
                title: Some("Shell command".to_owned()),
                subtitle: None,
                payload: json!({
                    "arguments": visual_context.arguments,
                    "_bcode_runtime": {
                        "output": text,
                        "columns": visual_context.columns,
                        "rows": visual_context.rows,
                        "timeout_at_ms": visual_context.timeout_at_ms,
                        "streaming": true,
                    }
                }),
            },
            streaming: true,
        },
    );
    let _ = visual_context.stream;
}

fn emit_tool_status(
    events: ServiceEventEmitter,
    tool_call_id: &str,
    sequence: u64,
    message: impl Into<String>,
) {
    emit_tool_stream_event(
        events,
        &ToolInvocationStreamEvent::Status {
            tool_call_id: tool_call_id.to_owned(),
            sequence,
            message: message.into(),
        },
    );
}

fn emit_tool_stream_event(events: ServiceEventEmitter, event: &ToolInvocationStreamEvent) {
    if let Ok(payload) = serde_json::to_vec(event) {
        events.emit(&payload);
    }
}

#[cfg(test)]
fn limit_output_bytes(bytes: &[u8], max_bytes: usize) -> LimitedOutput {
    limit_output_bytes_with_original(bytes, bytes.len(), max_bytes, false)
}

fn limit_output_bytes_with_original(
    bytes: &[u8],
    original_bytes: usize,
    max_bytes: usize,
    already_truncated: bool,
) -> LimitedOutput {
    let retained_len = valid_utf8_prefix_len(bytes, max_bytes.min(bytes.len()));
    let text = String::from_utf8_lossy(&bytes[..retained_len]).into_owned();
    LimitedOutput {
        text,
        original_bytes,
        retained_bytes: retained_len,
        truncated: already_truncated || retained_len < bytes.len() || bytes.len() < original_bytes,
    }
}

fn valid_utf8_prefix_len(bytes: &[u8], max_len: usize) -> usize {
    let mut len = max_len.min(bytes.len());
    while len > 0 && std::str::from_utf8(&bytes[..len]).is_err() {
        len = len.saturating_sub(1);
    }
    len
}

fn join_reader(
    handle: std::thread::JoinHandle<Result<TerminalStreamOutput, String>>,
) -> Result<TerminalStreamOutput, String> {
    handle
        .join()
        .map_err(|_| "output reader thread panicked".to_string())?
}

fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

fn shell_run_artifact(
    tool_call_id: &str,
    result: &ShellRunResult,
    clean_ref: Option<ToolArtifactRef>,
    raw_ref: Option<ToolArtifactRef>,
) -> ToolInvocationResult {
    ToolInvocationResult::Artifact {
        artifact: Box::new(ToolArtifact {
            artifact_id: format!("{tool_call_id}-shell-run"),
            producer_plugin_id: "bcode.shell".to_string(),
            schema: "bcode.shell.run".to_string(),
            schema_version: 1,
            tool_call_id: Some(tool_call_id.to_string()),
            title: Some("Shell run".to_string()),
            metadata: serde_json::to_value(result).unwrap_or_else(|_| json!({})),
            refs: clean_ref.into_iter().chain(raw_ref).collect(),
        }),
    }
}

fn clean_artifact_ref(path: &Path, output: &LimitedOutput) -> ToolArtifactRef {
    ToolArtifactRef {
        key: "clean_output".to_string(),
        content_type: Some("text/plain; charset=utf-8".to_string()),
        storage_uri: file_storage_uri(path),
        byte_len: Some(u64::try_from(output.original_bytes).unwrap_or(u64::MAX)),
        metadata: Some(json!({
            "description": "Model-oriented terminal transcript normalized by the shell plugin",
            "retained_tail_bytes": output.retained_bytes,
            "tail_truncated": output.truncated,
        })),
    }
}

fn raw_artifact_ref(
    path: &Path,
    output: &LimitedOutput,
    columns: u16,
    rows: u16,
) -> ToolArtifactRef {
    ToolArtifactRef {
        key: TERMINAL_PTY_STREAM_REF_KEY.to_string(),
        content_type: Some(TERMINAL_PTY_STREAM_CONTENT_TYPE.to_string()),
        storage_uri: file_storage_uri(path),
        byte_len: Some(u64::try_from(output.retained_bytes).unwrap_or(u64::MAX)),
        metadata: Some(json!({
            "description": "Raw terminal PTY stream for display replay",
            "stream": "pty",
            "columns": columns,
            "rows": rows,
            "retained_tail_bytes": output.retained_bytes,
            "tail_truncated": output.truncated,
            "encoding": "utf-8-lossy",
        })),
    }
}

fn file_storage_uri(path: &Path) -> Option<String> {
    url::Url::from_file_path(path)
        .ok()
        .map(|uri| uri.to_string())
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    let mut vtable = bcode_plugin_sdk::static_concurrent_plugin_vtable!(
        ShellPlugin,
        include_str!("../bcode-plugin.toml")
    );
    vtable.tui_registry = Some(shell_tui_registry);
    vtable
}

#[cfg(feature = "static-bundled")]
fn shell_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    registry.register_visual_adapter(Box::new(shell_run_tui::ShellRunTuiVisualAdapter));
    registry
}

bcode_plugin_sdk::export_concurrent_plugin!(ShellPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated_config_environment(name: &str) -> bcode_config::ConfigEnvironmentSnapshot {
        let root = std::env::temp_dir().join(format!(
            "bcode-shell-plugin-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        ));
        bcode_config::ConfigEnvironmentSnapshot::isolated(root)
    }

    fn shell_result_from_artifact(response: &ToolInvocationResponse) -> Option<ShellRunResult> {
        let Some(ToolInvocationResult::Artifact { artifact }) = &response.result else {
            return None;
        };
        if artifact.schema != "bcode.shell.run" {
            return None;
        }
        serde_json::from_value(artifact.metadata.clone()).ok()
    }

    fn test_limited_output() -> LimitedOutput {
        LimitedOutput {
            text: String::new(),
            original_bytes: 12,
            retained_bytes: 12,
            truncated: false,
        }
    }

    #[cfg(unix)]
    #[test]
    fn clean_artifact_ref_uses_encoded_file_uri() {
        let output = test_limited_output();
        let reference = clean_artifact_ref(Path::new("/tmp/bcode shell #output%?.txt"), &output);

        assert_eq!(
            reference.storage_uri.as_deref(),
            Some("file:///tmp/bcode%20shell%20%23output%25%3F.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn clean_artifact_ref_file_uri_round_trips_unicode_path() {
        let path = Path::new("/tmp/bcode café output.txt");
        let reference = clean_artifact_ref(path, &test_limited_output());
        let uri = reference
            .storage_uri
            .as_deref()
            .and_then(|value| url::Url::parse(value).ok())
            .expect("file uri should parse");

        assert_eq!(uri.scheme(), "file");
        assert_eq!(uri.to_file_path().expect("uri should become path"), path);
    }

    #[test]
    fn clean_artifact_ref_omits_storage_uri_for_relative_path() {
        let reference = clean_artifact_ref(
            Path::new("relative/path with spaces.txt"),
            &test_limited_output(),
        );

        assert_eq!(reference.storage_uri, None);
        assert_eq!(reference.key, "clean_output");
        assert_eq!(reference.byte_len, Some(12));
    }

    #[test]
    fn raw_artifact_ref_records_terminal_replay_metadata() {
        let reference = raw_artifact_ref(
            Path::new("/tmp/raw-pty.txt"),
            &test_limited_output(),
            80,
            24,
        );

        assert_eq!(reference.key, TERMINAL_PTY_STREAM_REF_KEY);
        assert_eq!(
            reference.content_type.as_deref(),
            Some(TERMINAL_PTY_STREAM_CONTENT_TYPE)
        );
        assert_eq!(reference.byte_len, Some(12));
        let metadata = reference.metadata.expect("metadata should exist");
        assert_eq!(metadata["stream"], "pty");
        assert_eq!(metadata["columns"], 80);
        assert_eq!(metadata["rows"], 24);
    }

    #[test]
    fn shell_run_schema_does_not_expose_terminal_toggle() {
        let request = ServiceRequest {
            interface_id: TOOL_SERVICE_INTERFACE_ID.to_string(),
            operation: OP_LIST_TOOLS.to_string(),
            payload: serde_json::to_vec(&ListToolsRequest::default())
                .expect("request should encode"),
        };
        let response = list_tools(&request);
        assert!(response.error.is_none());
        let tools = response
            .payload_json::<ToolList>()
            .expect("tool list should decode");
        let shell_run = tools
            .tools
            .iter()
            .find(|tool| tool.name == "shell.run")
            .expect("shell.run tool should be listed");
        let properties = shell_run
            .input_schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("schema should have object properties");

        assert!(!properties.contains_key("terminal"));
        assert!(shell_run.description.contains("streams live"));
    }

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_shell_process_group() {
        let environment = isolated_config_environment("timeout");
        let started = Instant::now();
        let response = run_terminal_shell_command_with_environment(
            ServiceEventEmitter::default(),
            &bcode_plugin_sdk::ServiceCancellation::default(),
            "test",
            &ShellRunArguments {
                command: "sh -c 'trap \"\" HUP TERM; sleep 5' | cat".to_string(),
                cwd: None,
                timeout_ms: Some(100),
                columns: None,
                rows: None,
            },
            json!({}),
            TerminalRunPaths {
                session_cwd: None,
                artifact_dir: None,
                cancellation_path: None,
            },
            &environment,
        );

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(response.is_error);
        assert!(response.output.contains("\"timed_out\":true"));
    }

    #[test]
    fn limit_output_bytes_truncates_at_utf8_boundary() {
        let output = limit_output_bytes("abcé".as_bytes(), 4);

        assert_eq!(output.text, "abc");
        assert_eq!(output.original_bytes, 5);
        assert_eq!(output.retained_bytes, 3);
        assert!(output.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn shell_pipeline_preserves_failing_left_side_status() {
        let environment = isolated_config_environment("pipeline");
        let response = run_terminal_shell_command_with_environment(
            ServiceEventEmitter::default(),
            &bcode_plugin_sdk::ServiceCancellation::default(),
            "test",
            &ShellRunArguments {
                command: "false | sed -n '1,1p'".to_string(),
                cwd: None,
                timeout_ms: Some(1_000),
                columns: None,
                rows: None,
            },
            json!({}),
            TerminalRunPaths {
                session_cwd: None,
                artifact_dir: None,
                cancellation_path: None,
            },
            &environment,
        );

        assert!(response.is_error);
        assert!(response.output.contains("\"exit_code\":1"));
    }

    #[cfg(unix)]
    #[test]
    fn terminal_mode_returns_semantic_terminal_result() {
        let environment = isolated_config_environment("terminal");
        let response = run_terminal_shell_command_with_environment(
            ServiceEventEmitter::default(),
            &bcode_plugin_sdk::ServiceCancellation::default(),
            "test-terminal-semantic",
            &ShellRunArguments {
                command: "printf 'semantic terminal\\n'".to_string(),
                cwd: None,
                timeout_ms: Some(5_000),
                columns: Some(80),
                rows: Some(24),
            },
            json!({}),
            TerminalRunPaths {
                session_cwd: None,
                artifact_dir: None,
                cancellation_path: None,
            },
            &environment,
        );

        assert!(!response.is_error, "{}", response.output);
        let ShellRunResult::Terminal {
            exit_code,
            timed_out,
            cancelled,
            output_tail,
            columns,
            rows,
            ..
        } = shell_result_from_artifact(&response).expect("expected shell artifact")
        else {
            panic!("expected semantic terminal shell result");
        };
        assert_eq!(exit_code, Some(0));
        assert!(!timed_out);
        assert!(!cancelled);
        assert!(output_tail.contains("semantic terminal"));
        assert_eq!(columns, 80);
        assert_eq!(rows, 24);
    }

    #[cfg(unix)]
    #[test]
    fn terminal_mode_preserves_ansi_output() {
        let response = run_terminal_shell_command(
            ServiceEventEmitter::default(),
            &bcode_plugin_sdk::ServiceCancellation::default(),
            "test-terminal-ansi",
            &ShellRunArguments {
                command: "printf '\\033[31mred\\033[0m\\n'".to_string(),
                cwd: None,
                timeout_ms: Some(5_000),
                columns: Some(80),
                rows: Some(24),
            },
            json!({}),
            TerminalRunPaths {
                session_cwd: None,
                artifact_dir: None,
                cancellation_path: None,
            },
        );

        assert!(!response.is_error, "{}", response.output);
        let ShellRunResult::Terminal { output_tail, .. } =
            shell_result_from_artifact(&response).expect("expected shell artifact")
        else {
            panic!("expected semantic terminal shell result");
        };
        assert!(output_tail.contains("\u{1b}[31mred\u{1b}[0m"));
    }

    #[test]
    fn terminal_output_encoding_returns_inline_tail() {
        let output = LimitedOutput {
            text: "hello".to_string(),
            original_bytes: 5,
            retained_bytes: 5,
            truncated: false,
        };
        let (_encoded, full_encoded, inline_output) = encode_terminal_output(
            "printf hello",
            None,
            TerminalShellStatus {
                exit_code: 0,
                success: true,
                timed_out: false,
                cancelled: false,
            },
            &output,
            80,
            24,
        )
        .expect("terminal output encodes");

        assert_eq!(inline_output.text, "hello");
        assert_eq!(inline_output.original_bytes, 5);
        assert_eq!(inline_output.retained_bytes, 5);
        assert!(!inline_output.truncated);
        assert!(full_encoded.contains("hello"));
    }

    #[test]
    fn terminal_result_tail_marks_truncation_and_byte_counts() {
        let output = LimitedOutput {
            text: format!("{}tail", "x".repeat(MAX_INLINE_TERMINAL_OUTPUT_BYTES + 128)),
            original_bytes: MAX_INLINE_TERMINAL_OUTPUT_BYTES + 132,
            retained_bytes: MAX_INLINE_TERMINAL_OUTPUT_BYTES + 132,
            truncated: false,
        };

        let limited = limit_terminal_inline_output(&output);

        assert!(limited.truncated);
        assert_eq!(limited.original_bytes, output.original_bytes);
        assert!(limited.retained_bytes <= MAX_INLINE_TERMINAL_OUTPUT_BYTES);
        assert!(limited.text.ends_with("tail"));
    }

    #[test]
    fn terminal_final_output_is_smaller_tail() {
        let output = LimitedOutput {
            text: format!("{}tail", "x".repeat(MAX_INLINE_TERMINAL_OUTPUT_BYTES + 128)),
            original_bytes: MAX_INLINE_TERMINAL_OUTPUT_BYTES + 132,
            retained_bytes: MAX_INLINE_TERMINAL_OUTPUT_BYTES + 132,
            truncated: false,
        };

        let limited = limit_terminal_inline_output(&output);

        assert!(limited.truncated);
        assert!(limited.retained_bytes <= MAX_INLINE_TERMINAL_OUTPUT_BYTES);
        assert!(limited.text.ends_with("tail"));
    }

    #[test]
    fn terminal_output_json_stays_valid_when_output_is_truncated() {
        let bytes = vec![b'x'; DEFAULT_MAX_OUTPUT_BYTES + 1];
        let output = limit_output_bytes(&bytes, DEFAULT_MAX_OUTPUT_BYTES);
        let terminal_output = TerminalCommandOutput {
            mode: "terminal",
            exit_code: Some(0),
            timed_out: false,
            cancelled: false,
            command: "printf hello".to_owned(),
            cwd: None,
            output: output.text,
            output_truncated: output.truncated,
            output_bytes: u64::try_from(output.original_bytes).unwrap_or(u64::MAX),
            retained_output_bytes: u64::try_from(output.retained_bytes).unwrap_or(u64::MAX),
            columns: DEFAULT_TERMINAL_COLUMNS,
            rows: DEFAULT_TERMINAL_ROWS,
        };

        let encoded = serde_json::to_string(&terminal_output).expect("terminal output encodes");
        let value = serde_json::from_str::<serde_json::Value>(&encoded).expect("valid json");

        assert_eq!(
            value.get("mode").and_then(serde_json::Value::as_str),
            Some("terminal")
        );
        assert_eq!(
            value
                .get("output_truncated")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }
}
