//! Request-local, content-free lineage bookkeeping. Never canonical session state.
use bcode_model::{
    ContentBlock, ModelMessage,
    lineage::{
        ContextLineageEntry, MAX_CONTEXT_LINEAGE_ENTRIES, RequestContextLineage,
        RequestContextRepresentation,
    },
};
use bcode_session_models::SessionId;
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
}

impl Incorporated {
    /// Allocate a stable source identity at incorporation time. No block survival is certified.
    /// Returns None when the observation bound or ordinal space is exhausted.
    pub fn record_incorporation(&mut self) -> Option<u64> {
        if self.calls.len() >= MAX_CONTEXT_LINEAGE_ENTRIES {
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

#[derive(Debug, Default)]
pub struct Captures {
    requests: BTreeMap<
        (SessionId, String, u64),
        (RequestContextLineage, Option<RequestContextRepresentation>),
    >,
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
        self.requests.insert(key, (lineage.clone(), None));
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
        let Some((lineage, retained)) =
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
    fn final_builder_retains_mandatory_and_omits_unmatched_optional() {
        let messages = messages();
        let optional = Incorporated {
            calls: vec![11, 12],
            omitted: 0,
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
        assert_eq!(retained, &(lineage, Some(receipt)));
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
