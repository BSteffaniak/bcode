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
    /// Exact request identity for this observation.
    pub invocation: ModelInvocationIdentity,
    /// Last canonical event represented by the observed request.
    pub context_through_sequence: u64,
    /// Full active input-context occupancy.
    #[serde(alias = "input_tokens")]
    pub context_input_tokens: u64,
    /// Local estimate of the complete model-visible request before calibration.
    #[serde(alias = "estimated_input_tokens")]
    pub local_request_estimate_tokens: u64,
    /// Observation source.
    pub source: ContextUsageSource,
}

impl ContextUsageSnapshot {
    /// Return whether this observation belongs to the supplied context generation.
    #[must_use]
    pub const fn belongs_to_context_epoch(&self, context_epoch: u64) -> bool {
        self.invocation.context_epoch == context_epoch
    }

    /// Return whether this observation came from the same exact request as `other`.
    #[must_use]
    pub fn matches_request_attempt(&self, other: &Self) -> bool {
        self.invocation.request_id == other.invocation.request_id
            && self.invocation.request_fingerprint == other.invocation.request_fingerprint
    }

    /// Return whether this observation can calibrate an estimate for `invocation`.
    #[must_use]
    pub fn is_compatible_anchor(&self, invocation: &ModelInvocationIdentity) -> bool {
        let current = &self.invocation;
        current.context_epoch == invocation.context_epoch
            && current.provider_plugin_id == invocation.provider_plugin_id
            && current.effective_model_id == invocation.effective_model_id
            && current.effective_auth_profile == invocation.effective_auth_profile
            && current.context_format_version == invocation.context_format_version
            && current.compatibility_key == invocation.compatibility_key
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
    /// Build a calibrated estimate from the current compatible occupancy when possible.
    #[must_use]
    pub fn project_estimate(
        current: Option<&Self>,
        invocation: ModelInvocationIdentity,
        context_through_sequence: u64,
        local_request_estimate_tokens: u64,
    ) -> ContextUsageSnapshot {
        let context_input_tokens = current
            .filter(|occupancy| {
                occupancy.context_epoch == invocation.context_epoch
                    && occupancy.snapshot.is_compatible_anchor(&invocation)
            })
            .map_or(local_request_estimate_tokens, |occupancy| {
                let anchor = &occupancy.snapshot;
                if local_request_estimate_tokens >= anchor.local_request_estimate_tokens {
                    anchor.context_input_tokens.saturating_add(
                        local_request_estimate_tokens - anchor.local_request_estimate_tokens,
                    )
                } else {
                    anchor.context_input_tokens.saturating_sub(
                        anchor.local_request_estimate_tokens - local_request_estimate_tokens,
                    )
                }
            });
        ContextUsageSnapshot {
            invocation,
            context_through_sequence,
            context_input_tokens,
            local_request_estimate_tokens,
            source: ContextUsageSource::Estimated,
        }
    }

    /// Reconcile one observation into the authoritative occupancy for `context_epoch`.
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
