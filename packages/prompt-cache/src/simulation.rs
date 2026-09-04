//! Deterministic reference prompt cache.
//!
//! [`PromptCacheSimulator`] models a provider prompt cache faithfully enough to verify the host
//! planner, the scenario suite, and the analyzer without credentials:
//!
//! * entries are keyed by the caller's cache key and a hash of the byte-stable request prefix;
//! * explicit mode caches only prefixes that end at a host cache point (plus the system prompt
//!   and tool definitions when the hints ask for them); automatic mode caches every message
//!   boundary;
//! * prefixes shorter than the configured minimum are never cached;
//! * requests carrying more explicit points than the budget drop the oldest points and report
//!   the drop through [`ProviderRequestProjection`];
//! * TTLs outside the configured set are rejected with an `UnsupportedFeature` error;
//! * `PromptCacheMode::Off` disables reads and writes.
//!
//! Token counts come from a whitespace-token estimate of the serialized request so results are
//! deterministic and proportional to content size.

use bcode_model::{
    ContentBlock, ModelCacheCapability, ModelCacheInfo, ModelPricingBucket, ModelPricingContext,
    ModelTokenModality, ModelTokenUsageDetail, ModelTurnRequest, ProviderError,
    ProviderErrorCategory, ProviderRequestProjection, TokenUsage,
};
use bcode_prompt_cache_models::PromptCacheMechanism;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};

/// Static behavior of one simulated cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCacheSimulatorProfile {
    /// Cache entry selection mechanism.
    pub mechanism: PromptCacheMechanism,
    /// Whether cache writes are reported as `cache_write_input_tokens`.
    pub reports_cache_writes: bool,
    /// Accepted explicit TTLs; empty means TTL hints are rejected.
    pub ttl_seconds: BTreeSet<u64>,
    /// Minimum cacheable prefix in simulated tokens.
    pub min_prefix_tokens: u64,
    /// Maximum explicit cache points per request (including system/tool points).
    pub max_cache_points: usize,
    /// Provider identifier reported in projections.
    pub provider_id: String,
    /// API shape reported in projections.
    pub api_shape: String,
}

impl PromptCacheSimulatorProfile {
    /// Advertised cache capabilities matching this profile.
    #[must_use]
    pub fn cache_info(&self) -> ModelCacheInfo {
        let mut capabilities = BTreeSet::from([
            ModelCacheCapability::PromptCacheKey,
            ModelCacheCapability::CacheUsageReporting,
        ]);
        capabilities.insert(match self.mechanism {
            PromptCacheMechanism::ExplicitPoints => ModelCacheCapability::ExplicitCachePoints,
            PromptCacheMechanism::AutomaticPrefix => ModelCacheCapability::AutomaticPrefixCache,
        });
        ModelCacheInfo {
            capabilities,
            ttl_seconds: self.ttl_seconds.clone(),
            min_prefix_tokens: Some(self.min_prefix_tokens),
        }
    }
}

/// Result of serving one request through the simulator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedCacheRound {
    /// Usage to report for the round; input reflects the complete request.
    pub usage: TokenUsage,
    /// Request projection describing point accounting.
    pub projection: ProviderRequestProjection,
}

/// Deterministic prompt cache keyed by caller cache key and prefix hash.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PromptCacheSimulator {
    entries: BTreeMap<String, BTreeMap<u64, u64>>,
}

impl PromptCacheSimulator {
    /// Number of cached prefixes across every cache key.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entries.values().map(BTreeMap::len).sum()
    }

    /// Validate a request's cache hints against the profile without mutating cache state.
    ///
    /// # Errors
    ///
    /// Returns an `UnsupportedFeature` error when a requested TTL is outside the profile's set or
    /// when explicit points are supplied to an automatic-prefix profile.
    pub fn validate(
        profile: &PromptCacheSimulatorProfile,
        request: &ModelTurnRequest,
    ) -> Result<(), ProviderError> {
        if !request.prompt_cache.mode.is_enabled() {
            return Ok(());
        }
        let requested_ttls = request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::CachePoint { hint } => hint.ttl_seconds,
                _ => None,
            })
            .chain(request.prompt_cache.ttl_seconds)
            .collect::<BTreeSet<_>>();
        if !requested_ttls.is_subset(&profile.ttl_seconds) {
            return Err(unsupported(
                "simulated_cache_ttl_unsupported",
                "requested cache TTL is not advertised by the simulated model",
            ));
        }
        if profile.mechanism == PromptCacheMechanism::AutomaticPrefix
            && explicit_point_count(request) > 0
        {
            return Err(unsupported(
                "simulated_cache_points_unsupported",
                "the simulated model caches prefixes automatically and accepts no cache points",
            ));
        }
        Ok(())
    }

    /// Serve one request, updating cache state and returning usage and projection.
    ///
    /// `output_tokens` is added to usage verbatim so callers can keep their own output accounting.
    #[must_use]
    pub fn serve(
        &mut self,
        profile: &PromptCacheSimulatorProfile,
        request: &ModelTurnRequest,
        output_tokens: u32,
    ) -> SimulatedCacheRound {
        let segments = prefix_segments(request);
        let total_input = segments
            .last()
            .map_or(0, |segment| segment.cumulative_tokens);
        let points = PointAccounting::for_request(profile, request);
        let (cached_tokens, written_tokens) = if request.prompt_cache.mode.is_enabled() {
            self.read_and_write(profile, request, &segments, points.dropped)
        } else {
            (0, 0)
        };
        let ttl = request
            .prompt_cache
            .ttl_seconds
            .filter(|_| profile.mechanism == PromptCacheMechanism::ExplicitPoints);
        let usage = simulated_usage(
            profile,
            &SimulatedTokens {
                total_input,
                cached: cached_tokens,
                written: written_tokens,
                output: output_tokens,
                ttl,
            },
        );
        let projection = ProviderRequestProjection {
            provider: Some(profile.provider_id.clone()),
            api_shape: Some(profile.api_shape.clone()),
            message_count: Some(request.messages.len()),
            original_message_count: Some(request.messages.len()),
            sent_message_count: Some(request.messages.len()),
            omitted_message_count: Some(0),
            cache_point_count: Some(points.requested),
            emitted_cache_point_count: Some(points.emitted),
            dropped_cache_point_count: Some(points.dropped),
            ..ProviderRequestProjection::default()
        };
        SimulatedCacheRound { usage, projection }
    }

    /// Look up the longest cached prefix and write any new cacheable boundaries.
    ///
    /// Returns `(cached_tokens, written_tokens)`.
    fn read_and_write(
        &mut self,
        profile: &PromptCacheSimulatorProfile,
        request: &ModelTurnRequest,
        segments: &[PrefixSegment],
        dropped_points: usize,
    ) -> (u64, u64) {
        let key = request
            .prompt_cache
            .key
            .clone()
            .unwrap_or_else(|| "simulated-default".to_string());
        let candidates = cacheable_boundaries(profile, request, segments, dropped_points);
        let store = self.entries.entry(key).or_default();
        let longest_hit = candidates
            .iter()
            .filter(|segment| store.contains_key(&segment.prefix_hash))
            .map(|segment| segment.cumulative_tokens)
            .max()
            .unwrap_or(0);
        let mut written_tokens = 0_u64;
        for segment in &candidates {
            if segment.cumulative_tokens >= profile.min_prefix_tokens
                && !store.contains_key(&segment.prefix_hash)
            {
                store.insert(segment.prefix_hash, segment.cumulative_tokens);
                written_tokens =
                    written_tokens.max(segment.cumulative_tokens.saturating_sub(longest_hit));
            }
        }
        (longest_hit, written_tokens)
    }

    /// Forget every entry, as if all TTLs expired.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

struct PointAccounting {
    requested: usize,
    emitted: usize,
    dropped: usize,
}

impl PointAccounting {
    fn for_request(profile: &PromptCacheSimulatorProfile, request: &ModelTurnRequest) -> Self {
        let requested = explicit_point_count(request);
        let implicit =
            usize::from(
                request.prompt_cache.cache_system_prompt && request.system_prompt.is_some(),
            ) + usize::from(request.prompt_cache.cache_tools && !request.tools.is_empty());
        if profile.mechanism == PromptCacheMechanism::ExplicitPoints {
            let budget = profile.max_cache_points.saturating_sub(implicit);
            Self {
                requested,
                emitted: requested.min(budget),
                dropped: requested.saturating_sub(budget),
            }
        } else {
            Self {
                requested,
                emitted: 0,
                dropped: requested,
            }
        }
    }
}

struct SimulatedTokens {
    total_input: u64,
    cached: u64,
    written: u64,
    output: u32,
    ttl: Option<u64>,
}

fn simulated_usage(profile: &PromptCacheSimulatorProfile, tokens: &SimulatedTokens) -> TokenUsage {
    let input_tokens = clamp(tokens.total_input);
    let cached_input_tokens = clamp(tokens.cached);
    let reported_writes = profile.reports_cache_writes && tokens.written > 0;
    let cache_write_input_tokens = reported_writes.then(|| clamp(tokens.written));
    let mut details = Vec::new();
    let ordinary = input_tokens
        .saturating_sub(cached_input_tokens)
        .saturating_sub(cache_write_input_tokens.unwrap_or_default());
    if ordinary > 0 {
        details.push(ModelTokenUsageDetail {
            bucket: ModelPricingBucket::Input,
            modality: ModelTokenModality::Text,
            tokens: ordinary,
            cache_ttl_seconds: None,
        });
    }
    if cached_input_tokens > 0 {
        details.push(ModelTokenUsageDetail {
            bucket: ModelPricingBucket::CacheReadInput,
            modality: ModelTokenModality::Text,
            tokens: cached_input_tokens,
            cache_ttl_seconds: None,
        });
    }
    if let Some(written) = cache_write_input_tokens {
        details.push(ModelTokenUsageDetail {
            bucket: ModelPricingBucket::CacheWriteInput,
            modality: ModelTokenModality::Text,
            tokens: written,
            cache_ttl_seconds: tokens.ttl,
        });
    }
    if tokens.output > 0 {
        details.push(ModelTokenUsageDetail {
            bucket: ModelPricingBucket::Output,
            modality: ModelTokenModality::Text,
            tokens: tokens.output,
            cache_ttl_seconds: None,
        });
    }
    TokenUsage {
        input_tokens: Some(input_tokens),
        output_tokens: Some(tokens.output),
        total_tokens: Some(input_tokens.saturating_add(tokens.output)),
        cached_input_tokens: Some(cached_input_tokens),
        cache_write_input_tokens,
        details: details.into_boxed_slice(),
        pricing_context: Box::new(ModelPricingContext {
            request_input_tokens: Some(tokens.total_input),
            cache_ttl_seconds: tokens
                .ttl
                .filter(|_| reported_writes || cached_input_tokens > 0),
            ..ModelPricingContext::default()
        }),
        reasoning_tokens: None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrefixSegment {
    /// Hash of the serialized prefix through this boundary.
    prefix_hash: u64,
    /// Simulated tokens in the prefix through this boundary.
    cumulative_tokens: u64,
    /// Message index this boundary follows; `None` for the system/tool prefix.
    message_index: Option<usize>,
    /// Whether the boundary message ends with a host cache point.
    ends_with_cache_point: bool,
}

fn prefix_segments(request: &ModelTurnRequest) -> Vec<PrefixSegment> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut tokens = 0_u64;
    let mut segments = Vec::with_capacity(request.messages.len() + 1);
    let static_prefix = serde_json::to_string(&(request.system_prompt.as_ref(), &request.tools))
        .unwrap_or_default();
    static_prefix.hash(&mut hasher);
    tokens = tokens.saturating_add(simulated_tokens(&static_prefix));
    segments.push(PrefixSegment {
        prefix_hash: hasher.finish(),
        cumulative_tokens: tokens,
        message_index: None,
        ends_with_cache_point: false,
    });
    for (index, message) in request.messages.iter().enumerate() {
        let stable_content = message
            .content
            .iter()
            .filter(|block| !matches!(block, ContentBlock::CachePoint { .. }))
            .collect::<Vec<_>>();
        let serialized =
            serde_json::to_string(&(&message.role, &stable_content)).unwrap_or_default();
        serialized.hash(&mut hasher);
        tokens = tokens.saturating_add(simulated_tokens(&serialized));
        segments.push(PrefixSegment {
            prefix_hash: hasher.finish(),
            cumulative_tokens: tokens,
            message_index: Some(index),
            ends_with_cache_point: matches!(
                message.content.last(),
                Some(ContentBlock::CachePoint { .. })
            ),
        });
    }
    segments
}

fn cacheable_boundaries(
    profile: &PromptCacheSimulatorProfile,
    request: &ModelTurnRequest,
    segments: &[PrefixSegment],
    dropped_points: usize,
) -> Vec<PrefixSegment> {
    match profile.mechanism {
        PromptCacheMechanism::AutomaticPrefix => segments.to_vec(),
        PromptCacheMechanism::ExplicitPoints => {
            let mut boundaries = Vec::new();
            let static_point = (request.prompt_cache.cache_system_prompt
                && request.system_prompt.is_some())
                || (request.prompt_cache.cache_tools && !request.tools.is_empty());
            if static_point && let Some(first) = segments.first() {
                boundaries.push(first.clone());
            }
            // Providers drop the oldest points first when over budget.
            let mut skip = dropped_points;
            for segment in segments
                .iter()
                .filter(|segment| segment.ends_with_cache_point)
            {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                boundaries.push(segment.clone());
            }
            boundaries
        }
    }
}

fn explicit_point_count(request: &ModelTurnRequest) -> usize {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| matches!(block, ContentBlock::CachePoint { .. }))
        .count()
}

fn simulated_tokens(serialized: &str) -> u64 {
    u64::try_from(serialized.split_whitespace().count()).unwrap_or(u64::MAX)
}

fn clamp(tokens: u64) -> u32 {
    u32::try_from(tokens).unwrap_or(u32::MAX)
}

fn unsupported(code: &str, message: &str) -> ProviderError {
    ProviderError {
        code: code.to_string(),
        category: ProviderErrorCategory::UnsupportedFeature,
        message: message.to_string(),
        retryable: false,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_model::{
        MessageRole, ModelMessage, ModelParameters, PromptCacheHints, PromptCacheMode,
        PromptCachePoint, ToolCallRequestPolicy,
    };

    fn explicit_profile() -> PromptCacheSimulatorProfile {
        PromptCacheSimulatorProfile {
            mechanism: PromptCacheMechanism::ExplicitPoints,
            reports_cache_writes: true,
            ttl_seconds: BTreeSet::from([300, 3_600]),
            min_prefix_tokens: 8,
            max_cache_points: 4,
            provider_id: "sim".into(),
            api_shape: "simulated".into(),
        }
    }

    fn request(messages: Vec<ModelMessage>, hints: PromptCacheHints) -> ModelTurnRequest {
        let session_id = bcode_session_models::SessionId::new();
        ModelTurnRequest {
            session_id,
            turn_id: "t".into(),
            model_id: "sim".into(),
            provider_context: bcode_model::ProviderRequestContext::default(),
            system_prompt: Some("stable system prompt with several words in it".into()),
            messages,
            tools: Vec::new(),
            tool_call_policy: ToolCallRequestPolicy::default(),
            tool_schema_mode: None,
            parameters: ModelParameters::default(),
            structured_output: None,
            context_management: bcode_model::ContextManagementRequest::default(),
            prompt_cache: hints,
            conversation_reuse: bcode_model::ConversationReuseHints::default(),
            metadata: BTreeMap::new(),
        }
    }

    fn hints() -> PromptCacheHints {
        PromptCacheHints {
            mode: PromptCacheMode::Auto,
            key: Some("k".into()),
            ttl_seconds: Some(3_600),
            supported_ttl_seconds: BTreeSet::from([300, 3_600]),
            cache_system_prompt: true,
            cache_tools: true,
        }
    }

    fn user(text: &str, point: bool) -> ModelMessage {
        let mut content = vec![ContentBlock::Text { text: text.into() }];
        if point {
            content.push(ContentBlock::CachePoint {
                hint: PromptCachePoint {
                    label: None,
                    ttl_seconds: Some(3_600),
                },
            });
        }
        ModelMessage {
            role: MessageRole::User,
            content,
        }
    }

    #[test]
    fn cold_then_warm_reports_write_then_read() {
        let profile = explicit_profile();
        let mut simulator = PromptCacheSimulator::default();
        let first = request(vec![user("one two three four five six", true)], hints());
        let cold = simulator.serve(&profile, &first, 3);
        assert!(cold.usage.cache_write_input_tokens.is_some_and(|w| w > 0));
        assert_eq!(cold.usage.cached_input_tokens, Some(0));
        assert!(cold.usage.has_valid_input_breakdown());
        assert_eq!(cold.projection.dropped_cache_point_count, Some(0));
        assert_eq!(cold.usage.pricing_context.cache_ttl_seconds, Some(3_600));

        let warm = simulator.serve(&profile, &first, 3);
        assert!(warm.usage.cached_input_tokens.is_some_and(|c| c > 0));
        assert_eq!(warm.usage.cache_write_input_tokens, None);
        assert_eq!(warm.usage.cached_input_tokens, warm.usage.input_tokens);
    }

    #[test]
    fn off_mode_never_caches() {
        let profile = explicit_profile();
        let mut simulator = PromptCacheSimulator::default();
        let off = PromptCacheHints::default();
        let off = PromptCacheHints {
            mode: PromptCacheMode::Off,
            ..off
        };
        let first = request(vec![user("one two three four five six", false)], off);
        let _ = simulator.serve(&profile, &first, 1);
        let second = simulator.serve(&profile, &first, 1);
        assert_eq!(second.usage.cached_input_tokens, Some(0));
        assert_eq!(second.usage.cache_write_input_tokens, None);
    }

    #[test]
    fn over_budget_points_are_dropped_and_reported() {
        let profile = explicit_profile();
        let mut simulator = PromptCacheSimulator::default();
        let messages = (0..6)
            .map(|index| user(&format!("message number {index} with words"), true))
            .collect();
        let round = simulator.serve(&profile, &request(messages, hints()), 1);
        // 4 total budget minus 1 implicit system point leaves 3 message points.
        assert_eq!(round.projection.cache_point_count, Some(6));
        assert_eq!(round.projection.emitted_cache_point_count, Some(3));
        assert_eq!(round.projection.dropped_cache_point_count, Some(3));
    }

    #[test]
    fn unsupported_ttl_and_points_are_rejected() {
        let profile = explicit_profile();
        let mut bad_ttl = hints();
        bad_ttl.ttl_seconds = Some(86_400);
        let error =
            PromptCacheSimulator::validate(&profile, &request(vec![user("x", false)], bad_ttl))
                .expect_err("unsupported ttl");
        assert_eq!(error.category, ProviderErrorCategory::UnsupportedFeature);

        let automatic = PromptCacheSimulatorProfile {
            mechanism: PromptCacheMechanism::AutomaticPrefix,
            ttl_seconds: BTreeSet::new(),
            ..explicit_profile()
        };
        let mut no_ttl = hints();
        no_ttl.ttl_seconds = None;
        let mut pointed = user("x", true);
        for block in &mut pointed.content {
            if let ContentBlock::CachePoint { hint } = block {
                hint.ttl_seconds = None;
            }
        }
        let error = PromptCacheSimulator::validate(&automatic, &request(vec![pointed], no_ttl))
            .expect_err("points unsupported");
        assert_eq!(error.code, "simulated_cache_points_unsupported");
    }

    #[test]
    fn automatic_prefix_caches_growing_conversation_without_points() {
        let profile = PromptCacheSimulatorProfile {
            mechanism: PromptCacheMechanism::AutomaticPrefix,
            reports_cache_writes: false,
            ttl_seconds: BTreeSet::new(),
            ..explicit_profile()
        };
        let mut simulator = PromptCacheSimulator::default();
        let mut no_ttl = hints();
        no_ttl.ttl_seconds = None;
        let mut messages = vec![user("first message with several words", false)];
        let first = simulator.serve(&profile, &request(messages.clone(), no_ttl.clone()), 1);
        assert_eq!(first.usage.cache_write_input_tokens, None);
        messages.push(ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: "reply".into(),
            }],
        });
        messages.push(user("second message with more words", false));
        let second = simulator.serve(&profile, &request(messages, no_ttl), 1);
        assert_eq!(second.usage.cached_input_tokens, first.usage.input_tokens);
        assert!(second.usage.input_tokens > first.usage.input_tokens);
    }

    #[test]
    fn short_prefixes_are_not_cached() {
        let profile = PromptCacheSimulatorProfile {
            min_prefix_tokens: 10_000,
            ..explicit_profile()
        };
        let mut simulator = PromptCacheSimulator::default();
        let first = request(vec![user("tiny", true)], hints());
        let cold = simulator.serve(&profile, &first, 1);
        assert_eq!(cold.usage.cache_write_input_tokens, None);
        let warm = simulator.serve(&profile, &first, 1);
        assert_eq!(warm.usage.cached_input_tokens, Some(0));
    }
}
