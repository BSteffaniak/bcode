//! TUI worktree create dialog state.

use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};
use bmux_tui::geometry::Rect;
use bmux_tui_components::text_input::{TextInputPolicy, TextInputState};

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

/// Worktree create dialog state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCreateDialog {
    title: String,
    name_label: String,
    create_label: String,
    name: TextInputState,
    target: WorktreeCreateTarget,
    base: WorktreeCreateBase,
    focus: WorktreeCreateFocus,
    status: String,
    current_session_available: bool,
}

impl WorktreeCreateDialog {
    /// Create a worktree create dialog.
    #[must_use]
    pub fn new(default_name: &str, current_session_available: bool) -> Self {
        Self::new_with_labels(
            "Create worktree",
            "Name",
            "create",
            default_name,
            current_session_available,
            "Enter name, Tab changes field, ←/→ changes value, Enter creates",
        )
    }

    /// Create a Ralph loop setup dialog.
    #[must_use]
    pub fn new_ralph_loop(default_name: &str, current_session_available: bool) -> Self {
        Self::new_with_labels(
            "Start Ralph loop",
            "Ralph loop",
            "start",
            default_name,
            current_session_available,
            "Enter loop name, Tab changes field, ←/→ changes value, Enter starts",
        )
    }

    fn new_with_labels(
        title: &str,
        name_label: &str,
        create_label: &str,
        default_name: &str,
        current_session_available: bool,
        status: &str,
    ) -> Self {
        let mut name = TextEditBuffer::from_text(default_name);
        name.move_cursor_with_selection(TextMotion::Start, SelectionMode::Extend);
        let target = if current_session_available {
            WorktreeCreateTarget::CurrentSession
        } else {
            WorktreeCreateTarget::NewSession
        };
        Self {
            title: title.to_owned(),
            name_label: name_label.to_owned(),
            create_label: create_label.to_owned(),
            name: TextInputState::new(name),
            target,
            base: WorktreeCreateBase::Head,
            focus: WorktreeCreateFocus::Name,
            status: status.to_owned(),
            current_session_available,
        }
    }

    /// Return focused field.
    #[must_use]
    pub const fn focus(&self) -> WorktreeCreateFocus {
        self.focus
    }

    /// Return dialog title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Return label for the name field.
    #[must_use]
    pub fn name_label(&self) -> &str {
        &self.name_label
    }

    /// Return the action label shown in help text.
    #[must_use]
    pub fn create_label(&self) -> &str {
        &self.create_label
    }

    /// Return name input state.
    #[must_use]
    pub const fn name(&self) -> &TextInputState {
        &self.name
    }

    /// Return name input state mutably.
    pub const fn name_mut(&mut self) -> &mut TextInputState {
        &mut self.name
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

    /// Return the requested worktree name.
    #[must_use]
    pub fn name_text(&self) -> String {
        self.name.buffer().text().trim().to_owned()
    }

    /// Move focus to the next field.
    pub const fn focus_next(&mut self) {
        self.focus = match self.focus {
            WorktreeCreateFocus::Name => WorktreeCreateFocus::Target,
            WorktreeCreateFocus::Target => WorktreeCreateFocus::Base,
            WorktreeCreateFocus::Base => WorktreeCreateFocus::Name,
        };
    }

    /// Select previous value for the focused choice field.
    pub const fn previous_choice(&mut self) {
        match self.focus {
            WorktreeCreateFocus::Name => {}
            WorktreeCreateFocus::Target => self.previous_target(),
            WorktreeCreateFocus::Base => self.previous_base(),
        }
    }

    /// Select next value for the focused choice field.
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
