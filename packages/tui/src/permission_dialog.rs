//! TUI permission modal state.

use bcode_ipc::PermissionSummary;

/// Pending permission dialog state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDialogState {
    permission: PermissionSummary,
    focused_action: usize,
}

impl PermissionDialogState {
    /// Create state for a permission summary.
    #[must_use]
    pub const fn new(permission: PermissionSummary) -> Self {
        Self {
            permission,
            focused_action: 0,
        }
    }

    /// Return the permission summary.
    #[must_use]
    pub const fn permission(&self) -> &PermissionSummary {
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
        self.permission.can_remember_policy
            && (self.focused_action == 1 || self.focused_action == 3)
    }

    /// Return the currently focused action approval value.
    #[must_use]
    pub const fn focused_approval(&self) -> bool {
        if self.permission.can_remember_policy {
            self.focused_action < 2
        } else {
            self.focused_action == 0
        }
    }

    /// Return the currently focused action label.
    #[must_use]
    pub const fn focused_label(&self) -> &'static str {
        match (self.permission.can_remember_policy, self.focused_action) {
            (true, 0) => "approve once",
            (true, 1) => "remember allow",
            (true, 2) => "deny once",
            (true, 3) => "remember deny",
            (false, 0) => "approve",
            (true | false, _) => "deny",
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
        if self.permission.can_remember_policy {
            4
        } else {
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_ipc::PermissionSummary;
    use bcode_session_models::SessionId;
    use uuid::Uuid;

    fn permission(can_remember_policy: bool) -> PermissionSummary {
        PermissionSummary {
            permission_id: "perm".to_string(),
            session_id: SessionId(Uuid::nil()),
            tool_call_id: "call".to_string(),
            tool_name: "tool".to_string(),
            arguments_json: "{}".to_string(),
            batch: None,
            agent_id: "build".to_string(),
            policy_source: can_remember_policy.then(|| "skill".to_string()),
            policy_reason: can_remember_policy.then(|| "skill asks".to_string()),
            can_remember_policy,
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
}
