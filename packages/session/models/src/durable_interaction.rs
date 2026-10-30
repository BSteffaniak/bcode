//! Explicit opt-in producer checkpoints. Payloads are producer contracts, never UI snapshots.

use crate::ToolInvocationResultRecord;
use serde::{Deserialize, Serialize};

/// Version of the independently evolving durable interaction envelope.
pub const DURABLE_INTERACTION_VERSION: u32 = 1;
/// Maximum UTF-8 bytes in any caller-owned identity.
pub const MAX_DURABLE_INTERACTION_ID_BYTES: usize = 256;
/// Maximum bytes in one opaque producer payload.
pub const MAX_DURABLE_INTERACTION_PAYLOAD_BYTES: usize = 64 * 1024;
/// Maximum encoded bytes in a complete operation, including a terminal result.
pub const MAX_DURABLE_INTERACTION_OPERATION_BYTES: usize = 192 * 1024;
/// Receipts are retained, not evicted. Further transitions fail at this ceiling.
pub const MAX_DURABLE_INTERACTION_OPERATIONS: u64 = 128;
/// Cumulative retained operation bytes allowed per interaction.
pub const MAX_DURABLE_INTERACTION_RETAINED_BYTES: u64 = 8 * 1024 * 1024;
/// Maximum metadata entries per recovery page.
pub const MAX_DURABLE_INTERACTION_PAGE: usize = 100;

/// Stable identity scoped by the owning session; no connection or attachment identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableInteractionKey {
    /// Canonical turn owning the invocation.
    pub turn_id: String,
    /// Canonical tool-call identity.
    pub invocation_id: String,
    /// Producer-assigned stable interaction identity.
    pub interaction_id: String,
}

/// Explicit producer-defined durable data. The host does not interpret its contents.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableInteractionPayload {
    /// Producer contract name.
    pub contract_id: String,
    /// Producer contract version; compatibility is decided by the producer.
    pub contract_version: u32,
    /// Bounded JSON containing only deliberately retained domain state.
    pub json: String,
}

impl std::fmt::Debug for DurableInteractionPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableInteractionPayload")
            .field("contract_id", &self.contract_id)
            .field("contract_version", &self.contract_version)
            .field("bytes", &self.json.len())
            .finish_non_exhaustive()
    }
}

/// One atomic interaction transition. Publication records an obligation, not execution.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableInteractionTransition {
    /// Create or replace a nonterminal checkpoint.
    Checkpoint {
        /// Producer-owned deliberate durable checkpoint.
        payload: DurableInteractionPayload,
    },
    /// Seal the interaction and atomically record its terminal tool result.
    Complete {
        /// Producer-defined terminal outcome.
        outcome: DurableInteractionPayload,
        /// Existing canonical invocation result, committed in the same transaction.
        result: Box<ToolInvocationResultRecord>,
        /// Resolution of the original canonical exchange, committed with the outcome and result.
        exchange_resolution: crate::ToolExchangeResolutionEvent,
    },
    /// Publish a single stable continuation obligation for the recorded outcome.
    PublishContinuation {
        /// Caller-stable identity; retry with the identical operation.
        publication_id: String,
    },
}

impl std::fmt::Debug for DurableInteractionTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Checkpoint { payload } => f.debug_tuple("Checkpoint").field(payload).finish(),
            Self::Complete { outcome, .. } => f
                .debug_tuple("Complete")
                .field(outcome)
                .finish_non_exhaustive(),
            Self::PublishContinuation { publication_id } => f
                .debug_tuple("PublishContinuation")
                .field(publication_id)
                .finish(),
        }
    }
}

/// Exact retry envelope. An operation ID cannot be reused with different fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableInteractionOperation {
    /// Must equal [`DURABLE_INTERACTION_VERSION`].
    pub version: u32,
    /// Session-local canonical identity.
    pub key: DurableInteractionKey,
    /// Stable caller-supplied transition identity, scoped to this interaction.
    pub operation_id: String,
    /// Lowercase SHA-256 of the canonical caller request, including its semantic input.
    /// The authenticated caller computes this; it is not a digest of the emitted checkpoint.
    pub request_digest: String,
    /// Producer from the canonical positioned tool-call request.
    pub producer_plugin_id: String,
    /// Exact canonical positioned tool-call request sequence.
    pub request_event_sequence: u64,
    /// Exact canonical original exchange request; fixed for this interaction.
    pub exchange_request_event_sequence: u64,
    /// `None` creates only; existing records require their exact revision.
    pub expected_revision: Option<u64>,
    /// Deliberately durable domain transition.
    pub transition: DurableInteractionTransition,
}

/// Committed identity returned only after the canonical transaction commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableInteractionReceipt {
    /// Envelope version.
    pub version: u32,
    /// Interaction identity.
    pub key: DurableInteractionKey,
    /// Exact operation identity.
    pub operation_id: String,
    /// Interaction-local monotonic revision, starting at one.
    pub revision: u64,
    /// Canonical interaction event sequence.
    pub event_sequence: u64,
    /// Canonical terminal tool-result sequence, when this transition completes the invocation.
    pub tool_result_event_sequence: Option<u64>,
}

/// Latest bounded recovery state, derived exclusively from canonical interaction events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableInteractionRecord {
    /// Envelope version.
    pub version: u32,
    /// Stable identity.
    pub key: DurableInteractionKey,
    /// Canonical producer binding.
    pub producer_plugin_id: String,
    /// Canonical request binding.
    pub request_event_sequence: u64,
    /// Exact canonical original exchange request; fixed for this interaction.
    pub exchange_request_event_sequence: u64,
    /// Latest committed receipt.
    pub receipt: DurableInteractionReceipt,
    /// Latest deliberate checkpoint, retained after completion.
    pub checkpoint: DurableInteractionPayload,
    /// Terminal outcome, never reopened by a new checkpoint.
    pub outcome: Option<DurableInteractionPayload>,
    /// Terminal result reference, retained across subsequent publication.
    pub tool_result_event_sequence: Option<u64>,
    /// At most one publication identity for this outcome.
    pub continuation_publication_id: Option<String>,
    /// Cumulative retained operation bytes; receipts are never silently truncated.
    pub retained_operation_bytes: u64,
}

/// Metadata-only page query; payloads are read individually with exact keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableInteractionQuery {
    /// Maximum returned records, between one and [`MAX_DURABLE_INTERACTION_PAGE`].
    pub limit: usize,
    /// Exclusive canonical interaction-event cursor.
    pub after_event_sequence: Option<u64>,
    /// Pin subsequent pages to the first page's canonical tail.
    pub expected_generation: Option<u64>,
}

/// Small recovery locator; no producer payload or tool result is included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableInteractionLocator {
    /// Identity for exact lookup.
    pub key: DurableInteractionKey,
    /// Latest interaction-event sequence.
    pub event_sequence: u64,
    /// Whether the interaction has a committed outcome.
    pub terminal: bool,
    /// Whether a continuation obligation has been published.
    pub continuation_published: bool,
}

/// Bounded recovery discovery, not execution authority or an event stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableInteractionPage {
    /// Canonical tail pinned by this page.
    pub generation: u64,
    /// Metadata entries in event-sequence order.
    pub entries: Vec<DurableInteractionLocator>,
    /// Continue with this cursor, or stop when absent. Rescan for later arrivals.
    pub next_after_event_sequence: Option<u64>,
}

impl DurableInteractionRecord {
    /// Acknowledge the latest durable recovery classification without granting dispatch authority.
    #[must_use]
    pub const fn recovery_state(&self) -> DurableInteractionRecoveryState {
        if self.continuation_publication_id.is_some() {
            DurableInteractionRecoveryState::ContinuationPublished
        } else if self.outcome.is_some() {
            DurableInteractionRecoveryState::OutcomeCommitted
        } else {
            DurableInteractionRecoveryState::DurableWait
        }
    }
}

/// Bounded recovery observation. None of these states permits original tool re-execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableInteractionRecoveryState {
    /// Restore the compatible producer from its deliberate checkpoint, not its original invocation.
    DurableWait,
    /// The original tool already has a canonical terminal result.
    OutcomeCommitted,
    /// A stable continuation obligation exists; dispatch still requires current authority.
    ContinuationPublished,
}

impl DurableInteractionKey {
    /// Validate bounded nonempty identities.
    ///
    /// # Errors
    /// Returns the invalid identity field.
    pub fn validate(&self) -> Result<(), &'static str> {
        identity(&self.turn_id)?;
        identity(&self.invocation_id)?;
        identity(&self.interaction_id)
    }
}

fn identity(value: &str) -> Result<(), &'static str> {
    if value.trim().is_empty() || value.len() > MAX_DURABLE_INTERACTION_ID_BYTES {
        Err("durable_interaction_identity")
    } else {
        Ok(())
    }
}

impl DurableInteractionPayload {
    /// Validate the envelope and JSON resource bounds, without interpreting producer data.
    ///
    /// # Errors
    /// Rejects empty identities, zero versions, oversized or malformed JSON.
    pub fn validate(&self) -> Result<(), &'static str> {
        identity(&self.contract_id)?;
        if self.contract_version == 0 || self.json.len() > MAX_DURABLE_INTERACTION_PAYLOAD_BYTES {
            return Err("durable_interaction_payload");
        }
        serde_json::from_str::<serde_json::Value>(&self.json)
            .map_err(|_| "durable_interaction_json")?;
        // The byte ceiling bounds total nodes; serde_json's recursion limit bounds depth.
        Ok(())
    }
}

impl DurableInteractionOperation {
    /// Validate caller identities, opaque envelopes, and the complete retained size.
    ///
    /// # Errors
    /// Rejects unsupported envelopes, invalid identities, oversized payloads or mismatched results.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != DURABLE_INTERACTION_VERSION {
            return Err("durable_interaction_version");
        }
        self.key.validate()?;
        identity(&self.operation_id)?;
        if self.request_digest.len() != 64
            || !self
                .request_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("durable_interaction_request_digest");
        }
        identity(&self.producer_plugin_id)?;
        match &self.transition {
            DurableInteractionTransition::Checkpoint { payload } => payload.validate()?,
            DurableInteractionTransition::Complete {
                outcome,
                result,
                exchange_resolution,
            } => {
                identity(&exchange_resolution.exchange_id)?;
                if exchange_resolution.invocation_id != self.key.invocation_id {
                    return Err("durable_interaction_exchange_identity");
                }
                outcome.validate()?;
                if result.presentation.as_ref().is_some_and(|presentation| {
                    presentation.retention == crate::ToolPresentationRetention::ActiveOnly
                }) {
                    return Err("durable_interaction_active_only_presentation");
                }
                if result.invocation_id != self.key.invocation_id {
                    return Err("durable_interaction_result_identity");
                }
            }
            DurableInteractionTransition::PublishContinuation { publication_id } => {
                identity(publication_id)?;
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| "durable_interaction_encoding")?
            .len()
            > MAX_DURABLE_INTERACTION_OPERATION_BYTES
        {
            return Err("durable_interaction_operation_bytes");
        }
        Ok(())
    }
}
