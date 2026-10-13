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
        // Evicted requests cannot be validated against a newer estimate or request.
        if self.requests.len() >= MAX_CAPTURED_REQUESTS {
            self.requests.pop_first();
        }
        self.requests.insert(
            (session, turn.to_owned(), lineage.request_ordinal),
            (lineage.clone(), None),
        );
    }

    pub fn observe(
        &mut self,
        session: SessionId,
        turn: &str,
        receipt: &RequestContextRepresentation,
    ) -> bool {
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
