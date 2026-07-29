//! Revision-tracked transcript document for TUI projection invalidation.

use super::transcript::TranscriptItem;
use std::collections::{BTreeMap, BTreeSet};

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

/// Transcript items plus a collection-level revision.
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
            && self.source_indices.len()
                == self
                    .items
                    .iter()
                    .filter(|item| item.source_view_item_id().is_some())
                    .count()
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

    /// Return a mutable item by index and bump the collection revision if it exists.
    #[cfg(test)]
    pub fn get_mut(&mut self, index: usize) -> Option<&mut TranscriptItem> {
        self.bump_revision();
        self.items.get_mut(index)
    }

    /// Return the last item mutably and bump the collection revision if it exists.
    pub fn last_mut(&mut self) -> Option<&mut TranscriptItem> {
        self.bump_revision();
        self.items.last_mut()
    }

    /// Apply streaming text to the newest matching role and bump the collection revision.
    #[cfg(test)]
    pub fn push_streaming_item(&mut self, role: &'static str, text: &str) {
        super::transcript::push_streaming_transcript_item(&mut self.items, role, text);
        self.bump_revision();
    }

    /// Upsert one item adapted from the renderer-neutral session transcript by stable source id.
    pub fn upsert_shared_item(&mut self, item: TranscriptItem) -> usize {
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
                return self.upsert_shared_item(item);
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

    /// Reorder shared items to canonical source order while preserving local-only items.
    pub fn reorder_shared_items(
        &mut self,
        ordered_source_ids: &[bcode_session_view_models::TranscriptViewItemId],
    ) {
        let positions = ordered_source_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.clone(), index))
            .collect::<std::collections::BTreeMap<_, _>>();
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

    /// Finish streaming text for a role and bump the collection revision.
    #[cfg(test)]
    pub fn finish_streaming_item(&mut self, role: &'static str, text: &str) {
        super::transcript::finish_streaming_transcript_item(&mut self.items, role, text);
        self.bump_revision();
    }

    /// Push a transcript item and bump the collection revision.
    pub fn push(&mut self, item: TranscriptItem) {
        let source_id = item.source_view_item_id().cloned();
        self.items.push(item);
        if let Some(id) = source_id {
            self.source_indices
                .insert(id, self.items.len().saturating_sub(1));
        }
        self.bump_revision();
    }

    /// Retain transcript items matching a predicate and bump the collection revision if any are removed.
    pub fn retain(&mut self, mut predicate: impl FnMut(&TranscriptItem) -> bool) {
        let before = self.items.len();
        self.items.retain(|item| predicate(item));
        if self.items.len() != before {
            self.rebuild_source_indices();
            self.bump_revision();
        }
    }

    /// Replace all transcript items and bump the collection revision.
    pub fn replace(&mut self, items: Vec<TranscriptItem>) {
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
