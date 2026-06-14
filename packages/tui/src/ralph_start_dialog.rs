//! TUI Ralph loop start dialog state.

use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};
use bmux_tui::geometry::Rect;
use bmux_tui_components::text_input::{TextInputPolicy, TextInputState};

/// Ralph loop start dialog state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RalphStartDialog {
    loop_name: TextInputState,
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
            status: "Enter Ralph loop name, Enter starts, Esc cancels".to_owned(),
        }
    }

    /// Return loop name input state.
    #[must_use]
    pub const fn loop_name(&self) -> &TextInputState {
        &self.loop_name
    }

    /// Return loop name input state mutably.
    pub const fn loop_name_mut(&mut self) -> &mut TextInputState {
        &mut self.loop_name
    }

    /// Update the latest loop name input content area.
    pub fn set_loop_name_content_area(&mut self, area: Rect) {
        self.loop_name
            .set_content_area(area, &loop_name_input_policy());
    }

    /// Return the requested Ralph loop name.
    #[must_use]
    pub fn loop_name_text(&self) -> String {
        self.loop_name.buffer().text().trim().to_owned()
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

/// Return the text-input policy used by the Ralph loop name field.
#[must_use]
pub const fn loop_name_input_policy() -> TextInputPolicy {
    TextInputPolicy::chat_composer()
}
