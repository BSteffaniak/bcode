//! Chat mouse handling for the TUI.

use bcode_client::BcodeClient;
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui_components::text_input::TextInputOutcome;

use super::helpers;
use super::permission_dialog::PermissionDialogState;
use super::{TuiError, permission_flow, session_flow::ActiveChat};

/// Return the hit-region id under a mouse event.
#[must_use]
pub fn mouse_hit_id(hits: &bmux_tui::hit::HitMap, mouse: MouseEvent) -> Option<String> {
    hits.hit_mouse(mouse)
        .map(|hit| hit.id().as_str().to_owned())
}

/// Handle one non-modal mouse event.
pub async fn handle_mouse(
    hit_id: Option<String>,
    client: &BcodeClient,
    chat: &mut ActiveChat,
    permission_dialog: &mut Option<PermissionDialogState>,
    mouse: MouseEvent,
    scroll_rows: usize,
) -> Result<bool, TuiError> {
    match mouse.kind {
        MouseEventKind::ScrollUp => match hit_id.as_deref() {
            Some("composer") => Ok(chat.app.previous_input_history()),
            Some("diff-files" | "diff-detail") if chat.app.diff_visible() => {
                Ok(chat.app.scroll_diff_up(scroll_rows))
            }
            _ => Ok(chat.app.scroll_transcript_up(scroll_rows)),
        },
        MouseEventKind::ScrollDown => match hit_id.as_deref() {
            Some("composer") => Ok(chat.app.next_input_history()),
            Some("diff-files" | "diff-detail") if chat.app.diff_visible() => {
                Ok(chat.app.scroll_diff_down(scroll_rows))
            }
            _ => Ok(chat.app.scroll_transcript_down(scroll_rows)),
        },
        MouseEventKind::Down(MouseButton::Left) if hit_id.as_deref() == Some("latest-bar") => {
            Ok(chat.app.transition_transcript_to_bottom())
        }
        MouseEventKind::Down(MouseButton::Left) if permission_dialog.is_some() => {
            permission_flow::handle_permission_mouse(client, chat, permission_dialog, mouse).await
        }
        MouseEventKind::Down(MouseButton::Left) if hit_id.as_deref() == Some("composer") => {
            Ok(composer_mouse_changed(chat, mouse))
        }
        MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
            if chat.app.composer_mouse_selection_active() =>
        {
            Ok(composer_mouse_changed(chat, mouse))
        }
        MouseEventKind::Down(MouseButton::Left) if hit_id.as_deref() == Some("diff-files") => {
            if chat.app.diff_visible()
                && let Some(row) = diff_file_row_from_mouse(mouse)
            {
                Ok(chat.app.select_diff_file(row))
            } else {
                Ok(false)
            }
        }
        MouseEventKind::Down(
            MouseButton::Left | MouseButton::Right | MouseButton::Middle | MouseButton::Other(_),
        )
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::Move
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => Ok(false),
    }
}

fn composer_mouse_changed(chat: &mut ActiveChat, mouse: MouseEvent) -> bool {
    matches!(
        chat.app.handle_composer_mouse(mouse),
        TextInputOutcome::Edited | TextInputOutcome::Redraw
    )
}

fn diff_file_row_from_mouse(mouse: MouseEvent) -> Option<usize> {
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return None;
    };
    let area = helpers::terminal_area().ok()?;
    let diff_top = area.height.saturating_sub(12);
    if mouse.position.y < diff_top {
        return None;
    }
    Some(usize::from(
        mouse.position.y.saturating_sub(diff_top).saturating_sub(1),
    ))
}
