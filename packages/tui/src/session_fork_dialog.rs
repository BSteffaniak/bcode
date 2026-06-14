//! TUI session fork/clone dialog state.

use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};
use bmux_tui::geometry::Rect;
use bmux_tui_components::text_input::{TextInputPolicy, TextInputState};

/// Fork/clone operation kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionForkDialogMode {
    /// Copy history before a selected prompt and return that prompt as draft.
    Fork,
    /// Copy the full current conversation.
    Clone,
}

impl SessionForkDialogMode {
    /// Return display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fork => "fork",
            Self::Clone => "clone",
        }
    }

    const fn previous(self) -> Self {
        match self {
            Self::Fork => Self::Clone,
            Self::Clone => Self::Fork,
        }
    }

    const fn next(self) -> Self {
        self.previous()
    }
}

/// Focused field in the session fork dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionForkDialogFocus {
    /// Operation kind.
    Mode,
    /// New session name.
    Name,
    /// Switch after create option.
    SwitchAfterCreate,
    /// Install returned draft option.
    InstallDraft,
}

/// Resulting dialog submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionForkDialogSubmission {
    /// Selected operation kind.
    pub mode: SessionForkDialogMode,
    /// Optional explicit session name.
    pub name: Option<String>,
    /// Whether the TUI should switch to the new session after creating it.
    pub switch_after_create: bool,
    /// Whether returned draft text should be installed into the composer.
    pub install_draft: bool,
}

/// Session fork/clone dialog state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionForkDialog {
    mode: SessionForkDialogMode,
    name: TextInputState,
    switch_after_create: bool,
    install_draft: bool,
    focus: SessionForkDialogFocus,
    status: String,
}

impl SessionForkDialog {
    /// Create a dialog with sensible defaults.
    #[must_use]
    pub fn new(mode: SessionForkDialogMode, default_name: &str) -> Self {
        let mut name = TextEditBuffer::from_text(default_name);
        name.move_cursor_with_selection(TextMotion::Start, SelectionMode::Extend);
        Self {
            mode,
            name: TextInputState::new(name),
            switch_after_create: true,
            install_draft: mode == SessionForkDialogMode::Fork,
            focus: SessionForkDialogFocus::Name,
            status: "Enter name, Tab changes field, ←/→ changes value, Enter creates".to_owned(),
        }
    }

    /// Return selected operation kind.
    #[must_use]
    pub const fn mode(&self) -> SessionForkDialogMode {
        self.mode
    }

    /// Return focused field.
    #[must_use]
    pub const fn focus(&self) -> SessionForkDialogFocus {
        self.focus
    }

    /// Return dialog status.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Return name input state.
    #[must_use]
    pub const fn name(&self) -> &TextInputState {
        &self.name
    }

    /// Return mutable name input state.
    pub const fn name_mut(&mut self) -> &mut TextInputState {
        &mut self.name
    }

    /// Update latest name input content area.
    pub fn set_name_content_area(&mut self, area: Rect) {
        self.name.set_content_area(area, &name_input_policy());
    }

    /// Return current name text.
    #[must_use]
    pub fn name_text(&self) -> String {
        self.name.buffer().text().trim().to_owned()
    }

    /// Return whether switch-after-create is enabled.
    #[must_use]
    pub const fn switch_after_create(&self) -> bool {
        self.switch_after_create
    }

    /// Return whether draft install is enabled.
    #[must_use]
    pub const fn install_draft(&self) -> bool {
        self.install_draft
    }

    /// Move focus to the next field.
    pub const fn focus_next(&mut self) {
        self.focus = match self.focus {
            SessionForkDialogFocus::Mode => SessionForkDialogFocus::Name,
            SessionForkDialogFocus::Name => SessionForkDialogFocus::SwitchAfterCreate,
            SessionForkDialogFocus::SwitchAfterCreate => SessionForkDialogFocus::InstallDraft,
            SessionForkDialogFocus::InstallDraft => SessionForkDialogFocus::Mode,
        };
    }

    /// Move selected value backward for focused non-text fields.
    pub const fn value_previous(&mut self) {
        match self.focus {
            SessionForkDialogFocus::Mode => self.mode = self.mode.previous(),
            SessionForkDialogFocus::SwitchAfterCreate => {
                self.switch_after_create = !self.switch_after_create;
            }
            SessionForkDialogFocus::InstallDraft => self.install_draft = !self.install_draft,
            SessionForkDialogFocus::Name => {}
        }
    }

    /// Move selected value forward for focused non-text fields.
    pub const fn value_next(&mut self) {
        match self.focus {
            SessionForkDialogFocus::Mode => self.mode = self.mode.next(),
            SessionForkDialogFocus::SwitchAfterCreate => {
                self.switch_after_create = !self.switch_after_create;
            }
            SessionForkDialogFocus::InstallDraft => self.install_draft = !self.install_draft,
            SessionForkDialogFocus::Name => {}
        }
    }

    /// Convert current state into a submission.
    #[must_use]
    pub fn submission(&self) -> SessionForkDialogSubmission {
        let name = self.name_text();
        SessionForkDialogSubmission {
            mode: self.mode,
            name: (!name.is_empty()).then_some(name),
            switch_after_create: self.switch_after_create,
            install_draft: self.install_draft,
        }
    }
}

/// Text input policy for the session fork name field.
#[must_use]
pub const fn name_input_policy() -> TextInputPolicy {
    TextInputPolicy::chat_composer()
}

#[cfg(test)]
mod tests {
    use super::{SessionForkDialog, SessionForkDialogFocus, SessionForkDialogMode};

    #[test]
    fn fork_dialog_defaults_switch_and_install_draft() {
        let dialog = SessionForkDialog::new(SessionForkDialogMode::Fork, "[fork] source");

        assert_eq!(dialog.mode(), SessionForkDialogMode::Fork);
        assert_eq!(dialog.name_text(), "[fork] source");
        assert!(dialog.switch_after_create());
        assert!(dialog.install_draft());
        assert_eq!(dialog.focus(), SessionForkDialogFocus::Name);
    }

    #[test]
    fn clone_dialog_defaults_switch_and_carry_draft_disabled() {
        let dialog = SessionForkDialog::new(SessionForkDialogMode::Clone, "[clone] source");

        assert_eq!(dialog.mode(), SessionForkDialogMode::Clone);
        assert_eq!(dialog.name_text(), "[clone] source");
        assert!(dialog.switch_after_create());
        assert!(!dialog.install_draft());
    }

    #[test]
    fn dialog_submission_reflects_toggled_options() {
        let mut dialog = SessionForkDialog::new(SessionForkDialogMode::Fork, "custom");

        dialog.focus_next();
        assert_eq!(dialog.focus(), SessionForkDialogFocus::SwitchAfterCreate);
        dialog.value_next();
        dialog.focus_next();
        assert_eq!(dialog.focus(), SessionForkDialogFocus::InstallDraft);
        dialog.value_next();

        let submission = dialog.submission();
        assert_eq!(submission.mode, SessionForkDialogMode::Fork);
        assert_eq!(submission.name.as_deref(), Some("custom"));
        assert!(!submission.switch_after_create);
        assert!(!submission.install_draft);
    }
}
