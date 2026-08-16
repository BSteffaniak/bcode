#![cfg(feature = "config")]

use bcode::{
    Bcode, BcodeError, ModelProviderInvoker, ModelSelector, ProviderRegistry, RuntimeError,
    RuntimeFuture,
};
use bcode_config::{BcodeConfig, ConfigEnvironmentSnapshot};
use bcode_model::{
    AckResponse, CancelTurnRequest, CapabilityExecution, CapabilityFidelity, CapabilityMechanism,
    CapabilitySource, CapabilitySupport, FinishTurnRequest, ModelCapability, ModelCatalogHints,
    ModelFeatureSupport, ModelInfo, ModelList, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderCapabilities, ProviderCapability, StartTurnResponse,
    StructuredOutputMode, ToolChoiceMode,
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
            CapabilitySupport::supported(CapabilitySource::Configuration),
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
        max_image_input_base64_bytes: None,
        api_surface: None,
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
    assert_eq!(negotiated.provider, Some(true));
    assert_eq!(negotiated.model, Some(true));
    assert!(negotiated.runtime);

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
    assert_eq!(legacy_parallel.provider, None);
    assert_eq!(legacy_parallel.model, None);

    let without_provider = ProviderRegistry::new().provider_models(
        "example.provider",
        ModelList {
            models: vec![model],
            catalog: ModelCatalogHints::default(),
        },
    );
    assert_eq!(
        without_provider
            .parallel_tool_capabilities(&selector)
            .provider,
        None
    );

    let without_model = ProviderRegistry::new().provider_capabilities(capabilities);
    assert_eq!(
        without_model.parallel_tool_capabilities(&selector).model,
        None
    );
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
                    max_image_input_base64_bytes: None,
                    api_surface: None,
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

#[derive(Debug, Default)]
struct UnexpectedProvider;

impl ModelProviderInvoker for UnexpectedProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        Box::pin(async {
            Err(RuntimeError::HostExtension(
                "provider reached after successful capability admission".to_string(),
            ))
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        unreachable!()
    }

    fn cancel_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a CancelTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        unreachable!()
    }

    fn finish_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a FinishTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        unreachable!()
    }
}

#[tokio::test]
async fn registry_agents_fail_closed_for_untrusted_structured_output_capabilities() {
    let selector = ModelSelector::with_provider("example.provider", "example-model");
    let capability = |support: CapabilitySupport| {
        let mut features = ModelFeatureSupport::default();
        features
            .structured_output
            .insert(StructuredOutputMode::StrictJsonSchema, support);
        features
    };
    let provider_capabilities = |feature_support| ProviderCapabilities {
        provider_id: "example.provider".to_string(),
        display_name: "Example".to_string(),
        capabilities: BTreeSet::new(),
        feature_support,
        auth_schemes: BTreeSet::new(),
        retry_rules: Vec::new(),
        metadata: Default::default(),
    };
    let model = |feature_support| ModelInfo {
        model_id: "example-model".to_string(),
        display_name: "Example model".to_string(),
        is_default: true,
        context_window: None,
        max_output_tokens: None,
        max_image_input_base64_bytes: None,
        api_surface: None,
        capabilities: BTreeSet::new(),
        feature_support,
        reasoning: None,
        cache: Default::default(),
        metadata_source: None,
        pricing: None,
        visibility: Default::default(),
    };
    let unsupported = CapabilitySupport::Unsupported {
        source: CapabilitySource::ProviderApi,
        reason: "not supported".to_string(),
    };
    let unknown_registry = ProviderRegistry::new()
        .provider_capabilities(provider_capabilities(ModelFeatureSupport::default()))
        .provider_models(
            "example.provider",
            ModelList {
                models: vec![model(ModelFeatureSupport::default())],
                catalog: ModelCatalogHints::default(),
            },
        )
        .default_model(selector.clone());
    let unsupported_registry = ProviderRegistry::new()
        .provider_capabilities(provider_capabilities(capability(unsupported.clone())))
        .provider_models(
            "example.provider",
            ModelList {
                models: vec![model(capability(unsupported))],
                catalog: ModelCatalogHints::default(),
            },
        )
        .default_model(selector.clone());
    let tool_free = CapabilitySupport::Supported {
        source: CapabilitySource::ProviderApi,
        mechanism: CapabilityMechanism::AdapterMediated,
        fidelity: CapabilityFidelity::Reduced,
        execution: CapabilityExecution::ToolFreeProviderRound,
    };
    let guaranteed_registry = ProviderRegistry::new()
        .provider_capabilities(provider_capabilities(capability(tool_free.clone())))
        .provider_models(
            "example.provider",
            ModelList {
                models: vec![model(capability(tool_free))],
                catalog: ModelCatalogHints::default(),
            },
        )
        .default_model(selector);

    for registry in [unknown_registry, unsupported_registry] {
        let agent = Bcode::builder()
            .provider_registry(registry)
            .build()
            .agent()
            .build();
        let error = agent
            .generate_object_with_provider::<serde_json::Value, _>(
                &mut UnexpectedProvider,
                "produce output",
            )
            .await
            .expect_err("unknown and unsupported claims must fail before provider work");
        assert!(matches!(error, BcodeError::StructuredOutput(_)));
    }

    let agent = Bcode::builder()
        .provider_registry(guaranteed_registry)
        .build()
        .agent()
        .build();
    let request = agent
        .generate_object_with_provider::<serde_json::Value, _>(
            &mut UnexpectedProvider,
            "produce output",
        )
        .await
        .expect_err("guaranteed request should reach the intentionally unavailable provider");
    assert!(matches!(request, BcodeError::Runtime(_)));
    assert_eq!(
        agent.selection_report().selector,
        ModelSelector::with_provider("example.provider", "example-model")
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
