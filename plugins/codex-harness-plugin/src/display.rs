//! Bounded, untrusted display projections of native Codex items.
//!
//! Agent messages become segmented `Text`, reasoning summaries `Reasoning`, command/file/tool
//! items `Tool` plus `ToolResult`, and per-turn token counters `Usage`. Streaming deltas are
//! transient live updates only: never retained, never digested, and superseded by the durable
//! observation with the same segment identity (the native item id). Observed native tools are
//! display content and never dispatch Bcode tools.
use bcode_harness_models::{
    HarnessLiveStream, HarnessLiveUpdate, HarnessObservationKind, MAX_HARNESS_ID_BYTES,
    MAX_HARNESS_LIVE_UPDATES, MAX_HARNESS_PREVIEW_BYTES, MAX_HARNESS_TEXT_BYTES,
    MAX_HARNESS_TOOL_INPUT_BYTES,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};

const TRUNCATED_SUFFIX: &str = "\n… [truncated]";
/// Total live text retained between two observe pages.
const MAX_LIVE_BYTES: usize = 256 * 1024;
/// Upper bound on one coalesced live update.
const MAX_UPDATE_TEXT: usize = 16 * 1024;
/// Live segments tracked at once; older segments stop streaming (durable text still arrives).
const MAX_LIVE_SEGMENTS: usize = 64;
/// Native tool items whose start was already presented.
const MAX_STARTED_TOOLS: usize = 64;
/// Separator between reasoning summary parts, identical for live and durable text.
const SUMMARY_SEPARATOR: &str = "\n\n";

struct Segment {
    stream: HarnessLiveStream,
    offset: u64,
    summary_index: u64,
    dropped: bool,
}

/// Per-turn token counters accumulated from native per-request usage.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
struct Tokens {
    input: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
}

/// Per-session display state. Bounded; cleared on connection loss.
#[derive(Default)]
pub struct Display {
    segments: BTreeMap<String, Segment>,
    segment_order: VecDeque<String>,
    pending: VecDeque<HarnessLiveUpdate>,
    bytes: usize,
    started_tools: VecDeque<String>,
    usage: Option<(String, Tokens)>,
    model: Option<String>,
}

impl Display {
    /// Record the native model reported for this thread, used to attribute usage.
    pub fn set_model(&mut self, model: Option<&str>) {
        self.model = model.and_then(|model| bounded_id(Some(&Value::String(model.to_owned()))));
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

    /// Forget streaming state, for example after the native connection is lost.
    pub fn clear(&mut self) {
        self.segments.clear();
        self.segment_order.clear();
        self.pending.clear();
        self.bytes = 0;
        self.started_tools.clear();
        self.usage = None;
    }

    /// Consume one native delta notification. Unknown shapes are ignored: live presentation is
    /// optional and never affects durable observations.
    pub fn delta(&mut self, method: &str, params: &Value) {
        let Some(item) = bounded_id(params.get("itemId")) else {
            return;
        };
        let Some(text) = params.get("delta").and_then(Value::as_str) else {
            return;
        };
        match method {
            "item/agentMessage/delta" => self.append(&item, HarnessLiveStream::Text, None, text),
            "item/reasoning/summaryTextDelta" => {
                let index = params
                    .get("summaryIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                self.append(&item, HarnessLiveStream::Reasoning, Some(index), text);
            }
            _ => {}
        }
    }

    /// A durable observation finalizes this segment; later deltas would be stale.
    fn finish_segment(&mut self, item: &str) {
        self.segments.remove(item);
        self.segment_order.retain(|id| id != item);
    }

    fn append(
        &mut self,
        item: &str,
        stream: HarnessLiveStream,
        summary_index: Option<u64>,
        text: &str,
    ) {
        if text.is_empty() {
            return;
        }
        if !self.segments.contains_key(item) {
            if self.segments.len() >= MAX_LIVE_SEGMENTS
                && let Some(oldest) = self.segment_order.pop_front()
            {
                self.segments.remove(&oldest);
            }
            self.segments.insert(
                item.to_owned(),
                Segment {
                    stream,
                    offset: 0,
                    summary_index: summary_index.unwrap_or(0),
                    dropped: false,
                },
            );
            self.segment_order.push_back(item.to_owned());
        }
        let mut separator = "";
        if let (Some(index), Some(segment)) = (summary_index, self.segments.get_mut(item)) {
            if segment.stream != stream || index < segment.summary_index {
                return;
            }
            if index > segment.summary_index {
                segment.summary_index = index;
                if segment.offset > 0 {
                    separator = SUMMARY_SEPARATOR;
                }
            }
        }
        if !separator.is_empty() {
            self.push(item, stream, separator);
        }
        self.push(item, stream, text);
    }

    fn push(&mut self, item: &str, stream: HarnessLiveStream, text: &str) {
        let Some(segment) = self.segments.get_mut(item) else {
            return;
        };
        if segment.stream != stream {
            return;
        }
        let mut rest = text;
        while !rest.is_empty() {
            let mut chunk = prefix_at_char_boundary(rest, MAX_UPDATE_TEXT);
            if chunk.is_empty() {
                chunk = rest;
            }
            rest = &rest[chunk.len()..];
            let length = chunk.len() as u64;
            if segment.dropped {
                segment.offset = segment.offset.saturating_add(length);
                continue;
            }
            let bytes = self.bytes;
            let coalesced = self.pending.back_mut().is_some_and(|last| {
                if last.stream == stream
                    && last.segment_id == item
                    && last.offset.saturating_add(last.text.len() as u64) == segment.offset
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
                    // Later appends would be non-contiguous; durable text stays authoritative.
                    segment.dropped = true;
                    segment.offset = segment.offset.saturating_add(length);
                    continue;
                }
                self.pending.push_back(HarnessLiveUpdate {
                    stream,
                    segment_id: item.to_owned(),
                    offset: segment.offset,
                    text: chunk.to_owned(),
                });
            }
            self.bytes += chunk.len();
            segment.offset = segment.offset.saturating_add(length);
        }
    }

    /// Display observations for a native `item/started`.
    pub fn item_started(&mut self, item: &Value) -> Vec<HarnessObservationKind> {
        match tool_call(item) {
            Some((id, name, input)) => {
                if self.started_tools.iter().any(|known| *known == id) {
                    return Vec::new();
                }
                if self.started_tools.len() >= MAX_STARTED_TOOLS {
                    self.started_tools.pop_front();
                }
                self.started_tools.push_back(id.clone());
                vec![HarnessObservationKind::Tool {
                    native_tool_id: id,
                    name,
                    status: "running".into(),
                    input_json: bounded_input_json(&input),
                }]
            }
            None => Vec::new(),
        }
    }

    /// Display observations for a native `item/completed`, other than agent messages.
    pub fn item_completed(&mut self, item: &Value) -> Vec<HarnessObservationKind> {
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            return self.reasoning(item).into_iter().collect();
        }
        let Some((id, name, input)) = tool_call(item) else {
            return Vec::new();
        };
        let mut kinds = Vec::new();
        if let Some(position) = self.started_tools.iter().position(|known| *known == id) {
            self.started_tools.remove(position);
        } else {
            kinds.push(HarnessObservationKind::Tool {
                native_tool_id: id.clone(),
                name,
                status: "completed".into(),
                input_json: bounded_input_json(&input),
            });
        }
        let (output, is_error) = tool_output(item);
        let (output, truncated) =
            truncate_display(&output.replace('\t', "    "), MAX_HARNESS_TEXT_BYTES);
        kinds.push(HarnessObservationKind::ToolResult {
            native_tool_id: id,
            output,
            is_error,
            truncated,
        });
        kinds
    }

    /// Durable segmented assistant text for a completed native agent message.
    pub fn agent_message(&mut self, item_id: &str, text: &str) -> HarnessObservationKind {
        self.finish_segment(item_id);
        HarnessObservationKind::Text {
            segment_id: Some(item_id.to_owned()),
            text: text.to_owned(),
        }
    }

    fn reasoning(&mut self, item: &Value) -> Option<HarnessObservationKind> {
        let id = bounded_id(item.get("id"))?;
        self.finish_segment(&id);
        let join = |key: &str| {
            item.get(key)
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(Value::as_str)
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join(SUMMARY_SEPARATOR)
                })
                .unwrap_or_default()
        };
        let mut text = join("summary");
        if text.trim().is_empty() {
            text = join("content");
        }
        // Encrypted reasoning without a summary has nothing presentable.
        if text.trim().is_empty() {
            return None;
        }
        Some(HarnessObservationKind::Reasoning {
            segment_id: Some(id),
            text: truncate_display(&text, MAX_HARNESS_TEXT_BYTES).0,
            opaque: false,
        })
    }

    /// Accumulate per-request native usage for the turn it belongs to.
    pub fn token_usage(&mut self, params: &Value) {
        let Some(turn) = bounded_id(params.get("turnId")) else {
            return;
        };
        let last = &params["tokenUsage"]["last"];
        let field = |key: &str| last.get(key).and_then(Value::as_u64).unwrap_or(0);
        let tokens = Tokens {
            input: field("inputTokens"),
            cached: field("cachedInputTokens"),
            cache_write: field("cacheWriteInputTokens"),
            output: field("outputTokens"),
        };
        match &mut self.usage {
            Some((current, total)) if *current == turn => {
                total.input = total.input.saturating_add(tokens.input);
                total.cached = total.cached.saturating_add(tokens.cached);
                total.cache_write = total.cache_write.saturating_add(tokens.cache_write);
                total.output = total.output.saturating_add(tokens.output);
            }
            _ => self.usage = Some((turn, tokens)),
        }
    }

    /// Usage for a terminal native turn, when native usage was reported for it.
    pub fn turn_usage(&mut self, turn: &Value) -> Option<HarnessObservationKind> {
        let id = turn.get("id").and_then(Value::as_str)?;
        if self.usage.as_ref().is_none_or(|(current, _)| current != id) {
            return None;
        }
        let (_, tokens) = self.usage.take()?;
        Some(HarnessObservationKind::Usage {
            // Native input counts include cache reads; the portable shape separates them.
            input_tokens: tokens.input.saturating_sub(tokens.cached),
            output_tokens: tokens.output,
            cache_read_input_tokens: tokens.cached,
            cache_creation_input_tokens: tokens.cache_write,
            total_cost_micros: None,
            duration_ms: turn.get("durationMs").and_then(Value::as_u64),
            model: self.model.clone(),
        })
    }
}

/// Native tool-like items as `(id, name, input)` display projections.
fn tool_call(item: &Value) -> Option<(String, String, Value)> {
    let id = bounded_id(item.get("id"))?;
    let (name, input) = match item.get("type").and_then(Value::as_str)? {
        "commandExecution" => (
            "shell".to_owned(),
            json!({"command": item["command"], "cwd": item["cwd"]}),
        ),
        "fileChange" => {
            let changes = item["changes"]
                .as_array()
                .map(|changes| {
                    changes
                        .iter()
                        .take(64)
                        .map(|change| json!({"path": change["path"], "kind": change["kind"]["type"]}))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            ("apply_patch".to_owned(), json!({"changes": changes}))
        }
        "mcpToolCall" => {
            let server = item["server"].as_str().unwrap_or("mcp");
            let tool = item["tool"].as_str().unwrap_or("tool");
            (format!("mcp__{server}__{tool}"), item["arguments"].clone())
        }
        "dynamicToolCall" => (
            item["tool"].as_str().unwrap_or("dynamic_tool").to_owned(),
            item["arguments"].clone(),
        ),
        "webSearch" => ("web_search".to_owned(), json!({"query": item["query"]})),
        "imageView" => ("view_image".to_owned(), json!({"path": item["path"]})),
        _ => return None,
    };
    let name = bounded_id(Some(&Value::String(name)))?;
    Some((id, name, input))
}

fn tool_output(item: &Value) -> (String, bool) {
    let status = item.get("status").and_then(Value::as_str);
    let failed = matches!(status, Some("failed" | "declined"));
    match item.get("type").and_then(Value::as_str) {
        Some("commandExecution") => {
            let mut output = item["aggregatedOutput"].as_str().unwrap_or("").to_owned();
            if status == Some("declined") && output.is_empty() {
                "Declined".clone_into(&mut output);
            }
            let exit_failed = item["exitCode"].as_i64().is_some_and(|code| code != 0);
            (output, failed || exit_failed)
        }
        Some("fileChange") => {
            let mut output = String::new();
            for change in item["changes"].as_array().into_iter().flatten().take(64) {
                if let Some(diff) = change["diff"].as_str() {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    if let Some(path) = change["path"].as_str() {
                        output.push_str(path);
                        output.push('\n');
                    }
                    output.push_str(diff);
                }
                if output.len() > MAX_HARNESS_TEXT_BYTES {
                    break;
                }
            }
            if status == Some("declined") && output.is_empty() {
                "Declined".clone_into(&mut output);
            }
            (output, failed)
        }
        Some("mcpToolCall") => {
            if let Some(message) = item["error"]["message"].as_str() {
                return (message.to_owned(), true);
            }
            let text = content_text(item["result"]["content"].as_array());
            (text, failed)
        }
        Some("dynamicToolCall") => {
            let text = content_text(item["contentItems"].as_array());
            (text, failed || item["success"] == false)
        }
        _ => (String::new(), failed),
    }
}

fn content_text(items: Option<&Vec<Value>>) -> String {
    let mut output = String::new();
    for item in items.into_iter().flatten().take(64) {
        let part = match item.get("type").and_then(Value::as_str) {
            Some("text" | "inputText") => item["text"].as_str().unwrap_or(""),
            Some("image" | "inputImage") => "[image]",
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
    output
}

/// Bounded approval display metadata for a native callback: `(tool_name, input_preview)`.
#[must_use]
pub fn approval_display(
    method: &str,
    params: &Value,
    diff: Option<&str>,
) -> (String, Option<String>) {
    if method == "item/fileChange/requestApproval" {
        (
            "apply_patch".into(),
            diff.map(|diff| truncate_display(diff, MAX_HARNESS_PREVIEW_BYTES).0),
        )
    } else {
        let preview = params
            .get("command")
            .and_then(Value::as_str)
            .map(|command| {
                let mut preview = command.to_owned();
                if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
                    preview.push_str("\n\ncwd: ");
                    preview.push_str(cwd);
                }
                if let Some(reason) = params.get("reason").and_then(Value::as_str) {
                    preview.push_str("\nreason: ");
                    preview.push_str(reason);
                }
                truncate_display(&preview, MAX_HARNESS_PREVIEW_BYTES).0
            });
        ("shell".into(), preview)
    }
}

/// Native identities usable as observation ids; rejects control characters and padding.
#[must_use]
pub fn bounded_id(value: Option<&Value>) -> Option<String> {
    let id = value?.as_str()?;
    (!id.is_empty()
        && id.len() <= MAX_HARNESS_ID_BYTES
        && id.trim() == id
        && !id.chars().any(char::is_control))
    .then(|| id.to_owned())
}

/// Longest prefix of at most `limit` bytes ending on a character boundary.
#[must_use]
pub fn prefix_at_char_boundary(value: &str, limit: usize) -> &str {
    if value.len() <= limit {
        return value;
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Truncate untrusted display text to `limit` bytes; returns whether anything was omitted.
#[must_use]
pub fn truncate_display(value: &str, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value.to_owned(), false);
    }
    let kept = prefix_at_char_boundary(value, limit.saturating_sub(TRUNCATED_SUFFIX.len()));
    (format!("{kept}{TRUNCATED_SUFFIX}"), true)
}

/// Compact JSON within the tool-input bound; oversized input becomes a valid JSON summary.
#[must_use]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(method: &str, item: &str, text: &str, index: u64) -> (String, Value) {
        (
            method.into(),
            json!({"threadId":"t","turnId":"u","itemId":item,"delta":text,"summaryIndex":index}),
        )
    }

    #[test]
    fn deltas_coalesce_into_contiguous_live_updates_per_item() {
        let mut display = Display::default();
        for (method, params) in [
            delta("item/agentMessage/delta", "msg-1", "Hel", 0),
            delta("item/agentMessage/delta", "msg-1", "lo", 0),
            delta("item/reasoning/summaryTextDelta", "rs-1", "Think", 0),
            delta("item/reasoning/summaryTextDelta", "rs-1", "More", 1),
        ] {
            display.delta(&method, &params);
        }
        let live = display.take_live();
        assert_eq!(live.len(), 2);
        assert_eq!(live[0].segment_id, "msg-1");
        assert_eq!(live[0].stream, HarnessLiveStream::Text);
        assert_eq!((live[0].offset, live[0].text.as_str()), (0, "Hello"));
        assert_eq!(live[1].stream, HarnessLiveStream::Reasoning);
        assert_eq!(live[1].text, "Think\n\nMore");
        assert!(!display.live_pending());
        // Offsets continue across pages.
        display.delta(
            "item/agentMessage/delta",
            &json!({"itemId":"msg-1","delta":"!"}),
        );
        assert_eq!(display.take_live()[0].offset, 5);
        // A durable message finalizes the segment; the durable reasoning text matches live.
        let text = display.agent_message("msg-1", "Hello!");
        assert!(
            matches!(text, HarnessObservationKind::Text { segment_id: Some(ref id), .. } if id == "msg-1")
        );
        let reasoning = display.item_completed(
            &json!({"type":"reasoning","id":"rs-1","summary":["Think","More"],"content":[]}),
        );
        assert!(
            matches!(&reasoning[..], [HarnessObservationKind::Reasoning { text, segment_id: Some(id), opaque: false }] if text == "Think\n\nMore" && id == "rs-1")
        );
        // Encrypted reasoning without text is not presented.
        assert_eq!(
            display
                .item_completed(&json!({"type":"reasoning","id":"rs-2","summary":[],"content":[]})),
            Vec::new()
        );
    }

    #[test]
    fn live_updates_are_bounded_and_drop_rather_than_grow() {
        let mut display = Display::default();
        let chunk = "x".repeat(MAX_UPDATE_TEXT);
        for index in 0..40 {
            display.delta(
                "item/agentMessage/delta",
                &json!({"itemId": format!("m{index}"), "delta": chunk}),
            );
        }
        let live = display.take_live();
        assert!(live.iter().map(|update| update.text.len()).sum::<usize>() <= MAX_LIVE_BYTES);
        assert!(bcode_harness_models::validate_live_updates(&live).is_ok());
    }

    #[test]
    fn command_items_become_tool_and_bounded_result() {
        let mut display = Display::default();
        let started = json!({"type":"commandExecution","id":"call_1","command":"cat README.md",
            "cwd":"/work","status":"inProgress","aggregatedOutput":null,"exitCode":null});
        let kinds = display.item_started(&started);
        assert!(
            matches!(&kinds[..], [HarnessObservationKind::Tool { native_tool_id, name, input_json: Some(input), .. }]
            if native_tool_id == "call_1" && name == "shell" && input.contains("cat README.md"))
        );
        assert_eq!(display.item_started(&started), Vec::new());
        let completed = json!({"type":"commandExecution","id":"call_1","command":"cat README.md",
            "cwd":"/work","status":"completed","aggregatedOutput":"a\tb\n","exitCode":0});
        let kinds = display.item_completed(&completed);
        assert!(
            matches!(&kinds[..], [HarnessObservationKind::ToolResult { output, is_error: false, truncated: false, .. }] if output == "a    b\n")
        );
        // A completion without an observed start still presents the tool first.
        let failed = json!({"type":"commandExecution","id":"call_2","command":"false",
            "cwd":"/work","status":"failed","aggregatedOutput":"x".repeat(MAX_HARNESS_TEXT_BYTES + 10),"exitCode":1});
        let kinds = display.item_completed(&failed);
        assert!(
            matches!(&kinds[..], [HarnessObservationKind::Tool { .. }, HarnessObservationKind::ToolResult { is_error: true, truncated: true, output, .. }] if output.len() <= MAX_HARNESS_TEXT_BYTES)
        );
        for kind in kinds {
            let observation = bcode_harness_models::HarnessObservation {
                observation_id: "o".into(),
                session_id: "s".into(),
                generation: 1,
                native_correlations: Vec::new(),
                completeness: bcode_harness_models::HarnessCompleteness::Complete,
                kind,
            };
            assert!(observation.validate().is_ok());
        }
        let file = json!({"type":"fileChange","id":"patch_1","status":"completed",
            "changes":[{"path":"/work/a.txt","kind":{"type":"add"},"diff":"+hi\n"}]});
        let kinds = display.item_completed(&file);
        assert!(
            matches!(&kinds[..], [HarnessObservationKind::Tool { name, .. }, HarnessObservationKind::ToolResult { output, .. }] if name == "apply_patch" && output.contains("+hi"))
        );
        assert_eq!(
            display.item_completed(&json!({"type":"plan","id":"p","text":"x"})),
            Vec::new()
        );
    }

    #[test]
    fn oversized_tool_input_is_a_valid_json_summary() {
        let input = json!({"command": "y".repeat(MAX_HARNESS_TOOL_INPUT_BYTES * 2)});
        let bounded = bounded_input_json(&input).unwrap();
        assert!(bounded.len() <= MAX_HARNESS_TOOL_INPUT_BYTES);
        let parsed: Value = serde_json::from_str(&bounded).unwrap();
        assert_eq!(parsed["truncated"], true);
    }

    #[test]
    fn usage_accumulates_per_turn_and_separates_cache_reads() {
        let mut display = Display::default();
        display.set_model(Some("gpt-test"));
        for (input, cached, output) in [(100, 40, 10), (50, 50, 5)] {
            display.token_usage(&json!({"threadId":"t","turnId":"turn-1","tokenUsage":{
                "last":{"inputTokens":input,"cachedInputTokens":cached,"cacheWriteInputTokens":0,
                    "outputTokens":output,"reasoningOutputTokens":0,"totalTokens":0},
                "total":{}, "modelContextWindow":null}}));
        }
        assert_eq!(display.turn_usage(&json!({"id":"other"})), None);
        let usage = display.turn_usage(&json!({"id":"turn-1","durationMs":1200}));
        assert_eq!(
            usage,
            Some(HarnessObservationKind::Usage {
                input_tokens: 60,
                output_tokens: 15,
                cache_read_input_tokens: 90,
                cache_creation_input_tokens: 0,
                total_cost_micros: None,
                duration_ms: Some(1200),
                model: Some("gpt-test".into()),
            })
        );
        assert_eq!(display.turn_usage(&json!({"id":"turn-1"})), None);
    }

    #[test]
    fn approval_display_uses_bounded_native_operation_details() {
        let (name, preview) = approval_display(
            "item/commandExecution/requestApproval",
            &json!({"command":"touch notes.txt","cwd":"/work"}),
            None,
        );
        assert_eq!(name, "shell");
        assert_eq!(preview.unwrap(), "touch notes.txt\n\ncwd: /work");
        let (name, preview) = approval_display(
            "item/fileChange/requestApproval",
            &json!({}),
            Some(&"d".repeat(MAX_HARNESS_PREVIEW_BYTES * 2)),
        );
        assert_eq!(name, "apply_patch");
        assert!(preview.unwrap().len() <= MAX_HARNESS_PREVIEW_BYTES);
    }
}
