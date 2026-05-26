//! Slash completion palette flow for the TUI.

use std::io::Write;

use bcode_client::BcodeClient;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::terminal::Terminal;

use super::helpers;
use super::keymap::BmuxKeyMap;
use super::terminal_events::TerminalEventStream;
use super::{
    TuiError, composer_flow, input, session_flow::ActiveChat, slash_palette, slash_palette_render,
};

/// Refresh slash completions for the current composer text.
pub async fn update_slash_palette(
    client: &BcodeClient,
    chat: &ActiveChat,
    slash_palette: &mut Option<slash_palette::SlashPalette>,
) {
    if chat.app.composer().text().starts_with('/') {
        let previous = slash_palette
            .as_ref()
            .and_then(|palette| palette.selected_command().map(str::to_owned));
        let mut next = slash_palette::SlashPalette::new(
            client,
            chat.app.session_id(),
            chat.app.composer().text(),
        )
        .await;
        if let Some(previous) = previous {
            next.select_command(&previous);
        }
        *slash_palette = (!next.is_empty()).then_some(next);
    } else {
        *slash_palette = None;
    }
}

/// Handle one key while the slash completion palette is open.
pub async fn handle_slash_palette_key<W: Write>(
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    slash_palette: &mut Option<slash_palette::SlashPalette>,
    terminal: &mut Terminal<&mut W>,
    terminal_events: &mut TerminalEventStream,
    stroke: KeyStroke,
) -> Result<Option<composer_flow::SubmitComposerOutcome>, TuiError> {
    let Some(active_palette) = slash_palette else {
        return Ok(Some(None));
    };
    match stroke.key {
        KeyCode::Up if stroke.modifiers.is_empty() => {
            active_palette.move_previous();
            Ok(Some(None))
        }
        KeyCode::Down if stroke.modifiers.is_empty() => {
            active_palette.move_next();
            Ok(Some(None))
        }
        KeyCode::Tab if stroke.modifiers.is_empty() => {
            accept_slash_completion(chat, slash_palette);
            Ok(Some(None))
        }
        KeyCode::Enter if stroke.modifiers.is_empty() => {
            if active_palette.selected_matches(chat.app.composer().text()) {
                *slash_palette = None;
                match composer_flow::submit_composer(
                    client,
                    keymap,
                    chat,
                    terminal,
                    terminal_events,
                )
                .await
                {
                    Ok(outcome) => return Ok(Some(outcome)),
                    Err(error) => {
                        helpers::report_client_error(&mut chat.app, "send failed", &error);
                    }
                }
            } else {
                accept_slash_completion(chat, slash_palette);
            }
            Ok(Some(None))
        }
        KeyCode::Escape if stroke.modifiers.is_empty() => {
            *slash_palette = None;
            chat.app.set_status("slash completions hidden".to_owned());
            Ok(Some(None))
        }
        _ => {
            let outcome = input::handle_key(&mut chat.app, keymap, stroke);
            update_slash_palette(client, chat, slash_palette).await;
            if outcome.interrupted {
                request_turn_cancellation(client, chat).await;
            }
            if outcome.submitted {
                match composer_flow::submit_composer(
                    client,
                    keymap,
                    chat,
                    terminal,
                    terminal_events,
                )
                .await
                {
                    Ok(outcome) => return Ok(Some(outcome)),
                    Err(error) => {
                        helpers::report_client_error(&mut chat.app, "send failed", &error);
                    }
                }
            }
            Ok(Some(None))
        }
    }
}

/// Handle one mouse event while the slash completion palette is open.
pub fn handle_slash_palette_mouse<W: Write>(
    chat: &mut ActiveChat,
    slash_palette: &mut Option<slash_palette::SlashPalette>,
    terminal: &Terminal<&mut W>,
    mouse: MouseEvent,
) -> bool {
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return false;
    };
    let Some(active_palette) = slash_palette else {
        return false;
    };
    let Some(row) = slash_palette_render::slash_palette_row_from_mouse(
        terminal.area(),
        chat.app.composer_content_area(),
        mouse.position.x,
        mouse.position.y,
        active_palette.item_count(),
    ) else {
        *slash_palette = None;
        return true;
    };
    if let Some(command) = active_palette
        .select_visible_row(row, usize::from(terminal.area().height))
        .map(str::to_owned)
    {
        chat.app.reset_input_history_navigation();
        chat.app.replace_composer_with(&command);
        *slash_palette = None;
    }
    true
}

async fn request_turn_cancellation(client: &BcodeClient, chat: &mut ActiveChat) {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return;
    };
    match client.cancel_session_turn(session_id).await {
        Ok(true) => {
            chat.app.set_cancelling();
            chat.app
                .set_status("turn cancellation requested".to_owned());
        }
        Ok(false) => {
            chat.app.set_idle();
            chat.app.set_status("no active turn".to_owned());
        }
        Err(error) => {
            chat.app.set_idle();
            chat.app.set_status(format!("cancel failed: {error}"));
        }
    }
}

fn accept_slash_completion(
    chat: &mut ActiveChat,
    slash_palette: &mut Option<slash_palette::SlashPalette>,
) {
    let Some(active_palette) = slash_palette else {
        return;
    };
    if let Some(command) = active_palette.selected_command().map(str::to_owned) {
        chat.app.reset_input_history_navigation();
        chat.app.replace_composer_with(&command);
    } else {
        chat.app
            .set_status("no slash completion available".to_owned());
    }
    *slash_palette = None;
}
