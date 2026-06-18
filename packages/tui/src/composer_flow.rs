//! Composer submission flow for the TUI.

use std::io::Write;

use bcode_client::BcodeClient;
use bcode_session_models::SessionId;

use super::activity::ActivityState;
use super::runtime_context::{TuiIo, TuiServices};
use super::session_flow::ActiveChat;
use super::{
    TuiError, helpers, model_flow, ralph_flow, session_flow, session_fork_flow, skill_flow,
    slash_commands, slash_registry, thinking_dialog, worktree_flow,
};

/// Result of submitting staged composer text.
pub type SubmitComposerOutcome = Option<ComposerModalRequest>;

/// Modal requested by a composer submission.
pub enum ComposerModalRequest {
    /// Open thinking settings.
    Thinking(thinking_dialog::ThinkingDialogState),
    /// Open timeline browser.
    Timeline(super::timeline_dialog::TimelineDialogState),
}

fn agent_selection_status(chat: &ActiveChat, agent_name: &str) -> String {
    if matches!(chat.app.activity(), ActivityState::Idle) {
        format!("agent {agent_name} selected")
    } else {
        format!("agent {agent_name} selected for next message")
    }
}

fn apply_draft_agent_selection(
    chat: &mut ActiveChat,
    agent_id: String,
    agent_name: &str,
    agent_accent: Option<String>,
) {
    if chat.app.session_id().is_some() {
        chat.app.set_pending_agent(agent_id, agent_accent);
        chat.app
            .set_status(agent_selection_status(chat, agent_name));
    } else {
        chat.app.set_current_agent(agent_id, agent_accent);
        chat.app.set_status(format!("agent set to {agent_name}"));
    }
}

async fn open_thinking_settings(
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
    session_id: Option<SessionId>,
    focus: thinking_dialog::ThinkingDialogFocus,
) -> Result<SubmitComposerOutcome, TuiError> {
    let status = match if let Some(session_id) = session_id {
        services.client.session_model_status(session_id).await
    } else {
        services.client.default_model_status().await
    } {
        Ok(status) => status,
        Err(error) => {
            helpers::report_client_issue(&mut chat.app, "thinking settings unavailable", &error);
            return Ok(None);
        }
    };
    chat.app.apply_model_status(status.clone());
    chat.app
        .set_status("thinking settings: enter apply, esc cancel".to_owned());
    Ok(Some(ComposerModalRequest::Thinking(
        thinking_dialog::ThinkingDialogState::new_focused(
            chat.app.reasoning_visible(),
            &status,
            focus,
        ),
    )))
}

#[allow(clippy::too_many_lines)]
async fn handle_slash_command<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
    session_id: Option<SessionId>,
    message: &str,
    resolution: slash_registry::SlashResolution,
) -> Result<SubmitComposerOutcome, TuiError> {
    match slash_commands::execute_resolved(
        services.client,
        session_id,
        chat.app.current_agent_id(),
        message,
        resolution,
    )
    .await?
    {
        slash_commands::SlashCommandOutcome::Unknown(_command) => {}
        slash_commands::SlashCommandOutcome::Handled(status) => {
            chat.app.clear_pending_submission(message);
            chat.app.set_status(status);
        }
        slash_commands::SlashCommandOutcome::SetThinkingDisplay(show) => {
            chat.app.clear_pending_submission(message);
            chat.app.set_reasoning_visible(show);
            chat.app.set_status(if show {
                "thinking display shown".to_owned()
            } else {
                "thinking display hidden".to_owned()
            });
        }
        slash_commands::SlashCommandOutcome::ToggleThinkingDisplay => {
            chat.app.clear_pending_submission(message);
            let show = !chat.app.reasoning_visible();
            chat.app.set_reasoning_visible(show);
            chat.app.set_status(if show {
                "thinking display shown".to_owned()
            } else {
                "thinking display hidden".to_owned()
            });
        }
        slash_commands::SlashCommandOutcome::SystemNote(note) => {
            chat.app.clear_pending_submission(message);
            chat.app.push_system_note(note);
            chat.app.set_status("slash command handled".to_owned());
        }
        slash_commands::SlashCommandOutcome::OpenThinkingSettings(focus) => {
            chat.app.clear_pending_submission(message);
            return open_thinking_settings(services, chat, session_id, focus).await;
        }
        slash_commands::SlashCommandOutcome::OpenTimeline => {
            chat.app.clear_pending_submission(message);
            let entries = if session_id.is_some() {
                chat.app.timeline_entries()
            } else {
                Vec::new()
            };
            chat.app.set_status(if entries.is_empty() {
                "timeline: no user messages".to_owned()
            } else {
                "timeline: select a user message".to_owned()
            });
            return Ok(Some(ComposerModalRequest::Timeline(
                super::timeline_dialog::TimelineDialogState::new(entries),
            )));
        }
        slash_commands::SlashCommandOutcome::NewDraftSession => {
            chat.app.clear_pending_submission(message);
            session_flow::switch_to_draft_session(chat);
            session_flow::start_draft_status_hydration(
                services.client,
                chat,
                std::env::current_dir()?,
            );
        }
        slash_commands::SlashCommandOutcome::DraftAgentSelected {
            agent_id,
            agent_name,
            agent_accent,
        } => {
            chat.app.clear_pending_submission(message);
            apply_draft_agent_selection(chat, agent_id, &agent_name, agent_accent);
        }
        slash_commands::SlashCommandOutcome::PickSession => {
            chat.app.clear_pending_submission(message);
            match session_flow::pick_session(io, services).await? {
                session_flow::PickSessionOutcome::Existing(next_session_id) => {
                    session_flow::switch_session(
                        io.terminal,
                        services.client,
                        chat,
                        next_session_id,
                    )?;
                }
                session_flow::PickSessionOutcome::Draft => {
                    session_flow::switch_to_draft_session(chat);
                    session_flow::start_draft_status_hydration(
                        services.client,
                        chat,
                        std::env::current_dir()?,
                    );
                }
            }
        }
        slash_commands::SlashCommandOutcome::PickModel => {
            chat.app.clear_pending_submission(message);
            model_flow::pick_model_for_session(io, services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::PickSkill => {
            chat.app.clear_pending_submission(message);
            skill_flow::pick_skill_for_session(io, services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::InvokeSkill {
            skill_id,
            arguments,
        } => {
            chat.app.clear_pending_submission(message);
            skill_flow::invoke_skill_for_session(io, services, chat, skill_id, arguments).await?;
        }
        slash_commands::SlashCommandOutcome::OpenWorktreeCreateDialog => {
            chat.app.clear_pending_submission(message);
            worktree_flow::create_for_current_session(io, services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::OpenForkSessionWizard => {
            chat.app.clear_pending_submission(message);
            session_fork_flow::fork_current_session(io, services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::SessionCloned { session_id } => {
            chat.app.clear_pending_submission(message);
            session_flow::switch_session(io.terminal, services.client, chat, session_id)?;
            chat.app
                .set_status("cloned session and switched".to_owned());
        }
        slash_commands::SlashCommandOutcome::OpenRalphHome => {
            chat.app.clear_pending_submission(message);
            ralph_flow::open_home(io, services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::OpenRalphStartDialog => {
            chat.app.clear_pending_submission(message);
            ralph_flow::start_loop(io, services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::ShowRalphStatus => {
            chat.app.clear_pending_submission(message);
            ralph_flow::show_status(services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::RunRalphLoop => {
            chat.app.clear_pending_submission(message);
            ralph_flow::run_loop(services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::ApproveRalphRun => {
            chat.app.clear_pending_submission(message);
            ralph_flow::approve_run(services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::StopRalphLoop => {
            chat.app.clear_pending_submission(message);
            ralph_flow::stop_loop(services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::ListRalphRuns => {
            chat.app.clear_pending_submission(message);
            ralph_flow::list_runs(services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::ListRalphIterations => {
            chat.app.clear_pending_submission(message);
            ralph_flow::list_iterations(services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::ResumeRalphRun => {
            chat.app.clear_pending_submission(message);
            ralph_flow::resume_run(services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::OpenRalphProgress => {
            chat.app.clear_pending_submission(message);
            ralph_flow::open_progress(chat)?;
        }
        slash_commands::SlashCommandOutcome::BuildRalphPrompt(kind) => {
            chat.app.clear_pending_submission(message);
            ralph_flow::show_prompt(chat, kind)?;
        }
        slash_commands::SlashCommandOutcome::ToggleDiff => {
            chat.app.clear_pending_submission(message);
            let _changed = chat.app.toggle_diff_visible();
            chat.app.set_status(if chat.app.diff_visible() {
                "diff panel shown".to_owned()
            } else {
                "diff panel hidden".to_owned()
            });
        }
    }
    Ok(None)
}

async fn commit_pending_agent(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    session_id: SessionId,
) -> Result<(), TuiError> {
    let Some(agent_id) = chat.app.pending_agent_id().map(ToOwned::to_owned) else {
        return Ok(());
    };
    match client.set_session_agent(session_id, agent_id).await {
        Ok(()) => {
            let _committed = chat.app.take_pending_agent();
            Ok(())
        }
        Err(error) => {
            helpers::report_client_issue(&mut chat.app, "agent switch failed", &error);
            Err(error.into())
        }
    }
}

async fn commit_pending_reasoning(
    client: &BcodeClient,
    chat: &ActiveChat,
    session_id: SessionId,
) -> Result<(), TuiError> {
    let effort = chat.app.reasoning_effort().map(ToOwned::to_owned);
    let summary = chat.app.reasoning_summary().map(ToOwned::to_owned);
    client
        .set_session_reasoning(session_id, effort, summary)
        .await?;
    Ok(())
}

/// Submit the staged composer text.
pub async fn submit_composer<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
    placement: bcode_ipc::PromptPlacement,
) -> Result<SubmitComposerOutcome, TuiError> {
    let session_id = chat.app.session_id();
    let message = chat.app.take_pending_submission();
    if message.trim().is_empty() {
        chat.app.clear_pending_submission(&message);
        return Ok(None);
    }
    let slash_resolution = if slash_registry::slash_command_name(&message).is_some() {
        slash_registry::resolve(services.client, &message)
            .await
            .ok()
    } else {
        None
    };
    if let Some(resolution) = slash_resolution.filter(slash_registry::SlashResolution::is_known) {
        if session_id.is_some() || resolution.is_draft_safe() {
            return handle_slash_command(io, services, chat, session_id, &message, resolution)
                .await;
        }
        chat.app.clear_pending_submission(&message);
        chat.app.set_status(
            "slash command requires an active session; send a message first".to_owned(),
        );
        return Ok(None);
    }
    let session_id =
        match session_flow::persist_draft_session(io.terminal, services.client, chat).await {
            Ok(session_id) => session_id,
            Err(TuiError::Client(error)) => {
                chat.app.restore_pending_submission(&message);
                helpers::report_client_issue(&mut chat.app, "session creation unavailable", &error);
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
    if let Err(error) = commit_pending_agent(services.client, chat, session_id).await {
        chat.app.restore_pending_submission(&message);
        chat.app.set_status(format!("agent switch failed: {error}"));
        return Ok(None);
    }
    if let Err(error) = commit_pending_reasoning(services.client, chat, session_id).await {
        chat.app.restore_pending_submission(&message);
        chat.app
            .set_status(format!("thinking settings failed: {error}"));
        return Ok(None);
    }
    match services
        .client
        .send_user_message(session_id, message.clone(), placement)
        .await
    {
        Ok(acceptance) => {
            match acceptance.disposition {
                bcode_ipc::MessageAcceptanceDisposition::AppliedSteering => {
                    chat.app.mark_pending_submission_sent();
                    chat.app.set_status("Steering sent".to_owned());
                }
                bcode_ipc::MessageAcceptanceDisposition::QueuedFollowUp
                | bcode_ipc::MessageAcceptanceDisposition::QueuedTurn => {
                    chat.app.set_idle();
                    chat.app
                        .mark_pending_submission_queued(acceptance.queue_position);
                    chat.app.set_status(format!(
                        "Message queued{}",
                        acceptance
                            .queue_position
                            .map_or_else(String::new, |position| format!(" at #{position}"))
                    ));
                }
                bcode_ipc::MessageAcceptanceDisposition::StartedTurn => {
                    chat.app.mark_pending_submission_sent();
                    chat.app.set_status("Message sent".to_owned());
                }
            }
            Ok(None)
        }
        Err(error) => {
            chat.app.restore_pending_submission(&message);
            helpers::report_client_issue(&mut chat.app, "send failed", &error);
            Ok(None)
        }
    }
}
