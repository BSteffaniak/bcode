//! Built-in model metadata for Amazon Bedrock models.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelMetadata {
    pub context_window: u32,
    pub max_output_tokens: u32,
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

const CLAUDE_METADATA: ModelMetadata = ModelMetadata {
    context_window: 200_000,
    max_output_tokens: 64_000,
};

const NOVA_METADATA: ModelMetadata = ModelMetadata {
    context_window: 300_000,
    max_output_tokens: 10_000,
};

const LLAMA_METADATA: ModelMetadata = ModelMetadata {
    context_window: 128_000,
    max_output_tokens: 8_192,
};

const MISTRAL_LARGE_METADATA: ModelMetadata = ModelMetadata {
    context_window: 128_000,
    max_output_tokens: 8_192,
};

const MISTRAL_METADATA: ModelMetadata = ModelMetadata {
    context_window: 32_000,
    max_output_tokens: 8_192,
};

const TITAN_METADATA: ModelMetadata = ModelMetadata {
    context_window: 8_000,
    max_output_tokens: 4_096,
};

const COMMAND_R_METADATA: ModelMetadata = ModelMetadata {
    context_window: 128_000,
    max_output_tokens: 4_096,
};

const COMMAND_METADATA: ModelMetadata = ModelMetadata {
    context_window: 4_000,
    max_output_tokens: 4_000,
};

/// Ordered from most-specific to least-specific.
const PATTERNS: &[CatalogPattern] = &[
    CatalogPattern {
        needle: "claude-opus-4",
        metadata: CLAUDE_METADATA,
    },
    CatalogPattern {
        needle: "claude-sonnet-4",
        metadata: CLAUDE_METADATA,
    },
    CatalogPattern {
        needle: "claude-haiku-4",
        metadata: CLAUDE_METADATA,
    },
    CatalogPattern {
        needle: "claude-4",
        metadata: CLAUDE_METADATA,
    },
    CatalogPattern {
        needle: "claude-3-7",
        metadata: CLAUDE_METADATA,
    },
    CatalogPattern {
        needle: "claude-3-5",
        metadata: CLAUDE_METADATA,
    },
    CatalogPattern {
        needle: "claude-3-haiku",
        metadata: CLAUDE_METADATA,
    },
    CatalogPattern {
        needle: "claude-3-opus",
        metadata: CLAUDE_METADATA,
    },
    CatalogPattern {
        needle: "claude-3-sonnet",
        metadata: CLAUDE_METADATA,
    },
    CatalogPattern {
        needle: "claude",
        metadata: CLAUDE_METADATA,
    },
    CatalogPattern {
        needle: "nova",
        metadata: NOVA_METADATA,
    },
    CatalogPattern {
        needle: "llama3",
        metadata: LLAMA_METADATA,
    },
    CatalogPattern {
        needle: "llama-3",
        metadata: LLAMA_METADATA,
    },
    CatalogPattern {
        needle: "llama4",
        metadata: LLAMA_METADATA,
    },
    CatalogPattern {
        needle: "llama-4",
        metadata: LLAMA_METADATA,
    },
    CatalogPattern {
        needle: "mistral-large",
        metadata: MISTRAL_LARGE_METADATA,
    },
    CatalogPattern {
        needle: "mixtral",
        metadata: MISTRAL_METADATA,
    },
    CatalogPattern {
        needle: "mistral",
        metadata: MISTRAL_METADATA,
    },
    CatalogPattern {
        needle: "titan",
        metadata: TITAN_METADATA,
    },
    CatalogPattern {
        needle: "command-r",
        metadata: COMMAND_R_METADATA,
    },
    CatalogPattern {
        needle: "command",
        metadata: COMMAND_METADATA,
    },
];

#[must_use]
pub fn metadata_for(model_id: &str) -> ModelMetadata {
    let normalized = normalize_model_id(model_id);
    PATTERNS
        .iter()
        .find(|pattern| normalized.contains(pattern.needle))
        .map_or(DEFAULT_METADATA, |pattern| pattern.metadata)
}

fn normalize_model_id(model_id: &str) -> String {
    let mut id = model_id.trim().to_ascii_lowercase();
    for prefix in ["us.", "eu.", "apac.", "us-gov."] {
        if let Some(stripped) = id.strip_prefix(prefix) {
            id = stripped.to_string();
            break;
        }
    }
    if let Some((base, _version)) = id.rsplit_once(':') {
        id = base.to_string();
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_bedrock_model_context_windows_are_populated() {
        assert_eq!(
            metadata_for("us.anthropic.claude-3-5-sonnet-20241022-v2:0").context_window,
            200_000
        );
        assert_eq!(metadata_for("amazon.nova-pro-v1:0").context_window, 300_000);
        assert_eq!(
            metadata_for("meta.llama3-1-70b-instruct-v1:0").context_window,
            128_000
        );
    }

    #[test]
    fn modern_or_future_family_names_get_sane_catalog_values() {
        assert_eq!(
            metadata_for("us.anthropic.claude-opus-4-8-20270301-v1:0").context_window,
            200_000
        );
        assert_eq!(
            metadata_for("anthropic.claude-fable-4-20270301-v1:0").context_window,
            200_000
        );
        assert_eq!(
            metadata_for("meta.llama4-scout-v1:0").context_window,
            128_000
        );
    }

    #[test]
    fn unknown_bedrock_models_get_sane_defaults() {
        let metadata = metadata_for("provider.future-model-v1:0");

        assert_eq!(metadata, DEFAULT_METADATA);
    }
}
