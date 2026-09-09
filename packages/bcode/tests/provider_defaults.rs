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
fn owned_store_initializes_sdk_defaults() {
    let root = tempfile::tempdir().unwrap();
    let mut store =
        bcode_provider_auth::store::AuthStore::create(&root.path().join("auth")).unwrap();
    store
        .update(|state| {
            state.subscriptions.pools.insert(
                "pool".into(),
                bcode_config::RuntimeAuthSubscriptionPool {
                    profiles: vec![bcode_config::RuntimeAuthSubscriptionProfile {
                        auth_profile: "owned".into(),
                        storage_profile: "stored".into(),
                        vault: "/not-accessed".into(),
                        provider: "openai".into(),
                        scheme: "api_key".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            );
            Ok(())
        })
        .unwrap();
    let mut config = BcodeConfig::default();
    config.model.auth_pool = Some("pool".into());
    let sdk = Bcode::builder()
        .provider_defaults_from_store(
            &config,
            &ConfigEnvironmentSnapshot::isolated("owned-store"),
            &store,
            |name, _| {
                assert_eq!(name, "owned");
                bcode_provider_auth::ResolvedProviderAuth {
                    auth: bcode_model::ProviderAuthContext::default(),
                    env: std::collections::BTreeMap::new(),
                }
            },
        )
        .unwrap()
        .build();
    assert_eq!(
        sdk.provider_context().auth_profile.as_deref(),
        Some("owned")
    );
}

#[tokio::test]
async fn explicit_sdk_inputs_materialize_auth_pool_and_model_defaults() {
    let mut config = BcodeConfig::default();
    config.model.provider_plugin_id = Some("example.provider".into());
    config.model.model_id = Some("example-model".into());
    config.model.auth_pool = Some("explicit-pool".into());
    let environment = ConfigEnvironmentSnapshot::isolated("explicit-sdk-inputs");
    let subscriptions = bcode_config::RuntimeAuthSubscriptions {
        pools: std::collections::BTreeMap::from([(
            "explicit-pool".into(),
            bcode_config::RuntimeAuthSubscriptionPool {
                preferred_profile: Some("explicit-profile".into()),
                profiles: vec![bcode_config::RuntimeAuthSubscriptionProfile {
                    auth_profile: "explicit-profile".into(),
                    storage_profile: "stored-profile".into(),
                    vault: "/fixture/auth.vault".into(),
                    provider: "openai".into(),
                    scheme: "api_key".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let mut acquisitions = Vec::new();
    let sdk = Bcode::builder().provider_defaults_with_auth_resolver(
        &config,
        &environment,
        &subscriptions,
        |name, profile| {
            acquisitions.push(name.to_owned());
            assert_eq!(name, "explicit-profile");
            assert_eq!(profile.backend, "sshenv");
            bcode_provider_auth::ResolvedProviderAuth {
                auth: bcode_model::ProviderAuthContext {
                    scheme: Some("api_key".into()),
                    ..bcode_model::ProviderAuthContext::default()
                },
                env: std::collections::BTreeMap::new(),
            }
        },
    );
    let sdk = sdk.build();
    assert_eq!(acquisitions, ["explicit-profile"]);
    let context = sdk.provider_context().clone();
    assert_eq!(
        sdk.default_model_selector(),
        Some(&ModelSelector::with_provider(
            "example.provider",
            "example-model"
        ))
    );
    assert_eq!(
        sdk.provider_context().auth_pool.as_deref(),
        Some("explicit-pool")
    );
    assert_eq!(
        sdk.provider_context().auth_profile.as_deref(),
        Some("explicit-profile")
    );
    assert_eq!(sdk.provider_context().auth_candidates.len(), 1);
    let session_id = "00000000-0000-4000-8000-000000000025"
        .parse()
        .expect("fixture ID");
    let agent = sdk
        .agent_from_context(session_id, "/fixture".into())
        .build();
    assert_eq!(agent.provider_context(), sdk.provider_context());
    struct ContextProvider(bcode::ProviderRequestContext);
    impl bcode::InProcessModelProvider for ContextProvider {
        fn run_turn(
            &self,
            request: ModelTurnRequest,
            _context: bcode::InProcessProviderContext,
        ) -> bcode::InProcessProviderFuture<'_> {
            assert_eq!(request.provider_context, self.0);
            assert_eq!(request.model_id, "example-model");
            Box::pin(async { Ok(bcode::InProcessProviderOutcome::EndTurn) })
        }
    }
    let mut provider =
        bcode::InProcessModelProviderAdapter::new(ContextProvider(sdk.provider_context().clone()));
    let response = agent
        .generate_text_with_provider(&mut provider, "context propagation")
        .await
        .expect("explicit context reaches provider execution");
    assert_eq!(
        response.runtime.stop_reason,
        Some(bcode::StopReason::EndTurn)
    );
    provider.shutdown_wait().await.expect("provider released");
    let override_context = bcode::ProviderRequestContext {
        auth_profile: Some("caller-profile".into()),
        ..bcode::ProviderRequestContext::default()
    };
    let overridden = Bcode::builder()
        .provider_defaults_from_config_environment(&config, &environment)
        .provider_context(context)
        .provider_context(override_context.clone())
        .build();
    assert_eq!(overridden.provider_context(), &override_context);
    assert_eq!(
        overridden
            .agent_from_context(session_id, "/fixture".into())
            .build()
            .provider_context(),
        &override_context
    );
    let empty = Bcode::builder()
        .provider_defaults_with_auth_resolver(
            &config,
            &environment,
            &bcode_config::RuntimeAuthSubscriptions::default(),
            |_, _| panic!("empty pool must not acquire credentials"),
        )
        .build();
    assert!(empty.provider_context().auth_candidates.is_empty());
    assert!(empty.provider_context().auth.is_none());
}

#[test]
fn explicit_sdk_inputs_share_one_selection_with_auth_context() {
    struct ChangingEnvironment(std::cell::Cell<usize>);
    impl bcode_config::ConfigEnvironment for ChangingEnvironment {
        fn var(&self, name: &str) -> Option<String> {
            if name != "BCODE_MODEL_PROVIDER" {
                return None;
            }
            let reads = self.0.get();
            self.0.set(reads + 1);
            Some(
                if reads == 0 {
                    "example.provider"
                } else {
                    "other.provider"
                }
                .into(),
            )
        }

        fn var_os(&self, name: &str) -> Option<std::ffi::OsString> {
            self.var(name).map(Into::into)
        }

        fn current_dir(&self) -> std::path::PathBuf {
            "/fixture".into()
        }
    }
    let mut config = BcodeConfig::default();
    config.model.provider_plugin_id = Some("example.provider".into());
    config.model.model_id = Some("example-model".into());
    config.model.auth_profile = Some("selected-profile".into());
    let environment = ChangingEnvironment(std::cell::Cell::new(0));
    let sdk = Bcode::builder()
        .provider_defaults_from_inputs(
            &config,
            &environment,
            &bcode_config::RuntimeAuthSubscriptions::default(),
        )
        .build();
    assert_eq!(environment.0.get(), 1);
    assert_eq!(
        sdk.default_model_selector().unwrap().provider_plugin_id(),
        Some("example.provider")
    );
    assert_eq!(
        sdk.provider_context().auth_profile.as_deref(),
        Some("selected-profile")
    );
}

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
