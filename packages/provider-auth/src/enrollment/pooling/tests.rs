#[cfg(test)]
mod fixes;

use super::*;
use bcode_config::{AuthPoolConfig, RuntimeAuthBinding};
use bcode_provider_auth_models::AuthMethodContribution;

fn provider() -> AuthProviderContribution {
    AuthProviderContribution {
        schema_version: bcode_provider_auth_models::AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
        provider_id: "example".to_owned(),
        display_name: "Example".to_owned(),
        methods: ["browser", "device"]
            .into_iter()
            .map(|method| AuthMethodContribution::Interactive {
                method_id: method.to_owned(),
                display_name: method.to_owned(),
                operation: "flow".to_owned(),
                credentials: vec![],
                supports_revocation: false,
            })
            .collect(),
    }
}
fn capability() -> AuthPoolCompatibility {
    AuthPoolCompatibility {
        schema_version: 1,
        provider_id: "example".to_owned(),
        group: "subscription".to_owned(),
        methods: vec!["browser".to_owned(), "device".to_owned()],
    }
}
fn make_plan(
    config: &BcodeConfig,
    runtime: &RuntimeAuthSubscriptions,
    name: &str,
    intent: PoolEnrollment,
) -> Result<EnrollmentPlan, EnrollmentError> {
    plan(
        config,
        runtime,
        &provider(),
        "example.plugin",
        "browser",
        EnrollmentDestination {
            profile: Some(name.to_owned()),
            ..Default::default()
        },
        intent,
        Some(&capability()),
    )
}
fn apply(plan: &EnrollmentPlan) -> RuntimeAuthSubscriptions {
    let mut runtime = plan.expected.clone();
    runtime
        .profiles
        .insert(plan.result.profile.clone(), plan.metadata.clone());
    runtime.bindings.insert(
        "example".to_owned(),
        RuntimeAuthBinding {
            profile: plan.result.profile.clone(),
            owner_plugin_id: "example.plugin".to_owned(),
        },
    );
    if let Some((name, pool)) = &plan.pool {
        runtime.pools.insert(name.clone(), pool.clone());
    }
    runtime
}

#[test]
fn automatic_creates_then_joins_and_explicit_wins() {
    let config = BcodeConfig::default();
    let first = make_plan(
        &config,
        &RuntimeAuthSubscriptions::default(),
        "one",
        PoolEnrollment::Automatic,
    )
    .unwrap();
    assert_eq!(
        first.result.pool.as_deref(),
        Some("example-subscription-default")
    );
    assert!(first.result.joined);
    let runtime = apply(&first);
    let device = plan(
        &config,
        &runtime,
        &provider(),
        "example.plugin",
        "device",
        EnrollmentDestination {
            profile: Some("device-account".to_owned()),
            ..Default::default()
        },
        PoolEnrollment::Automatic,
        Some(&capability()),
    )
    .unwrap();
    assert_eq!(device.result.pool, first.result.pool);
    let second = make_plan(&config, &runtime, "two", PoolEnrollment::Automatic).unwrap();
    assert_eq!(second.pool.as_ref().unwrap().1.profiles.len(), 2);
    assert_eq!(runtime.profiles.len(), 1); // planning is read-only
    let explicit = make_plan(
        &config,
        &runtime,
        "three",
        PoolEnrollment::Explicit("manual".to_owned()),
    )
    .unwrap();
    assert_eq!(explicit.result.pool.as_deref(), Some("manual"));
    assert!(!explicit.result.automatic);
    assert!(explicit.pool.unwrap().1.automatic_group.is_none());
}

#[test]
fn standalone_rejoin_does_not_require_oauth_and_manual_pools_stay_manual() {
    let config = BcodeConfig::default();
    let standalone = make_plan(
        &config,
        &RuntimeAuthSubscriptions::default(),
        "one",
        PoolEnrollment::Standalone,
    )
    .unwrap();
    let runtime = apply(&standalone);
    let joined = make_plan(&config, &runtime, "one", PoolEnrollment::Automatic).unwrap();
    assert!(!joined.prepared.publish_runtime);
    assert!(joined.result.joined);
    let mut config = config;
    config.auth.pools.insert(
        "example-subscription-default".to_owned(),
        AuthPoolConfig {
            provider_plugin_id: Some("example.plugin".to_owned()),
            ..Default::default()
        },
    );
    assert!(make_plan(&config, &runtime, "two", PoolEnrollment::Automatic).is_err());
}

#[test]
fn marker_opt_in_ambiguity_and_foreign_ownership_fail_closed() {
    let mut config = BcodeConfig::default();
    let marked = AuthPoolConfig {
        provider_plugin_id: Some("example.plugin".to_owned()),
        automatic_provider: Some("example".to_owned()),
        automatic_group: Some("subscription".to_owned()),
        ..Default::default()
    };
    config
        .auth
        .pools
        .insert("chosen".to_owned(), marked.clone());
    let runtime = RuntimeAuthSubscriptions::default();
    assert_eq!(
        make_plan(&config, &runtime, "one", PoolEnrollment::Automatic)
            .unwrap()
            .result
            .pool
            .as_deref(),
        Some("chosen")
    );
    config.auth.pools.insert("also".to_owned(), marked);
    assert!(make_plan(&config, &runtime, "one", PoolEnrollment::Automatic).is_err());
    config.auth.pools.clear();
    let mut runtime = runtime;
    runtime
        .pools
        .insert("manual".to_owned(), RuntimeAuthSubscriptionPool::default());
    assert!(
        make_plan(
            &config,
            &runtime,
            "one",
            PoolEnrollment::Explicit("manual".to_owned())
        )
        .is_err()
    );
}

#[test]
fn receipt_preserves_intent_and_refreshes_names_not_credentials() {
    use crate::managed_pool::*;
    let config = BcodeConfig::default();
    let selection = ManagedDefaultSelection {
        provider_plugin_id: "example.plugin",
        auth_profile: None,
        auth_pool: None,
        native_defaults: true,
    };
    let receipt = mint_managed_default(
        &config,
        &selection,
        &provider(),
        "example.plugin",
        &capability(),
    )
    .unwrap()
    .unwrap();
    let empty = RuntimeAuthSubscriptions::default();
    assert!(
        refresh_managed_default(&config, &empty, &receipt)
            .unwrap()
            .is_none()
    );
    let one = make_plan(&config, &empty, "one", PoolEnrollment::Automatic).unwrap();
    let runtime = apply(&one);
    assert_eq!(
        refresh_managed_default(&config, &runtime, &receipt)
            .unwrap()
            .unwrap()
            .profiles,
        ["one"]
    );
    assert!(resolve_managed_member(&config, &runtime, &receipt, "absent").is_err());
    let mut context = config.clone();
    context.active_context = Some("context".to_owned());
    assert!(
        mint_managed_default(
            &context,
            &selection,
            &provider(),
            "example.plugin",
            &capability()
        )
        .unwrap()
        .is_none()
    );
    assert!(refresh_managed_default(&context, &runtime, &receipt).is_err());
    for selection in [
        ManagedDefaultSelection {
            auth_profile: Some("one"),
            ..selection
        },
        ManagedDefaultSelection {
            auth_pool: Some("manual"),
            provider_plugin_id: "example.plugin",
            auth_profile: None,
            native_defaults: true,
        },
        ManagedDefaultSelection {
            provider_plugin_id: "example.plugin",
            auth_profile: None,
            auth_pool: None,
            native_defaults: false,
        },
    ] {
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
    }
}

#[test]
fn complete_rejects_tampered_destination_before_any_registry_access() {
    let mut plan = make_plan(
        &BcodeConfig::default(),
        &RuntimeAuthSubscriptions::default(),
        "one",
        PoolEnrollment::Automatic,
    )
    .unwrap();
    plan.prepared.resolved.profile_name = "different".to_owned();
    assert!(complete_with_config(&plan, &BcodeConfig::default()).is_err());
}

#[test]
fn completion_allows_only_device_seal_policy_adjustment() {
    let mut plan = make_plan(
        &BcodeConfig::default(),
        &RuntimeAuthSubscriptions::default(),
        "one",
        PoolEnrollment::Automatic,
    )
    .unwrap();
    // Context completion has no runtime side effects, allowing this policy validation test
    // to exercise the completion boundary without touching ambient user state.
    plan.context = true;
    plan.prepared
        .resolved
        .profile
        .settings
        .insert("device_seal".to_owned(), "disabled".to_owned());
    assert!(complete_with_config(&plan, &BcodeConfig::default()).is_ok());
    plan.prepared
        .resolved
        .profile
        .settings
        .insert("vault".to_owned(), "/different-vault".to_owned());
    assert!(complete_with_config(&plan, &BcodeConfig::default()).is_err());
}

#[test]
fn absent_capability_is_standalone_and_invalid_capability_is_error() {
    let config = BcodeConfig::default();
    let runtime = RuntimeAuthSubscriptions::default();
    let standalone = plan(
        &config,
        &runtime,
        &provider(),
        "example.plugin",
        "browser",
        EnrollmentDestination::default(),
        PoolEnrollment::Automatic,
        None,
    )
    .unwrap();
    assert!(standalone.result.pool.is_none());
    let mut capability = capability();
    capability.methods.push("api_key".to_owned());
    assert!(
        plan(
            &config,
            &runtime,
            &provider(),
            "example.plugin",
            "browser",
            EnrollmentDestination::default(),
            PoolEnrollment::Automatic,
            Some(&capability)
        )
        .is_err()
    );
}
