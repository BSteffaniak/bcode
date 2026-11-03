//! Frontend-independent, local-only account lifecycle planning and execution.
//!
//! The coordinator authorizes the action, retains configuration and store ownership, and
//! invalidates daemon credential/routing caches afterwards. No remote revocation occurs here.

use crate::lifecycle::AuthVaultLifecycle;
use crate::{ResolvedAuthProfile, resolve_auth_provider_profile};
use bcode_config::{BcodeConfig, RuntimeAuthSubscriptions, account_settings::AccountSettings};
use bcode_provider_auth_models::account_lifecycle::{
    ACCOUNT_LIFECYCLE_SCHEMA_VERSION, AccountLifecycleAction, AccountLifecycleEffect,
    AccountLifecycleError, AccountLifecycleOutcome, AccountLifecyclePreview,
    AccountLifecycleRequest,
};
use bcode_provider_auth_models::{AccountId, AccountRoute, AuthProviderContribution};

/// Caller-owned local state, loaded without repair or fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountLifecycleState {
    /// Runtime-only registration metadata.
    pub runtime: RuntimeAuthSubscriptions,
    /// Configuration-scoped account metadata and preferences.
    pub settings: AccountSettings,
    /// Routing observations for this state scope.
    pub routing: crate::auth_pool_state::AuthPoolState,
}

/// Publication failure evidence supplied by the owning persistence adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountLifecycleCommitError {
    /// No state was published.
    Rejected,
    /// One or more publications may have occurred; reload before a new operation.
    Uncertain,
}

/// Exclusive state ownership retained for the entire execution, including credential deletion.
///
/// Implementations must prevent concurrent enrollment/removal/metadata writes until execution
/// returns. `commit` must compare the expected snapshot before publication and never overwrite
/// unrelated changes. Multi-file adapters must report partial publication as `Uncertain`.
/// Configuration and contribution ownership must likewise be retained by the coordinator.
pub trait AccountLifecycleStore {
    /// Read authoritative state, without creating, repairing, or replacing damaged state.
    ///
    /// # Errors
    /// Returns a normalized error when a trustworthy snapshot is unavailable.
    /// Implementations must not substitute defaults for damaged state.
    fn snapshot(&mut self) -> Result<AccountLifecycleState, AccountLifecycleError>;

    /// Publish an owned mutation against exactly the supplied snapshot.
    ///
    /// # Errors
    /// Distinguishes proven rejection from uncertain or partial publication.
    fn commit(
        &mut self,
        expected: &AccountLifecycleState,
        replacement: &AccountLifecycleState,
    ) -> Result<(), AccountLifecycleCommitError>;
}

/// Concrete native adapter retaining all three existing domain locks.
///
/// Acquisition order is runtime registry, configuration-scoped account settings, then
/// observations (process mutex and file lock). All acquisitions are nonblocking. Publication
/// is ordered but not a multi-file atomic transaction: any attempted failed publication is
/// conservatively `Uncertain`, and earlier files may already contain the replacement.
/// The handle is synchronous and must be acquired/used/dropped in one blocking execution scope;
/// do not hold it across async suspension or plugin remote calls.
pub struct NativeAccountLifecycleStore {
    runtime: bcode_config::RuntimeAuthTransaction,
    settings: bcode_config::account_settings::AccountSettingsTransaction,
    routing: crate::auth_pool_state::AuthPoolStateTransaction,
    runtime_visible: bool,
}

impl NativeAccountLifecycleStore {
    /// Acquire stores selected by the current process configuration provenance and state root.
    /// For daemon caller-specific launch provenance, use `acquire_scoped` instead.
    ///
    /// # Errors
    /// Fails closed on unavailable ownership, unsupported state or configuration identity.
    pub fn acquire(config: &BcodeConfig) -> Result<Self, AccountLifecycleError> {
        let cwd = std::env::current_dir().map_err(|_| AccountLifecycleError::StateUnavailable)?;
        let launch = bcode_config::config_launch_context(&cwd);
        Self::acquire_scoped(config, &bcode_config::default_state_dir(), &launch)
    }

    /// Acquire native stores using the authenticated caller's state root and launch provenance.
    /// Runtime-registry selection still follows the serving process's configured registry path.
    ///
    /// # Errors
    /// Fails before credential effects on lock contention, unsupported state or identity errors.
    pub fn acquire_scoped(
        config: &BcodeConfig,
        state_root: &std::path::Path,
        launch: &bcode_config::ConfigLaunchContext,
    ) -> Result<Self, AccountLifecycleError> {
        let key = bcode_config::account_settings::configuration_key(
            launch,
            config.active_context.as_deref(),
        )
        .map_err(|_| AccountLifecycleError::StateUnavailable)?;
        let mut store = Self::acquire_at(
            bcode_config::runtime_auth_subscriptions_path(),
            state_root,
            &key,
            state_root.join("provider").join("auth-pool-state.json"),
            state_root
                .join("provider")
                .join("openai-compatible-auth-pool-state.json"),
        )?;
        // Named contexts do not inherit the ambient runtime registry in provider resolution.
        store.runtime_visible = config.active_context.is_none();
        Ok(store)
    }

    /// Acquire explicit domain locations, useful for isolated native persistence tests.
    ///
    /// # Errors
    /// Fails if any lock or trustworthy domain snapshot is unavailable.
    pub fn acquire_at(
        registry: std::path::PathBuf,
        state_root: &std::path::Path,
        configuration_key: &str,
        observations: std::path::PathBuf,
        legacy_observations: std::path::PathBuf,
    ) -> Result<Self, AccountLifecycleError> {
        let unavailable = |_| AccountLifecycleError::StateUnavailable;
        let runtime =
            bcode_config::RuntimeAuthTransaction::acquire_at(registry).map_err(unavailable)?;
        let settings = bcode_config::account_settings::AccountSettingsTransaction::acquire(
            state_root,
            configuration_key,
        )
        .map_err(unavailable)?;
        let routing = crate::auth_pool_state::AuthPoolStateTransaction::acquire_at(
            observations,
            legacy_observations,
        )
        .map_err(unavailable)?;
        Ok(Self {
            runtime,
            settings,
            routing,
            runtime_visible: true,
        })
    }
}

impl AccountLifecycleStore for NativeAccountLifecycleStore {
    fn snapshot(&mut self) -> Result<AccountLifecycleState, AccountLifecycleError> {
        let unavailable = |_| AccountLifecycleError::StateUnavailable;
        Ok(AccountLifecycleState {
            runtime: if self.runtime_visible {
                self.runtime.snapshot().map_err(unavailable)?
            } else {
                RuntimeAuthSubscriptions::default()
            },
            settings: self.settings.snapshot().map_err(unavailable)?,
            routing: self.routing.snapshot().map_err(unavailable)?,
        })
    }

    fn commit(
        &mut self,
        expected: &AccountLifecycleState,
        replacement: &AccountLifecycleState,
    ) -> Result<(), AccountLifecycleCommitError> {
        if self
            .snapshot()
            .map_err(|_| AccountLifecycleCommitError::Rejected)?
            != *expected
        {
            return Err(AccountLifecycleCommitError::Rejected);
        }
        // Never claim rollback: a domain's rename may succeed before its directory sync fails.
        if self.runtime_visible {
            self.runtime
                .commit(&expected.runtime, &replacement.runtime)
                .map_err(|_| AccountLifecycleCommitError::Uncertain)?;
        } else if replacement.runtime != RuntimeAuthSubscriptions::default() {
            return Err(AccountLifecycleCommitError::Rejected);
        }
        self.settings
            .commit(&expected.settings, &replacement.settings)
            .map_err(|_| AccountLifecycleCommitError::Uncertain)?;
        self.routing
            .commit(&expected.routing, &replacement.routing)
            .map_err(|_| AccountLifecycleCommitError::Uncertain)?;
        Ok(())
    }
}

/// In-process plan with a private revalidation snapshot and duplicate-delivery receipt.
///
/// Plans are not serializable capabilities. Retain the same handle for duplicate execution:
/// after an attempted effect its recorded outcome is returned without repeating side effects.
/// After a restart, reload state and plan again; uncertain outcomes require reconciliation.
pub struct AccountLifecyclePlan {
    preview: AccountLifecyclePreview,
    resolved: ResolvedAuthProfile,
    runtime: RuntimeAuthSubscriptions,
    contribution: AuthProviderContribution,
    auth_config: bcode_config::AuthConfig,
    receipt: Option<AccountLifecycleOutcome>,
}

impl AccountLifecyclePlan {
    /// Secret-free presentation data; credential presence is deliberately not probed.
    #[must_use]
    pub const fn preview(&self) -> &AccountLifecyclePreview {
        &self.preview
    }
}

/// Plan from explicit snapshots without credential reads, filesystem writes or refresh admission.
/// Logout retains all metadata; removal never edits declarative configuration.
///
/// # Errors
/// Rejects unsupported contracts, ambiguous ownership, methods or invalid storage mappings.
pub fn plan(
    config: &BcodeConfig,
    runtime: &RuntimeAuthSubscriptions,
    contribution: &AuthProviderContribution,
    owner_plugin_id: &str,
    request: AccountLifecycleRequest,
) -> Result<AccountLifecyclePlan, AccountLifecycleError> {
    let invalid = || AccountLifecycleError::InvalidSelection;
    if request.schema_version != ACCOUNT_LIFECYCLE_SCHEMA_VERSION
        || request.provider_id != contribution.provider_id
        || request.profile.trim().is_empty()
        || owner_plugin_id.trim().is_empty()
        || contribution.validate().is_err()
    {
        return Err(invalid());
    }
    let resolved = resolve_auth_provider_profile(
        config,
        &request.provider_id,
        owner_plugin_id,
        Some(&request.profile),
        runtime,
    )
    .map_err(|_| invalid())?;
    if resolved.profile_name != request.profile {
        return Err(invalid());
    }
    let method = contribution
        .methods
        .iter()
        .find(|method| Some(method.method_id()) == resolved.profile.scheme.as_deref())
        .ok_or_else(invalid)?;
    let lifecycle =
        AuthVaultLifecycle::new(&resolved, &request.provider_id, owner_plugin_id, method)
            .map_err(|_| invalid())?;
    let keys = lifecycle.credential_storage_keys().map_err(|_| invalid())?;
    let unique = keys.values().collect::<std::collections::BTreeSet<_>>();
    if unique.len() != keys.len() || unique.iter().any(|key| key.trim().is_empty()) {
        return Err(invalid());
    }
    let mut after = runtime.clone();
    let removal = bcode_config::remove_runtime_auth_profile_from(
        &mut after,
        &request.profile,
        owner_plugin_id,
    );
    let preview = AccountLifecyclePreview {
        retains_declarative_profile: config.auth.profiles.contains_key(&request.profile),
        credential_ids: keys.into_keys().collect(),
        runtime_pools: if request.action == AccountLifecycleAction::Remove {
            removal.removed_from_pools
        } else {
            Vec::new()
        },
        request,
        owner_plugin_id: owner_plugin_id.into(),
    };
    Ok(AccountLifecyclePlan {
        preview,
        resolved,
        runtime: runtime.clone(),
        contribution: contribution.clone(),
        auth_config: config.auth.clone(),
        receipt: None,
    })
}

/// Revalidate against fresh authoritative state and perform the authorized local action.
///
/// Caller must invalidate retained daemon credentials, explicit account routes and provider
/// caches after any attempted credential effect, including uncertain outcomes. Existing session
/// requests may already hold credential copies; local deletion cannot retract dispatched work.
/// The returned per-stage evidence is never a claim of remote revocation.
pub fn execute(
    config: &BcodeConfig,
    contribution: &AuthProviderContribution,
    owner_plugin_id: &str,
    planned: &mut AccountLifecyclePlan,
    store: &mut dyn AccountLifecycleStore,
) -> AccountLifecycleOutcome {
    execute_with(
        config,
        contribution,
        owner_plugin_id,
        planned,
        store,
        |selected, method| {
            AuthVaultLifecycle::new(
                selected,
                &selected.provider_id,
                &selected.owner_plugin_id,
                method,
            )
            .and_then(|lifecycle| lifecycle.delete())
            .map_err(|_| AccountLifecycleError::CredentialsUnavailable)
        },
    )
}

fn execute_with(
    config: &BcodeConfig,
    contribution: &AuthProviderContribution,
    owner_plugin_id: &str,
    planned: &mut AccountLifecyclePlan,
    store: &mut dyn AccountLifecycleStore,
    delete: impl FnOnce(
        &ResolvedAuthProfile,
        &bcode_provider_auth_models::AuthMethodContribution,
    ) -> Result<(), AccountLifecycleError>,
) -> AccountLifecycleOutcome {
    use AccountLifecycleEffect::{Complete, NotAttempted, Uncertain};
    let mut outcome = AccountLifecycleOutcome {
        credentials: NotAttempted,
        runtime: NotAttempted,
        operational: NotAttempted,
        error: None,
    };
    if owner_plugin_id != planned.preview.owner_plugin_id
        || *contribution != planned.contribution
        || config.auth != planned.auth_config
    {
        outcome.error = Some(AccountLifecycleError::StalePlan);
        return outcome;
    }
    if let Some(receipt) = &planned.receipt {
        return receipt.clone();
    }
    let before = match store.snapshot() {
        Ok(state) => state,
        Err(error) => {
            outcome.error = Some(error);
            return outcome;
        }
    };
    let current = plan(
        config,
        &before.runtime,
        contribution,
        owner_plugin_id,
        planned.preview.request.clone(),
    );
    let Ok(current) = current else {
        outcome.error = Some(AccountLifecycleError::StalePlan);
        return outcome;
    };
    if current.resolved != planned.resolved
        || before.runtime != planned.runtime
        || current.preview != planned.preview
        || *contribution != planned.contribution
        || config.auth != planned.auth_config
    {
        outcome.error = Some(AccountLifecycleError::StalePlan);
        return outcome;
    }
    let method = contribution
        .methods
        .iter()
        .find(|method| Some(method.method_id()) == current.resolved.profile.scheme.as_deref());
    let Some(method) = method else {
        outcome.error = Some(AccountLifecycleError::InvalidSelection);
        return outcome;
    };
    outcome.credentials = Uncertain;
    if let Err(error) = delete(&current.resolved, method) {
        outcome.error = Some(error);
    } else {
        outcome.credentials = Complete;
        if planned.preview.request.action == AccountLifecycleAction::Remove {
            let mut after = before.clone();
            remove_state(&mut after, &current);
            match store.commit(&before, &after) {
                Ok(()) => {
                    outcome.runtime = Complete;
                    outcome.operational = Complete;
                }
                Err(error) => {
                    if error == AccountLifecycleCommitError::Uncertain {
                        outcome.runtime = Uncertain;
                        outcome.operational = Uncertain;
                    }
                    outcome.error = Some(AccountLifecycleError::StateUnavailable);
                }
            }
        }
    }
    planned.receipt = Some(outcome.clone());
    outcome
}

fn remove_state(state: &mut AccountLifecycleState, planned: &AccountLifecyclePlan) {
    let profile = &planned.preview.request.profile;
    let owner = &planned.preview.owner_plugin_id;
    let account = AccountId {
        provider_id: owner.clone(),
        profile: profile.clone(),
    };
    // Collect only proven-owned memberships before forgetting registrations.
    let mut runtime = state.runtime.clone();
    let removal = bcode_config::remove_runtime_auth_profile_from(&mut runtime, profile, owner);
    let mut pools = removal.removed_from_pools;
    for (name, pool) in &planned.auth_config.pools {
        if pool.provider_plugin_id.as_deref() == Some(owner.as_str())
            && pool.profiles.contains(profile)
            && !pools.contains(name)
        {
            pools.push(name.clone());
        }
    }
    for pool in &pools {
        state.routing.entries.remove(&format!("{pool}/{profile}"));
        if let Some(cursor) = state.routing.pools.get_mut(pool)
            && cursor.last_selected_profile.as_deref() == Some(profile)
        {
            cursor.last_selected_profile = None;
        }
    }
    state.runtime = runtime;
    state
        .settings
        .accounts
        .retain(|entry| entry.account != account);
    state
        .settings
        .last_used
        .retain(|_, usage| usage.account != account);
    for route in state
        .settings
        .sessions
        .values_mut()
        .chain(state.settings.defaults.values_mut())
    {
        remove_route_account(route, &account);
    }
    for history in state.settings.default_history.values_mut() {
        for entry in history {
            remove_route_account(&mut entry.route, &account);
        }
    }
}

fn remove_route_account(route: &mut AccountRoute, removed: &AccountId) {
    match route {
        AccountRoute::Only { account } if account == removed => {
            *route = AccountRoute::FollowConfiguration;
        }
        AccountRoute::Prefer {
            account,
            alternatives,
        } => {
            alternatives.retain(|candidate| candidate != removed);
            if account == removed {
                if alternatives.is_empty() {
                    *route = AccountRoute::FollowConfiguration;
                } else {
                    *account = alternatives.remove(0);
                }
            }
        }
        AccountRoute::FollowConfiguration | AccountRoute::Only { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_config::{AuthProfileConfig, RuntimeAuthBinding, RuntimeAuthProfile};
    use bcode_provider_auth_models::{
        AuthMethodContribution, AuthSecretField, AuthSecretValidation,
    };
    use std::collections::BTreeMap;

    struct FakeStore {
        state: AccountLifecycleState,
        failure: Option<AccountLifecycleCommitError>,
        commits: usize,
    }

    impl AccountLifecycleStore for FakeStore {
        fn snapshot(&mut self) -> Result<AccountLifecycleState, AccountLifecycleError> {
            Ok(self.state.clone())
        }

        fn commit(
            &mut self,
            expected: &AccountLifecycleState,
            replacement: &AccountLifecycleState,
        ) -> Result<(), AccountLifecycleCommitError> {
            assert_eq!(expected, &self.state);
            self.commits += 1;
            if let Some(error) = self.failure {
                return Err(error);
            }
            self.state = replacement.clone();
            Ok(())
        }
    }

    fn fixture(
        action: AccountLifecycleAction,
    ) -> (
        BcodeConfig,
        AuthProviderContribution,
        FakeStore,
        AccountLifecyclePlan,
    ) {
        let mut config = BcodeConfig::default();
        config.auth.profiles.insert(
            "account".into(),
            AuthProfileConfig {
                backend: "sshenv".into(),
                provider_id: Some("test-provider".into()),
                owner_plugin_id: Some("test.plugin".into()),
                scheme: Some("api_key".into()),
                map: BTreeMap::new(),
                // Planning and injected execution must never touch this path.
                settings: BTreeMap::from([("vault".into(), "/nonexistent/test-vault".into())]),
            },
        );
        let provider = AuthProviderContribution {
            schema_version: 1,
            provider_id: "test-provider".into(),
            display_name: "Test provider".into(),
            methods: vec![AuthMethodContribution::SecretFields {
                method_id: "api_key".into(),
                display_name: "API key".into(),
                fields: vec![AuthSecretField {
                    credential_id: "api_key".into(),
                    storage_key: "TEST_KEY".into(),
                    prompt: "API key".into(),
                    optional: false,
                    validation: AuthSecretValidation::default(),
                    invocation_env: Vec::new(),
                    discovery_sources: Vec::new(),
                }],
                supports_verification: false,
                supports_revocation: false,
            }],
        };
        let mut runtime = RuntimeAuthSubscriptions::default();
        runtime.profiles.insert(
            "account".into(),
            RuntimeAuthProfile {
                owner_plugin_id: "test.plugin".into(),
                provider_id: "test-provider".into(),
                ..RuntimeAuthProfile::default()
            },
        );
        runtime.bindings.insert(
            "test-provider".into(),
            RuntimeAuthBinding {
                owner_plugin_id: "test.plugin".into(),
                profile: "account".into(),
            },
        );
        runtime.profiles.insert(
            "unrelated".into(),
            RuntimeAuthProfile {
                owner_plugin_id: "other.plugin".into(),
                ..RuntimeAuthProfile::default()
            },
        );
        let mut settings = AccountSettings::default();
        settings
            .accounts
            .push(bcode_config::account_settings::AccountMetadata {
                account: AccountId {
                    provider_id: "test.plugin".into(),
                    profile: "account".into(),
                },
                label: Some("Personal".into()),
                pause: bcode_provider_auth_models::AccountPauseState::Manual,
            });
        let planned = plan(
            &config,
            &runtime,
            &provider,
            "test.plugin",
            AccountLifecycleRequest {
                schema_version: 1,
                provider_id: "test-provider".into(),
                profile: "account".into(),
                action,
            },
        )
        .unwrap();
        let store = FakeStore {
            state: AccountLifecycleState {
                runtime,
                settings,
                routing: crate::auth_pool_state::AuthPoolState::default(),
            },
            failure: None,
            commits: 0,
        };
        (config, provider, store, planned)
    }

    #[test]
    fn native_store_retains_locks_and_commits_all_domains_without_credentials() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("auth").join("subscriptions.json");
        let observations = root.path().join("provider").join("auth-pool-state.json");
        let legacy = root.path().join("provider").join("legacy.json");
        let key = "a".repeat(64);
        {
            let mut store = NativeAccountLifecycleStore::acquire_at(
                registry.clone(),
                root.path(),
                &key,
                observations.clone(),
                legacy.clone(),
            )
            .unwrap();
            assert!(bcode_config::RuntimeAuthTransaction::acquire_at(registry.clone()).is_err());
            assert!(
                bcode_config::account_settings::AccountSettingsTransaction::acquire(
                    root.path(),
                    &key
                )
                .is_err()
            );
            let before = store.snapshot().unwrap();
            let mut after = before.clone();
            after.runtime.bindings.insert(
                "test".into(),
                RuntimeAuthBinding {
                    owner_plugin_id: "test.plugin".into(),
                    profile: "account".into(),
                },
            );
            after.settings.defaults.insert(
                "preset".into(),
                AccountRoute::Only {
                    account: AccountId {
                        provider_id: "test.plugin".into(),
                        profile: "account".into(),
                    },
                },
            );
            after.routing.pools.insert(
                "pool".into(),
                crate::auth_pool_state::AuthPoolRoutingState {
                    last_selected_profile: Some("account".into()),
                },
            );
            store.commit(&before, &after).unwrap();
            assert_eq!(store.snapshot().unwrap(), after);
            assert_eq!(
                store.commit(&before, &after),
                Err(AccountLifecycleCommitError::Rejected)
            );
        }
        let mut reopened = NativeAccountLifecycleStore::acquire_at(
            registry,
            root.path(),
            &key,
            observations,
            legacy,
        )
        .unwrap();
        let state = reopened.snapshot().unwrap();
        drop(reopened);
        assert!(state.runtime.bindings.contains_key("test"));
        assert!(state.settings.defaults.contains_key("preset"));
        assert_eq!(
            state.routing.pools["pool"].last_selected_profile.as_deref(),
            Some("account")
        );
    }

    #[test]
    fn native_store_rejects_corrupt_observations_and_preserves_bytes() {
        let root = tempfile::tempdir().unwrap();
        let observations = root.path().join("observations.json");
        std::fs::write(&observations, b"damaged").unwrap();
        let result = NativeAccountLifecycleStore::acquire_at(
            root.path().join("subscriptions.json"),
            root.path(),
            &"b".repeat(64),
            observations.clone(),
            root.path().join("legacy.json"),
        );
        assert!(result.is_err());
        drop(result);
        assert_eq!(std::fs::read(observations).unwrap(), b"damaged");
    }

    #[test]
    fn logout_preserves_metadata_and_duplicate_execution_does_not_delete_again() {
        let (config, provider, mut store, mut planned) = fixture(AccountLifecycleAction::Logout);
        let before = store.state.clone();
        let first = execute_with(
            &config,
            &provider,
            "test.plugin",
            &mut planned,
            &mut store,
            |_, _| Ok(()),
        );
        assert_eq!(first.credentials, AccountLifecycleEffect::Complete);
        assert_eq!(store.state, before);
        assert_eq!(store.commits, 0);
        let duplicate = execute_with(
            &config,
            &provider,
            "test.plugin",
            &mut planned,
            &mut store,
            |_, _| panic!("duplicate deletion"),
        );
        assert_eq!(duplicate, first);
    }

    #[test]
    fn removal_preserves_configuration_and_foreign_profiles() {
        let (config, provider, mut store, mut planned) = fixture(AccountLifecycleAction::Remove);
        let outcome = execute_with(
            &config,
            &provider,
            "test.plugin",
            &mut planned,
            &mut store,
            |_, _| Ok(()),
        );
        assert_eq!(outcome.runtime, AccountLifecycleEffect::Complete);
        assert_eq!(outcome.operational, AccountLifecycleEffect::Complete);
        assert_eq!(store.state.settings.accounts, Vec::new());
        assert!(!store.state.runtime.profiles.contains_key("account"));
        assert!(store.state.runtime.profiles.contains_key("unrelated"));
        assert!(config.auth.profiles.contains_key("account"));
    }

    #[test]
    fn changed_destination_is_rejected_before_deletion() {
        let (mut config, provider, mut store, mut planned) =
            fixture(AccountLifecycleAction::Remove);
        config
            .auth
            .profiles
            .get_mut("account")
            .unwrap()
            .settings
            .insert("profile".into(), "changed".into());
        let outcome = execute_with(
            &config,
            &provider,
            "test.plugin",
            &mut planned,
            &mut store,
            |_, _| panic!("stale deletion"),
        );
        assert_eq!(outcome.credentials, AccountLifecycleEffect::NotAttempted);
        assert_eq!(outcome.error, Some(AccountLifecycleError::StalePlan));
        assert_eq!(store.commits, 0);
    }

    #[test]
    fn partial_publication_is_explicit_and_never_replays_credentials() {
        let (config, provider, mut store, mut planned) = fixture(AccountLifecycleAction::Remove);
        store.failure = Some(AccountLifecycleCommitError::Uncertain);
        let first = execute_with(
            &config,
            &provider,
            "test.plugin",
            &mut planned,
            &mut store,
            |_, _| Ok(()),
        );
        assert_eq!(first.credentials, AccountLifecycleEffect::Complete);
        assert_eq!(first.runtime, AccountLifecycleEffect::Uncertain);
        assert_eq!(first.operational, AccountLifecycleEffect::Uncertain);
        let duplicate = execute_with(
            &config,
            &provider,
            "test.plugin",
            &mut planned,
            &mut store,
            |_, _| panic!("duplicate deletion"),
        );
        assert_eq!(duplicate, first);
        assert_eq!(store.commits, 1);
    }

    #[test]
    fn route_cleanup_preserves_unrelated_explicit_alternatives() {
        let removed = AccountId {
            provider_id: "test.plugin".into(),
            profile: "account".into(),
        };
        let other = AccountId {
            provider_id: "other.plugin".into(),
            profile: "other".into(),
        };
        let mut route = AccountRoute::Prefer {
            account: removed.clone(),
            alternatives: vec![removed.clone(), other.clone()],
        };
        remove_route_account(&mut route, &removed);
        assert_eq!(
            route,
            AccountRoute::Prefer {
                account: other,
                alternatives: Vec::new()
            }
        );
        let mut only = AccountRoute::Only {
            account: removed.clone(),
        };
        remove_route_account(&mut only, &removed);
        assert_eq!(only, AccountRoute::FollowConfiguration);
    }

    #[test]
    fn removal_clears_owned_preference_even_without_membership() {
        let mut runtime = RuntimeAuthSubscriptions::default();
        runtime.pools.insert(
            "owned".into(),
            bcode_config::RuntimeAuthSubscriptionPool {
                owner_plugin_id: Some("test.plugin".into()),
                preferred_profile: Some("account".into()),
                ..bcode_config::RuntimeAuthSubscriptionPool::default()
            },
        );
        runtime.pools.insert(
            "foreign".into(),
            bcode_config::RuntimeAuthSubscriptionPool {
                owner_plugin_id: Some("other.plugin".into()),
                preferred_profile: Some("account".into()),
                ..bcode_config::RuntimeAuthSubscriptionPool::default()
            },
        );
        let removal =
            bcode_config::remove_runtime_auth_profile_from(&mut runtime, "account", "test.plugin");
        assert!(removal.changed());
        assert_eq!(removal.cleared_preferences, vec!["owned"]);
        assert_eq!(runtime.pools["owned"].preferred_profile, None);
        assert_eq!(
            runtime.pools["foreign"].preferred_profile.as_deref(),
            Some("account")
        );
        assert!(
            !bcode_config::remove_runtime_auth_profile_from(&mut runtime, "account", "test.plugin")
                .changed()
        );
    }

    #[test]
    fn credential_failure_does_not_forget_runtime_metadata() {
        let (config, provider, mut store, mut planned) = fixture(AccountLifecycleAction::Remove);
        let before = store.state.clone();
        let outcome = execute_with(
            &config,
            &provider,
            "test.plugin",
            &mut planned,
            &mut store,
            |_, _| Err(AccountLifecycleError::CredentialsUnavailable),
        );
        assert_eq!(
            outcome.error,
            Some(AccountLifecycleError::CredentialsUnavailable)
        );
        assert_eq!(store.state, before);
        assert_eq!(store.commits, 0);
    }
}
