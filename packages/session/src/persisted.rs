//! Durable JSON compatibility DTOs for persisted session events.
//!
//! These types intentionally live in the session package instead of the IPC
//! package. Persistence DTOs may use JSON-oriented serde behavior for
//! compatibility and migration, while IPC DTOs must remain safe for the
//! non-self-describing `bmux_codec` wire format.

use bcode_session_models::{
    FileChangeResult, SessionEvent, SessionEventKind, SessionEventProvenance, SessionId,
    ShellRunResult, ToolInvocationResult, TraceBlobRef,
};
use serde::Deserialize;

/// Decode a persisted session event from durable JSON.
///
/// # Errors
///
/// Returns an error when the event is not a supported persisted session-event
/// shape or cannot be converted into the current domain model.
pub fn decode_session_event(payload: &str) -> Result<SessionEvent, serde_json::Error> {
    serde_json::from_str::<PersistedSessionEvent>(payload).map(PersistedSessionEvent::into_domain)
}

/// Persisted session event DTO.
#[derive(Debug, Deserialize)]
struct PersistedSessionEvent {
    schema_version: u16,
    sequence: u64,
    session_id: SessionId,
    #[serde(default)]
    provenance: Option<SessionEventProvenance>,
    kind: PersistedSessionEventKind,
}

impl PersistedSessionEvent {
    fn into_domain(self) -> SessionEvent {
        SessionEvent {
            schema_version: self.schema_version,
            sequence: self.sequence,
            session_id: self.session_id,
            provenance: self.provenance,
            kind: self.kind.into_domain(),
        }
    }
}

/// Persisted session event kind DTO.
#[derive(Debug)]
enum PersistedSessionEventKind {
    ToolCallFinished {
        tool_call_id: String,
        result: String,
        is_error: bool,
        output: Option<TraceBlobRef>,
        semantic_result: Option<PersistedToolInvocationResult>,
    },
    Domain(SessionEventKind),
}

impl<'de> Deserialize<'de> for PersistedSessionEventKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(payload) = value.get("tool_call_finished") {
            return serde_json::from_value::<PersistedToolCallFinished>(payload.clone())
                .map(|payload| Self::ToolCallFinished {
                    tool_call_id: payload.tool_call_id,
                    result: payload.result,
                    is_error: payload.is_error,
                    output: payload.output,
                    semantic_result: payload.semantic_result,
                })
                .map_err(serde::de::Error::custom);
        }
        serde_json::from_value::<SessionEventKind>(value)
            .map(Self::Domain)
            .map_err(serde::de::Error::custom)
    }
}

impl PersistedSessionEventKind {
    fn into_domain(self) -> SessionEventKind {
        match self {
            Self::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
                output,
                semantic_result,
            } => SessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
                output,
                semantic_result: semantic_result.map(PersistedToolInvocationResult::into_domain),
            },
            Self::Domain(kind) => kind,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PersistedToolCallFinished {
    tool_call_id: String,
    result: String,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    output: Option<TraceBlobRef>,
    #[serde(default)]
    semantic_result: Option<PersistedToolInvocationResult>,
}

/// Persisted semantic tool result DTO.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PersistedToolInvocationResult {
    Text { text: String },
    Json { value: String },
    ShellRun { result: PersistedShellRunResult },
    FileChange { result: FileChangeResult },
}

impl PersistedToolInvocationResult {
    fn into_domain(self) -> ToolInvocationResult {
        match self {
            Self::Text { text } => ToolInvocationResult::Text { text },
            Self::Json { value } => ToolInvocationResult::Json { value },
            Self::ShellRun { result } => ToolInvocationResult::ShellRun {
                result: result.into_domain(),
            },
            Self::FileChange { result } => ToolInvocationResult::FileChange { result },
        }
    }
}

/// Persisted shell-run result DTO.
#[derive(Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum PersistedShellRunResult {
    Terminal {
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        timed_out: bool,
        #[serde(default)]
        cancelled: bool,
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

impl PersistedShellRunResult {
    fn into_domain(self) -> ShellRunResult {
        match self {
            Self::Terminal {
                exit_code,
                timed_out,
                cancelled,
                output_tail,
                output_truncated,
                output_bytes,
                retained_output_bytes,
                columns,
                rows,
            } => ShellRunResult::Terminal {
                exit_code,
                timed_out,
                cancelled,
                output_tail,
                output_truncated,
                output_bytes,
                retained_output_bytes,
                columns,
                rows,
            },
            Self::Captured {
                exit_code,
                timed_out,
                cancelled,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
                stdout_bytes,
                stderr_bytes,
            } => ShellRunResult::Captured {
                exit_code,
                timed_out,
                cancelled,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
                stdout_bytes,
                stderr_bytes,
            },
        }
    }
}

const fn default_terminal_columns() -> u16 {
    80
}

const fn default_terminal_rows() -> u16 {
    24
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION;

    #[test]
    fn decodes_legacy_terminal_output_alias() {
        let session_id = SessionId::new();
        let payload = serde_json::json!({
            "schema_version": CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            "sequence": 1,
            "session_id": session_id,
            "provenance": null,
            "kind": {
                "tool_call_finished": {
                    "tool_call_id": "call-1",
                    "result": "legacy",
                    "semantic_result": {
                        "type": "shell_run",
                        "result": {
                            "mode": "terminal",
                            "output": "legacy tail"
                        }
                    }
                }
            }
        })
        .to_string();

        let event = decode_session_event(&payload).expect("persisted event should decode");

        assert!(matches!(
            event.kind,
            SessionEventKind::ToolCallFinished {
                semantic_result: Some(ToolInvocationResult::ShellRun {
                    result: ShellRunResult::Terminal { output_tail, .. },
                }),
                ..
            } if output_tail == "legacy tail"
        ));
    }
}
