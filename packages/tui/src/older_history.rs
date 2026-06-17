//! Older-history pagination state for the TUI app.

use bcode_session_models::{SessionEvent, SessionHistoryCursor};

/// Tracks older-history pagination and reveal requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OlderHistoryState {
    cursor: Option<SessionHistoryCursor>,
    reveal_request: Option<usize>,
    loading: bool,
}

impl OlderHistoryState {
    /// Create pagination state from replayed history.
    #[must_use]
    pub fn new(events: &[SessionEvent], has_older_history: bool) -> Self {
        Self {
            cursor: oldest_history_cursor(events, has_older_history),
            reveal_request: None,
            loading: false,
        }
    }

    /// Return whether older history may be available.
    #[must_use]
    pub const fn has_older_history(&self) -> bool {
        self.cursor.is_some()
    }

    /// Return whether an older-history request is in flight.
    #[must_use]
    pub const fn loading(&self) -> bool {
        self.loading
    }

    /// Mark older history as loading or idle.
    pub const fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
        if !loading {
            self.reveal_request = None;
        }
    }

    /// Return the cursor for loading older history.
    #[must_use]
    pub const fn cursor(&self) -> Option<SessionHistoryCursor> {
        self.cursor
    }

    /// Return whether an older-history request should be started.
    #[must_use]
    pub const fn should_load(&self) -> bool {
        self.cursor.is_some() && !self.loading && self.reveal_request.is_some()
    }

    /// Set cursor based on a loaded older page.
    pub fn update_cursor(&mut self, events: &[SessionEvent], has_more: bool) {
        self.cursor = oldest_history_cursor(events, has_more);
    }

    /// Mark previously resident events before `oldest_resident_sequence` as reloadable older history.
    pub const fn mark_dropped_history_before(&mut self, oldest_resident_sequence: u64) {
        self.cursor = if oldest_resident_sequence == 0 {
            None
        } else {
            Some(SessionHistoryCursor {
                sequence: oldest_resident_sequence.saturating_sub(1),
            })
        };
    }

    /// Request loading older history and reveal this many rows afterward.
    pub const fn request_load(&mut self, reveal_rows: usize) {
        self.reveal_request = Some(reveal_rows);
    }

    /// Take the pending reveal request.
    pub const fn take_reveal_request(&mut self) -> Option<usize> {
        self.reveal_request.take()
    }

    /// Return pending older-history reveal request rows.
    #[must_use]
    pub const fn reveal_request(&self) -> Option<usize> {
        self.reveal_request
    }

    /// Clear any pending reveal request.
    pub const fn clear_reveal_request(&mut self) {
        self.reveal_request = None;
    }
}

fn oldest_history_cursor(
    events: &[SessionEvent],
    has_older_history: bool,
) -> Option<SessionHistoryCursor> {
    if !has_older_history {
        return None;
    }
    let oldest_sequence = events.first()?.sequence;
    if oldest_sequence == 0 {
        None
    } else {
        Some(SessionHistoryCursor {
            sequence: oldest_sequence.saturating_sub(1),
        })
    }
}
