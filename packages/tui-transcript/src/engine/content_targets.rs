//! Content targets of a prepared frame (E1, C39): visible cells whose pointer and key input
//! belong to their content (plugin visual rows routed to their adapter, shell screens
//! scrolling horizontally on their own).
//!
//! Targets come from the retained layouts the snapshot painted, exactly like hit regions
//! (B3, B7): only visible units whose retained layout is still the painted one contribute, and
//! every target is clipped to the viewport, so no stale or invisible target survives a scroll,
//! a resize or a content change.

use bmux_tui::component::LogicalRect;

use super::TranscriptEngine;
use crate::frame::FrameSnapshot;
use crate::layout::producers::visual::ContentTargetKind;
use crate::units::UnitKey;

/// One content target of a prepared frame in viewport cells.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VisibleContentTarget {
    /// Region clipped to the viewport, in viewport cells.
    pub rect: LogicalRect,
    /// Owning unit.
    pub unit: UnitKey,
    /// What the region routes to.
    pub kind: ContentTargetKind,
}

impl TranscriptEngine {
    /// Content targets of a prepared frame, clipped to the viewport, in viewport cells, top
    /// to bottom.
    #[must_use]
    pub fn visible_content_targets(&self, snapshot: &FrameSnapshot) -> Vec<VisibleContentTarget> {
        let Some(viewport) = snapshot.viewport.as_ref() else {
            return Vec::new();
        };
        let geometry = snapshot.geometry;
        let mut targets = Vec::new();
        for unit in &viewport.units {
            let Some(layout) = self.painted_layout(snapshot, unit) else {
                continue;
            };
            for target in &layout.geometry.content_targets {
                let rect = target.rect;
                let top = i128::from(unit.top) + i128::from(rect.y);
                let bottom = (top + i128::from(rect.height)).min(i128::from(geometry.height));
                let top = top.max(0);
                let x = rect.x.min(geometry.width);
                let width = rect.width.min(geometry.width - x);
                let (Ok(top), Ok(bottom)) = (u64::try_from(top), u64::try_from(bottom)) else {
                    continue;
                };
                if top >= bottom || width == 0 {
                    continue;
                }
                targets.push(VisibleContentTarget {
                    rect: LogicalRect::new(x, top, width, bottom - top),
                    unit: unit.key.clone(),
                    kind: target.kind.clone(),
                });
            }
        }
        targets
    }
}
