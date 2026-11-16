//! The path picker: choose files, folders, globs, and line ranges of the repository as a tree.
//!
//! One component serves every wizard purpose that selects paths ([`PathPurpose`]): a files
//! review, a repository review limited to folders, uncommitted changes in some paths, and
//! editing such a source. It composes BMUX components only: a `picker_frame` with a
//! `text_input` filter over a `tree_view` of the visible rows, plus one-row text blocks for the
//! selection summary and the ignored-files toggle. Rows are one cell high in the tree view, and
//! every pointer hit is resolved against the layout the picker was last painted with (the tree
//! view's own `row_rect`, the summary's and the toggle's laid-out rects), so painting and hit
//! regions agree after scrolling, clipping, and resizing.
//!
//! The selection is a list of [`PathSelector`]s plus exclusions, judged with the service's own
//! rules ([`SelectionMatcher`]): a folder stays a folder selector (dynamic), unselecting a file
//! inside it adds an exclusion, and a folder is shown checked, partly checked, or unchecked.
//!
//! Two key modes keep letters useful: in tree mode `h`/`j`/`k`/`l`, `space`, and `/` are
//! commands and any other printable key starts filtering; in filter mode text edits the filter.

use super::theme::ReviewTheme;
use crate::context::UiContext;
use crate::services::ReviewServices;
use bcode_code_review::selectors::{FolderSelection, SelectionMatcher, is_under};
use bcode_code_review_models::{
    ParsedSelector, PathSelector, ReviewPathEntry, ReviewPathKind, ReviewPathStatus,
    ReviewPathsListRequest, ReviewPathsListResponse, ReviewPathsResolveRequest,
    ReviewPathsResolveResponse, ReviewPathsSearchRequest, ReviewPathsSearchResponse,
};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::{Component, Constraints, LayoutCx, LayoutId, LayoutNode};
use bmux_tui::composition::{Column, Flex, TextBlock};
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::geometry::{Insets, Point, Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::{Line, Span, Text, TextWrap};
use bmux_tui_components::picker_frame::{
    PickerFrame, PickerFrameComponent, PickerFramePlacement, PickerFramePolicy,
};
use bmux_tui_components::text_input::{
    EdgePolicy, TextInputComponent, TextInputControl, TextInputOutcome, TextInputPolicy,
    TextInputState,
};
use bmux_tui_components::tree_view::{
    TreeViewComponent, TreeViewItem, TreeViewLabelOverflow, TreeViewState, TreeViewTrailingLayout,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Entries one folder listing asks for.
const LIST_LIMIT: u32 = 1_000;
/// Matches one filter search asks for.
const SEARCH_LIMIT: u32 = 500;
/// Rows the wheel scrolls.
const WHEEL_ROWS: usize = 3;
/// Check glyphs: selected, partly selected, not selected.
const CHECK_ON: &str = "[x] ";
const CHECK_PARTIAL: &str = "[-] ";
const CHECK_OFF: &str = "[ ] ";

/// What the picker selects for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPurpose {
    /// A files review: files, folders, globs, and one-file line ranges, with exclusions.
    Files,
    /// A repository review limited to folders: folders only.
    Folders,
    /// Uncommitted changes in some paths: files and folders, counted by changed files (rows and
    /// the selection summary).
    Uncommitted,
}

impl PathPurpose {
    const fn title(self) -> &'static str {
        match self {
            Self::Files => "Files and folders",
            Self::Folders => "Repository: limit to folders",
            Self::Uncommitted => "Uncommitted changes in paths",
        }
    }

    const fn files_selectable(self) -> bool {
        !matches!(self, Self::Folders)
    }

    const fn patterns(self) -> bool {
        matches!(self, Self::Files)
    }

    const fn excludes(self) -> bool {
        matches!(self, Self::Files)
    }
}

/// What the picker chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathChoice {
    /// What is reviewed.
    pub include: Vec<PathSelector>,
    /// Files and folders left out of the folders and globs of `include`.
    pub exclude: Vec<PathSelector>,
    /// Whether ignored files were shown when the choice was made.
    pub show_ignored: bool,
}

/// What an input did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathPickerOutcome {
    /// Not handled.
    Ignored,
    /// Visual state changed.
    Redraw,
    /// Back one wizard step.
    Back,
    /// The selection is complete.
    Done(PathChoice),
}

/// The picker's input routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Letters are commands.
    Tree,
    /// Letters edit the filter.
    Filter,
    /// The selection list has the focus.
    Selection,
}

#[derive(Debug, Clone)]
enum DirState {
    Loading,
    Loaded {
        entries: Vec<ReviewPathEntry>,
        total: u32,
    },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RowKind {
    Entry(ReviewPathEntry),
    /// A folder still being listed (or that failed), under its folder.
    Note(String),
    /// Select the filter's glob.
    Pattern {
        pattern: String,
        files: u32,
    },
    /// Select lines of one file.
    Range {
        path: String,
        start: u32,
        end: u32,
    },
    /// One selector of the selection (`exclude` for an exclusion).
    Selected {
        exclude: bool,
        index: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    id: String,
    depth: u16,
    kind: RowKind,
}

#[derive(Debug)]
enum Message {
    Listed {
        generation: u64,
        dir: String,
        result: Result<ReviewPathsListResponse, String>,
    },
    Searched {
        seq: u64,
        query: String,
        result: Result<ReviewPathsSearchResponse, String>,
    },
    Counted {
        seq: u64,
        /// The selection revision the count was asked for.
        revision: u64,
        result: Result<ReviewPathsResolveResponse, String>,
    },
}

#[derive(Debug, Default)]
struct Search {
    seq: u64,
    /// The query of the newest request.
    requested: String,
    /// The query the entries answer.
    answered: String,
    entries: Vec<ReviewPathEntry>,
    total: u32,
    glob: bool,
    error: Option<String>,
}

/// Ids of the laid-out parts.
const FRAME_ID: &str = "wizard.paths";
const INPUT_ID: &str = "wizard.paths.input";
const TREE_ID: &str = "wizard.paths.tree";
const SUMMARY_ID: &str = "wizard.paths.summary";
const IGNORED_ID: &str = "wizard.paths.ignored";

/// The tree picker.
pub struct PathPicker {
    services: ReviewServices,
    repo: PathBuf,
    purpose: PathPurpose,
    inbox: Arc<Mutex<Vec<Message>>>,
    /// Increases whenever listings are dropped (ignored toggle, relist); older answers are
    /// stale.
    generation: u64,
    /// Ask the service to rebuild its path index on the next requests.
    refresh: bool,
    dirs: BTreeMap<String, DirState>,
    expanded: BTreeSet<String>,
    include: Vec<PathSelector>,
    exclude: Vec<PathSelector>,
    matcher: SelectionMatcher,
    /// Counts of the selection now, for the selection revision they answer.
    counts: Option<ReviewPathsResolveResponse>,
    counts_seq: u64,
    /// Selection revision of the newest count request.
    counts_requested: u64,
    /// Selection revision `counts` answers.
    counted: u64,
    selection_revision: u64,
    show_ignored: bool,
    mode: Mode,
    filter: RefCell<TextInputState>,
    filter_policy: TextInputPolicy,
    search: Search,
    rows: Vec<Row>,
    focus: usize,
    scroll: usize,
    /// Row index of the last toggled row (start of a range selection).
    anchor: Option<String>,
    tree_state: RefCell<TreeViewState>,
    /// Rows of the tree as last laid out.
    tree_rows: usize,
    painted: Option<(Rect, LayoutNode, Vec<TreeViewItem>)>,
    status: Option<String>,
    hints: String,
    /// Frame title, padded with a space on each side.
    title: String,
}

impl std::fmt::Debug for PathPicker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PathPicker")
            .field("purpose", &self.purpose)
            .field("include", &self.include)
            .field("exclude", &self.exclude)
            .finish_non_exhaustive()
    }
}

impl PathPicker {
    /// A picker over the repository at `repo`, for `purpose`.
    #[must_use]
    pub fn new(services: ReviewServices, repo: PathBuf, purpose: PathPurpose) -> Self {
        let mut filter_policy = TextInputPolicy::chat_composer();
        filter_policy.keyboard.shift_enter = None;
        filter_policy.viewport.max_rows = Some(1);
        filter_policy.edge = EdgePolicy::disabled();
        let mut picker = Self {
            services,
            repo,
            purpose,
            inbox: Arc::default(),
            generation: 0,
            refresh: true,
            dirs: BTreeMap::new(),
            expanded: BTreeSet::new(),
            include: Vec::new(),
            exclude: Vec::new(),
            matcher: SelectionMatcher::default(),
            counts: None,
            counts_seq: 0,
            counts_requested: 0,
            counted: 0,
            selection_revision: 0,
            show_ignored: false,
            mode: Mode::Tree,
            filter: RefCell::new(TextInputState::new(bmux_text_edit::TextEditBuffer::new())),
            filter_policy,
            search: Search::default(),
            rows: Vec::new(),
            focus: 0,
            scroll: 0,
            anchor: None,
            tree_state: RefCell::new(TreeViewState::new(None)),
            tree_rows: 10,
            painted: None,
            status: None,
            hints: String::new(),
            title: format!(" {} ", purpose.title()),
        };
        picker.rebuild_rows();
        picker
    }

    /// Start with `include` and `exclude` selected (editing a source), showing ignored files
    /// when `show_ignored`. The folders leading to selected paths are expanded.
    #[must_use]
    pub fn with_selection(
        mut self,
        include: Vec<PathSelector>,
        exclude: Vec<PathSelector>,
        show_ignored: bool,
    ) -> Self {
        for selector in include.iter().chain(&exclude) {
            if let Some(path) = selector.path() {
                let mut parent = path;
                while let Some((folder, _)) = parent.rsplit_once('/') {
                    self.expanded.insert(folder.to_owned());
                    parent = folder;
                }
            }
        }
        self.include = include;
        self.exclude = exclude;
        self.show_ignored = show_ignored;
        self.selection_changed();
        self
    }

    /// Set the footer's key hints.
    pub fn set_hints(&mut self, hints: impl Into<String>) {
        self.hints = hints.into();
    }

    /// The purpose.
    #[must_use]
    pub const fn purpose(&self) -> PathPurpose {
        self.purpose
    }

    /// The selected selectors.
    #[must_use]
    pub fn include(&self) -> &[PathSelector] {
        &self.include
    }

    /// The exclusions.
    #[must_use]
    pub fn exclude(&self) -> &[PathSelector] {
        &self.exclude
    }

    /// Whether ignored files are shown.
    #[must_use]
    pub const fn shows_ignored(&self) -> bool {
        self.show_ignored
    }

    /// The filter text.
    #[must_use]
    pub fn filter_text(&self) -> String {
        self.filter.borrow().buffer().text().to_owned()
    }

    /// Whether letters edit the filter.
    #[must_use]
    pub fn filtering(&self) -> bool {
        self.mode == Mode::Filter
    }

    /// Whether the selection list has the focus.
    #[must_use]
    pub fn showing_selection(&self) -> bool {
        self.mode == Mode::Selection
    }

    /// The path of the focused row (folders end with `/`), or the selector of a selection row.
    #[must_use]
    pub fn focused(&self) -> Option<String> {
        self.rows.get(self.focus).and_then(|row| match &row.kind {
            RowKind::Entry(entry) => Some(entry.path.clone()),
            RowKind::Selected { exclude, index } => self.selector(*exclude, *index).map(|s| {
                if *exclude {
                    format!("-{s}")
                } else {
                    s.to_string()
                }
            }),
            RowKind::Pattern { pattern, .. } => Some(pattern.clone()),
            RowKind::Range { path, start, end } => Some(format!("{path}:{start}-{end}")),
            RowKind::Note(_) => None,
        })
    }

    /// The selection's file count now, once counted: for uncommitted changes, the files that
    /// changed since `HEAD`.
    #[must_use]
    pub fn total(&self) -> Option<u32> {
        self.counts
            .as_ref()
            .filter(|_| self.counted == self.selection_revision)
            .map(|counts| {
                if self.purpose == PathPurpose::Uncommitted {
                    counts.changed
                } else {
                    counts.total
                }
            })
    }

    /// The status line text (why a row could not be selected, for example).
    #[must_use]
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Whether any listing, search, or count is still running.
    #[must_use]
    pub fn loading(&self) -> bool {
        self.dirs
            .values()
            .any(|dir| matches!(dir, DirState::Loading))
            || self.search.requested != self.search.answered
            || (!self.include.is_empty() && self.counted != self.selection_revision)
    }

    fn selector(&self, exclude: bool, index: usize) -> Option<&PathSelector> {
        if exclude {
            self.exclude.get(index)
        } else {
            self.include.get(index)
        }
    }

    // ----------------------------------------------------------------------------------------
    // Loading
    // ----------------------------------------------------------------------------------------

    fn spawn(
        &self,
        host: &UiContext,
        task: impl std::future::Future<Output = Message> + Send + 'static,
    ) {
        let inbox = Arc::clone(&self.inbox);
        host.spawn(Box::pin(async move {
            let message = task.await;
            if let Ok(mut queue) = inbox.lock() {
                queue.push(message);
            }
        }));
    }

    /// Apply finished work and start what is due. Returns whether the picker changed.
    pub fn poll(&mut self, host: &UiContext) -> bool {
        let messages = self
            .inbox
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default();
        let mut changed = !messages.is_empty();
        for message in messages {
            self.apply(message);
        }
        changed |= self.request_due(host);
        if changed {
            self.rebuild_rows();
        }
        changed
    }

    fn apply(&mut self, message: Message) {
        match message {
            Message::Listed {
                generation,
                dir,
                result,
            } => {
                if generation != self.generation {
                    return;
                }
                self.dirs.insert(
                    dir,
                    match result {
                        Ok(response) => DirState::Loaded {
                            entries: response.entries,
                            total: response.total_entries,
                        },
                        Err(error) => DirState::Failed(error),
                    },
                );
            }
            Message::Searched { seq, query, result } => {
                if seq != self.search.seq {
                    return;
                }
                self.search.answered = query;
                match result {
                    Ok(response) => {
                        self.search.entries = response.entries;
                        self.search.total = response.total_matches;
                        self.search.glob = response.glob;
                        self.search.error = None;
                    }
                    Err(error) => {
                        self.search.entries.clear();
                        self.search.total = 0;
                        self.search.error = Some(error);
                    }
                }
            }
            Message::Counted {
                seq,
                revision,
                result,
            } => {
                if seq != self.counts_seq {
                    return;
                }
                // The count answers the selection it was asked about: when the selection
                // changed since, it is not current and a new count is asked for.
                self.counted = revision;
                self.counts = result.ok();
            }
        }
    }

    /// Start the listings, search, and count the picker needs now. Returns whether any started.
    fn request_due(&mut self, host: &UiContext) -> bool {
        let mut started = false;
        let wanted: Vec<String> = std::iter::once(String::new())
            .chain(self.expanded.iter().cloned())
            .filter(|dir| !self.dirs.contains_key(dir))
            .collect();
        for dir in wanted {
            self.dirs.insert(dir.clone(), DirState::Loading);
            let request = ReviewPathsListRequest {
                repo_path: self.repo.clone(),
                dir: dir.clone(),
                include_ignored: self.show_ignored,
                refresh: std::mem::take(&mut self.refresh),
                cursor: None,
                limit: Some(LIST_LIMIT),
            };
            let (services, generation) = (self.services.clone(), self.generation);
            self.spawn(host, async move {
                Message::Listed {
                    generation,
                    dir,
                    result: services.call(request).await.map_err(|error| error.message),
                }
            });
            started = true;
        }
        let query = self.search_query();
        if !query.is_empty() && query != self.search.requested {
            self.search.seq += 1;
            self.search.requested.clone_from(&query);
            let request = ReviewPathsSearchRequest {
                repo_path: self.repo.clone(),
                query: query.clone(),
                include_ignored: self.show_ignored,
                refresh: std::mem::take(&mut self.refresh),
                limit: Some(SEARCH_LIMIT),
            };
            let (services, seq) = (self.services.clone(), self.search.seq);
            self.spawn(host, async move {
                Message::Searched {
                    seq,
                    query,
                    result: services.call(request).await.map_err(|error| error.message),
                }
            });
            started = true;
        } else if query.is_empty() {
            self.search.requested.clear();
            self.search.answered.clear();
        }
        if !self.include.is_empty()
            && self.counted != self.selection_revision
            && self.counts_requested != self.selection_revision
        {
            self.counts_seq += 1;
            self.counts_requested = self.selection_revision;
            let request = ReviewPathsResolveRequest {
                repo_path: self.repo.clone(),
                include: self.include.clone(),
                exclude: self.exclude.clone(),
                include_ignored: self.show_ignored,
                refresh: false,
            };
            let (services, seq, revision) = (
                self.services.clone(),
                self.counts_seq,
                self.selection_revision,
            );
            self.spawn(host, async move {
                Message::Counted {
                    seq,
                    revision,
                    result: services.call(request).await.map_err(|error| error.message),
                }
            });
            started = true;
        }
        started
    }
}

// --------------------------------------------------------------------------------------------
// Rows
// --------------------------------------------------------------------------------------------

/// A filter ending in `:START-END` (or `:LINE`): the path part and the ordered lines.
fn range_query(query: &str) -> Option<(String, u32, u32)> {
    match PathSelector::parse(query).ok()? {
        ParsedSelector::Selector(PathSelector::FileRange { path, start, end }) => {
            Some((path, start, end))
        }
        _ => None,
    }
}

/// Whether `query` is a glob.
fn glob_query(query: &str) -> bool {
    bcode_code_review::selectors::is_glob(query)
}

/// `path` without a trailing `/`.
fn bare(path: &str) -> &str {
    path.trim_end_matches('/')
}

impl PathPicker {
    /// What the filter searches: its text without a line range.
    fn search_query(&self) -> String {
        let text = self.filter_text();
        let text = text.trim();
        range_query(text).map_or_else(|| text.to_owned(), |(path, _, _)| path)
    }

    fn rebuild_rows(&mut self) {
        let focused = self.rows.get(self.focus).map(|row| row.id.clone());
        let mut rows = Vec::new();
        match self.mode {
            Mode::Selection => {
                for index in 0..self.include.len() {
                    rows.push(Row {
                        id: format!("include-{index}"),
                        depth: 0,
                        kind: RowKind::Selected {
                            exclude: false,
                            index,
                        },
                    });
                }
                for index in 0..self.exclude.len() {
                    rows.push(Row {
                        id: format!("exclude-{index}"),
                        depth: 0,
                        kind: RowKind::Selected {
                            exclude: true,
                            index,
                        },
                    });
                }
            }
            Mode::Tree | Mode::Filter if !self.search_query().is_empty() => {
                self.search_rows(&mut rows);
            }
            Mode::Tree | Mode::Filter => self.tree_rows("", 0, &mut rows),
        }
        self.rows = rows;
        self.title = self.frame_title();
        self.focus = focused
            .and_then(|id| self.rows.iter().position(|row| row.id == id))
            .unwrap_or_else(|| self.focus.min(self.rows.len().saturating_sub(1)));
        self.keep_focus_visible();
    }

    /// The frame's title, padded with a space on each side: the purpose, and how many of a
    /// filter's matches are shown when it found more than the picker lists.
    fn frame_title(&self) -> String {
        let shown = u32::try_from(self.search.entries.len()).unwrap_or(u32::MAX);
        let filtering = self.mode != Mode::Selection && !self.search_query().is_empty();
        if filtering && self.search.total > shown && self.search.requested == self.search.answered {
            format!(
                " {} · {shown} of {} matches, type more to narrow ",
                self.purpose.title(),
                self.search.total
            )
        } else {
            format!(" {} ", self.purpose.title())
        }
    }

    fn tree_rows(&self, dir: &str, depth: u16, rows: &mut Vec<Row>) {
        match self.dirs.get(dir) {
            None | Some(DirState::Loading) => rows.push(Row {
                id: format!("note:{dir}"),
                depth,
                kind: RowKind::Note("Loading…".to_owned()),
            }),
            Some(DirState::Failed(error)) => rows.push(Row {
                id: format!("note:{dir}"),
                depth,
                kind: RowKind::Note(error.clone()),
            }),
            Some(DirState::Loaded { entries, total }) => {
                if entries.is_empty() && depth == 0 {
                    rows.push(Row {
                        id: "note:".to_owned(),
                        depth,
                        kind: RowKind::Note("No files".to_owned()),
                    });
                }
                for entry in entries {
                    if !self.purpose.files_selectable() && entry.kind == ReviewPathKind::File {
                        continue;
                    }
                    let path = bare(&entry.path).to_owned();
                    rows.push(Row {
                        id: entry.path.clone(),
                        depth,
                        kind: RowKind::Entry(entry.clone()),
                    });
                    if entry.kind == ReviewPathKind::Folder && self.expanded.contains(&path) {
                        self.tree_rows(&path, depth.saturating_add(1), rows);
                    }
                }
                let shown = u32::try_from(entries.len()).unwrap_or(u32::MAX);
                if *total > shown {
                    rows.push(Row {
                        id: format!("note-more:{dir}"),
                        depth,
                        kind: RowKind::Note(format!("{} more; filter to find them", total - shown)),
                    });
                }
            }
        }
    }

    fn search_rows(&self, rows: &mut Vec<Row>) {
        let text = self.filter_text();
        let text = text.trim();
        if self.purpose.patterns() && glob_query(text) {
            rows.push(Row {
                id: format!("pattern:{text}"),
                depth: 0,
                kind: RowKind::Pattern {
                    pattern: text.to_owned(),
                    files: if self.search.answered == text {
                        self.search.total
                    } else {
                        0
                    },
                },
            });
        }
        if self.purpose.patterns()
            && let Some((path, start, end)) = range_query(text)
            && self.search.answered == path
            && self
                .search
                .entries
                .iter()
                .any(|entry| entry.kind == ReviewPathKind::File && entry.path == path)
        {
            rows.push(Row {
                id: format!("range:{path}:{start}-{end}"),
                depth: 0,
                kind: RowKind::Range { path, start, end },
            });
        }
        if let Some(error) = &self.search.error {
            rows.push(Row {
                id: "note:search".to_owned(),
                depth: 0,
                kind: RowKind::Note(error.clone()),
            });
        }
        for entry in &self.search.entries {
            if !self.purpose.files_selectable() && entry.kind == ReviewPathKind::File {
                continue;
            }
            rows.push(Row {
                id: entry.path.clone(),
                depth: 0,
                kind: RowKind::Entry(entry.clone()),
            });
        }

        if rows.is_empty() {
            rows.push(Row {
                id: "note:none".to_owned(),
                depth: 0,
                kind: RowKind::Note(if self.search.requested == self.search.answered {
                    "No matching files".to_owned()
                } else {
                    "Searching…".to_owned()
                }),
            });
        }
    }

    const fn selectable(row: &Row) -> bool {
        !matches!(row.kind, RowKind::Note(_))
    }

    fn keep_focus_visible(&mut self) {
        let height = self.tree_rows.max(1);
        if self.focus < self.scroll {
            self.scroll = self.focus;
        } else if self.focus >= self.scroll + height {
            self.scroll = self.focus + 1 - height;
        }
        let max_scroll = self.rows.len().saturating_sub(height);
        self.scroll = self.scroll.min(max_scroll);
    }

    fn move_focus(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        let mut next = self.focus.saturating_add_signed(delta).min(last);
        // Skip notes in the direction of travel, else stay.
        while !Self::selectable(&self.rows[next]) {
            let moved = if delta < 0 {
                next.checked_sub(1)
            } else {
                (next < last).then_some(next + 1)
            };
            if let Some(moved) = moved {
                next = moved;
            } else {
                next = self.focus;
                break;
            }
        }
        self.focus = next;
        self.keep_focus_visible();
    }

    // ----------------------------------------------------------------------------------------
    // Selecting
    // ----------------------------------------------------------------------------------------

    fn selection_changed(&mut self) {
        self.matcher = SelectionMatcher::new(&self.include, &self.exclude);
        self.selection_revision += 1;
        self.rebuild_rows();
    }

    /// Remove every selector and exclusion strictly inside `folder`.
    fn clear_inside(&mut self, folder: &str) {
        let inside = |selector: &PathSelector| match selector {
            PathSelector::Glob { pattern } => is_under(pattern, folder),
            other => other.path().is_some_and(|path| is_under(path, folder)),
        };
        self.include.retain(|selector| !inside(selector));
        self.exclude.retain(|selector| !inside(selector));
    }

    fn toggle_file(&mut self, path: &str) {
        let explicit =
            |selector: &PathSelector| selector.is_explicit_file() && selector.path() == Some(path);
        if self.matcher.file(path) {
            self.include.retain(|selector| !explicit(selector));
            if SelectionMatcher::new(&self.include, &self.exclude).file(path) {
                if !self.purpose.excludes() {
                    self.status = Some(format!(
                        "{path} is inside a selected folder; unselect the folder instead"
                    ));
                    return;
                }
                self.exclude.push(PathSelector::File {
                    path: path.to_owned(),
                });
            }
        } else {
            self.exclude.retain(|selector| !explicit(selector));
            if !SelectionMatcher::new(&self.include, &self.exclude).file(path) {
                if self.matcher.excluded(path) && self.exclude.iter().any(|s| !explicit(s)) {
                    let excluded = self
                        .exclude
                        .iter()
                        .find(|selector| {
                            selector.path().is_some_and(|folder| is_under(path, folder))
                        })
                        .map(ToString::to_string);
                    if let Some(folder) = excluded {
                        self.status = Some(format!(
                            "{path} is inside excluded {folder}; select {folder} first"
                        ));
                        return;
                    }
                }
                self.include.push(PathSelector::File {
                    path: path.to_owned(),
                });
            }
        }
        self.selection_changed();
    }

    fn toggle_folder(&mut self, path: &str) {
        let own = |selector: &PathSelector| matches!(selector, PathSelector::Folder { path: folder } if folder == path);
        match self.matcher.folder(path) {
            FolderSelection::All => {
                let explicit = self.include.iter().any(own);
                self.include.retain(|selector| !own(selector));
                self.clear_inside(path);
                if !explicit {
                    if !self.purpose.excludes() {
                        self.status = Some(format!(
                            "{path}/ is inside a selected folder; unselect that folder instead"
                        ));
                        self.selection_changed();
                        return;
                    }
                    self.exclude.push(PathSelector::Folder {
                        path: path.to_owned(),
                    });
                }
            }
            FolderSelection::Partial => {
                self.include.retain(|selector| !own(selector));
                self.exclude.retain(|selector| !own(selector));
                self.clear_inside(path);
                // A partly selected folder that a selected ancestor covers stays covered: leave
                // it out entirely.
                if SelectionMatcher::new(&self.include, &self.exclude).folder(path)
                    == FolderSelection::All
                {
                    self.exclude.push(PathSelector::Folder {
                        path: path.to_owned(),
                    });
                }
            }
            FolderSelection::None => {
                self.exclude.retain(|selector| !own(selector));
                self.clear_inside(path);
                if SelectionMatcher::new(&self.include, &self.exclude).folder(path)
                    != FolderSelection::All
                {
                    self.include.push(PathSelector::Folder {
                        path: path.to_owned(),
                    });
                }
            }
        }
        self.selection_changed();
    }

    fn toggle_selector(&mut self, selector: PathSelector) {
        if let Some(position) = self.include.iter().position(|known| *known == selector) {
            self.include.remove(position);
        } else {
            self.include.push(selector);
        }
        self.selection_changed();
    }

    /// Select or unselect the focused row.
    fn toggle_focused(&mut self) -> bool {
        let Some(row) = self.rows.get(self.focus).cloned() else {
            return false;
        };
        self.status = None;
        match row.kind {
            RowKind::Entry(entry) => {
                match entry.kind {
                    ReviewPathKind::File if self.purpose.files_selectable() => {
                        self.toggle_file(&entry.path);
                    }
                    ReviewPathKind::File => {
                        self.status = Some("choose folders".to_owned());
                    }
                    ReviewPathKind::Folder => self.toggle_folder(bare(&entry.path)),
                }
                self.anchor = Some(row.id);
            }
            RowKind::Pattern { pattern, .. } => {
                self.toggle_selector(PathSelector::Glob { pattern });
            }
            RowKind::Range { path, start, end } => {
                self.toggle_selector(PathSelector::FileRange { path, start, end });
            }
            RowKind::Selected { exclude, index } => {
                if exclude {
                    if index < self.exclude.len() {
                        self.exclude.remove(index);
                    }
                } else if index < self.include.len() {
                    self.include.remove(index);
                }
                self.selection_changed();
            }
            RowKind::Note(_) => return false,
        }
        true
    }

    /// Select every unselected entry row from the last toggled row to the focused one.
    fn toggle_range(&mut self) -> bool {
        let Some(anchor) = self
            .anchor
            .as_ref()
            .and_then(|id| self.rows.iter().position(|row| &row.id == id))
        else {
            return self.toggle_focused();
        };
        let (low, high) = (anchor.min(self.focus), anchor.max(self.focus));
        let entries: Vec<ReviewPathEntry> = self.rows[low..=high]
            .iter()
            .filter_map(|row| match &row.kind {
                RowKind::Entry(entry) => Some(entry.clone()),
                _ => None,
            })
            .collect();
        for entry in entries {
            match entry.kind {
                ReviewPathKind::File
                    if self.purpose.files_selectable() && !self.matcher.file(&entry.path) =>
                {
                    self.toggle_file(&entry.path);
                }
                ReviewPathKind::Folder
                    if self.matcher.folder(bare(&entry.path)) == FolderSelection::None =>
                {
                    self.toggle_folder(bare(&entry.path));
                }
                _ => {}
            }
        }
        true
    }

    fn set_expanded(&mut self, path: &str, expanded: bool) {
        if expanded {
            self.expanded.insert(path.to_owned());
        } else {
            self.expanded.remove(path);
            self.expanded.retain(|folder| !is_under(folder, path));
        }
        self.rebuild_rows();
        if expanded {
            self.reveal_children();
        }
    }

    /// Scroll so the focused folder's children show below it, as many as fit with the folder
    /// still on screen.
    fn reveal_children(&mut self) {
        let Some(depth) = self.rows.get(self.focus).map(|row| row.depth) else {
            return;
        };
        let end = self.rows[self.focus + 1..]
            .iter()
            .position(|row| row.depth <= depth)
            .map_or(self.rows.len(), |offset| self.focus + 1 + offset);
        let height = self.tree_rows.max(1);
        let wanted = end.saturating_sub(height).min(self.focus);
        self.scroll = self.scroll.max(wanted);
    }

    /// `left`/`h`: collapse the focused folder, or go to its parent.
    fn collapse(&mut self) {
        let Some(row) = self.rows.get(self.focus).cloned() else {
            return;
        };
        if let RowKind::Entry(entry) = &row.kind {
            let path = bare(&entry.path).to_owned();
            if entry.kind == ReviewPathKind::Folder && self.expanded.contains(&path) {
                self.set_expanded(&path, false);
                return;
            }
        }
        if row.depth > 0
            && let Some(parent) = self.rows[..self.focus]
                .iter()
                .rposition(|candidate| candidate.depth < row.depth)
        {
            self.focus = parent;
            self.keep_focus_visible();
        }
    }

    /// `right`/`l`: expand the focused folder, or go to its first child.
    fn expand(&mut self) {
        let Some(RowKind::Entry(entry)) = self.rows.get(self.focus).map(|row| row.kind.clone())
        else {
            return;
        };
        if entry.kind != ReviewPathKind::Folder || !self.search_query().is_empty() {
            return;
        }
        let path = bare(&entry.path).to_owned();
        if self.expanded.contains(&path) {
            self.move_focus(1);
        } else {
            self.set_expanded(&path, true);
        }
    }

    /// Show or hide ignored files: every listing is taken again.
    fn toggle_ignored(&mut self) {
        self.show_ignored = !self.show_ignored;
        self.relist(false);
    }

    /// Show (or hide) ignored files.
    pub fn set_show_ignored(&mut self, show: bool) {
        if self.show_ignored != show {
            self.toggle_ignored();
        }
    }

    /// Drop every listing (asking the service to rebuild its index when `refresh`).
    fn relist(&mut self, refresh: bool) {
        self.generation += 1;
        self.refresh |= refresh;
        self.dirs.clear();
        self.search.requested.clear();
        self.search.answered.clear();
        self.counts_requested = 0;
        self.counted = 0;
        self.selection_revision += 1;
        self.rebuild_rows();
    }

    /// The choice, or (with nothing selected) the focused row selected and chosen.
    fn finish(&mut self) -> PathPickerOutcome {
        if self.include.is_empty() {
            let focused = self.rows.get(self.focus).map(|row| row.kind.clone());
            match focused {
                Some(RowKind::Entry(entry))
                    if entry.kind == ReviewPathKind::Folder || self.purpose.files_selectable() =>
                {
                    self.toggle_focused();
                }
                Some(RowKind::Pattern { .. } | RowKind::Range { .. }) => {
                    self.toggle_focused();
                }
                _ => {}
            }
        }
        if self.include.is_empty() {
            self.status = Some("select a file or folder first (space)".to_owned());
            return PathPickerOutcome::Redraw;
        }
        PathPickerOutcome::Done(PathChoice {
            include: self.include.clone(),
            exclude: self.exclude.clone(),
            show_ignored: self.show_ignored,
        })
    }
}

// --------------------------------------------------------------------------------------------
// Input
// --------------------------------------------------------------------------------------------

impl PathPicker {
    /// Handle a key; `action` is the wizard keymap action it is bound to, if any (without the
    /// `bcode.code_review.` prefix).
    pub fn key(&mut self, stroke: KeyStroke, action: Option<&str>) -> PathPickerOutcome {
        let plain = stroke.modifiers.is_empty();
        let outcome = match action {
            Some("wizard_show_ignored") => {
                self.toggle_ignored();
                return PathPickerOutcome::Redraw;
            }
            Some("wizard_selection") => {
                self.mode = if self.mode == Mode::Selection {
                    Mode::Tree
                } else {
                    Mode::Selection
                };
                self.focus = 0;
                self.scroll = 0;
                self.rebuild_rows();
                return PathPickerOutcome::Redraw;
            }
            Some("wizard_relist") => {
                self.relist(true);
                return PathPickerOutcome::Redraw;
            }
            Some("wizard_mark") => Some(self.toggle_focused()),
            Some("wizard_mark_range") => Some(self.toggle_range()),
            Some("wizard_back") => return self.back(),
            _ => None,
        };
        if let Some(handled) = outcome {
            return if handled {
                PathPickerOutcome::Redraw
            } else {
                PathPickerOutcome::Ignored
            };
        }
        let rows = isize::try_from(self.tree_rows.max(1)).unwrap_or(1);
        match stroke.key {
            KeyCode::Enter if plain => return self.finish(),
            KeyCode::Up if plain => self.move_focus(-1),
            KeyCode::Down if plain => self.move_focus(1),
            KeyCode::PageUp if plain => self.move_focus(-rows),
            KeyCode::PageDown if plain => self.move_focus(rows),
            KeyCode::Home if plain && self.mode != Mode::Filter => {
                self.focus = 0;
                self.move_focus(0);
            }
            KeyCode::End if plain && self.mode != Mode::Filter => {
                self.focus = self.rows.len().saturating_sub(1);
                self.move_focus(0);
            }
            _ => return self.mode_key(stroke),
        }
        PathPickerOutcome::Redraw
    }

    fn back(&mut self) -> PathPickerOutcome {
        match self.mode {
            Mode::Filter => self.mode = Mode::Tree,
            Mode::Selection => {
                self.mode = Mode::Tree;
                self.focus = 0;
            }
            Mode::Tree if !self.filter_text().is_empty() => self.set_filter(""),
            Mode::Tree => return PathPickerOutcome::Back,
        }
        self.rebuild_rows();
        PathPickerOutcome::Redraw
    }

    fn set_filter(&mut self, text: &str) {
        *self.filter.borrow_mut() =
            TextInputState::new(bmux_text_edit::TextEditBuffer::from_text(text));
        self.focus = 0;
        self.scroll = 0;
        self.rebuild_rows();
    }

    fn mode_key(&mut self, stroke: KeyStroke) -> PathPickerOutcome {
        let plain = bmux_keyboard::Modifiers {
            shift: false,
            ..stroke.modifiers
        }
        .is_empty();
        match self.mode {
            Mode::Selection => match stroke.key {
                KeyCode::Char('j') if plain => self.move_focus(1),
                KeyCode::Char('k') if plain => self.move_focus(-1),
                KeyCode::Space
                | KeyCode::Char(' ' | 'x')
                | KeyCode::Delete
                | KeyCode::Backspace
                    if plain =>
                {
                    self.toggle_focused();
                    if self.include.is_empty() && self.exclude.is_empty() {
                        self.mode = Mode::Tree;
                        self.rebuild_rows();
                    }
                }
                _ => return PathPickerOutcome::Ignored,
            },
            Mode::Tree => match stroke.key {
                KeyCode::Char('j') if plain => self.move_focus(1),
                KeyCode::Char('k') if plain => self.move_focus(-1),
                KeyCode::Left | KeyCode::Char('h') if plain => self.collapse(),
                KeyCode::Right | KeyCode::Char('l') if plain => self.expand(),
                KeyCode::Space | KeyCode::Char(' ') if plain => {
                    self.toggle_focused();
                }
                KeyCode::Char('/') if plain => self.mode = Mode::Filter,
                KeyCode::Char(_) if plain => {
                    self.mode = Mode::Filter;
                    return self.edit(stroke);
                }
                KeyCode::Backspace if plain && !self.filter_text().is_empty() => {
                    self.mode = Mode::Filter;
                    return self.edit(stroke);
                }
                _ => return PathPickerOutcome::Ignored,
            },
            Mode::Filter => return self.edit(stroke),
        }
        PathPickerOutcome::Redraw
    }

    fn edit(&mut self, stroke: KeyStroke) -> PathPickerOutcome {
        let before = self.filter_text();
        let outcome = TextInputControl::new(&self.filter_policy)
            .handle_key(&mut self.filter.borrow_mut(), stroke);
        if outcome == TextInputOutcome::Ignored {
            return PathPickerOutcome::Ignored;
        }
        if self.filter_text() != before {
            self.focus = 0;
            self.scroll = 0;
            self.rebuild_rows();
        }
        PathPickerOutcome::Redraw
    }

    /// Insert pasted text into the filter.
    pub fn paste(&mut self, text: &str) -> PathPickerOutcome {
        if self.mode == Mode::Selection {
            return PathPickerOutcome::Ignored;
        }
        self.mode = Mode::Filter;
        TextInputControl::new(&self.filter_policy)
            .handle_paste(&mut self.filter.borrow_mut(), text);
        self.focus = 0;
        self.scroll = 0;
        self.rebuild_rows();
        PathPickerOutcome::Redraw
    }

    /// Route a pointer event through the layout last painted.
    pub fn mouse(&mut self, event: &Event) -> PathPickerOutcome {
        let Event::Mouse(mouse) = event else {
            return PathPickerOutcome::Ignored;
        };
        let Some((area, layout, items)) = self.painted.as_ref() else {
            return PathPickerOutcome::Ignored;
        };
        let at = |id: &'static str| {
            layout
                .find_rect(&LayoutId::new(id))
                .map(|rect| Rect::new(area.x + rect.x, area.y + rect.y, rect.width, rect.height))
        };
        let point = mouse.position;
        let on_tree = at(TREE_ID).filter(|rect| rect.contains(point));
        match mouse.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if on_tree.is_some() => {
                let height = self.tree_rows.max(1);
                let max_scroll = self.rows.len().saturating_sub(height);
                self.scroll = if mouse.kind == MouseEventKind::ScrollDown {
                    (self.scroll + WHEEL_ROWS).min(max_scroll)
                } else {
                    self.scroll.saturating_sub(WHEEL_ROWS)
                };
                // Keep the focus on screen, as the wheel moves the rows under it.
                self.focus = self.focus.clamp(self.scroll, self.scroll + height - 1);
                self.move_focus(0);
                PathPickerOutcome::Redraw
            }
            // A right click selects or unselects any row, a folder of the tree included (a
            // left click on one expands it).
            MouseEventKind::Down(MouseButton::Right) => {
                let Some(tree) = on_tree else {
                    return PathPickerOutcome::Ignored;
                };
                let row = Self::hit_row(layout, items, &self.tree_state, tree, point);
                row.map_or(PathPickerOutcome::Ignored, |row| self.select_row(row))
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(tree) = on_tree {
                    let row = Self::hit_row(layout, items, &self.tree_state, tree, point);
                    return row.map_or(PathPickerOutcome::Ignored, |row| self.click_row(row));
                }
                if at(IGNORED_ID).is_some_and(|rect| rect.contains(point)) {
                    self.toggle_ignored();
                    return PathPickerOutcome::Redraw;
                }
                if at(SUMMARY_ID).is_some_and(|rect| rect.contains(point)) {
                    return self.key(
                        KeyStroke::simple(KeyCode::Char('l')),
                        Some("wizard_selection"),
                    );
                }
                if at(INPUT_ID).is_some_and(|rect| rect.contains(point))
                    && self.mode != Mode::Selection
                {
                    self.mode = Mode::Filter;
                    return PathPickerOutcome::Redraw;
                }
                PathPickerOutcome::Ignored
            }
            _ => PathPickerOutcome::Ignored,
        }
    }

    /// The visible row under `point`, by the tree view's own row geometry in the painted
    /// layout and the expansion state it was painted with.
    fn hit_row(
        layout: &LayoutNode,
        items: &[TreeViewItem],
        state: &RefCell<TreeViewState>,
        tree: Rect,
        point: Point,
    ) -> Option<usize> {
        let node = layout.find(&LayoutId::new(TREE_ID))?;
        let component = TreeViewComponent::new(TREE_ID, items, state);
        let local_x = u64::from(point.x.checked_sub(tree.x)?);
        let local_y = u64::from(point.y.checked_sub(tree.y)?);
        (0..component.visible_count()).find(|row| {
            component.row_rect(node, *row).is_some_and(|rect| {
                (rect.y..rect.y + rect.height).contains(&local_y)
                    && (rect.x..rect.x + rect.width).contains(&local_x)
            })
        })
    }

    /// A right click on visible row `row`: focus it and select or unselect it.
    fn select_row(&mut self, row: usize) -> PathPickerOutcome {
        let index = self.scroll + row;
        if !self.rows.get(index).is_some_and(Self::selectable) {
            return PathPickerOutcome::Ignored;
        }
        self.focus = index;
        self.toggle_focused();
        PathPickerOutcome::Redraw
    }

    /// A click on visible row `row`: focus it; a folder of the tree expands or collapses, any
    /// other row is selected or unselected.
    fn click_row(&mut self, row: usize) -> PathPickerOutcome {
        let index = self.scroll + row;
        let Some(clicked) = self.rows.get(index).cloned() else {
            return PathPickerOutcome::Ignored;
        };
        if !Self::selectable(&clicked) {
            return PathPickerOutcome::Ignored;
        }
        self.focus = index;
        match &clicked.kind {
            RowKind::Entry(entry)
                if entry.kind == ReviewPathKind::Folder
                    && self.search_query().is_empty()
                    && self.mode != Mode::Selection =>
            {
                let path = bare(&entry.path).to_owned();
                let expanded = self.expanded.contains(&path);
                self.set_expanded(&path, !expanded);
            }
            _ => {
                self.toggle_focused();
            }
        }
        PathPickerOutcome::Redraw
    }
}

// --------------------------------------------------------------------------------------------
// Painting
// --------------------------------------------------------------------------------------------

impl PathPicker {
    fn check(&self, entry: &ReviewPathEntry) -> FolderSelection {
        match entry.kind {
            ReviewPathKind::File => {
                if self.matcher.file(&entry.path) {
                    FolderSelection::All
                } else {
                    FolderSelection::None
                }
            }
            ReviewPathKind::Folder => self.matcher.folder(bare(&entry.path)),
        }
    }

    fn check_span(selection: FolderSelection, theme: &ReviewTheme) -> Span {
        match selection {
            FolderSelection::All => Span::styled(CHECK_ON, theme.accent),
            FolderSelection::Partial => Span::styled(CHECK_PARTIAL, theme.accent),
            FolderSelection::None => Span::styled(CHECK_OFF, theme.muted),
        }
    }

    fn files_text(count: u32) -> String {
        format!("{count} file{}", if count == 1 { "" } else { "s" })
    }

    fn item(&self, row: &Row, theme: &ReviewTheme) -> TreeViewItem {
        let searching = !self.search_query().is_empty() && self.mode != Mode::Selection;
        match &row.kind {
            RowKind::Entry(entry) => self.entry_item(row, entry, searching, theme),
            RowKind::Note(text) => TreeViewItem::rich(
                row.id.clone(),
                Line::from_spans(vec![Span::styled(text.clone(), theme.muted)]),
                row.depth,
            )
            .disabled(true),
            RowKind::Pattern { pattern, files } => {
                let selected = self.include.iter().any(
                    |s| matches!(s, PathSelector::Glob { pattern: known } if known == pattern),
                );
                TreeViewItem::rich(
                    row.id.clone(),
                    Line::from_spans(vec![
                        Self::check_span(
                            if selected {
                                FolderSelection::All
                            } else {
                                FolderSelection::None
                            },
                            theme,
                        ),
                        Span::styled("Select pattern ", theme.text),
                        Span::styled(pattern.clone(), theme.accent),
                    ]),
                    0,
                )
                .trailing([Line::from_spans(vec![Span::styled(
                    format!("{} now", Self::files_text(*files)),
                    theme.muted,
                )])])
                .shrink_span(2)
            }
            RowKind::Range { path, start, end } => TreeViewItem::rich(
                row.id.clone(),
                Line::from_spans(vec![
                    Span::styled(format!("Add lines {start}–{end} of "), theme.text),
                    Span::styled(path.clone(), theme.path),
                ]),
                0,
            )
            .shrink_span(1),
            RowKind::Selected { exclude, index } => {
                self.selected_item(row, *exclude, *index, theme)
            }
        }
    }

    fn entry_item(
        &self,
        row: &Row,
        entry: &ReviewPathEntry,
        searching: bool,
        theme: &ReviewTheme,
    ) -> TreeViewItem {
        let ignored = entry.status == ReviewPathStatus::Ignored;
        let name_style = if ignored {
            theme.muted
        } else if entry.kind == ReviewPathKind::Folder {
            theme.directory
        } else {
            theme.path
        };
        let name = if searching {
            entry.path.clone()
        } else if entry.kind == ReviewPathKind::Folder {
            format!("{}/", entry.name)
        } else {
            entry.name.clone()
        };
        let label = Line::from_spans(vec![
            Self::check_span(self.check(entry), theme),
            Span::styled(name, name_style),
        ]);
        let mut trailing = Vec::new();
        match entry.kind {
            ReviewPathKind::Folder => {
                let count = if self.purpose == PathPurpose::Uncommitted {
                    Some(format!("{} changed", entry.changed))
                } else {
                    entry.files.map(Self::files_text)
                };
                if let Some(count) = count {
                    trailing.push(Line::from_spans(vec![Span::styled(count, theme.muted)]));
                }
            }
            ReviewPathKind::File => {
                let letter = match entry.status {
                    ReviewPathStatus::Ignored => Some(("!", theme.muted)),
                    ReviewPathStatus::Untracked => Some(("?", theme.status_added)),
                    ReviewPathStatus::Tracked if entry.changed > 0 => {
                        Some(("M", theme.status_modified))
                    }
                    ReviewPathStatus::Tracked => None,
                };
                if let Some((letter, style)) = letter {
                    trailing.push(Line::from_spans(vec![Span::styled(letter, style)]));
                }
            }
        }
        TreeViewItem::rich(row.id.clone(), label, row.depth)
            .trailing(trailing)
            .expandable(entry.kind == ReviewPathKind::Folder && !searching)
            .shrink_span(1)
    }

    fn selected_item(
        &self,
        row: &Row,
        exclude: bool,
        index: usize,
        theme: &ReviewTheme,
    ) -> TreeViewItem {
        let selector = self
            .selector(exclude, index)
            .map(ToString::to_string)
            .unwrap_or_default();
        let count = self
            .counts
            .as_ref()
            .filter(|_| self.counted == self.selection_revision)
            .and_then(|counts| {
                if exclude {
                    counts.exclude.get(index)
                } else {
                    counts.include.get(index)
                }
            });
        let trailing = count.map(|count| {
            if count.error.is_some() && count.files == 0 {
                Line::from_spans(vec![Span::styled("missing", theme.error)])
            } else if count.files == 0 {
                Line::from_spans(vec![Span::styled("0", theme.notice)])
            } else {
                Line::from_spans(vec![Span::styled(count.files.to_string(), theme.muted)])
            }
        });
        TreeViewItem::rich(
            row.id.clone(),
            Line::from_spans(vec![
                if exclude {
                    Span::styled(format!("{} ", theme.icons.minus), theme.deletions)
                } else {
                    Span::styled(CHECK_ON, theme.accent)
                },
                Span::styled(selector, theme.path),
            ]),
            0,
        )
        .trailing(trailing)
        .shrink_span(1)
    }

    fn summary(&self, theme: &ReviewTheme) -> Line {
        if self.mode == Mode::Selection {
            return Line::from_spans(vec![
                Span::styled("Selected", theme.title),
                Span::styled(" · space removes · esc back to the tree", theme.muted),
            ]);
        }
        if let Some(status) = &self.status {
            return Line::from_spans(vec![Span::styled(status.clone(), theme.notice)]);
        }
        if self.include.is_empty() {
            return Line::from_spans(vec![Span::styled(
                "Nothing selected · space selects",
                theme.muted,
            )]);
        }
        let items = self.include.len() + self.exclude.len();
        let files = self.total().map_or_else(
            || "counting…".to_owned(),
            |count| {
                if self.purpose == PathPurpose::Uncommitted {
                    format!("{count} changed file{}", if count == 1 { "" } else { "s" })
                } else {
                    Self::files_text(count)
                }
            },
        );
        Line::from_spans(vec![
            Span::styled(
                format!(
                    "Selected: {items} item{} · {files}",
                    if items == 1 { "" } else { "s" }
                ),
                theme.text,
            ),
            Span::styled(" ▸", theme.accent),
        ])
    }

    fn ignored_line(&self, theme: &ReviewTheme) -> Line {
        Line::from_spans(vec![
            Self::check_span(
                if self.show_ignored {
                    FolderSelection::All
                } else {
                    FolderSelection::None
                },
                theme,
            ),
            Span::styled("Show ignored files", theme.text),
        ])
    }

    /// The tree items of the visible slice of rows.
    fn visible_items(&self, theme: &ReviewTheme, height: usize) -> Vec<TreeViewItem> {
        let end = (self.scroll + height).min(self.rows.len());
        self.rows[self.scroll.min(end)..end]
            .iter()
            .map(|row| self.item(row, theme))
            .collect()
    }

    fn sync_tree_state(&self, items: &[TreeViewItem]) {
        let mut state = self.tree_state.borrow_mut();
        state.set_expanded_ids(
            items
                .iter()
                .filter(|item| item.expandable && self.expanded.contains(bare(&item.id)))
                .map(|item| item.id.clone()),
        );
        state.set_focused(true);
        state.set_selected_visible(self.focus.checked_sub(self.scroll));
    }

    fn layout_frame(&self, theme: &ReviewTheme, area: Size, items: &[TreeViewItem]) -> LayoutNode {
        let frame = self.frame(theme, area, items);
        frame.layout(Constraints::tight(area), &mut LayoutCx::new())
    }

    fn frame<'a>(
        &'a self,
        theme: &ReviewTheme,
        area: Size,
        items: &'a [TreeViewItem],
    ) -> PickerFrameComponent<'a> {
        let width = area.width.min(120);
        let mut policy = PickerFramePolicy::palette()
            .placement(PickerFramePlacement::UpperThird)
            .max_size(Size::new(width, area.height));
        policy.input = true;
        policy.header = false;
        policy.footer = !self.hints.is_empty();
        policy.margin = Insets::all(0);
        policy.min_size = Size::new(width, area.height);
        let frame = PickerFrame::new()
            .title(&self.title)
            .footer(Line::from_spans(vec![Span::styled(
                self.hints.clone(),
                theme.muted,
            )]))
            .policy(policy)
            .styles(theme.component.picker_frame_styles())
            .chrome(theme.component.chrome);
        let tree = TreeViewComponent::new(TREE_ID, items, &self.tree_state)
            .styles(theme.tree_focused)
            .focus_style(bmux_tui::style::Style::new())
            .selection_bar(theme.tree_selection_bar())
            .trailing_layout(
                TreeViewTrailingLayout::new()
                    .min_label_width(8)
                    .label_overflow(TreeViewLabelOverflow::Ellipsis),
            );
        let body = Column::new()
            .id("wizard.paths.body")
            .flex(Flex::new(1, tree))
            .child(
                TextBlock::new(Text::from_lines(vec![self.summary(theme)]))
                    .id(SUMMARY_ID)
                    .wrap(TextWrap::None),
            )
            .child(
                TextBlock::new(Text::from_lines(vec![self.ignored_line(theme)]))
                    .id(IGNORED_ID)
                    .wrap(TextWrap::None),
            );
        PickerFrameComponent::new(FRAME_ID, frame, body).input(
            TextInputComponent::new(INPUT_ID, &self.filter, &self.filter_policy)
                .style(theme.input)
                .placeholder(
                    match self.purpose {
                        PathPurpose::Files => "type to filter · a glob · path:START-END",
                        PathPurpose::Folders | PathPurpose::Uncommitted => "type to filter",
                    },
                    theme.placeholder,
                )
                .focused(self.mode == Mode::Filter),
        )
    }

    /// Paint over `area` and keep the layout for pointer routing.
    pub fn paint(&mut self, theme: &ReviewTheme, area: Rect, cx: &mut PaintCx<'_, '_>) {
        if area.width == 0 || area.height == 0 {
            self.painted = None;
            return;
        }
        // Lay out with the rows the tree had last time, then again if the tree's height
        // changed, so the slice painted is exactly the rows the tree shows.
        let mut items = self.visible_items(theme, self.tree_rows.max(1));
        self.sync_tree_state(&items);
        let mut layout = self.layout_frame(theme, area.size(), &items);
        let height = layout
            .find_rect(&LayoutId::new(TREE_ID))
            .map_or(0, |rect| usize::from(rect.height));
        if height != self.tree_rows {
            self.tree_rows = height;
            self.keep_focus_visible();
            items = self.visible_items(theme, height.max(1));
            self.sync_tree_state(&items);
            layout = self.layout_frame(theme, area.size(), &items);
        }
        {
            let frame = self.frame(theme, area.size(), &items);
            cx.with_style(theme.tiers.overlay, |cx| {
                cx.with_child(
                    i32::from(area.x),
                    i64::from(area.y),
                    LocalRect::new(0, 0, area.width, area.height),
                    |cx| frame.paint(&layout, cx),
                );
            });
        }
        self.painted = Some((area, layout, items));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::backend::TestBackend;

    fn counted(total: u32) -> ReviewPathsResolveResponse {
        ReviewPathsResolveResponse {
            include: Vec::new(),
            exclude: Vec::new(),
            total,
            changed: 0,
            truncated: false,
        }
    }

    #[test]
    fn a_count_for_an_earlier_selection_is_not_shown_as_current() {
        let backend = TestBackend::default();
        let picker = PathPicker::new(
            backend.services(),
            PathBuf::from("/repo"),
            PathPurpose::Files,
        );
        let mut picker = picker.with_selection(
            vec![PathSelector::Folder {
                path: "docs".to_owned(),
            }],
            Vec::new(),
            false,
        );
        // A count is asked for the folder alone…
        let asked = picker.selection_revision;
        picker.counts_seq += 1;
        picker.counts_requested = asked;
        // …the selection grows before it answers…
        picker.include.push(PathSelector::File {
            path: "README.md".to_owned(),
        });
        picker.selection_changed();
        picker.apply(Message::Counted {
            seq: picker.counts_seq,
            revision: asked,
            result: Ok(counted(3)),
        });
        // …so the answer is not the selection's count, and another is due.
        assert_eq!(picker.total(), None);
        assert!(picker.loading());
        assert_ne!(picker.counts_requested, picker.selection_revision);
        // The answer for the selection now is current.
        picker.counts_seq += 1;
        picker.counts_requested = picker.selection_revision;
        picker.apply(Message::Counted {
            seq: picker.counts_seq,
            revision: picker.selection_revision,
            result: Ok(counted(4)),
        });
        assert_eq!(picker.total(), Some(4));
        assert!(!picker.loading());
    }
}
