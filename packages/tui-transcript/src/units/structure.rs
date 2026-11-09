//! Structure hook for unit derivation (U1, U2, U5).
//!
//! Structure resolution decides, per transcript item, whether the item is its own unit, a
//! member of a group, or hidden behind a count marker. Phase 2 ships the hook and a flat
//! resolver; presentation-policy resolution (rules, plugin facts, visibility floor, disclosure
//! overrides, memoization) plugs in here in phase 3.

use bcode_session_view_models::{TranscriptViewItem, TranscriptViewItemKind};

/// Placement of one transcript item in the unit list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemPlacement {
    /// The item is its own unit.
    Visible,
    /// The item is hidden; consecutive hidden items share one `HiddenMarker` unit.
    Hidden,
    /// The item belongs to a group; consecutive members of the same group (within one turn)
    /// share one `GroupHeader` unit, followed by member units when expanded.
    Grouped {
        /// Caller-chosen group identity; a change of group starts a new group.
        group: u64,
        /// Whether member units follow the header.
        expanded: bool,
    },
}

/// Structure resolver consulted for every item whose placement may have changed.
pub trait UnitStructure {
    /// Placement of one item.
    fn placement(&self, item: &TranscriptViewItem) -> ItemPlacement;
}

/// Resolver that makes every item its own unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlatStructure;

impl UnitStructure for FlatStructure {
    fn placement(&self, _item: &TranscriptViewItem) -> ItemPlacement {
        ItemPlacement::Visible
    }
}

impl<F> UnitStructure for F
where
    F: Fn(&TranscriptViewItem) -> ItemPlacement,
{
    fn placement(&self, item: &TranscriptViewItem) -> ItemPlacement {
        self(item)
    }
}

/// Whether an item starts a turn. Groups and hidden runs never cross a turn start (A14).
#[must_use]
pub const fn starts_turn(item: &TranscriptViewItem) -> bool {
    matches!(item.kind, TranscriptViewItemKind::UserMessage { .. })
}
