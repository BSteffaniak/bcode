//! Composer submission flow for the TUI.

use std::io::Write;

use bcode_session_models::SessionId;

use super::runtime_context::{TuiIo, TuiServices};
use super::session_flow::ActiveChat;
use super::{
    TuiError, model_flow, session_flow, session_fork_flow, skill_flow, slash_commands,
    thinking_dialog, worktree_flow,
};

/// Result of submitting staged composer text.
pub type SubmitComposerOutcome = Option<thinking_dialog::ThinkingDialogState>;

fn is_slash_command_name(command: &str) -> bool {
    matches!(
        command,
        "sessions"
            | "new"
            | "plan"
            | "build"
            | "agent"
            | "compact"
            | "model"
            | "models"
            | "set-model"
            | "provider"
            | "set-provider"
            | "diff"
            | "cwd"
            | "worktree"
            | "worktrees"
            | "fork"
            | "clone"
            | "skills"
            | "skill"
            | "thinking"
            | "rescan-imports"
            | "runtime"
            | "status"
            | "ralph"
    )
}

fn is_draft_safe_slash_command(command: &str) -> bool {
    matches!(
        command,
        "sessions"
            | "new"
            | "plan"
            | "build"
            | "agent"
            | "diff"
            | "worktree"
            | "worktrees"
            | "fork"
            | "clone"
            | "skills"
            | "skill"
            | "thinking"
            | "rescan-imports"
            | "ralph"
    )
}

fn slash_command_name(message: &str) -> Option<&str> {
    message
        .strip_prefix('/')
        .and_then(|command| command.split_whitespace().next())
}

fn has_known_slash_command(message: &str) -> bool {
    slash_command_name(message).is_some_and(is_slash_command_name)
}

fn apply_draft_agent_selection(
    chat: &mut ActiveChat,
    agent_id: String,
    agent_name: &str,
    agent_accent: Option<String>,
) {
    chat.app.set_current_agent(agent_id, agent_accent);
    chat.app.set_status(format!("agent set to {agent_name}"));
}

async fn open_thinking_settings(
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
    session_id: Option<SessionId>,
    focus: thinking_dialog::ThinkingDialogFocus,
) -> Result<SubmitComposerOutcome, TuiError> {
    let Some(session_id) = session_id else {
        chat.app
            .set_status("thinking settings require an active session".to_owned());
        return Ok(None);
    };
    let status = services.client.session_model_status(session_id).await?;
    chat.app.apply_model_status(status.clone());
    chat.app
        .set_status("thinking settings: enter apply, esc cancel".to_owned());
    Ok(Some(thinking_dialog::ThinkingDialogState::new_focused(
        chat.app.reasoning_visible(),
        &status,
        focus,
    )))
}

#[allow(clippy::too_many_lines)]
async fn handle_slash_command<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
    session_id: Option<SessionId>,
    message: &str,
) -> Result<SubmitComposerOutcome, TuiError> {
    match slash_commands::execute(
        services.client,
        session_id,
        chat.app.current_agent_id(),
        message,
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
        slash_commands::SlashCommandOutcome::NewDraftSession => {
            chat.app.clear_pending_submission(message);
            session_flow::switch_to_draft_session(chat);
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
        slash_commands::SlashCommandOutcome::OpenRalphStartDialog => {
            chat.app.clear_pending_submission(message);
            worktree_flow::start_ralph_loop(io, services, chat).await?;
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
    if has_known_slash_command(&message) {
        let command = slash_command_name(&message);
        if session_id.is_some() || command.is_some_and(is_draft_safe_slash_command) {
            return handle_slash_command(io, services, chat, session_id, &message).await;
        }
        chat.app.clear_pending_submission(&message);
        chat.app.set_status(
            "slash command requires an active session; send a message first".to_owned(),
        );
        return Ok(None);
    }
    let session_id =
        session_flow::persist_draft_session(io.terminal, services.client, chat).await?;
    match services
        .client
        .send_user_message(session_id, message.clone(), placement)
        .await
    {
        Ok(acceptance) => {
            if acceptance.queued {
                chat.app.set_idle();
                chat.app
                    .mark_pending_submission_queued(acceptance.queue_position);
                chat.app.set_status(format!(
                    "Message queued{}",
                    acceptance
                        .queue_position
                        .map_or_else(String::new, |position| format!(" at #{position}"))
                ));
            } else {
                chat.app.mark_pending_submission_sent();
                chat.app.set_status("Message sent".to_owned());
            }
            Ok(None)
        }
        Err(error) => {
            chat.app.restore_pending_submission(&message);
            chat.app.set_status(format!("send failed: {error}"));
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn recognizes_import_rescan_slash_command() {
        assert!(super::has_known_slash_command("/rescan-imports"));
    }
}
