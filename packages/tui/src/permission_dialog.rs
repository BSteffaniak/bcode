//! TUI permission modal state.

use bcode_session_view_models::PermissionView;

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
}
