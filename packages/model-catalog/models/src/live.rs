//! Generated live model catalog snapshot types.

use crate::CatalogCapabilities;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Live snapshot schema version emitted by this crate.
pub const LIVE_SNAPSHOT_SCHEMA_VERSION: &str = "1.0.0";

/// Generated, non-committed live provider model snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveCatalogSnapshot {
    /// Semantic schema version for this live snapshot.
    pub schema_version: String,
    /// Provider id this snapshot describes.
    pub provider_id: String,
    /// Generation timestamp, encoded as RFC 3339 text.
    pub generated_at: String,
    /// Optional expiry timestamp, encoded as RFC 3339 text.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Live models keyed by provider-native model id.
    #[serde(default)]
    pub models: BTreeMap<String, LiveModel>,
}

impl LiveCatalogSnapshot {
    /// Construct an empty live snapshot.
    #[must_use]
    pub fn empty(provider_id: impl Into<String>, generated_at: impl Into<String>) -> Self {
        Self {
            schema_version: LIVE_SNAPSHOT_SCHEMA_VERSION.to_string(),
            provider_id: provider_id.into(),
            generated_at: generated_at.into(),
            expires_at: None,
            models: BTreeMap::new(),
        }
    }
}

/// One live provider model record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveModel {
    /// Provider-native model id.
    pub model_id: String,
    /// Provider display name, if returned by the provider API.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Provider lifecycle/status text.
    #[serde(default)]
    pub status: Option<String>,
    /// Regions where this model was observed.
    #[serde(default)]
    pub regions: BTreeSet<String>,
    /// Capability metadata returned or inferred from explicit provider API fields.
    #[serde(default)]
    pub capabilities: CatalogCapabilities,
    /// Model context window in tokens, when returned by the provider API.
    #[serde(default)]
    pub context_window: Option<u32>,
    /// Maximum output tokens, when returned by the provider API.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Compact raw provider metadata for debugging/auditing.
    #[serde(default)]
    pub raw: Option<serde_json::Value>,
}

/// Live metadata overlaid onto a curated catalog model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveModelMetadata {
    /// Provider lifecycle/status text.
    #[serde(default)]
    pub status: Option<String>,
    /// Regions where this model was observed.
    #[serde(default)]
    pub regions: BTreeSet<String>,
    /// Last time this model was seen in a provider snapshot.
    #[serde(default)]
    pub last_seen_at: Option<String>,
    /// Live snapshot/provider source label.
    #[serde(default)]
    pub source: Option<String>,
}
