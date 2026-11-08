//! Transient live text from native partial-message stream events.
//!
//! Live updates are presentation hints only: never retained, never part of the observation
//! digest, and superseded by the durable `Text`/`Reasoning` observation with the same
//! segment identity (`<message.id>:<content block index>`). Under pressure updates are
//! dropped rather than queued; durable text stays authoritative.
#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;

use bcode_harness_models::{
    HarnessLiveStream, HarnessLiveUpdate, MAX_HARNESS_ID_BYTES, MAX_HARNESS_LIVE_UPDATES,
};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};

/// Total live text retained between two observe pages.
const MAX_LIVE_BYTES: usize = 256 * 1024;
/// Upper bound on one coalesced update's text.
const MAX_UPDATE_TEXT: usize = 16 * 1024;
/// Content blocks tracked for the current native message.
const MAX_LIVE_BLOCKS: usize = 64;

struct Block {
    stream: HarnessLiveStream,
    /// Running UTF-8 byte offset of the next append within the segment's live text.
    offset: u64,
    /// An append was dropped; later appends would be non-contiguous, so suppress them.
    dropped: bool,
}

/// Bounded per-process live buffer. Cleared once offered; never a delivery record.
#[derive(Default)]
pub struct LiveBuffer {
    message_id: Option<String>,
    blocks: BTreeMap<u64, Block>,
    pending: VecDeque<HarnessLiveUpdate>,
    bytes: usize,
}

impl LiveBuffer {
    pub const fn new() -> Self {
        Self {
            message_id: None,
            blocks: BTreeMap::new(),
            pending: VecDeque::new(),
            bytes: 0,
        }
    }

    /// Whether live updates are waiting for the next observe page.
    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether the pending updates have used half of either bound. A waiting page stops
    /// coalescing here so later deltas are offered rather than dropped.
    pub fn near_capacity(&self) -> bool {
        self.pending.len() >= MAX_HARNESS_LIVE_UPDATES / 2 || self.bytes >= MAX_LIVE_BYTES / 2
    }

    /// Take every pending update for one page. Offered updates are never re-offered.
    pub fn take(&mut self) -> Vec<HarnessLiveUpdate> {
        self.bytes = 0;
        self.pending.drain(..).collect()
    }

    /// Forget all stream state, for example after the native connection is lost.
    pub fn clear(&mut self) {
        self.message_id = None;
        self.blocks.clear();
        self.pending.clear();
        self.bytes = 0;
    }

    /// Consume one native `stream_event` frame. Unknown or malformed shapes are ignored:
    /// live presentation is optional and never affects durable observations.
    pub fn stream_event(&mut self, value: &Value) {
        // Subagent streams belong to native child work, not the foreground transcript.
        if value
            .get("parent_tool_use_id")
            .is_some_and(|id| !id.is_null())
        {
            return;
        }
        let Some(event) = value.get("event") else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.blocks.clear();
                self.message_id = event
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .filter(|id| valid_message_id(id))
                    .map(str::to_owned);
            }
            Some("content_block_start") => {
                let Some(index) = event.get("index").and_then(Value::as_u64) else {
                    return;
                };
                let block = event.get("content_block");
                let (stream, initial) = match block
                    .and_then(|block| block.get("type"))
                    .and_then(Value::as_str)
                {
                    Some("text") => (HarnessLiveStream::Text, block.and_then(|b| b.get("text"))),
                    Some("thinking") => (
                        HarnessLiveStream::Reasoning,
                        block.and_then(|b| b.get("thinking")),
                    ),
                    _ => {
                        self.blocks.remove(&index);
                        return;
                    }
                };
                if !self.blocks.contains_key(&index) && self.blocks.len() >= MAX_LIVE_BLOCKS {
                    return;
                }
                self.blocks.insert(
                    index,
                    Block {
                        stream,
                        offset: 0,
                        dropped: false,
                    },
                );
                if let Some(text) = initial.and_then(Value::as_str) {
                    self.append(index, stream, text);
                }
            }
            Some("content_block_delta") => {
                let Some(index) = event.get("index").and_then(Value::as_u64) else {
                    return;
                };
                let Some(delta) = event.get("delta") else {
                    return;
                };
                let (stream, text) = match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => (HarnessLiveStream::Text, delta.get("text")),
                    Some("thinking_delta") => (HarnessLiveStream::Reasoning, delta.get("thinking")),
                    // Signatures and tool-input JSON deltas are never presented live.
                    _ => return,
                };
                if let Some(text) = text.and_then(Value::as_str) {
                    self.append(index, stream, text);
                }
            }
            _ => {}
        }
    }

    fn append(&mut self, index: u64, stream: HarnessLiveStream, text: &str) {
        if text.is_empty() {
            return;
        }
        let Some(message_id) = &self.message_id else {
            return;
        };
        // Offsets are only meaningful from an observed block start.
        let Some(block) = self.blocks.get_mut(&index) else {
            return;
        };
        if block.stream != stream {
            return;
        }
        let segment_id = format!("{message_id}:{index}");
        let mut rest = text;
        while !rest.is_empty() {
            let chunk = prefix_at_char_boundary(rest, MAX_UPDATE_TEXT);
            let chunk = if chunk.is_empty() {
                // A single character never exceeds the bound; defensive only.
                rest
            } else {
                chunk
            };
            rest = &rest[chunk.len()..];
            let length = chunk.len() as u64;
            if block.dropped {
                block.offset = block.offset.saturating_add(length);
                continue;
            }
            let coalesced = self.pending.back_mut().is_some_and(|last| {
                if last.stream == stream
                    && last.segment_id == segment_id
                    && last.offset.saturating_add(last.text.len() as u64) == block.offset
                    && last.text.len() + chunk.len() <= MAX_UPDATE_TEXT
                    && self.bytes + chunk.len() <= MAX_LIVE_BYTES
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
                    block.dropped = true;
                    block.offset = block.offset.saturating_add(length);
                    continue;
                }
                self.pending.push_back(HarnessLiveUpdate {
                    stream,
                    segment_id: segment_id.clone(),
                    offset: block.offset,
                    text: chunk.to_owned(),
                });
            }
            self.bytes += chunk.len();
            block.offset = block.offset.saturating_add(length);
        }
    }
}

/// Native message identities used as segment prefixes; leaves room for `:<index>`.
pub fn valid_message_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_HARNESS_ID_BYTES - 24
        && id.trim() == id
        && !id.chars().any(char::is_control)
}

/// Longest prefix of at most `limit` bytes ending on a character boundary.
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
