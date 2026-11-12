//! Syntax highlighting off the render path, bounded to what is on screen.
//!
//! Each side of a visible, resident text file is read whole and highlighted on the blocking
//! pool with BMUX's incremental whole-text highlighter, a bounded number of lines per step, and
//! cached by blob identity and palette. The lines highlighted after the first step are published
//! at once, so the top of a large file colors before the rest is done. Work for a blob whose file
//! leaves the screen is cancelled between steps (and a blob read that arrives after cancellation
//! is dropped), so fast scrolling never keeps a core busy with files already scrolled past.
//!
//! Rows look tokens up by line number, verified against the row's text, so a stale or mismatched
//! blob yields no tokens rather than wrong ones. Lines render unhighlighted until their blob's
//! tokens arrive; highlighting never changes geometry. Rows cache by a per-blob stamp
//! ([`SyntaxStore::stamp`]), so a highlight landing for one file leaves other files' rows cached.

use bcode_code_review_models::{ReviewBlobRef, ReviewBlobSelector, RevisionRef};
use bcode_syntax_render::{HighlightedText, SyntaxHighlighter, SyntaxPalette};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
/// Lines one highlighting step classifies before checking for cancellation.
pub const HIGHLIGHT_STEP_LINES: usize = 2_000;

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
    /// Whether `text` covers the whole blob (a partial entry shows the lines highlighted so
    /// far, and the blob is highlighted again when it is wanted after its job ended).
    complete: bool,
    lines: usize,
    used: Cell<u64>,
}

/// Cancellation flag of one blob's read-and-highlight job.
#[derive(Debug, Clone, Default)]
pub struct JobToken(Arc<AtomicBool>);

impl JobToken {
    /// Whether the job's result is no longer wanted.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Bounded highlight cache plus in-flight bookkeeping.
#[derive(Debug)]
pub struct SyntaxStore {
    palette: SyntaxPalette,
    palette_hash: u64,
    entries: BTreeMap<SyntaxKey, Entry>,
    pending: BTreeMap<SyntaxKey, JobToken>,
    total_lines: usize,
    tick: Cell<u64>,
    epoch: u64,
    generation: u64,
    /// Highlights landed, numbering [`Self::stamps`].
    landed: u64,
    /// Palette changes, mixed into every stamp.
    palette_epoch: u64,
    /// Landing number of each cached object blob's highlight, by object id.
    object_stamps: BTreeMap<String, u64>,
    /// Landing number of each cached working-tree highlight of this generation, by path.
    tree_stamps: BTreeMap<String, u64>,
}

impl Default for SyntaxStore {
    fn default() -> Self {
        let palette = SyntaxPalette::default();
        Self {
            palette,
            palette_hash: palette_hash(palette),
            entries: BTreeMap::new(),
            pending: BTreeMap::new(),
            total_lines: 0,
            tick: Cell::new(0),
            epoch: 0,
            generation: 0,
            landed: 0,
            palette_epoch: 0,
            object_stamps: BTreeMap::new(),
            tree_stamps: BTreeMap::new(),
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
        self.palette_epoch += 1;
        self.object_stamps.clear();
        self.tree_stamps.clear();
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
    pub fn next_generation(&mut self) {
        self.generation += 1;
        self.tree_stamps.clear();
    }

    /// A value that changes whenever the highlight available for one blob side of `path`
    /// changes (it lands, is evicted, or the palette changes), so rows built from it can be
    /// cached across highlights of other blobs. Looks up without allocating.
    #[must_use]
    pub fn stamp(&self, blob: &ReviewBlobRef, path: &str) -> u64 {
        let landed = match blob {
            ReviewBlobRef::Object { oid } => self.object_stamps.get(oid.as_str()),
            ReviewBlobRef::WorkingTree => self.tree_stamps.get(path),
            ReviewBlobRef::Absent => None,
        };
        landed.copied().unwrap_or(0) ^ self.palette_epoch.rotate_right(16)
    }

    /// The stamp map and name recording `blob`, or `None` for working-tree content of an
    /// earlier generation.
    const fn stamps_mut<'b>(
        object_stamps: &'b mut BTreeMap<String, u64>,
        tree_stamps: &'b mut BTreeMap<String, u64>,
        generation: u64,
        blob: &'b BlobId,
    ) -> Option<(&'b mut BTreeMap<String, u64>, &'b str)> {
        match blob {
            BlobId::Object(oid) => Some((object_stamps, oid.as_str())),
            BlobId::WorkingTree {
                path,
                generation: content,
            } if *content == generation => Some((tree_stamps, path.as_str())),
            BlobId::WorkingTree { .. } => None,
        }
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

    /// Whether `key` is fully highlighted (or skipped) or being prepared.
    #[must_use]
    pub fn known(&self, key: &SyntaxKey) -> bool {
        self.entries.get(key).is_some_and(|entry| entry.complete) || self.pending.contains_key(key)
    }

    /// Whether another blob may start being read now.
    #[must_use]
    pub fn has_capacity(&self) -> bool {
        self.pending.len() < MAX_PENDING
    }

    /// Mark `key` as being read and highlighted; the job stops when the token is cancelled.
    pub fn start(&mut self, key: SyntaxKey) -> JobToken {
        let token = JobToken::default();
        self.pending.insert(key, token.clone());
        token
    }

    /// Whether `token` is still the live job for `key`.
    #[must_use]
    pub fn is_current(&self, key: &SyntaxKey, token: &JobToken) -> bool {
        self.pending
            .get(key)
            .is_some_and(|current| current.same(token) && !token.is_cancelled())
    }

    /// Cancel every job whose blob is not in `wanted` (its file left the screen). Returns how
    /// many were cancelled; each frees capacity for wanted blobs at once.
    pub fn cancel_unwanted(&mut self, wanted: &BTreeSet<SyntaxKey>) -> usize {
        let before = self.pending.len();
        self.pending.retain(|key, token| {
            let keep = wanted.contains(key);
            if !keep {
                token.cancel();
            }
            keep
        });
        before - self.pending.len()
    }

    /// Jobs in flight.
    #[must_use]
    pub fn pending_jobs(&self) -> usize {
        self.pending.len()
    }

    /// A highlighter for the current palette.
    #[must_use]
    pub const fn highlighter(&self) -> SyntaxHighlighter {
        SyntaxHighlighter::with_palette(self.palette)
    }

    /// Store a finished (or skipped) highlight and evict beyond the bounds.
    pub fn finish(&mut self, key: SyntaxKey, text: Option<Arc<HighlightedText>>) {
        self.pending.remove(&key);
        self.land(key, text, true, None);
    }

    /// Store the lines of `key` highlighted so far by the job of `token` while it continues, or
    /// its finished highlight when `complete` (which ends that job, but not a newer job started
    /// for the same blob after it was cancelled). A partial result from a cancelled job is kept
    /// for display; the blob is highlighted again when it is next wanted.
    pub fn land(
        &mut self,
        key: SyntaxKey,
        text: Option<Arc<HighlightedText>>,
        complete: bool,
        token: Option<&JobToken>,
    ) {
        if complete
            && token.is_some_and(|token| {
                self.pending
                    .get(&key)
                    .is_some_and(|current| current.same(token))
            })
        {
            self.pending.remove(&key);
        }
        if key.palette != self.palette_hash {
            return;
        }
        let lines = text.as_ref().map_or(0, |text| text.line_count());
        let key_blob = key.blob.clone();
        let tick = self.tick.get() + 1;
        self.tick.set(tick);
        if let Some(previous) = self.entries.insert(
            key,
            Entry {
                text,
                complete,
                lines,
                used: Cell::new(tick),
            },
        ) {
            self.total_lines -= previous.lines;
        }
        self.total_lines += lines;
        self.epoch += 1;
        self.landed += 1;
        let landed = self.landed;
        if let Some((stamps, name)) = Self::stamps_mut(
            &mut self.object_stamps,
            &mut self.tree_stamps,
            self.generation,
            &key_blob,
        ) {
            stamps.insert(name.to_owned(), landed);
        }
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
            if let Some((stamps, name)) = Self::stamps_mut(
                &mut self.object_stamps,
                &mut self.tree_stamps,
                self.generation,
                &oldest.blob,
            ) {
                stamps.remove(name);
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

/// Highlight `text` with `highlighter` on the blocking pool, [`HIGHLIGHT_STEP_LINES`] lines per
/// step.
///
/// `publish` receives the lines highlighted after the first step when more remain (marked
/// incomplete), then the whole result (marked complete). Stops without publishing the rest when
/// `token` is cancelled between steps; returns whether the highlight completed.
pub fn highlight_steps(
    highlighter: &SyntaxHighlighter,
    path: &str,
    text: &str,
    token: &JobToken,
    mut publish: impl FnMut(Arc<HighlightedText>, bool),
) -> bool {
    let mut progress = highlighter.start_text(path);
    let mut first = true;
    loop {
        if token.is_cancelled() {
            return false;
        }
        if progress.advance(text, HIGHLIGHT_STEP_LINES) {
            publish(Arc::new(progress.into_highlighted()), true);
            return true;
        }
        if first {
            first = false;
            publish(Arc::new(progress.highlighted().clone()), false);
        }
    }
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
        let text = Arc::new(highlighter.highlight_text("x.rs", "fn main() {}\n"));
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

    fn object(store: &SyntaxStore, oid: &str) -> SyntaxKey {
        store
            .key(&ReviewBlobRef::Object { oid: oid.into() }, "x.rs")
            .unwrap()
    }

    #[test]
    fn jobs_for_blobs_off_screen_are_cancelled_and_free_capacity() {
        let mut store = SyntaxStore::default();
        let (a, b, c) = (
            object(&store, "a"),
            object(&store, "b"),
            object(&store, "c"),
        );
        let token_a = store.start(a.clone());
        let token_b = store.start(b.clone());
        assert!(!store.has_capacity());
        let wanted = BTreeSet::from([b.clone(), c]);
        assert_eq!(store.cancel_unwanted(&wanted), 1);
        assert!(token_a.is_cancelled() && !token_b.is_cancelled());
        assert!(!store.is_current(&a, &token_a) && store.is_current(&b, &token_b));
        assert!(
            !store.known(&a),
            "a cancelled blob is highlighted again when wanted"
        );
        assert!(store.has_capacity());

        // A partial result of the cancelled job is shown but does not count as done; a late
        // complete result of it does not end a newer job for the same blob.
        let text = Arc::new(store.highlighter().highlight_text("x.rs", "fn a() {}\n"));
        store.land(a.clone(), Some(Arc::clone(&text)), false, Some(&token_a));
        assert!(store.get(&a).is_some() && !store.known(&a));
        let token_a2 = store.start(a.clone());
        store.land(a.clone(), Some(text), true, Some(&token_a));
        assert!(store.is_current(&a, &token_a2));
    }

    #[test]
    fn stamps_change_only_for_the_blob_whose_highlight_landed() {
        let mut store = SyntaxStore::default();
        let blob_a = ReviewBlobRef::Object { oid: "a".into() };
        let blob_b = ReviewBlobRef::Object { oid: "b".into() };
        let (a_before, b_before) = (store.stamp(&blob_a, "x.rs"), store.stamp(&blob_b, "x.rs"));
        store.finish(object(&store, "a"), None);
        assert_ne!(store.stamp(&blob_a, "x.rs"), a_before);
        assert_eq!(store.stamp(&blob_b, "x.rs"), b_before);

        let tree = ReviewBlobRef::WorkingTree;
        let key = store.key(&tree, "y.rs").unwrap();
        let tree_before = store.stamp(&tree, "y.rs");
        store.finish(key, None);
        let landed = store.stamp(&tree, "y.rs");
        assert_ne!(landed, tree_before);
        store.next_generation();
        assert_ne!(
            store.stamp(&tree, "y.rs"),
            landed,
            "refreshed content has no highlight yet"
        );

        let a_landed = store.stamp(&blob_a, "x.rs");
        let mut palette = store.palette();
        palette.keyword = bcode_syntax_render::SyntaxColor::rgb(9, 9, 9);
        store.set_palette(palette);
        assert_ne!(store.stamp(&blob_a, "x.rs"), a_landed);
    }

    #[test]
    fn stepped_highlighting_publishes_early_lines_then_the_whole_text_and_stops_when_cancelled() {
        let highlighter = SyntaxHighlighter::new();
        let text = "let x = 1;\n".repeat(HIGHLIGHT_STEP_LINES * 2 + 1);
        let token = JobToken::default();
        let mut published = Vec::new();
        assert!(highlight_steps(
            &highlighter,
            "x.rs",
            &text,
            &token,
            |text, complete| {
                published.push((text.line_count(), complete));
            }
        ));
        assert_eq!(
            published,
            [
                (HIGHLIGHT_STEP_LINES, false),
                (HIGHLIGHT_STEP_LINES * 2 + 1, true)
            ]
        );

        let token = JobToken::default();
        let mut published = 0;
        let completed = highlight_steps(&highlighter, "x.rs", &text, &token, |_, _| {
            published += 1;
            token.cancel();
        });
        assert!(!completed);
        assert_eq!(published, 1, "nothing more is published once cancelled");
    }
}
