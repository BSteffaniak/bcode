//! Command palette flow for the TUI.

use std::io::Write;

use bcode_worktree_models::WorktreeListRequest;
use bmux_keyboard::KeyStroke;
use bmux_tui::palette::{CommandPalette, CommandPaletteKeyOutcome};

use bcode_command::CommandAction;

use super::command_palette::BmuxCommandPalette;
use super::effects::TuiEffect;
use super::helpers;
use super::picker_mouse::command_palette_row_from_mouse;
use super::runtime_context::{TuiIo, TuiServices};
use super::session_flow::{self, ActiveChat};
use super::{TuiError, model_flow, session_fork_flow, skill_flow, worktree_flow};

/// Build a command palette from host and manifest-declared plugin command contributions.
pub async fn open_palette(services: &TuiServices<'_>, chat: &mut ActiveChat) -> BmuxCommandPalette {
    match services.passive_client.plugin_contributions().await {
        Ok(contributions) => {
            BmuxCommandPalette::with_command_contributions(contributions.command_contributions)
        }
        Err(error) => {
            chat.app.set_status(format!(
                "plugin commands unavailable; using host commands: {error}"
            ));
            BmuxCommandPalette::new()
        }
    }
}

/// Handle one key while the command palette is open.
pub async fn handle_palette_key<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
    palette: &mut Option<BmuxCommandPalette>,
    stroke: KeyStroke,
) -> Result<bool, TuiError> {
    let Some(active_palette) = palette else {
        return Ok(false);
    };
    let items = active_palette.cloned_items();
    let widget = CommandPalette::new(&items);
    let outcome = widget.handle_key(active_palette.state_mut(), 12, stroke);
    match outcome {
        CommandPaletteKeyOutcome::Ignored => Ok(false),
        CommandPaletteKeyOutcome::QueryEdited | CommandPaletteKeyOutcome::SelectionMoved => {
            Ok(true)
        }
        CommandPaletteKeyOutcome::Canceled => {
            *palette = None;
            Ok(true)
        }
        CommandPaletteKeyOutcome::Activated(index) => {
            let command = active_palette.command_at(index);
            *palette = None;
            if let Some(command) = command
                && let Err(error) = execute_palette_command(io, services, chat, command).await
            {
                helpers::report_client_error(&mut chat.app, "command failed", &error);
            }
            Ok(true)
        }
    }
}

/// Handle one mouse event while the command palette is open.
pub async fn handle_palette_mouse<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
    palette: &mut Option<BmuxCommandPalette>,
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
        && let Err(error) = execute_palette_command(io, services, chat, command).await
    {
        helpers::report_client_error(&mut chat.app, "command failed", &error);
    }
    Ok(true)
}

async fn execute_palette_command<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
    command: CommandAction,
) -> Result<(), TuiError> {
    match command {
        CommandAction::Host { route } => {
            dispatch_host_palette_route(io, services, chat, &route).await
        }
        CommandAction::Plugin {
            plugin_id,
            command_id,
        } => {
            chat.app.set_status(format!(
                "plugin command {command_id} from {plugin_id} selected"
            ));
            Ok(())
        }
    }
}

async fn dispatch_host_palette_route<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
    route: &str,
) -> Result<(), TuiError> {
    match route {
        "session.new" => start_new_session(chat)?,
        "session.switch" => switch_session(io, services, chat).await?,
        "session.rename" => rename_session(io, services, chat).await?,
        "session.delete" => delete_session(io, services, chat).await?,
        "session.fork" => session_fork_flow::fork_current_session(io, services, chat).await?,
        "session.clone" => session_fork_flow::clone_current_session(io, services, chat).await?,
        "command.work-tree.list" => show_worktrees(chat),
        "command.work-tree.createSession" => {
            worktree_flow::create_for_current_session(io, services, chat).await?;
        }
        "command.work-tree.attach" => {
            worktree_flow::attach_current_session(io, services, chat).await?;
        }
        "command.work-tree.remove" => {
            worktree_flow::remove_worktree(io, services, chat).await?;
        }
        "model.status" => model_flow::show_model_status(services.passive_client, chat).await?,
        "model.serverStatus" => {
            model_flow::show_server_model_status(services.passive_client, chat).await?;
        }
        "runtime.status" => model_flow::show_runtime_status(services.passive_client, chat).await?,
        "model.select" => model_flow::pick_model_for_session(io, services, chat).await?,
        "skills.list" => skill_flow::pick_skill_for_session(io, services, chat).await?,
        "skills.active" => skill_flow::show_active_skills(services.passive_client, chat).await?,
        "diff.toggle" => toggle_diff(chat),
        "help" => show_bmux_help(chat),
        "turn.cancel" => start_cancel_turn(chat),
        "context.compact" => start_compact_context(chat),
        unknown => chat
            .app
            .set_status(format!("unknown host command route: {unknown}")),
    }
    Ok(())
}

fn start_new_session(chat: &mut ActiveChat) -> Result<(), TuiError> {
    session_flow::switch_to_draft_session(chat);
    chat.replace_effect(TuiEffect::LoadDraftStatus {
        launch_working_directory: std::env::current_dir()?,
    });
    Ok(())
}

async fn switch_session<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    match session_flow::pick_session(io, services, chat).await? {
        session_flow::PickSessionOutcome::Existing(selected_session_id) => {
            session_flow::switch_session(io.terminal, chat, selected_session_id)?;
        }
        session_flow::PickSessionOutcome::Draft => start_new_session(chat)?,
    }
    Ok(())
}

async fn rename_session<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    session_flow::pick_session_for_mutation(
        io,
        services,
        chat,
        session_flow::SessionPickerStartMode::Rename,
    )
    .await
}

async fn delete_session<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    session_flow::pick_session_for_mutation(
        io,
        services,
        chat,
        session_flow::SessionPickerStartMode::Delete,
    )
    .await
}

fn toggle_diff(chat: &mut ActiveChat) {
    let _changed = chat.app.toggle_diff_visible();
    chat.app.set_status(if chat.app.diff_visible() {
        "diff panel shown".to_owned()
    } else {
        "diff panel hidden".to_owned()
    });
}

fn start_cancel_turn(chat: &mut ActiveChat) {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return;
    };
    chat.start_effect(TuiEffect::CancelTurn { session_id });
    chat.app.set_cancelling();
    chat.app.set_status("cancel requested".to_owned());
}

fn start_compact_context(chat: &mut ActiveChat) {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return;
    };
    chat.start_effect(TuiEffect::CompactContext { session_id });
    chat.app.set_status("compacting context…".to_owned());
}

fn show_worktrees(chat: &mut ActiveChat) {
    chat.start_effect(TuiEffect::ListWorktrees {
        request: WorktreeListRequest {
            cwd: chat
                .app
                .working_directory()
                .map(std::path::Path::to_path_buf),
        },
    });
    chat.app.set_status("loading worktrees…".to_owned());
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
    chat.app.set_status("help shown".to_owned());
}
