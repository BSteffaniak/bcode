//! Pure `OpenAI` token-usage decoding shared across provider integrations.
use bcode_model::TokenUsage;
use serde::Deserialize;

/// Usage object shared by Responses and Chat Completions wire formats.
#[derive(Debug, Deserialize)]
pub struct OpenAiUsage {
    /// Server-confirmed service tier, when present inside the usage object.
    #[serde(default)]
    pub service_tier: Option<String>,
    /// Chat Completions inclusive prompt count.
    #[serde(default)]
    pub prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    total_tokens: Option<u32>,
    #[serde(default)]
    prompt_tokens_details: Option<OpenAiInputTokenDetails>,
    #[serde(default)]
    completion_tokens_details: Option<OpenAiCompletionTokenDetails>,
    /// Responses inclusive input count.
    #[serde(default)]
    pub input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
    #[serde(default)]
    input_tokens_details: Option<OpenAiInputTokenDetails>,
    #[serde(default)]
    output_tokens_details: Option<OpenAiOutputTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct OpenAiCompletionTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
    #[serde(default)]
    audio_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct OpenAiInputTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
    /// Separately billed input written to the prompt cache, not additional input.
    #[serde(
        default,
        rename = "cache_write_tokens",
        alias = "cache_creation_tokens"
    )]
    cache_write: Option<u32>,
    #[serde(default)]
    audio_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct OpenAiOutputTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
    #[serde(default)]
    audio_tokens: Option<u32>,
}

/// Billing context resolved from the request and confirmed response labels.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenAiUsageContext {
    /// Effective tier, preferring server confirmation over requested defaults.
    #[serde(default)]
    pub service_tier: Option<String>,
    /// Provider cache retention label.
    #[serde(default)]
    pub prompt_cache_retention: Option<String>,
    /// Effective provider model identifier.
    #[serde(default)]
    pub model: Option<String>,
}

/// Normalize inclusive input totals, cache subsets, reasoning, and modality evidence.
#[must_use]
pub fn normalize_usage(usage: OpenAiUsage, context: OpenAiUsageContext) -> TokenUsage {
    let service_tier = usage.service_tier.or(context.service_tier);
    let cached_input_tokens = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
        .or_else(|| {
            usage
                .input_tokens_details
                .as_ref()
                .and_then(|details| details.cached_tokens)
        });
    let cache_write_input_tokens = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cache_write)
        .or_else(|| {
            usage
                .input_tokens_details
                .as_ref()
                .and_then(|details| details.cache_write)
        });
    let reasoning_tokens = usage
        .completion_tokens_details
        .as_ref()
        .and_then(|details| details.reasoning_tokens)
        .or_else(|| {
            usage
                .output_tokens_details
                .as_ref()
                .and_then(|details| details.reasoning_tokens)
        });
    let input_audio_tokens = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.audio_tokens)
        .or_else(|| {
            usage
                .input_tokens_details
                .as_ref()
                .and_then(|details| details.audio_tokens)
        });
    let output_audio_tokens = usage
        .completion_tokens_details
        .as_ref()
        .and_then(|details| details.audio_tokens)
        .or_else(|| {
            usage
                .output_tokens_details
                .as_ref()
                .and_then(|details| details.audio_tokens)
        });
    let input_tokens = usage.prompt_tokens.or(usage.input_tokens);
    // OpenAI Chat Completions `prompt_tokens` and Responses `input_tokens` are totals that already
    // include cached tokens. Cached details are a billed subset, not additional model-visible
    // input, across both native OpenAI and compatible surfaces using this contract.
    let output_tokens = usage.completion_tokens.or(usage.output_tokens);
    let details = openai_modality_usage_details(
        input_tokens,
        output_tokens,
        cached_input_tokens,
        cache_write_input_tokens,
        input_audio_tokens,
        output_audio_tokens,
    );
    TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens,
        cache_write_input_tokens,
        details,
        pricing_context: Box::new(bcode_model::ModelPricingContext {
            service_tier: service_tier.map(|tier| bcode_model::normalize_model_service_tier(&tier)),
            invocation_class: Some(bcode_model::ModelInvocationClass::OnDemand),
            billing_scope: context
                .model
                .as_deref()
                .map(bcode_model::model_billing_scope_from_effective_id),
            request_input_tokens: input_tokens.map(u64::from),
            cache_ttl_seconds: context
                .prompt_cache_retention
                .as_deref()
                .and_then(|retention| match retention {
                    "24h" => Some(24 * 60 * 60),
                    _ => None,
                }),
        }),
        reasoning_tokens,
    }
}

/// Return text billing components for a complete inclusive-input report.
pub(crate) fn text_usage_details(usage: &TokenUsage) -> Box<[bcode_model::ModelTokenUsageDetail]> {
    let Some(input) = usage.input_tokens else {
        return Box::default();
    };
    let cached = usage.cached_input_tokens.unwrap_or_default();
    let written = usage.cache_write_input_tokens.unwrap_or_default();
    if u64::from(cached) + u64::from(written) > u64::from(input) {
        return Box::default();
    }
    [
        (
            bcode_model::ModelPricingBucket::Input,
            input - cached - written,
            None,
        ),
        (
            bcode_model::ModelPricingBucket::CacheReadInput,
            cached,
            usage.pricing_context.cache_ttl_seconds,
        ),
        (
            bcode_model::ModelPricingBucket::CacheWriteInput,
            written,
            usage.pricing_context.cache_ttl_seconds,
        ),
        (
            bcode_model::ModelPricingBucket::Output,
            usage.output_tokens.unwrap_or_default(),
            None,
        ),
    ]
    .into_iter()
    .filter(|(_, tokens, _)| *tokens > 0)
    .map(
        |(bucket, tokens, cache_ttl_seconds)| bcode_model::ModelTokenUsageDetail {
            bucket,
            modality: bcode_model::ModelTokenModality::Text,
            tokens,
            cache_ttl_seconds,
        },
    )
    .collect::<Vec<_>>()
    .into_boxed_slice()
}

fn openai_modality_usage_details(
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    cached_input_tokens: Option<u32>,
    cache_write_input_tokens: Option<u32>,
    input_audio_tokens: Option<u32>,
    output_audio_tokens: Option<u32>,
) -> Box<[bcode_model::ModelTokenUsageDetail]> {
    let ambiguous_modality = input_audio_tokens.is_some_and(|audio| {
        audio > input_tokens.unwrap_or_default()
            || (audio > 0
                && (cached_input_tokens.is_none_or(|cached| cached > 0)
                    || cache_write_input_tokens.is_some_and(|written| written > 0)))
    }) || output_audio_tokens
        .is_some_and(|audio| audio > output_tokens.unwrap_or_default());
    if ambiguous_modality {
        // Preserve evidence without inventing cached/written audio allocations.
        return [
            (bcode_model::ModelPricingBucket::Input, input_audio_tokens),
            (bcode_model::ModelPricingBucket::Output, output_audio_tokens),
        ]
        .into_iter()
        .filter_map(|(bucket, tokens)| {
            tokens
                .filter(|tokens| *tokens > 0)
                .map(|tokens| bcode_model::ModelTokenUsageDetail {
                    bucket,
                    modality: bcode_model::ModelTokenModality::Audio,
                    tokens,
                    cache_ttl_seconds: None,
                })
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    }
    if input_audio_tokens.is_none() && output_audio_tokens.is_none() {
        return Box::default();
    }
    let cached = cached_input_tokens.unwrap_or_default();
    let written = cache_write_input_tokens.unwrap_or_default();
    let input_audio = input_audio_tokens.unwrap_or_default();
    let output_audio = output_audio_tokens.unwrap_or_default();
    let mut details = Vec::new();
    for (bucket, modality, tokens) in [
        (
            bcode_model::ModelPricingBucket::Input,
            bcode_model::ModelTokenModality::Text,
            input_tokens
                .unwrap_or_default()
                .saturating_sub(cached)
                .saturating_sub(written)
                .saturating_sub(input_audio),
        ),
        (
            bcode_model::ModelPricingBucket::Input,
            bcode_model::ModelTokenModality::Audio,
            input_audio,
        ),
        (
            bcode_model::ModelPricingBucket::CacheReadInput,
            bcode_model::ModelTokenModality::Text,
            cached,
        ),
        (
            bcode_model::ModelPricingBucket::CacheWriteInput,
            bcode_model::ModelTokenModality::Text,
            written,
        ),
        (
            bcode_model::ModelPricingBucket::Output,
            bcode_model::ModelTokenModality::Text,
            output_tokens
                .unwrap_or_default()
                .saturating_sub(output_audio),
        ),
        (
            bcode_model::ModelPricingBucket::Output,
            bcode_model::ModelTokenModality::Audio,
            output_audio,
        ),
    ] {
        if tokens > 0 {
            details.push(bcode_model::ModelTokenUsageDetail {
                bucket,
                modality,
                tokens,
                cache_ttl_seconds: None,
            });
        }
    }
    details.into_boxed_slice()
}
