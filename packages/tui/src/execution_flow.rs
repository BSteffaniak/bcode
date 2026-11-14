//! In-session execution selection, reusing the startup picker and managed admission.

#[cfg(test)]
mod tests;

use crate::model_switcher::inventory::Access;
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
    /// The conversation's current model, marked in the switcher.
    pub current: crate::model_switcher::inventory::CurrentChoice,
    /// The conversation's current reasoning effort.
    pub current_effort: Option<String>,
    /// What the in-conversation switcher already decided, if anything.
    pub pending: Option<PendingSwitch>,
}

/// A decision made in the conversation's switcher that needs the terminal to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingSwitch {
    /// Apply this choice; it needs a new conversation or a native tool.
    Apply(Box<crate::model_switcher::Choice>),
    /// Open native tool setup, then choose again.
    Setup,
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

async fn select_provider_choice(
    client: &bcode_client::BcodeClient,
    session_id: SessionId,
    provider: &str,
    model: &str,
    access: &Access,
) -> Result<(), bcode_client::ClientError> {
    if let Access::Account { account_reference } = access {
        client
            .set_session_model_with_account(
                session_id,
                Some(provider.to_owned()),
                model.to_owned(),
                account_reference.clone(),
            )
            .await
    } else {
        let connection = match access {
            Access::Connection { binding_id, .. } => Some(binding_id.clone()),
            _ => None,
        };
        client
            .set_session_model_with_connection(
                session_id,
                Some(provider.to_owned()),
                model.to_owned(),
                connection,
            )
            .await
    }
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
        let mut pending = request.pending.clone();
        loop {
            let exit = match pending.take() {
                Some(PendingSwitch::Apply(choice)) => crate::SwitcherExit::Chose(choice),
                Some(PendingSwitch::Setup) => crate::SwitcherExit::Setup,
                None => {
                    Box::pin(crate::run_model_switcher_in_shell(
                        self,
                        request.current.clone(),
                        request.current_effort.clone(),
                    ))
                    .await?
                }
            };
            match exit {
                crate::SwitcherExit::Close => return Ok(request.session_id),
                crate::SwitcherExit::Setup => {
                    run_native_setup_in_shell(self, false).await?;
                }
                crate::SwitcherExit::Chose(choice) => {
                    let instances = configured_instances().await?;
                    return self.apply_execution(request, &choice, &instances).await;
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
        choice: &crate::model_switcher::Choice,
        instances: &BTreeMap<String, HarnessInstance>,
    ) -> Result<Option<SessionId>, TuiError> {
        let model = &choice.entry.model;
        let native_model_id = model.native_model_id.clone();
        let selection = ExecutionModelSelection {
            model_id: model.model.model_id.clone(),
            route: Some(model.route.clone()),
            account_reference: model.account_reference.clone(),
        };
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
                select_provider_choice(
                    &client,
                    session_id,
                    provider_plugin_id,
                    &selection.model_id,
                    &choice.entry.access,
                )
                .await?;
                if choice.effort.is_some() {
                    client
                        .set_session_reasoning(session_id, choice.effort.clone(), None)
                        .await?;
                }
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

pub async fn configured_instances() -> Result<BTreeMap<String, HarnessInstance>, TuiError> {
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
