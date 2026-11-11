//! Row presentation of review document items.
//!
//! Every item becomes [`ItemRows`]: the styled lines it paints, its height, and the interactive
//! segments of a gap row. Lines are produced once per item, width, and presentation revision
//! and kept in a bounded [`RowCache`]; measurement, painting, and hit registration read the same
//! cached rows, so their geometry always agrees.
//!
//! * Diff lines and side-by-side pairs are laid out by BMUX `layout_diff_row` with one gutter
//!   per document, intra-line emphasis, syntax ranges, and an explicit overflow policy.
//! * Header, hunk, notice, gap, and annotation rows are composed from styled spans and clipped
//!   by BMUX text primitives (`Line::truncate`, `place_trailing`); nothing measures text by
//!   bytes or characters.

use super::syntax::SyntaxStore;
use super::theme::{ReviewTheme, status_letter};
use crate::v2::document::{
    FileHeaderItem, FileNoticeItem, FileNoticeKind, GapItem, GapState, HunkHeaderItem, ItemKey,
    LayoutMode, LineContent, LineItem, LineKind, LinePairItem, PairSide, PlaceholderItem,
    ReviewDocument, ReviewItem, SourceHeaderItem,
};
use bcode_code_review_models::{ReviewIndexFlag, ReviewLineSide};
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::HitRegion;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::selection::SelectionTextRun;
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span, TrailingAlign, TrailingPlacement};
use bmux_tui::text_width::display_width;
use bmux_tui_components::diff_viewer::{
    ChangedRange, DiffLineKind, DiffRowCell, DiffRowGutter, DiffRowItem, DiffRowLayout,
    DiffRowOptions, DiffRowOverflow, DiffRowText, DiffSourceSide, DiffSyntaxRange, diff_row_height,
    intraline_changed_ranges, layout_diff_row,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;

/// Items at most scanned to pair a unified removed line with its added line.
const MAX_PAIRING_SCAN: usize = 4_096;
/// Cached item rows per cache generation.
const ROW_CACHE_CAPACITY: usize = 4_096;

/// Virtual-list key of one document item: its stable [`ItemKey`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RowKey(pub ItemKey);

impl fmt::Display for RowKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const SEP: char = '\u{1f}';
        let file = |f: &mut fmt::Formatter<'_>, file: &crate::v2::document::FileKey| {
            write!(
                f,
                "{}{SEP}{}{SEP}{}",
                file.source_id,
                file.part.map_or(-1, i64::from),
                file.path
            )
        };
        match &self.0 {
            ItemKey::SourceHeader { source_id, part } => {
                write!(f, "src{SEP}{source_id}{SEP}{}", part.map_or(-1, i64::from))
            }
            ItemKey::FileHeader(key) => {
                f.write_str("fh")?;
                f.write_char(SEP)?;
                file(f, key)
            }
            ItemKey::Placeholder(key) => {
                f.write_str("ph")?;
                f.write_char(SEP)?;
                file(f, key)
            }
            ItemKey::FileNotice { file: key, kind } => {
                write!(f, "fn{SEP}{kind:?}{SEP}")?;
                file(f, key)
            }
            ItemKey::HunkHeader {
                file: key,
                old_start,
                new_start,
            } => {
                write!(f, "hh{SEP}{old_start}{SEP}{new_start}{SEP}")?;
                file(f, key)
            }
            ItemKey::Line {
                file: key,
                side,
                line,
            } => {
                write!(f, "ln{SEP}{side:?}{SEP}{line}{SEP}")?;
                file(f, key)
            }
            ItemKey::LinePair {
                file: key,
                old,
                new,
            } => {
                write!(
                    f,
                    "lp{SEP}{}{SEP}{}{SEP}",
                    old.map_or(-1, i64::from),
                    new.map_or(-1, i64::from)
                )?;
                file(f, key)
            }
            ItemKey::Gap { file: key, start } => {
                write!(f, "gap{SEP}{start}{SEP}")?;
                file(f, key)
            }
            ItemKey::Thread { thread_id } => write!(f, "th{SEP}{thread_id}"),
            ItemKey::AgentThread { id } => write!(f, "ag{SEP}{id}"),
            ItemKey::Composer => f.write_str("composer"),
        }
    }
}

use std::fmt::Write as _;

/// Presentation inputs shared by every row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewParams {
    /// Review theme.
    pub theme: ReviewTheme,
    /// Overflow policy of diff content.
    pub overflow: DiffRowOverflow,
    /// Gutter shared by every diff row of the document.
    pub gutter: DiffRowGutter,
    /// Tab stop interval of diff text.
    pub tab_width: u16,
    /// Lines a gap segment reveals.
    pub expand_step: u32,
    /// Changes whenever row geometry can change (wrap, gutter, layout mode).
    pub layout_revision: u64,
    /// Changes whenever row styling changes (theme, palette).
    pub paint_revision: u64,
    /// Changes whenever annotation content (threads, AI answers) changes.
    pub annotation_revision: u64,
}

/// What a gap segment does when clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapAction {
    /// Reveal lines directly above the content below the gap.
    Up,
    /// Reveal lines directly below the content above the gap.
    Down,
    /// Reveal every line of the gap.
    All,
}

/// An action on a thread item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadAction {
    /// Reply.
    Reply,
    /// Edit the latest comment.
    Edit,
    /// Delete the latest comment.
    Delete,
    /// Resolve or reopen.
    Resolve,
    /// Ask Bcode about the thread's anchor.
    Ask,
    /// Collapse or expand the thread.
    Fold,
    /// Accept the latest pending suggestion as a draft.
    Accept,
    /// Ask Bcode to refine the latest pending suggestion.
    Refine,
    /// Reject the latest pending suggestion.
    Reject,
}

/// An action on an AI answer item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAction {
    /// Open the linked session.
    Open,
    /// Delete the exchange.
    Delete,
    /// Observe the session again after its stream stopped.
    Retry,
    /// Ask a follow-up question in the linked session.
    FollowUp,
    /// Copy the answer to the clipboard.
    Copy,
    /// Show the whole answer, or the preview again.
    Expand,
    /// Turn the answer into a suggested comment.
    Suggest,
    /// Turn the answer into a draft comment.
    Draft,
    /// Accept the latest pending suggestion as a draft.
    Accept,
    /// Ask Bcode to refine the latest pending suggestion.
    Refine,
    /// Reject the latest pending suggestion.
    Reject,
}

/// How threads and AI answers are folded (presentation only; persisted with the workspace).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Folds {
    /// Threads shown as their header only.
    pub collapsed: std::collections::BTreeSet<String>,
    /// AI exchanges whose whole answer is shown.
    pub expanded: std::collections::BTreeSet<String>,
    /// Changes whenever the folds change.
    pub revision: u64,
}

/// What a clickable item cell does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemAction {
    /// A gap segment.
    Gap(GapAction),
    /// A thread action.
    Thread(ThreadAction),
    /// An AI answer action.
    Agent(AgentAction),
    /// A composer button.
    Composer(super::composer::ComposerButton),
    /// The gutter's comment affordance on a hovered line.
    Comment,
}

/// Clickable cells on one row of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionCells<A> {
    /// Row within the item.
    pub row: u16,
    /// Cells covered on the row.
    pub cells: Range<u16>,
    /// Action.
    pub action: A,
}

/// A click on an item action, collected during event routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemClick {
    /// Item key.
    pub key: ItemKey,
    /// Requested action.
    pub action: ItemAction,
}

/// What the pointer did over an item, collected during event routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerKind {
    /// Button pressed.
    Down,
    /// Moved with the button held.
    Drag,
    /// Button released.
    Up,
    /// Moved without a button.
    Moved,
}

/// A pointer event over an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerHit {
    /// Item under the pointer.
    pub key: ItemKey,
    /// Event kind.
    pub kind: PointerKind,
    /// Whether the item is a diff line or row.
    pub diff: bool,
}

/// Interaction collected while routing one event through the list.
#[derive(Debug, Default)]
pub struct Interaction {
    /// Action clicks.
    pub clicks: RefCell<Vec<ItemClick>>,
    /// Pointer events over items.
    pub pointer: RefCell<Vec<PointerHit>>,
}

/// Per-item presentation flags the surface decides (paint only; layout never depends on them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ItemFlags {
    /// The cursor item.
    pub cursor: bool,
    /// Inside the selected line range.
    pub selected: bool,
    /// Under the pointer (shows the gutter comment affordance on diff rows).
    pub hovered: bool,
}

impl ItemFlags {
    /// Bits for paint revisions.
    #[must_use]
    pub const fn bits(self) -> u64 {
        (self.cursor as u64) | ((self.selected as u64) << 1) | ((self.hovered as u64) << 2)
    }
}

/// Search matches to highlight, by line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchHighlights {
    /// Byte ranges per `(file, side, line)`.
    pub lines: BTreeMap<(crate::v2::document::FileRef, ReviewLineSide, u32), Vec<(usize, usize)>>,
    /// The current match's line.
    pub current: Option<(crate::v2::document::FileRef, ReviewLineSide, u32)>,
    /// Changes whenever the highlights change.
    pub revision: u64,
}

/// Rows of one item at one width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRows {
    /// Painted lines, one per row (fewer than `height` for placeholders).
    pub lines: Vec<Line>,
    /// Item height in rows.
    pub height: u64,
    /// Clickable cells.
    pub actions: Vec<ActionCells<ItemAction>>,
    /// Row layout and source mapping of diff lines and rows.
    pub diff: Option<DiffRowLayout>,
    /// Selection runs of the painted diff text (one per painted row segment) with their row,
    /// in item coordinates.
    pub selection: Vec<(u16, SelectionTextRun)>,
}

impl ItemRows {
    fn single(line: Line) -> Self {
        Self {
            lines: vec![line],
            height: 1,
            actions: Vec::new(),
            diff: None,
            selection: Vec::new(),
        }
    }

    fn lines(lines: Vec<Line>, actions: Vec<ActionCells<ItemAction>>) -> Self {
        Self {
            height: lines.len().max(1) as u64,
            lines,
            actions,
            diff: None,
            selection: Vec::new(),
        }
    }

    /// Cell of the gutter comment affordance: the first gutter cell of the item's first row,
    /// when the gutter shows there.
    #[must_use]
    pub fn comment_cell(&self) -> Option<u16> {
        let segment = self
            .diff
            .as_ref()?
            .segments()
            .iter()
            .find(|segment| segment.row == 0 && segment.content.start > segment.cells.start)?;
        u16::try_from(segment.cells.start).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CacheKey {
    key: ItemKey,
    width: u16,
    layout_revision: u64,
    paint_revision: u64,
    syntax_epoch: u64,
    annotation_revision: u64,
}

/// Bounded two-generation cache of item rows. Lookups promote entries from the previous
/// generation; when the current generation fills up it becomes the previous one.
#[derive(Debug, Default)]
pub struct RowCache {
    current: BTreeMap<CacheKey, Arc<ItemRows>>,
    previous: BTreeMap<CacheKey, Arc<ItemRows>>,
    built: usize,
}

impl RowCache {
    /// Rows built (cache misses) so far.
    #[must_use]
    pub const fn built(&self) -> usize {
        self.built
    }

    /// Entries held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.current.len() + self.previous.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.current.is_empty() && self.previous.is_empty()
    }

    fn get_or_build(&mut self, key: CacheKey, build: impl FnOnce() -> ItemRows) -> Arc<ItemRows> {
        if let Some(rows) = self.current.get(&key) {
            return Arc::clone(rows);
        }
        let rows = self.previous.remove(&key).unwrap_or_else(|| {
            self.built += 1;
            Arc::new(build())
        });
        if self.current.len() >= ROW_CACHE_CAPACITY {
            self.previous = std::mem::take(&mut self.current);
        }
        self.current.insert(key, Arc::clone(&rows));
        rows
    }
}

/// Everything rows are built from.
pub struct RowContext<'a> {
    /// The document.
    pub doc: &'a ReviewDocument,
    /// Presentation inputs.
    pub params: &'a ViewParams,
    /// Ready highlights.
    pub syntax: &'a SyntaxStore,
    /// Row cache.
    pub cache: &'a RefCell<RowCache>,
    /// Annotation bodies and AI exchanges.
    pub notes: &'a super::notes::ReviewNotes,
    /// Live AI answer states.
    pub agents: &'a super::agent::AgentStreams,
    /// Folded threads and expanded answers.
    pub folds: &'a Folds,
    /// The open composer.
    pub composer: Option<&'a super::composer::Composer>,
    /// Search matches to highlight.
    pub search: Option<&'a SearchHighlights>,
}

impl RowContext<'_> {
    /// Rows of the item at `index`, at `width` cells.
    #[must_use]
    pub fn rows(&self, index: usize, width: u16) -> Arc<ItemRows> {
        let Some(item) = self.doc.item(index) else {
            return Arc::new(ItemRows::single(Line::default()));
        };
        let key = CacheKey {
            key: item.key(),
            width,
            layout_revision: self.params.layout_revision,
            paint_revision: self.params.paint_revision,
            syntax_epoch: if is_diff_row(item) {
                self.syntax.epoch()
            } else {
                0
            },
            annotation_revision: if matches!(
                item,
                ReviewItem::Thread(_) | ReviewItem::AgentThread(_)
            ) {
                self.params.annotation_revision
            } else {
                0
            },
        };
        self.cache.borrow_mut().get_or_build(key, || {
            let mut rows = self.build(index, item, width);
            rows.selection = selection_runs(item, &rows, self.params.layout_revision);
            rows
        })
    }

    /// Height of the item at `index` at `width` without building its rows: exactly the
    /// height [`Self::rows`] produces (diff rows are measured by BMUX `diff_row_height`, the
    /// same geometry `layout_diff_row` uses).
    #[must_use]
    pub fn height(&self, index: usize, width: u16) -> Option<u64> {
        let styles = &self.params.theme;
        let height = match self.doc.item(index)? {
            ReviewItem::Line(line) => diff_row_height(
                DiffRowItem::Unified {
                    kind: diff_kind(line.kind),
                    old_line: line.old_line,
                    new_line: line.new_line,
                    text: DiffRowText::plain(content_text(&line.content, styles).0),
                },
                width,
                self.options(),
            ) as u64,
            ReviewItem::LinePair(pair) => diff_row_height(
                DiffRowItem::SideBySide {
                    old: pair.old.as_ref().map(|side| plain_cell(side, styles)),
                    new: pair.new.as_ref().map(|side| plain_cell(side, styles)),
                },
                width,
                self.options(),
            ) as u64,
            ReviewItem::Placeholder(placeholder) => u64::from(placeholder.estimated_rows.max(1)),
            ReviewItem::Thread(_) | ReviewItem::AgentThread(_) => self.rows(index, width).height,
            ReviewItem::Composer(_) => self.composer_layout(width)?.height,
            _ => 1,
        };
        Some(height)
    }

    /// Layout of the open composer at `width`.
    #[must_use]
    pub fn composer_layout(&self, width: u16) -> Option<super::composer::ComposerLayout> {
        let composer = self.composer?;
        Some(composer.layout(width, self.gutter_indent(width), &self.params.theme))
    }

    fn build(&self, index: usize, item: &ReviewItem, width: u16) -> ItemRows {
        let styles = &self.params.theme;
        let cells = usize::from(width);
        match item {
            ReviewItem::SourceHeader(item) => ItemRows::single(source_header(item, styles, cells)),
            ReviewItem::FileHeader(item) => ItemRows::single(file_header(item, styles, cells)),
            ReviewItem::Placeholder(item) => placeholder(item, styles, cells),
            ReviewItem::FileNotice(item) => ItemRows::single(file_notice(item, styles, cells)),
            ReviewItem::HunkHeader(item) => {
                ItemRows::single(hunk_header(item, styles, cells, self.gutter_indent(width)))
            }
            ReviewItem::Line(line) => self.unified_rows(index, line, width),
            ReviewItem::LinePair(pair) => self.pair_rows(pair, width),
            ReviewItem::Gap(gap) => gap_rows(gap, self.params, cells, self.gutter_indent(width)),
            ReviewItem::Thread(thread) => thread_rows(
                thread,
                self.notes,
                self.folds,
                styles,
                cells,
                self.gutter_indent(width),
            ),
            ReviewItem::AgentThread(agent) => agent_rows(
                agent,
                self.notes,
                self.agents,
                self.folds,
                styles,
                cells,
                self.gutter_indent(width),
            ),
            // The composer is laid out live (see `composer_layout`), never cached.
            ReviewItem::Composer(_) => ItemRows::single(Line::default()),
        }
    }

    /// Cells before diff content, so annotation and gap rows align with it (bounded to a third
    /// of the row on narrow widths).
    fn gutter_indent(&self, width: u16) -> usize {
        let gutter = match self.doc.layout() {
            LayoutMode::Unified => self.params.gutter.unified_width(),
            LayoutMode::SideBySide => self.params.gutter.side_width(),
        };
        gutter.min(usize::from(width) / 3)
    }

    const fn options(&self) -> DiffRowOptions {
        DiffRowOptions::new(self.params.gutter)
            .with_overflow(self.params.overflow)
            .with_tab_width(self.params.tab_width)
    }

    fn unified_rows(&self, index: usize, line: &LineItem, width: u16) -> ItemRows {
        let styles = &self.params.theme;
        let (text, mut syntax) = content_text(&line.content, styles);
        let kind = diff_kind(line.kind);
        if syntax.is_empty() {
            let side = if line.kind == LineKind::Removed {
                ReviewLineSide::Old
            } else if line.new_line.is_some() {
                ReviewLineSide::New
            } else {
                ReviewLineSide::Old
            };
            let number = match side {
                ReviewLineSide::Old => line.old_line,
                ReviewLineSide::New => line.new_line,
            };
            syntax = number.map_or_else(Vec::new, |number| {
                self.syntax_ranges(&line.file, side, number, text)
            });
        }
        let changed = self.unified_emphasis(index, line);
        let layout = layout_diff_row(
            DiffRowItem::Unified {
                kind,
                old_line: line.old_line,
                new_line: line.new_line,
                text: DiffRowText::plain(text)
                    .with_changed_ranges(&changed)
                    .with_syntax(&syntax),
            },
            width,
            self.options(),
            &styles.diff,
        );
        diff_item_rows(layout)
    }

    fn pair_rows(&self, pair: &LinePairItem, width: u16) -> ItemRows {
        let styles = &self.params.theme;
        let emphasis = if pair.emphasize {
            match (
                pair.old.as_ref().and_then(|side| side.content.text()),
                pair.new.as_ref().and_then(|side| side.content.text()),
            ) {
                (Some(old), Some(new)) => intraline_changed_ranges(old, new).unwrap_or_default(),
                _ => bmux_tui_components::diff_viewer::IntralineChanges::default(),
            }
        } else {
            bmux_tui_components::diff_viewer::IntralineChanges::default()
        };
        let old = pair
            .old
            .as_ref()
            .map(|side| self.pair_side(&pair.file, ReviewLineSide::Old, side));
        let new = pair
            .new
            .as_ref()
            .map(|side| self.pair_side(&pair.file, ReviewLineSide::New, side));
        let layout = layout_diff_row(
            DiffRowItem::SideBySide {
                old: old.as_ref().map(|side| pair_cell(side, &emphasis.old)),
                new: new.as_ref().map(|side| pair_cell(side, &emphasis.new)),
            },
            width,
            self.options(),
            &styles.diff,
        );
        diff_item_rows(layout)
    }

    fn pair_side<'s>(
        &self,
        file: &crate::v2::document::FileRef,
        side: ReviewLineSide,
        pair: &'s PairSide,
    ) -> PairCell<'s> {
        let (text, syntax) = content_text(&pair.content, &self.params.theme);
        let syntax = if syntax.is_empty() {
            self.syntax_ranges(file, side, pair.line, text)
        } else {
            syntax
        };
        (pair.kind, pair.line, text, syntax)
    }

    fn syntax_ranges(
        &self,
        file: &crate::v2::document::FileKey,
        side: ReviewLineSide,
        line: u32,
        text: &str,
    ) -> Vec<DiffSyntaxRange> {
        let Some(entry) = self.doc.entry(file) else {
            return Vec::new();
        };
        let (blob, path) = match side {
            ReviewLineSide::Old => (
                &entry.old_blob,
                entry.old_path.as_deref().unwrap_or(&entry.path),
            ),
            ReviewLineSide::New => (&entry.new_blob, entry.path.as_str()),
        };
        let Some(key) = self.syntax.key(blob, path) else {
            return Vec::new();
        };
        self.syntax
            .get(&key)
            .and_then(|highlight| highlight.line_matching(line, text))
            .map(|tokens| {
                tokens
                    .iter()
                    .map(|token| DiffSyntaxRange::new(token.start, token.end, token.style.to_tui()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Changed ranges of a unified removed or added line, paired positionally with the line at
    /// the same position in the adjacent run of the other kind (as side-by-side pairs them).
    fn unified_emphasis(&self, index: usize, line: &LineItem) -> Vec<ChangedRange> {
        let Some(partner) = self.unified_partner(index, line) else {
            return Vec::new();
        };
        let Some(ReviewItem::Line(other)) = self.doc.item(partner) else {
            return Vec::new();
        };
        let (Some(text), Some(other_text)) = (line.content.text(), other.content.text()) else {
            return Vec::new();
        };
        let (old, new) = if line.kind == LineKind::Removed {
            (text, other_text)
        } else {
            (other_text, text)
        };
        intraline_changed_ranges(old, new).map_or_else(Vec::new, |changes| {
            if line.kind == LineKind::Removed {
                changes.old
            } else {
                changes.new
            }
        })
    }

    fn unified_partner(&self, index: usize, line: &LineItem) -> Option<usize> {
        let is = |at: usize, wanted: LineKind| {
            matches!(self.doc.item(at), Some(ReviewItem::Line(other))
                if other.kind == wanted && other.file == line.file)
        };
        let run_start = |mut at: usize, wanted: LineKind| {
            let mut steps = 0;
            while at > 0 && steps < MAX_PAIRING_SCAN && is(at - 1, wanted) {
                at -= 1;
                steps += 1;
            }
            at
        };
        match line.kind {
            LineKind::Removed => {
                let position = index - run_start(index, LineKind::Removed);
                let mut end = index;
                let mut steps = 0;
                while steps < MAX_PAIRING_SCAN && is(end + 1, LineKind::Removed) {
                    end += 1;
                    steps += 1;
                }
                let partner = end + 1 + position;
                (position < MAX_PAIRING_SCAN
                    && (end + 1..=partner).all(|at| is(at, LineKind::Added)))
                .then_some(partner)
            }
            LineKind::Added => {
                let start = run_start(index, LineKind::Added);
                if start == 0 || !is(start - 1, LineKind::Removed) {
                    return None;
                }
                let partner = run_start(start - 1, LineKind::Removed) + (index - start);
                (partner < start).then_some(partner)
            }
            LineKind::Context | LineKind::Expanded => None,
        }
    }
}

/// One side of a pair without emphasis or syntax (geometry only).
fn plain_cell<'a>(side: &'a PairSide, styles: &ReviewTheme) -> DiffRowCell<'a> {
    DiffRowCell::new(
        diff_kind(side.kind),
        Some(side.line),
        DiffRowText::plain(content_text(&side.content, styles).0),
    )
}

/// One laid-out side of a side-by-side pair.
type PairCell<'s> = (LineKind, u32, &'s str, Vec<DiffSyntaxRange>);

fn pair_cell<'a>(side: &'a PairCell<'_>, changed: &'a [ChangedRange]) -> DiffRowCell<'a> {
    DiffRowCell::new(
        diff_kind(side.0),
        Some(side.1),
        DiffRowText::plain(side.2)
            .with_changed_ranges(changed)
            .with_syntax(&side.3),
    )
}

const fn is_diff_row(item: &ReviewItem) -> bool {
    matches!(item, ReviewItem::Line(_) | ReviewItem::LinePair(_))
}

const fn diff_kind(kind: LineKind) -> DiffLineKind {
    match kind {
        LineKind::Added => DiffLineKind::Added,
        LineKind::Removed => DiffLineKind::Removed,
        LineKind::Context | LineKind::Expanded => DiffLineKind::Context,
    }
}

/// Text of a line and, for loading or unavailable content, a style range covering it.
fn content_text<'a>(
    content: &'a LineContent,
    theme: &ReviewTheme,
) -> (&'a str, Vec<DiffSyntaxRange>) {
    match content {
        LineContent::Text(text) => (text, Vec::new()),
        LineContent::Loading => {
            let loading = theme.icons.ellipsis;
            (
                loading,
                vec![DiffSyntaxRange::new(0, loading.len(), theme.muted)],
            )
        }
        LineContent::Unavailable(reason) => (
            reason,
            vec![DiffSyntaxRange::new(0, reason.len(), theme.error)],
        ),
    }
}

fn diff_item_rows(layout: DiffRowLayout) -> ItemRows {
    let lines = layout.rows().to_vec();
    ItemRows {
        height: lines.len().max(1) as u64,
        lines,
        actions: Vec::new(),
        diff: Some(layout),
        selection: Vec::new(),
    }
}

/// A row of `width` cells: `content` with `trailing` against the right edge (content shortened
/// first), painted over `fill`.
fn header_line(content: &Line, trailing: &Line, width: usize, fill: Style) -> Line {
    let placed = content.with_trailing(
        trailing,
        width,
        TrailingPlacement::new(TrailingAlign::Right).keep_visible(true),
    );
    let placed = placed.truncate(width);
    let pad = width.saturating_sub(placed.width());
    let mut spans = placed.spans;
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    Line::from_spans(spans).with_fallback_style(fill)
}

/// `+added −removed` as styled spans (zero counts muted).
#[must_use]
pub fn count_spans(additions: u64, deletions: u64, theme: &ReviewTheme) -> Vec<Span> {
    let style = |count: u64, style: Style| if count == 0 { theme.muted } else { style };
    vec![
        Span::styled(format!("+{additions}"), style(additions, theme.additions)),
        Span::raw(" "),
        Span::styled(
            format!("{}{deletions}", theme.icons.minus),
            style(deletions, theme.deletions),
        ),
    ]
}

/// Cells of a change bar.
pub const CHANGE_BAR_CELLS: usize = 5;

/// A five-cell change bar: as many cells as the change is large (one cell up to 4 changed
/// lines, five from 200), split between added and removed lines in proportion.
#[must_use]
pub fn change_bar(additions: u64, deletions: u64, theme: &ReviewTheme) -> Vec<Span> {
    let total = additions.saturating_add(deletions);
    let filled: usize = match total {
        0 => 0,
        1..=4 => 1,
        5..=19 => 2,
        20..=49 => 3,
        50..=199 => 4,
        _ => CHANGE_BAR_CELLS,
    };
    // Round to the nearest cell, keeping at least one cell for a side that changed.
    let added = (additions.saturating_mul(filled as u64) + total / 2)
        .checked_div(total)
        .map_or(0, |share| {
            let share = usize::try_from(share).unwrap_or(filled).min(filled);
            if additions > 0 && share == 0 {
                1
            } else if deletions > 0 && share == filled && filled > 1 {
                filled - 1
            } else {
                share
            }
        });
    let icons = &theme.icons;
    let mut spans = Vec::with_capacity(3);
    if added > 0 {
        spans.push(Span::styled(
            icons.bar_filled.repeat(added),
            theme.bar_added,
        ));
    }
    if filled > added {
        spans.push(Span::styled(
            icons.bar_filled.repeat(filled - added),
            theme.bar_removed,
        ));
    }
    if filled < CHANGE_BAR_CELLS {
        spans.push(Span::styled(
            icons.bar_empty.repeat(CHANGE_BAR_CELLS - filled),
            theme.bar_empty,
        ));
    }
    spans
}

fn file_header(item: &FileHeaderItem, theme: &ReviewTheme, width: usize) -> Line {
    let icons = &theme.icons;
    let mut content = vec![
        Span::styled(icons.accent_bar, theme.accent),
        Span::styled(status_letter(item.status), theme.status(item.status)),
        Span::raw(" "),
    ];
    if let Some(old) = &item.old_path {
        content.push(Span::styled(old.clone(), theme.muted));
        content.push(Span::styled(icons.rename, theme.muted));
    }
    content.push(Span::styled(
        item.file.path.clone(),
        if item.viewed || item.carried_over {
            theme.path_viewed
        } else {
            theme.path
        },
    ));
    let mut trailing = Vec::new();
    if item.carried_over {
        trailing.push(Span::styled(
            "unchanged since last review",
            theme.badge_outdated,
        ));
        trailing.push(Span::raw(" "));
    }
    for (flag, label, style) in [
        (ReviewIndexFlag::Binary, "binary", theme.status_binary),
        (ReviewIndexFlag::ModeChanged, "mode", theme.muted),
        (ReviewIndexFlag::Symlink, "symlink", theme.muted),
        (ReviewIndexFlag::Submodule, "submodule", theme.muted),
    ] {
        if item.flags.contains(&flag) {
            trailing.push(Span::styled(label, style));
            trailing.push(Span::raw(" "));
        }
    }
    if !item.layers.is_empty() {
        let layers: Vec<String> = item
            .layers
            .iter()
            .map(|layer| format!("#{layer}"))
            .collect();
        trailing.push(Span::styled(layers.join(" "), theme.badge_open));
        trailing.push(Span::raw(" "));
    }
    if item.thread_count > 0 {
        trailing.push(Span::styled(
            format!("{}{}", icons.thread, item.thread_count),
            theme.badge_open,
        ));
        trailing.push(Span::raw(" "));
    }
    let (additions, deletions) = (u64::from(item.additions), u64::from(item.deletions));
    trailing.extend(count_spans(additions, deletions, theme));
    trailing.push(Span::raw(" "));
    trailing.extend(change_bar(additions, deletions, theme));
    trailing.push(Span::raw(" "));
    trailing.push(if item.viewed {
        Span::styled(icons.viewed, theme.viewed)
    } else {
        Span::styled(icons.unviewed, theme.muted)
    });
    trailing.push(Span::raw(" "));
    header_line(
        &Line::from_spans(content),
        &Line::from_spans(trailing),
        width,
        theme.file_header,
    )
}

fn source_header(item: &SourceHeaderItem, theme: &ReviewTheme, width: usize) -> Line {
    let separator = theme.icons.separator;
    let mut content = vec![Span::styled(item.label.clone(), theme.source_header)];
    if let Some(layer) = &item.layer {
        content.push(Span::styled(separator, theme.muted));
        content.push(Span::styled(format!("#{}", layer.number), theme.badge_open));
        content.push(Span::raw(" "));
        content.push(Span::styled(layer.title.clone(), theme.text));
        content.push(Span::styled(
            format!(" ({}/{})", layer.position, layer.count),
            theme.muted,
        ));
    }
    header_line(
        &Line::from_spans(content),
        &Line::from_spans(vec![
            Span::styled(
                format!(
                    "{} file{}",
                    item.file_count,
                    if item.file_count == 1 { "" } else { "s" }
                ),
                theme.muted,
            ),
            Span::styled(separator, theme.muted),
            Span::styled(
                format!("{} {} viewed", theme.icons.viewed, item.viewed_count),
                theme.viewed,
            ),
        ]),
        width,
        Style::new(),
    )
}

fn placeholder(item: &PlaceholderItem, theme: &ReviewTheme, width: usize) -> ItemRows {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(theme.icons.ellipsis, theme.muted),
    ];
    if item.loading {
        spans.push(Span::styled(" loading", theme.muted));
    }
    ItemRows {
        lines: vec![Line::from_spans(spans).truncate(width)],
        height: u64::from(item.estimated_rows.max(1)),
        actions: Vec::new(),
        diff: None,
        selection: Vec::new(),
    }
}

fn file_notice(item: &FileNoticeItem, theme: &ReviewTheme, width: usize) -> Line {
    let (text, style) = match item.kind {
        FileNoticeKind::Binary => ("  binary file not shown".to_owned(), theme.notice),
        FileNoticeKind::NoTextChanges => ("  no textual changes".to_owned(), theme.muted),
        FileNoticeKind::TooLarge => (
            item.truncation.as_ref().map_or_else(
                || "  diff too large; truncated".to_owned(),
                |truncation| {
                    format!(
                        "  diff too large; showing {} of {} lines",
                        truncation.shown_lines, truncation.total_lines
                    )
                },
            ),
            theme.notice,
        ),
        FileNoticeKind::Unavailable => (
            format!(
                "  unavailable: {}",
                item.reason.as_deref().unwrap_or("unknown error")
            ),
            theme.error,
        ),
    };
    Line::from_spans(vec![Span::styled(text, style)]).truncate(width)
}

fn hunk_header(item: &HunkHeaderItem, theme: &ReviewTheme, width: usize, indent: usize) -> Line {
    let mut content = vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(
            format!(
                "@@ -{},{} +{},{} @@",
                item.old_start, item.old_count, item.new_start, item.new_count
            ),
            theme.hunk,
        ),
    ];
    if let Some(heading) = &item.heading {
        content.push(Span::raw(" "));
        content.push(Span::styled(heading.to_string(), theme.text));
    }
    let trailing = if item.layers.is_empty() {
        Line::default()
    } else {
        let layers: Vec<String> = item
            .layers
            .iter()
            .map(|layer| format!("#{layer}"))
            .collect();
        Line::from_spans(vec![
            Span::styled(layers.join(" "), theme.badge_open),
            Span::raw(" "),
        ])
    };
    header_line(
        &Line::from_spans(content),
        &trailing,
        width,
        theme.hunk_fill,
    )
}

/// One labelled part of a gap row.
struct Part {
    text: String,
    action: Option<GapAction>,
}

/// The gap row variants from widest to narrowest; the first that fits is shown. The narrowest
/// has no interactive segments (activating the row reveals below).
fn gap_variants(gap: &GapItem, step: u32, theme: &ReviewTheme) -> Vec<Vec<Part>> {
    let icons = &theme.icons;
    let separator = icons.separator;
    let text = |text: String| Part { text, action: None };
    let act = |text: String, action| Part {
        text,
        action: Some(action),
    };
    let hidden = gap.hidden_lines();
    let threads = if gap.hidden_threads > 0 {
        format!("{separator}{}{}", icons.thread, gap.hidden_threads)
    } else {
        String::new()
    };
    let count_long = hidden.map_or_else(
        || "more lines".to_owned(),
        |hidden| format!("{hidden} hidden line{}", if hidden == 1 { "" } else { "s" }),
    );
    let count_short = hidden.map_or_else(|| icons.ellipsis.to_owned(), |hidden| hidden.to_string());
    let small = hidden.is_some_and(|hidden| hidden <= step);
    let open = gap.state == GapState::OpenEnded;
    let mut full = vec![text(format!("{} ", icons.gap_rule))];
    let mut compact = Vec::new();
    if small {
        full.push(act(format!("show {count_long}"), GapAction::All));
        compact.push(act(
            format!("{}{count_short}", icons.reveal_all),
            GapAction::All,
        ));
    } else {
        if !open {
            full.push(act(format!("{} {step}", icons.reveal_up), GapAction::Up));
            full.push(text(separator.to_owned()));
            compact.push(act(format!("{}{step}", icons.reveal_up), GapAction::Up));
            compact.push(text(" ".to_owned()));
        }
        full.push(text(count_long));
        full.push(text(separator.to_owned()));
        full.push(act(
            format!("{} {step}", icons.reveal_down),
            GapAction::Down,
        ));
        full.push(text(separator.to_owned()));
        full.push(act("all".to_owned(), GapAction::All));
        compact.push(text(count_short.clone()));
        compact.push(text(" ".to_owned()));
        compact.push(act(format!("{}{step}", icons.reveal_down), GapAction::Down));
        compact.push(text(" ".to_owned()));
        compact.push(act("all".to_owned(), GapAction::All));
    }
    full.push(text(format!("{threads} {}", icons.gap_rule)));
    vec![
        full,
        compact,
        vec![text(format!("{} {count_short}", icons.ellipsis))],
    ]
}

fn gap_rows(gap: &GapItem, params: &ViewParams, width: usize, indent: usize) -> ItemRows {
    let theme = &params.theme;
    let variants = gap_variants(gap, params.expand_step, theme);
    let fits = |parts: &[Part], indent: usize| {
        indent
            + parts
                .iter()
                .map(|part| display_width(&part.text))
                .sum::<usize>()
            <= width
    };
    // Prefer the aligned indent; drop it before degrading the label.
    let (parts, indent) = variants
        .iter()
        .find_map(|parts| {
            if fits(parts, indent) {
                Some((parts, indent))
            } else {
                fits(parts, 0).then_some((parts, 0))
            }
        })
        .unwrap_or_else(|| (variants.last().expect("gap variants"), 0));
    let mut spans = vec![Span::raw(" ".repeat(indent))];
    let mut actions = Vec::new();
    let mut x = indent;
    for part in parts {
        let cells = display_width(&part.text);
        let style = if part.action.is_some() {
            theme.gap_action
        } else {
            theme.gap
        };
        spans.push(Span::styled(part.text.clone(), style));
        if let Some(action) = part.action
            && x + cells <= width
        {
            actions.push(ActionCells {
                row: 0,
                cells: u16::try_from(x).unwrap_or(u16::MAX)
                    ..u16::try_from(x + cells).unwrap_or(u16::MAX),
                action: ItemAction::Gap(action),
            });
        }
        x += cells;
    }
    ItemRows {
        lines: vec![Line::from_spans(spans).truncate(width)],
        height: 1,
        actions,
        diff: None,
        selection: Vec::new(),
    }
}

/// Selection runs of a diff item's painted text, in item coordinates: one run per painted row
/// segment, with the units of the row layout's own grapheme placement (so tabs, wide graphemes,
/// and zero-width characters map exactly as painted). The order is the side; the item's
/// position is added when painting (indices move).
fn selection_runs(
    item: &ReviewItem,
    rows: &ItemRows,
    revision: u64,
) -> Vec<(u16, SelectionTextRun)> {
    let Some(layout) = rows.diff.as_ref() else {
        return Vec::new();
    };
    let key = RowKey(item.key()).to_string();
    let content = [format!("{key}.0"), format!("{key}.1")];
    let mut runs = Vec::new();
    for segment in layout.segments() {
        let Ok(row) = u16::try_from(segment.row) else {
            continue;
        };
        let cells = layout.selection_cells(segment);
        if cells.is_empty() {
            continue;
        }
        let side = match segment.side {
            DiffSourceSide::Old => 0,
            DiffSourceSide::New => 1,
        };
        runs.push((
            row,
            SelectionTextRun::new(
                SELECTION_SCOPE,
                content[usize::from(side == 1)].as_str(),
                bmux_tui::geometry::Point::new(0, row),
                side,
                cells.into(),
            )
            .revision(revision),
        ));
    }
    runs
}

/// Selection scope of the document's diff text.
pub const SELECTION_SCOPE: &str = "review.document";

/// Comment lines shown per comment before eliding the rest.
const MAX_COMMENT_ROWS: usize = 12;
/// Answer lines shown per AI answer.
const MAX_ANSWER_ROWS: usize = 8;
/// Answer lines shown per expanded AI answer.
const MAX_EXPANDED_ANSWER_ROWS: usize = 400;

/// `lines` with `prefix` cells of padding before each, clipped to `width`.
fn indented(lines: Vec<Line>, prefix: &str, width: usize) -> Vec<Line> {
    lines
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::raw(prefix.to_owned())];
            spans.extend(line.spans);
            Line::from_spans(spans).truncate(width)
        })
        .collect()
}

/// Markdown rendered at `width` cells, at most `max` rows (an ellipsis row marks elided rows).
fn markdown(text: &str, width: usize, max: usize, theme: &ReviewTheme) -> Vec<Line> {
    let width = u16::try_from(width.max(1)).unwrap_or(u16::MAX);
    let mut lines = bcode_markdown_render::render_markdown(
        text,
        &bcode_markdown_render::MarkdownRenderOptions::new(width),
    )
    .lines;
    if lines.len() > max {
        lines.truncate(max.saturating_sub(1));
        lines.push(Line::from_spans(vec![Span::styled(
            theme.icons.ellipsis,
            theme.muted,
        )]));
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

/// Action rows: labelled buttons placed left to right after `indent`, wrapping onto further
/// rows (starting at item row `row`) when the next one does not fit. Each button is kept whole;
/// one wider than a whole row is dropped (with every later one).
fn action_row(
    buttons: &[(&str, ItemAction)],
    row: u16,
    indent: usize,
    width: usize,
    theme: &ReviewTheme,
) -> (Vec<Line>, Vec<ActionCells<ItemAction>>) {
    let indent = indent.min(width);
    let separator = display_width(theme.icons.separator);
    let mut lines = Vec::new();
    let mut spans = vec![Span::raw(" ".repeat(indent))];
    let mut actions = Vec::new();
    let mut x = indent;
    let mut first = true;
    for (label, action) in buttons {
        let cells = display_width(label);
        if indent + cells > width {
            break;
        }
        let gap = if first { 0 } else { separator };
        if x + gap + cells > width {
            lines.push(Line::from_spans(std::mem::take(&mut spans)).truncate(width));
            spans.push(Span::raw(" ".repeat(indent)));
            x = indent;
        } else if gap > 0 {
            spans.push(Span::styled(theme.icons.separator, theme.muted));
            x += gap;
        }
        first = false;
        spans.push(Span::styled((*label).to_owned(), theme.gap_action));
        actions.push(ActionCells {
            row: row.saturating_add(u16::try_from(lines.len()).unwrap_or(u16::MAX)),
            cells: u16::try_from(x).unwrap_or(u16::MAX)
                ..u16::try_from(x + cells).unwrap_or(u16::MAX),
            action: *action,
        });
        x += cells;
    }
    lines.push(Line::from_spans(spans).truncate(width));
    (lines, actions)
}

const fn outdated_label(reason: crate::v2::document::OutdatedReason) -> &'static str {
    use crate::v2::document::OutdatedReason;
    match reason {
        OutdatedReason::FileMissing => "outdated: file not in review",
        OutdatedReason::LineMissing => "outdated: line removed",
        OutdatedReason::LineChanged => "outdated: line changed",
        OutdatedReason::RevisionChanged => "outdated: older revision",
        OutdatedReason::ContentChanged => "outdated: file changed",
    }
}

/// Header spans of a thread: fold marker, comment count, kind, severity, state, and badges.
fn thread_header(
    thread: &crate::v2::document::ThreadItem,
    notes: &super::notes::ReviewNotes,
    comments: &[&bcode_code_review_models::DraftComment],
    collapsed: bool,
    theme: &ReviewTheme,
    indent: usize,
) -> Vec<Span> {
    let icons = &theme.icons;
    let saving = comments
        .iter()
        .any(|comment| super::notes::is_local(&comment.comment_id));
    let mut header = vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(
            if collapsed {
                icons.collapsed
            } else {
                icons.menu
            },
            theme.muted,
        ),
        Span::raw(" "),
        Span::styled(icons.thread, theme.thread),
        Span::raw(" "),
        Span::styled(
            format!(
                "{} comment{}",
                comments.len(),
                if comments.len() == 1 { "" } else { "s" }
            ),
            theme.thread,
        ),
        Span::styled(icons.separator, theme.muted),
        Span::styled(super::notes::kind_label(thread.kind), theme.muted),
        Span::styled(icons.separator, theme.muted),
        Span::styled(super::notes::severity_label(thread.severity), theme.muted),
        Span::raw(" "),
        if thread.resolved {
            Span::styled("resolved", theme.badge_resolved)
        } else {
            Span::styled("open", theme.badge_open)
        },
    ];
    if let crate::v2::document::AnchorPlacement::Outdated(reason) = thread.placement {
        header.push(Span::raw(" "));
        header.push(Span::styled(outdated_label(reason), theme.badge_outdated));
    }
    if saving {
        header.push(Span::raw(" "));
        header.push(Span::styled("saving…", theme.badge_draft));
    }
    if notes.is_published(&thread.thread_id) {
        header.push(Span::raw(" "));
        header.push(Span::styled("published", theme.badge_resolved));
    }
    if let Some((_, start, end)) = thread.lines {
        header.push(Span::styled(icons.separator, theme.muted));
        header.push(Span::styled(
            if start == end {
                format!("L{end}")
            } else {
                format!("L{start}-{end}")
            },
            theme.muted,
        ));
    }
    header
}

/// Rows of a thread: a header (a click folds it), its comments (markdown), its suggestions,
/// and its actions. A collapsed thread is its header only.
fn thread_rows(
    thread: &crate::v2::document::ThreadItem,
    notes: &super::notes::ReviewNotes,
    folds: &Folds,
    theme: &ReviewTheme,
    width: usize,
    indent: usize,
) -> ItemRows {
    if let Some(external) = notes.external(&thread.thread_id) {
        return external_thread_rows(thread, external, notes, theme, width, indent);
    }
    let comments = notes.thread(&thread.thread_id);
    let collapsed = folds.collapsed.contains(&thread.thread_id);
    let header = thread_header(thread, notes, &comments, collapsed, theme, indent);
    let mut lines = vec![Line::from_spans(header).truncate(width)];
    let fold = header_fold(indent, width);
    if collapsed {
        return ItemRows::lines(lines, fold.into_iter().collect());
    }
    let body_indent = indent + 2;
    let body_width = width.saturating_sub(body_indent);
    if !thread.resolved {
        for (index, comment) in comments.iter().enumerate() {
            let rows = markdown(&comment.body, body_width, MAX_COMMENT_ROWS, theme);
            let mut rows = indented(rows, &" ".repeat(body_indent), width);
            if index > 0
                && let Some(first) = rows.first_mut()
                && body_indent >= 2
            {
                let mut spans = vec![
                    Span::raw(" ".repeat(body_indent - 2)),
                    Span::styled("↳ ", theme.muted),
                ];
                spans.extend(first.spans.iter().skip(1).cloned());
                *first = Line::from_spans(spans).truncate(width);
            }
            lines.extend(rows);
        }
    }
    let mut actions: Vec<ActionCells<ItemAction>> = fold.into_iter().collect();
    let (suggestion_lines, suggestion_actions) = suggestion_rows(
        &thread.suggestion_ids,
        notes,
        theme,
        body_indent,
        width,
        u16::try_from(lines.len()).unwrap_or(u16::MAX),
        |action| {
            ItemAction::Thread(match action {
                SuggestionAction::Accept => ThreadAction::Accept,
                SuggestionAction::Refine => ThreadAction::Refine,
                SuggestionAction::Reject => ThreadAction::Reject,
            })
        },
    );
    lines.extend(suggestion_lines);
    actions.extend(suggestion_actions);
    let buttons: Vec<(&str, ItemAction)> = if thread.resolved {
        vec![
            ("Reopen", ItemAction::Thread(ThreadAction::Resolve)),
            ("Reply", ItemAction::Thread(ThreadAction::Reply)),
        ]
    } else {
        vec![
            ("Reply", ItemAction::Thread(ThreadAction::Reply)),
            ("Edit", ItemAction::Thread(ThreadAction::Edit)),
            ("Delete", ItemAction::Thread(ThreadAction::Delete)),
            ("Resolve", ItemAction::Thread(ThreadAction::Resolve)),
            ("Ask Bcode", ItemAction::Thread(ThreadAction::Ask)),
        ]
    };
    let row = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let (rows, placed) = action_row(&buttons, row, body_indent.min(width), width, theme);
    lines.extend(rows);
    actions.extend(placed);
    ItemRows::lines(lines, actions)
}

/// The fold action over a header row's cells after the indent.
fn header_fold(indent: usize, width: usize) -> Option<ActionCells<ItemAction>> {
    (width > indent).then(|| ActionCells {
        row: 0,
        cells: u16::try_from(indent).unwrap_or(u16::MAX)..u16::try_from(width).unwrap_or(u16::MAX),
        action: ItemAction::Thread(ThreadAction::Fold),
    })
}

/// An action on the latest pending suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuggestionAction {
    Accept,
    Refine,
    Reject,
}

/// Rows of suggestions: one per suggestion (status and first line), then Accept, Refine, and
/// Reject buttons for the latest pending one. Rows start at `first_row` of the item.
fn suggestion_rows(
    ids: &[String],
    notes: &super::notes::ReviewNotes,
    theme: &ReviewTheme,
    indent: usize,
    width: usize,
    first_row: u16,
    action: impl Fn(SuggestionAction) -> ItemAction,
) -> (Vec<Line>, Vec<ActionCells<ItemAction>>) {
    use bcode_code_review_models::ReviewSuggestionStatus;
    let pad = " ".repeat(indent.min(width));
    let mut lines = Vec::new();
    let suggestions: Vec<_> = ids.iter().filter_map(|id| notes.suggestion(id)).collect();
    for suggestion in &suggestions {
        let (label, style) = match suggestion.status {
            ReviewSuggestionStatus::Suggested => ("suggested ", theme.badge_ai),
            ReviewSuggestionStatus::Refining => ("refining… ", theme.badge_draft),
            ReviewSuggestionStatus::Accepted => ("accepted ", theme.badge_resolved),
            ReviewSuggestionStatus::Rejected => ("rejected ", theme.badge_outdated),
        };
        lines.push(
            Line::from_spans(vec![
                Span::raw(pad.clone()),
                Span::styled(label, style),
                Span::styled(
                    suggestion
                        .body
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_owned(),
                    theme.text,
                ),
            ])
            .truncate(width),
        );
    }
    let pending = suggestions
        .iter()
        .any(|suggestion| suggestion.status == ReviewSuggestionStatus::Suggested);
    let actions = if pending {
        let row = first_row.saturating_add(u16::try_from(lines.len()).unwrap_or(u16::MAX));
        let buttons = [
            ("Accept", action(SuggestionAction::Accept)),
            ("Refine", action(SuggestionAction::Refine)),
            ("Reject", action(SuggestionAction::Reject)),
        ];
        let (rows, placed) = action_row(&buttons, row, indent.min(width), width, theme);
        lines.extend(rows);
        placed
    } else {
        Vec::new()
    };
    (lines, actions)
}

/// Rows of an imported (read-only) thread: a header with the provider, remote state, and
/// mapping, then each comment with its author. Imported threads have no actions.
fn external_thread_rows(
    thread: &crate::v2::document::ThreadItem,
    external: &super::notes::ExternalThread,
    notes: &super::notes::ReviewNotes,
    theme: &ReviewTheme,
    width: usize,
    indent: usize,
) -> ItemRows {
    use bcode_code_review_models::{ExternalAnchorMappingStatus, ExternalReviewThreadState};
    let icons = &theme.icons;
    let comments = notes.thread(&thread.thread_id);
    let mut header = vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(icons.thread, theme.thread),
        Span::raw(" "),
        Span::styled("imported", theme.badge_ai),
        Span::styled(icons.separator, theme.muted),
        Span::styled(external.provider_id.clone(), theme.muted),
        Span::styled(icons.separator, theme.muted),
        Span::styled(
            format!(
                "{} comment{}",
                comments.len(),
                if comments.len() == 1 { "" } else { "s" }
            ),
            theme.thread,
        ),
        Span::raw(" "),
        match external.state {
            ExternalReviewThreadState::Open => Span::styled("open", theme.badge_open),
            ExternalReviewThreadState::Resolved => Span::styled("resolved", theme.badge_resolved),
            ExternalReviewThreadState::Outdated => Span::styled("outdated", theme.badge_outdated),
        },
    ];
    match external.mapping {
        ExternalAnchorMappingStatus::Exact => {}
        ExternalAnchorMappingStatus::Approximate => {
            header.push(Span::raw(" "));
            header.push(Span::styled("approximate", theme.badge_outdated));
        }
        ExternalAnchorMappingStatus::Unmappable => {
            header.push(Span::raw(" "));
            header.push(Span::styled("not in this review", theme.badge_outdated));
        }
    }
    if let crate::v2::document::AnchorPlacement::Outdated(reason) = thread.placement {
        header.push(Span::raw(" "));
        header.push(Span::styled(outdated_label(reason), theme.badge_outdated));
    }
    if let Some((_, start, end)) = thread.lines {
        header.push(Span::styled(icons.separator, theme.muted));
        header.push(Span::styled(
            if start == end {
                format!("L{end}")
            } else {
                format!("L{start}-{end}")
            },
            theme.muted,
        ));
    }
    let mut lines = vec![Line::from_spans(header).truncate(width)];
    let body_indent = indent + 2;
    let body_width = width.saturating_sub(body_indent);
    if let Some(message) = &external.message {
        lines.push(
            Line::from_spans(vec![
                Span::raw(" ".repeat(body_indent.min(width))),
                Span::styled(message.clone(), theme.muted),
            ])
            .truncate(width),
        );
    }
    if external.state != ExternalReviewThreadState::Resolved {
        for comment in &comments {
            if let Some(author) = external.authors.get(&comment.comment_id) {
                lines.push(
                    Line::from_spans(vec![
                        Span::raw(" ".repeat(body_indent.min(width))),
                        Span::styled(author.clone(), theme.title),
                    ])
                    .truncate(width),
                );
            }
            let rows = markdown(&comment.body, body_width, MAX_COMMENT_ROWS, theme);
            lines.extend(indented(rows, &" ".repeat(body_indent), width));
        }
    }
    ItemRows::lines(lines, Vec::new())
}

/// Header of an AI answer: icon, live status, and an outdated label. Returns whether the
/// question or its stream failed.
fn agent_header(
    agent: &crate::v2::document::AgentThreadItem,
    status: Option<bcode_code_review_models::ReviewAiExchangeStatus>,
    live: Option<&super::agent::AgentState>,
    theme: &ReviewTheme,
    indent: usize,
) -> (Vec<Span>, bool) {
    use super::agent::AgentPhase;
    use bcode_code_review_models::ReviewAiExchangeStatus;
    let icons = &theme.icons;
    let (status, status_style, failed) = match (status, live) {
        (Some(ReviewAiExchangeStatus::Failed), _) => ("failed", theme.error, true),
        (Some(ReviewAiExchangeStatus::Pending), _) => ("asking…", theme.badge_draft, false),
        (_, Some(state)) => match state.phase {
            AgentPhase::Waiting => ("connecting…", theme.muted, false),
            AgentPhase::Working => ("working…", theme.badge_draft, false),
            AgentPhase::Answered => ("answered", theme.badge_resolved, false),
            AgentPhase::Failed => ("stream stopped", theme.error, true),
        },
        _ => ("suggestion", theme.badge_ai, false),
    };
    let mut header = vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(icons.ai, theme.badge_ai),
        Span::raw(" "),
        Span::styled("Bcode", theme.badge_ai),
        Span::raw(" "),
        Span::styled(status, status_style),
    ];
    if let crate::v2::document::AnchorPlacement::Outdated(reason) = agent.placement {
        header.push(Span::raw(" "));
        header.push(Span::styled(outdated_label(reason), theme.badge_outdated));
    }
    (header, failed)
}

/// Rows of an AI answer: a header with live status, the question, the answer preview (or the
/// status), suggestions, and actions.
#[allow(clippy::too_many_lines)]
fn agent_rows(
    agent: &crate::v2::document::AgentThreadItem,
    notes: &super::notes::ReviewNotes,
    agents: &super::agent::AgentStreams,
    folds: &Folds,
    theme: &ReviewTheme,
    width: usize,
    indent: usize,
) -> ItemRows {
    let exchange = agent
        .exchange_id
        .as_deref()
        .and_then(|id| notes.exchange(id));
    let session_state = exchange
        .and_then(|exchange| exchange.session_id.as_deref())
        .and_then(|session| agents.state(session));
    let (header, failed) = agent_header(
        agent,
        exchange.map(|exchange| exchange.status),
        session_state,
        theme,
        indent,
    );
    let mut lines = vec![Line::from_spans(header).truncate(width)];
    let body_indent = indent + 2;
    let body_width = width.saturating_sub(body_indent);
    let pad = " ".repeat(body_indent);
    if let Some(exchange) = exchange {
        let question = exchange.question.lines().next().unwrap_or_default();
        lines.push(
            Line::from_spans(vec![
                Span::raw(pad.clone()),
                Span::styled("Q ", theme.badge_ai),
                Span::styled(question.to_owned(), theme.text),
            ])
            .truncate(width),
        );
        if let Some(error) = &exchange.error {
            lines.push(
                Line::from_spans(vec![
                    Span::raw(pad.clone()),
                    Span::styled(error.clone(), theme.error),
                ])
                .truncate(width),
            );
        }
    }
    let expanded = agent
        .exchange_id
        .as_ref()
        .is_some_and(|id| folds.expanded.contains(id));
    let answered = session_state.is_some_and(|state| !state.answer.trim().is_empty());
    match session_state {
        Some(state) if !state.answer.is_empty() => {
            let max = if expanded {
                MAX_EXPANDED_ANSWER_ROWS
            } else {
                MAX_ANSWER_ROWS
            };
            lines.extend(indented(
                markdown(&state.answer, body_width, max, theme),
                &pad,
                width,
            ));
        }
        Some(state) if !state.status.is_empty() => lines.push(
            Line::from_spans(vec![
                Span::raw(pad),
                Span::styled(state.status.clone(), theme.muted),
            ])
            .truncate(width),
        ),
        _ => {}
    }
    let (suggestion_lines, mut actions) = suggestion_rows(
        &agent.suggestion_ids,
        notes,
        theme,
        body_indent,
        width,
        u16::try_from(lines.len()).unwrap_or(u16::MAX),
        |action| {
            ItemAction::Agent(match action {
                SuggestionAction::Accept => AgentAction::Accept,
                SuggestionAction::Refine => AgentAction::Refine,
                SuggestionAction::Reject => AgentAction::Reject,
            })
        },
    );
    lines.extend(suggestion_lines);
    let session = exchange.and_then(|exchange| exchange.session_id.as_ref());
    let mut buttons = Vec::new();
    if session.is_some() {
        buttons.push(("Open session", ItemAction::Agent(AgentAction::Open)));
        buttons.push(("Follow up", ItemAction::Agent(AgentAction::FollowUp)));
    }
    if answered {
        buttons.push((
            if expanded { "Less" } else { "More" },
            ItemAction::Agent(AgentAction::Expand),
        ));
        buttons.push(("Copy", ItemAction::Agent(AgentAction::Copy)));
        buttons.push(("Suggest comment", ItemAction::Agent(AgentAction::Suggest)));
        buttons.push(("Draft", ItemAction::Agent(AgentAction::Draft)));
    }
    if failed && session.is_some() {
        buttons.push(("Retry", ItemAction::Agent(AgentAction::Retry)));
    }
    if exchange.is_some() {
        buttons.push(("Delete", ItemAction::Agent(AgentAction::Delete)));
    }
    if !buttons.is_empty() {
        let row = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let (rows, placed_actions) =
            action_row(&buttons, row, body_indent.min(width), width, theme);
        lines.extend(rows);
        actions.extend(placed_actions);
    }
    ItemRows::lines(lines, actions)
}

/// Component of one document item.
pub struct ItemView<'a> {
    id: LayoutId,
    key: ItemKey,
    index: usize,
    context: &'a RowContext<'a>,
    interaction: &'a Interaction,
    flags: ItemFlags,
}

impl<'a> ItemView<'a> {
    /// Component of the item at `index`.
    #[must_use]
    pub const fn new(
        id: LayoutId,
        key: ItemKey,
        index: usize,
        context: &'a RowContext<'a>,
        interaction: &'a Interaction,
        flags: ItemFlags,
    ) -> Self {
        Self {
            id,
            key,
            index,
            context,
            interaction,
            flags,
        }
    }

    fn rows(&self, width: u64) -> Arc<ItemRows> {
        self.context
            .rows(self.index, u16::try_from(width).unwrap_or(u16::MAX))
    }

    const fn is_composer(&self) -> bool {
        matches!(self.key, ItemKey::Composer)
    }

    fn paint_composer(&self, width: u16, height: u16, cx: &mut PaintCx<'_, '_>) {
        let (Some(composer), Some(layout)) =
            (self.context.composer, self.context.composer_layout(width))
        else {
            return;
        };
        let theme = &self.context.params.theme;
        cx.write_line(LocalRect::new(0, 0, width, 1), &layout.title);
        let inner = width.saturating_sub(layout.indent);
        if composer.preview {
            for (row, line) in (1_u16..).zip(&layout.preview) {
                if row >= height {
                    break;
                }
                cx.write_line(LocalRect::new(0, i64::from(row), width, 1), line);
            }
        } else if inner > 0 && layout.editor_rows > 0 {
            let editor = composer.editor(theme);
            let size = bmux_tui::geometry::Size::new(inner, layout.editor_rows);
            let node = editor.layout(Constraints::tight(size), &mut LayoutCx::new());
            let area = LocalRect::new(0, 0, inner, layout.editor_rows);
            let mut placed = (Rect::default(), Rect::default());
            cx.with_child(i32::from(layout.indent), 1, area, |cx| {
                editor.paint(&node, cx);
                let content = cx.project_raster_rect(area).unwrap_or_default();
                let caret = cx
                    .cursor()
                    .filter(|cursor| cursor.visible)
                    .map_or_else(Rect::default, |cursor| {
                        Rect::new(cursor.position.x, cursor.position.y, 1, 1)
                    });
                placed = (content, caret);
            });
            composer.painted(placed.0, placed.1);
        }
        let last = height.saturating_sub(1);
        cx.write_line(
            LocalRect::new(0, i64::from(last), width, 1),
            &layout.actions,
        );
        for (number, button) in layout.buttons.iter().enumerate() {
            let cells = button
                .cells
                .end
                .min(width)
                .saturating_sub(button.cells.start);
            if cells > 0 && last > 0 {
                cx.push_hit(HitRegion::new(
                    format!("{}.composer.{number}", self.id.as_str()),
                    Rect::new(button.cells.start, last, cells, 1),
                ));
            }
        }
        cx.push_damage(LocalRect::new(0, 0, width, height));
    }

    /// Register the painted diff text in the document's selection scope as one BMUX text run
    /// per painted row segment, so a selection controller copies exactly the source text under
    /// the selected cells. Runs materialize their per-grapheme fragments only when the scene is
    /// queried, so registering costs one value per visible row.
    fn register_selection(&self, rows: &ItemRows, height: u16, cx: &mut PaintCx<'_, '_>) {
        let base = (self.index as u64).saturating_mul(2);
        for (_, run) in rows.selection.iter().filter(|(row, _)| *row < height) {
            let mut run = run.clone();
            run.order += base;
            cx.push_selection_text_run(run);
        }
    }

    /// Restyle search matches on the item's diff rows.
    fn paint_matches(&self, rows: &ItemRows, width: u16, cx: &mut PaintCx<'_, '_>) {
        let (Some(search), Some(layout), Some(item)) = (
            self.context.search,
            rows.diff.as_ref(),
            self.context.doc.item(self.index),
        ) else {
            return;
        };
        let Some(file) = item.file() else {
            return;
        };
        let theme = &self.context.params.theme;
        let mut sides: Vec<(ReviewLineSide, u32, DiffSourceSide)> = Vec::new();
        match item {
            ReviewItem::Line(line) => {
                let (side, number) = line.key_side();
                let Some(source) = layout.columns().first().map(|column| column.side) else {
                    return;
                };
                sides.push((side, number, source));
            }
            ReviewItem::LinePair(pair) => {
                if let Some(old) = &pair.old {
                    sides.push((ReviewLineSide::Old, old.line, DiffSourceSide::Old));
                }
                if let Some(new) = &pair.new {
                    sides.push((ReviewLineSide::New, new.line, DiffSourceSide::New));
                }
            }
            _ => return,
        }
        for (side, number, source) in sides {
            let key = (file.clone(), side, number);
            let Some(ranges) = search.lines.get(&key) else {
                continue;
            };
            let style = if search.current.as_ref() == Some(&key) {
                theme.search_current
            } else {
                theme.search_match
            };
            for (start, end) in ranges {
                for covered in layout.source_cells(source, *start..*end) {
                    let x = u16::try_from(covered.cells.start).unwrap_or(u16::MAX);
                    let cells = u16::try_from(covered.cells.len()).unwrap_or(0);
                    if x < width && cells > 0 {
                        cx.restyle(
                            LocalRect::new(
                                i32::from(x),
                                i64::try_from(covered.row).unwrap_or(0),
                                cells.min(width - x),
                                1,
                            ),
                            style,
                        );
                    }
                }
            }
        }
    }
}

impl Component for ItemView<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = constraints.max_width().min(u64::from(u16::MAX));
        let height = if self.is_composer() {
            self.context
                .composer_layout(u16::try_from(width).unwrap_or(u16::MAX))
                .map_or(1, |layout| layout.height)
        } else {
            self.rows(width).height
        };
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, height)),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        if width == 0 || layout.size.height == 0 {
            return;
        }
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        if self.is_composer() {
            self.paint_composer(width, height, cx);
            return;
        }
        let theme = &self.context.params.theme;
        let rows = self.rows(layout.size.width);
        for (row, line) in (0_u16..height).zip(rows.lines.iter()) {
            cx.write_line(LocalRect::new(0, i64::from(row), width, 1), line);
        }
        if self.flags.selected {
            cx.restyle(LocalRect::new(0, 0, width, height), theme.selection);
        }
        if self.flags.cursor {
            cx.restyle(LocalRect::new(0, 0, width, height), theme.cursor);
        }
        self.paint_matches(&rows, width, cx);
        self.register_selection(&rows, height, cx);
        let comment = self
            .flags
            .hovered
            .then(|| rows.comment_cell())
            .flatten()
            .filter(|x| *x < width);
        if let Some(x) = comment {
            cx.write_line(
                LocalRect::new(i32::from(x), 0, 1, 1),
                &Line::from_spans(vec![Span::styled("+", theme.accent.patch(theme.selection))]),
            );
            cx.push_hit(HitRegion::new(
                format!("{}.{}.comment", self.id.as_str(), RowKey(self.key.clone())),
                Rect::new(x, 0, 1, 1),
            ));
        }
        for (number, action) in rows.actions.iter().enumerate() {
            let cells = action
                .cells
                .end
                .min(width)
                .saturating_sub(action.cells.start);
            if cells > 0 && action.row < height {
                cx.push_hit(HitRegion::new(
                    format!(
                        "{}.{}.action.{number}",
                        self.id.as_str(),
                        RowKey(self.key.clone())
                    ),
                    Rect::new(action.cells.start, action.row, cells, 1),
                ));
            }
        }
        cx.push_damage(LocalRect::new(0, 0, width, height));
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Event::Mouse(mouse) = event else {
            return EventOutcome::Ignored;
        };
        let kind = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => PointerKind::Down,
            MouseEventKind::Up(MouseButton::Left) => PointerKind::Up,
            MouseEventKind::Drag(MouseButton::Left) => PointerKind::Drag,
            MouseEventKind::Move => PointerKind::Moved,
            _ => return EventOutcome::Ignored,
        };
        let (x, y) = cx.local_point(mouse.position);
        let (Ok(x), Ok(y)) = (u16::try_from(x), u16::try_from(y)) else {
            return EventOutcome::Ignored;
        };
        let press = matches!(kind, PointerKind::Down | PointerKind::Up);
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        if self.is_composer() {
            let Some(composer) = self.context.composer_layout(width) else {
                return EventOutcome::Ignored;
            };
            let last = u16::try_from(composer.height.saturating_sub(1)).unwrap_or(0);
            if let Some(button) = composer
                .buttons
                .iter()
                .find(|button| y == last && button.cells.contains(&x))
                && kind == PointerKind::Down
            {
                self.interaction.clicks.borrow_mut().push(ItemClick {
                    key: self.key.clone(),
                    action: ItemAction::Composer(button.action),
                });
            }
            // The composer owns its rows: no list activation or drag passes through it.
            return if press {
                EventOutcome::Handled
            } else {
                EventOutcome::Ignored
            };
        }
        let rows = self.rows(layout.size.width);
        let comment = (self.flags.hovered && y == 0 && rows.comment_cell() == Some(x))
            .then_some(ItemAction::Comment);
        let action = comment.or_else(|| {
            rows.actions
                .iter()
                .find(|action| action.row == y && action.cells.contains(&x))
                .map(|action| action.action)
        });
        if let Some(action) = action
            && press
        {
            if kind == PointerKind::Down {
                self.interaction.clicks.borrow_mut().push(ItemClick {
                    key: self.key.clone(),
                    action,
                });
            }
            return EventOutcome::Handled;
        }
        self.interaction.pointer.borrow_mut().push(PointerHit {
            key: self.key.clone(),
            kind,
            diff: rows.diff.is_some(),
        });
        EventOutcome::Ignored
    }

    fn revision(&self) -> ComponentRevision {
        let content = if self.is_composer() {
            self.context
                .composer
                .map_or(0, super::composer::Composer::revision)
        } else {
            self.context.params.annotation_revision
        };
        ComponentRevision::new(
            self.context.params.layout_revision ^ content.wrapping_mul(0x9e37_79b9),
            self.context.params.paint_revision.wrapping_mul(8) ^ self.flags.bits(),
        )
    }
}
