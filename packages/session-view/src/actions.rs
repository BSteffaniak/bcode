//! Daemon-backed execution for renderer-neutral session actions.

use bcode_client::{BcodeClient, ClientError, MessageAcceptance};
use bcode_ipc::{ComposerDraftScope, PromptPlacement};
use bcode_session_models::SessionId;
use bcode_session_view_models::{
    ComposerDraftViewScope, MessageAcceptanceDispositionView, PromptPlacementView,
    SessionViewAction, SessionViewActionOutcome,
};
use bcode_skill_models::SkillId;

/// Execute one renderer-neutral session action through the daemon client.
///
/// # Errors
///
/// Returns an error when the daemon cannot be reached, rejects the request, or the action requires
/// data that was not supplied by the renderer.
pub async fn execute_session_view_action(
    client: &BcodeClient,
    action: SessionViewAction,
) -> Result<SessionViewActionOutcome, ClientError> {
    match action {
        SessionViewAction::SubmitMessage {
            session_id,
            launch_working_directory,
            text,
            placement,
        } => {
            execute_submit_message(
                client,
                session_id,
                launch_working_directory,
                text,
                placement,
            )
            .await
        }
        SessionViewAction::CancelTurn {
            session_id,
            clear_queue,
        } => execute_cancel_turn(client, session_id, clear_queue).await,
        SessionViewAction::ResolvePermission {
            permission_id,
            approved,
            remember,
        } => execute_resolve_permission(client, permission_id, approved, remember).await,
        SessionViewAction::ResolvePermissionBatch { batch_id, approved } => {
            Ok(SessionViewActionOutcome::PermissionBatchResolved {
                resolved_count: client.resolve_permission_batch(batch_id, approved).await?,
            })
        }
        SessionViewAction::ResolveExchange {
            interaction_id,
            resolution,
        } => execute_resolve_interaction(client, interaction_id, resolution).await,
        SessionViewAction::UpdateDraft { scope, text } => {
            execute_update_draft(client, scope, text).await
        }
        SessionViewAction::SetModel {
            session_id,
            provider_plugin_id,
            model_id,
        } => execute_set_model(client, session_id, provider_plugin_id, model_id).await,
        SessionViewAction::SetReasoning {
            session_id,
            effort,
            summary,
        } => execute_set_reasoning(client, session_id, effort, summary).await,
        SessionViewAction::RenameSession { session_id, name } => {
            execute_rename_session(client, session_id, name).await
        }
        SessionViewAction::DeleteSession { session_id } => {
            execute_delete_session(client, session_id).await
        }
        SessionViewAction::ForkSession {
            session_id,
            prompt_sequence,
            name,
        } => execute_fork_session(client, session_id, prompt_sequence, name).await,
        SessionViewAction::CloneSession { session_id, name } => {
            execute_clone_session(client, session_id, name).await
        }
        SessionViewAction::ChangeWorkingDirectory { session_id, path } => {
            execute_change_working_directory(client, session_id, path).await
        }
        SessionViewAction::CancelRuntimeWork {
            session_id,
            work_id,
        } => execute_cancel_runtime_work(client, session_id, work_id).await,
        SessionViewAction::CompactContext { session_id } => {
            execute_compact_context(client, session_id).await
        }
        SessionViewAction::SetAgent {
            session_id,
            agent_id,
        } => execute_set_agent(client, session_id, agent_id).await,
        SessionViewAction::ActivateSkill {
            session_id,
            skill_id,
        } => execute_activate_skill(client, session_id, skill_id).await,
        SessionViewAction::DeactivateSkill {
            session_id,
            skill_id,
        } => execute_deactivate_skill(client, session_id, skill_id).await,
        SessionViewAction::SwitchSession { .. }
        | SessionViewAction::LoadOlderHistory { .. }
        | SessionViewAction::LoadNewerHistory { .. } => Err(ClientError::Server {
            code: "renderer_action_not_daemon_effect".to_owned(),
            message: "session switching and history window loading are renderer state-flow actions"
                .to_owned(),
        }),
    }
}

async fn execute_set_agent(
    client: &BcodeClient,
    session_id: SessionId,
    agent_id: String,
) -> Result<SessionViewActionOutcome, ClientError> {
    client.set_session_agent(session_id, agent_id).await?;
    Ok(SessionViewActionOutcome::None)
}

async fn execute_activate_skill(
    client: &BcodeClient,
    session_id: SessionId,
    skill_id: String,
) -> Result<SessionViewActionOutcome, ClientError> {
    client
        .activate_skill(session_id, SkillId::new(skill_id))
        .await?;
    Ok(SessionViewActionOutcome::None)
}

async fn execute_deactivate_skill(
    client: &BcodeClient,
    session_id: SessionId,
    skill_id: String,
) -> Result<SessionViewActionOutcome, ClientError> {
    client
        .deactivate_skill(session_id, SkillId::new(skill_id))
        .await?;
    Ok(SessionViewActionOutcome::None)
}

async fn execute_update_draft(
    client: &BcodeClient,
    scope: ComposerDraftViewScope,
    text: String,
) -> Result<SessionViewActionOutcome, ClientError> {
    client
        .set_composer_draft(composer_draft_scope(scope), text)
        .await?;
    Ok(SessionViewActionOutcome::None)
}

async fn execute_set_model(
    client: &BcodeClient,
    session_id: SessionId,
    provider_plugin_id: Option<String>,
    model_id: String,
) -> Result<SessionViewActionOutcome, ClientError> {
    client
        .set_session_model(session_id, provider_plugin_id, model_id)
        .await?;
    Ok(SessionViewActionOutcome::None)
}

async fn execute_set_reasoning(
    client: &BcodeClient,
    session_id: SessionId,
    effort: Option<String>,
    summary: Option<String>,
) -> Result<SessionViewActionOutcome, ClientError> {
    client
        .set_session_reasoning(session_id, effort, summary)
        .await?;
    Ok(SessionViewActionOutcome::None)
}

async fn execute_rename_session(
    client: &BcodeClient,
    session_id: SessionId,
    name: Option<String>,
) -> Result<SessionViewActionOutcome, ClientError> {
    Ok(SessionViewActionOutcome::SessionRenamed {
        session: Box::new(client.rename_session(session_id, name).await?),
    })
}

async fn execute_delete_session(
    client: &BcodeClient,
    session_id: SessionId,
) -> Result<SessionViewActionOutcome, ClientError> {
    Ok(SessionViewActionOutcome::SessionDeleted {
        session: Box::new(client.delete_session(session_id).await?),
    })
}

async fn execute_fork_session(
    client: &BcodeClient,
    session_id: SessionId,
    prompt_sequence: u64,
    name: Option<String>,
) -> Result<SessionViewActionOutcome, ClientError> {
    Ok(SessionViewActionOutcome::SessionForked {
        fork: Box::new(
            client
                .fork_session(session_id, prompt_sequence, name)
                .await?,
        ),
    })
}

async fn execute_clone_session(
    client: &BcodeClient,
    session_id: SessionId,
    name: Option<String>,
) -> Result<SessionViewActionOutcome, ClientError> {
    Ok(SessionViewActionOutcome::SessionCloned {
        fork: Box::new(client.clone_session(session_id, name).await?),
    })
}

async fn execute_change_working_directory(
    client: &BcodeClient,
    session_id: SessionId,
    path: std::path::PathBuf,
) -> Result<SessionViewActionOutcome, ClientError> {
    Ok(SessionViewActionOutcome::WorkingDirectoryChanged {
        session: Box::new(
            client
                .change_session_working_directory(session_id, path)
                .await?,
        ),
    })
}

async fn execute_cancel_runtime_work(
    client: &BcodeClient,
    session_id: SessionId,
    work_id: bcode_session_models::WorkId,
) -> Result<SessionViewActionOutcome, ClientError> {
    Ok(SessionViewActionOutcome::RuntimeWorkCancellationRequested {
        cancelled: client.cancel_runtime_work(session_id, work_id).await?,
    })
}

async fn execute_compact_context(
    client: &BcodeClient,
    session_id: SessionId,
) -> Result<SessionViewActionOutcome, ClientError> {
    Ok(SessionViewActionOutcome::ContextCompacted {
        message: client.compact_session(session_id).await?,
    })
}

async fn execute_resolve_interaction(
    client: &BcodeClient,
    interaction_id: String,
    resolution: bcode_session_models::ToolExchangeResolution,
) -> Result<SessionViewActionOutcome, ClientError> {
    Ok(SessionViewActionOutcome::InteractionResolved {
        resolved: client
            .resolve_tool_exchange(interaction_id, resolution)
            .await?,
    })
}

async fn execute_submit_message(
    client: &BcodeClient,
    session_id: Option<SessionId>,
    launch_working_directory: Option<std::path::PathBuf>,
    text: String,
    placement: PromptPlacementView,
) -> Result<SessionViewActionOutcome, ClientError> {
    let session_id = if let Some(session_id) = session_id {
        session_id
    } else {
        let working_directory = launch_working_directory.ok_or_else(|| ClientError::Server {
            code: "invalid_renderer_action".to_owned(),
            message: "submit message without a session requires launch_working_directory"
                .to_owned(),
        })?;
        client
            .create_session_in_working_directory(None, working_directory)
            .await?
            .id
    };
    let acceptance = client
        .send_user_message(session_id, text, prompt_placement(placement))
        .await?;
    Ok(message_accepted_outcome(session_id, acceptance))
}

async fn execute_cancel_turn(
    client: &BcodeClient,
    session_id: SessionId,
    clear_queue: bool,
) -> Result<SessionViewActionOutcome, ClientError> {
    Ok(SessionViewActionOutcome::Cancelled {
        cancelled: client
            .cancel_session_turn_with_options(session_id, clear_queue)
            .await?,
    })
}

async fn execute_resolve_permission(
    client: &BcodeClient,
    permission_id: String,
    approved: bool,
    remember: bool,
) -> Result<SessionViewActionOutcome, ClientError> {
    Ok(SessionViewActionOutcome::PermissionResolved {
        resolved: client
            .resolve_permission_with_remember(permission_id, approved, remember)
            .await?,
    })
}

fn message_accepted_outcome(
    session_id: SessionId,
    acceptance: MessageAcceptance,
) -> SessionViewActionOutcome {
    SessionViewActionOutcome::MessageAccepted {
        session_id,
        queued: acceptance.queued,
        queue_position: acceptance
            .queue_position
            .and_then(|position| usize::try_from(position).ok()),
        disposition: match acceptance.disposition {
            bcode_ipc::MessageAcceptanceDisposition::AppliedSteering => {
                MessageAcceptanceDispositionView::AppliedSteering
            }
            bcode_ipc::MessageAcceptanceDisposition::QueuedFollowUp => {
                MessageAcceptanceDispositionView::QueuedFollowUp
            }
            bcode_ipc::MessageAcceptanceDisposition::QueuedTurn => {
                MessageAcceptanceDispositionView::QueuedTurn
            }
            bcode_ipc::MessageAcceptanceDisposition::StartedTurn => {
                MessageAcceptanceDispositionView::StartedTurn
            }
        },
    }
}

const fn prompt_placement(value: PromptPlacementView) -> PromptPlacement {
    match value {
        PromptPlacementView::Steering => PromptPlacement::Steering,
        PromptPlacementView::FollowUp => PromptPlacement::FollowUp,
    }
}

fn composer_draft_scope(value: ComposerDraftViewScope) -> ComposerDraftScope {
    match value {
        ComposerDraftViewScope::Session { session_id } => {
            ComposerDraftScope::Session { session_id }
        }
        ComposerDraftViewScope::DraftSession {
            launch_working_directory,
        } => ComposerDraftScope::DraftSession {
            launch_working_directory,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_placement_maps_to_ipc() {
        assert_eq!(
            prompt_placement(PromptPlacementView::Steering),
            PromptPlacement::Steering
        );
        assert_eq!(
            prompt_placement(PromptPlacementView::FollowUp),
            PromptPlacement::FollowUp
        );
    }

    #[test]
    fn draft_scope_maps_to_ipc() {
        let session_id = SessionId::new();
        assert_eq!(
            composer_draft_scope(ComposerDraftViewScope::Session { session_id }),
            ComposerDraftScope::Session { session_id }
        );
    }

    #[test]
    fn message_acceptance_outcome_preserves_all_dispositions() {
        let session_id = SessionId::new();
        for (ipc, view) in [
            (
                bcode_ipc::MessageAcceptanceDisposition::AppliedSteering,
                MessageAcceptanceDispositionView::AppliedSteering,
            ),
            (
                bcode_ipc::MessageAcceptanceDisposition::QueuedFollowUp,
                MessageAcceptanceDispositionView::QueuedFollowUp,
            ),
            (
                bcode_ipc::MessageAcceptanceDisposition::QueuedTurn,
                MessageAcceptanceDispositionView::QueuedTurn,
            ),
            (
                bcode_ipc::MessageAcceptanceDisposition::StartedTurn,
                MessageAcceptanceDispositionView::StartedTurn,
            ),
        ] {
            assert!(matches!(
                message_accepted_outcome(
                    session_id,
                    MessageAcceptance {
                        queued: !matches!(
                            ipc,
                            bcode_ipc::MessageAcceptanceDisposition::StartedTurn
                                | bcode_ipc::MessageAcceptanceDisposition::AppliedSteering
                        ),
                        queue_position: None,
                        disposition: ipc,
                    },
                ),
                SessionViewActionOutcome::MessageAccepted { disposition, .. }
                    if disposition == view
            ));
        }
    }

    #[test]
    fn message_acceptance_outcome_preserves_queue_state() {
        let session_id = SessionId::new();
        assert_eq!(
            message_accepted_outcome(
                session_id,
                MessageAcceptance {
                    queued: true,
                    queue_position: Some(3),
                    disposition: bcode_ipc::MessageAcceptanceDisposition::QueuedFollowUp,
                },
            ),
            SessionViewActionOutcome::MessageAccepted {
                session_id,
                queued: true,
                queue_position: Some(3),
                disposition: MessageAcceptanceDispositionView::QueuedFollowUp,
            }
        );
    }
}
