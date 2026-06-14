#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled shell execution tool plugin for Bcode.

use bcode_config::{
    ShellToolConfig, ShellToolEnvAutoFallback, ShellToolEnvConfig, ShellToolEnvMode,
    default_config_paths_from, load_config_from_paths,
};
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolDefinition,
    ToolInvocationRequest, ToolInvocationResponse, ToolInvocationStreamEvent, ToolList,
    ToolOutputStream, ToolSideEffect,
};
use bcode_tool_runtime::{ProcessExecutionRequest, ToolExecutionRuntime};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_TERMINAL_COLUMNS: u16 = 120;
const DEFAULT_TERMINAL_ROWS: u16 = 30;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024;
const MAX_INLINE_TERMINAL_OUTPUT_BYTES: usize = 16 * 1024;

/// Bundled shell plugin.
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

#[derive(Debug, Deserialize)]
struct ShellRunArguments {
    command: String,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default = "default_terminal_mode")]
    terminal: bool,
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

const fn default_terminal_mode() -> bool {
    true
}

fn list_tools(request: &ServiceRequest) -> ServiceResponse {
    if let Err(error) = request.payload_json::<ListToolsRequest>() {
        return invalid_request(&error);
    }
    json_response(&ToolList {
        tools: vec![ToolDefinition {
            name: "shell.run".to_string(),
            description: "Run a shell command. Defaults to pseudo-terminal mode for human-like CLI colors and formatting; set terminal=false for separate stdout/stderr capture.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 1 },
                    "terminal": { "type": "boolean", "default": true, "description": "Run under a pseudo-terminal for human-like CLI formatting. Set false only when separate stdout/stderr capture is required." },
                    "columns": { "type": "integer", "minimum": 1 },
                    "rows": { "type": "integer", "minimum": 1 }
                }
            }),
            side_effect: ToolSideEffect::ExecuteProcess,
            requires_permission: true,
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
            request.cancellation_path.as_deref(),
        ),
        _ => ToolInvocationResponse {
            output: format!("unknown shell tool: {}", request.name),
            is_error: true,
            content: Vec::new(),
            full_output: None,
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
    cancellation_path: Option<&std::path::Path>,
) -> ToolInvocationResponse {
    let arguments = match serde_json::from_value::<ShellRunArguments>(arguments) {
        Ok(arguments) => arguments,
        Err(error) => {
            return ToolInvocationResponse {
                output: error.to_string(),
                is_error: true,
                content: Vec::new(),
                full_output: None,
            };
        }
    };
    if arguments.command.trim().is_empty() {
        return ToolInvocationResponse {
            output: "command must not be empty".to_string(),
            is_error: true,
            content: Vec::new(),
            full_output: None,
        };
    }
    let now_ms = current_unix_millis();
    emit_tool_stream_event(
        events,
        &ToolInvocationStreamEvent::Started {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            terminal: arguments.terminal,
            columns: arguments.terminal.then_some(arguments.terminal_columns()),
            rows: arguments.terminal.then_some(arguments.terminal_rows()),
            started_at_ms: Some(now_ms),
        },
    );
    emit_tool_status(
        events,
        tool_call_id,
        0,
        format!("starting command: {}", arguments.command),
    );
    let response = if arguments.terminal {
        run_terminal_shell_command(
            events,
            &context.cancellation,
            tool_call_id,
            &arguments,
            session_cwd,
            cancellation_path,
        )
    } else {
        match run_shell_command(
            events,
            &context.cancellation,
            tool_call_id,
            &arguments,
            session_cwd,
            cancellation_path,
        ) {
            Ok(output) => output,
            Err(error) => ToolInvocationResponse {
                output: error,
                is_error: true,
                content: Vec::new(),
                full_output: None,
            },
        }
    };
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

fn shell_config(cwd: Option<&Path>) -> Result<ShellToolConfig, String> {
    let paths = cwd.map_or_else(
        bcode_config::default_config_paths,
        default_config_paths_from,
    );
    load_config_from_paths(&paths)
        .map(|config| config.tools.shell)
        .map_err(|error| error.to_string())
}

fn shell_env_config(cwd: Option<&Path>) -> Result<ShellToolEnvConfig, String> {
    shell_config(cwd).map(|config| config.env)
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

fn run_terminal_shell_command(
    events: ServiceEventEmitter,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    tool_call_id: &str,
    arguments: &ShellRunArguments,
    session_cwd: Option<&Path>,
    cancellation_path: Option<&Path>,
) -> ToolInvocationResponse {
    match run_terminal_shell_command_inner(
        events,
        cancellation,
        tool_call_id,
        arguments,
        session_cwd,
        cancellation_path,
    ) {
        Ok(response) => response,
        Err(error) => ToolInvocationResponse {
            output: error,
            is_error: true,
            content: Vec::new(),
            full_output: None,
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
    status: TerminalShellStatus,
    output: &LimitedOutput,
    columns: u16,
    rows: u16,
) -> Result<(String, String), String> {
    let inline_output = limit_terminal_inline_output(output);
    let terminal_output = TerminalCommandOutput {
        mode: "terminal",
        exit_code: Some(status.exit_code),
        timed_out: status.timed_out,
        cancelled: status.cancelled,
        output: inline_output.text,
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
    Ok((encoded, full_encoded))
}

fn run_terminal_shell_command_inner(
    events: ServiceEventEmitter,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    tool_call_id: &str,
    arguments: &ShellRunArguments,
    session_cwd: Option<&Path>,
    cancellation_path: Option<&Path>,
) -> Result<ToolInvocationResponse, String> {
    let timeout = Duration::from_millis(arguments.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
    let cwd = resolve_effective_cwd(arguments, session_cwd);
    let env_config = shell_env_config(cwd.as_deref())?;
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
    let reader_thread = std::thread::spawn({
        let tool_call_id = tool_call_id.to_owned();
        move || read_limited_streaming(&mut reader, events, &tool_call_id, ToolOutputStream::Pty)
    });

    let status = wait_for_terminal_shell_status(
        &mut child,
        cancellation,
        cancellation_path,
        timeout,
        tool_call_id,
        events,
    )?;
    drop(pair.master);
    let output = join_reader(reader_thread)?;
    let (encoded, full_encoded) = encode_terminal_output(status, &output, columns, rows)?;
    Ok(ToolInvocationResponse {
        output: encoded,
        is_error: status.timed_out || status.cancelled || !status.success,
        content: Vec::new(),
        full_output: Some(full_encoded),
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

fn build_process_tool_response(
    command: &str,
    result: &bcode_tool_runtime::ProcessExecutionResult,
    max_output_bytes: usize,
    inline_output_bytes: usize,
) -> ToolInvocationResponse {
    let stdout = limit_output_bytes_with_truncation(
        &result.stdout.bytes,
        max_output_bytes,
        result.stdout.truncated,
    );
    let stderr = limit_output_bytes_with_truncation(
        &result.stderr.bytes,
        max_output_bytes,
        result.stderr.truncated,
    );
    let inline_stdout = limit_inline_stream_output(&stdout, inline_output_bytes);
    let inline_stderr = limit_inline_stream_output(&stderr, inline_output_bytes);
    let output = format_command_output(
        command,
        result.exit_code,
        result.timed_out,
        result.cancelled,
        &inline_stdout,
        &inline_stderr,
    );
    let full_output = format_command_output(
        command,
        result.exit_code,
        result.timed_out,
        result.cancelled,
        &stdout,
        &stderr,
    );
    ToolInvocationResponse {
        output,
        is_error: result.timed_out
            || result.cancelled
            || result.exit_code.is_none_or(|code| code != 0),
        content: Vec::new(),
        full_output: Some(full_output),
    }
}

fn run_shell_command(
    events: ServiceEventEmitter,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    tool_call_id: &str,
    arguments: &ShellRunArguments,
    session_cwd: Option<&std::path::Path>,
    cancellation_path: Option<&std::path::Path>,
) -> Result<ToolInvocationResponse, String> {
    let timeout = Duration::from_millis(arguments.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
    let cwd = resolve_effective_cwd(arguments, session_cwd);
    let config = shell_config(cwd.as_deref())?;
    let env_config = config.env;
    let max_output_bytes = config.max_output_bytes;
    let inline_output_bytes = config.inline_output_bytes;
    let (program, args) = shell_program_and_args(&arguments.command, cwd.as_deref(), env_config)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let tool_call_id = tool_call_id.to_owned();
    let cancellation_path = cancellation_path.map(Path::to_path_buf);
    let cancellation = cancellation.clone();
    let result = runtime
        .block_on(async {
            let runtime = ToolExecutionRuntime::new(1);
            let cancel_handle = runtime.cancellation_handle();
            let cancel_task = cancellation_path.map(|path| {
                let cancel_handle = cancel_handle.clone();
                let cancellation = cancellation.clone();
                tokio::spawn(async move {
                    while !cancellation.is_cancelled() && !path.exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    cancel_handle.cancel();
                })
            });
            let context_cancel_task = if cancel_task.is_none() {
                let cancel_handle = cancel_handle.clone();
                Some(tokio::spawn(async move {
                    while !cancellation.is_cancelled() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    cancel_handle.cancel();
                }))
            } else {
                None
            };
            let result = runtime
                .run_process_streaming(
                    ProcessExecutionRequest {
                        program,
                        args,
                        cwd,
                        timeout: Some(timeout),
                        max_output_bytes,
                    },
                    move |event| {
                        let stream = match event.stream {
                            bcode_tool_runtime::ProcessOutputStream::Stdout => {
                                ToolOutputStream::Stdout
                            }
                            bcode_tool_runtime::ProcessOutputStream::Stderr => {
                                ToolOutputStream::Stderr
                            }
                        };
                        emit_tool_output_delta(
                            events,
                            &tool_call_id,
                            stream,
                            event.sequence,
                            &event.bytes,
                        );
                    },
                )
                .await;
            if let Some(cancel_task) = cancel_task {
                cancel_task.abort();
            }
            if let Some(context_cancel_task) = context_cancel_task {
                context_cancel_task.abort();
            }
            result
        })
        .map_err(|error| error.to_string())?;

    Ok(build_process_tool_response(
        &arguments.command,
        &result,
        max_output_bytes,
        inline_output_bytes,
    ))
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

fn read_limited_streaming<R>(
    mut reader: R,
    events: ServiceEventEmitter,
    tool_call_id: &str,
    stream: ToolOutputStream,
) -> Result<LimitedOutput, String>
where
    R: Read,
{
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut sequence = 0_u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        sequence = sequence.saturating_add(1);
        let remaining = DEFAULT_MAX_OUTPUT_BYTES.saturating_sub(bytes.len());
        if remaining == 0 {
            continue;
        }
        let retained = read.min(remaining);
        emit_tool_output_delta(events, tool_call_id, stream, sequence, &buffer[..retained]);
        bytes.extend_from_slice(&buffer[..retained]);
    }
    Ok(limit_output_bytes(&bytes, DEFAULT_MAX_OUTPUT_BYTES))
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
    stream: ToolOutputStream,
    sequence: u64,
    bytes: &[u8],
) {
    emit_tool_stream_event(
        events,
        &ToolInvocationStreamEvent::OutputDelta {
            tool_call_id: tool_call_id.to_owned(),
            stream,
            sequence,
            text: String::from_utf8_lossy(bytes).into_owned(),
            byte_len: bytes.len(),
        },
    );
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

fn limit_output_bytes(bytes: &[u8], max_bytes: usize) -> LimitedOutput {
    limit_output_bytes_with_truncation(bytes, max_bytes, false)
}

fn limit_output_bytes_with_truncation(
    bytes: &[u8],
    max_bytes: usize,
    already_truncated: bool,
) -> LimitedOutput {
    let original_bytes = bytes.len();
    let retained_len = valid_utf8_prefix_len(bytes, max_bytes.min(original_bytes));
    let text = String::from_utf8_lossy(&bytes[..retained_len]).into_owned();
    LimitedOutput {
        text,
        original_bytes,
        retained_bytes: retained_len,
        truncated: already_truncated || retained_len < original_bytes,
    }
}

fn limit_inline_stream_output(output: &LimitedOutput, max_bytes: usize) -> LimitedOutput {
    let bytes = output.text.as_bytes();
    let limit = max_bytes.min(bytes.len());
    if !output.truncated && limit == bytes.len() {
        return output.clone();
    }

    let tail_budget = limit.saturating_mul(3) / 5;
    let head_budget = limit.saturating_sub(tail_budget);
    let head_end = utf8_boundary_at_or_before(&output.text, head_budget);
    let tail_start =
        utf8_boundary_at_or_after(&output.text, bytes.len().saturating_sub(tail_budget));
    let text = if head_end >= tail_start {
        output.text.clone()
    } else {
        format!("{}{}", &output.text[..head_end], &output.text[tail_start..])
    };

    LimitedOutput {
        text,
        original_bytes: output.original_bytes,
        retained_bytes: head_end + bytes.len().saturating_sub(tail_start),
        truncated: true,
    }
}

const fn utf8_boundary_at_or_before(value: &str, mut index: usize) -> usize {
    while index > 0 && !value.is_char_boundary(index) {
        index = index.saturating_sub(1);
    }
    index
}

fn valid_utf8_prefix_len(bytes: &[u8], max_len: usize) -> usize {
    let mut len = max_len.min(bytes.len());
    while len > 0 && std::str::from_utf8(&bytes[..len]).is_err() {
        len = len.saturating_sub(1);
    }
    len
}

fn join_reader(
    handle: std::thread::JoinHandle<Result<LimitedOutput, String>>,
) -> Result<LimitedOutput, String> {
    handle
        .join()
        .map_err(|_| "output reader thread panicked".to_string())?
}

fn format_command_output(
    command: &str,
    exit_code: Option<i32>,
    timed_out: bool,
    cancelled: bool,
    stdout: &LimitedOutput,
    stderr: &LimitedOutput,
) -> String {
    let exit_code = exit_code.map_or_else(|| "signal".to_string(), |code| code.to_string());
    let pipeline_note = output_slicing_pipeline_note(command);
    format!(
        "exit_code: {exit_code}\ntimed_out: {timed_out}\ncancelled: {cancelled}{pipeline_note}\nstdout:\n{}\nstderr:\n{}",
        format_stream_output("stdout", stdout),
        format_stream_output("stderr", stderr),
    )
}

fn output_slicing_pipeline_note(command: &str) -> &'static str {
    if command_contains_output_slicing_pipe(command) {
        "\nnote: this command pipes output through sed/head/tail. Bcode already shows the beginning and end of long shell output; prefer unsliced validation commands plus artifact.read/artifact.grep for omitted retained output."
    } else {
        ""
    }
}

fn command_contains_output_slicing_pipe(command: &str) -> bool {
    let normalized = command.split_whitespace().collect::<Vec<_>>().join(" ");
    [
        "| sed ", "| head", "| tail", "|& sed ", "|& head", "|& tail",
    ]
    .iter()
    .any(|pattern| normalized.contains(pattern))
}

fn format_stream_output(stream: &str, output: &LimitedOutput) -> String {
    if !output.truncated {
        return output.text.clone();
    }
    let omitted = output.original_bytes.saturating_sub(output.retained_bytes);
    let capture_note = if output.truncated && output.original_bytes >= DEFAULT_MAX_OUTPUT_BYTES {
        " Process output may have exceeded Bcode's capture limit."
    } else {
        ""
    };
    format!(
        "[{stream} truncated: omitted {omitted} bytes; showing first and last {} of {} retained bytes.{capture_note} Bcode already shows both the beginning and end of long shell output. Do not rerun the same command with sed/head/tail just to inspect omitted output; use artifact.read/from_end or artifact.grep.]\n{}",
        output.retained_bytes, output.original_bytes, output.text
    )
}

fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_concurrent_plugin_vtable!(
        ShellPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

bcode_plugin_sdk::export_concurrent_plugin!(ShellPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_shell_process_group() {
        let started = Instant::now();
        let response = run_shell_command(
            ServiceEventEmitter::default(),
            &bcode_plugin_sdk::ServiceCancellation::default(),
            "test",
            &ShellRunArguments {
                command: "sh -c 'trap \"\" HUP TERM; sleep 5' | cat".to_string(),
                cwd: None,
                timeout_ms: Some(100),
                terminal: false,
                columns: None,
                rows: None,
            },
            None,
            None,
        )
        .expect("shell command should return timeout output");

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(response.is_error);
        assert!(response.output.contains("timed_out: true"));
    }

    #[test]
    fn limit_output_bytes_truncates_at_utf8_boundary() {
        let output = limit_output_bytes("abcé".as_bytes(), 4);

        assert_eq!(output.text, "abc");
        assert_eq!(output.original_bytes, 5);
        assert_eq!(output.retained_bytes, 3);
        assert!(output.truncated);
    }

    #[test]
    fn inline_stream_output_keeps_head_and_tail_when_truncated() {
        let output = limit_output_bytes(b"head-middle-tail", 16);
        let inline = limit_inline_stream_output(&output, 10);

        assert_eq!(inline.text, "heade-tail");
        assert_eq!(inline.original_bytes, 16);
        assert_eq!(inline.retained_bytes, 10);
        assert!(inline.truncated);
    }

    #[test]
    fn command_output_marks_truncated_streams() {
        let stdout = LimitedOutput {
            text: "headtail".to_string(),
            original_bytes: 16,
            retained_bytes: 8,
            truncated: true,
        };
        let stderr = LimitedOutput {
            text: String::new(),
            original_bytes: 0,
            retained_bytes: 0,
            truncated: false,
        };

        let output = format_command_output("echo test", Some(0), false, false, &stdout, &stderr);

        assert!(output.contains("stdout truncated"));
        assert!(output.contains("showing first and last 8 of 16 retained bytes"));
        assert!(output.contains("headtail"));
    }

    #[test]
    fn command_output_warns_about_output_slicing_pipelines() {
        let stdout = LimitedOutput {
            text: String::new(),
            original_bytes: 0,
            retained_bytes: 0,
            truncated: false,
        };
        let stderr = stdout.clone();

        let output = format_command_output(
            "cargo clippy 2>&1 | sed -n '1,120p'",
            Some(0),
            false,
            false,
            &stdout,
            &stderr,
        );

        assert!(output.contains("pipes output through sed/head/tail"));
    }

    #[cfg(unix)]
    #[test]
    fn shell_pipeline_preserves_failing_left_side_status() {
        let response = run_shell_command(
            ServiceEventEmitter::default(),
            &bcode_plugin_sdk::ServiceCancellation::default(),
            "test",
            &ShellRunArguments {
                command: "false | sed -n '1,1p'".to_string(),
                cwd: None,
                timeout_ms: Some(1_000),
                terminal: false,
                columns: None,
                rows: None,
            },
            None,
            None,
        )
        .expect("shell command should run");

        assert!(response.is_error);
        assert!(response.output.contains("exit_code: 1"));
        assert!(
            response
                .output
                .contains("pipes output through sed/head/tail")
        );
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
