//! Inline interaction placement for the V2 engine (`interactions.placement = transcript`,
//! C37, D10).
//!
//! The host shell owns the interactive surface (permission and question controls); the V2
//! engine reserves the surface's logical rows in its unresolved interaction unit, so the
//! transcript scrolls and anchors around them like any other content. After each frame the
//! host places the surface over the reserved unit's visible rows, from the same prepared
//! snapshot the transcript painted (B3, B7): a reservation scrolled out of view, clipped away
//! or not current in that frame has no placement, so the surface registers no stale hit
//! regions. Docked (`pinned`) placement does not use this module.

use std::time::Instant;

use bcode_tui_transcript::navigation::{NavIntent, Placement};
use bcode_tui_transcript::units::UnitKey;
use bcode_tui_transcript::units::structure::InlineInteraction;
use bmux_tui::geometry::Rect;

use super::TranscriptV2Host;

/// Most terminal rows an inline interaction surface paints in one frame (D10).
pub const MAX_VISIBLE_INTERACTION_ROWS: u16 = 512;
/// Most terminal cells an inline interaction surface paints in one frame (D10).
pub const MAX_VISIBLE_INTERACTION_CELLS: u32 = 131_072;

/// Where the inline interaction surface paints in the last frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlineInteractionArea {
    /// Terminal rectangle of the surface's visible rows.
    pub destination: Rect,
    /// Logical rows the surface occupies (the reserved unit's height), at most
    /// [`bcode_tui_transcript::units::structure::MAX_INLINE_INTERACTION_ROWS`].
    pub logical_height: u16,
    /// Logical row of the surface shown on the destination's first row.
    pub visible_offset: u16,
}

impl TranscriptV2Host {
    /// Reserve `rows` rows in the unresolved interaction unit of `interaction_id`, or drop the
    /// reservation (`None`, also for docked placement). Applied before the frame is prepared,
    /// at that frame's clock. Returns whether the transcript changed, so the frame being drawn
    /// repaints the transcript region ([`Self::with_transcript_damage`]).
    pub fn sync_inline_interaction(&mut self, active: Option<(&str, u16)>, at: Instant) -> bool {
        let interaction =
            active.map(|(id, rows)| InlineInteraction::new(id.to_owned(), u32::from(rows)));
        let update = self.engine.set_inline_interaction(interaction, at);
        let changed = update.invalidated;
        self.absorb(update);
        changed
    }

    /// Placement of the inline interaction surface in the last painted frame (D10): the
    /// reserved unit's rows clipped to the viewport, bounded to
    /// [`MAX_VISIBLE_INTERACTION_ROWS`] rows and [`MAX_VISIBLE_INTERACTION_CELLS`] cells.
    #[must_use]
    pub fn inline_interaction_area(&self) -> Option<InlineInteractionArea> {
        let viewport = self.regions.viewport?;
        let frame = self.last_frame.as_ref()?;
        let placement = self.engine.inline_interaction_placement(frame)?;
        let height = i64::try_from(placement.height).ok()?;
        let viewport_height = i64::from(viewport.height);
        let start = placement.top.max(0);
        let end = placement.top.saturating_add(height).min(viewport_height);
        if start >= end || viewport.width == 0 {
            return None;
        }
        let cell_rows = MAX_VISIBLE_INTERACTION_CELLS / u32::from(viewport.width);
        let max_rows = MAX_VISIBLE_INTERACTION_ROWS
            .min(u16::try_from(cell_rows).unwrap_or(u16::MAX))
            .max(1);
        let rows = u16::try_from(end - start).ok()?.min(max_rows);
        let top = u16::try_from(start).ok()?;
        Some(InlineInteractionArea {
            destination: Rect::new(
                viewport.x,
                viewport.y.saturating_add(top),
                viewport.width,
                rows,
            ),
            logical_height: u16::try_from(placement.height).unwrap_or(u16::MAX),
            visible_offset: u16::try_from(start - placement.top).unwrap_or(u16::MAX),
        })
    }

    /// Bring the reserved interaction unit's top into view (`InteractionFocusActive`, C37).
    /// Returns whether the transcript changed.
    pub fn reveal_inline_interaction(&mut self, at: Instant) -> bool {
        let Some(item) = self.engine.structure().inline_interaction_item().cloned() else {
            return false;
        };
        let update = self.engine.intent(
            NavIntent::JumpToItem {
                item: UnitKey::Item(item),
                placement: Placement::ItemTopAtTop,
            },
            at,
        );
        let changed = update.invalidated || !update.requests.is_empty();
        self.absorb(update);
        changed
    }
}

#[cfg(test)]
mod tests;
