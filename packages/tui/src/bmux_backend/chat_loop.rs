//! Main chat event loop for the BMUX backend.

use std::io::Write;

use bcode_client::BcodeClient;
use bcode_ipc::Event as BcodeEvent;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::crossterm::poll_event;
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;

use super::command_palette::BmuxCommandPalette;
use super::helpers;
use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use super::permission_dialog::PermissionDialogState;
use super::session_flow::ActiveChat;
use super::{
    EVENT_POLL_TIMEOUT, TuiError, command_palette_render, composer_flow, history_flow, input,
    mouse_flow, palette_flow, permission_dialog_render, permission_flow, render, slash_flow,
    slash_palette, slash_palette_render,
};

struct ModalState {
    palette: Option<BmuxCommandPalette>,
    slash_palette: Option<slash_palette::SlashPalette>,
    permission_dialog: Option<PermissionDialogState>,
}

/// Run the active chat UI loop.
pub(super) async fn run_with_client<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let mut modals = ModalState {
        palette: None,
        slash_palette: None,
        permission_dialog: None,
    };
    let mut needs_redraw = true;

    while !chat.app.should_exit() {
        while let Ok(event) = chat.event_receiver.try_recv() {
            match event {
                BcodeEvent::Session(event) if event.session_id == chat.session_id => {
                    chat.app.absorb_session_event(&event);
                    needs_redraw = true;
                }
                BcodeEvent::Session(_) => {}
            }
        }

        if chat.app.should_load_older_history() {
            history_flow::load_older_history(client, chat).await?;
            needs_redraw = true;
        }

        if modals.permission_dialog.is_none()
            && let Some(permission) = client
                .list_permissions()
                .await?
                .into_iter()
                .find(|permission| permission.session_id == chat.session_id)
        {
            modals.permission_dialog = Some(PermissionDialogState::new(permission));
            needs_redraw = true;
        }

        if helpers::resize_from_terminal(terminal)? {
            needs_redraw = true;
        }

        if needs_redraw {
            terminal.draw(|frame| {
                render::render(&mut chat.app, frame);
                if let Some(slash_palette) = &modals.slash_palette {
                    slash_palette_render::render_palette(
                        slash_palette,
                        chat.app.composer_content_area(),
                        frame,
                    );
                }
                if let Some(palette) = &mut modals.palette {
                    command_palette_render::render_palette(palette, frame);
                }
                if let Some(dialog) = &mut modals.permission_dialog {
                    permission_dialog_render::render_permission_dialog(dialog, frame);
                }
            })?;
            needs_redraw = false;
        }

        if let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? {
            if handle_event(client, keymap, chat, &mut modals, terminal, event).await? {
                needs_redraw = true;
            }
        } else if chat.app.tick() {
            needs_redraw = true;
        }
    }

    Ok(())
}

async fn handle_event<W: Write>(
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    modals: &mut ModalState,
    terminal: &mut Terminal<&mut W>,
    event: Event,
) -> Result<bool, TuiError> {
    match event {
        Event::Resize(size) => {
            terminal.resize(Rect::new(0, 0, size.width, size.height));
            Ok(true)
        }
        Event::Key(stroke) => handle_chat_key(client, keymap, chat, modals, terminal, stroke).await,
        Event::Paste(text) => {
            if let Some(palette) = &mut modals.palette {
                palette.state_mut().query.insert_str(&text);
                return Ok(true);
            }
            chat.app.composer_mut().insert_str(&text);
            chat.app.wake_cursor();
            slash_flow::update_slash_palette(client, chat, &mut modals.slash_palette).await;
            Ok(true)
        }
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick => Ok(true),
        Event::Mouse(mouse) => {
            if modals.palette.is_some() {
                return palette_flow::handle_palette_mouse(
                    client,
                    keymap,
                    chat,
                    &mut modals.palette,
                    terminal,
                    mouse,
                )
                .await;
            }
            if modals.slash_palette.is_some() {
                return Ok(slash_flow::handle_slash_palette_mouse(
                    chat,
                    &mut modals.slash_palette,
                    terminal,
                    mouse,
                ));
            }
            let hit_id = mouse_flow::mouse_hit_id(terminal.hits(), mouse);
            mouse_flow::handle_mouse(hit_id, client, chat, &mut modals.permission_dialog, mouse)
                .await
        }
        Event::User(_) => Ok(false),
    }
}

async fn handle_chat_key<W: Write>(
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    modals: &mut ModalState,
    terminal: &mut Terminal<&mut W>,
    stroke: KeyStroke,
) -> Result<bool, TuiError> {
    if modals.slash_palette.is_some() {
        return slash_flow::handle_slash_palette_key(
            client,
            keymap,
            chat,
            &mut modals.slash_palette,
            terminal,
            stroke,
        )
        .await;
    }
    let changed = match stroke.key {
        KeyCode::Char(']') if stroke.modifiers.is_empty() => chat.app.select_next_diff_file(),
        KeyCode::Char('[') if stroke.modifiers.is_empty() => chat.app.select_previous_diff_file(),
        _ => false,
    };
    if changed {
        return Ok(true);
    }
    if modals.permission_dialog.is_some() {
        return permission_flow::handle_permission_key(
            client,
            keymap,
            chat,
            &mut modals.permission_dialog,
            stroke,
        )
        .await;
    }
    if modals.palette.is_some() {
        return palette_flow::handle_palette_key(
            client,
            keymap,
            chat,
            &mut modals.palette,
            terminal,
            stroke,
        )
        .await;
    }
    if is_palette_open_key(keymap, stroke) {
        modals.palette = Some(BmuxCommandPalette::new());
        return Ok(true);
    }
    let outcome = input::handle_key(&mut chat.app, keymap, stroke);
    slash_flow::update_slash_palette(client, chat, &mut modals.slash_palette).await;
    if outcome.submitted
        && let Err(error) = composer_flow::submit_composer(client, keymap, chat, terminal).await
    {
        helpers::report_client_error(&mut chat.app, "send failed", &error);
    }
    Ok(outcome.redraw)
}

fn is_palette_open_key(keymap: &BmuxKeyMap, stroke: KeyStroke) -> bool {
    keymap.action_for_key(BmuxScope::Chat, stroke) == Some(BmuxAction::CommandPaletteOpen)
}
