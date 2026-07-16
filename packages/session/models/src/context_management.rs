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
pub enum RequestContextTokenCount {
    /// Conservative local projection of the model-visible request.
    Estimated(u64),
    /// Exact complete request-input usage reported by the provider.
    ProviderExact(u64),
}

impl RequestContextTokenCount {
    /// Return the represented request input token count.
    #[must_use]
    pub const fn tokens(self) -> u64 {
        match self {
            Self::Estimated(tokens) | Self::ProviderExact(tokens) => tokens,
        }
    }

    /// Return whether this count is a local estimate.
    #[must_use]
    pub const fn is_estimated(self) -> bool {
        matches!(self, Self::Estimated(_))
    }
}

/// Versioned local estimate of one complete model-visible request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalContextEstimate {
    /// Estimated request input tokens.
    pub tokens: u64,
    /// Accounting algorithm version used to produce `tokens`.
    pub algorithm_version: u16,
}

/// Exact identity of one assembled provider request.
///
/// Requested and effective model ids are deliberately distinct: the requested id is the
/// user-facing selection (and may be an alias), while the effective id is sent to the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequestIdentity {
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
pub struct RequestContextObservation {
    /// Exact request identity for this observation.
    pub request: ModelRequestIdentity,
    /// Last canonical event represented by the observed request.
    pub context_through_sequence: u64,
    /// Complete request-input occupancy, exact or estimated.
    pub context_tokens: RequestContextTokenCount,
    /// Local estimate of the complete model-visible request before calibration.
    pub local_estimate: LocalContextEstimate,
}

impl RequestContextObservation {
    /// Return whether this observation belongs to the supplied context generation.
    #[must_use]
    pub const fn belongs_to_context_epoch(&self, context_epoch: u64) -> bool {
        self.request.context_epoch == context_epoch
    }

    /// Return whether this observation came from the same exact request as `other`.
    #[must_use]
    pub fn matches_request_attempt(&self, other: &Self) -> bool {
        self.request.request_id == other.request.request_id
            && self.request.request_fingerprint == other.request.request_fingerprint
    }

    /// Return whether this observation can calibrate an estimate for `invocation`.
    #[must_use]
    pub fn is_compatible_anchor(&self, request: &ModelRequestIdentity) -> bool {
        let current = &self.request;
        current.context_epoch == request.context_epoch
            && current.provider_plugin_id == request.provider_plugin_id
            && current.effective_model_id == request.effective_model_id
            && current.effective_auth_profile == request.effective_auth_profile
            && current.context_format_version == request.context_format_version
            && current.compatibility_key == request.compatibility_key
    }
}

/// Authoritative current context occupancy projected from canonical session events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestContextOccupancy {
    /// Context generation to which this value belongs.
    pub context_epoch: u64,
    /// Event sequence of the accepted observation.
    pub observation_sequence: u64,
    /// Accepted observation.
    pub observation: RequestContextObservation,
}

impl RequestContextOccupancy {
    /// Build a calibrated estimate from the current compatible occupancy when possible.
    #[must_use]
    pub fn project_estimate(
        current: Option<&Self>,
        request: ModelRequestIdentity,
        context_through_sequence: u64,
        local_estimate: LocalContextEstimate,
    ) -> RequestContextObservation {
        let context_input_tokens = current
            .filter(|occupancy| {
                occupancy.context_epoch == request.context_epoch
                    && occupancy.observation.is_compatible_anchor(&request)
                    && occupancy.observation.local_estimate.algorithm_version
                        == local_estimate.algorithm_version
            })
            .map_or(local_estimate.tokens, |occupancy| {
                let anchor = &occupancy.observation;
                let anchor_tokens = anchor.context_tokens.tokens();
                if local_estimate.tokens >= anchor.local_estimate.tokens {
                    anchor_tokens
                        .saturating_add(local_estimate.tokens - anchor.local_estimate.tokens)
                } else {
                    anchor_tokens
                        .saturating_sub(anchor.local_estimate.tokens - local_estimate.tokens)
                }
            });
        RequestContextObservation {
            request,
            context_through_sequence,
            context_tokens: RequestContextTokenCount::Estimated(context_input_tokens),
            local_estimate,
        }
    }

    /// Reconcile one observation into the authoritative occupancy for `context_epoch`.
    #[must_use]
    pub fn reconcile(
        current: Option<&Self>,
        context_epoch: u64,
        observation_sequence: u64,
        observation: RequestContextObservation,
    ) -> Option<Self> {
        if !observation.belongs_to_context_epoch(context_epoch) {
            return current.cloned();
        }
        let accept = match observation.context_tokens {
            RequestContextTokenCount::Estimated(_) => true,
            RequestContextTokenCount::ProviderExact(_) => current.is_some_and(|occupancy| {
                occupancy.context_epoch == context_epoch
                    && occupancy.observation.matches_request_attempt(&observation)
            }),
        };
        accept
            .then_some(Self {
                context_epoch,
                observation_sequence,
                observation,
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
