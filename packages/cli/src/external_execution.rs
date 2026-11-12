//! Dispatch-time external authentication for direct CLI model probes.

use std::path::PathBuf;

use bcode_model::ProviderRequestContext;
use bcode_provider_auth::external_auth::{DispatchResolution, ExternalAuthStore, SourceRoots};

/// Reject diagnostics that route through the daemon's generic plugin call instead of dispatch.
pub fn reject_unsupported(context: &ProviderRequestContext, operation: &str) -> Result<(), String> {
    if context.external_auth_binding.is_some() {
        return Err(format!(
            "external connections are not supported for CLI {operation}; use a configured auth profile for this operation"
        ));
    }
    Ok(())
}

pub struct ExecutionLocation {
    pub state_root: PathBuf,
    pub home: Option<PathBuf>,
}

impl ExecutionLocation {
    pub fn current() -> Self {
        Self {
            state_root: bcode_config::default_state_dir(),
            home: std::env::home_dir(),
        }
    }
}

/// Acquire an approved connection for a direct CLI probe, exactly as daemon dispatch does.
/// Returns `None` when the context does not select an external connection.
pub fn resolve(
    host: &bcode_plugin::PluginHost,
    provider_plugin_id: Option<&str>,
    context: &ProviderRequestContext,
    model_id: &str,
    location: &ExecutionLocation,
) -> Result<Option<ProviderRequestContext>, String> {
    let Some(binding) = &context.external_auth_binding else {
        return Ok(None);
    };
    let plugin_id =
        provider_plugin_id.ok_or("external authentication requires an explicit provider")?;
    let home = location
        .home
        .clone()
        .ok_or("external authentication requires a home directory")?;
    let store = ExternalAuthStore::new(&location.state_root);
    let target = store
        .approved_target(binding)
        .map_err(|error| error.to_string())?;
    let choices: bcode_model::StartupModelChoicesResponse = host
        .invoke_service_json(
            plugin_id,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_STARTUP_MODEL_CHOICES,
            &bcode_model::StartupRecommendationRequest {
                auth_provider_id: target.provider_id,
                method_id: target.method_id,
            },
        )
        .map_err(|_| {
            bcode_provider_auth::external_auth::ExternalAuthError::DestinationRejected.to_string()
        })?;
    let registered = host
        .auth_provider_registry()
        .providers()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let enabled = registered
        .iter()
        .map(|entry| (entry.plugin_id.as_str(), &entry.contribution))
        .collect::<Vec<_>>();
    store
        .resolve_dispatch(DispatchResolution {
            recipe: context,
            provider_plugin_id: plugin_id,
            model_id,
            roots: &SourceRoots::from_process(home, &enabled),
            enabled: &enabled,
            provider_choices: &choices.choices,
        })
        .map(Some)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CliPluginTurnInvoker, configured_provider_context};
    use bcode_model_provider_runtime::BlockingModelProviderInvoker as _;
    use bcode_plugin_sdk::{NativeServiceContext, RustPlugin, ServiceResponse};
    use bcode_provider_auth::external_auth::discover;
    use bcode_provider_auth_models::external::{
        AuthExternalCredential, AuthExternalLocation, AuthExternalSource, AuthLiteralTokenPolicy,
    };
    use bcode_provider_auth_models::{
        AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION, AuthMethodContribution,
        AuthProviderContribution, AuthSecretField, AuthSecretValidation,
    };
    use std::collections::BTreeMap;

    fn contribution() -> AuthProviderContribution {
        AuthProviderContribution {
            schema_version: AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
            provider_id: "cli-fixture".into(),
            display_name: "Synthetic CLI provider".into(),
            methods: vec![AuthMethodContribution::SecretFields {
                method_id: "api_key".into(),
                display_name: "Static key".into(),
                supports_verification: false,
                supports_revocation: false,
                fields: vec![AuthSecretField {
                    credential_id: "api_key".into(),
                    prompt: "Key".into(),
                    storage_key: "FIXTURE_KEY".into(),
                    optional: false,
                    invocation_env: vec![],
                    validation: AuthSecretValidation::default(),
                    discovery_sources: vec![],
                }],
            }],
            external_sources: vec![AuthExternalSource {
                source_id: "codex".into(),
                application: "Codex".into(),
                method_id: "api_key".into(),
                location: AuthExternalLocation {
                    relative_path: ".codex/auth.json".into(),
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

    fn roots(home: &std::path::Path) -> SourceRoots {
        SourceRoots {
            home: home.to_owned(),
            overrides: BTreeMap::new(),
        }
    }

    fn settings() -> BTreeMap<String, String> {
        BTreeMap::from([("base_url".into(), "https://synthetic.invalid/v1".into())])
    }

    #[derive(Default)]
    struct FakeProvider;

    impl RustPlugin for FakeProvider {
        fn register_auth_providers(
            &mut self,
            registrar: bcode_plugin_sdk::AuthRegistrar,
        ) -> Result<(), bcode_plugin_sdk::PluginError> {
            registrar.register(&contribution()).unwrap();
            Ok(())
        }

        fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
            match context.request.operation.as_str() {
                bcode_model::OP_STARTUP_MODEL_CHOICES => {
                    let request: bcode_model::StartupRecommendationRequest =
                        context.request.payload_json().unwrap();
                    assert_eq!(request.auth_provider_id, "cli-fixture");
                    assert_eq!(request.method_id, "api_key");
                    ServiceResponse::json(&bcode_model::StartupModelChoicesResponse {
                        choices: vec![bcode_model::StartupRecommendation {
                            provider_plugin_id: "fixture.cli".into(),
                            model_id: "synthetic-model".into(),
                            settings: settings(),
                        }],
                    })
                    .unwrap()
                }
                bcode_model::OP_START_TURN => {
                    let request: bcode_model::ModelTurnRequest =
                        context.request.payload_json().unwrap();
                    assert_eq!(request.model_id, "synthetic-model");
                    assert_eq!(request.provider_context.settings, settings());
                    assert_eq!(request.provider_context.auth_candidates, []);
                    assert!(request.provider_context.env.is_empty());
                    let key = &request.provider_context.auth.unwrap().credentials["api_key"].value;
                    ServiceResponse::json(&bcode_model::StartTurnResponse {
                        provider_turn_id: key.clone(),
                    })
                    .unwrap()
                }
                _ => panic!("unexpected provider operation"),
            }
        }
    }

    fn host() -> bcode_plugin::PluginHost {
        const MANIFEST: &str = r#"
id = "fixture.cli"
name = "Synthetic CLI"
version = "0.0.1"
[[services]]
interface_id = "bcode.model-provider/v1"
[runtime]
type = "native"
abi_version = 5
library = "unused.dylib"
"#;
        let manifest = MANIFEST;
        bcode_plugin::PluginHost::load_static_plugins(&[(
            toml::from_str(manifest).unwrap(),
            bcode_plugin_sdk::static_plugin_vtable!(FakeProvider, MANIFEST),
        )])
        .unwrap()
    }

    fn request(binding: &str) -> bcode_model::ModelTurnRequest {
        let context = ProviderRequestContext {
            settings: settings(),
            external_auth_binding: Some(binding.into()),
            ..Default::default()
        };
        serde_json::from_value(serde_json::json!({
            "session_id": "ffffffff-ffff-4fff-8fff-ffffffffffff",
            "turn_id": "fixture-turn",
            "model_id": "synthetic-model",
            "messages": [],
            "provider_context": context
        }))
        .unwrap()
    }

    #[test]
    fn cli_invoker_rechecks_approval_rotation_revocation_and_state_root() {
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let other_state = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".codex")).unwrap();
        let file = home.path().join(".codex/auth.json");
        std::fs::write(&file, r#"{"type":"api","key":"synthetic-first"}"#).unwrap();
        let provider = contribution();
        let enabled = [("fixture.cli", &provider)];
        let candidate = discover(&roots(home.path()), &enabled).unwrap().remove(0);
        let store = ExternalAuthStore::new(state.path());
        let request = request(&candidate.binding_id);
        let mut host = host();
        let mut invoker = CliPluginTurnInvoker {
            host: &mut host,
            location: ExecutionLocation {
                state_root: state.path().into(),
                home: Some(home.path().into()),
            },
        };
        assert!(invoker.start_turn(Some("fixture.cli"), &request).is_err());
        let grant = store
            .approve(&roots(home.path()), &enabled, &candidate, 1)
            .unwrap();
        assert_eq!(
            invoker
                .start_turn(Some("fixture.cli"), &request)
                .unwrap()
                .provider_turn_id,
            "synthetic-first"
        );
        std::fs::write(&file, r#"{"type":"api","key":"synthetic-rotated"}"#).unwrap();
        assert_eq!(
            invoker
                .start_turn(Some("fixture.cli"), &request)
                .unwrap()
                .provider_turn_id,
            "synthetic-rotated"
        );
        std::fs::write(&file, "not-json").unwrap();
        assert!(invoker.start_turn(Some("fixture.cli"), &request).is_err());
        std::fs::write(&file, r#"{"type":"api","key":"synthetic-restored"}"#).unwrap();
        invoker.location.state_root = other_state.path().into();
        assert!(invoker.start_turn(Some("fixture.cli"), &request).is_err());
        invoker.location.state_root = state.path().into();
        assert!(store.revoke(&grant.binding_id).unwrap());
        assert!(invoker.start_turn(Some("fixture.cli"), &request).is_err());
        assert!(request.provider_context.auth.is_none());
    }

    #[test]
    fn cli_invoker_rejects_destination_model_profiles_and_disabled_provider() {
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".codex")).unwrap();
        let file = home.path().join(".codex/auth.json");
        std::fs::write(&file, r#"{"type":"api","key":"synthetic"}"#).unwrap();
        let provider = contribution();
        let enabled = [("fixture.cli", &provider)];
        let candidate = discover(&roots(home.path()), &enabled).unwrap().remove(0);
        ExternalAuthStore::new(state.path())
            .approve(&roots(home.path()), &enabled, &candidate, 1)
            .unwrap();
        // Unreadable source: destination/conflict checks must not need the secret.
        std::fs::write(&file, "not-json").unwrap();
        let request = request(&candidate.binding_id);
        let mut host = host();
        let mut invoker = CliPluginTurnInvoker {
            host: &mut host,
            location: ExecutionLocation {
                state_root: state.path().into(),
                home: Some(home.path().into()),
            },
        };
        let mut changed = request.clone();
        changed
            .provider_context
            .settings
            .insert("base_url".into(), "https://attacker.invalid".into());
        assert!(
            invoker
                .start_turn(Some("fixture.cli"), &changed)
                .unwrap_err()
                .contains("not approved for this external connection")
        );
        changed = request.clone();
        changed.model_id = "unapproved-model".into();
        assert!(
            invoker
                .start_turn(Some("fixture.cli"), &changed)
                .unwrap_err()
                .contains("not approved for this external connection")
        );
        for pool in [false, true] {
            changed = request.clone();
            if pool {
                changed.provider_context.auth_pool = Some("unapproved".into());
            } else {
                changed.provider_context.auth_profile = Some("unapproved".into());
            }
            assert!(
                invoker
                    .start_turn(Some("fixture.cli"), &changed)
                    .unwrap_err()
                    .contains("invalid external connection request")
            );
        }
        changed = request.clone();
        changed
            .provider_context
            .request
            .insert("billing".into(), serde_json::json!("override").into());
        assert!(
            invoker
                .start_turn(Some("fixture.cli"), &changed)
                .unwrap_err()
                .contains("invalid external connection request")
        );
        invoker.location.home = None;
        assert!(
            invoker
                .start_turn(Some("fixture.cli"), &request)
                .unwrap_err()
                .contains("home directory")
        );
        let mut disabled = bcode_plugin::PluginHost::default();
        invoker.host = &mut disabled;
        invoker.location.home = Some(home.path().into());
        assert!(invoker.start_turn(Some("fixture.cli"), &request).is_err());
    }

    #[test]
    fn configured_external_selection_is_a_nonsecret_recipe_and_other_operations_are_explicit() {
        let mut config = bcode_config::BcodeConfig::default();
        config.model.external_auth_binding = Some("synthetic-binding".into());
        let context = configured_provider_context(&config);
        assert_eq!(
            context.external_auth_binding.as_deref(),
            Some("synthetic-binding")
        );
        assert!(context.auth.is_none());
        assert_eq!(context.auth_candidates, []);
        assert!(context.env.is_empty());
        for operation in ["image upload verification", "auth usage", "capabilities"] {
            assert!(
                reject_unsupported(&context, operation)
                    .unwrap_err()
                    .contains(operation)
            );
        }
        assert!(reject_unsupported(&ProviderRequestContext::default(), "auth usage").is_ok());
    }
}
