//! Pending submission collection for TUI app state.

use super::pending_submission::PendingSubmission;

/// Pending submissions plus currently staged text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PendingSubmissions {
    items: Vec<PendingSubmission>,
    staged: Option<String>,
}

impl PendingSubmissions {
    /// Return pending submissions that have not been committed by the session stream.
    #[must_use]
    pub fn items(&self) -> &[PendingSubmission] {
        &self.items
    }

    /// Stage text as an in-flight submission.
    pub fn stage(&mut self, text: String) {
        self.staged = Some(text.clone());
        self.items.push(PendingSubmission::new(text));
    }

    /// Take the currently staged submission.
    pub fn take_staged(&mut self) -> String {
        self.staged.take().unwrap_or_default()
    }

    /// Clear the staged submission if it matches handled text.
    pub fn clear_staged_if(&mut self, text: &str) {
        if self.staged.as_deref() == Some(text) {
            self.staged = None;
        }
    }

    /// Return the oldest pending submission mutably.
    pub fn first_mut(&mut self) -> Option<&mut PendingSubmission> {
        self.items.first_mut()
    }

    /// Remove a pending submission by text.
    pub fn remove(&mut self, text: &str) {
        if let Some(index) = self.items.iter().position(|pending| pending.text() == text) {
            self.items.remove(index);
        }
    }
}
