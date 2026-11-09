//! Unit facts the reducer consults: roles for automatic policy (N9), sequence lookup for
//! timeline jumps (T17, T18, T21) and removal correspondence (P4, U2).
//!
//! The units layer owns unit membership and document order, so it implements [`UnitFacts`]
//! and hands it to the reducer through [`super::NavView`]. Facts are read-only and describe the
//! state *after* the change being reduced (N11).

use std::collections::BTreeMap;

use crate::units::UnitKey;

/// Role of a unit for automatic navigation (C10–C13).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnitRole {
    /// A user message (the target of a submission reveal, T2).
    UserMessage,
    /// An assistant segment (the target of an assistant reveal, T3).
    AssistantSegment,
    /// Any other unit (tools, notices, group headers, markers, boundaries).
    #[default]
    Other,
}

/// Where a removed anchor unit's reading position goes (P4, U2).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Correspondence {
    /// The unit collapsed into a group header or hidden marker; the row offset is kept and
    /// clamped to that unit (U2).
    Collapsed(UnitKey),
    /// The nearest surviving unit after it in document order, at row 0.
    Next(UnitKey),
    /// No surviving unit after it: the nearest surviving unit before it, at its last row.
    Previous(UnitKey),
}

/// Read-only unit facts supplied by the units layer.
pub trait UnitFacts {
    /// Role of a resident unit.
    fn role(&self, unit: &UnitKey) -> UnitRole;

    /// The resident unit that starts the item with this canonical sequence, if resident.
    fn unit_for_sequence(&self, sequence: u64) -> Option<UnitKey>;

    /// Correspondence for a unit that is no longer resident, if it was removed by the change
    /// being reduced.
    fn correspondence(&self, removed: &UnitKey) -> Option<Correspondence>;

    /// Whether a resident assistant segment is still streaming. Only a streaming segment's
    /// first visible content is revealed (T3); an assistant message that arrives complete is
    /// ordinary content growth (T1). The default treats every assistant segment as streaming.
    fn is_streaming(&self, unit: &UnitKey) -> bool {
        self.role(unit) == UnitRole::AssistantSegment
    }
}

/// Facts that know nothing: every unit is [`UnitRole::Other`], no sequence is resident and no
/// removal has a correspondence. Automatic reveal never triggers with these facts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct NoUnitFacts;

impl UnitFacts for NoUnitFacts {
    fn role(&self, _unit: &UnitKey) -> UnitRole {
        UnitRole::Other
    }

    fn unit_for_sequence(&self, _sequence: u64) -> Option<UnitKey> {
        None
    }

    fn correspondence(&self, _removed: &UnitKey) -> Option<Correspondence> {
        None
    }
}

/// Correspondence of the units removed by one membership change (P4, U2).
///
/// Built by the units layer from the unit order before the change and the membership after
/// it, so the reducer can remap an anchor whose unit disappeared without storing rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemovedUnits {
    map: BTreeMap<UnitKey, Correspondence>,
}

impl RemovedUnits {
    /// No removals.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            map: BTreeMap::new(),
        }
    }

    /// Derive correspondence for every unit of `before` that is not resident afterwards.
    ///
    /// `collapsed_into` names the group header or hidden marker a removed member collapsed
    /// into (U2); it is used only when that unit is resident. Otherwise the nearest resident
    /// unit after the removed one is chosen, else the nearest one before it (P4). Boundary
    /// units are never chosen as successors, because their position does not follow content.
    #[must_use]
    pub fn between(
        before: &[UnitKey],
        is_resident: impl Fn(&UnitKey) -> bool,
        collapsed_into: impl Fn(&UnitKey) -> Option<UnitKey>,
    ) -> Self {
        let eligible = |key: &UnitKey| !key.is_boundary() && is_resident(key);
        let mut next: Vec<Option<&UnitKey>> = vec![None; before.len()];
        let mut following = None;
        for (position, key) in before.iter().enumerate().rev() {
            next[position] = following;
            if eligible(key) {
                following = Some(key);
            }
        }
        let mut map = BTreeMap::new();
        let mut preceding: Option<&UnitKey> = None;
        for (position, key) in before.iter().enumerate() {
            if is_resident(key) {
                if eligible(key) {
                    preceding = Some(key);
                }
                continue;
            }
            let correspondence = collapsed_into(key)
                .filter(|header| is_resident(header))
                .map(Correspondence::Collapsed)
                .or_else(|| next[position].cloned().map(Correspondence::Next))
                .or_else(|| preceding.cloned().map(Correspondence::Previous));
            if let Some(correspondence) = correspondence {
                map.insert(key.clone(), correspondence);
            }
        }
        Self { map }
    }

    /// Record one removal.
    pub fn insert(&mut self, removed: UnitKey, correspondence: Correspondence) {
        self.map.insert(removed, correspondence);
    }

    /// Correspondence of a removed unit.
    #[must_use]
    pub fn get(&self, removed: &UnitKey) -> Option<&Correspondence> {
        self.map.get(removed)
    }

    /// Number of recorded removals.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether no removal is recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use bcode_session_view_models::TranscriptViewItemId;

    use super::*;
    use crate::units::BoundaryEdge;

    fn unit(id: &str) -> UnitKey {
        UnitKey::Item(TranscriptViewItemId::new(id))
    }

    #[test]
    fn removed_units_map_to_next_previous_or_header() {
        let before = [
            UnitKey::Boundary(BoundaryEdge::Older),
            unit("a"),
            unit("b"),
            unit("c"),
            unit("d"),
        ];
        let header = UnitKey::GroupHeader(TranscriptViewItemId::new("b"));
        let resident = [
            UnitKey::Boundary(BoundaryEdge::Older),
            unit("a"),
            header.clone(),
        ];
        let removed = RemovedUnits::between(
            &before,
            |key| resident.contains(key),
            |key| (key == &unit("b")).then(|| header.clone()),
        );
        assert_eq!(
            removed.get(&unit("b")),
            Some(&Correspondence::Collapsed(header))
        );
        // c and d have no surviving successor: nearest survivor before them, at its last row.
        assert_eq!(
            removed.get(&unit("c")),
            Some(&Correspondence::Previous(unit("a")))
        );
        assert_eq!(removed.len(), 3);
    }

    #[test]
    fn removed_units_prefer_the_nearest_survivor_after() {
        let before = [unit("a"), unit("b"), unit("c")];
        let removed = RemovedUnits::between(&before, |key| key != &unit("b"), |_| None);
        assert_eq!(
            removed.get(&unit("b")),
            Some(&Correspondence::Next(unit("c")))
        );
        assert!(removed.get(&unit("a")).is_none());
    }
}
