//! TUI worktree create dialog state.

use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui_components::form::{Form, FormFieldItem, FormOutcome, FormState};
use bmux_tui_components::text_input::{TextInputControl, TextInputPolicy, TextInputState};

/// Focused field in the worktree create dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeCreateFocus {
    /// Worktree/task name field.
    Name,
    /// Session target field.
    Target,
    /// Base ref strategy field.
    Base,
}

/// Validated worktree-create submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCreateSubmission {
    /// Requested worktree name.
    pub name: String,
    /// Session placement target.
    pub target: WorktreeCreateTarget,
    /// Base-ref strategy.
    pub base: WorktreeCreateBase,
}

/// Outcome from one worktree-create dialog event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeCreateDialogOutcome {
    /// Dialog state changed and remains open.
    Handled,
    /// Submit the validated worktree request.
    Create(WorktreeCreateSubmission),
    /// Close without creating a worktree.
    Canceled,
}

/// Worktree create dialog state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCreateDialog {
    name: TextInputState,
    target: WorktreeCreateTarget,
    base: WorktreeCreateBase,
    focus: WorktreeCreateFocus,
    status: String,
    current_session_available: bool,
    form: FormState,
}

impl WorktreeCreateDialog {
    /// Create a worktree create dialog.
    #[must_use]
    pub fn new(default_name: &str, current_session_available: bool) -> Self {
        let mut name = TextEditBuffer::from_text(default_name);
        name.move_cursor_with_selection(TextMotion::Start, SelectionMode::Extend);
        let target = if current_session_available {
            WorktreeCreateTarget::CurrentSession
        } else {
            WorktreeCreateTarget::NewSession
        };
        Self {
            name: TextInputState::new(name),
            target,
            base: WorktreeCreateBase::Head,
            focus: WorktreeCreateFocus::Name,
            status: "Enter name, Tab changes field, ←/→ changes value, Enter creates".to_owned(),
            current_session_available,
            form: FormState::new(Some(0)),
        }
    }

    /// Return focused field.
    #[must_use]
    pub const fn focus(&self) -> WorktreeCreateFocus {
        self.focus
    }

    /// Return name input state.
    #[must_use]
    pub const fn name(&self) -> &TextInputState {
        &self.name
    }

    /// Update the latest name input content area.
    pub fn set_name_content_area(&mut self, area: Rect) {
        self.name.set_content_area(area, &name_input_policy());
    }

    /// Return selected session target.
    #[must_use]
    pub const fn target(&self) -> WorktreeCreateTarget {
        self.target
    }

    /// Return selected base ref.
    #[must_use]
    pub const fn base(&self) -> WorktreeCreateBase {
        self.base
    }

    /// Return status text.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Replace the status text shown by the dialog.
    pub fn set_status(&mut self, status: String) {
        self.status = status;
    }

    /// Return the requested worktree name.
    #[must_use]
    pub fn name_text(&self) -> String {
        self.name.buffer().text().trim().to_owned()
    }

    /// Handle one terminal event through the dialog's component policies.
    pub fn handle_event(
        &mut self,
        event: &Event,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> WorktreeCreateDialogOutcome {
        match event {
            Event::Paste(text) if self.focus == WorktreeCreateFocus::Name => {
                let _ =
                    TextInputControl::new(&name_input_policy()).handle_paste(&mut self.name, text);
            }
            Event::Key(stroke) => match stroke.key {
                bmux_keyboard::KeyCode::Escape => return WorktreeCreateDialogOutcome::Canceled,
                bmux_keyboard::KeyCode::Tab => self.focus_next(),
                bmux_keyboard::KeyCode::Enter => {
                    let name = self.name_text();
                    if name.is_empty() {
                        self.set_status("worktree name is required".to_owned());
                    } else {
                        return WorktreeCreateDialogOutcome::Create(WorktreeCreateSubmission {
                            name,
                            target: self.target,
                            base: self.base,
                        });
                    }
                }
                bmux_keyboard::KeyCode::Left if self.focus != WorktreeCreateFocus::Name => {
                    self.previous_choice();
                }
                bmux_keyboard::KeyCode::Right if self.focus != WorktreeCreateFocus::Name => {
                    self.next_choice();
                }
                _ if self.focus == WorktreeCreateFocus::Name => {
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
            Event::Mouse(mouse) if self.focus == WorktreeCreateFocus::Name => {
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
        WorktreeCreateDialogOutcome::Handled
    }

    /// Move focus to the next field.
    pub fn focus_next(&mut self) {
        let fields = form_fields();
        let name = self.name.buffer().text().to_owned();
        let values = [
            Some(name.as_str()),
            Some(self.target.label()),
            Some(self.base.label()),
        ];
        if let FormOutcome::Focused(index) = Form::new(&fields, &values)
            .handle_event(&mut self.form, &bmux_tui::event::Event::Key(tab_stroke()))
        {
            self.focus = match index {
                1 => WorktreeCreateFocus::Target,
                2 => WorktreeCreateFocus::Base,
                _ => WorktreeCreateFocus::Name,
            };
        }
    }

    /// Move current choice backward.
    pub const fn previous_choice(&mut self) {
        match self.focus {
            WorktreeCreateFocus::Name => {}
            WorktreeCreateFocus::Target => self.previous_target(),
            WorktreeCreateFocus::Base => self.previous_base(),
        }
    }

    /// Move current choice forward.
    pub const fn next_choice(&mut self) {
        match self.focus {
            WorktreeCreateFocus::Name => {}
            WorktreeCreateFocus::Target => self.next_target(),
            WorktreeCreateFocus::Base => self.next_base(),
        }
    }

    const fn previous_target(&mut self) {
        self.target = self.target.previous(self.current_session_available);
    }

    const fn next_target(&mut self) {
        self.target = self.target.next(self.current_session_available);
    }

    const fn previous_base(&mut self) {
        self.base = self.base.previous();
    }

    const fn next_base(&mut self) {
        self.base = self.base.next();
    }
}

/// Return the text-input policy used by the worktree name field.
#[must_use]
pub const fn name_input_policy() -> TextInputPolicy {
    TextInputPolicy::chat_composer()
}

fn form_fields() -> [FormFieldItem; 3] {
    [
        FormFieldItem::new("name").required(true),
        FormFieldItem::new("target"),
        FormFieldItem::new("base"),
    ]
}

const fn tab_stroke() -> bmux_keyboard::KeyStroke {
    bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Tab)
}

/// Worktree session target in the create dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeCreateTarget {
    /// Move the current session into the created worktree.
    CurrentSession,
    /// Create and switch to a new session rooted at the worktree.
    NewSession,
}

impl WorktreeCreateTarget {
    const fn previous(self, current_session_available: bool) -> Self {
        self.next(current_session_available)
    }

    const fn next(self, current_session_available: bool) -> Self {
        if !current_session_available {
            return Self::NewSession;
        }
        match self {
            Self::CurrentSession => Self::NewSession,
            Self::NewSession => Self::CurrentSession,
        }
    }

    /// Return display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CurrentSession => "current_session",
            Self::NewSession => "new_session",
        }
    }
}

/// Worktree base strategy in the create dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeCreateBase {
    /// Context-sensitive default.
    Auto,
    /// Repository default branch.
    DefaultBranch,
    /// Current HEAD.
    Head,
}

impl WorktreeCreateBase {
    const fn previous(self) -> Self {
        match self {
            Self::Auto => Self::Head,
            Self::DefaultBranch => Self::Auto,
            Self::Head => Self::DefaultBranch,
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Auto => Self::DefaultBranch,
            Self::DefaultBranch => Self::Head,
            Self::Head => Self::Auto,
        }
    }

    /// Return display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::DefaultBranch => "default_branch",
            Self::Head => "head",
        }
    }

    /// Return model value.
    #[must_use]
    pub const fn model(self) -> bcode_worktree_models::WorktreeBaseRef {
        match self {
            Self::Auto => bcode_worktree_models::WorktreeBaseRef::Auto,
            Self::DefaultBranch => bcode_worktree_models::WorktreeBaseRef::DefaultBranch,
            Self::Head => bcode_worktree_models::WorktreeBaseRef::Head,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WorktreeCreateDialog, WorktreeCreateDialogOutcome, WorktreeCreateFocus};
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::event::Event;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyStroke::simple(code))
    }

    #[test]
    fn form_state_drives_wrapping_focus_order() {
        let mut dialog = WorktreeCreateDialog::new("work", true);

        assert_eq!(dialog.focus(), WorktreeCreateFocus::Name);
        dialog.focus_next();
        assert_eq!(dialog.focus(), WorktreeCreateFocus::Target);
        dialog.focus_next();
        assert_eq!(dialog.focus(), WorktreeCreateFocus::Base);
        dialog.focus_next();
        assert_eq!(dialog.focus(), WorktreeCreateFocus::Name);
    }

    #[test]
    fn event_policy_owns_validation_focus_choices_submission_and_cancel() {
        let keymap =
            super::super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
        let mut empty = WorktreeCreateDialog::new("", true);
        assert_eq!(
            empty.handle_event(&key(KeyCode::Enter), &keymap),
            WorktreeCreateDialogOutcome::Handled
        );
        assert_eq!(empty.status(), "worktree name is required");

        let mut dialog = WorktreeCreateDialog::new("work", true);
        assert_eq!(
            dialog.handle_event(&key(KeyCode::Tab), &keymap),
            WorktreeCreateDialogOutcome::Handled
        );
        assert_eq!(dialog.focus(), WorktreeCreateFocus::Target);
        assert_eq!(
            dialog.handle_event(&key(KeyCode::Right), &keymap),
            WorktreeCreateDialogOutcome::Handled
        );
        assert!(matches!(
            dialog.handle_event(&key(KeyCode::Enter), &keymap),
            WorktreeCreateDialogOutcome::Create(_)
        ));
        assert_eq!(
            dialog.handle_event(&key(KeyCode::Escape), &keymap),
            WorktreeCreateDialogOutcome::Canceled
        );
    }
}
