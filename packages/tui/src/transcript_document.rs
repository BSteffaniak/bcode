//! Revision-tracked terminal presentation document.

use crate::transcript::TranscriptItem;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "session_view_terminal_adapter.rs"]
mod session_view_terminal_adapter;
pub use session_view_terminal_adapter::SessionViewTerminalAdapter;

/// Stable identity for one entry in the terminal presentation document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TranscriptPresentationEntryId(u64);

impl TranscriptPresentationEntryId {
    fn next() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// Ownership of one terminal presentation entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptPresentationOrigin {
    /// Entry adapted from the canonical shared session view.
    Canonical {
        /// Stable shared transcript identity.
        source_item_id: bcode_session_view_models::TranscriptViewItemId,
    },
    /// Process-local entry owned by this TUI presentation context.
    Ephemeral {
        /// Stable process-local notice identity.
        notice_id: u64,
        /// Component or workflow that produced the notice.
        source: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EphemeralPlacement {
    after_source_item: Option<bcode_session_view_models::TranscriptViewItemId>,
    after_sequence: Option<u64>,
    insertion_order: u64,
    fallback_index: usize,
}

/// One owned entry in the ordered terminal presentation document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptPresentationEntry {
    id: TranscriptPresentationEntryId,
    origin: TranscriptPresentationOrigin,
    placement: Option<EphemeralPlacement>,
    item: TranscriptItem,
}

impl TranscriptPresentationEntry {
    /// Return stable presentation identity.
    #[must_use]
    pub const fn id(&self) -> TranscriptPresentationEntryId {
        self.id
    }

    /// Return entry provenance.
    #[must_use]
    pub const fn origin(&self) -> &TranscriptPresentationOrigin {
        &self.origin
    }

    /// Return the terminal rendering payload.
    #[must_use]
    pub const fn item(&self) -> &TranscriptItem {
        &self.item
    }
}

/// Zero-copy ordered view over terminal transcript payloads.
#[derive(Debug, Clone, Copy)]
pub struct TranscriptItems<'a> {
    entries: &'a [TranscriptPresentationEntry],
}

impl<'a> TranscriptItems<'a> {
    pub(crate) const fn new(entries: &'a [TranscriptPresentationEntry]) -> Self {
        Self { entries }
    }

    /// Return the number of visible transcript items.
    #[must_use]
    pub const fn len(self) -> usize {
        self.entries.len()
    }

    /// Return whether no transcript items are visible.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.entries.is_empty()
    }

    /// Return one visible item by index.
    #[must_use]
    pub fn get(self, index: usize) -> Option<&'a TranscriptItem> {
        self.entries
            .get(index)
            .map(TranscriptPresentationEntry::item)
    }

    /// Iterate visible items in presentation order.
    pub fn iter(self) -> impl ExactSizeIterator<Item = &'a TranscriptItem> + DoubleEndedIterator {
        self.entries.iter().map(TranscriptPresentationEntry::item)
    }
}

impl<'a> IntoIterator for TranscriptItems<'a> {
    type Item = &'a TranscriptItem;
    type IntoIter = std::iter::Map<
        std::slice::Iter<'a, TranscriptPresentationEntry>,
        fn(&'a TranscriptPresentationEntry) -> &'a TranscriptItem,
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter().map(TranscriptPresentationEntry::item)
    }
}

#[cfg(test)]
impl PartialEq<&[TranscriptItem]> for TranscriptItems<'_> {
    fn eq(&self, other: &&[TranscriptItem]) -> bool {
        self.iter().eq(other.iter())
    }
}

impl std::ops::Index<usize> for TranscriptItems<'_> {
    type Output = TranscriptItem;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index)
            .expect("transcript item index out of bounds")
    }
}

/// Scope of one shared transcript adaptation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TranscriptDocumentDamage {
    /// No terminal item changed.
    #[default]
    None,
    /// Existing canonical items changed in place.
    Items(BTreeSet<bcode_session_view_models::TranscriptViewItemId>),
    /// Item membership or ordering changed.
    Structural,
    /// The source index was inconsistent and required a full reset.
    FullReset,
}

/// Ordered terminal-native presentation entries from canonical and ephemeral sources.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TranscriptDocument {
    entries: Vec<TranscriptPresentationEntry>,
    presentation_indices: BTreeMap<TranscriptPresentationEntryId, usize>,
    source_indices: BTreeMap<bcode_session_view_models::TranscriptViewItemId, usize>,
    revision: u64,
    next_notice_id: u64,
}

impl TranscriptDocument {
    /// Return ordered transcript payloads.
    #[must_use]
    pub fn items(&self) -> TranscriptItems<'_> {
        TranscriptItems::new(&self.entries)
    }

    /// Copy process-local entries from another document and reconcile them around this document's canonical entries.
    pub fn copy_ephemeral_from(&mut self, source: &Self) {
        let existing_ids = self
            .entries
            .iter()
            .map(TranscriptPresentationEntry::id)
            .collect::<BTreeSet<_>>();
        let copied = source
            .entries
            .iter()
            .filter(|entry| {
                matches!(entry.origin, TranscriptPresentationOrigin::Ephemeral { .. })
                    && !existing_ids.contains(&entry.id)
            })
            .cloned()
            .collect::<Vec<_>>();
        if copied.is_empty() {
            return;
        }
        self.entries.extend(copied);
        self.next_notice_id = self.next_notice_id.max(source.next_notice_id);
        let order = self.canonical_source_order();
        self.reorder_entries(&order);
        self.bump_revision();
    }

    /// Return the collection revision.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Return item count.
    #[cfg(test)]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return the visible index for a shared source item.
    #[must_use]
    pub fn source_index(
        &self,
        id: &bcode_session_view_models::TranscriptViewItemId,
    ) -> Option<usize> {
        self.source_indices.get(id).copied()
    }

    /// Return the visible index for a presentation entry.
    #[must_use]
    pub fn presentation_index(&self, id: TranscriptPresentationEntryId) -> Option<usize> {
        self.presentation_indices.get(&id).copied()
    }

    /// Return entry provenance at a visible index.
    #[cfg(test)]
    #[must_use]
    pub fn origin(&self, index: usize) -> Option<&TranscriptPresentationOrigin> {
        self.entries
            .get(index)
            .map(TranscriptPresentationEntry::origin)
    }

    /// Return presentation identity at a visible index.
    #[must_use]
    pub fn presentation_id(&self, index: usize) -> Option<TranscriptPresentationEntryId> {
        self.entries.get(index).map(TranscriptPresentationEntry::id)
    }

    /// Return whether the maintained indexes match current entries.
    #[must_use]
    pub fn source_index_is_consistent(&self) -> bool {
        let canonical_count = self
            .entries
            .iter()
            .filter(|entry| matches!(entry.origin, TranscriptPresentationOrigin::Canonical { .. }))
            .count();
        self.entries.iter().enumerate().all(|(index, entry)| {
            self.presentation_indices.get(&entry.id) == Some(&index)
                && match &entry.origin {
                    TranscriptPresentationOrigin::Canonical { source_item_id } => {
                        self.source_indices.get(source_item_id) == Some(&index)
                            && entry.item.source_view_item_id() == Some(source_item_id)
                    }
                    TranscriptPresentationOrigin::Ephemeral { .. } => {
                        entry.item.source_view_item_id().is_none()
                    }
                }
        }) && self.presentation_indices.len() == self.entries.len()
            && self.source_indices.len() == canonical_count
    }

    /// Return an item by visible index.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&TranscriptItem> {
        self.entries
            .get(index)
            .map(TranscriptPresentationEntry::item)
    }

    /// Return an iterator over payloads in presentation order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &TranscriptItem> + DoubleEndedIterator {
        self.entries.iter().map(TranscriptPresentationEntry::item)
    }

    /// Append a process-local notice at the current canonical chronology boundary.
    pub fn push_ephemeral(&mut self, source: String, item: TranscriptItem) {
        debug_assert!(item.source_view_item_id().is_none());
        self.next_notice_id = self.next_notice_id.saturating_add(1);
        let after_source_item = self
            .entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.origin {
                TranscriptPresentationOrigin::Canonical { source_item_id } => {
                    Some(source_item_id.clone())
                }
                TranscriptPresentationOrigin::Ephemeral { .. } => None,
            });
        let after_sequence = self
            .entries
            .iter()
            .filter_map(|entry| entry.item.event_sequence())
            .max();
        self.entries.push(TranscriptPresentationEntry {
            id: TranscriptPresentationEntryId::next(),
            origin: TranscriptPresentationOrigin::Ephemeral {
                notice_id: self.next_notice_id,
                source,
            },
            placement: Some(EphemeralPlacement {
                after_source_item,
                after_sequence,
                insertion_order: self.next_notice_id,
                fallback_index: self.canonical_count(),
            }),
            item,
        });
        self.reorder_entries(&self.canonical_source_order());
        self.bump_revision();
    }

    fn canonical_count(&self) -> usize {
        self.source_indices.len()
    }

    fn canonical_source_order(&self) -> Vec<bcode_session_view_models::TranscriptViewItemId> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.origin {
                TranscriptPresentationOrigin::Canonical { source_item_id } => {
                    Some(source_item_id.clone())
                }
                TranscriptPresentationOrigin::Ephemeral { .. } => None,
            })
            .collect()
    }

    fn upsert_from_shared(&mut self, item: TranscriptItem) -> usize {
        let source_id = item
            .source_view_item_id()
            .expect("shared transcript item must carry source identity")
            .clone();
        if let Some(index) = self.source_indices.get(&source_id).copied() {
            if !matches!(
                self.entries.get(index).map(|entry| &entry.origin),
                Some(TranscriptPresentationOrigin::Canonical { source_item_id }) if source_item_id == &source_id
            ) {
                self.rebuild_indices();
                return self.upsert_from_shared(item);
            }
            if self.entries[index].item.replace_from_shared(item) {
                self.bump_revision();
            }
            return index;
        }
        self.entries.push(TranscriptPresentationEntry {
            id: TranscriptPresentationEntryId::next(),
            origin: TranscriptPresentationOrigin::Canonical {
                source_item_id: source_id,
            },
            placement: None,
            item,
        });
        let index = self.entries.len().saturating_sub(1);
        self.rebuild_indices();
        self.bump_revision();
        index
    }

    fn set_stream_integrity(
        &mut self,
        id: &bcode_session_view_models::TranscriptViewItemId,
        integrity: Option<crate::transcript::TranscriptStreamIntegrity>,
    ) -> bool {
        let Some(index) = self.source_indices.get(id).copied() else {
            return false;
        };
        if self.entries[index].item.set_stream_integrity(integrity) {
            self.bump_revision();
            return true;
        }
        false
    }

    fn reorder_from_shared(
        &mut self,
        ordered_source_ids: &[bcode_session_view_models::TranscriptViewItemId],
    ) {
        let before = self
            .entries
            .iter()
            .map(TranscriptPresentationEntry::id)
            .collect::<Vec<_>>();
        self.reorder_entries(ordered_source_ids);
        let after = self
            .entries
            .iter()
            .map(TranscriptPresentationEntry::id)
            .collect::<Vec<_>>();
        if before != after {
            self.bump_revision();
        }
    }

    fn reorder_entries(
        &mut self,
        ordered_source_ids: &[bcode_session_view_models::TranscriptViewItemId],
    ) {
        let canonical_positions = ordered_source_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.clone(), index))
            .collect::<BTreeMap<_, _>>();
        let canonical_sequences = self
            .entries
            .iter()
            .filter_map(|entry| match &entry.origin {
                TranscriptPresentationOrigin::Canonical { source_item_id } => entry
                    .item
                    .event_sequence()
                    .map(|sequence| (source_item_id.clone(), sequence)),
                TranscriptPresentationOrigin::Ephemeral { .. } => None,
            })
            .collect::<BTreeMap<_, _>>();
        self.entries.sort_by_key(|entry| match &entry.origin {
            TranscriptPresentationOrigin::Canonical { source_item_id } => {
                let slot = canonical_positions
                    .get(source_item_id)
                    .copied()
                    .unwrap_or(usize::MAX);
                (slot, 1_u8, 0_u64)
            }
            TranscriptPresentationOrigin::Ephemeral { .. } => {
                let placement = entry
                    .placement
                    .as_ref()
                    .expect("ephemeral entry must carry placement");
                let slot = placement
                    .after_source_item
                    .as_ref()
                    .and_then(|id| canonical_positions.get(id).copied())
                    .map(|index| index.saturating_add(1))
                    .or_else(|| {
                        placement.after_sequence.map(|after_sequence| {
                            ordered_source_ids
                                .iter()
                                .enumerate()
                                .filter(|(_, id)| {
                                    canonical_sequences
                                        .get(*id)
                                        .is_some_and(|sequence| *sequence <= after_sequence)
                                })
                                .map(|(index, _)| index.saturating_add(1))
                                .next_back()
                                .unwrap_or(0)
                        })
                    })
                    .unwrap_or_else(|| placement.fallback_index.min(ordered_source_ids.len()));
                (slot, 0_u8, placement.insertion_order)
            }
        });
        self.rebuild_indices();
    }

    fn retain_from_shared(
        &mut self,
        source_ids: &BTreeSet<bcode_session_view_models::TranscriptViewItemId>,
    ) {
        let before = self.entries.len();
        self.entries.retain(|entry| match &entry.origin {
            TranscriptPresentationOrigin::Canonical { source_item_id } => {
                source_ids.contains(source_item_id)
            }
            TranscriptPresentationOrigin::Ephemeral { .. } => true,
        });
        if self.entries.len() != before {
            self.rebuild_indices();
            self.bump_revision();
        }
    }

    fn replace_from_shared(&mut self, items: Vec<TranscriptItem>) {
        let mut canonical = BTreeMap::new();
        let mut ephemeral = Vec::new();
        for entry in self.entries.drain(..) {
            match &entry.origin {
                TranscriptPresentationOrigin::Canonical { source_item_id } => {
                    canonical.insert(source_item_id.clone(), entry);
                }
                TranscriptPresentationOrigin::Ephemeral { .. } => ephemeral.push(entry),
            }
        }
        self.entries = items
            .into_iter()
            .map(|item| {
                let source_item_id = item
                    .source_view_item_id()
                    .expect("shared transcript item must carry source identity")
                    .clone();
                if let Some(mut entry) = canonical.remove(&source_item_id) {
                    entry.item = item;
                    entry
                } else {
                    TranscriptPresentationEntry {
                        id: TranscriptPresentationEntryId::next(),
                        origin: TranscriptPresentationOrigin::Canonical { source_item_id },
                        placement: None,
                        item,
                    }
                }
            })
            .chain(ephemeral)
            .collect();
        let order = self.canonical_source_order();
        self.reorder_entries(&order);
        self.bump_revision();
    }

    #[cfg(test)]
    pub fn corrupt_source_index_for_test(
        &mut self,
        id: bcode_session_view_models::TranscriptViewItemId,
        index: usize,
    ) {
        self.source_indices.insert(id, index);
    }

    fn rebuild_indices(&mut self) {
        self.presentation_indices = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id, index))
            .collect();
        self.source_indices = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| match &entry.origin {
                TranscriptPresentationOrigin::Canonical { source_item_id } => {
                    Some((source_item_id.clone(), index))
                }
                TranscriptPresentationOrigin::Ephemeral { .. } => None,
            })
            .collect();
    }

    const fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}
