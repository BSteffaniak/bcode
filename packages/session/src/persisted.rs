//! Durable JSON compatibility DTOs for persisted session events.
//!
//! These types intentionally live in the session package instead of the IPC
//! package. Persistence DTOs may use JSON-oriented serde behavior for
//! compatibility and migration, while IPC DTOs must remain safe for the
//! non-self-describing `bmux_codec` wire format.

use bcode_session_models::*;
use bcode_skill_models::{SkillActivationMode, SkillId, SkillSource};
use serde::Deserialize;
use std::path::PathBuf;
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
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedSessionEventKind {
    SessionCreated {
        name: Option<String>,
        #[serde(default)]
        working_directory: PathBuf,
    },
    ClientAttached {
        client_id: ClientId,
    },
    ClientDetached {
        client_id: ClientId,
    },
    UserMessage {
        client_id: ClientId,
        text: String,
    },
    AssistantDelta {
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    ToolCallRequested {
        tool_call_id: String,
        tool_name: String,
        arguments_json: String,
    },
    ToolCallFinished {
        tool_call_id: String,
        result: String,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        output: Option<TraceBlobRef>,
        #[serde(default)]
        semantic_result: Option<PersistedToolInvocationResult>,
    },
    PermissionRequested {
        permission_id: String,
        tool_call_id: String,
        tool_name: String,
        arguments_json: String,
    },
    PermissionResolved {
        permission_id: String,
        approved: bool,
    },
    ModelChanged {
        provider: String,
        model: String,
    },
    SystemMessage {
        text: String,
    },
    AgentChanged {
        agent_id: String,
    },
    ModelTurnStarted {
        turn_id: String,
    },
    ModelTurnFinished {
        turn_id: String,
        outcome: ModelTurnOutcome,
        #[serde(default)]
        message: Option<String>,
    },
    ModelUsage {
        turn_id: String,
        usage: SessionTokenUsage,
    },
    ContextCompacted {
        summary: String,
        compacted_through_sequence: u64,
    },
    SessionRenamed {
        name: Option<String>,
    },
    TraceEvent {
        trace: Box<SessionTraceEvent>,
    },
    SkillInvoked {
        skill_id: SkillId,
        arguments: String,
        #[serde(default)]
        source: Option<SkillSource>,
        invoked_at_ms: u64,
    },
    SkillSuggested {
        skill_id: SkillId,
        #[serde(default)]
        reason: Option<String>,
        suggested_at_ms: u64,
    },
    SkillActivated {
        skill_id: SkillId,
        #[serde(default)]
        source: Option<SkillSource>,
        mode: SkillActivationMode,
        activated_at_ms: u64,
    },
    SkillDeactivated {
        skill_id: SkillId,
        deactivated_at_ms: u64,
    },
    SkillContextLoaded {
        skill_id: SkillId,
        bytes_loaded: usize,
        truncated: bool,
        loaded_at_ms: u64,
    },
    SkillInvocationFailed {
        skill_id: SkillId,
        error: String,
        failed_at_ms: u64,
    },
    /// Provider-exposed reasoning text delta.
    AssistantReasoningDelta {
        text: String,
    },
    /// Completed provider-exposed reasoning text.
    AssistantReasoningMessage {
        text: String,
    },
    /// Durable runtime work start marker.
    RuntimeWorkStarted {
        work_id: RuntimeWorkId,
        kind: RuntimeWorkKind,
        label: String,
        #[serde(default)]
        tool_call_id: Option<String>,
        #[serde(default)]
        plugin_id: Option<String>,
        #[serde(default)]
        service_interface: Option<String>,
        #[serde(default)]
        operation: Option<String>,
        #[serde(default)]
        parent_work_id: Option<RuntimeWorkId>,
        #[serde(default)]
        started_at_ms: Option<u64>,
        #[serde(default)]
        cancellable: bool,
    },
    /// Durable runtime work cancellation request marker.
    RuntimeWorkCancelRequested {
        work_id: RuntimeWorkId,
        #[serde(default)]
        requested_at_ms: Option<u64>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    /// Durable runtime work finish marker.
    RuntimeWorkFinished {
        work_id: RuntimeWorkId,
        status: RuntimeWorkStatus,
        #[serde(default)]
        finished_at_ms: Option<u64>,
        #[serde(default)]
        message: Option<String>,
    },
    /// Durable runtime work progress marker.
    RuntimeWorkProgress {
        work_id: RuntimeWorkId,
        message: String,
        #[serde(default)]
        progress_at_ms: Option<u64>,
        #[serde(default)]
        completed_units: Option<u64>,
        #[serde(default)]
        total_units: Option<u64>,
    },
    /// Durable marker that a model turn cancellation was requested.
    ModelTurnCancelRequested {
        turn_id: String,
        #[serde(default)]
        requested_at_ms: Option<u64>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    /// Incremental tool invocation event emitted while a tool is running.
    ToolInvocationStream {
        event: ToolInvocationStreamEvent,
    },
    /// Durable marker that moves the session's canonical working directory.
    WorkingDirectoryChanged {
        old_working_directory: PathBuf,
        new_working_directory: PathBuf,
    },
    /// Durable provenance marker for sessions imported from external agents.
    SessionImported {
        source_id: String,
        source_display_name: String,
        external_session_id: String,
        imported_at_ms: u64,
    },
    /// Durable bounded presentation state for a completed tool invocation.
    ToolInvocationPresentation {
        tool_call_id: String,
        #[serde(default)]
        started_at_ms: Option<u64>,
        #[serde(default)]
        finished_at_ms: Option<u64>,
        is_error: bool,
        presentation: ToolInvocationPresentation,
    },
    /// Durable provenance marker for sessions forked or cloned from another session.
    SessionForked {
        source_session_id: SessionId,
        #[serde(default)]
        source_title: Option<String>,
        #[serde(default)]
        source_cutoff_sequence: Option<u64>,
        #[serde(default)]
        source_prompt_sequence: Option<u64>,
        forked_at_ms: u64,
        kind: SessionForkKind,
    },
}

impl PersistedSessionEventKind {
    #[allow(clippy::too_many_lines)]
    fn into_domain(self) -> SessionEventKind {
        match self {
            Self::SessionCreated {
                name,
                working_directory,
            } => SessionEventKind::SessionCreated {
                name,
                working_directory,
            },
            Self::ClientAttached { client_id } => SessionEventKind::ClientAttached { client_id },
            Self::ClientDetached { client_id } => SessionEventKind::ClientDetached { client_id },
            Self::UserMessage { client_id, text } => {
                SessionEventKind::UserMessage { client_id, text }
            }
            Self::AssistantDelta { text } => SessionEventKind::AssistantDelta { text },
            Self::AssistantMessage { text } => SessionEventKind::AssistantMessage { text },
            Self::ToolCallRequested {
                tool_call_id,
                tool_name,
                arguments_json,
            } => SessionEventKind::ToolCallRequested {
                tool_call_id,
                tool_name,
                arguments_json,
            },
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
            Self::PermissionRequested {
                permission_id,
                tool_call_id,
                tool_name,
                arguments_json,
            } => SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                tool_name,
                arguments_json,
            },
            Self::PermissionResolved {
                permission_id,
                approved,
            } => SessionEventKind::PermissionResolved {
                permission_id,
                approved,
            },
            Self::ModelChanged { provider, model } => {
                SessionEventKind::ModelChanged { provider, model }
            }
            Self::SystemMessage { text } => SessionEventKind::SystemMessage { text },
            Self::AgentChanged { agent_id } => SessionEventKind::AgentChanged { agent_id },
            Self::ModelTurnStarted { turn_id } => SessionEventKind::ModelTurnStarted { turn_id },
            Self::ModelTurnFinished {
                turn_id,
                outcome,
                message,
            } => SessionEventKind::ModelTurnFinished {
                turn_id,
                outcome,
                message,
            },
            Self::ModelUsage { turn_id, usage } => SessionEventKind::ModelUsage { turn_id, usage },
            Self::ContextCompacted {
                summary,
                compacted_through_sequence,
            } => SessionEventKind::ContextCompacted {
                summary,
                compacted_through_sequence,
            },
            Self::SessionRenamed { name } => SessionEventKind::SessionRenamed { name },
            Self::TraceEvent { trace } => SessionEventKind::TraceEvent { trace },
            Self::SkillInvoked {
                skill_id,
                arguments,
                source,
                invoked_at_ms,
            } => SessionEventKind::SkillInvoked {
                skill_id,
                arguments,
                source,
                invoked_at_ms,
            },
            Self::SkillSuggested {
                skill_id,
                reason,
                suggested_at_ms,
            } => SessionEventKind::SkillSuggested {
                skill_id,
                reason,
                suggested_at_ms,
            },
            Self::SkillActivated {
                skill_id,
                source,
                mode,
                activated_at_ms,
            } => SessionEventKind::SkillActivated {
                skill_id,
                source,
                mode,
                activated_at_ms,
            },
            Self::SkillDeactivated {
                skill_id,
                deactivated_at_ms,
            } => SessionEventKind::SkillDeactivated {
                skill_id,
                deactivated_at_ms,
            },
            Self::SkillContextLoaded {
                skill_id,
                bytes_loaded,
                truncated,
                loaded_at_ms,
            } => SessionEventKind::SkillContextLoaded {
                skill_id,
                bytes_loaded,
                truncated,
                loaded_at_ms,
            },
            Self::SkillInvocationFailed {
                skill_id,
                error,
                failed_at_ms,
            } => SessionEventKind::SkillInvocationFailed {
                skill_id,
                error,
                failed_at_ms,
            },
            Self::AssistantReasoningDelta { text } => {
                SessionEventKind::AssistantReasoningDelta { text }
            }
            Self::AssistantReasoningMessage { text } => {
                SessionEventKind::AssistantReasoningMessage { text }
            }
            Self::RuntimeWorkStarted {
                work_id,
                kind,
                label,
                tool_call_id,
                plugin_id,
                service_interface,
                operation,
                parent_work_id,
                started_at_ms,
                cancellable,
            } => SessionEventKind::RuntimeWorkStarted {
                work_id,
                kind,
                label,
                tool_call_id,
                plugin_id,
                service_interface,
                operation,
                parent_work_id,
                started_at_ms,
                cancellable,
            },
            Self::RuntimeWorkCancelRequested {
                work_id,
                requested_at_ms,
                client_id,
            } => SessionEventKind::RuntimeWorkCancelRequested {
                work_id,
                requested_at_ms,
                client_id,
            },
            Self::RuntimeWorkFinished {
                work_id,
                status,
                finished_at_ms,
                message,
            } => SessionEventKind::RuntimeWorkFinished {
                work_id,
                status,
                finished_at_ms,
                message,
            },
            Self::RuntimeWorkProgress {
                work_id,
                message,
                progress_at_ms,
                completed_units,
                total_units,
            } => SessionEventKind::RuntimeWorkProgress {
                work_id,
                message,
                progress_at_ms,
                completed_units,
                total_units,
            },
            Self::ModelTurnCancelRequested {
                turn_id,
                requested_at_ms,
                client_id,
            } => SessionEventKind::ModelTurnCancelRequested {
                turn_id,
                requested_at_ms,
                client_id,
            },
            Self::ToolInvocationStream { event } => {
                SessionEventKind::ToolInvocationStream { event }
            }
            Self::WorkingDirectoryChanged {
                old_working_directory,
                new_working_directory,
            } => SessionEventKind::WorkingDirectoryChanged {
                old_working_directory,
                new_working_directory,
            },
            Self::SessionImported {
                source_id,
                source_display_name,
                external_session_id,
                imported_at_ms,
            } => SessionEventKind::SessionImported {
                source_id,
                source_display_name,
                external_session_id,
                imported_at_ms,
            },
            Self::ToolInvocationPresentation {
                tool_call_id,
                started_at_ms,
                finished_at_ms,
                is_error,
                presentation,
            } => SessionEventKind::ToolInvocationPresentation {
                tool_call_id,
                started_at_ms,
                finished_at_ms,
                is_error,
                presentation,
            },
            Self::SessionForked {
                source_session_id,
                source_title,
                source_cutoff_sequence,
                source_prompt_sequence,
                forked_at_ms,
                kind,
            } => SessionEventKind::SessionForked {
                source_session_id,
                source_title,
                source_cutoff_sequence,
                source_prompt_sequence,
                forked_at_ms,
                kind,
            },
        }
    }
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
            let event = decode_session_event(&event_payload(&semantic_result))
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

    fn event_payload(semantic_result: &serde_json::Value) -> String {
        serde_json::json!({
            "schema_version": CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            "sequence": 1,
            "session_id": SessionId::new(),
            "provenance": null,
            "kind": {
                "tool_call_finished": {
                    "tool_call_id": "call-1",
                    "result": "legacy",
                    "semantic_result": semantic_result.clone()
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
