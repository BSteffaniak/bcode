//! Background effect runner for TUI work that may touch daemon/client services.

use std::collections::{BTreeMap, VecDeque};

use bcode_client::{BcodeClient, ClientError, DaemonAvailability, MessageAcceptance};
use bcode_ipc::{ComposerDraftScope, PermissionSummary, PromptPlacement};
use bcode_session_models::{ProjectionWindowRequest, SessionId, SessionSummary, WorkId};
use bcode_session_view::execute_session_view_action;
use bcode_session_view_models::{SessionViewAction, SessionViewActionOutcome};
use bcode_skill_models::SkillId;
use bcode_worktree_models::WorktreeCreateRequest;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::{
    TuiError, clipboard_image, history_flow,
    session_flow::{self, AgentCatalog},
    slash_palette,
};

/// One non-resumable observation lifetime owned by a BMUX command. A pull stays
/// alive across UI updates; cancellation discards its dedicated client connection.
static ACTIVITY_WATCH_LIFETIMES: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(16)));

struct ActivityWatch {
    lifetime: Option<tokio::sync::OwnedSemaphorePermit>,
    client: BcodeClient,
    handle: bcode_session_models::SessionActivityWatchHandle,
}

impl ActivityWatch {
    async fn open(
        client: BcodeClient,
        scope: bcode_session_models::SessionActivityWatchScope,
    ) -> Result<Self, String> {
        Self::open_with_lifetimes(client, scope, ACTIVITY_WATCH_LIFETIMES.clone()).await
    }

    async fn open_with_lifetimes(
        client: BcodeClient,
        scope: bcode_session_models::SessionActivityWatchScope,
        lifetimes: std::sync::Arc<tokio::sync::Semaphore>,
    ) -> Result<Self, String> {
        use bcode_session_models::application::SessionApplication;
        let lifetime = lifetimes
            .acquire_owned()
            .await
            .map_err(|error| error.to_string())?;
        let handle = client
            .open_activity_watch(scope)
            .await
            .map_err(|error| error.message)?;
        Ok(Self {
            client,
            handle,
            lifetime: Some(lifetime),
        })
    }

    async fn next_delivery(
        &self,
    ) -> Result<bcode_session_models::SessionActivityWatchDelivery, String> {
        use bcode_session_models::application::SessionApplication;
        self.client
            .pull_activity_watch(self.handle)
            .await
            .map_err(|error| error.message)
    }
}

impl Drop for ActivityWatch {
    fn drop(&mut self) {
        use bcode_session_models::application::SessionApplication;
        let client = self.client.clone();
        let handle = self.handle;
        let lifetime = self.lifetime.take();
        // The client invalidates an uncertain pull on drop. Explicit close also
        // releases a settled/idle handle; cleanup never waits on the UI thread.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            drop(runtime.spawn(async move {
                let _closed = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    client.close_activity_watch(handle),
                )
                .await;
                drop(lifetime);
            }));
        }
    }
}

/// Split combined frontend demand without changing query windows or starving later
/// windows. Every returned watch obeys the portable host's actual candidate budget.
#[must_use]
pub fn activity_watch_batches(
    scope: &bcode_session_models::SessionActivityWatchScope,
) -> Vec<bcode_session_models::SessionActivityWatchScope> {
    use bcode_session_models::SessionActivityWatchScope;
    let mut batches = Vec::new();
    let mut batch = SessionActivityWatchScope {
        activity: Vec::new(),
        children: Vec::new(),
    };
    let mut budget = 0;
    for target in &scope.activity {
        let cost = target.query.limit + 1;
        if budget + cost > 512 || batch.activity.len() == 16 {
            batches.push(batch);
            batch = SessionActivityWatchScope {
                activity: Vec::new(),
                children: Vec::new(),
            };
            budget = 0;
        }
        batch.activity.push(target.clone());
        budget += cost;
    }
    for query in &scope.children {
        if budget + query.limit > 512 || batch.children.len() == 8 {
            batches.push(batch);
            batch = SessionActivityWatchScope {
                activity: Vec::new(),
                children: Vec::new(),
            };
            budget = 0;
        }
        batch.children.push(query.clone());
        budget += query.limit;
    }
    if !batch.activity.is_empty() || !batch.children.is_empty() {
        batches.push(batch);
    }
    batches
}

/// Own all admitted streams and their bounded aggregation in one cancellable command.
pub async fn run_activity_watches(
    client: BcodeClient,
    scope: bcode_session_models::SessionActivityWatchScope,
    generation: u64,
    handle: bmux_tui_runtime::RuntimeHandle<super::root_program::BcodeRuntimeMessage>,
) -> Option<super::root_program::BcodeRuntimeMessage> {
    use super::root_program::BcodeRuntimeMessage;
    use bcode_session_models::{
        SessionActivityWatchSnapshot, application::SessionApplicationFailure,
    };
    let batches = activity_watch_batches(&scope);
    if batches.is_empty() {
        return None;
    }
    let (sender, mut receiver) = mpsc::channel(batches.len());
    // JoinSet aborts every opening/pulling worker when BMUX drops this command.
    let mut workers = tokio::task::JoinSet::new();
    for (index, batch) in batches.iter().cloned().enumerate() {
        let client = client.clone();
        let sender = sender.clone();
        workers.spawn(async move {
            loop {
                let result = async {
                    let watch = ActivityWatch::open(client.clone(), batch.clone()).await?;
                    let result = loop {
                        let delivery = match watch.next_delivery().await {
                            Ok(delivery) => delivery,
                            Err(error) => break Err(error),
                        };
                        if sender.send((index, Ok(delivery.snapshot))).await.is_err() {
                            break Ok::<(), String>(());
                        }
                    };
                    drop(watch);
                    result
                }
                .await;
                match result {
                    Ok(()) => return,
                    Err(error) => {
                        if sender.send((index, Err(error))).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }
            }
        });
    }
    drop(sender);
    let unknown = || SessionApplicationFailure {
        code: "activity_watch_pending".into(),
        message: "Activity watch pending or disconnected".into(),
    };
    let mut latest: Vec<_> = batches
        .iter()
        .map(|batch| SessionActivityWatchSnapshot {
            activity: batch.activity.iter().map(|_| Err(unknown())).collect(),
            children: batch.children.iter().map(|_| Err(unknown())).collect(),
        })
        .collect();
    while let Some((index, result)) = receiver.recv().await {
        latest[index] = result.unwrap_or_else(|error| SessionActivityWatchSnapshot {
            activity: batches[index]
                .activity
                .iter()
                .map(|_| {
                    Err(SessionApplicationFailure {
                        code: "activity_watch_disconnected".into(),
                        message: error.clone(),
                    })
                })
                .collect(),
            children: batches[index]
                .children
                .iter()
                .map(|_| {
                    Err(SessionApplicationFailure {
                        code: "activity_watch_disconnected".into(),
                        message: error.clone(),
                    })
                })
                .collect(),
        });
        let mut snapshot = SessionActivityWatchSnapshot {
            activity: Vec::new(),
            children: Vec::new(),
        };
        for value in &latest {
            snapshot.activity.extend(value.activity.clone());
            snapshot.children.extend(value.children.clone());
        }
        if handle
            .send(BcodeRuntimeMessage::ChildWatchProjection {
                generation,
                result: Ok(snapshot),
            })
            .await
            .is_err()
        {
            break;
        }
    }
    None
}

/// Store interactive structural rules in user state, then read back every
/// stored rule so the caller recompiles from what was written.
fn persist_presentation_rules(
    rules: &[bcode_presentation_policy::models::PresentationRule],
) -> Result<
    (
        std::path::PathBuf,
        Vec<bcode_presentation_policy::models::PresentationRule>,
    ),
    String,
> {
    // One serialized read-modify-write: the config crate locks user state
    // across the update, so concurrent theme or profile saves are kept.
    let path = bcode_config::add_tui_presentation_rules(rules.iter().cloned())
        .map_err(|error| error.to_string())?;
    let stored = bcode_config::load_tui_presentation_rules().map_err(|error| error.to_string())?;
    Ok((path, stored))
}

#[cfg(test)]
mod activity_watch_lifetime_tests {
    use super::*;

    #[tokio::test]
    async fn close_tasks_retain_admission_and_cancelled_open_releases_its_waiter() {
        let lifetimes = std::sync::Arc::new(tokio::sync::Semaphore::new(2));
        let first = lifetimes.clone().acquire_owned().await.unwrap();
        let second = lifetimes.clone().acquire_owned().await.unwrap();
        let pending_gate = lifetimes.clone();
        let mut pending = tokio::spawn(async move {
            ActivityWatch::open_with_lifetimes(
                BcodeClient::default_endpoint(),
                bcode_session_models::SessionActivityWatchScope {
                    activity: Vec::new(),
                    children: Vec::new(),
                },
                pending_gate,
            )
            .await
        });
        tokio::task::yield_now().await;
        assert!(
            !pending.is_finished(),
            "opening waits for bounded live+cleanup admission"
        );
        pending.abort();
        assert!(matches!((&mut pending).await, Err(error) if error.is_cancelled()));
        drop(pending);
        let watch = |permit| ActivityWatch {
            client: BcodeClient::default_endpoint(),
            handle: bcode_session_models::SessionActivityWatchHandle {
                watch_id: SessionId::new(),
                epoch: SessionId::new(),
            },
            lifetime: Some(permit),
        };
        drop(watch(first));
        drop(watch(second));
        assert_eq!(
            lifetimes.available_permits(),
            0,
            "cleanup owns permits until it settles"
        );
        let one = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            lifetimes.clone().acquire_owned(),
        )
        .await
        .unwrap()
        .unwrap();
        let two = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            lifetimes.clone().acquire_owned(),
        )
        .await
        .unwrap()
        .unwrap();
        drop((one, two));
        assert_eq!(lifetimes.available_permits(), 2);
    }
}

#[cfg(test)]
#[path = "managed_effect_error_tests.rs"]
mod managed_effect_error_tests;

async fn invoke_plugin_command(
    application: &impl bcode_command::application::CommandApplication,
    plugin_id: String,
    command_id: String,
    args: BTreeMap<String, String>,
    working_directory: std::path::PathBuf,
    session_id: Option<SessionId>,
) -> Result<bcode_command::InvokeCommandResponse, TuiError> {
    use bcode_command::application::{CommandInvocationOutcome, CommandInvocationRequest};

    let result = application
        .invoke_command(CommandInvocationRequest {
            plugin_id,
            invocation_id: uuid::Uuid::new_v4().to_string(),
            agent_id: None,
            launch_root: Some(working_directory.clone()),
            deadline_ms: 30_000,
            command: bcode_command::InvokeCommandRequest {
                command_id,
                args,
                context: Some(bcode_command::CommandInvocationContext {
                    session_id,
                    working_directory,
                }),
            },
        })
        .await;
    match result.outcome {
        CommandInvocationOutcome::Completed(response) => Ok(response),
        CommandInvocationOutcome::Failed(error) => Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        }),
        CommandInvocationOutcome::Unknown(error) => Err(TuiError::PluginService {
            code: error.code,
            message: format!(
                "Command outcome unknown: {}. Effects may have occurred; retry is not known safe.",
                error.message,
            ),
        }),
    }
}

/// The agent's instructions for a native agent, from the agent's own provider. Missing
/// instructions announce the agent by name only.
async fn native_agent_instructions(
    commands: &bcode_client::ManagedSessionCommandHandle,
    session_id: SessionId,
    agent_id: String,
) -> Option<String> {
    let request = bcode_agent_profile::AgentContextRequest {
        session_id,
        agent_id,
        available_tools: Vec::new(),
        effective_config_toml: bcode_config::load_config()
            .ok()
            .and_then(|config| bcode_config::encode_effective_config(&config).ok())
            .map(Box::new),
        claimed_tools: Vec::new(),
        claimed_mutating_tools: Vec::new(),
    };
    let response = commands
        .host_client()
        .call_plugin_service(
            bcode_agent_profile::AGENT_PROFILE_INTERFACE_ID.to_owned(),
            bcode_agent_profile::OP_AGENT_CONTEXT.to_owned(),
            serde_json::to_vec(&request).ok()?,
        )
        .await
        .ok()?;
    if response.error.is_some() {
        return None;
    }
    serde_json::from_slice::<bcode_agent_profile::AgentContextResponse>(&response.payload)
        .ok()?
        .system_prompt_suffix
}

/// Apply a staged Bcode agent before a native message is sent: switch the native permission
/// mode when it differs, then save the agent with the session. A harness that cannot switch
/// modes mid-session still gets the agent; any other failure sends nothing.
async fn apply_native_agent_change(
    commands: &bcode_client::ManagedSessionCommandHandle,
    host: Option<&ManagedHostAccess>,
    generation: Option<u64>,
    change: &NativeAgentChange,
) -> Result<Option<bcode_harness_models::HarnessSessionSetting>, ClientError> {
    use bcode_harness_models::{HarnessFailure, HarnessServiceResponseKind, HarnessSessionSetting};
    let host = host.ok_or_else(|| {
        ClientError::Protocol(
            "Agent selection is unavailable in this managed conversation; nothing was sent".into(),
        )
    })?;
    let mut applied = None;
    if let Some(mode) = change.mode {
        let generation = generation
            .ok_or_else(|| ClientError::Protocol("Managed generation is missing".into()))?;
        let setting = HarnessSessionSetting::PermissionMode { mode };
        let label = super::managed_controls::permission_mode_label(mode);
        match commands
            .managed_session_configure(
                generation,
                uuid::Uuid::new_v4().to_string(),
                setting.clone(),
            )
            .await
        {
            Ok(response) => match response.kind {
                HarnessServiceResponseKind::Acknowledged { .. } => applied = Some(setting),
                HarnessServiceResponseKind::Failure(HarnessFailure::Unsupported) => {}
                other => {
                    return Err(ClientError::Protocol(format!(
                        "Switching to {label} was not confirmed ({other:?}); nothing was sent"
                    )));
                }
            },
            Err(ClientError::Server { code, .. }) if code == "managed_session_unsupported" => {}
            Err(error) => return Err(error),
        }
    }
    host.client()
        .set_session_agent(commands.session_id(), change.agent_id.clone())
        .await?;
    Ok(applied)
}

async fn managed_submit_message(
    commands: &bcode_client::ManagedSessionCommandHandle,
    host: Option<&ManagedHostAccess>,
    request: SubmitMessageRequest,
    generation: Option<u64>,
) -> TuiEffectResult {
    let session_id = commands.session_id();
    let submission_id = request.submission_id.clone();
    let message = request.message.clone();
    let result = async {
        if request.session_id != Some(session_id) {
            return Err(ClientError::Protocol("Managed composer session mismatch".into()));
        }
        validate_managed_composer_request(&request)?;
        let mut request = request;
        let mut auth = commands.auth_flow().await?;
        let readiness = auth.readiness().await?;
        if !matches!(readiness.readiness, bcode_provider_auth_models::managed::AuthReadiness::Ready | bcode_provider_auth_models::managed::AuthReadiness::Verified) {
            return Err(ClientError::Protocol("Native authentication is not ready; use the dedicated managed auth surface before sending".into()));
        }
        let mut result = SubmitMessageResult::empty(session_id);
        if let Some(retained) = request.managed.take() {
            if Some(retained.target().1) != generation {
                return Err(ClientError::Protocol("Retained managed input belongs to another generation".into()));
            }
            result.receipt = Some(ComposerReceipt::Managed(Box::new(commands.submit_managed_composer_request(retained).await?)));
        } else {
            if let Some(change) = request.native_agent_change.take() {
                result.applied_native_setting =
                    apply_native_agent_change(commands, host, generation, &change).await?;
                result.committed_agent_id = Some(change.agent_id);
            }
            let text = match request.native_agent.take() {
                Some((agent_id, name)) => {
                    let instructions = native_agent_instructions(commands, session_id, agent_id).await;
                    bcode_session_view::external_projection::with_agent_block(
                        &name,
                        instructions.as_deref(),
                        &request.message,
                    )
                }
                None => request.message.clone(),
            };
            let retained = commands.prepare_managed_composer_submission(
                (request.submission_id.0.clone(), request.attempt_id.clone()),
                text, request.placement == PromptPlacement::Steering,
                request.expected_native_turn_id.clone(),
            ).await?;
            if Some(retained.target().1) != generation {
                return Err(ClientError::Protocol("Managed generation changed during preparation; no input sent".into()));
            }
            request.managed = Some(retained);
            request.managed_only = true;
            result.prepared_request = Some(Box::new(request));
        }
        Ok(result)
    }.await;
    TuiEffectResult::SubmitMessage {
        submission_id,
        message,
        result: Box::new(result),
    }
}

/// Lazily constructed host-only application access for a managed conversation.
///
/// It serves only allow-listed host operations (catalog, search, picker, rename/delete,
/// monitor, timeline, X-ray, palettes). Submission, interruption, native responses and
/// configuration stay on the sealed managed command handle, so this access can never become
/// a model or native execution fallback. It is constructed on first use only.
#[derive(Clone, Default)]
pub struct ManagedHostAccess {
    /// Managed command handle whose credential-deferred configuration the host client reuses.
    commands: Option<bcode_client::ManagedSessionCommandHandle>,
    client: std::sync::Arc<std::sync::OnceLock<BcodeClient>>,
}

impl ManagedHostAccess {
    /// Host access sharing the managed conversation's credential-deferred configuration.
    #[must_use]
    pub fn for_commands(commands: bcode_client::ManagedSessionCommandHandle) -> Self {
        Self {
            commands: Some(commands),
            client: std::sync::Arc::default(),
        }
    }

    /// Host client, constructed on first use against the running daemon.
    pub(super) fn client(&self) -> BcodeClient {
        self.client
            .get_or_init(|| {
                self.commands.as_ref().map_or_else(
                    || {
                        BcodeClient::default_endpoint()
                            .with_daemon_availability(DaemonAvailability::RequireRunning)
                    },
                    bcode_client::ManagedSessionCommandHandle::host_client,
                )
            })
            .clone()
    }
}

/// Managed execution access threaded into effect commands.
#[derive(Clone, Default)]
pub struct ManagedEffectAccess {
    /// Sealed managed command handle; present only in a managed conversation.
    pub commands: Option<bcode_client::ManagedSessionCommandHandle>,
    /// Ownership generation every managed mutation must target.
    pub generation: Option<u64>,
    /// Host-only access for allow-listed host operations.
    pub host: Option<ManagedHostAccess>,
}

/// Composer context retained for managed host slash commands.
pub struct ManagedSlashContext {
    working_directory: std::path::PathBuf,
    current_agent_id: String,
    reasoning_display_mode: bcode_config::TuiThinkingMode,
    reasoning_visible: bool,
}

#[allow(clippy::too_many_lines)]
async fn managed_slash_command(
    commands: &bcode_client::ManagedSessionCommandHandle,
    message: String,
    generation: Option<u64>,
    host: Option<&ManagedHostAccess>,
    context: ManagedSlashContext,
) -> TuiEffectResult {
    use super::managed_controls::{ManagedCommandSupport, command_support};
    use super::slash_commands::SlashCommandOutcome;
    let result = async {
        let args: Vec<_> = message.split_whitespace().collect();
        match args.as_slice() {
            ["/auth"] | ["/auth", "status"] => {
                let mut auth = commands.auth_flow().await?;
                let status = auth.readiness().await?;
                let message = match status.readiness {
                    bcode_provider_auth_models::managed::AuthReadiness::Ready => "Selected managed instance authentication is locally ready",
                    bcode_provider_auth_models::managed::AuthReadiness::Verified => "Selected managed instance authentication is verified",
                    bcode_provider_auth_models::managed::AuthReadiness::LoginRequired => "Selected managed instance requires native login; use the dedicated terminal handoff, not provider enrollment",
                    bcode_provider_auth_models::managed::AuthReadiness::Unknown => "Selected managed instance authentication readiness is unknown; no login or billing change was attempted",
                    bcode_provider_auth_models::managed::AuthReadiness::Blocked(_) => "Selected managed instance authentication is blocked; no account or billing fallback was selected",
                };
                return Ok(SlashCommandOutcome::Handled(message.into()));
            }
            ["/auth", ..] => return Err(ClientError::Protocol("Managed /auth is bound to the attached configured instance. Only /auth status is currently available; provider/account selectors and credential input are not accepted in chat".into())),
            ["/native", "interrupt"] | ["/stop"] => {
                let expected = generation.ok_or_else(|| ClientError::Protocol("Managed generation is missing".into()))?;
                commands.interrupt(expected, uuid::Uuid::new_v4().to_string()).await?;
                return Ok(SlashCommandOutcome::Handled("Native interrupt acknowledged; settlement remains independent".into()));
            }
            ["/native", "respond", supplied_generation, request_id, response_id, rest @ ..] => {
                let supplied = supplied_generation.parse::<u64>().map_err(|_| ClientError::Protocol("Invalid native generation".into()))?;
                if Some(supplied) != generation {
                    return Err(ClientError::Protocol("Native response generation differs from attached session".into()));
                }
                let value = super::managed_controls::parse_native_response(&rest.join(" "))
                    .map_err(ClientError::Protocol)?;
                commands.managed_session_respond(supplied, (*request_id).into(), (*response_id).into(), value).await?;
                return Ok(SlashCommandOutcome::Handled("Native response recorded".into()));
            }
            ["/native", "composer", "next"] => return Ok(SlashCommandOutcome::ManagedComposer { expected_native_turn_id: None, retry_submission_id: None }),
            ["/native", "composer", "steer", turn] => return Ok(SlashCommandOutcome::ManagedComposer { expected_native_turn_id: Some((*turn).into()), retry_submission_id: None }),
            ["/native", "retry", submission] => return Ok(SlashCommandOutcome::ManagedComposer { expected_native_turn_id: None, retry_submission_id: Some((*submission).into()) }),
            ["/native", ..] => return Err(ClientError::Protocol("Managed commands: /native composer next|steer <turn>, /native interrupt, /native respond <generation> <request> <response> approve|deny|answer <choice> [text]|assist <text>|<JSON>, /native retry <submission>".into())),
            ["/model" | "/models"] => return Ok(SlashCommandOutcome::PickModel),
            ["/model" | "/set-model", model_id] => {
                return Ok(SlashCommandOutcome::ManagedConfigure(
                    bcode_harness_models::HarnessSessionSetting::Model { model_id: (*model_id).to_owned() },
                ));
            }
            ["/agent", "accept-edits" | "accept_edits" | "acceptedits"] => {
                // Accept-edits is a native mode with no Bcode agent, so it applies directly.
                return Ok(SlashCommandOutcome::ManagedConfigure(
                    bcode_harness_models::HarnessSessionSetting::PermissionMode {
                        mode: bcode_harness_models::HarnessPermissionMode::AcceptEdits,
                    },
                ));
            }
            ["/plan" | "/build" | "/agent", ..] => {
                // Bcode agents are staged for the next message, as in any Bcode conversation.
                let host = host.ok_or_else(|| {
                    ClientError::Protocol("Agent selection is unavailable in this managed conversation".into())
                })?;
                let message = if args.as_slice() == ["/agent", "default"] {
                    "/build".to_owned()
                } else {
                    message.clone()
                };
                let Some(command) = super::slash_registry::slash_command_name(&message)
                    .and_then(super::slash_registry::builtin_command)
                else {
                    return Ok(SlashCommandOutcome::Unknown(message));
                };
                return super::slash_commands::execute_resolved(
                    &host.client(),
                    Some(commands.session_id()),
                    super::slash_commands::SlashExecutionContext {
                        working_directory: &context.working_directory,
                        current_agent_id: &context.current_agent_id,
                        reasoning_display_mode: context.reasoning_display_mode,
                        reasoning_visible: context.reasoning_visible,
                    },
                    &message,
                    super::slash_registry::SlashResolution::Builtin(command),
                )
                .await;
            }
            ["/permissions"] => return Ok(SlashCommandOutcome::ReviewPermissions),
            _ => {}
        }
        match command_support(&message) {
            Some(ManagedCommandSupport::Host) => {
                let Some(command) = super::slash_registry::slash_command_name(&message)
                    .and_then(super::slash_registry::builtin_command)
                else {
                    return Ok(SlashCommandOutcome::Unknown(message.clone()));
                };
                let host = host.ok_or_else(|| {
                    ClientError::Protocol("Host commands are unavailable in this managed conversation".into())
                })?;
                super::slash_commands::execute_resolved(
                    &host.client(),
                    Some(commands.session_id()),
                    super::slash_commands::SlashExecutionContext {
                        working_directory: &context.working_directory,
                        current_agent_id: &context.current_agent_id,
                        reasoning_display_mode: context.reasoning_display_mode,
                        reasoning_visible: context.reasoning_visible,
                    },
                    &message,
                    super::slash_registry::SlashResolution::Builtin(command),
                )
                .await
            }
            Some(ManagedCommandSupport::NativePassthrough) => Ok(SlashCommandOutcome::Handled(
                "Native slash commands are sent as managed input; submit it from the composer".into(),
            )),
            Some(ManagedCommandSupport::Control | ManagedCommandSupport::Unavailable) | None => {
                Ok(SlashCommandOutcome::Handled(format!(
                    "{} is unavailable in this managed conversation; no model or plugin fallback was used. Type / to see managed commands",
                    args.first().copied().unwrap_or("This command")
                )))
            }
        }
    }.await;
    TuiEffectResult::SlashCommandExecuted { message, result }
}

#[cfg(test)]
mod managed_model_filter_tests {
    #[test]
    fn managed_host_allow_list_excludes_execution_and_model_effects() {
        use super::TuiEffect;
        let session_id = bcode_session_models::SessionId::new();
        for host in [
            TuiEffect::LoadCommandPalette,
            TuiEffect::DeleteSession { session_id },
            TuiEffect::RenameSession {
                session_id,
                name: Some("renamed".into()),
            },
            TuiEffect::LoadSessionPicker {
                generation: 1,
                scope: bcode_session_models::SessionDiscoveryScope::AllWorkingDirectories,
                query: bcode_session_models::SessionCatalogQuery::default(),
                cursor: None,
            },
        ] {
            assert!(host.is_managed_host_operation());
        }
        for denied in [
            TuiEffect::CancelTurn { session_id },
            TuiEffect::CompactContext { session_id },
            TuiEffect::ListPermissions,
            TuiEffect::ResolvePermission {
                permission_id: "p".into(),
                approved: true,
                remember: false,
                apply_to_batch: false,
                batch_id: None,
            },
            TuiEffect::RespondManagedInteraction {
                generation: 1,
                request_id: "r".into(),
                response_id: "id".into(),
                value: bcode_harness_models::HarnessResponseValue::Approval { approved: true },
            },
        ] {
            assert!(!denied.is_managed_host_operation());
        }
        // Local presentation state needs no daemon or host access at all.
        assert!(TuiEffect::ClearThemeSelection.run_local_state().is_ok());
        assert!(TuiEffect::ListPermissions.run_local_state().is_err());
    }
}

/// Submit-message effect request payload.
#[derive(Debug)]
pub struct SubmitMessageRequest {
    /// Stable identity allocated with the optimistic composer item.
    pub submission_id: bcode_session_models::SubmissionId,
    /// Captured native intent; populated by the preparation round trip before dispatch.
    pub managed: Option<bcode_session_models::ExternalHarnessRequest>,
    /// Native attempt identity retained before preparation.
    pub attempt_id: String,
    /// Explicit native composer mode must never create model work.
    pub managed_only: bool,
    /// Explicit native turn selected by the user, never inferred from assistant text.
    pub expected_native_turn_id: Option<String>,
    /// Existing session, if already attached.
    pub session_id: Option<SessionId>,
    /// Working directory to use when creating a draft session.
    pub launch_working_directory: std::path::PathBuf,
    /// Message text to submit.
    pub message: String,
    /// Prompt placement semantics.
    pub placement: PromptPlacement,
    /// Provider to apply before sending, if any.
    pub provider_plugin_id: Option<String>,
    /// Model to apply before sending, if any.
    pub model_id: Option<String>,
    /// Agent to apply before sending, if any.
    pub agent_id: Option<String>,
    /// Bcode agent (id and name) to announce to a native agent with this message, when it
    /// changed or the native agent dropped the earlier announcement.
    pub native_agent: Option<(String, String)>,
    /// Bcode agent staged for a native conversation, applied before this message is sent.
    pub native_agent_change: Option<NativeAgentChange>,
    /// Reasoning effort to apply before sending.
    pub reasoning_effort: Option<String>,
    /// Reasoning summary to apply before sending.
    pub reasoning_summary: Option<String>,
    /// Reasoning effort generation captured by this submission, if locally pending.
    pub reasoning_effort_generation: Option<u64>,
    /// Launch-scoped execution defaults for this exact turn.
    pub execution: bcode_session_models::TurnExecutionOptions,
    /// Event sender for a newly-created session stream.
    pub event_sender: mpsc::Sender<super::history_flow::SessionStreamUpdate>,
}

/// A staged Bcode agent for a native conversation, applied when the next message is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeAgentChange {
    /// Bcode agent saved with the session; the daemon decides native approvals with it.
    pub agent_id: String,
    /// Native permission mode to switch to, when it differs from the current one.
    pub mode: Option<bcode_harness_models::HarnessPermissionMode>,
}

/// Skill action kind requested by the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillActionKind {
    /// Activate the skill for the session.
    Activate,
    /// Deactivate the skill for the session.
    Deactivate,
    /// Invoke the skill for one turn.
    Invoke,
}

/// Skill action effect request payload.
pub struct SkillActionRequest {
    /// Existing session, if already attached.
    pub session_id: Option<SessionId>,
    /// Working directory to use when creating a draft session.
    pub launch_working_directory: std::path::PathBuf,
    /// Skill to act on.
    pub skill_id: SkillId,
    /// Skill action kind.
    pub action: SkillActionKind,
    /// Arguments for invocation.
    pub arguments: String,
    /// Provider to apply before invoking a skill, if any.
    pub provider_plugin_id: Option<String>,
    /// Model to apply before invoking a skill, if any.
    pub model_id: Option<String>,
    /// Agent to apply before invoking a skill, if any.
    pub agent_id: Option<String>,
    /// Reasoning effort captured for the invocation.
    pub reasoning_effort: Option<String>,
    /// Reasoning summary captured for the invocation.
    pub reasoning_summary: Option<String>,
    /// Pending reasoning effort generation captured by the invocation.
    pub reasoning_effort_generation: Option<u64>,
    /// Launch-scoped execution defaults for an invoked turn.
    pub execution: bcode_session_models::TurnExecutionOptions,
    /// Event sender for a newly-created session stream.
    pub event_sender: mpsc::Sender<super::history_flow::SessionStreamUpdate>,
}

/// Background work requested by local TUI event handling.
pub enum TuiEffect {
    /// Start shallow observation using the existing application client in background work.
    OpenXray {
        request: super::xray_inspectors::OpenRequest,
    },
    /// Attach to a session and start its event stream.
    OpenSession {
        /// Session to open.
        session_id: SessionId,
        /// Initial projection window request.
        initial_window_request: ProjectionWindowRequest,
        /// Event sender for the live session stream.
        event_sender: mpsc::Sender<super::history_flow::SessionStreamUpdate>,
        /// Whether this explicit open may start the daemon.
        allow_daemon_start: bool,
    },
    /// Load declarative configuration and global interactive TUI state.
    LoadConfig,
    /// Persist one interactive TUI theme selection to user state.
    PersistThemeSelection {
        /// Stable base theme id.
        name: String,
    },
    /// Clear the interactive theme selection and restore the configured default.
    ClearThemeSelection,
    /// Persist (or clear, with `None`) the interactive profile selection.
    PersistProfileSelection {
        /// Profile id, or none to clear.
        name: Option<String>,
    },
    /// Persist one interactive streaming-presentation policy to user state.
    PersistStreamingPresentation {
        /// Normalized renderer-neutral presentation policy.
        policy: bcode_session_view_models::StreamingPresentationPolicy,
    },
    /// Clear the interactive streaming-presentation override.
    ClearStreamingPresentation,
    /// Persist interactive per-tool structural defaults to user state, each
    /// replacing any stored rule with the same matcher.
    PersistPresentationRules {
        /// Rules to store, in order.
        rules: Vec<bcode_presentation_policy::models::PresentationRule>,
    },
    /// Reconcile auth security status for a loaded config.
    ReconcileAuthSecurity {
        /// Loaded configuration.
        config: Box<bcode_config::BcodeConfig>,
    },
    /// Load draft-session status.
    LoadDraftStatus {
        /// Directory for draft-session draft scope.
        launch_working_directory: std::path::PathBuf,
    },
    /// Execute one typed targeted recovery action through the shared application boundary.
    QueueRecovery {
        session_id: SessionId,
        generation: u64,
        action: Box<SessionViewAction>,
    },
    /// Load non-critical status for an attached session.
    LoadSessionStatus {
        /// Session to hydrate.
        session_id: SessionId,
    },
    /// Refresh resolved model metadata after a model event.
    LoadSessionModelStatus { session_id: SessionId },
    /// Refresh plugin-owned status after a plugin lifecycle event.
    LoadPluginStatus { session_id: SessionId },
    /// Load agent metadata.
    LoadAgentCatalog,
    /// Load the bounded pending-permission snapshot during attach/reconnect.
    ListPermissions,
    /// Save composer draft text for a scope.
    SaveDraft {
        /// Draft scope to save.
        scope: ComposerDraftScope,
        /// Draft text.
        text: String,
    },
    /// Load slash command completions for a composer query.
    LoadSlashPalette {
        /// Active provider scope for model discovery.
        provider_plugin_id: Option<String>,
        /// Composer discovery generation.
        generation: u64,
        /// Current slash query.
        query: String,
        /// Active session, if any.
        session_id: Option<SessionId>,
    },
    /// Load host and plugin command-palette contributions.
    LoadCommandPalette,
    /// Load one bounded direct-child page; no recursive daemon traversal.
    LoadSessionChildren {
        parent_id: SessionId,
        after: Option<SessionId>,
        generation: u64,
    },
    /// Observe normalized activity without appending transcript events.
    LoadChildActivity {
        session_id: SessionId,
        after: Option<SessionId>,
        generation: u64,
    },
    /// Load the bounded session catalog for the root session picker.
    LoadSessionPicker {
        generation: u64,
        scope: bcode_session_models::SessionDiscoveryScope,
        query: bcode_session_models::SessionCatalogQuery,
        cursor: Option<bcode_session_models::SessionCatalogCursor>,
    },
    /// Import one external session selected by the root picker.
    ImportSession {
        source_id: String,
        external_session_id: String,
    },
    /// Rename one session selected by the root picker.
    RenameSession {
        session_id: SessionId,
        name: Option<String>,
    },
    /// Delete one session selected by the root picker.
    DeleteSession { session_id: SessionId },
    /// Run one bounded transcript search from the root session picker.
    SearchSessions {
        request: Box<bcode_session_search::SessionSearchRequest>,
        policy: bcode_session_search::SessionSearchPlanPolicy,
    },
    /// Start compatibility inventory or confirmed canonical migration.
    StartBulkMigration {
        request: bcode_ipc::SessionBulkMigrationStartRequest,
    },
    /// Cooperatively cancel transient aggregate canonical migration.
    CancelBulkMigration { operation_id: String },
    /// Wait for the next transient aggregate canonical migration revision.
    WaitBulkMigration {
        operation_id: String,
        after_revision: u64,
    },
    /// Start explicit all-enabled-provider historical backfill.
    StartSearchIndexing {
        request: bcode_session_search::CompleteSessionSearchBackfillRequest,
    },
    /// Cancel transient aggregate derived-search backfill.
    CancelSearchIndexing { operation_id: String },
    /// Wait for the next transient aggregate derived-search backfill revision.
    WaitSearchIndexing {
        operation_id: String,
        after_revision: u64,
    },
    /// Execute a resolved slash command without terminal navigation.
    ExecuteSlashCommand {
        /// Current session.
        session_id: Option<SessionId>,
        /// Working directory context.
        working_directory: std::path::PathBuf,
        /// Current agent identity.
        current_agent_id: String,
        /// Current reasoning display mode.
        reasoning_display_mode: bcode_config::TuiThinkingMode,
        /// Whether reasoning is displayed.
        reasoning_visible: bool,
        /// Complete command text.
        message: String,
    },
    /// Execute one non-interactive Ralph action through the root runtime.
    RalphAction {
        /// Repository root for the Ralph lifecycle operation.
        repo_root: std::path::PathBuf,
        /// Action to execute.
        action: super::ralph_flow::RalphRootAction,
    },
    /// Create a Ralph loop from root-owned dialog input.
    RalphStart {
        /// Validated root dialog request.
        request: super::ralph_flow::RalphStartRequest,
    },
    /// Open a plugin-owned native surface for root-runtime presentation.
    OpenPluginSurface {
        /// Plugin owner.
        plugin_id: String,
        /// Surface kind.
        surface_kind: String,
        /// Instance identity.
        instance_id: String,
        /// Plugin-owned options.
        options: serde_json::Value,
        /// Working directory context.
        working_directory: std::path::PathBuf,
        /// Active session context.
        session_id: Option<SessionId>,
    },
    /// Find other agents' credentials not yet connected or announced, and record them as
    /// announced. Presence-only discovery; never reads credentials.
    AnnounceExternalConnections,
    /// Load portable auth-pool status before opening the subscription picker.
    LoadAuthPoolPicker { session_id: SessionId },
    /// Persist or clear an auth-pool preference.
    SetAuthPoolPreference {
        session_id: SessionId,
        action: super::auth_pool_picker::AccountAction,
    },
    /// Load skills before opening the root skill picker.
    LoadSkillPicker,
    /// Load one skill manifest for root picker help.
    DescribeSkill {
        /// Skill to describe.
        skill_id: SkillId,
    },
    /// Load reasoning status before opening the root thinking dialog.
    LoadThinkingDialog {
        /// Current session, if any.
        session_id: Option<SessionId>,
        /// Initially focused setting.
        focus: super::thinking_dialog::ThinkingDialogFocus,
    },
    /// Load one bounded canonical milestone page for timeline navigation.
    LoadMilestones {
        request: super::timeline_dialog::MilestonePageRequest,
    },
    /// Invoke a plugin-owned command without terminal navigation effects.
    InvokePluginCommand {
        /// Plugin owner.
        plugin_id: String,
        /// Command identifier.
        command_id: String,
        /// Optional command arguments.
        arguments: Option<String>,
        /// Working directory context.
        working_directory: std::path::PathBuf,
        /// Active session context.
        session_id: Option<SessionId>,
    },
    /// Submit a user message through the daemon-backed session pipeline.
    SubmitMessage {
        /// Submit request.
        request: Box<SubmitMessageRequest>,
    },
    /// Perform a skill action for a session.
    SkillAction {
        /// Skill action request.
        request: Box<SkillActionRequest>,
    },
    /// Set the active model for a session.
    SetSessionModel {
        /// Session to update.
        session_id: SessionId,
        /// Provider plugin id, when explicitly selected.
        provider_plugin_id: Option<String>,
        /// Model id to set.
        model_id: String,
        /// Approved external credential connection to use with the model, if any.
        external_auth_binding: Option<String>,
        /// Enrolled provider account to apply atomically with the model.
        account_reference: Option<String>,
    },
    /// Set session reasoning preferences.
    SetSessionReasoning {
        /// Session to update.
        session_id: SessionId,
        /// Optional reasoning effort.
        effort: Option<String>,
        /// Optional reasoning summary.
        summary: Option<String>,
        /// Pending effort generation staged for this update.
        effort_generation: Option<u64>,
        /// Success status text.
        status: String,
    },
    /// Append one durable presentation-only transcript note.
    AppendPresentationNote {
        /// Session that owns the note.
        session_id: SessionId,
        /// Stable producer identity.
        source_id: String,
        /// Unique note identity within the producer.
        note_id: String,
        /// Complete bounded note text.
        text: String,
        /// Renderer-neutral text format.
        format: bcode_command::CommandTextFormat,
    },
    /// Cancel runtime work for a session.
    CancelRuntimeWork {
        /// Session that owns the work.
        session_id: SessionId,
        /// Runtime work id.
        work_id: WorkId,
    },
    /// Request context compaction for the current session.
    CompactContext {
        /// Session to compact.
        session_id: SessionId,
    },
    /// Attach current session to a worktree path.
    AttachWorktree {
        /// Session to attach.
        session_id: SessionId,
        /// Selected worktree path.
        path: std::path::PathBuf,
    },
    /// Create a worktree.
    CreateWorktree {
        /// Stable idempotency identity retained across bounded waits.
        operation_id: String,
        /// Request payload.
        request: WorktreeCreateRequest,
    },
    /// Wait for worktree creation progress.
    WaitWorktreeCreate {
        operation_id: String,
        after_revision: u64,
    },
    /// Request cancellation of the active turn for a session.
    CancelTurn { session_id: SessionId },
    /// Resolve one pending permission request.
    ResolvePermission {
        /// Permission request id.
        permission_id: String,
        /// Whether to approve the request.
        approved: bool,
        /// Whether to remember the decision.
        remember: bool,
        /// Whether the action targets every request in the authorization batch.
        apply_to_batch: bool,
        /// Authorization batch id when `apply_to_batch` is true.
        batch_id: Option<String>,
    },
    /// Answer one managed native interaction with a retained response identity.
    ///
    /// Executed only through the sealed managed command handle; ordinary model access never
    /// answers native requests.
    RespondManagedInteraction {
        /// Ownership generation the response must target.
        generation: u64,
        /// Native request correlation.
        request_id: String,
        /// Retained response identity, reused when an earlier outcome was uncertain.
        response_id: String,
        /// Typed response value.
        value: bcode_harness_models::HarnessResponseValue,
    },
    /// Read input the daemon holds for this native session until it can be sent.
    LoadManagedQueue,
    /// Withdraw one held input before it is sent; `edit` returns its text to the composer.
    WithdrawManagedQueued { submission_id: String, edit: bool },
    /// Change one managed native session setting using a retained mutation identity.
    ConfigureManagedSession {
        /// Ownership generation the mutation must target.
        generation: u64,
        /// Retained mutation identity; ambiguous outcomes are never replayed.
        mutation_id: String,
        /// Requested setting.
        setting: bcode_harness_models::HarnessSessionSetting,
    },
    /// Execute one V2 transcript engine history request (W5, W10, W11).
    LoadTranscriptV2History {
        /// The request, its session and the resident tail it was issued against.
        load: super::transcript_v2_host::effects::HistoryLoad,
    },
}

/// Daemon connectivity observation reported by completed effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonObservation {
    /// The effect does not say anything about daemon connectivity.
    None,
    /// A daemon-backed request completed successfully.
    Success,
    /// The daemon was unavailable or unreachable.
    Unavailable(String),
    /// A daemon-backed request failed after reaching the daemon or for an unknown reason.
    Failed(String),
}

impl DaemonObservation {
    fn from_client_error(error: &ClientError) -> Self {
        if error.is_daemon_unavailable() {
            Self::Unavailable(error.to_string())
        } else {
            Self::Failed(error.to_string())
        }
    }

    fn from_client_result<T>(result: &Result<T, ClientError>) -> Self {
        match result {
            Ok(_) => Self::Success,
            Err(error) => Self::from_client_error(error),
        }
    }

    fn from_tui_result<T>(result: &Result<T, TuiError>) -> Self {
        match result {
            Ok(_) => Self::Success,
            Err(TuiError::Client(error)) => Self::from_client_error(error),
            Err(_) => Self::None,
        }
    }

    fn from_optional_error(connected: bool, error: Option<&str>) -> Self {
        if connected {
            Self::Success
        } else if let Some(error) = error {
            Self::Unavailable(error.to_owned())
        } else {
            Self::None
        }
    }
}

/// Completed TUI background work.
pub enum TuiEffectResult {
    /// A V2 transcript engine history request completed (W3, W5, W10, W11).
    TranscriptV2HistoryLoaded {
        /// The executed request.
        load: super::transcript_v2_host::effects::HistoryLoad,
        /// Loaded canonical events, ready for the engine.
        result: Result<super::transcript_v2_host::effects::HistoryLoaded, ClientError>,
    },
    /// Secret-safe result of a shallow observation launch.
    XrayOpened {
        session_id: SessionId,
        message: &'static str,
    },
    /// Targeted recovery action and fresh bounded status.
    QueueRecovery {
        session_id: SessionId,
        generation: u64,
        result: Box<
            Result<
                (
                    SessionViewActionOutcome,
                    bcode_session_models::SubmissionQueueStatus,
                ),
                ClientError,
            >,
        >,
    },
    SessionChildrenLoaded {
        parent_id: SessionId,
        generation: u64,
        result: Result<bcode_session_models::SessionChildPage, ClientError>,
    },
    ChildActivityLoaded {
        session_id: SessionId,
        generation: u64,
        result: Result<bcode_session_models::SessionActivityPage, ClientError>,
    },
    /// Session-open preparation progress emitted while the open effect is running.
    SessionOpenProgress {
        snapshot: bcode_session_models::SessionOpenOperationSnapshot,
    },
    /// Session open completed.
    SessionOpened {
        /// Session that was opened.
        session_id: SessionId,
        /// Attach result and event-stream task.
        result: Box<Result<(bcode_client::AttachedSessionHistory, JoinHandle<()>), TuiError>>,
    },
    /// User configuration load completed.
    ConfigLoaded {
        /// Config load result.
        config: Box<Result<bcode_config::BcodeConfig, String>>,
        /// Global interactive theme selection load result.
        theme_selection: Result<Option<String>, String>,
        /// Global interactive profile selection load result.
        profile_selection: Result<Option<String>, String>,
        /// Global interactive streaming-presentation override load result.
        streaming_presentation:
            Result<Option<bcode_session_view_models::StreamingPresentationPolicy>, String>,
    },
    /// Durable TUI profile state update completed.
    ProfileSelectionPersisted {
        /// Profile id requested by the user, or none when cleared.
        name: Option<String>,
        /// State write result.
        result: Result<std::path::PathBuf, String>,
    },
    /// Durable TUI theme state update completed.
    ThemeSelectionPersisted {
        /// Stable base theme id requested by the user, or none when cleared.
        name: Option<String>,
        /// State write result.
        result: Result<std::path::PathBuf, String>,
    },
    /// Interactive structural defaults were persisted (or failed to be).
    PresentationRulesPersisted {
        /// Rules requested for storage.
        rules: Vec<bcode_presentation_policy::models::PresentationRule>,
        /// State path and every stored user-state rule after the write.
        result: Result<
            (
                std::path::PathBuf,
                Vec<bcode_presentation_policy::models::PresentationRule>,
            ),
            String,
        >,
    },
    /// Durable streaming-presentation state update completed.
    StreamingPresentationPersisted {
        /// Policy requested by the user, or none when cleared.
        policy: Option<bcode_session_view_models::StreamingPresentationPolicy>,
        /// State write result.
        result: Result<std::path::PathBuf, String>,
    },
    /// Auth security reconciliation completed.
    AuthSecurityReconciled {
        /// Status to display, if any.
        status: Option<String>,
    },
    /// Draft-session status hydration completed.
    DraftStatusLoaded {
        /// Whether at least one daemon-backed request completed successfully.
        daemon_connected: bool,
        /// Default model status, if available.
        model: Option<bcode_model::SessionModelStatus>,
        /// Restored composer draft, if available.
        composer_draft: Option<String>,
        /// First non-critical error encountered.
        error: Option<String>,
    },
    /// Attached session status hydration completed.
    SessionStatusLoaded {
        /// Whether at least one daemon-backed request completed successfully.
        daemon_connected: bool,
        /// Session that was hydrated.
        session_id: SessionId,
        /// Hydrated semantic/runtime status.
        hydration: Box<SessionStatusHydration>,
    },
    /// Targeted model projection refresh completed.
    SessionModelStatusLoaded {
        session_id: SessionId,
        result: Result<bcode_model::SessionModelStatus, ClientError>,
    },
    /// Targeted plugin status projection refresh completed.
    PluginStatusLoaded {
        session_id: SessionId,
        plugin_status: Vec<bcode_session_view_models::PluginStatusView>,
        error: Option<String>,
    },
    /// Agent metadata load completed.
    AgentCatalogLoaded {
        /// Agent catalog result.
        agents: Result<AgentCatalog, String>,
    },
    /// Permission poll completed.
    PermissionList {
        /// Permission list result.
        result: Result<Vec<PermissionSummary>, ClientError>,
    },
    /// Composer draft save completed.
    SaveDraft {
        /// Saved draft text.
        text: String,
        /// Save result.
        result: Result<(), ClientError>,
    },
    /// Slash palette load completed.
    SlashPaletteLoaded {
        /// Composer discovery generation.
        generation: u64,
        /// Query used to build completions.
        query: String,
        /// Loaded palette state.
        palette: slash_palette::SlashPalette,
    },
    /// Command palette contributions load completed.
    CommandPaletteLoaded {
        /// Contributions result from the application client boundary.
        result: Result<Vec<bcode_command::CommandContribution>, ClientError>,
    },
    /// Root session-picker catalog load completed.
    SessionPickerLoaded {
        /// Subscription generation that produced the result.
        generation: u64,
        /// Bounded catalog result from the application client boundary.
        result: Result<bcode_session_models::SessionCatalogPage, ClientError>,
        /// Catalog invalidation restarts navigation rather than reusing a stale cursor.
        restart: bool,
    },
    /// External session import completed.
    SessionImported {
        result: Result<(SessionSummary, Vec<bcode_ipc::SessionImportWarning>), ClientError>,
    },
    /// Session rename completed.
    SessionRenamed {
        result: Result<SessionSummary, ClientError>,
    },
    /// Session deletion completed.
    SessionDeleted {
        result: Result<SessionSummary, ClientError>,
    },
    /// Root picker transcript search completed.
    SessionsSearched {
        result: Result<
            (
                bcode_session_search::FederatedSessionSearchResponse,
                Vec<bcode_session_search::HydratedSessionSearchHit>,
            ),
            ClientError,
        >,
    },
    BulkMigrationUpdated {
        result: Result<bcode_ipc::SessionBulkMigrationOperationStatus, ClientError>,
    },
    BulkMigrationWaited {
        result: Result<bcode_ipc::SessionBulkMigrationOperationStatus, ClientError>,
    },
    SearchIndexingStarted {
        result: Result<bcode_session_search::StartSessionSearchBackfillResponse, ClientError>,
    },
    SearchIndexingUpdated {
        result: Result<bcode_session_search::SessionSearchBackfillOperationStatus, ClientError>,
    },
    SearchIndexingWaited {
        result: Result<bcode_session_search::SessionSearchBackfillOperationStatus, ClientError>,
    },
    /// Resolved slash command execution completed.
    SlashCommandExecuted {
        /// Submitted command text.
        message: String,
        /// Command outcome.
        result: Result<super::slash_commands::SlashCommandOutcome, ClientError>,
    },
    /// Root Ralph action completed.
    RalphAction {
        /// Completed action.
        action: super::ralph_flow::RalphRootAction,
        /// Presentation result.
        result: Result<super::ralph_flow::RalphRootOutput, TuiError>,
    },
    /// Root Ralph loop creation completed.
    RalphStarted {
        /// Creation result.
        result: Result<super::ralph_flow::RalphStartOutput, TuiError>,
    },
    /// Plugin-owned native surface opened.
    PluginSurfaceOpened {
        /// Plugin owner.
        plugin_id: String,
        /// Opened surface or error.
        result: Result<bcode_plugin_sdk::tui::BoxedPluginTuiSurface, TuiError>,
    },
    /// Newly found, unconnected credentials of other coding agents, by application.
    ExternalConnectionsAnnounced {
        /// Applications to mention, or the discovery failure.
        result: Result<Vec<String>, ClientError>,
    },
    /// Portable auth-pool status loaded.
    AuthPoolPickerLoaded {
        /// Session whose configured context was read.
        session_id: SessionId,
        /// Loaded secret-free pool summaries.
        result: Result<bcode_provider_auth_models::AccountRouting, ClientError>,
    },
    /// Interactive auth-pool preference mutation completed.
    AuthPoolPreferenceSet {
        /// Session whose configured context was updated.
        session_id: SessionId,
        /// Authoritative post-mutation routing snapshot.
        result: Result<bcode_provider_auth_models::AccountRouting, ClientError>,
    },
    /// Skills for the root skill picker loaded.
    SkillPickerLoaded {
        /// Loaded skill summaries.
        result: Result<bcode_skill_models::SkillList, ClientError>,
    },
    /// Skill details for root picker help loaded.
    SkillDescribed {
        /// Requested skill.
        skill_id: SkillId,
        /// Loaded manifest.
        result: Result<bcode_skill_models::SkillManifest, ClientError>,
    },
    /// Reasoning status for the root thinking dialog loaded.
    ThinkingDialogLoaded {
        /// Initially focused setting.
        focus: super::thinking_dialog::ThinkingDialogFocus,
        /// Loaded model status.
        result: Result<bcode_model::SessionModelStatus, ClientError>,
    },
    /// Canonical milestones loaded without generation.
    MilestonesLoaded {
        /// Exact terminal-only request identity.
        request: super::timeline_dialog::MilestonePageRequest,
        /// Bounded canonical page.
        result: Result<bcode_session_models::SessionMilestonePage, ClientError>,
    },
    /// Plugin command invocation completed.
    PluginCommandInvoked {
        /// Plugin owner.
        plugin_id: String,
        /// Typed command response.
        result: Result<bcode_command::InvokeCommandResponse, TuiError>,
    },
    /// Submit message completed.
    SubmitMessage {
        /// Identity retained even when transport outcome is ambiguous.
        submission_id: bcode_session_models::SubmissionId,
        /// Message text originally submitted.
        message: String,
        /// Submit result.
        result: Box<Result<SubmitMessageResult, ClientError>>,
    },
    /// Skill action completed.
    SkillAction {
        /// Skill action kind.
        action: SkillActionKind,
        /// Skill acted on.
        skill_id: SkillId,
        /// Skill action result.
        result: Box<Result<SkillActionResult, ClientError>>,
    },
    /// Session model selection completed.
    SetSessionModel {
        /// Session that was updated.
        session_id: SessionId,
        /// Provider plugin id, when explicitly selected.
        provider_plugin_id: Option<String>,
        /// Model id that was requested.
        model_id: String,
        /// Daemon response.
        result: Result<(), ClientError>,
    },
    /// Session reasoning update completed.
    SetSessionReasoning {
        /// Session that was updated.
        session_id: SessionId,
        /// Effort value requested by the update.
        effort: Option<String>,
        /// Pending effort generation staged for the update.
        effort_generation: Option<u64>,
        /// Success status text.
        status: String,
        /// Daemon response.
        result: Result<(), ClientError>,
    },
    /// Durable presentation note append completed.
    AppendPresentationNote {
        /// Session whose ordered note append completed.
        session_id: SessionId,
        /// Daemon response.
        result: Result<(), ClientError>,
    },
    /// Runtime work cancellation completed.
    CancelRuntimeWork {
        /// Cancelled work id.
        work_id: WorkId,
        /// Daemon response.
        result: Result<bool, ClientError>,
    },
    /// Context compaction completed.
    CompactContext {
        /// Session the request targeted.
        session_id: SessionId,
        /// Daemon response.
        result: Result<String, ClientError>,
    },
    /// Worktree attach completed.
    AttachWorktree {
        /// Selected worktree path.
        path: std::path::PathBuf,
        /// Attach result.
        result: Result<SessionSummary, ClientError>,
    },
    /// Worktree creation completed.
    CreateWorktree {
        /// Worktree operation snapshot.
        result: Result<bcode_worktree_models::WorktreeCreateOperationStatus, ClientError>,
    },
    /// Result for active turn cancellation.
    CancelTurn {
        /// Session the request targeted.
        session_id: SessionId,
        /// Daemon response.
        result: Result<bool, ClientError>,
    },
    /// Result for a permission resolution.
    PermissionResolved {
        /// Permission request id.
        permission_id: String,
        /// Whether the request was approved.
        approved: bool,
        /// Whether the decision was remembered.
        remember: bool,
        /// Whether the action targeted the authorization batch.
        apply_to_batch: bool,
        /// Daemon response indicating whether any request was resolved.
        result: Result<bool, ClientError>,
    },
    /// Result for one managed native interaction response.
    ManagedInteractionResponded {
        /// Ownership generation the response targeted.
        generation: u64,
        /// Native request correlation.
        request_id: String,
        /// Retained response identity.
        response_id: String,
        /// Managed application response.
        result: Result<bcode_harness_models::HarnessServiceResponse, ClientError>,
    },
    /// Held native input, after a read or a withdrawal.
    ManagedQueueLoaded {
        /// Withdrawal this answers, with whether its text returns to the composer.
        withdrawal: Option<(String, bool)>,
        /// Held input and whether the withdrawal took effect.
        result: Result<bcode_ipc::harness::ManagedQueuedInput, ClientError>,
    },
    /// Result for one managed native setting change.
    ManagedSessionConfigured {
        /// Retained mutation identity.
        mutation_id: String,
        /// Requested setting.
        setting: bcode_harness_models::HarnessSessionSetting,
        /// Managed application response.
        result: Result<bcode_harness_models::HarnessServiceResponse, ClientError>,
    },
    /// An ordinary open found a managed binding; the root must hand off to managed reattach.
    ManagedSessionDetected {
        /// Session that was being opened.
        session_id: SessionId,
        /// Canonical managed binding evidence.
        binding: Box<bcode_ipc::ManagedSessionTargetSnapshot>,
    },
}

#[allow(clippy::match_same_arms, clippy::too_many_lines)]
impl TuiEffectResult {
    /// Return the daemon connectivity observation implied by this effect result.
    #[must_use]
    pub fn daemon_observation(&self) -> DaemonObservation {
        match self {
            Self::XrayOpened { .. } => DaemonObservation::None,
            Self::TranscriptV2HistoryLoaded { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::QueueRecovery { result, .. } => DaemonObservation::from_client_result(result),
            Self::SessionChildrenLoaded { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::ChildActivityLoaded { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::SessionOpenProgress { .. } => DaemonObservation::Success,
            Self::SessionOpened { result, .. } => DaemonObservation::from_tui_result(result),
            Self::DraftStatusLoaded {
                daemon_connected,
                error,
                ..
            } => DaemonObservation::from_optional_error(*daemon_connected, error.as_deref()),
            Self::SessionStatusLoaded {
                daemon_connected,
                hydration,
                ..
            } => DaemonObservation::from_optional_error(
                *daemon_connected,
                hydration.error.as_deref(),
            ),
            Self::SessionModelStatusLoaded { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::PluginStatusLoaded { error, .. } => {
                error.as_ref().map_or(DaemonObservation::Success, |error| {
                    DaemonObservation::Failed(error.clone())
                })
            }
            Self::AgentCatalogLoaded { agents } => match agents {
                Ok(_) => DaemonObservation::Success,
                Err(error) => DaemonObservation::Unavailable(error.clone()),
            },
            Self::PermissionList { result } => DaemonObservation::from_client_result(result),
            Self::SaveDraft { result, .. } => DaemonObservation::from_client_result(result),
            Self::SkillAction { result, .. } => DaemonObservation::from_client_result(result),
            Self::SetSessionModel { result, .. } => DaemonObservation::from_client_result(result),
            Self::SetSessionReasoning { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::AppendPresentationNote { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::SubmitMessage { result, .. } => DaemonObservation::from_client_result(result),
            Self::CompactContext { result, .. } => DaemonObservation::from_client_result(result),
            Self::CancelRuntimeWork { result, .. } => DaemonObservation::from_client_result(result),
            Self::AttachWorktree { result, .. } => DaemonObservation::from_client_result(result),
            Self::CreateWorktree { result } => DaemonObservation::from_client_result(result),
            Self::CancelTurn { result, .. } | Self::PermissionResolved { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::ConfigLoaded { .. }
            | Self::ThemeSelectionPersisted { .. }
            | Self::ProfileSelectionPersisted { .. }
            | Self::StreamingPresentationPersisted { .. }
            | Self::PresentationRulesPersisted { .. }
            | Self::AuthSecurityReconciled { .. }
            | Self::SlashPaletteLoaded { .. }
            | Self::CommandPaletteLoaded { .. }
            | Self::SessionPickerLoaded { .. }
            | Self::SessionImported { .. }
            | Self::SessionRenamed { .. }
            | Self::SessionDeleted { .. }
            | Self::SessionsSearched { .. }
            | Self::BulkMigrationUpdated { .. }
            | Self::BulkMigrationWaited { .. }
            | Self::SearchIndexingStarted { .. }
            | Self::SearchIndexingUpdated { .. }
            | Self::SearchIndexingWaited { .. }
            | Self::SlashCommandExecuted { .. }
            | Self::ThinkingDialogLoaded { .. }
            | Self::MilestonesLoaded { .. }
            | Self::PluginCommandInvoked { .. } => DaemonObservation::None,
            Self::RalphAction { result, .. } => DaemonObservation::from_tui_result(result),
            Self::RalphStarted { result } => DaemonObservation::from_tui_result(result),
            Self::PluginSurfaceOpened { result, .. } => DaemonObservation::from_tui_result(result),
            Self::ExternalConnectionsAnnounced { result } => {
                DaemonObservation::from_client_result(result)
            }
            Self::AuthPoolPickerLoaded { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::AuthPoolPreferenceSet { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::SkillPickerLoaded { result } => DaemonObservation::from_client_result(result),
            Self::SkillDescribed { result, .. } => DaemonObservation::from_client_result(result),
            Self::ManagedInteractionResponded { result, .. }
            | Self::ManagedSessionConfigured { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::ManagedQueueLoaded { result, .. } => {
                DaemonObservation::from_client_result(result)
            }
            Self::ManagedSessionDetected { .. } => DaemonObservation::Success,
        }
    }
}

/// Skill action effect success payload.
#[derive(Debug)]
pub struct SkillActionResult {
    /// Session that received the skill action.
    pub session_id: SessionId,
    /// Newly-created/attached session summary, if the action created a session.
    pub created_session: Option<SessionSummary>,
    /// Event stream task for a newly-created session.
    pub event_task: Option<JoinHandle<()>>,
    /// Invocation acceptance when invoking a skill.
    pub acceptance: Option<MessageAcceptance>,
    /// Agent committed during invocation.
    pub committed_agent_id: Option<String>,
    /// Pending reasoning effort generation committed during invocation.
    pub committed_reasoning_effort_generation: Option<u64>,
    /// Releases a newly-created session stream after the TUI installs the session id.
    pub event_stream_release: Option<oneshot::Sender<()>>,
}

/// Attached session status hydration payload.
#[derive(Debug)]
pub struct SessionStatusHydration {
    /// Model status, if available.
    pub model: Option<bcode_model::SessionModelStatus>,
    /// Active skills captured during bounded attach hydration.
    pub active_skills: Option<Vec<bcode_skill_models::SkillContextResponse>>,
    /// Runtime work snapshots, if available.
    pub runtime_work: Option<Vec<bcode_session_models::RuntimeWorkSnapshot>>,
    /// Pending interactive requests, if available.
    pub interactions: Option<Vec<bcode_session_view_models::InteractionViewSummary>>,
    /// Active plugin-owned status contributions.
    pub plugin_status: Vec<bcode_session_view_models::PluginStatusView>,
    /// First non-critical error encountered.
    pub error: Option<String>,
}

#[derive(Debug)]
pub enum ComposerReceipt {
    Model(bcode_session_models::SubmissionReceipt),
    Managed(Box<bcode_session_models::ExternalHarnessResponse>),
}

#[derive(Debug)]
pub struct SubmitMessageResult {
    /// Session that received the message.
    pub session_id: SessionId,
    /// Newly-created/attached session summary, if the submit created a session.
    pub created_session: Option<SessionSummary>,
    /// Bounded lifecycle read following acceptance, when available.
    pub submission: Result<Option<bcode_session_models::SubmissionRecord>, ClientError>,
    /// Server acceptance for the submitted message.
    pub receipt: Option<ComposerReceipt>,
    /// Prepared exact native envelope. Its debug representation is not printed.
    pub prepared_request: Option<Box<SubmitMessageRequest>>,
    /// Agent committed during submission.
    pub committed_agent_id: Option<String>,
    /// Native setting acknowledged while applying a staged agent.
    pub applied_native_setting: Option<bcode_harness_models::HarnessSessionSetting>,
    /// Pending reasoning effort generation committed during submission.
    pub committed_reasoning_effort_generation: Option<u64>,
    /// Event stream task for a newly-created session.
    pub event_task: Option<JoinHandle<()>>,
    /// Releases a newly-created session stream after the TUI installs the session id.
    pub event_stream_release: Option<oneshot::Sender<()>>,
}

impl SubmitMessageResult {
    const fn empty(session_id: SessionId) -> Self {
        Self {
            session_id,
            created_session: None,
            submission: Ok(None),
            receipt: None,
            prepared_request: None,
            committed_agent_id: None,
            applied_native_setting: None,
            committed_reasoning_effort_generation: None,
            event_task: None,
            event_stream_release: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum EffectKey {
    TranscriptV2History(super::transcript_v2_host::effects::HistoryLane),
    Xray,
    QueueRecovery,
    SessionOpen,
    Config,
    ThemeSelection,
    ProfileSelection,
    StreamingPresentation,
    PresentationRules,
    AuthSecurity,
    DraftStatus,
    SessionStatus,
    SessionModelStatus,
    PluginStatus,
    AgentCatalog,
    PermissionList,
    DraftSave,
    SlashPalette,
    CommandPalette,
    SessionChildren(SessionId),
    ChildActivity(SessionId),
    SessionPicker,
    ImportSession,
    RenameSession(SessionId),
    DeleteSession(SessionId),
    SearchSessions,
    BulkMigration,
    SearchIndexing,
    SlashCommand,
    RalphAction,
    RalphStart,
    PluginSurface,
    ExternalConnectionNotice,
    AuthPoolPicker,
    AuthPoolPreference,
    SkillPicker,
    SkillDescription,
    ThinkingDialog,
    TimelineJump,
    PluginCommand(String, String),
    SubmitMessage(bcode_session_models::SubmissionId),
    SkillAction(SkillId),
    SetSessionModel(SessionId),
    SetSessionReasoning(SessionId),
    AppendPresentationNote(SessionId, String),
    CancelRuntimeWork(SessionId),
    CompactContext(SessionId),
    AttachWorktree(SessionId),
    CreateWorktree,
    CancelTurn(SessionId),
    ResolvePermission(String),
    ManagedRespond(u64, String),
    ManagedConfigure,
    ManagedQueue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectSchedule {
    StartIfIdle,
    Replace,
    QueueLatest,
    Cancel,
}

/// Daemon-backed effect scheduling class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectDaemonIntent {
    /// Use the background client clone for non-foreground work.
    Background,
    /// Use the foreground client clone for explicit user actions.
    Foreground,
}

impl TuiEffect {
    /// Complete any rejected effect without assuming that only foreground work can fail.
    /// Managed callers have no model/client fallback; preserve correlation for stale-result fences.
    #[allow(clippy::too_many_lines, dead_code)]
    fn application_access_failed(self, client_error: ClientError) -> TuiEffectResult {
        match self {
            Self::LoadTranscriptV2History { load } => TuiEffectResult::TranscriptV2HistoryLoaded {
                load,
                result: Err(client_error),
            },
            Self::OpenXray { request } => TuiEffectResult::XrayOpened {
                session_id: request.session_id,
                message: "X-ray requires existing application access",
            },
            Self::QueueRecovery {
                session_id,
                generation,
                ..
            } => TuiEffectResult::QueueRecovery {
                generation,
                session_id,
                result: Box::new(Err(client_error)),
            },
            Self::LoadSessionChildren {
                parent_id,
                generation,
                ..
            } => TuiEffectResult::SessionChildrenLoaded {
                parent_id,
                generation,
                result: Err(client_error),
            },
            Self::LoadChildActivity {
                session_id,
                generation,
                ..
            } => TuiEffectResult::ChildActivityLoaded {
                session_id,
                generation,
                result: Err(client_error),
            },
            Self::SaveDraft { text, .. } => TuiEffectResult::SaveDraft {
                text,
                result: Err(client_error),
            },
            Self::ExecuteSlashCommand { message, .. } => TuiEffectResult::SlashCommandExecuted {
                message,
                result: Err(client_error),
            },
            Self::OpenSession { session_id, .. } => TuiEffectResult::SessionOpened {
                session_id,
                result: Box::new(Err(TuiError::Client(client_error))),
            },
            Self::LoadDraftStatus { .. } => TuiEffectResult::DraftStatusLoaded {
                daemon_connected: !client_error.is_daemon_unavailable(),
                model: None,
                composer_draft: None,
                error: Some(client_error.to_string()),
            },
            Self::LoadSessionStatus { session_id } => TuiEffectResult::SessionStatusLoaded {
                daemon_connected: !client_error.is_daemon_unavailable(),
                session_id,
                hydration: Box::new(SessionStatusHydration {
                    model: None,
                    active_skills: None,
                    runtime_work: None,
                    interactions: None,
                    plugin_status: Vec::new(),
                    error: Some(client_error.to_string()),
                }),
            },
            Self::LoadSessionModelStatus { session_id } => {
                TuiEffectResult::SessionModelStatusLoaded {
                    session_id,
                    result: Err(client_error),
                }
            }
            Self::LoadPluginStatus { session_id } => TuiEffectResult::PluginStatusLoaded {
                session_id,
                plugin_status: Vec::new(),
                error: Some(client_error.to_string()),
            },
            Self::SubmitMessage { request } => TuiEffectResult::SubmitMessage {
                submission_id: request.submission_id,
                message: request.message,
                result: Box::new(Err(client_error)),
            },
            Self::SkillAction { request } => TuiEffectResult::SkillAction {
                action: request.action,
                skill_id: request.skill_id,
                result: Box::new(Err(client_error)),
            },
            Self::SetSessionModel {
                session_id,
                provider_plugin_id,
                model_id,
                ..
            } => TuiEffectResult::SetSessionModel {
                session_id,
                provider_plugin_id,
                model_id,
                result: Err(client_error),
            },
            Self::SetSessionReasoning {
                session_id,
                effort,
                effort_generation,
                status,
                ..
            } => TuiEffectResult::SetSessionReasoning {
                session_id,
                effort,
                effort_generation,
                status,
                result: Err(client_error),
            },
            Self::AppendPresentationNote { session_id, .. } => {
                TuiEffectResult::AppendPresentationNote {
                    session_id,
                    result: Err(client_error),
                }
            }
            Self::CancelRuntimeWork { work_id, .. } => TuiEffectResult::CancelRuntimeWork {
                work_id,
                result: Err(client_error),
            },
            Self::CompactContext { session_id } => TuiEffectResult::CompactContext {
                session_id,
                result: Err(client_error),
            },
            Self::AttachWorktree { path, .. } => TuiEffectResult::AttachWorktree {
                path,
                result: Err(client_error),
            },
            Self::CreateWorktree { .. } | Self::WaitWorktreeCreate { .. } => {
                TuiEffectResult::CreateWorktree {
                    result: Err(client_error),
                }
            }
            Self::CancelTurn { session_id } => TuiEffectResult::CancelTurn {
                session_id,
                result: Err(client_error),
            },
            Self::ResolvePermission {
                permission_id,
                approved,
                remember,
                apply_to_batch,
                ..
            } => TuiEffectResult::PermissionResolved {
                permission_id,
                approved,
                remember,
                apply_to_batch,
                result: Err(client_error),
            },
            Self::ListPermissions => TuiEffectResult::PermissionList {
                result: Err(client_error),
            },
            Self::LoadCommandPalette => TuiEffectResult::CommandPaletteLoaded {
                result: Err(client_error),
            },
            Self::LoadSessionPicker { generation, .. } => TuiEffectResult::SessionPickerLoaded {
                generation,
                result: Err(client_error),
                restart: false,
            },
            Self::LoadAgentCatalog => TuiEffectResult::AgentCatalogLoaded {
                agents: Err(client_error.to_string()),
            },
            Self::LoadConfig => TuiEffectResult::ConfigLoaded {
                config: Box::new(Err(client_error.to_string())),
                theme_selection: Err(client_error.to_string()),
                profile_selection: Err(client_error.to_string()),
                streaming_presentation: Err(client_error.to_string()),
            },
            Self::PersistThemeSelection { name } => TuiEffectResult::ThemeSelectionPersisted {
                name: Some(name),
                result: Err(client_error.to_string()),
            },
            Self::ClearThemeSelection => TuiEffectResult::ThemeSelectionPersisted {
                name: None,
                result: Err(client_error.to_string()),
            },
            Self::PersistProfileSelection { name } => TuiEffectResult::ProfileSelectionPersisted {
                name,
                result: Err(client_error.to_string()),
            },
            Self::PersistStreamingPresentation { policy } => {
                TuiEffectResult::StreamingPresentationPersisted {
                    policy: Some(policy),
                    result: Err(client_error.to_string()),
                }
            }
            Self::ClearStreamingPresentation => TuiEffectResult::StreamingPresentationPersisted {
                policy: None,
                result: Err(client_error.to_string()),
            },
            Self::PersistPresentationRules { rules } => {
                TuiEffectResult::PresentationRulesPersisted {
                    rules,
                    result: Err(client_error.to_string()),
                }
            }
            Self::ReconcileAuthSecurity { .. } => TuiEffectResult::AuthSecurityReconciled {
                status: Some(client_error.to_string()),
            },
            Self::LoadSlashPalette {
                generation, query, ..
            } => {
                let palette = slash_palette::SlashPalette::immediate(
                    &query,
                    bcode_config::CompletionConfig::default(),
                );
                TuiEffectResult::SlashPaletteLoaded {
                    generation,
                    query,
                    palette,
                }
            }
            Self::ImportSession { .. } => TuiEffectResult::SessionImported {
                result: Err(client_error),
            },
            Self::RenameSession { .. } => TuiEffectResult::SessionRenamed {
                result: Err(client_error),
            },
            Self::DeleteSession { .. } => TuiEffectResult::SessionDeleted {
                result: Err(client_error),
            },
            Self::SearchSessions { .. } => TuiEffectResult::SessionsSearched {
                result: Err(client_error),
            },
            Self::StartBulkMigration { .. } | Self::CancelBulkMigration { .. } => {
                TuiEffectResult::BulkMigrationUpdated {
                    result: Err(client_error),
                }
            }
            Self::WaitBulkMigration { .. } => TuiEffectResult::BulkMigrationWaited {
                result: Err(client_error),
            },
            Self::StartSearchIndexing { .. } => TuiEffectResult::SearchIndexingStarted {
                result: Err(client_error),
            },
            Self::CancelSearchIndexing { .. } => TuiEffectResult::SearchIndexingUpdated {
                result: Err(client_error),
            },
            Self::WaitSearchIndexing { .. } => TuiEffectResult::SearchIndexingWaited {
                result: Err(client_error),
            },
            Self::RalphAction { action, .. } => TuiEffectResult::RalphAction {
                action,
                result: Err(client_error.into()),
            },
            Self::RalphStart { .. } => TuiEffectResult::RalphStarted {
                result: Err(client_error.into()),
            },
            Self::OpenPluginSurface { plugin_id, .. } => TuiEffectResult::PluginSurfaceOpened {
                plugin_id,
                result: Err(client_error.into()),
            },
            Self::AnnounceExternalConnections => TuiEffectResult::ExternalConnectionsAnnounced {
                result: Err(client_error),
            },
            Self::LoadAuthPoolPicker { session_id } => TuiEffectResult::AuthPoolPickerLoaded {
                session_id,
                result: Err(client_error),
            },
            Self::SetAuthPoolPreference { session_id, .. } => {
                TuiEffectResult::AuthPoolPreferenceSet {
                    session_id,
                    result: Err(client_error),
                }
            }
            Self::LoadSkillPicker => TuiEffectResult::SkillPickerLoaded {
                result: Err(client_error),
            },
            Self::DescribeSkill { skill_id } => TuiEffectResult::SkillDescribed {
                skill_id,
                result: Err(client_error),
            },
            Self::LoadThinkingDialog { focus, .. } => TuiEffectResult::ThinkingDialogLoaded {
                focus,
                result: Err(client_error),
            },
            Self::LoadMilestones { request } => TuiEffectResult::MilestonesLoaded {
                request,
                result: Err(client_error),
            },
            Self::InvokePluginCommand { plugin_id, .. } => TuiEffectResult::PluginCommandInvoked {
                plugin_id,
                result: Err(client_error.into()),
            },
            Self::RespondManagedInteraction {
                generation,
                request_id,
                response_id,
                ..
            } => TuiEffectResult::ManagedInteractionResponded {
                generation,
                request_id,
                response_id,
                result: Err(client_error),
            },
            Self::LoadManagedQueue => TuiEffectResult::ManagedQueueLoaded {
                withdrawal: None,
                result: Err(client_error),
            },
            Self::WithdrawManagedQueued {
                submission_id,
                edit,
            } => TuiEffectResult::ManagedQueueLoaded {
                withdrawal: Some((submission_id, edit)),
                result: Err(client_error),
            },
            Self::ConfigureManagedSession {
                mutation_id,
                setting,
                ..
            } => TuiEffectResult::ManagedSessionConfigured {
                mutation_id,
                setting,
                result: Err(client_error),
            },
        }
    }

    const fn daemon_intent(&self) -> EffectDaemonIntent {
        match self {
            Self::OpenSession {
                allow_daemon_start: true,
                ..
            }
            | Self::QueueRecovery { .. }
            | Self::LoadDraftStatus { .. }
            | Self::LoadSessionStatus { .. }
            | Self::LoadAgentCatalog
            | Self::SubmitMessage { .. }
            | Self::SkillAction { .. }
            | Self::SetSessionModel { .. }
            | Self::SetSessionReasoning { .. }
            | Self::AppendPresentationNote { .. }
            | Self::CancelRuntimeWork { .. }
            | Self::CompactContext { .. }
            | Self::AttachWorktree { .. }
            | Self::CreateWorktree { .. }
            | Self::WaitWorktreeCreate { .. }
            | Self::CancelTurn { .. }
            | Self::ResolvePermission { .. }
            | Self::OpenXray { .. }
            | Self::InvokePluginCommand { .. }
            | Self::RespondManagedInteraction { .. }
            | Self::LoadManagedQueue
            | Self::WithdrawManagedQueued { .. }
            | Self::ConfigureManagedSession { .. } => EffectDaemonIntent::Foreground,
            Self::OpenSession {
                allow_daemon_start: false,
                ..
            }
            | Self::LoadSessionChildren { .. }
            | Self::LoadChildActivity { .. }
            | Self::LoadConfig
            | Self::PersistThemeSelection { .. }
            | Self::ClearThemeSelection
            | Self::PersistProfileSelection { .. }
            | Self::PersistStreamingPresentation { .. }
            | Self::ClearStreamingPresentation
            | Self::PersistPresentationRules { .. }
            | Self::ReconcileAuthSecurity { .. }
            | Self::LoadSessionModelStatus { .. }
            | Self::LoadPluginStatus { .. }
            | Self::ListPermissions
            | Self::SaveDraft { .. }
            | Self::LoadSlashPalette { .. }
            | Self::LoadCommandPalette
            | Self::LoadSessionPicker { .. }
            | Self::ImportSession { .. }
            | Self::RenameSession { .. }
            | Self::DeleteSession { .. }
            | Self::SearchSessions { .. }
            | Self::StartBulkMigration { .. }
            | Self::CancelBulkMigration { .. }
            | Self::WaitBulkMigration { .. }
            | Self::StartSearchIndexing { .. }
            | Self::CancelSearchIndexing { .. }
            | Self::WaitSearchIndexing { .. }
            | Self::ExecuteSlashCommand { .. }
            | Self::RalphAction { .. }
            | Self::RalphStart { .. }
            | Self::OpenPluginSurface { .. }
            | Self::AnnounceExternalConnections
            | Self::LoadAuthPoolPicker { .. }
            | Self::SetAuthPoolPreference { .. }
            | Self::LoadSkillPicker
            | Self::DescribeSkill { .. }
            | Self::LoadThinkingDialog { .. }
            | Self::LoadMilestones { .. }
            | Self::LoadTranscriptV2History { .. } => EffectDaemonIntent::Background,
        }
    }
}

type OrderedPresentationEffects = BTreeMap<SessionId, VecDeque<TuiEffect>>;
type ScheduledEffects = Vec<(EffectSchedule, TuiEffect)>;

/// Queue of effects requested before the chat loop runner can start them.
///
/// The queue keeps only the latest pending request for each effect key. This
/// mirrors runner semantics and avoids spawning then immediately aborting stale
/// work when multiple state transitions request the same background effect
/// before the loop has a chance to drain the queue.
#[derive(Default)]
pub struct TuiEffectQueue {
    effects: BTreeMap<EffectKey, (EffectSchedule, TuiEffect)>,
}

impl TuiEffectQueue {
    /// Queue an effect using normal start-if-idle scheduling.
    pub fn start(&mut self, effect: TuiEffect) {
        self.push(effect, EffectSchedule::StartIfIdle);
    }

    /// Queue an effect that should replace any in-flight effect with the same key.
    pub fn replace(&mut self, effect: TuiEffect) {
        self.push(effect, EffectSchedule::Replace);
    }

    /// Queue the latest effect with this key to run after the current one finishes.
    pub fn queue_latest(&mut self, effect: TuiEffect) {
        self.push(effect, EffectSchedule::QueueLatest);
    }

    /// Queue an ordered presentation note without collapsing another note for the same session.
    pub fn start_ordered(&mut self, effect: TuiEffect) {
        let key = effect.key();
        debug_assert!(matches!(key, EffectKey::AppendPresentationNote(_, _)));
        self.effects
            .insert(key, (EffectSchedule::QueueLatest, effect));
    }

    /// Cancel active and pending work matching this effect key.
    pub fn cancel(&mut self, effect: TuiEffect) {
        self.push(effect, EffectSchedule::Cancel);
    }

    fn push(&mut self, effect: TuiEffect, schedule: EffectSchedule) {
        self.effects.insert(effect.key(), (schedule, effect));
    }

    #[cfg(test)]
    pub fn queued_effect_count(&self) -> usize {
        self.effects.len()
    }

    /// Return whether an open-session effect is queued for the given session.
    #[cfg(test)]
    pub fn has_open_session(&self, session_id: SessionId) -> bool {
        self.effects.values().any(|(_, effect)| {
            matches!(
                effect,
                TuiEffect::OpenSession {
                    session_id: queued,
                    ..
                } if *queued == session_id
            )
        })
    }

    #[cfg(test)]
    pub fn queued_agent_id(&self) -> Option<&str> {
        self.effects.values().find_map(|(_, effect)| match effect {
            TuiEffect::SubmitMessage { request } => request.agent_id.as_deref(),
            TuiEffect::SkillAction { request } => request.agent_id.as_deref(),
            _ => None,
        })
    }

    #[cfg(test)]
    pub fn queued_execution_options(&self) -> Option<&bcode_session_models::TurnExecutionOptions> {
        self.effects.values().find_map(|(_, effect)| match effect {
            TuiEffect::SubmitMessage { request } => Some(&request.execution),
            TuiEffect::SkillAction { request } => Some(&request.execution),
            _ => None,
        })
    }

    /// Return the queued V2 transcript history requests, in lane order.
    #[cfg(test)]
    pub fn queued_transcript_v2_history(
        &self,
    ) -> Vec<super::transcript_v2_host::effects::HistoryLoad> {
        self.effects
            .values()
            .filter_map(|(_, effect)| match effect {
                TuiEffect::LoadTranscriptV2History { load } => Some(*load),
                _ => None,
            })
            .collect()
    }

    /// Return whether a plugin surface-open effect is queued for the given plugin and kind.
    /// Return the projection request queued for one open session.
    #[cfg(test)]
    pub fn open_session_request(&self, session_id: SessionId) -> Option<&ProjectionWindowRequest> {
        self.effects.values().find_map(|(_, effect)| match effect {
            TuiEffect::OpenSession {
                session_id: queued,
                initial_window_request,
                ..
            } if *queued == session_id => Some(initial_window_request),
            _ => None,
        })
    }

    /// Return whether work with the same scheduling key is already queued.
    pub(super) fn contains_effect(&self, effect: &TuiEffect) -> bool {
        self.effects.contains_key(&effect.key())
    }

    /// Drain non-note effects while retaining ordered presentation notes for serialized release.
    pub(super) fn drain_runtime(&mut self) -> (ScheduledEffects, OrderedPresentationEffects) {
        let mut effects = Vec::new();
        let mut notes = BTreeMap::<SessionId, VecDeque<TuiEffect>>::new();
        for (schedule, effect) in std::mem::take(&mut self.effects).into_values() {
            if let TuiEffect::AppendPresentationNote { session_id, .. } = &effect {
                notes.entry(*session_id).or_default().push_back(effect);
            } else {
                effects.push((schedule, effect));
            }
        }
        (effects, notes)
    }
}

const TUI_EFFECT_STREAM_CAPACITY: usize = 64;

impl TuiEffect {
    #[cfg(test)]
    pub(super) fn run_for_test(
        self,
        client: BcodeClient,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TuiEffectResult> + Send>> {
        Box::pin(async move {
            let (streaming_sender, _streaming_receiver) = mpsc::channel(1);
            Box::pin(self.run(client, streaming_sender)).await
        })
    }

    pub(super) fn command_key(&self) -> bmux_tui_runtime::CommandKey {
        let key = match self {
            Self::AppendPresentationNote { session_id, .. } => {
                return bmux_tui_runtime::CommandKey::new(format!(
                    "bcode.effect.AppendPresentationNote({session_id})"
                ));
            }
            _ => self.key(),
        };
        bmux_tui_runtime::CommandKey::new(format!("bcode.effect.{key:?}"))
    }

    pub(super) fn command(
        self,
        schedule: EffectSchedule,
        foreground_client: Option<&BcodeClient>,
        passive_client: Option<&BcodeClient>,
        managed: ManagedEffectAccess,
        handle: bmux_tui_runtime::RuntimeHandle<super::root_program::BcodeRuntimeMessage>,
    ) -> bmux_tui_runtime::Command<super::root_program::BcodeRuntimeMessage> {
        let key = self.command_key();
        if matches!(schedule, EffectSchedule::Cancel) {
            return bmux_tui_runtime::Command::cancel(key);
        }
        let client = match self.daemon_intent() {
            EffectDaemonIntent::Background => passive_client.cloned(),
            EffectDaemonIntent::Foreground => foreground_client.cloned(),
        };
        let future = async move {
            let (streaming_sender, mut streaming_receiver) =
                mpsc::channel(TUI_EFFECT_STREAM_CAPACITY);
            let effect = Box::pin(async move {
                if let Some(commands) = managed.commands {
                    Box::pin(self.run_managed(
                        commands,
                        managed.generation,
                        managed.host,
                        streaming_sender,
                    ))
                    .await
                } else if let Some(client) = client {
                    Box::pin(self.run(client, streaming_sender)).await
                } else {
                    self.application_access_failed(ClientError::Protocol(
                        "No application access for this execution mode".into(),
                    ))
                }
            });
            tokio::pin!(effect);
            loop {
                tokio::select! {
                    result = &mut effect => {
                        break Some(super::root_program::BcodeRuntimeMessage::EffectCompleted(
                            Box::new(result),
                        ));
                    }
                    progress = streaming_receiver.recv() => {
                        let Some(progress) = progress else {
                            continue;
                        };
                        if handle
                            .send(super::root_program::BcodeRuntimeMessage::EffectCompleted(
                                Box::new(progress),
                            ))
                            .await
                            .is_err()
                        {
                            break None;
                        }
                    }
                }
            }
        };
        match schedule {
            EffectSchedule::StartIfIdle => bmux_tui_runtime::Command::start_if_idle(key, future),
            EffectSchedule::Replace => bmux_tui_runtime::Command::replace(key, future),
            EffectSchedule::QueueLatest => bmux_tui_runtime::Command::queue_latest(key, future),
            EffectSchedule::Cancel => bmux_tui_runtime::Command::cancel(key),
        }
    }

    fn key(&self) -> EffectKey {
        match self {
            Self::LoadTranscriptV2History { load } => EffectKey::TranscriptV2History(load.lane()),
            Self::OpenXray { .. } => EffectKey::Xray,
            Self::QueueRecovery { .. } => EffectKey::QueueRecovery,
            Self::OpenSession { .. } => EffectKey::SessionOpen,
            Self::LoadConfig => EffectKey::Config,
            Self::PersistThemeSelection { .. } | Self::ClearThemeSelection => {
                EffectKey::ThemeSelection
            }
            Self::PersistProfileSelection { .. } => EffectKey::ProfileSelection,
            Self::PersistStreamingPresentation { .. } | Self::ClearStreamingPresentation => {
                EffectKey::StreamingPresentation
            }
            Self::PersistPresentationRules { .. } => EffectKey::PresentationRules,
            Self::ReconcileAuthSecurity { .. } => EffectKey::AuthSecurity,
            Self::LoadDraftStatus { .. } => EffectKey::DraftStatus,
            Self::LoadSessionStatus { .. } => EffectKey::SessionStatus,
            Self::LoadSessionModelStatus { .. } => EffectKey::SessionModelStatus,
            Self::LoadPluginStatus { .. } => EffectKey::PluginStatus,
            Self::LoadAgentCatalog => EffectKey::AgentCatalog,
            Self::ListPermissions => EffectKey::PermissionList,
            Self::SaveDraft { .. } => EffectKey::DraftSave,
            Self::LoadSlashPalette { .. } => EffectKey::SlashPalette,
            Self::LoadCommandPalette => EffectKey::CommandPalette,
            Self::LoadSessionChildren { parent_id, .. } => EffectKey::SessionChildren(*parent_id),
            Self::LoadChildActivity { session_id, .. } => EffectKey::ChildActivity(*session_id),
            Self::LoadSessionPicker { .. } => EffectKey::SessionPicker,
            Self::ImportSession { .. } => EffectKey::ImportSession,
            Self::RenameSession { session_id, .. } => EffectKey::RenameSession(*session_id),
            Self::DeleteSession { session_id } => EffectKey::DeleteSession(*session_id),
            Self::SearchSessions { .. } => EffectKey::SearchSessions,
            Self::StartBulkMigration { .. }
            | Self::CancelBulkMigration { .. }
            | Self::WaitBulkMigration { .. } => EffectKey::BulkMigration,
            Self::StartSearchIndexing { .. }
            | Self::CancelSearchIndexing { .. }
            | Self::WaitSearchIndexing { .. } => EffectKey::SearchIndexing,
            Self::ExecuteSlashCommand { .. } => EffectKey::SlashCommand,
            Self::RalphAction { .. } => EffectKey::RalphAction,
            Self::RalphStart { .. } => EffectKey::RalphStart,
            Self::OpenPluginSurface { .. } => EffectKey::PluginSurface,
            Self::AnnounceExternalConnections => EffectKey::ExternalConnectionNotice,
            Self::LoadAuthPoolPicker { .. } => EffectKey::AuthPoolPicker,
            Self::SetAuthPoolPreference { .. } => EffectKey::AuthPoolPreference,
            Self::LoadSkillPicker => EffectKey::SkillPicker,
            Self::DescribeSkill { .. } => EffectKey::SkillDescription,
            Self::LoadThinkingDialog { .. } => EffectKey::ThinkingDialog,
            Self::LoadMilestones { .. } => EffectKey::TimelineJump,
            Self::InvokePluginCommand {
                plugin_id,
                command_id,
                ..
            } => EffectKey::PluginCommand(plugin_id.clone(), command_id.clone()),
            Self::SubmitMessage { request } => {
                EffectKey::SubmitMessage(request.submission_id.clone())
            }
            Self::SkillAction { request } => EffectKey::SkillAction(request.skill_id.clone()),
            Self::SetSessionModel { session_id, .. } => EffectKey::SetSessionModel(*session_id),
            Self::SetSessionReasoning { session_id, .. } => {
                EffectKey::SetSessionReasoning(*session_id)
            }
            Self::AppendPresentationNote {
                session_id,
                note_id,
                ..
            } => EffectKey::AppendPresentationNote(*session_id, note_id.clone()),
            Self::CancelRuntimeWork { session_id, .. } => EffectKey::CancelRuntimeWork(*session_id),
            Self::CompactContext { session_id } => EffectKey::CompactContext(*session_id),
            Self::AttachWorktree { session_id, .. } => EffectKey::AttachWorktree(*session_id),
            Self::CreateWorktree { .. } | Self::WaitWorktreeCreate { .. } => {
                EffectKey::CreateWorktree
            }
            Self::CancelTurn { session_id } => EffectKey::CancelTurn(*session_id),
            Self::ResolvePermission { permission_id, .. } => {
                EffectKey::ResolvePermission(permission_id.clone())
            }
            Self::RespondManagedInteraction {
                generation,
                request_id,
                ..
            } => EffectKey::ManagedRespond(*generation, request_id.clone()),
            Self::LoadManagedQueue | Self::WithdrawManagedQueued { .. } => EffectKey::ManagedQueue,
            Self::ConfigureManagedSession { .. } => EffectKey::ManagedConfigure,
        }
    }

    async fn run_session_status_effect(
        client: &BcodeClient,
        session_id: SessionId,
    ) -> TuiEffectResult {
        Box::pin(load_session_status(client, session_id)).await
    }

    /// Host operations a managed conversation may run through host-only application access.
    ///
    /// None of these submit input, answer native requests, or construct model work; the
    /// managed executor remains reachable only through the sealed command handle.
    pub(crate) const fn is_managed_host_operation(&self) -> bool {
        matches!(
            self,
            Self::LoadSessionChildren { .. }
                | Self::LoadChildActivity { .. }
                | Self::LoadPluginStatus { .. }
                | Self::LoadCommandPalette
                | Self::LoadSessionPicker { .. }
                | Self::RenameSession { .. }
                | Self::DeleteSession { .. }
                | Self::SearchSessions { .. }
                | Self::StartBulkMigration { .. }
                | Self::CancelBulkMigration { .. }
                | Self::WaitBulkMigration { .. }
                | Self::StartSearchIndexing { .. }
                | Self::CancelSearchIndexing { .. }
                | Self::WaitSearchIndexing { .. }
                | Self::LoadMilestones { .. }
                | Self::OpenXray { .. }
                | Self::LoadAgentCatalog
        )
    }

    /// Run purely local configuration/state effects that never contact the daemon.
    fn run_local_state(self) -> Result<TuiEffectResult, Box<Self>> {
        Ok(match self {
            Self::LoadConfig => TuiEffectResult::ConfigLoaded {
                config: Box::new(bcode_config::load_config().map_err(|error| error.to_string())),
                theme_selection: bcode_config::load_tui_theme_selection()
                    .map_err(|error| error.to_string()),
                profile_selection: bcode_config::load_tui_profile_selection()
                    .map_err(|error| error.to_string()),
                streaming_presentation: bcode_config::load_tui_streaming_presentation_override()
                    .map_err(|error| error.to_string()),
            },
            Self::PersistThemeSelection { name } => TuiEffectResult::ThemeSelectionPersisted {
                result: bcode_config::set_tui_theme_selection(&name)
                    .map_err(|error| error.to_string()),
                name: Some(name),
            },
            Self::ClearThemeSelection => TuiEffectResult::ThemeSelectionPersisted {
                result: bcode_config::clear_tui_theme_selection()
                    .map_err(|error| error.to_string()),
                name: None,
            },
            Self::PersistProfileSelection { name } => TuiEffectResult::ProfileSelectionPersisted {
                result: bcode_config::set_tui_profile_selection(name.as_deref())
                    .map_err(|error| error.to_string()),
                name,
            },
            Self::PersistStreamingPresentation { policy } => {
                TuiEffectResult::StreamingPresentationPersisted {
                    result: bcode_config::set_tui_streaming_presentation_override(policy)
                        .map_err(|error| error.to_string()),
                    policy: Some(policy.normalized()),
                }
            }
            Self::ClearStreamingPresentation => TuiEffectResult::StreamingPresentationPersisted {
                result: bcode_config::clear_tui_streaming_presentation_override()
                    .map_err(|error| error.to_string()),
                policy: None,
            },
            Self::PersistPresentationRules { rules } => {
                TuiEffectResult::PresentationRulesPersisted {
                    result: persist_presentation_rules(&rules),
                    rules,
                }
            }
            Self::ReconcileAuthSecurity { config } => TuiEffectResult::AuthSecurityReconciled {
                status: session_flow::auth_security_status(&config),
            },
            other => return Err(Box::new(other)),
        })
    }

    pub(super) async fn run_managed(
        self,
        commands: bcode_client::ManagedSessionCommandHandle,
        generation: Option<u64>,
        host: Option<ManagedHostAccess>,
        streaming_sender: mpsc::Sender<TuiEffectResult>,
    ) -> TuiEffectResult {
        let this = match self.run_local_state() {
            Ok(result) => return result,
            Err(this) => *this,
        };
        if let Self::LoadSlashPalette {
            query, generation, ..
        } = this
        {
            let palette = slash_palette::SlashPalette::managed(
                &query,
                bcode_config::CompletionConfig::default(),
            );
            return TuiEffectResult::SlashPaletteLoaded {
                query,
                palette,
                generation,
            };
        }
        if this.is_managed_host_operation() {
            return match host {
                Some(host) => Box::pin(this.run(host.client(), streaming_sender)).await,
                None => this.application_access_failed(ClientError::Protocol(
                    "Host operations are unavailable in this managed conversation".into(),
                )),
            };
        }
        Box::pin(this.run_managed_session(commands, generation, host)).await
    }

    #[allow(clippy::too_many_lines)]
    async fn run_managed_session(
        self,
        commands: bcode_client::ManagedSessionCommandHandle,
        generation: Option<u64>,
        host: Option<ManagedHostAccess>,
    ) -> TuiEffectResult {
        let session_id = commands.session_id();
        match commands.binding().await {
            Ok(binding) if Some(binding.target.generation()) == generation => {}
            Ok(_) => {
                return self.application_access_failed(ClientError::Protocol(
                    "Managed generation changed; reattach explicitly without replay".into(),
                ));
            }
            Err(error) => return self.application_access_failed(error),
        }
        match self {
            Self::SubmitMessage { request } => {
                Box::pin(managed_submit_message(
                    &commands,
                    host.as_ref(),
                    *request,
                    generation,
                ))
                .await
            }
            Self::CancelTurn { session_id: owner } if owner == session_id => {
                let result = async {
                    let expected = generation.ok_or_else(|| {
                        ClientError::Protocol("Managed generation is missing".into())
                    })?;
                    commands
                        .interrupt(expected, uuid::Uuid::new_v4().to_string())
                        .await?;
                    Ok(true)
                }
                .await;
                TuiEffectResult::CancelTurn { session_id, result }
            }
            Self::ExecuteSlashCommand {
                message,
                session_id: owner,
                working_directory,
                current_agent_id,
                reasoning_display_mode,
                reasoning_visible,
            } if owner == Some(session_id) => {
                Box::pin(managed_slash_command(
                    &commands,
                    message,
                    generation,
                    host.as_ref(),
                    ManagedSlashContext {
                        working_directory,
                        current_agent_id,
                        reasoning_display_mode,
                        reasoning_visible,
                    },
                ))
                .await
            }
            Self::LoadManagedQueue => TuiEffectResult::ManagedQueueLoaded {
                withdrawal: None,
                result: commands.queued_input().await,
            },
            Self::WithdrawManagedQueued {
                submission_id,
                edit,
            } => TuiEffectResult::ManagedQueueLoaded {
                result: commands.withdraw_queued_input(submission_id.clone()).await,
                withdrawal: Some((submission_id, edit)),
            },
            Self::RespondManagedInteraction {
                generation: target,
                request_id,
                response_id,
                value,
            } => {
                let result = if Some(target) == generation {
                    commands
                        .managed_session_respond(
                            target,
                            request_id.clone(),
                            response_id.clone(),
                            value,
                        )
                        .await
                } else {
                    Err(ClientError::Protocol(
                        "Native response generation differs from the attached session; no response sent"
                            .into(),
                    ))
                };
                TuiEffectResult::ManagedInteractionResponded {
                    generation: target,
                    request_id,
                    response_id,
                    result,
                }
            }
            Self::ConfigureManagedSession {
                generation: target,
                mutation_id,
                setting,
            } => {
                let result = if Some(target) == generation {
                    commands
                        .managed_session_configure(target, mutation_id.clone(), setting.clone())
                        .await
                } else {
                    Err(ClientError::Protocol(
                        "Native setting generation differs from the attached session; nothing changed"
                            .into(),
                    ))
                };
                TuiEffectResult::ManagedSessionConfigured {
                    mutation_id,
                    setting,
                    result,
                }
            }
            Self::LoadTranscriptV2History { load } if load.session_id == session_id => {
                TuiEffectResult::TranscriptV2HistoryLoaded {
                    load,
                    result: super::transcript_v2_host::effects::load_history(load, {
                        let commands = &commands;
                        move |query| commands.session_history_page(query)
                    })
                    .await,
                }
            }
            Self::SaveDraft {
                scope: ComposerDraftScope::Session { session_id: owner },
                text,
            } if owner == session_id => {
                let result = commands.set_composer_draft(text.clone()).await;
                TuiEffectResult::SaveDraft { text, result }
            }
            Self::LoadSessionStatus { session_id: owner } if owner == session_id => {
                let binding = commands.binding().await;
                TuiEffectResult::SessionStatusLoaded {
                    daemon_connected: binding.is_ok(),
                    session_id,
                    hydration: Box::new(SessionStatusHydration {
                        model: None,
                        active_skills: None,
                        runtime_work: None,
                        interactions: None,
                        plugin_status: Vec::new(),
                        error: binding.err().map(|error| error.to_string()),
                    }),
                }
            }
            other => other.application_access_failed(ClientError::Protocol(
                "Operation is unavailable in this managed conversation; no model fallback".into(),
            )),
        }
    }

    #[allow(clippy::too_many_lines, clippy::large_stack_frames)]
    async fn run(
        self,
        client: BcodeClient,
        streaming_sender: mpsc::Sender<TuiEffectResult>,
    ) -> TuiEffectResult {
        match self {
            Self::LoadTranscriptV2History { load } => {
                let client = &client;
                TuiEffectResult::TranscriptV2HistoryLoaded {
                    load,
                    result: super::transcript_v2_host::effects::load_history(load, move |query| {
                        client.session_history_page(load.session_id, query)
                    })
                    .await,
                }
            }
            Self::OpenXray { request } => {
                let session_id = request.session_id;
                let message = request.run(client).await;
                TuiEffectResult::XrayOpened {
                    session_id,
                    message,
                }
            }
            Self::QueueRecovery {
                session_id,
                generation,
                action,
            } => {
                let result = async {
                    let outcome = execute_session_view_action(&client, *action).await?;
                    let status = client.queue_status(session_id).await?;
                    Ok((outcome, status))
                }
                .await;
                TuiEffectResult::QueueRecovery {
                    session_id,
                    generation,
                    result: Box::new(result),
                }
            }
            Self::LoadSessionChildren {
                parent_id,
                after,
                generation,
            } => TuiEffectResult::SessionChildrenLoaded {
                parent_id,
                generation,
                result: client
                    .query_child_sessions(bcode_session_models::SessionChildQuery {
                        parent_id,
                        after,
                        limit: 64,
                    })
                    .await,
            },
            Self::LoadChildActivity {
                session_id,
                after,
                generation,
            } => TuiEffectResult::ChildActivityLoaded {
                session_id,
                generation,
                result: client
                    .session_activity(
                        session_id,
                        bcode_session_models::SessionActivityQuery {
                            scope: bcode_session_models::SessionActivityScope::DirectChildren,
                            after,
                            limit: 64,
                        },
                    )
                    .await
                    .map(|mut page| {
                        // A terminal continuation is complete for its query, not for the
                        // currently displayed window of the whole direct-child scope.
                        if after.is_some()
                            && page.coverage
                                == bcode_session_models::SessionActivityCoverage::Complete
                        {
                            page.coverage = bcode_session_models::SessionActivityCoverage::Partial;
                        }
                        page
                    }),
            },
            Self::OpenSession {
                session_id,
                initial_window_request,
                event_sender,
                allow_daemon_start: _,
            } => {
                // A managed session must reattach through the sealed managed route, never as
                // an ordinary model attachment. Binding lookup failures keep the ordinary path.
                if let Ok(Some(binding)) = client.managed_session_binding(session_id).await
                    && binding.validate().is_ok()
                    && binding.session_id == session_id
                {
                    return TuiEffectResult::ManagedSessionDetected {
                        session_id,
                        binding: Box::new(binding),
                    };
                }
                TuiEffectResult::SessionOpened {
                    session_id,
                    result: Box::new(
                        history_flow::attach_session_event_stream_with_window_request(
                            &client,
                            session_id,
                            event_sender,
                            initial_window_request,
                            |snapshot| {
                                let _result = streaming_sender.try_send(
                                    TuiEffectResult::SessionOpenProgress {
                                        snapshot: snapshot.clone(),
                                    },
                                );
                            },
                        )
                        .await,
                    ),
                }
            }
            local @ (Self::LoadConfig
            | Self::PersistThemeSelection { .. }
            | Self::ClearThemeSelection
            | Self::PersistProfileSelection { .. }
            | Self::PersistStreamingPresentation { .. }
            | Self::ClearStreamingPresentation
            | Self::PersistPresentationRules { .. }
            | Self::ReconcileAuthSecurity { .. }) => {
                local.run_local_state().unwrap_or_else(|other| {
                    other.application_access_failed(ClientError::Protocol(
                        "local state effect unavailable".into(),
                    ))
                })
            }
            managed @ (Self::RespondManagedInteraction { .. }
            | Self::LoadManagedQueue
            | Self::WithdrawManagedQueued { .. }
            | Self::ConfigureManagedSession { .. }) => managed.application_access_failed(
                ClientError::Protocol("Native controls require a managed conversation".into()),
            ),
            Self::LoadDraftStatus {
                launch_working_directory,
            } => Box::pin(load_draft_status(&client, launch_working_directory)).await,
            Self::LoadSessionStatus { session_id } => {
                Box::pin(Self::run_session_status_effect(&client, session_id)).await
            }
            Self::LoadSessionModelStatus { session_id } => {
                TuiEffectResult::SessionModelStatusLoaded {
                    session_id,
                    result: client.session_model_status(session_id).await,
                }
            }
            Self::LoadPluginStatus { session_id } => {
                let (plugin_status, error) = load_plugin_session_status(&client, session_id).await;
                TuiEffectResult::PluginStatusLoaded {
                    session_id,
                    plugin_status,
                    error,
                }
            }
            Self::LoadAgentCatalog => TuiEffectResult::AgentCatalogLoaded {
                agents: AgentCatalog::load(&client)
                    .await
                    .map_err(|error| error.to_string()),
            },
            Self::ListPermissions => TuiEffectResult::PermissionList {
                result: client.list_permissions().await,
            },
            Self::SaveDraft { scope, text } => {
                let scope = match scope {
                    ComposerDraftScope::Session { session_id } => {
                        bcode_session_view_models::ComposerDraftViewScope::Session { session_id }
                    }
                    ComposerDraftScope::DraftSession {
                        launch_working_directory,
                    } => bcode_session_view_models::ComposerDraftViewScope::DraftSession {
                        launch_working_directory,
                    },
                };
                let result = execute_session_view_action(
                    &client,
                    SessionViewAction::UpdateDraft {
                        scope,
                        text: text.clone(),
                    },
                )
                .await
                .map(|_| ());
                TuiEffectResult::SaveDraft { text, result }
            }
            Self::LoadSlashPalette {
                query,
                session_id,
                generation,
                provider_plugin_id,
            } => {
                let palette = slash_palette::SlashPalette::discover(
                    &client,
                    session_id,
                    provider_plugin_id,
                    &query,
                )
                .await;
                TuiEffectResult::SlashPaletteLoaded {
                    query,
                    palette,
                    generation,
                }
            }
            Self::LoadCommandPalette => TuiEffectResult::CommandPaletteLoaded {
                result: client
                    .plugin_contributions()
                    .await
                    .map(|contributions| contributions.command_contributions),
            },
            Self::LoadSessionPicker {
                generation,
                scope,
                query,
                cursor,
            } => {
                let result = async {
                    if let bcode_session_models::SessionDiscoveryScope::WorkingDirectory(_) = &scope
                    {
                        let mut watcher = client.watch_session_catalog_with_scope(scope).await?;
                        let mut snapshot = watcher.initial_snapshot().await?;
                        loop {
                            let page = bcode_session_models::SessionCatalogPage {
                                unclassified_origin_count: 0,
                                sessions: snapshot.sessions,
                                catalog_status: snapshot.catalog_status,
                                catalog_sources: snapshot.catalog_sources,
                                catalog_revision: snapshot.catalog_revision,
                                next_cursor: None,
                            };
                            if streaming_sender
                                .send(TuiEffectResult::SessionPickerLoaded {
                                    result: Ok(page),
                                    generation,
                                    restart: false,
                                })
                                .await
                                .is_err()
                            {
                                return Err(ClientError::Protocol(
                                    "session picker receiver closed".into(),
                                ));
                            }
                            snapshot = watcher.next_snapshot().await?;
                        }
                    }
                    let mut watcher = client
                        .watch_session_catalog_with_scope(scope.clone())
                        .await?;
                    let initial = client
                        .list_sessions_page_with_scope(
                            scope.clone(),
                            query.clone(),
                            cursor.clone(),
                            100,
                        )
                        .await?;
                    if streaming_sender
                        .send(TuiEffectResult::SessionPickerLoaded {
                            result: Ok(initial),
                            generation,
                            restart: false,
                        })
                        .await
                        .is_err()
                    {
                        return Err(ClientError::Protocol(
                            "session picker receiver closed".into(),
                        ));
                    }
                    loop {
                        watcher.next_revision().await?;
                        let snapshot = client
                            .list_sessions_page_with_scope(scope.clone(), query.clone(), None, 100)
                            .await?;
                        if streaming_sender
                            .send(TuiEffectResult::SessionPickerLoaded {
                                result: Ok(snapshot),
                                generation,
                                restart: true,
                            })
                            .await
                            .is_err()
                        {
                            return Err(ClientError::Protocol(
                                "session picker receiver closed".into(),
                            ));
                        }
                    }
                }
                .await;
                TuiEffectResult::SessionPickerLoaded {
                    result,
                    generation,
                    restart: false,
                }
            }
            Self::ImportSession {
                source_id,
                external_session_id,
            } => TuiEffectResult::SessionImported {
                result: client
                    .import_external_session(source_id, external_session_id)
                    .await,
            },
            Self::RenameSession { session_id, name } => TuiEffectResult::SessionRenamed {
                result: execute_session_view_action(
                    &client,
                    SessionViewAction::RenameSession { session_id, name },
                )
                .await
                .and_then(|outcome| match outcome {
                    SessionViewActionOutcome::SessionRenamed { session } => Ok(*session),
                    _ => Err(ClientError::UnexpectedResponse),
                }),
            },
            Self::DeleteSession { session_id } => TuiEffectResult::SessionDeleted {
                result: execute_session_view_action(
                    &client,
                    SessionViewAction::DeleteSession { session_id },
                )
                .await
                .and_then(|outcome| match outcome {
                    SessionViewActionOutcome::SessionDeleted { session } => Ok(*session),
                    _ => Err(ClientError::UnexpectedResponse),
                }),
            },
            Self::SearchSessions { request, policy } => TuiEffectResult::SessionsSearched {
                result: client
                    .session_search(*request, policy, Vec::new(), true)
                    .await,
            },
            Self::StartBulkMigration { request } => TuiEffectResult::BulkMigrationUpdated {
                result: client.start_session_bulk_migration(request).await,
            },
            Self::CancelBulkMigration { operation_id } => TuiEffectResult::BulkMigrationUpdated {
                result: client.cancel_session_bulk_migration(operation_id).await,
            },
            Self::WaitBulkMigration {
                operation_id,
                after_revision,
            } => TuiEffectResult::BulkMigrationWaited {
                result: client
                    .wait_session_bulk_migration(operation_id, after_revision, 30_000)
                    .await,
            },
            Self::StartSearchIndexing { request } => TuiEffectResult::SearchIndexingStarted {
                result: client.session_search_index_all_start(request).await,
            },
            Self::CancelSearchIndexing { operation_id } => TuiEffectResult::SearchIndexingUpdated {
                result: client.session_search_backfill_cancel(operation_id).await,
            },
            Self::WaitSearchIndexing {
                operation_id,
                after_revision,
            } => TuiEffectResult::SearchIndexingWaited {
                result: client
                    .session_search_backfill_wait(operation_id, after_revision, 30_000)
                    .await,
            },
            Self::ExecuteSlashCommand {
                session_id,
                working_directory,
                current_agent_id,
                reasoning_display_mode,
                reasoning_visible,
                message,
            } => {
                let result = match super::slash_registry::resolve(&client, &message).await {
                    Ok(resolution) => {
                        super::slash_commands::execute_resolved(
                            &client,
                            session_id,
                            super::slash_commands::SlashExecutionContext {
                                working_directory: &working_directory,
                                current_agent_id: &current_agent_id,
                                reasoning_display_mode,
                                reasoning_visible,
                            },
                            &message,
                            resolution,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                TuiEffectResult::SlashCommandExecuted { message, result }
            }
            Self::RalphAction { repo_root, action } => TuiEffectResult::RalphAction {
                action,
                result: super::ralph_flow::execute_root_action(&client, repo_root, action).await,
            },
            Self::RalphStart { request } => TuiEffectResult::RalphStarted {
                result: super::ralph_flow::execute_root_start(&client, request).await,
            },
            Self::OpenPluginSurface {
                plugin_id,
                surface_kind,
                instance_id,
                options,
                working_directory,
                session_id,
            } => {
                let result = async {
                    let options = super::palette_flow::hydrate_root_plugin_surface_options(
                        &client, session_id, options,
                    )
                    .await;
                    let runtime =
                        bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
                            &bcode_plugin::PluginSelection::all_enabled(),
                            &crate::static_bundled_plugins(),
                        )
                        .map_err(|error| TuiError::PluginService {
                            code: "plugin_runtime_load_failed".to_owned(),
                            message: error.to_string(),
                        })?;
                    crate::plugin_tui::open_plugin_tui_surface(
                        &runtime,
                        &plugin_id,
                        &surface_kind,
                        bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest {
                            instance_id,
                            repo_path: Some(working_directory),
                            session_id,
                            target: None,
                            options,
                        },
                    )
                    .await
                    .map_err(|error| TuiError::PluginService {
                        code: "tui_surface_open_failed".to_owned(),
                        message: error.to_string(),
                    })
                }
                .await;
                TuiEffectResult::PluginSurfaceOpened { plugin_id, result }
            }
            Self::AnnounceExternalConnections => TuiEffectResult::ExternalConnectionsAnnounced {
                result: announce_external_connections(&client).await,
            },
            Self::LoadAuthPoolPicker { session_id } => TuiEffectResult::AuthPoolPickerLoaded {
                session_id,
                result: client.account_routing(session_id).await,
            },
            Self::SetAuthPoolPreference { session_id, action } => {
                let result = match action {
                    super::auth_pool_picker::AccountAction::Route(request) => {
                        client.set_account_routing(session_id, request).await
                    }
                    super::auth_pool_picker::AccountAction::Pause(request) => {
                        client.set_account_pause(session_id, request).await
                    }
                    super::auth_pool_picker::AccountAction::Rename(request) => {
                        client.rename_account(session_id, request).await
                    }
                };
                TuiEffectResult::AuthPoolPreferenceSet { session_id, result }
            }
            Self::LoadSkillPicker => TuiEffectResult::SkillPickerLoaded {
                result: client.list_skills().await,
            },
            Self::DescribeSkill { skill_id } => TuiEffectResult::SkillDescribed {
                skill_id: skill_id.clone(),
                result: client.describe_skill(skill_id).await,
            },
            Self::LoadMilestones { request } => TuiEffectResult::MilestonesLoaded {
                request,
                result: client
                    .session_milestones(
                        request.session_id,
                        bcode_session_models::SessionMilestoneQuery {
                            after_sequence: request.after_sequence,
                            limit: 200,
                        },
                    )
                    .await,
            },
            Self::LoadThinkingDialog { session_id, focus } => {
                let result = match session_id {
                    Some(session_id) => client.session_model_status(session_id).await,
                    None => client.default_model_status().await,
                };
                TuiEffectResult::ThinkingDialogLoaded { focus, result }
            }
            Self::InvokePluginCommand {
                plugin_id,
                command_id,
                arguments,
                working_directory,
                session_id,
            } => {
                let mut args = BTreeMap::new();
                if let Some(arguments) = arguments.filter(|value| !value.is_empty()) {
                    args.insert("arguments".to_owned(), arguments);
                }
                let result = invoke_plugin_command(
                    &client,
                    plugin_id.clone(),
                    command_id,
                    args,
                    working_directory,
                    session_id,
                )
                .await;
                TuiEffectResult::PluginCommandInvoked { plugin_id, result }
            }
            Self::SubmitMessage { request } => {
                Box::pin(run_submit_message(&client, *request)).await
            }
            Self::SkillAction { request } => run_skill_action(&client, *request).await,
            Self::SetSessionModel {
                session_id,
                provider_plugin_id,
                model_id,
                external_auth_binding,
                account_reference,
            } => TuiEffectResult::SetSessionModel {
                session_id,
                provider_plugin_id: provider_plugin_id.clone(),
                model_id: model_id.clone(),
                result: if let Some(account) = account_reference {
                    client
                        .set_session_model_with_account(
                            session_id,
                            provider_plugin_id,
                            model_id,
                            account,
                        )
                        .await
                } else {
                    execute_session_view_action(
                        &client,
                        SessionViewAction::SetModel {
                            session_id,
                            provider_plugin_id,
                            model_id,
                            external_auth_binding,
                        },
                    )
                    .await
                    .map(|_| ())
                },
            },
            Self::SetSessionReasoning {
                session_id,
                effort,
                summary,
                effort_generation,
                status,
            } => TuiEffectResult::SetSessionReasoning {
                session_id,
                effort: effort.clone(),
                effort_generation,
                status,
                result: execute_session_view_action(
                    &client,
                    SessionViewAction::SetReasoning {
                        session_id,
                        effort,
                        summary,
                    },
                )
                .await
                .map(|_| ()),
            },
            Self::AppendPresentationNote {
                session_id,
                source_id,
                note_id,
                text,
                format,
            } => TuiEffectResult::AppendPresentationNote {
                session_id,
                result: client
                    .append_presentation_note(session_id, source_id, note_id, text, format)
                    .await,
            },
            Self::CancelRuntimeWork {
                session_id,
                work_id,
            } => TuiEffectResult::CancelRuntimeWork {
                work_id: work_id.clone(),
                result: match execute_session_view_action(
                    &client,
                    SessionViewAction::CancelRuntimeWork {
                        session_id,
                        work_id,
                    },
                )
                .await
                {
                    Ok(SessionViewActionOutcome::RuntimeWorkCancellationRequested {
                        cancelled,
                    }) => Ok(cancelled),
                    Ok(_) => Err(ClientError::UnexpectedResponse),
                    Err(error) => Err(error),
                },
            },
            Self::CompactContext { session_id } => TuiEffectResult::CompactContext {
                session_id,
                result: match execute_session_view_action(
                    &client,
                    SessionViewAction::CompactContext { session_id },
                )
                .await
                {
                    Ok(SessionViewActionOutcome::ContextCompacted { message }) => Ok(message),
                    Ok(_) => Err(ClientError::UnexpectedResponse),
                    Err(error) => Err(error),
                },
            },
            Self::AttachWorktree { session_id, path } => TuiEffectResult::AttachWorktree {
                path: path.clone(),
                result: match execute_session_view_action(
                    &client,
                    SessionViewAction::ChangeWorkingDirectory { session_id, path },
                )
                .await
                {
                    Ok(SessionViewActionOutcome::WorkingDirectoryChanged { session }) => {
                        Ok(*session)
                    }
                    Ok(_) => Err(ClientError::UnexpectedResponse),
                    Err(error) => Err(error),
                },
            },
            Self::CreateWorktree {
                operation_id,
                request,
            } => TuiEffectResult::CreateWorktree {
                result: client.start_worktree_create(operation_id, request).await,
            },
            Self::WaitWorktreeCreate {
                operation_id,
                after_revision,
            } => TuiEffectResult::CreateWorktree {
                result: client
                    .wait_worktree_create(operation_id, after_revision, 30_000)
                    .await,
            },
            Self::CancelTurn { session_id } => TuiEffectResult::CancelTurn {
                session_id,
                result: match execute_session_view_action(
                    &client,
                    active_turn_cancel_action(session_id),
                )
                .await
                {
                    Ok(SessionViewActionOutcome::Cancelled { cancelled }) => Ok(cancelled),
                    Ok(_) => Err(ClientError::UnexpectedResponse),
                    Err(error) => Err(error),
                },
            },
            Self::ResolvePermission {
                permission_id,
                approved,
                remember,
                apply_to_batch,
                batch_id,
            } => {
                let result = if apply_to_batch {
                    let Some(batch_id) = batch_id else {
                        return TuiEffectResult::PermissionResolved {
                            permission_id,
                            approved,
                            remember,
                            apply_to_batch,
                            result: Err(ClientError::UnexpectedResponse),
                        };
                    };
                    match execute_session_view_action(
                        &client,
                        SessionViewAction::ResolvePermissionBatch { batch_id, approved },
                    )
                    .await
                    {
                        Ok(SessionViewActionOutcome::PermissionBatchResolved {
                            resolved_count,
                        }) => Ok(resolved_count > 0),
                        Ok(_) => Err(ClientError::UnexpectedResponse),
                        Err(error) => Err(error),
                    }
                } else {
                    match execute_session_view_action(
                        &client,
                        SessionViewAction::ResolvePermission {
                            permission_id: permission_id.clone(),
                            approved,
                            remember,
                        },
                    )
                    .await
                    {
                        Ok(SessionViewActionOutcome::PermissionResolved { resolved }) => {
                            Ok(resolved)
                        }
                        Ok(_) => Err(ClientError::UnexpectedResponse),
                        Err(error) => Err(error),
                    }
                };
                TuiEffectResult::PermissionResolved {
                    permission_id,
                    approved,
                    remember,
                    apply_to_batch,
                    result,
                }
            }
        }
    }
}

async fn ensure_session_for_foreground_action(
    client: &BcodeClient,
    session_id: Option<SessionId>,
    launch_working_directory: std::path::PathBuf,
    event_sender: mpsc::Sender<super::history_flow::SessionStreamUpdate>,
) -> Result<
    (
        SessionId,
        Option<SessionSummary>,
        Option<JoinHandle<()>>,
        Option<oneshot::Sender<()>>,
    ),
    ClientError,
> {
    if let Some(session_id) = session_id {
        return Ok((session_id, None, None, None));
    }
    let session = client
        .create_session_in_working_directory(None, launch_working_directory.clone())
        .await?;
    let _ = execute_session_view_action(
        client,
        SessionViewAction::UpdateDraft {
            scope: bcode_session_view_models::ComposerDraftViewScope::DraftSession {
                launch_working_directory,
            },
            text: String::new(),
        },
    )
    .await;
    let (attached, task, release) =
        history_flow::attach_paused_session_event_stream(client, session.id, event_sender)
            .await
            .map_err(|error| match error {
                TuiError::Client(error) => error,
                other => ClientError::Server {
                    code: "tui_session_attach_failed".to_owned(),
                    message: other.to_string(),
                },
            })?;
    Ok((
        session.id,
        Some(attached.session),
        Some(task),
        Some(release),
    ))
}

async fn run_skill_action(client: &BcodeClient, request: SkillActionRequest) -> TuiEffectResult {
    let action = request.action;
    let skill_id = request.skill_id.clone();
    TuiEffectResult::SkillAction {
        action,
        skill_id,
        result: Box::new(skill_action(client, request).await),
    }
}

async fn skill_action(
    client: &BcodeClient,
    request: SkillActionRequest,
) -> Result<SkillActionResult, ClientError> {
    let SkillActionRequest {
        session_id,
        launch_working_directory,
        skill_id,
        action,
        arguments,
        provider_plugin_id,
        model_id,
        agent_id,
        reasoning_effort,
        reasoning_summary,
        reasoning_effort_generation,
        mut execution,
        event_sender,
    } = request;
    execution.reasoning = Some(Box::new(bcode_session_models::TurnReasoningOptions {
        effort: reasoning_effort.clone(),
        summary: reasoning_summary.clone(),
    }));
    execution.agent_profile.clone_from(&agent_id);
    let (session_id, created_session, event_task, event_stream_release) =
        ensure_session_for_foreground_action(
            client,
            session_id,
            launch_working_directory,
            event_sender,
        )
        .await?;
    let acceptance = match action {
        SkillActionKind::Activate => {
            execute_session_view_action(
                client,
                SessionViewAction::ActivateSkill {
                    session_id,
                    skill_id: skill_id.to_string(),
                },
            )
            .await?;
            None
        }
        SkillActionKind::Deactivate => {
            execute_session_view_action(
                client,
                SessionViewAction::DeactivateSkill {
                    session_id,
                    skill_id: skill_id.to_string(),
                },
            )
            .await?;
            None
        }
        SkillActionKind::Invoke => {
            apply_submit_runtime_selections(
                client,
                session_id,
                provider_plugin_id,
                model_id,
                None,
                reasoning_effort.clone(),
                reasoning_summary.clone(),
            )
            .await?;
            let display_text = if arguments.trim().is_empty() {
                format!("Invoke skill {skill_id}")
            } else {
                format!("Invoke skill {skill_id}: {arguments}")
            };
            let outcome = execute_session_view_action(
                client,
                SessionViewAction::InvokeSkill {
                    session_id,
                    skill_id: skill_id.to_string(),
                    arguments,
                    display_text,
                    execution: Box::new(execution),
                },
            )
            .await?;
            Some(message_acceptance_from_action_outcome(&outcome)?)
        }
    };
    Ok(SkillActionResult {
        session_id,
        created_session,
        event_task,
        acceptance,
        committed_agent_id: (action == SkillActionKind::Invoke)
            .then_some(agent_id)
            .flatten(),
        committed_reasoning_effort_generation: (action == SkillActionKind::Invoke)
            .then_some(reasoning_effort_generation)
            .flatten(),
        event_stream_release,
    })
}

async fn run_submit_message(
    client: &BcodeClient,
    request: SubmitMessageRequest,
) -> TuiEffectResult {
    let message = request.message.clone();
    TuiEffectResult::SubmitMessage {
        submission_id: request.submission_id.clone(),
        message,
        result: Box::new(Box::pin(submit_message(client, request)).await),
    }
}

pub async fn apply_submit_runtime_selections(
    client: &BcodeClient,
    session_id: SessionId,
    provider_plugin_id: Option<String>,
    model_id: Option<String>,
    agent_id: Option<String>,
    reasoning_effort: Option<String>,
    reasoning_summary: Option<String>,
) -> Result<(), ClientError> {
    if let Some(model_id) = model_id {
        execute_session_view_action(
            client,
            SessionViewAction::SetModel {
                session_id,
                provider_plugin_id,
                model_id,
                external_auth_binding: None,
            },
        )
        .await?;
    }
    if let Some(agent_id) = agent_id {
        execute_session_view_action(
            client,
            SessionViewAction::SetAgent {
                session_id,
                agent_id,
            },
        )
        .await?;
    }
    execute_session_view_action(
        client,
        SessionViewAction::SetReasoning {
            session_id,
            effort: reasoning_effort,
            summary: reasoning_summary,
        },
    )
    .await?;
    Ok(())
}

fn message_acceptance_from_action_outcome(
    outcome: &SessionViewActionOutcome,
) -> Result<MessageAcceptance, ClientError> {
    let SessionViewActionOutcome::MessageAccepted {
        queued,
        queue_position,
        disposition,
        ..
    } = outcome
    else {
        return Err(ClientError::UnexpectedResponse);
    };
    Ok(MessageAcceptance {
        queued: *queued,
        queue_position: queue_position.and_then(|position| u32::try_from(position).ok()),
        disposition: match disposition {
            bcode_session_view_models::MessageAcceptanceDispositionView::AppliedSteering => {
                bcode_ipc::MessageAcceptanceDisposition::AppliedSteering
            }
            bcode_session_view_models::MessageAcceptanceDispositionView::QueuedFollowUp => {
                bcode_ipc::MessageAcceptanceDisposition::QueuedFollowUp
            }
            bcode_session_view_models::MessageAcceptanceDispositionView::QueuedTurn => {
                bcode_ipc::MessageAcceptanceDisposition::QueuedTurn
            }
            bcode_session_view_models::MessageAcceptanceDispositionView::StartedTurn => {
                bcode_ipc::MessageAcceptanceDisposition::StartedTurn
            }
        },
    })
}

#[allow(clippy::too_many_lines)]
fn submission_stage_error(stage: &'static str, error: ClientError) -> ClientError {
    match error {
        ClientError::RequestTimeout { timeout } => ClientError::Server {
            code: "tui_submission_request_timeout".into(),
            message: format!(
                "Message submission timed out while {stage} after {timeout:?}. Acceptance is not confirmed; check session history before retrying."
            ),
        },
        other => other,
    }
}

#[cfg(test)]
mod submission_error_tests {
    use super::*;

    #[test]
    fn timeout_reports_stage_without_claiming_rejection_or_restart() {
        let error = submission_stage_error(
            "waiting for message acceptance",
            ClientError::RequestTimeout {
                timeout: std::time::Duration::from_secs(30),
            },
        );
        let ClientError::Server { code, message } = error else {
            panic!("stage diagnostic");
        };
        assert_eq!(code, "tui_submission_request_timeout");
        assert!(message.contains("waiting for message acceptance"));
        assert!(message.contains("Acceptance is not confirmed"));
        let startup = submission_stage_error(
            "creating the session",
            ClientError::DaemonStartupTimeout {
                timeout: std::time::Duration::from_secs(10),
            },
        );
        assert!(matches!(startup, ClientError::DaemonStartupTimeout { .. }));
    }
}

fn submission_attach_error(error: TuiError) -> ClientError {
    match error {
        TuiError::Client(error) => {
            submission_stage_error("attaching the session event stream", error)
        }
        other => ClientError::Server {
            code: "tui_session_attach_failed".into(),
            message: other.to_string(),
        },
    }
}

async fn require_model_submission_target(
    client: &BcodeClient,
    session_id: bcode_session_models::SessionId,
) -> Result<(), ClientError> {
    if client.managed_session_binding(session_id).await?.is_some() {
        return Err(ClientError::Protocol(
            "Managed external session: use /native status to inspect the exact prepared/confirmed target, then /native send <generation> <submission-id> <attempt-id> <text>; model/agent/reasoning selections are not applied to native execution".into(),
        ));
    }
    Ok(())
}

fn validate_managed_composer_request(request: &SubmitMessageRequest) -> Result<(), ClientError> {
    if let Some(retained) = request.managed.as_ref() {
        let bcode_session_models::ExternalHarnessRequestKind::Submit { submission, .. } =
            &retained.kind
        else {
            return Err(ClientError::Protocol(
                "Invalid managed composer request".into(),
            ));
        };
        if Some(
            submission
                .session_id
                .parse::<SessionId>()
                .map_err(|_| ClientError::Protocol("Invalid managed session".into()))?,
        ) != request.session_id
            || submission.submission_id != request.submission_id.0
            || submission.attempt_id != request.attempt_id
            || submission.text != request.message
        {
            return Err(ClientError::Protocol(
                "Managed composer request changed before dispatch".into(),
            ));
        }
    }
    Ok(())
}

async fn prepare_composer_session(
    client: &BcodeClient,
    request: &mut SubmitMessageRequest,
) -> Result<SubmitMessageResult, ClientError> {
    if let Some(session_id) = request.session_id {
        return Ok(SubmitMessageResult::empty(session_id));
    }
    if request.managed_only {
        return Err(ClientError::Protocol(
            "Create and attach a managed session before native input; no model session was created"
                .into(),
        ));
    }
    let session = client
        .create_session_in_working_directory(None, request.launch_working_directory.clone())
        .await
        .map_err(|error| submission_stage_error("creating the session", error))?;
    let _ = execute_session_view_action(
        client,
        SessionViewAction::UpdateDraft {
            scope: bcode_session_view_models::ComposerDraftViewScope::DraftSession {
                launch_working_directory: request.launch_working_directory.clone(),
            },
            text: String::new(),
        },
    )
    .await;
    let (attached, task, release) = history_flow::attach_paused_session_event_stream(
        client,
        session.id,
        request.event_sender.clone(),
    )
    .await
    .map_err(submission_attach_error)?;
    let session_id = session.id;
    request.message = clipboard_image::promote_draft_clipboard_images(
        &request.message,
        &request.launch_working_directory,
        session_id,
    )
    .map_err(|error| ClientError::Server {
        code: "tui_clipboard_image_promotion_failed".to_owned(),
        message: error.to_string(),
    })?;
    request.session_id = Some(session_id);
    let mut result = SubmitMessageResult::empty(session_id);
    result.created_session = Some(attached.session);
    result.event_task = Some(task);
    result.event_stream_release = Some(release);
    Ok(result)
}

async fn submit_message(
    client: &BcodeClient,
    mut request: SubmitMessageRequest,
) -> Result<SubmitMessageResult, ClientError> {
    validate_managed_composer_request(&request)?;
    let mut result = Box::pin(prepare_composer_session(client, &mut request)).await?;
    if let Some(retained) = request.managed.take() {
        let response = client.submit_managed_composer_request(retained).await?;
        result.receipt = Some(ComposerReceipt::Managed(Box::new(response)));
        return Ok(result);
    }
    let prepared = client
        .prepare_managed_composer_submission(
            result.session_id,
            (request.submission_id.0.clone(), request.attempt_id.clone()),
            request.message.clone(),
            request.placement == PromptPlacement::Steering,
            request.expected_native_turn_id.clone(),
        )
        .await?;
    if let Some(managed) = prepared {
        request.managed = Some(managed);
        request.managed_only = true;
        result.prepared_request = Some(Box::new(request));
        return Ok(result);
    }
    if request.managed_only {
        return Err(ClientError::Protocol(
            "Managed native target unavailable; no model fallback".into(),
        ));
    }
    Box::pin(submit_model_composer(client, request, result)).await
}

async fn submit_model_composer(
    client: &BcodeClient,
    mut request: SubmitMessageRequest,
    mut result: SubmitMessageResult,
) -> Result<SubmitMessageResult, ClientError> {
    require_model_submission_target(client, result.session_id).await?;
    apply_submit_runtime_selections(
        client,
        result.session_id,
        request.provider_plugin_id,
        request.model_id,
        None,
        request.reasoning_effort.clone(),
        request.reasoning_summary.clone(),
    )
    .await
    .map_err(|error| submission_stage_error("applying model/agent/reasoning selections", error))?;
    request.execution.reasoning = Some(Box::new(bcode_session_models::TurnReasoningOptions {
        effort: request.reasoning_effort,
        summary: request.reasoning_summary,
    }));
    request
        .execution
        .agent_profile
        .clone_from(&request.agent_id);
    let outcome = execute_session_view_action(
        client,
        SessionViewAction::SubmitMessage {
            submission_id: request.submission_id,
            session_id: Some(result.session_id),
            launch_working_directory: None,
            text: request.message,
            placement: request.placement,
            execution: Box::new(request.execution),
        },
    )
    .await
    .map_err(|error| submission_stage_error("waiting for message acceptance", error))?;
    let receipt = submission_receipt_from_action(outcome)?;
    result.submission = client
        .get_submission(result.session_id, receipt.submission_id.clone())
        .await;
    result.receipt = Some(ComposerReceipt::Model(receipt));
    result.committed_agent_id = request.agent_id;
    result.committed_reasoning_effort_generation = request.reasoning_effort_generation;
    Ok(result)
}

fn submission_receipt_from_action(
    outcome: SessionViewActionOutcome,
) -> Result<bcode_session_models::SubmissionReceipt, ClientError> {
    match outcome {
        SessionViewActionOutcome::SubmissionAccepted { receipt, .. } => Ok(receipt),
        _ => Err(ClientError::UnexpectedResponse),
    }
}

async fn optional_client_result<T>(
    future: impl std::future::Future<Output = Result<T, ClientError>>,
) -> (Option<T>, Option<String>) {
    match future.await {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(error.to_string())),
    }
}

async fn load_draft_status(
    client: &BcodeClient,
    launch_working_directory: std::path::PathBuf,
) -> TuiEffectResult {
    let (model, model_error) =
        Box::pin(optional_client_result(client.default_model_status())).await;
    let draft_scope = ComposerDraftScope::DraftSession {
        launch_working_directory,
    };
    let (composer_draft, draft_error) =
        Box::pin(optional_client_result(client.composer_draft(draft_scope))).await;
    TuiEffectResult::DraftStatusLoaded {
        daemon_connected: model.is_some() || composer_draft.is_some(),
        model,
        composer_draft: composer_draft.flatten(),
        error: model_error.or(draft_error),
    }
}

async fn load_plugin_session_status(
    client: &BcodeClient,
    session_id: SessionId,
) -> (
    Vec<bcode_session_view_models::PluginStatusView>,
    Option<String>,
) {
    let services = match client.plugin_services().await {
        Ok(services) => services,
        Err(error) => return (Vec::new(), Some(error.to_string())),
    };
    let mut contributions = Vec::new();
    let mut first_error = None;
    for service in services
        .into_iter()
        .filter(|service| service.interface_id == bcode_plugin_sdk::SESSION_STATUS_INTERFACE_ID)
    {
        let plugin_id = service.plugin_id.clone();
        let payload =
            match serde_json::to_vec(&bcode_plugin_sdk::SessionStatusRequest { session_id }) {
                Ok(payload) => payload,
                Err(error) => {
                    first_error.get_or_insert_with(|| error.to_string());
                    continue;
                }
            };
        match client
            .invoke_plugin_service(
                service.plugin_id,
                bcode_plugin_sdk::SESSION_STATUS_INTERFACE_ID.to_owned(),
                bcode_plugin_sdk::OP_SESSION_STATUS.to_owned(),
                payload,
            )
            .await
        {
            Ok(response) if response.error.is_none() => {
                match serde_json::from_slice::<bcode_plugin_sdk::SessionStatusResponse>(
                    &response.payload,
                ) {
                    Ok(response) => {
                        contributions.extend(response.contribution.map(|contribution| {
                            bcode_session_view_models::PluginStatusView {
                                plugin_id: plugin_id.clone(),
                                note_id: contribution.contribution_id,
                                text: contribution.text,
                                priority: contribution.priority,
                                metadata: contribution.metadata,
                            }
                        }));
                    }
                    Err(error) => {
                        first_error.get_or_insert_with(|| error.to_string());
                    }
                }
            }
            Ok(response) => {
                if let Some(error) = response.error {
                    first_error.get_or_insert(error.message);
                }
            }
            Err(error) if error.is_optional_domain_unavailable() => {}
            Err(error) => {
                first_error.get_or_insert_with(|| error.to_string());
            }
        }
    }
    contributions.sort_by_key(|contribution| contribution.priority);
    (contributions, first_error)
}

#[cfg(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-workflow-plugin"
))]
fn interaction_adapter_for_exchange(
    producer_id: &str,
    schema: &str,
    schema_version: u32,
    platform_id: &str,
) -> Option<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability> {
    bcode_bundled_plugins::interaction_adapter(producer_id, schema, schema_version, platform_id)
}

#[cfg(not(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-workflow-plugin"
)))]
const fn interaction_adapter_for_exchange(
    _producer_id: &str,
    _schema: &str,
    _schema_version: u32,
    _platform_id: &str,
) -> Option<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability> {
    None
}

/// Interaction kinds of pending tool exchanges from the bundled plugins' interaction adapters.
pub fn interaction_kind_resolver() -> bcode_session_view::follow::InteractionKindResolver {
    std::sync::Arc::new(|producer_id: &str, schema: &str, schema_version: u32| {
        interaction_adapter_for_exchange(producer_id, schema, schema_version, "tui")
            .map(|adapter| adapter.interaction_kind)
    })
}

pub async fn load_pending_interactions(
    client: &BcodeClient,
    session_id: SessionId,
) -> Result<Vec<bcode_session_view_models::InteractionViewSummary>, ClientError> {
    let resolver = interaction_kind_resolver();
    bcode_session_view::follow::load_pending_interactions(
        client,
        session_id,
        Some(resolver.as_ref()),
    )
    .await
}

const fn active_turn_cancel_action(session_id: SessionId) -> SessionViewAction {
    SessionViewAction::CancelTurn {
        session_id,
        clear_queue: false,
    }
}

async fn load_session_status(client: &BcodeClient, session_id: SessionId) -> TuiEffectResult {
    let (model, model_error) = Box::pin(optional_client_result(
        client.session_model_status(session_id),
    ))
    .await;
    let (
        (active_skills, skills_error),
        (runtime_work, runtime_work_error),
        (interactions, interactions_error),
        (plugin_status, plugin_error),
    ) = tokio::join!(
        optional_client_result(client.active_skills(session_id)),
        optional_client_result(client.list_runtime_work(session_id)),
        optional_client_result(load_pending_interactions(client, session_id)),
        load_plugin_session_status(client, session_id),
    );
    TuiEffectResult::SessionStatusLoaded {
        daemon_connected: model.is_some() || active_skills.is_some() || runtime_work.is_some(),
        session_id,
        hydration: Box::new(SessionStatusHydration {
            model,
            active_skills,
            runtime_work,
            interactions,
            plugin_status,
            error: model_error
                .or(skills_error)
                .or(runtime_work_error)
                .or(interactions_error)
                .or(plugin_error),
        }),
    }
}

#[cfg(test)]
mod projection_paging_tests {
    #[test]
    fn active_turn_cancel_preserves_all_pending_queue_items() {
        let session_id = SessionId::new();
        assert_eq!(
            active_turn_cancel_action(session_id),
            SessionViewAction::CancelTurn {
                session_id,
                clear_queue: false
            }
        );
    }

    use super::*;

    struct CancellationProbe {
        handle: Option<
            bmux_tui_runtime::RuntimeHandle<super::super::root_program::BcodeRuntimeMessage>,
        >,
        phase: u8,
        cancelled: std::sync::Arc<tokio::sync::Notify>,
    }

    impl bmux_tui_runtime::Program for CancellationProbe {
        type Message = super::super::root_program::BcodeRuntimeMessage;
        type Error = std::convert::Infallible;

        fn update(
            &mut self,
            event: bmux_tui_runtime::RuntimeEvent<Self::Message>,
        ) -> Result<bmux_tui_runtime::Update<Self::Message>, Self::Error> {
            use bmux_tui_runtime::{Command, Update};
            let effect = TuiEffect::LoadSessionPicker {
                generation: 1,
                scope: bcode_session_models::SessionDiscoveryScope::AllWorkingDirectories,
                query: bcode_session_models::SessionCatalogQuery::default(),
                cursor: None,
            };
            let update = match self.phase {
                0 => {
                    let bmux_tui_runtime::RuntimeEvent::Message(Self::Message::Bootstrap {
                        handle,
                    }) = event
                    else {
                        panic!("expected bootstrap");
                    };
                    self.handle = Some(handle.clone());
                    let cancelled = std::sync::Arc::clone(&self.cancelled);
                    Update::none().with_command(Command::start_if_idle(
                        effect.command_key(),
                        async move {
                            struct NotifyOnDrop(std::sync::Arc<tokio::sync::Notify>);
                            impl Drop for NotifyOnDrop {
                                fn drop(&mut self) {
                                    self.0.notify_one();
                                }
                            }
                            let _guard = NotifyOnDrop(cancelled);
                            handle
                                .send(Self::Message::PluginSurfaceInvalidated)
                                .await
                                .unwrap();
                            std::future::pending().await
                        },
                    ))
                }
                1 => {
                    let client = BcodeClient::default_endpoint();
                    let handle = self.handle.as_ref().unwrap().clone();
                    let cancelled = std::sync::Arc::clone(&self.cancelled);
                    Update::none()
                        .with_command(effect.command(
                            EffectSchedule::Cancel,
                            Some(&client),
                            Some(&client),
                            ManagedEffectAccess::default(),
                            handle,
                        ))
                        .with_command(Command::concurrent(async move {
                            cancelled.notified().await;
                            Some(Self::Message::PluginSurfaceInvalidated)
                        }))
                }
                _ => Update::exit(),
            };
            self.phase += 1;
            Ok(update)
        }
    }

    #[tokio::test]
    async fn cancellation_command_stops_active_session_picker_work() {
        let (runtime, handle) = bmux_tui_runtime::Runtime::new(
            CancellationProbe {
                handle: None,
                phase: 0,
                cancelled: std::sync::Arc::new(tokio::sync::Notify::new()),
            },
            bmux_tui_runtime::HeadlessPresenter::default(),
            bmux_tui_runtime::RuntimeConfig::default(),
        );
        assert!(
            handle
                .try_send(super::super::root_program::BcodeRuntimeMessage::Bootstrap {
                    handle: handle.clone(),
                })
                .is_ok()
        );
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), runtime.run())
            .await
            .expect("cancellation must drop active picker work");
        let output = outcome.unwrap_or_else(|_| panic!("probe runtime failed"));
        assert_eq!(output.program.phase, 3);
        assert_eq!(output.stats.commands_started, 2);
        assert_eq!(output.stats.commands_cancelled, 1);
    }

    struct CommandOwner {
        outcome: bcode_command::application::CommandInvocationOutcome,
        requests: std::sync::Mutex<Vec<bcode_command::application::CommandInvocationRequest>>,
    }

    impl bcode_command::application::CommandApplication for CommandOwner {
        fn list_commands(
            &self,
            _plugin_id: String,
        ) -> impl std::future::Future<
            Output = Result<
                bcode_command::CommandList,
                bcode_command::application::CommandApplicationFailure,
            >,
        > + Send {
            std::future::ready(Err(bcode_command::application::CommandApplicationFailure {
                code: "unexpected_discovery".to_owned(),
                message: "invocation must not rediscover commands".to_owned(),
            }))
        }

        fn invoke_command(
            &self,
            request: bcode_command::application::CommandInvocationRequest,
        ) -> impl std::future::Future<Output = bcode_command::application::CommandInvocationResult> + Send
        {
            let invocation_id = request.invocation_id.clone();
            self.requests.lock().unwrap().push(request);
            std::future::ready(bcode_command::application::CommandInvocationResult {
                invocation_id,
                outcome: self.outcome.clone(),
            })
        }

        fn cancel_command(
            &self,
            _invocation_id: String,
        ) -> impl std::future::Future<
            Output = Result<
                bcode_command::application::CommandCancellationAcknowledgment,
                bcode_command::application::CommandApplicationFailure,
            >,
        > + Send {
            std::future::ready(Err(bcode_command::application::CommandApplicationFailure {
                code: "unexpected_cancellation".to_owned(),
                message: "settled or unknown invocation must not be silently cancelled".to_owned(),
            }))
        }
    }

    #[tokio::test]
    async fn command_adapter_routes_context_through_owner_and_preserves_failed_response() {
        let response = bcode_command::InvokeCommandResponse {
            success: false,
            message: Some("plugin rejected command".to_owned()),
            updated_model: None,
            updated_provider: None,
            updated_thinking: None,
            effects: vec![bcode_command::CommandEffect::CopyText {
                text: "literal".to_owned(),
            }],
        };
        let owner = CommandOwner {
            outcome: bcode_command::application::CommandInvocationOutcome::Completed(
                response.clone(),
            ),
            requests: std::sync::Mutex::new(Vec::new()),
        };
        let root = std::path::PathBuf::from("/command-workspace");
        let session_id = SessionId::new();
        let args = BTreeMap::from([("arguments".to_owned(), "literal arguments".to_owned())]);
        for _ in 0..2 {
            let result = invoke_plugin_command(
                &owner,
                "plugin".to_owned(),
                "command".to_owned(),
                args.clone(),
                root.clone(),
                Some(session_id),
            )
            .await
            .unwrap();
            assert_eq!(result, response);
        }
        let requests = owner.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_ne!(requests[0].invocation_id, requests[1].invocation_id);
        for request in requests.iter() {
            assert!(uuid::Uuid::parse_str(&request.invocation_id).is_ok());
            assert_eq!(request.plugin_id, "plugin");
            assert_eq!(request.agent_id, None);
            assert_eq!(request.launch_root.as_ref(), Some(&root));
            assert_eq!(request.deadline_ms, 30_000);
            assert_eq!(request.command.command_id, "command");
            assert_eq!(request.command.args, args);
            let context = request.command.context.as_ref().unwrap();
            assert_eq!(context.session_id, Some(session_id));
            assert_eq!(context.working_directory, root);
        }
    }

    #[tokio::test]
    async fn command_adapter_does_not_retry_failed_or_unknown_outcomes() {
        use bcode_command::application::{CommandApplicationFailure, CommandInvocationOutcome};
        let failure = CommandApplicationFailure {
            code: "command_unavailable".to_owned(),
            message: "owner response unavailable".to_owned(),
        };
        for unknown in [false, true] {
            let owner = CommandOwner {
                outcome: if unknown {
                    CommandInvocationOutcome::Unknown(failure.clone())
                } else {
                    CommandInvocationOutcome::Failed(failure.clone())
                },
                requests: std::sync::Mutex::new(Vec::new()),
            };
            let error = invoke_plugin_command(
                &owner,
                "plugin".to_owned(),
                "command".to_owned(),
                BTreeMap::new(),
                std::path::PathBuf::from("/command-workspace"),
                None,
            )
            .await
            .unwrap_err();
            let TuiError::PluginService { code, message } = error else {
                panic!("expected normalized command error")
            };
            assert_eq!(code, failure.code);
            if unknown {
                assert!(message.contains("outcome unknown"));
                assert!(message.contains("Effects may have occurred"));
                assert!(message.contains("retry is not known safe"));
            } else {
                assert_eq!(message, failure.message);
            }
            assert_eq!(owner.requests.lock().unwrap().len(), 1);
        }
    }
    use bcode_session_models::{CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId};

    fn queue_test_hello(executable_digest: &str) -> bcode_ipc::ResponsePayload {
        bcode_ipc::ResponsePayload::Hello {
            protocol_version: bcode_ipc::ProtocolVersion::current(),
            client_id: ClientId::new(),
            daemon: bcode_ipc::DaemonStatus {
                namespace: bcode_ipc::daemon_namespace(),
                protocol_version: u32::from(bcode_ipc::ProtocolVersion::current().0),
                artifact_id: Some(bcode_ipc::ArtifactId::current()),
                executable_digest: Some(executable_digest.to_owned()),
                build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.to_owned(),
                storage_writer_epoch: Some(
                    bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH,
                ),
                session_event_schema_version: Some(CURRENT_SESSION_EVENT_SCHEMA_VERSION),
                state_location_id: Some(bcode_ipc::state_location_id()),
                ..bcode_ipc::DaemonStatus::default()
            },
        }
    }
    async fn respond_to_prepare(
        listener: bcode_ipc::LocalIpcListener,
        session_id: SessionId,
        submission_id: bcode_session_models::SubmissionId,
        confirmation: bcode_session_models::SubmissionResumeConfirmation,
        executable_digest: String,
    ) {
        let mut prepared = false;
        let mut finished = false;
        while !finished {
            let mut stream = listener.accept().await.unwrap();
            loop {
                let Ok(envelope) = bcode_ipc::recv_envelope(&mut stream).await else {
                    break;
                };
                let request = bcode_ipc::decode_request(&envelope.payload).unwrap();
                let response = match request {
                    bcode_ipc::Request::Hello { .. } => queue_test_hello(&executable_digest),
                    bcode_ipc::Request::PrepareSubmissionResume {
                        session_id: requested,
                        submission_id: requested_id,
                        expected_accepted_sequence,
                    } => {
                        assert_eq!(requested, session_id);
                        assert_eq!(requested_id, submission_id);
                        assert_eq!(expected_accepted_sequence, 17);
                        assert!(!prepared);
                        prepared = true;
                        bcode_ipc::ResponsePayload::SubmissionResumePrepared {
                            confirmation: confirmation.clone(),
                        }
                    }
                    bcode_ipc::Request::SubmissionQueueStatus {
                        session_id: requested,
                    } => {
                        assert!(prepared);
                        assert_eq!(requested, session_id);
                        finished = true;
                        bcode_ipc::ResponsePayload::SubmissionQueueStatus {
                            status: bcode_session_models::SubmissionQueueStatus {
                                oldest: None,
                                blocker: None,
                            },
                        }
                    }
                    other => panic!("prepare must not mutate or scan history: {other:?}"),
                };
                let reply = bcode_ipc::response_envelope(
                    envelope.request_id,
                    &bcode_ipc::Response::Ok(response),
                )
                .unwrap();
                bcode_ipc::send_envelope(&mut stream, &reply).await.unwrap();
                if finished {
                    break;
                }
            }
        }
    }

    #[tokio::test]
    async fn targeted_prepare_dispatches_read_only_identity_then_bounded_status() {
        use bcode_session_models::*;
        let session_id = SessionId::new();
        let submission_id = SubmissionId("exact-pending".to_owned());
        let confirmation = SubmissionResumeConfirmation {
            version: 1,
            submission_id: submission_id.clone(),
            expected_accepted_sequence: 17,
            descriptor: SubmissionExecutionDescriptor {
                version: 2,
                provider: "provider".to_owned(),
                model: "model".to_owned(),
                reasoning: None,
                model_profile: Some("selected-profile".to_owned()),
                auth_profile: None,
                config_identity: "digest".to_owned(),
                recovery_recipe: None,
            },
        };
        let expected = confirmation.clone();
        let socket_dir = tempfile::tempdir().unwrap();
        let endpoint =
            bcode_ipc::IpcEndpoint::unix_socket(socket_dir.path().join("ipc/queue.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        let executable_digest = bcode_daemon_lifecycle::current_executable_identity()
            .expect("test executable identity")
            .1;
        let responder = tokio::spawn(respond_to_prepare(
            listener,
            session_id,
            submission_id,
            confirmation,
            executable_digest,
        ));
        let effect = TuiEffect::QueueRecovery {
            session_id,
            generation: 0,
            action: Box::new(SessionViewAction::PrepareSubmissionResume {
                session_id,
                submission_id: expected.submission_id.clone(),
                expected_accepted_sequence: 17,
            }),
        };
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            effect.run(BcodeClient::new(endpoint), sender),
        )
        .await
        .unwrap();
        responder.await.unwrap();
        let TuiEffectResult::QueueRecovery { result, .. } = result else {
            panic!("expected recovery result");
        };
        let Ok((SessionViewActionOutcome::SubmissionResumePrepared { confirmation }, status)) =
            *result
        else {
            panic!("expected read-only prepared confirmation");
        };
        assert_eq!(confirmation, expected);
        assert!(status.oldest.is_none());
    }
}

#[cfg(test)]
mod progress_routing_tests {
    use super::*;

    fn picker_effect() -> TuiEffect {
        TuiEffect::LoadSessionPicker {
            generation: 1,
            scope: bcode_session_models::SessionDiscoveryScope::AllWorkingDirectories,
            query: bcode_session_models::SessionCatalogQuery::default(),
            cursor: None,
        }
    }

    #[test]
    fn session_picker_subscription_replacement_and_cancellation_are_coalesced() {
        let mut queue = TuiEffectQueue::default();
        queue.replace(picker_effect());
        queue.replace(picker_effect());
        let (effects, _) = queue.drain_runtime();
        assert_eq!(effects.len(), 1);
        assert!(matches!(effects[0].0, EffectSchedule::Replace));
        queue.replace(picker_effect());
        queue.cancel(picker_effect());
        let (effects, _) = queue.drain_runtime();
        assert_eq!(effects.len(), 1);
        assert!(matches!(effects[0].0, EffectSchedule::Cancel));
    }

    #[test]
    fn application_failures_do_not_claim_the_daemon_is_offline() {
        let server_error = ClientError::Server {
            code: "permission_denied".to_owned(),
            message: "request rejected".to_owned(),
        };
        assert!(matches!(
            DaemonObservation::from_client_result::<()>(&Err(server_error)),
            DaemonObservation::Failed(_)
        ));
        let optional_domain = ClientError::Server {
            code: "workflow_capability_unavailable".to_owned(),
            message: "maintenance required".to_owned(),
        };
        assert!(optional_domain.is_optional_domain_unavailable());
        assert_eq!(
            DaemonObservation::from_tui_result::<()>(&Err(TuiError::SessionUnavailable {
                session_id: SessionId::new(),
                reason: "owned elsewhere".to_owned(),
            })),
            DaemonObservation::None
        );
    }

    #[test]
    fn transport_unavailability_remains_connectivity_evidence() {
        let error = ClientError::Transport(bcode_ipc::IpcTransportError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "offline",
        )));
        assert!(matches!(
            DaemonObservation::from_client_result::<()>(&Err(error)),
            DaemonObservation::Unavailable(_)
        ));
    }

    #[test]
    fn runtime_effect_drain_preserves_every_ordered_note_per_session() {
        let session_id = SessionId::new();
        let mut queue = TuiEffectQueue::default();
        for note_id in ["0001", "0002", "0003"] {
            queue.start_ordered(TuiEffect::AppendPresentationNote {
                session_id,
                source_id: "test".to_owned(),
                note_id: note_id.to_owned(),
                text: note_id.to_owned(),
                format: bcode_command::CommandTextFormat::PlainText,
            });
        }

        let (effects, notes) = queue.drain_runtime();

        assert!(effects.is_empty());
        let queued = notes
            .get(&session_id)
            .expect("ordered session queue")
            .iter()
            .map(TuiEffect::key)
            .collect::<Vec<_>>();
        assert_eq!(
            queued,
            vec![
                EffectKey::AppendPresentationNote(session_id, "0001".to_owned()),
                EffectKey::AppendPresentationNote(session_id, "0002".to_owned()),
                EffectKey::AppendPresentationNote(session_id, "0003".to_owned()),
            ]
        );
    }
}

/// Applications whose discovered credentials are neither connected nor already announced.
/// Records them as announced so each source is mentioned once.
async fn announce_external_connections(client: &BcodeClient) -> Result<Vec<String>, ClientError> {
    use bcode_provider_auth_models::external::ExternalAuthApplication as _;
    let unavailable = |error: bcode_provider_auth_models::external::ExternalAuthError| {
        ClientError::Protocol(error.to_string())
    };
    let candidates = client.discover_external_auth().await.map_err(unavailable)?;
    let approved: std::collections::BTreeSet<String> = client
        .list_external_auth()
        .await
        .map_err(unavailable)?
        .into_iter()
        .map(|grant| grant.binding_id)
        .collect();
    let path = bcode_config::external_notices::default_path();
    let announced = bcode_config::external_notices::load_from(&path).unwrap_or_default();
    let new: Vec<_> = candidates
        .into_iter()
        .filter(|candidate| {
            candidate.available
                && !approved.contains(&candidate.binding_id)
                && !announced.contains(&candidate.binding_id)
        })
        .collect();
    if !new.is_empty() {
        // Best effort: a failed write mentions the source again next time, nothing worse.
        let _ = bcode_config::external_notices::record_at(
            &path,
            new.iter().map(|candidate| candidate.binding_id.clone()),
        );
    }
    let mut applications: Vec<String> = new
        .into_iter()
        .map(|candidate| candidate.application)
        .collect();
    applications.sort();
    applications.dedup();
    Ok(applications)
}
