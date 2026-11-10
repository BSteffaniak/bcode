//! The status row's transcript position label for the V2 transcript (C2).
//!
//! The host shell's status row reports how far the reader is from the latest content: "N rows
//! from bottom" while reading above the content bottom and "N rows below latest" in tail space,
//! exactly where V1 shows them. With V2 the values come from the last prepared frame: the
//! presented viewport offset and the engine's latest-bar decision
//! ([`LatestBarState::rows_below_latest`](bcode_tui_transcript::latest_bar::LatestBarState)).
//! The status row is regional chrome outside the transcript region, so a frame whose position
//! changed also damages the status row ([`TranscriptV2Host::with_status_damage`], F5).

use bmux_tui::damage::Damage;
use bmux_tui::geometry::Rect;

use super::TranscriptV2Host;
use crate::render::TranscriptStatusPosition;

impl TranscriptV2Host {
    /// The reading position of the last prepared frame (C2); zero before the first frame and
    /// while following the latest bottom.
    #[must_use]
    pub fn status_position(&self) -> TranscriptStatusPosition {
        let Some(frame) = self.last_frame.as_ref() else {
            return TranscriptStatusPosition::default();
        };
        let rows_from_bottom = frame.viewport.as_ref().map_or(0, |viewport| {
            let resolved = viewport.resolved;
            resolved.max_content_top.saturating_sub(resolved.top_offset)
        });
        TranscriptStatusPosition {
            rows_from_bottom,
            rows_below_latest: frame.latest_bar.rows_below_latest,
        }
    }

    /// `damage` (of the frame just prepared) extended by `status`, the status row, when the
    /// reported reading position differs from the one the status row last showed, and whether
    /// it was extended. The position is recorded as shown, since this frame paints it.
    #[must_use]
    pub fn with_status_damage(
        &mut self,
        damage: Damage,
        status: Rect,
        area: Rect,
    ) -> (Damage, bool) {
        let position = self.status_position();
        let changed = self.status_painted.replace(position) != Some(position);
        if !changed || damage.is_full() || status.is_empty() {
            return (damage, changed);
        }
        let extended = damage.merge(
            Damage::regions([status], area, super::paint::DAMAGE_POLICY),
            area,
            super::paint::DAMAGE_POLICY,
        );
        (extended, true)
    }
}
