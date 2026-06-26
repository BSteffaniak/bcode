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

    /// Return whether the focused action should remember the policy decision.
    #[must_use]
    pub const fn focused_remember(&self) -> bool {
        self.permission.can_remember_policy && self.focused_action == 1
    }

    /// Return the currently focused action approval value.
    #[must_use]
    pub const fn focused_approval(&self) -> bool {
        self.focused_action != 2
    }

    /// Return the currently focused action label.
    #[must_use]
    pub const fn focused_label(&self) -> &'static str {
        match (self.permission.can_remember_policy, self.focused_action) {
            (true, 0) => "approve once",
            (true, 1) => "remember allow",
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
            3
        } else {
            2
        }
    }
}
