//! Review document items. Items are renderer-neutral data: no layout,
//! styling, or terminal types.

use super::keys::{FileNoticeKind, FileRef, ItemKey};
use bcode_code_review_models::{
    ReviewFileStatus, ReviewHunksTruncation, ReviewIndexFlag, ReviewLineSide, ReviewThreadKind,
    ReviewThreadSeverity,
};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Diff layout of the document's line items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutMode {
    /// One [`ReviewItem::Line`] per diff line.
    #[default]
    Unified,
    /// One [`ReviewItem::LinePair`] per side-by-side row.
    SideBySide,
}

/// Kind of one displayed line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Unchanged line inside a hunk.
    Context,
    /// Added line.
    Added,
    /// Removed line.
    Removed,
    /// Unchanged line revealed from a gap.
    Expanded,
}

impl LineKind {
    /// Whether the line is an addition or removal.
    #[must_use]
    pub const fn is_change(self) -> bool {
        matches!(self, Self::Added | Self::Removed)
    }
}

/// Text of one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineContent {
    /// Loaded text (without the diff marker or line terminator).
    Text(Arc<str>),
    /// Revealed context whose text is being read from its blob.
    Loading,
    /// Revealed context whose text could not be read.
    Unavailable(Arc<str>),
}

impl LineContent {
    /// Loaded text, when available.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Loading | Self::Unavailable(_) => None,
        }
    }
}

/// Load state of a file's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileLoad {
    /// Not resident; shown as a placeholder.
    Unloaded,
    /// Requested; shown as a placeholder.
    Loading,
    /// Resident.
    Loaded,
    /// The request failed.
    Failed(Arc<str>),
}

/// Layer label of a stack review (pull request number), from `review.stack.attribution`.
pub type LayerNumber = u64;

/// Header of one source or source part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceHeaderItem {
    /// Workspace source id.
    pub source_id: String,
    /// Part index.
    pub part: Option<u32>,
    /// Source label (and part label for multi-part sources).
    pub label: String,
    /// Stack layer this source reviews, when it is a stack layer.
    pub layer: Option<StackLayerLabel>,
    /// Number of files in the group.
    pub file_count: usize,
    /// Number of viewed files in the group.
    pub viewed_count: usize,
    /// Whether the group's files are still being listed.
    pub loading: bool,
}

/// The introduction of a source, before its files.
///
/// For a pull request: its description, reviews, and conversation, ending in a break before
/// the first file. What it shows is the presenter's; the document only places it (see
/// [`super::ReviewDocument::set_intros`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceIntroItem {
    /// Workspace source id.
    pub source_id: String,
    /// Whether the source's header (banner) is shown above it.
    pub under_header: bool,
}

/// Position of a reviewed stack layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackLayerLabel {
    /// Pull request number.
    pub number: LayerNumber,
    /// Pull request title.
    pub title: String,
    /// One-based position from the trunk.
    pub position: usize,
    /// Layers in the stack: for a layer of a step-through review, the most any of the review's
    /// stack-layer sources resolved (the whole stack as far as the review reaches); for a stack
    /// source, its layers.
    pub count: usize,
    /// Whether this source reviews the whole stack rather than one layer.
    pub whole_stack: bool,
    /// Branch the layer's pull request goes into (empty when unknown).
    pub base_branch: String,
    /// Branch the layer's pull request comes from (empty when unknown).
    pub head_branch: String,
}

/// Header of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHeaderItem {
    /// File.
    pub file: FileRef,
    /// File status.
    pub status: ReviewFileStatus,
    /// Old path for renames and copies.
    pub old_path: Option<String>,
    /// Added lines.
    pub additions: u32,
    /// Removed lines.
    pub deletions: u32,
    /// Index flags.
    pub flags: BTreeSet<ReviewIndexFlag>,
    /// Whether the file shows as viewed.
    pub viewed: bool,
    /// Content load state.
    pub load: FileLoad,
    /// Stack layers that wrote or removed lines of this file (whole-stack reviews).
    pub layers: Vec<LayerNumber>,
    /// Threads anchored in this file (including outdated ones), resident or not.
    pub thread_count: usize,
    /// Whether the file's patch is unchanged since the last review pass (a re-review after a
    /// rebase): shown de-emphasized.
    pub carried_over: bool,
    /// Position of the file in its group (1-based).
    pub position: u32,
    /// Files in its group.
    pub count: u32,
    /// The file's content is collapsed (only its header shows).
    pub collapsed: bool,
}

/// The facts and actions under a file's header (see [`super::FileChrome::details`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDetailsItem {
    /// File.
    pub file: FileRef,
    /// File status.
    pub status: ReviewFileStatus,
    /// Old path for renames and copies.
    pub old_path: Option<String>,
    /// Index flags.
    pub flags: BTreeSet<ReviewIndexFlag>,
    /// Stack layers that wrote or removed lines of this file (whole-stack reviews).
    pub layers: Vec<LayerNumber>,
    /// Threads anchored in this file.
    pub thread_count: usize,
    /// Lines of the file's new side, when known.
    pub line_count: Option<u32>,
    /// Hunks of the file's diff, once its content is resident.
    pub hunks: Option<u32>,
    /// Whether the file shows as viewed.
    pub viewed: bool,
    /// The file's content is collapsed.
    pub collapsed: bool,
    /// The file belongs to a stack source.
    pub in_stack: bool,
}

/// Estimated-height stand-in for a file whose content is not resident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceholderItem {
    /// File.
    pub file: FileRef,
    /// Estimated rows of the file's content, from its `+/-` counts.
    pub estimated_rows: u32,
    /// Whether the content has been requested.
    pub loading: bool,
}

/// Notice about a file's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNoticeItem {
    /// File.
    pub file: FileRef,
    /// Kind.
    pub kind: FileNoticeKind,
    /// Failure reason for [`FileNoticeKind::Unavailable`].
    pub reason: Option<Arc<str>>,
    /// Truncation details for [`FileNoticeKind::TooLarge`].
    pub truncation: Option<ReviewHunksTruncation>,
}

/// Header of one hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkHeaderItem {
    /// File.
    pub file: FileRef,
    /// Old-side start line.
    pub old_start: u32,
    /// Old-side line count.
    pub old_count: u32,
    /// New-side start line.
    pub new_start: u32,
    /// New-side line count.
    pub new_count: u32,
    /// Hunk heading (enclosing function), when git reported one.
    pub heading: Option<Arc<str>>,
    /// Stack layers that wrote or removed lines of this hunk.
    pub layers: Vec<LayerNumber>,
}

/// One unified line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineItem {
    /// File.
    pub file: FileRef,
    /// Line kind.
    pub kind: LineKind,
    /// Old-side line number, when the line exists on the old side.
    pub old_line: Option<u32>,
    /// New-side line number, when the line exists on the new side.
    pub new_line: Option<u32>,
    /// Text.
    pub content: LineContent,
    /// Stack layer that wrote (added) or removed this line.
    pub layer: Option<LayerNumber>,
}

impl LineItem {
    /// Side this line is keyed by.
    #[must_use]
    pub const fn key_side(&self) -> (ReviewLineSide, u32) {
        match (self.kind, self.new_line, self.old_line) {
            (LineKind::Removed, _, Some(old)) | (_, None, Some(old)) => (ReviewLineSide::Old, old),
            (_, Some(new), _) => (ReviewLineSide::New, new),
            (_, None, None) => (ReviewLineSide::New, 0),
        }
    }
}

/// One side of a side-by-side row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairSide {
    /// Line number on this side.
    pub line: u32,
    /// Line kind.
    pub kind: LineKind,
    /// Text.
    pub content: LineContent,
    /// Stack layer that wrote or removed this line.
    pub layer: Option<LayerNumber>,
}

/// One side-by-side row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinePairItem {
    /// File.
    pub file: FileRef,
    /// Old side.
    pub old: Option<PairSide>,
    /// New side.
    pub new: Option<PairSide>,
    /// Intra-line pairing hint: a removed line paired with an added line, whose changed spans
    /// can be emphasized.
    pub emphasize: bool,
}

/// State of a gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GapState {
    /// Hidden context of known extent.
    Hidden,
    /// Hidden context after the last hunk whose end is not known until the file's line count
    /// arrives.
    OpenEnded,
}

/// Unexpanded context between, before, or after hunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapItem {
    /// File.
    pub file: FileRef,
    /// Side the gap's line numbers are counted on (the new side, or the old side for deleted
    /// files).
    pub side: ReviewLineSide,
    /// First hidden line on `side`.
    pub start: u32,
    /// Last hidden line on `side`; `None` when [`GapState::OpenEnded`].
    pub end: Option<u32>,
    /// First hidden line on the other side, when it exists.
    pub other_start: Option<u32>,
    /// State.
    pub state: GapState,
    /// Threads whose anchor line is hidden in this gap.
    pub hidden_threads: usize,
}

impl GapItem {
    /// Hidden lines, when known.
    #[must_use]
    pub fn hidden_lines(&self) -> Option<u32> {
        self.end.map(|end| end - self.start + 1)
    }

    /// Hidden range on the old side, when it exists and is known.
    #[must_use]
    pub fn old_range(&self) -> Option<(u32, u32)> {
        let len = self.hidden_lines()?;
        match self.side {
            ReviewLineSide::Old => Some((self.start, self.start + len - 1)),
            ReviewLineSide::New => self.other_start.map(|start| (start, start + len - 1)),
        }
    }

    /// Hidden range on the new side, when it exists and is known.
    #[must_use]
    pub fn new_range(&self) -> Option<(u32, u32)> {
        let len = self.hidden_lines()?;
        match self.side {
            ReviewLineSide::New => Some((self.start, self.start + len - 1)),
            ReviewLineSide::Old => self.other_start.map(|start| (start, start + len - 1)),
        }
    }
}

/// Where an annotation is shown relative to its anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorPlacement {
    /// A review-level annotation.
    Review,
    /// A file-level annotation.
    File,
    /// After its anchor line (the last line of a range).
    Line,
    /// After the gap that hides its anchor line.
    HiddenContext,
    /// The anchor no longer exists; shown at the file (or review) level, never moved to another
    /// line.
    Outdated(OutdatedReason),
}

impl AnchorPlacement {
    /// Whether the anchor is outdated.
    #[must_use]
    pub const fn is_outdated(self) -> bool {
        matches!(self, Self::Outdated(_))
    }
}

/// Why an anchor is outdated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutdatedReason {
    /// The anchored file is not part of the review.
    FileMissing,
    /// The anchored line no longer exists.
    LineMissing,
    /// The anchored line exists but is no longer the same kind of line (for example a removed
    /// line that is no longer removed).
    LineChanged,
    /// The anchor was made at a different head commit than the one under review, so its line
    /// numbers may refer to other content (anchors that recorded no blob).
    RevisionChanged,
    /// The anchored file side's content changed since the anchor was made (its blob differs),
    /// so its line numbers may refer to other content.
    ContentChanged,
}

/// A draft or imported thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadItem {
    /// Thread id.
    pub thread_id: String,
    /// Placement relative to the anchor.
    pub placement: AnchorPlacement,
    /// Comment ids in order.
    pub comment_ids: Vec<String>,
    /// Suggestions shown with this thread.
    pub suggestion_ids: Vec<String>,
    /// Whether every comment is resolved.
    pub resolved: bool,
    /// Thread kind.
    pub kind: ReviewThreadKind,
    /// Thread severity.
    pub severity: ReviewThreadSeverity,
    /// Anchored line range on the anchor side, for line anchors.
    pub lines: Option<(ReviewLineSide, u32, u32)>,
}

/// An AI exchange, or AI suggestions without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentThreadItem {
    /// Exchange id, or suggestion id.
    pub id: String,
    /// Exchange id, when this item shows an exchange.
    pub exchange_id: Option<String>,
    /// Placement relative to the anchor.
    pub placement: AnchorPlacement,
    /// Suggestions shown with this exchange.
    pub suggestion_ids: Vec<String>,
    /// Anchored line range on the anchor side, for line anchors.
    pub lines: Option<(ReviewLineSide, u32, u32)>,
}

/// The inline composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerItem {
    /// Anchor being commented on.
    pub anchor: super::anchors::DocAnchor,
}

/// One item of the review document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewItem {
    /// Source or part header.
    SourceHeader(SourceHeaderItem),
    /// A source's introduction, before its files.
    SourceIntro(SourceIntroItem),
    /// File header.
    FileHeader(FileHeaderItem),
    /// Facts and actions under a file header.
    FileDetails(FileDetailsItem),
    /// The end of a file: the break before the next one.
    FileEnd(FileRef),
    /// Placeholder for non-resident content.
    Placeholder(PlaceholderItem),
    /// File notice.
    FileNotice(FileNoticeItem),
    /// Hunk header.
    HunkHeader(HunkHeaderItem),
    /// Unified line.
    Line(LineItem),
    /// Side-by-side row.
    LinePair(LinePairItem),
    /// Unexpanded context.
    Gap(GapItem),
    /// Thread.
    Thread(ThreadItem),
    /// AI exchange.
    AgentThread(AgentThreadItem),
    /// Composer.
    Composer(ComposerItem),
}

impl ReviewItem {
    /// Stable key of this item.
    #[must_use]
    pub fn key(&self) -> ItemKey {
        match self {
            Self::SourceHeader(item) => ItemKey::SourceHeader {
                source_id: item.source_id.clone(),
                part: item.part,
            },
            Self::SourceIntro(item) => ItemKey::SourceIntro {
                source_id: item.source_id.clone(),
            },
            Self::FileHeader(item) => ItemKey::FileHeader(item.file.clone()),
            Self::FileDetails(item) => ItemKey::FileDetails(item.file.clone()),
            Self::FileEnd(file) => ItemKey::FileEnd(file.clone()),
            Self::Placeholder(item) => ItemKey::Placeholder(item.file.clone()),
            Self::FileNotice(item) => ItemKey::FileNotice {
                file: item.file.clone(),
                kind: item.kind,
            },
            Self::HunkHeader(item) => ItemKey::HunkHeader {
                file: item.file.clone(),
                old_start: item.old_start,
                new_start: item.new_start,
            },
            Self::Line(item) => {
                let (side, line) = item.key_side();
                ItemKey::Line {
                    file: item.file.clone(),
                    side,
                    line,
                }
            }
            Self::LinePair(item) => ItemKey::LinePair {
                file: item.file.clone(),
                old: item.old.as_ref().map(|side| side.line),
                new: item.new.as_ref().map(|side| side.line),
            },
            Self::Gap(item) => ItemKey::Gap {
                file: item.file.clone(),
                start: item.start,
            },
            Self::Thread(item) => ItemKey::Thread {
                thread_id: item.thread_id.clone(),
            },
            Self::AgentThread(item) => ItemKey::AgentThread {
                id: item.id.clone(),
            },
            Self::Composer(_) => ItemKey::Composer,
        }
    }

    /// File this item belongs to.
    #[must_use]
    pub const fn file(&self) -> Option<&FileRef> {
        match self {
            Self::FileHeader(FileHeaderItem { file, .. })
            | Self::FileDetails(FileDetailsItem { file, .. })
            | Self::FileEnd(file)
            | Self::Placeholder(PlaceholderItem { file, .. })
            | Self::FileNotice(FileNoticeItem { file, .. })
            | Self::HunkHeader(HunkHeaderItem { file, .. })
            | Self::Line(LineItem { file, .. })
            | Self::LinePair(LinePairItem { file, .. })
            | Self::Gap(GapItem { file, .. }) => Some(file),
            Self::SourceHeader(_)
            | Self::SourceIntro(_)
            | Self::Thread(_)
            | Self::AgentThread(_)
            | Self::Composer(_) => None,
        }
    }

    /// Old and new line numbers of a line or row item.
    #[must_use]
    pub fn line_numbers(&self) -> Option<(Option<u32>, Option<u32>)> {
        match self {
            Self::Line(item) => Some((item.old_line, item.new_line)),
            Self::LinePair(item) => Some((
                item.old.as_ref().map(|side| side.line),
                item.new.as_ref().map(|side| side.line),
            )),
            _ => None,
        }
    }

    /// Whether this is a changed line (or a row with a changed side).
    #[must_use]
    pub fn is_change(&self) -> bool {
        match self {
            Self::Line(item) => item.kind.is_change(),
            Self::LinePair(item) => {
                item.old.as_ref().is_some_and(|side| side.kind.is_change())
                    || item.new.as_ref().is_some_and(|side| side.kind.is_change())
            }
            _ => false,
        }
    }

    /// Sticky-header level: files are level 0, hunks level 1.
    #[must_use]
    pub const fn header_level(&self) -> Option<u8> {
        match self {
            Self::FileHeader(_) => Some(0),
            Self::HunkHeader(_) => Some(1),
            _ => None,
        }
    }
}
