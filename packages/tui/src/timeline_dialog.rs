//! Timeline dialog state for browsing user-sent messages.

/// One selectable user-message timeline row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEntry {
    transcript_index: usize,
    timestamp_ms: u64,
    text: String,
}

impl TimelineEntry {
    /// Create a timeline entry.
    #[must_use]
    pub fn new(transcript_index: usize, timestamp_ms: u64, text: impl Into<String>) -> Self {
        Self {
            transcript_index,
            timestamp_ms,
            text: text.into(),
        }
    }

    /// Return the committed transcript item index to jump to.
    #[must_use]
    pub const fn transcript_index(&self) -> usize {
        self.transcript_index
    }

    /// Return the event timestamp in Unix milliseconds.
    #[must_use]
    pub const fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }

    /// Return the message text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Timeline modal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineDialogState {
    entries: Vec<TimelineEntry>,
    selected: usize,
    scroll: usize,
}

impl TimelineDialogState {
    /// Create a timeline dialog from selectable entries.
    #[must_use]
    pub const fn new(entries: Vec<TimelineEntry>) -> Self {
        let selected = entries.len().saturating_sub(1);
        Self {
            entries,
            selected,
            scroll: 0,
        }
    }

    /// Return timeline entries.
    #[must_use]
    pub fn entries(&self) -> &[TimelineEntry] {
        &self.entries
    }

    /// Return the selected entry index.
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    /// Return the first rendered entry index.
    #[must_use]
    pub const fn scroll(&self) -> usize {
        self.scroll
    }

    /// Return the selected entry.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&TimelineEntry> {
        self.entries.get(self.selected)
    }

    /// Move selection up by one entry.
    pub const fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move selection down by one entry.
    pub const fn select_next(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    /// Move selection up by a page.
    pub fn page_previous(&mut self, rows: usize) {
        self.selected = self.selected.saturating_sub(rows.max(1));
    }

    /// Move selection down by a page.
    pub fn page_next(&mut self, rows: usize) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .saturating_add(rows.max(1))
            .min(self.entries.len().saturating_sub(1));
    }

    /// Select the first entry.
    pub const fn select_first(&mut self) {
        self.selected = 0;
    }

    /// Select the last entry.
    pub const fn select_last(&mut self) {
        self.selected = self.entries.len().saturating_sub(1);
    }

    /// Keep selected entry visible in the provided viewport height.
    pub fn sync_scroll(&mut self, visible_rows: usize) {
        let visible_rows = visible_rows.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll.saturating_add(visible_rows) {
            self.scroll = self.selected.saturating_add(1).saturating_sub(visible_rows);
        }
    }
}
