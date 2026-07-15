//! Durable context-usage and provider-compaction snapshot models.
//!
//! A snapshot event's own sequence orders competing durable markers, while the event's
//! `compacted_through_sequence` identifies the canonical history prefix replaced by the snapshot.
//! Legacy snapshots may omit newer identity fields through serde defaults and therefore must fall
//! back to portable context rather than replaying opaque messages. Opaque provider messages are
//! replayable only when provider, model, auth profile, format version, and compatibility key all
//! match; [`ProviderContextSnapshot::portable_summary`] is required for every incompatible surface.

use serde::{Deserialize, Serialize};

/// Source and confidence of a context occupancy observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextUsageSource {
    /// Exact usage reported by the active provider surface.
    Provider,
    /// Conservative local projection of the model-visible request.
    Estimated,
}

/// Exact identity of one assembled provider request.
///
/// Requested and effective model ids are deliberately distinct: the requested id is the
/// user-facing selection (and may be an alias), while the effective id is sent to the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInvocationIdentity {
    /// Provider plugin used for the request.
    pub provider_plugin_id: String,
    /// User-facing model selection, before alias/default resolution.
    #[serde(default)]
    pub requested_model_id: Option<String>,
    /// Concrete model id sent to the provider.
    pub effective_model_id: String,
    /// Unique host request-attempt identifier.
    pub request_id: String,
    /// Owning model-turn identifier.
    pub model_turn_id: String,
    /// Zero-based provider round within the model turn.
    pub round: u32,
    /// Stable fingerprint of the exact assembled request.
    pub request_fingerprint: String,
    /// Provider turn identifier.
    pub provider_turn_id: String,
    /// Effective non-secret auth profile chosen by routing.
    #[serde(default)]
    pub effective_auth_profile: Option<String>,
    /// Provider context format version used for this request.
    #[serde(default)]
    pub context_format_version: Option<u16>,
    /// Provider context compatibility identity used for this request.
    #[serde(default)]
    pub compatibility_key: Option<String>,
    /// Context generation captured before request assembly.
    #[serde(default)]
    pub context_epoch: u64,
}

/// Durable context occupancy observation tied to a model request boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsageSnapshot {
    /// Authoritative invocation identity for current events.
    #[serde(default)]
    pub invocation: Option<ModelInvocationIdentity>,
    /// Provider plugin used for the observed request (legacy compatibility mirror).
    pub provider_plugin_id: String,
    /// Effective model used for the observed request (legacy compatibility mirror).
    pub model_id: String,
    /// Full active input context occupancy.
    pub input_tokens: u64,
    /// Last canonical event represented by the observed request.
    pub context_through_sequence: u64,
    /// Unique host request-attempt identifier (legacy compatibility mirror).
    #[serde(default)]
    pub request_id: Option<String>,
    /// Owning model-turn identifier (legacy compatibility mirror).
    #[serde(default)]
    pub model_turn_id: Option<String>,
    /// Zero-based provider round within the model turn (legacy compatibility mirror).
    #[serde(default)]
    pub round: Option<u32>,
    /// Stable fingerprint of the exact assembled request (legacy compatibility mirror).
    #[serde(default)]
    pub request_fingerprint: Option<String>,
    /// Model turn/request identifier associated with this observation (legacy mirror).
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Effective non-secret auth profile used for this request (legacy mirror).
    #[serde(default)]
    pub auth_profile: Option<String>,
    /// Conservative local estimate captured for the same request.
    #[serde(default)]
    pub estimated_input_tokens: Option<u64>,
    /// Provider context format version used for this request, when supported (legacy mirror).
    #[serde(default)]
    pub context_format_version: Option<u16>,
    /// Provider context compatibility identity used for this request (legacy mirror).
    #[serde(default)]
    pub compatibility_key: Option<String>,
    /// Observation source.
    pub source: ContextUsageSource,
}

impl ContextUsageSnapshot {
    /// Return the request identity when the event uses the current unambiguous schema.
    #[must_use]
    pub const fn invocation(&self) -> Option<&ModelInvocationIdentity> {
        self.invocation.as_ref()
    }

    /// Return whether this observation belongs to the supplied context generation.
    #[must_use]
    pub fn belongs_to_context_epoch(&self, context_epoch: u64) -> bool {
        self.invocation
            .as_ref()
            .is_none_or(|identity| identity.context_epoch == context_epoch)
    }

    /// Return whether this observation came from the same exact request as `other`.
    #[must_use]
    pub fn matches_request_attempt(&self, other: &Self) -> bool {
        match (&self.invocation, &other.invocation) {
            (Some(left), Some(right)) => {
                left.request_id == right.request_id
                    && left.request_fingerprint == right.request_fingerprint
            }
            _ => matches!(
                (
                    self.request_id.as_deref(),
                    self.request_fingerprint.as_deref(),
                    other.request_id.as_deref(),
                    other.request_fingerprint.as_deref(),
                ),
                (Some(left_id), Some(left_fingerprint), Some(right_id), Some(right_fingerprint))
                    if left_id == right_id && left_fingerprint == right_fingerprint
            ),
        }
    }
}

/// Authoritative current context occupancy projected from canonical session events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextOccupancy {
    /// Context generation to which this value belongs.
    pub context_epoch: u64,
    /// Event sequence of the accepted observation.
    pub observation_sequence: u64,
    /// Accepted observation.
    pub snapshot: ContextUsageSnapshot,
}

impl ContextOccupancy {
    /// Reconcile one observation into the authoritative occupancy for `context_epoch`.
    ///
    /// Estimates start new request rounds. Provider observations may only confirm the exact
    /// request represented by the current estimate, preventing late usage from an older request
    /// from replacing newer pressure.
    #[must_use]
    pub fn reconcile(
        current: Option<&Self>,
        context_epoch: u64,
        observation_sequence: u64,
        snapshot: ContextUsageSnapshot,
    ) -> Option<Self> {
        if !snapshot.belongs_to_context_epoch(context_epoch) {
            return current.cloned();
        }
        let accept = match snapshot.source {
            ContextUsageSource::Estimated => true,
            ContextUsageSource::Provider => current.is_some_and(|occupancy| {
                occupancy.context_epoch == context_epoch
                    && occupancy.snapshot.matches_request_attempt(&snapshot)
            }),
        };
        accept
            .then_some(Self {
                context_epoch,
                observation_sequence,
                snapshot,
            })
            .or_else(|| current.cloned())
    }
}

/// Origin of a provider-native replacement context.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderContextSnapshotOrigin {
    /// Bcode explicitly invoked the provider's native compaction operation.
    #[default]
    Explicit,
    /// The provider compacted context while serving a normal model turn.
    ProviderManaged,
}

/// Durable provider-native replacement context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderContextSnapshot {
    /// Snapshot payload format version.
    #[serde(default = "default_provider_context_snapshot_version")]
    pub format_version: u16,
    /// Stable fingerprint of the exact request that produced this snapshot, when provider-managed.
    #[serde(default)]
    pub request_fingerprint: Option<String>,
    /// Unique host request-attempt identifier, when provider-managed.
    #[serde(default)]
    pub request_id: Option<String>,
    /// Provider plugin that owns the opaque replacement items.
    pub provider_plugin_id: String,
    /// Model for which the replacement context was produced.
    pub model_id: String,
    /// Non-secret provider surface identity required to replay opaque context safely.
    #[serde(default)]
    pub compatibility_key: String,
    /// Effective non-secret auth profile used to create the context.
    #[serde(default)]
    pub auth_profile: Option<String>,
    /// How the provider-native replacement was created.
    #[serde(default)]
    pub origin: ProviderContextSnapshotOrigin,
    /// Serialized provider-neutral model messages containing provider extensions.
    ///
    /// Callers must preserve this value losslessly and must not replay it on an incompatible
    /// surface.
    pub messages_json: String,
    /// Portable summary used when the active provider, model, auth profile, or format no longer
    /// matches, including legacy snapshots with incomplete identity metadata.
    #[serde(default)]
    pub portable_summary: String,
}

const fn default_provider_context_snapshot_version() -> u16 {
    1
}
