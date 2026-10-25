//! Transcript viewport scrolling state.

use super::transcript_history::TranscriptHistory;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TranscriptViewportMode {
    #[default]
    FollowBottom,
    AnchoredTop {
        top_row: usize,
        reveal: Option<bool>,
    },
    /// A stationary reading position with virtual space remaining below the tail.
    TailSpace { top_row: usize },
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
    /// Whether automatic segment reveals may replace the current intent.
    #[must_use]
    pub const fn allows_reveal(&self) -> bool {
        matches!(
            self.mode,
            TranscriptViewportMode::FollowBottom
                | TranscriptViewportMode::AnchoredTop {
                    reveal: Some(_),
                    ..
                }
        )
    }

    /// Whether a revealed entry hands off to tail following when it overflows.
    #[must_use]
    pub const fn allows_overflow(&self) -> bool {
        matches!(
            self.mode,
            TranscriptViewportMode::FollowBottom
                | TranscriptViewportMode::AnchoredTop {
                    reveal: Some(false),
                    ..
                }
        )
    }

    /// Set reveal policy on the current reading position.
    pub fn reveal(&mut self, sticky: bool) {
        let top_row = self.top_row(self.previous_total_rows, self.viewport_height);
        self.mode = TranscriptViewportMode::AnchoredTop {
            top_row,
            reveal: Some(sticky),
        };
    }

    /// Release a sticky reveal after the owning semantic segment ends.
    pub const fn release_sticky_reveal(&mut self) {
        if let TranscriptViewportMode::AnchoredTop { reveal, .. } = &mut self.mode
            && matches!(reveal, Some(true))
        {
            *reveal = Some(false);
        }
    }

    /// Detach navigation without altering its current coordinates.
    pub fn detach(&mut self) {
        if let TranscriptViewportMode::TailSpace { .. } = self.mode {
            return;
        }
        let top_row = self.top_row(self.previous_total_rows, self.viewport_height);
        self.mode = TranscriptViewportMode::AnchoredTop {
            top_row,
            reveal: None,
        };
    }

    /// Name of the resolved navigation intent for diagnostics.
    #[must_use]
    pub const fn intent_name(&self) -> &'static str {
        match self.mode {
            TranscriptViewportMode::FollowBottom => "bottom_follow",
            TranscriptViewportMode::AnchoredTop {
                reveal: Some(_), ..
            } => "anchored_to_entry",
            _ => "manual_detached",
        }
    }

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
            reveal: None,
        };
        self.bottom_overscroll = 0;
        self.refresh_offset_cache();
    }

    /// Materialize a top-origin viewport row into normal scroll state.
    pub fn materialize_top_row(&mut self, top_row: usize) {
        let reveal = match self.mode {
            TranscriptViewportMode::AnchoredTop { reveal, .. } => reveal,
            _ => None,
        };
        self.mode = TranscriptViewportMode::AnchoredTop {
            top_row: top_row.min(self.previous_total_rows),
            reveal,
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
            self.materialize_top_row(target_top_row);
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
            TranscriptViewportMode::AnchoredTop { top_row, .. }
            | TranscriptViewportMode::TailSpace { top_row } => top_row.min(total_rows),
        }
    }

    /// Scroll up by rendered rows.
    pub fn scroll_up(&mut self, rows: usize, older_history: &mut impl TranscriptHistory) -> bool {
        if rows == 0 {
            return false;
        }
        let previous = *self;
        let current_top = self.top_row(self.previous_total_rows, self.viewport_height);
        let new_top = current_top.saturating_sub(rows);
        let unrevealed_rows = rows.saturating_sub(current_top);
        let previous_request = older_history.reveal_request();
        older_history.cancel_newer_reveal();
        if unrevealed_rows > 0 {
            request_older_history_load(older_history, unrevealed_rows);
        }
        self.mode = TranscriptViewportMode::AnchoredTop {
            top_row: new_top,
            reveal: None,
        };
        self.bottom_overscroll = 0;
        self.refresh_offset_cache();
        *self != previous || previous_request != older_history.reveal_request()
    }

    /// Scroll down by rendered rows.
    pub fn scroll_down(&mut self, rows: usize, history: &mut impl TranscriptHistory) -> bool {
        if rows == 0 {
            return false;
        }
        let previous = *self;
        let viewport_height = usize::from(self.viewport_height);
        history.cancel_older_reveal();
        if history.has_newer_history() {
            let current_top = self.top_row(self.previous_total_rows, self.viewport_height);
            let boundary = self.previous_total_rows.saturating_sub(viewport_height);
            let next = current_top.saturating_add(rows);
            let previous_request = history.newer_reveal_request();
            if next > boundary {
                history.request_load_newer(next.saturating_sub(boundary));
            }
            self.follow_anchor(next.min(boundary));
            return *self != previous || previous_request != history.newer_reveal_request();
        }
        match self.mode {
            TranscriptViewportMode::FollowBottom | TranscriptViewportMode::TailSpace { .. } => {
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
                    self.mode = TranscriptViewportMode::AnchoredTop {
                        top_row: next_top,
                        reveal: None,
                    };
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
    pub fn scroll_to_bottom(&mut self, older_history: &mut impl TranscriptHistory) -> bool {
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
        _older_history: &mut impl TranscriptHistory,
    ) {
        self.previous_total_rows = total_rows;
        self.viewport_height = viewport_height;
        self.max_offset = max_offset;
        self.max_bottom_overscroll = max_bottom_overscroll;
        self.resolve_tail_space();
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
        older_page_rows: usize,
        newer_page_rows: usize,
        history: &mut impl TranscriptHistory,
    ) {
        let previous_top = self.top_row(self.previous_total_rows, self.viewport_height);
        let (older_ready, newer_ready, newer_has_content) = history.take_ready_pages();
        // A missing item anchor cannot distinguish prepended content from a
        // concurrent layout change elsewhere. Never infer a prepend from the
        // difference in total row counts; the caller must supply correspondence
        // measured against a surviving item before revealing loaded history.
        if let Some(top_row) = anchor {
            self.mode = match self.mode {
                TranscriptViewportMode::FollowBottom => TranscriptViewportMode::FollowBottom,
                TranscriptViewportMode::AnchoredTop { reveal, .. } => {
                    TranscriptViewportMode::AnchoredTop { top_row, reveal }
                }
                TranscriptViewportMode::TailSpace { .. } => {
                    TranscriptViewportMode::TailSpace { top_row }
                }
            };
        }
        self.sync_max(bounds.0, bounds.1, bounds.2, bounds.3, false, history);
        if older_ready {
            let requested = history.reveal_request().unwrap_or(0);
            let top = self.top_row(bounds.2, bounds.3);
            let movement = if anchor.is_some() {
                requested
                    .min(top.saturating_sub(previous_top))
                    .min(older_page_rows)
            } else {
                0
            };
            if requested > 0 {
                self.follow_anchor(top.saturating_sub(movement));
            }
            history.consume_reveal(true, movement);
        }
        if newer_ready {
            let requested = history.newer_reveal_request().unwrap_or(0);
            let top = self.top_row(bounds.2, bounds.3);
            let available = bounds
                .2
                .saturating_sub(usize::from(bounds.3))
                .saturating_sub(top);
            let movement = if anchor.is_some() && newer_has_content {
                requested.min(available).min(newer_page_rows)
            } else {
                0
            };
            if requested > 0 {
                self.follow_anchor(top.saturating_add(movement));
            }
            history.consume_reveal(false, movement);
        }
    }

    /// Apply explicit reveal overflow policy at the viewport ownership boundary.
    pub fn reconcile_overflow(
        &mut self,
        previous_bottom: usize,
        allowed: bool,
        history: &mut impl TranscriptHistory,
    ) -> bool {
        if allowed && self.previous_total_rows > previous_bottom {
            return self.scroll_to_bottom(history);
        }
        false
    }

    /// Restore content correspondence without changing the user's navigation intent.
    pub fn restore_anchor(&mut self, top_row: usize) {
        let top_row = top_row.min(self.previous_total_rows);
        self.mode = match self.mode {
            TranscriptViewportMode::FollowBottom => return,
            TranscriptViewportMode::AnchoredTop { reveal, .. } => {
                TranscriptViewportMode::AnchoredTop { top_row, reveal }
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
        if let TranscriptViewportMode::AnchoredTop { top_row, .. } = &mut self.mode {
            *top_row = (*top_row).min(self.previous_total_rows);
        }
    }

    fn refresh_offset_cache(&mut self) {
        self.offset = match self.mode {
            TranscriptViewportMode::FollowBottom | TranscriptViewportMode::TailSpace { .. } => 0,
            TranscriptViewportMode::AnchoredTop { top_row, .. } => self
                .previous_total_rows
                .saturating_sub(top_row.saturating_add(usize::from(self.viewport_height)))
                .min(self.max_offset),
        };
    }
}

fn request_older_history_load(older_history: &mut impl TranscriptHistory, reveal_rows: usize) {
    if !older_history.has_older_history() {
        return;
    }
    older_history.request_load(reveal_rows.max(1));
}
