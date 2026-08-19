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
                    let _ = entry.item.replace_from_shared(item);
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

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_view_models::{
        ChatMessageView, TranscriptViewItem, TranscriptViewItemId, TranscriptViewItemKind,
    };

    fn source_id(id: &str) -> TranscriptViewItemId {
        TranscriptViewItemId::new(id)
    }

    fn shared_item(id: &str, revision: u64, sequence: u64, text: &str) -> TranscriptViewItem {
        TranscriptViewItem {
            id: source_id(id),
            revision,
            sequence: Some(sequence),
            timestamp_ms: Some(sequence.saturating_mul(1_000)),
            output_location: None,
            streaming: false,
            kind: TranscriptViewItemKind::AssistantMessage {
                message: ChatMessageView::markdown(text),
            },
        }
    }

    fn canonical(id: &str, revision: u64, sequence: u64, text: &str) -> TranscriptItem {
        crate::transcript::terminal_item_from_shared(&shared_item(id, revision, sequence, text))
    }

    fn notice(text: &str) -> TranscriptItem {
        TranscriptItem::with_format(
            "System",
            text.to_owned(),
            bcode_session_view_models::TextFormat::PlainText,
        )
    }

    fn visible_text(document: &TranscriptDocument) -> Vec<&str> {
        document.iter().map(TranscriptItem::text).collect()
    }

    fn ordered_ids(ids: &[&str]) -> Vec<TranscriptViewItemId> {
        ids.iter().copied().map(source_id).collect()
    }

    #[test]
    fn presentation_identity_is_unique_per_entry() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 1, "first"));
        document.push_ephemeral("bcode.tui".to_owned(), notice("local"));
        document.upsert_from_shared(canonical("b", 1, 2, "second"));

        let ids = (0..document.len())
            .map(|index| document.presentation_id(index).expect("identity"))
            .collect::<BTreeSet<_>>();

        assert_eq!(ids.len(), document.len());
    }

    #[test]
    fn presentation_identity_resolves_back_to_its_current_index() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 2, "first"));
        let canonical_id = document.presentation_id(0).expect("identity");

        document.upsert_from_shared(canonical("older", 1, 1, "older"));
        document.reorder_from_shared(&ordered_ids(&["older", "a"]));

        assert_eq!(document.presentation_index(canonical_id), Some(1));
    }

    #[test]
    fn provenance_is_explicit_for_canonical_and_ephemeral_entries() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 1, "first"));
        document.push_ephemeral("bcode.tui".to_owned(), notice("local"));

        assert_eq!(
            document.origin(0),
            Some(&TranscriptPresentationOrigin::Canonical {
                source_item_id: source_id("a"),
            })
        );
        match document.origin(1).expect("origin") {
            TranscriptPresentationOrigin::Ephemeral { source, .. } => {
                assert_eq!(source, "bcode.tui");
            }
            TranscriptPresentationOrigin::Canonical { .. } => {
                panic!("notice must not be canonical");
            }
        }
        assert!(document.source_index_is_consistent());
    }

    #[test]
    fn ephemeral_entries_are_absent_from_canonical_source_lookup() {
        let mut document = TranscriptDocument::default();
        document.push_ephemeral("bcode.tui".to_owned(), notice("local"));
        document.upsert_from_shared(canonical("a", 1, 1, "first"));

        assert_eq!(document.len(), 2);
        assert_eq!(document.canonical_count(), 1);
        assert_eq!(document.source_index(&source_id("a")), Some(1));
    }

    #[test]
    fn notices_sharing_a_boundary_keep_insertion_order() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 1, "first"));
        document.push_ephemeral("bcode.tui".to_owned(), notice("first notice"));
        document.push_ephemeral("bcode.tui".to_owned(), notice("second notice"));
        document.push_ephemeral("bcode.tui".to_owned(), notice("third notice"));

        assert_eq!(
            visible_text(&document),
            vec!["first", "first notice", "second notice", "third notice"]
        );

        document.reorder_from_shared(&ordered_ids(&["a"]));

        assert_eq!(
            visible_text(&document),
            vec!["first", "first notice", "second notice", "third notice"]
        );
    }

    #[test]
    fn canonical_append_extends_visible_order() {
        let mut document = TranscriptDocument::default();
        assert_eq!(
            document.upsert_from_shared(canonical("a", 1, 1, "first")),
            0
        );
        assert_eq!(
            document.upsert_from_shared(canonical("b", 1, 2, "second")),
            1
        );

        assert_eq!(visible_text(&document), vec!["first", "second"]);
        assert!(document.source_index_is_consistent());
    }

    #[test]
    fn canonical_update_replaces_payload_without_changing_identity() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 1, "first"));
        let entry_id = document.presentation_id(0).expect("identity");
        let revision = document.revision();

        assert_eq!(
            document.upsert_from_shared(canonical("a", 2, 1, "edited")),
            0
        );

        assert_eq!(document.len(), 1);
        assert_eq!(document.presentation_id(0), Some(entry_id));
        assert_eq!(visible_text(&document), vec!["edited"]);
        assert!(document.revision() > revision);
    }

    #[test]
    fn canonical_update_at_same_revision_does_not_bump_document_revision() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 1, "first"));
        let revision = document.revision();

        document.upsert_from_shared(canonical("a", 1, 1, "first"));

        assert_eq!(document.revision(), revision);
    }

    #[test]
    fn canonical_removal_retains_surviving_entries_and_notices() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 1, "first"));
        document.upsert_from_shared(canonical("b", 1, 2, "second"));
        document.push_ephemeral("bcode.tui".to_owned(), notice("local"));

        document.retain_from_shared(&BTreeSet::from([source_id("a")]));

        assert_eq!(visible_text(&document), vec!["first", "local"]);
        assert_eq!(document.source_index(&source_id("a")), Some(0));
        assert_eq!(document.source_index(&source_id("b")), None);
        assert!(document.source_index_is_consistent());
    }

    #[test]
    fn canonical_reorder_moves_entries_and_reanchors_notices() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 1, "first"));
        document.push_ephemeral("bcode.tui".to_owned(), notice("after first"));
        document.upsert_from_shared(canonical("b", 1, 2, "second"));
        assert_eq!(
            visible_text(&document),
            vec!["first", "after first", "second"]
        );

        document.reorder_from_shared(&ordered_ids(&["b", "a"]));

        assert_eq!(
            visible_text(&document),
            vec!["second", "first", "after first"]
        );
        assert!(document.source_index_is_consistent());
    }

    #[test]
    fn canonical_reorder_without_change_does_not_bump_revision() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 1, "first"));
        document.upsert_from_shared(canonical("b", 1, 2, "second"));
        let revision = document.revision();

        document.reorder_from_shared(&ordered_ids(&["a", "b"]));

        assert_eq!(document.revision(), revision);
    }

    #[test]
    fn full_replacement_preserves_canonical_identity_and_notices() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 1, "first"));
        document.push_ephemeral("bcode.tui".to_owned(), notice("local"));
        let canonical_id = document.presentation_id(0).expect("identity");
        let notice_id = document.presentation_id(1).expect("identity");

        document.replace_from_shared(vec![
            canonical("a", 2, 1, "edited"),
            canonical("b", 1, 2, "second"),
        ]);

        assert_eq!(
            visible_text(&document),
            vec!["edited", "local", "second"],
            "the notice keeps its canonical anchor after a full replacement"
        );
        assert_eq!(document.presentation_id(0), Some(canonical_id));
        assert_eq!(document.presentation_id(1), Some(notice_id));
        assert!(document.source_index_is_consistent());
    }

    #[test]
    fn full_replacement_clearing_canonical_entries_keeps_notices() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 1, "first"));
        document.push_ephemeral("bcode.tui".to_owned(), notice("local"));

        document.replace_from_shared(Vec::new());

        assert_eq!(visible_text(&document), vec!["local"]);
        assert!(document.source_index_is_consistent());
    }

    #[test]
    fn notice_before_any_canonical_entry_stays_first() {
        let mut document = TranscriptDocument::default();
        document.push_ephemeral("bcode.tui".to_owned(), notice("local"));

        document.upsert_from_shared(canonical("a", 1, 1, "first"));
        document.reorder_from_shared(&ordered_ids(&["a"]));

        assert_eq!(visible_text(&document), vec!["local", "first"]);
    }

    #[test]
    fn notice_after_last_canonical_entry_stays_last() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 1, "first"));
        document.upsert_from_shared(canonical("b", 1, 2, "second"));

        document.push_ephemeral("bcode.tui".to_owned(), notice("local"));

        assert_eq!(visible_text(&document), vec!["first", "second", "local"]);
    }

    #[test]
    fn notice_between_canonical_entries_keeps_its_boundary() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 1, "first"));
        document.push_ephemeral("bcode.tui".to_owned(), notice("local"));
        document.upsert_from_shared(canonical("b", 1, 2, "second"));

        document.reorder_from_shared(&ordered_ids(&["a", "b"]));

        assert_eq!(visible_text(&document), vec!["first", "local", "second"]);
    }

    #[test]
    fn multiple_interleaved_notices_preserve_each_boundary() {
        let mut document = TranscriptDocument::default();
        document.push_ephemeral("bcode.tui".to_owned(), notice("before all"));
        document.upsert_from_shared(canonical("a", 1, 1, "first"));
        document.push_ephemeral("bcode.tui".to_owned(), notice("after first"));
        document.upsert_from_shared(canonical("b", 1, 2, "second"));
        document.push_ephemeral("bcode.tui".to_owned(), notice("after second"));
        document.upsert_from_shared(canonical("c", 1, 3, "third"));

        document.reorder_from_shared(&ordered_ids(&["a", "b", "c"]));

        assert_eq!(
            visible_text(&document),
            vec![
                "before all",
                "first",
                "after first",
                "second",
                "after second",
                "third",
            ]
        );
        assert!(document.source_index_is_consistent());
    }

    #[test]
    fn notice_falls_back_to_sequence_when_exact_anchor_is_removed() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 10, "first"));
        document.upsert_from_shared(canonical("b", 1, 20, "second"));
        document.push_ephemeral("bcode.tui".to_owned(), notice("local"));
        document.upsert_from_shared(canonical("c", 1, 30, "third"));
        document.reorder_from_shared(&ordered_ids(&["a", "b", "c"]));
        assert_eq!(
            visible_text(&document),
            vec!["first", "second", "local", "third"]
        );

        document.retain_from_shared(&BTreeSet::from([source_id("a"), source_id("c")]));
        document.reorder_from_shared(&ordered_ids(&["a", "c"]));

        assert_eq!(
            visible_text(&document),
            vec!["first", "local", "third"],
            "the notice stays after the newest canonical entry at or before its recorded sequence"
        );
    }

    #[test]
    fn notice_older_than_every_resident_entry_sorts_first() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 10, "first"));
        document.push_ephemeral("bcode.tui".to_owned(), notice("local"));
        document.retain_from_shared(&BTreeSet::new());

        document.upsert_from_shared(canonical("later", 1, 90, "later"));
        document.reorder_from_shared(&ordered_ids(&["later"]));

        assert_eq!(visible_text(&document), vec!["local", "later"]);
    }

    #[test]
    fn inconsistent_source_index_is_reported_and_recovered() {
        let mut document = TranscriptDocument::default();
        document.upsert_from_shared(canonical("a", 1, 1, "first"));
        assert!(document.source_index_is_consistent());

        document.corrupt_source_index_for_test(source_id("a"), 7);
        assert!(!document.source_index_is_consistent());

        document.upsert_from_shared(canonical("a", 2, 1, "edited"));

        assert!(document.source_index_is_consistent());
        assert_eq!(visible_text(&document), vec!["edited"]);
    }

    #[test]
    fn copying_notices_does_not_duplicate_already_present_entries() {
        let mut source = TranscriptDocument::default();
        source.push_ephemeral("bcode.tui".to_owned(), notice("local"));

        let mut target = TranscriptDocument::default();
        target.upsert_from_shared(canonical("a", 1, 1, "first"));
        target.copy_ephemeral_from(&source);
        target.copy_ephemeral_from(&source);

        assert_eq!(visible_text(&target), vec!["local", "first"]);
        assert!(target.source_index_is_consistent());
    }

    #[test]
    fn copying_notices_never_copies_canonical_entries() {
        let mut source = TranscriptDocument::default();
        source.upsert_from_shared(canonical("a", 1, 1, "first"));
        source.push_ephemeral("bcode.tui".to_owned(), notice("local"));

        let mut target = TranscriptDocument::default();
        target.copy_ephemeral_from(&source);

        assert_eq!(visible_text(&target), vec!["local"]);
        assert_eq!(target.canonical_count(), 0);
    }
}
