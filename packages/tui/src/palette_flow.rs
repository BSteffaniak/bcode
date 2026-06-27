//! Command palette flow for the TUI.

use std::collections::BTreeMap;
use std::io::Write;

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandEffect, InvokeCommandRequest,
    InvokeCommandResponse, OP_INVOKE_COMMAND,
};
use bmux_keyboard::KeyStroke;
use bmux_tui::palette::{CommandPalette, CommandPaletteKeyOutcome};

use super::command_palette::BmuxCommandPalette;
use super::effects::TuiEffect;
use super::helpers;
use super::picker_mouse::command_palette_row_from_mouse;
use super::runtime_context::{TuiIo, TuiServices};
use super::session_flow::{self, ActiveChat};
use super::{TuiError, session_fork_flow};

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
        } => dispatch_plugin_command(io, services, chat, plugin_id, command_id).await,
    }
}

async fn dispatch_plugin_command<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
    plugin_id: String,
    command_id: String,
) -> Result<(), TuiError> {
    let mut args = BTreeMap::new();
    if let Some(cwd) = chat.app.working_directory() {
        args.insert("cwd".to_string(), cwd.display().to_string());
    }
    if let Some(session_id) = chat.app.session_id() {
        args.insert("session_id".to_string(), session_id.to_string());
    }
    let payload = serde_json::to_vec(&InvokeCommandRequest { command_id, args })?;
    let response = services
        .passive_client
        .invoke_plugin_service(
            plugin_id.clone(),
            COMMAND_INTERFACE_ID.to_string(),
            OP_INVOKE_COMMAND.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let command_response = serde_json::from_slice::<InvokeCommandResponse>(&response.payload)?;
    if let Some(message) = command_response.message {
        chat.app.set_status(message);
    }
    for effect in command_response.effects {
        apply_command_effect(io, services, chat, &plugin_id, effect).await?;
    }
    Ok(())
}

async fn apply_command_effect<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    _services: &TuiServices<'_>,
    chat: &mut ActiveChat,
    plugin_id: &str,
    effect: CommandEffect,
) -> Result<(), TuiError> {
    match effect {
        CommandEffect::Status { message } => chat.app.set_status(message),
        CommandEffect::AppendText { text } => chat.app.push_system_note(text),
        CommandEffect::ToggleSurface { surface_id } => toggle_surface(chat, &surface_id),
        CommandEffect::OpenPluginSurface {
            surface_kind,
            instance_id,
            options,
        } => {
            open_command_plugin_surface(io, chat, plugin_id, surface_kind, instance_id, options)
                .await?;
        }
    }
    Ok(())
}

async fn open_command_plugin_surface<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    chat: &ActiveChat,
    plugin_id: &str,
    surface_kind: String,
    instance_id: String,
    options: serde_json::Value,
) -> Result<(), TuiError> {
    let runtime = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection::all_enabled(),
        &crate::static_bundled_plugins(),
    )
    .map_err(|error| TuiError::PluginService {
        code: "plugin_runtime_load_failed".to_string(),
        message: error.to_string(),
    })?;
    let mut surface = crate::plugin_tui::open_plugin_tui_surface(
        &runtime,
        plugin_id,
        &surface_kind,
        bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest {
            instance_id,
            repo_path: chat
                .app
                .working_directory()
                .map(std::path::Path::to_path_buf),
            target: None,
            options,
        },
    )
    .await
    .map_err(|error| TuiError::PluginService {
        code: "tui_surface_open_failed".to_string(),
        message: error.to_string(),
    })?;
    let _outcome = crate::plugin_surface_host::run_plugin_surface_with_input(
        io.terminal,
        io.input,
        surface.as_mut(),
    )
    .await?;
    Ok(())
}

fn toggle_surface(chat: &mut ActiveChat, surface_id: &str) {
    if surface_id == "diff" {
        let _changed = chat.app.toggle_diff_visible();
        chat.app.set_status(if chat.app.diff_visible() {
            "surface shown".to_owned()
        } else {
            "surface hidden".to_owned()
        });
    } else {
        chat.app
            .set_status(format!("surface toggle requested: {surface_id}"));
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

fn show_bmux_help(chat: &mut ActiveChat) {
    chat.app.push_system_note(
        [
            "TUI help",
            "* Use the command palette for sessions, plugin commands, cancel, and compact.",
            "* Transcript scrolling, composer history, session picker, and permissions honor configured keybindings where wired.",
            "* Permission dialogs: approve/deny or move focus and confirm.",
        ]
        .join("\n"),
    );
    chat.app.set_status("help shown".to_owned());
}
