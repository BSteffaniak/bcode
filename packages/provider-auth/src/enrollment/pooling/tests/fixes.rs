use super::*;
use crate::managed_pool::*;

fn marked(provider: &str) -> AuthPoolConfig {
    AuthPoolConfig {
        provider_plugin_id: Some("example.plugin".to_owned()),
        automatic_provider: Some(provider.to_owned()),
        automatic_group: Some("subscription".to_owned()),
        ..Default::default()
    }
}

#[test]
fn current_authorization_and_seal_removal_are_rejected_without_effects() {
    let mut config = BcodeConfig::default();
    config
        .auth
        .pools
        .insert("chosen".to_owned(), marked("example"));
    let plan = make_plan(
        &config,
        &RuntimeAuthSubscriptions::default(),
        "one",
        PoolEnrollment::Automatic,
    )
    .unwrap();
    assert!(complete_with_config(&plan, &config).is_ok());
    let mut changed = config.clone();
    changed
        .auth
        .pools
        .get_mut("chosen")
        .unwrap()
        .automatic_group = None;
    assert!(complete_with_config(&plan, &changed).is_err());
    let mut changed = config.clone();
    changed.active_context = Some("isolated".to_owned());
    assert!(complete_with_config(&plan, &changed).is_err());
    let mut changed = config.clone();
    changed
        .auth
        .pools
        .get_mut("chosen")
        .unwrap()
        .provider_plugin_id = Some("foreign".to_owned());
    assert!(complete_with_config(&plan, &changed).is_err());
    let mut changed = config.clone();
    changed
        .auth
        .profiles
        .insert("one".to_owned(), plan.prepared.resolved.profile.clone());
    changed
        .auth
        .profiles
        .get_mut("one")
        .unwrap()
        .settings
        .insert("vault".to_owned(), "/changed".to_owned());
    assert!(complete_with_config(&plan, &changed).is_err());
    let mut plan = plan;
    plan.original
        .profile
        .settings
        .insert("device_seal".to_owned(), "required".to_owned());
    plan.prepared
        .resolved
        .profile
        .settings
        .insert("device_seal".to_owned(), "required".to_owned());
    assert!(complete_with_config(&plan, &config).is_ok());
    plan.prepared
        .resolved
        .profile
        .settings
        .remove("device_seal");
    assert!(complete_with_config(&plan, &config).is_err());
    plan.prepared
        .resolved
        .profile
        .settings
        .insert("device_seal".to_owned(), "disabled".to_owned());
    assert!(complete_with_config(&plan, &config).is_ok());
}

#[test]
fn canonical_primary_joins_but_unrelated_accounts_and_api_keys_do_not() {
    let config = BcodeConfig::default();
    let empty = RuntimeAuthSubscriptions::default();
    let primary = make_plan(&config, &empty, "primary", PoolEnrollment::Standalone).unwrap();
    let runtime = apply(&primary);
    let mut config = config;
    config.auth.profiles.insert(
        "primary".to_owned(),
        primary.prepared.resolved.profile.clone(),
    );
    config.auth.profiles.insert(
        "unrelated".to_owned(),
        primary.prepared.resolved.profile.clone(),
    );
    config.auth.bindings.insert(
        "example".to_owned(),
        bcode_config::AuthBindingConfig {
            profile: Some("primary".to_owned()),
        },
    );
    let secondary = make_plan(&config, &empty, "secondary", PoolEnrollment::Automatic).unwrap();
    let members = &secondary.pool.as_ref().unwrap().1.profiles;
    assert_eq!(
        members
            .iter()
            .map(|member| member.auth_profile.as_str())
            .collect::<Vec<_>>(),
        ["primary", "secondary"]
    );
    config.auth.bindings.clear();
    let primary_profile = config.auth.profiles.remove("primary").unwrap();
    config
        .auth
        .profiles
        .insert("example".to_owned(), primary_profile);
    let canonical_default =
        make_plan(&config, &empty, "secondary", PoolEnrollment::Automatic).unwrap();
    assert_eq!(
        canonical_default.pool.as_ref().unwrap().1.profiles[0].auth_profile,
        "example"
    );
    config.auth.profiles.remove("example");
    config
        .auth
        .profiles
        .insert("primary".to_owned(), primary.prepared.resolved.profile);
    config.auth.bindings.insert(
        "example".to_owned(),
        bcode_config::AuthBindingConfig {
            profile: Some("primary".to_owned()),
        },
    );
    config.auth.profiles.get_mut("primary").unwrap().scheme = Some("api_key".to_owned());
    let secondary = make_plan(&config, &empty, "secondary", PoolEnrollment::Automatic).unwrap();
    assert_eq!(secondary.pool.as_ref().unwrap().1.profiles.len(), 1);
    // Runtime canonical bindings are also eligible, without sweeping unrelated accounts.
    let secondary = make_plan(
        &BcodeConfig::default(),
        &runtime,
        "secondary",
        PoolEnrollment::Automatic,
    )
    .unwrap();
    assert_eq!(secondary.pool.as_ref().unwrap().1.profiles.len(), 2);
}

#[test]
fn api_key_method_cannot_join_even_with_malformed_capability() {
    let mut provider = provider();
    provider.methods.push(AuthMethodContribution::SecretFields {
        method_id: "api_key".to_owned(),
        display_name: "API key".to_owned(),
        fields: vec![bcode_provider_auth_models::AuthSecretField {
            credential_id: "api_key".to_owned(),
            storage_key: "EXAMPLE_API_KEY".to_owned(),
            prompt: "API key".to_owned(),
            optional: false,
            validation: bcode_provider_auth_models::AuthSecretValidation::default(),
            discovery_sources: vec![],
            invocation_env: vec![],
        }],
        supports_verification: false,
        supports_revocation: false,
    });
    let config = BcodeConfig::default();
    let runtime = RuntimeAuthSubscriptions::default();
    assert!(
        plan(
            &config,
            &runtime,
            &provider,
            "example.plugin",
            "api_key",
            EnrollmentDestination::default(),
            PoolEnrollment::Explicit("pool".to_owned()),
            Some(&capability())
        )
        .is_err()
    );
    let standalone = plan(
        &config,
        &runtime,
        &provider,
        "example.plugin",
        "api_key",
        EnrollmentDestination::default(),
        PoolEnrollment::Automatic,
        Some(&capability()),
    )
    .unwrap();
    assert!(standalone.result.pool.is_none());
    let mut invalid = capability();
    invalid.methods.push("api_key".to_owned());
    assert!(
        plan(
            &config,
            &runtime,
            &provider,
            "example.plugin",
            "api_key",
            EnrollmentDestination::default(),
            PoolEnrollment::Automatic,
            Some(&invalid)
        )
        .is_err()
    );
}

#[test]
fn explicit_marked_pool_preserves_marker_and_rejects_other_group() {
    let config = BcodeConfig::default();
    let first = make_plan(
        &config,
        &RuntimeAuthSubscriptions::default(),
        "one",
        PoolEnrollment::Automatic,
    )
    .unwrap();
    let mut runtime = apply(&first);
    let name = first.result.pool.unwrap();
    runtime.pools.get_mut(&name).unwrap().preferred_profile = Some("one".to_owned());
    let joined = make_plan(
        &config,
        &runtime,
        "two",
        PoolEnrollment::Explicit(name.clone()),
    )
    .unwrap();
    let pool = joined.pool.as_ref().unwrap();
    assert_eq!(pool.1.automatic_group.as_deref(), Some("subscription"));
    assert_eq!(pool.1.preferred_profile.as_deref(), Some("one"));
    runtime.pools.get_mut(&name).unwrap().automatic_group = Some("other".to_owned());
    assert!(make_plan(&config, &runtime, "two", PoolEnrollment::Explicit(name)).is_err());
}

#[test]
fn providers_sharing_plugin_and_group_are_disambiguated() {
    let mut config = BcodeConfig::default();
    config
        .auth
        .pools
        .insert("ours".to_owned(), marked("example"));
    config
        .auth
        .pools
        .insert("theirs".to_owned(), marked("other"));
    let runtime = RuntimeAuthSubscriptions::default();
    assert_eq!(
        make_plan(&config, &runtime, "one", PoolEnrollment::Automatic)
            .unwrap()
            .result
            .pool
            .as_deref(),
        Some("ours")
    );
    config
        .auth
        .pools
        .get_mut("theirs")
        .unwrap()
        .automatic_provider = None;
    assert!(make_plan(&config, &runtime, "one", PoolEnrollment::Automatic).is_err());
}

#[test]
fn explicit_automatic_receipt_rejects_conflicting_defaults_but_manual_selection_stays_explicit() {
    let mut config = BcodeConfig::default();
    config
        .auth
        .pools
        .insert("first".to_owned(), marked("example"));
    config
        .auth
        .pools
        .insert("second".to_owned(), marked("example"));
    let account = make_plan(
        &BcodeConfig::default(),
        &RuntimeAuthSubscriptions::default(),
        "primary",
        PoolEnrollment::Standalone,
    )
    .unwrap();
    config
        .auth
        .profiles
        .insert("primary".to_owned(), account.prepared.resolved.profile);
    let selection = ManagedDefaultSelection {
        provider_plugin_id: "example.plugin",
        auth_profile: Some("primary"),
        auth_pool: Some("first"),
        native_defaults: true,
    };
    assert!(
        mint_managed_default(
            &config,
            &selection,
            &provider(),
            "example.plugin",
            &capability()
        )
        .is_err()
    );
    config.auth.pools.remove("second");
    let receipt = mint_managed_default(
        &config,
        &selection,
        &provider(),
        "example.plugin",
        &capability(),
    )
    .unwrap()
    .unwrap();
    let membership =
        refresh_managed_default(&config, &RuntimeAuthSubscriptions::default(), &receipt)
            .unwrap()
            .unwrap();
    assert_eq!(membership.pool, "first");
    assert_eq!(membership.profiles, ["primary"]);
    config.auth.pools.get_mut("first").unwrap().automatic_group = None;
    assert!(
        mint_managed_default(
            &config,
            &selection,
            &provider(),
            "example.plugin",
            &capability()
        )
        .unwrap()
        .is_none()
    );
    let config = BcodeConfig::default();
    let one = make_plan(
        &config,
        &RuntimeAuthSubscriptions::default(),
        "one",
        PoolEnrollment::Automatic,
    )
    .unwrap();
    let runtime = apply(&one);
    let selection = ManagedDefaultSelection {
        provider_plugin_id: "example.plugin",
        auth_profile: Some("one"),
        auth_pool: one.result.pool.as_deref(),
        native_defaults: true,
    };
    let receipt = mint_managed_default_with_runtime(
        &config,
        &runtime,
        &selection,
        &provider(),
        "example.plugin",
        &capability(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        refresh_managed_default(&config, &runtime, &receipt)
            .unwrap()
            .unwrap()
            .profiles,
        ["one"]
    );
}
