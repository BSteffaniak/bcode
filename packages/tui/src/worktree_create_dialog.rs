//! TUI worktree create dialog state.

use bmux_text_edit::TextEditBuffer;

/// Focused field in the worktree create dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeCreateFocus {
    /// Worktree/task name field.
    Name,
    /// Base ref strategy field.
    Base,
}

/// Worktree create dialog state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCreateDialog {
    name: TextEditBuffer,
    base: WorktreeCreateBase,
    focus: WorktreeCreateFocus,
    status: String,
}

impl WorktreeCreateDialog {
    /// Create a worktree create dialog.
    #[must_use]
    pub fn new(default_name: &str) -> Self {
        let mut name = TextEditBuffer::new();
        name.insert_str(default_name);
        Self {
            name,
            base: WorktreeCreateBase::Head,
            focus: WorktreeCreateFocus::Name,
            status: "Enter name, Tab changes field, ←/→ changes base, Enter creates".to_owned(),
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
            WorktreeCreateFocus::Name => WorktreeCreateFocus::Base,
            WorktreeCreateFocus::Base => WorktreeCreateFocus::Name,
        };
    }

    /// Select previous base strategy.
    pub const fn previous_base(&mut self) {
        self.base = self.base.previous();
    }

    /// Select next base strategy.
    pub const fn next_base(&mut self) {
        self.base = self.base.next();
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
