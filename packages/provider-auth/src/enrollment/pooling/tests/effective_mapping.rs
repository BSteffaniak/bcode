use super::*;
use bcode_config::AuthCredentialMapping;
use bcode_provider_auth_models::{AuthCredentialStorage, AuthSecretField, AuthSecretValidation};
use std::collections::BTreeMap;

fn mapped_provider(secret: bool) -> AuthProviderContribution {
    let mut provider = provider();
    provider.methods = vec![if secret {
        AuthMethodContribution::SecretFields {
            method_id: "browser".to_owned(),
            display_name: "Secret".to_owned(),
            fields: vec![AuthSecretField {
                credential_id: "token".to_owned(),
                storage_key: "EXAMPLE_TOKEN".to_owned(),
                prompt: "Token".to_owned(),
                optional: false,
                validation: AuthSecretValidation::default(),
                discovery_sources: vec![],
                invocation_env: vec![],
            }],
            supports_verification: false,
            supports_revocation: false,
        }
    } else {
        AuthMethodContribution::Interactive {
            method_id: "browser".to_owned(),
            display_name: "Browser".to_owned(),
            operation: "flow".to_owned(),
            credentials: vec![
                AuthCredentialStorage {
                    credential_id: "token".to_owned(),
                    storage_key: "EXAMPLE_TOKEN".to_owned(),
                },
                AuthCredentialStorage {
                    credential_id: "refresh".to_owned(),
                    storage_key: "EXAMPLE_REFRESH".to_owned(),
                },
            ],
            supports_revocation: false,
        }
    }];
    provider
}

fn mapped_plan(
    config: &BcodeConfig,
    runtime: &RuntimeAuthSubscriptions,
    provider: &AuthProviderContribution,
    name: &str,
    pooling: PoolEnrollment,
) -> Result<EnrollmentPlan, EnrollmentError> {
    let compatibility = AuthPoolCompatibility {
        methods: vec!["browser".to_owned()],
        ..capability()
    };
    let interactive = matches!(
        provider.methods[0],
        AuthMethodContribution::Interactive { .. }
    );
    plan(
        config,
        runtime,
        provider,
        "example.plugin",
        "browser",
        EnrollmentDestination {
            profile: Some(name.to_owned()),
            ..Default::default()
        },
        pooling,
        interactive.then_some(&compatibility),
    )
}

fn fixture(provider: &AuthProviderContribution) -> (BcodeConfig, RuntimeAuthSubscriptions) {
    let first = mapped_plan(
        &BcodeConfig::default(),
        &RuntimeAuthSubscriptions::default(),
        provider,
        "one",
        PoolEnrollment::Automatic,
    )
    .unwrap();
    let mut config = BcodeConfig::default();
    let mut declared = first.prepared.resolved.profile.clone();
    declared.map.clear();
    config.auth.profiles.insert("one".to_owned(), declared);
    (config, apply(&first))
}

#[test]
fn reauthentication_accepts_omitted_defaults_without_rewriting_destination() {
    for secret in [false, true] {
        let provider = mapped_provider(secret);
        let (config, runtime) = fixture(&provider);
        let before = runtime.clone();
        let plan = crate::enrollment::plan_intent(
            &config,
            &runtime,
            &provider,
            ("example.plugin", "browser"),
            EnrollmentDestination {
                profile: Some("one".to_owned()),
                ..Default::default()
            },
            crate::enrollment::EnrollmentIntent::Reauthenticate,
            None,
        )
        .unwrap();
        assert!(plan.preserve_memberships);
        assert!(plan.prepared.resolved.profile.map.is_empty());
        assert_eq!(plan.metadata.map, runtime.profiles["one"].map);
        assert!(complete_with_config(&plan, &config).is_ok());
        assert_eq!(runtime, before);
    }
}

#[test]
fn target_and_primary_pool_members_compare_effective_mappings() {
    let provider = mapped_provider(false);
    let (mut config, runtime) = fixture(&provider);
    let joined = mapped_plan(
        &config,
        &runtime,
        &provider,
        "one",
        PoolEnrollment::Automatic,
    )
    .unwrap();
    assert!(!joined.result.joined);
    assert_eq!(joined.metadata.map, runtime.profiles["one"].map);
    config.auth.bindings.insert(
        "example".to_owned(),
        bcode_config::AuthBindingConfig {
            profile: Some("one".to_owned()),
        },
    );
    let secondary = mapped_plan(
        &config,
        &runtime,
        &provider,
        "two",
        PoolEnrollment::Automatic,
    )
    .unwrap();
    assert_eq!(secondary.pool.unwrap().1.profiles.len(), 2);
}

#[test]
fn configured_key_env_and_partial_defaults_follow_lifecycle_precedence() {
    let provider = mapped_provider(false);
    let (mut config, runtime) = fixture(&provider);
    for mapping in [
        AuthCredentialMapping::default(),
        AuthCredentialMapping {
            key: None,
            env: Some("EXAMPLE_TOKEN".to_owned()),
        },
        AuthCredentialMapping {
            key: Some("EXAMPLE_TOKEN".to_owned()),
            env: Some("IGNORED_WHEN_KEY_EXISTS".to_owned()),
        },
    ] {
        config.auth.profiles.get_mut("one").unwrap().map =
            BTreeMap::from([("token".to_owned(), mapping)]);
        let plan = mapped_plan(
            &config,
            &runtime,
            &provider,
            "one",
            PoolEnrollment::Standalone,
        )
        .unwrap();
        let lifecycle = crate::lifecycle::AuthVaultLifecycle::new(
            &plan.prepared.resolved,
            "example",
            "example.plugin",
            &provider.methods[0],
        )
        .unwrap();
        let keys = lifecycle.credential_storage_keys().unwrap();
        assert_eq!(keys["token"], "EXAMPLE_TOKEN");
        assert_eq!(keys["refresh"], "EXAMPLE_REFRESH");
    }
}

#[test]
fn explicit_mapping_and_unknown_entry_conflicts_still_fail_closed() {
    let provider = mapped_provider(false);
    let (mut config, runtime) = fixture(&provider);
    for (id, mapping) in [
        (
            "token",
            AuthCredentialMapping {
                key: Some("OTHER".to_owned()),
                env: None,
            },
        ),
        (
            "token",
            AuthCredentialMapping {
                key: None,
                env: Some("OTHER".to_owned()),
            },
        ),
        (
            "token",
            AuthCredentialMapping {
                key: Some(String::new()),
                env: None,
            },
        ),
        ("unknown", AuthCredentialMapping::default()),
    ] {
        config.auth.profiles.get_mut("one").unwrap().map =
            BTreeMap::from([(id.to_owned(), mapping)]);
        assert!(
            mapped_plan(
                &config,
                &runtime,
                &provider,
                "one",
                PoolEnrollment::Standalone
            )
            .is_err()
        );
    }
}

#[test]
fn equivalent_maps_do_not_mask_other_metadata_or_ownership_conflicts() {
    let provider = mapped_provider(false);
    let (config, runtime) = fixture(&provider);
    let mut variants = Vec::new();
    let mut changed = runtime.clone();
    changed.profiles.get_mut("one").unwrap().storage_profile = "other".to_owned();
    variants.push(changed);
    let mut changed = runtime.clone();
    changed.profiles.get_mut("one").unwrap().vault = "/different-vault".into();
    variants.push(changed);
    let mut changed = runtime.clone();
    changed.profiles.get_mut("one").unwrap().device_seal = Some("disabled".to_owned());
    variants.push(changed);
    let mut changed = runtime.clone();
    changed.profiles.get_mut("one").unwrap().owner_plugin_id = "foreign.plugin".to_owned();
    variants.push(changed);
    let mut changed = runtime.clone();
    changed.bindings.get_mut("example").unwrap().owner_plugin_id = "foreign.plugin".to_owned();
    variants.push(changed);
    for changed in variants {
        assert!(
            mapped_plan(
                &config,
                &changed,
                &provider,
                "one",
                PoolEnrollment::Standalone
            )
            .is_err()
        );
    }
    let mut foreign = config;
    foreign
        .auth
        .profiles
        .get_mut("one")
        .unwrap()
        .owner_plugin_id = Some("foreign.plugin".to_owned());
    assert!(
        mapped_plan(
            &foreign,
            &runtime,
            &provider,
            "one",
            PoolEnrollment::Standalone
        )
        .is_err()
    );
}

#[test]
fn pool_member_conflicts_are_not_hidden_by_effective_defaults() {
    let provider = mapped_provider(false);
    let (config, runtime) = fixture(&provider);
    for change in ["map", "owner", "vault", "seal", "scheme"] {
        let mut changed = runtime.clone();
        let member = &mut changed.pools.values_mut().next().unwrap().profiles[0];
        match change {
            "map" => {
                member.map.get_mut("token").unwrap().key = Some("OTHER".to_owned());
            }
            "owner" => member.owner_plugin_id = Some("foreign.plugin".to_owned()),
            "vault" => member.vault = "/different-vault".into(),
            "seal" => member.device_seal = Some("disabled".to_owned()),
            "scheme" => member.scheme = "different".to_owned(),
            _ => unreachable!(),
        }
        assert!(
            mapped_plan(
                &config,
                &changed,
                &provider,
                "one",
                PoolEnrollment::Automatic
            )
            .is_err()
        );
    }
}

#[test]
fn runtime_omitted_defaults_accept_explicit_declaration_without_registry_rewrite() {
    let provider = mapped_provider(false);
    let (mut config, mut runtime) = fixture(&provider);
    config.auth.profiles.get_mut("one").unwrap().map = runtime.profiles["one"].map.clone();
    runtime.profiles.get_mut("one").unwrap().map.clear();
    let planned = mapped_plan(
        &config,
        &runtime,
        &provider,
        "one",
        PoolEnrollment::Standalone,
    )
    .unwrap();
    assert!(planned.metadata.map.is_empty());
    assert!(!planned.prepared.resolved.profile.map.is_empty());
}
