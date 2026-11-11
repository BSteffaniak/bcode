//! Whole-blob syntax highlighting off the render path (spec section 6.4).
//!
//! Each side of a visible, resident text file is read whole, highlighted once on the blocking
//! pool with BMUX's whole-text projection, and cached by blob identity and palette. Rows look
//! tokens up by line number, verified against the row's text, so a stale or mismatched blob
//! yields no tokens rather than wrong ones. Lines render unhighlighted until their blob is
//! ready; highlighting never changes geometry.

use bcode_code_review_models::{ReviewBlobRef, ReviewBlobSelector, RevisionRef};
use bcode_syntax_render::{HighlightedText, SyntaxHighlighter, SyntaxPalette};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Blobs with more lines are not highlighted.
pub const MAX_HIGHLIGHT_LINES: u32 = 100_000;
/// Blobs with more bytes are not highlighted.
pub const MAX_HIGHLIGHT_BYTES: usize = 8 * 1024 * 1024;
/// Cached highlights kept at most.
const MAX_ENTRIES: usize = 64;
/// Cached highlighted lines kept at most (across entries).
const MAX_TOTAL_LINES: usize = 400_000;
/// Blobs read or highlighted at once.
const MAX_PENDING: usize = 2;

/// Identity of one blob side.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BlobId {
    /// A blob object.
    Object(String),
    /// Working-tree content of a path at a refresh generation.
    WorkingTree {
        /// Repository-relative path.
        path: String,
        /// Refresh generation the content was read in.
        generation: u64,
    },
}

impl BlobId {
    /// Service selector that reads this blob.
    #[must_use]
    pub fn selector(&self) -> ReviewBlobSelector {
        match self {
            Self::Object(oid) => ReviewBlobSelector::Object { oid: oid.clone() },
            Self::WorkingTree { path, .. } => ReviewBlobSelector::Revision {
                revision: RevisionRef::WorkingTree,
                path: path.clone(),
            },
        }
    }
}

/// Cache key: a blob and the palette it was highlighted with.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SyntaxKey {
    /// Blob.
    pub blob: BlobId,
    /// Palette fingerprint.
    pub palette: u64,
}

#[derive(Debug)]
struct Entry {
    text: Option<Arc<HighlightedText>>,
    lines: usize,
    used: Cell<u64>,
}

/// Bounded highlight cache plus in-flight bookkeeping.
#[derive(Debug)]
pub struct SyntaxStore {
    palette: SyntaxPalette,
    palette_hash: u64,
    entries: BTreeMap<SyntaxKey, Entry>,
    pending: BTreeSet<SyntaxKey>,
    total_lines: usize,
    tick: Cell<u64>,
    epoch: u64,
    generation: u64,
}

impl Default for SyntaxStore {
    fn default() -> Self {
        let palette = SyntaxPalette::default();
        Self {
            palette,
            palette_hash: palette_hash(palette),
            entries: BTreeMap::new(),
            pending: BTreeSet::new(),
            total_lines: 0,
            tick: Cell::new(0),
            epoch: 0,
            generation: 0,
        }
    }
}

fn palette_hash(palette: SyntaxPalette) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    palette.hash(&mut hasher);
    hasher.finish()
}

impl SyntaxStore {
    /// Use `palette` for new highlights. Returns whether it changed (rows must be rebuilt).
    pub fn set_palette(&mut self, palette: SyntaxPalette) -> bool {
        if palette == self.palette {
            return false;
        }
        self.palette = palette;
        self.palette_hash = palette_hash(palette);
        self.epoch += 1;
        true
    }

    /// Current palette.
    #[must_use]
    pub const fn palette(&self) -> SyntaxPalette {
        self.palette
    }

    /// Changes whenever a highlight lands or the palette changes.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Treat working-tree content as new (after a refresh).
    pub const fn next_generation(&mut self) {
        self.generation += 1;
    }

    /// Cache key of one blob side of `path`, or `None` for an absent side.
    #[must_use]
    pub fn key(&self, blob: &ReviewBlobRef, path: &str) -> Option<SyntaxKey> {
        let blob = match blob {
            ReviewBlobRef::Object { oid } => BlobId::Object(oid.clone()),
            ReviewBlobRef::WorkingTree => BlobId::WorkingTree {
                path: path.to_owned(),
                generation: self.generation,
            },
            ReviewBlobRef::Absent => return None,
        };
        Some(SyntaxKey {
            blob,
            palette: self.palette_hash,
        })
    }

    /// Ready highlight of `key`.
    #[must_use]
    pub fn get(&self, key: &SyntaxKey) -> Option<&HighlightedText> {
        let entry = self.entries.get(key)?;
        let tick = self.tick.get() + 1;
        self.tick.set(tick);
        entry.used.set(tick);
        entry.text.as_deref()
    }

    /// Whether `key` is cached (highlighted or skipped) or being prepared.
    #[must_use]
    pub fn known(&self, key: &SyntaxKey) -> bool {
        self.entries.contains_key(key) || self.pending.contains(key)
    }

    /// Whether another blob may start being read now.
    #[must_use]
    pub fn has_capacity(&self) -> bool {
        self.pending.len() < MAX_PENDING
    }

    /// Mark `key` as being read and highlighted.
    pub fn start(&mut self, key: SyntaxKey) {
        self.pending.insert(key);
    }

    /// A highlighter for the current palette.
    #[must_use]
    pub const fn highlighter(&self) -> SyntaxHighlighter {
        SyntaxHighlighter::with_palette(self.palette)
    }

    /// Store a finished (or skipped) highlight and evict beyond the bounds.
    pub fn finish(&mut self, key: SyntaxKey, text: Option<Arc<HighlightedText>>) {
        self.pending.remove(&key);
        if key.palette != self.palette_hash {
            return;
        }
        let lines = text.as_ref().map_or(0, |text| text.line_count());
        let tick = self.tick.get() + 1;
        self.tick.set(tick);
        if let Some(previous) = self.entries.insert(
            key,
            Entry {
                text,
                lines,
                used: Cell::new(tick),
            },
        ) {
            self.total_lines -= previous.lines;
        }
        self.total_lines += lines;
        self.epoch += 1;
        while self.entries.len() > MAX_ENTRIES || self.total_lines > MAX_TOTAL_LINES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used.get())
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.total_lines -= entry.lines;
            }
        }
    }

    /// Cached entries (for bounds tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Highlight a whole text with `highlighter` (run on the blocking pool).
#[must_use]
pub fn highlight(highlighter: &SyntaxHighlighter, path: &str, text: &str) -> HighlightedText {
    highlighter.highlight_text(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_is_bounded_and_keys_follow_palette_and_generation() {
        let mut store = SyntaxStore::default();
        let object = ReviewBlobRef::Object { oid: "a".into() };
        let key = store.key(&object, "x.rs").unwrap();
        assert!(store.key(&ReviewBlobRef::Absent, "x.rs").is_none());
        let tree = store.key(&ReviewBlobRef::WorkingTree, "x.rs").unwrap();
        store.next_generation();
        assert_ne!(
            store.key(&ReviewBlobRef::WorkingTree, "x.rs").unwrap(),
            tree
        );
        assert_eq!(store.key(&object, "y.rs").unwrap(), key);

        let highlighter = store.highlighter();
        let text = Arc::new(highlight(&highlighter, "x.rs", "fn main() {}\n"));
        store.start(key.clone());
        assert!(store.known(&key));
        store.finish(key.clone(), Some(text));
        assert!(store.get(&key).is_some());
        for index in 0..(MAX_ENTRIES + 10) {
            let key = store
                .key(
                    &ReviewBlobRef::Object {
                        oid: index.to_string(),
                    },
                    "x.rs",
                )
                .unwrap();
            store.finish(key, None);
        }
        assert!(store.len() <= MAX_ENTRIES);

        let epoch = store.epoch();
        let mut palette = store.palette();
        palette.keyword = bcode_syntax_render::SyntaxColor::rgb(1, 2, 3);
        assert!(store.set_palette(palette));
        assert!(store.epoch() > epoch);
        assert_ne!(store.key(&object, "x.rs").unwrap(), key);
    }
}
