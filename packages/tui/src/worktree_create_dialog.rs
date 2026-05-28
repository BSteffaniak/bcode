//! TUI worktree create dialog state.

use bmux_text_edit::TextEditBuffer;

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
    name: TextEditBuffer,
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
        let mut name = TextEditBuffer::new();
        name.insert_str(default_name);
        let target = if current_session_available {
            WorktreeCreateTarget::CurrentSession
        } else {
            WorktreeCreateTarget::NewSession
        };
        Self {
            name,
            target,
            base: WorktreeCreateBase::Head,
            focus: WorktreeCreateFocus::Name,
            status: "Enter name, Tab changes field, ←/→ changes value, Enter creates".to_owned(),
            current_session_available,
        }
    }

    /// Return focused field.
    #[must_use]
    pub const fn focus(&self) -> WorktreeCreateFocus {
        self.focus
    }

    /// Return name input.
    #[must_use]
    pub const fn name(&self) -> &TextEditBuffer {
        &self.name
    }

    /// Return name input mutably.
    pub const fn name_mut(&mut self) -> &mut TextEditBuffer {
        &mut self.name
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
        self.name.text().trim().to_owned()
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
