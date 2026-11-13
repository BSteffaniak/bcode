//! In-session execution selection, reusing the startup picker and managed admission.

#[cfg(test)]
mod tests;

use crate::{TerminalShell, TuiError};
use bcode_harness_models::{HarnessInstance, HarnessOpenRequest};
use bcode_model::execution::{ExecutionModelSelection, ExecutionRoute};
use bcode_session_models::SessionId;
use std::{collections::BTreeMap, path::PathBuf};

/// Chat state needed to return from execution selection without transferring history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionPickerRequest {
    /// Conversation to retain when selection is cancelled or fails.
    pub session_id: Option<SessionId>,
    /// Unsubmitted input retained only when returning to this conversation.
    pub draft: String,
    /// Workspace for a newly selected backend.
    pub working_directory: PathBuf,
}

fn unavailable(message: &str) -> TuiError {
    std::io::Error::other(message).into()
}

pub fn managed_start_call(
    instance: HarnessInstance,
    session_id: SessionId,
    open_id: String,
    cwd: &std::path::Path,
) -> Result<bcode_ipc::harness::ManagedSessionCall, TuiError> {
    let cwd = std::fs::canonicalize(cwd)?;
    let working_directory = cwd
        .to_str()
        .ok_or_else(|| unavailable("Workspace must be UTF-8"))?
        .into();
    Ok(bcode_ipc::harness::ManagedSessionCall {
        session_id: Some(session_id),
        parent_session_id: None,
        assistance_destination_session_id: None,
        harness: bcode_harness_models::HarnessRequest {
            schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
            request_id: open_id.clone(),
            instance,
            kind: bcode_harness_models::HarnessRequestKind::Open(HarnessOpenRequest {
                open_id,
                session_id: session_id.to_string(),
                generation: 1,
                working_directory,
                resume: None,
                parent_assistance: None,
            }),
        },
    })
}

pub fn managed_started_session(
    response: &bcode_harness_models::HarnessServiceResponse,
    session_id: SessionId,
    request_id: &str,
    instance_id: &str,
) -> Result<SessionId, TuiError> {
    response
        .validate()
        .map_err(|_| unavailable("Invalid native start response"))?;
    let matches = match &response.kind {
        bcode_harness_models::HarnessServiceResponseKind::Prepared(t) => {
            t.session_id == session_id.to_string()
                && t.open_id == request_id
                && t.instance_id == instance_id
                && t.generation == 1
        }
        bcode_harness_models::HarnessServiceResponseKind::Opened(t) => {
            t.session_id == session_id.to_string()
                && t.instance_id == instance_id
                && t.generation == 1
        }
        _ => false,
    };
    if !matches
        || response.request_id != request_id
        || response.operation != bcode_harness_models::HarnessOperation::Open
    {
        return Err(unavailable(
            "Native start unavailable or uncertain; inspect Sessions before retrying",
        ));
    }
    Ok(session_id)
}

/// Configure a native installation without starting a conversation.
/// # Errors
/// Returns setup, configuration, authentication or terminal errors.
pub async fn run_native_setup_in_shell(
    shell: &mut TerminalShell,
    no_discovery: bool,
) -> Result<crate::native_setup::NativeSetupOutcome, TuiError> {
    crate::native_setup::run(shell, no_discovery).await
}

impl TerminalShell {
    pub(crate) async fn run_execution_selection(
        &mut self,
        request: ExecutionPickerRequest,
        plugins: &[bcode_plugin::StaticBundledPlugin],
        options: crate::TuiLaunchOptions,
    ) -> Result<(), TuiError> {
        let target = match Box::pin(self.select_execution(&request)).await {
            Ok(target) => target,
            Err(error) => {
                self.show_error_without_retry(&format!(
                    "Execution selection failed: {error}. No fallback was selected."
                ))
                .await?;
                request.session_id
            }
        };
        let draft = (target == request.session_id).then_some(request.draft);
        Box::pin(self.run_chat_with_draft(target, plugins, options, draft)).await
    }

    pub(crate) async fn select_execution(
        &mut self,
        request: &ExecutionPickerRequest,
    ) -> Result<Option<SessionId>, TuiError> {
        loop {
            let (outcome, models) = Box::pin(crate::run_execution_picker_loading_in_shell(
                self,
                discover_models(),
            ))
            .await?;
            match outcome {
                crate::ExecutionPickerOutcome::Close => return Ok(request.session_id),
                crate::ExecutionPickerOutcome::Setup => {
                    run_native_setup_in_shell(self, false).await?;
                }
                crate::ExecutionPickerOutcome::Selected(selection) => {
                    let connection = match selection.account_reference.as_deref() {
                        Some(account) => connection_selection(account).await,
                        None => None,
                    };
                    match connection {
                        Some(ConnectionSelection::Approve(candidate)) => {
                            Box::pin(crate::native_setup::approve_external(self, &candidate))
                                .await?;
                            continue;
                        }
                        Some(ConnectionSelection::Explain(message)) => {
                            self.show_error_without_retry(&message).await?;
                            continue;
                        }
                        Some(ConnectionSelection::Use) | None => {}
                    }
                    let resolved = bcode_model_catalog::ModelCatalogResolver::resolve_execution_selection(&models, selection.clone())
                        .map_err(|error| unavailable(&format!("Selected model/backend/account could not be resolved ({error:?}); open Setup")))?;
                    let native_model_id = resolved.target.native_model_id;
                    let instances = configured_instances().await?;
                    return self
                        .apply_execution(request, selection, native_model_id, &instances)
                        .await;
                }
            }
            if self.stop_requested() {
                return Ok(request.session_id);
            }
        }
    }

    async fn apply_execution(
        &mut self,
        request: &ExecutionPickerRequest,
        selection: ExecutionModelSelection,
        native_model_id: String,
        instances: &BTreeMap<String, HarnessInstance>,
    ) -> Result<Option<SessionId>, TuiError> {
        match &selection.route {
            Some(ExecutionRoute::ModelProvider { provider_plugin_id }) => {
                let client = bcode_client::BcodeClient::default_endpoint();
                let managed = if let Some(id) = request.session_id {
                    client.managed_session_binding(id).await?.is_some()
                } else {
                    false
                };
                let session_id = match (managed, request.session_id) {
                    (false, Some(id)) => id,
                    _ => {
                        client
                            .create_session_in_working_directory(
                                None,
                                request.working_directory.clone(),
                            )
                            .await?
                            .id
                    }
                };
                let connection = selection
                    .account_reference
                    .as_deref()
                    .and_then(|account| account.strip_prefix(CONNECTION_ACCOUNT_PREFIX))
                    .map(ToOwned::to_owned);
                client
                    .set_session_model_with_connection(
                        session_id,
                        Some(provider_plugin_id.clone()),
                        selection.model_id,
                        connection,
                    )
                    .await?;
                Ok(Some(session_id))
            }
            Some(ExecutionRoute::ManagedHarness { instance_id, .. }) => {
                let instance = instances
                    .get(instance_id)
                    .ok_or_else(|| unavailable("Selected instance is no longer configured"))?
                    .clone();
                let client = bcode_client::ManagedSessionClient::default_endpoint();
                if let Some(id) = request.session_id {
                    // Reuse canonical attachment and configuration for same-instance model changes.
                    if let Some(binding) = client.session_binding(id).await?
                        && binding.instance == instance
                    {
                        let (connection, _) = client
                            .attach_session_projection_window(
                                id,
                                crate::history_flow::initial_transcript_window_request(
                                    self.terminal()?.area(),
                                ),
                            )
                            .await?;
                        let commands = connection.command_handle();
                        let result = commands
                            .managed_session_configure(
                                binding.target.generation(),
                                uuid::Uuid::new_v4().to_string(),
                                bcode_harness_models::HarnessSessionSetting::Model {
                                    model_id: native_model_id,
                                },
                            )
                            .await;
                        commands.detach_commands().await;
                        let response = result?;
                        if !matches!(
                            response.kind,
                            bcode_harness_models::HarnessServiceResponseKind::Acknowledged { .. }
                        ) {
                            return Err(unavailable(
                                "Native model change was not acknowledged; no fallback",
                            ));
                        }
                        return Ok(Some(id));
                    }
                }
                start_managed(request, selection, instance, &client).await
            }
            None => Err(unavailable("Exact execution route required")),
        }
    }
}

async fn start_managed(
    request: &ExecutionPickerRequest,
    selection: ExecutionModelSelection,
    instance: HarnessInstance,
    client: &bcode_client::ManagedSessionClient,
) -> Result<Option<SessionId>, TuiError> {
    let id = SessionId::new();
    let call = managed_start_call(
        instance.clone(),
        id,
        uuid::Uuid::new_v4().to_string(),
        &request.working_directory,
    )?;
    let bcode_harness_models::HarnessRequestKind::Open(open) = call.harness.kind else {
        unreachable!()
    };
    let response = client
        .start_selected_session(bcode_ipc::harness::ManagedSelectedStartRequest {
            selection,
            instance,
            open,
            parent_session_id: None,
        })
        .await?;
    let bcode_harness_models::HarnessModelsResponseKind::Selected { target, .. } =
        response.response.kind
    else {
        return Err(unavailable(
            "Native creation uncertain; inspect Sessions before retrying",
        ));
    };
    target
        .validate_owner(&id.to_string(), 1)
        .map_err(|_| unavailable("Foreign selected session"))?;
    Ok(Some(id))
}

async fn configured_instances() -> Result<BTreeMap<String, HarnessInstance>, TuiError> {
    let resolution = bcode_client::ManagedSessionClient::default_endpoint()
        .native_registration_resolution()
        .await?;
    let value = resolution
        .configurations
        .get("bcode.harness")
        .ok_or_else(|| unavailable("Native registration catalog unavailable"))?;
    let mut instances = BTreeMap::new();
    if value.get("enabled") == Some(&serde_json::json!(false)) {
        return Ok(instances);
    }
    if let Some(entries) = value
        .get("instances")
        .and_then(serde_json::Value::as_object)
    {
        for (id, entry) in entries {
            if entry.get("enabled") == Some(&serde_json::json!(false)) {
                continue;
            }
            let instance = serde_json::from_value(entry["instance"].clone())?;
            instances.insert(id.clone(), instance);
        }
    }
    Ok(instances)
}

async fn discover_models() -> Result<(Vec<bcode_model::execution::ExecutionModel>, String), TuiError>
{
    let config = bcode_config::load_config()?;
    let local_instances = configured_instances().await;
    let registrations_incomplete = local_instances.is_err();
    let mut instances = local_instances.unwrap_or_default();
    let discovery_enabled = config
        .onboarding
        .credential_discovery_enabled(false, &bcode_config::ProcessConfigEnvironment);
    let client = bcode_client::ManagedSessionClient::default_endpoint();
    let discovered = client
        .discover_candidates(bcode_ipc::harness::ManagedCandidatesRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
        })
        .await;
    let mut setup_labels = std::collections::BTreeSet::new();
    if let Ok(found) = discovered {
        for plugin in found.plugins {
            if let bcode_harness_models::HarnessCandidatesResponseKind::Candidates {
                candidates,
                ..
            } = plugin.kind
            {
                for candidate in candidates {
                    if instances.values().any(|instance| {
                        instance.plugin_id == candidate.plugin_id
                            && instance.executable == candidate.executable.invocation()
                            && instance.config_namespace == candidate.config_namespace
                    }) {
                        continue;
                    }
                    // An installation that is already signed in is offered directly: its
                    // models appear as rows, and choosing one is the decision to run it.
                    if discovery_enabled
                        && candidate.readiness
                            == bcode_harness_models::HarnessCandidateReadiness::ExistingNativeLogin
                        && let Ok(instance) =
                            Box::pin(crate::native_setup::auto_register(candidate.clone())).await
                    {
                        instances.insert(instance.instance_id.clone(), instance);
                    } else {
                        setup_labels.insert(format!(
                            "Set up {}",
                            bcode_session_view::external_projection::harness_display_label(
                                &candidate.plugin_id
                            )
                        ));
                    }
                }
            }
        }
    }
    let selected_provider = config.resolved_model_selection().provider_plugin_id;
    // Query each enabled provider through the same application catalog, once per provider.
    // Native discovery is independent of provider failure and runs only on the first page.
    let provider_ids = client.model_provider_ids().await;
    let provider_incomplete = provider_ids.is_err();
    let mut providers: std::collections::BTreeSet<_> =
        provider_ids.unwrap_or_default().into_iter().collect();
    providers.extend(selected_provider);
    let mut models = Vec::new();
    let mut incomplete = provider_incomplete || registrations_incomplete;
    let mut instances = instances.into_values().collect();
    for provider in std::iter::once(None).chain(providers.into_iter().map(Some)) {
        let inventory = client
            .discover_execution_catalog(bcode_ipc::harness::ManagedExecutionCatalogRequest {
                request_id: uuid::Uuid::new_v4().to_string(),
                instances: std::mem::take(&mut instances),
                provider_plugin_id: provider,
            })
            .await?;
        incomplete |= !inventory.issues.is_empty();
        models.extend(inventory.models);
    }
    models.extend(connection_models(&models, discovery_enabled).await);
    let status = if incomplete {
        "Some catalogs unavailable; open Setup/sign in"
    } else {
        "Choose model/backend"
    };
    let setup = setup_labels.into_iter().collect::<Vec<_>>().join(" · ");
    Ok((
        models,
        format!(
            "{status} · {setup} · Changing backend opens a new conversation; history stays here"
        ),
    ))
}

/// Account reference of a row that uses an approved external credential connection.
const CONNECTION_ACCOUNT_PREFIX: &str = "external:";
/// Account reference of a row offering to connect discovered credentials.
const CONNECT_ACCOUNT_PREFIX: &str = "connect:";

/// Rows for other agents' credentials: approved connections under their provider, and
/// discovered credentials that can be connected. Discovery failures add no rows.
async fn connection_models(
    models: &[bcode_model::execution::ExecutionModel],
    discovery_enabled: bool,
) -> Vec<bcode_model::execution::ExecutionModel> {
    use bcode_model::execution::{
        ExecutionBillingRoute, ExecutionModel, ExecutionModelProvenance, ExecutionModelReadiness,
        ExecutionRoute,
    };
    use bcode_provider_auth_models::external::ExternalAuthApplication as _;
    let client = bcode_client::BcodeClient::default_endpoint();
    let connections = client.external_auth_status().await.unwrap_or_default();
    let model_info = |plugin_id: &str, model_id: &str, label: String| {
        let mut info = models
            .iter()
            .find(|entry| {
                entry.model.model_id == model_id
                    && entry.route
                        == ExecutionRoute::ModelProvider {
                            provider_plugin_id: plugin_id.to_owned(),
                        }
            })
            .map(|entry| entry.model.clone())
            .or_else(|| crate::model_picker::recommended_model(model_id))?;
        info.display_name = label;
        Some(info)
    };
    let row = |model: bcode_model::ModelInfo,
               plugin_id: &str,
               account: String,
               readiness: ExecutionModelReadiness| ExecutionModel {
        native_model_id: model.model_id.clone(),
        model,
        route: ExecutionRoute::ModelProvider {
            provider_plugin_id: plugin_id.to_owned(),
        },
        account_reference: Some(account),
        billing_route: ExecutionBillingRoute::Unknown,
        confirmed_model_id: None,
        provenance: ExecutionModelProvenance::ProviderCatalog,
        readiness,
    };
    let mut rows = Vec::new();
    for connection in &connections {
        let target = &connection.grant.target;
        let application = &connection.grant.application;
        let (readiness, note) = connection_row_state(connection);
        for model_id in &connection.recommended_models {
            if let Some(model) = model_info(
                &target.plugin_id,
                model_id,
                format!("{model_id} · via {application}{note}"),
            ) {
                rows.push(row(
                    model,
                    &target.plugin_id,
                    format!("{CONNECTION_ACCOUNT_PREFIX}{}", connection.grant.binding_id),
                    readiness,
                ));
            }
        }
    }
    if discovery_enabled {
        for candidate in client.discover_external_auth().await.unwrap_or_default() {
            if !candidate.available
                || connections
                    .iter()
                    .any(|connection| connection.grant.binding_id == candidate.binding_id)
            {
                continue;
            }
            let label = format!("Connect {}", candidate.application);
            if let Some(model) = crate::model_picker::recommended_model(&label).map(|mut model| {
                model.display_name = format!("{label} — use these credentials in Bcode");
                model
            }) {
                rows.push(row(
                    model,
                    &candidate.target.plugin_id,
                    format!("{CONNECT_ACCOUNT_PREFIX}{}", candidate.binding_id),
                    ExecutionModelReadiness::Unavailable,
                ));
            }
        }
    }
    rows
}

/// Readiness and label suffix for an approved connection's rows.
fn connection_row_state(
    connection: &bcode_provider_auth_models::external::ExternalAuthConnection,
) -> (bcode_model::execution::ExecutionModelReadiness, String) {
    use bcode_model::execution::ExecutionModelReadiness;
    use bcode_provider_auth_models::external::ExternalConnectionStatus;
    let application = &connection.grant.application;
    match connection.status {
        ExternalConnectionStatus::Ready { .. } => (ExecutionModelReadiness::Unknown, String::new()),
        ExternalConnectionStatus::Expired => (
            ExecutionModelReadiness::Unavailable,
            format!(" — expired, open {application} to renew"),
        ),
        ExternalConnectionStatus::AccountChanged => (
            ExecutionModelReadiness::Unavailable,
            " — different account, select to approve".to_owned(),
        ),
        ExternalConnectionStatus::SourceUnavailable => (
            ExecutionModelReadiness::Unavailable,
            format!(" — not found, sign in to {application}"),
        ),
        ExternalConnectionStatus::DeclarationChanged => (
            ExecutionModelReadiness::Unavailable,
            " — no longer offered".to_owned(),
        ),
    }
}

/// The connection behind a selected row, when it is a connection or connect row.
enum ConnectionSelection {
    /// Use the approved connection; its binding is the row's account reference.
    Use,
    /// Ask once, then approve this candidate and rediscover.
    Approve(bcode_provider_auth_models::external::ExternalAuthCandidate),
    /// Not usable now; the message names the fix.
    Explain(String),
}

async fn connection_selection(account: &str) -> Option<ConnectionSelection> {
    use bcode_provider_auth_models::external::{
        ExternalAuthApplication as _, ExternalAuthCandidate, ExternalConnectionStatus,
    };
    let client = bcode_client::BcodeClient::default_endpoint();
    if let Some(binding) = account.strip_prefix(CONNECT_ACCOUNT_PREFIX) {
        let candidate = client
            .discover_external_auth()
            .await
            .ok()?
            .into_iter()
            .find(|candidate| candidate.binding_id == binding)?;
        return Some(ConnectionSelection::Approve(candidate));
    }
    let binding = account.strip_prefix(CONNECTION_ACCOUNT_PREFIX)?;
    let connection = client
        .external_auth_status()
        .await
        .ok()?
        .into_iter()
        .find(|connection| connection.grant.binding_id == binding)?;
    let application = &connection.grant.application;
    Some(match connection.status {
        ExternalConnectionStatus::Ready { .. } => ConnectionSelection::Use,
        ExternalConnectionStatus::AccountChanged => {
            ConnectionSelection::Approve(ExternalAuthCandidate {
                binding_id: connection.grant.binding_id.clone(),
                target: connection.grant.target.clone(),
                application: application.clone(),
                available: true,
            })
        }
        ExternalConnectionStatus::Expired => ConnectionSelection::Explain(format!(
            "{application} sign-in expired. Open {application} to renew it, then choose again."
        )),
        ExternalConnectionStatus::SourceUnavailable => ConnectionSelection::Explain(format!(
            "{application} credentials were not found. Sign in to {application}, then choose again."
        )),
        ExternalConnectionStatus::DeclarationChanged => ConnectionSelection::Explain(
            "This connection is no longer offered. Run `bcode auth disconnect` to remove it."
                .into(),
        ),
    })
}

#[cfg(test)]
mod connection_row_tests {
    use super::*;
    use bcode_model::execution::ExecutionModelReadiness;
    use bcode_provider_auth_models::external::{
        ExternalAuthConnection, ExternalAuthGrant, ExternalAuthTarget, ExternalConnectionStatus,
    };

    fn connection(status: ExternalConnectionStatus) -> ExternalAuthConnection {
        ExternalAuthConnection {
            grant: ExternalAuthGrant {
                binding_id: "binding".into(),
                target: ExternalAuthTarget {
                    plugin_id: "plugin".into(),
                    provider_id: "provider".into(),
                    method_id: "chatgpt".into(),
                    source_id: "source".into(),
                },
                application: "Codex".into(),
                account_fingerprint: None,
                approved_at_unix: 1,
            },
            status,
            recommended_models: vec!["model".into()],
        }
    }

    #[test]
    fn ready_connections_are_selectable_and_others_name_the_fix() {
        assert_eq!(
            connection_row_state(&connection(ExternalConnectionStatus::Ready {
                expires_at_unix: None
            })),
            (ExecutionModelReadiness::Unknown, String::new())
        );
        let (readiness, note) =
            connection_row_state(&connection(ExternalConnectionStatus::Expired));
        assert_eq!(readiness, ExecutionModelReadiness::Unavailable);
        assert!(note.contains("open Codex to renew"));
    }
}
