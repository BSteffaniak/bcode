//! Built-in model metadata for OpenAI-compatible providers.

use bcode_model::ModelMetadataSource;
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

/// Ordered from most-specific to least-specific.
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
    fn unknown_openai_compatible_models_get_sane_defaults() {
        let resolved = resolve("custom-proxy-model", &BTreeMap::new());

        assert_eq!(resolved.metadata, DEFAULT_METADATA);
        assert_eq!(resolved.source, ModelMetadataSource::ProviderDefault);
    }
}
