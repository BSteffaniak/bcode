//! Bounded, untrusted display projections of native content.
//!
//! Tool inputs, tool outputs, reasoning and usage leave the normalizer only through these
//! helpers: bounded, as display text, and never as dispatchable Bcode tool calls.
use super::{HarnessObservationKind, MAX_HARNESS_ITEMS, MAX_HARNESS_TEXT_BYTES, Value, json};
use crate::live::prefix_at_char_boundary;
use bcode_harness_models::{MAX_HARNESS_PREVIEW_BYTES, MAX_HARNESS_TOOL_INPUT_BYTES};
use std::collections::VecDeque;

const TRUNCATED_SUFFIX: &str = "\n… [truncated]";
/// Native messages whose content block numbering is retained for segment identities.
const MAX_TRACKED_MESSAGES: usize = 32;
/// Upper bound on a native-reported cost (USD) converted to micros.
const MAX_REPORTED_COST_USD: f64 = 1_000_000.0;

/// Running content-block numbering per native message.
///
/// Native stream-json emits one assistant frame per content block, all sharing the API
/// `message.id`; numbering frames in arrival order reproduces the API content-block index
/// that partial `stream_event` frames use, so durable and live identities agree.
#[derive(Default)]
pub struct BlockNumbering {
    messages: VecDeque<(String, usize)>,
}
impl BlockNumbering {
    pub const fn new() -> Self {
        Self {
            messages: VecDeque::new(),
        }
    }

    /// Reserve `count` consecutive block indices for `message_id`; returns the first.
    pub fn reserve(&mut self, message_id: &str, count: usize) -> usize {
        if let Some((_, next)) = self.messages.iter_mut().find(|(id, _)| id == message_id) {
            let first = *next;
            *next = next.saturating_add(count);
            return first;
        }
        if self.messages.len() >= MAX_TRACKED_MESSAGES {
            self.messages.pop_front();
        }
        self.messages.push_back((message_id.to_owned(), count));
        0
    }
}

/// Truncate untrusted display text to `limit` bytes on a character boundary.
/// Returns the text and whether anything was omitted.
pub fn truncate_display(value: &str, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value.to_owned(), false);
    }
    let kept = prefix_at_char_boundary(value, limit.saturating_sub(TRUNCATED_SUFFIX.len()));
    (format!("{kept}{TRUNCATED_SUFFIX}"), true)
}

/// Compact JSON of a native tool input within the tool-input bound.
///
/// Oversized inputs become a valid JSON summary
/// `{"truncated":true,"original_bytes":N,"preview":"<compact JSON prefix>"}`, so the
/// observation always validates as JSON and never exceeds the contract bound.
pub fn bounded_input_json(input: &Value) -> Option<String> {
    let compact = serde_json::to_string(input).ok()?;
    if compact.len() <= MAX_HARNESS_TOOL_INPUT_BYTES {
        return Some(compact);
    }
    let mut limit = MAX_HARNESS_TOOL_INPUT_BYTES / 2;
    while limit > 0 {
        let preview = prefix_at_char_boundary(&compact, limit);
        let summary = serde_json::to_string(&json!({
            "truncated": true,
            "original_bytes": compact.len(),
            "preview": preview,
        }))
        .ok()?;
        if summary.len() <= MAX_HARNESS_TOOL_INPUT_BYTES {
            return Some(summary);
        }
        limit /= 2;
    }
    None
}

/// Pretty, bounded preview of a native tool input for an approval prompt.
pub fn input_preview(input: &Value) -> Option<String> {
    let pretty = serde_json::to_string_pretty(input).ok()?;
    Some(truncate_display(&pretty, MAX_HARNESS_PREVIEW_BYTES).0)
}

/// Native tool results carried by a user frame, as bounded display output.
pub fn tool_results(value: &Value) -> Vec<HarnessObservationKind> {
    let Some(content) = value.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };
    content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .take(MAX_HARNESS_ITEMS)
        .filter_map(|block| {
            let native_tool_id = super::bounded_id(block.get("tool_use_id"))?;
            let (output, truncated) = tool_output(block.get("content"));
            Some(HarnessObservationKind::ToolResult {
                native_tool_id,
                output,
                is_error: block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                truncated,
            })
        })
        .collect()
}

fn tool_output(content: Option<&Value>) -> (String, bool) {
    let mut output = String::new();
    match content {
        Some(Value::String(text)) => output.push_str(text),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                let part = match block.get("type").and_then(Value::as_str) {
                    Some("text") => block.get("text").and_then(Value::as_str).unwrap_or(""),
                    Some("image") => "[image]",
                    Some(_) => "[unsupported content]",
                    None => continue,
                };
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(part);
                if output.len() > MAX_HARNESS_TEXT_BYTES {
                    break;
                }
            }
        }
        _ => {}
    }
    truncate_display(&output, MAX_HARNESS_TEXT_BYTES)
}

/// Portable usage counters from a native result frame, when it reports usage.
pub fn usage(value: &Value, model: Option<String>) -> Option<HarnessObservationKind> {
    let usage = value.get("usage")?.as_object()?;
    let tokens = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    Some(HarnessObservationKind::Usage {
        input_tokens: tokens("input_tokens"),
        output_tokens: tokens("output_tokens"),
        cache_read_input_tokens: tokens("cache_read_input_tokens"),
        cache_creation_input_tokens: tokens("cache_creation_input_tokens"),
        total_cost_micros: value
            .get("total_cost_usd")
            .and_then(Value::as_f64)
            .and_then(cost_micros),
        duration_ms: value.get("duration_ms").and_then(Value::as_u64),
        model,
    })
}

/// Convert a native USD cost to micros; non-finite, negative or implausible values are omitted.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn cost_micros(usd: f64) -> Option<u64> {
    // Range-checked above zero and below 1e12 micros, so the cast is exact enough and lossless
    // in sign; precision beyond a micro-dollar is not meaningful for display.
    (usd.is_finite() && (0.0..=MAX_REPORTED_COST_USD).contains(&usd))
        .then(|| (usd * 1_000_000.0).round() as u64)
}

/// Native model identity usable for usage attribution.
pub fn model_identity(value: Option<&Value>) -> Option<String> {
    super::bounded_id(value).filter(|model| !model.starts_with('<') && model.trim() == model)
}
