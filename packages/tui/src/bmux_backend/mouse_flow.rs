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
        MouseEventKind::Down(MouseButton::Left) => {
            if hit_id.as_deref() == Some("composer") {
                if let Some((row, col)) =
                    composer_position_from_mouse(chat.app.composer_content_area(), mouse)
                {
                    let width = usize::from(chat.app.composer_content_area().width.max(1));
                    let row = row.saturating_add(chat.app.composer_scroll_offset());
                    chat.app.move_composer_to_wrapped_position(width, row, col);
                    Ok(true)
                } else {
                    Ok(false)
                }
            } else if hit_id.as_deref() == Some("diff-files") && chat.app.diff_visible() {
                if let Some(row) = diff_file_row_from_mouse(mouse) {
                    Ok(chat.app.select_diff_file(row))
                } else {
                    Ok(false)
                }
            } else {
                Ok(false)
            }
        }
        MouseEventKind::Down(MouseButton::Right | MouseButton::Middle | MouseButton::Other(_))
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::Move
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => Ok(false),
    }
}

fn composer_position_from_mouse(
    area: bmux_tui::geometry::Rect,
    mouse: MouseEvent,
) -> Option<(usize, usize)> {
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return None;
    };
    if mouse.position.y < area.y || mouse.position.y >= area.bottom() {
        return None;
    }
    if mouse.position.x < area.x || mouse.position.x >= area.right() {
        return None;
    }
    Some((
        usize::from(mouse.position.y.saturating_sub(area.y)),
        usize::from(mouse.position.x.saturating_sub(area.x)),
    ))
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
