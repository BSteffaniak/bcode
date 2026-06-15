//! TUI Ralph loop start dialog state.

use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};
use bmux_tui::geometry::Rect;
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
}

/// Ralph loop start dialog state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RalphStartDialog {
    loop_name: TextInputState,
    work_area_path: TextInputState,
    branch: TextInputState,
    focused_field: RalphStartDialogField,
    status: String,
}

impl RalphStartDialog {
    /// Create a Ralph loop start dialog.
    #[must_use]
    pub fn new(default_loop_name: &str) -> Self {
        let mut loop_name = TextEditBuffer::from_text(default_loop_name);
        loop_name.move_cursor_with_selection(TextMotion::Start, SelectionMode::Extend);
        Self {
            loop_name: TextInputState::new(loop_name),
            work_area_path: TextInputState::new(TextEditBuffer::default()),
            branch: TextInputState::new(TextEditBuffer::default()),
            focused_field: RalphStartDialogField::LoopName,
            status: "Enter starts, Tab switches optional fields, Esc cancels".to_owned(),
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
        }
    }

    /// Move focus to the next field.
    pub const fn focus_next(&mut self) {
        self.focused_field = match self.focused_field {
            RalphStartDialogField::LoopName => RalphStartDialogField::WorkAreaPath,
            RalphStartDialogField::WorkAreaPath => RalphStartDialogField::Branch,
            RalphStartDialogField::Branch => RalphStartDialogField::LoopName,
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

/// Return the text-input policy used by Ralph start fields.
#[must_use]
pub const fn input_policy() -> TextInputPolicy {
    TextInputPolicy::chat_composer()
}
