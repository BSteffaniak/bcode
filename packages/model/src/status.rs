//! Normalized model status exposed by application operations.

use serde::{Deserialize, Serialize};

/// Active model metadata and context policy for a session or application defaults.
///
/// Compatibility is governed by the enclosing application protocol. Optional fields retain
/// their existing missing-field defaults; moving this contract does not change its wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionModelStatus {
    /// Selected provider plugin.
    #[serde(default)]
    pub provider_plugin_id: Option<String>,
    /// User-facing requested model id before alias/default resolution.
    #[serde(default)]
    pub requested_model_id: Option<String>,
    /// Concrete effective model id used for metadata and provider requests.
    #[serde(default)]
    pub effective_model_id: Option<String>,
    /// Legacy display model field retained for wire compatibility.
    #[serde(default)]
    pub model_id: Option<String>,
    /// User-friendly display name from model catalog.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Model context capacity in tokens.
    #[serde(default)]
    pub context_window: Option<u32>,
    /// Authoritative active context occupancy.
    #[serde(default)]
    pub context_occupancy: Option<Box<bcode_session_models::RequestContextOccupancy>>,
    /// Projection error preventing a trustworthy occupancy value.
    #[serde(default)]
    pub request_context_error: Option<String>,
    /// Selected authentication profile identifier, never credentials.
    #[serde(default)]
    pub auth_profile: Option<String>,
    /// Context representation version when known.
    #[serde(default)]
    pub context_format_version: Option<u16>,
    /// Context compatibility identity when known.
    #[serde(default)]
    pub compatibility_key: Option<String>,
    /// Maximum output allowance in tokens.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Catalog-declared reasoning capabilities.
    #[serde(default)]
    pub reasoning: Option<crate::ModelReasoningInfo>,
    /// Selected reasoning effort.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Selected reasoning summary mode.
    #[serde(default)]
    pub reasoning_summary: Option<String>,
    /// Effective prompt-cache mode.
    #[serde(default)]
    pub prompt_cache_mode: Option<String>,
    /// Effective conversation-reuse mode.
    #[serde(default)]
    pub conversation_reuse_mode: Option<String>,
    /// Effective compaction mode.
    #[serde(default)]
    pub compaction_mode: Option<String>,
    /// Effective compaction backend.
    #[serde(default)]
    pub compaction_backend: Option<String>,
    /// Configured proactive threshold as a percentage of context capacity.
    #[serde(default)]
    pub proactive_compaction_threshold_percent: Option<u8>,
    /// Configured absolute proactive threshold when model-specific token policy is active.
    #[serde(default)]
    pub proactive_compaction_threshold_tokens: Option<u64>,
    /// Effective proactive threshold after safe-capacity capping.
    #[serde(default)]
    pub proactive_compaction_effective_threshold_tokens: Option<u64>,
    /// Threshold source (`percent` or `tokens`).
    #[serde(default)]
    pub proactive_compaction_threshold_source: Option<String>,
    /// Catalog-declared cache capabilities.
    #[serde(default)]
    pub cache: Option<crate::ModelCacheInfo>,
    /// Provenance of model metadata.
    #[serde(default)]
    pub metadata_source: Option<crate::ModelMetadataSource>,
    /// Catalog-declared pricing.
    #[serde(default)]
    pub pricing: Option<crate::ModelPricingInfo>,
}
