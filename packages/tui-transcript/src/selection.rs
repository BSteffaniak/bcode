//! Transcript selection (C34–C36, W7, BMUX 13).
//!
//! A transcript selection is a BMUX [`LogicalSelection`] over [`SelectionKey`]s: one key per
//! source-backed selectable part of a unit ([`crate::layout::geometry::SelectableText`]), an
//! endpoint being a key plus a byte offset into that part's logical source. Endpoints are
//! independent of rows, so a selection follows scrolling, paging and reflow (C34) and is kept
//! while its endpoints scroll out of view; eviction is deferred while a selection exists (W7).
//!
//! The frame registers one BMUX selection scope for the transcript viewport and one child
//! scope per visible unit with selectable text. A gesture starting inside a unit's text locks
//! to that unit (a drag inside a response stays within it); a gesture starting in the space
//! around units locks to the transcript scope and spans units in document order (C34). Items
//! are ordered by their position in the resident measured index ([`ResidentOrder`]).

use std::cmp::Ordering;

use bmux_tui::measured_list::MeasuredListIndex;
use bmux_tui::selection::{SelectionContentId, SelectionScopeId};
use bmux_tui_components::virtual_list::{ItemOrder, LogicalSelection, LogicalSelectionEndpoint};

use crate::units::UnitKey;

/// Selection scope id of the transcript viewport.
pub const SELECTION_SCOPE: &str = "transcript-v2";

/// One selectable part of a unit: the unit and the part's index among the unit's selectable
/// texts in layout order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SelectionKey {
    /// Owning unit.
    pub unit: UnitKey,
    /// Part index within the unit.
    pub part: u32,
}

impl SelectionKey {
    /// Key of `part` of `unit`.
    #[must_use]
    pub const fn new(unit: UnitKey, part: u32) -> Self {
        Self { unit, part }
    }

    /// BMUX content id the frame registers this part's fragments under.
    #[must_use]
    pub fn content_id(&self) -> SelectionContentId {
        SelectionContentId::new(format!("{SELECTION_SCOPE}:{}#{}", self.unit, self.part))
    }
}

/// Selection scope id of `unit`'s child scope.
#[must_use]
pub fn unit_scope_id(unit: &UnitKey) -> SelectionScopeId {
    SelectionScopeId::new(format!("{SELECTION_SCOPE}/{unit}"))
}

/// A logical selection endpoint in the transcript.
pub type TranscriptSelectionEndpoint = LogicalSelectionEndpoint<SelectionKey>;

/// A logical transcript selection.
pub type TranscriptSelection = LogicalSelection<SelectionKey>;

/// Document order of selection keys: resident unit position, then part.
///
/// Keys of units that are not resident are unordered (`None`); eviction is deferred while a
/// selection exists (W7), and a selection whose endpoint unit left the window anyway is dropped.
#[derive(Debug, Clone, Copy)]
pub struct ResidentOrder<'a> {
    index: &'a MeasuredListIndex<UnitKey>,
}

impl<'a> ResidentOrder<'a> {
    /// Order keys by their units' positions in `index`.
    #[must_use]
    pub const fn new(index: &'a MeasuredListIndex<UnitKey>) -> Self {
        Self { index }
    }
}

impl ItemOrder<SelectionKey> for ResidentOrder<'_> {
    fn compare_items(&self, left: &SelectionKey, right: &SelectionKey) -> Option<Ordering> {
        let units = if left.unit == right.unit {
            Ordering::Equal
        } else {
            self.index
                .index_of(&left.unit)?
                .cmp(&self.index.index_of(&right.unit)?)
        };
        Some(units.then(left.part.cmp(&right.part)))
    }
}

/// Transcript selection state; paint-only input (L5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct SelectionState {
    /// A selection gesture or selection is active (defers eviction, W7).
    pub active: bool,
}

/// The engine's selection: the logical selection, if any, and whether a pointer gesture is
/// still extending it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionModel {
    selection: Option<TranscriptSelection>,
    gesture: bool,
}

impl SelectionModel {
    /// The current selection; a collapsed one (an armed press) included.
    #[must_use]
    pub const fn selection(&self) -> Option<&TranscriptSelection> {
        self.selection.as_ref()
    }

    /// Whether a pointer gesture is extending the selection.
    #[must_use]
    pub const fn gesture_active(&self) -> bool {
        self.gesture
    }

    /// Paint-only state: whether a selection or gesture defers eviction (W7).
    #[must_use]
    pub const fn state(&self) -> SelectionState {
        SelectionState {
            active: self.selection.is_some() || self.gesture,
        }
    }

    /// Replace the selection and gesture flag. Returns whether anything changed.
    pub fn set(&mut self, selection: Option<TranscriptSelection>, gesture: bool) -> bool {
        let changed = self.selection != selection || self.gesture != gesture;
        self.selection = selection;
        self.gesture = gesture;
        changed
    }

    /// Drop the selection when an endpoint's unit is no longer resident. Returns whether it
    /// was dropped.
    pub fn retain_resident(&mut self, contains: impl Fn(&UnitKey) -> bool) -> bool {
        let lost = self.selection.as_ref().is_some_and(|selection| {
            !contains(&selection.anchor().key.unit) || !contains(&selection.focus().key.unit)
        });
        if lost {
            self.selection = None;
            self.gesture = false;
        }
        lost
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_view_models::TranscriptViewItemId;

    fn unit(id: &str) -> UnitKey {
        UnitKey::Item(TranscriptViewItemId::new(id))
    }

    fn index(ids: &[&str]) -> MeasuredListIndex<UnitKey> {
        let mut index = MeasuredListIndex::new(0);
        index.sync(ids.iter().map(|id| (unit(id), 0)), 10, 0, |_| 1);
        index
    }

    #[test]
    fn resident_order_follows_list_position_not_key_order() {
        // `z` sorts after `a` by key but comes first in the list.
        let index = index(&["z", "a"]);
        let order = ResidentOrder::new(&index);
        let first = SelectionKey::new(unit("z"), 1);
        let second = SelectionKey::new(unit("a"), 0);
        assert_eq!(order.compare_items(&first, &second), Some(Ordering::Less));
        assert_eq!(
            order.compare_items(&second, &SelectionKey::new(unit("a"), 2)),
            Some(Ordering::Less)
        );
        assert_eq!(
            order.compare_items(&first, &SelectionKey::new(unit("gone"), 0)),
            None
        );
    }

    #[test]
    fn selection_ranges_follow_document_order_across_units() {
        let index = index(&["z", "a", "m"]);
        let order = ResidentOrder::new(&index);
        // Dragged upwards: the focus precedes the anchor.
        let selection = TranscriptSelection::new(
            TranscriptSelectionEndpoint::new(SelectionKey::new(unit("m"), 0), 3),
            TranscriptSelectionEndpoint::new(SelectionKey::new(unit("z"), 0), 2),
        );
        assert_eq!(selection.is_reversed(&order), Some(true));
        assert_eq!(
            selection.range_in(&SelectionKey::new(unit("z"), 0), 10, &order),
            Some(2..10)
        );
        assert_eq!(
            selection.range_in(&SelectionKey::new(unit("a"), 0), 5, &order),
            Some(0..5)
        );
        assert_eq!(
            selection.range_in(&SelectionKey::new(unit("m"), 0), 10, &order),
            Some(0..3)
        );
    }

    #[test]
    fn model_drops_a_selection_whose_endpoint_left_the_window() {
        let mut model = SelectionModel::default();
        let selection = TranscriptSelection::new(
            TranscriptSelectionEndpoint::new(SelectionKey::new(unit("a"), 0), 0),
            TranscriptSelectionEndpoint::new(SelectionKey::new(unit("b"), 0), 4),
        );
        assert!(model.set(Some(selection.clone()), true));
        assert!(model.state().active);
        assert!(!model.set(Some(selection), true));
        assert!(!model.retain_resident(|key| key == &unit("a") || key == &unit("b")));
        assert!(model.retain_resident(|key| key == &unit("a")));
        assert!(model.selection().is_none());
        assert!(!model.state().active);
    }

    #[test]
    fn content_ids_are_distinct_per_part() {
        let key = SelectionKey::new(unit("a"), 0);
        assert_ne!(
            key.content_id(),
            SelectionKey::new(unit("a"), 1).content_id()
        );
        assert_ne!(unit_scope_id(&unit("a")), unit_scope_id(&unit("b")));
    }
}
