//! Native configured-keymap adapter for the shared permission decision policy.
use super::keymap::BmuxAction;
use bcode_session_view_models::PermissionView;
use bcode_tui_components::permission::decision::{
    PermissionAction, PermissionDialogState as SharedState,
};
pub use bcode_tui_components::permission::decision::{
    PermissionDialogOutcome, PermissionDialogResolution,
};

/// Native wrapper retaining the established dialog API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDialogState(SharedState);
impl std::ops::Deref for PermissionDialogState {
    type Target = SharedState;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for PermissionDialogState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl PermissionDialogState {
    /// Bind the shared policy to one pending checkpoint.
    #[must_use]
    pub const fn new(permission: PermissionView) -> Self {
        Self(SharedState::new(permission))
    }
    /// Resolve an already-configured native action through shared policy.
    pub const fn handle_action(&mut self, action: BmuxAction) -> PermissionDialogOutcome {
        self.0.handle_action(match action {
            BmuxAction::SelectUp => PermissionAction::SelectUp,
            BmuxAction::SelectDown => PermissionAction::SelectDown,
            BmuxAction::SelectConfirm => PermissionAction::SelectConfirm,
            BmuxAction::SelectCancel => PermissionAction::SelectCancel,
            BmuxAction::PermissionApprove => PermissionAction::PermissionApprove,
            BmuxAction::PermissionDeny => PermissionAction::PermissionDeny,
            _ => PermissionAction::Other,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::SessionId;
    use bcode_session_view_models::{PermissionBatchView, PermissionView};
    use uuid::Uuid;

    fn permission(can_remember_policy: bool) -> PermissionView {
        permission_with_batch(can_remember_policy, false)
    }

    fn permission_with_batch(can_remember_policy: bool, batched: bool) -> PermissionView {
        PermissionView {
            permission_id: "perm".to_string(),
            session_id: Some(SessionId(Uuid::nil())),
            tool_call_id: "call".to_string(),
            tool_name: "tool".to_string(),
            arguments_json: "{}".to_string(),
            batch: batched.then(|| PermissionBatchView {
                batch_id: "batch".to_string(),
                call_index: 1,
                call_count: 3,
            }),
            agent_id: "build".to_string(),
            title: Some("Permission requested: tool".to_owned()),
            policy_source: can_remember_policy.then(|| "skill".to_string()),
            detail: can_remember_policy.then(|| "skill asks".to_string()),
            resolved: false,
            approved: None,
            can_remember: can_remember_policy,
        }
    }

    #[test]
    fn action_cycle_without_remember_uses_two_actions() {
        let mut dialog = PermissionDialogState::new(permission(false));

        assert_eq!(dialog.focused_label(), "approve");
        assert!(dialog.focused_approval());
        assert!(!dialog.focused_remember());
        dialog.focus_next();
        assert_eq!(dialog.focused_label(), "deny");
        assert!(!dialog.focused_approval());
        dialog.focus_next();
        assert_eq!(dialog.focused_label(), "approve");
    }

    #[test]
    fn action_cycle_with_remember_uses_four_actions() {
        let mut dialog = PermissionDialogState::new(permission(true));

        assert_eq!(dialog.focused_label(), "approve once");
        assert!(dialog.focused_approval());
        assert!(!dialog.focused_remember());
        dialog.focus_next();
        assert_eq!(dialog.focused_label(), "remember allow");
        assert!(dialog.focused_approval());
        assert!(dialog.focused_remember());
        dialog.focus_next();
        assert_eq!(dialog.focused_label(), "deny once");
        assert!(!dialog.focused_approval());
        assert!(!dialog.focused_remember());
        dialog.focus_next();
        assert_eq!(dialog.focused_label(), "remember deny");
        assert!(!dialog.focused_approval());
        assert!(dialog.focused_remember());
    }

    #[test]
    fn batched_actions_keep_single_call_and_apply_to_all_distinct() {
        let mut dialog = PermissionDialogState::new(permission_with_batch(false, true));

        assert_eq!(dialog.focused_label(), "approve once");
        assert!(!dialog.focused_batch());
        dialog.focus_next();
        assert_eq!(dialog.focused_label(), "approve batch");
        assert!(dialog.focused_approval());
        assert!(dialog.focused_batch());
        dialog.focus_next();
        assert_eq!(dialog.focused_label(), "deny once");
        assert!(!dialog.focused_batch());
        dialog.focus_next();
        assert_eq!(dialog.focused_label(), "deny batch");
        assert!(!dialog.focused_approval());
        assert!(dialog.focused_batch());
    }

    #[test]
    fn batched_remember_actions_never_apply_to_all() {
        let mut dialog = PermissionDialogState::new(permission_with_batch(true, true));
        for expected in [
            ("approve once", false, false),
            ("approve batch", true, false),
            ("remember allow", false, true),
            ("deny once", false, false),
            ("deny batch", true, false),
            ("remember deny", false, true),
        ] {
            assert_eq!(dialog.focused_label(), expected.0);
            assert_eq!(dialog.focused_batch(), expected.1);
            assert_eq!(dialog.focused_remember(), expected.2);
            dialog.focus_next();
        }
    }

    #[test]
    fn semantic_actions_own_navigation_and_resolution_policy() {
        let mut dialog = PermissionDialogState::new(permission_with_batch(true, true));

        assert_eq!(
            dialog.handle_action(BmuxAction::SelectDown),
            PermissionDialogOutcome::FocusChanged("approve batch")
        );
        assert_eq!(
            dialog.handle_action(BmuxAction::SelectConfirm),
            PermissionDialogOutcome::Resolve(PermissionDialogResolution {
                approved: true,
                remember: false,
                apply_to_batch: true,
                label: "approve batch",
            })
        );
        assert_eq!(
            dialog.handle_action(BmuxAction::PermissionDeny),
            PermissionDialogOutcome::Resolve(PermissionDialogResolution {
                approved: false,
                remember: false,
                apply_to_batch: false,
                label: "deny",
            })
        );
        assert_eq!(
            dialog.handle_action(BmuxAction::AppExit),
            PermissionDialogOutcome::Ignored
        );
    }

    #[test]
    fn mouse_activation_returns_the_same_owned_resolution() {
        let mut dialog = PermissionDialogState::new(permission_with_batch(true, true));

        assert_eq!(
            dialog.activate_action(2),
            Some(PermissionDialogResolution {
                approved: true,
                remember: true,
                apply_to_batch: false,
                label: "remember allow",
            })
        );
        assert_eq!(dialog.activate_action(6), None);
    }
}
