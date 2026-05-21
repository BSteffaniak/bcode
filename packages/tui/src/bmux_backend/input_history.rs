//! Composer input history state for the BMUX backend.

use bcode_session_models::SessionInputHistoryEntry;

/// Input history plus draft restoration state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InputHistory {
    entries: Vec<String>,
    index: Option<usize>,
    draft: Option<String>,
}

impl InputHistory {
    /// Create input history from session entries.
    #[must_use]
    pub(super) fn from_entries(entries: &[SessionInputHistoryEntry]) -> Self {
        Self {
            entries: entries.iter().map(|entry| entry.text.clone()).collect(),
            index: None,
            draft: None,
        }
    }

    /// Push a submitted input and reset navigation state.
    pub(super) fn push_submission(&mut self, text: String) {
        self.entries.push(text);
        self.index = None;
        self.draft = None;
    }

    /// Return previous history entry and store draft when starting navigation.
    pub(super) fn previous(&mut self, current_draft: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let next_index = self.index.map_or_else(
            || self.entries.len().saturating_sub(1),
            |index| index.saturating_sub(1),
        );
        if self.index.is_none() {
            self.draft = Some(current_draft.to_owned());
        }
        self.index = Some(next_index);
        Some(self.entries[next_index].clone())
    }

    /// Return next history entry or the saved draft.
    pub(super) fn next(&mut self) -> Option<String> {
        let index = self.index?;
        if index + 1 < self.entries.len() {
            let next_index = index + 1;
            self.index = Some(next_index);
            Some(self.entries[next_index].clone())
        } else {
            self.index = None;
            Some(self.draft.take().unwrap_or_default())
        }
    }
}
