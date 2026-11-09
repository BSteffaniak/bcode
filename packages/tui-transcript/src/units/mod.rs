//! List units derived from the transcript document (spec section 3.3, rules U1–U5).
//!
//! A *unit* is one virtual-list item: a transcript item, a group header, a hidden-items marker
//! or a resident-window boundary row. Unit order equals document order (U1).
//!
//! [`Units::apply`] derives units from the document after every transcript operation and
//! reports the difference as keyed [`UnitChanges`] (splices and revisions), which the viewport
//! applies to the measured index incrementally (U4, L1). Revision-only changes take a fast path
//! that touches only the revised units; membership changes compare unit keys once and splice
//! only the units that were inserted or removed, so measurement work stays proportional to the
//! change.

pub mod facts;
pub mod structure;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use bcode_session_view_models::{
    TranscriptChange, TranscriptChanges, TranscriptViewItem, TranscriptViewItemId, ViewRevision,
};

use crate::navigation::{RemovedUnits, UnitRole};
use crate::window::WindowMode;
use structure::{ItemPlacement, UnitStructure, starts_turn};

/// Edge of the resident window a [`UnitKey::Boundary`] unit stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BoundaryEdge {
    /// Before the oldest resident turn; always present (U3).
    Older,
    /// After the newest resident turn; present only in detached windows (U3).
    Newer,
}

/// Stable identity of one unit.
///
/// Keys survive insertion, removal, reorder and reflow; they key the layout cache, the measured
/// index and every navigation anchor.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnitKey {
    /// One transcript item (or a member of an expanded group).
    Item(TranscriptViewItemId),
    /// Header of a group, keyed by its first member id.
    GroupHeader(TranscriptViewItemId),
    /// "N hidden" marker, keyed by its first hidden item id.
    HiddenMarker(TranscriptViewItemId),
    /// Resident-window boundary row (constant height 1, U3).
    Boundary(BoundaryEdge),
}

impl UnitKey {
    /// The transcript item id this unit is keyed by, if any.
    #[must_use]
    pub const fn item_id(&self) -> Option<&TranscriptViewItemId> {
        match self {
            Self::Item(id) | Self::GroupHeader(id) | Self::HiddenMarker(id) => Some(id),
            Self::Boundary(_) => None,
        }
    }

    /// Whether this unit is a resident-window boundary row.
    #[must_use]
    pub const fn is_boundary(&self) -> bool {
        matches!(self, Self::Boundary(_))
    }
}

/// Renders the key as a stable string; BMUX uses it for per-item layout ids.
impl fmt::Display for UnitKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Item(id) => write!(f, "item:{}", id.get()),
            Self::GroupHeader(id) => write!(f, "group:{}", id.get()),
            Self::HiddenMarker(id) => write!(f, "hidden:{}", id.get()),
            Self::Boundary(BoundaryEdge::Older) => f.write_str("boundary:older"),
            Self::Boundary(BoundaryEdge::Newer) => f.write_str("boundary:newer"),
        }
    }
}

/// What a unit shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnitKind {
    /// One transcript item.
    Item,
    /// Group header summarizing `members` items.
    GroupHeader {
        /// Number of member items.
        members: u64,
        /// Whether member units follow the header.
        expanded: bool,
    },
    /// Marker for `count` hidden items.
    HiddenMarker {
        /// Number of hidden items.
        count: u64,
    },
    /// Resident-window boundary row.
    Boundary(BoundaryEdge),
}

/// One derived unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitEntry {
    /// Unit identity.
    pub key: UnitKey,
    /// What the unit shows.
    pub kind: UnitKind,
    /// Canonical sequence that owns the unit (the first member's for groups and markers);
    /// `None` for live-only items and boundary rows.
    pub sequence: Option<u64>,
    /// Whether the unit (or any member) is streaming.
    pub streaming: bool,
    /// Role for automatic navigation (C10–C13).
    pub role: UnitRole,
    /// Unit layout revision. Bumped whenever the unit's layout inputs may have changed; never
    /// reused for different content of the same key. Boundary units keep revision 0 because
    /// their state changes are paint-only (U3).
    pub revision: ViewRevision,
    signature: u64,
}

/// One keyed membership change, in application order.
///
/// `at` is the index in the unit list as it stands just before this splice is applied: replace
/// `at..at + removed.len()` with `inserted` (the same convention as BMUX `WindowSplice`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitSplice {
    /// First affected index.
    pub at: usize,
    /// Removed keys, in order.
    pub removed: Vec<UnitKey>,
    /// Inserted keys, in order.
    pub inserted: Vec<UnitKey>,
}

impl UnitSplice {
    /// The index range this splice replaces.
    #[must_use]
    pub const fn removed_range(&self) -> std::ops::Range<usize> {
        self.at..self.at + self.removed.len()
    }
}

/// Unit-level result of one derivation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitChanges {
    /// Membership splices in application order.
    pub splices: Vec<UnitSplice>,
    /// Surviving units whose layout revision changed.
    pub revised: Vec<UnitKey>,
    /// Surviving units changed relative order, so the splices replace the whole list.
    pub reset: bool,
    /// Where each removed unit's reading position goes (P4, U2).
    pub correspondence: RemovedUnits,
}

impl UnitChanges {
    /// Whether nothing changed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.splices.is_empty() && self.revised.is_empty()
    }

    /// Whether unit membership changed.
    #[must_use]
    pub const fn is_structural(&self) -> bool {
        !self.splices.is_empty()
    }

    /// Keys inserted by any splice, in application order.
    pub fn inserted(&self) -> impl Iterator<Item = &UnitKey> + '_ {
        self.splices
            .iter()
            .flat_map(|splice| splice.inserted.iter())
    }

    /// Keys removed by any splice, in application order.
    pub fn removed(&self) -> impl Iterator<Item = &UnitKey> + '_ {
        self.splices.iter().flat_map(|splice| splice.removed.iter())
    }
}

/// Ordered unit membership with per-unit layout revisions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Units {
    order: Vec<UnitKey>,
    entries: BTreeMap<UnitKey, UnitEntry>,
    owners: BTreeMap<TranscriptViewItemId, UnitKey>,
    placements: BTreeMap<TranscriptViewItemId, ItemPlacement>,
    by_sequence: BTreeMap<u64, UnitKey>,
    /// Position of each resident item in the document the units were derived from. Revisions
    /// never move items, so only structural derivations rebuild it.
    item_positions: BTreeMap<TranscriptViewItemId, usize>,
    mode: WindowMode,
    next_revision: ViewRevision,
}

impl Units {
    /// Create an empty unit list. The first [`Self::apply`] adds the boundary units.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            order: Vec::new(),
            entries: BTreeMap::new(),
            owners: BTreeMap::new(),
            placements: BTreeMap::new(),
            by_sequence: BTreeMap::new(),
            item_positions: BTreeMap::new(),
            mode: WindowMode::Live,
            next_revision: 1,
        }
    }

    /// Unit keys in document order.
    #[must_use]
    pub fn keys(&self) -> &[UnitKey] {
        &self.order
    }

    /// Number of units.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether there are no units.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Layout revision of a unit, if it exists.
    #[must_use]
    pub fn revision(&self, key: &UnitKey) -> Option<ViewRevision> {
        self.entries.get(key).map(|entry| entry.revision)
    }

    /// Whether the unit exists.
    #[must_use]
    pub fn contains(&self, key: &UnitKey) -> bool {
        self.entries.contains_key(key)
    }

    /// The derived unit for a key.
    #[must_use]
    pub fn entry(&self, key: &UnitKey) -> Option<&UnitEntry> {
        self.entries.get(key)
    }

    /// Units in document order.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = &UnitEntry> + '_ {
        self.order.iter().map(|key| &self.entries[key])
    }

    /// Position of a unit in document order (linear; navigation uses the measured index).
    #[must_use]
    pub fn index_of(&self, key: &UnitKey) -> Option<usize> {
        if !self.entries.contains_key(key) {
            return None;
        }
        self.order.iter().position(|candidate| candidate == key)
    }

    /// Window mode the units were derived for.
    #[must_use]
    pub const fn mode(&self) -> WindowMode {
        self.mode
    }

    /// The unit currently representing a transcript item: the item's own unit, or the group
    /// header or hidden marker it is folded into (U2). `None` when the item is not resident.
    #[must_use]
    pub fn owner_unit(&self, id: &TranscriptViewItemId) -> Option<&UnitKey> {
        self.owners.get(id)
    }

    /// The document item `id` names, looked up through the positions recorded at the last
    /// derivation (logarithmic). `items` must be the document the units were derived from;
    /// `None` when the item is not resident there.
    #[must_use]
    pub fn item<'i>(
        &self,
        items: &'i [TranscriptViewItem],
        id: &TranscriptViewItemId,
    ) -> Option<&'i TranscriptViewItem> {
        self.item_positions
            .get(id)
            .and_then(|position| items.get(*position))
            .filter(|item| &item.id == id)
    }

    /// The first unit owned by a canonical sequence, if resident.
    #[must_use]
    pub fn unit_for_sequence(&self, sequence: u64) -> Option<&UnitKey> {
        self.by_sequence.get(&sequence)
    }

    /// Owning canonical sequence of a unit.
    #[must_use]
    pub fn sequence(&self, key: &UnitKey) -> Option<u64> {
        self.entries.get(key).and_then(|entry| entry.sequence)
    }

    /// Exact rows per owning canonical sequence, summed over units with a sequence.
    #[must_use]
    pub fn rows_by_sequence(&self, height: impl Fn(&UnitKey) -> u64) -> BTreeMap<u64, u64> {
        let mut rows = BTreeMap::new();
        for entry in self.entries() {
            if let Some(sequence) = entry.sequence {
                *rows.entry(sequence).or_insert(0) += height(&entry.key);
            }
        }
        rows
    }

    /// Re-derive units from the document after a transcript operation (U4).
    ///
    /// `changes` is the operation's item-level change set; `items` is the document after it.
    /// Returns the keyed unit difference to apply to the measured index.
    pub fn apply(
        &mut self,
        changes: &TranscriptChanges,
        items: &[TranscriptViewItem],
        mode: WindowMode,
        structure: &dyn UnitStructure,
    ) -> UnitChanges {
        if !self.order.is_empty()
            && mode == self.mode
            && let Some(fast) = self.try_revision_fast_path(changes, items, structure)
        {
            return fast;
        }
        let revised_ids = changes
            .changes
            .iter()
            .filter_map(|change| match change {
                TranscriptChange::Revised { id, .. } => Some(id),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let plan = derive(items, mode, structure);
        self.mode = mode;
        self.item_positions = items
            .iter()
            .enumerate()
            .map(|(position, item)| (item.id.clone(), position))
            .collect();
        self.reconcile(plan, &revised_ids)
    }

    /// Re-derive units for a window mode change without document changes (U3).
    pub fn set_mode(
        &mut self,
        items: &[TranscriptViewItem],
        mode: WindowMode,
        structure: &dyn UnitStructure,
    ) -> UnitChanges {
        if mode == self.mode && !self.order.is_empty() {
            return UnitChanges::default();
        }
        self.apply(&TranscriptChanges::default(), items, mode, structure)
    }

    fn try_revision_fast_path(
        &mut self,
        changes: &TranscriptChanges,
        items: &[TranscriptViewItem],
        structure: &dyn UnitStructure,
    ) -> Option<UnitChanges> {
        if changes.changes.is_empty()
            || changes
                .changes
                .iter()
                .any(|change| !matches!(change, TranscriptChange::Revised { .. }))
        {
            return None;
        }
        let mut targets = Vec::new();
        for change in &changes.changes {
            let TranscriptChange::Revised { id, .. } = change else {
                return None;
            };
            let item = self.item(items, id)?;
            let placement = structure.placement(item);
            if self.placements.get(id) != Some(&placement) {
                return None;
            }
            targets.push((
                id.clone(),
                item_signature(item, structure),
                item.streaming,
                placement,
            ));
        }
        let mut revised = Vec::new();
        for (id, signature, streaming, placement) in targets {
            if matches!(placement, ItemPlacement::Hidden | ItemPlacement::Suppressed) {
                // A hidden item's content does not change its marker's layout; a suppressed
                // item has no unit.
                continue;
            }
            let owner = self.owners.get(&id)?.clone();
            let revision = self.issue_revision();
            let entry = self.entries.get_mut(&owner)?;
            entry.revision = revision;
            entry.streaming = streaming;
            // Items keep the signature the full derivation computes; a header's signature
            // diverges, so the next full derivation re-revises it once (harmless).
            entry.signature = match owner {
                UnitKey::Item(_) => signature,
                _ => mix(entry.signature, signature),
            };
            if !revised.contains(&owner) {
                revised.push(owner);
            }
        }
        Some(UnitChanges {
            splices: Vec::new(),
            revised,
            reset: false,
            correspondence: RemovedUnits::new(),
        })
    }

    const fn issue_revision(&mut self) -> ViewRevision {
        let revision = self.next_revision;
        self.next_revision = self.next_revision.saturating_add(1);
        revision
    }

    fn reconcile(
        &mut self,
        plan: Plan,
        revised_ids: &BTreeSet<&TranscriptViewItemId>,
    ) -> UnitChanges {
        let new_order = plan
            .units
            .iter()
            .map(|unit| unit.key.clone())
            .collect::<Vec<_>>();
        let (splices, reset) = keyed_splices(&self.order, &new_order);
        let new_keys = new_order.iter().collect::<BTreeSet<_>>();
        let correspondence = if splices.is_empty() {
            RemovedUnits::new()
        } else {
            RemovedUnits::between(
                &self.order,
                |key| new_keys.contains(key),
                |key| {
                    key.item_id()
                        .and_then(|id| plan.owners.get(id))
                        .filter(|owner| *owner != key)
                        .cloned()
                },
            )
        };

        let mut revised = Vec::new();
        let mut entries = BTreeMap::new();
        for planned in plan.units {
            let touched = planned.members.iter().any(|id| revised_ids.contains(id));
            let revision = match self.entries.get(&planned.key) {
                _ if planned.key.is_boundary() => 0,
                Some(old) if old.signature == planned.signature && !touched => old.revision,
                Some(_) => {
                    revised.push(planned.key.clone());
                    self.issue_revision()
                }
                None => self.issue_revision(),
            };
            entries.insert(
                planned.key.clone(),
                UnitEntry {
                    key: planned.key,
                    kind: planned.kind,
                    sequence: planned.sequence,
                    streaming: planned.streaming,
                    role: planned.role,
                    revision,
                    signature: planned.signature,
                },
            );
        }
        self.by_sequence.clear();
        for key in &new_order {
            if let Some(sequence) = entries
                .get(key)
                .and_then(|entry: &UnitEntry| entry.sequence)
            {
                self.by_sequence
                    .entry(sequence)
                    .or_insert_with(|| key.clone());
            }
        }
        self.order = new_order;
        self.entries = entries;
        self.owners = plan.owners;
        self.placements = plan.placements;
        UnitChanges {
            splices,
            revised,
            reset,
            correspondence,
        }
    }
}

#[derive(Debug)]
struct PlannedUnit {
    key: UnitKey,
    kind: UnitKind,
    sequence: Option<u64>,
    streaming: bool,
    role: UnitRole,
    signature: u64,
    members: Vec<TranscriptViewItemId>,
}

#[derive(Debug, Default)]
struct Plan {
    units: Vec<PlannedUnit>,
    owners: BTreeMap<TranscriptViewItemId, UnitKey>,
    placements: BTreeMap<TranscriptViewItemId, ItemPlacement>,
}

/// Deterministic 64-bit mixing for derivation signatures (no pointers, no time).
const fn mix(state: u64, value: u64) -> u64 {
    let mixed = (state ^ value).wrapping_mul(0x0100_0000_01b3);
    mixed ^ (mixed >> 29)
}

/// A run of folded items being collected.
enum Run {
    Hidden(Vec<usize>),
    Group {
        group: u64,
        expanded: bool,
        members: Vec<usize>,
    },
}

const fn boundary_unit(edge: BoundaryEdge) -> PlannedUnit {
    PlannedUnit {
        key: UnitKey::Boundary(edge),
        kind: UnitKind::Boundary(edge),
        sequence: None,
        streaming: false,
        role: UnitRole::Other,
        signature: 0,
        members: Vec::new(),
    }
}

fn derive(items: &[TranscriptViewItem], mode: WindowMode, structure: &dyn UnitStructure) -> Plan {
    let mut plan = Plan::default();
    plan.units.push(boundary_unit(BoundaryEdge::Older));
    let mut run: Option<Run> = None;
    for (index, item) in items.iter().enumerate() {
        let placement = structure.placement(item);
        plan.placements.insert(item.id.clone(), placement);
        if placement == ItemPlacement::Suppressed {
            // No unit and no marker; the run around it continues.
            continue;
        }
        let continues = match (&run, placement) {
            (Some(Run::Hidden(_)), ItemPlacement::Hidden) => !starts_turn(item),
            (Some(Run::Group { group, .. }), ItemPlacement::Grouped { group: next, .. }) => {
                *group == next && !starts_turn(item)
            }
            _ => false,
        };
        if continues {
            if let Some(Run::Hidden(members) | Run::Group { members, .. }) = run.as_mut() {
                members.push(index);
            }
            continue;
        }
        if let Some(finished) = run.take() {
            flush_run(&mut plan, items, finished, structure);
        }
        match placement {
            ItemPlacement::Visible => push_item(&mut plan, item, structure),
            ItemPlacement::Suppressed => {}
            ItemPlacement::Hidden => run = Some(Run::Hidden(vec![index])),
            ItemPlacement::Grouped {
                group, expanded, ..
            } => {
                run = Some(Run::Group {
                    group,
                    expanded,
                    members: vec![index],
                });
            }
        }
    }
    if let Some(finished) = run.take() {
        flush_run(&mut plan, items, finished, structure);
    }
    if mode == WindowMode::Detached {
        plan.units.push(boundary_unit(BoundaryEdge::Newer));
    }
    plan
}

/// Layout signature of an item unit: its revision and its structural signature.
fn item_signature(item: &TranscriptViewItem, structure: &dyn UnitStructure) -> u64 {
    mix(mix(0x51, item.revision), structure.signature(item))
}

fn push_item(plan: &mut Plan, item: &TranscriptViewItem, structure: &dyn UnitStructure) {
    let key = UnitKey::Item(item.id.clone());
    plan.owners.insert(item.id.clone(), key.clone());
    plan.units.push(PlannedUnit {
        key,
        kind: UnitKind::Item,
        sequence: item.sequence,
        streaming: item.streaming,
        role: facts::item_role(item),
        signature: item_signature(item, structure),
        members: vec![item.id.clone()],
    });
}

fn flush_run(
    plan: &mut Plan,
    items: &[TranscriptViewItem],
    run: Run,
    structure: &dyn UnitStructure,
) {
    match run {
        Run::Hidden(members) => {
            let first = &items[members[0]];
            let key = UnitKey::HiddenMarker(first.id.clone());
            let count = members.len() as u64;
            for &member in &members {
                plan.owners.insert(items[member].id.clone(), key.clone());
            }
            plan.units.push(PlannedUnit {
                key,
                kind: UnitKind::HiddenMarker { count },
                sequence: first.sequence,
                streaming: false,
                role: UnitRole::Other,
                signature: mix(0x4d, count),
                members: Vec::new(),
            });
        }
        Run::Group {
            expanded, members, ..
        } => {
            let first = &items[members[0]];
            let header = UnitKey::GroupHeader(first.id.clone());
            let count = members
                .iter()
                .filter(|&&member| {
                    matches!(
                        structure.placement(&items[member]),
                        ItemPlacement::Grouped { member: true, .. }
                    )
                })
                .count() as u64;
            let mut signature = mix(
                mix(mix(0x47, count), u64::from(expanded)),
                structure.group_signature(first),
            );
            for &member in &members {
                signature = mix(signature, items[member].revision);
            }
            plan.units.push(PlannedUnit {
                key: header.clone(),
                kind: UnitKind::GroupHeader {
                    members: count,
                    expanded,
                },
                sequence: first.sequence,
                streaming: members.iter().any(|&member| items[member].streaming),
                role: UnitRole::Other,
                signature,
                members: members
                    .iter()
                    .map(|&member| items[member].id.clone())
                    .collect(),
            });
            for &member in &members {
                let item = &items[member];
                let renders = match structure.placement(item) {
                    ItemPlacement::Grouped { rendering, .. } => rendering.has_unit(),
                    _ => expanded,
                };
                if renders {
                    push_item(plan, item, structure);
                } else {
                    plan.owners.insert(item.id.clone(), header.clone());
                }
            }
        }
    }
}

/// Keyed difference between two unit orders as sequential splices.
///
/// Surviving keys keep their relative order in normal operation (U1), so the difference is a
/// series of removed and inserted runs between surviving keys. If surviving keys changed
/// relative order, the result is one splice replacing the whole list and `true`.
fn keyed_splices(old: &[UnitKey], new: &[UnitKey]) -> (Vec<UnitSplice>, bool) {
    let old_set = old.iter().collect::<BTreeSet<_>>();
    let new_set = new.iter().collect::<BTreeSet<_>>();
    let mut splices = Vec::new();
    let (mut i, mut j, mut position) = (0, 0, 0);
    while i < old.len() || j < new.len() {
        if i < old.len() && j < new.len() && old[i] == new[j] {
            i += 1;
            j += 1;
            position += 1;
            continue;
        }
        let mut removed = Vec::new();
        while i < old.len() && !new_set.contains(&old[i]) {
            removed.push(old[i].clone());
            i += 1;
        }
        let mut inserted = Vec::new();
        while j < new.len() && !old_set.contains(&new[j]) {
            inserted.push(new[j].clone());
            j += 1;
        }
        if removed.is_empty() && inserted.is_empty() {
            let full = UnitSplice {
                at: 0,
                removed: old.to_vec(),
                inserted: new.to_vec(),
            };
            return (vec![full], true);
        }
        let inserted_len = inserted.len();
        splices.push(UnitSplice {
            at: position,
            removed,
            inserted,
        });
        position += inserted_len;
    }
    (splices, false)
}

#[cfg(test)]
mod tests {
    use bcode_session_view_models::{ChatMessageView, TranscriptViewItemKind};

    use super::structure::FlatStructure;
    use super::*;

    fn item(id: &str, sequence: u64, revision: u64, user: bool) -> TranscriptViewItem {
        let message = ChatMessageView::markdown(format!("text {id}"));
        TranscriptViewItem {
            id: TranscriptViewItemId::new(id),
            revision,
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

    fn apply_all(units: &mut Units, items: &[TranscriptViewItem]) -> UnitChanges {
        let changes = TranscriptChanges {
            changes: vec![TranscriptChange::Reset],
            ..TranscriptChanges::default()
        };
        units.apply(&changes, items, WindowMode::Live, &FlatStructure)
    }

    /// Apply splices to a key list the way the measured index does.
    fn replay(mut keys: Vec<UnitKey>, changes: &UnitChanges) -> Vec<UnitKey> {
        for splice in &changes.splices {
            assert_eq!(
                &keys[splice.removed_range()],
                splice.removed.as_slice(),
                "splice removes the keys it names"
            );
            keys.splice(splice.removed_range(), splice.inserted.iter().cloned());
        }
        keys
    }

    #[test]
    fn u1_u3_units_follow_document_order_with_older_boundary_first() {
        let mut units = Units::new();
        let items = vec![item("a", 1, 1, true), item("b", 2, 1, false)];
        let changes = apply_all(&mut units, &items);
        assert_eq!(
            units.keys(),
            &[UnitKey::Boundary(BoundaryEdge::Older), key("a"), key("b")]
        );
        assert_eq!(replay(Vec::new(), &changes), units.keys());
        assert_eq!(
            units.revision(&UnitKey::Boundary(BoundaryEdge::Older)),
            Some(0)
        );
    }

    #[test]
    fn u3_newer_boundary_exists_only_when_detached() {
        let mut units = Units::new();
        let items = vec![item("a", 1, 1, true)];
        let _ = apply_all(&mut units, &items);
        let before = units.keys().to_vec();
        let detached = units.set_mode(&items, WindowMode::Detached, &FlatStructure);
        assert_eq!(
            units.keys().last(),
            Some(&UnitKey::Boundary(BoundaryEdge::Newer))
        );
        assert_eq!(replay(before.clone(), &detached), units.keys());
        let live = units.set_mode(&items, WindowMode::Live, &FlatStructure);
        assert_eq!(units.keys(), before.as_slice());
        // Removing the newer boundary touches only that unit.
        assert_eq!(live.splices.len(), 1);
        assert_eq!(
            live.splices[0].removed,
            vec![UnitKey::Boundary(BoundaryEdge::Newer)]
        );
    }

    #[test]
    fn u4_prepend_and_tail_removal_splice_only_affected_units() {
        let mut units = Units::new();
        let items = (0..5)
            .map(|n| item(&format!("i{n}"), n + 10, 1, n == 0))
            .collect::<Vec<_>>();
        let _ = apply_all(&mut units, &items);
        let before = units.keys().to_vec();
        let revisions = before
            .iter()
            .map(|key| units.revision(key))
            .collect::<Vec<_>>();

        let mut next = vec![item("p0", 8, 1, true), item("p1", 9, 1, false)];
        next.extend(items[..4].iter().cloned());
        let changes = TranscriptChanges {
            changes: vec![
                TranscriptChange::Inserted {
                    after: None,
                    ids: vec![
                        TranscriptViewItemId::new("p0"),
                        TranscriptViewItemId::new("p1"),
                    ],
                },
                TranscriptChange::Removed {
                    ids: vec![TranscriptViewItemId::new("i4")],
                },
            ],
            ..TranscriptChanges::default()
        };
        let unit_changes = units.apply(&changes, &next, WindowMode::Live, &FlatStructure);
        assert!(!unit_changes.reset);
        assert_eq!(unit_changes.splices.len(), 2);
        assert_eq!(unit_changes.splices[0].at, 1);
        assert_eq!(unit_changes.splices[0].inserted, vec![key("p0"), key("p1")]);
        assert_eq!(unit_changes.splices[1].removed, vec![key("i4")]);
        assert_eq!(unit_changes.revised, Vec::<UnitKey>::new());
        assert_eq!(replay(before.clone(), &unit_changes), units.keys());
        // Surviving units keep their layout revisions (L1).
        for (key, revision) in before.iter().zip(revisions) {
            if units.contains(key) {
                assert_eq!(units.revision(key), revision);
            }
        }
    }

    #[test]
    fn d3_revision_only_change_dirties_only_that_unit() {
        let mut units = Units::new();
        let mut items = vec![item("a", 1, 1, true), item("b", 2, 1, false)];
        let _ = apply_all(&mut units, &items);
        let a_revision = units.revision(&key("a"));
        let b_revision = units.revision(&key("b"));
        items[1].revision = 2;
        let changes = TranscriptChanges {
            changes: vec![TranscriptChange::Revised {
                id: TranscriptViewItemId::new("b"),
                revision: 2,
            }],
            ..TranscriptChanges::default()
        };
        let unit_changes = units.apply(&changes, &items, WindowMode::Live, &FlatStructure);
        assert_eq!(unit_changes.revised, vec![key("b")]);
        assert_eq!(unit_changes.splices, Vec::<UnitSplice>::new());
        assert_eq!(units.revision(&key("a")), a_revision);
        assert_ne!(units.revision(&key("b")), b_revision);
    }

    #[test]
    fn content_change_without_item_revision_still_bumps_layout_revision() {
        let mut units = Units::new();
        let items = vec![item("a", 1, 1, true)];
        let _ = apply_all(&mut units, &items);
        let first = units.revision(&key("a"));
        // A rebuild may report a content change at the same item revision; the general path
        // (forced here by a structural change in the same set) still bumps the unit.
        let changes = TranscriptChanges {
            changes: vec![
                TranscriptChange::Revised {
                    id: TranscriptViewItemId::new("a"),
                    revision: 1,
                },
                TranscriptChange::Removed { ids: Vec::new() },
            ],
            ..TranscriptChanges::default()
        };
        let unit_changes = units.apply(&changes, &items, WindowMode::Live, &FlatStructure);
        assert_eq!(unit_changes.revised, vec![key("a")]);
        assert_ne!(units.revision(&key("a")), first);
    }

    fn grouped(group_ids: &'static [&'static str], expanded: bool) -> impl UnitStructure {
        move |item: &TranscriptViewItem| {
            if group_ids.contains(&item.id.get()) {
                ItemPlacement::grouped(7, expanded)
            } else if item.id.get().starts_with('h') {
                ItemPlacement::Hidden
            } else {
                ItemPlacement::Visible
            }
        }
    }

    #[test]
    fn u2_groups_and_hidden_markers_fold_runs_and_own_members() {
        let items = vec![
            item("u", 1, 1, true),
            item("g1", 2, 1, false),
            item("g2", 3, 1, false),
            item("h1", 4, 1, false),
            item("h2", 5, 1, false),
            item("v", 6, 1, false),
        ];
        let mut units = Units::new();
        let collapsed = grouped(&["g1", "g2"], false);
        let _ = units.apply(
            &TranscriptChanges::default(),
            &items,
            WindowMode::Live,
            &collapsed,
        );
        let header = UnitKey::GroupHeader(TranscriptViewItemId::new("g1"));
        let marker = UnitKey::HiddenMarker(TranscriptViewItemId::new("h1"));
        assert_eq!(
            units.keys(),
            &[
                UnitKey::Boundary(BoundaryEdge::Older),
                key("u"),
                header.clone(),
                marker.clone(),
                key("v")
            ]
        );
        assert_eq!(
            units.entry(&marker).map(|entry| entry.kind),
            Some(UnitKind::HiddenMarker { count: 2 })
        );
        assert_eq!(
            units.owner_unit(&TranscriptViewItemId::new("g2")),
            Some(&header)
        );
        assert_eq!(
            units.owner_unit(&TranscriptViewItemId::new("h2")),
            Some(&marker)
        );

        // Expanding inserts member units after the header and nothing else.
        let before = units.keys().to_vec();
        let expanded = grouped(&["g1", "g2"], true);
        let changes = units.apply(
            &TranscriptChanges::default(),
            &items,
            WindowMode::Live,
            &expanded,
        );
        assert_eq!(changes.splices.len(), 1);
        assert_eq!(changes.splices[0].at, 3);
        assert_eq!(changes.splices[0].inserted, vec![key("g1"), key("g2")]);
        assert_eq!(changes.revised, vec![header]);
        assert_eq!(replay(before, &changes), units.keys());
        assert_eq!(
            units.owner_unit(&TranscriptViewItemId::new("g2")),
            Some(&key("g2"))
        );
    }

    #[test]
    fn a14_groups_never_cross_turn_starts() {
        let items = vec![
            item("g1", 1, 1, false),
            item("g2", 2, 1, true),
            item("g3", 3, 1, false),
        ];
        let structure = |_: &TranscriptViewItem| ItemPlacement::grouped(1, false);
        let mut units = Units::new();
        let _ = units.apply(
            &TranscriptChanges::default(),
            &items,
            WindowMode::Live,
            &structure,
        );
        assert_eq!(
            units.keys(),
            &[
                UnitKey::Boundary(BoundaryEdge::Older),
                UnitKey::GroupHeader(TranscriptViewItemId::new("g1")),
                UnitKey::GroupHeader(TranscriptViewItemId::new("g2")),
            ]
        );
    }

    #[test]
    fn reorder_of_surviving_units_resets_the_list() {
        let (splices, reset) = keyed_splices(&[key("a"), key("b")], &[key("b"), key("a")]);
        assert!(reset);
        assert_eq!(splices.len(), 1);
        assert_eq!(splices[0].inserted, vec![key("b"), key("a")]);
    }

    #[test]
    fn rows_by_sequence_sums_unit_heights() {
        let mut units = Units::new();
        let items = vec![item("a", 4, 1, true), item("b", 5, 1, false)];
        let _ = apply_all(&mut units, &items);
        let rows = units.rows_by_sequence(|unit| if unit == &key("a") { 3 } else { 2 });
        assert_eq!(rows.get(&4), Some(&3));
        assert_eq!(rows.get(&5), Some(&2));
        assert_eq!(rows.len(), 2);
    }
}
