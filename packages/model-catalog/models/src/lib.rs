#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Versioned schema types for Bcode model catalog documents.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub use live::{LiveCatalogSnapshot, LiveModel, LiveModelMetadata};

mod live;

/// Catalog schema version emitted by this crate.
pub const SCHEMA_VERSION: &str = "2.0.0";

/// Complete model catalog document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogDocument {
    /// Semantic schema version for this document.
    pub schema_version: String,
    /// Catalog revision, normally a git SHA or release identifier.
    pub catalog_revision: String,
    /// Generation timestamp, encoded as RFC 3339 text.
    pub generated_at: String,
    /// Providers keyed by stable provider id.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderCatalog>,
}

impl CatalogDocument {
    /// Construct an empty catalog document.
    #[must_use]
    pub fn empty(catalog_revision: impl Into<String>, generated_at: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            catalog_revision: catalog_revision.into(),
            generated_at: generated_at.into(),
            providers: BTreeMap::new(),
        }
    }
}

/// Catalog data for a single provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCatalog {
    /// Stable provider id, for example `openai` or `bedrock`.
    pub provider_id: String,
    /// Human-readable provider name.
    pub display_name: String,
    /// Provider integration/API kind.
    pub kind: CatalogProviderKind,
    /// Provider homepage or documentation URL.
    #[serde(default)]
    pub website_url: Option<String>,
    /// Default interactive model id.
    #[serde(default)]
    pub default_model_id: Option<String>,
    /// Default Codex/subscription model id.
    #[serde(default)]
    pub default_codex_model_id: Option<String>,
    /// Bundled fallback model ids for providers without a live model-list API.
    #[serde(default)]
    pub fallback_model_ids: Vec<String>,
    /// Provider defaults used for discovered models without exact catalog metadata.
    #[serde(default)]
    pub defaults: Option<ModelCatalogDefaults>,
    /// Descriptive provider error-handling metadata.
    #[serde(default)]
    pub error_handling: ProviderErrorHandlingMetadata,
    /// Models keyed by provider-native model id.
    #[serde(default)]
    pub models: BTreeMap<String, ModelCatalogEntry>,
}

/// Descriptive provider error-handling metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderErrorHandlingMetadata {
    /// Provider-declared recoverable error patterns.
    #[serde(default)]
    pub recoverable_error_patterns: Vec<RecoverableErrorPattern>,
}

/// Provider-declared recoverable error pattern.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverableErrorPattern {
    /// Stable pattern identifier.
    pub id: String,
    /// Whether this pattern is enabled by default.
    #[serde(default)]
    pub enabled_by_default: bool,
    /// Provider/model scope for this pattern.
    #[serde(default)]
    pub scope: RecoverableErrorPatternScope,
    /// Error match conditions.
    #[serde(default)]
    pub r#match: RecoverableErrorPatternMatch,
}

/// Provider/model scope for a recoverable error pattern.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverableErrorPatternScope {
    /// Exact provider plugin id scope.
    #[serde(default)]
    pub provider_plugin_id: Option<String>,
    /// Provider plugin id substring scope.
    #[serde(default)]
    pub provider_plugin_id_contains: Option<String>,
    /// Exact model id scope.
    #[serde(default)]
    pub model_id: Option<String>,
    /// Model id substring scope.
    #[serde(default)]
    pub model_id_contains: Option<String>,
}

/// Error match conditions for a recoverable error pattern.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverableErrorPatternMatch {
    /// Provider error category to match.
    #[serde(default)]
    pub category: Option<String>,
    /// Provider error code to match exactly.
    #[serde(default)]
    pub code: Option<String>,
    /// Provider error message to match exactly.
    #[serde(default)]
    pub message_equals: Option<String>,
    /// Provider error message substring to match.
    #[serde(default)]
    pub message_contains: Option<String>,
    /// Provider-native error message to match exactly.
    #[serde(default)]
    pub provider_message_equals: Option<String>,
    /// Provider-native error message substring to match.
    #[serde(default)]
    pub provider_message_contains: Option<String>,
}

/// Default metadata for provider-discovered models without exact catalog entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogDefaults {
    /// Default context window in tokens.
    #[serde(default)]
    pub context_window: Option<u32>,
    /// Default maximum output tokens.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Default capability metadata.
    #[serde(default)]
    pub capabilities: CatalogCapabilities,
    /// Default reasoning-specific metadata.
    #[serde(default)]
    pub reasoning: Option<CatalogReasoning>,
}

/// Provider integration/API kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogProviderKind {
    /// Provider uses an OpenAI-compatible API shape.
    OpenAiCompatible,
    /// Amazon Bedrock provider.
    Bedrock,
    /// Anthropic-native provider.
    Anthropic,
    /// Google-native provider.
    Google,
    /// OpenRouter-compatible aggregation provider.
    OpenRouter,
    /// Another provider kind.
    Other,
}

/// Catalog entry for one model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    /// Provider-native model id.
    pub model_id: String,
    /// Human-readable model name.
    pub display_name: String,
    /// Supported aliases; exact model ids or glob-like patterns ending in `*`.
    #[serde(default)]
    pub aliases: BTreeSet<String>,
    /// Provider/public availability status.
    pub status: CatalogModelStatus,
    /// Bcode support status for this model.
    #[serde(default)]
    pub bcode_support: BcodeSupportStatus,
    /// Model context window in tokens.
    #[serde(default)]
    pub context_window: Option<u32>,
    /// Maximum output tokens.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Provider-specific model family name.
    #[serde(default)]
    pub family: Option<String>,
    /// Provider-specific model kind/classification.
    #[serde(default)]
    pub provider_model_kind: Option<String>,
    /// Replacement model id, when this model is deprecated.
    #[serde(default)]
    pub replaced_by: Option<String>,
    /// Human notes for this model.
    #[serde(default)]
    pub notes: Option<String>,
    /// Public documentation URL for this model.
    #[serde(default)]
    pub documentation_url: Option<String>,
    /// Pricing metadata.
    #[serde(default)]
    pub pricing: Option<CatalogPricing>,
    /// Capability metadata.
    #[serde(default)]
    pub capabilities: CatalogCapabilities,
    /// Reasoning-specific metadata.
    #[serde(default)]
    pub reasoning: Option<CatalogReasoning>,
    /// Compatibility targets this model is objectively known to support.
    ///
    /// Deprecated for newly-authored catalog data. Use [`Self::deployments`] when operational
    /// metadata differs by serving target.
    #[serde(default)]
    pub supported_by: BTreeSet<ModelSupportTarget>,
    /// Serving deployments with target-specific operational metadata.
    #[serde(default)]
    pub deployments: Vec<ModelDeployment>,
    /// Live provider metadata overlaid from generated snapshots.
    #[serde(default)]
    pub live: Option<LiveModelMetadata>,
    /// Metadata source/verification data.
    #[serde(default)]
    pub source: CatalogSourceMetadata,
}

/// Operational metadata for one concrete model serving deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDeployment {
    /// Provider/authentication/API-surface identity for this deployment.
    #[serde(flatten)]
    pub target: ModelSupportTarget,
    /// Effective input context window exposed by this deployment.
    #[serde(default)]
    pub context_window: Option<u32>,
    /// Maximum output tokens exposed by this deployment.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Deployment-specific capability overrides. Empty means use model-level capabilities.
    #[serde(default)]
    pub capabilities: CatalogCapabilities,
    /// Deployment-specific reasoning metadata.
    #[serde(default)]
    pub reasoning: Option<CatalogReasoning>,
    /// Deployment-specific pricing metadata.
    #[serde(default)]
    pub pricing: Option<CatalogPricing>,
}

/// Objective compatibility target for a model.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ModelSupportTarget {
    /// Provider domain, for example `openai`, `openrouter`, or `bedrock`.
    pub provider: String,
    /// Authentication mode required for this target, for example `api_key` or
    /// `chatgpt_subscription`.
    pub auth_mode: String,
    /// Provider API surface, for example `responses_api`, `chat_completions`, or
    /// `chatgpt_codex`.
    pub api_surface: String,
    /// Optional integration/runtime qualifier, for example `bcode` or `provider_native`.
    #[serde(default)]
    pub integration: Option<String>,
}

impl ModelSupportTarget {
    /// Construct a support target.
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        auth_mode: impl Into<String>,
        api_surface: impl Into<String>,
        integration: Option<impl Into<String>>,
    ) -> Self {
        Self {
            provider: provider.into(),
            auth_mode: auth_mode.into(),
            api_surface: api_surface.into(),
            integration: integration.map(Into::into),
        }
    }

    /// Return true when this target is compatible with the requested active target.
    #[must_use]
    pub fn matches(&self, target: &Self) -> bool {
        self.provider == target.provider
            && self.auth_mode == target.auth_mode
            && self.api_surface == target.api_surface
            && (self.integration == target.integration
                || self.integration.is_none()
                || self.integration.as_deref() == Some("provider_native"))
    }
}

/// Provider/public status for a catalog model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogModelStatus {
    /// Generally available/stable.
    Stable,
    /// Preview/beta/experimental.
    Preview,
    /// Deprecated but still possibly callable.
    Deprecated,
    /// Unavailable or removed.
    Unavailable,
    /// Status is not known.
    Unknown,
}

/// Bcode support status for a catalog model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BcodeSupportStatus {
    /// Bcode support is known and expected to work.
    Supported,
    /// Some Bcode features may not work.
    PartiallySupported,
    /// Bcode does not currently support this model.
    Unsupported,
    /// Support status has not been verified.
    #[default]
    Unknown,
}

/// Token pricing metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogPricing {
    /// ISO 4217 currency code.
    pub currency: String,
    /// Unit the price applies to.
    pub unit: CatalogPricingUnit,
    /// Input token price in currency micros.
    #[serde(default)]
    pub input_micros: Option<u64>,
    /// Cached input token price in currency micros.
    #[serde(default)]
    pub cached_input_micros: Option<u64>,
    /// Cache write token price in currency micros.
    #[serde(default)]
    pub cache_write_input_micros: Option<u64>,
    /// Output token price in currency micros.
    #[serde(default)]
    pub output_micros: Option<u64>,
}

/// Pricing unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogPricingUnit {
    /// Price is per one million tokens.
    PerMillionTokens,
}

/// Model capability flags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct CatalogCapabilities {
    /// Accepts text input.
    #[serde(default)]
    pub text_input: bool,
    /// Accepts image input.
    #[serde(default)]
    pub image_input: bool,
    /// Produces text output.
    #[serde(default)]
    pub text_output: bool,
    /// Supports tool/function calling.
    #[serde(default)]
    pub tool_use: bool,
    /// Supports structured output controls.
    #[serde(default)]
    pub structured_outputs: bool,
    /// Supports reasoning controls or reasoning models.
    #[serde(default)]
    pub reasoning: bool,
    /// Supports prompt/cache discounts or cache controls.
    #[serde(default)]
    pub prompt_cache: bool,
    /// Supports native provider web search.
    #[serde(default)]
    pub native_web_search: bool,
}

/// Reasoning-specific metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogReasoning {
    /// Supported effort values.
    #[serde(default)]
    pub effort_values: BTreeSet<String>,
    /// Default effort value.
    #[serde(default)]
    pub default_effort: Option<String>,
    /// Supported summary values.
    #[serde(default)]
    pub summary_values: BTreeSet<String>,
    /// Default summary value.
    #[serde(default)]
    pub default_summary: Option<String>,
    /// Raw provider reasoning text can be requested.
    #[serde(default)]
    pub raw_reasoning_supported: bool,
}

/// Source and verification metadata for a catalog model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSourceMetadata {
    /// Pricing source identifier or URL.
    #[serde(default)]
    pub pricing: Option<String>,
    /// Metadata source identifier or URL.
    #[serde(default)]
    pub metadata: Option<String>,
    /// Verification timestamp, encoded as RFC 3339 text.
    #[serde(default)]
    pub last_verified_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_round_trip_preserves_target_specific_limits() {
        let deployment = ModelDeployment {
            target: ModelSupportTarget::new(
                "openai",
                "chatgpt_subscription",
                "chatgpt_codex",
                Some("bcode"),
            ),
            context_window: Some(372_000),
            max_output_tokens: Some(128_000),
            capabilities: CatalogCapabilities::default(),
            reasoning: None,
            pricing: None,
        };

        let value = serde_json::to_value(&deployment).expect("serialize deployment");
        let decoded: ModelDeployment =
            serde_json::from_value(value).expect("deserialize deployment");
        assert_eq!(decoded, deployment);
    }

    #[test]
    fn generic_integration_target_matches_specific_runtime_integration() {
        let generic = ModelSupportTarget::new("openai", "api_key", "responses_api", None::<String>);
        let runtime = ModelSupportTarget::new("openai", "api_key", "responses_api", Some("bcode"));

        assert!(generic.matches(&runtime));
    }

    #[test]
    fn different_api_surfaces_do_not_match() {
        let responses =
            ModelSupportTarget::new("openai", "api_key", "responses_api", Some("bcode"));
        let codex = ModelSupportTarget::new(
            "openai",
            "chatgpt_subscription",
            "chatgpt_codex",
            Some("bcode"),
        );

        assert!(!responses.matches(&codex));
    }
}
