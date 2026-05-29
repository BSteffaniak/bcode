//! Transcript viewport scrolling state.

use super::older_history::OlderHistoryState;

/// Rendered transcript viewport state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TranscriptViewport {
    offset: usize,
    max_offset: usize,
    bottom_overscroll: usize,
    max_bottom_overscroll: usize,
    anchor_top_row: Option<usize>,
    previous_total_rows: usize,
    viewport_height: u16,
    preserve_max_offset: Option<usize>,
}

impl TranscriptViewport {
    /// Return the number of transcript rows hidden below the viewport.
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.offset
    }

    /// Return the number of virtual rows below the newest transcript row.
    #[must_use]
    pub const fn bottom_overscroll(&self) -> usize {
        self.bottom_overscroll
    }

    /// Return whether the viewport is following live transcript output.
    #[must_use]
    pub const fn following(&self) -> bool {
        self.offset == 0 && self.bottom_overscroll == 0
    }

    /// Preserve viewport position before live transcript rows append.
    pub const fn preserve_for_append(&mut self) {
        if self.offset > 0 {
            self.preserve_max_offset = Some(self.max_offset);
        }
    }

    /// Follow live transcript output from a stable top row.
    pub fn follow_anchor(&mut self, top_row: usize) {
        self.offset = 0;
        self.bottom_overscroll = 0;
        self.anchor_top_row = Some(top_row.min(self.previous_total_rows));
    }

    /// Return the top-origin row to render for the current viewport.
    #[must_use]
    pub fn top_row(&self, total_rows: usize, viewport_height: u16) -> usize {
        if let Some(top_row) = self.anchor_top_row {
            return top_row;
        }
        let end = total_rows
            .saturating_sub(self.offset)
            .saturating_add(self.bottom_overscroll)
            .min(total_rows.saturating_add(self.max_bottom_overscroll));
        end.saturating_sub(usize::from(viewport_height))
    }

    /// Scroll up by rendered rows.
    pub fn scroll_up(&mut self, rows: usize, older_history: &mut OlderHistoryState) -> bool {
        if rows == 0 {
            return false;
        }
        let previous = *self;
        self.materialize_anchor_position();
        if self.bottom_overscroll > 0 {
            let consumed = rows.min(self.bottom_overscroll);
            self.bottom_overscroll = self.bottom_overscroll.saturating_sub(consumed);
            if consumed == rows {
                return *self != previous;
            }
        }
        let previous_request = older_history.reveal_request();
        let desired = self
            .offset
            .saturating_add(rows.saturating_sub(previous.bottom_overscroll));
        self.offset = desired.min(self.max_offset);
        if desired > self.max_offset {
            request_older_history_load(older_history, desired.saturating_sub(self.max_offset));
        }
        *self != previous || older_history.reveal_request() != previous_request
    }

    /// Scroll down by rendered rows.
    pub fn scroll_down(&mut self, rows: usize) -> bool {
        if rows == 0 {
            return false;
        }
        let previous = *self;
        self.materialize_anchor_position();
        if self.offset > 0 {
            self.offset = self.offset.saturating_sub(rows);
        } else {
            self.bottom_overscroll = self
                .bottom_overscroll
                .saturating_add(rows)
                .min(self.max_bottom_overscroll);
        }
        previous.offset != self.offset
            || previous.bottom_overscroll != self.bottom_overscroll
            || previous.anchor_top_row != self.anchor_top_row
    }

    /// Pin transcript to the newest rows.
    pub const fn scroll_to_bottom(&mut self, older_history: &mut OlderHistoryState) -> bool {
        let changed =
            self.offset != 0 || self.bottom_overscroll != 0 || self.anchor_top_row.is_some();
        self.offset = 0;
        self.bottom_overscroll = 0;
        self.anchor_top_row = None;
        older_history.clear_reveal_request();
        changed
    }

    /// Sync cached rendered transcript scroll bounds from the latest frame.
    pub fn sync_max(
        &mut self,
        max_offset: usize,
        max_bottom_overscroll: usize,
        total_rows: usize,
        viewport_height: u16,
        manual_scroll_active: bool,
        older_history: &mut OlderHistoryState,
    ) {
        let previous_max = self.max_offset;
        let appended_rows = total_rows.saturating_sub(self.previous_total_rows);
        self.previous_total_rows = total_rows;
        self.viewport_height = viewport_height;
        self.max_offset = max_offset;
        self.max_bottom_overscroll = max_bottom_overscroll;
        if !manual_scroll_active
            && self.offset == 0
            && self.bottom_overscroll > 0
            && appended_rows > 0
        {
            self.bottom_overscroll = self.bottom_overscroll.saturating_sub(appended_rows);
        }
        if let Some(requested_rows) = older_history.take_reveal_request() {
            let inserted_rows = max_offset.saturating_sub(previous_max);
            let reveal_rows = requested_rows.min(inserted_rows);
            self.offset = self.offset.saturating_add(reveal_rows);
        }
        if let Some(preserve_max) = self.preserve_max_offset.take()
            && self.offset > 0
        {
            let appended_rows = max_offset.saturating_sub(preserve_max);
            self.offset = self.offset.saturating_add(appended_rows);
        }
        self.offset = self.offset.min(self.max_offset);
        self.bottom_overscroll = self.bottom_overscroll.min(self.max_bottom_overscroll);
    }

    fn materialize_anchor_position(&mut self) {
        let Some(anchor_top_row) = self.anchor_top_row.take() else {
            return;
        };
        let end_row = anchor_top_row.saturating_add(usize::from(self.viewport_height));
        if end_row > self.previous_total_rows {
            self.offset = 0;
            self.bottom_overscroll = end_row
                .saturating_sub(self.previous_total_rows)
                .min(self.max_bottom_overscroll);
        } else {
            self.offset = self
                .previous_total_rows
                .saturating_sub(end_row)
                .min(self.max_offset);
            self.bottom_overscroll = 0;
        }
    }
}

fn request_older_history_load(older_history: &mut OlderHistoryState, reveal_rows: usize) {
    if older_history.cursor().is_none() || older_history.loading() {
        return;
    }
    older_history.request_load(reveal_rows.max(1));
}
