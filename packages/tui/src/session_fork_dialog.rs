//! TUI session fork/clone dialog state.

use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui_components::form::{Form, FormFieldItem, FormOutcome, FormState};
use bmux_tui_components::text_input::{TextInputControl, TextInputPolicy, TextInputState};

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

/// Outcome from one session fork/clone dialog event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionForkDialogOutcome {
    /// Dialog state changed and remains open.
    Handled,
    /// Submit the current dialog values.
    Submit(SessionForkDialogSubmission),
    /// Close without submitting.
    Canceled,
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
    form: FormState,
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
            form: FormState::new(Some(1)),
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
    pub fn focus_next(&mut self) {
        let fields = form_fields();
        let name = self.name.buffer().text().to_owned();
        let values = [
            Some(self.mode.label()),
            Some(name.as_str()),
            Some(""),
            Some(""),
        ];
        if let FormOutcome::Focused(index) = Form::new(&fields, &values)
            .handle_event(&mut self.form, &bmux_tui::event::Event::Key(tab_stroke()))
        {
            self.focus = match index {
                0 => SessionForkDialogFocus::Mode,
                2 => SessionForkDialogFocus::SwitchAfterCreate,
                3 => SessionForkDialogFocus::InstallDraft,
                _ => SessionForkDialogFocus::Name,
            };
        }
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

    /// Handle one terminal event through the dialog's component policies.
    pub fn handle_event(
        &mut self,
        event: &Event,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> SessionForkDialogOutcome {
        match event {
            Event::Paste(text) if self.focus == SessionForkDialogFocus::Name => {
                let _ =
                    TextInputControl::new(&name_input_policy()).handle_paste(&mut self.name, text);
            }
            Event::Key(stroke) => match stroke.key {
                bmux_keyboard::KeyCode::Escape => return SessionForkDialogOutcome::Canceled,
                bmux_keyboard::KeyCode::Tab => self.focus_next(),
                bmux_keyboard::KeyCode::Enter => {
                    return SessionForkDialogOutcome::Submit(self.submission());
                }
                bmux_keyboard::KeyCode::Left => self.value_previous(),
                bmux_keyboard::KeyCode::Right => self.value_next(),
                _ if self.focus == SessionForkDialogFocus::Name => {
                    if let Some(motion) = keymap.editor_selection_motion_for_key(*stroke) {
                        self.name
                            .buffer_mut()
                            .move_cursor_with_selection(motion, SelectionMode::Extend);
                        self.name.sync_scroll_to_cursor(&name_input_policy());
                    } else if let Some(command) = keymap.editor_command_for_key(*stroke) {
                        self.name.buffer_mut().apply_command(command);
                        self.name.sync_scroll_to_cursor(&name_input_policy());
                    } else {
                        let _ = TextInputControl::new(&name_input_policy())
                            .handle_key(&mut self.name, *stroke);
                    }
                }
                _ => {}
            },
            Event::Mouse(mouse) if self.focus == SessionForkDialogFocus::Name => {
                let _ = TextInputControl::new(&name_input_policy())
                    .handle_mouse(&mut self.name, *mouse);
            }
            Event::Focus(_)
            | Event::Resize(_)
            | Event::Tick
            | Event::User(_)
            | Event::Paste(_)
            | Event::Mouse(_) => {}
        }
        SessionForkDialogOutcome::Handled
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

fn form_fields() -> [FormFieldItem; 4] {
    [
        FormFieldItem::new("mode"),
        FormFieldItem::new("name"),
        FormFieldItem::new("switch-after-create"),
        FormFieldItem::new("install-draft"),
    ]
}

const fn tab_stroke() -> bmux_keyboard::KeyStroke {
    bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Tab)
}

#[cfg(test)]
mod tests {
    use super::{
        SessionForkDialog, SessionForkDialogFocus, SessionForkDialogMode, SessionForkDialogOutcome,
    };
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::event::Event;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyStroke::simple(code))
    }

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
    fn form_state_drives_wrapping_focus_order() {
        let mut dialog = SessionForkDialog::new(SessionForkDialogMode::Fork, "name");

        dialog.focus_next();
        assert_eq!(dialog.focus(), SessionForkDialogFocus::SwitchAfterCreate);
        dialog.focus_next();
        assert_eq!(dialog.focus(), SessionForkDialogFocus::InstallDraft);
        dialog.focus_next();
        assert_eq!(dialog.focus(), SessionForkDialogFocus::Mode);
        dialog.focus_next();
        assert_eq!(dialog.focus(), SessionForkDialogFocus::Name);
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

    #[test]
    fn event_policy_owns_focus_choice_submission_and_cancel() {
        let mut dialog = SessionForkDialog::new(SessionForkDialogMode::Fork, "custom");
        let keymap =
            super::super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

        assert_eq!(
            dialog.handle_event(&key(KeyCode::Tab), &keymap),
            SessionForkDialogOutcome::Handled
        );
        assert_eq!(dialog.focus(), SessionForkDialogFocus::SwitchAfterCreate);
        assert_eq!(
            dialog.handle_event(&key(KeyCode::Right), &keymap),
            SessionForkDialogOutcome::Handled
        );
        assert!(!dialog.switch_after_create());
        assert!(matches!(
            dialog.handle_event(&key(KeyCode::Enter), &keymap),
            SessionForkDialogOutcome::Submit(_)
        ));
        assert_eq!(
            dialog.handle_event(&key(KeyCode::Escape), &keymap),
            SessionForkDialogOutcome::Canceled
        );
    }
}
