//! Permission dialog input flow for the BMUX backend.

use bcode_client::BcodeClient;
use bmux_keyboard::KeyStroke;
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};

use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use super::permission_dialog::PermissionDialogState;
use super::{TuiError, session_flow::ActiveChat, terminal_area};

/// Handle one permission-dialog key.
pub(super) async fn handle_permission_key(
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
    chat: &mut ActiveChat,
    permission_dialog: &mut Option<PermissionDialogState>,
    stroke: KeyStroke,
) -> Result<bool, TuiError> {
    let Some(dialog) = permission_dialog else {
        return Ok(false);
    };
    let Some(action) = keymap.action_for_key(BmuxScope::Permission, stroke) else {
        return Ok(false);
    };
    match action {
        BmuxAction::SelectUp => {
            dialog.focus_previous();
            Ok(true)
        }
        BmuxAction::SelectDown => {
            dialog.focus_next();
            Ok(true)
        }
        BmuxAction::PermissionApprove => {
            resolve_permission_dialog(client, chat, permission_dialog, true).await
        }
        BmuxAction::PermissionDeny | BmuxAction::SelectCancel => {
            resolve_permission_dialog(client, chat, permission_dialog, false).await
        }
        BmuxAction::SelectConfirm => {
            let approved = dialog.focused_approval();
            resolve_permission_dialog(client, chat, permission_dialog, approved).await
        }
        BmuxAction::InputSubmit
        | BmuxAction::InputHistoryPrevious
        | BmuxAction::InputHistoryNext
        | BmuxAction::AppExit
        | BmuxAction::AppInterrupt
        | BmuxAction::CommandPaletteOpen
        | BmuxAction::TranscriptPageUp
        | BmuxAction::TranscriptPageDown
        | BmuxAction::TranscriptTop
        | BmuxAction::TranscriptBottom
        | BmuxAction::TranscriptLineUp
        | BmuxAction::TranscriptLineDown
        | BmuxAction::SessionNew
        | BmuxAction::SessionRename
        | BmuxAction::SessionDelete
        | BmuxAction::InputNewLine
        | BmuxAction::EditorMoveLeft
        | BmuxAction::EditorMoveRight
        | BmuxAction::EditorMoveWordLeft
        | BmuxAction::EditorMoveWordRight
        | BmuxAction::EditorMoveStart
        | BmuxAction::EditorMoveEnd
        | BmuxAction::EditorDeleteBackward
        | BmuxAction::EditorDeleteForward
        | BmuxAction::EditorDeleteWordBackward
        | BmuxAction::EditorDeleteWordForward
        | BmuxAction::EditorDeleteToStart
        | BmuxAction::EditorDeleteToEnd
        | BmuxAction::SkillInvoke
        | BmuxAction::SkillActivate
        | BmuxAction::SkillDeactivate
        | BmuxAction::SkillHelp => Ok(false),
    }
}

/// Resolve a left-click against the permission dialog buttons.
pub(super) async fn handle_permission_mouse(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    permission_dialog: &mut Option<PermissionDialogState>,
    mouse: MouseEvent,
) -> Result<bool, TuiError> {
    if let Some(approve) = permission_click_approval(mouse) {
        resolve_permission_dialog(client, chat, permission_dialog, approve).await
    } else {
        Ok(false)
    }
}

async fn resolve_permission_dialog(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    permission_dialog: &mut Option<PermissionDialogState>,
    approved: bool,
) -> Result<bool, TuiError> {
    let Some(dialog) = permission_dialog.take() else {
        return Ok(false);
    };
    let permission_id = dialog.permission().permission_id.clone();
    let resolved = client
        .resolve_permission(permission_id.clone(), approved)
        .await?;
    chat.app.set_status(if resolved {
        if approved {
            format!("approved permission {permission_id}")
        } else {
            format!("denied permission {permission_id}")
        }
    } else {
        format!("permission {permission_id} was already resolved")
    });
    Ok(true)
}

fn permission_click_approval(mouse: MouseEvent) -> Option<bool> {
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return None;
    };
    let area = terminal_area().ok()?;
    let dialog_width = area.width.saturating_sub(4).min(76);
    let dialog_height = area.height.saturating_sub(4).min(14);
    let dialog_x = area
        .x
        .saturating_add(area.width.saturating_sub(dialog_width) / 2);
    let dialog_y = area
        .y
        .saturating_add(area.height.saturating_sub(dialog_height) / 3);
    let button_y = dialog_y.saturating_add(dialog_height).saturating_sub(3);
    if mouse.position.y != button_y {
        return None;
    }
    let approve_start = dialog_x.saturating_add(2);
    let approve_end = approve_start.saturating_add(12);
    let deny_start = approve_end.saturating_add(2);
    let deny_end = deny_start.saturating_add(9);
    if (approve_start..approve_end).contains(&mouse.position.x) {
        Some(true)
    } else if (deny_start..deny_end).contains(&mouse.position.x) {
        Some(false)
    } else {
        None
    }
}
