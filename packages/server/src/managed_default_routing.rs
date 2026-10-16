//! Native-only managed routing. No serialized request field can mint a route receipt.

use super::model_request_target::ModelRequestTargetError;
use super::{ServerState, SessionModelSelection};
use bcode_provider_auth::managed_pool::{
    ManagedDefaultRoute, ManagedDefaultSelection, mint_managed_default_with_runtime,
    refresh_managed_default, resolve_managed_member,
};
use bcode_provider_auth_models::{
    AUTH_INTERFACE_ID, AuthPoolCompatibility, AuthPoolCompatibilityRequest,
    OP_AUTH_POOL_COMPATIBILITY,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default)]
pub struct State {
    initialized: bool,
    receipt: Option<ManagedDefaultRoute>,
    candidates: BTreeMap<String, bcode_model::ProviderAuthCandidate>,
    live: BTreeSet<String>,
    active: bool,
}

const fn unavailable() -> ModelRequestTargetError {
    ModelRequestTargetError::ManagedAuthentication
}

/// Snapshot membership once, retaining credential bytes for accounts already materialized.
pub async fn refresh(
    state: &ServerState,
    provider: Option<&str>,
    context: &bcode_model::ProviderRequestContext,
    native_default: bool,
) -> Result<bcode_model::ProviderRequestContext, ModelRequestTargetError> {
    if !native_default || provider != state.selected_provider_plugin_id.as_deref() {
        return Ok(context.clone());
    }
    let config = &state.startup_config;
    let original = config.resolved_model_selection();
    // Check original intent before even invoking optional provider capabilities.
    if !native_intent(config, &original, native_default) {
        return Ok(context.clone());
    }
    let Some(provider) = provider else {
        return Ok(context.clone());
    };
    let mut cache = state.managed_default_routing.lock().await;
    if !cache.initialized {
        cache.receipt = discover_receipt(state, config, &original, provider).await?;
        cache.initialized = true;
    }
    let Some(receipt) = cache.receipt.clone() else {
        return Ok(context.clone());
    };
    drop(cache);
    let runtime = bcode_config::try_load_runtime_auth_subscriptions().map_err(|_| unavailable())?;
    let mut cache = state.managed_default_routing.lock().await;
    let refreshed = refresh_snapshot(
        config,
        &runtime,
        &receipt,
        &mut cache,
        context,
        bcode_provider_auth::resolve_auth_profile,
    );
    drop(cache);
    refreshed
}

fn refresh_snapshot(
    config: &bcode_config::BcodeConfig,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
    receipt: &ManagedDefaultRoute,
    cache: &mut State,
    context: &bcode_model::ProviderRequestContext,
    mut materialize: impl FnMut(
        &str,
        &bcode_config::AuthProfileConfig,
    ) -> bcode_provider_auth::ResolvedProviderAuth,
) -> Result<bcode_model::ProviderRequestContext, ModelRequestTargetError> {
    let membership =
        refresh_managed_default(config, runtime, receipt).map_err(|_| unavailable())?;
    let Some(membership) = membership else {
        // Never revive a removed managed pool's old credentials or fall back to ambient auth.
        cache.live.clear();
        return if cache.active {
            Err(unavailable())
        } else {
            Ok(context.clone())
        };
    };
    if membership.profiles.is_empty() {
        cache.live.clear();
        return Err(unavailable());
    }
    let initial = seed_candidates(
        config,
        runtime,
        receipt,
        cache,
        context,
        &membership.profiles,
    )?;
    if membership
        .profiles
        .iter()
        .any(|name| cache.candidates.contains_key(name) && !cache.live.contains(name))
    {
        // Reusing a removed name may denote a different account generation. Profile-keyed
        // continuation cannot prove compatibility; require a fresh daemon snapshot instead.
        return Err(unavailable());
    }
    if cache.candidates.len()
        + membership
            .profiles
            .iter()
            .filter(|name| !cache.candidates.contains_key(*name))
            .count()
        > 256
    {
        return Err(unavailable());
    }
    let candidates = snapshot_candidates(&membership.profiles, &initial, |name| {
        let resolved =
            resolve_managed_member(config, runtime, receipt, name).map_err(|_| unavailable())?;
        let material = materialize(name, &resolved.profile);
        if material
            .auth
            .credentials
            .values()
            .all(|credential| credential.value.is_empty())
            || material
                .auth
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity != "info")
        {
            return Err(unavailable());
        }
        Ok(bcode_model::ProviderAuthCandidate {
            profile: Some(name.to_owned()),
            auth: material.auth,
            env: material.env,
        })
    })?;
    let mut fresh = context.clone();
    fresh.auth_pool_routing =
        bcode_provider_auth::selected_auth_pool_routing(config, Some(&membership.pool));
    fresh.auth_pool = Some(membership.pool);
    fresh.auth_candidates = membership
        .profiles
        .iter()
        .filter_map(|name| candidates.get(name).cloned())
        .collect();
    let selected = fresh.auth_candidates.first().ok_or_else(unavailable)?;
    fresh.auth_profile.clone_from(&selected.profile);
    fresh.auth = Some(selected.auth.clone());
    fresh.env.clone_from(&selected.env);
    fresh.auth_pool_selection_reason = None;
    // Retain already acquired bytes even while a member is absent. Re-enrollment under the
    // same name must not replace material underneath an existing profile-keyed continuation.
    cache.live = membership.profiles.into_iter().collect();
    cache.candidates.extend(candidates);
    cache.active = true;
    Ok(fresh)
}

fn seed_candidates(
    config: &bcode_config::BcodeConfig,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
    receipt: &ManagedDefaultRoute,
    cache: &State,
    context: &bcode_model::ProviderRequestContext,
    names: &[String],
) -> Result<BTreeMap<String, bcode_model::ProviderAuthCandidate>, ModelRequestTargetError> {
    let mut initial = cache.candidates.clone();
    if !initial.is_empty() || cache.active {
        return Ok(initial);
    }
    let mut supplied = context.auth_candidates.clone();
    if let (Some(profile), Some(auth)) = (&context.auth_profile, &context.auth) {
        supplied.push(bcode_model::ProviderAuthCandidate {
            profile: Some(profile.clone()),
            auth: auth.clone(),
            env: context.env.clone(),
        });
    }
    for candidate in supplied {
        let Some(name) = candidate.profile.as_ref() else {
            continue;
        };
        if !names.contains(name) {
            continue;
        }
        resolve_managed_member(config, runtime, receipt, name).map_err(|_| unavailable())?;
        if let Some(previous) = initial.get(name)
            && previous != &candidate
        {
            return Err(unavailable());
        }
        initial.insert(name.clone(), candidate);
    }
    Ok(initial)
}

/// Read authoritative membership for status without materializing credentials or changing caches.
pub async fn status_profile(
    state: &ServerState,
    selection: &SessionModelSelection,
) -> Result<Option<String>, ModelRequestTargetError> {
    if !selection.native_default
        || selection.provider_plugin_id != state.selected_provider_plugin_id
    {
        return Ok(selection.provider_context.auth_profile.clone());
    }
    let config = &state.startup_config;
    let original = config.resolved_model_selection();
    if !native_intent(config, &original, true) {
        return Ok(selection.provider_context.auth_profile.clone());
    }
    let Some(provider) = selection.provider_plugin_id.as_deref() else {
        return Ok(selection.provider_context.auth_profile.clone());
    };
    let Some(receipt) = discover_receipt(state, config, &original, provider).await? else {
        return Ok(selection.provider_context.auth_profile.clone());
    };
    let runtime = bcode_config::try_load_runtime_auth_subscriptions().map_err(|_| unavailable())?;
    let membership =
        refresh_managed_default(config, &runtime, &receipt).map_err(|_| unavailable())?;
    Ok(membership.and_then(|membership| membership.profiles.into_iter().next()))
}

async fn discover_receipt(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
    original: &bcode_config::ResolvedModelSelection,
    provider: &str,
) -> Result<Option<ManagedDefaultRoute>, ModelRequestTargetError> {
    let runtime = if original
        .auth_pool
        .as_ref()
        .is_some_and(|pool| !config.auth.pools.contains_key(pool))
    {
        bcode_config::try_load_runtime_auth_subscriptions().map_err(|_| unavailable())?
    } else {
        bcode_config::RuntimeAuthSubscriptions::default()
    };
    let mut receipt = None;
    for registered in state.plugins.auth_provider_registry().providers() {
        if registered.plugin_id != provider {
            continue;
        }
        let declaration = state
            .plugins
            .invoke_service_json::<_, AuthPoolCompatibility>(
                &registered.plugin_id,
                AUTH_INTERFACE_ID,
                OP_AUTH_POOL_COMPATIBILITY,
                &AuthPoolCompatibilityRequest {
                    schema_version: 1,
                    provider_id: registered.contribution.provider_id.clone(),
                },
            )
            .await;
        let compatibility = match declaration {
            Ok(value) => value,
            Err(bcode_plugin::PluginServiceCallError::Service { code, .. })
                if code == "unsupported_operation" || code == "unsupported_provider" =>
            {
                continue;
            }
            Err(_) => return Err(unavailable()),
        };
        let minted = mint_managed_default_with_runtime(
            config,
            &runtime,
            &ManagedDefaultSelection {
                provider_plugin_id: provider,
                auth_profile: original.auth_profile.as_deref(),
                auth_pool: original.auth_pool.as_deref(),
                native_defaults: true,
            },
            &registered.contribution,
            &registered.plugin_id,
            &compatibility,
        )
        .map_err(|_| unavailable())?;
        if let Some(minted) = minted
            && receipt.replace(minted).is_some()
        {
            return Err(unavailable());
        }
    }
    Ok(receipt)
}

const fn native_intent(
    config: &bcode_config::BcodeConfig,
    selection: &bcode_config::ResolvedModelSelection,
    native: bool,
) -> bool {
    native
        && config.active_context.is_none()
        && config.auth.openai.is_none()
        && (selection.auth_profile.is_none() || selection.auth_pool.is_some())
}

fn snapshot_candidates(
    names: &[String],
    previous: &BTreeMap<String, bcode_model::ProviderAuthCandidate>,
    mut resolve: impl FnMut(&str) -> Result<bcode_model::ProviderAuthCandidate, ModelRequestTargetError>,
) -> Result<BTreeMap<String, bcode_model::ProviderAuthCandidate>, ModelRequestTargetError> {
    let mut snapshot = BTreeMap::new();
    for name in names {
        if snapshot.contains_key(name) {
            return Err(unavailable());
        }
        let candidate = match previous.get(name) {
            Some(candidate) => candidate.clone(),
            None => resolve(name)?,
        };
        if candidate.profile.as_deref() != Some(name.as_str()) {
            return Err(unavailable());
        }
        snapshot.insert(name.clone(), candidate);
    }
    Ok(snapshot)
}

#[cfg(test)]
mod integration_tests;

#[cfg(test)]
mod tests {
    use super::{native_intent, refresh, snapshot_candidates, unavailable};
    use crate::{
        ClientRuntimeContext, SessionId, SessionManager,
        default_model_selection_with_runtime_context, session_model_selection_with_runtime_context,
    };
    use std::collections::BTreeMap;

    fn candidate(name: &str, token: &str) -> bcode_model::ProviderAuthCandidate {
        bcode_model::ProviderAuthCandidate {
            profile: Some(name.to_owned()),
            auth: bcode_model::ProviderAuthContext {
                credentials: BTreeMap::from([(
                    "access_token".to_owned(),
                    bcode_model::ProviderAuthCredential {
                        value: token.to_owned(),
                        source: None,
                    },
                )]),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn next_snapshot_discovers_new_member_without_replacing_existing_material() {
        let old = BTreeMap::from([("a".to_owned(), candidate("a", "original"))]);
        let mut reads = Vec::new();
        let refreshed = snapshot_candidates(&["b".into(), "a".into()], &old, |name| {
            reads.push(name.to_owned());
            Ok(candidate(name, "new"))
        })
        .unwrap();
        assert_eq!(reads, ["b"]);
        assert_eq!(refreshed["a"], old["a"]);
        assert_eq!(refreshed["b"], candidate("b", "new"));
    }

    #[test]
    fn removed_members_are_not_dispatch_candidates() {
        let old = BTreeMap::from([
            ("a".to_owned(), candidate("a", "a")),
            ("b".to_owned(), candidate("b", "b")),
        ]);
        let refreshed = snapshot_candidates(&["b".into()], &old, |_| panic!("no reads")).unwrap();
        assert_eq!(refreshed.len(), 1);
        assert!(!refreshed.contains_key("a"));
    }

    #[test]
    fn failed_addition_never_publishes_partial_membership() {
        let old = BTreeMap::from([("a".to_owned(), candidate("a", "original"))]);
        assert!(
            snapshot_candidates(&["a".into(), "b".into()], &old, |_| Err(unavailable())).is_err()
        );
        assert_eq!(old["a"], candidate("a", "original"));
        assert!(
            snapshot_candidates(&["a".into(), "a".into()], &old, |_| panic!("no reads")).is_err()
        );
        assert!(snapshot_candidates(&["b".into()], &old, |_| Ok(candidate("other", "x"))).is_err());
    }

    #[tokio::test]
    async fn caller_context_cannot_enable_native_refresh() {
        let state = crate::tests::test_server_state(SessionManager::default());
        let context = bcode_model::ProviderRequestContext {
            auth_pool: Some("claimed-managed".into()),
            auth_candidates: vec![candidate("caller", "external")],
            ..Default::default()
        };
        let result = refresh(
            &state,
            state.selected_provider_plugin_id.as_deref(),
            &context,
            false,
        )
        .await
        .unwrap();
        let initialized = state.managed_default_routing.lock().await.initialized;
        drop(state);
        assert_eq!(result, context);
        assert!(!initialized);
    }

    #[tokio::test]
    async fn transient_overlay_clears_native_provenance() {
        let state = crate::tests::test_server_state(SessionManager::default());
        let session = SessionId::new();
        let mut selection = default_model_selection_with_runtime_context(&state, None);
        assert!(selection.native_default);
        selection.provider_plugin_id = Some("same.provider".into());
        state
            .session_model_selections
            .lock()
            .await
            .insert(session, selection);
        let resolved = session_model_selection_with_runtime_context(
            &state,
            session,
            Some(ClientRuntimeContext {
                selected_provider_plugin_id: Some("same.provider".into()),
                provider_context: bcode_model::ProviderRequestContext {
                    auth_profile: Some("external".into()),
                    ..Default::default()
                },
                ..Default::default()
            }),
        )
        .await;
        drop(state);
        assert!(!resolved.native_default);
        assert_eq!(
            resolved.provider_context.auth_profile.as_deref(),
            Some("external")
        );
    }

    #[tokio::test]
    async fn client_config_cannot_leave_cached_native_authority() {
        let state = crate::tests::test_server_state(SessionManager::default());
        let session = SessionId::new();
        state.session_model_selections.lock().await.insert(
            session,
            default_model_selection_with_runtime_context(&state, None),
        );
        state
            .set_session_config_from_runtime_context(
                session,
                Some(&ClientRuntimeContext::default()),
            )
            .await;
        let native_default = state.session_model_selections.lock().await[&session].native_default;
        let external = state
            .session_external_configs
            .lock()
            .await
            .contains(&session);
        drop(state);
        assert!(!native_default);
        assert!(external);
    }

    #[test]
    fn original_explicit_intent_and_non_native_sources_never_refresh() {
        let mut config = bcode_config::BcodeConfig::default();
        let mut selection = config.resolved_model_selection();
        assert!(native_intent(&config, &selection, true));
        assert!(!native_intent(&config, &selection, false));
        selection.auth_profile = Some("explicit".into());
        assert!(!native_intent(&config, &selection, true));
        selection.auth_profile = None;
        selection.auth_pool = Some("manual".into());
        // Pool eligibility is domain-owned; even manual pools reach mint and return no receipt.
        assert!(native_intent(&config, &selection, true));
        selection.auth_profile = Some("primary".into());
        assert!(native_intent(&config, &selection, true));
        selection.auth_pool = None;
        config.active_context = Some("isolated".into());
        assert!(!native_intent(&config, &selection, true));
    }
}
