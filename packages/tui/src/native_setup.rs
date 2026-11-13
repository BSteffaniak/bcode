//! Reviewed first-run setup: native installations and other agents' credentials side by
//! side. Native protocols and credential formats stay plugin-owned.

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

/// Result of one setup journey.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeSetupOutcome {
    /// Nothing changed.
    Unchanged,
    /// Configuration changed; re-read execution choices.
    Refresh,
    /// The user approved another agent's credentials; the caller chooses a model for them.
    ExternalApproved(bcode_provider_auth_models::external::ExternalAuthCandidate),
}

/// Present credentials of other coding agents; discovery problems never block setup.
async fn external_candidates() -> Vec<bcode_provider_auth_models::external::ExternalAuthCandidate> {
    use bcode_provider_auth_models::external::ExternalAuthApplication as _;
    bcode_client::BcodeClient::default_endpoint()
        .discover_external_auth()
        .await
        .map(|candidates| {
            candidates
                .into_iter()
                .filter(|candidate| candidate.available)
                .collect()
        })
        .unwrap_or_default()
}

/// Confirm and persist approval for one discovered connection.
pub async fn approve_external(
    shell: &mut crate::TerminalShell,
    candidate: &bcode_provider_auth_models::external::ExternalAuthCandidate,
) -> Result<bool, CliError> {
    use bcode_provider_auth_models::external::ExternalAuthApplication as _;
    let outcome = crate::run_execution_confirmation_in_shell(shell, crate::QuickOnboardingInput {
        choices: vec![crate::QuickConnectionChoice {
            label: format!("Allow Bcode to use {} credentials", candidate.application),
            detail: format!(
                "Every local Bcode application may use them for {}. Requests use that account's quota or billing. Disconnecting later (`bcode auth disconnect`) does not sign you out of {}.",
                candidate.target.provider_id, candidate.application
            ),
        }],
        recovery: Some("Nothing is approved until you confirm. Close to leave everything unchanged.".into()),
    })
    .await?;
    if !matches!(outcome, crate::QuickOnboardingOutcome::Selected(0)) {
        return Ok(false);
    }
    bcode_client::BcodeClient::default_endpoint()
        .approve_external_auth(candidate.clone())
        .await
        .map_err(|error| {
            CliError::Io(std::io::Error::other(format!(
                "External connection failed: {error}."
            )))
        })?;
    Ok(true)
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
pub async fn run(
    shell: &mut crate::TerminalShell,
    no_discovery: bool,
) -> Result<NativeSetupOutcome, CliError> {
    if no_discovery {
        return Ok(NativeSetupOutcome::Unchanged);
    }
    let Some((observed, complete)) =
        Box::pin(shell.wait("Inspecting native installations", async {
            candidates().await
        }))
        .await?
    else {
        return Ok(NativeSetupOutcome::Refresh);
    };
    let external = external_candidates().await;
    let entries = ordered_entries(
        &observed
            .iter()
            .map(|candidate| {
                bcode_session_view::external_projection::harness_display_label(&candidate.plugin_id)
            })
            .collect::<Vec<_>>(),
        &external
            .iter()
            .map(|candidate| agent_name(&candidate.application).to_owned())
            .collect::<Vec<_>>(),
    );
    let mut choices: Vec<_> = entries
        .iter()
        .map(|entry| match *entry {
            SetupEntry::Native(index) => {
                let candidate = &observed[index];
                crate::QuickConnectionChoice {
                    label: format!(
                        "Run {}",
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
                }
            }
            SetupEntry::External(index) => {
                let candidate = &external[index];
                crate::QuickConnectionChoice {
                    label: format!("Use {} credentials in Bcode", candidate.application),
                    detail: format!(
                        "{} ({}) · read in place when Bcode makes requests; never copied, refreshed, or modified",
                        candidate.target.provider_id, candidate.target.method_id
                    ),
                }
            }
        })
        .collect();
    let provider_index = choices.len();
    choices.push(crate::QuickConnectionChoice {
        label: "Provider connections and advanced settings".into(),
        detail: "Optional alternative; native execution and existing agent credentials do not require a provider API key".into(),
    });
    let outcome = crate::run_backend_selection_in_shell(shell, crate::QuickOnboardingInput {
        choices,
        recovery: Some(format!("{} Each tool keeps its own sign-in; nothing is installed or changed until you confirm.", if complete { "Found these coding agents on this machine." } else { "Found these coding agents; some locations could not be checked." })),
    }).await?;
    match outcome {
        crate::QuickOnboardingOutcome::Selected(index) if index < provider_index => {
            match entries[index] {
                SetupEntry::Native(native) => {
                    let refresh = Box::pin(configure(shell, observed[native].clone())).await?;
                    Ok(if refresh {
                        NativeSetupOutcome::Refresh
                    } else {
                        NativeSetupOutcome::Unchanged
                    })
                }
                SetupEntry::External(external_index) => {
                    let candidate = &external[external_index];
                    Ok(if Box::pin(approve_external(shell, candidate)).await? {
                        NativeSetupOutcome::ExternalApproved(candidate.clone())
                    } else {
                        NativeSetupOutcome::Unchanged
                    })
                }
            }
        }
        crate::QuickOnboardingOutcome::Selected(index) if index == provider_index => {
            shell.run_setup(!no_discovery).await?;
            Ok(NativeSetupOutcome::Refresh)
        }
        crate::QuickOnboardingOutcome::Advanced => {
            shell.run_setup(!no_discovery).await?;
            Ok(NativeSetupOutcome::Refresh)
        }
        _ => Ok(NativeSetupOutcome::Unchanged),
    }
}

/// One Setup picker entry, by index into its source list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupEntry {
    Native(usize),
    External(usize),
}

/// Agent a credential source belongs to: `Codex (ChatGPT sign-in)` belongs to `Codex`.
fn agent_name(application: &str) -> &str {
    application
        .split_once(" (")
        .map_or(application, |(agent, _)| agent)
}

/// Group entries by agent, keeping each agent's native installation before its credentials and
/// otherwise preserving discovery order.
fn ordered_entries(native_agents: &[String], external_agents: &[String]) -> Vec<SetupEntry> {
    let entries: Vec<(&str, SetupEntry)> = native_agents
        .iter()
        .enumerate()
        .map(|(index, agent)| (agent.as_str(), SetupEntry::Native(index)))
        .chain(
            external_agents
                .iter()
                .enumerate()
                .map(|(index, agent)| (agent.as_str(), SetupEntry::External(index))),
        )
        .collect();
    let mut agents: Vec<&str> = Vec::new();
    for (agent, _) in &entries {
        if !agents.contains(agent) {
            agents.push(agent);
        }
    }
    agents
        .iter()
        .flat_map(|agent| {
            entries
                .iter()
                .filter(move |(other, _)| other == agent)
                .map(|(_, entry)| *entry)
        })
        .collect()
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

/// Effective native catalog as configuration, with the instance already configured for this
/// exact installation, if any.
async fn native_catalog_config(
    candidate: &HarnessInstallationCandidate,
) -> Result<(BcodeConfig, Option<bcode_harness_models::HarnessInstance>), CliError> {
    let mut config = bcode_config::load_config()?;
    let client = bcode_client::ManagedSessionClient::default_endpoint();
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
    let Some(generic) = resolution.configurations.get("bcode.harness") else {
        return Ok((config, None));
    };
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
                    return Ok((config, Some(instance)));
                }
            }
        }
    }
    config.plugins.config.insert(
        "bcode.harness".into(),
        toml::Value::try_from(generic).map_err(|_| unavailable("invalid native catalog"))?,
    );
    Ok((config, None))
}

/// Register a signed-in installation without prompting, so its models are offered directly.
///
/// Registration is Bcode's own local state and grants no execution authority; choosing one of
/// its models later is the user's decision to run that tool. Returns the registered instance.
pub async fn auto_register(
    candidate: HarnessInstallationCandidate,
) -> Result<bcode_harness_models::HarnessInstance, CliError> {
    let client = bcode_client::ManagedSessionClient::default_endpoint();
    let snapshot = client.native_registration_snapshot().await?;
    let (config, existing) = native_catalog_config(&candidate).await?;
    if let Some(instance) = existing {
        return Ok(instance);
    }
    let request = HarnessPrepareCandidateRequest {
        schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
        request_id: uuid::Uuid::new_v4().to_string(),
        instance_id: friendly_instance_id(&config, &candidate.plugin_id),
        candidate,
    };
    let (prepared, response) = prepare(&config, &request).await?;
    verify(&prepared, &request, &response)?;
    let registration = bcode_harness_models::Registration {
        instance: prepared.native.instance.clone(),
        adapter_settings: prepared.native.adapter_settings.clone(),
    };
    client
        .commit_native_registration(bcode_ipc::harness::ManagedRegistrationCommitRequest {
            prepare: request,
            expected_revision: snapshot.revision,
            expected_registration: registration,
        })
        .await?;
    Ok(prepared.native.instance)
}

async fn configure(
    shell: &mut crate::TerminalShell,
    candidate: HarnessInstallationCandidate,
) -> Result<bool, CliError> {
    let signed_in = candidate.readiness == HarnessCandidateReadiness::ExistingNativeLogin;
    let client = bcode_client::ManagedSessionClient::default_endpoint();
    let snapshot = client.native_registration_snapshot().await?;
    let (config, existing) = native_catalog_config(&candidate).await?;
    if let Some(instance) = existing {
        if !signed_in {
            sign_in(shell, instance).await?;
        }
        return Ok(true);
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
    // Already signed in: go straight to choosing a model.
    if signed_in {
        return Ok(!shell.stop_requested());
    }
    let outcome = crate::run_backend_selection_in_shell(shell, crate::QuickOnboardingInput {
        choices: vec![
            crate::QuickConnectionChoice { label: "Sign in".into(), detail: "Run the tool's own sign-in in this terminal; your credentials never pass through Bcode".into() },
            crate::QuickConnectionChoice { label: "Choose a model".into(), detail: "Pick a model and start chatting".into() },
        ],
        recovery: Some("All set. Sign in to the tool first, then choose a model.".into()),
    }).await?;
    if matches!(outcome, crate::QuickOnboardingOutcome::Selected(0)) {
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

    #[test]
    fn setup_entries_group_each_agent_with_native_first() {
        let native = ["Codex".to_owned(), "Claude Code".to_owned()];
        let external = [
            "OpenCode".to_owned(),
            agent_name("Codex (ChatGPT sign-in)").to_owned(),
            agent_name("Codex (API key)").to_owned(),
        ];
        assert_eq!(
            ordered_entries(&native, &external),
            [
                SetupEntry::Native(0),
                SetupEntry::External(1),
                SetupEntry::External(2),
                SetupEntry::Native(1),
                SetupEntry::External(0),
            ]
        );
        assert_eq!(agent_name("Pi"), "Pi");
    }

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
