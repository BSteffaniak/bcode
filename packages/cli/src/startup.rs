//! Quick startup routing at the CLI boundary; authentication stays provider-owned.

use super::{CliError, load_cli_plugin_host};
use bcode_config::BcodeConfig;
use bcode_provider_auth::startup::{StartupAuthCandidate, discover_candidates};
use std::io::IsTerminal as _;

/// Interactive startup is opt-in for terminal launches, not automation.
const fn interactive_startup(terminal: bool, ci: bool, opted_out: bool) -> bool {
    terminal && !ci && !opted_out
}

pub async fn advanced(no_discovery: bool) -> Result<(), CliError> {
    Box::pin(super::handle_onboard_command(&super::OnboardOptions {
        no_credential_discovery: no_discovery,
        ..super::OnboardOptions::default()
    }))
    .await
}

pub async fn route(no_discovery: bool) -> Result<bool, CliError> {
    if !interactive_startup(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        std::env::var_os("CI").is_some(),
        std::env::var_os("BCODE_NO_ONBOARD").is_some(),
    ) {
        return Ok(false);
    }
    let config = bcode_config::load_config()?;
    let remembered = bcode_config::startup::load();
    let mut host = load_cli_plugin_host()?;
    register_enabled(&host)?;
    let readiness = local_readiness(&config, &host);
    let explicit_selection = has_explicit_selection(&config);
    let route = startup_route(readiness.is_ok(), explicit_selection, remembered.is_ok());
    if route == StartupRoute::Continue {
        host.deactivate_all()?;
        return Ok(false);
    }
    let discovery_enabled = config
        .onboarding
        .credential_discovery_enabled(no_discovery, &bcode_config::ProcessConfigEnvironment);
    let candidates = collect_candidates(&host, discovery_enabled);
    let choices = if route
        == (StartupRoute::Recover {
            allow_choices: true,
        }) {
        candidate_selections(&host, &config, &candidates)
    } else {
        Vec::new()
    };
    host.deactivate_all()?;
    let input = quick_input(remembered.is_ok(), readiness, &candidates, &choices);
    match Box::pin(bcode_tui::run_quick_onboarding(input)).await? {
        bcode_tui::QuickOnboardingOutcome::Advanced => advanced(no_discovery).await?,
        bcode_tui::QuickOnboardingOutcome::Close => {}
        bcode_tui::QuickOnboardingOutcome::Selected(index) => {
            Box::pin(launch_selected(index, remembered.is_ok(), route, &choices)).await?;
        }
        bcode_tui::QuickOnboardingOutcome::Connected {
            provider_id,
            profile,
        } => {
            Box::pin(launch_connected(
                &config,
                provider_id,
                profile,
                explicit_selection,
                remembered.is_ok(),
                no_discovery,
            ))
            .await?;
        }
    }
    Ok(true)
}

async fn launch_selected(
    index: usize,
    state_valid: bool,
    route: StartupRoute,
    choices: &[(
        bcode_config::startup::StartupSelection,
        StartupAuthCandidate,
    )],
) -> Result<(), CliError> {
    if !state_valid {
        return Err(CliError::InvalidArguments(
            "Remembered startup state requires explicit maintenance".to_owned(),
        ));
    }
    let (_, candidate) = choices.get(index).ok_or_else(|| {
        CliError::InvalidArguments("Selected startup connection is unavailable".to_owned())
    })?;
    let mut host = load_cli_plugin_host()?;
    register_enabled(&host)?;
    let provider = super::registered_auth_provider(&host, &candidate.provider_id)?;
    let bcode_provider_auth::startup::StartupCredentialSource::Environment { name } =
        &candidate.source
    else {
        unreachable!()
    };
    bcode_provider_auth::startup::resolve_selected_environment(
        &provider.contribution,
        &candidate.method_id,
        &candidate.credential_id,
        name,
        |name| std::env::var(name).ok().map(zeroize::Zeroizing::new),
    )
    .map_err(|_| {
        CliError::InvalidArguments("Selected launch credential is no longer available".to_owned())
    })?;
    host.deactivate_all()?;
    consent_selection(
        &bcode_tui::QuickOnboardingOutcome::Selected(index),
        route,
        choices,
        |selection| {
            bcode_config::startup::save(selection)
                .map(|_| ())
                .map_err(|_| "Startup selection could not be saved; existing state was preserved")
        },
    )
    .map_err(|message| CliError::InvalidArguments(message.to_owned()))?;
    Box::pin(super::run_new_session_tui(
        None,
        bcode_tui::TuiLaunchOptions::default(),
    ))
    .await?;
    Ok(())
}

async fn launch_connected(
    config: &BcodeConfig,
    provider_id: String,
    profile: String,
    explicit_selection: bool,
    state_valid: bool,
    no_discovery: bool,
) -> Result<(), CliError> {
    if !state_valid {
        return Err(CliError::InvalidArguments("Connection enrolled, but remembered startup state requires explicit maintenance before selecting it".to_owned()));
    }
    let mut host = load_cli_plugin_host()?;
    register_enabled(&host)?;
    if explicit_selection {
        let repaired = bcode_config::load_config()?;
        local_readiness(&repaired, &host).map_err(|_| CliError::InvalidArguments("Connection enrolled, but the explicit model/account selection still needs repair. Use Advanced settings; it was not replaced.".to_owned()))?;
        host.deactivate_all()?;
        Box::pin(super::run_new_session_tui(
            None,
            bcode_tui::TuiLaunchOptions::default(),
        ))
        .await?;
        return Ok(());
    }
    let provider = super::registered_auth_provider(&host, &provider_id)?;
    let runtime = bcode_config::try_load_runtime_auth_subscriptions()?;
    let resolved = bcode_provider_auth::resolve_auth_provider_profile(
        config,
        &provider_id,
        &provider.plugin_id,
        Some(&profile),
        &runtime,
    )
    .map_err(|_| {
        CliError::InvalidArguments("Enrolled account metadata could not be resolved".to_owned())
    })?;
    let method = super::resolved_auth_method(&provider, &resolved)?;
    let selections = model_choices(
        &host,
        config,
        &provider_id,
        method.method_id(),
        Some(profile),
        None,
    );
    if selections.is_empty() {
        host.deactivate_all()?;
        Box::pin(super::handle_onboard_command(&super::OnboardOptions {
            provider: Some(provider_id),
            no_credential_discovery: no_discovery,
            experience_mode: super::OnboardExperienceMode::ControlCenter,
            ..super::OnboardOptions::default()
        }))
        .await?;
        return Ok(());
    }
    host.deactivate_all()?;
    let outcome = Box::pin(bcode_tui::run_quick_onboarding(enrolled_input(
        &provider.contribution.display_name,
        &selections,
    )))
    .await?;
    match outcome {
        bcode_tui::QuickOnboardingOutcome::Advanced => {
            return Box::pin(advanced(no_discovery)).await;
        }
        bcode_tui::QuickOnboardingOutcome::Connected {
            provider_id,
            profile,
        } => {
            return Box::pin(launch_connected(
                config,
                provider_id,
                profile,
                false,
                state_valid,
                no_discovery,
            ))
            .await;
        }
        _ => {}
    }
    if !consent_enrolled_selection(&outcome, &selections, |selection| {
        bcode_config::startup::save(selection)
            .map(|_| ())
            .map_err(|_| "Startup selection could not be saved; existing state was preserved")
    })
    .map_err(|message| CliError::InvalidArguments(message.to_owned()))?
    {
        return Ok(());
    }
    Box::pin(super::run_new_session_tui(
        None,
        bcode_tui::TuiLaunchOptions::default(),
    ))
    .await?;
    Ok(())
}

fn enrolled_input(
    provider_name: &str,
    selections: &[bcode_config::startup::StartupSelection],
) -> bcode_tui::QuickOnboardingInput {
    bcode_tui::QuickOnboardingInput {
        choices: selections.iter().enumerate().map(|(index, selection)| bcode_tui::QuickConnectionChoice {
            label: format!("{provider_name} — {}{}", selection.model_id, if index == 0 { " (recommended)" } else { "" }),
            detail: "Use the enrolled account with this model. API usage may incur provider charges; credentials have not been checked remotely.".to_owned(),
        }).collect(),
        recovery: None,
    }
}

fn consent_enrolled_selection(
    outcome: &bcode_tui::QuickOnboardingOutcome,
    selections: &[bcode_config::startup::StartupSelection],
    mut save: impl FnMut(&bcode_config::startup::StartupSelection) -> Result<(), &'static str>,
) -> Result<bool, &'static str> {
    let bcode_tui::QuickOnboardingOutcome::Selected(index) = outcome else {
        return Ok(false);
    };
    let selection = selections
        .get(*index)
        .ok_or("Selected enrolled model is unavailable")?;
    save(selection)?;
    Ok(true)
}

fn quick_input(
    state_valid: bool,
    readiness: Result<(), &'static str>,
    candidates: &[StartupAuthCandidate],
    choices: &[(
        bcode_config::startup::StartupSelection,
        StartupAuthCandidate,
    )],
) -> bcode_tui::QuickOnboardingInput {
    let mut recovery = if state_valid {
        readiness.err().map(str::to_owned)
    } else {
        Some("Remembered startup state needs maintenance. It has been preserved; use Advanced settings to configure an explicit connection.".to_owned())
    };
    let unavailable_recommendations: std::collections::BTreeSet<_> = candidates
        .iter()
        .filter(|candidate| {
            candidate.availability
                == bcode_provider_auth::startup::StartupCredentialAvailability::Available
                && !choices.iter().any(|(_, offered)| offered == *candidate)
        })
        .map(|candidate| candidate.provider_name.as_str())
        .collect();
    if !unavailable_recommendations.is_empty() {
        let message = format!(
            "Detected local sources for {}. No quick model recommendation is available for these sources; use Advanced to choose a model or explicitly import a credential. Nothing was authenticated or selected.",
            unavailable_recommendations
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ")
        );
        recovery = Some(recovery.map_or_else(
            || message.clone(),
            |existing| format!("{existing}\n{message}"),
        ));
    }
    bcode_tui::QuickOnboardingInput {
        choices: if state_valid {
            choices
                .iter()
                .enumerate()
                .map(|(index, (selection, candidate))| bcode_tui::QuickConnectionChoice {
                    label: format!("{} — {}{}", candidate.provider_name, selection.model_id,
                        if choices[..index].iter().any(|(_, previous)| previous == candidate) { "" } else { " (recommended)" }),
                    detail: match &candidate.source {
                        bcode_provider_auth::startup::StartupCredentialSource::Environment {
                            name,
                        } => format!("Found {name}; not authenticated. API usage may incur provider charges. The variable must remain available in each launch environment; no key is copied."),
                        bcode_provider_auth::startup::StartupCredentialSource::Import {
                            ..
                        } => unreachable!(),
                    },
                })
                .collect()
        } else {
            Vec::new()
        },
        recovery,
    }
}

fn collect_candidates(host: &bcode_plugin::PluginHost, enabled: bool) -> Vec<StartupAuthCandidate> {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return Vec::new();
    };
    discover_candidates(
        host.auth_provider_registry()
            .providers()
            .into_iter()
            .map(|provider| &provider.contribution),
        enabled,
        &home,
        |name| std::env::var(name).ok().map(zeroize::Zeroizing::new),
    )
}

pub fn prepare_remembered_source() -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    if config.startup_selection_applied {
        let mut host = load_cli_plugin_host()?;
        register_enabled(&host)?;
        host.deactivate_all()?;
    }
    Ok(())
}

pub fn register_enabled(host: &bcode_plugin::PluginHost) -> Result<(), CliError> {
    bcode_provider_auth::startup::set_enabled_providers(
        host.auth_provider_registry()
            .providers()
            .into_iter()
            .map(|provider| (provider.plugin_id.as_str(), &provider.contribution)),
    )
    .map_err(|_| {
        CliError::InvalidArguments(
            "Enabled provider authentication declarations are invalid".to_owned(),
        )
    })
}

fn candidate_selections(
    host: &bcode_plugin::PluginHost,
    config: &BcodeConfig,
    candidates: &[StartupAuthCandidate],
) -> Vec<(
    bcode_config::startup::StartupSelection,
    StartupAuthCandidate,
)> {
    candidates
        .iter()
        .flat_map(|candidate| {
            if candidate.availability
                != bcode_provider_auth::startup::StartupCredentialAvailability::Available
                || !candidate.completes_method
                || !matches!(
                    candidate.source,
                    bcode_provider_auth::startup::StartupCredentialSource::Environment { .. }
                )
            {
                return Vec::new();
            }
            model_choices(
                host,
                config,
                &candidate.provider_id,
                &candidate.method_id,
                None,
                Some(candidate),
            )
            .into_iter()
            .map(|selection| (selection, candidate.clone()))
            .collect()
        })
        .collect()
}

fn model_choices(
    host: &bcode_plugin::PluginHost,
    config: &BcodeConfig,
    provider_id: &str,
    method_id: &str,
    profile: Option<String>,
    candidate: Option<&StartupAuthCandidate>,
) -> Vec<bcode_config::startup::StartupSelection> {
    let Some(provider) = host.auth_provider(provider_id) else {
        return Vec::new();
    };
    let response: Result<bcode_model::StartupModelChoicesResponse, _> = host.invoke_service_json(
        &provider.plugin_id,
        bcode_model::MODEL_PROVIDER_INTERFACE_ID,
        bcode_model::OP_STARTUP_MODEL_CHOICES,
        &bcode_model::StartupRecommendationRequest {
            auth_provider_id: provider_id.to_owned(),
            method_id: method_id.to_owned(),
        },
    );
    let mut selections: Vec<_> = response
        .ok()
        .into_iter()
        .flat_map(|response| response.choices)
        .filter_map(|recommended| {
            recommended_selection(
                config,
                &provider.plugin_id,
                provider_id,
                method_id,
                profile.clone(),
                candidate,
                bcode_model::StartupRecommendationResponse {
                    recommendation: Some(recommended),
                },
            )
        })
        .collect();
    if selections.is_empty() {
        selections.extend(recommendation(
            host,
            config,
            provider_id,
            method_id,
            profile,
            candidate,
        ));
    }
    selections
}

fn recommendation(
    host: &bcode_plugin::PluginHost,
    config: &BcodeConfig,
    provider_id: &str,
    method_id: &str,
    profile: Option<String>,
    candidate: Option<&StartupAuthCandidate>,
) -> Option<bcode_config::startup::StartupSelection> {
    let provider = host.auth_provider(provider_id)?.clone();
    let response: bcode_model::StartupRecommendationResponse = host
        .invoke_service_json(
            &provider.plugin_id,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_STARTUP_RECOMMENDATION,
            &bcode_model::StartupRecommendationRequest {
                auth_provider_id: provider_id.to_owned(),
                method_id: method_id.to_owned(),
            },
        )
        .ok()?;
    recommended_selection(
        config,
        &provider.plugin_id,
        provider_id,
        method_id,
        profile,
        candidate,
        response,
    )
}

fn recommended_selection(
    config: &BcodeConfig,
    owner_plugin_id: &str,
    provider_id: &str,
    method_id: &str,
    profile: Option<String>,
    candidate: Option<&StartupAuthCandidate>,
    response: bcode_model::StartupRecommendationResponse,
) -> Option<bcode_config::startup::StartupSelection> {
    let recommended = response.recommendation?;
    if recommended.provider_plugin_id != owner_plugin_id || recommended.model_id.trim().is_empty() {
        return None;
    }
    let environment = candidate.and_then(|candidate| match &candidate.source {
        bcode_provider_auth::startup::StartupCredentialSource::Environment { name } => {
            Some(name.clone())
        }
        bcode_provider_auth::startup::StartupCredentialSource::Import { .. } => None,
    });
    Some(bcode_config::startup::StartupSelection {
        provider_plugin_id: recommended.provider_plugin_id,
        model_id: recommended.model_id,
        base_url: recommended.settings.get("base_url").cloned(),
        dialect: recommended.settings.get("dialect").cloned(),
        auth_mode: recommended.settings.get("auth_mode").cloned(),
        catalog_provider_id: recommended.settings.get("catalog_provider_id").cloned(),
        auth_profile: profile,
        environment_auth_provider: environment.as_ref().map(|_| provider_id.to_owned()),
        environment_method: environment.as_ref().map(|_| method_id.to_owned()),
        environment_credential: candidate
            .filter(|_| environment.is_some())
            .map(|candidate| candidate.credential_id.clone()),
        environment_variable: environment,
        context: config.active_context.clone(),
    })
}

fn has_explicit_selection(config: &BcodeConfig) -> bool {
    let selection = config.resolved_model_selection();
    !config.startup_selection_applied
        && (selection.provider_plugin_id.is_some()
            || selection.model_id.is_some()
            || selection.model_profile.is_some()
            || selection.auth_profile.is_some()
            || selection.auth_pool.is_some()
            || !selection.settings.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupRoute {
    Continue,
    Recover { allow_choices: bool },
}

const fn startup_route(ready: bool, explicit: bool, state_valid: bool) -> StartupRoute {
    if ready {
        StartupRoute::Continue
    } else {
        StartupRoute::Recover {
            allow_choices: !explicit && state_valid,
        }
    }
}

fn check_model_readiness(
    selection: &bcode_config::ResolvedModelSelection,
    service: impl FnOnce(
        &bcode_model::StartupReadinessRequest,
    ) -> Option<bcode_model::StartupReadinessResponse>,
) -> Result<(), &'static str> {
    let request = bcode_model::StartupReadinessRequest {
        model_id: selection
            .model_id
            .clone()
            .ok_or("Choose a model to get started.")?,
        settings: selection.settings.clone(),
    };
    match service(&request).map(|response| response.model) {
        Some(bcode_model::StartupModelReadiness::Unavailable) => Err(
            "The selected model is missing or unsupported for this provider. Choose a supported model in Advanced settings; no replacement was selected.",
        ),
        _ => Ok(()),
    }
}

fn consent_selection(
    outcome: &bcode_tui::QuickOnboardingOutcome,
    route: StartupRoute,
    choices: &[(
        bcode_config::startup::StartupSelection,
        StartupAuthCandidate,
    )],
    mut save: impl FnMut(&bcode_config::startup::StartupSelection) -> Result<(), &'static str>,
) -> Result<bool, &'static str> {
    let bcode_tui::QuickOnboardingOutcome::Selected(index) = outcome else {
        return Ok(false);
    };
    if route
        != (StartupRoute::Recover {
            allow_choices: true,
        })
    {
        return Err("Startup selection requires explicit recovery; existing state was preserved");
    }
    let (selection, _) = choices
        .get(*index)
        .ok_or("Selected startup connection is unavailable")?;
    save(selection)?;
    Ok(true)
}

fn local_readiness(
    config: &BcodeConfig,
    host: &bcode_plugin::PluginHost,
) -> Result<(), &'static str> {
    let selection = config.resolved_model_selection();
    selection
        .validate_selection()
        .map_err(|_| "Choose a provider connection to get started.")?;
    let plugin_id = selection
        .provider_plugin_id
        .as_deref()
        .ok_or("Choose a provider connection to get started.")?;
    if !host.has_service(plugin_id, bcode_model::MODEL_PROVIDER_INTERFACE_ID) {
        return Err(
            "The selected model provider is disabled or unavailable. Enable that provider or choose a connection explicitly.",
        );
    }
    check_model_readiness(&selection, |request| {
        host.invoke_service_json::<_, bcode_model::StartupReadinessResponse>(
            plugin_id,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_STARTUP_READINESS,
            request,
        )
        .ok()
    })?;
    bcode_provider_auth::inspect_auth_selection(&bcode_provider_auth::ProviderRequestContextResolution {
        config,
        selection: selection.clone(),
    }).map_err(|_| "The selected authentication account needs repair. Reconnect that account or choose another explicitly.")?;
    let context = bcode_provider_auth::try_resolve_provider_request_context(
        bcode_provider_auth::ProviderRequestContextResolution {
            config,
            selection: selection.clone(),
        },
    )
    .map_err(|_| "The selected credentials are unavailable. Reconnect the selected account.")?;
    if let Some(auth) = &context.auth
        && !auth.credentials.is_empty()
    {
        return Ok(());
    }
    if selection.auth_profile.is_some() || selection.auth_pool.is_some() {
        return Err(
            "The selected account has no locally available credentials. Reconnect that account; other environment accounts were not selected.",
        );
    }
    for provider in host
        .auth_provider_registry()
        .providers()
        .into_iter()
        .filter(|provider| provider.plugin_id == plugin_id)
    {
        if bcode_provider_auth::resolve_declared_environment_context(&provider.contribution).is_ok()
        {
            return Ok(());
        }
    }
    Err(
        "No locally available credentials for the selected provider. Connect an account; authentication has not been checked remotely.",
    )
}

#[cfg(test)]
mod tests {
    use super::interactive_startup;

    fn choices() -> Vec<(
        bcode_config::startup::StartupSelection,
        super::StartupAuthCandidate,
    )> {
        let selection = bcode_config::startup::StartupSelection {
            provider_plugin_id: "fake.provider".to_owned(),
            model_id: "exact-model".to_owned(),
            base_url: Some("https://fake.example/v1".to_owned()),
            dialect: Some("chat_completions".to_owned()),
            auth_mode: Some("api_key".to_owned()),
            catalog_provider_id: Some("fake".to_owned()),
            auth_profile: None,
            environment_variable: Some("FAKE_SELECTED".to_owned()),
            environment_auth_provider: Some("fake".to_owned()),
            environment_method: Some("key".to_owned()),
            environment_credential: Some("api_key".to_owned()),
            context: None,
        };
        let candidate = super::StartupAuthCandidate {
            provider_id: "fake".to_owned(),
            provider_name: "Fake".to_owned(),
            method_id: "key".to_owned(),
            method_name: "API key".to_owned(),
            credential_id: "api_key".to_owned(),
            completes_method: true,
            availability: bcode_provider_auth::startup::StartupCredentialAvailability::Available,
            source: bcode_provider_auth::startup::StartupCredentialSource::Environment {
                name: "FAKE_SELECTED".to_owned(),
            },
        };
        vec![(selection, candidate)]
    }

    struct EmptyEnvironment;
    impl bcode_config::ConfigEnvironment for EmptyEnvironment {
        fn var(&self, _: &str) -> Option<String> {
            None
        }
        fn var_os(&self, _: &str) -> Option<std::ffi::OsString> {
            None
        }
        fn current_dir(&self) -> std::path::PathBuf {
            std::path::PathBuf::from("/nonexistent")
        }
    }

    #[test]
    fn first_run_consent_then_remembered_return_does_not_reopen_setup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("startup.toml");
        let choices = choices();
        let route = super::startup_route(false, false, true);
        assert!(
            !super::consent_selection(
                &bcode_tui::QuickOnboardingOutcome::Close,
                route,
                &choices,
                |_| panic!("close is not consent")
            )
            .unwrap()
        );
        assert!(!path.exists());
        assert!(
            super::consent_selection(
                &bcode_tui::QuickOnboardingOutcome::Selected(0),
                route,
                &choices,
                |selection| {
                    bcode_config::startup::save_to(&path, selection)
                        .map(|_| ())
                        .map_err(|_| "save failed")
                }
            )
            .unwrap()
        );
        let remembered = bcode_config::startup::load_from(&path).unwrap().unwrap();
        let mut config = bcode_config::BcodeConfig::default();
        assert!(bcode_config::startup::apply_fallback(
            &mut config,
            &remembered,
            &EmptyEnvironment
        ));
        let resolved = config.resolved_model_selection_with_environment(&EmptyEnvironment);
        super::check_model_readiness(&resolved, |request| {
            assert_eq!(request.model_id, "exact-model");
            assert_eq!(request.settings["base_url"], "https://fake.example/v1");
            Some(bcode_model::StartupReadinessResponse {
                model: bcode_model::StartupModelReadiness::Available,
            })
        })
        .unwrap();
        assert_eq!(
            super::startup_route(true, false, true),
            super::StartupRoute::Continue
        );
    }

    #[test]
    fn unknown_optional_model_readiness_preserves_explicit_custom_model() {
        let selection = bcode_config::ResolvedModelSelection {
            model_id: Some("custom-exact".to_owned()),
            ..Default::default()
        };
        assert!(super::check_model_readiness(&selection, |_| None).is_ok());
        assert!(
            super::check_model_readiness(&selection, |_| Some(
                bcode_model::StartupReadinessResponse {
                    model: bcode_model::StartupModelReadiness::Unknown
                }
            ))
            .is_ok()
        );
        assert!(
            super::check_model_readiness(&selection, |_| Some(
                bcode_model::StartupReadinessResponse {
                    model: bcode_model::StartupModelReadiness::Unavailable
                }
            ))
            .is_err()
        );
        assert_eq!(selection.model_id.as_deref(), Some("custom-exact"));
    }

    #[test]
    fn partial_explicit_selection_prevents_candidate_save_and_fallback() {
        let choices = choices();
        for provider_only in [true, false] {
            let mut config = bcode_config::BcodeConfig::default();
            if provider_only {
                config.model.provider_plugin_id = Some("explicit.provider".to_owned());
            } else {
                config.model.model_id = Some("explicit-model".to_owned());
            }
            assert!(!bcode_config::startup::apply_fallback(
                &mut config,
                &choices[0].0,
                &EmptyEnvironment
            ));
            assert!(!config.startup_selection_applied);
            assert!(
                super::consent_selection(
                    &bcode_tui::QuickOnboardingOutcome::Selected(0),
                    super::startup_route(false, true, true),
                    &choices,
                    |_| panic!("explicit selection must not save fallback")
                )
                .is_err()
            );
        }
    }

    #[test]
    fn future_state_recovery_preserves_bytes_and_refuses_selection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("startup.toml");
        let future = "version = 999\nfuture_field = 'preserve'\n";
        std::fs::write(&path, future).unwrap();
        assert!(bcode_config::startup::load_from(&path).is_err());
        let choices = choices();
        assert!(
            super::consent_selection(
                &bcode_tui::QuickOnboardingOutcome::Selected(0),
                super::startup_route(false, false, false),
                &choices,
                |_| panic!("future state must not save")
            )
            .is_err()
        );
        assert!(bcode_config::startup::save_to(&path, &choices[0].0).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), future);
    }

    #[test]
    fn missing_selected_source_routes_to_recovery_without_automatic_save() {
        let choices = choices();
        let route = super::startup_route(false, false, true);
        assert_eq!(
            route,
            super::StartupRoute::Recover {
                allow_choices: true
            }
        );
        assert!(
            !super::consent_selection(
                &bcode_tui::QuickOnboardingOutcome::Close,
                route,
                &choices,
                |_| panic!("no automatic alternate account")
            )
            .unwrap()
        );
    }

    #[test]
    fn malformed_state_recovery_does_not_write_even_valid_choice() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("startup.toml");
        let original = "not valid = [toml";
        std::fs::write(&path, original).unwrap();
        let choices = choices();
        assert!(bcode_config::startup::load_from(&path).is_err());
        assert!(
            super::consent_selection(
                &bcode_tui::QuickOnboardingOutcome::Selected(0),
                super::startup_route(false, false, false),
                &choices,
                |_| panic!("malformed state must not save")
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn explicit_working_selection_continues_even_with_damaged_optional_state() {
        assert_eq!(
            super::startup_route(true, true, false),
            super::StartupRoute::Continue
        );
        assert_eq!(
            super::startup_route(false, true, false),
            super::StartupRoute::Recover {
                allow_choices: false
            }
        );
        assert!(
            super::consent_selection(
                &bcode_tui::QuickOnboardingOutcome::Selected(99),
                super::startup_route(false, false, true),
                &choices(),
                |_| panic!("invalid choice must not save")
            )
            .is_err()
        );
    }

    #[test]
    fn fake_offline_recommendation_preserves_owner_model_and_endpoint() {
        let choices = choices();
        let config = bcode_config::BcodeConfig::default();
        let response = bcode_model::StartupRecommendationResponse {
            recommendation: Some(bcode_model::StartupRecommendation {
                provider_plugin_id: "fake.provider".to_owned(),
                model_id: "recommended-exact".to_owned(),
                settings: std::collections::BTreeMap::from([
                    (
                        "base_url".to_owned(),
                        "https://billing.example/v1".to_owned(),
                    ),
                    ("dialect".to_owned(), "chat_completions".to_owned()),
                ]),
            }),
        };
        let selection = super::recommended_selection(
            &config,
            "fake.provider",
            "fake",
            "key",
            None,
            Some(&choices[0].1),
            response.clone(),
        )
        .unwrap();
        assert_eq!(selection.model_id, "recommended-exact");
        assert_eq!(
            selection.base_url.as_deref(),
            Some("https://billing.example/v1")
        );
        assert_eq!(
            selection.environment_variable.as_deref(),
            Some("FAKE_SELECTED")
        );
        assert!(
            super::recommended_selection(
                &config,
                "foreign.provider",
                "fake",
                "key",
                None,
                Some(&choices[0].1),
                response
            )
            .is_none()
        );
        assert!(
            super::recommended_selection(
                &config,
                "fake.provider",
                "fake",
                "key",
                None,
                None,
                bcode_model::StartupRecommendationResponse {
                    recommendation: None
                }
            )
            .is_none()
        );
    }

    #[test]
    fn fake_xai_detection_offers_provider_owned_offline_choice() {
        let mut choices = choices();
        let candidate = &mut choices[0].1;
        candidate.provider_id = "xai".to_owned();
        candidate.provider_name = "xAI".to_owned();
        candidate.source = bcode_provider_auth::startup::StartupCredentialSource::Environment {
            name: "XAI_API_KEY".to_owned(),
        };
        let recommended = bcode_model::StartupRecommendation {
            provider_plugin_id: "fake.provider".to_owned(),
            model_id: "fixture-catalog-model".to_owned(),
            settings: std::collections::BTreeMap::new(),
        };
        let selection = super::recommended_selection(
            &bcode_config::BcodeConfig::default(),
            "fake.provider",
            "xai",
            "key",
            None,
            Some(candidate),
            bcode_model::StartupRecommendationResponse {
                recommendation: Some(recommended),
            },
        )
        .unwrap();
        let input = super::quick_input(
            true,
            Err("Choose a model to get started."),
            std::slice::from_ref(candidate),
            &[(selection.clone(), candidate.clone())],
        );
        assert_eq!(input.choices.len(), 1);
        assert!(input.choices[0].detail.contains("XAI_API_KEY"));
        assert!(input.choices[0].detail.contains("not authenticated"));
        assert!(input.choices[0].label.ends_with(" (recommended)"));
        let mut alternative = selection.clone();
        alternative.model_id = "fixture-alternative".to_owned();
        let alternatives = super::quick_input(
            true,
            Ok(()),
            std::slice::from_ref(candidate),
            &[
                (selection.clone(), candidate.clone()),
                (alternative, candidate.clone()),
            ],
        );
        assert!(!alternatives.choices[1].label.contains("recommended"));
        assert_eq!(
            selection.environment_variable.as_deref(),
            Some("XAI_API_KEY")
        );
        assert_eq!(selection.environment_auth_provider.as_deref(), Some("xai"));
        assert_eq!(selection.auth_profile, None);
    }

    #[test]
    fn routing_preserves_explicit_settings_and_model_profiles() {
        let mut config = bcode_config::BcodeConfig::default();
        config.model.settings.insert(
            "base_url".to_owned(),
            "https://explicit.example/v1".to_owned(),
        );
        assert!(super::has_explicit_selection(&config));
        assert_eq!(
            super::startup_route(false, super::has_explicit_selection(&config), true),
            super::StartupRoute::Recover {
                allow_choices: false
            }
        );
        config.model.settings.clear();
        config.model.profile = Some("explicit-profile".to_owned());
        assert!(super::has_explicit_selection(&config));
        config.startup_selection_applied = true;
        assert!(!super::has_explicit_selection(&config));
    }

    #[test]
    fn accepted_selection_never_changes_declarative_config() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("bcode.toml");
        let original = "# user-owned configuration\n";
        std::fs::write(&config_path, original).unwrap();
        let state_path = directory.path().join("state/startup.toml");
        let choices = choices();
        super::consent_selection(
            &bcode_tui::QuickOnboardingOutcome::Selected(0),
            super::startup_route(false, false, true),
            &choices,
            |selection| {
                bcode_config::startup::save_to(&state_path, selection)
                    .map(|_| ())
                    .map_err(|_| "save failed")
            },
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(config_path).unwrap(), original);
        assert_eq!(
            bcode_config::startup::load_from(&state_path)
                .unwrap()
                .unwrap(),
            choices[0].0
        );
    }

    #[test]
    fn enrolled_account_offers_alternatives_and_saves_only_confirmed_model() {
        let mut recommended = choices().remove(0).0;
        recommended.auth_profile = Some("enrolled-fixture".to_owned());
        recommended.environment_variable = None;
        recommended.environment_auth_provider = None;
        recommended.environment_method = None;
        recommended.environment_credential = None;
        let mut alternate = recommended.clone();
        alternate.model_id = "alternate-fixture".to_owned();
        let selections = vec![recommended, alternate.clone()];
        let input = super::enrolled_input("Fixture", &selections);
        assert_eq!(input.choices.len(), 2);
        assert!(input.choices[0].label.ends_with(" (recommended)"));
        assert!(input.choices[1].label.contains("alternate-fixture"));
        for outcome in [
            bcode_tui::QuickOnboardingOutcome::Close,
            bcode_tui::QuickOnboardingOutcome::Advanced,
        ] {
            assert!(
                !super::consent_enrolled_selection(&outcome, &selections, |_| panic!(
                    "unconfirmed model must not save"
                ))
                .unwrap()
            );
        }
        assert!(
            super::consent_enrolled_selection(
                &bcode_tui::QuickOnboardingOutcome::Selected(2),
                &selections,
                |_| panic!("invalid choice must not save")
            )
            .is_err()
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("startup.toml");
        assert!(
            super::consent_enrolled_selection(
                &bcode_tui::QuickOnboardingOutcome::Selected(1),
                &selections,
                |selection| bcode_config::startup::save_to(&path, selection)
                    .map(|_| ())
                    .map_err(|_| "save failed")
            )
            .unwrap()
        );
        assert_eq!(
            bcode_config::startup::load_from(&path).unwrap(),
            Some(alternate)
        );
    }

    #[test]
    fn automation_and_opt_out_never_open_quick_setup() {
        assert!(interactive_startup(true, false, false));
        assert!(!interactive_startup(false, false, false));
        assert!(!interactive_startup(true, true, false));
        assert!(!interactive_startup(true, false, true));
    }
}
