//! Composer input history state for the TUI.

use std::collections::BTreeSet;

use bcode_session_models::SessionInputHistoryEntry;

/// Result of moving through input history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputHistoryOutcome {
    /// A history entry was selected.
    Entry {
        /// One-based selected entry index.
        index: usize,
        /// Total entry count.
        total: usize,
        /// Selected entry text.
        text: String,
    },
    /// The draft from before history navigation was restored.
    DraftRestored(String),
    /// No history entries are available.
    Empty,
    /// Next was requested while not currently browsing history.
    NotBrowsing,
}

/// Input history plus draft restoration state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputHistory {
    entries: Vec<SessionInputHistoryEntry>,
    sequences: BTreeSet<u64>,
    index: Option<usize>,
    browse_len: Option<usize>,
    draft: Option<String>,
}

impl InputHistory {
    /// Create input history from session entries.
    #[must_use]
    pub fn from_entries(entries: &[SessionInputHistoryEntry]) -> Self {
        let entries = entries
            .iter()
            .filter(|entry| !entry.text.trim().is_empty())
            .cloned()
            .collect::<Vec<_>>();
        Self {
            sequences: entries.iter().map(|entry| entry.sequence).collect(),
            entries,
            index: None,
            browse_len: None,
            draft: None,
        }
    }

    /// Return all committed input-history entries.
    #[must_use]
    pub fn entries(&self) -> &[SessionInputHistoryEntry] {
        &self.entries
    }

    /// Push a committed user message and reset navigation state.
    pub fn push_committed(&mut self, sequence: u64, timestamp_ms: u64, text: &str) {
        if text.trim().is_empty() || !self.sequences.insert(sequence) {
            return;
        }
        self.entries.push(SessionInputHistoryEntry {
            sequence,
            timestamp_ms,
            text: text.to_owned(),
        });
    }

    /// Prepend older committed user messages in chronological order.
    pub fn prepend_committed(
        &mut self,
        messages: impl IntoIterator<Item = SessionInputHistoryEntry>,
    ) {
        let messages = messages
            .into_iter()
            .filter(|entry| !entry.text.trim().is_empty() && self.sequences.insert(entry.sequence))
            .collect::<Vec<_>>();
        if messages.is_empty() {
            return;
        }
        let added = messages.len();
        self.entries.splice(0..0, messages);
        if let Some(index) = &mut self.index {
            *index = index.saturating_add(added);
        }
        if let Some(browse_len) = &mut self.browse_len {
            *browse_len = browse_len.saturating_add(added);
        }
    }

    /// Return previous history entry and store draft when starting navigation.
    pub fn previous(&mut self, current_draft: &str) -> InputHistoryOutcome {
        if self.entries.is_empty() {
            return InputHistoryOutcome::Empty;
        }
        let browse_len = self.browse_len.unwrap_or(self.entries.len());
        let next_index = self.index.map_or_else(
            || browse_len.saturating_sub(1),
            |index| index.saturating_sub(1),
        );
        if self.index.is_none() {
            self.draft = Some(current_draft.to_owned());
            self.browse_len = Some(self.entries.len());
        }
        self.index = Some(next_index);
        self.entry_outcome(next_index)
    }

    /// Return next history entry or the saved draft.
    pub fn next(&mut self) -> InputHistoryOutcome {
        let Some(index) = self.index else {
            return InputHistoryOutcome::NotBrowsing;
        };
        let browse_len = self.browse_len.unwrap_or(self.entries.len());
        if index + 1 < browse_len {
            let next_index = index + 1;
            self.index = Some(next_index);
            self.entry_outcome(next_index)
        } else {
            self.index = None;
            self.browse_len = None;
            InputHistoryOutcome::DraftRestored(self.draft.take().unwrap_or_default())
        }
    }

    /// Return whether history navigation is active.
    #[must_use]
    pub const fn is_browsing(&self) -> bool {
        self.index.is_some()
    }

    /// Reset active history navigation.
    pub fn reset_navigation(&mut self) {
        self.index = None;
        self.browse_len = None;
        self.draft = None;
    }

    fn entry_outcome(&self, index: usize) -> InputHistoryOutcome {
        InputHistoryOutcome::Entry {
            index: index.saturating_add(1),
            total: self.entries.len(),
            text: self.entries[index].text.clone(),
        }
    }
}
