//! Durable JSON compatibility DTOs for persisted session events.
//!
//! These types intentionally live in the session package instead of the IPC
//! package. Persistence DTOs may use JSON-oriented serde behavior for
//! compatibility and migration, while IPC DTOs must remain safe for the
//! non-self-describing `bmux_codec` wire format.

use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, FileChangeResult, SessionEvent, SessionEventKind,
    SessionEventProvenance, SessionId, ShellRunResult, ToolInvocationResult, TraceBlobRef,
};
use serde::Deserialize;
use thiserror::Error;

/// Decode a persisted session event from durable JSON.
///
/// # Errors
///
/// Returns an error when the event is not a supported persisted session-event
/// shape or cannot be converted into the current domain model.
pub fn decode_session_event(payload: &str) -> Result<SessionEvent, PersistedSessionEventError> {
    let persisted = serde_json::from_str::<PersistedSessionEvent>(payload)?;
    persisted.into_domain()
}

/// Errors returned when decoding persisted session events.
#[derive(Debug, Error)]
pub enum PersistedSessionEventError {
    /// Persisted JSON was malformed or incompatible with known DTOs.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Persisted event uses a future schema version not supported by this build.
    #[error(
        "unsupported persisted session event schema version {actual}; current version is {current}"
    )]
    UnsupportedSchemaVersion { actual: u16, current: u16 },
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
    fn into_domain(self) -> Result<SessionEvent, PersistedSessionEventError> {
        if self.schema_version > CURRENT_SESSION_EVENT_SCHEMA_VERSION {
            return Err(PersistedSessionEventError::UnsupportedSchemaVersion {
                actual: self.schema_version,
                current: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            });
        }
        Ok(SessionEvent {
            schema_version: self.schema_version,
            sequence: self.sequence,
            session_id: self.session_id,
            provenance: self.provenance,
            kind: self.kind.into_domain(),
        })
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

    #[test]
    fn decodes_current_and_legacy_persisted_tool_results() {
        for (semantic_result, assertion) in semantic_result_cases() {
            let event = decode_session_event(&event_payload(semantic_result))
                .expect("persisted event should decode");

            assertion(event);
        }
    }

    #[test]
    fn rejects_future_schema_version() {
        let payload = serde_json::json!({
            "schema_version": CURRENT_SESSION_EVENT_SCHEMA_VERSION + 1,
            "sequence": 1,
            "session_id": SessionId::new(),
            "kind": { "assistant_message": { "text": "future" } }
        })
        .to_string();

        let error = decode_session_event(&payload).expect_err("future schema should fail");

        assert!(matches!(
            error,
            PersistedSessionEventError::UnsupportedSchemaVersion { .. }
        ));
    }

    #[test]
    fn rejects_corrupt_event_payload() {
        let payload = serde_json::json!({
            "schema_version": CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            "sequence": 1,
            "session_id": SessionId::new(),
            "kind": { "tool_call_finished": { "result": "missing id" } }
        })
        .to_string();

        let error = decode_session_event(&payload).expect_err("corrupt event should fail");

        assert!(matches!(error, PersistedSessionEventError::Json(_)));
    }

    type PersistedAssertion = fn(SessionEvent);

    fn semantic_result_cases() -> Vec<(serde_json::Value, PersistedAssertion)> {
        vec![
            (
                serde_json::json!({ "type": "text", "text": "plain" }),
                assert_text_result,
            ),
            (
                serde_json::json!({ "type": "json", "value": "{\"ok\":true}" }),
                assert_json_result,
            ),
            (
                serde_json::json!({
                    "type": "file_change",
                    "result": {
                        "tool_name": "filesystem.write",
                        "summary": "wrote bytes",
                        "path": "file.txt",
                        "future_field": "ignored"
                    },
                    "future_top_level": "ignored"
                }),
                assert_file_change_result,
            ),
            (
                serde_json::json!({
                    "type": "shell_run",
                    "result": {
                        "mode": "terminal",
                        "output": "legacy tail"
                    }
                }),
                assert_legacy_terminal_result,
            ),
            (
                serde_json::json!({
                    "type": "shell_run",
                    "result": {
                        "mode": "captured",
                        "stdout": "hello\n",
                        "stderr": ""
                    }
                }),
                assert_captured_result,
            ),
        ]
    }

    fn event_payload(semantic_result: serde_json::Value) -> String {
        serde_json::json!({
            "schema_version": CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            "sequence": 1,
            "session_id": SessionId::new(),
            "provenance": null,
            "kind": {
                "tool_call_finished": {
                    "tool_call_id": "call-1",
                    "result": "legacy",
                    "semantic_result": semantic_result
                }
            }
        })
        .to_string()
    }

    fn tool_result(event: SessionEvent) -> ToolInvocationResult {
        let SessionEventKind::ToolCallFinished {
            semantic_result: Some(result),
            ..
        } = event.kind
        else {
            panic!("expected semantic tool result");
        };
        result
    }

    fn assert_text_result(event: SessionEvent) {
        assert_eq!(
            tool_result(event),
            ToolInvocationResult::Text {
                text: "plain".to_string(),
            }
        );
    }

    fn assert_json_result(event: SessionEvent) {
        assert_eq!(
            tool_result(event),
            ToolInvocationResult::Json {
                value: r#"{"ok":true}"#.to_string(),
            }
        );
    }

    fn assert_file_change_result(event: SessionEvent) {
        assert_eq!(
            tool_result(event),
            ToolInvocationResult::FileChange {
                result: FileChangeResult {
                    tool_name: "filesystem.write".to_string(),
                    summary: "wrote bytes".to_string(),
                    path: Some("file.txt".to_string()),
                },
            }
        );
    }

    fn assert_legacy_terminal_result(event: SessionEvent) {
        assert_eq!(
            tool_result(event),
            ToolInvocationResult::ShellRun {
                result: ShellRunResult::Terminal {
                    exit_code: None,
                    timed_out: false,
                    cancelled: false,
                    output_tail: "legacy tail".to_string(),
                    output_truncated: false,
                    output_bytes: None,
                    retained_output_bytes: None,
                    columns: 80,
                    rows: 24,
                },
            }
        );
    }

    fn assert_captured_result(event: SessionEvent) {
        assert_eq!(
            tool_result(event),
            ToolInvocationResult::ShellRun {
                result: ShellRunResult::Captured {
                    exit_code: None,
                    timed_out: false,
                    cancelled: false,
                    stdout: "hello\n".to_string(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    stdout_bytes: None,
                    stderr_bytes: None,
                },
            }
        );
    }
}
