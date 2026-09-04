//! Derive expected prompt-cache behavior from normalized capability claims.

use crate::DEFAULT_MIN_PREFIX_TOKENS;
use bcode_model::{
    CapabilitySupport, ModelCacheCapability, ModelCacheInfo, ModelCapability, ModelFeatureSupport,
    ModelInfo, PromptCacheFeature, ProviderCapabilities, ProviderCapability,
};
use bcode_prompt_cache_models::{PromptCacheExpectations, PromptCacheMechanism};
use std::collections::BTreeSet;

/// Overrides for values a claim source cannot express.
///
/// Every field is optional; unset fields fall back to claim-derived values or documented defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptCacheExpectationOverrides {
    /// Replace the minimum cacheable prefix when the claim source omits it.
    pub min_prefix_tokens: Option<u64>,
    /// Declare the explicit breakpoint budget when known from provider documentation.
    pub max_cache_points: Option<usize>,
    /// Declare whether the provider reports cache writes when known.
    pub reports_cache_writes: Option<bool>,
}

/// Derive expectations from provider and model claims.
///
/// Returns `None` when caching is not affirmatively advertised by both scopes; callers should
/// treat that as "skip" rather than "unsupported" because an unknown claim is not a rejection.
#[must_use]
pub fn expectations_from_claims(
    provider: &ProviderCapabilities,
    model: &ModelInfo,
    overrides: &PromptCacheExpectationOverrides,
) -> Option<PromptCacheExpectations> {
    if !provider
        .capabilities
        .contains(&ProviderCapability::PromptCaching)
        || !model.capabilities.contains(&ModelCapability::PromptCaching)
    {
        return None;
    }
    Some(expectations_from_cache_info(
        &model.cache,
        &provider.feature_support,
        &model.feature_support,
        overrides,
    ))
}

/// Derive expectations from a resolved [`ModelCacheInfo`] and feature claims.
///
/// Callers that already know caching is advertised (for example the host request planner) use
/// this directly.
#[must_use]
pub fn expectations_from_cache_info(
    cache: &ModelCacheInfo,
    provider_features: &ModelFeatureSupport,
    model_features: &ModelFeatureSupport,
    overrides: &PromptCacheExpectationOverrides,
) -> PromptCacheExpectations {
    let explicit = cache
        .capabilities
        .contains(&ModelCacheCapability::ExplicitCachePoints)
        || [
            PromptCacheFeature::ExplicitMessage,
            PromptCacheFeature::ExplicitSystem,
            PromptCacheFeature::ExplicitTools,
        ]
        .into_iter()
        .any(|feature| {
            claim_is_supported(provider_features.prompt_cache(feature))
                && claim_is_supported(model_features.prompt_cache(feature))
        });
    let mechanism = if explicit {
        PromptCacheMechanism::ExplicitPoints
    } else {
        PromptCacheMechanism::AutomaticPrefix
    };
    let ttl_seconds =
        if explicit || claim_is_supported(model_features.prompt_cache(PromptCacheFeature::Ttl)) {
            cache.ttl_seconds.clone()
        } else {
            BTreeSet::new()
        };
    let declared_min_prefix = cache.min_prefix_tokens.or(overrides.min_prefix_tokens);
    PromptCacheExpectations {
        mechanism,
        reports_cache_writes: overrides.reports_cache_writes,
        supports_cache_key: cache
            .capabilities
            .contains(&ModelCacheCapability::PromptCacheKey),
        ttl_seconds,
        min_prefix_tokens: declared_min_prefix.unwrap_or(DEFAULT_MIN_PREFIX_TOKENS),
        min_prefix_declared: declared_min_prefix.is_some(),
        max_cache_points: overrides.max_cache_points,
        thresholds: bcode_prompt_cache_models::PromptCacheThresholds::default(),
    }
}

const fn claim_is_supported(claim: &CapabilitySupport) -> bool {
    matches!(claim, CapabilitySupport::Supported { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_model::{CapabilitySource, ModelVisibility};
    use std::collections::BTreeMap;

    fn model(cache: ModelCacheInfo, caching: bool) -> ModelInfo {
        let mut capabilities = BTreeSet::from([ModelCapability::StreamingText]);
        if caching {
            capabilities.insert(ModelCapability::PromptCaching);
        }
        ModelInfo {
            model_id: "m".into(),
            display_name: "M".into(),
            is_default: true,
            context_window: None,
            max_output_tokens: None,
            max_image_input_base64_bytes: None,
            capabilities,
            feature_support: ModelFeatureSupport::default(),
            reasoning: None,
            cache,
            metadata_source: None,
            pricing: None,
            api_surface: None,
            visibility: ModelVisibility::Visible,
        }
    }

    fn provider(caching: bool) -> ProviderCapabilities {
        let mut capabilities = BTreeSet::from([ProviderCapability::Streaming]);
        if caching {
            capabilities.insert(ProviderCapability::PromptCaching);
        }
        ProviderCapabilities {
            provider_id: "p".into(),
            display_name: "P".into(),
            capabilities,
            feature_support: ModelFeatureSupport::default(),
            auth_schemes: BTreeSet::new(),
            retry_rules: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn unadvertised_caching_yields_no_expectations() {
        let cache = ModelCacheInfo::default();
        assert!(
            expectations_from_claims(
                &provider(false),
                &model(cache.clone(), true),
                &PromptCacheExpectationOverrides::default()
            )
            .is_none()
        );
        assert!(
            expectations_from_claims(
                &provider(true),
                &model(cache, false),
                &PromptCacheExpectationOverrides::default()
            )
            .is_none()
        );
    }

    #[test]
    fn explicit_claims_derive_explicit_mechanism_with_ttls() {
        let cache = ModelCacheInfo {
            capabilities: BTreeSet::from([
                ModelCacheCapability::ExplicitCachePoints,
                ModelCacheCapability::PromptCacheKey,
            ]),
            ttl_seconds: BTreeSet::from([300, 3_600]),
            min_prefix_tokens: Some(2_048),
        };
        let expectations = expectations_from_claims(
            &provider(true),
            &model(cache, true),
            &PromptCacheExpectationOverrides::default(),
        )
        .expect("caching advertised");

        assert_eq!(expectations.mechanism, PromptCacheMechanism::ExplicitPoints);
        assert!(expectations.supports_cache_key);
        assert_eq!(expectations.ttl_seconds, BTreeSet::from([300, 3_600]));
        assert_eq!(expectations.min_prefix_tokens, 2_048);
        assert!(expectations.min_prefix_declared);
        assert_eq!(expectations.max_cache_points, None);
    }

    #[test]
    fn automatic_claims_use_default_min_prefix_and_drop_ttls() {
        let cache = ModelCacheInfo {
            capabilities: BTreeSet::from([ModelCacheCapability::AutomaticPrefixCache]),
            ttl_seconds: BTreeSet::from([86_400]),
            min_prefix_tokens: None,
        };
        let expectations = expectations_from_claims(
            &provider(true),
            &model(cache, true),
            &PromptCacheExpectationOverrides::default(),
        )
        .expect("caching advertised");

        assert_eq!(
            expectations.mechanism,
            PromptCacheMechanism::AutomaticPrefix
        );
        assert!(expectations.ttl_seconds.is_empty());
        assert_eq!(expectations.min_prefix_tokens, DEFAULT_MIN_PREFIX_TOKENS);
        assert!(!expectations.min_prefix_declared);
    }

    #[test]
    fn feature_claims_alone_can_select_explicit_mechanism() {
        let supported = CapabilitySupport::supported(CapabilitySource::BundledCatalog);
        let mut features = ModelFeatureSupport::default();
        features
            .prompt_cache
            .insert(PromptCacheFeature::ExplicitMessage, supported.clone());
        features
            .prompt_cache
            .insert(PromptCacheFeature::Ttl, supported);
        let cache = ModelCacheInfo {
            ttl_seconds: BTreeSet::from([300]),
            ..ModelCacheInfo::default()
        };
        let expectations = expectations_from_cache_info(
            &cache,
            &features,
            &features,
            &PromptCacheExpectationOverrides {
                max_cache_points: Some(4),
                reports_cache_writes: Some(true),
                min_prefix_tokens: Some(512),
            },
        );

        assert_eq!(expectations.mechanism, PromptCacheMechanism::ExplicitPoints);
        assert_eq!(expectations.ttl_seconds, BTreeSet::from([300]));
        assert_eq!(expectations.max_cache_points, Some(4));
        assert_eq!(expectations.reports_cache_writes, Some(true));
        assert_eq!(expectations.min_prefix_tokens, 512);
        assert!(expectations.min_prefix_declared);
    }
}
