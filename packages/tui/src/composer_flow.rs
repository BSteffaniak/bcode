//! Composer submission flow for the TUI.

use std::io::Write;

use bcode_session_models::SessionId;

use super::runtime_context::{TuiIo, TuiServices};
use super::session_flow::ActiveChat;
use super::{TuiError, model_flow, session_flow, skill_flow, slash_commands, thinking_dialog};

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
            | "skills"
            | "skill"
            | "thinking"
            | "rescan-imports"
            | "runtime"
            | "status"
    )
}

fn has_known_slash_command(message: &str) -> bool {
    message
        .strip_prefix('/')
        .and_then(|command| command.split_whitespace().next())
        .is_some_and(is_slash_command_name)
}

async fn handle_slash_command<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
    session_id: SessionId,
    message: &str,
) -> Result<SubmitComposerOutcome, TuiError> {
    match slash_commands::execute(services.client, session_id, message).await? {
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
            let status = services.client.session_model_status(session_id).await?;
            chat.app.apply_model_status(status.clone());
            chat.app
                .set_status("thinking settings: enter apply, esc cancel".to_owned());
            return Ok(Some(thinking_dialog::ThinkingDialogState::new_focused(
                chat.app.reasoning_visible(),
                &status,
                focus,
            )));
        }
        slash_commands::SlashCommandOutcome::SwitchSession(next_session_id) => {
            chat.app.clear_pending_submission(message);
            session_flow::switch_session(services.client, chat, next_session_id).await?;
        }
        slash_commands::SlashCommandOutcome::PickSession => {
            chat.app.clear_pending_submission(message);
            let next_session_id = session_flow::pick_session(io, services).await?;
            session_flow::switch_session(services.client, chat, next_session_id).await?;
        }
        slash_commands::SlashCommandOutcome::PickModel => {
            chat.app.clear_pending_submission(message);
            model_flow::pick_model_for_session(io, services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::PickSkill => {
            chat.app.clear_pending_submission(message);
            skill_flow::pick_skill_for_session(io, services, chat).await?;
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
) -> Result<SubmitComposerOutcome, TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(None);
    };
    let message = chat.app.take_pending_submission();
    if message.trim().is_empty() {
        chat.app.clear_pending_submission(&message);
        return Ok(None);
    }
    if has_known_slash_command(&message) {
        return handle_slash_command(io, services, chat, session_id, &message).await;
    }
    match services
        .client
        .send_user_message(session_id, message.clone())
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
