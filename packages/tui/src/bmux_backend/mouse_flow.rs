//! Chat mouse handling for the BMUX backend.

use bcode_client::BcodeClient;
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};

use super::helpers;
use super::permission_dialog::PermissionDialogState;
use super::{MOUSE_WHEEL_ROWS, TuiError, permission_flow, session_flow::ActiveChat};

/// Return the hit-region id under a mouse event.
#[must_use]
pub(super) fn mouse_hit_id(hits: &bmux_tui::hit::HitMap, mouse: MouseEvent) -> Option<String> {
    hits.hit_mouse(mouse)
        .map(|hit| hit.id().as_str().to_owned())
}

/// Handle one non-modal mouse event.
pub(super) async fn handle_mouse(
    hit_id: Option<String>,
    client: &BcodeClient,
    chat: &mut ActiveChat,
    permission_dialog: &mut Option<PermissionDialogState>,
    mouse: MouseEvent,
) -> Result<bool, TuiError> {
    match mouse.kind {
        MouseEventKind::ScrollUp => match hit_id.as_deref() {
            Some("composer") => Ok(chat.app.previous_input_history()),
            Some("diff-files" | "diff-detail") if chat.app.diff_visible() => {
                Ok(chat.app.scroll_diff_up(MOUSE_WHEEL_ROWS))
            }
            _ => Ok(chat.app.scroll_transcript_up(MOUSE_WHEEL_ROWS)),
        },
        MouseEventKind::ScrollDown => match hit_id.as_deref() {
            Some("composer") => Ok(chat.app.next_input_history()),
            Some("diff-files" | "diff-detail") if chat.app.diff_visible() => {
                Ok(chat.app.scroll_diff_down(MOUSE_WHEEL_ROWS))
            }
            _ => Ok(chat.app.scroll_transcript_down(MOUSE_WHEEL_ROWS)),
        },
        MouseEventKind::Down(MouseButton::Left) if permission_dialog.is_some() => {
            permission_flow::handle_permission_mouse(client, chat, permission_dialog, mouse).await
        }
        MouseEventKind::Down(MouseButton::Left) if hit_id.as_deref() == Some("composer") => {
            Ok(begin_composer_selection(chat, mouse))
        }
        MouseEventKind::Drag(MouseButton::Left) if chat.app.composer_mouse_selection_active() => {
            Ok(extend_composer_selection(chat, mouse))
        }
        MouseEventKind::Up(MouseButton::Left) if chat.app.composer_mouse_selection_active() => {
            Ok(chat.app.end_composer_mouse_selection())
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

fn begin_composer_selection(chat: &mut ActiveChat, mouse: MouseEvent) -> bool {
    let Some((width, row, col)) = composer_position_from_mouse(chat, mouse) else {
        return false;
    };
    chat.app.begin_composer_mouse_selection(width, row, col);
    true
}

fn extend_composer_selection(chat: &mut ActiveChat, mouse: MouseEvent) -> bool {
    let Some((width, row, col)) = composer_position_from_mouse(chat, mouse) else {
        return false;
    };
    chat.app.extend_composer_mouse_selection(width, row, col)
}

fn composer_position_from_mouse(
    chat: &ActiveChat,
    mouse: MouseEvent,
) -> Option<(usize, usize, usize)> {
    let area = chat.app.composer_content_area();
    if mouse.position.y < area.y || mouse.position.y >= area.bottom() {
        return None;
    }
    if mouse.position.x < area.x || mouse.position.x >= area.right() {
        return None;
    }
    let width = usize::from(area.width.max(1));
    let row = usize::from(mouse.position.y.saturating_sub(area.y))
        .saturating_add(chat.app.composer_scroll_offset());
    let col = usize::from(mouse.position.x.saturating_sub(area.x));
    Some((width, row, col))
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
