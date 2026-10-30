//! Versioned renderer-neutral dynamic interaction-controller contracts.
//!
//! The plugin owns transitions. The host retains opaque state only for the lifetime
//! of a pending exchange; neither state nor snapshots imply durable resumability.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use bcode_tool_models::interaction::{
    InteractionControlId, InteractionInput, InteractionNavigation, InteractionOutput,
    InteractionValue,
};

/// Secret-safe controller application failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionControllerError {
    /// No compatible exact producer/schema controller.
    Unsupported,
    /// Unsupported version, identity, route or transition.
    InvalidContract,
    /// Owner/revision or conflicting duplicate mismatch.
    Conflict,
    /// No longer pending.
    NotPending,
    /// Encoded size or ephemeral lifecycle limit exceeded.
    LimitExceeded,
    /// Plugin or application unavailable, cancelled or timed out.
    Unavailable,
    /// Caller lacks consumer/session authority.
    Unauthorized,
}
impl InteractionControllerError {
    /// Stable secret-safe diagnostic.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Unsupported => "no compatible interaction controller",
            Self::InvalidContract => "invalid or unsupported interaction controller contract",
            Self::Conflict => "interaction controller owner or revision conflict",
            Self::NotPending => "interaction is no longer pending",
            Self::LimitExceeded => "interaction controller resource limit exceeded",
            Self::Unavailable => "interaction controller unavailable",
            Self::Unauthorized => "interaction consumer is not authorized",
        }
    }
}

/// Independently versioned plugin service interface.
pub const INTERACTION_CONTROLLER_INTERFACE: &str = "bcode.interaction-controller/v1";
/// Current controller contract version.
pub const INTERACTION_CONTROLLER_VERSION: u32 = 1;
/// Maximum encoded request, state, snapshot or transition size.
pub const MAX_CONTROLLER_BYTES: usize = 256 * 1024;
/// Maximum routes in one producer's discovery response.
pub const MAX_CONTROLLER_ROUTES: usize = 64;

/// Producer-owned controller route; no renderer or transport identity is involved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionControllerRoute {
    /// Exact producer owning the pending exchange.
    pub producer_id: String,
    /// Exact opaque exchange request schema.
    pub exchange_schema: String,
    /// First supported request schema version.
    pub min_schema_version: u32,
    /// Last supported request schema version, inclusive.
    pub max_schema_version: u32,
    /// Plugin-owned semantic kind.
    pub interaction_kind: String,
}

impl InteractionControllerRoute {
    /// Return whether this route accepts the exact producer/schema/version.
    #[must_use]
    pub fn supports(&self, producer: &str, schema: &str, version: u32) -> bool {
        self.producer_id == producer
            && self.exchange_schema == schema
            && self.min_schema_version <= version
            && version <= self.max_schema_version
    }
}

/// Discovery request for a versioned controller interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionControllerDiscoveryRequest {
    /// Required contract version; future versions fail closed.
    pub version: u32,
}

/// Dynamically discovered routes from one loaded producer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionControllerDiscovery {
    /// Contract version.
    pub version: u32,
    /// Bounded producer-owned routes.
    pub routes: Vec<InteractionControllerRoute>,
}

/// Host-to-plugin transition request. State is host-owned ephemeral data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InteractionControllerRequest {
    /// Initialize from a pending exchange without interpreting its opaque payload.
    Open {
        /// Required contract version.
        version: u32,
        /// Pinned producer route.
        route: InteractionControllerRoute,
        /// Request schema version.
        schema_version: u32,
        /// Original exchange payload.
        payload: Value,
    },
    /// Apply semantic input to the previous ephemeral plugin state.
    Input {
        /// Required contract version.
        version: u32,
        /// Pinned producer route.
        route: InteractionControllerRoute,
        /// Opaque state from the previous transition.
        state: Value,
        /// Renderer-neutral semantic input.
        input: InteractionInput,
    },
}

/// Plugin-owned transition. No side effect is committed merely by returning it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionControllerTransition {
    /// Contract version.
    pub version: u32,
    /// Exact semantic kind from the pinned route.
    pub interaction_kind: String,
    /// Opaque ephemeral state; never exposed in public snapshots or persisted.
    pub state: Value,
    /// Renderer-neutral plugin-owned presentation snapshot.
    pub snapshot: Value,
    /// Semantic effect for the application owner to consume.
    pub output: InteractionOutput,
}

/// Public bounded semantic view, without opaque controller state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionControllerSnapshot {
    /// Contract version.
    pub version: u32,
    /// Canonical pending exchange identity.
    pub exchange_id: String,
    /// Pinned producer and schema identity.
    pub route: InteractionControllerRoute,
    /// Exact request schema version.
    pub schema_version: u32,
    /// Monotonic host revision used for conflict-safe input.
    pub revision: u64,
    /// Plugin-owned semantic snapshot.
    pub snapshot: Value,
    /// Last semantic effect; terminal effects are stable until lifecycle cleanup.
    pub output: InteractionOutput,
}

/// Separate opt-in durable producer protocol. Implementing these DTOs alone does not
/// establish persistence, acknowledgment, execution authority, or recovery in a host.
pub const DURABLE_INTERACTION_VERSION: u32 = 1;
/// Discovery and pure checkpoint/continuation service; distinct from ephemeral v1.
pub const DURABLE_INTERACTION_SERVICE_ID: &str = "bcode.interaction-durable/v1";

/// Explicit compatibility fence for a producer checkpoint and pure continuation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableInteractionCapability {
    /// Exact ephemeral controller route.
    pub route: InteractionControllerRoute,
    /// Producer-owned durable representation identifier.
    pub checkpoint_schema: String,
    /// Exact supported checkpoint version; never downgrade an unknown version.
    pub checkpoint_version: u32,
    /// Producer-owned deterministic completion compatibility identifier.
    pub continuation_schema: String,
    /// Exact completion version.
    pub continuation_version: u32,
}

/// Original canonical identity, not an execution or attachment authority grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableInteractionBinding {
    /// Original session identity.
    pub session_id: String,
    /// Original turn identity.
    pub turn_id: String,
    /// Original invocation; completion must never allocate a replacement invocation.
    pub invocation_id: String,
    /// Canonical positioned invocation request sequence.
    pub request_sequence: u64,
    /// Original exchange identity.
    pub exchange_id: String,
    /// Original tool identity.
    pub tool_name: String,
    /// Host-verified SHA-256 of the original canonical request (lowercase hex).
    pub request_digest: String,
    /// Exact exchange request version.
    pub exchange_schema_version: u32,
    /// Pinned producer and recovery compatibility fence.
    pub capability: DurableInteractionCapability,
}

/// Stable retry identity. Hosts retain the exact admitted request with its receipt;
/// the same identity with different content conflicts, including after resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableInteractionOperation {
    /// Caller-generated identity, stable across transport retries.
    pub operation_id: String,
    /// Exact committed predecessor revision.
    pub expected_revision: u64,
}

/// Durable input admission is separate from ephemeral controller v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableInteractionInput {
    /// Durable protocol version.
    pub version: u32,
    /// Original canonical identity.
    pub binding: DurableInteractionBinding,
    /// Retry identity and optimistic revision.
    pub operation: DurableInteractionOperation,
    /// Renderer-neutral semantic input.
    pub input: InteractionInput,
}

/// Public projection of a canonically committed durable interaction.
///
/// Contains no producer checkpoint or ephemeral controller state. Constructing or
/// validating this value does not establish persistence or execution authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableInteractionView {
    /// Durable protocol version.
    pub version: u32,
    /// Original canonical identity and producer compatibility.
    pub binding: DurableInteractionBinding,
    /// Committed host revision, equal to the public snapshot revision.
    pub revision: u64,
    /// Public producer semantics and outcome, including retained terminal outcomes.
    pub snapshot: InteractionControllerSnapshot,
}

/// Bounded canonical discovery request; no producer checkpoint or execution authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableInteractionQuery {
    /// Maximum returned views, in the inclusive range 1..=100.
    pub limit: u32,
    /// Exclusive canonical event-sequence cursor from the previous page.
    pub after_event_sequence: Option<u64>,
    /// Pin continuation to the original page generation; mismatch requires restarting discovery.
    pub expected_generation: Option<u64>,
}

impl DurableInteractionQuery {
    /// Validate the caller-supplied page bound.
    ///
    /// # Errors
    /// Rejects zero or over-budget limits; never clamps caller requests.
    pub const fn validate(&self) -> Result<(), InteractionControllerError> {
        if self.limit < 1 || self.limit > 100 {
            return Err(InteractionControllerError::LimitExceeded);
        }
        Ok(())
    }
}

/// Bounded public discovery results from one canonical session generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableInteractionPage {
    /// Canonical discovery generation, not an execution grant.
    pub generation: u64,
    /// Public producer semantics and verified original bindings; never checkpoints.
    pub views: Vec<DurableInteractionView>,
    /// Exclusive cursor for continuation, including for an empty filtered page.
    pub next_after_event_sequence: Option<u64>,
}

impl DurableInteractionPage {
    /// Validate the entire response against the requested session, generation and budget.
    ///
    /// # Errors
    /// Rejects malformed views/bindings, foreign sessions, stale generations, nonadvancing
    /// cursors or over-budget responses. Never silently truncates results.
    pub fn validate(
        &self,
        session_id: &str,
        query: &DurableInteractionQuery,
    ) -> Result<(), InteractionControllerError> {
        query.validate()?;
        if !durable_identifier(session_id) {
            return Err(InteractionControllerError::InvalidContract);
        }
        if u32::try_from(self.views.len()).map_or(true, |count| count > query.limit) {
            return Err(InteractionControllerError::LimitExceeded);
        }
        if query
            .expected_generation
            .is_some_and(|generation| generation != self.generation)
        {
            return Err(InteractionControllerError::Conflict);
        }
        if self.next_after_event_sequence.is_some_and(|next| {
            query
                .after_event_sequence
                .is_some_and(|after| next <= after)
        }) {
            return Err(InteractionControllerError::InvalidContract);
        }
        for view in &self.views {
            if view.binding.session_id != session_id {
                return Err(InteractionControllerError::Unauthorized);
            }
            view.validate()?;
        }
        validate_durable_size(self)
    }
}

/// Original commit receipt retained for exact retries, including after resolution.
///
/// Hosts may return it only after canonical commit; this DTO cannot prove that commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableInteractionReceipt {
    /// Durable protocol version.
    pub version: u32,
    /// Exact admitted binding, operation identity, predecessor revision and semantic input.
    pub input: DurableInteractionInput,
    /// Original committed result, not a newer view substituted during retry.
    pub view: DurableInteractionView,
}

impl DurableInteractionView {
    /// Validate compatibility, identity correspondence and bounded public semantics.
    ///
    /// # Errors
    /// Rejects unknown versions, inconsistent snapshots, malformed bindings or oversize.
    pub fn validate(&self) -> Result<(), InteractionControllerError> {
        self.binding.validate()?;
        if self.version != DURABLE_INTERACTION_VERSION
            || self.snapshot.version != INTERACTION_CONTROLLER_VERSION
            || self.snapshot.exchange_id != self.binding.exchange_id
            || self.snapshot.route != self.binding.capability.route
            || self.snapshot.schema_version != self.binding.exchange_schema_version
            || self.snapshot.revision != self.revision
        {
            return Err(InteractionControllerError::InvalidContract);
        }
        validate_durable_size(self)
    }
}

impl DurableInteractionReceipt {
    /// Validate exact operation identity and its original committed successor view.
    ///
    /// Terminal output is permitted so retries never need to reopen an exchange.
    ///
    /// # Errors
    /// Rejects unknown versions, malformed operations, mismatched identity/revision or oversize.
    pub fn validate(&self) -> Result<(), InteractionControllerError> {
        self.input.validate()?;
        self.view.validate()?;
        if self.version != DURABLE_INTERACTION_VERSION
            || self.input.binding != self.view.binding
            || self.input.operation.expected_revision.checked_add(1) != Some(self.view.revision)
        {
            return Err(InteractionControllerError::InvalidContract);
        }
        validate_durable_size(self)
    }
}

/// Explicit producer export, never a blind copy of ephemeral controller state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerInteractionCheckpoint {
    /// Original identity and compatibility fence.
    pub binding: DurableInteractionBinding,
    /// Committed host revision, supplied by the host during export.
    pub revision: u64,
    /// Bounded, producer-defined durable representation; not a frontend snapshot.
    pub payload: Value,
}

/// Pure post-exchange result, to be committed by the current execution owner.
/// Hosts reconcile this with the original invocation, never invoke the tool again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableInvocationCompletion {
    /// Exact original identity.
    pub binding: DurableInteractionBinding,
    /// Stable identity of the canonical terminal resolution.
    pub completion_id: String,
    /// Serialized existing tool response contract, interpreted by the tool owner.
    pub response: Value,
}

/// Separate versioned producer operations. Export accepts ephemeral data only as
/// input; only the explicit producer export may become canonical durable data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DurableInteractionRequest {
    /// Discover exact opt-in capabilities; grants no execution authority.
    Discover {
        /// Protocol version.
        version: u32,
    },
    /// Export one validated producer transition before host acknowledgment.
    Export {
        /// Protocol version.
        version: u32,
        /// Canonical identity verified by the application owner.
        binding: DurableInteractionBinding,
        /// Proposed host revision.
        revision: u64,
        /// Producer transition, including ephemeral state (never persisted verbatim).
        transition: InteractionControllerTransition,
    },
    /// Restore producer state from the exact durable representation.
    Restore {
        /// Protocol version.
        version: u32,
        /// Expected original identity.
        binding: DurableInteractionBinding,
        /// Canonical checkpoint.
        checkpoint: ProducerInteractionCheckpoint,
    },
    /// Deterministic, side-effect-free completion of a committed terminal exchange.
    Complete {
        /// Protocol version.
        version: u32,
        /// Expected original identity.
        binding: DurableInteractionBinding,
        /// Canonical terminal checkpoint, including producer terminal outcome.
        checkpoint: ProducerInteractionCheckpoint,
        /// Stable canonical completion identity; retries reuse this value.
        completion_id: String,
    },
}

/// Producer responses. A host must commit before acknowledging or publishing them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DurableInteractionResponse {
    /// Exact opt-in capabilities.
    Capabilities(Vec<DurableInteractionCapability>),
    /// Explicit producer checkpoint.
    Exported(ProducerInteractionCheckpoint),
    /// Recreated ephemeral controller and authoritative producer snapshot.
    Restored(InteractionControllerTransition),
    /// Pure terminal response; not evidence that the host committed it.
    Completed(DurableInvocationCompletion),
}

fn durable_identifier(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

/// Validate an encoded durable message without allocating its serialized contents.
///
/// # Errors
/// Rejects representations exceeding the controller byte bound or serialization errors.
pub fn validate_durable_size(value: &impl Serialize) -> Result<(), InteractionControllerError> {
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("durable interaction size exceeded"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(MAX_CONTROLLER_BYTES), value)
        .map_err(|_| InteractionControllerError::LimitExceeded)
}

impl DurableInteractionBinding {
    /// Check bounded identities and explicit compatibility fields. Does not verify
    /// request digest contents, canonical existence, producer support, or ownership.
    ///
    /// # Errors
    /// Returns `InvalidContract` for malformed fields and `LimitExceeded` for size.
    pub fn validate(&self) -> Result<(), InteractionControllerError> {
        let c = &self.capability;
        let r = &c.route;
        if ![
            &self.session_id,
            &self.turn_id,
            &self.invocation_id,
            &self.exchange_id,
            &self.tool_name,
            &r.producer_id,
            &r.exchange_schema,
            &r.interaction_kind,
            &c.checkpoint_schema,
            &c.continuation_schema,
        ]
        .into_iter()
        .all(|v| durable_identifier(v))
            || self.request_sequence == 0
            || c.checkpoint_version == 0
            || c.continuation_version == 0
            || r.min_schema_version == 0
            || r.min_schema_version > r.max_schema_version
            || !(r.min_schema_version..=r.max_schema_version)
                .contains(&self.exchange_schema_version)
            || self.request_digest.len() != 64
            || !self
                .request_digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(InteractionControllerError::InvalidContract);
        }
        validate_durable_size(self)
    }
}

impl ProducerInteractionCheckpoint {
    /// Check identity and encoded bounds; payload validation remains producer-owned.
    ///
    /// # Errors
    /// Rejects malformed bindings and oversized checkpoints.
    pub fn validate(&self) -> Result<(), InteractionControllerError> {
        self.binding.validate()?;
        validate_durable_size(self)
    }
}

impl DurableInvocationCompletion {
    /// Validate original identity, stable completion identity and response bounds.
    /// This is not proof of canonical commit or authority to complete an invocation.
    ///
    /// # Errors
    /// Rejects malformed identities and oversized responses.
    pub fn validate(&self) -> Result<(), InteractionControllerError> {
        self.binding.validate()?;
        if !durable_identifier(&self.completion_id) {
            return Err(InteractionControllerError::InvalidContract);
        }
        validate_durable_size(self)
    }
}

impl DurableInteractionOperation {
    /// Validate retry identity; deduplication requires the host's durable receipt.
    ///
    /// # Errors
    /// Rejects empty, oversized or control-character identities.
    pub fn validate(&self) -> Result<(), InteractionControllerError> {
        if !durable_identifier(&self.operation_id) {
            return Err(InteractionControllerError::InvalidContract);
        }
        Ok(())
    }
}

impl DurableInteractionInput {
    /// Validate protocol, binding, identity, and message size without granting authority.
    ///
    /// # Errors
    /// Rejects unknown versions, malformed fields and oversized messages.
    pub fn validate(&self) -> Result<(), InteractionControllerError> {
        if self.version != DURABLE_INTERACTION_VERSION {
            return Err(InteractionControllerError::InvalidContract);
        }
        self.binding.validate()?;
        self.operation.validate()?;
        validate_durable_size(self)
    }
}

impl DurableInteractionRequest {
    /// Validate the generic envelope; producers additionally enforce exact capability
    /// support and checkpoint semantics. Unknown versions are never downgraded.
    ///
    /// # Errors
    /// Rejects version, binding, completion identity, or size mismatches.
    pub fn validate(&self) -> Result<(), InteractionControllerError> {
        let version = match self {
            Self::Discover { version } => *version,
            Self::Export {
                version, binding, ..
            } => {
                binding.validate()?;
                *version
            }
            Self::Restore {
                version,
                binding,
                checkpoint,
            }
            | Self::Complete {
                version,
                binding,
                checkpoint,
                ..
            } => {
                binding.validate()?;
                if binding != &checkpoint.binding {
                    return Err(InteractionControllerError::InvalidContract);
                }
                *version
            }
        };
        if version != DURABLE_INTERACTION_VERSION {
            return Err(InteractionControllerError::InvalidContract);
        }
        if let Self::Complete { completion_id, .. } = self
            && !durable_identifier(completion_id)
        {
            return Err(InteractionControllerError::InvalidContract);
        }
        validate_durable_size(self)
    }
}

/// Input admission uses optimistic revision checks; it never silently reapplies input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionControllerInput {
    /// Required contract version.
    pub version: u32,
    /// Canonical exchange identity.
    pub exchange_id: String,
    /// Expected host revision.
    pub expected_revision: u64,
    /// Semantic input, independent of renderer gestures.
    pub input: InteractionInput,
}

#[cfg(test)]
mod durable_contract_tests {
    use super::*;

    fn binding() -> DurableInteractionBinding {
        DurableInteractionBinding {
            session_id: "session".into(),
            turn_id: "turn".into(),
            invocation_id: "call".into(),
            request_sequence: 1,
            exchange_id: "exchange".into(),
            tool_name: "tool".into(),
            request_digest: "a".repeat(64),
            exchange_schema_version: 1,
            capability: DurableInteractionCapability {
                route: InteractionControllerRoute {
                    producer_id: "producer".into(),
                    exchange_schema: "request".into(),
                    min_schema_version: 1,
                    max_schema_version: 1,
                    interaction_kind: "kind".into(),
                },
                checkpoint_schema: "checkpoint".into(),
                checkpoint_version: 1,
                continuation_schema: "complete".into(),
                continuation_version: 1,
            },
        }
    }

    #[test]
    fn durable_discovery_rejects_invalid_queries_pages_and_unknown_fields() {
        let query = DurableInteractionQuery {
            limit: 1,
            after_event_sequence: Some(2),
            expected_generation: Some(7),
        };
        let binding = binding();
        let view = DurableInteractionView {
            version: DURABLE_INTERACTION_VERSION,
            binding: binding.clone(),
            revision: 5,
            snapshot: InteractionControllerSnapshot {
                version: INTERACTION_CONTROLLER_VERSION,
                exchange_id: binding.exchange_id,
                route: binding.capability.route,
                schema_version: 1,
                revision: 5,
                snapshot: serde_json::json!({}),
                output: InteractionOutput::None,
            },
        };
        let page = DurableInteractionPage {
            generation: 7,
            views: vec![view],
            next_after_event_sequence: Some(3),
        };
        assert_eq!(page.validate("session", &query), Ok(()));
        let encoded = serde_json::to_value(&page).unwrap();
        assert_eq!(
            serde_json::from_value::<DurableInteractionPage>(encoded.clone()).unwrap(),
            page
        );
        let mut unknown = encoded;
        unknown["checkpoint"] = serde_json::json!({});
        assert!(serde_json::from_value::<DurableInteractionPage>(unknown).is_err());
        let mut unknown = serde_json::to_value(&query).unwrap();
        unknown["checkpoint"] = serde_json::json!({});
        assert!(serde_json::from_value::<DurableInteractionQuery>(unknown).is_err());
        for limit in [0, 101, u32::MAX] {
            assert_eq!(
                DurableInteractionQuery {
                    limit,
                    ..query.clone()
                }
                .validate(),
                Err(InteractionControllerError::LimitExceeded)
            );
        }
        assert_eq!(
            page.validate("other", &query),
            Err(InteractionControllerError::Unauthorized)
        );
        let mut wrong = page.clone();
        wrong.views[0].binding.request_digest.clear();
        assert_eq!(
            wrong.validate("session", &query),
            Err(InteractionControllerError::InvalidContract)
        );
        let mut wrong = page.clone();
        wrong.views[0].snapshot.revision += 1;
        assert_eq!(
            wrong.validate("session", &query),
            Err(InteractionControllerError::InvalidContract)
        );
        let mut wrong = page.clone();
        wrong.views.push(wrong.views[0].clone());
        assert_eq!(
            wrong.validate("session", &query),
            Err(InteractionControllerError::LimitExceeded)
        );
        let mut wrong = page.clone();
        wrong.generation += 1;
        assert_eq!(
            wrong.validate("session", &query),
            Err(InteractionControllerError::Conflict)
        );
        let mut wrong = page.clone();
        wrong.next_after_event_sequence = Some(2);
        assert_eq!(
            wrong.validate("session", &query),
            Err(InteractionControllerError::InvalidContract)
        );
        let mut wrong = page;
        wrong.views[0].snapshot.snapshot = serde_json::json!("x".repeat(MAX_CONTROLLER_BYTES));
        assert_eq!(
            wrong.validate("session", &query),
            Err(InteractionControllerError::LimitExceeded)
        );
    }

    #[test]
    fn durable_public_view_and_terminal_receipt_validate_without_checkpoint_state() {
        let input = DurableInteractionInput {
            version: DURABLE_INTERACTION_VERSION,
            binding: binding(),
            operation: DurableInteractionOperation {
                operation_id: "operation".into(),
                expected_revision: 4,
            },
            input: InteractionInput::Submit,
        };
        let view = DurableInteractionView {
            version: DURABLE_INTERACTION_VERSION,
            binding: input.binding.clone(),
            revision: 5,
            snapshot: InteractionControllerSnapshot {
                version: INTERACTION_CONTROLLER_VERSION,
                exchange_id: input.binding.exchange_id.clone(),
                route: input.binding.capability.route.clone(),
                schema_version: 1,
                revision: 5,
                snapshot: serde_json::json!({"answered":true}),
                output: InteractionOutput::Submitted {
                    payload: serde_json::json!({"status":"answered"}),
                },
            },
        };
        let receipt = DurableInteractionReceipt {
            version: DURABLE_INTERACTION_VERSION,
            input,
            view,
        };
        assert_eq!(receipt.validate(), Ok(()));
        let encoded = serde_json::to_value(&receipt).unwrap();
        assert_eq!(
            serde_json::from_value::<DurableInteractionReceipt>(encoded.clone()).unwrap(),
            receipt
        );
        assert!(encoded["view"].get("checkpoint").is_none());
        let mut future = receipt.clone();
        future.version += 1;
        assert!(future.validate().is_err());
        let mut wrong = receipt.clone();
        wrong.view.binding.exchange_id = "other".into();
        assert!(wrong.validate().is_err());
        let mut wrong = receipt.clone();
        wrong.view.snapshot.revision += 1;
        assert!(wrong.validate().is_err());
        let mut wrong = receipt.clone();
        wrong.input.operation.operation_id.clear();
        assert!(wrong.validate().is_err());
        let mut wrong = receipt.clone();
        wrong.input.operation.expected_revision += 1;
        assert!(wrong.validate().is_err());
        let mut oversized = receipt;
        oversized.view.snapshot.snapshot = serde_json::json!("x".repeat(MAX_CONTROLLER_BYTES));
        assert_eq!(
            oversized.validate(),
            Err(InteractionControllerError::LimitExceeded)
        );
        for path in ["receipt", "view", "operation"] {
            let mut extra = encoded.clone();
            let object = match path {
                "view" => &mut extra["view"],
                "operation" => &mut extra["input"]["operation"],
                _ => &mut extra,
            };
            object["unexpected"] = true.into();
            assert!(serde_json::from_value::<DurableInteractionReceipt>(extra).is_err());
        }
    }

    #[test]
    fn durable_envelope_roundtrips_without_changing_ephemeral_contract() {
        let binding = binding();
        let checkpoint = ProducerInteractionCheckpoint {
            binding: binding.clone(),
            revision: 7,
            payload: serde_json::json!({"semantic": true}),
        };
        let requests = [
            DurableInteractionRequest::Discover { version: 1 },
            DurableInteractionRequest::Export {
                version: 1,
                binding: binding.clone(),
                revision: 7,
                transition: InteractionControllerTransition {
                    version: 1,
                    interaction_kind: "kind".into(),
                    state: Value::Null,
                    snapshot: Value::Null,
                    output: InteractionOutput::None,
                },
            },
            DurableInteractionRequest::Restore {
                version: 1,
                binding: binding.clone(),
                checkpoint: checkpoint.clone(),
            },
            DurableInteractionRequest::Complete {
                version: 1,
                binding,
                checkpoint,
                completion_id: "completion".into(),
            },
        ];
        for request in requests {
            request.validate().unwrap();
            let json = serde_json::to_value(&request).unwrap();
            assert_eq!(
                serde_json::from_value::<DurableInteractionRequest>(json.clone()).unwrap(),
                request
            );
            let mut future = json.clone();
            future["version"] = 2.into();
            assert_eq!(
                serde_json::from_value::<DurableInteractionRequest>(future)
                    .unwrap()
                    .validate(),
                Err(InteractionControllerError::InvalidContract)
            );
            let mut malformed = json;
            malformed["unknown"] = true.into();
            assert!(serde_json::from_value::<DurableInteractionRequest>(malformed).is_err());
        }
    }

    #[test]
    fn durable_bindings_operations_and_size_fail_closed() {
        let mut binding = binding();
        binding.request_digest = "A".repeat(64);
        assert_eq!(
            binding.validate(),
            Err(InteractionControllerError::InvalidContract)
        );
        assert!(
            DurableInteractionOperation {
                operation_id: "\n".into(),
                expected_revision: 0
            }
            .validate()
            .is_err()
        );
        let binding = self::binding();
        let mut checkpoint = ProducerInteractionCheckpoint {
            binding: binding.clone(),
            revision: 0,
            payload: Value::Null,
        };
        checkpoint.binding.invocation_id = "other".into();
        assert!(
            DurableInteractionRequest::Restore {
                version: 1,
                binding,
                checkpoint
            }
            .validate()
            .is_err()
        );
        assert_eq!(
            validate_durable_size(&"x".repeat(MAX_CONTROLLER_BYTES)),
            Err(InteractionControllerError::LimitExceeded)
        );
    }
}
