//! Versioned shell-owned invocation, stream, control, and recording contracts.
//!
//! These contracts are interpreted only by the shell plugin and its platform adapters. Generic
//! runtime, persistence, and renderer code transports their payloads opaquely.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Model-callable shell execution tool name.
pub const SHELL_RUN_TOOL_NAME: &str = "shell.run";
/// Schema for shell execution result artifacts and live recording contributions.
pub const SHELL_RUN_SCHEMA: &str = "bcode.shell.run";
/// Schema for durable shell execution summaries.
pub const SHELL_RUN_SUMMARY_SCHEMA: &str = "bcode.shell.run.summary";
/// Schema for unsolicited shell invocation controls.
pub const SHELL_INVOCATION_INPUT_SCHEMA: &str = "bcode.shell.invocation-input";
/// Current version of all JSON shell invocation schemas above.
pub const SHELL_SCHEMA_VERSION: u32 = 1;

/// Raw PTY byte-stream artifact reference key.
pub const TERMINAL_PTY_STREAM_REF_KEY: &str = "terminal_pty_stream";
/// Raw PTY byte-stream content type.
pub const TERMINAL_PTY_STREAM_CONTENT_TYPE: &str =
    "application/x-bcode-terminal-pty-stream; charset=utf-8";
/// Authoritative shell recording artifact reference key.
pub const SHELL_RECORDING_REF_KEY: &str = "shell_recording";
/// Authoritative shell recording media type, without a format-version parameter.
#[cfg(feature = "static-bundled")]
pub const SHELL_RECORDING_MEDIA_TYPE: &str = "application/x-bcode-shell-recording";
/// Current authoritative shell recording content type.
pub const SHELL_RECORDING_CONTENT_TYPE: &str = "application/x-bcode-shell-recording; version=3";

/// Input payload for one shell execution invocation.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShellRunArguments {
    pub command: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub columns: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    pub format_commands: Option<bool>,
}

impl ShellRunArguments {
    pub const fn terminal_columns(&self, default: u16) -> u16 {
        match self.columns {
            Some(columns) if columns > 0 => columns,
            _ => default,
        }
    }

    pub const fn terminal_rows(&self, default: u16) -> u16 {
        match self.rows {
            Some(rows) if rows > 0 => rows,
            _ => default,
        }
    }
}

/// Final shell-owned execution result payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ShellRunResult {
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
        #[serde(default = "default_format_commands")]
        format_commands: bool,
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

/// Unsolicited control payload delivered to an active shell invocation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShellInvocationAction {
    Resize { columns: u16, rows: u16 },
}

/// Payload accompanying an incrementally committed shell recording artifact.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ShellLiveRecordingPayload {
    pub mode: &'static str,
}

const fn default_format_commands() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_control_schema_round_trips_resize() {
        let action = ShellInvocationAction::Resize {
            columns: 120,
            rows: 40,
        };
        let payload = serde_json::to_value(&action).expect("encode shell control");
        assert_eq!(payload["type"], "resize");
        assert_eq!(
            serde_json::from_value::<ShellInvocationAction>(payload).expect("decode shell control"),
            action
        );
    }

    #[test]
    fn shell_execution_result_schema_round_trips_terminal_metadata() {
        let result = ShellRunResult::Terminal {
            exit_code: Some(0),
            timed_out: false,
            cancelled: false,
            duration_ms: Some(12),
            output_tail: "done".to_owned(),
            output_truncated: false,
            output_bytes: Some(4),
            retained_output_bytes: Some(4),
            columns: 80,
            rows: 24,
            format_commands: true,
        };
        let payload = serde_json::to_value(&result).expect("encode shell result");
        assert_eq!(payload["mode"], "terminal");
        assert_eq!(
            serde_json::from_value::<ShellRunResult>(payload).expect("decode shell result"),
            result
        );
    }
}
