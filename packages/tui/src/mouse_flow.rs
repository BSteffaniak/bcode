//! Chat mouse handling for the TUI.

use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui_components::text_input::TextInputOutcome;

use super::effects::TuiEffect;
use super::permission_dialog::PermissionDialogState;
use super::session_flow::ActiveChat;

/// Return the hit-region id under a mouse event.
#[must_use]
pub fn mouse_hit_id(hits: &bmux_tui::hit::HitMap, mouse: MouseEvent) -> Option<String> {
    hits.hit_mouse(mouse)
        .map(|hit| hit.id().as_str().to_owned())
}

/// Handle one non-modal mouse event that does not require daemon work.
pub fn handle_non_permission_mouse(
    hit_id: Option<&str>,
    chat: &mut ActiveChat,
    mouse: MouseEvent,
    scroll_rows: usize,
) -> bool {
    match mouse.kind {
        MouseEventKind::ScrollUp => match hit_id {
            Some("composer") => chat.app.previous_input_history(),
            _ => chat.app.scroll_transcript_up(scroll_rows),
        },
        MouseEventKind::ScrollDown => match hit_id {
            Some("composer") => chat.app.next_input_history(),
            _ => chat.app.scroll_transcript_down(scroll_rows),
        },
        MouseEventKind::Down(MouseButton::Left) if hit_id == Some("latest-bar") => {
            chat.app.transition_transcript_to_bottom()
        }
        MouseEventKind::Down(MouseButton::Left)
            if hit_id
                .and_then(crate::markdown_interaction::contribution_id_from_hit)
                .is_some() =>
        {
            let contribution_id = hit_id
                .and_then(crate::markdown_interaction::contribution_id_from_hit)
                .expect("guarded Markdown contribution hit");
            let focus_changed = chat.app.focus_markdown_contribution(contribution_id);
            let activated = chat.app.activate_markdown_contribution(contribution_id);
            focus_changed || activated
        }
        MouseEventKind::Down(MouseButton::Left) if hit_id == Some("composer") => {
            composer_mouse_changed(chat, mouse)
        }
        MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
            if chat.app.composer_mouse_selection_active() =>
        {
            composer_mouse_changed(chat, mouse)
        }
        MouseEventKind::Down(_)
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::Move
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => false,
    }
}

/// Handle a committed-frame permission action hit without blocking the root update task.
pub fn handle_permission_action_mouse(
    hit_id: Option<&str>,
    chat: &mut ActiveChat,
    permission_dialog: &mut Option<PermissionDialogState>,
    mouse: MouseEvent,
) -> bool {
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return false;
    };
    let Some(index) = hit_id.and_then(permission_action_index) else {
        return false;
    };
    let Some(dialog) = permission_dialog.as_mut() else {
        return false;
    };
    if !dialog.focus_action(index) {
        return false;
    }
    let permission_id = dialog.permission().permission_id.clone();
    let batch_id = dialog
        .permission()
        .batch
        .as_ref()
        .map(|batch| batch.batch_id.clone());
    let approved = dialog.focused_approval();
    let remember = dialog.focused_remember();
    let apply_to_batch = dialog.focused_batch();
    let label = dialog.focused_label();
    chat.start_effect(TuiEffect::ResolvePermission {
        permission_id,
        approved,
        remember,
        apply_to_batch,
        batch_id,
    });
    *permission_dialog = None;
    chat.app
        .set_status(format!("resolving permission: {label}"));
    true
}

fn permission_action_index(hit_id: &str) -> Option<usize> {
    hit_id.strip_prefix("permission-action:")?.parse().ok()
}

fn composer_mouse_changed(chat: &mut ActiveChat, mouse: MouseEvent) -> bool {
    matches!(
        chat.app.handle_composer_mouse(mouse),
        TextInputOutcome::Edited | TextInputOutcome::Redraw
    )
}
