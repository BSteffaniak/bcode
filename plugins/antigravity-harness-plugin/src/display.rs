//! Bounded, untrusted display projections of native Antigravity ACP updates.
//!
//! `agent_message_chunk` and `agent_thought_chunk` stream as transient live updates and finalize
//! into durable segmented `Text` / `Reasoning` observations when the block ends (e.g. a tool call
//! starts, the stream kind changes, or `session/prompt` completes). Observed native tools are
//! display content and never dispatch Bcode tools.
use bcode_harness_models::{
    HarnessLiveStream, HarnessLiveUpdate, HarnessObservationKind, MAX_HARNESS_ID_BYTES,
    MAX_HARNESS_LIVE_UPDATES, MAX_HARNESS_PREVIEW_BYTES, MAX_HARNESS_TEXT_BYTES,
    MAX_HARNESS_TOOL_INPUT_BYTES,
};
use serde_json::{Value, json};
use std::collections::VecDeque;

const TRUNCATED_SUFFIX: &str = "\n… [truncated]";
/// Total live text retained between two observe pages.
const MAX_LIVE_BYTES: usize = 256 * 1024;
/// Upper bound on one coalesced live update.
const MAX_UPDATE_TEXT: usize = 16 * 1024;
/// Native tool items whose start was already presented.
const MAX_STARTED_TOOLS: usize = 64;

struct ActiveSegment {
    segment_id: String,
    stream: HarnessLiveStream,
    offset: u64,
    buffer: String,
    truncated: bool,
    dropped_live: bool,
}

/// Per-session display state. Bounded; cleared on connection loss.
#[derive(Default)]
pub struct Display {
    active: Option<ActiveSegment>,
    segment_seq: u64,
    pending: VecDeque<HarnessLiveUpdate>,
    bytes: usize,
    started_tools: VecDeque<String>,
    model: Option<String>,
    last_finalized_text: Option<String>,
}

impl Display {
    /// Record the native model reported for this session, used to attribute usage.
    pub fn set_model(&mut self, model: Option<&str>) {
        self.model = model.and_then(|value| bounded_id(Some(&Value::String(value.to_owned()))));
    }

    /// Return the current native model id if known.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Whether live updates are waiting for the next page.
    #[must_use]
    pub fn live_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether pending live updates used half of either bound.
    #[must_use]
    pub fn live_near_capacity(&self) -> bool {
        self.pending.len() >= MAX_HARNESS_LIVE_UPDATES / 2 || self.bytes >= MAX_LIVE_BYTES / 2
    }

    /// Take every pending live update for one page. Offered updates are never re-offered.
    pub fn take_live(&mut self) -> Vec<HarnessLiveUpdate> {
        self.bytes = 0;
        self.pending.drain(..).collect()
    }

    /// Reset turn-local last finalized text before a new foreground turn begins.
    pub fn begin_turn(&mut self) {
        self.last_finalized_text = None;
    }

    /// Return the last finalized assistant text of the current turn, if any.
    #[must_use]
    pub fn last_finalized_text(&self) -> Option<&str> {
        self.last_finalized_text.as_deref()
    }

    /// Forget streaming state, for example after the native connection is lost.
    pub fn clear(&mut self) {
        self.active = None;
        self.pending.clear();
        self.bytes = 0;
        self.started_tools.clear();
        self.last_finalized_text = None;
    }

    /// Append a streaming text or reasoning chunk. If the stream kind changes, flushes and returns
    /// the finalized durable observation for the previous segment.
    pub fn push_chunk(
        &mut self,
        stream: HarnessLiveStream,
        text: &str,
    ) -> Option<HarnessObservationKind> {
        if text.is_empty() {
            return None;
        }
        let flushed = if self.active.as_ref().is_some_and(|seg| seg.stream != stream) {
            self.flush_active()
        } else {
            None
        };

        if self.active.is_none() {
            self.segment_seq = self.segment_seq.saturating_add(1);
            let prefix = match stream {
                HarnessLiveStream::Text => "msg",
                HarnessLiveStream::Reasoning => "thought",
            };
            self.active = Some(ActiveSegment {
                segment_id: format!("acp-{prefix}-{}", self.segment_seq),
                stream,
                offset: 0,
                buffer: String::new(),
                truncated: false,
                dropped_live: false,
            });
        }

        if let Some(seg) = self.active.as_mut() {
            let max_durable = MAX_HARNESS_TEXT_BYTES.saturating_sub(TRUNCATED_SUFFIX.len());
            if !seg.truncated {
                let remaining = max_durable.saturating_sub(seg.buffer.len());
                if text.len() <= remaining {
                    seg.buffer.push_str(text);
                } else {
                    let prefix = prefix_at_char_boundary(text, remaining);
                    seg.buffer.push_str(prefix);
                    seg.buffer.push_str(TRUNCATED_SUFFIX);
                    seg.truncated = true;
                }
            }
        }

        self.push_live_chunks(text);
        flushed
    }

    /// Finalize the active text or reasoning segment, returning its durable observation.
    pub fn flush_active(&mut self) -> Option<HarnessObservationKind> {
        let seg = self.active.take()?;
        if seg.buffer.is_empty() {
            return None;
        }
        match seg.stream {
            HarnessLiveStream::Text => {
                self.last_finalized_text = Some(seg.buffer.clone());
                Some(HarnessObservationKind::Text {
                    text: seg.buffer,
                    segment_id: Some(seg.segment_id),
                })
            }
            HarnessLiveStream::Reasoning => Some(HarnessObservationKind::Reasoning {
                segment_id: Some(seg.segment_id),
                text: seg.buffer,
                opaque: false,
            }),
        }
    }

    /// Project a `tool_call` or `tool_call_update` payload into durable `Tool` / `ToolResult`
    /// observations.
    pub fn project_tool_update(&mut self, update: &Value) -> Vec<HarnessObservationKind> {
        let mut out = Vec::new();
        if let Some(flushed) = self.flush_active() {
            out.push(flushed);
        }
        let Some(tool_id) = bounded_id(update.get("toolCallId")) else {
            return out;
        };
        let status = update
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("in_progress");
        let name = update
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| update.get("kind").and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= MAX_HARNESS_ID_BYTES)
            .unwrap_or("tool")
            .to_owned();

        let already_started = self.started_tools.iter().any(|id| id == &tool_id);
        if !already_started {
            if self.started_tools.len() >= MAX_STARTED_TOOLS {
                self.started_tools.pop_front();
            }
            self.started_tools.push_back(tool_id.clone());
            let input_json = update
                .get("rawInput")
                .or_else(|| update.get("args"))
                .and_then(bounded_tool_input);
            out.push(HarnessObservationKind::Tool {
                native_tool_id: tool_id.clone(),
                name,
                status: status.to_owned(),
                input_json,
            });
        }

        if matches!(status, "completed" | "failed" | "cancelled") {
            let is_error = status != "completed"
                || update
                    .get("rawOutput")
                    .and_then(|o| o.get("exit_code"))
                    .and_then(Value::as_i64)
                    .is_some_and(|code| code != 0);
            let raw_text = extract_tool_output(update);
            let (output, truncated) = truncate_text(&raw_text, MAX_HARNESS_TEXT_BYTES);
            out.push(HarnessObservationKind::ToolResult {
                native_tool_id: tool_id,
                output,
                is_error,
                truncated,
            });
        }
        out
    }

    /// Build a bounded `Usage` observation from a `session/prompt` result's `usage` object.
    #[must_use]
    pub fn project_prompt_usage(&self, usage: &Value) -> Option<HarnessObservationKind> {
        let input_tokens = usage.get("inputTokens").and_then(Value::as_u64)?;
        let output_tokens = usage
            .get("outputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_read_input_tokens = usage
            .get("cachedReadTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_creation_input_tokens = usage
            .get("cachedWriteTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Some(HarnessObservationKind::Usage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            total_cost_micros: None,
            duration_ms: None,
            model: self.model.clone(),
        })
    }

    fn push_live_chunks(&mut self, text: &str) {
        let Some(seg) = self.active.as_mut() else {
            return;
        };
        let mut rest = text;
        while !rest.is_empty() {
            let mut chunk = prefix_at_char_boundary(rest, MAX_UPDATE_TEXT);
            if chunk.is_empty() {
                chunk = rest;
            }
            rest = &rest[chunk.len()..];
            let length = chunk.len() as u64;
            if seg.dropped_live {
                seg.offset = seg.offset.saturating_add(length);
                continue;
            }
            let bytes = self.bytes;
            let coalesced = self.pending.back_mut().is_some_and(|last| {
                if last.stream == seg.stream
                    && last.segment_id == seg.segment_id
                    && last.offset.saturating_add(last.text.len() as u64) == seg.offset
                    && last.text.len() + chunk.len() <= MAX_UPDATE_TEXT
                    && bytes + chunk.len() <= MAX_LIVE_BYTES
                {
                    last.text.push_str(chunk);
                    true
                } else {
                    false
                }
            });
            if !coalesced {
                if self.pending.len() >= MAX_HARNESS_LIVE_UPDATES
                    || self.bytes + chunk.len() > MAX_LIVE_BYTES
                {
                    seg.dropped_live = true;
                    seg.offset = seg.offset.saturating_add(length);
                    continue;
                }
                self.pending.push_back(HarnessLiveUpdate {
                    segment_id: seg.segment_id.clone(),
                    stream: seg.stream,
                    offset: seg.offset,
                    text: chunk.to_owned(),
                });
            }
            self.bytes = self.bytes.saturating_add(chunk.len());
            seg.offset = seg.offset.saturating_add(length);
        }
    }
}

/// Extract text from an ACP `agent_message_chunk` or `agent_thought_chunk` update.
#[must_use]
pub fn extract_chunk_text(update: &Value) -> Option<&str> {
    update
        .get("content")
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Format a bounded display preview of an inbound `session/request_permission` tool call.
#[must_use]
pub fn format_permission_preview(tool_call: &Value) -> Option<String> {
    let raw = tool_call
        .get("rawInput")
        .or_else(|| tool_call.get("args"))
        .and_then(|v| serde_json::to_string(v).ok())
        .or_else(|| {
            tool_call
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })?;
    let (preview, _) = truncate_text(&raw, MAX_HARNESS_PREVIEW_BYTES);
    Some(preview)
}

/// Validate and return a bounded identifier string from a JSON value.
#[must_use]
pub fn bounded_id(value: Option<&Value>) -> Option<String> {
    let s = value?.as_str()?.trim();
    if s.is_empty() || s.len() > MAX_HARNESS_ID_BYTES || s.chars().any(char::is_control) {
        return None;
    }
    Some(s.to_owned())
}

fn bounded_tool_input(value: &Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let encoded = serde_json::to_string(value).ok()?;
    if encoded.len() <= MAX_HARNESS_TOOL_INPUT_BYTES {
        Some(encoded)
    } else {
        let summary = json!({
            "truncated": true,
            "bytes": encoded.len(),
        });
        serde_json::to_string(&summary).ok()
    }
}

fn extract_tool_output(update: &Value) -> String {
    if let Some(items) = update.get("content").and_then(Value::as_array) {
        let mut parts = Vec::new();
        for item in items {
            if let Some(text) = item
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
            {
                parts.push(text);
            } else if let Some(text) = item.get("text").and_then(Value::as_str) {
                parts.push(text);
            }
        }
        if !parts.is_empty() {
            return parts.join("\n").replace('\t', "    ");
        }
    }
    if let Some(output) = update
        .get("rawOutput")
        .and_then(|o| o.get("output"))
        .and_then(Value::as_str)
    {
        return output.replace('\t', "    ");
    }
    String::new()
}

/// Truncate UTF-8 text safely at a character boundary, appending a suffix if truncated.
#[must_use]
pub fn truncate_text(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let budget = max_bytes.saturating_sub(TRUNCATED_SUFFIX.len());
    let prefix = prefix_at_char_boundary(text, budget);
    (format!("{prefix}{TRUNCATED_SUFFIX}"), true)
}

fn prefix_at_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
