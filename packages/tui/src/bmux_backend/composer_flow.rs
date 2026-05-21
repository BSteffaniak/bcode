//! Composer submission flow for the BMUX backend.

use std::io::Write;

use bcode_client::BcodeClient;
use bmux_tui::terminal::Terminal;

use super::keymap::BmuxKeyMap;
use super::session_flow::ActiveChat;
use super::{TuiError, model_flow, session_flow, skill_flow, slash_commands};

/// Submit the staged composer text.
pub(super) async fn submit_composer<W: Write>(
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    terminal: &mut Terminal<&mut W>,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let message = chat.app.take_pending_submission();
    if message.trim().is_empty() {
        chat.app.clear_pending_submission(&message);
        return Ok(());
    }
    if message.starts_with('/') {
        chat.app.clear_pending_submission(&message);
        match slash_commands::execute(client, session_id, &message).await? {
            slash_commands::SlashCommandOutcome::Handled(status) => chat.app.set_status(status),
            slash_commands::SlashCommandOutcome::SystemNote(note) => {
                chat.app.push_system_note(note);
                chat.app.set_status("slash command handled".to_owned());
            }
            slash_commands::SlashCommandOutcome::SwitchSession(next_session_id) => {
                session_flow::switch_session(client, chat, next_session_id).await?;
            }
            slash_commands::SlashCommandOutcome::PickSession => {
                let next_session_id = session_flow::pick_session(terminal, client, keymap).await?;
                session_flow::switch_session(client, chat, next_session_id).await?;
            }
            slash_commands::SlashCommandOutcome::PickModel => {
                model_flow::pick_model_for_session(terminal, client, chat, keymap).await?;
            }
            slash_commands::SlashCommandOutcome::PickSkill => {
                skill_flow::pick_skill_for_session(terminal, client, chat, keymap).await?;
            }
            slash_commands::SlashCommandOutcome::ToggleDiff => {
                let _changed = chat.app.toggle_diff_visible();
                chat.app.set_status(if chat.app.diff_visible() {
                    "diff panel shown".to_owned()
                } else {
                    "diff panel hidden".to_owned()
                });
            }
            slash_commands::SlashCommandOutcome::Unknown(command) => {
                chat.app
                    .set_status(format!("unknown slash command: {command}"));
            }
        }
        return Ok(());
    }
    match client.send_user_message(session_id, message.clone()).await {
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
            Ok(())
        }
        Err(error) => {
            chat.app.restore_pending_submission(&message);
            chat.app.set_status(format!("send failed: {error}"));
            Ok(())
        }
    }
}
