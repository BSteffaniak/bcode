//! The review screen: loading pipeline, review chrome and panels, the virtualized document
//! list, the file panel, input, and refresh.
//!
//! * **Loading.** The first `poll` spawns the open task (provider routing, source resolution,
//!   a change token, and every index page); the screen shows a spinner and index progress until
//!   the document exists. Afterwards each `poll` drains finished work into the document and
//!   serves [`ReviewDocument::take_needs`] with at most [`MAX_IN_FLIGHT`] service calls at once.
//!   Frames never call services: rendering reports the visible range to the document, and the
//!   next `poll` issues what it asks for.
//! * **Screen.** A header row ([`super::chrome`]), the body, and a footer row. The body is a BMUX
//!   `panel_group` of bordered panes ([`super::panels`]): the file panel ([`super::files`]), the
//!   review document, and the optional threads panel. Each frame lays the body out twice: once
//!   with a probe in the document's place to learn its size, then, after the document list is
//!   prepared at exactly that size, for real. Painting and event routing use that second layout,
//!   so hit regions always match what was painted.
//! * **Document.** A lazy item source for a BMUX `VirtualListComponent` with sticky file and
//!   hunk headers and scrollbar marks. Every document splice is noted on the list state, so keyed
//!   positions survive loads, evictions, expansion, and layout switches.
//! * **Cursor and focus.** The cursor is a keyed item. Moving it reveals it below the sticky
//!   headers; scrolling the viewport drags it along. Keyboard focus is on the document, the file
//!   tree, the file filter, or the threads panel; the focused pane's border is highlighted.
//!   Selecting a file in the tree moves the cursor to its header, and the tree selection follows
//!   the cursor's file.
//! * **Refresh.** `r` and terminal focus regain compare `review.sources.token`; a changed token
//!   re-opens the review, keeping expansions, the cursor, the top item, and the tree state.

use super::agent::AgentStreams;
use super::chrome::{self, HeaderModel, HeaderPart, HeaderPlan, Status, Toasts};
use super::composer::{Composer, ComposerInput};
use super::config::{ReviewViewConfig, resolve};
use super::files::FileTree;
use super::loader::{self, Inbox, Message, NeedResult, Opened};
use super::notes::ReviewNotes;
use super::palette::Overlay;
use super::panels::{PanelId, Panels};
use super::rows::{
    Folds, GapAction, Interaction, RowCache, RowContext, RowKey, SearchHighlights, ViewParams,
};
use super::services::ReviewServices;
use super::source::Decor;
use super::source::DocumentSource;
use super::syntax::{MAX_HIGHLIGHT_BYTES, MAX_HIGHLIGHT_LINES, SyntaxStore};
use super::theme::ReviewTheme;
use crate::action::Action;
use crate::action::EventResult;
use crate::config::DiffLayout;
use crate::context::UiContext;
use crate::view::View;
use bcode_code_review::document::{
    Direction, DocumentConfig, DocumentNeed, FileRef, ItemKey, LayoutMode, NavKind, ReviewDocument,
    ReviewItem,
};
use bcode_code_review_models::{
    REVIEW_PRESENTATION_SCHEMA_VERSION, ReviewIndexFlag, ReviewPresentationState, ReviewSource,
    ReviewSourceDiagnostic, ReviewWorkspace,
};
use bcode_tui_theme::components::ComponentStyles;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, Element, EventCx, LayoutCx, LayoutId, LayoutNode,
    LogicalSize,
};
use bmux_tui::composition::{Column, Flex, Row, TextBlock};
use bmux_tui::event::{Event, EventOutcome, FocusEvent, MouseButton, MouseEventKind};
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span, Text};
use bmux_tui_components::diff_viewer::{DiffGutterShape, DiffRowGutter, DiffRowOverflow};
use bmux_tui_components::empty_state::{EmptyStateComponent, EmptyStatePolicy};
use bmux_tui_components::pane::{Pane, PaneComponent, PaneState, PaneStyles, PaneTitleAction};
use bmux_tui_components::panel_group::{PanelGroupAxis, PanelGroupComponent};
use bmux_tui_components::scroll_view::{ScrollViewComponent, ScrollViewState};
use bmux_tui_components::spinner::Spinner;
use bmux_tui_components::split_button::{SplitButtonOutcome, SplitButtonState};
use bmux_tui_components::status_bar::StatusSeverity;
use bmux_tui_components::text_input::{
    EdgePolicy, TextInputComponent, TextInputControl, TextInputOutcome, TextInputPolicy,
    TextInputState,
};
use bmux_tui_components::toast_stack::ToastSeverity;
use bmux_tui_components::tree_view::{
    TreeViewComponent, TreeViewItem, TreeViewLabelOverflow, TreeViewSelectionBar, TreeViewState,
    TreeViewTrailingLayout,
};
use bmux_tui_components::virtual_list::{
    AnchorPolicy, Edge, ListAnchor, ListPosition, ScrollbarMark, StickyHeaders, VirtualListAction,
    VirtualListChrome, VirtualListComponent, VirtualListState,
};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Keymap scope of the review screen.
pub const SCOPE: &str = "code-review";
/// Keymap scope of the file panel (its own keys; motions come from [`SCOPE`]).
pub const FILES_SCOPE: &str = "code-review-files";
/// Keymap scope of the threads panel (its own keys; motions come from [`SCOPE`]).
pub const THREADS_SCOPE: &str = "code-review-threads";
/// Service calls in flight at once.
pub const MAX_IN_FLIGHT: usize = 4;
/// Lines a gap reveals per step without a count prefix.
pub const EXPAND_STEP: u32 = 20;
/// Minimum line-number digits of the shared gutter.
const MIN_GUTTER_DIGITS: u32 = 4;
/// Repeats of a motion that waits for its target file to load.
const MAX_MOTION_REPEATS: u8 = 8;
/// Count prefix bound.
const MAX_COUNT: u32 = 100_000;
/// Identity of the review list.
const LIST_ID: &str = "review";
/// Layout ids of the body.
const ROOT_ID: &str = "review.panels";
const COLUMN_ID: &str = "review.column";
/// Pane of the review document.
pub const REVIEW_PANE_ID: &str = "review.pane";
/// Pane of the file panel.
pub const FILES_PANE_ID: &str = "review.files";
/// Pane of the threads panel.
pub const THREADS_PANE_ID: &str = "review.threads";
/// The file tree.
pub const TREE_ID: &str = "review.files.tree";
/// The file tree's scroll viewport.
const TREE_VIEWPORT_ID: &str = "review.files.viewport";
/// The file filter input.
pub const FILTER_ID: &str = "review.files.filter";
/// The threads list.
const THREADS_TREE_ID: &str = "review.threads.list";
/// Rows the tree scrolls per wheel step.
const TREE_WHEEL_ROWS: i64 = 3;
/// Columns a keyboard resize moves the file panel divider.
const RESIZE_STEP: i32 = 4;
/// Longest wait for running saves and annotation calls when exiting.
const CLOSE_GRACE: Duration = Duration::from_secs(5);
/// Id of a transient (unsaved) workspace.
use super::entry::TRANSIENT_WORKSPACE_ID;

/// Phase of the surface.
#[derive(Debug)]
enum Phase {
    /// Nothing started yet.
    Idle,
    /// The open task is running, at `stage`.
    Opening { stage: String },
    /// Opening failed.
    Failed(String),
    /// The document is shown.
    Ready,
}

mod annotate;
mod assist;
mod dock;
mod feed;
mod find;
mod flows;
mod history;
mod pointer;
mod stack;
mod stack_files;

pub use annotate::COMPOSER_SCOPE;
pub use dock::{ASSISTANT_PANE_ID, ASSISTANT_SCOPE, DockResult};
pub use feed::FEED_SCOPE;
pub use flows::Modal;

pub use find::{SearchState, ThreadsFilter};

/// Which pane receives keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The review document.
    Review,
    /// The inline comment composer.
    Composer,
    /// The search input.
    Search,
    /// The file tree.
    Files,
    /// The file filter input.
    Filter,
    /// The threads panel.
    Threads,
    /// The assistant dock.
    Assistant,
}

/// Saving of presentation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveState {
    /// Nothing to save.
    Idle,
    /// Changed; saved on the next poll.
    Pending,
    /// A save is running.
    Running,
    /// A save is running and the state changed since it started.
    RunningThenPending,
}

/// A motion waiting for its target file to load before it repeats.
#[derive(Debug, Clone)]
struct PendingMotion {
    origin: ItemKey,
    kind: NavKind,
    direction: Direction,
    file: FileRef,
    repeats: u8,
}

/// Geometry of the last frame, for event routing.
#[derive(Debug)]
struct FrameState {
    /// Body area (absolute).
    body: Rect,
    /// Body layout (relative to `body`).
    layout: LayoutNode,
    /// Document list area (absolute).
    list_area: Rect,
}

/// Interaction state of the body that a routed mouse event may change.
#[derive(Debug, PartialEq, Eq)]
struct PanelSnapshot {
    tree: TreeViewState,
    thread: Option<usize>,
    root: Vec<bmux_tui_components::panel_group::PanelSize>,
    column: Vec<bmux_tui_components::panel_group::PanelSize>,
}

/// A mouse event routed through the body.
#[derive(Debug)]
struct RoutedMouse {
    outcome: EventOutcome,
    before: PanelSnapshot,
    after: PanelSnapshot,
}

/// A selected range of lines.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Selection {
    /// Where the range started.
    anchor: ItemKey,
    /// The other end (the cursor while `follow`).
    end: ItemKey,
    /// The end follows the cursor (keyboard range mode).
    follow: bool,
}

/// Surface state rows read besides the document.
#[derive(Clone, Copy)]
struct Extras<'a> {
    notes: &'a ReviewNotes,
    agents: &'a AgentStreams,
    folds: &'a Folds,
    composer: Option<&'a Composer>,
    search: Option<&'a SearchHighlights>,
}

const fn row_context<'a>(
    review: &'a Review,
    params: &'a ViewParams,
    syntax: &'a SyntaxStore,
    extras: Extras<'a>,
) -> RowContext<'a> {
    RowContext {
        doc: &review.doc,
        params,
        syntax,
        cache: &review.rows,
        notes: extras.notes,
        agents: extras.agents,
        folds: extras.folds,
        composer: extras.composer,
        search: extras.search,
    }
}

/// Paint decoration of the list items.
fn decor<'a>(review: &'a Review, search: Option<&SearchHighlights>) -> Decor<'a> {
    let selected = review.selection.as_ref().and_then(|selection| {
        let a = review.doc.index_of(&selection.anchor)?;
        let b = review.doc.index_of(&selection.end)?;
        Some((a.min(b), a.max(b)))
    });
    Decor {
        cursor: review.cursor.as_ref(),
        selected,
        hovered: review.hover.as_ref(),
        search_revision: search.map_or(0, |search| search.revision),
    }
}

/// Rows of the threads panel, cached per document revision.
#[derive(Debug, Default)]
struct ThreadRows {
    revision: Option<u64>,
    items: Vec<TreeViewItem>,
    keys: Vec<ItemKey>,
}

/// The open a surface is running: [`Self::start`] cancels the one before, and dropping the
/// surface cancels the last, so a review that was closed or opened again stops loading.
#[derive(Debug, Default)]
struct OpenInFlight(Option<bcode_code_review::provider::Cancel>);

impl OpenInFlight {
    /// Cancel the open in flight and return the new one's token.
    fn start(&mut self) -> bcode_code_review::provider::Cancel {
        let cancel = bcode_code_review::provider::Cancel::default();
        if let Some(previous) = self.0.replace(cancel.clone()) {
            previous.cancel();
        }
        cancel
    }
}

impl Drop for OpenInFlight {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            cancel.cancel();
        }
    }
}

/// The open review.
struct Review {
    doc: ReviewDocument,
    list: RefCell<VirtualListState<RowKey>>,
    rows: RefCell<RowCache>,
    interaction: Interaction,
    cursor: Option<ItemKey>,
    /// Item under the pointer (diff rows show the comment affordance).
    hover: Option<ItemKey>,
    /// A mouse drag across lines: where it started and where it is.
    drag: Option<(ItemKey, ItemKey)>,
    /// Selected line range: its anchor; the other end is the cursor while `follow` is set.
    selection: Option<Selection>,
    /// A thread to move to once its file loads.
    pending_jump: Option<ItemKey>,
    /// A restored scroll position (top item and rows into it) to apply once its file loads.
    pending_restore: Option<(ItemKey, u64)>,
    /// Target offset of the running wheel motion.
    wheel_target: Option<u64>,
    /// Annotation revision last applied to the document.
    notes_applied: Option<u64>,
    cursor_index: usize,
    reveal: Option<AnchorPolicy>,
    follow_viewport: bool,
    motion: Option<PendingMotion>,
    gutter_digits: u32,
    token: Option<String>,
    working_tree: bool,
    diagnostics: Vec<ReviewSourceDiagnostic>,
    frame: Option<FrameState>,
    viewport_height: u64,
    visible: (usize, usize),
    marks: Vec<ScrollbarMark<RowKey>>,
    marks_revision: Option<(u64, Option<ItemKey>)>,
    /// Viewport, measurement generation, and revisions the rows around the viewport were last
    /// measured for (see [`ReviewSurface::prepare_ahead`]).
    prepared_ahead: Option<[u64; 5]>,
    totals: (u64, u64),
    files: FileTree,
    tree: RefCell<TreeViewState>,
    /// Files whose header the document rebuilt since the tree last looked.
    dirty_files: BTreeSet<FileRef>,
    threads: RefCell<ThreadRows>,
    threads_state: RefCell<TreeViewState>,
    /// Commit sides each source resolved to, recorded as the pass when the review closes.
    revisions: BTreeMap<String, bcode_code_review_models::ReviewedRevision>,
}

impl Review {
    fn new(
        repo_path: PathBuf,
        opened: Opened,
        config: DocumentConfig,
        theme: &ReviewTheme,
    ) -> Self {
        let Opened {
            inputs,
            diagnostics,
            token,
            working_tree,
            ..
        } = opened;
        let totals = inputs.iter().fold((0, 0), |totals, input| {
            input.entries.iter().fold(totals, |(add, del), entry| {
                (
                    add + u64::from(entry.additions),
                    del + u64::from(entry.deletions),
                )
            })
        });
        let recorded_at = Some(annotate::now_ms());
        let mut revisions = BTreeMap::new();
        for input in &inputs {
            // A source resolving to several parts records its first part.
            revisions
                .entry(input.source.source_id.clone())
                .or_insert_with(|| {
                    bcode_code_review_models::ReviewedRevision::from_resolved(
                        &input.source,
                        recorded_at,
                    )
                });
        }
        revisions.retain(
            |_, revision: &mut bcode_code_review_models::ReviewedRevision| {
                revision.reviewed_head_oid.is_some()
            },
        );
        let doc = ReviewDocument::new(repo_path, inputs, config);
        let files = FileTree::new(&doc, theme);
        let mut tree = TreeViewState::new(None);
        tree.set_expanded_ids(files.expandable_ids().map(str::to_owned));
        let mut review = Self {
            doc,
            list: RefCell::new(VirtualListState::new(0)),
            rows: RefCell::new(RowCache::default()),
            interaction: Interaction::default(),
            cursor: None,
            hover: None,
            drag: None,
            selection: None,
            pending_jump: None,
            pending_restore: None,
            wheel_target: None,
            notes_applied: None,
            cursor_index: 0,
            reveal: None,
            follow_viewport: false,
            motion: None,
            gutter_digits: MIN_GUTTER_DIGITS,
            token,
            working_tree,
            diagnostics,
            frame: None,
            viewport_height: 0,
            visible: (0, 0),
            marks: Vec::new(),
            marks_revision: None,
            prepared_ahead: None,
            totals,
            files,
            tree: RefCell::new(tree),
            dirty_files: BTreeSet::new(),
            threads: RefCell::new(ThreadRows::default()),
            threads_state: RefCell::new(TreeViewState::new(None)),
            revisions,
        };
        review.cursor = review.doc.item(0).map(ReviewItem::key);
        review
    }

    /// Rebuild the file panel from the document (after its focus changed), every directory open.
    fn rebuild_files(&mut self, theme: &ReviewTheme, hide_viewed: bool) {
        self.files = FileTree::new(&self.doc, theme);
        self.files.set_hide_viewed(hide_viewed, theme);
        self.tree
            .borrow_mut()
            .set_expanded_ids(self.files.expandable_ids().map(str::to_owned));
    }

    /// Open the file panel's group of source `source` and close the other groups.
    fn open_only_group(&self, source: &str) {
        let mut tree = self.tree.borrow_mut();
        for (id, group) in self.files.group_ids() {
            tree.set_expanded(id, group == source);
        }
    }

    /// Note every pending document splice on the list, grow the shared gutter for new line
    /// numbers, and remember rebuilt file headers for the tree. Returns whether anything
    /// changed.
    fn note_splices(&mut self) -> bool {
        let splices = self.doc.take_splices();
        if splices.is_empty() {
            return false;
        }
        let mut digits = self.gutter_digits;
        {
            let mut list = self.list.borrow_mut();
            for splice in &splices {
                list.note_splice(splice.start..splice.start + splice.removed, splice.inserted);
            }
        }
        for splice in &splices {
            for index in splice.start..splice.start + splice.inserted {
                if let Some(item) = self.doc.item(index) {
                    let max = match item {
                        ReviewItem::Gap(gap) => gap.end.unwrap_or(gap.start),
                        ReviewItem::FileHeader(header) => {
                            self.dirty_files.insert(header.file.clone());
                            0
                        }
                        _ => item
                            .line_numbers()
                            .map_or(0, |(old, new)| old.unwrap_or(0).max(new.unwrap_or(0))),
                    };
                    digits = digits.max(digit_count(max));
                }
            }
        }
        self.gutter_digits = digits;
        self.fix_cursor();
        true
    }

    /// Keep the cursor on an existing item: its key when present, else the item now at its last
    /// index.
    fn fix_cursor(&mut self) {
        if let Some(index) = self.cursor.as_ref().and_then(|key| self.doc.index_of(key)) {
            self.cursor_index = index;
            self.cursor = self.doc.item(index).map(ReviewItem::key);
            return;
        }
        let len = self.doc.len();
        if len == 0 {
            self.cursor = None;
            return;
        }
        self.cursor_index = self.cursor_index.min(len - 1);
        self.cursor = self.doc.item(self.cursor_index).map(ReviewItem::key);
    }

    fn set_cursor(&mut self, index: usize, reveal: AnchorPolicy) {
        let len = self.doc.len();
        if len == 0 {
            return;
        }
        let index = index.min(len - 1);
        self.cursor_index = index;
        self.cursor = self.doc.item(index).map(ReviewItem::key);
        self.reveal = Some(reveal);
        self.follow_viewport = false;
    }

    /// File of the cursor item.
    fn cursor_file(&self) -> Option<FileRef> {
        self.doc
            .item(self.cursor_index)
            .and_then(ReviewItem::file)
            .cloned()
    }

    fn tree_component(&self) -> TreeViewComponent<'_, '_> {
        TreeViewComponent::new(TREE_ID, self.files.items(), &self.tree)
            .items_revision(self.files.revision())
            .toggle_on_click(true)
            .trailing_layout(
                TreeViewTrailingLayout::new()
                    .min_label_width(10)
                    .label_overflow(TreeViewLabelOverflow::Ellipsis),
            )
    }
}

const fn digit_count(value: u32) -> u32 {
    match value.checked_ilog10() {
        Some(log) => log + 1,
        None => 1,
    }
}

/// The review screen.
// The flags are independent pieces of surface state (refresh, tree reveal, restore, and
// annotation loading), not a mode that an enum would describe.
#[allow(clippy::struct_excessive_bools)]
pub struct ReviewSurface {
    services: ReviewServices,
    repo_path: PathBuf,
    sources: Vec<ReviewSource>,
    title: String,
    config: ReviewViewConfig,
    document_config: DocumentConfig,
    inbox: Inbox,
    phase: Phase,
    started: Option<Instant>,
    review: Option<Review>,
    theme_input: Option<ComponentStyles>,
    theme: ReviewTheme,
    host_layout: Option<DiffLayout>,
    layout_override: Option<LayoutMode>,
    overflow: DiffRowOverflow,
    layout_revision: u64,
    paint_revision: u64,
    syntax: SyntaxStore,
    queue: VecDeque<DocumentNeed>,
    in_flight: usize,
    count: Option<u32>,
    status: Option<Status>,
    refreshing: bool,
    action_keys: BTreeMap<String, String>,
    opened_at: Option<Instant>,
    /// Document generation; increases whenever a (re)opened document replaces the previous.
    generation: u64,
    focus: Focus,
    panels: Panels,
    files_pane: Cell<PaneState>,
    review_pane: Cell<PaneState>,
    threads_pane: Cell<PaneState>,
    tree_scroll: Cell<ScrollViewState>,
    tree_reveal: bool,
    synced_file: Option<FileRef>,
    filter: RefCell<TextInputState>,
    filter_policy: TextInputPolicy,
    toasts: Toasts,
    publish: SplitButtonState,
    header: Option<(Rect, HeaderPlan)>,
    screen: Rect,
    threads_empty: Vec<Line>,
    workspace: Option<ReviewWorkspace>,
    save: SaveState,
    notes: ReviewNotes,
    agents: AgentStreams,
    composer: Option<Composer>,
    search: Option<SearchState>,
    overlay: Option<Overlay>,
    /// Cursor and list position to restore when returning from a linked session.
    return_to: Option<(Option<ItemKey>, ListPosition<RowKey>)>,
    /// An action to return from the next poll (opening a session after asking).
    pending_action: Option<Action>,
    /// When exiting started while saves or annotation calls were still running; the surface
    /// closes once they finish (or after [`CLOSE_GRACE`]), so exiting never drops them.
    closing: Option<Instant>,
    threads_filter: ThreadsFilter,
    /// Duration of wheel scroll motions (zero scrolls at once).
    motion: Duration,
    /// When presentation state was last compared with the stored state.
    presentation_checked: Option<Instant>,
    /// Whether stored presentation state was applied to the opened document.
    restored: bool,
    /// Questions to send from the next poll.
    asks: VecDeque<(bcode_code_review_models::ReviewAiExchange, String)>,
    /// Questions on their way that create their own session, by exchange id.
    fresh_asks: std::collections::BTreeSet<String>,
    next_exchange: u64,
    /// Whether the open document's annotations were requested.
    notes_requested: bool,
    /// Whether a transient review is being saved as a durable one.
    saving_as: bool,
    /// Viewed files and marks of an unsaved review as last stored for its target.
    transient_viewed: Option<(
        std::collections::BTreeSet<String>,
        Vec<bcode_code_review_models::ViewedMark>,
    )>,
    /// The open publish or import flow.
    modal: Option<Modal>,
    /// Stack rail tab state, and the rail's area and layout in the last frame.
    rail_state: RefCell<bmux_tui_components::tab_bar::TabBarState>,
    rail: Option<(Rect, LayoutNode)>,
    /// Layer (source id) the cursor was last in, for items outside every layer.
    rail_layer: Option<String>,
    /// Last position in each stack layer, by source id (persisted).
    layer_positions: BTreeMap<String, bcode_code_review_models::ReviewItemKey>,
    /// Commit sides each source of the whole review resolved to (the pass recorded when the
    /// reviewer completes it or publishes).
    pass_revisions: BTreeMap<String, bcode_code_review_models::ReviewedRevision>,
    /// What changed since the last recorded pass.
    rereview: Option<super::rereview::RereviewPlan>,
    rereview_checked: bool,
    /// The whole review's sources while the changes since the last review are shown.
    since_last: Option<Vec<ReviewSource>>,
    /// Files whose patch is unchanged since the last pass.
    carried: BTreeSet<bcode_code_review::document::FileKey>,
    /// A re-open that is not a refresh (switching to or from the changes since the last
    /// review): no "refreshed" toast.
    switching: bool,
    /// Folded threads and expanded AI answers (persisted).
    folds: Folds,
    /// Resolved threads are hidden from the document (persisted).
    hide_resolved: bool,
    /// Viewed files are hidden from the file panel (persisted).
    hide_viewed_files: bool,
    /// How a step-through stack review is shown (persisted).
    stack_view: stack::StackView,
    /// The review shown has sources still loading; they arrive one by one.
    filling: bool,
    /// What is still loading after the first frame (the open's stage), shown on the layer strip.
    filling_note: Option<String>,
    /// When the review's stack was last resolved or confirmed by its provider.
    stack_confirmed_at: Option<std::time::SystemTime>,
    /// Sources that failed after the first frame.
    failed_sources: BTreeSet<String>,
    /// The stack this review last saw (and its provider), for its history.
    stack_news: Option<bcode_code_review::session::StackNews>,
    /// The stack's history, once read.
    stack_history: Option<bcode_code_review_models::StackTimeline>,
    /// The open running now: cancelled when another starts or the surface goes away.
    opening: OpenInFlight,
    /// Text to copy once the host is at hand, and what it is.
    clipboard: Option<(String, &'static str)>,
    /// Exiting an unsaved review with annotations was asked once (the next exit leaves).
    exit_armed: bool,
    /// Closing asks the app to edit the review's sources.
    edit_requested: bool,
    /// Permission answers and stops waiting to be sent.
    feed_requests: VecDeque<feed::FeedRequest>,
    /// When the next spinner frame was asked for.
    feed_wake: Option<Instant>,
    /// The review assistant dock.
    dock: dock::Dock,
}

impl std::fmt::Debug for ReviewSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewSurface")
            .field("repo_path", &self.repo_path)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

impl ReviewSurface {
    /// A surface that opens `sources` of the repository at `repo_path` on its first poll.
    #[must_use]
    pub fn new(
        services: ReviewServices,
        repo_path: PathBuf,
        sources: Vec<ReviewSource>,
        title: String,
        config: ReviewViewConfig,
    ) -> Self {
        let icons = config.icons.unwrap_or_default();
        let mut filter_policy = TextInputPolicy::chat_composer();
        filter_policy.keyboard.shift_enter = None;
        filter_policy.viewport.max_rows = Some(1);
        filter_policy.edge = EdgePolicy::disabled();
        Self {
            services,
            repo_path,
            sources,
            title,
            config,
            document_config: DocumentConfig {
                viewed_reset: config.viewed_reset,
                ..DocumentConfig::default()
            },
            inbox: Inbox::default(),
            phase: Phase::Idle,
            started: None,
            review: None,
            theme_input: None,
            theme: ReviewTheme::derive(None, icons),
            host_layout: None,
            layout_override: None,
            overflow: DiffRowOverflow::Wrap,
            layout_revision: 0,
            paint_revision: 0,
            syntax: SyntaxStore::default(),
            queue: VecDeque::new(),
            in_flight: 0,
            count: None,
            status: None,
            refreshing: false,
            action_keys: BTreeMap::new(),
            opened_at: None,
            generation: 0,
            focus: Focus::Review,
            dock: dock::Dock::default(),
            panels: Panels::default(),
            files_pane: Cell::new(PaneState::new(Rect::default())),
            review_pane: Cell::new(PaneState::new(Rect::default())),
            threads_pane: Cell::new(PaneState::new(Rect::default())),
            tree_scroll: Cell::new(ScrollViewState::new()),
            tree_reveal: true,
            synced_file: None,
            filter: RefCell::new(TextInputState::new(bmux_text_edit::TextEditBuffer::new())),
            filter_policy,
            toasts: Toasts::default(),
            publish: SplitButtonState::new(),
            header: None,
            screen: Rect::default(),
            threads_empty: Vec::new(),
            workspace: None,
            save: SaveState::Idle,
            notes: ReviewNotes::default(),
            agents: AgentStreams::default(),
            composer: None,
            search: None,
            overlay: None,
            return_to: None,
            pending_action: None,
            closing: None,
            threads_filter: ThreadsFilter::All,
            motion: Duration::from_millis(90),
            presentation_checked: None,
            restored: false,
            asks: VecDeque::new(),
            fresh_asks: std::collections::BTreeSet::new(),
            next_exchange: 0,
            notes_requested: false,
            saving_as: false,
            transient_viewed: None,
            modal: None,
            rail_state: RefCell::new(bmux_tui_components::tab_bar::TabBarState::new(None)),
            rail: None,
            rail_layer: None,
            layer_positions: BTreeMap::new(),
            pass_revisions: BTreeMap::new(),
            rereview: None,
            rereview_checked: false,
            since_last: None,
            carried: BTreeSet::new(),
            switching: false,
            folds: Folds::default(),
            hide_resolved: false,
            hide_viewed_files: false,
            stack_view: stack::StackView::default(),
            filling: false,
            filling_note: None,
            stack_confirmed_at: None,
            failed_sources: BTreeSet::new(),
            stack_news: None,
            stack_history: None,
            opening: OpenInFlight::default(),
            clipboard: None,
            exit_armed: false,
            edit_requested: false,
            feed_requests: VecDeque::new(),
            feed_wake: None,
        }
    }

    /// Animate wheel scrolling over `duration` (zero scrolls at once).
    #[must_use]
    pub const fn with_scroll_motion(mut self, duration: Duration) -> Self {
        self.motion = duration;
        self
    }

    fn extras(&self) -> Extras<'_> {
        Extras {
            notes: &self.notes,
            agents: &self.agents,
            folds: &self.folds,
            composer: self.composer.as_ref(),
            search: self.search.as_ref().map(|search| &search.highlights),
        }
    }

    /// Use `config` for the document (residency budget, windows).
    #[must_use]
    pub const fn with_document_config(mut self, config: DocumentConfig) -> Self {
        self.document_config = config;
        self
    }

    /// Keep presentation state with `workspace`: panels restore from its presentation state,
    /// and changes are saved back when the workspace is durable (not transient).
    #[must_use]
    pub fn with_workspace(mut self, mut workspace: ReviewWorkspace) -> Self {
        // Presentation state is disposable: a newer schema than this build's is dropped, never
        // read (or rewritten) as the current form.
        if workspace
            .presentation
            .as_ref()
            .is_some_and(|state| state.schema_version > REVIEW_PRESENTATION_SCHEMA_VERSION)
        {
            workspace.presentation = None;
        }
        if let Some(state) = &workspace.presentation {
            self.panels = Panels::restore(&state.panels);
            self.layer_positions.clone_from(&state.layer_positions);
            self.folds.collapsed.clone_from(&state.collapsed_threads);
            self.folds.expanded.clone_from(&state.expanded_answers);
            self.hide_resolved = state.hide_resolved_threads;
            self.hide_viewed_files = state.hide_viewed_files;
            self.stack_view = if state.all_stack_layers {
                stack::StackView::AllLayers
            } else {
                stack::StackView::OneLayer
            };
        }
        self.workspace = Some(workspace);
        self
    }

    /// The open document, once loaded.
    #[must_use]
    pub fn document(&self) -> Option<&ReviewDocument> {
        self.review.as_ref().map(|review| &review.doc)
    }

    /// Key of the cursor item.
    #[must_use]
    pub fn cursor(&self) -> Option<&ItemKey> {
        self.review.as_ref()?.cursor.as_ref()
    }

    /// Key of the item on the viewport's top row, after the last frame.
    #[must_use]
    pub fn top_key(&self) -> Option<ItemKey> {
        let review = self.review.as_ref()?;
        let list = review.list.borrow();
        let top = list.resolved()?.top_offset;
        list.key_at_offset(top).map(|key| key.0.clone())
    }

    /// Keys of the headers pinned at the top by the last frame, outermost first.
    #[must_use]
    pub fn pinned_keys(&self) -> Vec<ItemKey> {
        self.review.as_ref().map_or_else(Vec::new, |review| {
            review
                .list
                .borrow()
                .pinned_header_keys()
                .into_iter()
                .map(|key| key.0)
                .collect()
        })
    }

    /// Item rows built so far (row cache misses), for performance assertions.
    #[must_use]
    pub fn rows_built(&self) -> usize {
        self.review
            .as_ref()
            .map_or(0, |review| review.rows.borrow().built())
    }

    /// Service calls queued or in flight.
    #[must_use]
    pub fn outstanding_requests(&self) -> usize {
        self.queue.len() + self.in_flight
    }

    /// When the first index arrived (before the document and the file tree were built), for
    /// latency measurements.
    #[must_use]
    pub const fn opened_at(&self) -> Option<Instant> {
        self.opened_at
    }

    /// Status line text.
    #[must_use]
    pub fn status(&self) -> Option<&str> {
        self.status.as_ref().map(|status| status.text.as_str())
    }

    /// Titles of the toasts shown, oldest first.
    #[must_use]
    pub fn toasts(&self) -> Vec<&str> {
        self.toasts.titles()
    }

    /// Pane with keyboard focus.
    #[must_use]
    pub const fn focus(&self) -> Focus {
        self.focus
    }

    /// Document list area (absolute) of the last frame.
    #[must_use]
    pub fn list_area(&self) -> Option<Rect> {
        self.review
            .as_ref()?
            .frame
            .as_ref()
            .map(|frame| frame.list_area)
    }

    /// Area (absolute) of a laid-out body node, such as [`FILES_PANE_ID`] or [`TREE_ID`], in
    /// the last frame.
    #[must_use]
    pub fn body_area(&self, id: &str) -> Option<Rect> {
        let frame = self.review.as_ref()?.frame.as_ref()?;
        let rect = frame.layout.find_rect(&LayoutId::new(id.to_owned()))?;
        Some(Rect::new(
            frame.body.x.saturating_add(rect.x),
            frame.body.y.saturating_add(rect.y),
            rect.width,
            rect.height,
        ))
    }

    /// The file tree, once the review is open.
    #[must_use]
    pub fn file_tree(&self) -> Option<&FileTree> {
        self.review.as_ref().map(|review| &review.files)
    }

    /// Id of the tree row selected in the file panel.
    #[must_use]
    pub fn tree_selection(&self) -> Option<String> {
        let review = self.review.as_ref()?;
        let tree = review.tree.borrow();
        tree.selected_id().map(str::to_owned)
    }

    /// Workspace presentation state as the surface would save it.
    #[must_use]
    pub fn presentation(&self) -> ReviewPresentationState {
        let mut state = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.presentation.clone())
            .unwrap_or_default();
        state.panels = self.panels.state();
        state
    }

    /// The review theme in use.
    #[must_use]
    pub const fn theme(&self) -> &ReviewTheme {
        &self.theme
    }

    /// The review's annotations.
    #[must_use]
    pub const fn notes(&self) -> &ReviewNotes {
        &self.notes
    }

    /// The open composer.
    #[must_use]
    pub const fn composer(&self) -> Option<&Composer> {
        self.composer.as_ref()
    }

    /// The open palette or help overlay.
    #[must_use]
    pub const fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    /// The open publish or import flow.
    #[must_use]
    pub const fn modal(&self) -> Option<&Modal> {
        self.modal.as_ref()
    }

    /// What changed since the last recorded review pass, once compared.
    #[must_use]
    pub const fn rereview(&self) -> Option<&super::rereview::RereviewPlan> {
        self.rereview.as_ref()
    }

    /// Whether the changes since the last review are shown instead of the whole review.
    #[must_use]
    pub const fn showing_since_last(&self) -> bool {
        self.since_last.is_some()
    }

    /// The sources the review currently shows.
    #[must_use]
    pub fn sources(&self) -> &[ReviewSource] {
        &self.sources
    }

    /// Labels of the stack rail's tabs (full form), in order, and the selected one.
    #[must_use]
    pub fn rail(&self) -> (Vec<String>, Option<usize>) {
        let entries = self.rail_entries();
        let current = self.current_layer(&entries);
        (
            entries
                .iter()
                .map(|entry| entry.labels[0].plain_text())
                .collect(),
            current,
        )
    }

    /// The workspace (with viewed marks and presentation state) as the surface keeps it.
    #[must_use]
    pub const fn workspace(&self) -> Option<&ReviewWorkspace> {
        self.workspace.as_ref()
    }

    /// Whether the review is not saved (a direct entry); `save_review` makes it durable.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        self.workspace
            .as_ref()
            .is_none_or(|workspace| workspace.id == TRANSIENT_WORKSPACE_ID)
    }

    /// Search status text (match position, pending files), while a search is active.
    #[must_use]
    pub fn search_status(&self) -> Option<String> {
        self.search.as_ref().map(SearchState::status)
    }

    /// Ends of the selected line range.
    #[must_use]
    pub fn selection(&self) -> Option<(ItemKey, ItemKey)> {
        let review = self.review.as_ref()?;
        let selection = review.selection.as_ref()?;
        let end = if selection.follow {
            review.cursor.clone()?
        } else {
            selection.end.clone()
        };
        Some((selection.anchor.clone(), end))
    }

    /// Collection row on the viewport's first row after the last frame.
    #[must_use]
    pub fn scroll_offset(&self) -> Option<u64> {
        let review = self.review.as_ref()?;
        let list = review.list.borrow();
        list.resolved().map(|resolved| resolved.top_offset)
    }

    /// Visible rows (absolute) of an item in the last frame.
    #[must_use]
    pub fn item_area(&self, key: &ItemKey) -> Option<Rect> {
        let review = self.review.as_ref()?;
        let area = review.frame.as_ref()?.list_area;
        let list = review.list.borrow();
        let top = list.resolved()?.top_offset;
        let start = list.item_offset(&RowKey(key.clone()))?;
        let height = list.item_height(&RowKey(key.clone()))?;
        let first = start.max(top);
        let end = (start + height).min(top + u64::from(area.height));
        (end > first).then(|| {
            Rect::new(
                area.x,
                area.y + u16::try_from(first - top).unwrap_or(0),
                area.width,
                u16::try_from(end - first).unwrap_or(0),
            )
        })
    }

    fn params(&self, review: &Review) -> ViewParams {
        ViewParams {
            theme: self.theme,
            overflow: self.overflow,
            gutter: DiffRowGutter::new(
                review.gutter_digits as usize,
                DiffGutterShape::LineNumbersThenMarker,
            ),
            tab_width: self.config.tab_width(),
            expand_step: EXPAND_STEP,
            layout_revision: self
                .layout_revision
                .wrapping_mul(64)
                .wrapping_add(u64::from(review.gutter_digits)),
            paint_revision: self.paint_revision,
            annotation_revision: self.notes.revision().wrapping_mul(1_000_003)
                ^ self.folds.revision.wrapping_mul(0x5851_f42d),
        }
    }

    fn policy(&self) -> DiffLayout {
        self.config.policy(self.host_layout)
    }

    /// Warn, in a toast that stays until dismissed, when a stack of the review has more layers
    /// than the configured limit and the limit only warns. Every layer is in the review; the
    /// warning says so and how to change the limit.
    fn warn_about_stack_size(&mut self, deepest: Option<usize>) {
        let limit = self.services.stack_limit();
        let Some(layers) = deepest.filter(|layers| limit.warns_for(*layers)) else {
            return;
        };
        self.toasts.push_sticky(
            ToastSeverity::Warning,
            format!("Stack of {layers} layers"),
            Some(format!(
                "Over the {}-layer limit; every layer is loaded. Set \
                 [plugins.config.\"bcode.code_review\".stack] max_layers, or over_limit = \
                 \"stop\" to cut stacks there.",
                limit.max_layers
            )),
        );
    }

    fn set_status(&mut self, text: impl Into<String>, severity: StatusSeverity) {
        self.status = Some(Status {
            text: text.into(),
            severity,
        });
    }

    // ----------------------------------------------------------------------------------------
    // Async work
    // ----------------------------------------------------------------------------------------

    fn start_open(&mut self, host: &UiContext) {
        // The stacks the review last opened with are shown at once (and confirmed after).
        if let Some(workspace) = &self.workspace {
            let registry = self.services.providers();
            for snapshot in &workspace.stack_snapshots {
                registry.seed_stack(
                    &snapshot.provider,
                    &snapshot.repository,
                    &snapshot.stack,
                    std::time::UNIX_EPOCH
                        + std::time::Duration::from_millis(snapshot.confirmed_at_ms),
                );
            }
        }
        self.phase = Phase::Opening {
            stage: "opening".to_owned(),
        };
        self.started = Some(Instant::now());
        let services = self.services.clone();
        let repo = self.repo_path.clone();
        let sources = self.sources.clone();
        let inbox = self.inbox.clone();
        let viewed = loader::ViewedInputs {
            transient_scope: self
                .is_transient()
                .then_some(self.workspace.as_ref())
                .flatten()
                .map(bcode_code_review::scope::annotation_workspace_id),
            ..self.viewed_inputs()
        };
        let first = self.first_source();
        let cancel = self.opening.start();
        host.spawn(Box::pin(async move {
            // Streamed: the first frame, then each source as it arrives.
            let options = loader::OpenOptions {
                viewed,
                first,
                stream: true,
                cancel,
            };
            let message = match loader::open(services, repo, sources, inbox.clone(), options).await
            {
                Ok(Some(opened)) => Message::Opened(Box::new(opened)),
                Ok(None) => return,
                Err(error) => Message::OpenFailed(error),
            };
            inbox.push(message);
        }));
    }

    /// The source to make ready first when opening: the layer last entered, else the source of the
    /// saved scroll position.
    fn first_source(&self) -> Option<String> {
        use bcode_code_review_models::ReviewItemKey;
        if let Some(layer) = &self.rail_layer {
            return Some(layer.clone());
        }
        let anchor = self
            .workspace
            .as_ref()?
            .presentation
            .as_ref()?
            .scroll_anchor
            .as_ref()?;
        match &anchor.item {
            ReviewItemKey::SourceHeader { source_id, .. }
            | ReviewItemKey::FileHeader { source_id, .. }
            | ReviewItemKey::HunkHeader { source_id, .. }
            | ReviewItemKey::Line { source_id, .. } => Some(source_id.clone()),
            ReviewItemKey::Thread { .. } => None,
        }
    }

    /// The paths the workspace's viewed marks name, for loading their fingerprints on open.
    fn viewed_inputs(&self) -> loader::ViewedInputs {
        loader::ViewedInputs {
            marked: self
                .workspace
                .as_ref()
                .map(|workspace| {
                    bcode_code_review::viewed::effective_marks(workspace)
                        .into_iter()
                        .map(|mark| mark.path)
                        .collect()
                })
                .unwrap_or_default(),
            transient_scope: None,
        }
    }

    fn request_token(&mut self, host: &UiContext, manual: bool) {
        let Some(review) = &self.review else {
            return;
        };
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        let services = self.services.clone();
        let repo = self.repo_path.clone();
        let working_tree = review.working_tree;
        let inbox = self.inbox.clone();
        host.spawn(Box::pin(async move {
            let result = loader::token(&services, &repo, working_tree).await;
            inbox.push(Message::Token { result, manual });
        }));
    }

    /// Serve queued needs up to the in-flight bound.
    fn pump(&mut self, host: &UiContext) {
        if let Some(review) = &mut self.review {
            self.queue.extend(review.doc.take_needs());
        }
        while self.in_flight < MAX_IN_FLIGHT {
            let Some(need) = self.queue.pop_front() else {
                break;
            };
            self.in_flight += 1;
            let services = self.services.clone();
            let inbox = self.inbox.clone();
            let generation = self.generation;
            host.spawn(Box::pin(async move {
                let (ticket, result) = loader::serve_need(services, need).await;
                inbox.push(Message::Need {
                    generation,
                    ticket,
                    result,
                });
            }));
        }
    }

    /// Save the workspace's presentation state when it changed and the workspace is durable.
    /// Whether nothing is left to save: no workspace save and no annotation call is running
    /// or queued.
    /// Whether leaving should first offer saving: the review is transient and has drafts, AI
    /// questions, or suggestions.
    fn offers_save(&self) -> bool {
        self.is_transient()
            && (!self.notes.drafts().is_empty()
                || !self.notes.exchanges().is_empty()
                || !self.notes.suggestions().is_empty())
    }

    /// Save presentation state and close once saves finished (at once when they have, or when
    /// closing already started).
    fn begin_close(&mut self, host: &UiContext) -> Action {
        self.persist_presentation(true);
        self.save(host);
        // Exiting again while saving closes at once.
        if self.close_ready() || self.closing.is_some() {
            return self.close_action();
        }
        // Finish saving first: the host may end the process once the surface closes.
        self.closing = Some(Instant::now());
        self.set_status("saving…", StatusSeverity::Info);
        Action::Redraw
    }

    /// The close action: plain, or asking the app to edit the review's sources.
    const fn close_action(&self) -> Action {
        Action::Close {
            edit_sources: self.edit_requested,
        }
    }

    /// Leave the review to edit its sources (the app shows the wizard with them).
    fn edit_sources(&mut self, host: &UiContext) -> Action {
        if self.notes.pending() > 0 || !self.asks.is_empty() {
            self.set_status(
                "comments are still saving; try again in a moment",
                StatusSeverity::Warning,
            );
            return Action::Redraw;
        }
        self.edit_requested = true;
        self.begin_close(host)
    }

    /// The workspace to edit: the review's own, or a transient one of its sources.
    #[must_use]
    pub fn edit_target(&self) -> ReviewWorkspace {
        let mut workspace = self.workspace.clone().unwrap_or_else(|| {
            super::entry::transient_workspace(
                self.repo_path.clone(),
                Some(self.title.clone()),
                Vec::new(),
            )
        });
        // The whole review's sources (not the changes since the last review).
        workspace.sources = self
            .since_last
            .clone()
            .unwrap_or_else(|| self.sources.clone());
        workspace.presentation = Some(self.current_presentation());
        workspace
    }

    fn close_ready(&self) -> bool {
        self.save == SaveState::Idle && self.notes.pending() == 0
    }

    fn save(&mut self, host: &UiContext) {
        if self.save != SaveState::Pending {
            return;
        }
        let Some(mut workspace) = self.workspace.clone() else {
            self.save = SaveState::Idle;
            return;
        };
        // An unsaved review keeps only its viewed state, under its target's scope.
        if workspace.id == TRANSIENT_WORKSPACE_ID {
            workspace = loader::transient_viewed_record(&workspace);
        }
        self.save = SaveState::Running;
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        host.spawn(Box::pin(async move {
            let result = loader::save_workspace(services, workspace).await;
            inbox.push(Message::Saved(result));
        }));
    }

    /// Record the panel state in the workspace's presentation state.
    fn persist_panels(&mut self) {
        let panels = self.panels.state();
        let Some(workspace) = &mut self.workspace else {
            return;
        };
        let state = workspace.presentation.get_or_insert_with(Default::default);
        if state.panels == panels {
            return;
        }
        state.panels = panels;
        if workspace.id != TRANSIENT_WORKSPACE_ID {
            self.save = match self.save {
                SaveState::Idle | SaveState::Pending => SaveState::Pending,
                SaveState::Running | SaveState::RunningThenPending => SaveState::RunningThenPending,
            };
        }
    }

    /// Record the revisions this pass reviewed (durable reviews; saved with the workspace).
    fn record_pass(&mut self) {
        let revisions = self.pass_revisions.clone();
        if revisions.is_empty() {
            return;
        }
        if self.is_transient() {
            return;
        }
        let workspace = self.workspace_mut();
        let same = workspace.reviewed_revisions.len() == revisions.len()
            && revisions.iter().all(|(id, revision)| {
                workspace
                    .reviewed_revisions
                    .get(id)
                    .is_some_and(|recorded| {
                        recorded.reviewed_base_oid == revision.reviewed_base_oid
                            && recorded.reviewed_head_oid == revision.reviewed_head_oid
                    })
            });
        if same {
            return;
        }
        workspace.reviewed_revisions = revisions;
        self.request_save();
    }

    /// Save a transient review as a durable one: create the workspace, copy its viewed and
    /// presentation state, and move its drafts.
    fn save_review(&mut self, host: &UiContext) {
        if self.saving_as {
            self.set_status("saving the review…", StatusSeverity::Info);
            return;
        }
        if !self.is_transient() {
            self.set_status("this review is already saved", StatusSeverity::Info);
            return;
        }
        if self.notes.pending() > 0 {
            self.set_status(
                "comments are still saving; try again in a moment",
                StatusSeverity::Warning,
            );
            return;
        }
        if !self.pass_revisions.is_empty() {
            self.workspace_mut().reviewed_revisions = self.pass_revisions.clone();
        }
        let workspace = self.workspace_mut().clone();
        let notes = super::notes::LoadedNotes {
            drafts: self.notes.drafts().to_vec(),
            exchanges: self
                .notes
                .exchanges()
                .iter()
                .filter(|exchange| {
                    exchange.status != bcode_code_review_models::ReviewAiExchangeStatus::Pending
                })
                .cloned()
                .collect(),
            suggestions: self.notes.suggestions().to_vec(),
            ..super::notes::LoadedNotes::default()
        };
        self.saving_as = true;
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        host.spawn(Box::pin(async move {
            let result = loader::save_as(services, workspace, notes).await;
            inbox.push(Message::SavedAs(Box::new(result)));
        }));
        self.set_status("saving the review…", StatusSeverity::Info);
    }

    fn finish_save_review(&mut self, result: Result<ReviewWorkspace, String>) {
        self.saving_as = false;
        match result {
            Ok(saved) => {
                // Keep what changed while saving; take the durable identity.
                let workspace = self.workspace_mut();
                workspace.id = saved.id;
                workspace.created_at_ms = saved.created_at_ms;
                workspace.updated_at_ms = saved.updated_at_ms;
                workspace.repo_root = saved.repo_root;
                self.request_save();
                // Annotations now live in the saved review's scope.
                self.notes_requested = false;
                self.status = None;
                self.toasts
                    .push(ToastSeverity::Success, "Review saved", None);
            }
            Err(error) => {
                self.status = None;
                self.toasts.push(
                    ToastSeverity::Error,
                    "Could not save the review",
                    Some(error),
                );
            }
        }
    }

    /// Start reading and highlighting both sides of visible resident text files, and cancel jobs
    /// for blobs whose files left the screen.
    fn schedule_syntax(&mut self, host: &UiContext) {
        let Some(review) = &self.review else {
            return;
        };
        let wanted = visible_blobs(review, &self.syntax);
        let wanted_keys: BTreeSet<_> = wanted.iter().map(|(key, _)| key.clone()).collect();
        self.syntax.cancel_unwanted(&wanted_keys);
        for (key, path) in wanted {
            if !self.syntax.has_capacity() {
                break;
            }
            if self.syntax.known(&key) {
                continue;
            }
            let token = self.syntax.start(key.clone());
            let services = self.services.clone();
            let repo = self.repo_path.clone();
            let inbox = self.inbox.clone();
            host.spawn(Box::pin(async move {
                let text = loader::read_blob_text(
                    services,
                    repo,
                    key.blob.selector(),
                    MAX_HIGHLIGHT_LINES,
                    MAX_HIGHLIGHT_BYTES,
                )
                .await;
                inbox.push(Message::BlobText {
                    key,
                    token,
                    path,
                    text,
                });
            }));
        }
    }

    /// Apply one highlighting message. Returns whether it changes what is on screen.
    fn apply_syntax(&mut self, message: Message, host: &UiContext) -> bool {
        match message {
            Message::BlobText {
                key,
                token,
                path,
                text,
            } => {
                // A read that finishes after its file left the screen is dropped.
                if !self.syntax.is_current(&key, &token) {
                    return false;
                }
                let Some(text) = text else {
                    let shown = self.blob_on_screen(&key);
                    self.syntax.finish(key, None);
                    return shown;
                };
                let highlighter = self.syntax.highlighter();
                let inbox = self.inbox.clone();
                host.spawn_blocking(Box::new(move || {
                    super::syntax::highlight_steps(
                        &highlighter,
                        &path,
                        &text,
                        &token,
                        |highlighted, complete| {
                            inbox.push(Message::Highlighted {
                                key: key.clone(),
                                text: Some(highlighted),
                                complete,
                                token: token.clone(),
                            });
                        },
                    );
                }));
                false
            }
            Message::Highlighted {
                key,
                text,
                complete,
                token,
            } => {
                let shown = self.blob_on_screen(&key);
                self.syntax.land(key, text, complete, Some(&token));
                shown
            }
            _ => false,
        }
    }

    /// Whether a blob side of a file on screen has `key`.
    fn blob_on_screen(&self, key: &super::syntax::SyntaxKey) -> bool {
        self.review.as_ref().is_some_and(|review| {
            visible_blobs(review, &self.syntax)
                .iter()
                .any(|(wanted, _)| wanted == key)
        })
    }

    /// While idle, build the rows of the viewport and of the pages above and below it, and give
    /// the list exact layouts for them, so paging, scrolling, and highlights that just landed
    /// show rows already built instead of building them in the frame. Runs once per viewport,
    /// measurement generation, and presentation revision.
    fn prepare_ahead(&mut self) {
        let Some(review) = &self.review else {
            return;
        };
        let (resolved, generation, items, width) = {
            let list = review.list.borrow();
            let Some(resolved) = list.resolved() else {
                return;
            };
            let page = resolved.viewport_height;
            let items = list.index().visible_range(
                resolved.top_offset.saturating_sub(page),
                page.saturating_mul(3),
            );
            let width = list
                .index()
                .item(items.start)
                .and_then(|item| u16::try_from(item.width).ok());
            (
                resolved,
                list.measurement_generation(),
                items.start..items.end,
                width,
            )
        };
        let params = self.params(review);
        let stamp = [
            resolved.top_offset,
            resolved.viewport_height,
            generation,
            params.layout_revision ^ params.annotation_revision.rotate_left(17),
            self.syntax.epoch() ^ params.paint_revision.rotate_left(31),
        ];
        if review.prepared_ahead == Some(stamp) {
            return;
        }
        let extras = self.extras();
        let context = row_context(review, &params, &self.syntax, extras);
        if let Some(width) = width {
            for index in items {
                let _built = context.rows(index, width);
            }
        }
        let source =
            DocumentSource::new(&context, &review.interaction, decor(review, extras.search));
        let page = resolved.viewport_height;
        list_component(&source, &review.list, &params, &review.marks).prepare_around(
            page,
            page,
            &mut LayoutCx::new(),
        );
        if let Some(review) = &mut self.review {
            review.prepared_ahead = Some(stamp);
        }
    }

    /// Apply finished work. Returns whether anything visible changed.
    fn drain(&mut self, host: &UiContext) -> bool {
        let mut changed = false;
        for message in self.inbox.drain() {
            // Highlights for files off screen land without a frame; every other result (and a
            // highlight on screen) is drawn by the one frame that follows this drain.
            changed |= match message {
                Message::BlobText { .. } | Message::Highlighted { .. } => {
                    self.apply_syntax(message, host)
                }
                message => {
                    self.apply_message(message, host);
                    true
                }
            };
        }
        if let Some(review) = &mut self.review
            && let Some((key, row)) = review.pending_restore.clone()
            && let Some(index) = review.doc.index_of(&key)
        {
            review.pending_restore = None;
            // The persisted key may name a line in another layout's form: use the item's own.
            let key = review.doc.item(index).map_or(key, ReviewItem::key);
            review
                .list
                .borrow_mut()
                .set_position(ListPosition::Anchored(ListAnchor::row(
                    RowKey(key.clone()),
                    row,
                )));
            review.cursor_index = index;
            review.cursor = Some(key);
            changed = true;
        }
        if let Some(review) = &mut self.review
            && let Some(key) = review.pending_jump.clone()
            && review.doc.index_of(&key).is_some()
        {
            annotate::jump_to_annotation(review, &key);
            changed = true;
        }
        if let Some(review) = &mut self.review
            && let Some(motion) = review.motion.clone()
            && !matches!(
                review.doc.load_state(&motion.file),
                Some(
                    bcode_code_review::document::FileLoad::Unloaded
                        | bcode_code_review::document::FileLoad::Loading
                )
            )
        {
            review.motion = None;
            changed |= Self::run_motion(
                review,
                motion.origin,
                motion.kind,
                motion.direction,
                motion.repeats,
            );
        }
        changed
    }

    /// Apply one finished piece of work.
    fn apply_message(&mut self, message: Message, host: &UiContext) {
        match message {
            Message::Stage(text) => {
                if let Phase::Opening { stage } = &mut self.phase {
                    *stage = text;
                }
            }
            Message::Opened(opened) => self.apply_opened(*opened),
            Message::Arrived(arrived) => self.apply_arrived(*arrived),
            Message::SourceFailed(diagnostic) => self.apply_source_failed(diagnostic),
            Message::StackConfirmed(news) => self.apply_stack_news(*news),
            Message::Filling(note) => self.filling_note = note,
            Message::Filled {
                token,
                working_tree,
            } => {
                self.filling = false;
                self.filling_note = None;
                if let Some(review) = self.review.as_mut() {
                    review.token = token;
                    review.working_tree = working_tree;
                }
                // Every layer is in: how the stack changed lately is read for the layer strip.
                self.load_history_quietly(host);
            }
            Message::History(result) => self.apply_history(result),
            Message::HistoryPanel(result) => self.apply_history_panel(result),
            Message::HistoryCompared(result) => self.apply_history_compared(result),
            Message::OpenFailed(error) => {
                self.switching = false;
                if self.review.is_some() {
                    self.status = None;
                    self.toasts
                        .push(ToastSeverity::Error, "Refresh failed", Some(error));
                } else {
                    self.phase = Phase::Failed(error);
                }
                self.refreshing = false;
            }
            Message::Need {
                generation,
                ticket,
                result,
            } => self.apply_need(generation, ticket, result),
            message @ (Message::BlobText { .. } | Message::Highlighted { .. }) => {
                self.apply_syntax(message, host);
            }
            Message::Token { result, manual } => self.apply_token(host, result, manual),
            Message::Saved(result) => {
                // Changes made while saving are saved next.
                self.save = if self.save == SaveState::RunningThenPending {
                    SaveState::Pending
                } else {
                    SaveState::Idle
                };
                if let Err(error) = result {
                    self.toasts.push(
                        ToastSeverity::Error,
                        "Could not save the review state",
                        Some(error),
                    );
                }
            }
            Message::NotesLoaded { generation, result } => {
                if generation == self.generation {
                    match result {
                        Ok(loaded) => self.notes.load(loaded),
                        Err(error) => self.toasts.push(
                            ToastSeverity::Error,
                            "Could not load comments",
                            Some(error),
                        ),
                    }
                    self.sync_notes();
                }
            }
            Message::NoteDone(result) => self.finish_note(result),
            Message::Asked(result) => self.finish_ask(*result),
            Message::Feed(outcome) => self.finish_feed(outcome),
            Message::Dock(result) => self.finish_dock(*result, host),
            Message::SavedAs(result) => self.finish_save_review(*result),
            Message::Publish(result) => self.apply_publish(result),
            Message::Import(result) => self.apply_import(result),
            Message::Rereview(plan) => self.apply_rereview(*plan, host),
            Message::Carried(result) => self.apply_carried(result),
            Message::PathCompletions { generation, result } => {
                if let Some(composer) = self.composer.as_mut() {
                    composer.answer_completion(generation, result);
                }
            }
        }
    }

    /// Apply one service answer to the document that asked for it.
    fn apply_need(
        &mut self,
        generation: u64,
        ticket: bcode_code_review::document::Ticket,
        result: NeedResult,
    ) {
        self.in_flight = self.in_flight.saturating_sub(1);
        // Tickets restart with every document; results for a replaced one are stale.
        if generation != self.generation {
            return;
        }
        let Some(review) = &mut self.review else {
            return;
        };
        match result {
            NeedResult::Hunks(response) => {
                review.doc.accept_hunks(ticket, &response);
            }
            NeedResult::BlobLines(response) => {
                review.doc.accept_blob_lines(ticket, &response);
            }
            NeedResult::Attribution(response) => {
                review.doc.accept_attribution(ticket, *response);
            }
            NeedResult::Failed(error) => {
                review.doc.fail(ticket, &error);
            }
        }
        review.note_splices();
    }

    fn apply_opened(&mut self, mut opened: Opened) {
        let arrived = Instant::now();
        let was_filling = std::mem::replace(&mut self.filling, !opened.complete);
        let (first_open, deepest_stack) = (self.review.is_none(), deepest_stack(&opened));
        self.refreshing = false;
        self.generation += 1;
        let fingerprints = std::mem::take(&mut opened.fingerprints);
        // An unsaved review picks up the viewed state stored for its target when it first opens.
        let stored = opened.stored_viewed.take();
        if self.review.is_none() && self.is_transient() {
            let workspace = self.workspace_mut();
            if let Some(stored) = stored {
                workspace.viewed_files.extend(stored.viewed_files);
                workspace.viewed_marks.extend(stored.viewed_marks);
                workspace.viewed_marks.sort();
                workspace.viewed_marks.dedup();
            }
            let viewed = (
                workspace.viewed_files.clone(),
                workspace.viewed_marks.clone(),
            );
            self.transient_viewed = Some(viewed);
        }
        let mut review = Review::new(
            self.repo_path.clone(),
            opened,
            self.document_config,
            &self.theme,
        );
        review
            .files
            .set_hide_viewed(self.hide_viewed_files, &self.theme);
        if let Some(previous) = self.review.as_ref() {
            // Refresh: keep expansions, layout, the cursor, the top item, and the tree state.
            review.doc.restore_expansions(&previous.doc.expansions());
            review.doc.set_layout(previous.doc.layout());
            let _ = review.doc.take_splices();
            let top = previous
                .list
                .borrow()
                .position()
                .anchor()
                .map(|anchor| anchor.key.0.clone());
            if let Some(key) = top.and_then(|key| carried_key(&review.doc, &key)) {
                review
                    .list
                    .borrow_mut()
                    .set_position(ListPosition::Anchored(ListAnchor::top(RowKey(key))));
            }
            if let Some(cursor) = previous
                .cursor
                .clone()
                .and_then(|key| carried_key(&review.doc, &key))
            {
                review.cursor = Some(cursor);
                review.cursor_index = previous.cursor_index;
                review.fix_cursor();
            }
            review.gutter_digits = previous.gutter_digits;
            carry_tree(previous, &mut review, &self.theme);
            self.queue.clear();
            self.syntax.next_generation();
            // Filling in sources that were still loading is not a refresh.
            if !std::mem::take(&mut self.switching) && !was_filling {
                self.status = None;
                self.toasts
                    .push(ToastSeverity::Success, "Review refreshed", None);
            }
        }
        if self.since_last.is_none() {
            self.pass_revisions.clone_from(&review.revisions);
        }
        if !review.diagnostics.is_empty() {
            let body = review
                .diagnostics
                .iter()
                .map(|diagnostic| format!("{}: {}", diagnostic.code, diagnostic.message))
                .collect::<Vec<_>>()
                .join("; ");
            self.toasts
                .push(ToastSeverity::Warning, "Some sources failed", Some(body));
        }
        if first_open {
            self.warn_about_stack_size(deepest_stack);
        }
        for (source_id, answer) in &fingerprints {
            review.doc.accept_fingerprints(source_id, answer);
        }
        let workspace = self.workspace_mut().clone();
        review.doc.set_viewed_marks(&workspace);
        review.doc.set_carried_over(&self.carried);
        let _ = review.doc.take_splices();
        let previous = self.review.replace(review);
        self.phase = Phase::Ready;
        self.opened_at.get_or_insert(arrived);
        if previous.is_none() {
            self.restore_presentation();
        }
        // A refreshed document starts unfocused; show the stack as chosen again.
        self.apply_stack_view();
        // Annotations are re-applied to the new document (and re-listed by the next poll).
        self.notes_requested = false;
        self.sync_notes();
    }

    /// Fill in (or add) the group of a source that arrived after the first frame, in place: the
    /// cursor, the scroll position, and every other group stay as they are.
    fn apply_arrived(&mut self, arrived: loader::Arrived) {
        let order: Vec<&str> = self
            .sources
            .iter()
            .filter(|source| source.included)
            .map(|source| source.id.as_str())
            .collect();
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let id = arrived.input.source.source_id.clone();
        self.failed_sources.remove(&id);
        // Its place among the groups shown: after every group earlier in the workspace.
        let at = review
            .doc
            .groups()
            .filter(|group| {
                order
                    .iter()
                    .position(|source| *source == group.source.source_id)
                    .is_some_and(|index| index < arrived.position)
            })
            .count();
        if !arrived.input.loading {
            let revision = bcode_code_review_models::ReviewedRevision::from_resolved(
                &arrived.input.source,
                Some(annotate::now_ms()),
            );
            if revision.reviewed_head_oid.is_some() {
                review.revisions.insert(id.clone(), revision);
            }
        }
        if !review.doc.upsert_group(arrived.input, at) {
            return;
        }
        if let Some((source, answer)) = &arrived.fingerprints {
            review.doc.accept_fingerprints(source, answer);
        }
        review.doc.set_carried_over(&self.carried);
        review.totals = review.doc.totals();
        review.rebuild_files(&self.theme, self.hide_viewed_files);
        review.note_splices();
        self.apply_stack_view();
    }

    /// A stack the review follows was resolved or confirmed (or recalled): shown on the layer
    /// strip, and saved with the review so it opens at once next time.
    fn apply_stack_news(&mut self, news: bcode_code_review::session::StackNews) {
        self.stack_confirmed_at = Some(news.confirmed_at);
        self.stack_news = Some(news.clone());
        let confirmed_at_ms = news
            .confirmed_at
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|since| u64::try_from(since.as_millis()).ok())
            .unwrap_or(0);
        let snapshot = bcode_code_review_models::ReviewStackSnapshot {
            provider: news.provider,
            repository: news.repository,
            stack: news.stack,
            confirmed_at_ms,
        };
        let Some(workspace) = self.workspace.as_mut() else {
            return;
        };
        if workspace.stack_snapshots.contains(&snapshot) {
            return;
        }
        // A snapshot replaces the ones it shares a layer with.
        workspace.stack_snapshots.retain(|kept| {
            kept.provider != snapshot.provider
                || kept.repository != snapshot.repository
                || !kept.stack.layers.iter().any(|layer| {
                    snapshot
                        .stack
                        .layers
                        .iter()
                        .any(|new| new.pull.number == layer.pull.number)
                })
        });
        workspace.stack_snapshots.push(snapshot);
        self.request_save();
    }

    /// A source that failed after the first frame: its group stops loading and the layer strip
    /// and rail say it failed.
    fn apply_source_failed(&mut self, diagnostic: ReviewSourceDiagnostic) {
        self.toasts.push(
            ToastSeverity::Warning,
            format!("{} could not be opened", diagnostic.source_id),
            Some(format!("{}: {}", diagnostic.code, diagnostic.message)),
        );
        self.failed_sources.insert(diagnostic.source_id.clone());
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let failed = review
            .doc
            .groups()
            .find(|group| group.source.source_id == diagnostic.source_id && group.loading)
            .map(|group| {
                (
                    bcode_code_review::document::SourceInput {
                        source: group.source.clone(),
                        label: group.label.to_owned(),
                        entries: Vec::new(),
                        stack: group.stack.cloned(),
                        loading: false,
                    },
                    group.index,
                )
            });
        review.diagnostics.push(diagnostic);
        if let Some((input, at)) = failed
            && review.doc.upsert_group(input, at)
        {
            review.rebuild_files(&self.theme, self.hide_viewed_files);
            review.note_splices();
        }
    }

    fn apply_token(&mut self, host: &UiContext, result: Result<String, String>, manual: bool) {
        self.refreshing = false;
        let Some(review) = &mut self.review else {
            return;
        };
        match result {
            Ok(token) if review.token.as_deref() == Some(token.as_str()) => {
                if manual {
                    self.set_status("review is up to date", StatusSeverity::Success);
                }
            }
            Ok(_) => {
                self.set_status("refreshing review…", StatusSeverity::Info);
                self.reopen(host);
            }
            Err(error) => {
                if manual {
                    self.status = None;
                    self.toasts
                        .push(ToastSeverity::Error, "Refresh failed", Some(error));
                }
            }
        }
    }

    /// Open the review's current sources again, keeping position and expansions (refresh, and
    /// switching to or from the changes since the last review).
    fn reopen(&mut self, host: &UiContext) {
        self.refreshing = true;
        let services = self.services.clone();
        let repo = self.repo_path.clone();
        let sources = self.sources.clone();
        let inbox = self.inbox.clone();
        let viewed = self.viewed_inputs();
        let first = self.first_source();
        // A refresh keeps showing the review until every source is listed again: early,
        // incomplete answers go to a throwaway inbox.
        let cancel = self.opening.start();
        host.spawn(Box::pin(async move {
            let message = match loader::open(
                services,
                repo,
                sources,
                Inbox::default(),
                loader::OpenOptions {
                    viewed,
                    first,
                    stream: false,
                    cancel,
                },
            )
            .await
            {
                Ok(Some(opened)) => Message::Opened(Box::new(opened)),
                Ok(None) => return,
                Err(error) => Message::OpenFailed(error),
            };
            inbox.push(message);
        }));
    }

    // ----------------------------------------------------------------------------------------
    // Input
    // ----------------------------------------------------------------------------------------

    fn handle_key(&mut self, stroke: KeyStroke, host: &UiContext) -> Action {
        if self.modal.is_some() {
            return self.modal_key(stroke, host);
        }
        if self.publish.open {
            return self.publish_input(&Event::Key(stroke), host);
        }
        if let Some(overlay) = self.overlay.as_mut() {
            let outcome = overlay.key(stroke, &self.theme);
            return self.overlay_outcome(outcome, host);
        }
        match self.focus {
            Focus::Composer => return self.composer_key(stroke, host),
            Focus::Search => return self.search_key(stroke),
            Focus::Filter => return self.filter_key(stroke),
            Focus::Files => {
                if let Some(action) = self.tree_key(stroke, host) {
                    return action;
                }
            }
            Focus::Threads => {
                if let Some(action) = self.threads_key(stroke, host) {
                    return action;
                }
            }
            Focus::Assistant => return self.assistant_key(stroke, host),
            Focus::Review => {}
        }
        if let Some(action) = self.feed_key(stroke, host) {
            self.count = None;
            return action;
        }
        let action = host.keymap_action(SCOPE, stroke);
        let Some(action) = action else {
            if let Some(digit) = digit_of(stroke) {
                let count = self
                    .count
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(digit);
                self.count = (count > 0).then_some(count.min(MAX_COUNT));
                return Action::Redraw;
            }
            return Action::None;
        };
        let count = self.count.take();
        let action = action.strip_prefix("bcode.code_review.").unwrap_or(&action);
        if action != "exit" {
            self.exit_armed = false;
        }
        self.run_action(action, count, host)
    }

    /// Actions on the surface itself (panels, focus, presentation, refresh, exit).
    fn surface_action(
        &mut self,
        action: &str,
        count: Option<u32>,
        host: &UiContext,
    ) -> Option<Action> {
        if let Some(result) = self.review_action(action, host) {
            return Some(result);
        }
        Some(match action {
            "exit" => {
                // `esc` first clears a selection or a search.
                let clearing = self
                    .review
                    .as_ref()
                    .is_some_and(|review| review.selection.is_some() || review.drag.is_some())
                    || self.search.is_some();
                if clearing {
                    if let Some(review) = self.review.as_mut() {
                        review.selection = None;
                        review.drag = None;
                    }
                    self.clear_search();
                    return Some(Action::Redraw);
                }
                // An unsaved review with annotations offers saving once before leaving.
                if !std::mem::take(&mut self.exit_armed) && self.offers_save() {
                    self.exit_armed = true;
                    let save = self
                        .action_keys
                        .get("save_review")
                        .cloned()
                        .unwrap_or_else(|| "the palette".to_owned());
                    self.set_status(
                        format!(
                            "this review is not saved: {save} saves it with its comments; exit again to leave without saving"
                        ),
                        StatusSeverity::Warning,
                    );
                    return Some(Action::Redraw);
                }
                self.begin_close(host)
            }
            "save_review" => {
                self.save_review(host);
                Action::Redraw
            }
            "refresh" => {
                self.request_token(host, true);
                self.set_status("checking for changes…", StatusSeverity::Info);
                Action::Redraw
            }
            "layout_toggle" => {
                let current = self
                    .review
                    .as_ref()
                    .map_or(LayoutMode::Unified, |review| review.doc.layout());
                self.layout_override = Some(match current {
                    LayoutMode::Unified => LayoutMode::SideBySide,
                    LayoutMode::SideBySide => LayoutMode::Unified,
                });
                Action::Redraw
            }
            "wrap_toggle" => {
                self.overflow = match self.overflow {
                    DiffRowOverflow::Wrap => DiffRowOverflow::Truncate,
                    _ => DiffRowOverflow::Wrap,
                };
                self.layout_revision += 1;
                Action::Redraw
            }
            "focus_next" => {
                self.focus_next();
                Action::Redraw
            }
            "files_toggle" => {
                if !self.panels.toggle_files() && matches!(self.focus, Focus::Files | Focus::Filter)
                {
                    self.focus = Focus::Review;
                }
                self.persist_panels();
                Action::Redraw
            }
            "files_filter" => {
                if !self.files_shown() {
                    self.panels.show_files();
                    self.persist_panels();
                }
                self.focus = Focus::Filter;
                Action::Redraw
            }
            "threads_toggle" => {
                if !self.panels.toggle_threads() && self.focus == Focus::Threads {
                    self.focus = Focus::Review;
                }
                self.persist_panels();
                Action::Redraw
            }
            "files_grow" | "files_shrink" => {
                let step = RESIZE_STEP * i32::try_from(count.unwrap_or(1)).unwrap_or(1);
                let delta = if action == "files_grow" { step } else { -step };
                if self.panels.resize_files(delta) {
                    self.persist_panels();
                }
                Action::Redraw
            }
            _ => return None,
        })
    }

    /// Review actions: comments, threads, AI answers, viewed marks, selection, copying, search,
    /// the palette, and help.
    fn review_action(&mut self, action: &str, host: &UiContext) -> Option<Action> {
        Some(match action {
            "palette" | "help" => {
                self.open_overlay(action == "help", host);
                Action::Redraw
            }
            "search" => {
                self.start_search();
                Action::Redraw
            }
            "search_previous" => {
                if !self.search_next(Direction::Backward) {
                    self.set_status("no active search; press / first", StatusSeverity::Info);
                }
                Action::Redraw
            }
            "comment" | "ask" => {
                self.start_comment(action == "ask");
                Action::Redraw
            }
            "file_comment" | "review_comment" => {
                self.start_scoped_comment(action == "file_comment");
                Action::Redraw
            }
            "edit" | "delete" | "resolve" => {
                let on_agent = self.review.as_ref().is_some_and(|review| {
                    matches!(review.cursor, Some(ItemKey::AgentThread { .. }))
                });
                let result = match (action, on_agent) {
                    ("delete", true) => self.agent_action(super::rows::AgentAction::Delete),
                    ("edit", _) => self.thread_action(super::rows::ThreadAction::Edit),
                    ("delete", _) => self.thread_action(super::rows::ThreadAction::Delete),
                    _ => self.thread_action(super::rows::ThreadAction::Resolve),
                };
                result.unwrap_or(Action::Redraw)
            }
            "thread_kind" | "thread_severity" => {
                self.classify_thread(action == "thread_severity");
                Action::Redraw
            }
            "open_session" => self
                .agent_action(super::rows::AgentAction::Open)
                .unwrap_or(Action::Redraw),
            "viewed" => {
                self.toggle_viewed();
                Action::Redraw
            }
            "range" => {
                self.toggle_range();
                Action::Redraw
            }
            "copy" => {
                self.copy_lines(host);
                Action::Redraw
            }
            "publish" => {
                self.open_publish();
                Action::Redraw
            }
            "import" => {
                self.open_import(host);
                Action::Redraw
            }
            "layer_next" | "layer_previous" => {
                self.step_layer(action == "layer_next");
                Action::Redraw
            }
            "since_last_review" => {
                self.toggle_since_last(host);
                Action::Redraw
            }
            "complete_pass" => {
                self.complete_pass();
                Action::Redraw
            }
            _ => return self.assist_action(action, host),
        })
    }

    /// Folding, AI answers and suggestions, bulk viewed marks, menus, the report, and editing
    /// the review's sources.
    fn assist_action(&mut self, action: &str, host: &UiContext) -> Option<Action> {
        use bcode_code_review_models::ReviewSuggestionStatus;
        if let Some(line) = action
            .strip_prefix(super::palette::GOTO_LINE_PREFIX)
            .and_then(|line| line.parse::<u32>().ok())
        {
            self.goto_line(line);
            return Some(Action::Redraw);
        }
        if let Some(target) = action.strip_prefix(stack_files::STACK_FILE_PREFIX) {
            return Some(if self.jump_to_stack_file(target) {
                Action::Redraw
            } else {
                Action::None
            });
        }
        if let Some(command) = super::agent::AiCommand::from_action(action) {
            self.ai_command(command);
            return Some(Action::Redraw);
        }
        match action {
            "stack_history" => self.open_history(host),
            "stack_file" => self.open_file_through_stack(),
            "stack_overlaps" => self.open_stack_overlaps(),
            "fold" => self.fold(),
            "fold_all" => self.fold_all(),
            "resolved_toggle" => self.toggle_resolved(),
            "viewed_files_toggle" => self.toggle_viewed_files(),
            "copy_answer" => self.copy_answer(),
            "suggest_comment" => self.suggest_from_answer(),
            "draft_answer" => self.draft_from_answer(),
            "accept_suggestion" => self.decide_suggestion(ReviewSuggestionStatus::Accepted),
            "refine_suggestion" => self.decide_suggestion(ReviewSuggestionStatus::Refining),
            "reject_suggestion" => self.decide_suggestion(ReviewSuggestionStatus::Rejected),
            "retry" => {
                return Some(
                    self.agent_action(super::rows::AgentAction::Retry)
                        .unwrap_or(Action::Redraw),
                );
            }
            "feed_allow" => self.feed_action(super::rows::AgentAction::Allow),
            "feed_always" => self.feed_action(super::rows::AgentAction::Always),
            "feed_decline" => self.feed_action(super::rows::AgentAction::Decline),
            "feed_never" => self.feed_action(super::rows::AgentAction::Never),
            "feed_stop" => self.feed_action(super::rows::AgentAction::Stop),
            "view_all" => self.view_all(true),
            "unview_all" => self.view_all(false),
            "attention_next" => self.attention_next(),
            "copy_report" => self.copy_report(),
            "stack_view" => self.toggle_stack_view(),
            "dismiss_notifications" => {
                if !self.toasts.dismiss_all() {
                    self.set_status("No notifications", StatusSeverity::Info);
                }
            }
            "ai_menu" => self.open_ai_menu(host),
            "activate" => return Some(self.activate(host)),
            "goto_line" => {
                self.open_overlay(false, host);
                self.set_status(
                    "type a line number of this file, then enter",
                    StatusSeverity::Info,
                );
            }
            "edit_sources" => return Some(self.edit_sources(host)),
            "appearance" => {
                if !host.request_appearance_picker() {
                    self.set_status(
                        "the theme is set by the program hosting the review",
                        StatusSeverity::Info,
                    );
                }
            }
            _ => return None,
        }
        Some(Action::Redraw)
    }

    /// Actions on the document: cursor motions, scrolling, navigation, and expansion.
    fn document_action(&mut self, action: &str, count: Option<u32>) -> Action {
        if action == "next_file" && self.search_next(Direction::Forward) {
            return Action::Redraw;
        }
        let result = self.document_motion(action, count);
        if let Some(review) = self.review.as_mut()
            && let Some(selection) = review.selection.as_mut()
            && selection.follow
            && let Some(cursor) = review.cursor.clone()
        {
            // A keyboard range extends to the cursor (within the file it started in).
            if cursor.file() == selection.anchor.file()
                && matches!(cursor, ItemKey::Line { .. } | ItemKey::LinePair { .. })
            {
                selection.end = cursor;
            }
        }
        result
    }

    fn document_motion(&mut self, action: &str, count: Option<u32>) -> Action {
        let Some(review) = self.review.as_mut() else {
            return Action::None;
        };
        let repeat = count.unwrap_or(1) as usize;
        let viewport = review.viewport_height.max(1);
        match action {
            "down" => review.set_cursor(
                review.cursor_index.saturating_add(repeat),
                AnchorPolicy::EnsureVisibleFrom(Edge::Bottom),
            ),
            "up" => review.set_cursor(
                review.cursor_index.saturating_sub(repeat),
                AnchorPolicy::EnsureVisibleFrom(Edge::Top),
            ),
            "first" => review.set_cursor(0, AnchorPolicy::ItemTopAtTop),
            "last" => review.set_cursor(usize::MAX, AnchorPolicy::EnsureVisibleFrom(Edge::Bottom)),
            "half_page_down" | "half_page_up" | "page_down" | "page_up" => {
                let rows = if action.starts_with("half") {
                    (viewport / 2).max(1)
                } else {
                    viewport
                };
                let rows = i64::try_from(rows.saturating_mul(repeat as u64)).unwrap_or(i64::MAX);
                let rows = if action.ends_with("up") { -rows } else { rows };
                review.list.borrow_mut().scroll_by(rows, viewport);
                review.follow_viewport = true;
            }
            "next_file"
            | "previous_file"
            | "hunk_next"
            | "hunk_previous"
            | "change_next"
            | "change_previous"
            | "thread_next"
            | "thread_previous"
            | "unviewed_next"
            | "unviewed_previous"
            | "open_thread_next"
            | "open_thread_previous"
            | "ai_thread_next"
            | "ai_thread_previous" => {
                let (kind, direction) = match action {
                    "open_thread_next" => (NavKind::OpenThread, Direction::Forward),
                    "open_thread_previous" => (NavKind::OpenThread, Direction::Backward),
                    "ai_thread_next" => (NavKind::AiAnswer, Direction::Forward),
                    "ai_thread_previous" => (NavKind::AiAnswer, Direction::Backward),
                    "next_file" => (NavKind::File, Direction::Forward),
                    "previous_file" => (NavKind::File, Direction::Backward),
                    "hunk_next" => (NavKind::Hunk, Direction::Forward),
                    "hunk_previous" => (NavKind::Hunk, Direction::Backward),
                    "change_next" => (NavKind::Change, Direction::Forward),
                    "thread_next" => (NavKind::Thread, Direction::Forward),
                    "thread_previous" => (NavKind::Thread, Direction::Backward),
                    "unviewed_next" => (NavKind::UnviewedFile, Direction::Forward),
                    "unviewed_previous" => (NavKind::UnviewedFile, Direction::Backward),
                    _ => (NavKind::Change, Direction::Backward),
                };
                for _ in 0..repeat.min(1_000) {
                    if review.motion.is_some() {
                        break;
                    }
                    let Some(origin) = review.cursor.clone() else {
                        break;
                    };
                    if !Self::run_motion(review, origin, kind, direction, 0) {
                        break;
                    }
                }
            }
            "expand_down" | "expand_up" | "expand_all" | "expand_file" => {
                let lines = count.unwrap_or(EXPAND_STEP);
                Self::expand(review, action, lines);
            }
            _ => return Action::None,
        }
        Action::Redraw
    }

    fn files_shown(&self) -> bool {
        self.panels.current().is_some_and(|plan| plan.files)
    }

    /// Move focus to the next shown pane.
    fn focus_next(&mut self) {
        let plan = self.panels.current();
        let files = plan.is_some_and(|plan| plan.files);
        let threads = plan.is_some_and(|plan| plan.threads != super::panels::SidePlacement::Hidden);
        let assistant =
            plan.is_some_and(|plan| plan.assistant != super::panels::SidePlacement::Hidden);
        self.focus = match self.focus {
            Focus::Review if files => Focus::Files,
            Focus::Review | Focus::Files | Focus::Filter if threads => Focus::Threads,
            Focus::Review | Focus::Files | Focus::Filter | Focus::Threads if assistant => {
                Focus::Assistant
            }
            Focus::Composer | Focus::Search => self.focus,
            _ => Focus::Review,
        };
    }

    /// Keys of the file filter: editing, `enter`/`down` to the tree, `esc` clears.
    fn filter_key(&mut self, stroke: KeyStroke) -> Action {
        let plain = stroke.modifiers.is_empty();
        match stroke.key {
            KeyCode::Escape if plain => {
                if self.filter.borrow().buffer().text().is_empty() {
                    self.focus = Focus::Files;
                } else {
                    *self.filter.borrow_mut() =
                        TextInputState::new(bmux_text_edit::TextEditBuffer::new());
                    self.apply_filter();
                }
                return Action::Redraw;
            }
            KeyCode::Down | KeyCode::Tab if plain => {
                self.focus = Focus::Files;
                return Action::Redraw;
            }
            _ => {}
        }
        let outcome = TextInputControl::new(&self.filter_policy)
            .handle_key(&mut self.filter.borrow_mut(), stroke);
        match outcome {
            TextInputOutcome::Submitted => {
                self.focus = Focus::Files;
                self.select_first_match();
                Action::Redraw
            }
            TextInputOutcome::Edited => {
                self.apply_filter();
                Action::Redraw
            }
            // Every other key is typing context: never a review action.
            _ => Action::Redraw,
        }
    }

    fn apply_filter(&mut self) {
        let query = self.filter.borrow().buffer().text().to_string();
        let theme = self.theme;
        if let Some(review) = &mut self.review
            && review.files.set_filter(&query, &theme)
        {
            self.tree_scroll.set(ScrollViewState::new());
            self.tree_reveal = true;
        }
    }

    fn select_first_match(&mut self) {
        let Some(review) = &self.review else {
            return;
        };
        let first = review
            .files
            .items()
            .iter()
            .find(|item| review.files.file_of(&item.id).is_some())
            .map(|item| item.id.clone());
        if let Some(id) = first {
            review.tree.borrow_mut().set_selected_id(id.clone());
            self.tree_reveal = true;
            self.open_tree_item(&id, false, false);
        }
    }

    /// Keys of the file tree. Returns `None` for keys the tree does not use.
    fn tree_key(&mut self, stroke: KeyStroke, host: &UiContext) -> Option<Action> {
        let review = self.review.as_ref()?;
        let strip = |action: Option<String>| {
            action.map(|action| {
                action
                    .strip_prefix("bcode.code_review.")
                    .unwrap_or(&action)
                    .to_owned()
            })
        };
        // The panel's own keys come first; motions are the review's.
        let panel = strip(host.keymap_action(FILES_SCOPE, stroke));
        let action = strip(host.keymap_action(SCOPE, stroke));
        let selected = review.tree.borrow().selected_id().map(str::to_owned);
        let rows = i64::from(
            self.body_area(TREE_VIEWPORT_ID)
                .map_or(10, |area| area.height.max(1)),
        );
        let count = i64::from(self.count.unwrap_or(1));
        let delta = match (panel.as_deref(), action.as_deref()) {
            (Some(_), _) => None,
            (_, Some("down")) => Some(count),
            (_, Some("up")) => Some(-count),
            (_, Some("half_page_down")) => Some(rows / 2),
            (_, Some("half_page_up")) => Some(-rows / 2),
            (_, Some("page_down")) => Some(rows),
            (_, Some("page_up")) => Some(-rows),
            (_, Some("first")) => Some(i64::MIN / 2),
            (_, Some("last")) => Some(i64::MAX / 2),
            _ => None,
        };
        if let Some(delta) = delta {
            self.count = None;
            self.move_tree(delta);
            return Some(Action::Redraw);
        }
        if panel.as_deref() == Some("files_back") {
            self.focus = Focus::Review;
            return Some(Action::Redraw);
        }
        let selected = selected?;
        let expandable = review.files.is_expandable(&selected);
        let expanded = review.tree.borrow().is_expanded(&selected);
        match panel.as_deref() {
            Some("files_open") => {
                self.open_tree_item(&selected, true, true);
                Some(Action::Redraw)
            }
            Some("files_fold") if expandable => {
                review.tree.borrow_mut().toggle_expanded(&selected);
                self.tree_reveal = true;
                Some(Action::Redraw)
            }
            Some("files_expand") if expandable && !expanded => {
                review.tree.borrow_mut().set_expanded(&selected, true);
                Some(Action::Redraw)
            }
            Some("files_collapse") => {
                if expandable && expanded {
                    review.tree.borrow_mut().set_expanded(&selected, false);
                } else if let Some(parent) = review.files.ancestors(&selected).last() {
                    review.tree.borrow_mut().set_selected_id(*parent);
                    self.tree_reveal = true;
                }
                Some(Action::Redraw)
            }
            Some(_) => Some(Action::None),
            None => None,
        }
    }

    /// Move the tree selection by `delta` visible rows; a file row moves the document cursor.
    fn move_tree(&mut self, delta: i64) {
        let Some(review) = &self.review else {
            return;
        };
        let id = {
            let tree = review.tree_component();
            let count = tree.visible_count();
            if count == 0 {
                return;
            }
            let last = i64::try_from(count - 1).unwrap_or(i64::MAX);
            let next = tree.selected_visible().map_or(0, |row| {
                i64::try_from(row)
                    .unwrap_or(0)
                    .saturating_add(delta)
                    .clamp(0, last)
            });
            let next = usize::try_from(next).unwrap_or(0);
            tree.visible_source(next)
                .map(|source| review.files.items()[source].id.clone())
        };
        let Some(id) = id else {
            return;
        };
        review.tree.borrow_mut().set_selected_id(id.clone());
        self.tree_reveal = true;
        self.open_tree_item(&id, false, false);
    }

    /// Act on tree row `id`: a file moves the document cursor to its header (and focuses the
    /// document when `focus_review`); a directory toggles when `toggle`.
    fn open_tree_item(&mut self, id: &str, focus_review: bool, toggle: bool) {
        let Some(review) = &mut self.review else {
            return;
        };
        if let Some(file) = review.files.file_of(id).cloned() {
            if let Some(index) = review.doc.index_of(&ItemKey::FileHeader(file.clone())) {
                review.set_cursor(index, AnchorPolicy::ItemTopAtTop);
            }
            self.synced_file = Some(file);
            if focus_review {
                self.focus = Focus::Review;
            }
        } else if toggle && review.files.is_expandable(id) {
            review.tree.borrow_mut().toggle_expanded(id);
        }
    }

    /// Keys of the threads panel.
    fn threads_key(&mut self, stroke: KeyStroke, host: &UiContext) -> Option<Action> {
        let review = self.review.as_mut()?;
        let strip = |action: Option<String>| {
            action.map(|action| {
                action
                    .strip_prefix("bcode.code_review.")
                    .unwrap_or(&action)
                    .to_owned()
            })
        };
        let panel = strip(host.keymap_action(THREADS_SCOPE, stroke));
        let action = strip(host.keymap_action(SCOPE, stroke));
        let delta: i64 = match (panel.as_deref(), action.as_deref()) {
            (Some(filter @ ("threads_filter_next" | "threads_filter_previous")), _) => {
                self.threads_filter = if filter == "threads_filter_next" {
                    self.threads_filter.next()
                } else {
                    self.threads_filter.previous()
                };
                review.threads_state.borrow_mut().set_selected_visible(None);
                return Some(Action::Redraw);
            }
            (Some("threads_open" | "threads_back"), _) => {
                self.focus = Focus::Review;
                return Some(Action::Redraw);
            }
            (Some(_), _) => return Some(Action::None),
            (_, Some("down")) => 1,
            (_, Some("up")) => -1,
            _ => return None,
        };
        let count = review.threads.borrow().keys.len();
        if count == 0 {
            return Some(Action::None);
        }
        let current = review.threads_state.borrow().selected_visible();
        let last = i64::try_from(count - 1).unwrap_or(0);
        let next = current.map_or(0, |row| {
            i64::try_from(row)
                .unwrap_or(0)
                .saturating_add(delta)
                .clamp(0, last)
        });
        let next = usize::try_from(next).unwrap_or(0);
        review
            .threads_state
            .borrow_mut()
            .set_selected_visible(Some(next));
        let key = review.threads.borrow().keys.get(next).cloned();
        if let Some(key) = key {
            annotate::jump_to_annotation(review, &key);
        }
        Some(Action::Redraw)
    }

    /// Run one navigation motion from `motion.origin`. A target inside a non-resident file moves
    /// the cursor to that file's header and records the motion to repeat once it loads.
    fn run_motion(
        review: &mut Review,
        origin: ItemKey,
        kind: NavKind,
        direction: Direction,
        repeats: u8,
    ) -> bool {
        let Some(from) = review.doc.index_of(&origin) else {
            return false;
        };
        let Some(target) = review.doc.navigate(from, kind, direction) else {
            return false;
        };
        let policy = match kind {
            NavKind::Change | NavKind::Thread | NavKind::OpenThread | NavKind::AiAnswer => {
                AnchorPolicy::Center
            }
            NavKind::File | NavKind::Hunk | NavKind::UnviewedFile => AnchorPolicy::ItemTopAtTop,
        };
        review.set_cursor(target.index, policy);
        if let Some(file) = target.needs_load
            && repeats < MAX_MOTION_REPEATS
        {
            review.motion = Some(PendingMotion {
                origin,
                kind,
                direction,
                file,
                repeats: repeats + 1,
            });
        }
        true
    }

    fn expand(review: &mut Review, action: &str, lines: u32) {
        let Some(cursor) = review.cursor.clone() else {
            return;
        };
        let changed = match (action, &cursor) {
            ("expand_file", _) => review
                .doc
                .item(review.cursor_index)
                .and_then(ReviewItem::file)
                .cloned()
                .is_some_and(|file| review.doc.reveal_file(&file)),
            ("expand_down", ItemKey::Gap { file, start }) => {
                let end = match review.doc.item(review.cursor_index) {
                    Some(ReviewItem::Gap(gap)) => gap.end,
                    _ => None,
                };
                let changed = review.doc.reveal_below(&cursor, lines);
                // The rest of the gap now starts below the revealed lines; keep the cursor on it
                // (only when part of this gap is left: another gap may start there).
                let rest_start = start.saturating_add(lines);
                let rest = ItemKey::Gap {
                    file: file.clone(),
                    start: rest_start,
                };
                if changed
                    && end.is_none_or(|end| rest_start <= end)
                    && review.doc.index_of(&rest).is_some()
                {
                    review.cursor = Some(rest);
                }
                changed
            }
            ("expand_up", ItemKey::Gap { .. }) => review.doc.reveal_above(&cursor, lines),
            ("expand_all", ItemKey::Gap { .. }) => review.doc.reveal_all(&cursor),
            _ => false,
        };
        if changed {
            review.note_splices();
        }
    }

    fn apply_gap_click(review: &mut Review, gap: &ItemKey, action: GapAction) {
        if let Some(index) = review.doc.index_of(gap) {
            review.cursor_index = index;
            review.cursor = Some(gap.clone());
        }
        let changed = match action {
            GapAction::Up => review.doc.reveal_above(gap, EXPAND_STEP),
            GapAction::Down => review.doc.reveal_below(gap, EXPAND_STEP),
            GapAction::All => review.doc.reveal_all(gap),
        };
        if changed {
            review.note_splices();
        }
    }

    fn publish_menu_action(&mut self, id: &str, host: &UiContext) -> Action {
        match id {
            chrome::PUBLISH_MENU_IMPORT => self.open_import(host),
            chrome::PUBLISH_MENU_COMPLETE => self.complete_pass(),
            chrome::PUBLISH_MENU_SINCE_LAST => self.toggle_since_last(host),
            _ => self.open_publish(),
        }
        Action::Redraw
    }

    fn publish_input(&mut self, event: &Event, host: &UiContext) -> Action {
        let Some((row, plan)) = &self.header else {
            self.publish.close();
            return Action::Redraw;
        };
        let Some(button) = plan.area(HeaderPart::Publish) else {
            self.publish.close();
            return Action::Redraw;
        };
        let button = Rect::new(row.x + button.x, row.y, button.width, 1);
        let outcome =
            chrome::publish_event(&self.theme, &mut self.publish, button, self.screen, event);
        match outcome {
            SplitButtonOutcome::Ignored => Action::None,
            SplitButtonOutcome::Primary => {
                self.open_publish();
                Action::Redraw
            }
            SplitButtonOutcome::Selected { id, .. } => self.publish_menu_action(&id, host),
            _ => Action::Redraw,
        }
    }

    /// Route a mouse event to the toasts, which are painted over the body (under a modal or an
    /// overlay). `None` when they do not take it.
    fn toast_mouse(&mut self, event: &Event) -> Option<Action> {
        (self.modal.is_none() && self.overlay.is_none() && self.toasts.mouse(event))
            .then_some(Action::Redraw)
    }

    fn handle_mouse(&mut self, event: &Event, host: &UiContext) -> Action {
        let Event::Mouse(mouse) = event else {
            return Action::None;
        };
        if self.modal.is_some() {
            return self.modal_mouse(event, host);
        }
        if let Some(overlay) = self.overlay.as_mut() {
            let outcome = overlay.mouse(event, &self.theme);
            return self.overlay_outcome(outcome, host);
        }
        if let Some(action) = self.side_mouse(event, *mouse) {
            return action;
        }
        let wheel = match mouse.kind {
            MouseEventKind::ScrollDown => 3,
            MouseEventKind::ScrollUp => -3,
            _ => 0,
        };
        if wheel != 0
            && self
                .list_area()
                .is_some_and(|area| area.contains(mouse.position))
        {
            return if self.wheel(wheel) {
                Action::Redraw
            } else {
                Action::None
            };
        }
        let header_hit = self
            .header
            .as_ref()
            .is_some_and(|(row, _)| row.contains(mouse.position));
        if self.publish.open || header_hit {
            let action = self.publish_input(event, host);
            if action != Action::None || self.publish.open {
                return action;
            }
        }
        let pressed = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
        let in_files = self
            .body_area(FILES_PANE_ID)
            .is_some_and(|area| area.contains(mouse.position));
        if pressed {
            self.focus_at(mouse.position);
            // The file panel's hide action.
            if in_files && self.files_action_at(mouse.position) {
                self.panels.toggle_files();
                self.focus = Focus::Review;
                self.persist_panels();
                return Action::Redraw;
            }
        }
        let tree_viewport = self.body_area(TREE_VIEWPORT_ID);
        let in_list = self
            .list_area()
            .is_some_and(|area| area.contains(mouse.position));
        let Some(routed) = self.route_body(event) else {
            return Action::None;
        };
        let mut redraw = routed.outcome != EventOutcome::Ignored;
        redraw |= self.after_body_event(&routed, mouse.kind);
        // Wheel over the file panel scrolls the tree.
        let rows = match mouse.kind {
            MouseEventKind::ScrollDown => TREE_WHEEL_ROWS,
            MouseEventKind::ScrollUp => -TREE_WHEEL_ROWS,
            _ => 0,
        };
        if rows != 0
            && in_files
            && let Some(viewport) = tree_viewport
        {
            redraw |= self.scroll_tree(rows, viewport.height);
        }
        let (clicked, host_action) = self.after_list_event(*mouse, in_list);
        redraw |= clicked;
        if let Some(action) = host_action {
            return action;
        }
        let Some(review) = self.review.as_mut() else {
            return Action::None;
        };
        let actions: Vec<VirtualListAction<RowKey>> =
            review.list.borrow_mut().drain_actions().collect();
        for action in actions {
            if let VirtualListAction::Activated(key) = action
                && let Some(index) = review.doc.index_of(&key.0)
            {
                review.cursor_index = index;
                review.cursor = Some(key.0);
            }
        }
        if in_list && routed.outcome == EventOutcome::Redraw {
            review.follow_viewport = true;
        }
        if redraw || pressed {
            Action::Redraw
        } else {
            Action::None
        }
    }

    /// A mouse event the stack rail or the assistant dock handles.
    fn side_mouse(&mut self, event: &Event, mouse: bmux_tui::event::MouseEvent) -> Option<Action> {
        if self.rail_mouse(event) {
            return Some(Action::Redraw);
        }
        self.assistant_mouse(mouse)
    }

    /// Give keyboard focus to the pane under `position`.
    fn focus_at(&mut self, position: bmux_tui::geometry::Point) {
        let inside = |id: &str| {
            self.body_area(id)
                .is_some_and(|area| area.contains(position))
        };
        let focus = if inside(FILTER_ID) {
            Some(Focus::Filter)
        } else if inside(FILES_PANE_ID) {
            Some(Focus::Files)
        } else if inside(THREADS_PANE_ID) {
            Some(Focus::Threads)
        } else if inside(ASSISTANT_PANE_ID) {
            Some(Focus::Assistant)
        } else if inside(REVIEW_PANE_ID) || inside(LIST_ID) {
            Some(Focus::Review)
        } else {
            None
        };
        if let Some(focus) = focus {
            self.focus = focus;
        }
    }

    /// Route a mouse event through the body laid out by the last frame, recording the state it
    /// may change.
    fn route_body(&self, event: &Event) -> Option<RoutedMouse> {
        let review = self.review.as_ref()?;
        let frame = review.frame.as_ref()?;
        let params = self.params(review);
        let snapshot = |review: &Review| PanelSnapshot {
            tree: review.tree.borrow().clone(),
            thread: review.threads_state.borrow().selected_visible(),
            root: self.panels.root.borrow().sizes().to_vec(),
            column: self.panels.column.borrow().sizes().to_vec(),
        };
        let before = snapshot(review);
        let extras = self.extras();
        let outcome = {
            let context = row_context(review, &params, &self.syntax, extras);
            let source =
                DocumentSource::new(&context, &review.interaction, decor(review, extras.search));
            let list = list_component(&source, &review.list, &params, &review.marks);
            let body = self.body(
                review,
                ListSlot::Ready(Box::new(ReadyList {
                    list,
                    prepared: None,
                })),
            );
            let mut cx = EventCx::new(&frame.layout);
            let area = frame.body;
            cx.with_transform(0, 0, i32::from(area.x), i64::from(area.y), area, |cx| {
                body.event(event, &frame.layout, cx)
            })
        };
        Some(RoutedMouse {
            outcome,
            before,
            after: snapshot(review),
        })
    }

    /// Apply what a routed mouse event changed: tree and threads selections move the document,
    /// and divider drags persist panel sizes. Returns whether anything changed.
    fn after_body_event(&mut self, routed: &RoutedMouse, kind: MouseEventKind) -> bool {
        let (before, after) = (&routed.before, &routed.after);
        let mut changed = false;
        if after.tree != before.tree {
            changed = true;
            // A click on a file row moves the document there (even when already selected).
            let released = matches!(kind, MouseEventKind::Up(MouseButton::Left));
            let selected = after.tree.selected_id().map(str::to_owned);
            if let Some(id) = selected
                && (released || Some(id.as_str()) != before.tree.selected_id())
            {
                self.open_tree_item(&id, false, false);
            }
        }
        if after.thread != before.thread
            && let Some(row) = after.thread
            && let Some(review) = self.review.as_mut()
        {
            changed = true;
            let key = review.threads.borrow().keys.get(row).cloned();
            if let Some(key) = key {
                annotate::jump_to_annotation(review, &key);
            }
        }
        if (&after.root, &after.column) != (&before.root, &before.column) {
            changed = true;
            if self.panels.note_resize() {
                self.persist_panels();
            }
        }
        changed
    }

    /// Scroll the file tree by `rows` within a viewport of `height` rows.
    fn scroll_tree(&self, rows: i64, height: u16) -> bool {
        let Some(review) = self.review.as_ref() else {
            return false;
        };
        let content = u64::try_from(review.tree_component().visible_count()).unwrap_or(0);
        let mut state = self.tree_scroll.get();
        let max = content.saturating_sub(u64::from(height));
        let offset = state.vertical_offset();
        let next = if rows < 0 {
            offset.saturating_sub(rows.unsigned_abs())
        } else {
            offset.saturating_add(rows.unsigned_abs())
        }
        .min(max);
        if next == offset {
            return false;
        }
        state.set_vertical_offset(next);
        self.tree_scroll.set(state);
        true
    }

    /// Whether `position` is on the file panel's hide action, from the pane's own title layout.
    fn files_action_at(&self, position: bmux_tui::geometry::Point) -> bool {
        let (Some(review), Some(area)) = (self.review.as_ref(), self.body_area(FILES_PANE_ID))
        else {
            return false;
        };
        let pane = self.files_pane_chrome(review);
        pane.title_layout(Rect::new(0, 0, area.width, area.height))
            .actions
            .iter()
            .any(|(_, rect)| {
                Rect::new(area.x + rect.x, area.y + rect.y, rect.width, rect.height)
                    .contains(position)
            })
    }

    // ----------------------------------------------------------------------------------------
    // Rendering
    // ----------------------------------------------------------------------------------------

    fn apply_theme(&mut self, theme: Option<&ComponentStyles>) {
        if self.theme_input.as_ref() == theme && self.paint_revision > 0 {
            return;
        }
        self.theme_input = theme.copied();
        self.theme = self.config.theme(theme);
        if let Some(theme) = theme {
            self.syntax.set_palette(theme.syntax);
        }
        self.threads_empty = vec![Line::from_spans(vec![Span::styled(
            "Comment threads and AI answers appear here.",
            self.theme.muted,
        )])];
        if let Some(review) = &mut self.review {
            review.files.rebuild_items(&self.theme);
            review.threads.borrow_mut().revision = None;
        }
        self.paint_revision += 1;
    }

    fn render_frame(&mut self, area: Rect, cx: &mut PaintCx<'_, '_>) {
        self.paint_frame(area, cx);
    }

    fn paint_frame(&mut self, area: Rect, cx: &mut PaintCx<'_, '_>) {
        self.screen = area;
        cx.fill(
            LocalRect::new(area.x.into(), area.y.into(), area.width, area.height),
            " ",
            self.theme.canvas,
        );
        if area.width == 0 || area.height == 0 {
            if let Some(review) = &mut self.review {
                review.frame = None;
            }
            self.header = None;
            return;
        }
        let header = area.height >= 3;
        let footer = area.height >= 2;
        // The stack rail takes a row under the header when the body keeps at least 6 rows.
        let rail = header
            && matches!(self.phase, Phase::Ready)
            && area.height >= 9
            && !self.rail_entries().is_empty();
        // In the every-layer view of a stack, a strip under the rail names the cursor's layer.
        let strip = rail && area.height >= 10 && self.shows_layer_strip();
        let top = u16::from(header) + u16::from(rail) + u16::from(strip);
        let body = Rect::new(
            area.x,
            area.y + top,
            area.width,
            area.height - top - u16::from(footer),
        );
        if rail {
            let bar = self.theme.header_bar;
            cx.with_style(bar, |cx| {
                self.paint_rail(Rect::new(area.x, area.y + 1, area.width, 1), cx);
            });
            if strip {
                self.paint_layer_strip(Rect::new(area.x, area.y + 2, area.width, 1), cx);
            }
        } else {
            self.rail = None;
        }
        if matches!(self.phase, Phase::Ready) {
            self.render_body(body, cx);
        } else {
            self.paint_loading(body, cx);
        }
        if header {
            let bar = self.theme.header_bar;
            cx.with_style(bar, |cx| {
                self.paint_header(Rect::new(area.x, area.y, area.width, 1), cx);
            });
        } else {
            self.header = None;
        }
        if footer {
            let bar = self.theme.footer;
            cx.with_style(bar, |cx| {
                self.paint_footer(Rect::new(area.x, area.bottom() - 1, area.width, 1), cx);
            });
        }
        self.toasts.paint(&self.theme, body, cx);
        let theme = self.theme;
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.paint(&theme, area, cx);
        }
        match self.modal.as_mut() {
            Some(Modal::Publish(flow)) => flow.paint(&theme, area, cx),
            Some(Modal::Import(flow)) => flow.paint(&theme, area, cx),
            Some(Modal::History(panel)) => panel.paint(&theme, area, cx),
            None => {}
        }
        if self.publish.open
            && let Some((row, plan)) = &self.header
            && let Some(button) = plan.area(HeaderPart::Publish)
        {
            chrome::paint_publish_menu(
                &self.theme,
                &self.publish,
                Rect::new(row.x + button.x, row.y, button.width, 1),
                area,
                cx,
            );
        }
    }

    fn paint_loading(&self, area: Rect, cx: &mut PaintCx<'_, '_>) {
        let elapsed = self
            .started
            .map_or(Duration::ZERO, |started| started.elapsed());
        let (title, body) = match &self.phase {
            Phase::Failed(error) => ("Could not open the review", error.clone()),
            Phase::Opening { stage } => ("Opening review…", stage.clone()),
            _ => ("Opening review…", "opening".to_owned()),
        };
        let spinner = if self.theme.icons.ellipsis.is_ascii() {
            Spinner::ascii()
        } else {
            Spinner::braille()
        };
        let icon = match self.phase {
            Phase::Failed(_) => "!".to_owned(),
            _ => spinner.frame(elapsed).to_owned(),
        };
        let body = [Line::from_spans(vec![Span::styled(
            body,
            if matches!(self.phase, Phase::Failed(_)) {
                self.theme.error
            } else {
                self.theme.muted
            },
        )])];
        let state = EmptyStateComponent::new("review.loading", title)
            .icon(&icon)
            .body(&body)
            .policy(EmptyStatePolicy::centered())
            .styles(self.theme.component.empty_state_styles());
        cx.paint_component(&state, area);
    }

    fn header_model(&self) -> HeaderModel {
        let (viewed, totals, mode) = self.review.as_ref().map_or_else(
            || ((0, 0), (0, 0), String::new()),
            |review| {
                let progress = review.doc.viewed_progress();
                let mut mode = match review.doc.layout() {
                    LayoutMode::Unified => "unified".to_owned(),
                    LayoutMode::SideBySide => "split".to_owned(),
                };
                if self.overflow != DiffRowOverflow::Wrap {
                    mode.push_str(self.theme.icons.separator);
                    mode.push_str("nowrap");
                }
                ((progress.viewed, progress.total), review.totals, mode)
            },
        );
        HeaderModel {
            title: self.title.clone(),
            // A single source named like the review adds nothing as a chip.
            sources: self
                .sources
                .iter()
                .filter(|source| source.included)
                .map(|source| source.label.clone())
                .filter(|label| self.sources.len() > 1 || *label != self.title)
                .collect(),
            viewed,
            totals,
            drafts: self.notes.draft_count(),
            unsaved: self.is_transient(),
            mode,
        }
    }

    fn paint_header(&mut self, area: Rect, cx: &mut PaintCx<'_, '_>) {
        let mut model = self.header_model();
        if self.review.is_none() {
            // Until the review opens only the title shows.
            let plan = HeaderPlan {
                parts: vec![(HeaderPart::Title, Rect::new(0, 0, area.width, 1))],
            };
            model.sources.clear();
            chrome::paint_header(&model, &plan, &self.theme, &self.publish, area, cx);
            self.header = None;
            return;
        }
        let plan = chrome::plan_header(&model, area.width, &self.theme);
        chrome::paint_header(&model, &plan, &self.theme, &self.publish, area, cx);
        self.header = Some((area, plan));
    }

    /// Contextual key hints `(key, label)` for the focused pane and the cursor item.
    fn hints(&self) -> Vec<(String, String)> {
        let key = |action: &str| self.action_keys.get(action).cloned();
        let mut hints: Vec<(String, String)> = Vec::new();
        let mut push = |key: Option<String>, label: &str| {
            if let Some(key) = key {
                hints.push((key, label.to_owned()));
            }
        };
        match self.focus {
            Focus::Composer => {
                push(key("composer_submit"), "save");
                push(Some("enter".to_owned()), "submit");
                push(Some("shift+enter".to_owned()), "newline");
                push(key("composer_preview"), "preview");
                push(key("composer_switch"), "ask/comment");
                push(key("composer_kind"), "kind");
                push(key("composer_severity"), "severity");
                push(key("composer_cancel"), "cancel");
            }
            Focus::Search => {
                push(Some("enter".to_owned()), "done");
                push(Some("esc".to_owned()), "clear");
            }
            Focus::Filter => {
                push(Some("enter".to_owned()), "apply");
                push(Some("esc".to_owned()), "clear");
                push(Some("↓".to_owned()), "files");
            }
            Focus::Files => {
                push(key("down"), "move");
                push(key("files_open"), "open");
                push(key("files_fold"), "fold");
                push(key("files_filter"), "filter");
                push(key("viewed_files_toggle"), "hide viewed");
                push(key("files_toggle"), "hide");
                push(key("focus_next"), "next pane");
                push(key("files_back"), "back");
            }
            Focus::Assistant => {
                for (key, label) in self.assistant_hints() {
                    push(key, label);
                }
            }
            Focus::Threads => {
                push(key("down"), "move");
                push(key("threads_open"), "open");
                push(key("threads_filter_next"), "filter");
                push(key("focus_next"), "next pane");
                push(key("threads_back"), "back");
            }
            Focus::Review => {
                let cursor = self
                    .review
                    .as_ref()
                    .and_then(|review| review.cursor.as_ref());
                let feed = self.feed_hints();
                for (key, label) in &feed {
                    push(Some(key.clone()), label);
                }
                if feed.is_empty() {
                    for (action, label) in item_hints(cursor) {
                        push(key(action), label);
                    }
                }
                let on_gap = self
                    .review
                    .as_ref()
                    .and_then(|review| review.cursor.as_ref())
                    .is_some_and(|cursor| matches!(cursor, ItemKey::Gap { .. }));
                if on_gap {
                    push(key("expand_down"), "expand ↓");
                    push(key("expand_up"), "expand ↑");
                    push(key("expand_all"), "all");
                }
                push(key("next_file"), "file");
                push(key("hunk_next"), "hunk");
                push(key("change_next"), "change");
                if !on_gap {
                    push(key("expand_file"), "whole file");
                }
                push(key("viewed"), "viewed");
                push(key("search"), "search");
                push(key("palette"), "commands");
                push(key("help"), "help");
                push(key("files_toggle"), "files");
                push(key("focus_next"), "focus");
                push(key("layout_toggle"), "layout");
                push(key("wrap_toggle"), "wrap");
                push(key("refresh"), "refresh");
                if self.is_transient() {
                    push(key("save_review"), "save review");
                }
                push(key("exit"), "quit");
            }
        }
        hints
    }

    fn paint_footer(&self, area: Rect, cx: &mut PaintCx<'_, '_>) {
        if let Some(search) = self.search.as_ref().filter(|_| self.focus == Focus::Search) {
            self.paint_search_bar(search, area, cx);
            return;
        }
        let search = self
            .search
            .as_ref()
            .map(|search| (search.query(), search.status()))
            .filter(|(query, _)| !query.is_empty())
            .map(|(query, status)| Status {
                text: format!("/{query} {status}"),
                severity: StatusSeverity::Info,
            });
        let status = match (self.status.as_ref().or(search.as_ref()), self.count) {
            (Some(status), Some(count)) => Some(Status {
                text: format!("{count} · {}", status.text),
                severity: status.severity,
            }),
            (Some(status), None) => Some(status.clone()),
            (None, Some(count)) => Some(Status {
                text: count.to_string(),
                severity: StatusSeverity::Info,
            }),
            (None, None) => None,
        };
        chrome::paint_footer(&self.hints(), status.as_ref(), &self.theme, area, cx);
    }

    /// The search input in the footer row: a prompt, the BMUX text input, and the match status
    /// on the right (at most half the row).
    fn paint_search_bar(&self, search: &SearchState, area: Rect, cx: &mut PaintCx<'_, '_>) {
        let theme = &self.theme;
        cx.fill(
            LocalRect::new(area.x.into(), area.y.into(), area.width, area.height),
            " ",
            theme.footer,
        );
        let status = search.status();
        let status = Line::from_spans(vec![Span::styled(status, theme.muted)])
            .truncate(usize::from(area.width / 2));
        let status_width = u16::try_from(status.width()).unwrap_or(0);
        let prompt = Line::from_spans(vec![Span::styled("/", theme.accent)]);
        cx.write_line(
            LocalRect::new(area.x.into(), area.y.into(), area.width.min(1), 1),
            &prompt,
        );
        if status_width > 0 {
            cx.write_line(
                LocalRect::new(
                    i32::from(area.right() - status_width),
                    area.y.into(),
                    status_width,
                    1,
                ),
                &status,
            );
        }
        let input_width = area.width.saturating_sub(2 + status_width);
        if input_width == 0 {
            return;
        }
        let input = TextInputComponent::new("review.search", &search.input, &search.policy)
            .style(theme.input)
            .placeholder("search the review", theme.placeholder)
            .focused(true);
        let node = input.layout(
            Constraints::tight(Size::new(input_width, 1)),
            &mut LayoutCx::new(),
        );
        cx.with_child(
            i32::from(area.x + 1),
            i64::from(area.y),
            LocalRect::new(0, 0, input_width, 1),
            |cx| input.paint(&node, cx),
        );
    }

    /// Open the command palette or the keymap help.
    fn open_overlay(&mut self, help: bool, host: &UiContext) {
        let keys = |scope: &'static str| move |id: &str| host.keymap_keys(scope, id);
        let review = crate::keymap::actions_for(SCOPE);
        let entries = if help {
            let mut entries = super::palette::entries(&review, keys(SCOPE), Some("Review"));
            entries.extend(super::palette::entries(
                &crate::keymap::actions_for(annotate::COMPOSER_SCOPE),
                keys(annotate::COMPOSER_SCOPE),
                Some("Comment composer"),
            ));
            entries.extend(super::palette::entries(
                &crate::keymap::actions_for(FEED_SCOPE),
                keys(FEED_SCOPE),
                Some("AI card"),
            ));
            entries.extend(super::palette::entries(
                &crate::keymap::actions_for(ASSISTANT_SCOPE),
                keys(ASSISTANT_SCOPE),
                Some("Review assistant"),
            ));
            entries.extend(super::palette::entries(
                &crate::keymap::actions_for(FILES_SCOPE),
                keys(FILES_SCOPE),
                Some("File panel"),
            ));
            entries.extend(super::palette::entries(
                &crate::keymap::actions_for(THREADS_SCOPE),
                keys(THREADS_SCOPE),
                Some("Threads panel"),
            ));
            entries
        } else {
            super::palette::entries(&review, keys(SCOPE), None)
        };
        let kind = if help {
            super::palette::OverlayKind::Help
        } else {
            super::palette::OverlayKind::Palette
        };
        self.overlay = Some(Overlay::new(kind, entries, &self.theme));
    }

    /// Apply what an input did to the overlay.
    fn overlay_outcome(
        &mut self,
        outcome: super::palette::OverlayOutcome,
        host: &UiContext,
    ) -> Action {
        use super::palette::OverlayOutcome;
        match outcome {
            OverlayOutcome::Close => {
                self.overlay = None;
                Action::Redraw
            }
            OverlayOutcome::Run(action) => {
                self.overlay = None;
                let action = action.strip_prefix("bcode.code_review.").unwrap_or(&action);
                match action.strip_prefix("composer_") {
                    Some(_) if self.composer.is_none() => Action::Redraw,
                    Some(_) => {
                        // Composer actions run as their key would.
                        let keys = host.keymap_keys(
                            annotate::COMPOSER_SCOPE,
                            &format!("bcode.code_review.{action}"),
                        );
                        keys.first()
                            .and_then(|key| bmux_keyboard::parse_key_stroke(key).ok())
                            .map_or(Action::Redraw, |stroke| self.composer_key(stroke, host))
                    }
                    None => {
                        let result = self.run_action(action, None, host);
                        if result == Action::None {
                            Action::Redraw
                        } else {
                            result
                        }
                    }
                }
            }
            OverlayOutcome::Redraw => Action::Redraw,
            OverlayOutcome::Ignored => Action::None,
        }
    }

    /// Run one review action (from a key, the palette, or help).
    fn run_action(&mut self, action: &str, count: Option<u32>, host: &UiContext) -> Action {
        if action == "assistant_toggle" {
            return self.toggle_assistant(host);
        }
        if let Some(result) = self.surface_action(action, count, host) {
            return result;
        }
        self.document_action(action, count)
    }

    /// Lay out, prepare, and paint the body: panels, the file tree, and the document list.
    fn render_body(&mut self, body: Rect, cx: &mut PaintCx<'_, '_>) {
        let plan = self.panels.sync(body.width, body.height);
        // Focus never stays on a pane that is not shown.
        let shown = match self.focus {
            Focus::Files | Focus::Filter => plan.files,
            Focus::Threads => plan.threads != super::panels::SidePlacement::Hidden,
            Focus::Assistant => plan.assistant != super::panels::SidePlacement::Hidden,
            Focus::Review | Focus::Composer | Focus::Search => true,
        };
        if !shown {
            self.focus = Focus::Review;
        }
        // Apply rebuilt file headers to the tree.
        if let Some(review) = &mut self.review {
            for file in std::mem::take(&mut review.dirty_files) {
                review.files.refresh(&review.doc, &file, &self.theme);
            }
        }
        self.sync_thread_rows();
        let constraints = Constraints::tight(body.size());
        // Pass 1: learn the document's size from a probe in its place.
        let probe = Cell::new(None);
        let (list_size, tree_rows) = {
            let Some(review) = self.review.as_ref() else {
                return;
            };
            let layout = self
                .body(review, ListSlot::Probe(&probe))
                .layout(constraints, &mut LayoutCx::new());
            let tree_rows = layout
                .find(&LayoutId::new(TREE_VIEWPORT_ID))
                .map(|node| node.size.height);
            if let Some(node) = layout.find(&LayoutId::new(dock::ASSISTANT_FEED_ID)) {
                self.prepare_assistant(u16::try_from(node.size.width).unwrap_or(u16::MAX));
            }
            (probe.get().unwrap_or(Size::new(0, 0)), tree_rows)
        };
        let prepared = self.prepare_list(list_size);
        self.sync_tree_to_cursor();
        if let Some(rows) = tree_rows {
            self.reveal_tree_selection(rows);
        }
        // Pass 2: lay out with the prepared document list, paint, and keep the layout.
        let params = match &self.review {
            Some(review) => self.params(review),
            None => return,
        };
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let extras = self.extras();
        let layout = {
            let context = row_context(review, &params, &self.syntax, extras);
            let source =
                DocumentSource::new(&context, &review.interaction, decor(review, extras.search));
            let list = list_component(&source, &review.list, &params, &review.marks);
            let body_component = self.body(
                review,
                ListSlot::Ready(Box::new(ReadyList {
                    list,
                    prepared: prepared.map(|node| (list_size, node)),
                })),
            );
            let layout = body_component.layout(constraints, &mut LayoutCx::new());
            cx.with_child(
                i32::from(body.x),
                i64::from(body.y),
                LocalRect::new(0, 0, body.width, body.height),
                |cx| body_component.paint(&layout, cx),
            );
            layout
        };
        let list_area = layout
            .find_rect(&LayoutId::new(LIST_ID))
            .map_or_else(Rect::default, |rect| {
                Rect::new(body.x + rect.x, body.y + rect.y, rect.width, rect.height)
            });
        if !list_area.is_empty() {
            // The document's diff text is one selection scope; rows register their fragments.
            cx.push_selection_scope(bmux_tui::selection::SelectionScope::new(
                super::rows::SELECTION_SCOPE,
                list_area,
            ));
        }
        if let Some(review) = self.review.as_mut() {
            review.frame = Some(FrameState {
                body,
                layout,
                list_area,
            });
        }
    }

    /// Bring the document list to `size`: layout mode, splices, cursor reveal, viewport
    /// following, and the visible range reported to the document. Returns the list layout.
    fn prepare_list(&mut self, size: Size) -> Option<LayoutNode> {
        let policy = self.policy();
        let review = self.review.as_mut()?;
        pointer::advance_motion(review);
        // Layout mode follows the toggle, else the policy at this width.
        let mode = self
            .layout_override
            .unwrap_or_else(|| resolve(policy, size.width));
        if review.doc.layout() != mode {
            switch_layout(review, mode);
            self.layout_revision += 1;
        }
        review.note_splices();
        let review = self.review.as_ref()?;
        let params = self.params(review);
        let height = u64::from(size.height);
        let review = self.review.as_mut()?;
        review.viewport_height = height;
        if size.height == 0 || size.width == 0 {
            return None;
        }
        refresh_marks(review, &params);
        let constraints = Constraints::tight(size);
        let syntax = &self.syntax;
        let extras = Extras {
            notes: &self.notes,
            agents: &self.agents,
            folds: &self.folds,
            composer: self.composer.as_ref(),
            search: self.search.as_ref().map(|search| &search.highlights),
        };
        // Lay out (twice when the cursor must be revealed: the reveal needs the synced index).
        let mut layout = layout_list(review, &params, syntax, extras, constraints);
        if let Some(policy) = review.reveal.take()
            && let Some(cursor) = review.cursor.clone()
        {
            let position = {
                let context = row_context(review, &params, syntax, extras);
                let source = DocumentSource::new(
                    &context,
                    &review.interaction,
                    decor(review, extras.search),
                );
                list_component(&source, &review.list, &params, &review.marks).anchor_for(
                    &RowKey(cursor),
                    policy,
                    height,
                )
            };
            if let Some(position) = position {
                review.list.borrow_mut().set_position(position);
                layout = layout_list(review, &params, syntax, extras, constraints);
            }
        }
        if review.follow_viewport {
            review.follow_viewport = false;
            follow_cursor(review);
        }
        // Report the visible range; the next poll serves what it needs.
        let visible = visible_range(review);
        review.visible = visible;
        review.doc.set_viewport(visible.0, visible.1);
        review.note_splices();
        Some(layout)
    }

    /// Select the cursor's file in the tree when the cursor moved to another file.
    fn sync_tree_to_cursor(&mut self) {
        let Some(review) = &self.review else {
            return;
        };
        let file = review.cursor_file();
        if file == self.synced_file {
            return;
        }
        self.synced_file.clone_from(&file);
        let Some(file) = file else {
            return;
        };
        let Some(id) = review.files.file_id(&file).map(str::to_owned) else {
            return;
        };
        // Under a collapsed directory, the outermost collapsed directory stands for the file.
        let target = if review.files.filter().is_empty() {
            let tree = review.tree.borrow();
            review
                .files
                .ancestors(&id)
                .into_iter()
                .find(|ancestor| !tree.is_expanded(ancestor))
                .map_or(id, str::to_owned)
        } else {
            id
        };
        review.tree.borrow_mut().set_selected_id(target);
        self.tree_reveal = true;
    }

    /// Scroll the tree so its selected row is inside a viewport of `rows` rows.
    fn reveal_tree_selection(&mut self, rows: u64) {
        if !self.tree_reveal || rows == 0 {
            return;
        }
        self.tree_reveal = false;
        let Some(review) = &self.review else {
            return;
        };
        let Some(row) = review.tree_component().selected_visible() else {
            return;
        };
        let row = u64::try_from(row).unwrap_or(0);
        let mut state = self.tree_scroll.get();
        let offset = state.vertical_offset();
        let next = if row < offset {
            row
        } else if row >= offset + rows {
            row + 1 - rows
        } else {
            offset
        };
        if next != offset {
            state.set_vertical_offset(next);
            self.tree_scroll.set(state);
        }
    }

    const fn pane_styles(&self) -> PaneStyles {
        PaneStyles {
            background: None,
            border: self.theme.border,
            focused_border: self.theme.border_focused,
        }
    }

    fn files_pane_chrome(&self, review: &Review) -> Pane<'static> {
        let theme = &self.theme;
        let files = &review.files;
        let count = if files.filter().is_empty() {
            files.file_count().to_string()
        } else {
            format!("{}/{}", files.matches(), files.file_count())
        };
        let focused = matches!(self.focus, Focus::Files | Focus::Filter);
        Pane::new()
            .title(theme.pane_title("Files", focused))
            .right_title(theme.pane_status(&count))
            .action(PaneTitleAction::new(
                "collapse",
                Line::from_spans(vec![Span::styled(
                    format!(" {} ", theme.icons.collapse),
                    theme.muted,
                )]),
            ))
            .styles(self.pane_styles())
            .chrome(theme.component.chrome)
    }

    fn pane<'a>(
        id: &'static str,
        chrome: Pane<'a>,
        focused: bool,
        state: &'a Cell<PaneState>,
        child: impl Component + 'a,
    ) -> PaneComponent<'a, 'a> {
        let mut current = state.get();
        current.interaction.focused = focused;
        state.set(current);
        PaneComponent::new(id, chrome, state, child)
    }

    /// The body component: panels and their contents, with `list` as the document.
    fn body<'a>(&'a self, review: &'a Review, list: ListSlot<'a>) -> Element<'a> {
        let theme = &self.theme;
        let Some(plan) = self.panels.current() else {
            return Element::new(list);
        };
        if !plan.bordered {
            return Element::new(list);
        }
        let review_pane = Self::pane(
            REVIEW_PANE_ID,
            Pane::new()
                .title(theme.pane_title("Review", self.focus == Focus::Review))
                .right_title(theme.pane_status(&{
                    let files = review.files.file_count();
                    format!("{files} file{}", if files == 1 { "" } else { "s" })
                }))
                .styles(self.pane_styles())
                .chrome(theme.component.chrome),
            self.focus == Focus::Review,
            &self.review_pane,
            list,
        );
        let mut review_pane = Some(review_pane);
        let mut root = PanelGroupComponent::new(
            ROOT_ID,
            super::panels::group(PanelGroupAxis::Horizontal, theme.panels),
            &self.panels.root,
        );
        for member in self.panels.root_members() {
            match member {
                PanelId::Files => root = root.child(self.files_pane(review)),
                PanelId::Review => {
                    let Some(pane) = review_pane.take() else {
                        continue;
                    };
                    root = if self.panels.column_members().len() > 1 {
                        let mut column = PanelGroupComponent::new(
                            COLUMN_ID,
                            super::panels::group(PanelGroupAxis::Vertical, theme.panels),
                            &self.panels.column,
                        )
                        .child(pane);
                        for member in &self.panels.column_members()[1..] {
                            column = match member {
                                PanelId::Threads => column.child(self.threads_pane(review)),
                                PanelId::Assistant => column.child(self.assistant_pane()),
                                PanelId::Files | PanelId::Review => column,
                            };
                        }
                        root.child(column)
                    } else {
                        root.child(pane)
                    };
                }
                PanelId::Threads => root = root.child(self.threads_pane(review)),
                PanelId::Assistant => root = root.child(self.assistant_pane()),
            }
        }
        Element::new(root)
    }

    fn files_pane<'a>(&'a self, review: &'a Review) -> PaneComponent<'a, 'a> {
        let theme = &self.theme;
        let focused = matches!(self.focus, Focus::Files | Focus::Filter);
        let tree = review
            .tree_component()
            .styles(if self.focus == Focus::Files {
                theme.tree_focused
            } else {
                theme.tree
            })
            .focus_style(Style::new())
            .selection_bar(theme.tree_selection_bar());
        let viewport = ScrollViewComponent::new(
            TREE_VIEWPORT_ID,
            LogicalSize::new(0, 0),
            ScrollViewState::new(),
            tree,
        )
        .fill()
        .retain_state(&self.tree_scroll);
        let filter = TextInputComponent::new(FILTER_ID, &self.filter, &self.filter_policy)
            .style(theme.input)
            .placeholder("filter files", theme.placeholder)
            .focused(self.focus == Focus::Filter);
        let prompt = TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
            Span::styled(theme.icons.filter, theme.accent),
            Span::raw(" "),
        ])]));
        let content = Column::new()
            .id("review.files.content")
            .child(
                Row::new()
                    .id("review.files.filter_row")
                    .child(prompt)
                    .flex(Flex::new(1, filter)),
            )
            .flex(Flex::new(1, viewport));
        Self::pane(
            FILES_PANE_ID,
            self.files_pane_chrome(review),
            focused,
            &self.files_pane,
            content,
        )
        .border_scrollbar(TREE_VIEWPORT_ID, &self.tree_scroll)
    }

    fn assistant_pane(&self) -> PaneComponent<'_, '_> {
        let theme = &self.theme;
        let focused = self.focus == Focus::Assistant;
        let status = self
            .dock
            .session()
            .and_then(|session| self.agents.state(session))
            .map(|state| {
                let mut parts = Vec::new();
                if state.needs_you() {
                    parts.push("needs you".to_owned());
                } else if state.working {
                    parts.push("working…".to_owned());
                }
                if state.feed.tokens > 0 {
                    parts.push(super::agent::feed::tokens_label(state.feed.tokens));
                }
                if let Some(cost) = &state.feed.cost {
                    parts.push(cost.clone());
                }
                parts.join(" · ")
            })
            .unwrap_or_default();
        let chrome = Pane::new()
            .title(theme.pane_title("Assistant", focused))
            .right_title(theme.pane_status(&status))
            .styles(self.pane_styles())
            .chrome(theme.component.chrome);
        // The chips and the prompt hint clip rather than wrap: each is one row.
        let chips = TextBlock::new(Text::from_lines(vec![self.assistant_chips()]))
            .wrap(bmux_tui::text::TextWrap::None);
        let mut content = Column::new()
            .id("review.assistant.content")
            .child(chips)
            .flex(Flex::new(1, self.assistant_feed()));
        if self.assistant_prompt_pending() {
            let hint = self
                .assistant_hints()
                .into_iter()
                .filter_map(|(key, label)| key.map(|key| format!("{key} {label}")))
                .collect::<Vec<_>>()
                .join(" · ");
            content = content.child(
                TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
                    Span::styled(hint, theme.pill_warning),
                ])]))
                .wrap(bmux_tui::text::TextWrap::None),
            );
        } else {
            let prompt = TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
                Span::styled(theme.icons.accent_bar, theme.accent),
                Span::raw(" "),
            ])]));
            let input = TextInputComponent::new(
                dock::ASSISTANT_INPUT_ID,
                self.dock.input(),
                self.dock.policy(),
            )
            .style(theme.input)
            .placeholder("Ask about this review…", theme.placeholder)
            .focused(focused);
            content = content.child(
                Row::new()
                    .id("review.assistant.input_row")
                    .child(prompt)
                    .flex(Flex::new(1, input)),
            );
        }
        Self::pane(ASSISTANT_PANE_ID, chrome, focused, &self.dock.pane, content)
    }

    fn threads_pane<'a>(&'a self, review: &'a Review) -> PaneComponent<'a, 'a> {
        let theme = &self.theme;
        let threads = review.threads.borrow();
        let count = threads.keys.len();
        drop(threads);
        let chrome = Pane::new()
            .title(theme.pane_title("Threads", self.focus == Focus::Threads))
            .right_title(theme.pane_status(&count.to_string()))
            .styles(self.pane_styles())
            .chrome(theme.component.chrome);
        let focused = self.focus == Focus::Threads;
        if count == 0 {
            let empty = EmptyStateComponent::new("review.threads.empty", "No threads yet")
                .icon(theme.icons.thread)
                .body(&self.threads_empty)
                .policy(EmptyStatePolicy::centered())
                .styles(theme.component.empty_state_styles());
            return Self::pane(THREADS_PANE_ID, chrome, focused, &self.threads_pane, empty);
        }
        let list = ThreadsList(
            review,
            if focused {
                theme.tree_focused
            } else {
                theme.tree
            },
            theme.tree_selection_bar(),
        );
        Self::pane(THREADS_PANE_ID, chrome, focused, &self.threads_pane, list)
    }
}

/// The threads list: a flat BMUX tree over the cached thread rows.
struct ThreadsList<'a>(
    &'a Review,
    bmux_tui_components::tree_view::TreeViewStyles,
    TreeViewSelectionBar,
);

impl Component for ThreadsList<'_> {
    fn revision(&self) -> ComponentRevision {
        let threads = self.0.threads.borrow();
        TreeViewComponent::new(THREADS_TREE_ID, &threads.items, &self.0.threads_state)
            .styles(self.1)
            .selection_bar(self.2)
            .revision()
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        let threads = self.0.threads.borrow();
        TreeViewComponent::new(THREADS_TREE_ID, &threads.items, &self.0.threads_state)
            .styles(self.1)
            .selection_bar(self.2)
            .layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let threads = self.0.threads.borrow();
        TreeViewComponent::new(THREADS_TREE_ID, &threads.items, &self.0.threads_state)
            .styles(self.1)
            .selection_bar(self.2)
            .focus_style(Style::new())
            .paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let threads = self.0.threads.borrow();
        TreeViewComponent::new(THREADS_TREE_ID, &threads.items, &self.0.threads_state)
            .styles(self.1)
            .selection_bar(self.2)
            .event(event, layout, cx)
    }
}

/// The document's place in the body: a probe that records the size it is given, or the list,
/// whose layout was prepared at that size.
enum ListSlot<'a> {
    Probe(&'a Cell<Option<Size>>),
    Ready(Box<ReadyList<'a>>),
}

/// The document list and, when it was prepared, its layout at the size it was prepared for.
struct ReadyList<'a> {
    list: VirtualListComponent<'a, RowKey, DocumentSource<'a>>,
    prepared: Option<(Size, LayoutNode)>,
}

impl Component for ListSlot<'_> {
    fn revision(&self) -> ComponentRevision {
        match self {
            Self::Probe(_) => ComponentRevision::new(0, 0),
            Self::Ready(ready) => ready.list.revision(),
        }
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        let size = Size::new(
            u16::try_from(constraints.max_width()).unwrap_or(u16::MAX),
            u16::try_from(constraints.max_height().unwrap_or(0)).unwrap_or(u16::MAX),
        );
        match self {
            Self::Probe(probe) => {
                probe.set(Some(size));
                LayoutNode::leaf(
                    LayoutId::new(LIST_ID),
                    constraints.constrain(LogicalSize::new(
                        constraints.max_width(),
                        constraints.max_height().unwrap_or(0),
                    )),
                )
            }
            Self::Ready(ready) => match &ready.prepared {
                Some((prepared, node))
                    if *prepared == size && constraints == Constraints::tight(size) =>
                {
                    node.clone()
                }
                _ => ready.list.layout(constraints, cx),
            },
        }
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if let Self::Ready(ready) = self {
            ready.list.paint(layout, cx);
        }
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        match self {
            Self::Probe(_) => EventOutcome::Ignored,
            Self::Ready(ready) => ready.list.event(event, layout, cx),
        }
    }
}

/// Highlight keys (with grammar paths) of both sides of every resident text file intersecting
/// the viewport, in viewport order.
fn visible_blobs(review: &Review, syntax: &SyntaxStore) -> Vec<(super::syntax::SyntaxKey, String)> {
    let (first, last) = review.visible;
    let mut wanted: Vec<(super::syntax::SyntaxKey, String)> = Vec::new();
    let mut seen: Option<&FileRef> = None;
    for index in first..=last {
        let Some(file) = review.doc.item(index).and_then(ReviewItem::file) else {
            continue;
        };
        if seen == Some(file) {
            continue;
        }
        seen = Some(file);
        if review.doc.load_state(file) != Some(bcode_code_review::document::FileLoad::Loaded) {
            continue;
        }
        let Some(entry) = review.doc.entry(file) else {
            continue;
        };
        if entry.has(ReviewIndexFlag::Binary) || entry.has(ReviewIndexFlag::Submodule) {
            continue;
        }
        for (blob, path) in [
            (
                &entry.old_blob,
                entry.old_path.as_deref().unwrap_or(&entry.path),
            ),
            (&entry.new_blob, entry.path.as_str()),
        ] {
            if let Some(key) = syntax.key(blob, path)
                && !wanted.iter().any(|(wanted, _)| *wanted == key)
            {
                wanted.push((key, path.to_owned()));
            }
        }
    }
    wanted
}

/// Carry the tree's collapsed directories, selection, and filter into a re-opened review.
fn carry_tree(previous: &Review, review: &mut Review, theme: &ReviewTheme) {
    let old = previous.tree.borrow();
    let collapsed: BTreeSet<&str> = previous
        .files
        .expandable_ids()
        .filter(|id| !old.is_expanded(id))
        .collect();
    let mut tree = review.tree.borrow_mut();
    tree.set_expanded_ids(
        review
            .files
            .expandable_ids()
            .filter(|id| !collapsed.contains(id))
            .map(str::to_owned),
    );
    if let Some(selected) = old.selected_id() {
        tree.set_selected_id(selected.to_owned());
    }
    drop(tree);
    let filter = previous.files.filter().to_owned();
    review.files.set_filter(&filter, theme);
}

fn list_component<'a>(
    source: &'a DocumentSource<'a>,
    state: &'a RefCell<VirtualListState<RowKey>>,
    params: &ViewParams,
    marks: &'a [ScrollbarMark<RowKey>],
) -> VirtualListComponent<'a, RowKey, DocumentSource<'a>> {
    VirtualListComponent::new(LIST_ID, source, state)
        .chrome(
            VirtualListChrome::interactive()
                .keyboard(false)
                .mouse_wheel(false)
                .scrollbar_styles(params.theme.scrollbar),
        )
        .sticky_headers(StickyHeaders::enabled())
        .scrollbar_marks(marks)
}

fn layout_list(
    review: &Review,
    params: &ViewParams,
    syntax: &SyntaxStore,
    extras: Extras<'_>,
    constraints: Constraints,
) -> LayoutNode {
    let context = row_context(review, params, syntax, extras);
    let source = DocumentSource::new(&context, &review.interaction, decor(review, extras.search));
    list_component(&source, &review.list, params, &review.marks)
        .layout(constraints, &mut LayoutCx::new())
}

/// Switch the document layout, keeping the top item and the cursor on the same lines.
fn switch_layout(review: &mut Review, mode: LayoutMode) {
    let (top, row) = {
        let list = review.list.borrow();
        list.position().anchor().map_or((None, 0), |anchor| {
            let row = match anchor.offset {
                bmux_tui_components::virtual_list::AnchorOffset::Row(row) => row,
                bmux_tui_components::virtual_list::AnchorOffset::Source { row_delta, .. } => {
                    row_delta
                }
            };
            (Some(anchor.key.0.clone()), row)
        })
    };
    let cursor = review.cursor.clone();
    review.doc.set_layout(mode);
    review.note_splices();
    if let Some(key) = top.and_then(|key| review.doc.map_key(&key)) {
        let row = if matches!(key, ItemKey::Line { .. } | ItemKey::LinePair { .. }) {
            0
        } else {
            row
        };
        review
            .list
            .borrow_mut()
            .set_position(ListPosition::Anchored(ListAnchor::row(RowKey(key), row)));
    }
    if let Some(key) = cursor.and_then(|key| review.doc.map_key(&key)) {
        review.cursor = Some(key);
        review.fix_cursor();
    }
}

/// `key` in a re-opened document: the same item when it exists, else its file's header (line
/// content is not resident in a fresh document).
/// Layers of the deepest stack among `opened`'s sources, when any source is a stack.
fn deepest_stack(opened: &Opened) -> Option<usize> {
    opened
        .inputs
        .iter()
        .filter_map(|input| input.stack.as_ref())
        .map(|stack| stack.stack.layers.len())
        .max()
}

fn carried_key(doc: &ReviewDocument, key: &ItemKey) -> Option<ItemKey> {
    doc.map_key(key)
        .filter(|key| doc.index_of(key).is_some())
        .or_else(|| {
            let header = ItemKey::FileHeader(key.file()?.clone());
            doc.index_of(&header).map(|_| header)
        })
}

/// Inclusive range of items intersecting the last layout's viewport.
fn visible_range(review: &Review) -> (usize, usize) {
    let list = review.list.borrow();
    let Some(resolved) = list.resolved() else {
        return (0, 0);
    };
    let range = list
        .index()
        .visible_range(resolved.top_offset, resolved.viewport_height.max(1));
    let first = range.start;
    (first, range.end.saturating_sub(1).max(first))
}

/// After a viewport scroll, move the cursor into the viewport rows below the pinned headers
/// when it left them (the cursor highlight is paint-only, so the layout stays valid).
fn follow_cursor(review: &mut Review) {
    let target = {
        let list = review.list.borrow();
        let Some(resolved) = list.resolved() else {
            return;
        };
        let top = resolved.top_offset + list.sticky_header_rows();
        let bottom = resolved.top_offset + resolved.viewport_height.saturating_sub(1);
        let index = list.index();
        let Some(position) = review
            .cursor
            .as_ref()
            .and_then(|key| review.doc.index_of(key))
        else {
            return;
        };
        let offset = index.item_offset(position).unwrap_or(0);
        if (top..=bottom).contains(&offset) {
            return;
        }
        index.item_at_offset(if offset < top { top } else { bottom })
    };
    if let Some(index) = target {
        review.cursor_index = index;
        review.cursor = review.doc.item(index).map(ReviewItem::key);
    }
}

/// Recompute scrollbar marks when the document or the cursor's file changed: the current file,
/// and every thread when the document has annotations.
fn refresh_marks(review: &mut Review, params: &ViewParams) {
    let file = review
        .doc
        .item(review.cursor_index)
        .and_then(ReviewItem::file)
        .map(|file| ItemKey::FileHeader(file.clone()));
    let stamp = (review.doc.revision(), file.clone());
    if review.marks_revision.as_ref() == Some(&stamp) {
        return;
    }
    review.marks_revision = Some(stamp);
    let mut marks = Vec::new();
    if review.doc.annotation_count() > 0 {
        let mut from = 0;
        while marks.len() < 512 {
            let Some(target) = review
                .doc
                .navigate(from, NavKind::Thread, Direction::Forward)
            else {
                break;
            };
            marks.push(ScrollbarMark::item(
                RowKey(target.key.clone()),
                params.theme.mark_thread,
            ));
            let next = if target.needs_load.is_some() {
                target.index + 1
            } else {
                target.index
            };
            if next <= from && from != 0 {
                break;
            }
            from = next.max(from + usize::from(next == from));
        }
    }
    if let Some(file) = file {
        let end = review
            .doc
            .navigate(review.cursor_index, NavKind::File, Direction::Forward)
            .map_or_else(
                || review.doc.len().saturating_sub(1),
                |next| next.index.saturating_sub(1),
            );
        if let Some(last) = review.doc.item(end).map(ReviewItem::key) {
            marks.push(ScrollbarMark::new(
                bmux_tui_components::virtual_list::ScrollbarMarkPosition::Items {
                    first: RowKey(file),
                    last: RowKey(last),
                },
                params.theme.mark_file,
            ));
        }
    }
    review.marks = marks;
}

fn digit_of(stroke: KeyStroke) -> Option<u32> {
    if !(stroke.modifiers.is_empty()
        || stroke.modifiers
            == (bmux_keyboard::Modifiers {
                shift: true,
                ..bmux_keyboard::Modifiers::NONE
            }))
    {
        return None;
    }
    match stroke.key {
        KeyCode::Char(character) => character.to_digit(10),
        _ => None,
    }
}

/// Hints of the cursor item's own actions, as `(action, label)`.
const fn item_hints(cursor: Option<&ItemKey>) -> &'static [(&'static str, &'static str)] {
    match cursor {
        Some(ItemKey::Thread { .. }) => &[
            ("activate", "actions"),
            ("comment", "reply"),
            ("edit", "edit"),
            ("delete", "delete"),
            ("resolve", "resolve"),
            ("fold", "fold"),
            ("thread_kind", "kind"),
            ("thread_severity", "severity"),
        ],
        Some(ItemKey::AgentThread { .. }) => &[
            ("activate", "actions"),
            ("open_session", "open in Bcode"),
            ("ask", "follow up"),
            ("fold", "more"),
            ("copy_answer", "copy"),
            ("suggest_comment", "suggest"),
            ("accept_suggestion", "accept"),
            ("delete", "delete"),
        ],
        Some(ItemKey::Line { .. } | ItemKey::LinePair { .. }) => &[
            ("comment", "comment"),
            ("ask", "ask Bcode"),
            ("ai_menu", "AI commands"),
            ("range", "select"),
        ],
        _ => &[],
    }
}

/// Actions whose first key the footer hints show.
const HINT_ACTIONS: &[&str] = &[
    "since_last_review",
    "publish",
    "comment",
    "ask",
    "range",
    "edit",
    "delete",
    "resolve",
    "thread_kind",
    "thread_severity",
    "open_session",
    "viewed",
    "search",
    "palette",
    "help",
    "down",
    "next_file",
    "hunk_next",
    "change_next",
    "expand_down",
    "expand_up",
    "expand_all",
    "expand_file",
    "files_toggle",
    "files_filter",
    "focus_next",
    "layout_toggle",
    "wrap_toggle",
    "refresh",
    "save_review",
    "exit",
    "activate",
    "fold",
    "ai_menu",
    "copy_answer",
    "suggest_comment",
    "accept_suggestion",
    "viewed_files_toggle",
];

impl ReviewSurface {
    /// Paint with `theme` and the layout preference read at the last keymap preparation.
    pub fn render_with_theme(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&ComponentStyles>,
    ) {
        self.apply_theme(theme);
        let canvas = self.theme.canvas;
        frame.with_style(canvas, |frame| self.render_frame(area, frame));
    }

    /// The linked session opened from the review returned (or failed to open).
    pub fn session_navigation_finished(
        &mut self,
        _session_id: bcode_session_models::SessionId,
        result: Result<(), String>,
    ) {
        match result {
            Ok(()) => self.restore_return(),
            Err(error) => {
                self.return_to = None;
                self.set_status(
                    format!("could not open linked session: {error}"),
                    StatusSeverity::Error,
                );
            }
        }
    }
}

impl View for ReviewSurface {
    fn prepare_keymap(&mut self, host: &UiContext) {
        self.host_layout = host.diff_layout();
        self.prepare_feed_keys(host);
        self.action_keys = HINT_ACTIONS
            .iter()
            .filter_map(|action| {
                host.keymap_keys(SCOPE, &format!("bcode.code_review.{action}"))
                    .into_iter()
                    .next()
                    .map(|key| ((*action).to_owned(), key))
            })
            .collect();
        for scope in [FILES_SCOPE, THREADS_SCOPE, ASSISTANT_SCOPE] {
            for action in crate::keymap::actions_for(scope) {
                if let Some(key) = host.keymap_keys(scope, &action.id).into_iter().next() {
                    let name = action
                        .id
                        .strip_prefix("bcode.code_review.")
                        .unwrap_or(&action.id)
                        .to_owned();
                    self.action_keys.insert(name, key);
                }
            }
        }
        for action in [
            "composer_submit",
            "composer_preview",
            "composer_switch",
            "composer_kind",
            "composer_severity",
            "composer_cancel",
        ] {
            if let Some(key) = host
                .keymap_keys(
                    annotate::COMPOSER_SCOPE,
                    &format!("bcode.code_review.{action}"),
                )
                .into_iter()
                .next()
            {
                self.action_keys.insert(action.to_owned(), key);
            }
        }
    }

    fn render(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&ComponentStyles>,
        host: &UiContext,
    ) {
        self.host_layout = host.diff_layout();
        self.apply_theme(theme);
        let canvas = self.theme.canvas;
        frame.with_style(canvas, |frame| self.render_frame(area, frame));
    }

    fn handle_event(&mut self, event: &Event, host: &UiContext) -> EventResult {
        let action = match event {
            Event::Key(stroke) => {
                let before = self.status.clone();
                let action = self.handle_key(*stroke, host);
                // A status set by this key stays.
                if before.is_some()
                    && self.status == before
                    && action != Action::None
                    && !matches!(action, Action::Close { .. })
                {
                    // Any handled key acknowledges the previous status message, except the
                    // progress of a refresh still running.
                    if !self.refreshing {
                        self.status = None;
                    }
                }
                action
            }
            Event::Paste(text) if self.modal.is_some() => self.modal_paste(text, host),
            Event::Paste(text) if self.overlay.is_some() => {
                let theme = self.theme;
                if let Some(overlay) = self.overlay.as_mut() {
                    overlay.paste(text, &theme);
                }
                Action::Redraw
            }
            Event::Paste(text) if self.focus == Focus::Composer => {
                if let Some(composer) = self.composer.as_mut()
                    && composer.paste(text) == ComposerInput::Edited
                {
                    self.complete_paths(host);
                }
                Action::Redraw
            }
            Event::Paste(text) if self.focus == Focus::Assistant => {
                self.assistant_paste(text);
                Action::Redraw
            }
            Event::Paste(text) if self.focus == Focus::Search => {
                self.search_paste(text);
                Action::Redraw
            }
            Event::Paste(text) if self.focus == Focus::Filter => {
                let outcome = TextInputControl::new(&self.filter_policy)
                    .handle_paste(&mut self.filter.borrow_mut(), text);
                if outcome == TextInputOutcome::Edited {
                    self.apply_filter();
                }
                Action::Redraw
            }
            Event::Mouse(_) => self
                .toast_mouse(event)
                .unwrap_or_else(|| self.handle_mouse(event, host)),
            Event::Resize(_) => {
                // A resize cancels a drag and the hover (their rows moved).
                if let Some(review) = self.review.as_mut() {
                    review.drag = None;
                    review.hover = None;
                }
                Action::Redraw
            }
            Event::Focus(FocusEvent::Gained) => {
                self.request_token(host, false);
                Action::None
            }
            _ => Action::None,
        };
        let action = if self.flush_clipboard(host) && action == Action::None {
            Action::Redraw
        } else {
            action
        };
        EventResult::from_action(action)
    }

    fn poll(&mut self, host: &UiContext) -> Action {
        if matches!(self.phase, Phase::Idle) {
            self.start_open(host);
            return Action::Redraw;
        }
        let mut changed = self.drain(host);
        changed |= self.flush_clipboard(host);
        if let Some(action) = self.pending_action.take() {
            return action;
        }
        if let Some(started) = self.closing {
            self.pump_notes(host);
            self.save(host);
            if self.close_ready() || started.elapsed() >= CLOSE_GRACE {
                self.closing = None;
                return self.close_action();
            }
            return Action::None;
        }
        if self.review.is_some() && !self.notes_requested {
            self.notes_requested = true;
            self.load_notes(host);
        }
        if self.review.is_some() && !self.rereview_checked {
            self.start_rereview(host);
        }
        let notes_before = self.notes.pending();
        self.pump_notes(host);
        self.pump_feed(host);
        changed |= self.agents.drain();
        self.reflect_review_tools(host);
        changed |= self.tick_feed(host);
        self.feed_attention();
        if changed {
            self.sync_notes();
        }
        self.refresh_search();
        self.update_viewed_marks();
        self.persist_presentation(false);
        let before = self.outstanding_requests();
        self.pump(host);
        self.schedule_syntax(host);
        self.prepare_ahead();
        self.save(host);
        changed |= self.outstanding_requests() != before || self.notes.pending() != notes_before;
        changed |= self.toasts.expire(Instant::now());
        changed |= self.animating();
        if changed {
            Action::Redraw
        } else {
            Action::None
        }
    }
}
