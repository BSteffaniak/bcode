//! Legacy persisted compatibility for historical session logs.
//!
//! This module exists only to decode pre-artifact tool result/presentation
//! shapes. Do not add new encode paths here. Delete this module when legacy log
//! compatibility is dropped.

use bcode_session_models::ToolInvocationResult;
use serde::{Deserialize, Serialize};

/// Persisted legacy presentation DTO retained only to decode old session logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInvocationPresentation {
    Terminal {
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        timed_out: bool,
        #[serde(default)]
        cancelled: bool,
        #[serde(default)]
        output: String,
        #[serde(default)]
        output_truncated: bool,
        #[serde(default)]
        output_bytes: Option<u64>,
        #[serde(default)]
        retained_output_bytes: Option<u64>,
        #[serde(default = "default_terminal_columns")]
        columns: u16,
        #[serde(default = "default_terminal_rows")]
        rows: u16,
    },
    FileChange {
        tool_name: String,
        summary: String,
        #[serde(default)]
        path: Option<String>,
    },
}

#[must_use]
pub fn semantic_from_presentation(
    presentation: &ToolInvocationPresentation,
) -> ToolInvocationResult {
    ToolInvocationResult::Text {
        text: presentation_result_text(presentation),
    }
}

#[must_use]
pub fn presentation_result_text(presentation: &ToolInvocationPresentation) -> String {
    match presentation {
        ToolInvocationPresentation::Terminal { output, .. } => output.clone(),
        ToolInvocationPresentation::FileChange { summary, .. } => summary.clone(),
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolInvocationResultCompat {
    ShellRun { result: ShellRunResult },
    FileChange { result: FileChangeResult },
}

impl ToolInvocationResultCompat {
    #[must_use]
    pub fn into_domain(self) -> ToolInvocationResult {
        match self {
            Self::ShellRun { result } => ToolInvocationResult::Json {
                value: serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string()),
            },
            Self::FileChange { result } => ToolInvocationResult::Json {
                value: serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string()),
            },
        }
    }
}

/// Legacy shell-run result DTO.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ShellRunResult {
    Terminal {
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        timed_out: bool,
        #[serde(default)]
        cancelled: bool,
        #[serde(default)]
        duration_ms: Option<u64>,
        #[serde(default, alias = "output")]
        output_tail: String,
        #[serde(default)]
        output_truncated: bool,
        #[serde(default)]
        output_bytes: Option<u64>,
        #[serde(default)]
        retained_output_bytes: Option<u64>,
        #[serde(default = "default_terminal_columns")]
        columns: u16,
        #[serde(default = "default_terminal_rows")]
        rows: u16,
    },
    Captured {
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        timed_out: bool,
        #[serde(default)]
        cancelled: bool,
        #[serde(default)]
        duration_ms: Option<u64>,
        #[serde(default)]
        stdout: String,
        #[serde(default)]
        stderr: String,
        #[serde(default)]
        stdout_truncated: bool,
        #[serde(default)]
        stderr_truncated: bool,
        #[serde(default)]
        stdout_bytes: Option<u64>,
        #[serde(default)]
        stderr_bytes: Option<u64>,
    },
}

/// Legacy filesystem-change result DTO.
#[derive(Debug, Serialize, Deserialize)]
pub struct FileChangeResult {
    tool_name: String,
    summary: String,
    #[serde(default)]
    path: Option<String>,
}

const fn default_terminal_columns() -> u16 {
    80
}

const fn default_terminal_rows() -> u16 {
    24
}
