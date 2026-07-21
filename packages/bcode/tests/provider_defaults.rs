#![cfg(feature = "config")]

use bcode::{Bcode, BcodeError, ModelSelector, ProviderRegistry};
use bcode_config::{BcodeConfig, ConfigEnvironmentSnapshot};
use bcode_model::{
    ModelCapability, ModelCatalogHints, ModelInfo, ModelList, ProviderCapabilities,
    ProviderCapability,
};
use std::collections::BTreeSet;

#[test]
fn provider_defaults_resolve_from_explicit_config() {
    let mut config = BcodeConfig::default();
    config.model.provider_plugin_id = Some("example.provider".to_string());
    config.model.model_id = Some("example-model".to_string());
    let environment = ConfigEnvironmentSnapshot::isolated("provider-default-config-test");

    let registry = ProviderRegistry::from_config_environment(&config, &environment);

    assert_eq!(
        registry.provider_ids().collect::<Vec<_>>(),
        ["example.provider"]
    );
    assert_eq!(
        registry.default_model_selector(),
        Some(&ModelSelector::with_provider(
            "example.provider",
            "example-model"
        ))
    );

    let sdk = Bcode::builder()
        .provider_defaults_from_config_environment(&config, &environment)
        .build();
    assert_eq!(
        sdk.default_model_selector(),
        registry.default_model_selector()
    );
}

#[test]
fn environment_provider_and_model_override_config_defaults() {
    let mut config = BcodeConfig::default();
    config.model.provider_plugin_id = Some("bcode.bedrock".to_string());
    config.model.model_id = Some("configured-model".to_string());
    let mut environment = ConfigEnvironmentSnapshot::isolated("provider-default-env-test");
    environment.set_var("BCODE_MODEL_PROVIDER", "openai");
    environment.set_var("BCODE_OPENAI_MODEL", "environment-model");

    let registry = ProviderRegistry::from_config_environment(&config, &environment);

    assert!(
        registry
            .provider_registration("bcode.openai-compatible")
            .is_some()
    );
    assert_eq!(
        registry.default_model_selector(),
        Some(&ModelSelector::with_provider(
            "bcode.openai-compatible",
            "environment-model"
        ))
    );
}

#[test]
fn provider_registry_negotiates_parallel_only_when_provider_and_model_support_it() {
    let selector = ModelSelector::with_provider("example.provider", "example-model");
    let capabilities = ProviderCapabilities {
        provider_id: "example.provider".to_owned(),
        display_name: "Example".to_owned(),
        capabilities: BTreeSet::from([
            ProviderCapability::Tools,
            ProviderCapability::ParallelToolCalls,
        ]),
        auth_schemes: BTreeSet::new(),
        retry_rules: Vec::new(),
        metadata: Default::default(),
    };
    let model = ModelInfo {
        model_id: "example-model".to_owned(),
        display_name: "Example model".to_owned(),
        is_default: true,
        context_window: None,
        max_output_tokens: None,
        capabilities: BTreeSet::from([
            ModelCapability::ToolCalls,
            ModelCapability::ParallelToolCalls,
        ]),
        reasoning: None,
        cache: Default::default(),
        metadata_source: None,
        pricing: None,
        visibility: Default::default(),
    };
    let registry = ProviderRegistry::new()
        .provider_capabilities(capabilities.clone())
        .provider_models(
            "example.provider",
            ModelList {
                models: vec![model.clone()],
                catalog: ModelCatalogHints::default(),
            },
        );
    let negotiated = registry.parallel_tool_capabilities(&selector);
    assert!(negotiated.provider && negotiated.model && negotiated.runtime);

    let without_provider = ProviderRegistry::new().provider_models(
        "example.provider",
        ModelList {
            models: vec![model],
            catalog: ModelCatalogHints::default(),
        },
    );
    assert!(
        !without_provider
            .parallel_tool_capabilities(&selector)
            .provider
    );

    let without_model = ProviderRegistry::new().provider_capabilities(capabilities);
    assert!(!without_model.parallel_tool_capabilities(&selector).model);
}

#[test]
fn provider_setup_errors_include_next_steps() {
    let missing_provider = BcodeError::MissingProvider.to_string();
    assert!(missing_provider.contains("pass a provider"));
    assert!(missing_provider.contains("embedded-plugins"));

    let bad_configuration =
        BcodeError::ProviderConfiguration("connection rejected".to_string()).to_string();
    assert!(bad_configuration.contains("credentials"));
    assert!(bad_configuration.contains("model settings"));
}
