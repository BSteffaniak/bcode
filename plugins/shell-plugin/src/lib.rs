#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled shell execution tool plugin for Bcode.

use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolDefinition,
    ToolInvocationRequest, ToolInvocationResponse, ToolList, ToolSideEffect,
};
use bcode_tool_runtime::{ProcessExecutionRequest, ToolExecutionRuntime};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_TERMINAL_COLUMNS: u16 = 120;
const DEFAULT_TERMINAL_ROWS: u16 = 30;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Bundled shell plugin.
#[derive(Default)]
pub struct ShellPlugin;

impl RustPlugin for ShellPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != TOOL_SERVICE_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported shell plugin service interface",
            );
        }

        match context.request.operation.as_str() {
            OP_LIST_TOOLS => list_tools(&context.request),
            OP_INVOKE_TOOL => invoke_tool(&context.request),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported tool service operation",
            ),
        }
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

fn invoke_tool(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ToolInvocationRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let response = match request.name.as_str() {
        "shell.run" => run_shell_tool(request.arguments, request.cwd.as_deref()),
        _ => ToolInvocationResponse {
            output: format!("unknown shell tool: {}", request.name),
            is_error: true,
        },
    };
    json_response(&response)
}

fn run_shell_tool(
    arguments: serde_json::Value,
    session_cwd: Option<&std::path::Path>,
) -> ToolInvocationResponse {
    let arguments = match serde_json::from_value::<ShellRunArguments>(arguments) {
        Ok(arguments) => arguments,
        Err(error) => {
            return ToolInvocationResponse {
                output: error.to_string(),
                is_error: true,
            };
        }
    };
    if arguments.command.trim().is_empty() {
        return ToolInvocationResponse {
            output: "command must not be empty".to_string(),
            is_error: true,
        };
    }
    if arguments.terminal {
        return run_terminal_shell_command(&arguments, session_cwd);
    }
    match run_shell_command(&arguments, session_cwd) {
        Ok(output) => output,
        Err(error) => ToolInvocationResponse {
            output: error,
            is_error: true,
        },
    }
}

#[derive(Debug, Serialize)]
struct TerminalCommandOutput {
    mode: &'static str,
    exit_code: Option<i32>,
    timed_out: bool,
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

fn run_terminal_shell_command(
    arguments: &ShellRunArguments,
    session_cwd: Option<&Path>,
) -> ToolInvocationResponse {
    match run_terminal_shell_command_inner(arguments, session_cwd) {
        Ok(response) => response,
        Err(error) => ToolInvocationResponse {
            output: error,
            is_error: true,
        },
    }
}

fn run_terminal_shell_command_inner(
    arguments: &ShellRunArguments,
    session_cwd: Option<&Path>,
) -> Result<ToolInvocationResponse, String> {
    let timeout = Duration::from_millis(arguments.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
    let columns = arguments.columns.unwrap_or(DEFAULT_TERMINAL_COLUMNS).max(1);
    let rows = arguments.rows.unwrap_or(DEFAULT_TERMINAL_ROWS).max(1);
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows,
            cols: columns,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| error.to_string())?;

    let mut command = portable_pty::CommandBuilder::new("sh");
    command.arg("-c");
    command.arg(&arguments.command);
    if let Some(cwd) = arguments.cwd.as_deref().or(session_cwd) {
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
    let reader_thread = std::thread::spawn(move || read_limited(&mut reader));

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break status;
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            child.kill().map_err(|error| error.to_string())?;
            break child.wait().map_err(|error| error.to_string())?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(pair.master);
    let output = join_reader(reader_thread)?;
    let terminal_output = TerminalCommandOutput {
        mode: "terminal",
        exit_code: Some(i32::try_from(status.exit_code()).unwrap_or(i32::MAX)),
        timed_out,
        output: output.text,
        output_truncated: output.truncated,
        output_bytes: u64::try_from(output.original_bytes).unwrap_or(u64::MAX),
        retained_output_bytes: u64::try_from(output.retained_bytes).unwrap_or(u64::MAX),
        columns,
        rows,
    };
    let encoded = serde_json::to_string(&terminal_output).map_err(|error| error.to_string())?;
    Ok(ToolInvocationResponse {
        output: encoded,
        is_error: timed_out || !status.success(),
    })
}

fn run_shell_command(
    arguments: &ShellRunArguments,
    session_cwd: Option<&std::path::Path>,
) -> Result<ToolInvocationResponse, String> {
    let timeout = Duration::from_millis(arguments.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
    let cwd = arguments
        .cwd
        .as_deref()
        .or(session_cwd)
        .map(Path::to_path_buf);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let result = runtime
        .block_on(async {
            ToolExecutionRuntime::new(1)
                .run_process(ProcessExecutionRequest {
                    program: shell_program().to_string(),
                    args: shell_args(&arguments.command),
                    cwd,
                    timeout: Some(timeout),
                    max_output_bytes: MAX_OUTPUT_BYTES,
                })
                .await
        })
        .map_err(|error| error.to_string())?;

    let stdout = limit_output_bytes(&result.stdout, MAX_OUTPUT_BYTES);
    let stderr = limit_output_bytes(&result.stderr, MAX_OUTPUT_BYTES);
    let output = format_command_output(
        result.exit_code,
        result.timed_out,
        &stdout.text,
        &stderr.text,
    );
    Ok(ToolInvocationResponse {
        output,
        is_error: result.timed_out || result.exit_code.is_none_or(|code| code != 0),
    })
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
    vec!["-c".to_string(), command.to_string()]
}

#[cfg(windows)]
fn shell_args(command: &str) -> Vec<String> {
    vec!["/C".to_string(), command.to_string()]
}

fn read_limited<R>(mut reader: R) -> Result<LimitedOutput, String>
where
    R: Read,
{
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(limit_output_bytes(&bytes, MAX_OUTPUT_BYTES))
}

fn limit_output_bytes(bytes: &[u8], max_bytes: usize) -> LimitedOutput {
    let original_bytes = bytes.len();
    let retained_len = valid_utf8_prefix_len(bytes, max_bytes.min(original_bytes));
    let text = String::from_utf8_lossy(&bytes[..retained_len]).into_owned();
    LimitedOutput {
        text,
        original_bytes,
        retained_bytes: retained_len,
        truncated: retained_len < original_bytes,
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
    handle: std::thread::JoinHandle<Result<LimitedOutput, String>>,
) -> Result<LimitedOutput, String> {
    handle
        .join()
        .map_err(|_| "output reader thread panicked".to_string())?
}

fn format_command_output(
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: &str,
    stderr: &str,
) -> String {
    let exit_code = exit_code.map_or_else(|| "signal".to_string(), |code| code.to_string());
    format!("exit_code: {exit_code}\ntimed_out: {timed_out}\nstdout:\n{stdout}\nstderr:\n{stderr}")
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
    bcode_plugin_sdk::static_plugin_vtable!(ShellPlugin, include_str!("../bcode-plugin.toml"))
}

bcode_plugin_sdk::export_plugin!(ShellPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_shell_process_group() {
        let started = Instant::now();
        let response = run_shell_command(
            &ShellRunArguments {
                command: "sh -c 'trap \"\" HUP TERM; sleep 5' | cat".to_string(),
                cwd: None,
                timeout_ms: Some(100),
                terminal: false,
                columns: None,
                rows: None,
            },
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
    fn terminal_output_json_stays_valid_when_output_is_truncated() {
        let bytes = vec![b'x'; MAX_OUTPUT_BYTES + 1];
        let output = limit_output_bytes(&bytes, MAX_OUTPUT_BYTES);
        let terminal_output = TerminalCommandOutput {
            mode: "terminal",
            exit_code: Some(0),
            timed_out: false,
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
