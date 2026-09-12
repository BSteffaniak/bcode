#![cfg(all(
    feature = "embedded-plugins",
    feature = "static-bundled-fake-provider-plugin"
))]

use bcode::{Bcode, ModelSelector, ToolExecutionOptions};
use bcode_model::{ModelCapability, ProviderCapability, StopReason};
use bcode_plugin::{PluginRuntimeHost, PluginSelection};
use std::num::NonZeroUsize;

#[cfg(all(feature = "config", unix))]
#[tokio::test]
async fn retained_custody_refreshes_through_real_plugin_bridge() {
    use bcode_provider_auth::{
        AuthProfileSource, ResolvedAuthProfile,
        lifecycle::AuthVaultLifecycle,
        operations::{AuthCustodyKeySource, AuthRequestCustody as _, RetainedAuthRequestCustody},
    };
    use std::{collections::BTreeMap, sync::Arc};
    struct Keys;
    impl AuthCustodyKeySource for Keys {
        fn generate(
            &self,
        ) -> Result<
            zeroize::Zeroizing<[u8; 32]>,
            bcode_provider_auth::lifecycle::AuthVaultLifecycleError,
        > {
            #[cfg(feature = "custody-simulation")]
            return Ok(zeroize::Zeroizing::new([7; 32]));
            #[cfg(not(feature = "custody-simulation"))]
            Ok(sshenv_vault::crypto::generate_data_key()
                .as_slice()
                .try_into()
                .map(zeroize::Zeroizing::new)
                .unwrap())
        }
    }
    let plugins = PluginRuntimeHost::load_defaults_with_static_bundled(
        &PluginSelection::all_enabled(),
        &bcode_bundled_plugins::static_bundled_plugins(),
    )
    .unwrap();
    let method = plugins.auth_provider("fake").unwrap().contribution.methods[0].clone();
    let root = tempfile::tempdir().unwrap();
    let (public, identity) = custody_test_identity(root.path());
    let profile = bcode_config::AuthProfileConfig {
        backend: "sshenv".into(),
        provider_id: Some("fake".into()),
        owner_plugin_id: Some("bcode.fake-provider".into()),
        scheme: Some("fake_token".into()),
        settings: BTreeMap::from([
            ("profile".into(), "selected".into()),
            (
                "vault".into(),
                root.path().join("unused").display().to_string(),
            ),
            ("device_seal".into(), "off".into()),
        ]),
        ..Default::default()
    };
    let resolved = ResolvedAuthProfile {
        profile_name: "selected".into(),
        provider_id: "fake".into(),
        owner_plugin_id: "bcode.fake-provider".into(),
        profile: profile.clone(),
        source: AuthProfileSource::Declarative,
    };
    let bytes = AuthVaultLifecycle::new(&resolved, "fake", "bcode.fake-provider", &method)
        .unwrap()
        .prepare_custody(&public, &Keys)
        .unwrap();
    let storage = custody_test_storage(root.path(), &bytes);
    let custody = Arc::new(
        RetainedAuthRequestCustody::from_storage(
            storage,
            resolved,
            "fake",
            "bcode.fake-provider",
            method,
            vec![zeroize::Zeroizing::new(identity)],
            None,
        )
        .unwrap()
        .key_source(Arc::new(Keys)),
    );
    let mut config = bcode_config::BcodeConfig::default();
    config.auth.profiles.insert("selected".into(), profile);
    let invoker = bcode::PluginModelProviderInvoker::new(plugins)
        .auth_inputs(config, Default::default())
        .retained_custody(custody.clone());
    let sdk = Bcode::builder()
        .provider_invoker(invoker)
        .provider("bcode.fake-provider")
        .default_model(ModelSelector::with_provider(
            "bcode.fake-provider",
            "fake-echo",
        ))
        .build();
    let context = bcode_model::ProviderRequestContext {
        auth_profile: Some("selected".into()),
        settings: BTreeMap::from([(
            "fake_persist_refreshed_access_token".into(),
            "refreshed".into(),
        )]),
        ..Default::default()
    };
    let response = sdk
        .agent()
        .provider_context(context.clone())
        .build()
        .generate_text("hello")
        .await
        .unwrap();
    assert_eq!(response.runtime.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(
        custody
            .materialize("bcode.fake-provider", &context)
            .unwrap()
            .env["FAKE_ACCESS_TOKEN"],
        "refreshed"
    );
    assert!(!root.path().join("unused").exists());
}

#[cfg(all(feature = "config", unix))]
fn custody_test_identity(root: &std::path::Path) -> (String, String) {
    #[cfg(feature = "custody-simulation")]
    {
        let _ = root;
        ("sim-age:owned".into(), "sim-age:owned".into())
    }
    #[cfg(not(feature = "custody-simulation"))]
    {
        let path = root.join("identity");
        let public = bcode_provider_auth::security::ensure_vault_recipient_key(&path).unwrap();
        let private =
            std::fs::read_to_string(bcode_provider_auth::security::vault_private_key_path(&path))
                .unwrap();
        (public, private)
    }
}

#[cfg(all(feature = "config", unix))]
fn custody_test_storage(
    root: &std::path::Path,
    bytes: &[u8],
) -> Box<dyn bcode_provider_auth::custody_storage::AuthCustodyStorage> {
    #[cfg(feature = "custody-simulation")]
    {
        let _ = root;
        bcode_provider_auth::custody_storage::simulation::CustodySimulation::create(bytes)
            .unwrap()
            .1
    }
    #[cfg(not(feature = "custody-simulation"))]
    {
        Box::new(
            bcode_provider_auth::custody_storage::CredentialCustodyStorage::create(
                &root.join("custody"),
                bytes,
            )
            .unwrap(),
        )
    }
}

fn fake_bcode() -> Bcode {
    let plugins = PluginRuntimeHost::load_defaults_with_static_bundled(
        &PluginSelection::all_enabled(),
        &bcode_bundled_plugins::static_bundled_plugins(),
    )
    .expect("load static fake provider");
    Bcode::builder()
        .plugin_runtime(plugins)
        .provider("bcode.fake-provider")
        .default_model(ModelSelector::with_provider(
            "bcode.fake-provider",
            "fake-echo",
        ))
        .build()
}

#[tokio::test]
async fn embedded_provider_discovery_reports_registration_and_model_provenance() {
    let bcode = fake_bcode()
        .discover_provider(
            "bcode.fake-provider",
            bcode::ProviderRequestContext::default(),
            Some("fake-echo".to_string()),
        )
        .await
        .expect("discover fake provider");
    let report = bcode.provider_registry().selection_report(
        ModelSelector::with_provider("bcode.fake-provider", "fake-echo"),
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
        Some(bcode::ModelMetadataSource::BundledCatalog)
    );
}

#[tokio::test]
async fn static_provider_adapter_conforms_for_multiple_calls_and_sequential_fallback() {
    let bcode = fake_bcode();
    let capabilities = bcode
        .provider_capabilities("bcode.fake-provider")
        .await
        .expect("provider capabilities");
    assert!(
        capabilities
            .capabilities
            .contains(&ProviderCapability::ParallelToolCalls)
    );
    let models = bcode
        .provider_models("bcode.fake-provider")
        .await
        .expect("provider models");
    assert!(
        models.models[0]
            .capabilities
            .contains(&ModelCapability::ParallelToolCalls)
    );

    let parallel = bcode
        .agent()
        .provider_context(bcode_model::ProviderRequestContext {
            settings: [("fake_parallel_tool_calls".to_owned(), "2".to_owned())]
                .into_iter()
                .collect(),
            ..Default::default()
        })
        .execution_options(ToolExecutionOptions {
            max_concurrency: NonZeroUsize::new(2),
            ..ToolExecutionOptions::default()
        })
        .inline_tool(bcode_tool_definition("first"), |_| {
            Ok(bcode::ToolInvocationResponse {
                output: "first".to_owned(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                result: None,
            })
        })
        .inline_tool(bcode_tool_definition("second"), |_| {
            Ok(bcode::ToolInvocationResponse {
                output: "second".to_owned(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                result: None,
            })
        })
        .build();
    bcode_fake_provider_plugin::reset_fake_compaction_started();
    let response = parallel
        .generate_text("multiple")
        .await
        .expect("parallel fake provider round");
    let ids = response
        .runtime
        .events
        .iter()
        .filter_map(|event| match event {
            bcode::AgentEvent::ToolCallFinished(call) => Some(call.id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ids, ["fake-call-0", "fake-call-1"]);
    assert!(bcode_fake_provider_plugin::fake_last_parallel_tool_policy());

    let sequential = fake_bcode()
        .agent()
        .provider_context(bcode_model::ProviderRequestContext {
            settings: [("fake_parallel_tool_calls".to_owned(), "2".to_owned())]
                .into_iter()
                .collect(),
            ..Default::default()
        })
        .execution_options(ToolExecutionOptions {
            parallel: false,
            ..ToolExecutionOptions::default()
        })
        .inline_tool(bcode_tool_definition("first"), |_| {
            Ok(bcode::ToolInvocationResponse {
                output: "first".to_owned(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                result: None,
            })
        })
        .inline_tool(bcode_tool_definition("second"), |_| {
            Ok(bcode::ToolInvocationResponse {
                output: "second".to_owned(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                result: None,
            })
        })
        .build();
    bcode_fake_provider_plugin::reset_fake_compaction_started();
    assert_eq!(
        sequential
            .generate_text("sequential")
            .await
            .expect("sequential fallback")
            .runtime
            .stop_reason,
        Some(StopReason::EndTurn)
    );
    assert!(!bcode_fake_provider_plugin::fake_last_parallel_tool_policy());
}

#[tokio::test]
async fn static_provider_adapter_conforms_for_malformed_calls_and_cancellation() {
    let malformed = fake_bcode()
        .agent()
        .provider_context(bcode_model::ProviderRequestContext {
            settings: [("fake_malformed_tool_call".to_owned(), "true".to_owned())]
                .into_iter()
                .collect(),
            ..Default::default()
        })
        .build()
        .generate_text("malformed")
        .await
        .expect_err("malformed provider call must fail");
    assert!(malformed.to_string().contains("malformed tool call"));

    let agent = fake_bcode()
        .agent()
        .provider_context(bcode_model::ProviderRequestContext {
            settings: [("fake_turn_delay_ms".to_owned(), "1000".to_owned())]
                .into_iter()
                .collect(),
            ..Default::default()
        })
        .build();
    let cancellation = bcode::CancellationToken::new();
    let mut stream = agent
        .stream_text_with_cancellation("cancel", cancellation.clone())
        .expect("start cancellable stream");
    cancellation.cancel();
    let mut cancelled = false;
    while let Some(item) = stream.next().await {
        if matches!(
            item,
            bcode::TextStreamItem::Error(bcode::BcodeError::Runtime(
                bcode::RuntimeError::Cancelled
            ))
        ) {
            cancelled = true;
            break;
        }
    }
    assert!(cancelled);
}

fn bcode_tool_definition(name: &str) -> bcode::ToolDefinition {
    bcode::ToolDefinition {
        name: name.to_owned(),
        description: name.to_owned(),
        input_schema: serde_json::json!({"type": "object"}),
    }
}
