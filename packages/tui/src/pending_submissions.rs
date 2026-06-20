//! Pending submission collection for TUI app state.

use super::pending_submission::PendingSubmission;

/// Pending submissions plus currently staged text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PendingSubmissions {
    items: Vec<PendingSubmission>,
    staged: Option<String>,
    revision: u64,
}

impl PendingSubmissions {
    /// Return pending submissions that have not been committed by the session stream.
    #[must_use]
    pub fn items(&self) -> &[PendingSubmission] {
        &self.items
    }

    /// Return revision for layout-affecting pending submission changes.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Stage text as an in-flight submission.
    pub fn stage(&mut self, text: String) {
        self.staged = Some(text.clone());
        self.items.push(PendingSubmission::new(text));
        self.bump_revision();
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

    /// Mark the oldest pending submission as queued.
    pub fn mark_first_queued(&mut self, queue_position: Option<u32>) {
        if let Some(pending) = self.items.first_mut() {
            pending.mark_queued(queue_position);
            self.bump_revision();
        }
    }

    /// Remove a pending submission by text.
    pub fn remove(&mut self, text: &str) {
        if let Some(index) = self.items.iter().position(|pending| pending.text() == text) {
            self.items.remove(index);
            self.bump_revision();
        }
    }

    const fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}
