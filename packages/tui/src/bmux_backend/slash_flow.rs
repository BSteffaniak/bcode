//! Slash completion palette flow for the BMUX backend.

use std::io::Write;

use bcode_client::BcodeClient;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::terminal::Terminal;

use super::keymap::BmuxKeyMap;
use super::{
    ActiveChat, TuiError, composer_flow, input, report_client_error, slash_palette,
    slash_palette_render,
};

/// Refresh slash completions for the current composer text.
pub(super) async fn update_slash_palette(
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
pub(super) async fn handle_slash_palette_key<W: Write>(
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    slash_palette: &mut Option<slash_palette::SlashPalette>,
    terminal: &mut Terminal<&mut W>,
    stroke: KeyStroke,
) -> Result<bool, TuiError> {
    let Some(active_palette) = slash_palette else {
        return Ok(false);
    };
    match stroke.key {
        KeyCode::Up if stroke.modifiers.is_empty() => {
            active_palette.move_previous();
            Ok(true)
        }
        KeyCode::Down if stroke.modifiers.is_empty() => {
            active_palette.move_next();
            Ok(true)
        }
        KeyCode::Tab if stroke.modifiers.is_empty() => {
            accept_slash_completion(chat, slash_palette);
            Ok(true)
        }
        KeyCode::Enter if stroke.modifiers.is_empty() => {
            if active_palette.selected_matches(chat.app.composer().text()) {
                *slash_palette = None;
                if let Err(error) =
                    composer_flow::submit_composer(client, keymap, chat, terminal).await
                {
                    report_client_error(&mut chat.app, "send failed", &error);
                }
            } else {
                accept_slash_completion(chat, slash_palette);
            }
            Ok(true)
        }
        KeyCode::Escape if stroke.modifiers.is_empty() => {
            *slash_palette = None;
            Ok(true)
        }
        _ => {
            let outcome = input::handle_key(&mut chat.app, keymap, stroke);
            update_slash_palette(client, chat, slash_palette).await;
            if outcome.submitted
                && let Err(error) =
                    composer_flow::submit_composer(client, keymap, chat, terminal).await
            {
                report_client_error(&mut chat.app, "send failed", &error);
            }
            Ok(outcome.redraw || slash_palette.is_some())
        }
    }
}

/// Handle one mouse event while the slash completion palette is open.
pub(super) fn handle_slash_palette_mouse<W: Write>(
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
        chat.app.replace_composer_with(&command);
        *slash_palette = None;
    }
    true
}

fn accept_slash_completion(
    chat: &mut ActiveChat,
    slash_palette: &mut Option<slash_palette::SlashPalette>,
) {
    let Some(active_palette) = slash_palette else {
        return;
    };
    if let Some(command) = active_palette.selected_command().map(str::to_owned) {
        chat.app.replace_composer_with(&command);
    }
    *slash_palette = None;
}
