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
/// Schema for unsolicited shell invocation controls.
pub const SHELL_INVOCATION_INPUT_SCHEMA: &str = "bcode.shell.invocation-input";
/// Current version of all JSON shell invocation schemas above.
pub const SHELL_SCHEMA_VERSION: u32 = 1;
/// Default shell execution timeout in milliseconds.
pub const DEFAULT_SHELL_TIMEOUT_MS: u64 = 30_000;

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

/// Legacy typed command-plan workflow block contract version.
pub const SHELL_COMMAND_PLAN_VERSION_1: u32 = 1;
/// Current typed command-plan workflow block contract version.
pub const SHELL_COMMAND_PLAN_VERSION: u32 = 2;

/// One argv-mode command. No implicit shell-string parsing is performed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShellWorkflowCommand {
    pub argv: Vec<String>,
    pub timeout_ms: u64,
    /// Version-1 continuation policy retained for exact compatibility.
    #[serde(default)]
    pub continue_on_nonzero: bool,
    /// Accepted process exit codes. Version 1 always uses `[0]`; version 2 defaults to `[0]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_exit_codes: Option<Vec<i32>>,
    /// Whether version 2 continues after an exited code outside the accepted set.
    #[serde(default)]
    pub continue_on_unaccepted_exit: bool,
}

/// Explicit environment policy for a workflow command plan.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShellWorkflowEnvironment {
    pub inherit: bool,
    /// Explicit non-secret environment values. Secret-bearing names are rejected; workflows must
    /// use owner/runtime secret injection that is not persisted in the command plan.
    pub set: std::collections::BTreeMap<String, String>,
}

/// Bounded output retention policy.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShellWorkflowOutputPolicy {
    pub preview_bytes: u32,
    pub artifact_spill: bool,
}

/// Typed procedural workflow command plan.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShellWorkflowCommandPlan {
    pub version: u32,
    pub cwd: PathBuf,
    pub commands: Vec<ShellWorkflowCommand>,
    pub environment: ShellWorkflowEnvironment,
    pub output: ShellWorkflowOutputPolicy,
}

/// Stable terminal state for one command in a workflow plan.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellWorkflowCommandStatus {
    Exited,
    Signaled,
    SpawnFailed,
    TimedOut,
    Cancelled,
}

/// Typed result for one command, preserving declaration order by index.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShellWorkflowCommandResult {
    pub index: u32,
    pub status: ShellWorkflowCommandStatus,
    pub exit_code: Option<i32>,
    /// Exact accepted exit codes used to classify this command.
    pub accepted_exit_codes: Vec<i32>,
    /// Whether an ordinary exited code was contained in `accepted_exit_codes`.
    /// Version-1 results omit this field for exact compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_accepted: Option<bool>,
    pub signal: Option<i32>,
    pub duration_ms: u64,
    pub stdout_preview: String,
    pub stderr_preview: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// Typed final command-plan result.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShellWorkflowCommandPlanResult {
    pub version: u32,
    /// Canonical SHA-256 of the exact normalized command plan executed by the shell owner.
    pub plan_sha256: String,
    pub passed: bool,
    pub commands: Vec<ShellWorkflowCommandResult>,
    pub artifacts: Vec<bcode_workflow::ArtifactReference>,
}

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
    pub timeout_ms: u64,
    pub arguments: serde_json::Value,
}

const fn default_format_commands() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_command_plan_contract_is_versioned_bounded_and_argv_explicit() {
        let plan = ShellWorkflowCommandPlan {
            version: SHELL_COMMAND_PLAN_VERSION,
            cwd: PathBuf::from("workspace"),
            commands: vec![ShellWorkflowCommand {
                argv: vec!["cargo".to_string(), "test".to_string()],
                timeout_ms: 30_000,
                continue_on_nonzero: false,
                accepted_exit_codes: None,
                continue_on_unaccepted_exit: false,
            }],
            environment: ShellWorkflowEnvironment {
                inherit: false,
                set: std::collections::BTreeMap::from([("CI".to_string(), "true".to_string())]),
            },
            output: ShellWorkflowOutputPolicy {
                preview_bytes: 4096,
                artifact_spill: true,
            },
        };
        let payload = serde_json::to_value(&plan).expect("encode");
        assert_eq!(payload["version"], SHELL_COMMAND_PLAN_VERSION);
        assert_eq!(
            payload["commands"][0]["argv"],
            serde_json::json!(["cargo", "test"])
        );
        assert_eq!(
            serde_json::from_value::<ShellWorkflowCommandPlan>(payload).expect("decode"),
            plan
        );
    }

    #[test]
    fn workflow_command_plan_v1_decodes_without_v2_fields() {
        let payload = serde_json::json!({
            "version": SHELL_COMMAND_PLAN_VERSION_1,
            "cwd": ".",
            "commands": [{
                "argv": ["true"],
                "timeout_ms": 1_000,
                "continue_on_nonzero": false
            }],
            "environment": {"inherit": false, "set": {}},
            "output": {"preview_bytes": 1_024, "artifact_spill": false}
        });
        let plan: ShellWorkflowCommandPlan = serde_json::from_value(payload).expect("version 1");
        assert_eq!(plan.version, SHELL_COMMAND_PLAN_VERSION_1);
        assert_eq!(plan.commands[0].accepted_exit_codes, None);
        assert!(!plan.commands[0].continue_on_unaccepted_exit);
    }

    #[test]
    fn workflow_command_plan_result_carries_terminal_detail_and_artifacts() {
        let result = ShellWorkflowCommandPlanResult {
            version: SHELL_COMMAND_PLAN_VERSION,
            plan_sha256: "a".repeat(64),
            passed: false,
            commands: vec![ShellWorkflowCommandResult {
                index: 0,
                status: ShellWorkflowCommandStatus::Exited,
                exit_code: Some(1),
                accepted_exit_codes: vec![0],
                exit_accepted: Some(false),
                signal: None,
                duration_ms: 12,
                stdout_preview: String::new(),
                stderr_preview: "failed".to_string(),
                stdout_truncated: false,
                stderr_truncated: true,
            }],
            artifacts: vec![bcode_workflow::ArtifactReference::new(
                "stderr-1",
                "bcode.shell.stderr",
                1,
                "text/plain",
                "shell/stderr-1.txt",
            )],
        };
        let payload = serde_json::to_value(&result).expect("encode");
        assert_eq!(payload["passed"], false);
        assert_eq!(payload["plan_sha256"], "a".repeat(64));
        assert_eq!(payload["commands"][0]["status"], "exited");
        assert_eq!(payload["artifacts"][0]["artifact_id"], "stderr-1");
    }

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
