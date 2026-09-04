//! Cache-simulating fake models.
//!
//! `fake-cache-explicit` and `fake-cache-prefix` back their usage reports with
//! [`bcode_prompt_cache::simulation::PromptCacheSimulator`] so host planning, provider
//! conformance, and eval telemetry can be verified against a deterministic reference cache
//! without credentials. `fake-echo` keeps rejecting cache hints so callers that must not send
//! them still fail fast.

use bcode_model::{
    CapabilitySource, CapabilitySupport, ContentBlock, MessageRole, ModelCapability,
    ModelFeatureSupport, ModelInfo, ModelMessage, ModelTurnRequest, PromptCacheFeature,
    ProviderError, ProviderTurnEvent, StopReason, ToolCall, ToolChoice,
};
use bcode_prompt_cache::PromptCacheMechanism;
use bcode_prompt_cache::simulation::{
    PromptCacheSimulator, PromptCacheSimulatorProfile, SimulatedCacheRound,
};
use std::collections::BTreeSet;
use std::sync::Mutex;

/// Explicit-breakpoint cache model that reports cache writes (Anthropic-style).
pub const FAKE_CACHE_EXPLICIT_MODEL_ID: &str = "fake-cache-explicit";
/// Automatic-prefix cache model that reports reads only (OpenAI-style).
pub const FAKE_CACHE_PREFIX_MODEL_ID: &str = "fake-cache-prefix";

/// Minimum simulated prefix tokens before the fake cache stores an entry.
///
/// Small enough that conformance workloads stay cheap, large enough that a bare user message
/// does not qualify, so "too short to cache" is exercised.
pub const FAKE_CACHE_MIN_PREFIX_TOKENS: u64 = 64;

/// Explicit breakpoint budget (system + tools + messages) for the explicit fake model.
pub const FAKE_CACHE_MAX_CACHE_POINTS: usize = 4;

/// Whether a model id is served by the cache simulator.
#[must_use]
pub fn is_cache_model(model_id: &str) -> bool {
    matches!(
        model_id,
        FAKE_CACHE_EXPLICIT_MODEL_ID | FAKE_CACHE_PREFIX_MODEL_ID
    )
}

/// Simulator profile for a cache model id, if it is one.
#[must_use]
pub fn profile_for(model_id: &str) -> Option<PromptCacheSimulatorProfile> {
    match model_id {
        FAKE_CACHE_EXPLICIT_MODEL_ID => Some(PromptCacheSimulatorProfile {
            mechanism: PromptCacheMechanism::ExplicitPoints,
            reports_cache_writes: true,
            ttl_seconds: BTreeSet::from([300, 3_600]),
            min_prefix_tokens: FAKE_CACHE_MIN_PREFIX_TOKENS,
            max_cache_points: FAKE_CACHE_MAX_CACHE_POINTS,
            provider_id: "bcode.fake-provider".to_string(),
            api_shape: "fake_cache_explicit".to_string(),
        }),
        FAKE_CACHE_PREFIX_MODEL_ID => Some(PromptCacheSimulatorProfile {
            mechanism: PromptCacheMechanism::AutomaticPrefix,
            reports_cache_writes: false,
            ttl_seconds: BTreeSet::new(),
            min_prefix_tokens: FAKE_CACHE_MIN_PREFIX_TOKENS,
            max_cache_points: 0,
            provider_id: "bcode.fake-provider".to_string(),
            api_shape: "fake_cache_prefix".to_string(),
        }),
        _ => None,
    }
}

/// Model listings for the cache models.
#[must_use]
pub fn models() -> Vec<ModelInfo> {
    [FAKE_CACHE_EXPLICIT_MODEL_ID, FAKE_CACHE_PREFIX_MODEL_ID]
        .into_iter()
        .filter_map(|model_id| profile_for(model_id).map(|profile| (model_id, profile)))
        .map(|(model_id, profile)| ModelInfo {
            model_id: model_id.to_string(),
            display_name: match profile.mechanism {
                PromptCacheMechanism::ExplicitPoints => "Fake Cache (explicit points)",
                PromptCacheMechanism::AutomaticPrefix => "Fake Cache (automatic prefix)",
            }
            .to_string(),
            is_default: false,
            context_window: Some(64_000),
            max_output_tokens: Some(1_000),
            max_image_input_base64_bytes: None,
            capabilities: [
                ModelCapability::StreamingText,
                ModelCapability::ToolCalls,
                ModelCapability::PromptCaching,
            ]
            .into_iter()
            .collect(),
            feature_support: feature_support(&profile),
            reasoning: None,
            cache: profile.cache_info(),
            metadata_source: Some(bcode_model::ModelMetadataSource::BundledCatalog),
            pricing: None,
            api_surface: None,
            visibility: bcode_model::ModelVisibility::Visible,
        })
        .collect()
}

/// Prompt-cache feature claims matching a simulator profile.
#[must_use]
pub fn feature_support(profile: &PromptCacheSimulatorProfile) -> ModelFeatureSupport {
    let supported = || CapabilitySupport::supported(CapabilitySource::TestContract);
    let unsupported = |reason: &str| CapabilitySupport::Unsupported {
        source: CapabilitySource::TestContract,
        reason: reason.to_string(),
    };
    let explicit = profile.mechanism == PromptCacheMechanism::ExplicitPoints;
    let mut support =
        super::fake_feature_support_for_execution(bcode_model::CapabilityExecution::Direct);
    support.prompt_cache = [
        (PromptCacheFeature::ConversationPrefix, true),
        (PromptCacheFeature::ExplicitSystem, explicit),
        (PromptCacheFeature::ExplicitTools, explicit),
        (PromptCacheFeature::ExplicitMessage, explicit),
        (
            PromptCacheFeature::Ttl,
            explicit && !profile.ttl_seconds.is_empty(),
        ),
    ]
    .into_iter()
    .map(|(feature, is_supported)| {
        (
            feature,
            if is_supported {
                supported()
            } else {
                unsupported("fake cache model does not implement this cache feature")
            },
        )
    })
    .collect();
    support
}

/// Provider-side cache feature claims: the union of what any fake cache model can do.
#[must_use]
pub fn provider_feature_claims() -> Vec<(PromptCacheFeature, CapabilitySupport)> {
    let supported = CapabilitySupport::supported(CapabilitySource::TestContract);
    [
        PromptCacheFeature::ConversationPrefix,
        PromptCacheFeature::ExplicitSystem,
        PromptCacheFeature::ExplicitTools,
        PromptCacheFeature::ExplicitMessage,
        PromptCacheFeature::Ttl,
    ]
    .into_iter()
    .map(|feature| (feature, supported.clone()))
    .collect()
}

/// Process-wide simulator shared by every cache-model turn.
///
/// Cache entries are partitioned by the request's cache key, so independent sessions do not
/// observe each other's entries even though they share this store.
static SIMULATOR: Mutex<Option<PromptCacheSimulator>> = Mutex::new(None);

/// Forget every simulated cache entry.
pub fn reset() {
    if let Ok(mut simulator) = SIMULATOR.lock() {
        *simulator = None;
    }
}

/// Validate cache hints for a cache model without mutating state.
///
/// # Errors
///
/// Returns the simulator's `UnsupportedFeature` error for unadvertised TTLs or misplaced cache
/// points.
pub fn validate(
    profile: &PromptCacheSimulatorProfile,
    request: &ModelTurnRequest,
) -> Result<(), ProviderError> {
    PromptCacheSimulator::validate(profile, request)
}

/// Serve one turn through the simulator, pushing the complete event stream onto `turn`.
///
/// The response is deterministic: a pending tool call is answered with the next `cache_probe`
/// call when the last message is a user message and tools are offered; otherwise a short text
/// reply. Usage always comes from the simulator so cache accounting reflects the real prefix.
pub fn serve_turn(
    profile: &PromptCacheSimulatorProfile,
    request: &ModelTurnRequest,
    push: &dyn Fn(ProviderTurnEvent),
) {
    let last_role = request.messages.last().map(|message| message.role);
    let tool_choice = &request.tool_call_policy.choice;
    let wants_tool_call = last_role == Some(MessageRole::User)
        && !request.tools.is_empty()
        && match tool_choice {
            ToolChoice::None => false,
            ToolChoice::Required | ToolChoice::Tool { .. } => true,
            ToolChoice::Auto => last_user_text(&request.messages).contains("probe"),
        };
    let (text, tool_call): (String, Option<ToolCall>) = if wants_tool_call {
        let next_index = request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter(|block| matches!(block, ContentBlock::ToolCall { .. }))
            .count();
        let tool_name = match tool_choice {
            ToolChoice::Tool { name } => name.clone(),
            _ => request.tools[0].name.clone(),
        };
        (
            String::new(),
            Some(ToolCall {
                id: format!("fake-cache-probe-{next_index}"),
                name: tool_name,
                arguments: serde_json::json!({"index": next_index}),
            }),
        )
    } else {
        (
            format!("fake cache reply: {}", last_user_text(&request.messages)),
            None,
        )
    };
    let output_tokens = u32::try_from(text.split_whitespace().count().max(1)).unwrap_or(u32::MAX);
    let SimulatedCacheRound { usage, projection } = {
        let mut guard = SIMULATOR
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .get_or_insert_with(PromptCacheSimulator::default)
            .serve(profile, request, output_tokens)
    };
    push(ProviderTurnEvent::RequestProjection { projection });
    if let Some(call) = tool_call {
        push(ProviderTurnEvent::ToolCallStarted {
            call_id: call.id.clone(),
            name: call.name.clone(),
        });
        push(ProviderTurnEvent::ToolCallFinished { call });
        push(ProviderTurnEvent::Usage { usage });
        push(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::ToolCall,
        });
    } else {
        push(ProviderTurnEvent::TextDelta { text });
        push(ProviderTurnEvent::Usage { usage });
        push(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::EndTurn,
        });
    }
}

fn last_user_text(messages: &[ModelMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::User)
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}
