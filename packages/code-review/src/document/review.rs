//! The review document: one keyed, virtualizable item sequence over every file of every source.

use super::anchors::{self, AnchorTarget, Annotation, Annotations, DocAnchor};
use super::fenwick::Fenwick;
use super::gaps::{self, HunkRange, Region, RegionInput};
use super::interval::IntervalSet;
use super::items::{FileLoad, LayoutMode, ReviewItem};
use super::keys::{FileKey, FileRef, ItemKey};
use crate::viewed::{EntryIdentity, ViewedReset, ViewedState, best_state};
use bcode_code_review_models::{
    REVIEW_BLOB_LINES_MAX_COUNT, REVIEW_HUNKS_DEFAULT_CONTEXT, ResolvedReviewStack, ResolvedSource,
    ResolvedSourceMode, ReviewBlobLinesRequest, ReviewBlobLinesResponse, ReviewBlobRef,
    ReviewBlobSelector, ReviewFileExpansion, ReviewFileHunksRequest, ReviewFileHunksResponse,
    ReviewHunksTruncation, ReviewIndexEntry, ReviewLineKind, ReviewLineSide,
    ReviewStackAttributionRequest, ReviewStackAttributionResponse, ReviewStackLineAttribution,
    ReviewWorkspace, ViewedMark,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

/// Default hunk residency budget: 64 MiB.
pub const DEFAULT_RESIDENCY_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// Document configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentConfig {
    /// Resident content budget in bytes. Least recently used content outside the prefetch
    /// window is evicted beyond it.
    pub residency_budget_bytes: usize,
    /// Estimated rows above and below the viewport whose files are loaded ahead of time while
    /// the budget allows, and never evicted while they stay in the window.
    pub prefetch_rows: u64,
    /// Estimated rows above and below the viewport whose files are loaded even over the
    /// budget (the protected anchor window). Clamped to at most `prefetch_rows`.
    pub protected_rows: u64,
    /// Context lines the hunks are requested with (used to infer a file's end).
    pub context_lines: u32,
    /// Maximum hunk requests outstanding at once.
    pub max_inflight_hunks: usize,
    /// Maximum blob-line and attribution requests outstanding at once.
    pub max_inflight_blobs: usize,
    /// Lines per blob-line request (at most the service maximum).
    pub blob_chunk_lines: u32,
    /// What makes a viewed file stale.
    pub viewed_reset: ViewedReset,
}

impl Default for DocumentConfig {
    fn default() -> Self {
        Self {
            residency_budget_bytes: DEFAULT_RESIDENCY_BUDGET_BYTES,
            prefetch_rows: 400,
            protected_rows: 120,
            context_lines: REVIEW_HUNKS_DEFAULT_CONTEXT,
            max_inflight_hunks: 8,
            max_inflight_blobs: 16,
            blob_chunk_lines: REVIEW_BLOB_LINES_MAX_COUNT,
            viewed_reset: ViewedReset::Diff,
        }
    }
}

/// Stack structure of one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceStack {
    /// The resolved stack.
    pub stack: ResolvedReviewStack,
    /// Whether the source reviews the whole stack (files and hunks carry layer attribution)
    /// rather than its top layer only.
    pub whole: bool,
}

/// One resolved source (or source part) and its file index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInput {
    /// The resolved source.
    pub source: ResolvedSource,
    /// Display label of the source; the document appends the part label for multi-part
    /// sources.
    pub label: String,
    /// Every index entry of the source, in index order.
    pub entries: Vec<ReviewIndexEntry>,
    /// Stack structure, for stack and stack-layer sources.
    pub stack: Option<SourceStack>,
    /// The source's files are still being listed: the group shows its header (saying so) and no
    /// files until the source is opened again with them.
    pub loading: bool,
}

/// Identifies one outstanding request.
pub type Ticket = u64;

/// One source group of the document (a source, or one part of a multi-part source).
#[derive(Debug, Clone)]
pub struct GroupInfo<'a> {
    /// The resolved source.
    pub source: &'a ResolvedSource,
    /// Display label.
    pub label: &'a str,
    /// Stack structure, for stack and stack-layer sources.
    pub stack: Option<&'a SourceStack>,
    /// Position of the group (for [`ReviewDocument::group_files`]).
    pub index: usize,
    /// Number of files in the group.
    pub file_count: usize,
    /// Files of the group shown as viewed.
    pub viewed: usize,
    /// The group's source header item, when headers are shown.
    pub header: Option<ItemKey>,
    /// Whether [`ReviewDocument::set_focus`] hides the group.
    pub hidden: bool,
    /// Whether the group's files are still being listed.
    pub loading: bool,
}

/// A request the client should issue on the document's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentNeed {
    /// Load a file's hunks (`review.file.hunks.get`).
    Hunks {
        /// Ticket to pass back with the result.
        ticket: Ticket,
        /// File.
        file: FileRef,
        /// Request.
        request: ReviewFileHunksRequest,
    },
    /// Read context lines or the line count of a file's context-side blob
    /// (`review.blob.lines.get`).
    BlobLines {
        /// Ticket to pass back with the result.
        ticket: Ticket,
        /// File.
        file: FileRef,
        /// Request.
        request: ReviewBlobLinesRequest,
    },
    /// Load whole-stack layer attribution (`review.stack.attribution`).
    Attribution {
        /// Ticket to pass back with the result.
        ticket: Ticket,
        /// File.
        file: FileRef,
        /// Request.
        request: ReviewStackAttributionRequest,
    },
}

impl DocumentNeed {
    /// Ticket of this need.
    #[must_use]
    pub const fn ticket(&self) -> Ticket {
        match self {
            Self::Hunks { ticket, .. }
            | Self::BlobLines { ticket, .. }
            | Self::Attribution { ticket, .. } => *ticket,
        }
    }

    /// File of this need.
    #[must_use]
    pub const fn file(&self) -> &FileRef {
        match self {
            Self::Hunks { file, .. }
            | Self::BlobLines { file, .. }
            | Self::Attribution { file, .. } => file,
        }
    }
}

/// One change to the item sequence: `removed` items starting at `start` were replaced by
/// `inserted` items. Splices apply in order; each is relative to the sequence after the
/// previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Splice {
    /// First changed index.
    pub start: usize,
    /// Items removed.
    pub removed: usize,
    /// Items inserted.
    pub inserted: usize,
    /// Document revision after this splice.
    pub revision: u64,
}

/// Sticky-header ancestry of one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sections {
    /// Index of the file header whose section contains the item.
    pub file_header: Option<usize>,
    /// Index of the hunk header whose section contains the item.
    pub hunk_header: Option<usize>,
}

/// Viewed progress counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewedProgress {
    /// Files shown as viewed.
    pub viewed: usize,
    /// All files.
    pub total: usize,
}

/// One line of a loaded hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DocLine {
    pub kind: ReviewLineKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: Arc<str>,
}

/// One loaded hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DocHunk {
    pub range: HunkRange,
    pub heading: Option<Arc<str>>,
    pub lines: Vec<DocLine>,
}

/// Resident content of one file.
#[derive(Debug, Clone, Default)]
pub(super) struct Loaded {
    pub hunks: Vec<DocHunk>,
    pub ranges: Vec<HunkRange>,
    pub binary: bool,
    pub truncated: Option<ReviewHunksTruncation>,
    /// Context-side lines read from the blob.
    pub context: BTreeMap<u32, Arc<str>>,
    /// Lines cached, requested, or unavailable (never requested again).
    pub covered: IntervalSet,
    /// Lines that could not be read, with the reason.
    pub unavailable: Vec<(u32, u32, Arc<str>)>,
    /// Bytes of hunk and context text plus per-line overhead.
    pub content_bytes: usize,
}

#[derive(Debug, Clone)]
pub(super) enum Content {
    Unloaded,
    Loading(Ticket),
    Loaded(Box<Loaded>),
    Failed(Arc<str>),
}

/// Whole-stack attribution of one file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Attribution {
    pub new_ranges: Vec<ReviewStackLineAttribution>,
    pub old_ranges: Vec<ReviewStackLineAttribution>,
}

impl Attribution {
    pub fn layer(&self, side: ReviewLineSide, line: u32) -> Option<u64> {
        let ranges = match side {
            ReviewLineSide::Old => &self.old_ranges,
            ReviewLineSide::New => &self.new_ranges,
        };
        let index = ranges.partition_point(|range| range.end < line);
        ranges
            .get(index)
            .filter(|range| range.start <= line)
            .map(|range| range.layer)
    }

    pub fn layers(&self) -> Vec<u64> {
        let mut layers: Vec<u64> = self
            .new_ranges
            .iter()
            .chain(&self.old_ranges)
            .map(|range| range.layer)
            .collect();
        layers.sort_unstable();
        layers.dedup();
        layers
    }
}

#[derive(Debug, Clone)]
pub(super) enum AttributionState {
    NotNeeded,
    Missing,
    Pending(Ticket),
    Loaded(Attribution),
    Failed,
}

#[derive(Debug, Clone)]
pub(super) struct FileState {
    pub key: FileRef,
    pub entry: ReviewIndexEntry,
    pub group: usize,
    pub block: usize,
    pub content: Content,
    pub revealed: IntervalSet,
    pub line_count: Option<u32>,
    pub patch_id: Option<String>,
    /// Object id of working-tree content on the new side, once identified.
    pub content_oid: Option<String>,
    pub viewed: ViewedState,
    pub attribution: AttributionState,
    pub last_used: u64,
    pub resident_bytes: usize,
    /// Patch unchanged since the last review pass.
    pub carried_over: bool,
}

impl FileState {
    /// Side context lines are counted on: the new side, or the old side when the new side does
    /// not exist (deleted files).
    pub fn context_side(&self) -> ReviewLineSide {
        if self.entry.new_blob == ReviewBlobRef::Absent {
            ReviewLineSide::Old
        } else {
            ReviewLineSide::New
        }
    }

    /// Whether the other side exists.
    pub fn other_side_exists(&self) -> bool {
        match self.context_side() {
            ReviewLineSide::New => self.entry.old_blob != ReviewBlobRef::Absent,
            ReviewLineSide::Old => false,
        }
    }

    pub fn loaded(&self) -> Option<&Loaded> {
        match &self.content {
            Content::Loaded(loaded) => Some(loaded),
            _ => None,
        }
    }

    pub fn load_state(&self) -> FileLoad {
        match &self.content {
            Content::Unloaded => FileLoad::Unloaded,
            Content::Loading(_) => FileLoad::Loading,
            Content::Loaded(_) => FileLoad::Loaded,
            Content::Failed(reason) => FileLoad::Failed(reason.clone()),
        }
    }

    /// Whether the file has line content that can be expanded: not binary and not a
    /// submodule (whose "content" is a commit id).
    pub fn expandable(&self) -> bool {
        use bcode_code_review_models::ReviewIndexFlag;
        !self.entry.has(ReviewIndexFlag::Binary)
            && !self.entry.has(ReviewIndexFlag::Submodule)
            && !self.loaded().is_some_and(|loaded| loaded.binary)
    }

    /// Unchanged regions of the loaded content.
    pub fn regions(&self) -> Vec<Region> {
        let Some(loaded) = self.loaded() else {
            return Vec::new();
        };
        if !self.expandable() {
            return Vec::new();
        }
        gaps::regions(RegionInput {
            hunks: &loaded.ranges,
            side: self.context_side(),
            other_exists: self.other_side_exists(),
            line_count: self.line_count,
            truncated: loaded.truncated.is_some(),
        })
    }

    /// Normalize the revealed set against the loaded hunks and the line count: hunk coverage
    /// and lines past the end are removed.
    pub fn clamp_revealed(&mut self) {
        if !self.expandable() {
            self.revealed = IntervalSet::new();
            return;
        }
        let side = self.context_side();
        if let Content::Loaded(loaded) = &self.content {
            for range in &loaded.ranges {
                let span = gaps::hunk_span(range, side);
                if span.next > span.first {
                    self.revealed.remove(span.first, span.next - 1);
                }
            }
        }
        if let Some(count) = self.line_count {
            if count == 0 {
                self.revealed = IntervalSet::new();
            } else {
                self.revealed.retain_within(1, count);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct Group {
    pub source: ResolvedSource,
    pub label: String,
    pub stack: Option<SourceStack>,
    pub header_block: usize,
    pub files: std::ops::Range<usize>,
    pub show_header: bool,
    /// Whether the group is hidden by [`ReviewDocument::set_focus`]: its blocks build no items.
    pub hidden: bool,
    /// Whether the group's files are still being listed (see [`SourceInput::loading`]).
    pub loading: bool,
    /// Files of the group shown as viewed.
    pub viewed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockKind {
    ReviewNotes,
    SourceHeader(usize),
    File(usize),
}

#[derive(Debug, Clone)]
pub(super) struct Block {
    pub kind: BlockKind,
    pub items: Vec<ReviewItem>,
    /// Block-local index of the hunk header whose section contains each item.
    pub hunk_parent: Vec<Option<u32>>,
    /// Key lookup for this block's items.
    pub lookup: BlockLookup,
}

impl Block {
    const fn new(kind: BlockKind) -> Self {
        Self {
            kind,
            items: Vec::new(),
            hunk_parent: Vec::new(),
            lookup: BlockLookup::empty(),
        }
    }
}

/// `O(log n)` key → block-local index lookup for one block.
///
/// Line and row items are found by side and line number (both sides of every line item are
/// indexed, so a unified line key finds the side-by-side row containing it and a row key finds
/// its unified line); every other item by its exact key.
#[derive(Debug, Clone, Default)]
pub(super) struct BlockLookup {
    /// `(old line, local index)` sorted by line.
    old_lines: Vec<(u32, u32)>,
    /// `(new line, local index)` sorted by line.
    new_lines: Vec<(u32, u32)>,
    /// Exact keys of non-line items.
    keys: BTreeMap<ItemKey, u32>,
}

impl BlockLookup {
    const fn empty() -> Self {
        Self {
            old_lines: Vec::new(),
            new_lines: Vec::new(),
            keys: BTreeMap::new(),
        }
    }

    pub(super) fn build(items: &[ReviewItem]) -> Self {
        let mut lookup = Self::empty();
        for (local, item) in items.iter().enumerate() {
            let local = u32::try_from(local).unwrap_or(u32::MAX);
            match item.line_numbers() {
                Some((old, new)) => {
                    if let Some(old) = old {
                        lookup.old_lines.push((old, local));
                    }
                    if let Some(new) = new {
                        lookup.new_lines.push((new, local));
                    }
                }
                None => {
                    lookup.keys.insert(item.key(), local);
                }
            }
        }
        // Line numbers increase in item order on each side; sorting keeps lookups correct
        // even if a future item kind breaks that.
        lookup.old_lines.sort_unstable();
        lookup.new_lines.sort_unstable();
        lookup
    }

    fn line(&self, side: ReviewLineSide, line: u32) -> Option<usize> {
        let lines = match side {
            ReviewLineSide::Old => &self.old_lines,
            ReviewLineSide::New => &self.new_lines,
        };
        let at = lines.partition_point(|(candidate, _)| *candidate < line);
        lines
            .get(at)
            .filter(|(candidate, _)| *candidate == line)
            .map(|(_, local)| *local as usize)
    }

    /// Block-local index of `key`, or of the item that contains its line in the current
    /// layout.
    fn find(&self, key: &ItemKey) -> Option<usize> {
        match key {
            ItemKey::Line { side, line, .. } => self.line(*side, *line),
            ItemKey::LinePair { old, new, .. } => new
                .and_then(|line| self.line(ReviewLineSide::New, line))
                .or_else(|| old.and_then(|line| self.line(ReviewLineSide::Old, line))),
            _ => self.keys.get(key).map(|local| *local as usize),
        }
    }

    /// Approximate heap bytes of the lookup.
    fn bytes(&self) -> usize {
        (self.old_lines.len() + self.new_lines.len()) * std::mem::size_of::<(u32, u32)>()
            + self.keys.len() * (std::mem::size_of::<ItemKey>() + 16)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pending {
    Hunks(usize),
    Blob { file: usize, start: u32, count: u32 },
    Attribution(usize),
}

/// Path persisted for review-level anchors (the spelling stored reviews use).
pub const REVIEW_LEVEL_PATH: &str = "<review>";

/// The renderer-neutral review document.
#[derive(Debug, Clone)]
pub struct ReviewDocument {
    pub(super) repo_path: PathBuf,
    pub(super) config: DocumentConfig,
    pub(super) layout: LayoutMode,
    pub(super) ignore_whitespace: bool,
    pub(super) groups: Vec<Group>,
    pub(super) files: Vec<FileState>,
    pub(super) file_index: BTreeMap<FileKey, usize>,
    /// Files by display path, in document order (anchor matching).
    pub(super) paths: BTreeMap<String, Vec<usize>>,
    pub(super) blocks: Vec<Block>,
    pub(super) lengths: Fenwick,
    pub(super) rows: Fenwick,
    pub(super) annotations: Vec<Annotation>,
    pub(super) review_annotations: Vec<usize>,
    pub(super) file_annotations: Vec<Vec<usize>>,
    /// Block each thread and agent-thread key is placed in.
    pub(super) annotation_blocks: BTreeMap<ItemKey, usize>,
    pub(super) composer: Option<DocAnchor>,
    /// The annotations last given to [`Self::set_annotations`], to place again after a group
    /// changes (see [`Self::upsert_group`]).
    pub(super) annotations_input: Option<Annotations>,
    pub(super) marks: BTreeMap<String, Vec<ViewedMark>>,
    pub(super) pending: BTreeMap<Ticket, Pending>,
    pub(super) next_ticket: Ticket,
    pub(super) tick: u64,
    pub(super) window: Option<Window>,
    pub(super) resident_bytes: usize,
    /// Resident files by `(last_used, file)`, least recently used first.
    pub(super) lru: BTreeSet<(u64, usize)>,
    /// Files shown as viewed.
    pub(super) viewed_count: usize,
    pub(super) splices: Vec<Splice>,
    pub(super) revision: u64,
}

/// Viewport, protected, and prefetch block ranges (inclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Window {
    pub viewport: (usize, usize),
    pub protected: (usize, usize),
    pub prefetch: (usize, usize),
}

impl ReviewDocument {
    /// Build a document over the given sources. Every file is resident as an index entry; no
    /// content is loaded until requested through [`Self::take_needs`].
    #[must_use]
    pub fn new(repo_path: PathBuf, sources: Vec<SourceInput>, config: DocumentConfig) -> Self {
        let mut doc = Self::structure(repo_path, sources, config);
        doc.build_all();
        doc
    }

    /// The groups, files, and blocks of `sources`, with no items built yet.
    pub(super) fn structure(
        repo_path: PathBuf,
        sources: Vec<SourceInput>,
        config: DocumentConfig,
    ) -> Self {
        let show_headers = sources.len() > 1 || sources.iter().any(|input| input.stack.is_some());
        let mut doc = Self {
            repo_path,
            config,
            layout: LayoutMode::Unified,
            ignore_whitespace: false,
            groups: Vec::new(),
            files: Vec::new(),
            file_index: BTreeMap::new(),
            paths: BTreeMap::new(),
            blocks: vec![Block::new(BlockKind::ReviewNotes)],
            lengths: Fenwick::default(),
            rows: Fenwick::default(),
            annotations: Vec::new(),
            review_annotations: Vec::new(),
            file_annotations: Vec::new(),
            annotation_blocks: BTreeMap::new(),
            composer: None,
            annotations_input: None,
            marks: BTreeMap::new(),
            pending: BTreeMap::new(),
            next_ticket: 1,
            tick: 0,
            window: None,
            resident_bytes: 0,
            lru: BTreeSet::new(),
            viewed_count: 0,
            splices: Vec::new(),
            revision: 0,
        };
        for input in sources {
            doc.add_source(input, show_headers);
        }
        doc
    }

    /// Append one source group and its file blocks (items are built by the caller).
    fn add_source(&mut self, input: SourceInput, show_headers: bool) {
        let group = self.groups.len();
        let header_block = self.blocks.len();
        self.blocks.push(Block::new(BlockKind::SourceHeader(group)));
        let first_file = self.files.len();
        let part = input.source.part.as_ref().map(|part| part.index);
        let browse = input.source.mode == ResolvedSourceMode::Browse;
        let whole_stack = input.stack.as_ref().is_some_and(|stack| stack.whole);
        for entry in input.entries {
            let key = FileKey::new(input.source.source_id.clone(), part, entry.path.clone());
            if self.file_index.contains_key(&key) {
                continue;
            }
            let index = self.files.len();
            self.file_index.insert(key.clone(), index);
            self.paths
                .entry(entry.path.clone())
                .or_default()
                .push(index);
            let mut revealed = IntervalSet::new();
            if browse {
                let (start, end) = input
                    .source
                    .line_range
                    .map_or((1, u32::MAX), |range| (range.start.max(1), range.end));
                revealed.insert(start, end);
            }
            let attribution =
                if whole_stack && !entry.has(bcode_code_review_models::ReviewIndexFlag::Binary) {
                    AttributionState::Missing
                } else {
                    AttributionState::NotNeeded
                };
            self.files.push(FileState {
                key: Arc::new(key),
                entry,
                group,
                block: self.blocks.len(),
                content: Content::Unloaded,
                revealed,
                line_count: None,
                patch_id: None,
                content_oid: None,
                viewed: ViewedState::Unviewed,
                attribution,
                last_used: 0,
                resident_bytes: 0,
                carried_over: false,
            });
            self.blocks.push(Block::new(BlockKind::File(index)));
            self.file_annotations.push(Vec::new());
        }
        self.groups.push(Group {
            source: input.source,
            label: input.label,
            stack: input.stack,
            header_block,
            files: first_file..self.files.len(),
            show_header: show_headers || input.loading,
            hidden: false,
            loading: input.loading,
            viewed: 0,
        });
    }

    // ----------------------------------------------------------------------------------------
    // Sequence API
    // ----------------------------------------------------------------------------------------

    /// Number of items.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::try_from(self.lengths.total()).unwrap_or(usize::MAX)
    }

    /// Whether the document has no items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of anchored annotations (threads, AI exchanges, and suggestions) shown in the
    /// document.
    #[must_use]
    pub const fn annotation_count(&self) -> usize {
        self.annotations.len()
    }

    /// Current revision; increases with every splice.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Drain the splices emitted since the last call, in order.
    pub fn take_splices(&mut self) -> Vec<Splice> {
        std::mem::take(&mut self.splices)
    }

    /// Item at `index`.
    #[must_use]
    pub fn item(&self, index: usize) -> Option<&ReviewItem> {
        let (block, local) = self.locate_index(index)?;
        self.blocks[block].items.get(local)
    }

    /// Estimated rows of the item at `index` before it is measured: a placeholder's estimate,
    /// otherwise one row.
    #[must_use]
    pub fn estimated_rows(&self, index: usize) -> u32 {
        match self.item(index) {
            Some(ReviewItem::Placeholder(item)) => item.estimated_rows,
            Some(_) => 1,
            None => 0,
        }
    }

    /// Index of the item with `key` in the current layout. A line key resolves to the item that
    /// shows that side's line: a unified line key to the side-by-side row containing it, and a
    /// row key to the unified line of its new side (its old side when it has none), so anchors
    /// survive layout switches. Costs `O(log files + log items)`.
    #[must_use]
    pub fn index_of(&self, key: &ItemKey) -> Option<usize> {
        let block = self.block_of_key(key)?;
        let local = self.blocks[block].lookup.find(key)?;
        Some(self.block_start(block) + local)
    }

    /// Map `key` to the equivalent key in the current layout: a line key to the key of the line
    /// or row that shows it. Other keys map to themselves. Returns `None` when a line key's line
    /// is not resident.
    #[must_use]
    pub fn map_key(&self, key: &ItemKey) -> Option<ItemKey> {
        match key {
            ItemKey::Line { .. } | ItemKey::LinePair { .. } => {
                let index = self.index_of(key)?;
                self.item(index).map(ReviewItem::key)
            }
            _ => Some(key.clone()),
        }
    }

    /// Sticky-header ancestry of the item at `index`: its file header (level 0) and hunk header
    /// (level 1). Headers are their own section's header.
    #[must_use]
    pub fn sections(&self, index: usize) -> Sections {
        let Some((block, local)) = self.locate_index(index) else {
            return Sections::default();
        };
        if !matches!(self.blocks[block].kind, BlockKind::File(_)) {
            return Sections::default();
        }
        let start = self.block_start(block);
        Sections {
            file_header: Some(start),
            hunk_header: self.blocks[block].hunk_parent[local].map(|hunk| start + hunk as usize),
        }
    }

    /// Current layout mode.
    #[must_use]
    pub const fn layout(&self) -> LayoutMode {
        self.layout
    }

    /// Switch layout. Every file is rebuilt (placeholder estimates depend on the layout); keys
    /// of threads, headers, and gaps are unchanged and line keys map through
    /// [`Self::index_of`].
    pub fn set_layout(&mut self, layout: LayoutMode) {
        if self.layout == layout {
            return;
        }
        self.layout = layout;
        for file in 0..self.files.len() {
            self.rebuild_file(file);
        }
    }

    /// Whether hunks are requested ignoring whitespace.
    #[must_use]
    pub const fn ignore_whitespace(&self) -> bool {
        self.ignore_whitespace
    }

    /// Toggle whitespace-ignoring hunks: every file's content is dropped and
    /// re-requested; expansion state is kept.
    pub fn set_ignore_whitespace(&mut self, ignore: bool) {
        if self.ignore_whitespace == ignore {
            return;
        }
        self.ignore_whitespace = ignore;
        self.pending.clear();
        for file in 0..self.files.len() {
            if !matches!(self.files[file].content, Content::Unloaded) {
                self.unload(file);
            }
        }
    }

    /// Key of every file in document order.
    pub fn file_keys(&self) -> impl Iterator<Item = &FileRef> {
        self.files.iter().map(|file| &file.key)
    }

    /// Index entry of a file.
    #[must_use]
    pub fn entry(&self, file: &FileKey) -> Option<&ReviewIndexEntry> {
        self.file_index
            .get(file)
            .map(|index| &self.files[*index].entry)
    }

    /// Load state of a file.
    #[must_use]
    pub fn load_state(&self, file: &FileKey) -> Option<FileLoad> {
        self.file_index
            .get(file)
            .map(|index| self.files[*index].load_state())
    }

    /// Bytes of resident content and items.
    #[must_use]
    pub const fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    /// Files whose content is resident.
    pub fn resident_files(&self) -> impl Iterator<Item = &FileRef> {
        self.files
            .iter()
            .filter(|file| file.loaded().is_some())
            .map(|file| &file.key)
    }

    // ----------------------------------------------------------------------------------------
    // Residency
    // ----------------------------------------------------------------------------------------

    /// Report the visible item range (inclusive). Files in and around it are marked recently
    /// used, become the protected anchor window, and drive [`Self::take_needs`]. Content beyond
    /// the budget is evicted, least recently used first, outside the protected window.
    pub fn set_viewport(&mut self, first: usize, last: usize) {
        let len = self.len();
        if len == 0 {
            self.window = None;
            return;
        }
        let first = first.min(len - 1);
        let last = last.clamp(first, len - 1);
        let (first_block, _) = self.locate_index(first).unwrap_or((0, 0));
        let (last_block, _) = self.locate_index(last).unwrap_or((first_block, 0));
        let protected = self.expand_blocks(
            first_block,
            last_block,
            self.config.protected_rows.min(self.config.prefetch_rows),
        );
        let prefetch = self.expand_blocks(first_block, last_block, self.config.prefetch_rows);
        self.window = Some(Window {
            viewport: (first_block, last_block),
            protected,
            prefetch,
        });
        self.tick += 1;
        for block in protected.0..=protected.1 {
            if let BlockKind::File(file) = self.blocks[block].kind {
                self.touch(file);
            }
        }
        self.enforce_budget();
    }

    /// Mark `file` used at the current tick.
    fn touch(&mut self, file: usize) {
        let state = &mut self.files[file];
        if state.last_used == self.tick {
            return;
        }
        if self.lru.remove(&(state.last_used, file)) {
            self.lru.insert((self.tick, file));
        }
        state.last_used = self.tick;
    }

    fn expand_blocks(&self, first: usize, last: usize, rows: u64) -> (usize, usize) {
        let above_target = self.rows.prefix(first).saturating_sub(rows);
        let below_target = self.rows.prefix(last + 1).saturating_add(rows);
        let start = self.rows.find(above_target).unwrap_or(0).min(first);
        // `below_target` is one past the last row of the window.
        let end = if below_target >= self.rows.total() {
            self.blocks.len() - 1
        } else {
            self.rows
                .find(below_target.saturating_sub(1))
                .unwrap_or(last)
                .max(last)
        };
        (start, end)
    }

    /// Whether `file` lies in the window range chosen by `range`.
    fn in_window(&self, file: usize, range: fn(&Window) -> (usize, usize)) -> bool {
        self.window.as_ref().is_some_and(|window| {
            let (first, last) = range(window);
            let block = self.files[file].block;
            block >= first && block <= last
        })
    }

    /// Evict least recently used content outside the prefetch window until the budget holds.
    /// Content inside the window (which contains the protected window around the viewport) is
    /// never evicted; prefetching beyond the protected window stops once the budget is reached
    /// (see [`Self::take_needs`]), so the window cannot thrash.
    ///
    /// Costs `O((window + evicted) · log files)`: resident files are kept ordered by last use, so
    /// only files inside the window are skipped.
    pub(super) fn enforce_budget(&mut self) {
        let budget = self.config.residency_budget_bytes;
        if self.resident_bytes <= budget {
            return;
        }
        let mut excess = self.resident_bytes - budget;
        let mut victims = Vec::new();
        for &(_, file) in &self.lru {
            if excess == 0 {
                break;
            }
            if self.in_window(file, |window| window.prefetch) {
                continue;
            }
            excess = excess.saturating_sub(self.files[file].resident_bytes);
            victims.push(file);
        }
        for file in victims {
            self.unload(file);
        }
    }

    /// Drop a file's content (expansion, viewed, and attribution state are kept).
    pub(super) fn unload(&mut self, file: usize) {
        let state = &mut self.files[file];
        if let Content::Loading(ticket) = state.content {
            self.pending.remove(&ticket);
        }
        state.content = Content::Unloaded;
        if let AttributionState::Pending(ticket) = state.attribution {
            self.pending.remove(&ticket);
            state.attribution = AttributionState::Missing;
        }
        self.pending.retain(|_, pending| match pending {
            Pending::Blob { file: owner, .. } => *owner != file,
            Pending::Hunks(_) | Pending::Attribution(_) => true,
        });
        self.rebuild_file(file);
    }

    /// Requests needed for the current viewport and prefetch window, nearest first. Returned
    /// needs are marked outstanding; pass each result back with its ticket. Without a viewport,
    /// the window is the start of the document.
    pub fn take_needs(&mut self) -> Vec<DocumentNeed> {
        if self.window.is_none() && !self.is_empty() {
            self.set_viewport(0, 0);
        }
        let Some(window) = self.window else {
            return Vec::new();
        };
        let mut order: Vec<usize> = (window.viewport.0..=window.viewport.1).collect();
        let (mut above, mut below) = (window.viewport.0, window.viewport.1);
        while above > window.prefetch.0 || below < window.prefetch.1 {
            if below < window.prefetch.1 {
                below += 1;
                order.push(below);
            }
            if above > window.prefetch.0 {
                above -= 1;
                order.push(above);
            }
        }
        let mut needs = Vec::new();
        for block in order {
            let BlockKind::File(file) = self.blocks[block].kind else {
                continue;
            };
            self.needs_for_file(file, &mut needs);
        }
        needs
    }

    fn inflight(&self, hunks: bool) -> usize {
        self.pending
            .values()
            .filter(|pending| matches!(pending, Pending::Hunks(_)) == hunks)
            .count()
    }

    fn ticket(&mut self, pending: Pending) -> Ticket {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        self.pending.insert(ticket, pending);
        ticket
    }

    fn needs_for_file(&mut self, file: usize, needs: &mut Vec<DocumentNeed>) {
        if matches!(self.files[file].content, Content::Unloaded) {
            let group = &self.groups[self.files[file].group];
            if group.source.mode == ResolvedSourceMode::Browse {
                // Browse sources have no hunks: the file is resident at once and its content is
                // read through blob lines.
                self.files[file].content = Content::Loaded(Box::default());
                self.rebuild_file(file);
            } else if self.inflight(true) < self.config.max_inflight_hunks
                && (self.in_window(file, |window| window.protected)
                    || self.resident_bytes < self.config.residency_budget_bytes)
            {
                let request = ReviewFileHunksRequest {
                    repo_path: self.repo_path.clone(),
                    source: group.source.clone(),
                    path: self.files[file].entry.path.clone(),
                    old_path: self.files[file].entry.old_path.clone(),
                    context_lines: Some(self.config.context_lines),
                    ignore_whitespace: self.ignore_whitespace,
                    max_lines: None,
                };
                let ticket = self.ticket(Pending::Hunks(file));
                self.files[file].content = Content::Loading(ticket);
                self.rebuild_file(file);
                needs.push(DocumentNeed::Hunks {
                    ticket,
                    file: self.files[file].key.clone(),
                    request,
                });
                return;
            } else {
                return;
            }
        }
        if self.files[file].loaded().is_none() {
            return;
        }
        if matches!(self.files[file].attribution, AttributionState::Missing)
            && self.inflight(false) < self.config.max_inflight_blobs
            && let Some(stack) = self.groups[self.files[file].group].stack.clone()
        {
            let entry = &self.files[file].entry;
            let request = ReviewStackAttributionRequest {
                repo_path: self.repo_path.clone(),
                stack: stack.stack,
                path: entry.path.clone(),
                old_path: entry.old_path.clone(),
            };
            let ticket = self.ticket(Pending::Attribution(file));
            self.files[file].attribution = AttributionState::Pending(ticket);
            needs.push(DocumentNeed::Attribution {
                ticket,
                file: self.files[file].key.clone(),
                request,
            });
        }
        for (start, count) in self.missing_blob_ranges(file) {
            if self.inflight(false) >= self.config.max_inflight_blobs {
                break;
            }
            let Some(selector) = self.blob_selector(file) else {
                break;
            };
            if let Content::Loaded(loaded) = &mut self.files[file].content
                && count > 0
            {
                loaded.covered.insert(start, start + count - 1);
            }
            let ticket = self.ticket(Pending::Blob { file, start, count });
            needs.push(DocumentNeed::BlobLines {
                ticket,
                file: self.files[file].key.clone(),
                request: ReviewBlobLinesRequest {
                    repo_path: self.repo_path.clone(),
                    blob: selector,
                    start_line: start,
                    count,
                },
            });
        }
    }

    /// Blob ranges to request for a resident file: the line count when the trailing region is
    /// open-ended (with the first revealed chunk there, if any), then revealed lines that are
    /// neither cached, requested, nor unavailable.
    fn missing_blob_ranges(&self, file: usize) -> Vec<(u32, u32)> {
        let state = &self.files[file];
        let Some(loaded) = state.loaded() else {
            return Vec::new();
        };
        if !state.expandable() {
            return Vec::new();
        }
        let chunk = self
            .config
            .blob_chunk_lines
            .clamp(1, REVIEW_BLOB_LINES_MAX_COUNT);
        let line_count_pending = self.pending.values().any(|pending| {
            matches!(pending, Pending::Blob { file: owner, count: 0, .. } if *owner == file)
        });
        let mut out = Vec::new();
        let regions = state.regions();
        let open = regions.iter().find(|region| region.end.is_none());
        let limit = state
            .line_count
            .unwrap_or_else(|| open.map_or(u32::MAX, |region| region.start.saturating_sub(1)));
        if let Some(region) = open
            && state.line_count.is_none()
            && !line_count_pending
            && !loaded.covered.contains(region.start)
        {
            let count = if state.revealed.contains(region.start) {
                chunk
            } else {
                0
            };
            out.push((region.start, count));
        }
        for (start, end) in state.revealed.intersect(1, limit) {
            for (s, e) in loaded.covered.complement_within(start, end) {
                let mut cursor = s;
                while cursor <= e {
                    let count = (e - cursor + 1).min(chunk);
                    out.push((cursor, count));
                    match cursor.checked_add(count) {
                        Some(next) => cursor = next,
                        None => break,
                    }
                }
            }
        }
        out
    }

    fn blob_selector(&self, file: usize) -> Option<ReviewBlobSelector> {
        let state = &self.files[file];
        let source = &self.groups[state.group].source;
        let (blob, revision, path) = match state.context_side() {
            ReviewLineSide::New => (&state.entry.new_blob, &source.head, &state.entry.path),
            ReviewLineSide::Old => (
                &state.entry.old_blob,
                &source.base,
                state.entry.old_path.as_ref().unwrap_or(&state.entry.path),
            ),
        };
        match blob {
            ReviewBlobRef::Object { oid } => Some(ReviewBlobSelector::Object { oid: oid.clone() }),
            ReviewBlobRef::WorkingTree => Some(ReviewBlobSelector::Revision {
                revision: revision.clone(),
                path: path.clone(),
            }),
            ReviewBlobRef::Absent => None,
        }
    }

    /// Accept a file's hunks. Results for tickets that are no longer outstanding (the file was
    /// evicted or re-requested) are ignored; returns whether the result was applied.
    pub fn accept_hunks(&mut self, ticket: Ticket, response: &ReviewFileHunksResponse) -> bool {
        let Some(Pending::Hunks(file)) = self.pending.get(&ticket).copied() else {
            return false;
        };
        self.pending.remove(&ticket);
        let hunks: Vec<DocHunk> = response
            .hunks
            .iter()
            .map(|hunk| DocHunk {
                range: HunkRange::of(hunk),
                heading: hunk.heading.as_deref().map(Arc::from),
                lines: hunk
                    .lines
                    .iter()
                    .map(|line| DocLine {
                        kind: line.kind,
                        old: line.old_line,
                        new: line.new_line,
                        text: Arc::from(line.content.as_str()),
                    })
                    .collect(),
            })
            .collect();
        let ranges: Vec<HunkRange> = hunks.iter().map(|hunk| hunk.range).collect();
        let content_bytes = hunks
            .iter()
            .map(|hunk| {
                std::mem::size_of::<DocHunk>()
                    + hunk
                        .lines
                        .iter()
                        .map(|line| std::mem::size_of::<DocLine>() + line.text.len())
                        .sum::<usize>()
            })
            .sum();
        let state = &mut self.files[file];
        let side = state.context_side();
        let derived = if response.truncated.is_some() || response.binary {
            None
        } else if state.other_side_exists() {
            gaps::line_count_from_trailing_context(&ranges, side, self.config.context_lines)
        } else {
            // A whole-file hunk; an added or deleted empty file has no hunks and no lines.
            Some(
                ranges
                    .last()
                    .map_or(0, |range| gaps::hunk_span(range, side).next - 1),
            )
        };
        // A line count read for an object id stays valid; working-tree content may have
        // changed since it was read, so its count is re-derived (or probed again).
        let pinned = matches!(
            match side {
                ReviewLineSide::New => &state.entry.new_blob,
                ReviewLineSide::Old => &state.entry.old_blob,
            },
            ReviewBlobRef::Object { .. }
        );
        if derived.is_some() || !pinned {
            state.line_count = derived;
        }
        if response.patch_id.is_some() {
            state.patch_id.clone_from(&response.patch_id);
        }
        state.content = Content::Loaded(Box::new(Loaded {
            hunks,
            ranges,
            binary: response.binary,
            truncated: response.truncated,
            context: BTreeMap::new(),
            covered: IntervalSet::new(),
            unavailable: Vec::new(),
            content_bytes,
        }));
        state.clamp_revealed();
        self.touch(file);
        if self.refresh_viewed(file) {
            self.rebuild_block(self.groups[self.files[file].group].header_block);
        }
        self.rebuild_file(file);
        self.enforce_budget();
        true
    }

    /// Accept context lines or a line count. Returns whether the result was applied.
    pub fn accept_blob_lines(
        &mut self,
        ticket: Ticket,
        response: &ReviewBlobLinesResponse,
    ) -> bool {
        let Some(Pending::Blob { file, start, count }) = self.pending.get(&ticket).copied() else {
            return false;
        };
        self.pending.remove(&ticket);
        let state = &mut self.files[file];
        let Content::Loaded(loaded) = &mut state.content else {
            return false;
        };
        if response.binary {
            // Not retried: a line-count probe of binary content covers its first line so the
            // open-ended gap is not probed again.
            let end = start + count.max(1) - 1;
            loaded.covered.insert(start, end);
            loaded
                .unavailable
                .push((start, end, Arc::from("binary content")));
        } else {
            for (offset, text) in response.lines.iter().enumerate() {
                let Some(line) = u32::try_from(offset)
                    .ok()
                    .and_then(|offset| response.start_line.checked_add(offset))
                else {
                    break;
                };
                loaded.content_bytes += text.len() + std::mem::size_of::<(u32, Arc<str>)>();
                loaded.context.insert(line, Arc::from(text.as_str()));
                loaded.covered.insert(line, line);
            }
            state.line_count = Some(response.total_lines);
        }
        if count == 0 {
            // A line-count probe covers nothing.
        } else if !response.binary {
            // Lines past the end are never requested again.
            let returned = u32::try_from(response.lines.len()).unwrap_or(u32::MAX);
            if returned < count {
                loaded.covered.insert(start + returned, start + count - 1);
            }
        }
        state.clamp_revealed();
        self.rebuild_file(file);
        self.enforce_budget();
        true
    }

    /// Accept whole-stack attribution for a file. Returns whether the result was applied.
    pub fn accept_attribution(
        &mut self,
        ticket: Ticket,
        response: ReviewStackAttributionResponse,
    ) -> bool {
        let Some(Pending::Attribution(file)) = self.pending.get(&ticket).copied() else {
            return false;
        };
        self.pending.remove(&ticket);
        let mut attribution = Attribution {
            new_ranges: response.new_ranges,
            old_ranges: response.old_ranges,
        };
        attribution.new_ranges.sort_unstable();
        attribution.old_ranges.sort_unstable();
        self.files[file].attribution = AttributionState::Loaded(attribution);
        self.rebuild_file(file);
        true
    }

    /// Record a failed request. A failed hunk load shows the file as unavailable until
    /// [`Self::retry`]; failed context lines show as unavailable lines; failed attribution
    /// leaves the file unlabeled. Returns whether the ticket was outstanding.
    pub fn fail(&mut self, ticket: Ticket, reason: &str) -> bool {
        let Some(pending) = self.pending.remove(&ticket) else {
            return false;
        };
        match pending {
            Pending::Hunks(file) => {
                self.files[file].content = Content::Failed(Arc::from(reason));
                self.rebuild_file(file);
            }
            Pending::Blob { file, start, count } => {
                if let Content::Loaded(loaded) = &mut self.files[file].content {
                    // A failed line-count probe is not retried automatically either.
                    let end = start + count.max(1) - 1;
                    loaded.covered.insert(start, end);
                    loaded.unavailable.push((start, end, Arc::from(reason)));
                }
                self.rebuild_file(file);
            }
            Pending::Attribution(file) => {
                self.files[file].attribution = AttributionState::Failed;
            }
        }
        true
    }

    /// Make a failed file loadable again. Returns whether it had failed.
    pub fn retry(&mut self, file: &FileKey) -> bool {
        let Some(&index) = self.file_index.get(file) else {
            return false;
        };
        if !matches!(self.files[index].content, Content::Failed(_)) {
            return false;
        }
        self.files[index].content = Content::Unloaded;
        self.rebuild_file(index);
        true
    }

    // ----------------------------------------------------------------------------------------
    // Context expansion
    // ----------------------------------------------------------------------------------------

    /// Reveal `count` lines at the bottom of the gap `gap` (directly above the content below
    /// it, growing upward). An open-ended gap has no known bottom and is left unchanged.
    /// Returns whether anything was revealed.
    pub fn reveal_above(&mut self, gap: &ItemKey, count: u32) -> bool {
        let Some((file, start, end)) = self.gap_extent(gap) else {
            return false;
        };
        let Some(end) = end else {
            return false;
        };
        let first = end.saturating_sub(count.saturating_sub(1)).max(start);
        count > 0 && self.reveal_range(file, first, end)
    }

    /// Reveal `count` lines at the top of the gap `gap` (directly below the content above it,
    /// growing downward). Returns whether anything was revealed.
    pub fn reveal_below(&mut self, gap: &ItemKey, count: u32) -> bool {
        let Some((file, start, end)) = self.gap_extent(gap) else {
            return false;
        };
        let last = start.saturating_add(count.saturating_sub(1));
        let last = end.map_or(last, |end| last.min(end));
        count > 0 && self.reveal_range(file, start, last)
    }

    /// Reveal the whole gap `gap`. Returns whether anything was revealed.
    pub fn reveal_all(&mut self, gap: &ItemKey) -> bool {
        let Some((file, start, end)) = self.gap_extent(gap) else {
            return false;
        };
        self.reveal_range(file, start, end.unwrap_or(u32::MAX))
    }

    /// Reveal every unchanged line of a file. Works before the file is loaded; the request is
    /// clamped once hunks and the line count are known. Returns whether anything changed.
    pub fn reveal_file(&mut self, file: &FileKey) -> bool {
        let Some(&index) = self.file_index.get(file) else {
            return false;
        };
        self.reveal_range(index, 1, u32::MAX)
    }

    /// Reveal context-side lines `[start, end]` of a file (clamped to its gaps). Returns whether
    /// anything changed.
    pub fn reveal_lines(&mut self, file: &FileKey, start: u32, end: u32) -> bool {
        let Some(&index) = self.file_index.get(file) else {
            return false;
        };
        self.reveal_range(index, start.max(1), end)
    }

    fn reveal_range(&mut self, file: usize, start: u32, end: u32) -> bool {
        let before = self.files[file].revealed.clone();
        self.files[file].revealed.insert(start, end);
        self.files[file].clamp_revealed();
        if self.files[file].revealed == before {
            return false;
        }
        if self.files[file].loaded().is_some() {
            self.rebuild_file(file);
        }
        true
    }

    /// File and context-side extent (`end` is `None` when open-ended) of the gap `gap`.
    fn gap_extent(&self, gap: &ItemKey) -> Option<(usize, u32, Option<u32>)> {
        let ItemKey::Gap { file, start } = gap else {
            return None;
        };
        let index = *self.file_index.get(file)?;
        let state = &self.files[index];
        state.loaded()?;
        for region in state.regions() {
            if !region.contains(*start) {
                continue;
            }
            if state.revealed.contains(*start) {
                return None;
            }
            let end = region.end.map(|end| {
                state
                    .revealed
                    .intersect(*start, end)
                    .first()
                    .map_or(end, |(next, _)| next - 1)
            });
            return Some((index, *start, end));
        }
        None
    }

    /// Revealed context of a file, on its context side.
    #[must_use]
    pub fn revealed(&self, file: &FileKey) -> Option<&IntervalSet> {
        self.file_index
            .get(file)
            .map(|index| &self.files[*index].revealed)
    }

    /// Persistable expansion state of every file with revealed context.
    #[must_use]
    pub fn expansions(&self) -> Vec<ReviewFileExpansion> {
        self.files
            .iter()
            .filter(|file| !file.revealed.is_empty())
            .filter(|file| self.groups[file.group].source.mode == ResolvedSourceMode::Diff)
            .map(|file| ReviewFileExpansion {
                source_id: file.key.source_id.clone(),
                path: file.key.path.clone(),
                part: file.key.part,
                revealed_new_lines: file.revealed.to_intervals(),
            })
            .collect()
    }

    /// Restore persisted expansion state; files that are no longer in the review are skipped.
    pub fn restore_expansions(&mut self, expansions: &[ReviewFileExpansion]) {
        for expansion in expansions {
            let key = FileKey::new(
                expansion.source_id.clone(),
                expansion.part,
                expansion.path.clone(),
            );
            let Some(&index) = self.file_index.get(&key) else {
                continue;
            };
            for interval in IntervalSet::from_intervals(&expansion.revealed_new_lines).iter() {
                self.reveal_range(index, interval.0, interval.1);
            }
        }
    }

    // ----------------------------------------------------------------------------------------
    // Annotations, composer
    // ----------------------------------------------------------------------------------------

    /// Replace the persisted annotations shown in the document.
    pub fn set_annotations(&mut self, annotations: &Annotations) {
        self.annotations_input = Some(annotations.clone());
        let collected = anchors::collect(annotations);
        let mut affected: std::collections::BTreeSet<usize> = self
            .file_annotations
            .iter()
            .enumerate()
            .filter(|(_, list)| !list.is_empty())
            .map(|(file, _)| file)
            .collect();
        self.review_annotations.clear();
        for list in &mut self.file_annotations {
            list.clear();
        }
        for (index, annotation) in collected.iter().enumerate() {
            match self.match_file(&annotation.anchor) {
                Some(file) if annotation.target != AnchorTarget::Review => {
                    self.file_annotations[file].push(index);
                    affected.insert(file);
                }
                _ => self.review_annotations.push(index),
            }
        }
        self.annotation_blocks.clear();
        for &index in &self.review_annotations {
            self.annotation_blocks
                .insert(collected[index].key.clone(), 0);
        }
        for (file, list) in self.file_annotations.iter().enumerate() {
            for &index in list {
                self.annotation_blocks
                    .insert(collected[index].key.clone(), self.files[file].block);
            }
        }
        self.annotations = collected;
        self.rebuild_block(0);
        for file in affected {
            self.rebuild_file(file);
        }
    }

    /// The file a persisted anchor refers to: same source and path, else the first file with
    /// that path.
    fn match_file(&self, anchor: &bcode_code_review_models::DraftAnchor) -> Option<usize> {
        if anchor.kind == bcode_code_review_models::ReviewAnchorKind::Review {
            return None;
        }
        let candidates = self.paths.get(&anchor.file_path)?;
        anchor
            .source_id
            .as_ref()
            .and_then(|source| {
                candidates
                    .iter()
                    .copied()
                    .find(|file| &self.files[*file].key.source_id == source)
            })
            .or_else(|| candidates.first().copied())
    }

    /// Show the composer at `anchor` (replacing any open composer).
    pub fn open_composer(&mut self, anchor: DocAnchor) {
        self.close_composer();
        let file = anchor
            .file
            .as_ref()
            .and_then(|file| self.file_index.get(file).copied());
        self.composer = Some(anchor);
        match file {
            Some(file) => self.rebuild_file(file),
            None => self.rebuild_block(0),
        }
    }

    /// Remove the composer. Returns its anchor.
    pub fn close_composer(&mut self) -> Option<DocAnchor> {
        let anchor = self.composer.take()?;
        match anchor
            .file
            .as_ref()
            .and_then(|file| self.file_index.get(file).copied())
        {
            Some(file) => self.rebuild_file(file),
            None => self.rebuild_block(0),
        }
        Some(anchor)
    }

    /// The open composer's anchor.
    #[must_use]
    pub const fn composer(&self) -> Option<&DocAnchor> {
        self.composer.as_ref()
    }

    /// Anchor for commenting on one item: a file header anchors the file; a line or row
    /// anchors its line (the new side of a changed row when it has one).
    #[must_use]
    pub fn anchor_for(&self, key: &ItemKey) -> Option<DocAnchor> {
        let index = self.index_of(key)?;
        match self.item(index)? {
            ReviewItem::FileHeader(item) => Some(DocAnchor {
                file: Some(item.file.clone()),
                target: AnchorTarget::File,
                head_oid: self.head_oid(&item.file),
                blob_oid: None,
            }),
            item @ (ReviewItem::Line(_) | ReviewItem::LinePair(_)) => {
                self.range_anchor_between(index, index, item.file()?.clone())
            }
            _ => None,
        }
    }

    /// Anchor for a range of lines between two line or row keys of the same file (in either
    /// order). Both line numbers of every line in between are included.
    #[must_use]
    pub fn range_anchor(&self, from: &ItemKey, to: &ItemKey) -> Option<DocAnchor> {
        let file = from.file()?.clone();
        if to.file() != Some(&file) {
            return None;
        }
        let a = self.index_of(from)?;
        let b = self.index_of(to)?;
        self.range_anchor_between(a.min(b), a.max(b), file)
    }

    fn range_anchor_between(&self, first: usize, last: usize, file: FileRef) -> Option<DocAnchor> {
        use super::items::LineKind;
        let mut old: Option<(u32, u32)> = None;
        let mut new: Option<(u32, u32)> = None;
        let mut end: Option<(ReviewLineSide, LineKind)> = None;
        let mut lines = 0_usize;
        let widen = |range: &mut Option<(u32, u32)>, line: u32| {
            *range = Some(range.map_or((line, line), |(s, e)| (s.min(line), e.max(line))));
        };
        for index in first..=last {
            match self.item(index)? {
                ReviewItem::Line(item) => {
                    if let Some(line) = item.old_line {
                        widen(&mut old, line);
                    }
                    if let Some(line) = item.new_line {
                        widen(&mut new, line);
                    }
                    end = Some((item.key_side().0, item.kind));
                }
                ReviewItem::LinePair(item) => {
                    // A single changed row anchors its new side only.
                    let new_only = first == last && item.emphasize;
                    if let Some(side) = item.old.as_ref().filter(|_| !new_only) {
                        widen(&mut old, side.line);
                    }
                    if let Some(side) = &item.new {
                        widen(&mut new, side.line);
                    }
                    end = item
                        .new
                        .as_ref()
                        .map(|side| (ReviewLineSide::New, side.kind))
                        .or_else(|| {
                            item.old
                                .as_ref()
                                .map(|side| (ReviewLineSide::Old, side.kind))
                        });
                }
                _ => continue,
            }
            lines += 1;
        }
        let (end_side, kind) = end?;
        let end_kind = match kind {
            LineKind::Added => ReviewLineKind::Added,
            LineKind::Removed => ReviewLineKind::Removed,
            LineKind::Context | LineKind::Expanded => ReviewLineKind::Context,
        };
        let span = |range: Option<(u32, u32)>| range.is_some_and(|(s, e)| s != e);
        Some(DocAnchor {
            head_oid: self.head_oid(&file),
            blob_oid: self.blob_oid(&file, end_side),
            file: Some(file),
            target: AnchorTarget::Lines(anchors::AnchorLines {
                old,
                new,
                end_side,
                end_kind,
                is_range: lines > 1 || span(old) || span(new),
            }),
        })
    }

    /// Object id of `file`'s blob on `side`, when that side is a blob object (not working-tree
    /// content or absent).
    #[must_use]
    pub fn blob_oid(&self, file: &FileKey, side: ReviewLineSide) -> Option<String> {
        let entry = &self.files[*self.file_index.get(file)?].entry;
        match side {
            ReviewLineSide::New => &entry.new_blob,
            ReviewLineSide::Old => &entry.old_blob,
        }
        .oid()
        .map(str::to_owned)
    }

    /// Commit the source of `file` resolved its head to, when the head is a commit.
    #[must_use]
    pub fn head_oid(&self, file: &FileKey) -> Option<String> {
        let state = &self.files[*self.file_index.get(file)?];
        self.groups[state.group]
            .source
            .head
            .commit_oid()
            .map(str::to_owned)
    }

    // ----------------------------------------------------------------------------------------
    // Viewed state
    // ----------------------------------------------------------------------------------------

    /// Apply the workspace's viewed marks (its effective marks, including legacy path marks).
    pub fn set_viewed_marks(&mut self, workspace: &ReviewWorkspace) {
        self.marks.clear();
        for mark in crate::viewed::effective_marks(workspace) {
            self.marks.entry(mark.path.clone()).or_default().push(mark);
        }
        let mut changed_groups = BTreeSet::new();
        for file in 0..self.files.len() {
            if self.refresh_viewed(file) {
                changed_groups.insert(self.files[file].group);
            }
        }
        for group in changed_groups {
            self.rebuild_block(self.groups[group].header_block);
        }
    }

    /// Recompute a file's viewed state, rebuilding its block when it changed. The caller
    /// rebuilds the group's source header when this returns `true`.
    pub(super) fn refresh_viewed(&mut self, file: usize) -> bool {
        let state = &self.files[file];
        let identity = Self::identity(state);
        let viewed_identity = identity.with_patch(state.patch_id.as_deref());
        let marks = std::iter::once(viewed_identity.path)
            .chain(viewed_identity.old_path)
            .filter_map(|path| self.marks.get(path))
            .flatten();
        let viewed = best_state(marks, &viewed_identity, self.config.viewed_reset);
        let state = &mut self.files[file];
        if state.viewed == viewed {
            return false;
        }
        let group = &mut self.groups[state.group];
        match (state.viewed.is_viewed(), viewed.is_viewed()) {
            (false, true) => {
                self.viewed_count += 1;
                group.viewed += 1;
            }
            (true, false) => {
                self.viewed_count -= 1;
                group.viewed -= 1;
            }
            _ => {}
        }
        state.viewed = viewed;
        self.rebuild_file(file);
        true
    }

    /// Viewed state of a file.
    #[must_use]
    pub fn viewed_state(&self, file: &FileKey) -> Option<ViewedState> {
        self.file_index
            .get(file)
            .map(|index| self.files[*index].viewed)
    }

    /// Identity to record when marking a file viewed: its index blob identities and, once its
    /// hunks were loaded, its patch id.
    #[must_use]
    pub fn viewed_identity(&self, file: &FileKey) -> Option<(EntryIdentity, Option<String>)> {
        let state = &self.files[*self.file_index.get(file)?];
        Some((Self::identity(state), state.patch_id.clone()))
    }

    fn identity(state: &FileState) -> EntryIdentity {
        EntryIdentity::from_entry(&state.entry).with_content(state.content_oid.as_deref())
    }

    /// Accept the diff fingerprints and working-tree content ids of files of `source_id`
    /// answered by `review.patch_ids.get`, and update their viewed state. Fingerprints of files
    /// whose hunks were loaded already are kept (they are the same identity).
    pub fn accept_fingerprints(
        &mut self,
        source_id: &str,
        response: &bcode_code_review_models::ReviewPatchIdsResponse,
    ) {
        let mut changed_groups = BTreeSet::new();
        for file in 0..self.files.len() {
            let state = &mut self.files[file];
            if state.key.source_id != source_id {
                continue;
            }
            let path = state.entry.path.as_str();
            if let Some(patch) = response.patch_ids.get(path)
                && state.patch_id.is_none()
            {
                state.patch_id = Some(patch.clone());
            }
            if let Some(content) = response.content_ids.get(path) {
                state.content_oid = Some(content.clone());
            }
            if self.refresh_viewed(file) {
                changed_groups.insert(self.files[file].group);
            }
        }
        for group in changed_groups {
            self.rebuild_block(self.groups[group].header_block);
        }
    }

    /// Files whose stored mark should be re-recorded with their current identity.
    #[must_use]
    pub fn viewed_marks_to_update(&self) -> Vec<FileRef> {
        self.files
            .iter()
            .filter(|file| file.viewed == ViewedState::ViewedNeedsUpdate)
            .map(|file| file.key.clone())
            .collect()
    }

    /// Mark the files whose patch is unchanged since the last review pass; every
    /// other file loses the mark. Rebuilds only the files whose mark changed.
    pub fn set_carried_over(&mut self, files: &BTreeSet<FileKey>) {
        for index in 0..self.files.len() {
            let carried = files.contains(self.files[index].key.as_ref());
            if self.files[index].carried_over != carried {
                self.files[index].carried_over = carried;
                self.rebuild_file(index);
            }
        }
    }

    /// Whether a file is marked unchanged since the last review pass.
    #[must_use]
    pub fn carried_over(&self, file: &FileKey) -> bool {
        self.file_index
            .get(file)
            .is_some_and(|index| self.files[*index].carried_over)
    }

    /// The file's patch id, once its hunks were loaded (kept after eviction).
    #[must_use]
    pub fn patch_id(&self, file: &FileKey) -> Option<&str> {
        self.file_index
            .get(file)
            .and_then(|index| self.files[*index].patch_id.as_deref())
    }

    /// The document's source groups (one per source or source part), in order.
    pub fn groups(&self) -> impl Iterator<Item = GroupInfo<'_>> {
        self.groups
            .iter()
            .enumerate()
            .map(|(index, group)| GroupInfo {
                index,
                source: &group.source,
                label: &group.label,
                stack: group.stack.as_ref(),
                file_count: group.files.len(),
                viewed: group.viewed,
                header: group.show_header.then(|| ItemKey::SourceHeader {
                    source_id: group.source.source_id.clone(),
                    part: group.source.part.as_ref().map(|part| part.index),
                }),
                hidden: group.hidden,
                loading: group.loading,
            })
    }

    /// Show only the groups of source `focus` (every group when `None`). The other groups keep
    /// their files, viewed state, and annotations but contribute no items, so the review reads as
    /// that source alone; the change reaches the list as splices. Returns whether anything changed.
    pub fn set_focus(&mut self, focus: Option<&str>) -> bool {
        let mut changed = Vec::new();
        for (index, group) in self.groups.iter_mut().enumerate() {
            let hidden = focus.is_some_and(|focus| group.source.source_id != focus);
            if group.hidden != hidden {
                group.hidden = hidden;
                changed.push(index);
            }
        }
        for &group in &changed {
            let header = self.groups[group].header_block;
            self.rebuild_block(header);
            for file in self.groups[group].files.clone() {
                self.rebuild_file(file);
            }
        }
        !changed.is_empty()
    }

    /// The source shown alone (see [`Self::set_focus`]), when one is.
    #[must_use]
    pub fn focus(&self) -> Option<&str> {
        let mut shown = self.groups.iter().filter(|group| !group.hidden);
        let first = shown.next()?;
        let focused = self.groups.iter().any(|group| group.hidden)
            && shown.all(|group| group.source.source_id == first.source.source_id);
        focused.then_some(first.source.source_id.as_str())
    }

    /// Whether file `file` (an index into the files) is in a hidden group.
    pub(super) fn file_hidden(&self, file: usize) -> bool {
        self.groups[self.files[file].group].hidden
    }

    /// Files of group `index` (see [`GroupInfo::index`]), in document order.
    pub fn group_files(&self, index: usize) -> impl Iterator<Item = &FileRef> {
        self.groups
            .get(index)
            .map_or(&[][..], |group| &self.files[group.files.clone()])
            .iter()
            .map(|file| &file.key)
    }

    /// Lines added and removed across every file.
    #[must_use]
    pub fn totals(&self) -> (u64, u64) {
        self.files.iter().fold((0, 0), |(add, del), file| {
            (
                add + u64::from(file.entry.additions),
                del + u64::from(file.entry.deletions),
            )
        })
    }

    /// Viewed progress across the whole review (`O(1)`).
    #[must_use]
    pub const fn viewed_progress(&self) -> ViewedProgress {
        ViewedProgress {
            viewed: self.viewed_count,
            total: self.files.len(),
        }
    }

    // ----------------------------------------------------------------------------------------
    // Internals
    // ----------------------------------------------------------------------------------------

    pub(super) fn block_start(&self, block: usize) -> usize {
        usize::try_from(self.lengths.prefix(block)).unwrap_or(usize::MAX)
    }

    pub(super) fn locate_index(&self, index: usize) -> Option<(usize, usize)> {
        let block = self.lengths.find(index as u64)?;
        Some((block, index - self.block_start(block)))
    }

    pub(super) fn block_of_key(&self, key: &ItemKey) -> Option<usize> {
        if let Some(file) = key.file() {
            return self
                .file_index
                .get(file)
                .map(|index| self.files[*index].block);
        }
        match key {
            ItemKey::SourceHeader { source_id, part } => self
                .groups
                .iter()
                .find(|group| {
                    &group.source.source_id == source_id
                        && group.source.part.as_ref().map(|p| p.index) == *part
                })
                .map(|group| group.header_block),
            ItemKey::Thread { .. } | ItemKey::AgentThread { .. } => {
                self.annotation_blocks.get(key).copied()
            }
            ItemKey::Composer => Some(
                self.composer
                    .as_ref()
                    .and_then(|anchor| anchor.file.as_ref())
                    .and_then(|file| self.file_index.get(file))
                    .map_or(0, |file| self.files[*file].block),
            ),
            _ => None,
        }
    }

    pub(super) fn rebuild_file(&mut self, file: usize) {
        let block = self.files[file].block;
        self.rebuild_block(block);
    }

    /// Rebuild one block and emit the minimal splice for it.
    pub(super) fn rebuild_block(&mut self, block: usize) {
        let built = self.build_block(block);
        let old = std::mem::take(&mut self.blocks[block].items);
        let new = built.items;
        let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let max_suffix = old.len().min(new.len()) - prefix;
        let suffix = old
            .iter()
            .rev()
            .zip(new.iter().rev())
            .take(max_suffix)
            .take_while(|(a, b)| a == b)
            .count();
        let removed = old.len() - prefix - suffix;
        let inserted = new.len() - prefix - suffix;
        let start = self.block_start(block) + prefix;
        let lookup = BlockLookup::build(&new);
        if let BlockKind::File(file) = self.blocks[block].kind {
            let state = &mut self.files[file];
            let bytes = state.loaded().map_or(0, |loaded| {
                loaded.content_bytes
                    + new.len() * (std::mem::size_of::<ReviewItem>() + std::mem::size_of::<u32>())
                    + lookup.bytes()
            });
            self.resident_bytes = self.resident_bytes - state.resident_bytes + bytes;
            state.resident_bytes = bytes;
            if state.loaded().is_some() {
                self.lru.insert((state.last_used, file));
            } else {
                self.lru.remove(&(state.last_used, file));
            }
        }
        self.blocks[block].items = new;
        self.blocks[block].hunk_parent = built.hunk_parent;
        self.blocks[block].lookup = lookup;
        if self.lengths.len() == self.blocks.len() {
            self.lengths
                .set(block, self.blocks[block].items.len() as u64);
            self.rows
                .set(block, super::build::block_rows(&self.blocks[block]));
        }
        if removed > 0 || inserted > 0 {
            self.revision += 1;
            self.splices.push(Splice {
                start,
                removed,
                inserted,
                revision: self.revision,
            });
        }
    }
}

impl ReviewDocument {
    /// Assert the derived state agrees with a from-scratch derivation:
    ///
    /// * every stored block equals a fresh build of it (no stale items after any operation);
    /// * every item's key is unique and resolves back to its own index;
    /// * resident bytes, the least-recently-used order, and viewed counts match the files.
    ///
    /// # Panics
    ///
    /// Panics, naming `context`, when any of them disagrees.
    #[cfg(any(test, feature = "testing"))]
    pub fn assert_consistent(&self, context: &str) {
        for block in 0..self.blocks.len() {
            let fresh = self.build_block(block);
            assert_eq!(
                self.blocks[block].items, fresh.items,
                "{context}: block {block} is stale"
            );
            assert_eq!(
                self.blocks[block].hunk_parent, fresh.hunk_parent,
                "{context}: block {block} sections are stale"
            );
        }
        let mut seen = BTreeSet::new();
        for index in 0..self.len() {
            let key = self.item(index).expect("item").key();
            assert!(seen.insert(key.clone()), "{context}: duplicate key {key:?}");
            assert_eq!(
                self.index_of(&key),
                Some(index),
                "{context}: key {key:?} does not resolve to its index"
            );
        }
        let resident: usize = self.files.iter().map(|file| file.resident_bytes).sum();
        assert_eq!(self.resident_bytes(), resident, "{context}: resident bytes");
        let lru: BTreeSet<(u64, usize)> = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, file)| file.loaded().is_some())
            .map(|(index, file)| (file.last_used, index))
            .collect();
        assert_eq!(self.lru, lru, "{context}: least-recently-used order");
        let viewed = self
            .files
            .iter()
            .filter(|file| file.viewed.is_viewed())
            .count();
        assert_eq!(
            self.viewed_progress().viewed,
            viewed,
            "{context}: viewed count"
        );
    }
}
