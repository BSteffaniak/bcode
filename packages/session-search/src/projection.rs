//! Projection of finalized canonical session events into bounded search records.

use crate::{
    CURRENT_NORMALIZATION_VERSION, CURRENT_SEARCH_POLICY_VERSION, CURRENT_SEARCH_RECORD_VERSION,
    ContractValidationError, DEFAULT_MAX_TEXT_BYTES_PER_RECORD, SearchContentKind, SearchField,
    SessionSearchLocator, SessionSearchRecord,
};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionEventKind,
    ToolInvocationLifecycleStage, ToolInvocationResult,
};
use std::collections::BTreeMap;

/// Policy controlling which sensitive finalized content is copied into derived search records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchProjectionPolicy {
    /// Maximum UTF-8 bytes retained in one projected text record.
    pub max_text_bytes_per_record: usize,
    /// Copy finalized assistant reasoning into derived search state.
    pub include_reasoning: bool,
    /// Copy tool argument payloads into derived search state.
    pub include_tool_arguments: bool,
    /// Copy successful generic tool output into derived search state.
    pub include_tool_output: bool,
}

impl Default for SearchProjectionPolicy {
    fn default() -> Self {
        Self {
            max_text_bytes_per_record: DEFAULT_MAX_TEXT_BYTES_PER_RECORD,
            include_reasoning: false,
            include_tool_arguments: false,
            include_tool_output: false,
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

/// Result of classifying and projecting one finalized canonical event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventProjection {
    /// The event produced one or more bounded derived records.
    Records(Vec<SessionSearchRecord>),
    /// The event was intentionally excluded.
    Excluded(ProjectionExclusion),
}

/// Normalize terminal-like text into a deterministic sanitized transcript.
///
/// This removes ANSI CSI and OSC control sequences, converts CRLF and standalone carriage returns
/// to line feeds, applies backspaces to preceding characters, preserves tabs and line feeds, and
/// removes other control characters. It does not emulate a terminal screen.
#[must_use]
pub fn normalize_terminal_text(source: &str) -> String {
    let mut normalized = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();

    while let Some(character) = chars.next() {
        match character {
            '\u{1b}' => consume_escape(&mut chars),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                normalized.push('\n');
            }
            '\n' | '\t' => normalized.push(character),
            '\u{8}' => remove_previous_text_character(&mut normalized),
            character if character.is_control() => {}
            character => normalized.push(character),
        }
    }

    normalized
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

    if let Some(projection) = project_transcript_event(event, policy) {
        return Ok(projection);
    }
    if let Some(projection) = project_tool_event(event, policy) {
        return Ok(projection);
    }

    let projection = match &event.kind {
        SessionEventKind::AssistantDelta { .. }
        | SessionEventKind::AssistantReasoningDelta { .. } => {
            EventProjection::Excluded(ProjectionExclusion::NonFinalContent)
        }
        SessionEventKind::AssistantReasoningMessage { text } if policy.include_reasoning => {
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
        SessionEventKind::AssistantReasoningMessage { .. }
        | SessionEventKind::AssistantReasoningActivity { .. } => {
            EventProjection::Excluded(ProjectionExclusion::DisabledByPolicy)
        }
        _ => EventProjection::Excluded(ProjectionExclusion::NoSearchableContent),
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
        } if policy.include_tool_arguments => {
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
            if record.is_error || policy.include_tool_output =>
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

fn project_text(
    event: &SessionEvent,
    record_kind: &str,
    content_kind: SearchContentKind,
    field: SearchField,
    source: &str,
    attributes: BTreeMap<String, String>,
    maximum_bytes: usize,
) -> EventProjection {
    let normalized = normalize_terminal_text(source);
    if normalized.trim().is_empty() {
        return EventProjection::Excluded(ProjectionExclusion::EmptyAfterNormalization);
    }

    let normalized_bytes = normalized.len();
    let indexed_text = truncate_utf8(&normalized, maximum_bytes);
    let indexed_bytes = indexed_text.len();
    let record_id = format!("{}:{record_kind}:0", event.sequence);
    let source_bytes = u64::try_from(source.len()).unwrap_or(u64::MAX);

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
        text: Some(indexed_text.to_owned()),
        attributes,
        source_bytes,
        normalized_bytes: u64::try_from(normalized_bytes).unwrap_or(u64::MAX),
        indexed_bytes: u64::try_from(indexed_bytes).unwrap_or(u64::MAX),
        truncated: indexed_bytes < normalized_bytes,
        source_range_start: Some(0),
        source_range_end: Some(source_bytes),
        normalization_version: CURRENT_NORMALIZATION_VERSION,
        policy_version: CURRENT_SEARCH_POLICY_VERSION,
    }])
}

fn truncate_utf8(text: &str, maximum_bytes: usize) -> &str {
    if text.len() <= maximum_bytes {
        return text;
    }
    let mut end = maximum_bytes;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &text[..end]
}

const fn result_kind(result: &ToolInvocationResult) -> &'static str {
    match result {
        ToolInvocationResult::Text { .. } => "text",
        ToolInvocationResult::Json { .. } => "json",
        ToolInvocationResult::Artifact { .. } => "artifact",
    }
}

fn consume_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => {
            for character in chars.by_ref() {
                if ('@'..='~').contains(&character) {
                    break;
                }
            }
        }
        Some(']') => {
            let mut escape = false;
            for character in chars.by_ref() {
                if character == '\u{7}' || (escape && character == '\\') {
                    break;
                }
                escape = character == '\u{1b}';
            }
        }
        Some(_) | None => {}
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
        assert_eq!(record.normalized_bytes, 5);
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
