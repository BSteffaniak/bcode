//! Manual disclosure overrides (U5, D12).
//!
//! Overrides are ephemeral renderer state keyed by [`DisclosureTarget`]: bounded, dropped
//! oldest-first, and reconciled on residency (targets whose items left the resident window are
//! forgotten). They are never persisted (A3).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use bcode_presentation_policy::models::Disclosure;
use bcode_session_view_models::TranscriptViewItemId;

/// Most retained overrides; the oldest is dropped first.
pub const MAX_DISCLOSURE_OVERRIDES: usize = 4096;

/// Something whose disclosure the user can toggle (a disclosure focus target, BMUX 12).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DisclosureTarget {
    /// One unit, by transcript item id.
    Unit(TranscriptViewItemId),
    /// A group, by its first member's transcript item id.
    Group(TranscriptViewItemId),
    /// A named section of a unit, by transcript item id.
    Section(TranscriptViewItemId, String),
}

impl DisclosureTarget {
    /// Transcript item the target belongs to (the first member for groups).
    #[must_use]
    pub const fn item_id(&self) -> &TranscriptViewItemId {
        match self {
            Self::Unit(id) | Self::Group(id) | Self::Section(id, _) => id,
        }
    }

    /// Disclosure one level more open than `current`, or `None` when already expanded
    /// (focus-mode expand). Groups open collapsed → summary → expanded; units and sections
    /// expand directly.
    #[must_use]
    pub const fn expand_level(&self, current: Disclosure) -> Option<Disclosure> {
        match (self, current) {
            (_, Disclosure::Expanded) => None,
            (Self::Group(_), Disclosure::Collapsed | Disclosure::Hidden) => {
                Some(Disclosure::Summary)
            }
            _ => Some(Disclosure::Expanded),
        }
    }

    /// Disclosure one level more closed than `current`, or `None` when already collapsed
    /// (focus-mode collapse; focus then moves to the parent). Groups close expanded → summary
    /// → collapsed; units and sections collapse directly.
    #[must_use]
    pub const fn collapse_level(&self, current: Disclosure) -> Option<Disclosure> {
        match (self, current) {
            (_, Disclosure::Collapsed | Disclosure::Hidden) => None,
            (Self::Group(_), Disclosure::Expanded) => Some(Disclosure::Summary),
            _ => Some(Disclosure::Collapsed),
        }
    }
}

/// `unit:<id>`, `group:<id>` or `section:<id>:<name>`; used as a stable hit identity.
impl fmt::Display for DisclosureTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unit(id) => write!(f, "unit:{}", id.get()),
            Self::Group(id) => write!(f, "group:{}", id.get()),
            Self::Section(id, name) => write!(f, "section:{}:{name}", id.get()),
        }
    }
}

/// Next disclosure in the detail cycle collapsed → summary → expanded → collapsed. Hidden
/// targets enter the cycle at summary, so cycling always reveals something first.
#[must_use]
pub const fn next_detail(current: Disclosure) -> Disclosure {
    match current {
        Disclosure::Collapsed | Disclosure::Hidden => Disclosure::Summary,
        Disclosure::Summary => Disclosure::Expanded,
        Disclosure::Expanded => Disclosure::Collapsed,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Override {
    disclosure: Disclosure,
    sequence: u64,
}

/// Bounded, oldest-first manual disclosure overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DisclosureOverrides {
    entries: BTreeMap<DisclosureTarget, Override>,
    next_sequence: u64,
    revision: u64,
}

impl DisclosureOverrides {
    /// No overrides.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            next_sequence: 0,
            revision: 0,
        }
    }

    /// Override recorded for `target`, before the visibility floor.
    #[must_use]
    pub fn get(&self, target: &DisclosureTarget) -> Option<Disclosure> {
        self.entries.get(target).map(|entry| entry.disclosure)
    }

    /// Number of overrides.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no override is recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Revision bumped by every change; structure resolution re-places items when it moves.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Section overrides of one item as `(section, disclosure)`, in section order. Bounded by
    /// that item's overrides.
    pub fn sections<'a>(
        &'a self,
        item: &'a TranscriptViewItemId,
    ) -> impl Iterator<Item = (&'a str, Disclosure)> + 'a {
        let start = DisclosureTarget::Section(item.clone(), String::new());
        self.entries
            .range(start..)
            .map_while(move |(target, entry)| match target {
                DisclosureTarget::Section(id, name) if id == item => {
                    Some((name.as_str(), entry.disclosure))
                }
                _ => None,
            })
    }

    /// Record `disclosure` for `target`; returns whether anything changed. `replaced` targets
    /// (for example the unit overrides of a group's members when the group is chosen) are
    /// dropped in the same change. The oldest override is dropped beyond
    /// [`MAX_DISCLOSURE_OVERRIDES`].
    pub fn set(
        &mut self,
        target: DisclosureTarget,
        disclosure: Disclosure,
        replaced: &[DisclosureTarget],
    ) -> bool {
        let unchanged = self.get(&target) == Some(disclosure)
            && replaced
                .iter()
                .all(|target| !self.entries.contains_key(target));
        if unchanged {
            return false;
        }
        for target in replaced {
            self.entries.remove(target);
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.entries.insert(
            target,
            Override {
                disclosure,
                sequence,
            },
        );
        // Linear, but only once the bounded map is full.
        while self.entries.len() > MAX_DISCLOSURE_OVERRIDES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.sequence)
                .map(|(target, _)| target.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// Drop every override; returns whether any existed.
    pub fn clear(&mut self) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        self.entries.clear();
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// Forget overrides whose items are no longer resident (D12); returns whether any were
    /// dropped.
    pub fn reconcile(&mut self, resident: &BTreeSet<&TranscriptViewItemId>) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|target, _| resident.contains(target.item_id()));
        let changed = self.entries.len() != before;
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(text: &str) -> TranscriptViewItemId {
        TranscriptViewItemId::new(text)
    }

    #[test]
    fn focus_levels_step_groups_through_summary_and_units_directly() {
        use Disclosure::{Collapsed, Expanded, Hidden, Summary};
        let group = DisclosureTarget::Group(id("g"));
        let unit = DisclosureTarget::Unit(id("u"));
        let section = DisclosureTarget::Section(id("u"), "output".to_owned());
        assert_eq!(group.expand_level(Collapsed), Some(Summary));
        assert_eq!(group.expand_level(Hidden), Some(Summary));
        assert_eq!(group.expand_level(Summary), Some(Expanded));
        assert_eq!(group.expand_level(Expanded), None);
        assert_eq!(group.collapse_level(Expanded), Some(Summary));
        assert_eq!(group.collapse_level(Summary), Some(Collapsed));
        assert_eq!(group.collapse_level(Collapsed), None);
        for direct in [&unit, &section] {
            assert_eq!(direct.expand_level(Collapsed), Some(Expanded));
            assert_eq!(direct.collapse_level(Expanded), Some(Collapsed));
            assert_eq!(direct.collapse_level(Hidden), None);
        }
        assert_eq!(next_detail(Hidden), Summary);
        assert_eq!(next_detail(Summary), Expanded);
        assert_eq!(next_detail(Expanded), Collapsed);
    }

    #[test]
    fn d12_overrides_are_bounded_oldest_first() {
        let mut overrides = DisclosureOverrides::new();
        for n in 0..=MAX_DISCLOSURE_OVERRIDES {
            overrides.set(
                DisclosureTarget::Unit(id(&format!("i{n}"))),
                Disclosure::Collapsed,
                &[],
            );
        }
        assert_eq!(overrides.len(), MAX_DISCLOSURE_OVERRIDES);
        assert_eq!(overrides.get(&DisclosureTarget::Unit(id("i0"))), None);
        assert_eq!(
            overrides.get(&DisclosureTarget::Unit(id("i1"))),
            Some(Disclosure::Collapsed)
        );
    }

    #[test]
    fn d12_reconcile_forgets_non_resident_targets() {
        let mut overrides = DisclosureOverrides::new();
        overrides.set(DisclosureTarget::Unit(id("a")), Disclosure::Collapsed, &[]);
        overrides.set(
            DisclosureTarget::Section(id("b"), "output".to_owned()),
            Disclosure::Expanded,
            &[],
        );
        let resident = [id("b")];
        let resident = resident.iter().collect::<BTreeSet<_>>();
        assert!(overrides.reconcile(&resident));
        assert_eq!(overrides.get(&DisclosureTarget::Unit(id("a"))), None);
        assert_eq!(
            overrides.sections(&id("b")).collect::<Vec<_>>(),
            vec![("output", Disclosure::Expanded)]
        );
        assert!(!overrides.reconcile(&resident));
    }

    #[test]
    fn setting_a_group_replaces_member_choices() {
        let mut overrides = DisclosureOverrides::new();
        overrides.set(DisclosureTarget::Unit(id("m")), Disclosure::Expanded, &[]);
        let revision = overrides.revision();
        assert!(overrides.set(
            DisclosureTarget::Group(id("g")),
            Disclosure::Summary,
            &[DisclosureTarget::Unit(id("m"))],
        ));
        assert!(overrides.revision() > revision);
        assert_eq!(overrides.get(&DisclosureTarget::Unit(id("m"))), None);
        assert!(!overrides.set(DisclosureTarget::Group(id("g")), Disclosure::Summary, &[]));
    }
}
