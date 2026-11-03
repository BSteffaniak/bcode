//! Caller-bound account lifecycle planning. Mutation requires domain-owned exclusive custody.

use bcode_provider_auth::operations::account_lifecycle as operations;
use bcode_provider_auth_models::account_lifecycle::AccountLifecycleRequest;
use bcode_provider_auth_models::application::{
    AccountLifecycleAccount, AccountLifecycleCatalog, AccountLifecycleExecution,
    AccountLifecyclePlan, AccountLifecycleProvider, AuthPoolApplicationFailure,
};
use bcode_session_models::ClientId;
use std::time::{Duration, Instant};

use crate::ServerState;

pub struct RetainedPlan {
    pub client_id: ClientId,
    expires: Instant,
    operation: operations::AccountLifecyclePlan,
    config: bcode_config::BcodeConfig,
    revoke: bool,
    scheme: String,
    resolved: bcode_provider_auth::ResolvedAuthProfile,
    contribution: bcode_provider_auth_models::AuthProviderContribution,
    launch: bcode_config::ConfigLaunchContext,
}

impl std::fmt::Debug for RetainedPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetainedPlan")
            .field("client_id", &self.client_id)
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
}

fn failure(code: &str, message: &str) -> AuthPoolApplicationFailure {
    AuthPoolApplicationFailure {
        code: code.into(),
        message: message.into(),
    }
}

async fn context(
    state: &ServerState,
    client: ClientId,
) -> Result<
    (
        bcode_config::BcodeConfig,
        bcode_config::RuntimeAuthSubscriptions,
    ),
    AuthPoolApplicationFailure,
> {
    let runtime = state.client_runtime_context(client).await;
    let config = crate::submission_admission_config(state, runtime.as_ref()).map_err(|_| {
        failure(
            "account_lifecycle_configuration_unavailable",
            "caller configuration is unavailable",
        )
    })?;
    let registry = if config.active_context.is_some() {
        bcode_config::RuntimeAuthSubscriptions::default()
    } else {
        bcode_config::try_load_runtime_auth_subscriptions().map_err(|_| {
            failure(
                "account_lifecycle_state_unavailable",
                "account registry is unavailable",
            )
        })?
    };
    Ok((config, registry))
}

fn local_credentials(
    resolved: &bcode_provider_auth::ResolvedAuthProfile,
    registered: &bcode_plugin::RegisteredAuthProvider,
) -> bcode_provider_auth_models::application::AccountLifecycleCredentials {
    use bcode_provider_auth_models::application::AccountLifecycleCredentials;
    let Some(method) = registered
        .contribution
        .methods
        .iter()
        .find(|method| Some(method.method_id()) == resolved.profile.scheme.as_deref())
    else {
        return AccountLifecycleCredentials::Inaccessible;
    };
    let result = bcode_provider_auth::lifecycle::AuthVaultLifecycle::new(
        resolved,
        &registered.contribution.provider_id,
        &registered.plugin_id,
        method,
    )
    .and_then(|lifecycle| {
        let status = lifecycle.inspect()?;
        if !status.profile_exists || status.present_credentials.is_empty() {
            return Ok(std::collections::BTreeMap::default());
        }
        lifecycle.read()
    });
    result.map_or(AccountLifecycleCredentials::Inaccessible, |mut values| {
        let present = values.values().any(|value| !value.is_empty());
        for value in values.values_mut() {
            zeroize::Zeroize::zeroize(value);
        }
        if present {
            AccountLifecycleCredentials::Present
        } else {
            AccountLifecycleCredentials::Missing
        }
    })
}

pub async fn catalog(
    state: &ServerState,
    client: ClientId,
) -> Result<AccountLifecycleCatalog, AuthPoolApplicationFailure> {
    let (config, runtime) = context(state, client).await?;
    let registered = state
        .plugins
        .auth_provider_registry()
        .providers()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        let providers = registered
            .iter()
            .map(|registered| {
                let provider = &registered.contribution;
                let accounts = bcode_provider_auth::discover_provider_profiles(
                    &config,
                    &provider.provider_id,
                    &registered.plugin_id,
                    &runtime,
                )
                .into_values()
                .filter_map(|resolved| {
                    // Resolution, not pool membership or the selected model, establishes plugin ownership.
                    let resolved = resolved.ok()?;
                    let credentials = local_credentials(&resolved, registered);
                    Some(AccountLifecycleAccount {
                        credentials,
                        profile: resolved.profile_name,
                        method_id: resolved.profile.scheme,
                        declared: resolved.source
                            == bcode_provider_auth::AuthProfileSource::Declarative,
                    })
                })
                .collect();
                AccountLifecycleProvider {
                    provider_id: provider.provider_id.clone(),
                    owner_plugin_id: registered.plugin_id.clone(),
                    display_name: provider.display_name.clone(),
                    methods: provider.methods.clone(),
                    accounts,
                }
            })
            .collect();
        AccountLifecycleCatalog { providers }
    })
    .await
    .map_err(|_| {
        failure(
            "account_lifecycle_catalog_unavailable",
            "local account inspection interrupted",
        )
    })
}

fn revocation_supported(registered: &bcode_plugin::RegisteredAuthProvider, scheme: &str) -> bool {
    registered.contribution.methods.iter().any(|method| {
        method.method_id() == scheme
            && matches!(
                method,
                bcode_provider_auth_models::AuthMethodContribution::Interactive {
                    supports_revocation: true,
                    ..
                }
            )
    })
}

pub async fn preview(
    state: &ServerState,
    client: ClientId,
    request: AccountLifecycleRequest,
    revoke: bool,
) -> Result<AccountLifecyclePlan, AuthPoolApplicationFailure> {
    let (config, runtime) = context(state, client).await?;
    let registered = state
        .plugins
        .auth_provider(&request.provider_id)
        .ok_or_else(|| {
            failure(
                "account_lifecycle_invalid_selection",
                "authentication provider is not registered",
            )
        })?;
    let operation = operations::plan(
        &config,
        &runtime,
        &registered.contribution,
        &registered.plugin_id,
        request,
    )
    .map_err(|_| {
        failure(
            "account_lifecycle_invalid_selection",
            "account ownership or method could not be verified",
        )
    })?;
    let resolved = bcode_provider_auth::resolve_auth_provider_profile(
        &config,
        &operation.preview().request.provider_id,
        &registered.plugin_id,
        Some(&operation.preview().request.profile),
        &runtime,
    )
    .map_err(|_| {
        failure(
            "account_lifecycle_invalid_selection",
            "account is unavailable",
        )
    })?;
    let scheme = resolved.profile.scheme.clone().ok_or_else(|| {
        failure(
            "account_lifecycle_invalid_selection",
            "account method is unavailable",
        )
    })?;
    if revoke && !revocation_supported(registered, &scheme) {
        return Err(failure(
            "account_lifecycle_revocation_unsupported",
            "selected method has no supported remote revocation flow",
        ));
    }
    let plan = AccountLifecyclePlan {
        plan_id: uuid::Uuid::new_v4().to_string(),
        local: operation.preview().clone(),
        revoke,
    };
    let runtime_context = state.client_runtime_context(client).await;
    let launch = runtime_context
        .as_ref()
        .and_then(|runtime| runtime.config_launch.as_deref())
        .cloned()
        .unwrap_or_else(|| bcode_config::config_launch_context(&state.state_root));
    let mut plans = state.account_lifecycle_plans.lock().await;
    plans.retain(|_, retained| retained.expires > Instant::now());
    if plans.len() >= 128 {
        return Err(failure(
            "account_lifecycle_busy",
            "too many pending account lifecycle plans",
        ));
    }
    plans.insert(
        plan.plan_id.clone(),
        RetainedPlan {
            client_id: client,
            expires: Instant::now() + Duration::from_secs(300),
            operation,
            config: config.clone(),
            launch,
            contribution: registered.contribution.clone(),
            revoke,
            resolved,
            scheme,
        },
    );
    drop(plans);
    Ok(plan)
}

/// Re-materialize invalidated accounts after transient client overlays and before route freezing.
/// A stale selected credential must not become an implicit switch to another account.
pub async fn refresh_invalidated_context(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
    selection: &mut crate::SessionModelSelection,
) -> Result<(), String> {
    let invalidations = state.account_lifecycle_invalidations.lock().await.clone();
    let Some(owner) = selection.provider_plugin_id.as_deref() else {
        return Ok(());
    };
    let context = &mut selection.provider_context;
    for (account, provider) in invalidations
        .iter()
        .filter(|(account, _)| account.provider_id == owner)
    {
        let selected = context.auth_profile.as_deref() == Some(account.profile.as_str());
        let present = selected
            || context
                .auth_candidates
                .iter()
                .any(|candidate| candidate.profile.as_deref() == Some(account.profile.as_str()));
        if !present {
            continue;
        }
        let fresh = bcode_provider_auth::resolve_owned_profile_context(
            config,
            provider,
            owner,
            &account.profile,
        );
        // Remove all retained material before evaluating whether authoritative custody can supply it.
        for candidate in context
            .auth_candidates
            .iter()
            .filter(|candidate| candidate.profile.as_deref() == Some(account.profile.as_str()))
        {
            for key in candidate.env.keys() {
                context.env.remove(key);
            }
        }
        context
            .auth_candidates
            .retain(|candidate| candidate.profile.as_deref() != Some(account.profile.as_str()));
        context.account_routing_frozen = false;
        if selected {
            context.auth = None;
            // This map is credential-bearing. Never merge a stale client's values back in.
            context.env.clear();
        }
        match fresh {
            Ok(fresh) => {
                if selected {
                    context.auth.clone_from(&fresh.auth);
                    context.env.extend(fresh.env.clone());
                }
                if let Some(auth) = fresh.auth {
                    context.auth_candidates.push(bcode_model::ProviderAuthCandidate {
                        profile: Some(account.profile.clone()), auth, env: fresh.env,
                    });
                }
            }
            Err(_) if selected => return Err("selected account credentials were removed; explicitly reconnect or select another account".into()),
            Err(_) => {}
        }
    }
    Ok(())
}

async fn revoke(
    state: &ServerState,
    client: ClientId,
    registered: &bcode_plugin::RegisteredAuthProvider,
    profile: &str,
    scheme: &str,
) -> bcode_provider_auth_models::application::AccountLifecycleRevocation {
    use bcode_provider_auth_models::application::AccountLifecycleRevocation;
    use bcode_provider_auth_models::{
        AuthFlowOperation, AuthFlowRequest, AuthFlowResponse, AuthFlowStatus,
        AuthMethodContribution,
    };
    let Some(AuthMethodContribution::Interactive {
        operation,
        supports_revocation: true,
        ..
    }) = registered
        .contribution
        .methods
        .iter()
        .find(|method| method.method_id() == scheme)
    else {
        return AccountLifecycleRevocation::Failed;
    };
    let request = AuthFlowRequest {
        schema_version: bcode_provider_auth_models::AUTH_FLOW_SCHEMA_VERSION,
        provider_id: registered.contribution.provider_id.clone(),
        method_id: scheme.into(),
        profile: profile.into(),
        operation: AuthFlowOperation::Begin,
        state: None,
        input: None,
        verify: false,
        revoke: true,
    };
    let response = state
        .plugins
        .invoke_service_json_scoped_with_timeout::<_, AuthFlowResponse>(
            &registered.plugin_id,
            bcode_provider_auth_models::AUTH_INTERFACE_ID,
            operation,
            &request,
            bcode_plugin::PluginInvocationScope::session(format!("account-lifecycle-{client}"))
                .with_client_id(client.to_string()),
            Duration::from_secs(30),
        )
        .await;
    match response {
        Ok(mut response) => {
            let complete = response.validate().is_ok()
                && response.status == AuthFlowStatus::Succeeded
                && response.credentials.is_empty();
            // Revocation never enrolls credentials or exposes opaque provider state to clients.
            for value in response.credentials.values_mut() {
                zeroize::Zeroize::zeroize(value);
            }
            response.credentials.clear();
            if let Some(opaque) = response.state.as_mut() {
                zeroize::Zeroize::zeroize(opaque);
            }
            if complete {
                AccountLifecycleRevocation::Complete
            } else {
                AccountLifecycleRevocation::Failed
            }
        }
        Err(_) => AccountLifecycleRevocation::Failed,
    }
}

fn forget_material(context: &mut bcode_model::ProviderRequestContext, profile: &str) {
    if context.auth_profile.as_deref() == Some(profile) {
        context.auth = None;
        for value in context.env.values_mut() {
            zeroize::Zeroize::zeroize(value);
        }
        context.env.clear();
        context.account_routing_frozen = false;
    }
    context
        .auth_candidates
        .retain(|candidate| candidate.profile.as_deref() != Some(profile));
}

async fn invalidate(state: &ServerState, provider: &str, owner: &str, profile: &str) {
    let account = bcode_provider_auth_models::AccountId {
        provider_id: owner.into(),
        profile: profile.into(),
    };
    state
        .account_lifecycle_invalidations
        .lock()
        .await
        .insert(account, provider.into());
    for selection in state.session_model_selections.lock().await.values_mut() {
        if selection.provider_plugin_id.as_deref() == Some(owner) {
            forget_material(&mut selection.provider_context, profile);
        }
    }
    for runtime in state.client_runtime_contexts.lock().await.values_mut() {
        if runtime.selected_provider_plugin_id.as_deref() == Some(owner) {
            forget_material(&mut runtime.provider_context, profile);
        }
    }
    crate::managed_default_routing::invalidate_account(state, profile).await;
    // Provider continuation state may embed credential/profile affinity. Drop it rather than
    // allowing a continuation to resume under another account after local custody changes.
    let mut continuations = state.provider_state.lock().await;
    continuations.records.clear();
    continuations.save();
}

/// Adapter-level safety check: domain publication must never relax an existing account-only route.
struct RouteProtectedStore(operations::NativeAccountLifecycleStore);

impl operations::AccountLifecycleStore for RouteProtectedStore {
    fn snapshot(
        &mut self,
    ) -> Result<
        operations::AccountLifecycleState,
        bcode_provider_auth_models::account_lifecycle::AccountLifecycleError,
    > {
        operations::AccountLifecycleStore::snapshot(&mut self.0)
    }

    fn commit(
        &mut self,
        expected: &operations::AccountLifecycleState,
        replacement: &operations::AccountLifecycleState,
    ) -> Result<(), operations::AccountLifecycleCommitError> {
        if !preserves_only_routes(&expected.settings, &replacement.settings) {
            return Err(operations::AccountLifecycleCommitError::Rejected);
        }
        operations::AccountLifecycleStore::commit(&mut self.0, expected, replacement)
    }
}

fn preserves_only_routes(
    before: &bcode_config::account_settings::AccountSettings,
    after: &bcode_config::account_settings::AccountSettings,
) -> bool {
    use bcode_provider_auth_models::AccountRoute;
    let preserves = |old: &std::collections::BTreeMap<String, AccountRoute>,
                     new: &std::collections::BTreeMap<String, AccountRoute>| {
        old.iter().all(|(key, route)| {
            !matches!(route, AccountRoute::Only { .. }) || new.get(key) == Some(route)
        })
    };
    preserves(&before.sessions, &after.sessions)
        && preserves(&before.defaults, &after.defaults)
        && before.default_history.iter().all(|(key, history)| {
            history.iter().enumerate().all(|(index, entry)| {
                !matches!(entry.route, AccountRoute::Only { .. })
                    || after
                        .default_history
                        .get(key)
                        .and_then(|history| history.get(index))
                        .is_some_and(|after| after == entry)
            })
        })
}

/// Refresh the core's registry snapshot only when the reviewed account and all visible effects
/// are exactly equivalent. The same guard is used before revocation and under local custody.
fn equivalent_operation(
    retained: &RetainedPlan,
    config: &bcode_config::BcodeConfig,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
    registered: &bcode_plugin::RegisteredAuthProvider,
) -> Result<
    operations::AccountLifecyclePlan,
    bcode_provider_auth_models::account_lifecycle::AccountLifecycleError,
> {
    use bcode_provider_auth_models::account_lifecycle::AccountLifecycleError::StalePlan;
    let reviewed = retained.operation.preview();
    let resolved = bcode_provider_auth::resolve_auth_provider_profile(
        config,
        &reviewed.request.provider_id,
        &registered.plugin_id,
        Some(&reviewed.request.profile),
        runtime,
    )
    .map_err(|_| StalePlan)?;
    let current = operations::plan(
        config,
        runtime,
        &registered.contribution,
        &registered.plugin_id,
        reviewed.request.clone(),
    )
    .map_err(|_| StalePlan)?;
    if resolved != retained.resolved
        || current.preview() != reviewed
        || registered.contribution != retained.contribution
        || config.auth != retained.config.auth
        || config.active_context != retained.config.active_context
    {
        return Err(StalePlan);
    }
    Ok(current)
}

fn execute_guarded_local(
    plan: &RetainedPlan,
    config: &bcode_config::BcodeConfig,
    registered: &bcode_plugin::RegisteredAuthProvider,
    mut store: operations::NativeAccountLifecycleStore,
) -> bcode_provider_auth_models::account_lifecycle::AccountLifecycleOutcome {
    use bcode_provider_auth_models::account_lifecycle::{
        AccountLifecycleEffect, AccountLifecycleOutcome,
    };
    use operations::AccountLifecycleStore as _;
    let refreshed = store
        .snapshot()
        .and_then(|snapshot| equivalent_operation(plan, config, &snapshot.runtime, registered));
    match refreshed {
        Ok(mut operation) => operations::execute(
            config,
            &registered.contribution,
            &registered.plugin_id,
            &mut operation,
            &mut RouteProtectedStore(store),
        ),
        Err(error) => AccountLifecycleOutcome {
            credentials: AccountLifecycleEffect::NotAttempted,
            runtime: AccountLifecycleEffect::NotAttempted,
            operational: AccountLifecycleEffect::NotAttempted,
            error: Some(error),
        },
    }
}

struct PreparedExecution {
    plan: RetainedPlan,
    config: bcode_config::BcodeConfig,
    registered: bcode_plugin::RegisteredAuthProvider,
    launch: bcode_config::ConfigLaunchContext,
}

async fn prepare_execution(
    state: &ServerState,
    client: ClientId,
    plan_id: &str,
) -> Result<PreparedExecution, AuthPoolApplicationFailure> {
    let mut plans = state.account_lifecycle_plans.lock().await;
    if !plans
        .get(plan_id)
        .is_some_and(|plan| plan.client_id == client && plan.expires > Instant::now())
    {
        return Err(failure(
            "account_lifecycle_plan_unavailable",
            "reviewed plan is unavailable; preview again",
        ));
    }
    let plan = plans.remove(plan_id).ok_or_else(|| {
        failure(
            "account_lifecycle_plan_unavailable",
            "reviewed plan is unavailable",
        )
    })?;
    drop(plans);
    let (config, _) = context(state, client).await?;
    if config.auth != plan.config.auth || config.active_context != plan.config.active_context {
        return Err(failure(
            "account_lifecycle_stale_plan",
            "account configuration changed; preview again",
        ));
    }
    let request = plan.operation.preview().request.clone();
    let registered = state
        .plugins
        .auth_provider(&request.provider_id)
        .cloned()
        .ok_or_else(|| {
            failure(
                "account_lifecycle_invalid_selection",
                "authentication provider is unavailable",
            )
        })?;
    if registered.plugin_id != plan.operation.preview().owner_plugin_id
        || registered.contribution != plan.contribution
    {
        return Err(failure(
            "account_lifecycle_stale_plan",
            "registered authentication provider changed; preview again",
        ));
    }
    let runtime = state.client_runtime_context(client).await;
    let launch = runtime
        .as_ref()
        .and_then(|runtime| runtime.config_launch.as_deref())
        .cloned()
        .unwrap_or_else(|| bcode_config::config_launch_context(&state.state_root));
    if launch != plan.launch {
        return Err(failure(
            "account_lifecycle_stale_plan",
            "configuration provenance changed; preview again",
        ));
    }
    if state.locations.is_some() {
        return Err(failure(
            "account_lifecycle_scope_unavailable",
            "scoped account lifecycle persistence is unavailable",
        ));
    }
    Ok(PreparedExecution {
        plan,
        config,
        registered,
        launch,
    })
}

fn validate_local_custody(
    plan: &RetainedPlan,
    config: &bcode_config::BcodeConfig,
    state_root: &std::path::Path,
    launch: &bcode_config::ConfigLaunchContext,
    registered: &bcode_plugin::RegisteredAuthProvider,
) -> Result<(), AuthPoolApplicationFailure> {
    use operations::AccountLifecycleStore as _;
    let mut store =
        operations::NativeAccountLifecycleStore::acquire_scoped(config, state_root, launch)
            .map_err(|_| {
                failure(
                    "account_lifecycle_state_unavailable",
                    "exclusive account custody is unavailable",
                )
            })?;
    let snapshot = store.snapshot().map_err(|_| {
        failure(
            "account_lifecycle_state_unavailable",
            "account state is unavailable",
        )
    })?;
    equivalent_operation(plan, config, &snapshot.runtime, registered).map_err(|_| {
        failure(
            "account_lifecycle_stale_plan",
            "account custody or reviewed effects changed; preview again",
        )
    })?;
    drop(store);
    Ok(())
}

pub async fn execute(
    state: &std::sync::Arc<ServerState>,
    client: ClientId,
    plan_id: &str,
) -> Result<AccountLifecycleExecution, AuthPoolApplicationFailure> {
    let PreparedExecution {
        plan,
        config,
        registered,
        launch,
    } = prepare_execution(state, client, plan_id).await?;
    let request = plan.operation.preview().request.clone();
    let owned_state = std::sync::Arc::clone(state);
    let owner = registered.plugin_id.clone();
    let registered_for_validation = registered.clone();
    let config_for_validation = config.clone();
    let launch_for_validation = launch.clone();
    // Establish trustworthy local custody before any provider effect, then release all file
    // locks before the optional network call. Local execution reacquires and revalidates.
    let (plan, validated) = tokio::task::spawn_blocking(move || {
        let result = validate_local_custody(
            &plan,
            &config_for_validation,
            &owned_state.state_root,
            &launch_for_validation,
            &registered_for_validation,
        );
        (plan, result)
    })
    .await
    .map_err(|_| {
        failure(
            "account_lifecycle_state_unavailable",
            "account custody preparation interrupted",
        )
    })?;
    validated?;
    let revocation = if plan.revoke {
        revoke(state, client, &registered, &request.profile, &plan.scheme).await
    } else {
        bcode_provider_auth_models::application::AccountLifecycleRevocation::NotRequested
    };
    let owned_state = std::sync::Arc::clone(state);
    let result = tokio::task::spawn_blocking(move || {
        let local = operations::NativeAccountLifecycleStore::acquire_scoped(
            &config,
            &owned_state.state_root,
            &launch,
        )
        .map_or_else(
            |_| {
                use bcode_provider_auth_models::account_lifecycle::{
                    AccountLifecycleEffect, AccountLifecycleError, AccountLifecycleOutcome,
                };
                AccountLifecycleOutcome {
                    credentials: AccountLifecycleEffect::NotAttempted,
                    runtime: AccountLifecycleEffect::NotAttempted,
                    operational: AccountLifecycleEffect::NotAttempted,
                    error: Some(AccountLifecycleError::StateUnavailable),
                }
            },
            |store| execute_guarded_local(&plan, &config, &registered, store),
        );
        Ok::<_, AuthPoolApplicationFailure>(AccountLifecycleExecution { local, revocation })
    })
    .await;
    // Even an uncertain custody attempt must fence all retained material and continuations.
    invalidate(state, &request.provider_id, &owner, &request.profile).await;
    result.map_err(|_| {
        failure(
            "account_lifecycle_execution_uncertain",
            "account execution interrupted; reconcile state before retrying",
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered_provider() -> bcode_plugin::RegisteredAuthProvider {
        use bcode_provider_auth_models::{
            AuthMethodContribution, AuthProviderContribution, AuthSecretField, AuthSecretValidation,
        };
        bcode_plugin::RegisteredAuthProvider {
            plugin_id: "test.plugin".into(),
            contribution: AuthProviderContribution {
                schema_version: 1,
                provider_id: "test".into(),
                display_name: "Test".into(),
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
            },
        }
    }

    #[test]
    fn equivalent_repreview_accepts_unrelated_registry_changes_but_rejects_changed_custody() {
        use bcode_provider_auth_models::account_lifecycle::AccountLifecycleAction;
        let mut config = bcode_config::BcodeConfig::default();
        config.auth.profiles.insert(
            "personal".into(),
            bcode_config::AuthProfileConfig {
                backend: "sshenv".into(),
                provider_id: Some("test".into()),
                owner_plugin_id: Some("test.plugin".into()),
                scheme: Some("api_key".into()),
                map: std::collections::BTreeMap::new(),
                settings: std::collections::BTreeMap::from([(
                    "vault".into(),
                    "/nonexistent/repreview-test".into(),
                )]),
            },
        );
        let registered = registered_provider();
        let mut runtime = bcode_config::RuntimeAuthSubscriptions::default();
        runtime.profiles.insert(
            "work".into(),
            bcode_config::RuntimeAuthProfile {
                owner_plugin_id: "other.plugin".into(),
                ..Default::default()
            },
        );
        let request = AccountLifecycleRequest {
            schema_version: 1,
            provider_id: "test".into(),
            profile: "personal".into(),
            action: AccountLifecycleAction::Remove,
        };
        let operation = operations::plan(
            &config,
            &runtime,
            &registered.contribution,
            &registered.plugin_id,
            request,
        )
        .unwrap();
        let retained = RetainedPlan {
            client_id: ClientId::new(),
            expires: Instant::now() + Duration::from_secs(30),
            resolved: bcode_provider_auth::resolve_auth_provider_profile(
                &config,
                "test",
                "test.plugin",
                Some("personal"),
                &runtime,
            )
            .unwrap(),
            operation,
            config: config.clone(),
            revoke: false,
            scheme: "api_key".into(),
            contribution: registered.contribution.clone(),
            launch: bcode_config::ConfigLaunchContext::default(),
        };
        assert_eq!(
            local_credentials(&retained.resolved, &registered),
            bcode_provider_auth_models::application::AccountLifecycleCredentials::Missing
        );
        let mut unknown_method = retained.resolved.clone();
        unknown_method.profile.scheme = Some("unknown".into());
        assert_eq!(
            local_credentials(&unknown_method, &registered),
            bcode_provider_auth_models::application::AccountLifecycleCredentials::Inaccessible
        );
        runtime.profiles.remove("work");
        assert!(equivalent_operation(&retained, &config, &runtime, &registered).is_ok());
        config
            .auth
            .profiles
            .get_mut("personal")
            .unwrap()
            .settings
            .insert("vault".into(), "/nonexistent/changed-custody".into());
        assert!(equivalent_operation(&retained, &config, &runtime, &registered).is_err());
        let debug = format!("{retained:?}");
        assert!(!debug.contains("nonexistent"));
    }

    fn retained_plan(
        profile: &str,
        config: &bcode_config::BcodeConfig,
        runtime: &bcode_config::RuntimeAuthSubscriptions,
        registered: &bcode_plugin::RegisteredAuthProvider,
    ) -> RetainedPlan {
        use bcode_provider_auth_models::account_lifecycle::AccountLifecycleAction;
        let request = AccountLifecycleRequest {
            schema_version: 1,
            provider_id: "test".into(),
            profile: profile.into(),
            action: AccountLifecycleAction::Remove,
        };
        RetainedPlan {
            client_id: ClientId::new(),
            expires: Instant::now() + Duration::from_secs(30),
            operation: operations::plan(
                config,
                runtime,
                &registered.contribution,
                &registered.plugin_id,
                request,
            )
            .unwrap(),
            resolved: bcode_provider_auth::resolve_auth_provider_profile(
                config,
                "test",
                "test.plugin",
                Some(profile),
                runtime,
            )
            .unwrap(),
            config: config.clone(),
            revoke: false,
            scheme: "api_key".into(),
            contribution: registered.contribution.clone(),
            launch: bcode_config::ConfigLaunchContext::default(),
        }
    }

    fn assert_capability_rejection(
        reviewed: &RetainedPlan,
        config: &bcode_config::BcodeConfig,
        runtime: &bcode_config::RuntimeAuthSubscriptions,
        registered: &bcode_plugin::RegisteredAuthProvider,
    ) {
        let server = std::sync::Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let async_runtime = tokio::runtime::Runtime::new().unwrap();
        // Foreign and expired capabilities are rejected before configuration/store admission.
        async_runtime.block_on(async {
            let foreign = ClientId::new();
            let operation = operations::plan(
                config,
                runtime,
                &registered.contribution,
                &registered.plugin_id,
                reviewed.operation.preview().request.clone(),
            )
            .unwrap();
            server.account_lifecycle_plans.lock().await.insert(
                "reviewed".into(),
                RetainedPlan {
                    client_id: reviewed.client_id,
                    expires: reviewed.expires,
                    operation,
                    config: config.clone(),
                    revoke: false,
                    scheme: reviewed.scheme.clone(),
                    resolved: reviewed.resolved.clone(),
                    contribution: registered.contribution.clone(),
                    launch: bcode_config::ConfigLaunchContext::default(),
                },
            );
            assert_eq!(
                Box::pin(execute(&server, foreign, "reviewed"))
                    .await
                    .unwrap_err()
                    .code,
                "account_lifecycle_plan_unavailable"
            );
            assert!(
                server
                    .account_lifecycle_plans
                    .lock()
                    .await
                    .contains_key("reviewed")
            );
            server
                .account_lifecycle_plans
                .lock()
                .await
                .get_mut("reviewed")
                .unwrap()
                .expires = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
            assert_eq!(
                Box::pin(execute(&server, reviewed.client_id, "reviewed"))
                    .await
                    .unwrap_err()
                    .code,
                "account_lifecycle_plan_unavailable"
            );
        });
        drop(async_runtime);
        drop(server);
    }

    fn native_accounts(
        root: &std::path::Path,
    ) -> (
        bcode_config::BcodeConfig,
        bcode_config::RuntimeAuthSubscriptions,
    ) {
        let mut config = bcode_config::BcodeConfig::default();
        let mut runtime = bcode_config::RuntimeAuthSubscriptions::default();
        for name in ["personal", "work"] {
            config.auth.profiles.insert(
                name.into(),
                bcode_config::AuthProfileConfig {
                    backend: "sshenv".into(),
                    provider_id: Some("test".into()),
                    owner_plugin_id: Some("test.plugin".into()),
                    scheme: Some("api_key".into()),
                    map: std::collections::BTreeMap::default(),
                    settings: std::collections::BTreeMap::from([(
                        "vault".into(),
                        root.join("absent-vault").to_string_lossy().into_owned(),
                    )]),
                },
            );
            runtime.profiles.insert(
                name.into(),
                bcode_config::RuntimeAuthProfile {
                    provider_id: "test".into(),
                    owner_plugin_id: "test.plugin".into(),
                    ..bcode_config::RuntimeAuthProfile::default()
                },
            );
        }
        (config, runtime)
    }

    #[test]
    fn native_daemon_local_phase_executes_two_reviewed_removals_without_registry_staleness() {
        use bcode_provider_auth_models::account_lifecycle::AccountLifecycleEffect;
        use operations::AccountLifecycleStore as _;
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("auth/subscriptions.json");
        let observation = root.path().join("provider/observations.json");
        let legacy = root.path().join("provider/legacy.json");
        let key = "a".repeat(64);
        let registered = registered_provider();
        let (config, runtime) = native_accounts(root.path());
        let acquire = || {
            operations::NativeAccountLifecycleStore::acquire_at(
                registry.clone(),
                root.path(),
                &key,
                observation.clone(),
                legacy.clone(),
            )
            .unwrap()
        };
        {
            let mut store = acquire();
            let before = store.snapshot().unwrap();
            let mut after = before.clone();
            after.runtime = runtime.clone();
            store.commit(&before, &after).unwrap();
            drop(store);
        }
        // Both are reviewed against the same pre-mutation registry snapshot.
        let plans = ["personal", "work"]
            .map(|profile| retained_plan(profile, &config, &runtime, &registered));
        assert_capability_rejection(&plans[0], &config, &runtime, &registered);
        let mut changed = config.clone();
        changed
            .auth
            .profiles
            .get_mut("personal")
            .unwrap()
            .settings
            .insert(
                "vault".into(),
                root.path()
                    .join("other-vault")
                    .to_string_lossy()
                    .into_owned(),
            );
        let stale = execute_guarded_local(&plans[0], &changed, &registered, acquire());
        assert_eq!(
            stale.error,
            Some(bcode_provider_auth_models::account_lifecycle::AccountLifecycleError::StalePlan)
        );
        assert_eq!(stale.credentials, AccountLifecycleEffect::NotAttempted);
        assert_eq!(acquire().snapshot().unwrap().runtime.profiles.len(), 2);
        for plan in &plans {
            let outcome = execute_guarded_local(plan, &config, &registered, acquire());
            assert_eq!(outcome.error, None);
            assert_eq!(outcome.credentials, AccountLifecycleEffect::Complete);
            assert_eq!(outcome.runtime, AccountLifecycleEffect::Complete);
        }
        assert!(acquire().snapshot().unwrap().runtime.profiles.is_empty());
        assert!(!root.path().join("absent-vault").exists());
    }

    #[test]
    fn account_only_route_publication_cannot_reset_to_configuration() {
        let mut before = bcode_config::account_settings::AccountSettings::default();
        let only = bcode_provider_auth_models::AccountRoute::Only {
            account: bcode_provider_auth_models::AccountId {
                provider_id: "test.plugin".into(),
                profile: "personal".into(),
            },
        };
        before.sessions.insert("session".into(), only);
        assert!(preserves_only_routes(&before, &before));
        let mut replacement = before.clone();
        replacement.sessions.insert(
            "session".into(),
            bcode_provider_auth_models::AccountRoute::FollowConfiguration,
        );
        assert!(!preserves_only_routes(&before, &replacement));
        replacement.sessions.clear();
        assert!(!preserves_only_routes(&before, &replacement));
    }

    #[test]
    fn invalidation_keeps_account_identity_but_discards_retained_environment() {
        let mut context = bcode_model::ProviderRequestContext {
            auth_profile: Some("personal".into()),
            account_routing_frozen: true,
            env: std::collections::BTreeMap::from([("TOKEN".into(), "test-secret".into())]),
            ..Default::default()
        };
        forget_material(&mut context, "work");
        assert!(context.account_routing_frozen);
        assert_eq!(context.env.len(), 1);
        forget_material(&mut context, "personal");
        assert_eq!(context.auth_profile.as_deref(), Some("personal"));
        assert!(!context.account_routing_frozen);
        assert!(context.env.is_empty());
        assert!(context.auth.is_none());
    }

    #[tokio::test]
    async fn invalidated_selected_account_never_becomes_an_implicit_fallback() {
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        state.account_lifecycle_invalidations.lock().await.insert(
            bcode_provider_auth_models::AccountId {
                provider_id: "test.plugin".into(),
                profile: "personal".into(),
            },
            "test".into(),
        );
        let mut selection = crate::SessionModelSelection {
            provider_plugin_id: Some("test.plugin".into()),
            provider_context: bcode_model::ProviderRequestContext {
                auth_profile: Some("personal".into()),
                env: std::collections::BTreeMap::from([(
                    "TOKEN".into(),
                    "stale-test-secret".into(),
                )]),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(
            refresh_invalidated_context(
                &state,
                &bcode_config::BcodeConfig::default(),
                &mut selection
            )
            .await
            .is_err()
        );
        assert_eq!(
            selection.provider_context.auth_profile.as_deref(),
            Some("personal")
        );
        assert!(selection.provider_context.auth.is_none());
        assert!(selection.provider_context.env.is_empty());
    }
}
