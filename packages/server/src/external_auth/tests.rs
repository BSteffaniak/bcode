use crate::{ServerState, external_context_for_dispatch, invoke_model_provider_start_turn_at_home};
use bcode_model::{ModelTurnRequest, OP_START_TURN, StartTurnResponse};
use bcode_plugin::PluginInvocationScope;
use bcode_plugin_sdk::{
    AuthRegistrar, NativeServiceContext, PluginError, RustPlugin, ServiceResponse,
};
use bcode_provider_auth::external_auth::{ExternalAuthStore, SourceRoots, discover};
use bcode_provider_auth_models::external::{
    AuthExternalCredential, AuthExternalLocation, AuthExternalSource, AuthLiteralTokenPolicy,
};
use bcode_provider_auth_models::{
    AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION, AuthMethodContribution, AuthProviderContribution,
    AuthSecretField, AuthSecretValidation,
};
use bcode_session::SessionManager;
use bcode_session_models::SessionId;
use std::collections::{BTreeMap, BTreeSet};

const PLUGIN: &str = "test.external-dispatch";
const MANIFEST: &str = r#"
id = "test.external-dispatch"
name = "Synthetic External Dispatch"
version = "0.0.1"
[[services]]
interface_id = "bcode.model-provider/v1"
name = "Synthetic Provider"
[concurrency]
type = "exclusive"
[runtime]
type = "native"
abi_version = 5
library = "test"
"#;

fn declaration() -> AuthProviderContribution {
    AuthProviderContribution {
        schema_version: AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
        provider_id: "synthetic".into(),
        display_name: "Synthetic".into(),
        methods: vec![AuthMethodContribution::SecretFields {
            method_id: "api_key".into(),
            display_name: "Synthetic".into(),
            supports_verification: false,
            supports_revocation: false,
            fields: vec![AuthSecretField {
                credential_id: "api_key".into(),
                prompt: "Key".into(),
                storage_key: "KEY".into(),
                optional: false,
                invocation_env: vec![],
                validation: AuthSecretValidation::default(),
                discovery_sources: vec![],
            }],
        }],
        external_sources: vec![AuthExternalSource {
            source_id: "synthetic".into(),
            application: "Synthetic".into(),
            method_id: "api_key".into(),
            location: AuthExternalLocation {
                relative_path: ".synthetic/auth.json".into(),
                root_override: None,
            },
            discriminator: Some(("/type".into(), "api".into())),
            required_absent_pointers: vec![],
            credentials: vec![AuthExternalCredential {
                credential_id: "api_key".into(),
                pointer: "/key".into(),
                optional: false,
            }],
            account_pointer: None,
            literal_policy: AuthLiteralTokenPolicy {
                reject_command_prefix: false,
                reject_dollar_expansions: false,
                reject_whitespace_or_control: true,
                reject_environment_identifier: false,
            },
            expiry: None,
        }],
    }
}

#[derive(Default)]
struct SyntheticProvider;
impl RustPlugin for SyntheticProvider {
    fn register_auth_providers(&mut self, registrar: AuthRegistrar) -> Result<(), PluginError> {
        registrar
            .register(&declaration())
            .map_err(|_| PluginError::failed("synthetic declaration rejected"))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.operation.as_str() {
            bcode_model::OP_STARTUP_MODEL_CHOICES => {
                ServiceResponse::json(&bcode_model::StartupModelChoicesResponse {
                    choices: vec![bcode_model::StartupRecommendation {
                        provider_plugin_id: PLUGIN.into(),
                        model_id: "synthetic-model".into(),
                        settings: BTreeMap::from([("transport".into(), "synthetic".into())]),
                    }],
                    routing_settings: ["transport", "base_url", "endpoint", "api_key"]
                        .map(Into::into)
                        .to_vec(),
                })
                .unwrap()
            }
            OP_START_TURN => {
                let request: ModelTurnRequest = context.request.payload_json().unwrap();
                assert_eq!(request.provider_context.settings["transport"], "synthetic");
                let auth = request.provider_context.auth.unwrap();
                // Return the synthetic key as evidence of the exact bytes received by the provider.
                ServiceResponse::json(&StartTurnResponse {
                    provider_turn_id: auth.credentials["api_key"].value.clone(),
                })
                .unwrap()
            }
            _ => ServiceResponse::error("unsupported", "unsupported"),
        }
    }
}

fn roots(home: &std::path::Path) -> SourceRoots {
    SourceRoots {
        home: home.to_owned(),
        overrides: BTreeMap::new(),
    }
}

struct Fixture {
    home: tempfile::TempDir,
    _state: tempfile::TempDir,
    server: ServerState,
    store: ExternalAuthStore,
    request: ModelTurnRequest,
    candidate: bcode_provider_auth_models::external::ExternalAuthCandidate,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".synthetic")).unwrap();
        std::fs::write(
            home.path().join(".synthetic/auth.json"),
            r#"{"type":"api","key":"synthetic-first"}"#,
        )
        .unwrap();
        let plugin = bcode_plugin::StaticBundledPlugin::new(
            MANIFEST,
            bcode_plugin_sdk::static_plugin_vtable!(SyntheticProvider, MANIFEST),
        );
        let plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection {
                mode: bcode_plugin::PluginSelectionMode::Explicit,
                enabled: BTreeSet::from([PLUGIN.into()]),
                disabled: BTreeSet::new(),
            },
            &[plugin],
        )
        .unwrap();
        let mut server =
            crate::tests::test_server_state_with_plugins(SessionManager::default(), plugins);
        server.state_root = state.path().to_path_buf();
        let registered = server
            .plugins
            .auth_provider_registry()
            .providers()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let enabled = registered
            .iter()
            .map(|entry| (entry.plugin_id.as_str(), &entry.contribution))
            .collect::<Vec<_>>();
        let candidate = discover(&roots(home.path()), &enabled).unwrap().remove(0);
        let store = ExternalAuthStore::new(state.path());
        let request = serde_json::from_value(serde_json::json!({
            "session_id": SessionId::new(), "turn_id": "synthetic", "model_id": "synthetic-model",
            "messages": [], "provider_context": {
                "settings": { "transport": "synthetic" },
                "external_auth_binding": candidate.binding_id
            }
        }))
        .unwrap();
        Self {
            home,
            _state: state,
            server,
            store,
            request,
            candidate,
        }
    }

    fn approve(&self) -> bcode_provider_auth_models::external::ExternalAuthGrant {
        let declaration = declaration();
        self.store
            .approve(
                &roots(self.home.path()),
                &[(PLUGIN, &declaration)],
                &self.candidate,
                1,
            )
            .unwrap()
    }

    async fn dispatch(&self, request: &ModelTurnRequest) -> Result<StartTurnResponse, String> {
        invoke_model_provider_start_turn_at_home(
            &self.server,
            Some(PLUGIN.into()),
            request,
            PluginInvocationScope::Global,
            &bcode_config::BcodeConfig::default(),
            None,
            Some(self.home.path()),
        )
        .await
    }
}

#[tokio::test]
async fn approval_rotation_revocation_reach_real_dispatch_without_retaining_credentials() {
    let fixture = Fixture::new();
    assert!(fixture.dispatch(&fixture.request).await.is_err());
    let grant = fixture.approve();
    assert_eq!(
        fixture
            .dispatch(&fixture.request)
            .await
            .unwrap()
            .provider_turn_id,
        "synthetic-first"
    );
    std::fs::write(
        fixture.home.path().join(".synthetic/auth.json"),
        r#"{"type":"api","key":"synthetic-rotated"}"#,
    )
    .unwrap();
    assert_eq!(
        fixture
            .dispatch(&fixture.request)
            .await
            .unwrap()
            .provider_turn_id,
        "synthetic-rotated"
    );
    assert!(fixture.request.provider_context.auth.is_none());
    assert!(fixture.request.provider_context.env.is_empty());
    assert!(fixture.store.revoke(&grant.binding_id).unwrap());
    assert!(fixture.dispatch(&fixture.request).await.is_err());
    drop(fixture);
}

#[tokio::test]
async fn injected_credentials_overrides_and_custom_destinations_never_fall_back() {
    let fixture = Fixture::new();
    fixture.approve();
    for key in ["base_url", "endpoint", "api_key"] {
        let mut request = fixture.request.clone();
        request
            .provider_context
            .settings
            .insert(key.into(), "https://untrusted.invalid".into());
        assert!(fixture.dispatch(&request).await.is_err());
    }
    // A setting the provider does not declare as routing is free to differ.
    let mut request = fixture.request.clone();
    request
        .provider_context
        .settings
        .insert("reasoning_summary".into(), "detailed".into());
    assert_eq!(
        fixture.dispatch(&request).await.unwrap().provider_turn_id,
        "synthetic-first"
    );
    let mut request = fixture.request.clone();
    request
        .provider_context
        .env
        .insert("KEY".into(), "client-secret".into());
    assert!(fixture.dispatch(&request).await.is_err());
    let mut request = fixture.request.clone();
    request.provider_context.auth_profile = Some("fallback".into());
    assert!(fixture.dispatch(&request).await.is_err());
    let mut request = fixture.request.clone();
    request.model_id = "unapproved-model".into();
    assert!(fixture.dispatch(&request).await.is_err());
    let mut request = fixture.request.clone();
    let material = external_context_for_dispatch(
        &fixture.server,
        Some(PLUGIN),
        &request,
        Some(fixture.home.path()),
    )
    .await
    .unwrap()
    .unwrap();
    request.provider_context.auth = material.auth;
    assert!(fixture.dispatch(&request).await.is_err());
    assert!(
        external_context_for_dispatch(
            &fixture.server,
            None,
            &fixture.request,
            Some(fixture.home.path())
        )
        .await
        .is_err()
    );
    drop(fixture);
}

#[tokio::test]
async fn session_connection_selection_persists_restores_and_fails_closed() {
    let fixture = Fixture::new();
    let binding = fixture.approve().binding_id;
    let session = fixture
        .server
        .sessions
        .create_session(None, fixture.home.path().to_path_buf())
        .await
        .unwrap();
    // A model the provider does not recommend for the approved method changes nothing.
    assert!(matches!(
        crate::session_operations::set_model(
            &fixture.server,
            session.id,
            Some(PLUGIN.into()),
            "unapproved-model".into(),
            Some(binding.clone()),
        )
        .await,
        Err(
            crate::session_operations::SetModelError::ExternalConnection(
                bcode_provider_auth_models::external::ExternalAuthError::DestinationRejected
            )
        )
    ));
    assert_eq!(
        fixture
            .server
            .sessions
            .current_runtime_selection(session.id)
            .await
            .unwrap()
            .external_auth_binding,
        None
    );
    crate::session_operations::set_model(
        &fixture.server,
        session.id,
        Some(PLUGIN.into()),
        "synthetic-model".into(),
        Some(binding.clone()),
    )
    .await
    .unwrap();
    let expected_settings = BTreeMap::from([("transport".into(), "synthetic".into())]);
    let selected = crate::session_model_selection(&fixture.server, session.id).await;
    assert_eq!(
        selected.provider_context.external_auth_binding.as_deref(),
        Some(binding.as_str())
    );
    assert_eq!(selected.provider_context.settings, expected_settings);
    assert!(!selected.native_default);
    // Restoring from the persisted selection, as after a daemon restart, rebuilds the context.
    fixture.server.session_model_selections.lock().await.clear();
    let restored = crate::session_model_selection(&fixture.server, session.id).await;
    assert_eq!(restored.provider_context, selected.provider_context);
    // After revocation the session keeps the connection without routing, so dispatch refuses
    // it instead of switching to another account.
    assert!(fixture.store.revoke(&binding).unwrap());
    fixture.server.session_model_selections.lock().await.clear();
    let revoked = crate::session_model_selection(&fixture.server, session.id).await;
    assert_eq!(
        revoked.provider_context.external_auth_binding.as_deref(),
        Some(binding.as_str())
    );
    assert!(revoked.provider_context.settings.is_empty());
    assert!(revoked.provider_context.auth.is_none());
    drop(fixture);
}
