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

    /// Show native input held by the daemon, oldest first. Queued rows the daemon no longer
    /// holds were sent or withdrawn and leave; held input missing a row (after a restart) gets
    /// one. `held` pairs each row identity with its text.
    pub fn sync_held(&mut self, held: &[(bcode_session_models::SubmissionId, String)]) {
        self.items.retain(|item| {
            !matches!(
                item.state(),
                super::pending_submission::PendingSubmissionState::Queued { .. }
            ) || held.iter().any(|(id, _)| id == item.id())
        });
        for (id, text) in held {
            if !self.items.iter().any(|item| item.id() == id) {
                self.items
                    .push(PendingSubmission::queued(id.clone(), text.clone()));
            }
        }
    }

    /// Mark the pending submission with this identity as queued.
    pub fn mark_queued_id(&mut self, id: &bcode_session_models::SubmissionId) {
        if let Some(pending) = self.items.iter_mut().find(|item| item.id() == id) {
            pending.mark_queued(None);
        }
    }

    /// Mark the oldest pending submission as queued.
    pub fn mark_first_queued(&mut self, queue_position: Option<u32>) {
        if let Some(pending) = self.items.first_mut() {
            pending.mark_queued(queue_position);
        }
    }

    /// Mark the oldest pending submission as sent.
    pub fn mark_first_sent(&mut self) {
        if let Some(pending) = self.items.first_mut() {
            pending.mark_sent();
        }
    }

    /// Reconcile an authoritative acceptance by identity.
    pub fn remove_id(&mut self, id: &bcode_session_models::SubmissionId) {
        self.items.retain(|item| item.id() != id);
    }

    /// Remove a pending submission by text.
    pub fn remove(&mut self, text: &str) {
        if let Some(index) = self.items.iter().position(|pending| pending.text() == text) {
            self.items.remove(index);
        }
    }
}
