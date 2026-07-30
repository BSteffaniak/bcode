//! Projection of finalized canonical session events into bounded search records.

use crate::{
    CURRENT_NORMALIZATION_VERSION, CURRENT_SEARCH_POLICY_VERSION, CURRENT_SEARCH_RECORD_VERSION,
    ContractValidationError, DEFAULT_MAX_TEXT_BYTES_PER_RECORD, SearchContentKind, SearchField,
    SessionSearchLocator, SessionSearchRecord,
};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, ReasoningActivity, SessionEvent, SessionEventKind,
    ToolInvocationLifecycleStage, ToolInvocationResult,
};
use serde::Deserialize;
use std::collections::BTreeMap;

const SHELL_RUN_TOOL_NAME: &str = "shell.run";
const SHELL_RUN_SCHEMA: &str = "bcode.shell.run";
const SHELL_RUN_SCHEMA_VERSION: u32 = 1;

/// Whether one sensitive content category is copied into derived search state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionContentPolicy {
    /// Do not copy this content category.
    Exclude,
    /// Copy this content category subject to projection bounds.
    Include,
}

impl ProjectionContentPolicy {
    const fn enabled(self) -> bool {
        matches!(self, Self::Include)
    }
}

/// Policy controlling which sensitive finalized content is copied into derived search records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchProjectionPolicy {
    /// Maximum UTF-8 bytes retained in one projected text record.
    pub max_text_bytes_per_record: usize,
    /// Copy finalized assistant reasoning into derived search state.
    pub reasoning: ProjectionContentPolicy,
    /// Copy shell command text into derived search state.
    pub shell_commands: ProjectionContentPolicy,
    /// Copy finalized bounded shell output into derived search state.
    pub shell_output: ProjectionContentPolicy,
    /// Copy tool argument payloads into derived search state.
    pub tool_arguments: ProjectionContentPolicy,
    /// Copy successful generic tool output into derived search state.
    pub tool_output: ProjectionContentPolicy,
}

impl Default for SearchProjectionPolicy {
    fn default() -> Self {
        Self {
            max_text_bytes_per_record: DEFAULT_MAX_TEXT_BYTES_PER_RECORD,
            reasoning: ProjectionContentPolicy::Exclude,
            shell_commands: ProjectionContentPolicy::Include,
            shell_output: ProjectionContentPolicy::Exclude,
            tool_arguments: ProjectionContentPolicy::Exclude,
            tool_output: ProjectionContentPolicy::Exclude,
        }
    }
}

/// Why a finalized event did not produce a search record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionExclusion {
    /// The event contains transient or incremental content rather than a finalized semantic fact.
    NonFinalContent,
    /// The event contains no approved searchable text in the current projection version.
    NoSearchableContent,
    /// The relevant sensitive content category is disabled by policy.
    DisabledByPolicy,
    /// The approved text is empty after normalization.
    EmptyAfterNormalization,
}

/// Projection disposition assigned explicitly to every persisted event variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistedEventClassification {
    /// The event has approved finalized text projected by default.
    Searchable,
    /// The event has finalized text that is projected only when its policy gate is enabled.
    PolicyControlled,
    /// The event is an incremental compatibility shape and is never independently indexed.
    NonFinal,
    /// The event is durable metadata but has no text record in the current policy version.
    MetadataOnly,
    /// The event owns opaque, renderer, trace, or inert content intentionally excluded from search.
    Excluded,
}

/// Classify every persisted event variant for the current projection policy version.
///
/// This exhaustive match intentionally has no wildcard so adding a persisted event variant requires
/// an explicit search projection decision.
#[must_use]
pub const fn classify_persisted_event(kind: &SessionEventKind) -> PersistedEventClassification {
    match kind {
        SessionEventKind::SessionCreated { name: Some(_), .. }
        | SessionEventKind::UserMessage { .. }
        | SessionEventKind::AssistantMessage { .. }
        | SessionEventKind::SystemMessage { .. }
        | SessionEventKind::ContextCompacted { .. }
        | SessionEventKind::SessionRenamed { name: Some(_) }
        | SessionEventKind::AssistantResponseSegment { .. } => {
            PersistedEventClassification::Searchable
        }
        SessionEventKind::ToolCallRequested { .. }
        | SessionEventKind::ToolInvocationResultRecorded {
            record:
                bcode_session_models::ToolInvocationResultRecord {
                    is_error: false, ..
                },
        }
        | SessionEventKind::AssistantReasoningMessage { .. }
        | SessionEventKind::AssistantReasoningActivity { .. } => {
            PersistedEventClassification::PolicyControlled
        }
        SessionEventKind::ToolInvocationResultRecorded {
            record: bcode_session_models::ToolInvocationResultRecord { is_error: true, .. },
        }
        | SessionEventKind::ToolInvocationLifecycle {
            event:
                bcode_session_models::ToolInvocationLifecycleEvent {
                    stage:
                        ToolInvocationLifecycleStage::Failed | ToolInvocationLifecycleStage::Cancelled,
                    message: Some(_),
                    ..
                },
        } => PersistedEventClassification::Searchable,
        SessionEventKind::AssistantDelta { .. }
        | SessionEventKind::AssistantReasoningDelta { .. } => {
            PersistedEventClassification::NonFinal
        }
        SessionEventKind::SessionCreated { name: None, .. }
        | SessionEventKind::ClientAttached { .. }
        | SessionEventKind::ClientDetached { .. }
        | SessionEventKind::PermissionRequested { .. }
        | SessionEventKind::PermissionResolved { .. }
        | SessionEventKind::ModelChanged { .. }
        | SessionEventKind::AgentChanged { .. }
        | SessionEventKind::ModelTurnStarted { .. }
        | SessionEventKind::ModelTurnFinished { .. }
        | SessionEventKind::ModelUsage { .. }
        | SessionEventKind::SessionRenamed { name: None }
        | SessionEventKind::SkillInvoked { .. }
        | SessionEventKind::SkillSuggested { .. }
        | SessionEventKind::SkillActivated { .. }
        | SessionEventKind::SkillDeactivated { .. }
        | SessionEventKind::SkillContextLoaded { .. }
        | SessionEventKind::SkillInvocationFailed { .. }
        | SessionEventKind::RuntimeWorkStarted { .. }
        | SessionEventKind::RuntimeWorkCancelRequested { .. }
        | SessionEventKind::RuntimeWorkFinished { .. }
        | SessionEventKind::RuntimeWorkProgress { .. }
        | SessionEventKind::ModelTurnCancelRequested { .. }
        | SessionEventKind::WorkingDirectoryChanged { .. }
        | SessionEventKind::SessionImported { .. }
        | SessionEventKind::SessionForked { .. }
        | SessionEventKind::RalphLifecycle { .. }
        | SessionEventKind::ReasoningChanged { .. }
        | SessionEventKind::ToolExchangeRequested { .. }
        | SessionEventKind::ToolExchangeResolved { .. }
        | SessionEventKind::ProviderContextCompacted { .. }
        | SessionEventKind::RequestContextObserved { .. }
        | SessionEventKind::PluginStatusNote { .. }
        | SessionEventKind::ToolInvocationLifecycle { .. }
        | SessionEventKind::ExecutionSessionCreated { .. } => {
            PersistedEventClassification::MetadataOnly
        }
        SessionEventKind::TraceEvent { .. }
        | SessionEventKind::InertHistory { .. }
        | SessionEventKind::ToolContribution { .. }
        | SessionEventKind::ToolContributionPlaced { .. } => PersistedEventClassification::Excluded,
    }
}

/// Result of classifying and projecting one finalized canonical event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventProjection {
    /// The event produced one or more bounded derived records.
    Records(Vec<SessionSearchRecord>),
    /// The event was intentionally excluded.
    Excluded(ProjectionExclusion),
}

/// Result of bounded terminal-text normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedTerminalText {
    /// Sanitized UTF-8 text retained for indexing.
    pub text: String,
    /// Source bytes inspected by the normalizer.
    pub source_bytes_consumed: usize,
    /// UTF-8 bytes produced from the inspected source before final indexing truncation.
    pub normalized_bytes: usize,
    /// Whether source bytes were omitted because the source inspection bound was reached.
    pub source_truncated: bool,
    /// Whether malformed UTF-8 was replaced with U+FFFD.
    pub invalid_utf8_replaced: bool,
}

/// Normalize terminal-like bytes into a bounded deterministic sanitized transcript.
///
/// The source inspection and retained output are both bounded by `maximum_bytes`. ANSI CSI and OSC
/// controls are removed, CRLF and standalone carriage returns become line feeds, backspaces remove
/// preceding retained characters, tabs and line feeds are preserved, and other controls are
/// removed. Malformed UTF-8 is replaced with U+FFFD. This is a sanitized stream transcript, not a
/// terminal-screen emulator.
#[must_use]
pub fn normalize_terminal_bytes(source: &[u8], maximum_bytes: usize) -> NormalizedTerminalText {
    let source_limit = source.len().min(maximum_bytes);
    let mut normalized = String::with_capacity(source_limit);
    let mut index = 0;
    let mut invalid_utf8_replaced = false;

    while index < source_limit {
        match source[index] {
            0x1b => consume_escape_bytes(source, &mut index, source_limit),
            b'\r' => {
                index += 1;
                if index < source_limit && source[index] == b'\n' {
                    index += 1;
                }
                push_bounded(&mut normalized, '\n', maximum_bytes);
            }
            b'\n' | b'\t' => {
                push_bounded(&mut normalized, char::from(source[index]), maximum_bytes);
                index += 1;
            }
            0x08 => {
                remove_previous_text_character(&mut normalized);
                index += 1;
            }
            byte if byte < 0x20 || byte == 0x7f => index += 1,
            b' '..=b'~' => {
                push_bounded(&mut normalized, char::from(source[index]), maximum_bytes);
                index += 1;
            }
            _ => {
                let remaining = &source[index..source_limit];
                match std::str::from_utf8(remaining) {
                    Ok(valid) => {
                        if let Some(character) = valid.chars().next() {
                            if !character.is_control() {
                                push_bounded(&mut normalized, character, maximum_bytes);
                            }
                            index += character.len_utf8();
                        } else {
                            index = source_limit;
                        }
                    }
                    Err(error) if error.valid_up_to() > 0 => {
                        let Ok(valid) = std::str::from_utf8(&remaining[..error.valid_up_to()])
                        else {
                            index += 1;
                            continue;
                        };
                        if let Some(character) = valid.chars().next() {
                            if !character.is_control() {
                                push_bounded(&mut normalized, character, maximum_bytes);
                            }
                            index += character.len_utf8();
                        } else {
                            index += error.error_len().unwrap_or(1).min(source_limit - index);
                        }
                    }
                    Err(error) => {
                        push_bounded(&mut normalized, '\u{fffd}', maximum_bytes);
                        invalid_utf8_replaced = true;
                        index += error.error_len().unwrap_or(1).min(source_limit - index);
                    }
                }
            }
        }
    }

    collapse_adjacent_duplicate_lines(&mut normalized, maximum_bytes);
    let normalized_bytes = normalized.len();
    NormalizedTerminalText {
        text: normalized,
        source_bytes_consumed: source_limit,
        normalized_bytes,
        source_truncated: source_limit < source.len(),
        invalid_utf8_replaced,
    }
}

/// Normalize terminal-like text into a deterministic sanitized transcript.
#[must_use]
pub fn normalize_terminal_text(source: &str) -> String {
    normalize_terminal_bytes(source.as_bytes(), source.len()).text
}

/// Project one decoded canonical event according to an explicit bounded policy.
///
/// # Errors
///
/// Returns an error when the policy has a zero text limit or the event uses a future schema version
/// that this projection does not understand.
pub fn project_event(
    event: &SessionEvent,
    policy: &SearchProjectionPolicy,
) -> Result<EventProjection, ContractValidationError> {
    if policy.max_text_bytes_per_record == 0 {
        return Err(ContractValidationError::InvalidProjection(
            "maximum record text bytes must be greater than zero",
        ));
    }
    if event.schema_version > CURRENT_SESSION_EVENT_SCHEMA_VERSION {
        return Err(ContractValidationError::InvalidProjection(
            "future session event schema is unsupported",
        ));
    }
    let _classification = classify_persisted_event(&event.kind);

    if let Some(projection) = project_transcript_event(event, policy) {
        return Ok(projection);
    }
    if let Some(projection) = project_tool_event(event, policy) {
        return Ok(projection);
    }
    if let SessionEventKind::AssistantReasoningActivity { activity, .. } = &event.kind {
        return Ok(if policy.reasoning.enabled() {
            project_reasoning_activity(event, activity, policy.max_text_bytes_per_record)
        } else {
            EventProjection::Excluded(ProjectionExclusion::DisabledByPolicy)
        });
    }

    let projection = match &event.kind {
        SessionEventKind::AssistantDelta { .. }
        | SessionEventKind::AssistantReasoningDelta { .. } => {
            EventProjection::Excluded(ProjectionExclusion::NonFinalContent)
        }
        SessionEventKind::AssistantReasoningMessage { text } if policy.reasoning.enabled() => {
            project_text(
                event,
                "assistant-reasoning",
                SearchContentKind::AssistantReasoning,
                SearchField::Text,
                text,
                BTreeMap::new(),
                policy.max_text_bytes_per_record,
            )
        }
        SessionEventKind::AssistantReasoningMessage { .. } => {
            EventProjection::Excluded(ProjectionExclusion::DisabledByPolicy)
        }
        SessionEventKind::SessionCreated { name: None, .. }
        | SessionEventKind::SessionRenamed { name: None }
        | SessionEventKind::ClientAttached { .. }
        | SessionEventKind::ClientDetached { .. }
        | SessionEventKind::PermissionRequested { .. }
        | SessionEventKind::PermissionResolved { .. }
        | SessionEventKind::ModelChanged { .. }
        | SessionEventKind::AgentChanged { .. }
        | SessionEventKind::ModelTurnStarted { .. }
        | SessionEventKind::ModelTurnFinished { .. }
        | SessionEventKind::ModelUsage { .. }
        | SessionEventKind::TraceEvent { .. }
        | SessionEventKind::SkillInvoked { .. }
        | SessionEventKind::SkillSuggested { .. }
        | SessionEventKind::SkillActivated { .. }
        | SessionEventKind::SkillDeactivated { .. }
        | SessionEventKind::SkillContextLoaded { .. }
        | SessionEventKind::SkillInvocationFailed { .. }
        | SessionEventKind::RuntimeWorkStarted { .. }
        | SessionEventKind::RuntimeWorkCancelRequested { .. }
        | SessionEventKind::RuntimeWorkFinished { .. }
        | SessionEventKind::RuntimeWorkProgress { .. }
        | SessionEventKind::ModelTurnCancelRequested { .. }
        | SessionEventKind::WorkingDirectoryChanged { .. }
        | SessionEventKind::SessionImported { .. }
        | SessionEventKind::SessionForked { .. }
        | SessionEventKind::RalphLifecycle { .. }
        | SessionEventKind::ReasoningChanged { .. }
        | SessionEventKind::ToolExchangeRequested { .. }
        | SessionEventKind::ToolExchangeResolved { .. }
        | SessionEventKind::ProviderContextCompacted { .. }
        | SessionEventKind::RequestContextObserved { .. }
        | SessionEventKind::PluginStatusNote { .. }
        | SessionEventKind::InertHistory { .. }
        | SessionEventKind::ToolInvocationLifecycle { .. }
        | SessionEventKind::ToolContribution { .. }
        | SessionEventKind::ToolContributionPlaced { .. }
        | SessionEventKind::ExecutionSessionCreated { .. } => {
            EventProjection::Excluded(ProjectionExclusion::NoSearchableContent)
        }
        SessionEventKind::SessionCreated { name: Some(_), .. }
        | SessionEventKind::SessionRenamed { name: Some(_) }
        | SessionEventKind::UserMessage { .. }
        | SessionEventKind::AssistantMessage { .. }
        | SessionEventKind::SystemMessage { .. }
        | SessionEventKind::ContextCompacted { .. }
        | SessionEventKind::ToolCallRequested { .. }
        | SessionEventKind::ToolInvocationResultRecorded { .. }
        | SessionEventKind::AssistantReasoningActivity { .. }
        | SessionEventKind::AssistantResponseSegment { .. } => {
            unreachable!("searchable event variants must be handled by their focused projector")
        }
    };

    Ok(projection)
}

fn project_transcript_event(
    event: &SessionEvent,
    policy: &SearchProjectionPolicy,
) -> Option<EventProjection> {
    let (record_kind, content_kind, field, text) = match &event.kind {
        SessionEventKind::SessionCreated {
            name: Some(name), ..
        }
        | SessionEventKind::SessionRenamed { name: Some(name) } => (
            "title",
            SearchContentKind::SessionTitle,
            SearchField::Title,
            name,
        ),
        SessionEventKind::UserMessage { text, .. } => (
            "user-message",
            SearchContentKind::UserMessage,
            SearchField::Text,
            text,
        ),
        SessionEventKind::AssistantMessage { text } => (
            "assistant-message",
            SearchContentKind::AssistantMessage,
            SearchField::Text,
            text,
        ),
        SessionEventKind::AssistantResponseSegment {
            turn_id,
            segment_id,
            segment_order,
            text,
        } => {
            return Some(project_text(
                event,
                &format!("assistant-segment-{segment_order}"),
                SearchContentKind::AssistantMessage,
                SearchField::Text,
                text,
                BTreeMap::from([
                    ("turn_id".to_owned(), turn_id.clone()),
                    ("segment_id".to_owned(), segment_id.clone()),
                    ("segment_order".to_owned(), segment_order.to_string()),
                ]),
                policy.max_text_bytes_per_record,
            ));
        }
        SessionEventKind::SystemMessage { text } => (
            "system-message",
            SearchContentKind::SystemMessage,
            SearchField::Text,
            text,
        ),
        SessionEventKind::ContextCompacted { summary, .. } => (
            "compaction",
            SearchContentKind::Compaction,
            SearchField::Text,
            summary,
        ),
        _ => return None,
    };

    Some(project_text(
        event,
        record_kind,
        content_kind,
        field,
        text,
        BTreeMap::new(),
        policy.max_text_bytes_per_record,
    ))
}

fn project_tool_event(
    event: &SessionEvent,
    policy: &SearchProjectionPolicy,
) -> Option<EventProjection> {
    match &event.kind {
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            arguments_json,
            working_directory,
            ..
        } if tool_name == SHELL_RUN_TOOL_NAME && policy.shell_commands.enabled() => {
            Some(project_shell_command(
                event,
                tool_call_id,
                arguments_json,
                working_directory.as_ref(),
                policy.max_text_bytes_per_record,
            ))
        }
        SessionEventKind::ToolCallRequested { tool_name, .. }
            if tool_name == SHELL_RUN_TOOL_NAME =>
        {
            Some(EventProjection::Excluded(
                ProjectionExclusion::DisabledByPolicy,
            ))
        }
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            arguments_json,
            working_directory,
            ..
        } if policy.tool_arguments.enabled() => {
            let mut attributes = BTreeMap::from([
                ("invocation_id".to_owned(), tool_call_id.clone()),
                ("tool_name".to_owned(), tool_name.clone()),
            ]);
            if let Some(working_directory) = working_directory {
                attributes.insert(
                    "working_directory".to_owned(),
                    working_directory.to_string_lossy().into_owned(),
                );
            }
            Some(project_text(
                event,
                "tool-arguments",
                SearchContentKind::ToolArguments,
                SearchField::ToolArguments,
                arguments_json,
                attributes,
                policy.max_text_bytes_per_record,
            ))
        }
        SessionEventKind::ToolInvocationResultRecorded { record }
            if shell_result_metadata(record).is_some() && policy.shell_output.enabled() =>
        {
            Some(project_shell_result(
                event,
                record,
                policy.max_text_bytes_per_record,
            ))
        }
        SessionEventKind::ToolInvocationResultRecorded { record }
            if shell_result_metadata(record).is_some() =>
        {
            Some(EventProjection::Excluded(
                ProjectionExclusion::DisabledByPolicy,
            ))
        }
        SessionEventKind::ToolInvocationResultRecorded { record }
            if record.is_error || policy.tool_output.enabled() =>
        {
            Some(project_tool_result(event, record, policy))
        }
        SessionEventKind::ToolCallRequested { .. }
        | SessionEventKind::ToolInvocationResultRecorded { .. } => Some(EventProjection::Excluded(
            ProjectionExclusion::DisabledByPolicy,
        )),
        SessionEventKind::ToolInvocationLifecycle { event: lifecycle }
            if matches!(
                lifecycle.stage,
                ToolInvocationLifecycleStage::Failed | ToolInvocationLifecycleStage::Cancelled
            ) && lifecycle.message.is_some() =>
        {
            Some(project_text(
                event,
                "tool-lifecycle-error",
                SearchContentKind::ToolError,
                SearchField::ErrorMessage,
                lifecycle.message.as_deref().unwrap_or_default(),
                BTreeMap::from([("invocation_id".to_owned(), lifecycle.invocation_id.clone())]),
                policy.max_text_bytes_per_record,
            ))
        }
        _ => None,
    }
}

#[derive(Deserialize)]
struct ShellRunArgumentsProjection {
    command: String,
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum ShellRunResultProjection {
    Terminal {
        exit_code: Option<i32>,
        timed_out: bool,
        cancelled: bool,
        duration_ms: Option<u64>,
        output_tail: String,
        output_truncated: bool,
        output_bytes: Option<u64>,
        retained_output_bytes: Option<u64>,
    },
    Captured {
        exit_code: Option<i32>,
        timed_out: bool,
        cancelled: bool,
        duration_ms: Option<u64>,
        stdout: String,
        stderr: String,
        stdout_truncated: bool,
        stderr_truncated: bool,
        stdout_bytes: Option<u64>,
        stderr_bytes: Option<u64>,
    },
}

fn project_shell_command(
    event: &SessionEvent,
    invocation_id: &str,
    arguments_json: &str,
    working_directory: Option<&std::path::PathBuf>,
    maximum_bytes: usize,
) -> EventProjection {
    let Ok(arguments) = serde_json::from_str::<ShellRunArgumentsProjection>(arguments_json) else {
        return EventProjection::Excluded(ProjectionExclusion::NoSearchableContent);
    };
    let mut attributes = BTreeMap::from([
        ("invocation_id".to_owned(), invocation_id.to_owned()),
        ("tool_name".to_owned(), SHELL_RUN_TOOL_NAME.to_owned()),
    ]);
    if let Some(working_directory) = working_directory {
        attributes.insert(
            "working_directory".to_owned(),
            working_directory.to_string_lossy().into_owned(),
        );
    }
    project_text(
        event,
        "shell-command",
        SearchContentKind::ShellCommand,
        SearchField::Command,
        &arguments.command,
        attributes,
        maximum_bytes,
    )
}

fn shell_result_metadata(
    record: &bcode_session_models::ToolInvocationResultRecord,
) -> Option<ShellRunResultProjection> {
    let Some(ToolInvocationResult::Artifact { artifact }) = &record.result else {
        return None;
    };
    if artifact.schema != SHELL_RUN_SCHEMA || artifact.schema_version != SHELL_RUN_SCHEMA_VERSION {
        return None;
    }
    serde_json::from_value(artifact.metadata.clone()).ok()
}

fn project_shell_result(
    event: &SessionEvent,
    record: &bcode_session_models::ToolInvocationResultRecord,
    maximum_bytes: usize,
) -> EventProjection {
    let Some(result) = shell_result_metadata(record) else {
        return EventProjection::Excluded(ProjectionExclusion::NoSearchableContent);
    };
    let mut records = Vec::new();
    match result {
        ShellRunResultProjection::Terminal {
            exit_code,
            timed_out,
            cancelled,
            duration_ms,
            output_tail,
            output_truncated,
            output_bytes,
            retained_output_bytes,
        } => {
            let attributes = shell_result_attributes(
                &record.invocation_id,
                exit_code,
                timed_out,
                cancelled,
                duration_ms,
                output_truncated,
                output_bytes,
                retained_output_bytes,
            );
            append_projected_records(
                &mut records,
                project_text(
                    event,
                    "shell-combined-output",
                    SearchContentKind::ShellOutput,
                    SearchField::Text,
                    &output_tail,
                    attributes,
                    maximum_bytes,
                ),
            );
        }
        ShellRunResultProjection::Captured {
            exit_code,
            timed_out,
            cancelled,
            duration_ms,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
            stdout_bytes,
            stderr_bytes,
        } => {
            let common = shell_result_attributes(
                &record.invocation_id,
                exit_code,
                timed_out,
                cancelled,
                duration_ms,
                stdout_truncated || stderr_truncated,
                sum_optional(stdout_bytes, stderr_bytes),
                None,
            );
            append_projected_records(
                &mut records,
                project_text(
                    event,
                    "shell-stdout",
                    SearchContentKind::ShellOutput,
                    SearchField::StandardOutput,
                    &stdout,
                    common.clone(),
                    maximum_bytes,
                ),
            );
            append_projected_records(
                &mut records,
                project_text(
                    event,
                    "shell-stderr",
                    SearchContentKind::ShellOutput,
                    SearchField::StandardError,
                    &stderr,
                    common,
                    maximum_bytes,
                ),
            );
        }
    }
    if records.is_empty() {
        EventProjection::Excluded(ProjectionExclusion::EmptyAfterNormalization)
    } else {
        EventProjection::Records(records)
    }
}

fn append_projected_records(target: &mut Vec<SessionSearchRecord>, projection: EventProjection) {
    if let EventProjection::Records(mut records) = projection {
        target.append(&mut records);
    }
}

#[allow(clippy::too_many_arguments)]
fn shell_result_attributes(
    invocation_id: &str,
    exit_code: Option<i32>,
    timed_out: bool,
    cancelled: bool,
    duration_ms: Option<u64>,
    source_truncated: bool,
    output_bytes: Option<u64>,
    retained_output_bytes: Option<u64>,
) -> BTreeMap<String, String> {
    let mut attributes = BTreeMap::from([
        ("invocation_id".to_owned(), invocation_id.to_owned()),
        ("tool_name".to_owned(), SHELL_RUN_TOOL_NAME.to_owned()),
        ("timed_out".to_owned(), timed_out.to_string()),
        ("cancelled".to_owned(), cancelled.to_string()),
        ("source_truncated".to_owned(), source_truncated.to_string()),
    ]);
    if let Some(exit_code) = exit_code {
        attributes.insert("exit_code".to_owned(), exit_code.to_string());
    }
    if let Some(duration_ms) = duration_ms {
        attributes.insert("duration_ms".to_owned(), duration_ms.to_string());
    }
    if let Some(output_bytes) = output_bytes {
        attributes.insert("output_bytes".to_owned(), output_bytes.to_string());
    }
    if let Some(retained_output_bytes) = retained_output_bytes {
        attributes.insert(
            "retained_output_bytes".to_owned(),
            retained_output_bytes.to_string(),
        );
    }
    attributes
}

const fn sum_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        _ => None,
    }
}

fn project_tool_result(
    event: &SessionEvent,
    record: &bcode_session_models::ToolInvocationResultRecord,
    policy: &SearchProjectionPolicy,
) -> EventProjection {
    let (content_kind, field, record_kind) = if record.is_error {
        (
            SearchContentKind::ToolError,
            SearchField::ErrorMessage,
            "tool-error",
        )
    } else {
        (
            SearchContentKind::ToolOutput,
            SearchField::Text,
            "tool-output",
        )
    };
    let mut attributes =
        BTreeMap::from([("invocation_id".to_owned(), record.invocation_id.clone())]);
    if let Some(result) = &record.result {
        attributes.insert("result_kind".to_owned(), result_kind(result).to_owned());
    }
    project_text(
        event,
        record_kind,
        content_kind,
        field,
        &record.model_output,
        attributes,
        policy.max_text_bytes_per_record,
    )
}

fn project_reasoning_activity(
    event: &SessionEvent,
    activity: &ReasoningActivity,
    maximum_bytes: usize,
) -> EventProjection {
    let records = activity
        .parts
        .iter()
        .map(|part| {
            project_text(
                event,
                &format!("reasoning-{}-{}", activity.order, part.order),
                SearchContentKind::AssistantReasoning,
                SearchField::Text,
                &part.text,
                BTreeMap::from([
                    ("activity_id".to_owned(), activity.activity_id.clone()),
                    ("activity_order".to_owned(), activity.order.to_string()),
                    ("part_id".to_owned(), part.part_id.clone()),
                    ("part_order".to_owned(), part.order.to_string()),
                ]),
                maximum_bytes,
            )
        })
        .filter_map(|projection| match projection {
            EventProjection::Records(mut records) => records.pop(),
            EventProjection::Excluded(_) => None,
        })
        .collect::<Vec<_>>();

    if records.is_empty() {
        EventProjection::Excluded(ProjectionExclusion::EmptyAfterNormalization)
    } else {
        EventProjection::Records(records)
    }
}

fn project_text(
    event: &SessionEvent,
    record_kind: &str,
    content_kind: SearchContentKind,
    field: SearchField,
    source: &str,
    attributes: BTreeMap<String, String>,
    maximum_bytes: usize,
) -> EventProjection {
    let normalized = normalize_terminal_bytes(source.as_bytes(), maximum_bytes);
    if normalized.text.trim().is_empty() {
        return EventProjection::Excluded(ProjectionExclusion::EmptyAfterNormalization);
    }

    let indexed_bytes = normalized.text.len();
    let record_id = format!("{}:{record_kind}:0", event.sequence);
    let source_bytes = u64::try_from(source.len()).unwrap_or(u64::MAX);
    let source_range_end = u64::try_from(normalized.source_bytes_consumed).unwrap_or(u64::MAX);
    let mut attributes = attributes;
    if normalized.invalid_utf8_replaced {
        attributes.insert("invalid_utf8_replaced".to_owned(), "true".to_owned());
    }

    EventProjection::Records(vec![SessionSearchRecord {
        schema_version: CURRENT_SEARCH_RECORD_VERSION,
        record_id: record_id.clone(),
        locator: SessionSearchLocator {
            session_id: event.session_id,
            sequence: event.sequence,
            record_id: Some(record_id),
        },
        timestamp_ms: event.timestamp_ms,
        content_kind,
        field: Some(field),
        text: Some(normalized.text),
        attributes,
        source_bytes,
        normalized_bytes: u64::try_from(normalized.normalized_bytes).unwrap_or(u64::MAX),
        indexed_bytes: u64::try_from(indexed_bytes).unwrap_or(u64::MAX),
        truncated: normalized.source_truncated,
        source_range_start: Some(0),
        source_range_end: Some(source_range_end),
        normalization_version: CURRENT_NORMALIZATION_VERSION,
        policy_version: CURRENT_SEARCH_POLICY_VERSION,
    }])
}

fn collapse_adjacent_duplicate_lines(text: &mut String, maximum_bytes: usize) {
    if !text.contains('\n') {
        return;
    }
    let trailing_newline = text.ends_with('\n');
    let mut collapsed = String::with_capacity(text.len().min(maximum_bytes));
    let mut previous: Option<&str> = None;
    for line in text.split('\n') {
        if previous == Some(line) && !line.is_empty() {
            continue;
        }
        if !collapsed.is_empty() {
            collapsed.push('\n');
        }
        collapsed.push_str(line);
        previous = Some(line);
    }
    if trailing_newline && !collapsed.ends_with('\n') && collapsed.len() < maximum_bytes {
        collapsed.push('\n');
    }
    *text = collapsed;
}

fn push_bounded(text: &mut String, character: char, maximum_bytes: usize) {
    if text.len().saturating_add(character.len_utf8()) <= maximum_bytes {
        text.push(character);
    }
}

fn consume_escape_bytes(source: &[u8], index: &mut usize, source_limit: usize) {
    *index += 1;
    if *index >= source_limit {
        return;
    }
    match source[*index] {
        b'[' => {
            *index += 1;
            while *index < source_limit {
                let byte = source[*index];
                *index += 1;
                if (b'@'..=b'~').contains(&byte) {
                    break;
                }
            }
        }
        b']' => {
            *index += 1;
            while *index < source_limit {
                match source[*index] {
                    0x07 => {
                        *index += 1;
                        break;
                    }
                    0x1b if *index + 1 < source_limit && source[*index + 1] == b'\\' => {
                        *index += 2;
                        break;
                    }
                    _ => *index += 1,
                }
            }
        }
        _ => *index += 1,
    }
}

const fn result_kind(result: &ToolInvocationResult) -> &'static str {
    match result {
        ToolInvocationResult::Text { .. } => "text",
        ToolInvocationResult::Json { .. } => "json",
        ToolInvocationResult::Artifact { .. } => "artifact",
    }
}

fn remove_previous_text_character(text: &mut String) {
    if text.ends_with('\n') {
        return;
    }
    text.pop();
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{SessionId, TurnAdmissionMetadata};

    fn event(sequence: u64, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: 123,
            session_id: SessionId::new(),
            provenance: None,
            kind,
        }
    }

    #[test]
    fn normalization_sanitizes_terminal_transcript_without_screen_emulation() {
        let source = "start\rnext\u{1b}[31m red\u{1b}[0m\nlink: \u{1b}]8;;https://secret\u{7}label\u{1b}]8;;\u{7}\nabc\u{8}d\u{0}";
        assert_eq!(
            normalize_terminal_text(source),
            "start\nnext red\nlink: label\nabd"
        );
    }

    #[test]
    fn normalization_collapses_only_adjacent_exact_progress_duplicates() {
        let source = b"working\rworking\rERROR one\rERROR two\rERROR two\rfinished";
        let normalized = normalize_terminal_bytes(source, source.len());
        assert_eq!(normalized.text, "working\nERROR one\nERROR two\nfinished");
    }

    #[test]
    fn byte_normalization_is_bounded_and_replaces_invalid_utf8() {
        let source = [b'a', 0xff, b'b', 0x1b, b'[', b'3', b'1', b'm', b'c'];
        let normalized = normalize_terminal_bytes(&source, 6);
        assert_eq!(normalized.text, "a�b");
        assert_eq!(normalized.source_bytes_consumed, 6);
        assert!(normalized.source_truncated);
        assert!(normalized.invalid_utf8_replaced);
        assert!(normalized.text.len() <= 6);
    }

    #[test]
    fn byte_normalization_never_exceeds_bound_for_adversarial_inputs() {
        let inputs = [
            vec![0xff; 1024],
            b"\x1b]8;;unterminated-secret".repeat(100),
            b"progress\rprogress\rprogress".repeat(100),
            "é🙂\u{8}\r\n".repeat(100).into_bytes(),
        ];
        for input in inputs {
            for maximum in 1..64 {
                let normalized = normalize_terminal_bytes(&input, maximum);
                assert!(normalized.text.len() <= maximum);
                assert!(normalized.source_bytes_consumed <= maximum);
                assert!(std::str::from_utf8(normalized.text.as_bytes()).is_ok());
            }
        }
    }

    #[test]
    fn user_message_projection_is_stable_bounded_and_utf8_safe() {
        let event = event(
            7,
            SessionEventKind::UserMessage {
                client_id: bcode_session_models::ClientId::new(),
                text: "abéz".to_owned(),
                admission: TurnAdmissionMetadata::default(),
            },
        );
        let projection = project_event(
            &event,
            &SearchProjectionPolicy {
                max_text_bytes_per_record: 4,
                ..SearchProjectionPolicy::default()
            },
        )
        .expect("project event");
        let EventProjection::Records(records) = projection else {
            panic!("user message must produce a record");
        };
        let record = &records[0];
        assert_eq!(record.record_id, "7:user-message:0");
        assert_eq!(record.text.as_deref(), Some("abé"));
        assert_eq!(record.normalized_bytes, 4);
        assert_eq!(record.indexed_bytes, 4);
        assert!(record.truncated);
        assert_eq!(
            record.locator.record_id.as_deref(),
            Some("7:user-message:0")
        );
    }

    #[test]
    fn deltas_and_sensitive_categories_are_excluded_by_default() {
        let delta = event(
            1,
            SessionEventKind::AssistantDelta {
                text: "partial".to_owned(),
            },
        );
        assert_eq!(
            project_event(&delta, &SearchProjectionPolicy::default()),
            Ok(EventProjection::Excluded(
                ProjectionExclusion::NonFinalContent
            ))
        );

        let tool = event(
            2,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: None,
                tool_name: "shell".to_owned(),
                arguments_json: "{\"command\":\"secret\"}".to_owned(),
                working_directory: None,
            },
        );
        assert_eq!(
            project_event(&tool, &SearchProjectionPolicy::default()),
            Ok(EventProjection::Excluded(
                ProjectionExclusion::DisabledByPolicy
            ))
        );
    }

    #[test]
    fn finalized_reasoning_projects_ordered_parts_with_distinct_identities() {
        use bcode_session_models::{
            ReasoningActivityStatus, ReasoningContentKind, ReasoningContentRole, ReasoningPart,
        };

        let event = event(
            11,
            SessionEventKind::AssistantReasoningActivity {
                turn_id: "turn-1".to_owned(),
                activity: ReasoningActivity {
                    activity_id: "activity-1".to_owned(),
                    order: 2,
                    status: ReasoningActivityStatus::Completed,
                    parts: vec![
                        ReasoningPart {
                            part_id: "summary".to_owned(),
                            kind: ReasoningContentKind::Summary,
                            role: ReasoningContentRole::Milestone,
                            order: 0,
                            text: "first".to_owned(),
                        },
                        ReasoningPart {
                            part_id: "detail".to_owned(),
                            kind: ReasoningContentKind::Raw,
                            role: ReasoningContentRole::Detail,
                            order: 1,
                            text: "second".to_owned(),
                        },
                    ],
                    opaque: true,
                },
            },
        );
        let projection = project_event(
            &event,
            &SearchProjectionPolicy {
                reasoning: ProjectionContentPolicy::Include,
                ..SearchProjectionPolicy::default()
            },
        )
        .expect("project reasoning");
        let EventProjection::Records(records) = projection else {
            panic!("reasoning must produce records");
        };
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].record_id, "11:reasoning-2-0:0");
        assert_eq!(records[1].record_id, "11:reasoning-2-1:0");
        assert_eq!(records[0].locator.sequence, records[1].locator.sequence);
        assert!(!records[0].attributes.contains_key("opaque"));
    }

    #[test]
    fn assistant_response_segment_preserves_stable_segment_metadata() {
        let event = event(
            12,
            SessionEventKind::AssistantResponseSegment {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-1".to_owned(),
                segment_order: 3,
                text: "answer".to_owned(),
            },
        );
        let projection = project_event(&event, &SearchProjectionPolicy::default())
            .expect("project assistant segment");
        let EventProjection::Records(records) = projection else {
            panic!("assistant segment must produce a record");
        };
        assert_eq!(records[0].record_id, "12:assistant-segment-3:0");
        assert_eq!(
            records[0].attributes.get("segment_id").map(String::as_str),
            Some("segment-1")
        );
    }

    #[test]
    fn shell_command_projects_from_typed_arguments_without_generic_argument_policy() {
        let event = event(
            13,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-shell".to_owned(),
                producer_plugin_id: Some("bcode.shell".to_owned()),
                tool_name: SHELL_RUN_TOOL_NAME.to_owned(),
                arguments_json: serde_json::json!({"command": "printf hello"}).to_string(),
                working_directory: Some(std::path::PathBuf::from("/workspace")),
            },
        );
        let projection = project_event(&event, &SearchProjectionPolicy::default())
            .expect("project shell command");
        let EventProjection::Records(records) = projection else {
            panic!("shell command must produce a record");
        };
        assert_eq!(records[0].content_kind, SearchContentKind::ShellCommand);
        assert_eq!(records[0].field, Some(SearchField::Command));
        assert_eq!(records[0].text.as_deref(), Some("printf hello"));
        assert_eq!(
            records[0]
                .attributes
                .get("working_directory")
                .map(String::as_str),
            Some("/workspace")
        );
    }

    #[test]
    fn shell_captured_output_projects_stdout_and_stderr_only_when_enabled() {
        let metadata = serde_json::json!({
            "mode": "captured",
            "exit_code": 1,
            "timed_out": false,
            "cancelled": false,
            "duration_ms": 25,
            "stdout": "out",
            "stderr": "error",
            "stdout_truncated": false,
            "stderr_truncated": true,
            "stdout_bytes": 3,
            "stderr_bytes": 99
        });
        let event = event(
            14,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-shell".to_owned(),
                    model_output: "model summary".to_owned(),
                    is_error: true,
                    presentation: None,
                    result: Some(ToolInvocationResult::Artifact {
                        artifact: Box::new(bcode_session_models::ToolArtifact {
                            artifact_id: "shell-result".to_owned(),
                            producer_plugin_id: "bcode.shell".to_owned(),
                            schema: SHELL_RUN_SCHEMA.to_owned(),
                            schema_version: SHELL_RUN_SCHEMA_VERSION,
                            tool_call_id: Some("call-shell".to_owned()),
                            title: None,
                            metadata,
                            refs: Vec::new(),
                        }),
                    }),
                },
            },
        );
        assert_eq!(
            project_event(&event, &SearchProjectionPolicy::default()),
            Ok(EventProjection::Excluded(
                ProjectionExclusion::DisabledByPolicy
            ))
        );
        let projection = project_event(
            &event,
            &SearchProjectionPolicy {
                shell_output: ProjectionContentPolicy::Include,
                ..SearchProjectionPolicy::default()
            },
        )
        .expect("project shell output");
        let EventProjection::Records(records) = projection else {
            panic!("shell output must produce records");
        };
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].field, Some(SearchField::StandardOutput));
        assert_eq!(records[1].field, Some(SearchField::StandardError));
        assert_eq!(
            records[0].attributes.get("exit_code").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            records[0]
                .attributes
                .get("source_truncated")
                .map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn finalized_tool_errors_are_projected_without_raw_structured_payloads() {
        let event = event(
            9,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-1".to_owned(),
                    model_output: "\u{1b}[31mfailed\u{1b}[0m".to_owned(),
                    is_error: true,
                    presentation: None,
                    result: Some(ToolInvocationResult::Json {
                        value: "{\"secret\":true}".to_owned(),
                    }),
                },
            },
        );
        let projection =
            project_event(&event, &SearchProjectionPolicy::default()).expect("project tool error");
        let EventProjection::Records(records) = projection else {
            panic!("tool error must produce a record");
        };
        assert_eq!(records[0].text.as_deref(), Some("failed"));
        assert_eq!(records[0].content_kind, SearchContentKind::ToolError);
        assert_eq!(
            records[0].attributes.get("result_kind").map(String::as_str),
            Some("json")
        );
        assert!(
            !records[0]
                .attributes
                .values()
                .any(|value| value.contains("secret"))
        );
    }
}
