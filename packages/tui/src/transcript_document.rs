//! Revision-tracked transcript document for TUI projection invalidation.

use super::transcript::TranscriptItem;

/// Transcript items plus a collection-level revision.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TranscriptDocument {
    items: Vec<TranscriptItem>,
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
    #[must_use]
    pub const fn len(&self) -> usize {
        self.items.len()
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
    pub fn get_mut(&mut self, index: usize) -> Option<&mut TranscriptItem> {
        self.bump_revision();
        self.items.get_mut(index)
    }

    /// Return a mutable iterator over items and bump the collection revision.
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, TranscriptItem> {
        self.bump_revision();
        self.items.iter_mut()
    }

    /// Return the last item mutably and bump the collection revision if it exists.
    pub fn last_mut(&mut self) -> Option<&mut TranscriptItem> {
        self.bump_revision();
        self.items.last_mut()
    }

    /// Merge streaming boundary with `prefix`, prepend it, and bump the collection revision.
    pub fn merge_prepend(&mut self, prefix: &mut Vec<TranscriptItem>) {
        super::transcript::merge_transcript_boundary(prefix, &mut self.items);
        prefix.append(&mut self.items);
        self.items = std::mem::take(prefix);
        self.bump_revision();
    }

    /// Merge streaming boundary with `suffix`, append it, and bump the collection revision.
    pub fn merge_append(&mut self, suffix: &mut Vec<TranscriptItem>) {
        super::transcript::merge_transcript_boundary(&mut self.items, suffix);
        self.items.append(suffix);
        self.bump_revision();
    }

    /// Apply streaming text to the newest matching role and bump the collection revision.
    pub fn push_streaming_item(&mut self, role: &'static str, text: &str) {
        super::transcript::push_streaming_transcript_item(&mut self.items, role, text);
        self.bump_revision();
    }

    /// Finish streaming text for a role and bump the collection revision.
    pub fn finish_streaming_item(&mut self, role: &'static str, text: &str) {
        super::transcript::finish_streaming_transcript_item(&mut self.items, role, text);
        self.bump_revision();
    }

    /// Push a transcript item and bump the collection revision.
    pub fn push(&mut self, item: TranscriptItem) {
        self.items.push(item);
        self.bump_revision();
    }

    /// Retain transcript items matching a predicate and bump the collection revision if any are removed.
    pub fn retain(&mut self, mut predicate: impl FnMut(&TranscriptItem) -> bool) {
        let before = self.items.len();
        self.items.retain(|item| predicate(item));
        if self.items.len() != before {
            self.bump_revision();
        }
    }

    /// Replace all transcript items and bump the collection revision.
    pub fn replace(&mut self, items: Vec<TranscriptItem>) {
        self.items = items;
        self.bump_revision();
    }

    /// Mutate the newest matching item and return its index.
    pub fn mutate_rev_find(
        &mut self,
        predicate: impl Fn(&TranscriptItem) -> bool,
        update: impl FnOnce(&mut TranscriptItem),
    ) -> Option<usize> {
        let index = self.items.iter().rposition(predicate)?;
        update(&mut self.items[index]);
        self.bump_revision();
        Some(index)
    }

    const fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}
