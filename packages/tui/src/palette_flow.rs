//! Command palette flow for the TUI.

use std::io::Write;

use bcode_client::BcodeClient;
use bcode_worktree_models::WorktreeListRequest;
use bmux_keyboard::KeyStroke;
use bmux_tui::palette::{CommandPalette, CommandPaletteKeyOutcome};
use bmux_tui::terminal::Terminal;

use super::command_palette::{BmuxCommandPalette, PaletteCommand};
use super::helpers;
use super::keymap::BmuxKeyMap;
use super::picker_mouse::command_palette_row_from_mouse;
use super::session_flow::{self, ActiveChat};
use super::terminal_events::TuiInput;
use super::{TuiError, model_flow, skill_flow, worktree_flow};

/// Handle one key while the command palette is open.
pub async fn handle_palette_key<W: Write>(
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    palette: &mut Option<BmuxCommandPalette>,
    terminal: &mut Terminal<&mut W>,
    terminal_events: &mut TuiInput,
    stroke: KeyStroke,
) -> Result<bool, TuiError> {
    let Some(active_palette) = palette else {
        return Ok(false);
    };
    let items = active_palette.cloned_items();
    let widget = CommandPalette::new(&items);
    let outcome = widget.handle_key(active_palette.state_mut(), terminal.area().height, stroke);
    match outcome {
        CommandPaletteKeyOutcome::Activated(index) => {
            let command = active_palette.command_at(index);
            *palette = None;
            if let Some(command) = command
                && let Err(error) = execute_palette_command(
                    client,
                    chat,
                    terminal,
                    terminal_events,
                    keymap,
                    command,
                )
                .await
            {
                helpers::report_client_error(&mut chat.app, "command failed", &error);
            }
            Ok(true)
        }
        CommandPaletteKeyOutcome::Canceled => {
            *palette = None;
            Ok(true)
        }
        CommandPaletteKeyOutcome::Ignored => Ok(false),
        CommandPaletteKeyOutcome::QueryEdited | CommandPaletteKeyOutcome::SelectionMoved => {
            Ok(true)
        }
    }
}

/// Handle one mouse event while the command palette is open.
pub async fn handle_palette_mouse<W: Write>(
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    palette: &mut Option<BmuxCommandPalette>,
    terminal: &mut Terminal<&mut W>,
    terminal_events: &mut TuiInput,
    mouse: bmux_tui::event::MouseEvent,
) -> Result<bool, TuiError> {
    let Some(index) = command_palette_row_from_mouse(mouse) else {
        return Ok(false);
    };
    let Some(active_palette) = palette else {
        return Ok(false);
    };
    let command = active_palette.command_at(index);
    *palette = None;
    if let Some(command) = command
        && let Err(error) =
            execute_palette_command(client, chat, terminal, terminal_events, keymap, command).await
    {
        helpers::report_client_error(&mut chat.app, "command failed", &error);
    }
    Ok(true)
}

#[allow(clippy::too_many_lines)]
async fn execute_palette_command<W: Write>(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    terminal: &mut Terminal<&mut W>,
    terminal_events: &mut TuiInput,
    keymap: &BmuxKeyMap,
    command: PaletteCommand,
) -> Result<(), TuiError> {
    match command {
        PaletteCommand::NewSession => {
            let session = client.create_session(None).await?;
            session_flow::switch_session(client, chat, session.id).await?;
        }
        PaletteCommand::SwitchSession => {
            let selected_session_id = session_flow::pick_session(
                terminal,
                terminal_events,
                client,
                &BmuxKeyMap::from_config(&bcode_config::load_config()?.tui),
            )
            .await?;
            session_flow::switch_session(client, chat, selected_session_id).await?;
        }
        PaletteCommand::ListWorktrees => {
            show_worktrees(client, chat).await?;
        }
        PaletteCommand::CreateSessionWorktree => {
            worktree_flow::create_for_current_session(
                terminal,
                terminal_events,
                client,
                chat,
                keymap,
            )
            .await?;
        }
        PaletteCommand::AttachWorktree => {
            worktree_flow::attach_current_session(terminal, terminal_events, client, chat, keymap)
                .await?;
        }
        PaletteCommand::RemoveWorktree => {
            worktree_flow::remove_worktree(terminal, terminal_events, client, chat, keymap).await?;
        }
        PaletteCommand::ShowModelStatus => {
            model_flow::show_model_status(client, chat).await?;
        }
        PaletteCommand::ShowServerModelStatus => {
            model_flow::show_server_model_status(client, chat).await?;
        }
        PaletteCommand::ShowRuntimeStatus => {
            model_flow::show_runtime_status(client, chat).await?;
        }
        PaletteCommand::SelectModel => {
            model_flow::pick_model_for_session(terminal, terminal_events, client, chat, keymap)
                .await?;
        }
        PaletteCommand::ToggleDiff => {
            let _changed = chat.app.toggle_diff_visible();
            chat.app.set_status(if chat.app.diff_visible() {
                "diff panel shown".to_owned()
            } else {
                "diff panel hidden".to_owned()
            });
        }
        PaletteCommand::ListSkills => {
            skill_flow::pick_skill_for_session(terminal, terminal_events, client, chat, keymap)
                .await?;
        }
        PaletteCommand::ActiveSkills => {
            skill_flow::show_active_skills(client, chat).await?;
        }
        PaletteCommand::Help => {
            show_bmux_help(chat);
        }
        PaletteCommand::RenameSession => {
            session_flow::pick_session_for_mutation(
                terminal,
                terminal_events,
                client,
                session_flow::SessionPickerStartMode::Rename,
            )
            .await?;
        }
        PaletteCommand::DeleteSession => {
            session_flow::pick_session_for_mutation(
                terminal,
                terminal_events,
                client,
                session_flow::SessionPickerStartMode::Delete,
            )
            .await?;
        }
        PaletteCommand::CancelTurn => {
            let Some(session_id) = chat.app.session_id() else {
                chat.app.set_status("No active session".to_owned());
                return Ok(());
            };
            let cancelled = client.cancel_session_turn(session_id).await?;
            if cancelled {
                chat.app.set_cancelling();
                chat.app.set_status("cancel requested".to_owned());
            } else {
                chat.app.set_idle();
                chat.app.set_status("no active turn to cancel".to_owned());
            }
        }
        PaletteCommand::CompactContext => {
            let Some(session_id) = chat.app.session_id() else {
                chat.app.set_status("No active session".to_owned());
                return Ok(());
            };
            let message = client.compact_session(session_id).await?;
            chat.app.set_status(message);
        }
    }
    Ok(())
}

async fn show_worktrees(client: &BcodeClient, chat: &mut ActiveChat) -> Result<(), TuiError> {
    let response = client
        .list_worktrees(WorktreeListRequest {
            cwd: chat
                .app
                .working_directory()
                .map(std::path::Path::to_path_buf),
        })
        .await?;
    let lines = response
        .worktrees
        .into_iter()
        .map(|worktree| {
            let marker = if worktree.is_main { "main" } else { "linked" };
            let branch = worktree.branch.unwrap_or_else(|| "<detached>".to_string());
            format!("* {marker} {branch} — {}", worktree.path.display())
        })
        .collect::<Vec<_>>();
    chat.app.push_system_note(
        std::iter::once(format!("Worktrees for {}", response.repo_root.display()))
            .chain(lines)
            .collect::<Vec<_>>()
            .join("\n"),
    );
    chat.app.set_status("shown worktrees".to_owned());
    Ok(())
}

fn show_bmux_help(chat: &mut ActiveChat) {
    chat.app.push_system_note(
        [
            "TUI help",
            "* Use the command palette for sessions, model status, skills, cancel, and compact.",
            "* Transcript scrolling, composer history, session picker, and permissions honor configured keybindings where wired.",
            "* Permission dialogs: approve/deny or move focus and confirm.",
        ]
        .join("\n"),
    );
    chat.app.set_status("shown help".to_owned());
}
