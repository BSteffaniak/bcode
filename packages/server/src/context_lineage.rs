//! Request-local, content-free lineage bookkeeping. Never canonical session state.
use bcode_model::{
    ContentBlock, ModelMessage,
    lineage::{
        ContextLineageEntry, MAX_CONTEXT_LINEAGE_ENTRIES, RequestContextLineage,
        RequestContextRepresentation,
    },
};
use bcode_session_models::SessionId;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

static ORDINAL: AtomicU64 = AtomicU64::new(1);

fn ordinal() -> Option<u64> {
    ORDINAL
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .ok()
}

/// Numeric identities allocated once per actual incorporation, never from call IDs.
#[derive(Default)]
pub struct Incorporated {
    pub calls: Vec<u64>,
    pub omitted: u64,
    whole_blocks: Vec<WholeBlock>,
}

// Private request-local guard; neither quotation bytes nor fingerprints enter lineage exports.
struct WholeBlock {
    entry: ContextLineageEntry,
    fingerprint: [u8; 32],
    message_count: usize,
}

fn user_text_fingerprint(message: &ModelMessage, block_index: usize) -> Option<[u8; 32]> {
    if message.role != bcode_model::MessageRole::User || message.content.len() != 1 {
        return None;
    }
    let ContentBlock::Text { text } = message.content.get(block_index)? else {
        return None;
    };
    Some(Sha256::digest(text.as_bytes()).into())
}

impl Incorporated {
    /// Record a dedicated whole User/Text block at its actual installed position.
    /// Invalid shapes and exhausted observation capacity remain omissions.
    pub fn record_whole_block(&mut self, messages: &[ModelMessage], message_index: usize) {
        if self.calls.len() + self.whole_blocks.len() >= MAX_CONTEXT_LINEAGE_ENTRIES {
            self.omitted = self.omitted.saturating_add(1);
            return;
        }
        let fingerprint = messages
            .get(message_index)
            .and_then(|message| user_text_fingerprint(message, 0));
        let identities = ordinal().zip(ordinal());
        let (Some(fingerprint), Some((source_ordinal, operation_ordinal))) =
            (fingerprint, identities)
        else {
            self.omitted = self.omitted.saturating_add(1);
            return;
        };
        self.whole_blocks.push(WholeBlock {
            entry: ContextLineageEntry {
                source_ordinal,
                operation_ordinal,
                mandatory: false,
                message_index,
                block_index: 0,
            },
            fingerprint,
            message_count: messages.len(),
        });
    }

    /// Merge request-local producer records without weakening their original guards.
    pub fn merge(&mut self, other: Self) {
        self.omitted = self.omitted.saturating_add(other.omitted);
        for call in other.calls {
            if self.calls.len() + self.whole_blocks.len() < MAX_CONTEXT_LINEAGE_ENTRIES {
                self.calls.push(call);
            } else {
                self.omitted = self.omitted.saturating_add(1);
            }
        }
        for block in other.whole_blocks {
            if self.calls.len() + self.whole_blocks.len() < MAX_CONTEXT_LINEAGE_ENTRIES {
                self.whole_blocks.push(block);
            } else {
                self.omitted = self.omitted.saturating_add(1);
            }
        }
    }
    /// Allocate a stable source identity at incorporation time. No block survival is certified.
    /// Returns None when the observation bound or ordinal space is exhausted.
    pub fn record_incorporation(&mut self) -> Option<u64> {
        if self.calls.len() + self.whole_blocks.len() >= MAX_CONTEXT_LINEAGE_ENTRIES {
            self.omitted = self.omitted.saturating_add(1);
            return None;
        }
        let Some(source) = ordinal() else {
            self.omitted = self.omitted.saturating_add(1);
            return None;
        };
        self.calls.push(source);
        Some(source)
    }
}

pub fn fresh_request_ordinal() -> Option<u64> {
    ordinal()
}

pub fn final_lineage(
    messages: &[ModelMessage],
    optional: &Incorporated,
) -> Option<RequestContextLineage> {
    super::xray_context::final_messages(messages);
    let mut lineage = RequestContextLineage {
        version: 1,
        request_ordinal: ordinal()?,
        entries: Vec::new(),
        omitted: optional
            .omitted
            .saturating_add(u64::try_from(optional.calls.len()).unwrap_or(u64::MAX)),
    };
    let operation_ordinal = ordinal()?;
    for (message_index, message) in messages.iter().enumerate() {
        for (block_index, block) in message.content.iter().enumerate() {
            let ContentBlock::ToolResult { .. } = block else {
                continue;
            };
            append_entry(
                &mut lineage,
                operation_ordinal,
                true,
                message_index,
                block_index,
            )?;
            // Final mandatory blocks are observed directly. Optional contribution identity
            // alone cannot prove that hydration, triage or compaction preserved its bytes.
        }
    }
    for block in &optional.whole_blocks {
        let unchanged = messages.len() == block.message_count
            && messages
                .get(block.entry.message_index)
                .and_then(|message| user_text_fingerprint(message, block.entry.block_index))
                == Some(block.fingerprint);
        if unchanged && lineage.entries.len() < MAX_CONTEXT_LINEAGE_ENTRIES {
            lineage.entries.push(block.entry.clone());
        } else {
            lineage.omitted = lineage.omitted.saturating_add(1);
        }
    }
    lineage.validate(messages).ok()?;
    Some(lineage)
}

fn append_entry(
    lineage: &mut RequestContextLineage,
    operation_ordinal: u64,
    mandatory: bool,
    message_index: usize,
    block_index: usize,
) -> Option<()> {
    if lineage.entries.len() == MAX_CONTEXT_LINEAGE_ENTRIES {
        lineage.omitted = lineage.omitted.saturating_add(1);
    } else {
        lineage.entries.push(ContextLineageEntry {
            source_ordinal: ordinal()?,
            operation_ordinal,
            mandatory,
            message_index,
            block_index,
        });
    }
    Some(())
}

type RetainedRequestCapture = (
    RequestContextLineage,
    Option<RequestContextRepresentation>,
    Option<u64>,
);

#[derive(Debug, Default)]
pub struct Captures {
    requests: BTreeMap<(SessionId, String, u64), RetainedRequestCapture>,
}

const MAX_CAPTURED_REQUESTS: usize = 256;
/// UTF-8 byte bound for internal correlation identity, never contribution content.
const MAX_TURN_BYTES: usize = 1024;
// Each retained request has at most MAX_TURN_BYTES string bytes and two bounded
// vectors of fixed-size, content-free entries. Reserve receipt space at admission;
// the count bound therefore also bounds retained payload bytes (not allocator overhead).
const MAX_REQUEST_CAPTURE_BYTES: usize = MAX_TURN_BYTES
    + std::mem::size_of::<RequestContextLineage>()
    + std::mem::size_of::<RequestContextRepresentation>()
    + std::mem::size_of::<Option<u64>>()
    + MAX_CONTEXT_LINEAGE_ENTRIES
        * (std::mem::size_of::<ContextLineageEntry>()
            + std::mem::size_of::<bcode_model::lineage::ContextRepresentationEntry>());
const MAX_CAPTURE_BYTES: usize = MAX_CAPTURED_REQUESTS * MAX_REQUEST_CAPTURE_BYTES;

pub struct CaptureLease<'a> {
    pub captures: &'a std::sync::Mutex<Captures>,
    pub session: SessionId,
    pub turn: &'a str,
}

impl Drop for CaptureLease<'_> {
    fn drop(&mut self) {
        if let Ok(mut captures) = self.captures.lock() {
            captures
                .requests
                .retain(|(session, turn, _), _| *session != self.session || turn != self.turn);
        }
    }
}

impl Captures {
    pub fn insert(&mut self, session: SessionId, turn: &str, lineage: &RequestContextLineage) {
        // Reject before allocating identity or cloning an in-memory producer's vectors.
        if turn.len() > MAX_TURN_BYTES
            || lineage.version != 1
            || lineage.entries.len() > MAX_CONTEXT_LINEAGE_ENTRIES
        {
            return;
        }
        let key = (session, turn.to_owned(), lineage.request_ordinal);
        // Request identity is immutable: reinsertion must never reopen a terminal receipt
        // or replace the lineage against which it was accepted.
        if self.requests.contains_key(&key) {
            return;
        }
        // Reserve the worst-case receipt payload at admission, without retaining bodies.
        if self.requests.len() >= MAX_CAPTURED_REQUESTS
            || self.requests.len() >= MAX_CAPTURE_BYTES / MAX_REQUEST_CAPTURE_BYTES
        {
            self.requests.pop_first();
        }
        self.requests.insert(key, (lineage.clone(), None, None));
    }

    /// Bind the committed `ModelRequestBuilt` event supplied by the owning writer.
    /// Zero, absent scopes and conflicting rebinding fail closed. Event-kind verification
    /// belongs to the caller that appended the canonical event, not this ephemeral cache.
    pub fn bind_request_event(
        &mut self,
        session: SessionId,
        turn: &str,
        request_ordinal: u64,
        sequence: u64,
    ) -> bool {
        if sequence == 0 || turn.len() > MAX_TURN_BYTES {
            return false;
        }
        let Some((_, _, bound)) =
            self.requests
                .get_mut(&(session, turn.to_owned(), request_ordinal))
        else {
            return false;
        };
        if let Some(previous) = bound {
            return *previous == sequence;
        }
        *bound = Some(sequence);
        true
    }

    /// Return validated content-free lineage and its exact committed request-event sequence.
    /// Missing bindings, evictions, foreign scopes and conflicting receipts return `None`.
    /// Accepting a receipt here uses the same immutable first-receipt policy as `observe`.
    pub fn observation(
        &mut self,
        session: SessionId,
        turn: &str,
        receipt: &RequestContextRepresentation,
    ) -> Option<(RequestContextLineage, u64)> {
        if turn.len() > MAX_TURN_BYTES {
            return None;
        }
        let key = (session, turn.to_owned(), receipt.request_ordinal);
        let sequence = self.requests.get(&key)?.2?;
        if !self.observe(session, turn, receipt) {
            return None;
        }
        Some((self.requests.get(&key)?.0.clone(), sequence))
    }

    pub fn observe(
        &mut self,
        session: SessionId,
        turn: &str,
        receipt: &RequestContextRepresentation,
    ) -> bool {
        if turn.len() > MAX_TURN_BYTES {
            return false;
        }
        let Some((lineage, retained, _)) =
            self.requests
                .get_mut(&(session, turn.to_owned(), receipt.request_ordinal))
        else {
            return false;
        };
        if receipt.validate(lineage).is_err() {
            return false;
        }
        if let Some(previous) = retained {
            return previous == receipt;
        }
        *retained = Some(receipt.clone());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn messages() -> Vec<ModelMessage> {
        vec![ModelMessage {
            role: bcode_model::MessageRole::Tool,
            content: vec![ContentBlock::ToolResult {
                result: bcode_model::ToolResult {
                    call_id: "invocation".into(),
                    output: "mandatory".into(),
                    is_error: false,
                    content: Vec::new(),
                },
            }],
        }]
    }
    #[test]
    fn merging_whole_blocks_respects_shared_observation_bound() {
        let messages = vec![ModelMessage {
            role: bcode_model::MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "quotation".to_owned(),
            }],
        }];
        let mut first = Incorporated::default();
        let mut second = Incorporated::default();
        for _ in 0..MAX_CONTEXT_LINEAGE_ENTRIES {
            first.record_whole_block(&messages, 0);
            second.record_whole_block(&messages, 0);
        }
        first.merge(second);
        let lineage = final_lineage(&messages, &first).unwrap();
        assert_eq!(lineage.entries.len(), MAX_CONTEXT_LINEAGE_ENTRIES);
        assert_eq!(lineage.omitted, MAX_CONTEXT_LINEAGE_ENTRIES as u64);
        assert!(lineage.validate(&messages).is_ok());
    }

    #[test]
    fn final_builder_retains_mandatory_and_omits_unmatched_optional() {
        let messages = messages();
        let optional = Incorporated {
            calls: vec![11, 12],
            omitted: 0,
            ..Incorporated::default()
        };
        let lineage = final_lineage(&messages, &optional).unwrap();
        assert!(lineage.validate(&messages).is_ok());
        assert_eq!(lineage.entries.len(), 1);
        assert!(lineage.entries[0].mandatory);
        assert_eq!(lineage.omitted, 2);
        assert_ne!(
            lineage.request_ordinal,
            final_lineage(&messages, &optional).unwrap().request_ordinal
        );
    }
    #[test]
    fn two_sources_in_one_call_never_certify_mutated_or_compacted_output() {
        let mut optional = Incorporated::default();
        let first = optional.record_incorporation().unwrap();
        let second = optional.record_incorporation().unwrap();
        assert_ne!(first, second);
        let original = messages();
        let mut changed = original.clone();
        if let ContentBlock::ToolResult { result } = &mut changed[0].content[0] {
            result.output = "replacement after triage".into();
        }
        for final_messages in [&original[..], &changed[..], &[][..]] {
            let lineage = final_lineage(final_messages, &optional).unwrap();
            assert_eq!(optional.calls, vec![first, second]);
            assert_eq!(lineage.omitted, 2);
            assert!(lineage.entries.iter().all(|entry| entry.mandatory));
            assert!(lineage.validate(final_messages).is_ok());
        }
    }

    #[test]
    fn bounded_capture_is_released_at_scope_terminal() {
        let session = SessionId::new();
        let captures = std::sync::Mutex::new(Captures::default());
        {
            let _lease = CaptureLease {
                captures: &captures,
                session,
                turn: "round",
            };
            for _ in 0..=MAX_CAPTURED_REQUESTS {
                let lineage = final_lineage(&messages(), &Incorporated::default()).unwrap();
                captures.lock().unwrap().insert(session, "round", &lineage);
            }
            assert_eq!(
                captures.lock().unwrap().requests.len(),
                MAX_CAPTURED_REQUESTS
            );
        }
        assert!(captures.lock().unwrap().requests.is_empty());
    }

    #[test]
    fn capture_rejects_oversized_turn_bytes_and_lineage_before_retention() {
        let session = SessionId::new();
        let mut captures = Captures::default();
        let mut lineage = final_lineage(&messages(), &Incorporated::default()).unwrap();
        let boundary = "x".repeat(MAX_TURN_BYTES);
        captures.insert(session, &boundary, &lineage);
        assert_eq!(captures.requests.len(), 1);
        let oversized = "é".repeat(MAX_TURN_BYTES / 2 + 1);
        captures.insert(session, &oversized, &lineage);
        assert_eq!(captures.requests.len(), 1);
        let receipt = RequestContextRepresentation {
            version: 1,
            request_ordinal: lineage.request_ordinal,
            entries: Vec::new(),
            omitted: 1,
        };
        assert!(!captures.observe(session, &oversized, &receipt));
        lineage.entries = vec![lineage.entries[0].clone(); MAX_CONTEXT_LINEAGE_ENTRIES + 1];
        captures.insert(session, "oversized", &lineage);
        assert_eq!(captures.requests.len(), 1);
        lineage.entries.clear();
        lineage.version = 2;
        captures.insert(session, "future", &lineage);
        assert_eq!(captures.requests.len(), 1);
    }

    #[test]
    fn accepted_receipt_survives_reinsertion_and_conflicting_delivery() {
        let session = SessionId::new();
        let lineage = final_lineage(&messages(), &Incorporated::default()).unwrap();
        let mut captures = Captures::default();
        captures.insert(session, "round", &lineage);
        let receipt = RequestContextRepresentation {
            version: 1,
            request_ordinal: lineage.request_ordinal,
            entries: Vec::new(),
            omitted: 1,
        };
        assert!(captures.observe(session, "round", &receipt));
        assert!(captures.observe(session, "round", &receipt));
        let mut conflicting = receipt.clone();
        conflicting.omitted = 0;
        conflicting
            .entries
            .push(bcode_model::lineage::ContextRepresentationEntry {
                source_ordinal: lineage.entries[0].source_ordinal,
                disposition: bcode_model::lineage::ContextRepresentationDisposition::Unknown,
            });
        assert!(conflicting.validate(&lineage).is_ok());
        assert!(!captures.observe(session, "round", &conflicting));
        let mut changed = lineage.clone();
        changed.entries.clear();
        captures.insert(session, "round", &changed);
        assert!(captures.observe(session, "round", &receipt));
        let retained = &captures.requests[&(session, "round".to_owned(), lineage.request_ordinal)];
        assert_eq!(retained, &(lineage, Some(receipt), None));
    }

    #[test]
    fn lease_removes_only_exact_session_and_turn() {
        let session = SessionId::new();
        let foreign = SessionId::new();
        let lineage = final_lineage(&messages(), &Incorporated::default()).unwrap();
        let captures = std::sync::Mutex::new(Captures::default());
        {
            let mut guard = captures.lock().unwrap();
            guard.insert(session, "round", &lineage);
            guard.insert(session, "round-other", &lineage);
            guard.insert(foreign, "round", &lineage);
        }
        drop(CaptureLease {
            captures: &captures,
            session,
            turn: "round",
        });
        let guard = captures.lock().unwrap();
        assert_eq!(guard.requests.len(), 2);
        assert!(guard.requests.contains_key(&(
            session,
            "round-other".to_owned(),
            lineage.request_ordinal
        )));
        assert!(guard.requests.contains_key(&(
            foreign,
            "round".to_owned(),
            lineage.request_ordinal
        )));
        drop(guard);
    }

    #[test]
    fn duplicate_final_calls_count_unknown_optional_sources_once() {
        let mut optional = Incorporated::default();
        optional.record_incorporation().unwrap();
        optional.record_incorporation().unwrap();
        let mut duplicate = messages();
        duplicate.extend(messages());
        let lineage = final_lineage(&duplicate, &optional).unwrap();
        assert_eq!(lineage.omitted, 2);
        assert_eq!(lineage.entries.len(), 2);
        assert!(lineage.entries.iter().all(|entry| entry.mandatory));
        assert!(lineage.validate(&duplicate).is_ok());
        assert!(
            lineage
                .entries
                .iter()
                .all(|entry| !optional.calls.contains(&entry.source_ordinal))
        );
    }

    #[test]
    fn request_event_binding_requires_exact_scope_and_is_immutable() {
        let session = SessionId::new();
        let lineage = final_lineage(&messages(), &Incorporated::default()).unwrap();
        let mut captures = Captures::default();
        captures.insert(session, "round", &lineage);
        let ordinal = lineage.request_ordinal;
        assert!(!captures.bind_request_event(session, "round", ordinal, 0));
        assert!(!captures.bind_request_event(SessionId::new(), "round", ordinal, 7));
        assert!(!captures.bind_request_event(session, "other", ordinal, 7));
        assert!(!captures.bind_request_event(session, "round", ordinal + 1, 7));
        assert!(captures.bind_request_event(session, "round", ordinal, 7));
        assert!(captures.bind_request_event(session, "round", ordinal, 7));
        assert!(!captures.bind_request_event(session, "round", ordinal, 8));
        captures.insert(session, "round", &lineage);
        assert!(!captures.bind_request_event(session, "round", ordinal, 8));
        assert_eq!(
            captures.requests[&(session, "round".to_owned(), ordinal)].2,
            Some(7)
        );
    }

    #[test]
    fn observation_requires_bound_event_and_rejects_conflicting_receipts() {
        let session = SessionId::new();
        let lineage = final_lineage(&messages(), &Incorporated::default()).unwrap();
        let mut captures = Captures::default();
        captures.insert(session, "round", &lineage);
        let receipt = RequestContextRepresentation {
            version: 1,
            request_ordinal: lineage.request_ordinal,
            entries: Vec::new(),
            omitted: 1,
        };
        assert!(captures.observation(session, "round", &receipt).is_none());
        assert!(
            captures.requests[&(session, "round".to_owned(), lineage.request_ordinal)]
                .1
                .is_none()
        );
        assert!(captures.bind_request_event(session, "round", lineage.request_ordinal, 9));
        let mut unsupported = receipt.clone();
        unsupported.version = 2;
        assert!(
            captures
                .observation(session, "round", &unsupported)
                .is_none()
        );
        assert!(
            captures
                .observation(SessionId::new(), "round", &receipt)
                .is_none()
        );
        assert!(captures.observation(session, "other", &receipt).is_none());
        assert_eq!(
            captures.observation(session, "round", &receipt),
            Some((lineage.clone(), 9))
        );
        let mut conflicting = receipt.clone();
        conflicting.omitted = 0;
        conflicting
            .entries
            .push(bcode_model::lineage::ContextRepresentationEntry {
                source_ordinal: lineage.entries[0].source_ordinal,
                disposition: bcode_model::lineage::ContextRepresentationDisposition::Unknown,
            });
        assert!(conflicting.validate(&lineage).is_ok());
        assert!(
            captures
                .observation(session, "round", &conflicting)
                .is_none()
        );
        assert_eq!(
            captures.observation(session, "round", &receipt),
            Some((lineage, 9))
        );
    }

    #[test]
    fn evicted_request_event_cannot_supply_an_observation() {
        let session = SessionId::new();
        let lineage = final_lineage(&messages(), &Incorporated::default()).unwrap();
        let mut captures = Captures::default();
        captures.insert(session, "round", &lineage);
        assert!(captures.bind_request_event(session, "round", lineage.request_ordinal, 9));
        for _ in 0..MAX_CAPTURED_REQUESTS {
            let next = final_lineage(&messages(), &Incorporated::default()).unwrap();
            captures.insert(session, "round", &next);
        }
        let receipt = RequestContextRepresentation {
            version: 1,
            request_ordinal: lineage.request_ordinal,
            entries: Vec::new(),
            omitted: 1,
        };
        assert!(!captures.bind_request_event(session, "round", lineage.request_ordinal, 9));
        assert!(!captures.observe(session, "round", &receipt));
        assert!(captures.observation(session, "round", &receipt).is_none());
    }

    #[test]
    fn capture_never_uses_latest_request_for_foreign_receipt() {
        let session = SessionId::new();
        let lineage = final_lineage(&messages(), &Incorporated::default()).unwrap();
        let mut captures = Captures::default();
        captures.insert(session, "round", &lineage);
        let mut receipt = RequestContextRepresentation {
            version: 1,
            request_ordinal: lineage.request_ordinal,
            entries: Vec::new(),
            omitted: 1,
        };
        assert!(captures.observe(session, "round", &receipt));
        assert!(!captures.observe(session, "other", &receipt));
        receipt.request_ordinal += 1;
        assert!(!captures.observe(session, "round", &receipt));
    }
}
