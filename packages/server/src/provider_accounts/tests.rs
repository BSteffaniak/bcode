//! Offline account-routing regression tests. All credentials and state are temporary fixtures.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use bcode_provider_auth_models::{
    AuthMethodContribution, AuthProviderContribution, AuthSecretField,
};

const PROVIDER: &str = "bcode.test-account-provider";
const MANIFEST: &str = r#"
id = "bcode.test-account-provider"
name = "Account routing fixture"
version = "0.0.1"
[[services]]
interface_id = "bcode.model-provider/v1"
[runtime]
type = "native"
library = "libaccount_fixture.dylib"
abi_version = 5
event_symbol = "bcode_plugin_handle_event_v1"
"#;

fn method() -> AuthMethodContribution {
    AuthMethodContribution::SecretFields {
        method_id: "api_key".into(),
        display_name: "API key".into(),
        fields: vec![AuthSecretField {
            credential_id: "api_key".into(),
            storage_key: "API_KEY".into(),
            prompt: "Key".into(),
            optional: false,
            validation: bcode_provider_auth_models::AuthSecretValidation::default(),
            invocation_env: Vec::new(),
            discovery_sources: Vec::new(),
        }],
        supports_verification: false,
        supports_revocation: false,
    }
}

#[derive(Default)]
struct Provider;

impl bcode_plugin_sdk::RustPlugin for Provider {
    fn register_auth_providers(
        &mut self,
        registrar: bcode_plugin_sdk::AuthRegistrar,
    ) -> Result<(), bcode_plugin_sdk::PluginError> {
        for (id, name) in [("openai", "Sol/OpenAI"), ("opencode-go", "OpenCode Go")] {
            registrar
                .register(&AuthProviderContribution {
                    schema_version:
                        bcode_provider_auth_models::AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
                    provider_id: id.into(),
                    display_name: name.into(),
                    methods: vec![method()],
                    external_sources: Vec::new(),
                })
                .unwrap();
        }
        Ok(())
    }

    fn invoke_service(
        &mut self,
        context: bcode_plugin_sdk::NativeServiceContext,
    ) -> bcode_plugin_sdk::ServiceResponse {
        if context.request.operation == bcode_model::OP_CONTEXT_MANAGEMENT_CAPABILITIES {
            return bcode_plugin_sdk::ServiceResponse::json(
                &bcode_model::ContextManagementCapabilities::default(),
            )
            .unwrap();
        }
        if context.request.operation == bcode_model::OP_START_TURN {
            let request: bcode_model::ModelTurnRequest = context.request.payload_json().unwrap();
            assert_eq!(
                request.provider_context.account_reference.as_deref(),
                Some("go")
            );
            assert_eq!(
                request.provider_context.settings["base_url"],
                "https://go.invalid/v1"
            );
            assert_eq!(
                request.provider_context.request.get("fixture-extension"),
                Some(&bcode_model::ProviderRequestValue::from(
                    serde_json::json!({"keep": true})
                ))
            );
            let auth = request.provider_context.auth.as_ref().unwrap();
            assert_eq!(auth.profile.as_deref(), Some("go"));
            assert_eq!(auth.credentials["api_key"].value, "fixture-go-secret");
            return bcode_plugin_sdk::ServiceResponse::json(&bcode_model::StartTurnResponse {
                provider_turn_id: "account-dispatched".into(),
            })
            .unwrap();
        }
        assert_eq!(
            context.request.operation,
            bcode_model::OP_STARTUP_MODEL_CHOICES
        );
        let request: bcode_model::StartupRecommendationRequest =
            context.request.payload_json().unwrap();
        let (model, endpoint) = match request.auth_provider_id.as_str() {
            "openai" => ("gpt-5", "https://sol.invalid/v1"),
            "opencode-go" => ("glm-5", "https://go.invalid/v1"),
            _ => panic!("unexpected fixture provider"),
        };
        bcode_plugin_sdk::ServiceResponse::json(&bcode_model::StartupModelChoicesResponse {
            choices: vec![bcode_model::StartupRecommendation {
                provider_plugin_id: PROVIDER.into(),
                model_id: model.into(),
                settings: BTreeMap::from([
                    ("base_url".into(), endpoint.into()),
                    ("dialect".into(), "chat_completions".into()),
                    ("catalog_provider_id".into(), request.auth_provider_id),
                ]),
            }],
            routing_settings: vec![
                "base_url".into(),
                "dialect".into(),
                "catalog_provider_id".into(),
            ],
        })
        .unwrap()
    }
}

fn config(root: &std::path::Path) -> bcode_config::BcodeConfig {
    let mut config = bcode_config::BcodeConfig::default();
    for (name, provider) in [("sol", "openai"), ("go", "opencode-go")] {
        config.auth.profiles.insert(
            name.into(),
            bcode_config::AuthProfileConfig {
                backend: "sshenv".into(),
                provider_id: Some(provider.into()),
                owner_plugin_id: Some(PROVIDER.into()),
                scheme: Some("api_key".into()),
                map: BTreeMap::new(),
                settings: BTreeMap::from([
                    ("profile".into(), name.into()),
                    ("vault".into(), root.join(name).display().to_string()),
                    ("device_seal".into(), "off".into()),
                ]),
            },
        );
        let resolved = bcode_provider_auth::ResolvedAuthProfile {
            profile_name: name.into(),
            provider_id: provider.into(),
            owner_plugin_id: PROVIDER.into(),
            profile: config.auth.profiles[name].clone(),
            source: bcode_provider_auth::AuthProfileSource::Declarative,
        };
        bcode_provider_auth::lifecycle::AuthVaultLifecycle::new(
            &resolved,
            provider,
            PROVIDER,
            &method(),
        )
        .unwrap()
        .upsert(BTreeMap::from([(
            "api_key".into(),
            format!("fixture-{name}-secret"),
        )]))
        .unwrap();
    }
    config
}

fn fixture_state(
    sessions: bcode_session::SessionManager,
    config: bcode_config::BcodeConfig,
) -> ServerState {
    let fixture = bcode_plugin::StaticBundledPlugin::new(
        MANIFEST,
        bcode_plugin_sdk::static_plugin_vtable!(Provider, MANIFEST),
    );
    let plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: BTreeSet::from([PROVIDER.into()]),
            disabled: BTreeSet::new(),
        },
        &[fixture],
    )
    .unwrap();
    let mut state = crate::tests::test_server_state_with_plugins(sessions, plugins);
    state.startup_config = config;
    state
}

#[test]
fn oversized_account_sources_fail_explicitly_without_enumerating_candidates() {
    let config = bcode_config::BcodeConfig::default();
    let mut runtime = bcode_config::RuntimeAuthSubscriptions::default();
    for index in 0..65 {
        runtime.profiles.insert(
            format!("account-{index}"),
            bcode_config::RuntimeAuthProfile::default(),
        );
    }
    assert!(
        require_discovery_budget(&config, &runtime)
            .unwrap_err()
            .contains("source limit")
    );
    runtime.profiles.clear();
    let pool = bcode_config::RuntimeAuthSubscriptionPool {
        profiles: (0..65)
            .map(|index| bcode_config::RuntimeAuthSubscriptionProfile {
                auth_profile: format!("member-{index}"),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    runtime.pools.insert("oversized".into(), pool);
    assert!(require_discovery_budget(&config, &runtime).is_err());
}

#[tokio::test]
async fn independent_accounts_validate_materialize_and_resume_without_transient_sol() {
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path());
    let sessions_root = root.path().join("sessions");
    let sessions = bcode_session::SessionManager::persistent(&sessions_root).unwrap();
    let session = sessions
        .create_session(None, root.path().to_path_buf())
        .await
        .unwrap();
    let state = fixture_state(sessions, config.clone());
    let request = assert_account_discovery_and_dispatch(&state, &config, session.id).await;
    state.sessions.shutdown_catalog_updates().await;
    drop(state);
    let sessions = bcode_session::SessionManager::persistent(&sessions_root).unwrap();
    let restored = fixture_state(sessions, config.clone());
    assert_account_resume(&restored, &config, session.id, &request).await;
    Box::pin(assert_account_rejections(&restored, config, session.id)).await;
    restored.sessions.shutdown_catalog_updates().await;
    drop(restored);
}

async fn assert_account_discovery_and_dispatch(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
    session_id: SessionId,
) -> bcode_model::ModelTurnRequest {
    let discovered = discover(state, ClientId::new()).await.unwrap();
    assert_eq!(discovered.len(), 2);
    let public = serde_json::to_string(&discovered).unwrap();
    assert!(!public.contains("fixture-go-secret"));
    assert!(!public.contains("fixture-sol-secret"));
    assert!(!public.contains(config.auth.profiles["go"].settings["vault"].as_str()));
    assert!(
        discovered
            .iter()
            .any(|account| account.account_reference == "go"
                && account.display_name.starts_with("OpenCode Go"))
    );
    let sol = route(state, config, PROVIDER, "sol", Some("gpt-5"))
        .await
        .unwrap();
    let go = route(state, config, PROVIDER, "go", Some("glm-5"))
        .await
        .unwrap();
    assert_ne!(sol.0.settings["base_url"], go.0.settings["base_url"]);
    assert_eq!(go.1.models[0].model_id, "glm-5");
    assert!(go.0.auth.is_none());
    assert!(go.0.env.is_empty());
    set(
        state,
        session_id,
        config,
        Some(PROVIDER.into()),
        "glm-5".into(),
        "go".into(),
    )
    .await
    .unwrap();
    let selected = crate::session_model_selection_with_runtime_context(
        state,
        session_id,
        Some(crate::ClientRuntimeContext {
            selected_provider_plugin_id: Some(PROVIDER.into()),
            selected_model_id: Some("gpt-5".into()),
            provider_context: sol.0.clone(),
            ..Default::default()
        }),
    )
    .await;
    let mut context = selected.provider_context;
    crate::overlay_transient_provider_context(
        &mut context,
        ProviderRequestContext {
            auth_profile: Some("sol".into()),
            settings: sol.0.settings.clone(),
            auth: Some(bcode_model::ProviderAuthContext::default()),
            ..Default::default()
        },
    );
    assert_eq!(context.account_reference.as_deref(), Some("go"));
    assert_eq!(context.settings["base_url"], "https://go.invalid/v1");
    context.request.insert(
        "fixture-extension".into(),
        bcode_model::ProviderRequestValue::from(serde_json::json!({"keep": true})),
    );
    let dispatch = materialize(state, config, PROVIDER, "glm-5", "go")
        .await
        .unwrap();
    assert_eq!(
        dispatch.auth.as_ref().unwrap().profile.as_deref(),
        Some("go")
    );
    assert!(dispatch.auth_pool.is_none());
    assert_eq!(dispatch.auth_candidates.len(), 0);
    let request: bcode_model::ModelTurnRequest = serde_json::from_value(serde_json::json!({
        "session_id": session_id, "turn_id": "account-test", "model_id": "glm-5", "messages": [],
        "provider_context": context,
    }))
    .unwrap();
    let response = crate::invoke_model_provider_start_turn_at_home(
        state,
        Some(PROVIDER.into()),
        &request,
        bcode_plugin::PluginInvocationScope::session(session_id.to_string()),
        config,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(response.provider_turn_id, "account-dispatched");
    request
}

async fn assert_account_resume(
    restored: &ServerState,
    config: &bcode_config::BcodeConfig,
    session_id: SessionId,
    request: &bcode_model::ModelTurnRequest,
) {
    let selected = crate::session_model_selection(restored, session_id).await;
    assert_eq!(
        selected.provider_context.account_reference.as_deref(),
        Some("go")
    );
    assert_eq!(
        selected.provider_context.settings["base_url"],
        "https://go.invalid/v1"
    );
    let status =
        crate::model_status_for_selection(restored, selected.clone(), Some(session_id), config)
            .await;
    assert_eq!(status.account_reference.as_deref(), Some("go"));
    let runtime = crate::session_runtime_selection_payload(restored, session_id).await;
    assert_eq!(runtime.account_reference.as_deref(), Some("go"));
    let mut resumed_request = request.clone();
    resumed_request.provider_context = selected.provider_context.clone();
    resumed_request
        .provider_context
        .request
        .clone_from(&request.provider_context.request);
    let response = crate::invoke_model_provider_start_turn_at_home(
        restored,
        Some(PROVIDER.into()),
        &resumed_request,
        bcode_plugin::PluginInvocationScope::session(session_id.to_string()),
        config,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(response.provider_turn_id, "account-dispatched");
}

async fn assert_account_rejections(
    restored: &ServerState,
    config: bcode_config::BcodeConfig,
    session_id: SessionId,
) {
    assert!(
        validate(restored, &config, PROVIDER, "gpt-5", "go")
            .await
            .is_err()
    );
    assert!(
        validate(restored, &config, "bcode.other", "glm-5", "go")
            .await
            .is_err()
    );
    let mut scoped = config.clone();
    scoped.active_context = Some("isolated".into());
    assert!(
        validate(restored, &scoped, PROVIDER, "glm-5", "go")
            .await
            .is_err()
    );
    let before = restored
        .sessions
        .current_runtime_selection(session_id)
        .await
        .unwrap();
    for (bad_config, provider, model, account) in [
        (&config, PROVIDER, "gpt-5", "go"),
        (&config, "bcode.other", "glm-5", "go"),
        (&scoped, PROVIDER, "glm-5", "go"),
    ] {
        assert!(
            set(
                restored,
                session_id,
                bad_config,
                Some(provider.into()),
                model.into(),
                account.into()
            )
            .await
            .is_err()
        );
        let after = restored
            .sessions
            .current_runtime_selection(session_id)
            .await
            .unwrap();
        assert_eq!(before.account_reference, after.account_reference);
        assert_eq!(before.model_id, after.model_id);
    }
    Box::pin(assert_unavailable_credentials(restored, config, session_id)).await;
}

async fn assert_unavailable_credentials(
    restored: &ServerState,
    config: bcode_config::BcodeConfig,
    session_id: SessionId,
) {
    let owned = bcode_provider_auth::ResolvedAuthProfile {
        profile_name: "go".into(),
        provider_id: "opencode-go".into(),
        owner_plugin_id: PROVIDER.into(),
        profile: config.auth.profiles["go"].clone(),
        source: bcode_provider_auth::AuthProfileSource::Declarative,
    };
    // A present but empty required secret is rejected before model-change publication.
    bcode_provider_auth::lifecycle::AuthVaultLifecycle::new(
        &owned,
        "opencode-go",
        PROVIDER,
        &method(),
    )
    .unwrap()
    .replace_owned(BTreeMap::from([("api_key".into(), String::new())]))
    .unwrap();
    assert!(
        set(
            restored,
            session_id,
            &config,
            Some(PROVIDER.into()),
            "glm-5".into(),
            "go".into()
        )
        .await
        .is_err()
    );
    assert!(
        materialize(restored, &config, PROVIDER, "glm-5", "go")
            .await
            .is_err()
    );
    assert_eq!(
        restored
            .sessions
            .current_runtime_selection(session_id)
            .await
            .unwrap()
            .account_reference
            .as_deref(),
        Some("go")
    );
    let mut removed = config;
    removed.auth.profiles.remove("go");
    assert!(
        materialize(restored, &removed, PROVIDER, "glm-5", "go")
            .await
            .is_err()
    );
    assert!(
        set(
            restored,
            session_id,
            &removed,
            Some(PROVIDER.into()),
            "glm-5".into(),
            "missing".into()
        )
        .await
        .is_err()
    );
    assert_eq!(
        restored
            .sessions
            .current_runtime_selection(session_id)
            .await
            .unwrap()
            .account_reference
            .as_deref(),
        Some("go")
    );
}
