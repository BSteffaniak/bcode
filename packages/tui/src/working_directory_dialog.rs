//! Session working-directory dialog state.

use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui_components::text_input::{TextInputControl, TextInputPolicy, TextInputState};
use std::path::{Path, PathBuf};

/// Outcome from one working-directory dialog event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkingDirectoryDialogOutcome {
    /// Dialog state changed and remains open.
    Handled,
    /// Apply the validated working directory.
    Apply(PathBuf),
    /// Close without changing the working directory.
    Canceled,
}

/// Working-directory dialog state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingDirectoryDialog {
    path: TextInputState,
    base: PathBuf,
    status: String,
    submitting: bool,
}

impl WorkingDirectoryDialog {
    /// Create a dialog rooted at the session's current working directory.
    #[must_use]
    pub fn new(current_working_directory: &Path) -> Self {
        let mut path =
            TextEditBuffer::from_text(current_working_directory.to_string_lossy().into_owned());
        path.move_cursor_with_selection(TextMotion::Start, SelectionMode::Extend);
        Self {
            path: TextInputState::new(path),
            base: current_working_directory.to_path_buf(),
            status: "Enter an existing directory, Enter applies, Esc cancels".to_owned(),
            submitting: false,
        }
    }

    /// Return the path input.
    #[must_use]
    pub const fn path(&self) -> &TextInputState {
        &self.path
    }

    /// Update the latest path input content area.
    pub fn set_path_content_area(&mut self, area: Rect) {
        self.path.set_content_area(area, &path_input_policy());
    }

    /// Return the current validation/status message.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Report a submission failure and allow another attempt.
    pub fn set_submission_error(&mut self, message: String) {
        self.submitting = false;
        self.status = message;
    }

    /// Handle one terminal event.
    pub fn handle_event(
        &mut self,
        event: &Event,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> WorkingDirectoryDialogOutcome {
        match event {
            Event::Paste(text) => {
                let _ =
                    TextInputControl::new(&path_input_policy()).handle_paste(&mut self.path, text);
                self.validate_for_display();
            }
            Event::Key(stroke) => match stroke.key {
                bmux_keyboard::KeyCode::Escape => return WorkingDirectoryDialogOutcome::Canceled,
                bmux_keyboard::KeyCode::Enter if !self.submitting => match self.validated_path() {
                    Ok(path) => {
                        self.submitting = true;
                        "Changing working directory…".clone_into(&mut self.status);
                        return WorkingDirectoryDialogOutcome::Apply(path);
                    }
                    Err(message) => self.status = message,
                },
                _ => {
                    if let Some(motion) = keymap.editor_selection_motion_for_key(*stroke) {
                        self.path
                            .buffer_mut()
                            .move_cursor_with_selection(motion, SelectionMode::Extend);
                        self.path.sync_scroll_to_cursor(&path_input_policy());
                    } else if let Some(command) = keymap.editor_command_for_key(*stroke) {
                        self.path.buffer_mut().apply_command(command);
                        self.path.sync_scroll_to_cursor(&path_input_policy());
                    } else {
                        let _ = TextInputControl::new(&path_input_policy())
                            .handle_key(&mut self.path, *stroke);
                    }
                    self.validate_for_display();
                }
            },
            Event::Mouse(mouse) => {
                let _ = TextInputControl::new(&path_input_policy())
                    .handle_mouse(&mut self.path, *mouse);
            }
            Event::Focus(_) | Event::Resize(_) | Event::Tick | Event::User(_) => {}
        }
        WorkingDirectoryDialogOutcome::Handled
    }

    fn validate_for_display(&mut self) {
        if !self.submitting {
            self.status = match self.validated_path() {
                Ok(path) => format!("Valid directory: {}", path.display()),
                Err(message) => message,
            };
        }
    }

    fn validated_path(&self) -> Result<PathBuf, String> {
        let value = self.path.buffer().text().trim();
        if value.is_empty() {
            return Err("A working directory is required".to_owned());
        }
        let requested = PathBuf::from(value);
        let resolved = if requested.is_absolute() {
            requested
        } else {
            self.base.join(requested)
        };
        let metadata = std::fs::metadata(&resolved)
            .map_err(|error| format!("Directory is not accessible: {error}"))?;
        if !metadata.is_dir() {
            return Err("Path exists but is not a directory".to_owned());
        }
        std::fs::canonicalize(&resolved)
            .map_err(|error| format!("Directory is not accessible: {error}"))
    }
}

/// Return the text-input policy used by the path field.
#[must_use]
pub const fn path_input_policy() -> TextInputPolicy {
    TextInputPolicy::chat_composer()
}

#[cfg(test)]
mod tests {
    use super::WorkingDirectoryDialog;
    use bmux_text_edit::TextEditBuffer;
    use bmux_tui_components::text_input::TextInputState;

    #[test]
    fn validates_existing_directories_and_rejects_missing_paths() {
        let root = tempfile::tempdir().expect("temporary directory");
        let mut dialog = WorkingDirectoryDialog::new(root.path());
        assert_eq!(
            dialog.validated_path().expect("existing directory"),
            std::fs::canonicalize(root.path()).expect("canonical path")
        );

        dialog.path = TextInputState::new(TextEditBuffer::from_text("missing"));
        assert!(dialog.validated_path().is_err());
    }

    #[test]
    fn resolves_relative_paths_against_current_session_directory() {
        let root = tempfile::tempdir().expect("temporary directory");
        let child = root.path().join("child");
        std::fs::create_dir(&child).expect("child directory");
        let mut dialog = WorkingDirectoryDialog::new(root.path());
        dialog.path = TextInputState::new(TextEditBuffer::from_text("child"));
        assert_eq!(
            dialog.validated_path().expect("relative directory"),
            std::fs::canonicalize(child).expect("canonical child")
        );
    }
}
