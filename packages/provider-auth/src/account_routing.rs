//! Account identity, user metadata, and frozen dispatch constraints.

use bcode_config::account_settings::AccountSettings;
use bcode_provider_auth_models::{
    AccountId, AccountPauseState, AccountReadiness, AccountRoute, AccountSummary,
};
use std::collections::BTreeMap;

/// Deduplicate configured and enrolled accounts without materializing credentials.
#[must_use]
pub fn accounts(
    config: &bcode_config::BcodeConfig,
    registry: &bcode_config::RuntimeAuthSubscriptions,
    settings: &AccountSettings,
    now: u64,
) -> Vec<AccountSummary> {
    let mut accounts = BTreeMap::<AccountId, AccountSummary>::new();
    let mut insert = |provider: &str, profile: &str| {
        let account = AccountId {
            provider_id: provider.into(),
            profile: profile.into(),
        };
        let metadata = settings
            .accounts
            .iter()
            .find(|entry| entry.account == account);
        let pause = metadata.map_or(AccountPauseState::Eligible, |entry| {
            effective_pause(&entry.pause, now)
        });
        accounts
            .entry(account.clone())
            .or_insert_with(|| AccountSummary {
                account,
                label: metadata
                    .and_then(|entry| entry.label.clone())
                    .unwrap_or_else(|| profile.into()),
                pools: Vec::new(),
                pause,
                readiness: AccountReadiness::Unknown,
                last_used_unix: settings
                    .last_used
                    .values()
                    .filter(|usage| {
                        usage.account.provider_id == provider && usage.account.profile == profile
                    })
                    .map(|usage| usage.observed_at_ms / 1000)
                    .max(),
            });
    };
    for (name, profile) in &config.auth.profiles {
        if let Some(owner) = &profile.owner_plugin_id {
            insert(owner, name);
        }
    }
    for (name, profile) in &registry.profiles {
        insert(&profile.owner_plugin_id, name);
    }
    for pool in registry.pools.values() {
        for profile in &pool.profiles {
            if let Some(owner) = &profile.owner_plugin_id {
                insert(owner, &profile.auth_profile);
            }
        }
    }
    let observed = crate::auth_pool_state::load_state();
    for summary in accounts.values_mut() {
        for (name, pool) in &config.auth.pools {
            if pool.profiles.contains(&summary.account.profile)
                && pool
                    .provider_plugin_id
                    .as_ref()
                    .is_none_or(|owner| owner == &summary.account.provider_id)
            {
                summary.pools.push(name.clone());
            }
        }
        for (name, pool) in &registry.pools {
            if pool.profiles.iter().any(|profile| {
                profile.auth_profile == summary.account.profile
                    && profile.owner_plugin_id.as_deref() == Some(&summary.account.provider_id)
            }) && !summary.pools.contains(name)
            {
                summary.pools.push(name.clone());
            }
        }
        summary.pools.sort();
        if let Some(until_unix) = summary
            .pools
            .iter()
            .filter_map(|pool| {
                observed
                    .entries
                    .get(&format!("{pool}/{}", summary.account.profile))
            })
            .map(|entry| entry.cooldown_until_unix)
            .filter(|until| *until > now)
            .max()
        {
            summary.readiness = AccountReadiness::Cooldown { until_unix };
        }
    }
    accounts.into_values().collect()
}

/// Evaluate a timed pause against one snapshot timestamp.
#[must_use]
pub const fn effective_pause(pause: &AccountPauseState, now: u64) -> AccountPauseState {
    match pause {
        AccountPauseState::Until { unix_seconds } if *unix_seconds > now => {
            AccountPauseState::Until {
                unix_seconds: *unix_seconds,
            }
        }
        AccountPauseState::Manual => AccountPauseState::Manual,
        _ => AccountPauseState::Eligible,
    }
}

/// Validate a route against visible accounts and the session's effective provider.
///
/// # Errors
/// Rejects foreign, unknown, duplicate, or excessive alternatives before materialization.
pub fn validate_route(
    route: &AccountRoute,
    provider: Option<&str>,
    visible: &[AccountSummary],
) -> Result<(), &'static str> {
    let requested = match route {
        AccountRoute::FollowConfiguration => return Ok(()),
        AccountRoute::Only { account } => vec![account],
        AccountRoute::Prefer {
            account,
            alternatives,
        } => {
            if alternatives.len() > 32 {
                return Err("too many fallback accounts");
            }
            std::iter::once(account).chain(alternatives).collect()
        }
    };
    let mut seen = std::collections::BTreeSet::new();
    for account in requested {
        if provider != Some(account.provider_id.as_str())
            || !seen.insert(account)
            || !visible.iter().any(|entry| &entry.account == account)
        {
            return Err("account is unavailable for this provider and configuration");
        }
    }
    Ok(())
}

/// Freeze account eligibility into the credential candidates sent to provider plugins.
///
/// No settings reads happen inside provider retries. Only/prefer never inherit implicit
/// environment fallback, pool priming, or candidates outside the explicit list.
///
/// # Errors
/// Rejects invalid ownership, unavailable credentials, and routes with no eligible account.
pub fn freeze(
    config: &bcode_config::BcodeConfig,
    registry: &bcode_config::RuntimeAuthSubscriptions,
    settings: &AccountSettings,
    route: &AccountRoute,
    provider: Option<&str>,
    context: &mut bcode_model::ProviderRequestContext,
    now: u64,
) -> Result<(), &'static str> {
    let visible = accounts(config, registry, settings, now);
    validate_route(route, provider, &visible)?;
    let eligible = |profile: Option<&str>| {
        !visible.iter().any(|entry| {
            Some(entry.account.provider_id.as_str()) == provider
                && Some(entry.account.profile.as_str()) == profile
                && entry.pause != AccountPauseState::Eligible
        })
    };
    match route {
        AccountRoute::FollowConfiguration => {
            let had_candidates = !context.auth_candidates.is_empty();
            context
                .auth_candidates
                .retain(|candidate| eligible(candidate.profile.as_deref()));
            if had_candidates && context.auth_candidates.is_empty() {
                return Err("all configured accounts are paused");
            }
            if !eligible(context.auth_profile.as_deref()) {
                let candidate = context.auth_candidates.first().ok_or(
                    "selected account is paused and no configured alternative is eligible",
                )?;
                context.auth_profile.clone_from(&candidate.profile);
                context.auth = Some(candidate.auth.clone());
                context.env.clone_from(&candidate.env);
            }
        }
        AccountRoute::Only { account } | AccountRoute::Prefer { account, .. } => {
            let alternatives = match route {
                AccountRoute::Prefer { alternatives, .. } => alternatives.as_slice(),
                _ => &[],
            };
            let mut candidates = Vec::new();
            for account in std::iter::once(account).chain(alternatives) {
                if !eligible(Some(&account.profile)) {
                    continue;
                }
                // Resolve against this configuration's already-selected registry, never a second ambient registry read.
                if let Some(existing) = retained_candidate(context, account) {
                    candidates.push(existing);
                    continue;
                }
                let mut selected = config.clone();
                if !selected.auth.profiles.contains_key(&account.profile) {
                    let profile = crate::selected_runtime_auth_profile(
                        config,
                        registry,
                        &account.profile,
                        Some(&account.provider_id),
                    )
                    .map(|resolved| resolved.profile)
                    .or_else(|| {
                        registry.pools.keys().find_map(|pool| {
                            crate::runtime_pool_candidate_profile(
                                config,
                                registry,
                                pool,
                                &account.profile,
                                Some(&account.provider_id),
                            )
                        })
                    })
                    .ok_or("account ownership is unavailable")?;
                    selected
                        .auth
                        .profiles
                        .insert(account.profile.clone(), profile);
                }
                let resolved = crate::resolve_explicit_profile_context(
                    &selected,
                    &account.provider_id,
                    &account.profile,
                )?;
                candidates.push(bcode_model::ProviderAuthCandidate {
                    profile: Some(account.profile.clone()),
                    auth: resolved.auth.ok_or("account credentials are unavailable")?,
                    env: resolved.env,
                });
            }
            let selected = candidates
                .first()
                .ok_or("all selected accounts are paused")?;
            context.auth_profile.clone_from(&selected.profile);
            context.auth = Some(selected.auth.clone());
            context.env.clone_from(&selected.env);
            context.auth_candidates = candidates;
            context.auth_pool = None;
            context.auth_pool_routing = bcode_model::ProviderAuthPoolRouting::default();
            context.auth_pool_selection_reason = Some("explicit_account_route".into());
        }
    }
    if matches!(route, AccountRoute::FollowConfiguration) {
        crate::auth_pool_routing::apply_auth_pool_selection(context);
    }
    context.account_routing_frozen = true;
    Ok(())
}

fn retained_candidate(
    context: &bcode_model::ProviderRequestContext,
    account: &AccountId,
) -> Option<bcode_model::ProviderAuthCandidate> {
    if let Some(candidate) = context
        .auth_candidates
        .iter()
        .find(|candidate| candidate.profile.as_deref() == Some(account.profile.as_str()))
    {
        return Some(candidate.clone());
    }
    if context.auth_profile.as_deref() != Some(account.profile.as_str()) {
        return None;
    }
    context
        .auth
        .as_ref()
        .map(|auth| bcode_model::ProviderAuthCandidate {
            profile: Some(account.profile.clone()),
            auth: auth.clone(),
            env: context.env.clone(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_only_uses_retained_credentials_and_removes_implicit_fallback() {
        let (config, settings, mut context) = fixture();
        let route = AccountRoute::Only {
            account: AccountId {
                provider_id: "test.plugin".into(),
                profile: "two".into(),
            },
        };
        freeze(
            &config,
            &bcode_config::RuntimeAuthSubscriptions::default(),
            &settings,
            &route,
            Some("test.plugin"),
            &mut context,
            1,
        )
        .unwrap();
        assert_eq!(context.auth_profile.as_deref(), Some("two"));
        assert_eq!(context.auth_candidates.len(), 1);
        assert!(context.auth_pool.is_none());
        assert!(context.account_routing_frozen);
    }

    #[test]
    fn expiry_and_manual_pause_are_distinct() {
        assert_eq!(
            effective_pause(&AccountPauseState::Until { unix_seconds: 10 }, 10),
            AccountPauseState::Eligible
        );
        assert_eq!(
            effective_pause(&AccountPauseState::Manual, u64::MAX),
            AccountPauseState::Manual
        );
    }
    #[test]
    fn explicit_fallback_never_authorizes_foreign_provider() {
        let id = AccountId {
            provider_id: "other".into(),
            profile: "one".into(),
        };
        let route = AccountRoute::Only { account: id };
        assert!(validate_route(&route, Some("selected"), &[]).is_err());
        assert!(validate_route(&AccountRoute::FollowConfiguration, None, &[]).is_ok());
    }
    fn fixture() -> (
        bcode_config::BcodeConfig,
        AccountSettings,
        bcode_model::ProviderRequestContext,
    ) {
        let mut config = bcode_config::BcodeConfig::default();
        for profile in ["one", "two"] {
            config.auth.profiles.insert(
                profile.into(),
                bcode_config::AuthProfileConfig {
                    backend: "test_no_credentials".into(),
                    provider_id: Some("test".into()),
                    owner_plugin_id: Some("test.plugin".into()),
                    ..Default::default()
                },
            );
        }
        let candidates = ["one", "two"]
            .into_iter()
            .map(|profile| bcode_model::ProviderAuthCandidate {
                profile: Some(profile.into()),
                auth: bcode_model::ProviderAuthContext {
                    profile: Some(profile.into()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .collect();
        let context = bcode_model::ProviderRequestContext {
            auth_profile: Some("one".into()),
            auth: Some(bcode_model::ProviderAuthContext {
                profile: Some("one".into()),
                ..Default::default()
            }),
            auth_candidates: candidates,
            ..Default::default()
        };
        (config, AccountSettings::default(), context)
    }

    #[test]
    fn pause_filters_all_pools_and_single_account_and_is_frozen() {
        let (config, mut settings, mut context) = fixture();
        settings
            .accounts
            .push(bcode_config::account_settings::AccountMetadata {
                account: AccountId {
                    provider_id: "test.plugin".into(),
                    profile: "one".into(),
                },
                label: None,
                pause: AccountPauseState::Manual,
            });
        freeze(
            &config,
            &bcode_config::RuntimeAuthSubscriptions::default(),
            &settings,
            &AccountRoute::FollowConfiguration,
            Some("test.plugin"),
            &mut context,
            5,
        )
        .unwrap();
        assert_eq!(context.auth_profile.as_deref(), Some("two"));
        assert_eq!(context.auth_candidates.len(), 1);
        assert!(context.account_routing_frozen);
        settings.accounts[0].pause = AccountPauseState::Eligible;
        // A change to user settings cannot expand the already-frozen request/retry snapshot.
        assert_eq!(context.auth_candidates[0].profile.as_deref(), Some("two"));
        let (_, _, mut direct) = fixture();
        direct.auth_candidates.clear();
        settings.accounts[0].pause = AccountPauseState::Manual;
        assert!(
            freeze(
                &config,
                &bcode_config::RuntimeAuthSubscriptions::default(),
                &settings,
                &AccountRoute::FollowConfiguration,
                Some("test.plugin"),
                &mut direct,
                5
            )
            .is_err()
        );
    }

    #[test]
    fn account_rows_are_unique_and_foreign_identity_cannot_alias_a_pause() {
        let (config, mut settings, _) = fixture();
        settings
            .accounts
            .push(bcode_config::account_settings::AccountMetadata {
                account: AccountId {
                    provider_id: "foreign.plugin".into(),
                    profile: "one".into(),
                },
                label: Some("foreign".into()),
                pause: AccountPauseState::Manual,
            });
        let rows = accounts(
            &config,
            &bcode_config::RuntimeAuthSubscriptions::default(),
            &settings,
            5,
        );
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|row| row.pause == AccountPauseState::Eligible)
        );
        assert!(
            rows.iter()
                .all(|row| row.readiness == AccountReadiness::Unknown)
        );
        let duplicate = AccountRoute::Prefer {
            account: rows[0].account.clone(),
            alternatives: vec![rows[0].account.clone()],
        };
        assert!(validate_route(&duplicate, Some("test.plugin"), &rows).is_err());
    }
}
