#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Portable prompt-cache semantics types.
//!
//! These types describe how a model's prompt cache is expected to behave, what one provider
//! round actually reported about cache usage, and the outcome of comparing the two. They carry
//! no policy: expectation derivation, planning, simulation, and analysis live in
//! `bcode_prompt_cache`.

use bcode_model::{ModelPricingBucket, ProviderRequestProjection, TokenUsage};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Current schema version for [`PromptCacheVerificationReport`].
pub const PROMPT_CACHE_VERIFICATION_REPORT_SCHEMA_VERSION: u32 = 1;

/// How a provider decides which request prefixes become cache entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheMechanism {
    /// The provider caches the longest previously seen prefix without caller hints.
    AutomaticPrefix,
    /// The caller marks cache breakpoints; only marked prefixes become entries.
    ExplicitPoints,
}

/// Pass/fail thresholds used when judging observed cache behavior.
///
/// Thresholds are ratios in `0.0..=1.0` unless documented otherwise. Defaults are conservative
/// values that a correctly functioning cache comfortably meets across providers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptCacheThresholds {
    /// Minimum fraction of eligible rounds that must report cache reads.
    pub min_hit_round_ratio: f64,
    /// Minimum `cached / input` ratio required on a warm same-prefix repeat.
    pub min_warm_read_ratio: f64,
    /// Maximum `uncached / input` ratio allowed in the late tail of a long loop.
    pub max_late_uncached_ratio: f64,
    /// Maximum `total cache writes / final round input` ratio; guards write churn.
    pub max_write_amplification: f64,
}

impl Default for PromptCacheThresholds {
    fn default() -> Self {
        Self {
            min_hit_round_ratio: 0.9,
            min_warm_read_ratio: 0.5,
            max_late_uncached_ratio: 0.15,
            max_write_amplification: 3.0,
        }
    }
}

/// Expected prompt-cache behavior for one provider/model pair.
///
/// Values derive from normalized capability claims, never from model identifiers. `None`
/// fields mean the claim source did not say; consumers apply documented conservative defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptCacheExpectations {
    /// Cache entry selection mechanism.
    pub mechanism: PromptCacheMechanism,
    /// Whether the provider reports cache-write tokens separately.
    ///
    /// `None` means the claim source does not say; the verifier records the observed behavior
    /// rather than asserting it.
    #[serde(default)]
    pub reports_cache_writes: Option<bool>,
    /// Whether the provider accepts a caller-supplied cache partition key.
    pub supports_cache_key: bool,
    /// Explicit TTLs the model advertises; empty for automatic-prefix caches.
    #[serde(default)]
    pub ttl_seconds: BTreeSet<u64>,
    /// Minimum stable prefix, in tokens, before the provider creates a cache entry.
    pub min_prefix_tokens: u64,
    /// Whether the minimum prefix came from a capability claim rather than a default.
    pub min_prefix_declared: bool,
    /// Maximum explicit breakpoints one request may carry, when the claim source says.
    #[serde(default)]
    pub max_cache_points: Option<usize>,
    /// Thresholds for judging observed rounds.
    #[serde(default)]
    pub thresholds: PromptCacheThresholds,
}

/// Cache-write tokens attributed to one confirmed TTL.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheWriteTtlTokens {
    /// Confirmed cache TTL for the written tokens, when the provider reported one.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    /// Tokens written with this TTL.
    pub tokens: u32,
}

/// Cache-relevant facts from one provider round, normalized from usage and request projection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRoundObservation {
    /// Zero-based order of this round within the observed sequence.
    pub round: usize,
    /// Complete model-visible input tokens for the round.
    #[serde(default)]
    pub input_tokens: Option<u32>,
    /// Input tokens served from the cache.
    #[serde(default)]
    pub cached_input_tokens: Option<u32>,
    /// Input tokens written to the cache.
    #[serde(default)]
    pub cache_write_input_tokens: Option<u32>,
    /// Input tokens neither read from nor written to the cache.
    #[serde(default)]
    pub uncached_input_tokens: Option<u32>,
    /// Whether cache subsets fit inside the complete input count.
    pub valid_input_breakdown: bool,
    /// Cache TTL confirmed by the provider for this round, when reported.
    #[serde(default)]
    pub cache_ttl_seconds: Option<u64>,
    /// Cache-write tokens grouped by confirmed TTL, from detailed usage buckets.
    #[serde(default)]
    pub cache_write_tokens_by_ttl: Vec<CacheWriteTtlTokens>,
    /// Cache-read tokens from detailed usage buckets.
    #[serde(default)]
    pub detailed_cache_read_tokens: Option<u32>,
    /// Explicit cache points the host placed in the request.
    #[serde(default)]
    pub requested_cache_points: Option<usize>,
    /// Cache points the provider actually serialized.
    #[serde(default)]
    pub emitted_cache_points: Option<usize>,
    /// Cache points the provider dropped to satisfy a budget or API shape.
    #[serde(default)]
    pub dropped_cache_points: Option<usize>,
    /// Whether the round completed a tool call (versus ending the turn).
    pub tool_round: bool,
}

impl CacheRoundObservation {
    /// Build an observation from a normalized provider usage snapshot and optional projection.
    #[must_use]
    pub fn from_provider_usage(
        round: usize,
        usage: &TokenUsage,
        projection: Option<&ProviderRequestProjection>,
    ) -> Self {
        let mut cache_write_tokens_by_ttl: BTreeMap<Option<u64>, u32> = BTreeMap::new();
        let mut detailed_cache_read_tokens = None;
        for detail in &usage.details {
            match detail.bucket {
                ModelPricingBucket::CacheWriteInput => {
                    let total = cache_write_tokens_by_ttl
                        .entry(detail.cache_ttl_seconds)
                        .or_insert(0_u32);
                    *total = total.saturating_add(detail.tokens);
                }
                ModelPricingBucket::CacheReadInput => {
                    detailed_cache_read_tokens = Some(
                        detailed_cache_read_tokens
                            .unwrap_or(0_u32)
                            .saturating_add(detail.tokens),
                    );
                }
                ModelPricingBucket::Input | ModelPricingBucket::Output => {}
            }
        }
        Self {
            round,
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_write_input_tokens: usage.cache_write_input_tokens,
            uncached_input_tokens: usage.uncached_input_tokens(),
            valid_input_breakdown: usage.has_valid_input_breakdown(),
            cache_ttl_seconds: usage.pricing_context.cache_ttl_seconds,
            cache_write_tokens_by_ttl: cache_write_tokens_by_ttl
                .into_iter()
                .map(|(ttl_seconds, tokens)| CacheWriteTtlTokens {
                    ttl_seconds,
                    tokens,
                })
                .collect(),
            detailed_cache_read_tokens,
            requested_cache_points: projection.and_then(|projection| projection.cache_point_count),
            emitted_cache_points: projection
                .and_then(|projection| projection.emitted_cache_point_count),
            dropped_cache_points: projection
                .and_then(|projection| projection.dropped_cache_point_count),
            tool_round: false,
        }
    }

    /// Mark whether this round ended in a tool call.
    #[must_use]
    pub const fn with_tool_round(mut self, tool_round: bool) -> Self {
        self.tool_round = tool_round;
        self
    }

    /// Build an observation from a persisted session usage record.
    ///
    /// Session usage is converted to the normalized [`TokenUsage`] shape first so the same
    /// accounting rules apply to live and historical observations. Persisted usage carries no
    /// request projection, so point accounting fields stay `None`.
    #[must_use]
    pub fn from_session_usage(
        round: usize,
        usage: &bcode_session_models::SessionTokenUsage,
    ) -> Self {
        let details = usage
            .pricing_usage_details
            .iter()
            .filter_map(|detail| {
                Some(bcode_model::ModelTokenUsageDetail {
                    bucket: match detail.bucket.as_str() {
                        "input" => ModelPricingBucket::Input,
                        "cache_read_input" => ModelPricingBucket::CacheReadInput,
                        "cache_write_input" => ModelPricingBucket::CacheWriteInput,
                        "output" => ModelPricingBucket::Output,
                        _ => return None,
                    },
                    modality: match detail.modality.as_str() {
                        "image" => bcode_model::ModelTokenModality::Image,
                        "audio" => bcode_model::ModelTokenModality::Audio,
                        "video" => bcode_model::ModelTokenModality::Video,
                        _ => bcode_model::ModelTokenModality::Text,
                    },
                    tokens: detail.tokens,
                    cache_ttl_seconds: detail.cache_ttl_seconds,
                })
            })
            .collect::<Vec<_>>();
        let normalized = TokenUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_write_input_tokens: usage.cache_write_input_tokens,
            details: details.into_boxed_slice(),
            pricing_context: Box::new(bcode_model::ModelPricingContext {
                request_input_tokens: usage.pricing_context.request_input_tokens,
                cache_ttl_seconds: usage.pricing_context.cache_ttl_seconds,
                ..bcode_model::ModelPricingContext::default()
            }),
            reasoning_tokens: usage.reasoning_tokens,
        };
        Self::from_provider_usage(round, &normalized, None)
    }

    /// Whether the provider reported any cached input for this round.
    #[must_use]
    pub fn has_cache_read(&self) -> bool {
        self.cached_input_tokens.is_some_and(|tokens| tokens > 0)
    }

    /// Whether the provider reported any cache writes for this round.
    #[must_use]
    pub fn has_cache_write(&self) -> bool {
        self.cache_write_input_tokens
            .is_some_and(|tokens| tokens > 0)
    }
}

/// Stable measurement keys produced by prompt-cache analysis.
///
/// Keys are stable strings so measurements can flow into eval artifacts and JSON reports
/// without a shared enum dependency.
pub mod measurement {
    /// Number of rounds observed.
    pub const ROUND_COUNT: &str = "prompt_cache.round_count";
    /// Number of rounds eligible for a cache read (every round after the first).
    pub const ELIGIBLE_ROUND_COUNT: &str = "prompt_cache.eligible_round_count";
    /// Number of eligible rounds reporting a cache read.
    pub const HIT_ROUND_COUNT: &str = "prompt_cache.hit_round_count";
    /// `HIT_ROUND_COUNT / ELIGIBLE_ROUND_COUNT`.
    pub const HIT_ROUND_RATIO: &str = "prompt_cache.hit_round_ratio";
    /// Number of consecutive eligible rounds whose cached tokens increased.
    pub const CACHED_INPUT_INCREASE_COUNT: &str = "prompt_cache.cached_input_increase_count";
    /// Sum of ordinary (uncached, unwritten) input tokens across rounds.
    pub const UNCACHED_INPUT_TOKENS: &str = "prompt_cache.uncached_input_tokens";
    /// Sum of cache-read tokens across rounds.
    pub const CACHED_INPUT_TOKENS: &str = "prompt_cache.cached_input_tokens";
    /// Sum of cache-write tokens across rounds.
    pub const CACHE_WRITE_INPUT_TOKENS: &str = "prompt_cache.cache_write_input_tokens";
    /// Sum of complete input tokens across rounds.
    pub const INPUT_TOKENS: &str = "prompt_cache.input_tokens";
    /// `uncached / input` over the final third of eligible rounds.
    pub const LATE_UNCACHED_RATIO: &str = "prompt_cache.late_uncached_ratio";
    /// `total cache writes / final round input`.
    pub const WRITE_AMPLIFICATION: &str = "prompt_cache.write_amplification";
    /// `cached / input` on the warm same-prefix repeat.
    pub const WARM_READ_RATIO: &str = "prompt_cache.warm_read_ratio";
    /// Total cache points dropped by the provider across rounds.
    pub const DROPPED_CACHE_POINTS: &str = "prompt_cache.dropped_cache_points";
}

/// Outcome of one prompt-cache scenario or analysis check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum PromptCacheScenarioOutcome {
    /// The behavior matched expectations.
    Passed,
    /// The scenario does not apply to this model's advertised capabilities.
    Skipped {
        /// Why the scenario did not apply.
        reason: String,
    },
    /// Observed behavior contradicted expectations.
    Failed {
        /// What was expected versus observed.
        reason: String,
    },
}

impl PromptCacheScenarioOutcome {
    /// Whether the outcome is a pass.
    #[must_use]
    pub const fn is_passed(&self) -> bool {
        matches!(self, Self::Passed)
    }

    /// Whether the outcome is a failure.
    #[must_use]
    pub const fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// One named scenario result with its measurements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptCacheScenarioResult {
    /// Stable scenario identifier.
    pub scenario: String,
    /// Outcome of the scenario.
    pub outcome: PromptCacheScenarioOutcome,
    /// Measurements keyed by [`measurement`] constants.
    #[serde(default)]
    pub measurements: BTreeMap<String, f64>,
    /// Per-round observations that produced the measurements.
    #[serde(default)]
    pub rounds: Vec<CacheRoundObservation>,
}

/// Versioned report from verifying one provider/model pair's prompt-cache behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptCacheVerificationReport {
    /// Schema version of this report.
    pub schema_version: u32,
    /// Provider plugin identifier under test.
    pub provider_id: String,
    /// Model identifier under test.
    pub model_id: String,
    /// Expectations the scenarios were judged against, when caching was advertised.
    #[serde(default)]
    pub expectations: Option<PromptCacheExpectations>,
    /// Ordered scenario results.
    pub scenarios: Vec<PromptCacheScenarioResult>,
}

impl PromptCacheVerificationReport {
    /// Whether every scenario passed or was skipped.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.scenarios
            .iter()
            .all(|scenario| !scenario.outcome.is_failed())
    }

    /// Whether at least one scenario actually ran and passed.
    #[must_use]
    pub fn verified_any_behavior(&self) -> bool {
        self.scenarios
            .iter()
            .any(|scenario| scenario.outcome.is_passed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_model::{ModelPricingContext, ModelTokenModality, ModelTokenUsageDetail};

    #[test]
    fn observation_normalizes_usage_and_projection() {
        let usage = TokenUsage {
            input_tokens: Some(1_000),
            output_tokens: Some(10),
            total_tokens: Some(1_010),
            cached_input_tokens: Some(600),
            cache_write_input_tokens: Some(300),
            details: Box::new([
                ModelTokenUsageDetail {
                    bucket: ModelPricingBucket::CacheWriteInput,
                    modality: ModelTokenModality::Text,
                    tokens: 300,
                    cache_ttl_seconds: Some(3_600),
                },
                ModelTokenUsageDetail {
                    bucket: ModelPricingBucket::CacheReadInput,
                    modality: ModelTokenModality::Text,
                    tokens: 600,
                    cache_ttl_seconds: None,
                },
            ]),
            pricing_context: Box::new(ModelPricingContext {
                cache_ttl_seconds: Some(3_600),
                ..ModelPricingContext::default()
            }),
            reasoning_tokens: None,
        };
        let projection = ProviderRequestProjection {
            cache_point_count: Some(3),
            emitted_cache_point_count: Some(3),
            dropped_cache_point_count: Some(0),
            ..ProviderRequestProjection::default()
        };

        let observation = CacheRoundObservation::from_provider_usage(2, &usage, Some(&projection))
            .with_tool_round(true);

        assert_eq!(observation.round, 2);
        assert_eq!(observation.uncached_input_tokens, Some(100));
        assert!(observation.valid_input_breakdown);
        assert_eq!(observation.cache_ttl_seconds, Some(3_600));
        assert_eq!(
            observation.cache_write_tokens_by_ttl,
            vec![CacheWriteTtlTokens {
                ttl_seconds: Some(3_600),
                tokens: 300,
            }]
        );
        assert_eq!(observation.detailed_cache_read_tokens, Some(600));
        assert_eq!(observation.requested_cache_points, Some(3));
        assert_eq!(observation.dropped_cache_points, Some(0));
        assert!(observation.tool_round);
        assert!(observation.has_cache_read());
        assert!(observation.has_cache_write());
    }

    #[test]
    fn report_success_ignores_skips_but_not_failures() {
        let mut report = PromptCacheVerificationReport {
            schema_version: PROMPT_CACHE_VERIFICATION_REPORT_SCHEMA_VERSION,
            provider_id: "p".into(),
            model_id: "m".into(),
            expectations: None,
            scenarios: vec![PromptCacheScenarioResult {
                scenario: "a".into(),
                outcome: PromptCacheScenarioOutcome::Skipped {
                    reason: "not advertised".into(),
                },
                measurements: BTreeMap::new(),
                rounds: Vec::new(),
            }],
        };
        assert!(report.is_success());
        assert!(!report.verified_any_behavior());

        report.scenarios.push(PromptCacheScenarioResult {
            scenario: "b".into(),
            outcome: PromptCacheScenarioOutcome::Failed {
                reason: "no reuse".into(),
            },
            measurements: BTreeMap::new(),
            rounds: Vec::new(),
        });
        assert!(!report.is_success());
    }

    #[test]
    fn report_round_trips_through_json() {
        let report = PromptCacheVerificationReport {
            schema_version: PROMPT_CACHE_VERIFICATION_REPORT_SCHEMA_VERSION,
            provider_id: "p".into(),
            model_id: "m".into(),
            expectations: Some(PromptCacheExpectations {
                mechanism: PromptCacheMechanism::ExplicitPoints,
                reports_cache_writes: Some(true),
                supports_cache_key: true,
                ttl_seconds: BTreeSet::from([300, 3_600]),
                min_prefix_tokens: 1_024,
                min_prefix_declared: true,
                max_cache_points: Some(4),
                thresholds: PromptCacheThresholds::default(),
            }),
            scenarios: Vec::new(),
        };
        let json = serde_json::to_string(&report).expect("serialize");
        let decoded: PromptCacheVerificationReport =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, report);
    }
}
