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
