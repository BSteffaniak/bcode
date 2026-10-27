//! Session-bound account application operations and turn snapshot preparation.

use super::{ServerState, SessionId, SessionModelSelection, session_model_selection};
use bcode_config::account_settings::{self, AccountMetadata, AccountSettings};
use bcode_provider_auth_models::{
    AccountId, AccountPause, AccountPauseState, AccountRoute, AccountRouting, AccountRoutingScope,
    RenameAccountRequest, SetAccountPauseRequest, SetAccountRoutingRequest,
};

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

struct Context {
    config: bcode_config::BcodeConfig,
    registry: bcode_config::RuntimeAuthSubscriptions,
    key: String,
    preset: Option<String>,
    provider: Option<String>,
    created: u64,
}

async fn context(state: &ServerState, session: SessionId) -> Result<Context, String> {
    let summary = state
        .sessions
        .session_summary_non_owning(session)
        .await
        .map_err(|_| "session unavailable")?;
    let config = state.session_config(session).await;
    let selection = session_model_selection(state, session).await;
    let preset = selection
        .provider_context
        .model_profile
        .clone()
        .or_else(|| config.model.profile.clone())
        .filter(|name| config.model.profiles.contains_key(name));
    let launch = state
        .session_config_launches
        .lock()
        .await
        .get(&session)
        .cloned()
        .unwrap_or_else(|| bcode_config::config_launch_context(&state.state_root));
    let key = account_settings::configuration_key(&launch, config.active_context.as_deref())
        .map_err(|_| "configuration unavailable")?;
    let registry = if config.active_context.is_some() {
        bcode_config::RuntimeAuthSubscriptions::default()
    } else {
        bcode_config::try_load_runtime_auth_subscriptions()
            .map_err(|_| "account registry unavailable")?
    };
    Ok(Context {
        config,
        registry,
        key,
        preset,
        provider: selection.provider_plugin_id,
        created: summary.created_at_ms,
    })
}

fn route(settings: &AccountSettings, session: SessionId, context: &Context) -> AccountRoute {
    settings
        .sessions
        .get(&session.to_string())
        .cloned()
        .unwrap_or_else(|| {
            context
                .preset
                .as_ref()
                .and_then(|preset| settings.default_history.get(preset))
                .and_then(|history| {
                    history
                        .iter()
                        .rev()
                        .find(|change| change.changed_at_ms < context.created)
                })
                .map_or(AccountRoute::FollowConfiguration, |change| {
                    change.route.clone()
                })
        })
}

pub async fn read(state: &ServerState, session: SessionId) -> Result<AccountRouting, String> {
    let context = context(state, session).await?;
    let settings = account_settings::load(&state.state_root, &context.key)
        .map_err(|_| "account settings unavailable")?;
    let session_route = route(&settings, session, &context);
    let new_session_route = context
        .preset
        .as_ref()
        .and_then(|preset| settings.defaults.get(preset))
        .cloned()
        .unwrap_or(AccountRoute::FollowConfiguration);
    Ok(AccountRouting {
        accounts: bcode_provider_auth::account_routing::accounts(
            &context.config,
            &context.registry,
            &settings,
            now(),
        ),
        pools: bcode_provider_auth::auth_pool_summaries_with_subscriptions(
            &context.config,
            &context.registry,
        ),
        model_profile: context.preset,
        session_route,
        new_session_route,
        last_used: settings
            .last_used
            .get(&session.to_string())
            .map(|usage| usage.account.clone()),
        running_turn: state.session_current_turn(session).await.is_some(),
    })
}

pub async fn set_route(
    state: &ServerState,
    session: SessionId,
    request: SetAccountRoutingRequest,
) -> Result<AccountRouting, String> {
    let context = context(state, session).await?;
    let settings = account_settings::load(&state.state_root, &context.key)
        .map_err(|_| "account settings unavailable")?;
    let visible = bcode_provider_auth::account_routing::accounts(
        &context.config,
        &context.registry,
        &settings,
        now(),
    );
    bcode_provider_auth::account_routing::validate_route(
        &request.route,
        context.provider.as_deref(),
        &visible,
    )?;
    if request.scope != AccountRoutingScope::Session && context.preset.is_none() {
        return Err("new-session routing requires a selected model preset".into());
    }
    account_settings::update(&state.state_root, &context.key, |settings| {
        if request.scope != AccountRoutingScope::NewSessions {
            settings
                .sessions
                .insert(session.to_string(), request.route.clone());
        }
        if request.scope != AccountRoutingScope::Session {
            let preset = context.preset.as_ref().expect("validated preset");
            // Timestamp gates ensure old, previously unopened sessions cannot inherit a new default.
            settings
                .defaults
                .insert(preset.clone(), request.route.clone());
            settings
                .default_history
                .entry(preset.clone())
                .or_default()
                .push(account_settings::DefaultRoute {
                    changed_at_ms: u64::try_from(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis(),
                    )
                    .unwrap_or(u64::MAX),
                    route: request.route.clone(),
                });
        }
        Ok(())
    })
    .map_err(|_| "could not persist account routing")?;
    read(state, session).await
}

enum LabelChange {
    Keep,
    Set(Option<String>),
}

async fn metadata(
    state: &ServerState,
    session: SessionId,
    account: AccountId,
    label: LabelChange,
    pause: Option<AccountPauseState>,
) -> Result<AccountRouting, String> {
    let context = context(state, session).await?;
    let settings = account_settings::load(&state.state_root, &context.key)
        .map_err(|_| "account settings unavailable")?;
    let visible = bcode_provider_auth::account_routing::accounts(
        &context.config,
        &context.registry,
        &settings,
        now(),
    );
    if !visible.iter().any(|entry| entry.account == account) {
        return Err("account unavailable in this configuration".into());
    }
    account_settings::update(&state.state_root, &context.key, |settings| {
        let index = settings
            .accounts
            .iter()
            .position(|entry| entry.account == account)
            .unwrap_or_else(|| {
                settings.accounts.push(AccountMetadata {
                    account,
                    label: None,
                    pause: AccountPauseState::Eligible,
                });
                settings.accounts.len() - 1
            });
        let entry = &mut settings.accounts[index];
        if let LabelChange::Set(label) = label {
            entry.label = label;
        }
        if let Some(pause) = pause {
            entry.pause = pause;
        }
        Ok(())
    })
    .map_err(|_| "could not persist account metadata")?;
    read(state, session).await
}

pub async fn pause(
    state: &ServerState,
    session: SessionId,
    request: SetAccountPauseRequest,
) -> Result<AccountRouting, String> {
    let pause = match request.pause {
        AccountPause::Resume => AccountPauseState::Eligible,
        AccountPause::Manual => AccountPauseState::Manual,
        AccountPause::OneHour => AccountPauseState::Until {
            unix_seconds: now().saturating_add(3600),
        },
        AccountPause::FourHours => AccountPauseState::Until {
            unix_seconds: now().saturating_add(14400),
        },
    };
    metadata(
        state,
        session,
        request.account,
        LabelChange::Keep,
        Some(pause),
    )
    .await
}

pub async fn rename(
    state: &ServerState,
    session: SessionId,
    request: RenameAccountRequest,
) -> Result<AccountRouting, String> {
    if request.label.as_ref().is_some_and(|label| {
        label.trim().is_empty() || label.len() > 128 || label.chars().any(char::is_control)
    }) {
        return Err("account label must be 1–128 bytes without control characters".into());
    }
    metadata(
        state,
        session,
        request.account,
        LabelChange::Set(request.label),
        None,
    )
    .await
}

pub async fn freeze(
    state: &ServerState,
    session: SessionId,
    selection: &mut SessionModelSelection,
) -> Result<(), String> {
    let context = context(state, session).await?;
    if selection.provider_context.account_routing_frozen {
        return Ok(());
    }
    let settings = account_settings::load(&state.state_root, &context.key)
        .map_err(|_| "account settings unavailable")?;
    let route = route(&settings, session, &context);
    // Capture inherited defaults once. Subsequent default changes cannot affect this session.
    if !settings.sessions.contains_key(&session.to_string()) {
        account_settings::update(&state.state_root, &context.key, |settings| {
            settings
                .sessions
                .entry(session.to_string())
                .or_insert_with(|| route.clone());
            Ok(())
        })
        .map_err(|_| "could not freeze session routing")?;
    }
    bcode_provider_auth::account_routing::freeze(
        &context.config,
        &context.registry,
        &settings,
        &route,
        selection.provider_plugin_id.as_deref(),
        &mut selection.provider_context,
        now(),
    )?;
    selection.native_default = false;
    Ok(())
}

/// Retain only successful actual-account evidence, never inferred next selection.
pub async fn record_usage(
    state: &ServerState,
    session: SessionId,
    observation: &bcode_model::ModelAvailabilityObservation,
) {
    if observation.status != bcode_model::ModelCheckStatus::Working {
        return;
    }
    let Some(profile) = &observation.scope.profile else {
        return;
    };
    let Ok(context) = context(state, session).await else {
        return;
    };
    let Some(provider) = context.provider.as_ref() else {
        return;
    };
    let account = AccountId {
        provider_id: provider.clone(),
        profile: profile.clone(),
    };
    let Ok(settings) = account_settings::load(&state.state_root, &context.key) else {
        return;
    };
    if !bcode_provider_auth::account_routing::accounts(
        &context.config,
        &context.registry,
        &settings,
        now(),
    )
    .iter()
    .any(|entry| entry.account == account)
    {
        return;
    }
    if let Err(error) = account_settings::update(&state.state_root, &context.key, |settings| {
        let key = session.to_string();
        if settings
            .last_used
            .get(&key)
            .is_none_or(|usage| usage.observed_at_ms <= observation.observed_at_ms)
        {
            settings.last_used.insert(
                key,
                account_settings::AccountUsage {
                    account,
                    observed_at_ms: observation.observed_at_ms,
                },
            );
        }
        Ok(())
    }) {
        tracing::warn!(%error, "could not retain account usage evidence");
    }
}

/// Capture inherited routing at creation without materializing credentials.
pub async fn initialize(state: &ServerState, session: SessionId) -> Result<(), String> {
    let context = context(state, session).await?;
    account_settings::update(&state.state_root, &context.key, |settings| {
        let inherited = context
            .preset
            .as_ref()
            .and_then(|preset| settings.defaults.get(preset))
            .cloned()
            .unwrap_or(AccountRoute::FollowConfiguration);
        settings
            .sessions
            .entry(session.to_string())
            .or_insert(inherited);
        Ok(())
    })
    .map_err(|_| "could not initialize account routing".into())
}

#[cfg(test)]
mod tests {
    use super::{context, freeze, initialize, pause, read, rename, set_route};
    use crate::SessionModelSelection;
    use bcode_provider_auth_models::{
        AccountId, AccountPause, AccountPauseState, AccountRoute, AccountRoutingScope,
        RenameAccountRequest, SetAccountPauseRequest, SetAccountRoutingRequest,
    };

    fn fixture() -> (
        tempfile::TempDir,
        crate::ServerState,
        bcode_config::BcodeConfig,
    ) {
        let root = tempfile::tempdir().unwrap();
        let sessions = bcode_session::SessionManager::persistent(root.path()).unwrap();
        let mut state = crate::tests::test_server_state(sessions);
        state.state_root = root.path().to_path_buf();
        let mut config = bcode_config::BcodeConfig {
            active_context: Some("isolated-account-test".into()),
            ..bcode_config::BcodeConfig::default()
        };
        config.model.profile = Some("test".into());
        config.model.profiles.insert(
            "test".into(),
            bcode_config::ModelProfileConfig {
                provider_plugin_id: "test.plugin".into(),
                model_id: Some("model".into()),
                ..bcode_config::ModelProfileConfig::default()
            },
        );
        config.auth.profiles.insert(
            "work".into(),
            bcode_config::AuthProfileConfig {
                backend: "unavailable-test-backend".into(),
                provider_id: Some("test".into()),
                owner_plugin_id: Some("test.plugin".into()),
                ..bcode_config::AuthProfileConfig::default()
            },
        );
        (root, state, config)
    }

    async fn create(state: &crate::ServerState, root: &std::path::Path) -> crate::SessionId {
        state
            .sessions
            .create_session(None, root.to_path_buf())
            .await
            .unwrap()
            .id
    }

    async fn assert_frozen_selection_unchanged(
        state: &crate::ServerState,
        session: crate::SessionId,
    ) {
        let mut selection = SessionModelSelection::default();
        selection.provider_context.account_routing_frozen = true;
        let retained = selection.clone();
        freeze(state, session, &mut selection).await.unwrap();
        assert_eq!(selection, retained);
    }

    #[tokio::test]
    async fn account_routing_scopes_pause_labels_and_frozen_turns_are_independent() {
        let (root, state, config) = fixture();
        let original = create(&state, root.path()).await;
        let existing = create(&state, root.path()).await;
        for id in [original, existing] {
            state
                .session_configs
                .lock()
                .await
                .insert(id, config.clone());
            state.session_model_selections.lock().await.insert(
                id,
                SessionModelSelection {
                    provider_plugin_id: Some("test.plugin".into()),
                    ..SessionModelSelection::default()
                },
            );
            initialize(&state, id).await.unwrap();
        }
        let account = AccountId {
            provider_id: "test.plugin".into(),
            profile: "work".into(),
        };
        let only = AccountRoute::Only {
            account: account.clone(),
        };
        let result = set_route(
            &state,
            original,
            SetAccountRoutingRequest {
                scope: AccountRoutingScope::NewSessions,
                route: only.clone(),
            },
        )
        .await
        .unwrap();
        assert_eq!(result.session_route, AccountRoute::FollowConfiguration);
        assert_eq!(result.new_session_route, only);
        assert_eq!(
            read(&state, existing).await.unwrap().session_route,
            AccountRoute::FollowConfiguration
        );
        let future = state
            .sessions
            .create_session(None, root.path().to_path_buf())
            .await
            .unwrap()
            .id;
        state.session_configs.lock().await.insert(future, config);
        initialize(&state, future).await.unwrap();
        assert_eq!(read(&state, future).await.unwrap().session_route, only);
        rename(
            &state,
            original,
            RenameAccountRequest {
                account: account.clone(),
                label: Some("Work".into()),
            },
        )
        .await
        .unwrap();
        let result = pause(
            &state,
            original,
            SetAccountPauseRequest {
                account: account.clone(),
                pause: AccountPause::Manual,
            },
        )
        .await
        .unwrap();
        assert_eq!(result.accounts[0].label, "Work");
        assert_eq!(
            read(&state, future).await.unwrap().accounts[0].pause,
            AccountPauseState::Manual
        );
        assert_frozen_selection_unchanged(&state, original).await;
        pause(
            &state,
            original,
            SetAccountPauseRequest {
                account,
                pause: AccountPause::Resume,
            },
        )
        .await
        .unwrap();
        assert_eq!(read(&state, future).await.unwrap().session_route, only);
        let same_configuration = context(&state, original).await.unwrap().key
            == context(&state, future).await.unwrap().key;
        drop(state);
        assert!(same_configuration);
    }
}
