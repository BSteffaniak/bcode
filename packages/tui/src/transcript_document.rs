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

    /// Return the last item mutably and bump the collection revision if it exists.
    pub fn last_mut(&mut self) -> Option<&mut TranscriptItem> {
        self.bump_revision();
        self.items.last_mut()
    }

    /// Apply streaming text to the newest matching role and bump the collection revision.
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
        if let Some(index) = self
            .items
            .iter()
            .position(|existing| existing.source_view_item_id() == Some(&source_id))
        {
            if self.items[index].replace_from_shared(item) {
                self.bump_revision();
            }
            return index;
        }
        self.items.push(item);
        self.bump_revision();
        self.items.len().saturating_sub(1)
    }

    /// Finish streaming text for a role and bump the collection revision.
    pub fn finish_streaming_item(&mut self, role: &'static str, text: &str) {
        super::transcript::finish_streaming_transcript_item(&mut self.items, role, text);
        self.bump_revision();
    }

    /// Upsert a plugin visual item and bump the collection revision.
    pub fn upsert_tool_visual_item(&mut self, item: TranscriptItem) -> usize {
        let index = super::transcript::upsert_tool_visual_item(&mut self.items, item);
        self.bump_revision();
        index
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
