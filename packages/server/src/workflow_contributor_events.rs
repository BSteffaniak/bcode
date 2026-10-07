//! Model-only contributor evidence projection; callers own contributor authorization.
//!
//! This module performs no retrieval, artifact access, or session authority selection.
//! Canonical tool text is untrusted and may be sensitive. Do not publish it as diagnostics.
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionEventKind, SessionHistoryPage,
    SessionId,
};
use serde::Serialize;

/// Maximum canonical events examined by one discovery call.
pub const MAX_EVENTS: usize = 100;
/// Maximum combined textual payload returned by an exact read.
pub const MAX_BYTES: usize = 65_536;
const MAX_ID_BYTES: usize = 1_024;

/// Fail-closed projection outcome, without untrusted details in error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceError {
    Missing,
    Incompatible,
    Conflict,
    LimitExceeded,
}

/// Kind of canonical event available for subsequent exact inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventRole {
    Request,
    Result,
}

/// Bounded locator, not a grant of access or proof of successful execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventReference {
    pub sequence: u64,
    pub invocation_id: String,
    pub role: EventRole,
}

/// Discovery describes only the supplied canonical prefix, never future session EOF.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Discovery {
    pub session_id: SessionId,
    pub events: Vec<EventReference>,
    pub scanned_events: usize,
    pub next_sequence: Option<u64>,
}

/// Text completeness describes retrieval, not producer capture or semantic sufficiency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvidenceText {
    pub text: String,
    pub original_bytes: usize,
    pub retained_bytes: usize,
    pub truncated: bool,
}

/// Arguments and result text only; no provider metadata, presentation, or artifact paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InvocationEvidence {
    pub session_id: SessionId,
    pub invocation_id: String,
    pub request_sequence: u64,
    pub result_sequence: u64,
    pub producer_plugin_id: Option<String>,
    pub tool_name: String,
    pub arguments_json: EvidenceText,
    pub model_output: EvidenceText,
    pub is_error: bool,
    /// Always unknown: the generic durable record does not certify producer capture.
    pub producer_capture_complete: Option<bool>,
}

/// Exact pair requested within a contributor session already authorized by the caller.
pub struct PairQuery<'a> {
    pub session_id: SessionId,
    pub invocation_id: &'a str,
    pub request_sequence: u64,
    pub result_sequence: u64,
    pub byte_limit: usize,
}

fn validate(
    page: &SessionHistoryPage,
    session_id: SessionId,
    after: u64,
    limit: usize,
) -> Result<(), EvidenceError> {
    if limit == 0 || limit > MAX_EVENTS || page.events.len() > limit {
        return Err(EvidenceError::LimitExceeded);
    }
    if page.session_id != session_id {
        return Err(EvidenceError::Conflict);
    }
    if !page.compatibility_issues.is_empty() {
        return Err(EvidenceError::Incompatible);
    }
    let mut expected = after;
    for event in &page.events {
        if event.schema_version != CURRENT_SESSION_EVENT_SCHEMA_VERSION {
            return Err(EvidenceError::Incompatible);
        }
        if event.session_id != session_id || event.sequence != expected {
            return Err(EvidenceError::Conflict);
        }
        expected = expected.checked_add(1).ok_or(EvidenceError::Conflict)?;
    }
    if page.has_more {
        if page.events.is_empty()
            || page.next_cursor.map(|cursor| cursor.sequence) != Some(expected)
        {
            return Err(EvidenceError::Conflict);
        }
    } else if page.next_cursor.is_some() {
        return Err(EvidenceError::Conflict);
    }
    Ok(())
}

type RequestFields<'a> = (&'a str, Option<&'a str>, &'a str, &'a str);

fn request(event: &SessionEvent) -> Option<RequestFields<'_>> {
    match &event.kind {
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            producer_plugin_id,
            tool_name,
            arguments_json,
            ..
        }
        | SessionEventKind::PositionedToolCallRequested {
            tool_call_id,
            producer_plugin_id,
            tool_name,
            arguments_json,
            ..
        } => Some((
            tool_call_id,
            producer_plugin_id.as_deref(),
            tool_name,
            arguments_json,
        )),
        _ => None,
    }
}

/// Discover request/result locators in one bounded, forward canonical page.
///
/// Errors reject limits, foreign/conflicting events, and unsupported representations.
/// EOF means only that this page had no further events at retrieval time.
pub fn discover(
    session_id: SessionId,
    page: &SessionHistoryPage,
    after: u64,
    limit: usize,
) -> Result<Discovery, EvidenceError> {
    validate(page, session_id, after, limit)?;
    let mut events = Vec::new();
    for event in &page.events {
        let reference = if let Some((id, _, _, _)) = request(event) {
            Some((id, EventRole::Request))
        } else if let SessionEventKind::ToolInvocationResultRecorded { record } = &event.kind {
            Some((record.invocation_id.as_str(), EventRole::Result))
        } else {
            None
        };
        if let Some((id, role)) = reference {
            if id.is_empty() || id.len() > MAX_ID_BYTES {
                return Err(EvidenceError::LimitExceeded);
            }
            events.push(EventReference {
                sequence: event.sequence,
                invocation_id: id.to_owned(),
                role,
            });
        }
    }
    Ok(Discovery {
        session_id,
        events,
        scanned_events: page.events.len(),
        next_sequence: page.next_cursor.map(|cursor| cursor.sequence),
    })
}

fn text(value: &str, available: &mut usize) -> EvidenceText {
    let mut end = value.len().min(*available);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    *available -= end;
    EvidenceText {
        text: value[..end].to_owned(),
        original_bytes: value.len(),
        retained_bytes: end,
        truncated: end != value.len(),
    }
}

/// Project an exact canonical request/result pair from two single-event pages.
///
/// Errors report missing events, wrong kinds/identities, unsupported versions, or limits.
/// Text truncation is explicit. No plugin artifact or rich-content reference is dereferenced.
pub fn read_pair(
    query: &PairQuery<'_>,
    request_page: &SessionHistoryPage,
    result_page: &SessionHistoryPage,
) -> Result<InvocationEvidence, EvidenceError> {
    if query.byte_limit == 0
        || query.byte_limit > MAX_BYTES
        || query.invocation_id.is_empty()
        || query.invocation_id.len() > MAX_ID_BYTES
    {
        return Err(EvidenceError::LimitExceeded);
    }
    if query.request_sequence >= query.result_sequence {
        return Err(EvidenceError::Conflict);
    }
    validate(request_page, query.session_id, query.request_sequence, 1)?;
    validate(result_page, query.session_id, query.result_sequence, 1)?;
    let requested = request_page.events.first().ok_or(EvidenceError::Missing)?;
    let completed = result_page.events.first().ok_or(EvidenceError::Missing)?;
    let (id, producer, name, arguments) = request(requested).ok_or(EvidenceError::Conflict)?;
    let SessionEventKind::ToolInvocationResultRecorded { record } = &completed.kind else {
        return Err(EvidenceError::Conflict);
    };
    if id != query.invocation_id || record.invocation_id != query.invocation_id {
        return Err(EvidenceError::Conflict);
    }
    if name.len() > MAX_ID_BYTES || producer.is_some_and(|value| value.len() > MAX_ID_BYTES) {
        return Err(EvidenceError::LimitExceeded);
    }
    let mut available = query.byte_limit;
    let arguments_json = text(arguments, &mut available);
    let model_output = text(&record.model_output, &mut available);
    Ok(InvocationEvidence {
        session_id: query.session_id,
        invocation_id: id.to_owned(),
        request_sequence: query.request_sequence,
        result_sequence: query.result_sequence,
        producer_plugin_id: producer.map(str::to_owned),
        tool_name: name.to_owned(),
        arguments_json,
        model_output,
        is_error: record.is_error,
        producer_capture_complete: None,
    })
}

#[cfg(test)]
mod tests;
