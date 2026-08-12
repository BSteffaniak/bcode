//! TUI Ralph loop start dialog state.

use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};
use bmux_tui::geometry::Rect;
use bmux_tui_components::form::{Form, FormFieldItem, FormOutcome, FormPolicy, FormState};
use bmux_tui_components::text_input::{TextInputPolicy, TextInputState};

/// Focusable fields in the Ralph loop start dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RalphStartDialogField {
    /// User-facing Ralph loop name.
    LoopName,
    /// Optional explicit isolated work area path.
    WorkAreaPath,
    /// Optional explicit branch name.
    Branch,
    /// Optional validation commands separated by semicolons.
    ValidationCommands,
}

/// Ralph loop start dialog state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RalphStartDialog {
    loop_name: TextInputState,
    work_area_path: TextInputState,
    branch: TextInputState,
    validation_commands: TextInputState,
    focused_field: RalphStartDialogField,
    status: String,
    form: FormState,
}

impl RalphStartDialog {
    /// Create a Ralph loop start dialog.
    #[must_use]
    pub fn new(default_loop_name: &str, default_validation_commands: &[String]) -> Self {
        let mut loop_name = TextEditBuffer::from_text(default_loop_name);
        loop_name.move_cursor_with_selection(TextMotion::Start, SelectionMode::Extend);
        let validation_commands = default_validation_commands.join("; ");
        Self {
            loop_name: TextInputState::new(loop_name),
            work_area_path: TextInputState::new(TextEditBuffer::default()),
            branch: TextInputState::new(TextEditBuffer::default()),
            validation_commands: TextInputState::new(TextEditBuffer::from_text(
                &validation_commands,
            )),
            focused_field: RalphStartDialogField::LoopName,
            status: "Enter starts, Tab switches optional fields, Esc cancels".to_owned(),
            form: FormState::new(Some(0)),
        }
    }

    /// Return loop name input state.
    #[must_use]
    pub const fn loop_name(&self) -> &TextInputState {
        &self.loop_name
    }

    /// Return work area path input state.
    #[must_use]
    pub const fn work_area_path(&self) -> &TextInputState {
        &self.work_area_path
    }

    /// Return branch input state.
    #[must_use]
    pub const fn branch(&self) -> &TextInputState {
        &self.branch
    }

    /// Return validation commands input state.
    #[must_use]
    pub const fn validation_commands(&self) -> &TextInputState {
        &self.validation_commands
    }

    /// Return currently focused field.
    #[must_use]
    pub const fn focused_field(&self) -> RalphStartDialogField {
        self.focused_field
    }

    /// Return the focused input state mutably.
    pub const fn focused_input_mut(&mut self) -> &mut TextInputState {
        match self.focused_field {
            RalphStartDialogField::LoopName => &mut self.loop_name,
            RalphStartDialogField::WorkAreaPath => &mut self.work_area_path,
            RalphStartDialogField::Branch => &mut self.branch,
            RalphStartDialogField::ValidationCommands => &mut self.validation_commands,
        }
    }

    /// Move focus to the next field.
    pub fn focus_next(&mut self) {
        let fields = form_fields();
        let loop_name = self.loop_name.buffer().text().to_owned();
        let work_area_path = self.work_area_path.buffer().text().to_owned();
        let branch = self.branch.buffer().text().to_owned();
        let validation_commands = self.validation_commands.buffer().text().to_owned();
        let values = [
            Some(loop_name.as_str()),
            Some(work_area_path.as_str()),
            Some(branch.as_str()),
            Some(validation_commands.as_str()),
        ];
        if let FormOutcome::Focused(index) = Form::new(&fields, &values)
            .policy(FormPolicy::wrapping())
            .handle_event(&mut self.form, &bmux_tui::event::Event::Key(tab_stroke()))
        {
            self.set_focused_index(index);
        }
    }

    const fn set_focused_index(&mut self, index: usize) {
        self.focused_field = match index {
            1 => RalphStartDialogField::WorkAreaPath,
            2 => RalphStartDialogField::Branch,
            3 => RalphStartDialogField::ValidationCommands,
            _ => RalphStartDialogField::LoopName,
        };
    }

    /// Update the latest loop name input content area.
    pub fn set_loop_name_content_area(&mut self, area: Rect) {
        self.loop_name.set_content_area(area, &input_policy());
    }

    /// Update the work area path input content area.
    pub fn set_work_area_path_content_area(&mut self, area: Rect) {
        self.work_area_path.set_content_area(area, &input_policy());
    }

    /// Update the branch input content area.
    pub fn set_branch_content_area(&mut self, area: Rect) {
        self.branch.set_content_area(area, &input_policy());
    }

    /// Update the validation commands input content area.
    pub fn set_validation_commands_content_area(&mut self, area: Rect) {
        self.validation_commands
            .set_content_area(area, &input_policy());
    }

    /// Return the requested Ralph loop name.
    #[must_use]
    pub fn loop_name_text(&self) -> String {
        self.loop_name.buffer().text().trim().to_owned()
    }

    /// Return the optional custom work area path.
    #[must_use]
    pub fn work_area_path_text(&self) -> Option<String> {
        let text = self.work_area_path.buffer().text().trim().to_owned();
        (!text.is_empty()).then_some(text)
    }

    /// Return the optional custom branch name.
    #[must_use]
    pub fn branch_text(&self) -> Option<String> {
        let text = self.branch.buffer().text().trim().to_owned();
        (!text.is_empty()).then_some(text)
    }

    /// Return validation commands separated in the setup dialog.
    #[must_use]
    pub fn validation_command_texts(&self) -> Vec<String> {
        self.validation_commands
            .buffer()
            .text()
            .split(';')
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }

    /// Handle one terminal event and report whether the dialog should submit or cancel.
    pub fn handle_event(
        &mut self,
        event: &bmux_tui::event::Event,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> RalphStartDialogOutcome {
        use bmux_keyboard::KeyCode;
        use bmux_tui::event::Event;
        use bmux_tui_components::text_input::TextInputControl;

        match event {
            Event::Paste(text) => {
                let _ = TextInputControl::new(&input_policy())
                    .handle_paste(self.focused_input_mut(), text);
            }
            Event::Key(stroke) => match stroke.key {
                KeyCode::Escape => return RalphStartDialogOutcome::Canceled,
                KeyCode::Tab => self.focus_next(),
                KeyCode::Enter => return RalphStartDialogOutcome::Submit,
                _ => {
                    if let Some(motion) = keymap.editor_selection_motion_for_key(*stroke) {
                        self.focused_input_mut()
                            .buffer_mut()
                            .move_cursor_with_selection(motion, SelectionMode::Extend);
                        self.focused_input_mut()
                            .sync_scroll_to_cursor(&input_policy());
                    } else if let Some(command) = keymap.editor_command_for_key(*stroke) {
                        self.focused_input_mut().buffer_mut().apply_command(command);
                        self.focused_input_mut()
                            .sync_scroll_to_cursor(&input_policy());
                    } else {
                        let _ = TextInputControl::new(&input_policy())
                            .handle_key(self.focused_input_mut(), *stroke);
                    }
                }
            },
            Event::Mouse(mouse) => {
                let _ = TextInputControl::new(&input_policy())
                    .handle_mouse(self.focused_input_mut(), *mouse);
            }
            Event::Resize(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {}
        }
        RalphStartDialogOutcome::Handled
    }

    /// Return status text.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Set status text.
    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }
}

/// Outcome from one root-owned Ralph start dialog update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RalphStartDialogOutcome {
    /// Input was handled and the dialog remains open.
    Handled,
    /// Submit the current dialog values.
    Submit,
    /// Close without submitting.
    Canceled,
}

/// Return the text-input policy used by Ralph start fields.
#[must_use]
pub const fn input_policy() -> TextInputPolicy {
    TextInputPolicy::chat_composer()
}

fn form_fields() -> [FormFieldItem; 4] {
    [
        FormFieldItem::new("loop-name").required(true),
        FormFieldItem::new("work-area-path"),
        FormFieldItem::new("branch"),
        FormFieldItem::new("validation-commands"),
    ]
}

const fn tab_stroke() -> bmux_keyboard::KeyStroke {
    bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Tab)
}

#[cfg(test)]
mod tests {
    use super::{RalphStartDialog, RalphStartDialogField};

    #[test]
    fn form_state_drives_wrapping_tab_focus_order() {
        let mut dialog = RalphStartDialog::new("loop", &[]);

        assert_eq!(dialog.focused_field(), RalphStartDialogField::LoopName);
        dialog.focus_next();
        assert_eq!(dialog.focused_field(), RalphStartDialogField::WorkAreaPath);
        dialog.focus_next();
        assert_eq!(dialog.focused_field(), RalphStartDialogField::Branch);
        dialog.focus_next();
        assert_eq!(
            dialog.focused_field(),
            RalphStartDialogField::ValidationCommands
        );
        dialog.focus_next();
        assert_eq!(dialog.focused_field(), RalphStartDialogField::LoopName);
    }
}
