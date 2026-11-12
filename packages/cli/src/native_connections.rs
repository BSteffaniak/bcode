//! Native first-use presentation. Plugins own discovery, probes and compatibility admission.
use crate::CliError;
use bcode_harness_models::native_connections::{
    MAX_NATIVE_CONNECTIONS, NATIVE_PREPARATION_INTERFACE, NativeCandidate, NativeConnectionRecord,
    NativeConnectionRequest, NativeConnectionResponse,
};

fn blocked() -> CliError {
    CliError::InvalidArguments("Native connection setup is blocked or changed. Inspect connections before retrying. No model/account fallback or native session was started.".into())
}

trait Boundary: Sync {
    fn call(
        &self,
        request: NativeConnectionRequest,
    ) -> impl std::future::Future<Output = Result<NativeConnectionResponse, CliError>> + Send;
}
impl Boundary for bcode_client::ManagedSessionClient {
    async fn call(
        &self,
        request: NativeConnectionRequest,
    ) -> Result<NativeConnectionResponse, CliError> {
        use bcode_harness_models::native_connections::NATIVE_CONNECTION_APPLICATION_INTERFACE;
        let response = self
            .call_plugin_service(
                NATIVE_CONNECTION_APPLICATION_INTERFACE.into(),
                "execute".into(),
                serde_json::to_vec(&request).map_err(|_| blocked())?,
            )
            .await
            .map_err(|_| blocked())?;
        if response.error.is_some()
            || response.payload.len()
                > bcode_harness_models::native_connections::MAX_NATIVE_CONNECTION_BYTES
        {
            return Err(blocked());
        }
        serde_json::from_slice(&response.payload).map_err(|_| blocked())
    }
}
trait Choices: Send {
    fn choose(
        &mut self,
        choices: Vec<bcode_tui::QuickConnectionChoice>,
        explanation: String,
    ) -> impl std::future::Future<Output = Result<Option<usize>, CliError>>;
}
struct ShellChoices<'a>(&'a mut bcode_tui::TerminalShell);
impl Choices for ShellChoices<'_> {
    async fn choose(
        &mut self,
        choices: Vec<bcode_tui::QuickConnectionChoice>,
        explanation: String,
    ) -> Result<Option<usize>, CliError> {
        match Box::pin(bcode_tui::run_backend_selection_in_shell(
            self.0,
            bcode_tui::QuickOnboardingInput {
                choices,
                recovery: Some(explanation),
            },
        ))
        .await?
        {
            bcode_tui::QuickOnboardingOutcome::Selected(index) => Ok(Some(index)),
            bcode_tui::QuickOnboardingOutcome::Close
            | bcode_tui::QuickOnboardingOutcome::Advanced => Ok(None),
            bcode_tui::QuickOnboardingOutcome::Connected { .. } => Err(blocked()),
        }
    }
}

/// Discover only enabled services declaring the generic native preparation contract.
#[must_use]
pub fn providers(host: &bcode_plugin::PluginHost) -> Vec<String> {
    host.service_registry()
        .providers_for(NATIVE_PREPARATION_INTERFACE)
        .map(|providers| providers.iter().cloned().collect())
        .unwrap_or_default()
}

async fn records(boundary: &impl Boundary) -> Result<Vec<NativeConnectionRecord>, CliError> {
    let NativeConnectionResponse::Records { records } =
        boundary.call(NativeConnectionRequest::List {}).await?
    else {
        return Err(blocked());
    };
    if records.len() > MAX_NATIVE_CONNECTIONS
        || records.iter().any(|record| record.validate().is_err())
    {
        return Err(blocked());
    }
    let unique = records
        .iter()
        .map(|record| &record.candidate.candidate_id)
        .collect::<std::collections::BTreeSet<_>>();
    if unique.len() != records.len() {
        return Err(blocked());
    }
    Ok(records)
}

fn description(candidate: &NativeCandidate) -> String {
    format!(
        "Plugin: {}. Executable: {}. Native config/credential namespace: {}. Native-managed account and billing; no vault import or model-provider fallback. Preparation must verify isolated settings and compatibility before publication.",
        candidate.plugin_id, candidate.executable, candidate.config_namespace
    )
}
fn choice(label: &str, detail: String) -> bcode_tui::QuickConnectionChoice {
    bcode_tui::QuickConnectionChoice {
        label: label.into(),
        detail,
    }
}
fn mutation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Review a passive candidate, approve it, separately consent to native probes, then publish.
/// Returns true only after the server returns the exact prepared connection. No session starts.
/// # Errors
/// Failures or stale replies block setup without selecting another backend/account.
pub async fn connect_in_shell(
    shell: &mut bcode_tui::TerminalShell,
    plugins: &[String],
) -> Result<bool, CliError> {
    connect_with(
        &bcode_client::ManagedSessionClient::default_endpoint(),
        &mut ShellChoices(shell),
        plugins,
    )
    .await
}

async fn connect_with(
    boundary: &impl Boundary,
    ui: &mut impl Choices,
    plugins: &[String],
) -> Result<bool, CliError> {
    if plugins.len() > MAX_NATIVE_CONNECTIONS {
        return Err(blocked());
    }
    let mut candidates = Vec::new();
    let mut identities = std::collections::BTreeSet::new();
    for plugin in plugins {
        let NativeConnectionResponse::Candidates {
            candidates: discovered,
        } = boundary
            .call(NativeConnectionRequest::Discover {
                plugin_id: plugin.clone(),
            })
            .await?
        else {
            return Err(blocked());
        };
        for candidate in discovered {
            if candidate.validate().is_err()
                || candidate.plugin_id != *plugin
                || !identities.insert(candidate.candidate_id.clone())
                || candidates.len() == MAX_NATIVE_CONNECTIONS
            {
                return Err(blocked());
            }
            candidates.push(candidate);
        }
    }
    if candidates.is_empty() {
        return Err(blocked());
    }
    let Some(index) = ui.choose(candidates.iter().map(|candidate| choice(&format!("Connect native installation: {}", candidate.plugin_id), description(candidate))).collect(), "Passive discovery does not verify login or compatibility and has not launched a process. Close to leave setup without a fallback.".into()).await? else { return Ok(false); };
    let candidate = candidates.get(index).ok_or_else(blocked)?.clone();
    let existing = records(boundary).await?;
    let revision = existing
        .iter()
        .find(|record| record.candidate.candidate_id == candidate.candidate_id)
        .map_or(0, |record| record.revision);
    let Some(decision) = ui.choose(vec![choice("Cancel", "Do not change native connection consent.".into()), choice("Approve this exact installation for all local Bcode applications", description(&candidate))], "This grants native-account consent only. Compatibility/status processes require a separate confirmation; no generation starts.".into()).await? else { return Ok(false); };
    if decision == 0 {
        return Ok(false);
    }
    if decision != 1 {
        return Err(blocked());
    }
    // Retain exact requests and mutation IDs; never retry an ambiguous mutation with a new ID.
    let approval = NativeConnectionRequest::Approve {
        candidate: candidate.clone(),
        expected_revision: revision,
        mutation_id: mutation_id(),
    };
    let NativeConnectionResponse::Record { record } = boundary.call(approval).await? else {
        return Err(blocked());
    };
    if record.validate().is_err()
        || record.candidate != candidate
        || !record.approved
        || record.prepared.is_some()
        || record.revision != revision.checked_add(1).ok_or_else(blocked)?
    {
        return Err(blocked());
    }
    let Some(decision) = ui.choose(vec![choice("Keep approval only; do not run probes", "No prepared connection will be published by this action.".into()), choice("Allow native compatibility and local account-status probes", description(&candidate))], "Separate process consent: the selected plugin may execute this exact installation to verify compatibility, native account status, billing and settings isolation. This is not permission to generate a conversation. Unsupported profiles remain blocked.".into()).await? else { return Ok(false); };
    if decision == 0 {
        return Ok(false);
    }
    if decision != 1 {
        return Err(blocked());
    }
    let preparation = NativeConnectionRequest::Prepare {
        candidate_id: candidate.candidate_id.clone(),
        expected_revision: record.revision,
        mutation_id: mutation_id(),
    };
    let NativeConnectionResponse::Record { record: published } = boundary.call(preparation).await?
    else {
        return Err(blocked());
    };
    if published.validate().is_err()
        || published.candidate != candidate
        || !published.approved
        || published.revision != record.revision.checked_add(1).ok_or_else(blocked)?
        || published.prepared.is_none()
    {
        return Err(blocked());
    }
    Ok(true)
}

/// Run native first-use setup without requiring a configured model or vault.
/// Publication does not start a conversation; startup offers the existing managed picker.
/// # Errors
/// Nonterminal use, disabled discovery, or failed native preparation blocks setup.
pub async fn connect_command(no_discovery: bool) -> Result<(), CliError> {
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(blocked());
    }
    let config = bcode_config::load_config()?;
    if !config
        .onboarding
        .credential_discovery_enabled(no_discovery, &bcode_config::ProcessConfigEnvironment)
    {
        return Err(blocked());
    }
    let mut host = crate::load_cli_plugin_host()?;
    let plugins = providers(&host);
    host.deactivate_all()?;
    let mut shell = bcode_tui::TerminalShell::enter()?;
    let result = connect_in_shell(&mut shell, &plugins).await;
    shell.finish()?;
    if result? {
        println!(
            "Native connection prepared and published. No conversation started. Select this managed backend at startup to use it."
        );
    }
    Ok(())
}

/// Revoke native consent through the same reviewed UI as startup, without source logout.
/// # Errors
/// Nonterminal use or invalid/stale durable state fails without mutation replay.
pub async fn revoke_command() -> Result<(), CliError> {
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(blocked());
    }
    let mut shell = bcode_tui::TerminalShell::enter()?;
    let result = revoke_in_shell(&mut shell).await;
    shell.finish()?;
    result
}

/// List native approvals without model credentials, probes or source logout.
/// # Errors
/// Fails on invalid or unavailable durable state.
pub async fn list_command() -> Result<(), CliError> {
    let client = bcode_client::ManagedSessionClient::default_endpoint();
    for record in records(&client).await? {
        println!(
            "{} revision {} [{}; {}] {}",
            record.candidate.candidate_id,
            record.revision,
            if record.approved {
                "approved"
            } else {
                "revoked"
            },
            if record.prepared.is_some() {
                "prepared"
            } else {
                "not prepared"
            },
            description(&record.candidate)
        );
    }
    Ok(())
}

/// Revoke only the exact displayed consent revision, without modifying the native application.
/// # Errors
/// Stale/ambiguous responses fail without automatic mutation replay.
pub async fn revoke_in_shell(shell: &mut bcode_tui::TerminalShell) -> Result<(), CliError> {
    revoke_with(
        &bcode_client::ManagedSessionClient::default_endpoint(),
        &mut ShellChoices(shell),
    )
    .await
}
async fn revoke_with(boundary: &impl Boundary, ui: &mut impl Choices) -> Result<(), CliError> {
    let records = records(boundary).await?;
    let active = records
        .iter()
        .filter(|record| record.approved)
        .collect::<Vec<_>>();
    if active.is_empty() {
        return Ok(());
    }
    let Some(index) = ui.choose(active.iter().map(|record| choice(&format!("Disconnect {}", record.candidate.candidate_id), description(&record.candidate))).collect(), "Disconnect removes Bcode permission for future use; it does not cancel existing work or log out the native application.".into()).await? else { return Ok(()); };
    let record = *active.get(index).ok_or_else(blocked)?;
    match ui
        .choose(
            vec![
                choice("Cancel", "Keep current consent.".into()),
                choice("Revoke Bcode consent", description(&record.candidate)),
            ],
            "No native credentials or declarative configuration will be modified.".into(),
        )
        .await?
    {
        None | Some(0) => return Ok(()),
        Some(1) => {}
        _ => return Err(blocked()),
    }
    let NativeConnectionResponse::Record { record: revoked } = boundary
        .call(NativeConnectionRequest::Revoke {
            candidate_id: record.candidate.candidate_id.clone(),
            expected_revision: record.revision,
            mutation_id: mutation_id(),
        })
        .await?
    else {
        return Err(blocked());
    };
    if revoked.validate().is_err()
        || revoked.candidate != record.candidate
        || revoked.approved
        || revoked.prepared.is_some()
        || revoked.revision != record.revision.checked_add(1).ok_or_else(blocked)?
    {
        return Err(blocked());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::native_connections::{
        NativeConnectionError, NativePreparedConnection,
    };
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeBoundary {
        responses: Mutex<VecDeque<NativeConnectionResponse>>,
        requests: Mutex<Vec<NativeConnectionRequest>>,
    }
    impl Boundary for FakeBoundary {
        fn call(
            &self,
            request: NativeConnectionRequest,
        ) -> impl std::future::Future<Output = Result<NativeConnectionResponse, CliError>> + Send
        {
            self.requests.lock().unwrap().push(request);
            std::future::ready(
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .ok_or_else(blocked),
            )
        }
    }
    struct Decisions(VecDeque<Option<usize>>);
    impl Choices for Decisions {
        fn choose(
            &mut self,
            choices: Vec<bcode_tui::QuickConnectionChoice>,
            _: String,
        ) -> impl std::future::Future<Output = Result<Option<usize>, CliError>> {
            let decision = self.0.pop_front().expect("unexpected UI prompt");
            assert!(decision.is_none_or(|index| index < choices.len()));
            std::future::ready(Ok(decision))
        }
    }
    fn candidate() -> NativeCandidate {
        NativeCandidate {
            candidate_id: "synthetic-installation".into(),
            plugin_id: "fixture.native".into(),
            executable: "/synthetic/native".into(),
            config_namespace: "/synthetic/account".into(),
            installation_id: "exact-synthetic-installation".into(),
        }
    }
    fn record(revision: u64, prepared: bool) -> NativeConnectionRecord {
        let candidate = candidate();
        NativeConnectionRecord {
            candidate: candidate.clone(),
            revision,
            approved: true,
            approved_at_unix: 1,
            prepared: prepared.then(|| NativePreparedConnection {
                instance: bcode_harness_models::HarnessInstance {
                    instance_id: "synthetic-instance".into(),
                    plugin_id: candidate.plugin_id.clone(),
                    executable: candidate.executable.clone(),
                    config_namespace: candidate.config_namespace.clone(),
                    auth: bcode_harness_models::HarnessAuthBinding {
                        provider_id: "fixture.auth".into(),
                        profile: "native".into(),
                        credential_namespace: candidate.config_namespace.clone(),
                        custody: bcode_harness_models::HarnessAuthCustody::NativeManaged,
                    },
                },
                candidate,
                protocol_profile: "fixture-protocol".into(),
                compatibility_evidence: "fixture-evidence".into(),
                isolation_profile: "fixture-isolation".into(),
            }),
        }
    }
    fn boundary(responses: Vec<NativeConnectionResponse>) -> FakeBoundary {
        FakeBoundary {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(vec![]),
        }
    }
    fn responses() -> Vec<NativeConnectionResponse> {
        vec![
            NativeConnectionResponse::Candidates {
                candidates: vec![candidate()],
            },
            NativeConnectionResponse::Records { records: vec![] },
            NativeConnectionResponse::Record {
                record: Box::new(record(1, false)),
            },
            NativeConnectionResponse::Record {
                record: Box::new(record(2, true)),
            },
        ]
    }
    #[tokio::test]
    async fn actual_decisions_pin_candidate_revisions_and_distinct_mutation_ids() {
        let client = boundary(responses());
        assert!(
            connect_with(
                &client,
                &mut Decisions([Some(0), Some(1), Some(1)].into()),
                &["fixture.native".into()]
            )
            .await
            .unwrap()
        );
        let requests = client.requests.lock().unwrap();
        let NativeConnectionRequest::Approve {
            candidate: approved,
            expected_revision: 0,
            mutation_id: approve_id,
        } = &requests[2]
        else {
            panic!()
        };
        assert_eq!(approved, &candidate());
        let NativeConnectionRequest::Prepare {
            candidate_id,
            expected_revision: 1,
            mutation_id: prepare_id,
        } = &requests[3]
        else {
            panic!()
        };
        assert_eq!(candidate_id, &candidate().candidate_id);
        assert_ne!(approve_id, "");
        assert_ne!(approve_id, prepare_id);
        assert_eq!(requests.len(), 4); // No session start, fallback, or duplicate mutation.
    }
    #[tokio::test]
    async fn cancellation_and_separate_probe_denial_do_not_prepare() {
        for decisions in [
            vec![None],
            vec![Some(0), Some(0)],
            vec![Some(0), Some(1), Some(0)],
        ] {
            let client = boundary(responses());
            assert!(
                !connect_with(
                    &client,
                    &mut Decisions(decisions.into()),
                    &["fixture.native".into()]
                )
                .await
                .unwrap()
            );
            assert!(
                !client
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|request| matches!(request, NativeConnectionRequest::Prepare { .. }))
            );
        }
    }
    #[tokio::test]
    async fn unsupported_or_stale_preparation_blocks_without_replay_or_fallback() {
        for response in [
            NativeConnectionResponse::Failed {
                error: NativeConnectionError::Conflict,
            },
            NativeConnectionResponse::Record {
                record: Box::new(record(7, true)),
            },
        ] {
            let mut replies = responses();
            replies[3] = response;
            let client = boundary(replies);
            assert!(
                connect_with(
                    &client,
                    &mut Decisions([Some(0), Some(1), Some(1)].into()),
                    &["fixture.native".into()]
                )
                .await
                .is_err()
            );
            assert_eq!(client.requests.lock().unwrap().len(), 4);
        }
        let client = boundary(vec![NativeConnectionResponse::Candidates {
            candidates: vec![],
        }]);
        assert!(
            connect_with(
                &client,
                &mut Decisions(VecDeque::new()),
                &["fixture.native".into()]
            )
            .await
            .is_err()
        );
    }
    #[tokio::test]
    async fn stale_approval_or_foreign_discovery_never_reaches_process_consent() {
        let mut replies = responses();
        replies[2] = NativeConnectionResponse::Record {
            record: Box::new(record(9, false)),
        };
        let client = boundary(replies);
        assert!(
            connect_with(
                &client,
                &mut Decisions([Some(0), Some(1)].into()),
                &["fixture.native".into()]
            )
            .await
            .is_err()
        );
        assert_eq!(client.requests.lock().unwrap().len(), 3);
        let mut foreign = candidate();
        foreign.plugin_id = "foreign.native".into();
        let client = boundary(vec![NativeConnectionResponse::Candidates {
            candidates: vec![foreign],
        }]);
        assert!(
            connect_with(
                &client,
                &mut Decisions(VecDeque::new()),
                &["fixture.native".into()]
            )
            .await
            .is_err()
        );
        assert_eq!(client.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn revoke_requires_confirmation_and_uses_displayed_revision() {
        let mut revoked = record(3, false);
        revoked.approved = false;
        let client = boundary(vec![
            NativeConnectionResponse::Records {
                records: vec![record(2, true)],
            },
            NativeConnectionResponse::Record {
                record: Box::new(revoked),
            },
        ]);
        revoke_with(&client, &mut Decisions([Some(0), Some(1)].into()))
            .await
            .unwrap();
        assert!(
            matches!(&client.requests.lock().unwrap()[1], NativeConnectionRequest::Revoke { expected_revision: 2, candidate_id, mutation_id } if candidate_id == &candidate().candidate_id && !mutation_id.is_empty())
        );
    }
}
