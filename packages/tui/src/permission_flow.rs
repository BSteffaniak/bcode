//! Permission dialog input flow for the TUI.

use bcode_client::BcodeClient;
use bmux_keyboard::KeyStroke;
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};

use super::helpers;
use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use super::permission_dialog::PermissionDialogState;
use super::permission_dialog_render;
use super::{TuiError, session_flow::ActiveChat};

/// Handle one permission-dialog key.
pub async fn handle_permission_key(
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
            chat.app
                .set_status(format!("permission choice: {}", dialog.focused_label()));
            Ok(true)
        }
        BmuxAction::SelectDown => {
            dialog.focus_next();
            chat.app
                .set_status(format!("permission choice: {}", dialog.focused_label()));
            Ok(true)
        }
        BmuxAction::PermissionApprove => {
            resolve_permission_dialog(client, chat, permission_dialog, true, false, false).await
        }
        BmuxAction::PermissionDeny | BmuxAction::SelectCancel => {
            resolve_permission_dialog(client, chat, permission_dialog, false, false, false).await
        }
        BmuxAction::SelectConfirm => {
            let approved = dialog.focused_approval();
            let remember = dialog.focused_remember();
            let apply_to_batch = dialog.focused_batch();
            resolve_permission_dialog(
                client,
                chat,
                permission_dialog,
                approved,
                remember,
                apply_to_batch,
            )
            .await
        }
        BmuxAction::InputSubmitSteering
        | BmuxAction::InputSubmitFollowUp
        | BmuxAction::InputHistoryPrevious
        | BmuxAction::InputHistoryNext
        | BmuxAction::AppExit
        | BmuxAction::AppInterrupt
        | BmuxAction::ClipboardPasteImage
        | BmuxAction::CommandPaletteOpen
        | BmuxAction::AgentCycle
        | BmuxAction::DiffViewerLayoutCycle
        | BmuxAction::ThinkingEffortCycle
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
        | BmuxAction::EditorSelectLeft
        | BmuxAction::EditorSelectRight
        | BmuxAction::EditorSelectWordLeft
        | BmuxAction::EditorSelectWordRight
        | BmuxAction::EditorSelectUp
        | BmuxAction::EditorSelectDown
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
pub async fn handle_permission_mouse(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    permission_dialog: &mut Option<PermissionDialogState>,
    mouse: MouseEvent,
) -> Result<bool, TuiError> {
    if let Some(approve) = permission_click_approval(permission_dialog.as_ref(), mouse) {
        resolve_permission_dialog(client, chat, permission_dialog, approve, false, false).await
    } else {
        Ok(false)
    }
}

async fn resolve_permission_dialog(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    permission_dialog: &mut Option<PermissionDialogState>,
    approved: bool,
    remember: bool,
    apply_to_batch: bool,
) -> Result<bool, TuiError> {
    let Some(dialog) = permission_dialog.take() else {
        return Ok(false);
    };
    let permission_id = dialog.permission().permission_id.clone();
    let batch_id = dialog
        .permission()
        .batch
        .as_ref()
        .map(|batch| batch.batch_id.clone());
    let resolved = if apply_to_batch {
        let batch_id = batch_id.expect("batch action requires batch correlation");
        client.resolve_permission_batch(batch_id, approved).await? > 0
    } else {
        client
            .resolve_permission_with_remember(permission_id.clone(), approved, remember)
            .await?
    };
    chat.app.set_status(if resolved {
        if apply_to_batch {
            format!(
                "{} permission batch",
                if approved { "approved" } else { "denied" }
            )
        } else if approved {
            if remember {
                format!("approved and remembered permission {permission_id}")
            } else {
                format!("approved permission {permission_id}")
            }
        } else if remember {
            format!("denied and remembered permission {permission_id}")
        } else {
            format!("denied permission {permission_id}")
        }
    } else {
        format!("permission {permission_id} was already resolved")
    });
    Ok(true)
}

fn permission_click_approval(
    permission_dialog: Option<&PermissionDialogState>,
    mouse: MouseEvent,
) -> Option<bool> {
    if permission_dialog?.permission().batch.is_some() {
        return None;
    }
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return None;
    };
    let area = helpers::terminal_area().ok()?;
    let dialog = permission_dialog_render::dialog_area(area);
    let (approve_area, deny_area) = permission_dialog_render::action_areas(dialog);
    if approve_area.contains(mouse.position) {
        Some(true)
    } else if deny_area.contains(mouse.position) {
        Some(false)
    } else {
        None
    }
}
