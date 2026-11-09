//! The units layer's implementation of navigation [`UnitFacts`] (N9, P4, U2, T17, T18).

use bcode_session_view_models::{TranscriptViewItem, TranscriptViewItemKind};

use super::{UnitKey, Units};
use crate::navigation::{Correspondence, RemovedUnits, UnitFacts, UnitRole};

/// Role of an item for automatic navigation.
#[must_use]
pub const fn item_role(item: &TranscriptViewItem) -> UnitRole {
    match item.kind {
        TranscriptViewItemKind::UserMessage { .. } => UnitRole::UserMessage,
        TranscriptViewItemKind::AssistantMessage { .. } => UnitRole::AssistantSegment,
        _ => UnitRole::Other,
    }
}

/// Unit facts after one change: current units plus the correspondence of the units that
/// change removed.
#[derive(Debug, Clone, Copy)]
pub struct UnitFactsView<'a> {
    /// Current units (after the change).
    pub units: &'a Units,
    /// Correspondence of units removed by the change being reduced.
    pub removed: &'a RemovedUnits,
}

impl<'a> UnitFactsView<'a> {
    /// Facts for the current units and the removals of the change being reduced.
    #[must_use]
    pub const fn new(units: &'a Units, removed: &'a RemovedUnits) -> Self {
        Self { units, removed }
    }
}

impl UnitFacts for UnitFactsView<'_> {
    fn role(&self, unit: &UnitKey) -> UnitRole {
        self.units
            .entry(unit)
            .map_or(UnitRole::Other, |entry| entry.role)
    }

    fn unit_for_sequence(&self, sequence: u64) -> Option<UnitKey> {
        self.units.unit_for_sequence(sequence).cloned()
    }

    fn is_streaming(&self, unit: &UnitKey) -> bool {
        self.units.entry(unit).is_some_and(|entry| entry.streaming)
    }

    fn correspondence(&self, removed: &UnitKey) -> Option<Correspondence> {
        if self.units.contains(removed) {
            return None;
        }
        self.removed.get(removed).cloned()
    }
}

#[cfg(test)]
mod tests {
    use bcode_session_view_models::{ChatMessageView, TranscriptChanges, TranscriptViewItemId};

    use super::*;
    use crate::units::structure::{FlatStructure, ItemPlacement};
    use crate::window::WindowMode;

    fn item(id: &str, sequence: u64, user: bool) -> TranscriptViewItem {
        let message = ChatMessageView::markdown(id);
        TranscriptViewItem {
            id: TranscriptViewItemId::new(id),
            revision: 1,
            sequence: Some(sequence),
            timestamp_ms: None,
            output_location: None,
            streaming: false,
            kind: if user {
                TranscriptViewItemKind::UserMessage { message }
            } else {
                TranscriptViewItemKind::AssistantMessage { message }
            },
        }
    }

    fn key(id: &str) -> UnitKey {
        UnitKey::Item(TranscriptViewItemId::new(id))
    }

    #[test]
    fn roles_and_sequences_come_from_items() {
        let mut units = Units::new();
        let items = vec![item("u", 3, true), item("a", 4, false)];
        let changes = units.apply(
            &TranscriptChanges::default(),
            &items,
            WindowMode::Live,
            &FlatStructure,
        );
        let facts = UnitFactsView::new(&units, &changes.correspondence);
        assert_eq!(facts.role(&key("u")), UnitRole::UserMessage);
        assert_eq!(facts.role(&key("a")), UnitRole::AssistantSegment);
        assert_eq!(facts.unit_for_sequence(4), Some(key("a")));
        assert_eq!(facts.unit_for_sequence(9), None);
    }

    #[test]
    fn p4_u2_removed_member_corresponds_to_its_header() {
        let items = vec![
            item("u", 1, true),
            item("g1", 2, false),
            item("g2", 3, false),
        ];
        let mut units = Units::new();
        let expanded = |item: &TranscriptViewItem| {
            if item.id.get().starts_with('g') {
                ItemPlacement::Grouped {
                    group: 1,
                    expanded: true,
                }
            } else {
                ItemPlacement::Visible
            }
        };
        let collapsed = |item: &TranscriptViewItem| {
            if item.id.get().starts_with('g') {
                ItemPlacement::Grouped {
                    group: 1,
                    expanded: false,
                }
            } else {
                ItemPlacement::Visible
            }
        };
        let _ = units.apply(
            &TranscriptChanges::default(),
            &items,
            WindowMode::Live,
            &expanded,
        );
        let changes = units.apply(
            &TranscriptChanges::default(),
            &items,
            WindowMode::Live,
            &collapsed,
        );
        let facts = UnitFactsView::new(&units, &changes.correspondence);
        let header = UnitKey::GroupHeader(TranscriptViewItemId::new("g1"));
        assert_eq!(
            facts.correspondence(&key("g2")),
            Some(Correspondence::Collapsed(header))
        );
        assert_eq!(facts.correspondence(&key("u")), None);
    }

    #[test]
    fn p4_removed_unit_without_header_moves_to_next_survivor() {
        let mut units = Units::new();
        let items = vec![item("a", 1, true), item("b", 2, false), item("c", 3, false)];
        let _ = units.apply(
            &TranscriptChanges::default(),
            &items,
            WindowMode::Live,
            &FlatStructure,
        );
        let remaining = vec![items[0].clone(), items[2].clone()];
        let changes = units.apply(
            &TranscriptChanges::default(),
            &remaining,
            WindowMode::Live,
            &FlatStructure,
        );
        let facts = UnitFactsView::new(&units, &changes.correspondence);
        assert_eq!(
            facts.correspondence(&key("b")),
            Some(Correspondence::Next(key("c")))
        );
    }
}
