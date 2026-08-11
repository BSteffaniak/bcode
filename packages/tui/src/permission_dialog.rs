//! TUI permission modal state.

use bcode_session_view_models::PermissionView;

use super::keymap::BmuxAction;

/// Permission decision requested by the dialog's interaction policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionDialogResolution {
    /// Whether the permission should be approved.
    pub approved: bool,
    /// Whether the decision should be persisted as policy.
    pub remember: bool,
    /// Whether the decision applies to the complete permission batch.
    pub apply_to_batch: bool,
    /// User-facing label for the selected decision.
    pub label: &'static str,
}

/// Outcome from one permission-dialog semantic action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDialogOutcome {
    /// Focus changed and the status should present the selected label.
    FocusChanged(&'static str),
    /// Resolve the permission with the selected decision.
    Resolve(PermissionDialogResolution),
    /// The action is not owned by the permission dialog.
    Ignored,
}

/// Pending permission dialog state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDialogState {
    permission: PermissionView,
    focused_action: usize,
}

impl PermissionDialogState {
    /// Create state for a permission summary.
    #[must_use]
    pub const fn new(permission: PermissionView) -> Self {
        Self {
            permission,
            focused_action: 0,
        }
    }

    /// Return the permission summary.
    #[must_use]
    pub const fn permission(&self) -> &PermissionView {
        &self.permission
    }

    /// Return the pending permission identity.
    #[must_use]
    pub fn permission_id(&self) -> &str {
        &self.permission.permission_id
    }

    /// Return whether the focused action should remember the policy decision.
    #[must_use]
    pub const fn focused_remember(&self) -> bool {
        match (
            self.permission.batch.is_some(),
            self.permission.can_remember,
            self.focused_action,
        ) {
            (true, true, 2 | 5) | (false, true, 1 | 3) => true,
            (true | false, true | false, _) => false,
        }
    }

    /// Return whether the focused action applies to the complete authorization batch.
    #[must_use]
    pub const fn focused_batch(&self) -> bool {
        match (
            self.permission.batch.is_some(),
            self.permission.can_remember,
            self.focused_action,
        ) {
            (true, true, 1 | 4) | (true, false, 1 | 3) => true,
            (true | false, true | false, _) => false,
        }
    }

    /// Return the zero-based focused action index.
    #[must_use]
    pub const fn focused_action_index(&self) -> usize {
        self.focused_action
    }

    /// Return the currently focused action approval value.
    #[must_use]
    pub const fn focused_approval(&self) -> bool {
        match (
            self.permission.batch.is_some(),
            self.permission.can_remember,
        ) {
            (true, true) => self.focused_action < 3,
            (true, false) | (false, true) => self.focused_action < 2,
            (false, false) => self.focused_action == 0,
        }
    }

    /// Return the currently focused action label.
    #[must_use]
    pub const fn focused_label(&self) -> &'static str {
        match (
            self.permission.batch.is_some(),
            self.permission.can_remember,
            self.focused_action,
        ) {
            (true | false, true, 0) | (true, false, 0) => "approve once",
            (true, true | false, 1) => "approve batch",
            (true, true, 2) | (false, true, 1) => "remember allow",
            (true, true, 3) | (true, false, 2) | (false, true, 2) => "deny once",
            (true, true, 4) | (true, false, 3) => "deny batch",
            (true, true, 5) | (false, true, 3) => "remember deny",
            (false, false, 0) => "approve",
            (true | false, true | false, _) => "deny",
        }
    }

    /// Handle one semantic permission-dialog action.
    pub const fn handle_action(&mut self, action: BmuxAction) -> PermissionDialogOutcome {
        match action {
            BmuxAction::SelectUp => {
                self.focus_previous();
                PermissionDialogOutcome::FocusChanged(self.focused_label())
            }
            BmuxAction::SelectDown => {
                self.focus_next();
                PermissionDialogOutcome::FocusChanged(self.focused_label())
            }
            BmuxAction::PermissionApprove => {
                PermissionDialogOutcome::Resolve(Self::resolution(true, false, false, "approve"))
            }
            BmuxAction::PermissionDeny | BmuxAction::SelectCancel => {
                PermissionDialogOutcome::Resolve(Self::resolution(false, false, false, "deny"))
            }
            BmuxAction::SelectConfirm => PermissionDialogOutcome::Resolve(Self::resolution(
                self.focused_approval(),
                self.focused_remember(),
                self.focused_batch(),
                self.focused_label(),
            )),
            _ => PermissionDialogOutcome::Ignored,
        }
    }

    const fn resolution(
        approved: bool,
        remember: bool,
        apply_to_batch: bool,
        label: &'static str,
    ) -> PermissionDialogResolution {
        PermissionDialogResolution {
            approved,
            remember,
            apply_to_batch,
            label,
        }
    }

    /// Focus one action by zero-based index and return its resolution.
    #[must_use]
    pub const fn activate_action(&mut self, index: usize) -> Option<PermissionDialogResolution> {
        if !self.focus_action(index) {
            return None;
        }
        Some(Self::resolution(
            self.focused_approval(),
            self.focused_remember(),
            self.focused_batch(),
            self.focused_label(),
        ))
    }

    /// Focus one action by zero-based index.
    ///
    /// Returns whether the requested action exists.
    pub const fn focus_action(&mut self, index: usize) -> bool {
        if index >= self.action_count() {
            return false;
        }
        self.focused_action = index;
        true
    }

    /// Focus next action.
    pub const fn focus_next(&mut self) {
        self.focused_action = self.focused_action.saturating_add(1) % self.action_count();
    }

    /// Focus previous action.
    pub const fn focus_previous(&mut self) {
        if self.focused_action == 0 {
            self.focused_action = self.action_count().saturating_sub(1);
        } else {
            self.focused_action = self.focused_action.saturating_sub(1);
        }
    }

    const fn action_count(&self) -> usize {
        match (
            self.permission.batch.is_some(),
            self.permission.can_remember,
        ) {
            (true, true) => 6,
            (true, false) | (false, true) => 4,
            (false, false) => 2,
        }
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
