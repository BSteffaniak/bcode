//! End-to-end domain receipt -> authoritative names -> request snapshot, without vault effects.
use super::{
    AuthPoolCompatibility, ManagedDefaultRoute, ManagedDefaultSelection, State,
    mint_managed_default_with_runtime, refresh, refresh_snapshot,
};
use crate::SessionManager;
use bcode_config::{
    AuthPoolConfig, AuthProfileConfig, RuntimeAuthSubscriptionPool, RuntimeAuthSubscriptionProfile,
};
use bcode_provider_auth_models::{AuthMethodContribution, AuthProviderContribution};
use std::collections::BTreeMap;

fn provider() -> AuthProviderContribution {
    AuthProviderContribution {
        schema_version: bcode_provider_auth_models::AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
        provider_id: "example".into(),
        display_name: "Example".into(),
        methods: vec![AuthMethodContribution::Interactive {
            method_id: "browser".into(),
            display_name: "Browser".into(),
            operation: "flow".into(),
            credentials: vec![],
            supports_revocation: false,
        }],
    }
}

fn capability() -> AuthPoolCompatibility {
    AuthPoolCompatibility {
        schema_version: 1,
        provider_id: "example".into(),
        group: "subscription".into(),
        methods: vec!["browser".into()],
    }
}

fn config(marked: bool) -> bcode_config::BcodeConfig {
    let mut config = bcode_config::BcodeConfig::default();
    config.model.auth_profile = Some("primary".into());
    config.model.auth_pool = Some("chosen".into());
    config.auth.pools.insert(
        "chosen".into(),
        AuthPoolConfig {
            automatic_provider: marked.then(|| "example".into()),
            automatic_group: marked.then(|| "subscription".into()),
            provider_plugin_id: Some("example.plugin".into()),
            profiles: vec!["primary".into()],
            ..Default::default()
        },
    );
    config.auth.profiles.insert(
        "primary".into(),
        AuthProfileConfig {
            backend: "sshenv".into(),
            scheme: Some("browser".into()),
            provider_id: Some("example".into()),
            owner_plugin_id: Some("example.plugin".into()),
            ..Default::default()
        },
    );
    config
}

fn mint(config: &bcode_config::BcodeConfig) -> Option<ManagedDefaultRoute> {
    let selection = config.resolved_model_selection();
    mint_managed_default_with_runtime(
        config,
        &bcode_config::RuntimeAuthSubscriptions::default(),
        &ManagedDefaultSelection {
            provider_plugin_id: "example.plugin",
            auth_profile: selection.auth_profile.as_deref(),
            auth_pool: selection.auth_pool.as_deref(),
            native_defaults: true,
        },
        &provider(),
        "example.plugin",
        &capability(),
    )
    .unwrap()
}

fn material(name: &str) -> bcode_provider_auth::ResolvedProviderAuth {
    bcode_provider_auth::ResolvedProviderAuth {
        auth: bcode_model::ProviderAuthContext {
            profile: Some(name.into()),
            credentials: BTreeMap::from([(
                "access_token".into(),
                bcode_model::ProviderAuthCredential {
                    value: format!("original-{name}"),
                    source: None,
                },
            )]),
            ..Default::default()
        },
        env: BTreeMap::new(),
    }
}

fn names(context: &bcode_model::ProviderRequestContext) -> Vec<&str> {
    context
        .auth_candidates
        .iter()
        .filter_map(|candidate| candidate.profile.as_deref())
        .collect()
}

fn primary_context() -> bcode_model::ProviderRequestContext {
    let primary = material("primary");
    bcode_model::ProviderRequestContext {
        auth_profile: Some("primary".into()),
        auth_pool: Some("chosen".into()),
        auth: Some(primary.auth.clone()),
        env: primary.env.clone(),
        auth_candidates: vec![bcode_model::ProviderAuthCandidate {
            profile: Some("primary".into()),
            auth: primary.auth.clone(),
            env: primary.env,
        }],
        ..Default::default()
    }
}

fn assert_conflicting_seed_rejected(
    config: &bcode_config::BcodeConfig,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
    receipt: &ManagedDefaultRoute,
    cache: &mut State,
    original: &bcode_model::ProviderRequestContext,
) {
    let mut invalid = original.clone();
    invalid
        .auth
        .as_mut()
        .unwrap()
        .credentials
        .get_mut("access_token")
        .unwrap()
        .value = "changed".into();
    assert!(
        refresh_snapshot(config, runtime, receipt, cache, &invalid, |_, _| panic!(
            "conflicting initial material is not a read grant"
        ))
        .is_err()
    );
    assert!(cache.candidates.is_empty());
}

#[tokio::test]
async fn marked_pool_next_request_adds_member_preserves_primary_and_freezes_previous_request() {
    let config = config(true);
    let receipt = mint(&config).expect("explicit marked pool authorizes native membership");
    let mut runtime = bcode_config::RuntimeAuthSubscriptions::default();
    let original = primary_context();
    let mut cache = State::default();
    assert_conflicting_seed_rejected(&config, &runtime, &receipt, &mut cache, &original);
    let first = refresh_snapshot(
        &config,
        &runtime,
        &receipt,
        &mut cache,
        &original,
        |_, _| panic!("existing primary must never be rematerialized"),
    )
    .unwrap();
    assert_eq!(names(&first), ["primary"]);
    runtime.pools.insert(
        "chosen".into(),
        RuntimeAuthSubscriptionPool {
            automatic_group: Some("subscription".into()),
            provider_plugin_id: Some("example.plugin".into()),
            provider_id: Some("example".into()),
            owner_plugin_id: Some("example.plugin".into()),
            profiles: vec![RuntimeAuthSubscriptionProfile {
                auth_profile: "new".into(),
                storage_profile: "new".into(),
                vault: "/unused/test-vault".into(),
                provider: "example".into(),
                scheme: "browser".into(),
                owner_plugin_id: Some("example.plugin".into()),
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    let mut reads = Vec::new();
    let next = refresh_snapshot(
        &config,
        &runtime,
        &receipt,
        &mut cache,
        &original,
        |name, _| {
            reads.push(name.to_owned());
            material(name)
        },
    )
    .unwrap();
    assert_eq!(reads, ["new"]);
    assert_eq!(names(&next), ["primary", "new"]);
    assert_eq!(next.auth_candidates[0], first.auth_candidates[0]);
    // A retry/compaction holds the first owned request context, not a live candidate handle.
    let state = crate::tests::test_server_state(SessionManager::default());
    let retry = refresh(&state, Some("example.plugin"), &first, false)
        .await
        .unwrap();
    let initialized = state.managed_default_routing.lock().await.initialized;
    drop(state);
    assert_eq!(retry, first);
    drop(retry);
    assert!(!initialized);
    assert_eq!(names(&first), ["primary"]);
    runtime.pools.get_mut("chosen").unwrap().profiles.clear();
    let removed = refresh_snapshot(
        &config,
        &runtime,
        &receipt,
        &mut cache,
        &original,
        |_, _| panic!("removal needs no credential reads"),
    )
    .unwrap();
    assert_eq!(names(&removed), ["primary"]);
    assert_eq!(names(&next), ["primary", "new"]);
    runtime.pools.get_mut("chosen").unwrap().owner_plugin_id = Some("foreign.plugin".into());
    assert!(
        refresh_snapshot(
            &config,
            &runtime,
            &receipt,
            &mut cache,
            &original,
            |_, _| panic!("invalid ownership must fail before materialization")
        )
        .is_err()
    );
}

#[test]
fn manual_pool_and_account_only_never_mint_receipts() {
    assert!(mint(&config(false)).is_none());
    let mut account = config(true);
    account.model.auth_pool = None;
    assert!(mint(&account).is_none());
}
