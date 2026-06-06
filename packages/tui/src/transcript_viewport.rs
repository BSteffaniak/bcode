//! Transcript viewport scrolling state.

use super::older_history::OlderHistoryState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TranscriptViewportMode {
    #[default]
    FollowBottom,
    AnchoredTop {
        top_row: usize,
    },
}

/// Rendered transcript viewport state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TranscriptViewport {
    mode: TranscriptViewportMode,
    offset: usize,
    max_offset: usize,
    bottom_overscroll: usize,
    max_bottom_overscroll: usize,
    previous_total_rows: usize,
    viewport_height: u16,
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

    /// Return the last synced viewport height.
    #[must_use]
    pub const fn height(&self) -> u16 {
        self.viewport_height
    }

    /// Return whether the viewport is following live transcript output.
    #[must_use]
    pub const fn following(&self) -> bool {
        matches!(self.mode, TranscriptViewportMode::FollowBottom) && self.bottom_overscroll == 0
    }

    /// Return whether newest content is at or above the viewport's bottom edge.
    #[must_use]
    pub const fn at_bottom_threshold(&self) -> bool {
        matches!(self.mode, TranscriptViewportMode::FollowBottom) && self.bottom_overscroll == 0
    }

    /// Return the current viewport bottom row in transcript row coordinates.
    #[must_use]
    pub fn bottom_row(&self, total_rows: usize) -> usize {
        self.top_row(total_rows, self.viewport_height)
            .saturating_add(usize::from(self.viewport_height))
    }

    /// Preserve viewport position before live transcript rows append.
    ///
    /// The viewport is top-anchored while detached from the bottom, so appends
    /// below the visible history are naturally stable. This is retained as a
    /// compatibility hook for callers at transcript mutation boundaries.
    pub fn preserve_for_append(&mut self) {
        self.refresh_offset_cache();
    }

    /// Follow live transcript output from a stable top row.
    pub fn follow_anchor(&mut self, top_row: usize) {
        self.mode = TranscriptViewportMode::AnchoredTop {
            top_row: top_row.min(self.previous_total_rows),
        };
        self.bottom_overscroll = 0;
        self.refresh_offset_cache();
    }

    /// Materialize a top-origin viewport row into normal scroll state.
    pub fn materialize_top_row(&mut self, top_row: usize) {
        self.mode = TranscriptViewportMode::AnchoredTop {
            top_row: top_row.min(self.previous_total_rows),
        };
        self.bottom_overscroll = 0;
        self.refresh_offset_cache();
    }

    /// Start following live output from an animated top-row transition.
    #[must_use]
    pub fn start_follow_anchor_animation(
        &mut self,
        target_top_row: usize,
    ) -> Option<(usize, usize)> {
        let target_top_row = target_top_row.min(self.previous_total_rows);
        let start_top_row = self.top_row(self.previous_total_rows, self.viewport_height);
        if start_top_row == target_top_row {
            self.follow_anchor(target_top_row);
            None
        } else {
            Some((start_top_row, target_top_row))
        }
    }

    /// Return the top-origin row to render for the current viewport.
    #[must_use]
    pub fn top_row(&self, total_rows: usize, viewport_height: u16) -> usize {
        let viewport_height = usize::from(viewport_height);
        match self.mode {
            TranscriptViewportMode::FollowBottom => total_rows
                .saturating_add(self.bottom_overscroll)
                .min(total_rows.saturating_add(self.max_bottom_overscroll))
                .saturating_sub(viewport_height),
            TranscriptViewportMode::AnchoredTop { top_row } => top_row.min(total_rows),
        }
    }

    /// Scroll up by rendered rows.
    pub fn scroll_up(&mut self, rows: usize, older_history: &mut OlderHistoryState) -> bool {
        if rows == 0 {
            return false;
        }
        let previous = *self;
        let current_top = self.top_row(self.previous_total_rows, self.viewport_height);
        let new_top = current_top.saturating_sub(rows);
        let unrevealed_rows = rows.saturating_sub(current_top);
        if unrevealed_rows > 0 {
            let previous_request = older_history.reveal_request();
            request_older_history_load(older_history, unrevealed_rows);
            if previous_request != older_history.reveal_request() {
                return true;
            }
        }
        self.mode = TranscriptViewportMode::AnchoredTop { top_row: new_top };
        self.bottom_overscroll = 0;
        self.refresh_offset_cache();
        *self != previous
    }

    /// Scroll down by rendered rows.
    pub fn scroll_down(&mut self, rows: usize) -> bool {
        if rows == 0 {
            return false;
        }
        let previous = *self;
        let viewport_height = usize::from(self.viewport_height);
        match self.mode {
            TranscriptViewportMode::FollowBottom => {
                self.bottom_overscroll = self
                    .bottom_overscroll
                    .saturating_add(rows)
                    .min(self.max_bottom_overscroll);
            }
            TranscriptViewportMode::AnchoredTop { .. } => {
                let current_top = self.top_row(self.previous_total_rows, self.viewport_height);
                let bottom_top = self.previous_total_rows.saturating_sub(viewport_height);
                let next_top = current_top.saturating_add(rows);
                if next_top >= bottom_top {
                    self.mode = TranscriptViewportMode::FollowBottom;
                    self.bottom_overscroll = next_top
                        .saturating_sub(bottom_top)
                        .min(self.max_bottom_overscroll);
                } else {
                    self.mode = TranscriptViewportMode::AnchoredTop { top_row: next_top };
                    self.bottom_overscroll = 0;
                }
            }
        }
        self.refresh_offset_cache();
        *self != previous
    }

    /// Pin transcript to the newest rows.
    pub const fn scroll_to_bottom(&mut self, older_history: &mut OlderHistoryState) -> bool {
        let changed = !matches!(self.mode, TranscriptViewportMode::FollowBottom)
            || self.bottom_overscroll != 0;
        self.mode = TranscriptViewportMode::FollowBottom;
        self.offset = 0;
        self.bottom_overscroll = 0;
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
        let previous_total_rows = self.previous_total_rows;
        let previous_max = self.max_offset;
        let appended_rows = total_rows.saturating_sub(previous_total_rows);
        self.previous_total_rows = total_rows;
        self.viewport_height = viewport_height;
        self.max_offset = max_offset;
        self.max_bottom_overscroll = max_bottom_overscroll;
        if !manual_scroll_active
            && matches!(self.mode, TranscriptViewportMode::FollowBottom)
            && self.bottom_overscroll > 0
            && appended_rows > 0
        {
            self.bottom_overscroll = self.bottom_overscroll.saturating_sub(appended_rows);
        }
        if let Some(requested_rows) = older_history.take_reveal_request() {
            let inserted_rows = max_offset.saturating_sub(previous_max);
            let reveal_rows = requested_rows.min(inserted_rows);
            if let TranscriptViewportMode::AnchoredTop { top_row } = &mut self.mode {
                *top_row = top_row.saturating_add(reveal_rows);
            }
        }
        self.clamp_anchor();
        self.bottom_overscroll = self.bottom_overscroll.min(self.max_bottom_overscroll);
        self.refresh_offset_cache();
    }

    fn clamp_anchor(&mut self) {
        if let TranscriptViewportMode::AnchoredTop { top_row } = &mut self.mode {
            *top_row = (*top_row).min(self.previous_total_rows);
        }
    }

    fn refresh_offset_cache(&mut self) {
        self.offset = match self.mode {
            TranscriptViewportMode::FollowBottom => 0,
            TranscriptViewportMode::AnchoredTop { top_row } => self
                .previous_total_rows
                .saturating_sub(top_row.saturating_add(usize::from(self.viewport_height)))
                .min(self.max_offset),
        };
    }
}

fn request_older_history_load(older_history: &mut OlderHistoryState, reveal_rows: usize) {
    if older_history.cursor().is_none() || older_history.loading() {
        return;
    }
    older_history.request_load(reveal_rows.max(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn older_history() -> OlderHistoryState {
        OlderHistoryState::new(&[], false)
    }

    #[test]
    fn anchored_history_top_row_does_not_move_when_rows_append() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(20, 0, 30, 10, false, &mut older);
        viewport.scroll_up(8, &mut older);
        let top_row = viewport.top_row(30, 10);

        viewport.sync_max(25, 0, 35, 10, false, &mut older);

        assert_eq!(viewport.top_row(35, 10), top_row);
    }

    #[test]
    fn following_bottom_tracks_appended_rows() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(20, 0, 30, 10, false, &mut older);
        assert_eq!(viewport.top_row(30, 10), 20);

        viewport.sync_max(25, 0, 35, 10, false, &mut older);

        assert_eq!(viewport.top_row(35, 10), 25);
        assert_eq!(viewport.offset(), 0);
        assert!(viewport.following());
    }

    #[test]
    fn anchored_history_top_row_does_not_move_when_viewport_shrinks() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(20, 0, 30, 10, false, &mut older);
        viewport.scroll_up(8, &mut older);
        let top_row = viewport.top_row(30, 10);

        viewport.sync_max(21, 0, 30, 9, false, &mut older);

        assert_eq!(viewport.top_row(30, 9), top_row);
    }

    #[test]
    fn older_history_reveal_keeps_same_content_visible_after_prepend() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(20, 0, 30, 10, false, &mut older);
        viewport.scroll_up(8, &mut older);
        older.request_load(4);

        viewport.sync_max(24, 0, 34, 10, false, &mut older);

        assert_eq!(viewport.top_row(34, 10), 16);
    }
}
