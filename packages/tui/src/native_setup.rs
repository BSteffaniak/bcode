//! Reviewed native-instance setup at the CLI boundary; native protocols stay plugin-owned.

use crate::TuiError as CliError;
use bcode_config::BcodeConfig;
use bcode_harness_models::{
    HarnessCandidateReadiness, HarnessInstallationCandidate, HarnessPrepareCandidateRequest,
    HarnessPrepareCandidateResponse, HarnessPrepareCandidateResponseKind, HarnessPreparedCandidate,
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

/// Setup refusal naming the failed step; reasons are fixed text, never native output.
fn unavailable(reason: &'static str) -> CliError {
    CliError::Io(std::io::Error::other(format!(
        "Native setup changed or is unavailable ({reason}); no fallback or configuration change was selected"
    )))
}

/// Discover native-owned installation/account observations, not provider credentials.
async fn candidates() -> Result<(Vec<HarnessInstallationCandidate>, bool), CliError> {
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
pub async fn run(shell: &mut crate::TerminalShell, no_discovery: bool) -> Result<bool, CliError> {
    if no_discovery {
        return Ok(false);
    }
    let Some((observed, complete)) =
        Box::pin(shell.wait("Inspecting native installations", async {
            candidates().await
        }))
        .await?
    else {
        return Ok(true);
    };
    let mut choices: Vec<_> = observed
        .iter()
        .map(|candidate| crate::QuickConnectionChoice {
            label: format!(
                "Use {}",
                bcode_session_view::external_projection::harness_display_label(
                    &candidate.plugin_id
                )
            ),
            detail: format!(
                "{} · {} · {} · {}",
                candidate.native_version,
                readiness_label(candidate.readiness),
                home_relative(candidate.executable.invocation()),
                home_relative(&candidate.config_namespace)
            ),
        })
        .collect();
    let provider_index = choices.len();
    choices.push(crate::QuickConnectionChoice {
        label: "Provider connections and advanced settings".into(),
        detail: "Optional alternative; native execution does not require a provider API key".into(),
    });
    let outcome = crate::run_backend_selection_in_shell(shell, crate::QuickOnboardingInput {
        choices,
        recovery: Some(format!("{} Each tool keeps its own sign-in; nothing is installed or changed until you confirm.", if complete { "Found these coding agents on this machine." } else { "Found these coding agents; some locations could not be checked." })),
    }).await?;
    match outcome {
        crate::QuickOnboardingOutcome::Selected(index) if index < observed.len() => {
            Box::pin(configure(shell, observed[index].clone())).await
        }
        crate::QuickOnboardingOutcome::Selected(index) if index == provider_index => {
            shell.run_setup(!no_discovery).await?;
            Ok(true)
        }
        crate::QuickOnboardingOutcome::Advanced => {
            shell.run_setup(!no_discovery).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// A path for display, abbreviating the user's home directory as `~`.
fn home_relative(path: &str) -> String {
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty() && home != "/")
        .and_then(|home| {
            path.strip_prefix(&home)
                .filter(|rest| rest.is_empty() || rest.starts_with('/'))
                .map(|rest| format!("~{rest}"))
        })
        .unwrap_or_else(|| path.to_owned())
}

const fn readiness_label(readiness: HarnessCandidateReadiness) -> &'static str {
    match readiness {
        HarnessCandidateReadiness::ExistingNativeLogin => "signed in",
        HarnessCandidateReadiness::LoginRequired => "sign-in required",
        HarnessCandidateReadiness::Unsupported => "unsupported version",
        HarnessCandidateReadiness::Unknown => "sign-in status unknown",
    }
}

async fn prepare(
    config: &BcodeConfig,
    request: &HarnessPrepareCandidateRequest,
) -> Result<(NativeSetup, HarnessPrepareCandidateResponse), CliError> {
    let client = bcode_client::ManagedSessionClient::default_endpoint();
    let response = client.prepare_candidate(request.clone()).await?;
    let HarnessPrepareCandidateResponseKind::Prepared(native) = &response.kind else {
        return Err(unavailable("installation preparation rejected"));
    };
    let adapter = config
        .plugins
        .config
        .get(&native.instance.plugin_id)
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| unavailable("adapter settings unreadable"))?
        .unwrap_or_else(|| serde_json::json!({}));
    let result = client.call_plugin_service("bcode.harness.configuration/v1".into(), "prepare_setup".into(), serde_json::to_vec(&serde_json::json!({
        "request": request, "response": response, "adapter_configuration": adapter, "make_default": false,
    })).map_err(|_| unavailable("request encoding"))?).await?;
    if result.error.is_some() {
        return Err(unavailable("instance settings refused"));
    }
    let prepared: NativeSetup = serde_json::from_slice(&result.payload)
        .map_err(|_| unavailable("instance settings unreadable"))?;
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
        .map_err(|_| unavailable("preparation mismatch"))?;
    if !matches!(&response.kind, HarnessPrepareCandidateResponseKind::Prepared(native) if native.as_ref() == &prepared.native)
        || prepared.harness.prepared.instance != prepared.native.instance
        || prepared.harness.prepared.selection
            != serde_json::json!({"kind":"instance","instance_id":request.instance_id})
    {
        return Err(unavailable("preparation mismatch"));
    }
    if prepared.harness.configuration["enabled"] != true
        || prepared.harness.configuration["instances"][&request.instance_id]["enabled"] != true
        || prepared.harness.configuration["instances"][&request.instance_id]["instance"]
            != serde_json::to_value(&prepared.native.instance)
                .map_err(|_| unavailable("instance encoding"))?
        || prepared.adapter_configuration["instances"][&request.instance_id]
            != prepared.native.adapter_settings
    {
        return Err(unavailable("instance settings mismatch"));
    }
    Ok(())
}

/// A readable instance identity derived from the plugin (`bcode.claude-code` becomes
/// `claude-code`), suffixed only when that name is already configured.
fn friendly_instance_id(config: &BcodeConfig, plugin_id: &str) -> String {
    let base: String = plugin_id
        .rsplit(['.', '/', ':'])
        .next()
        .unwrap_or_default()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let base = if base.trim_matches('-').is_empty() {
        "native".to_owned()
    } else {
        base.trim_matches('-').to_owned()
    };
    let taken = |id: &str| {
        [plugin_id, "bcode.harness"].iter().any(|owner| {
            config
                .plugins
                .config
                .get(*owner)
                .and_then(|settings| settings.get("instances"))
                .and_then(toml::Value::as_table)
                .is_some_and(|instances| instances.contains_key(id))
        })
    };
    if !taken(&base) {
        return base;
    }
    // Configured instances are bounded, so a free suffix exists well within this range.
    (2_u32..=1024)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| format!("{base}-{}", uuid::Uuid::new_v4()))
}

async fn configure(
    shell: &mut crate::TerminalShell,
    candidate: HarnessInstallationCandidate,
) -> Result<bool, CliError> {
    let mut config = bcode_config::load_config()?;
    let client = bcode_client::ManagedSessionClient::default_endpoint();
    let snapshot = client.native_registration_snapshot().await?;
    let resolution = client.native_registration_resolution().await?;
    // Use the effective identity catalog for reuse and collision-free local IDs.
    if resolution
        .configurations
        .get("bcode.harness")
        .and_then(|value| value.get("enabled"))
        == Some(&serde_json::json!(false))
    {
        return Err(unavailable(
            "native registrations are disabled by configuration",
        ));
    }
    if let Some(generic) = resolution.configurations.get("bcode.harness") {
        if let Some(entries) = generic
            .get("instances")
            .and_then(serde_json::Value::as_object)
        {
            for entry in entries.values() {
                if entry.get("enabled") == Some(&serde_json::json!(false)) {
                    continue;
                }
                if let Some(value) = entry.get("instance") {
                    let instance: bcode_harness_models::HarnessInstance =
                        serde_json::from_value(value.clone())?;
                    if instance.plugin_id == candidate.plugin_id
                        && instance.executable == candidate.executable.invocation()
                        && instance.config_namespace == candidate.config_namespace
                    {
                        if candidate.readiness != HarnessCandidateReadiness::ExistingNativeLogin {
                            sign_in(shell, instance).await?;
                        }
                        return Ok(true);
                    }
                }
            }
        }
        config.plugins.config.insert(
            "bcode.harness".into(),
            toml::Value::try_from(generic).map_err(|_| unavailable("invalid native catalog"))?,
        );
    }
    let request = HarnessPrepareCandidateRequest {
        schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
        request_id: uuid::Uuid::new_v4().to_string(),
        instance_id: friendly_instance_id(&config, &candidate.plugin_id),
        candidate,
    };
    let (prepared, _) = prepare(&config, &request).await?;
    let registration = bcode_harness_models::Registration {
        instance: prepared.native.instance.clone(),
        adapter_settings: prepared.native.adapter_settings.clone(),
    };
    let outcome = crate::run_execution_confirmation_in_shell(shell, crate::QuickOnboardingInput {
        choices: vec![crate::QuickConnectionChoice {
            label: "Save and continue".into(),
            detail: format!("{} · {} · saved to {}", bcode_session_view::external_projection::harness_display_label(&prepared.native.instance.plugin_id), home_relative(&prepared.native.instance.config_namespace), "Bcode local registration state"),
        }],
        recovery: Some("Bcode will remember this installation. Your other providers, models and credentials stay as they are, and nothing is sent yet.".into()),
    }).await?;
    if !matches!(outcome, crate::QuickOnboardingOutcome::Selected(0)) {
        return Ok(false);
    }
    let (fresh, response) = prepare(&config, &request).await?;
    verify(&prepared, &request, &response)?;
    if fresh != prepared {
        return Err(unavailable("installation changed"));
    }
    client
        .commit_native_registration(bcode_ipc::harness::ManagedRegistrationCommitRequest {
            prepare: request,
            expected_revision: snapshot.revision,
            expected_registration: registration,
        })
        .await?;
    let instance = prepared.native.instance;
    let outcome = crate::run_backend_selection_in_shell(shell, crate::QuickOnboardingInput {
        choices: vec![
            crate::QuickConnectionChoice { label: "Choose a model".into(), detail: "Pick a model and start chatting".into() },
            crate::QuickConnectionChoice { label: "Sign in".into(), detail: "Run the tool's own sign-in in this terminal; your credentials never pass through Bcode".into() },
        ],
        recovery: Some("All set. Choose a model to start, or sign in first if you haven't yet.".into()),
    }).await?;
    if matches!(outcome, crate::QuickOnboardingOutcome::Selected(1)) {
        sign_in(shell, instance).await?;
    }
    Ok(
        matches!(outcome, crate::QuickOnboardingOutcome::Selected(0 | 1))
            && !shell.stop_requested(),
    )
}

async fn sign_in(
    shell: &mut crate::TerminalShell,
    instance: bcode_harness_models::HarnessInstance,
) -> Result<(), CliError> {
    use bcode_ipc::harness::{
        MANAGED_SESSION_INTERFACE_ID, MANAGED_SESSION_VERSION, ManagedSessionOperation,
        ManagedSessionRequest,
    };
    let session_id = bcode_session_models::SessionId::new();
    let call = crate::execution_flow::managed_start_call(
        instance.clone(),
        session_id,
        uuid::Uuid::new_v4().to_string(),
        &std::env::current_dir()?,
    )?;
    let request = ManagedSessionRequest {
        schema_version: MANAGED_SESSION_VERSION,
        operation: ManagedSessionOperation::Start,
        request_json: serde_json::to_string(&call).map_err(|_| unavailable("request encoding"))?,
    };
    let client = bcode_client::ManagedSessionClient::default_endpoint();
    let response = client
        .call_plugin_service(
            MANAGED_SESSION_INTERFACE_ID.into(),
            "start".into(),
            serde_json::to_vec(&request).map_err(|_| unavailable("request encoding"))?,
        )
        .await?;
    if response.error.is_some() {
        return Err(unavailable("session start refused"));
    }
    let response: bcode_harness_models::HarnessServiceResponse =
        serde_json::from_slice(&response.payload)
            .map_err(|_| unavailable("session start unreadable"))?;
    crate::execution_flow::managed_started_session(
        &response,
        session_id,
        &call.harness.request_id,
        &instance.instance_id,
    )?;
    let interface = client
        .session_terminal_auth_interface(session_id, &instance.instance_id)
        .await?
        .ok_or_else(|| unavailable("terminal login unavailable"))?;
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
    fn instance_ids_are_readable_and_never_reuse_a_configured_name() {
        let mut config = BcodeConfig::default();
        assert_eq!(
            friendly_instance_id(&config, "bcode.claude-code"),
            "claude-code"
        );
        config.plugins.config.insert(
            "bcode.harness".into(),
            toml::from_str("[instances.claude-code]\nenabled = true\n").unwrap(),
        );
        assert_eq!(
            friendly_instance_id(&config, "bcode.claude-code"),
            "claude-code-2"
        );
        assert_eq!(
            friendly_instance_id(&config, "bcode.codex-harness"),
            "codex-harness"
        );
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
