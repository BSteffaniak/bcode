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
) -> Result<bool, TuiError> {
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
                client
                    .set_session_model(
                        session_id,
                        Some(provider_plugin_id.clone()),
                        selection.model_id,
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
    let instances = local_instances.unwrap_or_default();
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
                    if !instances.values().any(|instance| {
                        instance.plugin_id == candidate.plugin_id
                            && instance.executable == candidate.executable.invocation()
                            && instance.config_namespace == candidate.config_namespace
                    }) {
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
