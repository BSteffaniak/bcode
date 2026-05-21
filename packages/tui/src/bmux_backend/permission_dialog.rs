//! BMUX backend permission modal state.

use bcode_ipc::PermissionSummary;
use bmux_tui::dialog::DialogState;

/// Pending permission dialog state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PermissionDialogState {
    permission: PermissionSummary,
    dialog: DialogState,
}

impl PermissionDialogState {
    /// Create state for a permission summary.
    #[must_use]
    pub(super) const fn new(permission: PermissionSummary) -> Self {
        Self {
            permission,
            dialog: DialogState { focused_action: 0 },
        }
    }

    /// Return the permission summary.
    #[must_use]
    pub(super) const fn permission(&self) -> &PermissionSummary {
        &self.permission
    }

    /// Return dialog state mutably.
    pub(super) const fn dialog_mut(&mut self) -> &mut DialogState {
        &mut self.dialog
    }

    /// Return the currently focused action approval value.
    #[must_use]
    pub(super) const fn focused_approval(&self) -> bool {
        self.dialog.focused_action == 0
    }

    /// Return the currently focused action label.
    #[must_use]
    pub(super) const fn focused_label(&self) -> &'static str {
        if self.focused_approval() {
            "approve"
        } else {
            "deny"
        }
    }

    /// Focus next action.
    pub(super) const fn focus_next(&mut self) {
        self.dialog.focus_next(2);
    }

    /// Focus previous action.
    pub(super) const fn focus_previous(&mut self) {
        self.dialog.focus_previous(2);
    }
}
