//! Revision-tracked terminal projection of the canonical shared transcript.

use crate::transcript::TranscriptItem;
use std::collections::{BTreeMap, BTreeSet};

#[path = "session_view_terminal_adapter.rs"]
mod session_view_terminal_adapter;
pub use session_view_terminal_adapter::SessionViewTerminalAdapter;

/// Scope of one shared transcript adaptation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TranscriptDocumentDamage {
    /// No terminal item changed.
    #[default]
    None,
    /// Existing items changed in place.
    Items(BTreeSet<bcode_session_view_models::TranscriptViewItemId>),
    /// Item membership or ordering changed.
    Structural,
    /// The source index was inconsistent and required a full reset.
    FullReset,
}

/// Terminal-native items derived exclusively from the shared transcript document.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TranscriptDocument {
    items: Vec<TranscriptItem>,
    source_indices: BTreeMap<bcode_session_view_models::TranscriptViewItemId, usize>,
    revision: u64,
}

impl TranscriptDocument {
    /// Return transcript items.
    #[must_use]
    pub fn items(&self) -> &[TranscriptItem] {
        &self.items
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
        self.items.len()
    }

    /// Return the terminal index for a shared source item.
    #[must_use]
    pub fn source_index(
        &self,
        id: &bcode_session_view_models::TranscriptViewItemId,
    ) -> Option<usize> {
        self.source_indices.get(id).copied()
    }

    /// Return whether the maintained source index matches current items.
    #[must_use]
    pub fn source_index_is_consistent(&self) -> bool {
        self.items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| item.source_view_item_id().map(|id| (id, index)))
            .all(|(id, index)| self.source_indices.get(id) == Some(&index))
            && self.source_indices.len() == self.items.len()
            && self
                .items
                .iter()
                .all(|item| item.source_view_item_id().is_some())
    }

    /// Return an item by index.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&TranscriptItem> {
        self.items.get(index)
    }

    /// Return an iterator over items.
    pub fn iter(&self) -> std::slice::Iter<'_, TranscriptItem> {
        self.items.iter()
    }

    fn upsert_from_shared(&mut self, item: TranscriptItem) -> usize {
        let source_id = item
            .source_view_item_id()
            .expect("shared transcript item must carry source identity")
            .clone();
        if let Some(index) = self.source_indices.get(&source_id).copied() {
            if self
                .items
                .get(index)
                .and_then(TranscriptItem::source_view_item_id)
                != Some(&source_id)
            {
                self.rebuild_source_indices();
                return self.upsert_from_shared(item);
            }
            if self.items[index].replace_from_shared(item) {
                self.bump_revision();
            }
            return index;
        }
        self.items.push(item);
        let index = self.items.len().saturating_sub(1);
        self.source_indices.insert(source_id, index);
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
        if self.items[index].set_stream_integrity(integrity) {
            self.bump_revision();
            return true;
        }
        false
    }

    fn reorder_from_shared(
        &mut self,
        ordered_source_ids: &[bcode_session_view_models::TranscriptViewItemId],
    ) {
        let positions = ordered_source_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.clone(), index))
            .collect::<BTreeMap<_, _>>();
        let before = self
            .items
            .iter()
            .filter_map(TranscriptItem::source_view_item_id)
            .cloned()
            .collect::<Vec<_>>();
        self.items.sort_by_key(|item| {
            item.source_view_item_id()
                .and_then(|id| positions.get(id).copied())
                .unwrap_or(usize::MAX)
        });
        let after = self
            .items
            .iter()
            .filter_map(TranscriptItem::source_view_item_id)
            .cloned()
            .collect::<Vec<_>>();
        if before != after {
            self.rebuild_source_indices();
            self.bump_revision();
        }
    }

    fn retain_from_shared(
        &mut self,
        source_ids: &BTreeSet<bcode_session_view_models::TranscriptViewItemId>,
    ) {
        let before = self.items.len();
        self.items.retain(|item| {
            item.source_view_item_id()
                .is_some_and(|id| source_ids.contains(id))
        });
        if self.items.len() != before {
            self.rebuild_source_indices();
            self.bump_revision();
        }
    }

    fn replace_from_shared(&mut self, items: Vec<TranscriptItem>) {
        debug_assert!(
            items
                .iter()
                .all(|item| item.source_view_item_id().is_some()),
            "terminal transcript items must originate from SessionView"
        );
        self.items = items;
        self.rebuild_source_indices();
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

    fn rebuild_source_indices(&mut self) {
        self.source_indices = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| item.source_view_item_id().cloned().map(|id| (id, index)))
            .collect();
    }

    const fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}
