//! Provider context-management contracts.
//!
//! Provider-managed compaction occurs during a normal model turn, while explicit native
//! compaction uses the dedicated compact operation. Both mechanisms may emit opaque replacement
//! context only when they advertise the same [`ProviderContextFormat`]. Opaque messages must be
//! replayed losslessly only on a compatible provider surface; callers must retain portable
//! fallback context for incompatible surfaces and legacy records.
//!
//! Capability discovery is best-effort and may fail when the provider is unavailable or rejects
//! the active surface. The explicit compact operation may fail for unsupported surfaces, invalid
//! or structurally incomplete messages, authentication failures, cancellation, transport errors,
//! provider errors, or responses whose opaque output does not match the advertised format.

use super::{ModelMessage, ProviderRequestContext, ToolDefinition};
use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};

/// Provider-owned identity for an opaque replacement-context format.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderContextFormat {
    /// Provider format version understood by the emitting plugin.
    #[serde(default = "default_provider_context_format_version")]
    pub version: u16,
    /// Stable, non-secret provider-surface compatibility key.
    pub compatibility_key: String,
}

const fn default_provider_context_format_version() -> u16 {
    1
}

/// Context-management capabilities for one active provider/model surface.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextManagementCapabilities {
    /// Provider autonomously manages context during normal model requests.
    #[serde(default)]
    pub provider_managed: bool,
    /// Provider supports the explicit compact-context operation for this surface.
    #[serde(default)]
    pub native_compaction: bool,
    /// Opaque context format produced by the supported compaction mechanisms.
    ///
    /// This must be present and valid before provider-managed opaque context can be requested or
    /// replayed.
    #[serde(default)]
    pub context_format: Option<ProviderContextFormat>,
}

/// Request for context-management capability discovery.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextManagementCapabilitiesRequest {
    /// Active provider configuration and request surface.
    #[serde(default)]
    pub provider_context: ProviderRequestContext,
    /// Selected model, or `None` when the provider should evaluate its configured default.
    #[serde(default)]
    pub model_id: Option<String>,
}

/// Request for provider-native compaction of model-visible context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactContextRequest {
    /// Session whose model-visible context is being compacted.
    pub session_id: SessionId,
    /// Active provider configuration and request surface.
    #[serde(default)]
    pub provider_context: ProviderRequestContext,
    /// Model that must own and understand the compacted context.
    pub model_id: String,
    /// Optional system prompt included in the context being compacted.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Structurally complete model messages selected for compaction.
    pub messages: Vec<ModelMessage>,
    /// Tool definitions needed to interpret tool-call content in `messages`.
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
}

/// Provider-native compacted replacement context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactContextResponse {
    /// Lossless opaque replacement messages.
    ///
    /// These messages must not be normalized or replayed on an incompatible provider surface.
    pub messages: Vec<ModelMessage>,
    /// Provider-owned format required to replay `messages`.
    pub context_format: ProviderContextFormat,
}

/// Provider context-management request for a normal model turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextManagementRequest {
    /// Token threshold at which a supporting provider should compact context.
    ///
    /// `None` disables provider-managed compaction instructions for this request.
    #[serde(default)]
    pub compact_threshold: Option<u64>,
}
