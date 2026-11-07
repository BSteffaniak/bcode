//! Reviewed native-instance setup at the CLI boundary; native protocols stay plugin-owned.

use super::CliError;
use bcode_config::{
    BcodeConfig,
    edit::{PluginConfigurationUpdate, PolicyConfigEdit},
};
use bcode_harness_models::{
    HarnessInstallationCandidate, HarnessPrepareCandidateRequest, HarnessPrepareCandidateResponse,
    HarnessPrepareCandidateResponseKind, HarnessPreparedCandidate,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedInstance {
    selection: serde_json::Value,
    instance: bcode_harness_models::HarnessInstance,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceSetup {
    prepared: PreparedInstance,
    configuration: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeSetup {
    native: HarnessPreparedCandidate,
    adapter_configuration: serde_json::Value,
    harness: InstanceSetup,
}

fn unavailable() -> CliError {
    CliError::InvalidArguments(
        "Native setup changed or is unavailable; no fallback or configuration change was selected"
            .into(),
    )
}

/// Discover native-owned installation/account observations, not provider credentials.
async fn candidates() -> Result<(Vec<HarnessInstallationCandidate>, bool), CliError> {
    super::super::ensure_server_running().await?;
    let observed = bcode_client::ManagedSessionClient::default_endpoint()
        .discover_candidates(bcode_ipc::harness::ManagedCandidatesRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
        })
        .await?;
    let mut candidates = Vec::new();
    let mut complete = true;
    for plugin in observed.plugins {
        match plugin.kind {
            bcode_harness_models::HarnessCandidatesResponseKind::Candidates {
                candidates: found,
                complete: covered,
            } => {
                candidates.extend(found);
                complete &= covered;
            }
            bcode_harness_models::HarnessCandidatesResponseKind::Failure(_) => complete = false,
        }
    }
    Ok((candidates, complete))
}

/// One explicit setup journey; closing never falls through to a provider connection.
pub(super) async fn run(
    shell: &mut bcode_tui::TerminalShell,
    no_discovery: bool,
) -> Result<bool, CliError> {
    if no_discovery {
        return Ok(false);
    }
    let Some((observed, complete)) =
        Box::pin(shell.wait("Inspecting native installations", async {
            candidates()
                .await
                .map_err(|error| super::super::startup_tui_error(&error))
        }))
        .await?
    else {
        return Ok(true);
    };
    let mut choices: Vec<_> = observed
        .iter()
        .map(|candidate| bcode_tui::QuickConnectionChoice {
            label: format!("Set up {}", candidate.plugin_id),
            detail: format!(
                "{} · {} · {:?} · {}",
                candidate.native_version,
                candidate.executable.canonical_path,
                candidate.readiness,
                candidate.config_namespace
            ),
        })
        .collect();
    let provider_index = choices.len();
    choices.push(bcode_tui::QuickConnectionChoice {
        label: "Provider connections and advanced settings".into(),
        detail: "Optional alternative; native execution does not require a provider API key".into(),
    });
    let outcome = bcode_tui::run_backend_selection_in_shell(shell, bcode_tui::QuickOnboardingInput {
        choices,
        recovery: Some(format!("Native credentials remain owned by the selected harness. {} No installation, login or configuration change occurs until explicitly reviewed.", if complete { "Inspection complete." } else { "Inspection incomplete; unavailable sources are not evidence of absence." })),
    }).await?;
    match outcome {
        bcode_tui::QuickOnboardingOutcome::Selected(index) if index < observed.len() => {
            Box::pin(configure(shell, observed[index].clone())).await?;
            Ok(true)
        }
        bcode_tui::QuickOnboardingOutcome::Selected(index) if index == provider_index => {
            Box::pin(super::advanced_in_shell(shell, no_discovery)).await?;
            Ok(true)
        }
        bcode_tui::QuickOnboardingOutcome::Advanced => {
            Box::pin(super::advanced_in_shell(shell, no_discovery)).await?;
            Ok(true)
        }
        _ => Ok(true),
    }
}

async fn prepare(
    config: &BcodeConfig,
    request: &HarnessPrepareCandidateRequest,
) -> Result<(NativeSetup, HarnessPrepareCandidateResponse), CliError> {
    let client = bcode_client::ManagedSessionClient::default_endpoint();
    let response = client.prepare_candidate(request.clone()).await?;
    let HarnessPrepareCandidateResponseKind::Prepared(native) = &response.kind else {
        return Err(CliError::InvalidArguments(
            "Native installation preparation was rejected; inspect the installation again".into(),
        ));
    };
    let adapter = config
        .plugins
        .config
        .get(&native.instance.plugin_id)
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| unavailable())?
        .unwrap_or_else(|| serde_json::json!({}));
    let result = client.call_plugin_service(super::HARNESS_CONFIGURATION_INTERFACE.into(), "prepare_setup".into(), serde_json::to_vec(&serde_json::json!({
        "request": request, "response": response, "adapter_configuration": adapter, "make_default": false,
    })).map_err(|_| unavailable())?).await?;
    if result.error.is_some() {
        return Err(unavailable());
    }
    let prepared: NativeSetup =
        serde_json::from_slice(&result.payload).map_err(|_| unavailable())?;
    verify(&prepared, request, &response)?;
    Ok((prepared, response))
}

fn verify(
    prepared: &NativeSetup,
    request: &HarnessPrepareCandidateRequest,
    response: &HarnessPrepareCandidateResponse,
) -> Result<(), CliError> {
    response
        .validate_for(request, &request.candidate.plugin_id)
        .map_err(|_| unavailable())?;
    if !matches!(&response.kind, HarnessPrepareCandidateResponseKind::Prepared(native) if native.as_ref() == &prepared.native)
        || prepared.harness.prepared.instance != prepared.native.instance
        || prepared.harness.prepared.selection
            != serde_json::json!({"kind":"instance","instance_id":request.instance_id})
    {
        return Err(unavailable());
    }
    if prepared.harness.configuration["enabled"] != true
        || prepared.harness.configuration["instances"][&request.instance_id]["enabled"] != true
        || prepared.harness.configuration["instances"][&request.instance_id]["instance"]
            != serde_json::to_value(&prepared.native.instance).map_err(|_| unavailable())?
        || prepared.adapter_configuration["instances"][&request.instance_id]
            != prepared.native.adapter_settings
    {
        return Err(unavailable());
    }
    Ok(())
}

fn plan(config: &BcodeConfig, prepared: &NativeSetup) -> Result<PolicyConfigEdit, CliError> {
    let (current, launch) = bcode_config::recovery::load_current_config_with_recovery_context()?;
    if bcode_config::recovery::execution_fingerprint(&current).map_err(|_| unavailable())?
        != bcode_config::recovery::execution_fingerprint(config).map_err(|_| unavailable())?
    {
        return Err(unavailable());
    }
    let sources = launch.recovery_sources.as_ref().ok_or_else(unavailable)?;
    let roots = bcode_config::recovery::default_allowed_recovery_roots(&sources.cwd)
        .map_err(|_| unavailable())?;
    let reference = bcode_config::recovery::capture_with_sources(&current, sources, &roots)
        .map_err(|_| unavailable())?;
    let adapter: toml::Value =
        toml::Value::try_from(&prepared.adapter_configuration).map_err(|_| unavailable())?;
    let generic: toml::Value =
        toml::Value::try_from(&prepared.harness.configuration).map_err(|_| unavailable())?;
    let updates = [
        PluginConfigurationUpdate {
            plugin_id: &prepared.native.instance.plugin_id,
            expected: current
                .plugins
                .config
                .get(&prepared.native.instance.plugin_id),
            settings: &adapter,
        },
        PluginConfigurationUpdate {
            plugin_id: "bcode.harness",
            expected: current.plugins.config.get("bcode.harness"),
            settings: &generic,
        },
    ];
    Ok(bcode_config::edit::plan_native_setup_with_policy(
        &current, &reference, &roots, &updates,
    )?)
}

async fn configure(
    shell: &mut bcode_tui::TerminalShell,
    candidate: HarnessInstallationCandidate,
) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let request = HarnessPrepareCandidateRequest {
        schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
        request_id: uuid::Uuid::new_v4().to_string(),
        instance_id: format!("native-{}", uuid::Uuid::new_v4()),
        candidate,
    };
    let (prepared, _) = prepare(&config, &request).await?;
    let edit = plan(&config, &prepared)?;
    let outcome = bcode_tui::run_execution_confirmation_in_shell(shell, bcode_tui::QuickOnboardingInput {
        choices: vec![bcode_tui::QuickConnectionChoice {
            label: "Configure this native instance".into(),
            detail: format!("{} · {} · write {}. Existing provider/model preferences and credentials remain unchanged.", prepared.native.instance.plugin_id, prepared.native.instance.config_namespace, edit.path().display()),
        }],
        recovery: Some("Both native-adapter and generic instance settings are committed together after fresh preparation. No model is chosen and no user prompt is sent. You can sign in next using the native terminal flow.".into()),
    }).await?;
    if !matches!(outcome, bcode_tui::QuickOnboardingOutcome::Selected(0)) {
        return Ok(());
    }
    let (fresh, response) = prepare(&config, &request).await?;
    verify(&prepared, &request, &response)?;
    if fresh != prepared {
        return Err(unavailable());
    }
    edit.apply()?;
    let instance = prepared.native.instance;
    let outcome = bcode_tui::run_backend_selection_in_shell(shell, bcode_tui::QuickOnboardingInput {
        choices: vec![
            bcode_tui::QuickConnectionChoice { label: "Choose a model".into(), detail: "Use freshly discovered models and this native account".into() },
            bcode_tui::QuickConnectionChoice { label: "Sign in with the native harness".into(), detail: "Explicitly yield the terminal for native login; credentials do not enter Bcode's vault".into() },
        ],
        recovery: Some("Instance configured. Closing does not launch work. Native login creates a visible empty Bcode session for authenticated ownership; no prompt or default-model execution is used.".into()),
    }).await?;
    if matches!(outcome, bcode_tui::QuickOnboardingOutcome::Selected(1)) {
        sign_in(shell, instance).await?;
    }
    if matches!(outcome, bcode_tui::QuickOnboardingOutcome::Selected(0 | 1))
        && !shell.stop_requested()
    {
        let host = super::super::load_cli_plugin_host()?;
        Box::pin(super::route_backend_selection(&host, shell, false)).await?;
    }
    Ok(())
}

async fn sign_in(
    shell: &mut bcode_tui::TerminalShell,
    instance: bcode_harness_models::HarnessInstance,
) -> Result<(), CliError> {
    use bcode_ipc::harness::{
        MANAGED_SESSION_INTERFACE_ID, MANAGED_SESSION_VERSION, ManagedSessionOperation,
        ManagedSessionRequest,
    };
    let session_id = bcode_session_models::SessionId::new();
    let call = super::managed_start_call(
        instance.clone(),
        session_id,
        uuid::Uuid::new_v4().to_string(),
        &std::env::current_dir()?,
    )?;
    let request = ManagedSessionRequest {
        schema_version: MANAGED_SESSION_VERSION,
        operation: ManagedSessionOperation::Start,
        request_json: serde_json::to_string(&call).map_err(|_| unavailable())?,
    };
    let client = bcode_client::ManagedSessionClient::default_endpoint();
    let response = client
        .call_plugin_service(
            MANAGED_SESSION_INTERFACE_ID.into(),
            "start".into(),
            serde_json::to_vec(&request).map_err(|_| unavailable())?,
        )
        .await?;
    if response.error.is_some() {
        return Err(unavailable());
    }
    let response: bcode_harness_models::HarnessServiceResponse =
        serde_json::from_slice(&response.payload).map_err(|_| unavailable())?;
    super::managed_started_session(
        &response,
        session_id,
        &call.harness.request_id,
        &instance.instance_id,
    )?;
    let interface = client
        .session_terminal_auth_interface(session_id, &instance.instance_id)
        .await?
        .ok_or_else(unavailable)?;
    #[cfg(unix)]
    {
        let mut flow = bcode_client::ManagedSessionClient::default_endpoint()
            .session_auth_flow(session_id, &instance.instance_id)
            .await?;
        let (_, status) = shell.run_managed_auth_handoff(&mut flow, interface).await?;
        if status.readiness != bcode_provider_auth_models::managed::AuthReadiness::Ready {
            shell.show_error_without_retry("Native login is not ready. Inspect the visible setup session before explicitly trying login again; no automatic retry or provider fallback.").await?;
        }
    }
    #[cfg(not(unix))]
    {
        drop(interface);
        shell
            .show_error_without_retry(
                "Native terminal login is unsupported on this platform. No fallback was selected.",
            )
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn proposal() -> (
        HarnessPrepareCandidateRequest,
        HarnessPrepareCandidateResponse,
        NativeSetup,
    ) {
        let candidate = json!({
            "plugin_id":"example.native", "executable":{"canonical_path":"/opt/native",
                "byte_len":12,"modified_unix_nanos":null,"file_key":null},
            "native_version":"1.0.0","config_namespace":"/native/config",
            "custody":"native_managed","readiness":"login_required"
        });
        let request: HarnessPrepareCandidateRequest = serde_json::from_value(json!({
            "schema_version":bcode_harness_models::HARNESS_SCHEMA_VERSION,
            "request_id":"prepare","instance_id":"primary","candidate":candidate
        }))
        .unwrap();
        let instance = json!({"instance_id":"primary","plugin_id":"example.native",
            "executable":"/opt/native","config_namespace":"/native/config",
            "auth":{"provider_id":"example","profile":"native",
                "credential_namespace":"/native/config","custody":"native_managed"}});
        let native = json!({"candidate":candidate,"instance":instance,
            "adapter_settings":{"native_owned":true}});
        let response = serde_json::from_value(json!({
            "schema_version":bcode_harness_models::HARNESS_SCHEMA_VERSION,
            "request_id":"prepare","plugin_id":"example.native",
            "kind":{"kind":"prepared","response":native}
        }))
        .unwrap();
        let prepared = serde_json::from_value(json!({
            "native":native,
            "adapter_configuration":{"instances":{"primary":{"native_owned":true}}},
            "harness":{"prepared":{"selection":{"kind":"instance","instance_id":"primary"},
                "instance":instance},"configuration":{"enabled":true,
                "instances":{"primary":{"enabled":true,"instance":instance}}}}
        }))
        .unwrap();
        (request, response, prepared)
    }

    #[test]
    fn setup_commit_requires_exact_native_and_generic_instance_echoes() {
        let (request, response, prepared) = proposal();
        assert!(verify(&prepared, &request, &response).is_ok());
        for pointer in [
            "/harness/configuration/instances/primary/instance/executable",
            "/harness/configuration/instances/primary/instance/auth/credential_namespace",
            "/harness/configuration/instances/primary/instance/instance_id",
            "/harness/prepared/selection/instance_id",
        ] {
            let mut changed = serde_json::to_value(&prepared).unwrap();
            *changed.pointer_mut(pointer).unwrap() = json!("changed");
            let changed = serde_json::from_value(changed).unwrap();
            assert!(verify(&changed, &request, &response).is_err(), "{pointer}");
        }
        let mut changed = response;
        changed.request_id = "foreign".into();
        assert!(verify(&prepared, &request, &changed).is_err());
    }
}
