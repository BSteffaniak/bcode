//! Transcript viewport scrolling state.

use super::older_history::OlderHistoryState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TranscriptViewportMode {
    #[default]
    FollowBottom,
    AnchoredTop {
        top_row: usize,
    },
    /// A stationary reading position with virtual space remaining below the tail.
    TailSpace {
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
    /// Return whether the viewport follows the newest transcript rows.
    #[must_use]
    pub const fn follows_bottom(&self) -> bool {
        matches!(self.mode, TranscriptViewportMode::FollowBottom)
    }

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
            TranscriptViewportMode::AnchoredTop { top_row }
            | TranscriptViewportMode::TailSpace { top_row } => top_row.min(total_rows),
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
    pub fn scroll_down(&mut self, rows: usize, history: &mut OlderHistoryState) -> bool {
        if rows == 0 {
            return false;
        }
        let previous = *self;
        let viewport_height = usize::from(self.viewport_height);
        match self.mode {
            TranscriptViewportMode::FollowBottom | TranscriptViewportMode::TailSpace { .. } => {
                if history.has_newer_history() && !history.loading_newer() {
                    let previous_request = history.newer_reveal_request();
                    history.request_load_newer(rows.max(1));
                    return previous_request != history.newer_reveal_request();
                }
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
                    if history.has_newer_history() && !history.loading_newer() {
                        let previous_request = history.newer_reveal_request();
                        history.request_load_newer(next_top.saturating_sub(bottom_top).max(1));
                        self.mode = TranscriptViewportMode::FollowBottom;
                        self.bottom_overscroll = 0;
                        self.refresh_offset_cache();
                        return *self != previous
                            || previous_request != history.newer_reveal_request();
                    }
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
        if self.bottom_overscroll > 0 {
            self.mode = TranscriptViewportMode::TailSpace {
                top_row: self
                    .previous_total_rows
                    .saturating_add(self.bottom_overscroll)
                    .saturating_sub(viewport_height),
            };
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
        _manual_scroll_active: bool,
        older_history: &mut OlderHistoryState,
    ) {
        let previous_max = self.max_offset;
        self.previous_total_rows = total_rows;
        self.viewport_height = viewport_height;
        self.max_offset = max_offset;
        self.max_bottom_overscroll = max_bottom_overscroll;
        self.resolve_tail_space();
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

    /// Resolve existing content correspondence before testing tail-space exhaustion.
    /// Bounds are maximum offset, maximum virtual space, total rows, and viewport height.
    pub fn sync_with_anchor(
        &mut self,
        bounds: (usize, usize, usize, u16),
        anchor: Option<usize>,
        history: &mut OlderHistoryState,
    ) {
        if let Some(top_row) = anchor {
            self.mode = match self.mode {
                TranscriptViewportMode::FollowBottom => TranscriptViewportMode::FollowBottom,
                TranscriptViewportMode::AnchoredTop { .. } => {
                    TranscriptViewportMode::AnchoredTop { top_row }
                }
                TranscriptViewportMode::TailSpace { .. } => {
                    TranscriptViewportMode::TailSpace { top_row }
                }
            };
        }
        if anchor.is_some() {
            // Identity correspondence already includes history inserted above the
            // reader. Do not apply a second max-offset-based prepend adjustment.
            history.clear_reveal_request();
        }
        self.sync_max(bounds.0, bounds.1, bounds.2, bounds.3, false, history);
    }

    /// Apply explicit reveal overflow policy at the viewport ownership boundary.
    pub const fn reconcile_overflow(
        &mut self,
        previous_bottom: usize,
        allowed: bool,
        history: &mut OlderHistoryState,
    ) -> bool {
        if allowed && self.previous_total_rows > previous_bottom {
            return self.scroll_to_bottom(history);
        }
        false
    }

    /// Restore content correspondence without changing the user's navigation intent.
    #[cfg(test)]
    pub fn restore_anchor(&mut self, top_row: usize) {
        let top_row = top_row.min(self.previous_total_rows);
        self.mode = match self.mode {
            TranscriptViewportMode::FollowBottom => return,
            TranscriptViewportMode::AnchoredTop { .. } => {
                TranscriptViewportMode::AnchoredTop { top_row }
            }
            TranscriptViewportMode::TailSpace { .. } => {
                TranscriptViewportMode::TailSpace { top_row }
            }
        };
        self.resolve_tail_space();
        self.refresh_offset_cache();
    }

    fn resolve_tail_space(&mut self) {
        if let TranscriptViewportMode::TailSpace { top_row } = self.mode {
            let top_row = top_row.min(self.previous_total_rows);
            self.mode = TranscriptViewportMode::TailSpace { top_row };
            let bottom = top_row.saturating_add(usize::from(self.viewport_height));
            self.bottom_overscroll = bottom
                .saturating_sub(self.previous_total_rows)
                .min(self.max_bottom_overscroll);
            if bottom <= self.previous_total_rows {
                self.mode = TranscriptViewportMode::FollowBottom;
            }
        }
    }

    fn clamp_anchor(&mut self) {
        if let TranscriptViewportMode::AnchoredTop { top_row } = &mut self.mode {
            *top_row = (*top_row).min(self.previous_total_rows);
        }
    }

    fn refresh_offset_cache(&mut self) {
        self.offset = match self.mode {
            TranscriptViewportMode::FollowBottom | TranscriptViewportMode::TailSpace { .. } => 0,
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
    fn correspondence_precedes_tail_exhaustion() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        viewport.sync_max(20, 9, 30, 10, false, &mut history);
        viewport.scroll_down(4, &mut history);
        // Ten rows inserted above the reading position do not consume tail space.
        viewport.sync_with_anchor((30, 9, 40, 10), Some(34), &mut history);
        assert!(!viewport.follows_bottom());
        assert_eq!(viewport.bottom_overscroll(), 4);
        assert_eq!(viewport.top_row(40, 10), 34);
    }

    #[test]
    fn tail_space_restore_preserves_intent_and_derives_blank_rows() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(20, 9, 30, 10, false, &mut older);
        viewport.scroll_down(4, &mut older);
        viewport.sync_max(21, 9, 31, 10, false, &mut older);
        viewport.restore_anchor(25);
        assert_eq!(viewport.bottom_overscroll(), 4);
        assert!(!viewport.follows_bottom());
        assert_eq!(viewport.top_row(31, 10), 25);
    }

    #[test]
    fn tail_space_does_not_ratchet_when_existing_geometry_oscillates() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(20, 9, 30, 10, false, &mut older);
        viewport.scroll_down(4, &mut older);
        let top = viewport.top_row(30, 10);
        for _ in 0..20 {
            viewport.sync_max(21, 9, 31, 10, false, &mut older);
            assert_eq!(viewport.top_row(31, 10), top);
            viewport.sync_max(20, 9, 30, 10, false, &mut older);
            assert_eq!(viewport.top_row(30, 10), top);
            assert_eq!(viewport.bottom_overscroll(), 4);
        }
    }

    #[test]
    fn tail_space_exhaustion_is_independent_of_manual_scroll_timing() {
        for manual_scroll_active in [false, true] {
            let mut viewport = TranscriptViewport::default();
            let mut older = older_history();
            viewport.sync_max(20, 9, 30, 10, false, &mut older);
            viewport.scroll_down(4, &mut older);
            assert!(!viewport.follows_bottom());
            viewport.sync_max(22, 9, 32, 10, manual_scroll_active, &mut older);
            assert_eq!(viewport.top_row(32, 10), 24);
            assert_eq!(viewport.bottom_overscroll(), 2);
            viewport.sync_max(25, 9, 35, 10, manual_scroll_active, &mut older);
            assert!(viewport.follows_bottom());
            assert_eq!(viewport.top_row(35, 10), 25);
        }
    }

    #[test]
    fn tail_space_repeated_measurement_is_idempotent() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(20, 9, 30, 10, false, &mut older);
        viewport.scroll_down(4, &mut older);
        viewport.sync_max(21, 9, 31, 10, false, &mut older);
        let resolved = viewport;
        viewport.sync_max(21, 9, 31, 10, false, &mut older);
        assert_eq!(viewport, resolved);
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
    fn anchored_history_top_row_survives_async_row_height_growth() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(30, 0, 40, 10, false, &mut older);
        viewport.scroll_up(12, &mut older);
        let top_row = viewport.top_row(40, 10);

        viewport.sync_max(37, 0, 47, 10, false, &mut older);

        assert_eq!(viewport.top_row(47, 10), top_row);
        assert_eq!(viewport.offset(), 47 - top_row - 10);
    }

    #[test]
    fn following_bottom_tracks_async_row_height_growth() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(30, 0, 40, 10, false, &mut older);

        viewport.sync_max(37, 0, 47, 10, false, &mut older);

        assert_eq!(viewport.top_row(47, 10), 37);
        assert!(viewport.at_bottom_threshold());
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
        assert!(viewport.at_bottom_threshold());
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
