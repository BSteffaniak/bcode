//! Bidirectional transcript-history pagination state for the TUI app.

use bcode_session_models::{SessionEvent, SessionHistoryCursor};

/// Current resident transcript window position relative to the full session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TranscriptWindowMode {
    /// Resident events represent the latest known tail of the session.
    #[default]
    Tail,
    /// Resident events are a bounded historical window around an anchor event.
    Centered {
        /// Source event sequence selected by the timeline jump.
        anchor_sequence: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageAcceptance {
    Pending,
    Ready,
}

/// Tracks bidirectional history pagination and reveal requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OlderHistoryState {
    older_cursor: Option<SessionHistoryCursor>,
    newer_cursor: Option<SessionHistoryCursor>,
    older_reveal_request: Option<usize>,
    newer_reveal_request: Option<usize>,
    older_page_ready: PageAcceptance,
    newer_page_ready: PageAcceptance,
    loading_older: bool,
    loading_newer: bool,
    mode: TranscriptWindowMode,
}

impl OlderHistoryState {
    /// Create pagination state from replayed history.
    #[must_use]
    pub fn new(events: &[SessionEvent], has_older_history: bool) -> Self {
        Self {
            older_cursor: oldest_history_cursor(events, has_older_history),
            newer_cursor: None,
            older_reveal_request: None,
            newer_reveal_request: None,
            older_page_ready: PageAcceptance::Pending,
            newer_page_ready: PageAcceptance::Pending,
            loading_older: false,
            loading_newer: false,
            mode: TranscriptWindowMode::Tail,
        }
    }

    /// Return whether older history may be available.
    #[must_use]
    pub const fn has_older_history(&self) -> bool {
        self.older_cursor.is_some()
    }

    /// Return whether newer history may be available.
    #[must_use]
    pub const fn has_newer_history(&self) -> bool {
        self.newer_cursor.is_some()
    }

    /// Return whether the resident window is at the latest tail.
    #[must_use]
    pub const fn at_tail(&self) -> bool {
        matches!(self.mode, TranscriptWindowMode::Tail)
    }

    /// Return whether an older-history request is in flight.
    #[must_use]
    pub const fn loading(&self) -> bool {
        self.loading_older
    }

    /// Mark older history as loading or idle.
    pub const fn set_loading(&mut self, loading: bool) {
        self.loading_older = loading;
        if !loading && matches!(self.older_page_ready, PageAcceptance::Pending) {
            self.older_reveal_request = None;
        }
    }

    /// Mark newer history as loading or idle.
    pub const fn set_loading_newer(&mut self, loading: bool) {
        self.loading_newer = loading;
        if !loading && matches!(self.newer_page_ready, PageAcceptance::Pending) {
            self.newer_reveal_request = None;
        }
    }

    /// Return the cursor for loading older history.
    #[must_use]
    pub const fn cursor(&self) -> Option<SessionHistoryCursor> {
        self.older_cursor
    }

    /// Return the cursor for loading newer history.
    #[must_use]
    pub const fn newer_cursor(&self) -> Option<SessionHistoryCursor> {
        self.newer_cursor
    }

    /// Return whether an older-history request should be started.
    #[must_use]
    pub const fn should_load(&self) -> bool {
        self.older_cursor.is_some()
            && !self.loading_older
            && matches!(self.older_page_ready, PageAcceptance::Pending)
            && self.older_reveal_request.is_some()
    }

    /// Return whether a newer-history request should be started.
    #[must_use]
    pub const fn should_load_newer(&self) -> bool {
        self.newer_cursor.is_some()
            && !self.loading_newer
            && matches!(self.newer_page_ready, PageAcceptance::Pending)
            && self.newer_reveal_request.is_some()
    }

    /// Set cursor based on a loaded older page.
    pub fn update_cursor(&mut self, events: &[SessionEvent], has_more: bool) {
        self.older_cursor = oldest_history_cursor(events, has_more);
        self.older_page_ready = PageAcceptance::Ready;
    }

    /// Set cursor based on a loaded newer page.
    pub fn update_newer_cursor(&mut self, events: &[SessionEvent], has_more: bool) {
        self.newer_cursor = newest_history_cursor(events, has_more);
        self.newer_page_ready = PageAcceptance::Ready;
        if self.newer_cursor.is_none() {
            self.mode = TranscriptWindowMode::Tail;
        }
    }

    /// Replace state for a centered transcript window.
    pub fn replace_centered(
        &mut self,
        events: &[SessionEvent],
        has_older: bool,
        has_newer: bool,
        anchor_sequence: u64,
    ) {
        self.older_cursor = oldest_history_cursor(events, has_older);
        self.newer_cursor = newest_history_cursor(events, has_newer);
        self.older_reveal_request = None;
        self.newer_reveal_request = None;
        self.older_page_ready = PageAcceptance::Pending;
        self.newer_page_ready = PageAcceptance::Pending;
        self.loading_older = false;
        self.loading_newer = false;
        self.mode = if has_newer {
            TranscriptWindowMode::Centered { anchor_sequence }
        } else {
            TranscriptWindowMode::Tail
        };
    }

    /// Mark previously resident events before `oldest_resident_sequence` as reloadable older history.
    pub const fn mark_dropped_history_before(&mut self, oldest_resident_sequence: u64) {
        self.older_cursor = if oldest_resident_sequence == 0 {
            None
        } else {
            Some(SessionHistoryCursor {
                sequence: oldest_resident_sequence.saturating_sub(1),
            })
        };
    }

    /// Mark newly arrived content as available below a centered historical window.
    pub const fn mark_newer_available_after(&mut self, newest_resident_sequence: Option<u64>) {
        if self.at_tail() {
            return;
        }
        if let Some(sequence) = newest_resident_sequence {
            self.newer_cursor = Some(SessionHistoryCursor {
                sequence: sequence.saturating_add(1),
            });
        }
    }

    /// Request loading older history and reveal this many rows afterward.
    pub const fn request_load(&mut self, reveal_rows: usize) {
        self.newer_reveal_request = None;
        let previous = match self.older_reveal_request {
            Some(rows) => rows,
            None => 0,
        };
        self.older_reveal_request = Some(previous.saturating_add(reveal_rows));
    }

    /// Request loading newer history.
    pub const fn request_load_newer(&mut self, reveal_rows: usize) {
        self.older_reveal_request = None;
        let previous = match self.newer_reveal_request {
            Some(rows) => rows,
            None => 0,
        };
        self.newer_reveal_request = Some(previous.saturating_add(reveal_rows));
    }

    /// Cancel movement waiting beyond the older boundary after a direction reversal.
    pub const fn cancel_older_reveal(&mut self) {
        self.older_reveal_request = None;
    }

    /// Cancel movement waiting beyond the newer boundary after a direction reversal.
    pub const fn cancel_newer_reveal(&mut self) {
        self.newer_reveal_request = None;
    }

    /// Consume page acceptance separately from the fetch request and navigation intent.
    pub const fn take_ready_pages(&mut self) -> (bool, bool) {
        let ready = (
            matches!(self.older_page_ready, PageAcceptance::Ready),
            matches!(self.newer_page_ready, PageAcceptance::Ready),
        );
        self.older_page_ready = PageAcceptance::Pending;
        self.newer_page_ready = PageAcceptance::Pending;
        ready
    }

    /// Consume only movement fulfilled by newly available rows.
    pub fn consume_reveal(&mut self, older: bool, rows: usize) {
        let (request, has_more) = if older {
            (&mut self.older_reveal_request, self.older_cursor.is_some())
        } else {
            (&mut self.newer_reveal_request, self.newer_cursor.is_some())
        };
        *request = request.and_then(|requested| {
            let remaining = requested.saturating_sub(rows);
            (has_more && remaining > 0).then_some(remaining)
        });
    }

    /// Return pending older-history reveal request rows.
    #[must_use]
    pub const fn reveal_request(&self) -> Option<usize> {
        self.older_reveal_request
    }

    /// Return pending newer-history reveal request rows.
    #[must_use]
    pub const fn newer_reveal_request(&self) -> Option<usize> {
        self.newer_reveal_request
    }

    /// Clear any pending reveal request.
    pub const fn clear_reveal_request(&mut self) {
        self.older_reveal_request = None;
        self.newer_reveal_request = None;
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

fn newest_history_cursor(
    events: &[SessionEvent],
    has_newer_history: bool,
) -> Option<SessionHistoryCursor> {
    if !has_newer_history {
        return None;
    }
    events.last().map(|event| SessionHistoryCursor {
        sequence: event.sequence.saturating_add(1),
    })
}
