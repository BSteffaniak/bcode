//! Streaming decode helpers for the `OpenAI` Responses API.
//!
//! These helpers translate decoded Responses stream events into normalized provider turn events.
//! They own no transport, no authentication, and no runtime state: callers supply a
//! [`ResponsesEventSink`] implementation plus the mutable accumulators that carry partial state
//! across events.

use crate::ResponsesEventSink;
use std::collections::BTreeMap;

/// Partial state for one streamed tool call.
#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    /// Provider-assigned tool-call id.
    pub id: Option<String>,
    /// Tool name as reported by the provider.
    pub name: Option<String>,
    /// Accumulated serialized JSON arguments.
    pub arguments: String,
    /// Whether a start event was already reported.
    pub started: bool,
}

/// Partial state for one streamed reasoning item.
#[derive(Debug, Default)]
pub struct ReasoningItemAccumulator {
    /// Provider reasoning item id.
    pub id: Option<String>,
    /// Opaque provider reasoning payload.
    pub encrypted_content: Option<String>,
    /// Summary parts by part index.
    pub summary: BTreeMap<u32, String>,
    /// Raw content parts by part index.
    pub content: BTreeMap<u32, String>,
    /// Whether a start event was already reported.
    pub started: bool,
    /// Whether a completion event was already reported.
    pub finished: bool,
}

impl ReasoningItemAccumulator {
    /// Stable activity id for this reasoning item.
    ///
    /// Falls back to a positional id when the provider has not yet reported an item id.
    #[must_use]
    pub fn activity_id(&self, output_index: u32) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| format!("reasoning-{output_index}"))
    }
}

/// Resolve the output index for a reasoning event.
///
/// Prefers the provider-reported `output_index`, then matches an already-tracked item by id, and
/// finally allocates the next positional index.
#[must_use]
pub fn reasoning_output_index(
    event: &serde_json::Value,
    reasoning_items: &BTreeMap<u32, ReasoningItemAccumulator>,
) -> u32 {
    if let Some(output_index) = event
        .get("output_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
    {
        return output_index;
    }
    let item_id = event
        .get("item_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            event
                .get("item")
                .and_then(|item| item.get("id"))
                .and_then(serde_json::Value::as_str)
        });
    if let Some((index, _)) = reasoning_items
        .iter()
        .find(|(_, item)| item.id.as_deref() == item_id)
    {
        return *index;
    }
    u32::try_from(reasoning_items.len()).unwrap_or(u32::MAX)
}

/// Report a reasoning activity start exactly once per item.
pub fn ensure_reasoning_activity_started(
    sink: &impl ResponsesEventSink,
    output_index: u32,
    item: &mut ReasoningItemAccumulator,
) {
    if item.started {
        return;
    }
    item.started = true;
    sink.push(bcode_model::ProviderTurnEvent::ReasoningActivity {
        event: bcode_session_models::ReasoningActivityEvent::Started {
            activity_id: item.activity_id(output_index),
            order: output_index,
        },
    });
}

/// Part addressing and presentation metadata for one reasoning content kind.
const fn reasoning_part_shape(
    kind: bcode_session_models::ReasoningContentKind,
) -> (&'static str, bcode_session_models::ReasoningContentRole) {
    match kind {
        bcode_session_models::ReasoningContentKind::Summary => (
            "summary",
            bcode_session_models::ReasoningContentRole::Milestone,
        ),
        bcode_session_models::ReasoningContentKind::Raw => {
            ("raw", bcode_session_models::ReasoningContentRole::Detail)
        }
        bcode_session_models::ReasoningContentKind::Legacy => (
            "legacy",
            bcode_session_models::ReasoningContentRole::Unknown,
        ),
    }
}

/// Index of the part addressed by a reasoning event.
fn reasoning_part_index(
    event: &serde_json::Value,
    kind: bcode_session_models::ReasoningContentKind,
) -> u32 {
    match kind {
        bcode_session_models::ReasoningContentKind::Summary => event.get("summary_index"),
        bcode_session_models::ReasoningContentKind::Raw
        | bcode_session_models::ReasoningContentKind::Legacy => event.get("content_index"),
    }
    .and_then(serde_json::Value::as_u64)
    .and_then(|index| u32::try_from(index).ok())
    .unwrap_or(0)
}

/// Select the accumulator part map for one reasoning content kind.
const fn reasoning_parts(
    item: &mut ReasoningItemAccumulator,
    kind: bcode_session_models::ReasoningContentKind,
) -> &mut BTreeMap<u32, String> {
    match kind {
        bcode_session_models::ReasoningContentKind::Summary => &mut item.summary,
        bcode_session_models::ReasoningContentKind::Raw
        | bcode_session_models::ReasoningContentKind::Legacy => &mut item.content,
    }
}

/// Adopt a provider item id onto an accumulator that does not have one yet.
fn adopt_item_id(item: &mut ReasoningItemAccumulator, event: &serde_json::Value) {
    if item.id.is_none()
        && let Some(item_id) = event.get("item_id").and_then(serde_json::Value::as_str)
    {
        item.id = Some(item_id.to_owned());
    }
}

/// Apply an incremental reasoning text delta.
pub fn process_responses_reasoning_delta(
    event: &serde_json::Value,
    sink: &impl ResponsesEventSink,
    reasoning_items: &mut BTreeMap<u32, ReasoningItemAccumulator>,
    kind: bcode_session_models::ReasoningContentKind,
) {
    let Some(delta) = event
        .get("delta")
        .and_then(serde_json::Value::as_str)
        .filter(|delta| !delta.is_empty())
    else {
        return;
    };
    let output_index = reasoning_output_index(event, reasoning_items);
    let part_index = reasoning_part_index(event, kind);
    let item = reasoning_items.entry(output_index).or_default();
    adopt_item_id(item, event);
    ensure_reasoning_activity_started(sink, output_index, item);
    let activity_id = item.activity_id(output_index);
    let (prefix, role) = reasoning_part_shape(kind);
    reasoning_parts(item, kind)
        .entry(part_index)
        .or_default()
        .push_str(delta);
    sink.push(bcode_model::ProviderTurnEvent::ReasoningActivity {
        event: bcode_session_models::ReasoningActivityEvent::PartDelta {
            activity_id,
            activity_order: output_index,
            part_id: format!("{prefix}-{part_index}"),
            kind,
            role,
            part_order: part_index,
            text: delta.to_owned(),
        },
    });
}

/// Apply a completed reasoning text part.
pub fn process_responses_reasoning_done(
    event: &serde_json::Value,
    sink: &impl ResponsesEventSink,
    reasoning_items: &mut BTreeMap<u32, ReasoningItemAccumulator>,
    kind: bcode_session_models::ReasoningContentKind,
) {
    let Some(text) = event.get("text").and_then(serde_json::Value::as_str) else {
        return;
    };
    let output_index = reasoning_output_index(event, reasoning_items);
    let part_index = reasoning_part_index(event, kind);
    let item = reasoning_items.entry(output_index).or_default();
    adopt_item_id(item, event);
    ensure_reasoning_activity_started(sink, output_index, item);
    let activity_id = item.activity_id(output_index);
    let (prefix, role) = reasoning_part_shape(kind);
    reasoning_parts(item, kind).insert(part_index, text.to_owned());
    sink.push(bcode_model::ProviderTurnEvent::ReasoningActivity {
        event: bcode_session_models::ReasoningActivityEvent::PartCompleted {
            activity_id,
            activity_order: output_index,
            part_id: format!("{prefix}-{part_index}"),
            kind,
            role,
            part_order: part_index,
            text: text.to_owned(),
        },
    });
}

/// Apply an incremental tool-call arguments delta.
///
/// A `ToolCallDelta` event is reported only once the provider has assigned a call id, so callers
/// never observe a delta they cannot correlate.
pub fn process_responses_function_arguments_delta(
    event: &serde_json::Value,
    sink: &impl ResponsesEventSink,
    tool_calls: &mut BTreeMap<u32, ToolCallAccumulator>,
) {
    let output_index = reported_output_index(event);
    if let Some(delta) = event.get("delta").and_then(serde_json::Value::as_str) {
        let entry = tool_calls.entry(output_index).or_default();
        entry.arguments.push_str(delta);
        if !delta.is_empty()
            && let Some(call_id) = &entry.id
        {
            sink.push(bcode_model::ProviderTurnEvent::ToolCallDelta {
                call_id: call_id.clone(),
                delta: delta.to_string(),
            });
        }
    }
}

/// Replace accumulated tool-call arguments with the provider's completed arguments.
pub fn process_responses_function_arguments_done(
    event: &serde_json::Value,
    tool_calls: &mut BTreeMap<u32, ToolCallAccumulator>,
) {
    let output_index = reported_output_index(event);
    if let Some(arguments) = event.get("arguments").and_then(serde_json::Value::as_str) {
        tool_calls.entry(output_index).or_default().arguments = arguments.to_string();
    }
}

/// Output index reported directly by a Responses stream event, defaulting to the first slot.
///
/// This is the simple positional read. Reasoning events additionally fall back to id matching;
/// see [`reasoning_output_index`].
#[must_use]
pub fn reported_output_index(event: &serde_json::Value) -> u32 {
    event
        .get("output_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or(0)
}

/// Resolve the tool-call output index for an event.
///
/// Prefers the provider-reported `output_index`, then matches an already-tracked call by
/// `call_id`, and finally allocates the next positional index.
#[must_use]
pub fn tool_call_output_index(
    event: &serde_json::Value,
    item: &serde_json::Value,
    tool_calls: &BTreeMap<u32, ToolCallAccumulator>,
) -> u32 {
    if let Some(output_index) = event
        .get("output_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
    {
        return output_index;
    }
    let call_id = item.get("call_id").and_then(serde_json::Value::as_str);
    if let Some((index, _)) = tool_calls
        .iter()
        .find(|(_, call)| call.id.as_deref() == call_id)
    {
        return *index;
    }
    u32::try_from(tool_calls.len()).unwrap_or(u32::MAX)
}

/// Concatenated assistant text carried by a message output item, when non-empty.
#[must_use]
pub fn responses_output_item_text(event: &serde_json::Value) -> Option<String> {
    let item = event.get("item")?;
    if item.get("type").and_then(serde_json::Value::as_str) != Some("message") {
        return None;
    }
    let text = item
        .get("content")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .filter(|part| {
            matches!(
                part.get("type").and_then(serde_json::Value::as_str),
                Some("output_text" | "text" | "refusal")
            )
        })
        .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

/// Apply a `function_call` output item, reporting a tool-call start once id and name are known.
///
/// `resolve_tool_name` maps the provider-visible tool name back to its original name.
pub fn process_responses_output_item(
    event: &serde_json::Value,
    sink: &impl ResponsesEventSink,
    tool_calls: &mut BTreeMap<u32, ToolCallAccumulator>,
    saw_tool_call: &mut bool,
    resolve_tool_name: &dyn Fn(&str) -> String,
) {
    let Some(item) = event.get("item") else {
        return;
    };
    if item.get("type").and_then(serde_json::Value::as_str) != Some("function_call") {
        return;
    }
    *saw_tool_call = true;
    let output_index = tool_call_output_index(event, item, tool_calls);
    let entry = tool_calls.entry(output_index).or_default();
    if let Some(call_id) = item.get("call_id").and_then(serde_json::Value::as_str) {
        entry.id = Some(call_id.to_string());
    }
    if let Some(name) = item.get("name").and_then(serde_json::Value::as_str) {
        entry.name = Some(name.to_string());
    }
    if let Some(arguments) = item.get("arguments").and_then(serde_json::Value::as_str)
        && !arguments.is_empty()
    {
        entry.arguments = arguments.to_string();
    }
    if !entry.started
        && let (Some(id), Some(name)) = (&entry.id, &entry.name)
    {
        sink.push(bcode_model::ProviderTurnEvent::ToolCallStarted {
            call_id: id.clone(),
            name: resolve_tool_name(name),
        });
        entry.started = true;
    }
}

/// Apply a `reasoning` output item, reporting opaque state, completed parts, and completion.
pub fn process_responses_reasoning_output_item(
    event: &serde_json::Value,
    sink: &impl ResponsesEventSink,
    reasoning_items: &mut BTreeMap<u32, ReasoningItemAccumulator>,
    completed: bool,
) {
    let Some(item_value) = event.get("item") else {
        return;
    };
    if item_value.get("type").and_then(serde_json::Value::as_str) != Some("reasoning") {
        return;
    }
    let output_index = event
        .get("output_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or_else(|| reasoning_output_index(event, reasoning_items));
    let item = reasoning_items.entry(output_index).or_default();
    if let Some(id) = item_value.get("id").and_then(serde_json::Value::as_str) {
        item.id = Some(id.to_owned());
    }
    ensure_reasoning_activity_started(sink, output_index, item);
    let activity_id = item.activity_id(output_index);
    if let Some(encrypted_content) = item_value
        .get("encrypted_content")
        .and_then(serde_json::Value::as_str)
        .filter(|encrypted_content| !encrypted_content.is_empty())
    {
        item.encrypted_content = Some(encrypted_content.to_owned());
        sink.push(bcode_model::ProviderTurnEvent::ReasoningActivity {
            event: bcode_session_models::ReasoningActivityEvent::OpaqueObserved {
                activity_id: activity_id.clone(),
                activity_order: output_index,
            },
        });
    }
    if let Some(summary) = item_value
        .get("summary")
        .and_then(serde_json::Value::as_array)
    {
        for (index, part) in summary.iter().enumerate() {
            let Some(text) = part
                .get("text")
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.is_empty())
            else {
                continue;
            };
            let part_order = u32::try_from(index).unwrap_or(u32::MAX);
            item.summary.insert(part_order, text.to_owned());
            sink.push(bcode_model::ProviderTurnEvent::ReasoningActivity {
                event: bcode_session_models::ReasoningActivityEvent::PartCompleted {
                    activity_id: activity_id.clone(),
                    activity_order: output_index,
                    part_id: format!("summary-{part_order}"),
                    kind: bcode_session_models::ReasoningContentKind::Summary,
                    role: bcode_session_models::ReasoningContentRole::Milestone,
                    part_order,
                    text: text.to_owned(),
                },
            });
        }
    }
    if let Some(content) = item_value
        .get("content")
        .and_then(serde_json::Value::as_array)
    {
        for (index, part) in content.iter().enumerate() {
            if !matches!(
                part.get("type").and_then(serde_json::Value::as_str),
                Some("reasoning_text" | "text")
            ) {
                continue;
            }
            let Some(text) = part
                .get("text")
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.is_empty())
            else {
                continue;
            };
            let part_order = u32::try_from(index).unwrap_or(u32::MAX);
            item.content.insert(part_order, text.to_owned());
            sink.push(bcode_model::ProviderTurnEvent::ReasoningActivity {
                event: bcode_session_models::ReasoningActivityEvent::PartCompleted {
                    activity_id: activity_id.clone(),
                    activity_order: output_index,
                    part_id: format!("raw-{part_order}"),
                    kind: bcode_session_models::ReasoningContentKind::Raw,
                    role: bcode_session_models::ReasoningContentRole::Detail,
                    part_order,
                    text: text.to_owned(),
                },
            });
        }
    }
    if completed && !item.finished {
        item.finished = true;
        sink.push(bcode_model::ProviderTurnEvent::ReasoningActivity {
            event: bcode_session_models::ReasoningActivityEvent::Finished {
                activity_id,
                activity_order: output_index,
                status: bcode_session_models::ReasoningActivityStatus::Completed,
            },
        });
    }
}

/// Reason string for an incomplete Responses stream, defaulting to `response_incomplete`.
///
/// Accepts either a `response.incomplete` event or a bare response object.
#[must_use]
pub fn responses_incomplete_reason(event: &serde_json::Value) -> &str {
    event
        .get("response")
        .unwrap_or(event)
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(serde_json::Value::as_str)
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or("response_incomplete")
}

/// Decode accumulated tool-call arguments into a JSON value.
///
/// Empty or whitespace-only arguments decode to an empty object, matching providers that omit
/// arguments entirely for zero-argument tools.
///
/// # Errors
///
/// Returns a non-retryable `tool_arguments_decode_failed` error when the accumulated arguments are
/// not valid JSON. The message includes the call id, tool name, and byte length, but never the
/// argument content itself, so tool payloads cannot leak into diagnostics.
pub fn parse_tool_arguments(
    arguments: &str,
    call_id: &str,
    tool_name: &str,
) -> Result<serde_json::Value, bcode_model::ProviderError> {
    if arguments.trim().is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(arguments).map_err(|decode_error| {
        let mut error = decode_error_template(format!(
            "failed to decode arguments for tool call {call_id} ({tool_name}): {decode_error}; received {} bytes",
            arguments.len()
        ));
        error.retryable = false;
        error
    })
}

/// Build the `tool_arguments_decode_failed` provider error.
fn decode_error_template(message: String) -> bcode_model::ProviderError {
    bcode_model::ProviderError {
        code: "tool_arguments_decode_failed".to_string(),
        category: bcode_model::ProviderErrorCategory::ProviderInternal,
        message,
        retryable: false,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    }
}

/// One decoded line from a Responses SSE stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponsesStreamLine {
    /// The line carries no payload (comment, blank line, or a non-`data:` field).
    Ignored,
    /// The stream signalled completion with the `[DONE]` sentinel.
    Done,
    /// The line carries a JSON event payload.
    Event(serde_json::Value),
}

/// Classify one already-trimmed SSE line from a Responses stream.
///
/// Only `data:` lines carry payloads; everything else is [`ResponsesStreamLine::Ignored`] so
/// callers can skip comments and framing fields without interpreting them.
///
/// # Errors
///
/// Returns a `stream_decode_failed` error when a `data:` payload is not valid JSON.
pub fn classify_responses_stream_line(
    line: &str,
) -> Result<ResponsesStreamLine, bcode_model::ProviderError> {
    let Some(data) = line.strip_prefix("data: ") else {
        return Ok(ResponsesStreamLine::Ignored);
    };
    if data == "[DONE]" {
        return Ok(ResponsesStreamLine::Done);
    }
    serde_json::from_str::<serde_json::Value>(data)
        .map(ResponsesStreamLine::Event)
        .map_err(|error| stream_decode_failed(&error.to_string()))
}

/// Build the `stream_decode_failed` provider error.
fn stream_decode_failed(message: &str) -> bcode_model::ProviderError {
    bcode_model::ProviderError {
        code: "stream_decode_failed".to_string(),
        category: bcode_model::ProviderErrorCategory::ProviderInternal,
        message: message.to_string(),
        // Matches the provider runtime's default retryability for `ProviderInternal`.
        retryable: true,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    }
}

/// Split a streaming buffer into complete SSE lines, leaving any partial trailing line in place.
///
/// Handles both `\n` and `\r\n` terminators and trims each yielded line, so a caller can feed
/// arbitrarily fragmented network chunks without reimplementing framing.
pub fn drain_complete_stream_lines(buffer: &mut String) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(position) = buffer.find('\n') {
        let mut line = buffer[..position].to_string();
        if line.ends_with('\r') {
            line.pop();
        }
        buffer.drain(..=position);
        lines.push(line.trim().to_string());
    }
    lines
}

/// The event type reported by a Responses stream event, or an empty string when absent.
#[must_use]
pub fn responses_event_type(event: &serde_json::Value) -> &str {
    event
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
}

/// Non-empty assistant text carried by a text or refusal delta event.
#[must_use]
pub fn responses_text_delta(event: &serde_json::Value) -> Option<&str> {
    event
        .get("delta")
        .and_then(serde_json::Value::as_str)
        .filter(|delta| !delta.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        events: std::cell::RefCell<Vec<bcode_model::ProviderTurnEvent>>,
    }

    impl ResponsesEventSink for Recorder {
        fn push(&self, event: bcode_model::ProviderTurnEvent) {
            self.events.borrow_mut().push(event);
        }
    }

    #[test]
    fn stream_lines_classify_by_sse_field() {
        assert_eq!(
            classify_responses_stream_line("data: [DONE]").expect("done decodes"),
            ResponsesStreamLine::Done
        );
        assert_eq!(
            classify_responses_stream_line(r#"data: {"type":"response.completed"}"#)
                .expect("event decodes"),
            ResponsesStreamLine::Event(serde_json::json!({"type": "response.completed"}))
        );
        // Comments, blank lines, and non-`data:` fields carry no payload.
        for line in ["", ": keep-alive", "event: message", "id: 1"] {
            assert_eq!(
                classify_responses_stream_line(line).expect("non-data lines are ignored"),
                ResponsesStreamLine::Ignored,
                "line: {line:?}"
            );
        }
    }

    #[test]
    fn malformed_stream_payloads_fail_with_a_retryable_decode_error() {
        let error = classify_responses_stream_line("data: {not json")
            .expect_err("malformed payload must fail");
        assert_eq!(error.code, "stream_decode_failed");
        // Matches the provider runtime's retryability for `ProviderInternal`.
        assert!(error.retryable);
    }

    #[test]
    fn draining_handles_fragmented_chunks_and_both_line_terminators() {
        let mut buffer = String::from("data: a\r\ndata: b\n");
        assert_eq!(
            drain_complete_stream_lines(&mut buffer),
            vec!["data: a".to_string(), "data: b".to_string()]
        );
        assert!(buffer.is_empty());

        // A partial trailing line stays buffered until its terminator arrives.
        let mut buffer = String::from("data: {\"ty");
        assert!(drain_complete_stream_lines(&mut buffer).is_empty());
        assert_eq!(buffer, "data: {\"ty");
        buffer.push_str("pe\":\"x\"}\n");
        assert_eq!(
            drain_complete_stream_lines(&mut buffer),
            vec!["data: {\"type\":\"x\"}".to_string()]
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn event_type_and_text_delta_accessors_tolerate_missing_fields() {
        assert_eq!(
            responses_event_type(&serde_json::json!({"type": "response.completed"})),
            "response.completed"
        );
        assert_eq!(responses_event_type(&serde_json::json!({})), "");

        assert_eq!(
            responses_text_delta(&serde_json::json!({"delta": "hi"})),
            Some("hi")
        );
        // Empty deltas are treated as absent so callers never emit empty text.
        assert_eq!(
            responses_text_delta(&serde_json::json!({"delta": ""})),
            None
        );
        assert_eq!(responses_text_delta(&serde_json::json!({})), None);
    }

    #[test]
    fn empty_tool_arguments_decode_to_an_empty_object() {
        for arguments in ["", "   ", "\n\t"] {
            let value = parse_tool_arguments(arguments, "call_1", "read")
                .expect("blank arguments must decode");
            assert_eq!(value, serde_json::json!({}));
        }
    }

    #[test]
    fn valid_tool_arguments_decode_verbatim() {
        let value = parse_tool_arguments(r#"{"path":"a.rs","limit":10}"#, "call_1", "read")
            .expect("valid arguments must decode");
        assert_eq!(value, serde_json::json!({"path": "a.rs", "limit": 10}));
    }

    #[test]
    fn malformed_tool_arguments_fail_closed_without_leaking_content() {
        let secret = r#"{"token":"sk-super-secret-value"#;
        let error = parse_tool_arguments(secret, "call_9", "shell.run")
            .expect_err("malformed arguments must fail");

        assert_eq!(error.code, "tool_arguments_decode_failed");
        assert!(
            !error.retryable,
            "a malformed payload will not become valid on retry"
        );
        // Diagnostics identify the call and size but must never echo the payload, which can carry
        // secrets from tool arguments.
        assert!(error.message.contains("call_9"));
        assert!(error.message.contains("shell.run"));
        assert!(error.message.contains(&format!("{} bytes", secret.len())));
        assert!(
            !error.message.contains("sk-super-secret-value"),
            "argument content must not enter diagnostics: {}",
            error.message
        );
    }

    #[test]
    fn incomplete_reason_reads_nested_or_bare_responses_and_defaults() {
        assert_eq!(
            responses_incomplete_reason(&serde_json::json!({
                "response": {"incomplete_details": {"reason": "max_output_tokens"}}
            })),
            "max_output_tokens"
        );
        // A bare response object is accepted too.
        assert_eq!(
            responses_incomplete_reason(&serde_json::json!({
                "incomplete_details": {"reason": "content_filter"}
            })),
            "content_filter"
        );
        // Missing and blank reasons fall back to a stable default.
        assert_eq!(
            responses_incomplete_reason(&serde_json::json!({})),
            "response_incomplete"
        );
        assert_eq!(
            responses_incomplete_reason(&serde_json::json!({
                "incomplete_details": {"reason": "   "}
            })),
            "response_incomplete"
        );
    }

    #[test]
    fn tool_call_output_index_prefers_reported_index_then_call_id_then_position() {
        let mut calls = BTreeMap::new();
        assert_eq!(
            tool_call_output_index(
                &serde_json::json!({"output_index": 4}),
                &serde_json::json!({}),
                &calls
            ),
            4
        );

        calls.insert(
            2,
            ToolCallAccumulator {
                id: Some("call_a".to_string()),
                ..ToolCallAccumulator::default()
            },
        );
        assert_eq!(
            tool_call_output_index(
                &serde_json::json!({}),
                &serde_json::json!({"call_id": "call_a"}),
                &calls
            ),
            2
        );
        assert_eq!(
            tool_call_output_index(
                &serde_json::json!({}),
                &serde_json::json!({"call_id": "unknown"}),
                &calls
            ),
            1
        );
    }

    #[test]
    fn output_item_text_concatenates_only_text_parts() {
        let event = serde_json::json!({
            "item": {
                "type": "message",
                "content": [
                    {"type": "output_text", "text": "a"},
                    {"type": "reasoning_text", "text": "skipped"},
                    {"type": "refusal", "text": "b"},
                ]
            }
        });
        assert_eq!(responses_output_item_text(&event).as_deref(), Some("ab"));

        // Non-message items and empty text yield nothing.
        assert!(
            responses_output_item_text(&serde_json::json!({"item": {"type": "reasoning"}}))
                .is_none()
        );
        assert!(
            responses_output_item_text(&serde_json::json!({
                "item": {"type": "message", "content": []}
            }))
            .is_none()
        );
    }

    #[test]
    fn function_call_output_item_reports_start_once_and_maps_the_tool_name() {
        let recorder = Recorder::default();
        let mut calls = BTreeMap::new();
        let mut saw_tool_call = false;
        let event = serde_json::json!({
            "output_index": 0,
            "item": {
                "type": "function_call",
                "call_id": "call_1",
                "name": "provider_name",
                "arguments": "{}"
            }
        });

        let rename = |name: &str| format!("original::{name}");
        process_responses_output_item(&event, &recorder, &mut calls, &mut saw_tool_call, &rename);
        process_responses_output_item(&event, &recorder, &mut calls, &mut saw_tool_call, &rename);

        assert!(saw_tool_call);
        let events = recorder.events.borrow();
        assert_eq!(events.len(), 1, "start must be reported exactly once");
        assert!(matches!(
            &events[0],
            bcode_model::ProviderTurnEvent::ToolCallStarted { call_id, name }
                if call_id == "call_1" && name == "original::provider_name"
        ));
        assert_eq!(calls[&0].arguments, "{}");
    }

    #[test]
    fn non_function_call_output_items_are_ignored_by_the_tool_handler() {
        let recorder = Recorder::default();
        let mut calls = BTreeMap::new();
        let mut saw_tool_call = false;

        process_responses_output_item(
            &serde_json::json!({"item": {"type": "message"}}),
            &recorder,
            &mut calls,
            &mut saw_tool_call,
            &|name: &str| name.to_string(),
        );

        assert!(!saw_tool_call);
        assert!(calls.is_empty());
        assert!(recorder.events.borrow().is_empty());
    }

    #[test]
    fn reasoning_output_item_records_opaque_state_parts_and_completion() {
        let recorder = Recorder::default();
        let mut items = BTreeMap::new();
        let event = serde_json::json!({
            "output_index": 0,
            "item": {
                "type": "reasoning",
                "id": "rs_1",
                "encrypted_content": "opaque",
                "summary": [{"text": "sum"}],
                "content": [
                    {"type": "reasoning_text", "text": "raw"},
                    {"type": "image", "text": "ignored"}
                ]
            }
        });

        process_responses_reasoning_output_item(&event, &recorder, &mut items, true);

        let item = &items[&0];
        assert_eq!(item.encrypted_content.as_deref(), Some("opaque"));
        assert_eq!(item.summary.get(&0).map(String::as_str), Some("sum"));
        assert_eq!(item.content.get(&0).map(String::as_str), Some("raw"));
        assert!(!item.content.contains_key(&1), "non-text parts are skipped");
        assert!(item.finished);

        // Completion is reported only once.
        let before = recorder.events.borrow().len();
        process_responses_reasoning_output_item(&event, &recorder, &mut items, true);
        let finished = recorder
            .events
            .borrow()
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    bcode_model::ProviderTurnEvent::ReasoningActivity {
                        event: bcode_session_models::ReasoningActivityEvent::Finished { .. }
                    }
                )
            })
            .count();
        assert_eq!(finished, 1);
        assert!(recorder.events.borrow().len() >= before);
    }

    #[test]
    fn tool_argument_deltas_accumulate_and_only_emit_with_a_call_id() {
        let recorder = Recorder::default();
        let mut calls = BTreeMap::new();

        // No call id yet: arguments accumulate silently.
        process_responses_function_arguments_delta(
            &serde_json::json!({"output_index": 0, "delta": "{\"a\""}),
            &recorder,
            &mut calls,
        );
        assert!(recorder.events.borrow().is_empty());
        assert_eq!(calls[&0].arguments, "{\"a\"");

        calls.get_mut(&0).expect("entry exists").id = Some("call_1".to_string());
        process_responses_function_arguments_delta(
            &serde_json::json!({"output_index": 0, "delta": ":1}"}),
            &recorder,
            &mut calls,
        );
        assert_eq!(calls[&0].arguments, "{\"a\":1}");
        assert_eq!(recorder.events.borrow().len(), 1);

        // Empty deltas never emit.
        process_responses_function_arguments_delta(
            &serde_json::json!({"output_index": 0, "delta": ""}),
            &recorder,
            &mut calls,
        );
        assert_eq!(recorder.events.borrow().len(), 1);
    }

    #[test]
    fn completed_tool_arguments_replace_accumulated_text() {
        let mut calls = BTreeMap::new();
        calls.insert(
            0,
            ToolCallAccumulator {
                arguments: "partial".to_string(),
                ..ToolCallAccumulator::default()
            },
        );

        process_responses_function_arguments_done(
            &serde_json::json!({"output_index": 0, "arguments": "{\"done\":true}"}),
            &mut calls,
        );
        assert_eq!(calls[&0].arguments, "{\"done\":true}");
    }

    #[test]
    fn missing_output_index_defaults_to_the_first_slot() {
        assert_eq!(reported_output_index(&serde_json::json!({})), 0);
        assert_eq!(
            reported_output_index(&serde_json::json!({"output_index": 5})),
            5
        );
    }

    #[test]
    fn reasoning_output_index_prefers_reported_index_then_id_then_position() {
        let mut items = BTreeMap::new();
        assert_eq!(
            reasoning_output_index(&serde_json::json!({"output_index": 7}), &items),
            7
        );

        items.insert(
            3,
            ReasoningItemAccumulator {
                id: Some("rs_1".to_string()),
                ..ReasoningItemAccumulator::default()
            },
        );
        assert_eq!(
            reasoning_output_index(&serde_json::json!({"item_id": "rs_1"}), &items),
            3
        );
        // Nested `item.id` is also honored.
        assert_eq!(
            reasoning_output_index(&serde_json::json!({"item": {"id": "rs_1"}}), &items),
            3
        );
        // Unknown ids allocate the next positional index.
        assert_eq!(
            reasoning_output_index(&serde_json::json!({"item_id": "other"}), &items),
            1
        );
    }

    #[test]
    fn reasoning_start_is_reported_exactly_once() {
        let recorder = Recorder::default();
        let mut item = ReasoningItemAccumulator::default();
        ensure_reasoning_activity_started(&recorder, 0, &mut item);
        ensure_reasoning_activity_started(&recorder, 0, &mut item);
        assert_eq!(recorder.events.borrow().len(), 1);
        assert!(item.started);
    }

    #[test]
    fn activity_id_falls_back_to_positional_id() {
        let item = ReasoningItemAccumulator::default();
        assert_eq!(item.activity_id(4), "reasoning-4");

        let identified = ReasoningItemAccumulator {
            id: Some("rs_9".to_string()),
            ..ReasoningItemAccumulator::default()
        };
        assert_eq!(identified.activity_id(4), "rs_9");
    }

    #[test]
    fn summary_and_raw_deltas_accumulate_into_separate_part_maps() {
        let recorder = Recorder::default();
        let mut items = BTreeMap::new();

        process_responses_reasoning_delta(
            &serde_json::json!({"output_index": 0, "summary_index": 0, "delta": "sum"}),
            &recorder,
            &mut items,
            bcode_session_models::ReasoningContentKind::Summary,
        );
        process_responses_reasoning_delta(
            &serde_json::json!({"output_index": 0, "content_index": 1, "delta": "raw"}),
            &recorder,
            &mut items,
            bcode_session_models::ReasoningContentKind::Raw,
        );

        let item = &items[&0];
        assert_eq!(item.summary.get(&0).map(String::as_str), Some("sum"));
        assert_eq!(item.content.get(&1).map(String::as_str), Some("raw"));
    }

    #[test]
    fn empty_and_missing_reasoning_text_is_ignored() {
        let recorder = Recorder::default();
        let mut items = BTreeMap::new();

        process_responses_reasoning_delta(
            &serde_json::json!({"output_index": 0, "delta": ""}),
            &recorder,
            &mut items,
            bcode_session_models::ReasoningContentKind::Summary,
        );
        process_responses_reasoning_done(
            &serde_json::json!({"output_index": 0}),
            &recorder,
            &mut items,
            bcode_session_models::ReasoningContentKind::Summary,
        );

        assert!(recorder.events.borrow().is_empty());
        assert!(items.is_empty());
    }

    #[test]
    fn completed_part_replaces_accumulated_delta_text() {
        let recorder = Recorder::default();
        let mut items = BTreeMap::new();

        process_responses_reasoning_delta(
            &serde_json::json!({"output_index": 0, "summary_index": 0, "delta": "par"}),
            &recorder,
            &mut items,
            bcode_session_models::ReasoningContentKind::Summary,
        );
        process_responses_reasoning_done(
            &serde_json::json!({"output_index": 0, "summary_index": 0, "text": "partial done"}),
            &recorder,
            &mut items,
            bcode_session_models::ReasoningContentKind::Summary,
        );

        assert_eq!(
            items[&0].summary.get(&0).map(String::as_str),
            Some("partial done")
        );
    }
}
