//! Built-in model metadata for OpenAI-compatible providers.

use bcode_model::{
    ModelMetadataSource, ModelPricingInfo, ModelPricingSource, ModelPricingUnit, ModelTokenPrice,
};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelMetadata {
    pub context_window: u32,
    pub max_output_tokens: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelMetadataResolution {
    pub metadata: ModelMetadata,
    pub source: ModelMetadataSource,
}

#[derive(Debug, Clone, Copy)]
struct CatalogPattern {
    needle: &'static str,
    metadata: ModelMetadata,
}

#[derive(Debug, Clone, Copy)]
struct PricingPattern {
    needle: &'static str,
    pricing: PricingDefinition,
}

#[derive(Debug, Clone, Copy)]
struct PricingDefinition {
    input: u64,
    cached_input: Option<u64>,
    output: u64,
}

const DEFAULT_METADATA: ModelMetadata = ModelMetadata {
    context_window: 128_000,
    max_output_tokens: 16_384,
};

const GPT_4_1_METADATA: ModelMetadata = ModelMetadata {
    context_window: 1_047_576,
    max_output_tokens: 32_768,
};

const GPT_4O_METADATA: ModelMetadata = ModelMetadata {
    context_window: 128_000,
    max_output_tokens: 16_384,
};

const OPENAI_REASONING_METADATA: ModelMetadata = ModelMetadata {
    context_window: 200_000,
    max_output_tokens: 100_000,
};

const CODEX_LARGE_METADATA: ModelMetadata = ModelMetadata {
    context_window: 272_000,
    max_output_tokens: 100_000,
};

const CODEX_DEFAULT_METADATA: ModelMetadata = ModelMetadata {
    context_window: 128_000,
    max_output_tokens: 100_000,
};

const GROK_4_METADATA: ModelMetadata = ModelMetadata {
    context_window: 256_000,
    max_output_tokens: 32_000,
};

const GROK_3_METADATA: ModelMetadata = ModelMetadata {
    context_window: 131_072,
    max_output_tokens: 32_000,
};

const PRICING_PATTERNS: &[PricingPattern] = &[
    PricingPattern {
        needle: "gpt-5.5-pro",
        pricing: PricingDefinition {
            input: 21_000_000,
            cached_input: Some(2_100_000),
            output: 168_000_000,
        },
    },
    PricingPattern {
        needle: "gpt-5.5",
        pricing: PricingDefinition {
            input: 3_500_000,
            cached_input: Some(350_000),
            output: 28_000_000,
        },
    },
    PricingPattern {
        needle: "gpt-5.4-pro",
        pricing: PricingDefinition {
            input: 15_000_000,
            cached_input: Some(1_500_000),
            output: 120_000_000,
        },
    },
    PricingPattern {
        needle: "gpt-5.4-mini",
        pricing: PricingDefinition {
            input: 250_000,
            cached_input: Some(25_000),
            output: 2_000_000,
        },
    },
    PricingPattern {
        needle: "gpt-5.4-nano",
        pricing: PricingDefinition {
            input: 50_000,
            cached_input: Some(5_000),
            output: 400_000,
        },
    },
    PricingPattern {
        needle: "gpt-5.4",
        pricing: PricingDefinition {
            input: 2_000_000,
            cached_input: Some(200_000),
            output: 16_000_000,
        },
    },
    PricingPattern {
        needle: "gpt-5.3-codex-spark",
        pricing: PricingDefinition {
            input: 1_750_000,
            cached_input: Some(175_000),
            output: 14_000_000,
        },
    },
    PricingPattern {
        needle: "gpt-5.2-pro",
        pricing: PricingDefinition {
            input: 15_000_000,
            cached_input: Some(1_500_000),
            output: 120_000_000,
        },
    },
    PricingPattern {
        needle: "gpt-5-mini",
        pricing: PricingDefinition {
            input: 250_000,
            cached_input: Some(25_000),
            output: 2_000_000,
        },
    },
    PricingPattern {
        needle: "gpt-5-nano",
        pricing: PricingDefinition {
            input: 50_000,
            cached_input: Some(5_000),
            output: 400_000,
        },
    },
    PricingPattern {
        needle: "gpt-5-pro",
        pricing: PricingDefinition {
            input: 15_000_000,
            cached_input: Some(1_500_000),
            output: 120_000_000,
        },
    },
    PricingPattern {
        needle: "gpt-5",
        pricing: PricingDefinition {
            input: 1_250_000,
            cached_input: Some(125_000),
            output: 10_000_000,
        },
    },
    PricingPattern {
        needle: "gpt-4.1-mini",
        pricing: PricingDefinition {
            input: 400_000,
            cached_input: Some(100_000),
            output: 1_600_000,
        },
    },
    PricingPattern {
        needle: "gpt-4.1-nano",
        pricing: PricingDefinition {
            input: 100_000,
            cached_input: Some(25_000),
            output: 400_000,
        },
    },
    PricingPattern {
        needle: "gpt-4.1",
        pricing: PricingDefinition {
            input: 2_000_000,
            cached_input: Some(500_000),
            output: 8_000_000,
        },
    },
    PricingPattern {
        needle: "gpt-4o-mini",
        pricing: PricingDefinition {
            input: 150_000,
            cached_input: Some(75_000),
            output: 600_000,
        },
    },
    PricingPattern {
        needle: "gpt-4o",
        pricing: PricingDefinition {
            input: 2_500_000,
            cached_input: Some(1_250_000),
            output: 10_000_000,
        },
    },
    PricingPattern {
        needle: "o3-pro",
        pricing: PricingDefinition {
            input: 20_000_000,
            cached_input: None,
            output: 80_000_000,
        },
    },
    PricingPattern {
        needle: "o3",
        pricing: PricingDefinition {
            input: 2_000_000,
            cached_input: Some(500_000),
            output: 8_000_000,
        },
    },
    PricingPattern {
        needle: "o4-mini",
        pricing: PricingDefinition {
            input: 1_100_000,
            cached_input: Some(275_000),
            output: 4_400_000,
        },
    },
];

const PATTERNS: &[CatalogPattern] = &[
    CatalogPattern {
        needle: "gpt-5.5",
        metadata: CODEX_LARGE_METADATA,
    },
    CatalogPattern {
        needle: "gpt-4.1",
        metadata: GPT_4_1_METADATA,
    },
    CatalogPattern {
        needle: "chatgpt-4o",
        metadata: GPT_4O_METADATA,
    },
    CatalogPattern {
        needle: "gpt-4o",
        metadata: GPT_4O_METADATA,
    },
    CatalogPattern {
        needle: "gpt-5",
        metadata: CODEX_DEFAULT_METADATA,
    },
    CatalogPattern {
        needle: "o1",
        metadata: OPENAI_REASONING_METADATA,
    },
    CatalogPattern {
        needle: "o3",
        metadata: OPENAI_REASONING_METADATA,
    },
    CatalogPattern {
        needle: "o4",
        metadata: OPENAI_REASONING_METADATA,
    },
    CatalogPattern {
        needle: "grok-4",
        metadata: GROK_4_METADATA,
    },
    CatalogPattern {
        needle: "grok-3",
        metadata: GROK_3_METADATA,
    },
    CatalogPattern {
        needle: "fable",
        metadata: DEFAULT_METADATA,
    },
    CatalogPattern {
        needle: "grok",
        metadata: GROK_3_METADATA,
    },
];

#[must_use]
pub fn resolve(
    model_id: &str,
    provider_metadata: &BTreeMap<String, Value>,
) -> ModelMetadataResolution {
    if let Some(metadata) = metadata_from_provider_api(provider_metadata) {
        return ModelMetadataResolution {
            metadata,
            source: ModelMetadataSource::ProviderApi,
        };
    }

    let normalized = normalize_model_id(model_id);
    if let Some(pattern) = PATTERNS
        .iter()
        .find(|pattern| normalized.contains(pattern.needle))
    {
        return ModelMetadataResolution {
            metadata: pattern.metadata,
            source: ModelMetadataSource::PatternMatch,
        };
    }

    ModelMetadataResolution {
        metadata: DEFAULT_METADATA,
        source: ModelMetadataSource::ProviderDefault,
    }
}

#[must_use]
pub fn pricing_for(
    model_id: &str,
    provider_metadata: &BTreeMap<String, Value>,
) -> Option<ModelPricingInfo> {
    if let Some(pricing) = pricing_from_provider_api(provider_metadata) {
        return Some(pricing);
    }
    let normalized = normalize_model_id(model_id);
    PRICING_PATTERNS
        .iter()
        .find(|pattern| normalized.contains(pattern.needle))
        .map(|pattern| pricing_info(pattern.pricing, ModelPricingSource::PatternMatch))
}

fn pricing_info(definition: PricingDefinition, source: ModelPricingSource) -> ModelPricingInfo {
    ModelPricingInfo {
        currency: "USD".to_string(),
        unit: ModelPricingUnit::PerMillionTokens,
        input: Some(ModelTokenPrice::from_micros(definition.input)),
        cached_input: definition.cached_input.map(ModelTokenPrice::from_micros),
        cache_write_input: None,
        output: Some(ModelTokenPrice::from_micros(definition.output)),
        source,
    }
}

fn pricing_from_provider_api(metadata: &BTreeMap<String, Value>) -> Option<ModelPricingInfo> {
    let input = first_u64(metadata, &["input_price_micros", "inputPriceMicros"])
        .map(ModelTokenPrice::from_micros);
    let output = first_u64(metadata, &["output_price_micros", "outputPriceMicros"])
        .map(ModelTokenPrice::from_micros);
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(ModelPricingInfo {
        currency: metadata
            .get("currency")
            .and_then(Value::as_str)
            .unwrap_or("USD")
            .to_string(),
        unit: ModelPricingUnit::PerMillionTokens,
        input,
        cached_input: first_u64(
            metadata,
            &["cached_input_price_micros", "cachedInputPriceMicros"],
        )
        .map(ModelTokenPrice::from_micros),
        cache_write_input: first_u64(
            metadata,
            &[
                "cache_write_input_price_micros",
                "cacheWriteInputPriceMicros",
            ],
        )
        .map(ModelTokenPrice::from_micros),
        output,
        source: ModelPricingSource::ProviderApi,
    })
}
fn metadata_from_provider_api(metadata: &BTreeMap<String, Value>) -> Option<ModelMetadata> {
    let context_window = first_u32(
        metadata,
        &[
            "context_window",
            "contextWindow",
            "max_context_length",
            "maxContextLength",
            "max_input_tokens",
            "maxInputTokens",
        ],
    )?;
    let max_output_tokens = first_u32(
        metadata,
        &[
            "max_output_tokens",
            "maxOutputTokens",
            "max_completion_tokens",
            "maxCompletionTokens",
            "max_tokens",
            "maxTokens",
        ],
    )
    .unwrap_or(DEFAULT_METADATA.max_output_tokens);
    Some(ModelMetadata {
        context_window,
        max_output_tokens,
    })
}

fn first_u32(metadata: &BTreeMap<String, Value>, keys: &[&str]) -> Option<u32> {
    keys.iter()
        .filter_map(|key| metadata.get(*key))
        .find_map(value_to_u32)
        .filter(|value| *value > 0)
}

fn first_u64(metadata: &BTreeMap<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .filter_map(|key| metadata.get(*key))
        .find_map(value_to_u64)
        .filter(|value| *value > 0)
}

fn value_to_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse::<u64>().ok()))
}

fn value_to_u32(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .or_else(|| value.as_str().and_then(|value| value.parse::<u32>().ok()))
}

fn normalize_model_id(model_id: &str) -> String {
    model_id.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_openai_model_context_windows_are_populated() {
        assert_eq!(
            resolve("gpt-4.1-mini", &BTreeMap::new())
                .metadata
                .context_window,
            1_047_576
        );
        assert_eq!(
            resolve("gpt-4o", &BTreeMap::new()).metadata.context_window,
            128_000
        );
        assert_eq!(
            resolve("o1-preview", &BTreeMap::new())
                .metadata
                .context_window,
            200_000
        );
        assert_eq!(
            resolve("gpt-5.5-fast", &BTreeMap::new())
                .metadata
                .context_window,
            272_000
        );
        assert_eq!(
            resolve("grok-4.3", &BTreeMap::new())
                .metadata
                .context_window,
            256_000
        );
    }

    #[test]
    fn modern_or_future_family_names_get_sane_catalog_values() {
        assert_eq!(
            resolve("gpt-5.3-codex-spark", &BTreeMap::new())
                .metadata
                .context_window,
            128_000
        );
        assert_eq!(
            resolve("fable", &BTreeMap::new()).metadata.context_window,
            128_000
        );
        assert_eq!(
            resolve("openrouter/fable-beta", &BTreeMap::new())
                .metadata
                .context_window,
            128_000
        );
    }

    #[test]
    fn provider_api_metadata_wins() {
        let mut metadata = BTreeMap::new();
        metadata.insert("context_window".to_string(), Value::from(65_536));
        metadata.insert("max_output_tokens".to_string(), Value::from(4_096));

        let resolved = resolve("gpt-4.1", &metadata);

        assert_eq!(resolved.metadata.context_window, 65_536);
        assert_eq!(resolved.metadata.max_output_tokens, 4_096);
        assert_eq!(resolved.source, ModelMetadataSource::ProviderApi);
    }

    #[test]
    fn provider_api_pricing_wins() {
        let mut metadata = BTreeMap::new();
        metadata.insert("input_price_micros".to_string(), Value::from(12_345));
        metadata.insert("output_price_micros".to_string(), Value::from(67_890));

        let pricing = pricing_for("gpt-5", &metadata).expect("pricing should resolve");

        assert_eq!(pricing.source, ModelPricingSource::ProviderApi);
        assert_eq!(pricing.input.map(|price| price.micros), Some(12_345));
        assert_eq!(pricing.output.map(|price| price.micros), Some(67_890));
    }

    #[test]
    fn bundled_pricing_resolves_known_models() {
        let pricing =
            pricing_for("gpt-5.3-codex-spark", &BTreeMap::new()).expect("pricing should resolve");

        assert_eq!(pricing.source, ModelPricingSource::PatternMatch);
        assert_eq!(pricing.input.map(|price| price.micros), Some(1_750_000));
        assert_eq!(
            pricing.cached_input.map(|price| price.micros),
            Some(175_000)
        );
        assert_eq!(pricing.output.map(|price| price.micros), Some(14_000_000));
    }

    #[test]
    fn unknown_openai_compatible_models_get_sane_defaults() {
        let resolved = resolve("custom-proxy-model", &BTreeMap::new());

        assert_eq!(resolved.metadata, DEFAULT_METADATA);
        assert_eq!(resolved.source, ModelMetadataSource::ProviderDefault);
    }
}
