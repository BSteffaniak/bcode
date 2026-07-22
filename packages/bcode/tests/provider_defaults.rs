#![cfg(feature = "config")]

use bcode::{Bcode, BcodeError, ModelSelector, ProviderRegistry};
use bcode_config::{BcodeConfig, ConfigEnvironmentSnapshot};
use bcode_model::{
    CapabilitySource, CapabilitySupport, ModelCapability, ModelCatalogHints, ModelFeatureSupport,
    ModelInfo, ModelList, ProviderCapabilities, ProviderCapability, ToolChoiceMode,
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
        registry
            .provider_registration("example.provider")
            .map(|registration| registration.source),
        Some(bcode::ProviderRegistrationSource::Configuration)
    );
    assert_eq!(
        registry.default_model_selector(),
        Some(&ModelSelector::with_provider(
            "example.provider",
            "example-model"
        ))
    );
    assert_eq!(
        registry.default_selection_provenance(),
        Some(&bcode::ModelSelectionProvenance {
            provider: Some(bcode::ModelSelectionSource::Config),
            model: Some(bcode::ModelSelectionSource::Config),
        })
    );

    let sdk = Bcode::builder()
        .provider_defaults_from_config_environment(&config, &environment)
        .build();
    let agent = sdk.agent().build();
    assert_eq!(
        sdk.default_model_selector(),
        registry.default_model_selector()
    );
    assert_eq!(
        agent.selection_provenance(),
        registry
            .default_selection_provenance()
            .expect("config provenance")
    );
    assert_eq!(
        agent.selection_report(),
        registry
            .default_selection_report()
            .expect("selection report")
    );
    let unqualified = ProviderRegistry::new().default_model("model-only");
    assert_eq!(
        unqualified.default_selection_provenance(),
        Some(&bcode::ModelSelectionProvenance {
            provider: None,
            model: Some(bcode::ModelSelectionSource::ExplicitRegistration),
        })
    );

    let request_override = sdk.agent().model("request-model").build();
    let report = request_override.selection_report();
    assert_eq!(report.selector.model_id(), "request-model");
    assert_eq!(
        report.provenance.model,
        Some(bcode::ModelSelectionSource::PerRequest)
    );
    assert_eq!(report.model_metadata_source, None);
    let provider_override = sdk.agent().provider_plugin("other.provider").build();
    let report = provider_override.selection_report();
    assert_eq!(
        report.provenance.provider,
        Some(bcode::ModelSelectionSource::PerRequest)
    );
    assert_eq!(report.registration_source, None);
    assert_eq!(report.model_metadata_source, None);
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
    assert_eq!(
        registry.default_selection_provenance(),
        Some(&bcode::ModelSelectionProvenance {
            provider: Some(bcode::ModelSelectionSource::Environment {
                variable: "BCODE_MODEL_PROVIDER".to_string(),
            }),
            model: Some(bcode::ModelSelectionSource::Environment {
                variable: "BCODE_OPENAI_MODEL".to_string(),
            }),
        })
    );
}

#[test]
fn provider_registry_negotiates_parallel_only_when_provider_and_model_support_it() {
    let selector = ModelSelector::with_provider("example.provider", "example-model");
    let feature_support = ModelFeatureSupport {
        tool_choice: std::iter::once((
            ToolChoiceMode::Parallel,
            CapabilitySupport::Supported {
                source: CapabilitySource::Configuration,
            },
        ))
        .collect(),
        ..ModelFeatureSupport::default()
    };
    let capabilities = ProviderCapabilities {
        provider_id: "example.provider".to_owned(),
        display_name: "Example".to_owned(),
        capabilities: BTreeSet::from([
            ProviderCapability::Tools,
            ProviderCapability::ParallelToolCalls,
        ]),
        feature_support: feature_support.clone(),
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
        feature_support,
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

    let legacy_capabilities = ProviderCapabilities {
        feature_support: ModelFeatureSupport::default(),
        ..capabilities.clone()
    };
    let legacy_model = ModelInfo {
        feature_support: ModelFeatureSupport::default(),
        ..model.clone()
    };
    let legacy = ProviderRegistry::new()
        .provider_capabilities(legacy_capabilities)
        .provider_models(
            "example.provider",
            ModelList {
                models: vec![legacy_model],
                catalog: ModelCatalogHints::default(),
            },
        );
    let legacy_parallel = legacy.parallel_tool_capabilities(&selector);
    assert!(!legacy_parallel.provider && !legacy_parallel.model);

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
fn selection_report_combines_registration_and_model_discovery_provenance() {
    let selector = ModelSelector::with_provider("discovered.provider", "discovered-model");
    let registry = ProviderRegistry::new()
        .discovered_provider("discovered.provider")
        .provider_models(
            "discovered.provider",
            ModelList {
                models: vec![ModelInfo {
                    model_id: "discovered-model".to_string(),
                    display_name: "Discovered model".to_string(),
                    is_default: true,
                    context_window: None,
                    max_output_tokens: None,
                    capabilities: BTreeSet::new(),
                    feature_support: ModelFeatureSupport::default(),
                    reasoning: None,
                    cache: Default::default(),
                    metadata_source: Some(bcode::ModelMetadataSource::ProviderApi),
                    pricing: None,
                    visibility: Default::default(),
                }],
                catalog: ModelCatalogHints::default(),
            },
        );
    let report = registry.selection_report(
        selector,
        bcode::ModelSelectionProvenance {
            provider: Some(bcode::ModelSelectionSource::ExplicitRegistration),
            model: Some(bcode::ModelSelectionSource::PerRequest),
        },
    );

    assert_eq!(
        report.registration_source,
        Some(bcode::ProviderRegistrationSource::Discovery)
    );
    assert_eq!(
        report.model_metadata_source,
        Some(bcode::ModelMetadataSource::ProviderApi)
    );
    let encoded = serde_json::to_value(&report).expect("report should serialize");
    assert_eq!(
        serde_json::from_value::<bcode::ModelSelectionReport>(encoded)
            .expect("report should deserialize"),
        report
    );
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
