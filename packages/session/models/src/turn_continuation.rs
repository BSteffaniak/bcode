//! Explicit original-turn continuation envelopes. No process, renderer or credential state.
use crate::{SessionId, ToolInvocationResultRecord};
use serde::{Deserialize, Serialize};

/// Independently versioned continuation contract.
pub const TURN_CONTINUATION_VERSION: u32 = 1;
/// Recoverability admission only; never a limit on ordinary tool dispatch.
pub const MAX_TURN_CONTINUATION_MEMBERS: usize = 256;
/// Maximum complete serialized operation envelope.
pub const MAX_TURN_CONTINUATION_BYTES: usize = 256 * 1024;
/// Receipts are never evicted; further operations reject at this ceiling.
pub const MAX_TURN_CONTINUATION_OPERATIONS: u64 = 1024;
/// Cumulative operation bytes retained per continuation.
pub const MAX_TURN_CONTINUATION_RETAINED_BYTES: u64 = 8 * 1024 * 1024;

/// Session-owned original-turn identity, not a new turn or execution permit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContinuationKey {
    /// Canonical session.
    pub session_id: SessionId,
    /// Original activated turn.
    pub turn_id: String,
    /// Caller-stable continuation identity.
    pub continuation_id: String,
}

/// Exact canonical source locator. Digest encoding belongs to the session codec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContinuationSourceRef {
    /// Canonical event sequence.
    pub sequence: u64,
    /// Exact persisted kind name.
    pub kind: String,
    /// Domain-separated lowercase SHA-256 of the canonical current event envelope.
    pub digest: String,
}

/// One ordered batch member. Raw arguments remain solely in canonical source events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContinuationMember {
    /// Stable invocation identity.
    pub invocation_id: String,
    /// Exact positioned request source.
    pub request: TurnContinuationSourceRef,
}

/// Deliberately captured host semantic loop state; the storage layer never interprets it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContinuationHostState {
    /// Host-owned semantic schema, never a controller snapshot schema.
    pub schema: String,
    /// Version of the host semantic contract.
    pub version: u32,
    /// Deliberately retained JSON, excluding credentials, handles and raw tool arguments.
    pub json: String,
}
impl std::fmt::Debug for TurnContinuationHostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnContinuationHostState")
            .field("schema", &self.schema)
            .field("version", &self.version)
            .field("bytes", &self.json.len())
            .finish_non_exhaustive()
    }
}

/// Immutable original accepted execution and complete ordered batch membership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableTurnContinuationV1 {
    /// Envelope version.
    pub version: u32,
    /// Original identity.
    pub key: TurnContinuationKey,
    /// Canonical accepted submission identity.
    pub submission_id: String,
    /// Original model-work identity; execution owner validates its current authority.
    pub model_work_id: String,
    /// Complete batch identity.
    pub batch_id: String,
    /// Accepted submission event.
    pub accepted: TurnContinuationSourceRef,
    /// Original activation event, never synthesized on recovery.
    pub activated: TurnContinuationSourceRef,
    /// Accepted or recovery-authorized effective descriptor source.
    pub descriptor_source: TurnContinuationSourceRef,
    /// Digest of the complete effective descriptor including its durable config recipe.
    pub descriptor_digest: String,
    /// Selected recoverable invocation; it must occur exactly once in members.
    pub selected_invocation_id: String,
    /// Original selected exchange identity, not a renderer/controller identity.
    pub selected_exchange_id: String,
    /// Complete ordered membership; admission rejects oversized batches without truncation.
    pub members: Vec<TurnContinuationMember>,
    /// Explicit, versioned deliberate host semantics at the post-batch boundary.
    pub host_state: TurnContinuationHostState,
}

/// One member's execution ambiguity boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnContinuationMemberState {
    /// No authorized dispatch marker committed.
    Pending,
    /// Side effects may have occurred; never automatically dispatch again.
    Started { dispatch_id: String },
    /// Canonical result committed atomically with settlement.
    Settled {
        result_ref: TurnContinuationSourceRef,
    },
}

/// Original model-loop continuation phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnContinuationPhase {
    /// Siblings must settle; this path does not dispatch them.
    AwaitingBatchSettlement,
    /// Every member has an exact canonical result.
    ReadyForModel,
    /// Next provider dispatch claimed; unknown remote outcome cannot be replayed.
    ModelDispatchStarted { dispatch_id: String },
    /// Canonical successor/outcome proves retirement.
    Retired {
        successor_ref: TurnContinuationSourceRef,
    },
}

/// Typed CAS transition, not an arbitrary event append interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnContinuationTransition {
    /// Immutable create-only capture before dispatch/readiness acknowledgement.
    Prepare {
        capture: Box<DurableTurnContinuationV1>,
    },
    /// Commit before invoking a member.
    StartMember {
        invocation_id: String,
        dispatch_id: String,
    },
    /// Commit a member's result and settlement together. Selected interactive completion
    /// additionally requires the combined durable-interaction transaction (not this API).
    SettleMember {
        invocation_id: String,
        dispatch_id: String,
        result: Box<ToolInvocationResultRecord>,
    },
    /// Explicit policy denial/cancellation before dispatch; never invent a start marker.
    SettleWithoutDispatch {
        invocation_id: String,
        result: Box<ToolInvocationResultRecord>,
    },
    /// Attach the selected pure completion's already-committed-in-this-transaction result.
    /// Accepted only in the four-event combined interaction/continuation batch.
    AttachSelectedResult {
        invocation_id: String,
        result_ref: TurnContinuationSourceRef,
    },
    /// CAS only after all members settle; host post-batch advancement happens once here.
    ReadyForModel,
    /// Commit before the next provider request.
    StartModel { dispatch_id: String },
    /// Retire only with exact canonical successor evidence.
    Retire {
        successor_ref: TurnContinuationSourceRef,
    },
}

/// Caller-stable attachment identity for the selected pure completion. Result reference
/// is derived inside the transaction, never supplied independently by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContinuationSettlement {
    /// Original continuation key.
    pub key: TurnContinuationKey,
    /// Exact current revision.
    pub expected_revision: u64,
    /// Stable attachment operation identity.
    pub operation_id: String,
    /// Canonical input digest for this attachment operation.
    pub request_digest: String,
}

/// Both original receipts returned only after the four-event transaction commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionContinuationReceipt {
    /// Original durable interaction receipt.
    pub interaction: crate::DurableInteractionReceipt,
    /// Original selected membership attachment receipt.
    pub continuation: TurnContinuationReceipt,
}

/// Exact caller-stable operation identity, including semantic input digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContinuationOperation {
    /// Operation envelope version.
    pub version: u32,
    /// Original continuation identity.
    pub key: TurnContinuationKey,
    /// Stable retry identity scoped to this continuation.
    pub operation_id: String,
    /// Caller input digest, never inferred from emitted state.
    pub request_digest: String,
    /// None creates only; existing records require exact revision.
    pub expected_revision: Option<u64>,
    /// Typed operation.
    pub transition: TurnContinuationTransition,
}

/// Commit-before-ack receipt, retained for the continuation's complete active lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContinuationReceipt {
    /// Envelope version.
    pub version: u32,
    /// Original key.
    pub key: TurnContinuationKey,
    /// Exact retry identity.
    pub operation_id: String,
    /// Monotonic revision.
    pub revision: u64,
    /// Canonical transition event sequence.
    pub event_sequence: u64,
    /// Result atomically attached by settlement, if any.
    pub result_ref: Option<TurnContinuationSourceRef>,
}

/// Checkpointed bounded original-turn recovery record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContinuationRecord {
    /// Immutable complete capture.
    pub capture: DurableTurnContinuationV1,
    /// Ordered states, parallel to capture.members.
    pub members: Vec<TurnContinuationMemberState>,
    /// Exact operation locator for each member's latest state, in capture order.
    pub member_operation_ids: Vec<String>,
    /// Durable recovery phase.
    pub phase: TurnContinuationPhase,
    /// Latest acknowledgement.
    pub receipt: TurnContinuationReceipt,
    /// Immutable prepare operation locator; no history scan is needed to prove capture.
    pub prepare_operation_id: String,
    /// Exact retained cumulative operation bytes; never silently evicted.
    pub retained_operation_bytes: u64,
}

/// Original operation plus its original acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContinuationOperationRecord {
    /// Deliberate original operation.
    pub operation: TurnContinuationOperation,
    /// Original committed receipt.
    pub receipt: TurnContinuationReceipt,
}

/// Validate bounded identity without accepting arbitrary whitespace identities.
/// # Errors
/// Rejects empty or overlong identities.
pub fn validate_turn_continuation_id(value: &str) -> Result<(), &'static str> {
    if value.trim().is_empty() || value.len() > 256 {
        Err("turn continuation identity")
    } else {
        Ok(())
    }
}
/// Validate lowercase SHA-256 syntax without guessing an input digest.
/// # Errors
/// Rejects noncanonical digest syntax.
pub fn validate_turn_continuation_digest(value: &str) -> Result<(), &'static str> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err("turn continuation digest")
    }
}
impl TurnContinuationOperation {
    /// Validate resource bounds and deliberately captured envelopes.
    /// # Errors
    /// Rejects unsupported versions, invalid identities, malformed JSON and admission exhaustion.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != TURN_CONTINUATION_VERSION {
            return Err("turn continuation version");
        }
        validate_turn_continuation_id(&self.key.turn_id)?;
        validate_turn_continuation_id(&self.key.continuation_id)?;
        validate_turn_continuation_id(&self.operation_id)?;
        validate_turn_continuation_digest(&self.request_digest)?;
        if let TurnContinuationTransition::Prepare { capture } = &self.transition {
            if capture.version != TURN_CONTINUATION_VERSION
                || capture.key != self.key
                || capture.members.is_empty()
                || capture.members.len() > MAX_TURN_CONTINUATION_MEMBERS
            {
                return Err("turn continuation capture");
            }
            for id in [
                &capture.submission_id,
                &capture.model_work_id,
                &capture.batch_id,
                &capture.selected_invocation_id,
                &capture.selected_exchange_id,
                &capture.host_state.schema,
            ] {
                validate_turn_continuation_id(id)?;
            }
            validate_turn_continuation_digest(&capture.descriptor_digest)?;
            let mut ids = std::collections::BTreeSet::new();
            for member in &capture.members {
                validate_turn_continuation_id(&member.invocation_id)?;
                if !ids.insert(&member.invocation_id) {
                    return Err("duplicate continuation member");
                }
            }
            if !ids.contains(&capture.selected_invocation_id) {
                return Err("missing selected continuation member");
            }
            if capture.host_state.version == 0
                || capture.host_state.json.len() > MAX_TURN_CONTINUATION_BYTES
            {
                return Err("turn continuation host state");
            }
            serde_json::from_str::<serde_json::Value>(&capture.host_state.json)
                .map_err(|_| "turn continuation host json")?;
        }
        let source = |source: &TurnContinuationSourceRef| -> Result<(), &'static str> {
            validate_turn_continuation_id(&source.kind)?;
            validate_turn_continuation_digest(&source.digest)
        };
        match &self.transition {
            TurnContinuationTransition::Prepare { capture } => {
                for reference in [
                    &capture.accepted,
                    &capture.activated,
                    &capture.descriptor_source,
                ] {
                    source(reference)?;
                }
                for member in &capture.members {
                    source(&member.request)?;
                }
            }
            TurnContinuationTransition::StartMember {
                invocation_id,
                dispatch_id,
            }
            | TurnContinuationTransition::SettleMember {
                invocation_id,
                dispatch_id,
                ..
            } => {
                validate_turn_continuation_id(invocation_id)?;
                validate_turn_continuation_id(dispatch_id)?;
            }
            TurnContinuationTransition::SettleWithoutDispatch { invocation_id, .. } => {
                validate_turn_continuation_id(invocation_id)?;
            }
            TurnContinuationTransition::AttachSelectedResult {
                invocation_id,
                result_ref,
            } => {
                validate_turn_continuation_id(invocation_id)?;
                source(result_ref)?;
            }
            TurnContinuationTransition::StartModel { dispatch_id } => {
                validate_turn_continuation_id(dispatch_id)?;
            }
            TurnContinuationTransition::Retire { successor_ref } => source(successor_ref)?,
            TurnContinuationTransition::ReadyForModel => {}
        }
        if serde_json::to_vec(self)
            .map_err(|_| "turn continuation encoding")?
            .len()
            > MAX_TURN_CONTINUATION_BYTES
        {
            return Err("turn continuation bytes");
        }
        Ok(())
    }
}
