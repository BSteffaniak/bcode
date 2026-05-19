#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled shell execution tool plugin for Bcode.

use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolDefinition,
    ToolInvocationRequest, ToolInvocationResponse, ToolList, ToolSideEffect,
};
use serde::Deserialize;
use serde_json::json;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const TERMINATION_GRACE_MS: u64 = 500;
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
}

fn list_tools(request: &ServiceRequest) -> ServiceResponse {
    if let Err(error) = request.payload_json::<ListToolsRequest>() {
        return invalid_request(&error);
    }
    json_response(&ToolList {
        tools: vec![ToolDefinition {
            name: "shell.run".to_string(),
            description: "Run a shell command with stdout/stderr capture".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 1 }
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
        "shell.run" => run_shell_tool(request.arguments),
        _ => ToolInvocationResponse {
            output: format!("unknown shell tool: {}", request.name),
            is_error: true,
        },
    };
    json_response(&response)
}

fn run_shell_tool(arguments: serde_json::Value) -> ToolInvocationResponse {
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
    match run_shell_command(&arguments) {
        Ok(output) => output,
        Err(error) => ToolInvocationResponse {
            output: error,
            is_error: true,
        },
    }
}

fn run_shell_command(arguments: &ShellRunArguments) -> Result<ToolInvocationResponse, String> {
    let timeout = Duration::from_millis(arguments.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
    let mut command = shell_command(&arguments.command);
    configure_command_for_timeout(&mut command);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(cwd) = &arguments.cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture stderr".to_string())?;
    let stdout_reader = std::thread::spawn(move || read_limited(stdout));
    let stderr_reader = std::thread::spawn(move || read_limited(stderr));

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break status;
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            break terminate_child_after_timeout(&mut child)?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let stdout = join_reader(stdout_reader)?;
    let stderr = join_reader(stderr_reader)?;
    let output = format_command_output(status.code(), timed_out, &stdout, &stderr);
    Ok(ToolInvocationResponse {
        output,
        is_error: timed_out || !status.success(),
    })
}

#[cfg(unix)]
fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("sh");
    shell.arg("-c").arg(command);
    shell
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("cmd");
    shell.arg("/C").arg(command);
    shell
}

#[cfg(unix)]
fn configure_command_for_timeout(command: &mut Command) {
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_command_for_timeout(_command: &mut Command) {}

#[cfg(unix)]
fn terminate_child_after_timeout(child: &mut Child) -> Result<ExitStatus, String> {
    let process_group_id = i32::try_from(child.id()).map_err(|error| error.to_string())?;
    send_signal_to_process_group(process_group_id, SIGTERM)?;
    let grace_started = Instant::now();
    let mut status = None;
    loop {
        if status.is_none() {
            status = child.try_wait().map_err(|error| error.to_string())?;
        }
        if status.is_some()
            || grace_started.elapsed() >= Duration::from_millis(TERMINATION_GRACE_MS)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    send_signal_to_process_group(process_group_id, SIGKILL)?;
    status.map_or_else(|| child.wait().map_err(|error| error.to_string()), Ok)
}

#[cfg(not(unix))]
fn terminate_child_after_timeout(child: &mut Child) -> Result<ExitStatus, String> {
    child.kill().map_err(|error| error.to_string())?;
    child.wait().map_err(|error| error.to_string())
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
fn send_signal_to_process_group(process_group_id: i32, signal: i32) -> Result<(), String> {
    let target = process_group_id
        .checked_neg()
        .ok_or_else(|| "process group id cannot be negated".to_string())?;
    // SAFETY: `kill` is called with a negative process id to target the child process group
    // created by `CommandExt::process_group(0)`. The signal constants are valid POSIX signals.
    let result = unsafe { kill(target, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ESRCH) {
        return Ok(());
    }
    Err(error.to_string())
}

fn read_limited<R>(mut reader: R) -> Result<String, String>
where
    R: Read,
{
    let mut bytes = Vec::new();
    let limit = u64::try_from(MAX_OUTPUT_BYTES).map_err(|error| error.to_string())?;
    reader
        .by_ref()
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn join_reader(handle: std::thread::JoinHandle<Result<String, String>>) -> Result<String, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn shell_command_preserves_inherited_path() {
        let inherited_path = "/tmp/bcode-path-sentinel:/usr/bin:/bin";
        let output = shell_command("printf '%s' \"$PATH\"")
            .env("PATH", inherited_path)
            .output()
            .expect("shell command should run");

        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), inherited_path);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_shell_process_group() {
        let started = Instant::now();
        let response = run_shell_command(&ShellRunArguments {
            command: "sh -c 'trap \"\" HUP TERM; sleep 5' | cat".to_string(),
            cwd: None,
            timeout_ms: Some(100),
        })
        .expect("shell command should return timeout output");

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(response.is_error);
        assert!(response.output.contains("timed_out: true"));
    }
}

bcode_plugin_sdk::export_plugin!(ShellPlugin, include_str!("../bcode-plugin.toml"));
