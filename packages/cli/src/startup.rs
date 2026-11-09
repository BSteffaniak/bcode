//! Quick startup routing at the CLI boundary; authentication stays provider-owned.

use super::{CliError, load_cli_plugin_host};
use bcode_config::BcodeConfig;
use bcode_provider_auth::startup::{StartupAuthCandidate, discover_candidates};
use std::io::IsTerminal as _;

mod native_setup;

/// Interactive startup is opt-in for terminal launches, not automation.
const fn interactive_startup(terminal: bool, ci: bool, opted_out: bool) -> bool {
    terminal && !ci && !opted_out
}

pub async fn advanced(no_discovery: bool) -> Result<(), CliError> {
    let mut shell = bcode_tui::TerminalShell::enter()?;
    let result = advanced_in_shell(&mut shell, no_discovery).await;
    if let Err(error) = result {
        let message = launch_recovery_message(Some(&error));
        let _ = shell.show_error_without_retry(&message).await?;
    }
    shell.finish()?;
    Ok(())
}

async fn advanced_in_shell(
    shell: &mut bcode_tui::TerminalShell,
    no_discovery: bool,
) -> Result<(), CliError> {
    Box::pin(super::handle_onboard_command_in_shell(
        shell,
        &super::OnboardOptions {
            no_credential_discovery: no_discovery,
            ..super::OnboardOptions::default()
        },
    ))
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
    let mut host = load_cli_plugin_host()?;
    let readiness = if config.execution.default_selection.is_none() {
        register_enabled(&host)?;
        local_readiness(&config, &host)
    } else {
        // A saved execution route must not inspect an unrelated provider account.
        Err("Execution preference is handled separately")
    };
    let mut shell = bcode_tui::TerminalShell::enter()?;
    let backend = if should_route_backends(&config, readiness.is_ok()) {
        Box::pin(route_backend_selection(&host, &mut shell, no_discovery)).await
    } else {
        Ok(false)
    };
    host.deactivate_all()?;
    match backend {
        Ok(true) => {
            shell.finish()?;
            return Ok(true);
        }
        Ok(false) => {}
        Err(error) => {
            shell.finish()?;
            return Err(error);
        }
    }
    let remembered = bcode_config::startup::load();
    let mut host = load_cli_plugin_host()?;
    register_enabled(&host)?;
    let readiness = local_readiness(&config, &host);
    let explicit_selection = has_explicit_selection(&config);
    let route = startup_route(readiness.is_ok(), explicit_selection, remembered.is_ok());
    if route == StartupRoute::Continue {
        host.deactivate_all()?;
        shell.finish()?;
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
    let result = run_startup_journey(
        &mut shell,
        input,
        remembered.is_ok(),
        route,
        &choices,
        explicit_selection,
        no_discovery,
    )
    .await;
    shell.finish()?;
    result?;
    Ok(true)
}

const HARNESS_CONFIGURATION_INTERFACE: &str = "bcode.harness.configuration/v1";

// Wire DTOs for the plugin-owned configuration service. These are non-secret
// references, never readiness evidence or authority to invoke native processes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum BackendSelection {
    Default {},
    Instance { instance_id: String },
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BackendCatalog {
    default_instance: Option<String>,
    instances: std::collections::BTreeMap<String, bcode_harness_models::HarnessInstance>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedBackend {
    selection: BackendSelection,
    instance: bcode_harness_models::HarnessInstance,
}

fn backend_choices(catalog: &BackendCatalog) -> Result<Vec<BackendSelection>, CliError> {
    if catalog.instances.len() > 128
        || catalog
            .instances
            .iter()
            .any(|(id, instance)| id != &instance.instance_id || instance.validate().is_err())
        || catalog
            .default_instance
            .as_ref()
            .is_some_and(|id| !catalog.instances.contains_key(id))
    {
        return Err(CliError::InvalidArguments(
            "Invalid configured execution backend catalog".into(),
        ));
    }
    let mut choices = Vec::new();
    if catalog.default_instance.is_some() {
        choices.push(BackendSelection::Default {});
    }
    choices.extend(
        catalog
            .instances
            .keys()
            .map(|instance_id| BackendSelection::Instance {
                instance_id: instance_id.clone(),
            }),
    );
    Ok(choices)
}

fn verify_prepared_backend(
    catalog: &BackendCatalog,
    selection: &BackendSelection,
    prepared: &PreparedBackend,
) -> Result<(), CliError> {
    let id = match selection {
        BackendSelection::Default {} => catalog.default_instance.as_deref(),
        BackendSelection::Instance { instance_id } => Some(instance_id.as_str()),
    };
    if prepared.selection != *selection
        || prepared.instance.validate().is_err()
        || id.and_then(|id| catalog.instances.get(id)) != Some(&prepared.instance)
    {
        return Err(CliError::InvalidArguments(
            "Selected execution backend changed; no session was started".into(),
        ));
    }
    Ok(())
}

/// Optional native inventory must not preempt a usable or explicitly configured provider.
fn should_route_backends(config: &BcodeConfig, provider_ready: bool) -> bool {
    config.execution.default_selection.is_some()
        || (!provider_ready && !has_explicit_selection(config))
}

fn configured_backend_catalog(
    host: &bcode_plugin::PluginHost,
    config: &bcode_config::BcodeConfig,
    remembered_managed: bool,
) -> Result<Option<BackendCatalog>, CliError> {
    let catalog: BackendCatalog = if config.plugins.config.contains_key("bcode.harness") {
        match host.invoke_service_json(
            "bcode.harness",
            HARNESS_CONFIGURATION_INTERFACE,
            "catalog",
            &serde_json::json!({}),
        ) {
            Ok(catalog) => catalog,
            Err(bcode_plugin::PluginServiceCallError::Service { code, .. })
                if code == "configuration_disabled" =>
            {
                if remembered_managed {
                    return Err(CliError::InvalidArguments(
                        "Remembered managed route is disabled; no provider fallback was selected"
                            .into(),
                    ));
                }
                return Ok(None);
            }
            Err(error) => {
                return Err(CliError::InvalidArguments(format!(
                    "Configured execution backends could not be read ({error}); no selection was changed"
                )));
            }
        }
    } else {
        // No generic instance settings exist yet. Candidate discovery remains native-owned;
        // absence is not a configured default or permission to create an instance.
        BackendCatalog {
            default_instance: None,
            instances: std::collections::BTreeMap::new(),
        }
    };
    Ok(Some(catalog))
}

async fn route_backend_selection(
    host: &bcode_plugin::PluginHost,
    shell: &mut bcode_tui::TerminalShell,
    no_discovery: bool,
) -> Result<bool, CliError> {
    let config = bcode_config::load_config()?;
    if let Some(preference) = &config.execution.default_selection
        && let bcode_model::execution::ExecutionRoute::ModelProvider { provider_plugin_id } =
            &preference.route
    {
        // A provider preference does not depend on the optional harness service
        // or its configuration/catalog being available.
        launch_provider_model(
            shell,
            provider_plugin_id.clone(),
            preference.model_id.clone(),
        )
        .await?;
        return Ok(true);
    }
    let remembered_managed = config.execution.default_selection.is_some();
    // Provider preferences returned above; any remaining saved route is native.
    if !host.has_service("bcode.harness", HARNESS_CONFIGURATION_INTERFACE) {
        if remembered_managed {
            return Err(CliError::InvalidArguments("Remembered managed route is disabled; enable it in Setup or explicitly change the execution preference. No provider fallback was selected".into()));
        }
        return Ok(false);
    }
    let Some(catalog) = configured_backend_catalog(host, &config, remembered_managed)? else {
        return Ok(false);
    };
    let _ = backend_choices(&catalog)?;
    if catalog.instances.is_empty() && config.execution.default_selection.is_none() {
        // Reached only without a usable or explicitly selected provider; native setup
        // offers provider connections explicitly rather than preempting one.
        return Box::pin(native_setup::run(shell, no_discovery)).await;
    }
    let remembered = config
        .execution
        .default_selection
        .as_ref()
        .map(bcode_config::ExecutionPreference::selection);
    let provider = if remembered_managed {
        // A saved native route must not inspect/materialize an unrelated provider account.
        None
    } else {
        config.resolved_model_selection().provider_plugin_id
    };
    let client = bcode_client::ManagedSessionClient::default_endpoint();
    let request = bcode_ipc::harness::ManagedExecutionCatalogRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        instances: catalog.instances.values().cloned().collect(),
        provider_plugin_id: provider,
    };
    let result = Box::pin(async {
        if let Some(selection) = remembered.clone() {
            // Only the selected backend can gate a remembered launch. Optional
            // sources must not delay it or cause fallback to a different account.
            let mut selected_request = request;
            selected_request.instances.retain(|instance| {
                matches!(&selection.route, Some(bcode_model::execution::ExecutionRoute::ManagedHarness { instance_id, .. }) if instance_id == &instance.instance_id)
            });
            let inventory = Box::pin(shell.wait("Checking selected execution route", async {
                client.discover_execution_catalog(selected_request).await.map_err(bcode_tui::TuiError::from)
            })).await?;
            if let Some(inventory) = inventory {
                Box::pin(launch_execution_choice(host, shell, &catalog, &inventory.models, selection, false)).await?;
            }
            return Ok(true);
        }
        let (outcome, entries) = bcode_tui::run_execution_picker_loading_in_shell(
            shell,
            async move {
                let inventory = client.discover_execution_catalog(request).await.map_err(bcode_tui::TuiError::from)?;
                let status = if inventory.issues.is_empty() {
                    "Choose a model and account"
                } else {
                    "Some sources could not be checked. Choose an observed model or open Setup"
                };
                Ok((inventory.models, status.to_owned()))
            },
        ).await?;
        match outcome {
            bcode_tui::ExecutionPickerOutcome::Selected(selection) => {
                Box::pin(launch_execution_choice(
                    host, shell, &catalog, &entries, selection, true,
                ))
                .await?;
                Ok(true)
            }
            bcode_tui::ExecutionPickerOutcome::Setup => {
                Box::pin(native_setup::run(shell, no_discovery)).await?;
                Ok(true)
            }
            bcode_tui::ExecutionPickerOutcome::Close => Ok(true),
        }
    })
    .await;
    if result.is_err() {
        // Start may have committed: show one safe recovery screen, never reopen
        // the picker or fall through into model onboarding after managed failure.
        let _ = shell.show_error_without_retry("Execution startup did not complete. Inspect Sessions before starting another session. No provider fallback or billing change was selected.").await;
    }
    result
}

async fn launch_execution_choice(
    host: &bcode_plugin::PluginHost,
    shell: &mut bcode_tui::TerminalShell,
    catalog: &BackendCatalog,
    entries: &[bcode_model::execution::ExecutionModel],
    selection: bcode_model::execution::ExecutionModelSelection,
    review_preference: bool,
) -> Result<(), CliError> {
    match selection.route.clone() {
        Some(bcode_model::execution::ExecutionRoute::ManagedHarness { instance_id, .. }) => {
            // EligibleToAttempt includes an authenticated discovered native route with unknown
            // model entitlement. Never replace that with a costly proof or claim confirmed access.
            bcode_model_catalog::ModelCatalogResolver::resolve_execution_selection(
                entries, selection.clone(),
            ).map_err(|_| CliError::InvalidArguments("The exact model/account is unavailable, ambiguous, or not ready. Use Setup/sign in or explicitly change the execution preference. No fallback was selected".into()))?;
            let backend = BackendSelection::Instance { instance_id };
            let prepared: PreparedBackend = host
                .invoke_service_json(
                    "bcode.harness",
                    HARNESS_CONFIGURATION_INTERFACE,
                    "prepare",
                    &serde_json::json!({"selection":backend}),
                )
                .map_err(|error| {
                    CliError::InvalidArguments(format!(
                        "Selected execution backend could not be prepared ({error}); no fallback was selected"
                    ))
                })?;
            verify_prepared_backend(catalog, &backend, &prepared)?;
            if review_preference
                && !Box::pin(confirm_execution_preference(shell, &selection)).await?
            {
                return Ok(());
            }
            Box::pin(launch_selected_managed(shell, prepared.instance, selection)).await
        }
        Some(bcode_model::execution::ExecutionRoute::ModelProvider { provider_plugin_id }) => {
            if review_preference
                && !Box::pin(confirm_execution_preference(shell, &selection)).await?
            {
                return Ok(());
            }
            // Ordinary provider selection/readiness remains owned by the existing client path.
            launch_provider_model(shell, provider_plugin_id, selection.model_id).await
        }
        None => Err(CliError::InvalidArguments(
            "An exact execution route is required".into(),
        )),
    }
}

fn managed_start_call(
    instance: bcode_harness_models::HarnessInstance,
    session_id: bcode_session_models::SessionId,
    open_id: String,
    working_directory: &std::path::Path,
) -> Result<bcode_ipc::harness::ManagedSessionCall, CliError> {
    let working_directory = working_directory
        .to_str()
        .filter(|_| working_directory.is_absolute())
        .ok_or_else(|| {
            CliError::InvalidArguments(
                "Managed startup requires an absolute UTF-8 working directory".into(),
            )
        })?;
    let call = bcode_ipc::harness::ManagedSessionCall {
        session_id: Some(session_id),
        parent_session_id: None,
        assistance_destination_session_id: None,
        harness: bcode_harness_models::HarnessRequest {
            schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
            request_id: open_id.clone(),
            instance,
            kind: bcode_harness_models::HarnessRequestKind::Open(
                bcode_harness_models::HarnessOpenRequest {
                    open_id,
                    session_id: session_id.to_string(),
                    generation: 1,
                    working_directory: working_directory.into(),
                    resume: None,
                    parent_assistance: None,
                },
            ),
        },
    };
    call.harness
        .validate()
        .map_err(|_| CliError::InvalidArguments("Invalid managed startup selection".into()))?;
    Ok(call)
}

fn managed_started_session(
    response: &bcode_harness_models::HarnessServiceResponse,
    session_id: bcode_session_models::SessionId,
    request_id: &str,
    instance_id: &str,
) -> Result<bcode_session_models::SessionId, CliError> {
    response
        .validate()
        .map_err(|_| CliError::InvalidArguments("Invalid managed startup response".into()))?;
    let target_matches = match &response.kind {
        bcode_harness_models::HarnessServiceResponseKind::Prepared(target) => {
            target.session_id == session_id.to_string()
                && target.open_id == request_id
                && target.instance_id == instance_id
                && target.generation == 1
        }
        bcode_harness_models::HarnessServiceResponseKind::Opened(target) => {
            target.session_id == session_id.to_string()
                && target.instance_id == instance_id
                && target.generation == 1
        }
        _ => false,
    };
    if response.request_id != request_id
        || response.operation != bcode_harness_models::HarnessOperation::Open
        || !target_matches
    {
        return Err(CliError::InvalidArguments(
            "Managed startup is unavailable or uncertain; inspect session status before retrying"
                .into(),
        ));
    }
    Ok(session_id)
}

#[cfg(test)]
fn decode_managed_start_response(
    response: &bcode_plugin_sdk::application::PluginServiceResponse,
) -> Result<bcode_harness_models::HarnessServiceResponse, &'static str> {
    if response.error.is_some()
        || response.payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES
    {
        return Err("managed startup response unavailable");
    }
    serde_json::from_slice(&response.payload).map_err(|_| "invalid managed startup response")
}

async fn launch_selected_managed(
    shell: &mut bcode_tui::TerminalShell,
    instance: bcode_harness_models::HarnessInstance,
    selection: bcode_model::execution::ExecutionModelSelection,
) -> Result<(), CliError> {
    let cwd = std::fs::canonicalize(std::env::current_dir()?)?;
    let session_id = bcode_session_models::SessionId::new();
    let call = managed_start_call(instance, session_id, uuid::Uuid::new_v4().to_string(), &cwd)?;
    let bcode_harness_models::HarnessRequestKind::Open(open) = call.harness.kind else {
        return Err(CliError::InvalidArguments(
            "Invalid selected creation intent".into(),
        ));
    };
    let response = Box::pin(shell.wait("Starting the selected model", async {
        bcode_client::ManagedSessionClient::default_endpoint()
            .start_selected_session(bcode_ipc::harness::ManagedSelectedStartRequest {
                selection,
                instance: call.harness.instance,
                open,
                parent_session_id: None,
            })
            .await
            .map_err(bcode_tui::TuiError::from)
    }))
    .await?;
    if !shell.stop_requested()
        && let Some(response) = response
    {
        let bcode_harness_models::HarnessModelsResponseKind::Selected { target, .. } =
            response.response.kind
        else {
            return Err(CliError::InvalidArguments(
                "Selected startup was not acknowledged; inspect Sessions without retrying".into(),
            ));
        };
        target
            .validate_owner(&session_id.to_string(), 1)
            .map_err(|_| {
                CliError::InvalidArguments("Selected startup returned a foreign session".into())
            })?;
        shell
            .run_selected_managed_chat(
                bcode_client::ManagedSessionClient::default_endpoint(),
                target,
                &super::static_bundled_plugins(),
                bcode_tui::TuiLaunchOptions::default(),
            )
            .await?;
    }
    Ok(())
}

async fn launch_provider_model(
    shell: &mut bcode_tui::TerminalShell,
    provider: String,
    model: String,
) -> Result<(), CliError> {
    let session = Box::pin(shell.wait("Starting your session", async {
        super::ensure_server_running()
            .await
            .map_err(|error| super::startup_tui_error(&error))?;
        let client = bcode_client::BcodeClient::default_endpoint();
        let session = client.create_session(None).await?;
        client
            .set_session_model(session.id, Some(provider), model)
            .await?;
        Ok::<_, bcode_tui::TuiError>(session)
    }))
    .await?;
    if !shell.stop_requested()
        && let Some(session) = session
    {
        shell
            .run_chat(
                Some(session.id),
                &super::static_bundled_plugins(),
                bcode_tui::TuiLaunchOptions::default(),
            )
            .await?;
    }
    Ok(())
}

async fn confirm_execution_preference(
    shell: &mut bcode_tui::TerminalShell,
    selection: &bcode_model::execution::ExecutionModelSelection,
) -> Result<bool, CliError> {
    let detail = match &selection.route {
        // A native executor runs under its own signed-in account.
        Some(bcode_model::execution::ExecutionRoute::ManagedHarness { plugin_id, .. }) => format!(
            "{} · {}",
            selection.model_id,
            bcode_session_view::external_projection::harness_display_label(plugin_id)
        ),
        Some(bcode_model::execution::ExecutionRoute::ModelProvider { provider_plugin_id }) => {
            format!(
                "{} · Provider {provider_plugin_id} · account {}",
                selection.model_id,
                selection
                    .account_reference
                    .as_deref()
                    .unwrap_or("unspecified")
            )
        }
        None => return Err(CliError::InvalidArguments("Exact route required".into())),
    };
    let (config, launch) = bcode_config::recovery::load_current_config_with_recovery_context()?;
    let preference = bcode_config::ExecutionPreference {
        model_id: selection.model_id.clone(),
        route: selection
            .route
            .clone()
            .ok_or_else(|| CliError::InvalidArguments("Exact route required".into()))?,
        account_reference: selection.account_reference.clone(),
    };
    let edit = launch.recovery_sources.as_ref().and_then(|sources| {
        let roots = bcode_config::recovery::default_allowed_recovery_roots(&sources.cwd).ok()?;
        let reference =
            bcode_config::recovery::capture_with_sources(&config, sources, &roots).ok()?;
        bcode_config::edit::plan_execution_preference_with_policy(
            &config,
            &reference,
            &roots,
            Some(&preference),
        )
        .ok()
    });
    let destination = edit.as_ref().map(|edit| edit.path().to_path_buf());
    let mut choices = vec![bcode_tui::QuickConnectionChoice {
        label: "Use it for this conversation".into(),
        detail: detail.clone(),
    }];
    if let Some(path) = &destination {
        choices.push(bcode_tui::QuickConnectionChoice {
            label: "Use it and remember it for next time".into(),
            detail: format!("{detail} · saved to {}", path.display()),
        });
    }
    let outcome = bcode_tui::run_execution_confirmation_in_shell(shell, bcode_tui::QuickOnboardingInput {
        choices,
        recovery: Some("Remembering saves only this model choice, so `bcode` opens straight into it next time. Nothing has been sent yet.".into()),
    }).await?;
    match outcome {
        bcode_tui::QuickOnboardingOutcome::Selected(0) => Ok(true),
        bcode_tui::QuickOnboardingOutcome::Selected(1) => {
            edit.ok_or_else(|| {
                CliError::InvalidArguments(
                    "No reviewed configuration destination; no preference was written".into(),
                )
            })?
            .apply()?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

async fn run_startup_journey(
    shell: &mut bcode_tui::TerminalShell,
    mut input: bcode_tui::QuickOnboardingInput,
    state_valid: bool,
    route: StartupRoute,
    choices: &[(
        bcode_config::startup::StartupSelection,
        StartupAuthCandidate,
    )],
    explicit_selection: bool,
    no_discovery: bool,
) -> Result<(), CliError> {
    let Ok(outcome) = Box::pin(bcode_tui::run_quick_onboarding_in_shell(
        shell,
        input.clone(),
    ))
    .await
    else {
        // Enrollment may have committed before presentation failed.
        return recover_startup_journey(shell, &mut input, None).await;
    };
    let result = match outcome {
        bcode_tui::QuickOnboardingOutcome::Advanced => advanced_in_shell(shell, no_discovery).await,
        bcode_tui::QuickOnboardingOutcome::Close => return Ok(()),
        bcode_tui::QuickOnboardingOutcome::Selected(index) => {
            Box::pin(launch_selected(shell, index, state_valid, route, choices)).await
        }
        bcode_tui::QuickOnboardingOutcome::Connected {
            provider_id,
            profile,
        } => {
            Box::pin(launch_connected(
                shell,
                provider_id,
                profile,
                explicit_selection,
                state_valid,
                no_discovery,
            ))
            .await
        }
    };
    if let Err(error) = result {
        return recover_startup_journey(shell, &mut input, Some(&error)).await;
    }
    Ok(())
}

async fn recover_startup_journey(
    shell: &mut bcode_tui::TerminalShell,
    input: &mut bcode_tui::QuickOnboardingInput,
    error: Option<&CliError>,
) -> Result<(), CliError> {
    // Do not expose potentially secret-bearing provider errors or retry a mutation.
    recover_launch_input(input);
    let message = launch_recovery_message(error);
    let _ = shell.show_error_without_retry(&message).await?;
    // Back leaves this journey too: Advanced can create sessions, so reopening the
    // picker here would offer an indirect retry before ambiguous effects are inspected.
    Ok(())
}

fn launch_recovery_message(error: Option<&CliError>) -> String {
    let label = match error {
        Some(CliError::Tui(bcode_tui::TuiError::PluginService { code, .. })) => match code.as_str()
        {
            "daemon_start_timeout" => {
                Some("Local server startup timed out (daemon_start_timeout).")
            }
            "daemon_start_exited" => {
                Some("Local server exited before becoming ready (daemon_start_exited).")
            }
            "daemon_start_health_check" => {
                Some("Local server readiness health check failed (daemon_start_health_check).")
            }
            "daemon_start_endpoint_occupied" => {
                Some("Local server endpoint is occupied (daemon_start_endpoint_occupied).")
            }
            "daemon_start_coordination_timeout" => Some(
                "Local server startup coordination timed out (daemon_start_coordination_timeout).",
            ),
            "daemon_start_incompatible" => Some(
                "Local server identity or compatibility check failed (daemon_start_incompatible).",
            ),
            "daemon_start_connection" => {
                Some("Local server connection could not be established (daemon_start_connection).")
            }
            "daemon_start_failed" => {
                Some("Local server could not be started (daemon_start_failed).")
            }
            _ => None,
        },
        _ => None,
    };
    let guidance = "Setup or launch could not complete. If session creation was interrupted, a session may already exist; inspect Sessions before starting another. Back or Close leaves onboarding without retrying. Existing configuration was not replaced.";
    label.map_or_else(
        || guidance.to_owned(),
        |label| format!("{label}\n\n{guidance}"),
    )
}

fn recover_launch_input(input: &mut bcode_tui::QuickOnboardingInput) {
    input.choices.clear();
    input.recovery = Some(launch_recovery_message(None));
}

async fn launch_selected(
    shell: &mut bcode_tui::TerminalShell,
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
    Box::pin(super::run_new_session_tui_in_shell(
        shell,
        None,
        bcode_tui::TuiLaunchOptions::default(),
    ))
    .await?;
    Ok(())
}

async fn launch_connected(
    shell: &mut bcode_tui::TerminalShell,
    mut provider_id: String,
    mut profile: String,
    explicit_selection: bool,
    state_valid: bool,
    no_discovery: bool,
) -> Result<(), CliError> {
    if !state_valid {
        return Err(CliError::InvalidArguments("Connection enrolled, but remembered startup state requires explicit maintenance before selecting it".to_owned()));
    }
    loop {
        let config = bcode_config::load_config()?;
        let mut host = load_cli_plugin_host()?;
        register_enabled(&host)?;
        if explicit_selection {
            let repaired = bcode_config::load_config()?;
            local_readiness(&repaired, &host).map_err(|_| CliError::InvalidArguments("Connection enrolled, but the explicit model/account selection still needs repair. Use Advanced settings; it was not replaced.".to_owned()))?;
            host.deactivate_all()?;
            Box::pin(super::run_new_session_tui_in_shell(
                shell,
                None,
                bcode_tui::TuiLaunchOptions::default(),
            ))
            .await?;
            return Ok(());
        }
        let provider = super::registered_auth_provider(&host, &provider_id)?;
        let runtime = bcode_config::try_load_runtime_auth_subscriptions()?;
        let resolved = bcode_provider_auth::resolve_auth_provider_profile(
            &config,
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
            &config,
            &provider_id,
            method.method_id(),
            Some(profile.clone()),
            None,
        );
        if selections.is_empty() {
            host.deactivate_all()?;
            Box::pin(super::handle_onboard_command_in_shell(
                shell,
                &super::OnboardOptions {
                    provider: Some(provider_id),
                    no_credential_discovery: no_discovery,
                    experience_mode: super::OnboardExperienceMode::ControlCenter,
                    ..super::OnboardOptions::default()
                },
            ))
            .await?;
            return Ok(());
        }
        host.deactivate_all()?;
        let outcome = Box::pin(bcode_tui::run_quick_onboarding_in_shell(
            shell,
            enrolled_input(&provider.contribution.display_name, &selections),
        ))
        .await?;
        match outcome {
            bcode_tui::QuickOnboardingOutcome::Advanced => {
                return Box::pin(advanced_in_shell(shell, no_discovery)).await;
            }
            bcode_tui::QuickOnboardingOutcome::Connected {
                provider_id: next_provider,
                profile: next_profile,
            } => {
                provider_id = next_provider;
                profile = next_profile;
                continue;
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
        Box::pin(super::run_new_session_tui_in_shell(
            shell,
            None,
            bcode_tui::TuiLaunchOptions::default(),
        ))
        .await?;
        return Ok(());
    }
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
    has_explicit_selection_with_environment(config, &bcode_config::ProcessConfigEnvironment)
}

fn has_explicit_selection_with_environment(
    config: &BcodeConfig,
    environment: &impl bcode_config::ConfigEnvironment,
) -> bool {
    let selection = config.resolved_model_selection_with_environment(environment);
    let explicit_provider = match &selection.provider_source {
        Some(bcode_config::ModelSelectionSource::Environment { variable }) => {
            matches!(variable.as_str(), "BCODE_MODEL_PROVIDER" | "BCODE_PROVIDER")
        }
        Some(_) => true,
        None => false,
    };
    !config.startup_selection_applied
        && (config.model.provider_plugin_id.is_some()
            || explicit_provider
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

/// Validate only the explicit headless selection; never enroll, discover, or repair accounts.
///
/// # Errors
/// Returns configuration, selection, provider-readiness, or authentication errors.
pub fn headless_preflight() -> Result<bcode_config::ResolvedModelSelection, CliError> {
    let config = bcode_config::load_config()?;
    let selection = config.resolved_model_selection();
    validate_headless_selection(&config, &selection)?;
    let mut host = load_cli_plugin_host()?;
    let readiness = register_enabled(&host).and_then(|()| {
        local_readiness_with_policy(&config, &host, true)
            .map_err(|message| CliError::InvalidArguments(message.to_owned()))
    });
    let deactivated = host.deactivate_all().map_err(CliError::from);
    readiness?;
    deactivated?;
    Ok(selection)
}

fn validate_headless_selection(
    config: &BcodeConfig,
    selection: &bcode_config::ResolvedModelSelection,
) -> Result<(), CliError> {
    selection
        .validate_selection()
        .map_err(|error| CliError::InvalidArguments(error.to_string()))?;
    let profile = selection
        .model_profile
        .as_ref()
        .and_then(|name| config.model.profiles.get(name));
    if profile.is_none_or(|profile| {
        profile.provider_plugin_id.trim().is_empty()
            || profile
                .model_id
                .as_deref()
                .is_none_or(|id| id.trim().is_empty())
    }) {
        return Err(CliError::InvalidArguments(
            "run requires a profile with an explicit provider and model".to_owned(),
        ));
    }
    // A pool can rotate accounts after overload/expiry. Reject ambiguous account/none
    // selection as well: an explicit account must actually participate in preflight.
    let auth_none = selection.settings.get("auth_mode").map(String::as_str) == Some("none");
    let account = selection
        .auth_profile
        .as_deref()
        .is_some_and(|name| !name.trim().is_empty());
    if selection.auth_pool.is_some()
        || account == auth_none
        || selection.auth_profile.is_some() && !account
    {
        return Err(CliError::InvalidArguments("run requires exactly one explicit auth_profile (no auth_pool), or auth_mode = none without an account".to_owned()));
    }
    Ok(())
}

fn local_readiness(
    config: &BcodeConfig,
    host: &bcode_plugin::PluginHost,
) -> Result<(), &'static str> {
    local_readiness_with_policy(config, host, false)
}

fn local_readiness_with_policy(
    config: &BcodeConfig,
    host: &bcode_plugin::PluginHost,
    headless: bool,
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
    let request = bcode_model::StartupReadinessRequest {
        model_id: selection
            .model_id
            .clone()
            .ok_or("Choose a model to get started.")?,
        settings: selection.settings.clone(),
    };
    let readiness = host
        .invoke_service_json::<_, bcode_model::StartupReadinessResponse>(
            plugin_id,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_STARTUP_READINESS,
            &request,
        )
        .ok();
    if headless && readiness.is_none() {
        return Err(
            "The selected provider cannot verify startup readiness; no turn was submitted.",
        );
    }
    check_model_readiness(&selection, |_| readiness)?;
    if headless && selection.settings.get("auth_mode").map(String::as_str) == Some("none") {
        return Ok(());
    }
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
    #[test]
    fn optional_backends_cannot_intercept_provider_startup_or_repair() {
        let mut config = bcode_config::BcodeConfig::default();
        assert!(!super::should_route_backends(&config, true));
        config.model.provider_plugin_id = Some("example.provider".into());
        // A broken explicit provider goes to its own repair path, not native setup.
        assert!(!super::should_route_backends(&config, false));
        assert!(!super::should_route_backends(&config, true));
        config.execution.default_selection = Some(bcode_config::ExecutionPreference {
            model_id: "selected-model".into(),
            route: bcode_model::execution::ExecutionRoute::ManagedHarness {
                instance_id: "selected-account".into(),
                plugin_id: "example.native".into(),
            },
            account_reference: Some("selected-account".into()),
        });
        // Explicit native intent never falls back to an otherwise usable provider.
        assert!(super::should_route_backends(&config, true));
        assert!(super::should_route_backends(&config, false));
    }

    fn backend_instance() -> bcode_harness_models::HarnessInstance {
        bcode_harness_models::HarnessInstance {
            instance_id: "native-account".into(),
            plugin_id: "test.harness".into(),
            executable: "/isolated/native".into(),
            config_namespace: "/isolated/config".into(),
            auth: bcode_harness_models::HarnessAuthBinding {
                provider_id: "native.auth".into(),
                profile: "selected".into(),
                credential_namespace: "/isolated/config".into(),
                custody: bcode_harness_models::HarnessAuthCustody::NativeManaged,
            },
        }
    }

    #[test]
    fn backend_catalog_is_not_readiness_or_implicit_default_consent() {
        let instance = backend_instance();
        let mut catalog = super::BackendCatalog {
            default_instance: None,
            instances: [(instance.instance_id.clone(), instance.clone())].into(),
        };
        let selection = super::BackendSelection::Instance {
            instance_id: instance.instance_id.clone(),
        };
        assert_eq!(
            super::backend_choices(&catalog).unwrap(),
            vec![selection.clone()]
        );
        let mut prepared = super::PreparedBackend {
            selection: selection.clone(),
            instance,
        };
        assert!(super::verify_prepared_backend(&catalog, &selection, &prepared).is_ok());
        catalog.default_instance = Some(prepared.instance.instance_id.clone());
        assert_eq!(
            super::backend_choices(&catalog).unwrap()[0],
            super::BackendSelection::Default {}
        );
        // Requesting an exact reference cannot be answered as default, even when
        // both currently resolve to the same instance.
        prepared.selection = super::BackendSelection::Default {};
        assert!(super::verify_prepared_backend(&catalog, &selection, &prepared).is_err());
        assert!(
            super::verify_prepared_backend(
                &catalog,
                &super::BackendSelection::Default {},
                &prepared
            )
            .is_ok()
        );
        prepared.selection = selection.clone();
        prepared.instance.auth.profile = "different-billing-profile".into();
        assert!(super::verify_prepared_backend(&catalog, &selection, &prepared).is_err());
        catalog.default_instance = Some("missing".into());
        assert!(super::backend_choices(&catalog).is_err());
        // Model route remains recovery-required even when a harness is configured.
        assert_eq!(
            super::startup_route(false, true, true),
            super::StartupRoute::Recover {
                allow_choices: false
            }
        );
    }

    #[test]
    fn managed_start_keeps_one_canonical_session_and_rejects_uncertain_or_foreign_reply() {
        use bcode_harness_models::{
            HarnessOperation, HarnessPreparedSession, HarnessServiceResponse,
            HarnessServiceResponseKind,
        };
        let session = bcode_session_models::SessionId::new();
        let call = super::managed_start_call(
            backend_instance(),
            session,
            "retained-open".into(),
            std::path::Path::new("/isolated/work"),
        )
        .unwrap();
        assert_eq!(call.session_id, Some(session));
        assert!(call.parent_session_id.is_none());
        let bcode_harness_models::HarnessRequestKind::Open(open) = &call.harness.kind else {
            panic!("open required")
        };
        assert_eq!(open.session_id, session.to_string());
        assert_eq!(open.open_id, call.harness.request_id);
        assert!(open.resume.is_none() && open.parent_assistance.is_none());
        let mut response = HarnessServiceResponse {
            schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
            request_id: "retained-open".into(),
            operation: HarnessOperation::Open,
            kind: HarnessServiceResponseKind::Prepared(HarnessPreparedSession {
                open_id: "retained-open".into(),
                session_id: session.to_string(),
                instance_id: "native-account".into(),
                generation: 1,
                native_version: "fixture".into(),
                requested_native_session_id: None,
            }),
        };
        assert_eq!(
            super::managed_started_session(&response, session, "retained-open", "native-account")
                .unwrap(),
            session
        );
        response.kind =
            HarnessServiceResponseKind::Opened(bcode_harness_models::HarnessSessionBinding {
                schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
                session_id: session.to_string(),
                instance_id: "native-account".into(),
                native_session_id: "native-confirmed".into(),
                native_version: "fixture".into(),
                generation: 1,
            });
        assert_eq!(
            super::managed_started_session(&response, session, "retained-open", "native-account")
                .unwrap(),
            session
        );
        assert!(
            super::managed_started_session(
                &response,
                bcode_session_models::SessionId::new(),
                "retained-open",
                "native-account"
            )
            .is_err()
        );
        response.kind =
            HarnessServiceResponseKind::Failure(bcode_harness_models::HarnessFailure::Uncertain);
        assert!(
            super::managed_started_session(&response, session, "retained-open", "native-account")
                .is_err()
        );
    }

    #[test]
    fn sealed_managed_reply_rejects_service_errors_and_invalid_payloads() {
        use bcode_plugin_sdk::application::{PluginServiceError, PluginServiceResponse};
        let mut response = PluginServiceResponse {
            payload: serde_json::to_vec(&bcode_harness_models::HarnessServiceResponse {
                schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
                request_id: "retained-open".into(),
                operation: bcode_harness_models::HarnessOperation::Open,
                kind: bcode_harness_models::HarnessServiceResponseKind::Failure(
                    bcode_harness_models::HarnessFailure::Uncertain,
                ),
            })
            .unwrap(),
            error: None,
        };
        let decoded = super::decode_managed_start_response(&response).unwrap();
        assert!(
            super::managed_started_session(
                &decoded,
                bcode_session_models::SessionId::new(),
                "retained-open",
                "native-account"
            )
            .is_err()
        );
        response.error = Some(PluginServiceError {
            code: "denied".into(),
            message: "not rendered".into(),
        });
        assert!(super::decode_managed_start_response(&response).is_err());
        response.error = None;
        response.payload = b"invalid-json".to_vec();
        assert!(super::decode_managed_start_response(&response).is_err());
        response.payload = vec![b' '; bcode_harness_models::MAX_HARNESS_WIRE_BYTES + 1];
        assert!(super::decode_managed_start_response(&response).is_err());
    }

    #[test]
    fn headless_auth_source_is_explicit_and_unambiguous() {
        let mut config = BcodeConfig::default();
        config.model.profiles.insert(
            "headless-test".to_owned(),
            bcode_config::ModelProfileConfig {
                provider_plugin_id: "test.provider".to_owned(),
                model_id: Some("test-model".to_owned()),
                ..Default::default()
            },
        );
        let mut selection = bcode_config::ResolvedModelSelection {
            provider_plugin_id: Some("test.provider".to_owned()),
            provider_source: None,
            model_id: Some("test-model".to_owned()),
            model_source: None,
            selected_model_id: Some("test-model".to_owned()),
            model_profile: Some("headless-test".to_owned()),
            auth_profile: None,
            auth_pool: None,
            settings: std::collections::BTreeMap::new(),
            request: std::collections::BTreeMap::new(),
            reasoning: bcode_config::ReasoningConfig::default(),
        };
        assert!(super::validate_headless_selection(&config, &selection).is_err());
        selection.auth_profile = Some("explicit-account".to_owned());
        assert!(super::validate_headless_selection(&config, &selection).is_ok());
        selection.auth_pool = Some("rotating-pool".to_owned());
        assert!(super::validate_headless_selection(&config, &selection).is_err());
        selection.auth_pool = None;
        selection
            .settings
            .insert("auth_mode".to_owned(), "none".to_owned());
        assert!(super::validate_headless_selection(&config, &selection).is_err());
        selection.auth_profile = None;
        assert!(super::validate_headless_selection(&config, &selection).is_ok());
        selection.auth_profile = Some(" ".to_owned());
        assert!(super::validate_headless_selection(&config, &selection).is_err());
        selection.auth_profile = None;
        selection.model_profile = Some("missing".to_owned());
        assert!(super::validate_headless_selection(&config, &selection).is_err());
    }

    use super::{BcodeConfig, interactive_startup};

    #[test]
    fn daemon_recovery_labels_only_whitelisted_codes_and_never_messages() {
        for (code, label) in [
            ("daemon_start_timeout", "startup timed out"),
            ("daemon_start_exited", "exited before becoming ready"),
            ("daemon_start_health_check", "health check failed"),
            ("daemon_start_endpoint_occupied", "endpoint is occupied"),
            (
                "daemon_start_coordination_timeout",
                "coordination timed out",
            ),
            ("daemon_start_incompatible", "compatibility check failed"),
            (
                "daemon_start_connection",
                "connection could not be established",
            ),
            ("daemon_start_failed", "could not be started"),
        ] {
            let error = super::CliError::Tui(bcode_tui::TuiError::PluginService {
                code: code.to_owned(),
                message: "secret-bearing-arbitrary-provider-message".to_owned(),
            });
            let message = super::launch_recovery_message(Some(&error));
            assert!(message.contains(label));
            assert!(message.contains(code));
            assert!(!message.contains("secret-bearing"));
            assert!(message.contains("without retrying"));
        }
        let generic = super::launch_recovery_message(None);
        for error in [
            super::CliError::Tui(bcode_tui::TuiError::PluginService {
                code: "daemon_start_future_secret-code".to_owned(),
                message: "daemon_start_timeout secret-bearing-message".to_owned(),
            }),
            super::CliError::InvalidArguments(
                "daemon_start_timeout secret-bearing-message".to_owned(),
            ),
        ] {
            assert_eq!(super::launch_recovery_message(Some(&error)), generic);
        }
    }

    #[test]
    fn failed_launch_recovery_removes_stale_choices_and_warns_about_side_effects() {
        let mut input = bcode_tui::QuickOnboardingInput {
            choices: vec![bcode_tui::QuickConnectionChoice {
                label: "Previously offered account".to_owned(),
                detail: "Local readiness only".to_owned(),
            }],
            recovery: None,
        };
        super::recover_launch_input(&mut input);
        assert_eq!(input.choices, [] as [bcode_tui::QuickConnectionChoice; 0]);
        let recovery = input.recovery.unwrap();
        assert!(recovery.contains("a session may already exist"));
        assert!(recovery.contains("inspect Sessions"));
    }

    #[test]
    fn enrolled_navigation_never_saves_a_model_without_confirmation() {
        for outcome in [
            bcode_tui::QuickOnboardingOutcome::Close,
            bcode_tui::QuickOnboardingOutcome::Advanced,
            bcode_tui::QuickOnboardingOutcome::Connected {
                provider_id: "test-provider".to_owned(),
                profile: "new-account".to_owned(),
            },
        ] {
            let mut saves = 0;
            let confirmed = super::consent_enrolled_selection(&outcome, &[], |_| {
                saves += 1;
                Ok(())
            })
            .unwrap();
            assert!(!confirmed);
            assert_eq!(saves, 0);
        }
    }

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
    fn detected_xai_key_does_not_count_as_explicit_selection() {
        let environment = bcode_config::ConfigEnvironmentSnapshot::new(
            std::collections::BTreeMap::from([(
                "XAI_API_KEY".to_owned(),
                std::ffi::OsString::from("fake-offline-test-key"),
            )]),
            std::path::PathBuf::from("."),
        );
        let mut config = bcode_config::BcodeConfig::default();
        assert!(
            config
                .resolved_model_selection_with_environment(&environment)
                .provider_plugin_id
                .is_some()
        );
        assert!(!super::has_explicit_selection_with_environment(
            &config,
            &environment
        ));
        assert_eq!(
            super::startup_route(
                false,
                super::has_explicit_selection_with_environment(&config, &environment),
                true,
            ),
            super::StartupRoute::Recover {
                allow_choices: true
            }
        );
        config.model.provider_plugin_id = Some("explicit.provider".to_owned());
        // Environment precedence is unchanged; explicitly configured provider intent still
        // prevents onboarding from replacing the user's selection.
        assert!(super::has_explicit_selection_with_environment(
            &config,
            &environment
        ));
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
