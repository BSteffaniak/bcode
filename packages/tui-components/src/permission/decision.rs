//! Shared native/browser permission decision policy, with no execution authority.
use bcode_session_view_models::PermissionView;

/// Permission-specific semantic commands resolved by the host keymap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionAction {
    /// Focus previous decision.
    SelectUp,
    /// Focus next decision.
    SelectDown,
    /// Activate focused decision.
    SelectConfirm,
    /// Approve once without persistence.
    PermissionApprove,
    /// Deny once.
    SelectCancel,
    /// Deny once without persistence.
    PermissionDeny,
    /// Not owned by this dialog.
    Other,
}

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
    pub const fn handle_action(&mut self, action: PermissionAction) -> PermissionDialogOutcome {
        match action {
            PermissionAction::SelectUp => {
                self.focus_previous();
                PermissionDialogOutcome::FocusChanged(self.focused_label())
            }
            PermissionAction::SelectDown => {
                self.focus_next();
                PermissionDialogOutcome::FocusChanged(self.focused_label())
            }
            PermissionAction::PermissionApprove => {
                PermissionDialogOutcome::Resolve(Self::resolution(true, false, false, "approve"))
            }
            PermissionAction::PermissionDeny | PermissionAction::SelectCancel => {
                PermissionDialogOutcome::Resolve(Self::resolution(false, false, false, "deny"))
            }
            PermissionAction::SelectConfirm => PermissionDialogOutcome::Resolve(Self::resolution(
                self.focused_approval(),
                self.focused_remember(),
                self.focused_batch(),
                self.focused_label(),
            )),
            PermissionAction::Other => PermissionDialogOutcome::Ignored,
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

    /// Number of available decisions for this checkpoint.
    #[must_use]
    pub const fn action_count(&self) -> usize {
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
