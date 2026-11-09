//! Derived geometry helpers for the reducer (P1, N3).
//!
//! Every row here is computed from the keyed position and the current measured index on
//! demand; nothing is stored across inputs.

use std::time::Instant;

use bmux_tui_components::virtual_list::{self as vl, anchor_at_offset};

use super::{AnchorOffset, ListPosition, NavMode, NavState, NavView, ReadingAnchor};
use crate::layout::SourcePosition;
use crate::units::UnitKey;
use crate::viewport::{BmuxListPosition, bmux_motion};
use crate::window::WindowMode;

/// Map a BMUX anchor back to a Bcode anchor (materialization of a derived position).
pub(super) fn from_bmux_anchor(anchor: vl::ListAnchor<UnitKey, SourcePosition>) -> ReadingAnchor {
    let offset = match anchor.offset {
        vl::AnchorOffset::Row(row) => AnchorOffset::Row(row),
        vl::AnchorOffset::Source {
            position,
            row_delta,
        } => AnchorOffset::Source {
            position,
            row_delta,
        },
    };
    ReadingAnchor {
        unit: anchor.key,
        offset,
    }
}

/// Navigation mode that rests at a position.
pub(super) fn mode_for(position: ListPosition) -> NavMode {
    match position {
        ListPosition::FollowBottom => NavMode::Following,
        ListPosition::Anchored { anchor } => NavMode::Reading { anchor },
        ListPosition::TailSpace { anchor } => NavMode::TailSpace { anchor },
    }
}

impl NavView<'_> {
    /// Whether the window contains the session tail (tail space exists only then, C31).
    pub(super) const fn is_live(&self) -> bool {
        matches!(self.window.mode, WindowMode::Live)
    }

    /// Whether a unit is resident.
    pub(super) fn contains(&self, key: &UnitKey) -> bool {
        self.index.contains_key(key)
    }

    /// Whether a unit is resident with at least one visible row.
    pub(super) fn is_visible(&self, key: &UnitKey) -> bool {
        self.unit_height(key).is_some_and(|height| height > 0)
    }

    /// First resident unit in document order.
    pub(super) fn first_unit(&self) -> Option<&UnitKey> {
        self.index.item(0).map(|item| &item.key)
    }

    /// Largest top offset without blank rows below content.
    pub(super) fn max_content_top(&self) -> u64 {
        self.geometry().max_content_top(self.total_height())
    }

    /// Largest top offset reachable by manual movement: tail space only in live windows.
    pub(super) fn max_top(&self) -> u64 {
        if self.is_live() {
            self.geometry().max_overscroll_top(self.total_height())
        } else {
            self.max_content_top()
        }
    }

    /// Keyed anchor for the collection row `top`; rows past content anchor the last row.
    pub(super) fn anchor_at(&self, top: u64) -> Option<ReadingAnchor> {
        let first = self.first_unit()?;
        let total = self.total_height();
        if total == 0 {
            return Some(ReadingAnchor::top(first.clone()));
        }
        anchor_at_offset(self.index, top.min(total - 1), self.sources).map(from_bmux_anchor)
    }

    /// Keyed position whose top is `top`: tail space past the content bottom in live windows,
    /// otherwise anchored (clamped on resolution).
    pub(super) fn position_at(&self, top: u64) -> ListPosition {
        let Some(anchor) = self.anchor_at(top) else {
            return ListPosition::FollowBottom;
        };
        if self.is_live() && top > self.max_content_top() {
            ListPosition::TailSpace { anchor }
        } else {
            ListPosition::Anchored { anchor }
        }
    }

    /// Bcode position for a BMUX position produced by motion sampling or target completion.
    pub(super) fn position_from_bmux(&self, position: BmuxListPosition) -> ListPosition {
        match position {
            vl::ListPosition::FollowBottom => ListPosition::FollowBottom,
            vl::ListPosition::Top => {
                self.first_unit()
                    .map_or(ListPosition::FollowBottom, |key| ListPosition::Anchored {
                        anchor: ReadingAnchor::top(key.clone()),
                    })
            }
            vl::ListPosition::Anchored(anchor) => ListPosition::Anchored {
                anchor: from_bmux_anchor(anchor),
            },
            vl::ListPosition::Overscrolled(anchor) => {
                let anchor = from_bmux_anchor(anchor);
                if self.is_live() {
                    ListPosition::TailSpace { anchor }
                } else {
                    ListPosition::Anchored { anchor }
                }
            }
        }
    }
}

/// Top row of the resting position, or `None` when its anchor unit is not resident.
pub(super) fn resting_top(state: &NavState, view: &NavView<'_>) -> Option<u64> {
    view.resolve(&state.resting_position())
        .map(|resolved| resolved.top_offset)
}

/// Top row actually shown at `at`: the running motion sampled at `at` (N6), else the resting
/// position. Without `at` a running motion is not sampled.
pub(super) fn visual_top(state: &NavState, view: &NavView<'_>, at: Option<Instant>) -> u64 {
    if let (Some(motion), Some(at)) = (&state.motion, at)
        && let Some(sample) =
            bmux_motion(motion).sample(at, view.index, view.geometry(), view.sources)
    {
        return sample.top_offset;
    }
    resting_top(state, view).unwrap_or(0)
}
