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

/// Durable context occupancy observation tied to a model request boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsageSnapshot {
    /// Provider plugin used for the observed request.
    pub provider_plugin_id: String,
    /// Model used for the observed request.
    pub model_id: String,
    /// Full active input context occupancy.
    pub input_tokens: u64,
    /// Last canonical event represented by the observed request.
    pub context_through_sequence: u64,
    /// Model turn/request identifier associated with this observation.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Effective non-secret auth profile used for this request.
    #[serde(default)]
    pub auth_profile: Option<String>,
    /// Conservative local estimate captured for the same request.
    #[serde(default)]
    pub estimated_input_tokens: Option<u64>,
    /// Observation source.
    pub source: ContextUsageSource,
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
