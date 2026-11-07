//! Application state and the runtime program.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bcode_git::models::{
    ChangedFile, Commit, CommitMessageSource, DiffLineId, LineStats, RepoOperation, StatusEntry,
};
use bcode_git::{DiffTarget, LogQuery, Repository};
use bmux_keyboard::KeyStroke;
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::geometry::Point;
use bmux_tui::hit::HitMap;
use bmux_tui_components::selectable_list::SelectableListState;
use bmux_tui_components::text_input::{
    EnterBehavior, TextInputControl, TextInputOutcome, TextInputPolicy,
};
use bmux_tui_runtime::{Command, CommandKey, Program, RuntimeEvent, SubscriptionKey, Update};

pub use crate::config::ScreenMode;
use crate::config::{GitUiConfig, LineCountsShow};
use crate::data::{
    FileNode, MainContent, MainRequest, PresentContext, SectionKind, Snapshot, file_rows,
    load_main, load_snapshot,
};
use crate::diffview::RowKind;
use crate::graph;
use crate::host::GitUiHost;
use crate::keymap::{Action, Keymap, Resolution, Scope};
use crate::layout::{Layout, Start};
use crate::ops::{self, HeadPosition, Op};
use crate::panels::{PanelView, Panels, window_key};
use crate::popup::{External, Popup, Prompt, PromptKind, Then};
use crate::session::{
    SavedCommitFiles, SavedJournal, SavedMain, SavedStaging, SavedSubCommits, SavedTarget, Session,
    view_from_id, view_id,
};
use crate::theme::GitTheme;
use bmux_text_edit::pointer::{ClickPolicy, ClickTracker};

/// Top-level focus target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Window {
    /// A side window, by position in the configured layout (top first).
    Side(usize),
    /// Main view.
    Main,
}

impl Window {
    /// Window number shown in titles: side windows count from 1, the main view is 0.
    #[must_use]
    pub const fn number(self) -> usize {
        match self {
            Self::Side(index) => index + 1,
            Self::Main => 0,
        }
    }
}

/// A list shown in a side window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum View {
    /// Status summary.
    Status,
    /// Changed files.
    Files,
    /// Worktrees.
    Worktrees,
    /// Submodules.
    Submodules,
    /// Local branches.
    Branches,
    /// Remotes.
    Remotes,
    /// Branches of one remote.
    RemoteBranches,
    /// Tags.
    Tags,
    /// Commits of a selected branch or tag.
    SubCommits,
    /// History.
    Commits,
    /// Reflog.
    Reflog,
    /// Stash.
    Stash,
    /// Files of a commit or stash.
    CommitFiles,
}

impl View {
    /// Every view.
    pub const ALL: [Self; 13] = [
        Self::Status,
        Self::Files,
        Self::Worktrees,
        Self::Submodules,
        Self::Branches,
        Self::Remotes,
        Self::RemoteBranches,
        Self::Tags,
        Self::SubCommits,
        Self::Commits,
        Self::Reflog,
        Self::Stash,
        Self::CommitFiles,
    ];

    /// Keymap scope.
    #[must_use]
    pub const fn scope(self) -> Scope {
        match self {
            Self::Status => Scope::Status,
            Self::Files => Scope::Files,
            Self::Worktrees => Scope::Worktrees,
            Self::Submodules => Scope::Submodules,
            Self::Branches => Scope::Branches,
            Self::Remotes => Scope::Remotes,
            Self::RemoteBranches => Scope::RemoteBranches,
            Self::Tags => Scope::Tags,
            Self::SubCommits | Self::Commits => Scope::Commits,
            Self::Reflog => Scope::Reflog,
            Self::Stash => Scope::Stash,
            Self::CommitFiles => Scope::CommitFiles,
        }
    }

    /// Tab label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::Files | Self::CommitFiles => "Files",
            Self::Worktrees => "Worktrees",
            Self::Submodules => "Submodules",
            Self::Branches => "Local",
            Self::Remotes => "Remotes",
            Self::RemoteBranches => "Remote branches",
            Self::Tags => "Tags",
            Self::SubCommits | Self::Commits => "Commits",
            Self::Reflog => "Reflog",
            Self::Stash => "Stash",
        }
    }
}

/// Row identity within a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// Status summary.
    Status,
    /// Files panel node index.
    File(usize),
    /// Worktree index.
    Worktree(usize),
    /// Submodule index.
    Submodule(usize),
    /// Branch index.
    Branch(usize),
    /// Remote index.
    Remote(usize),
    /// Remote branch index.
    RemoteBranch(usize),
    /// Tag index.
    Tag(usize),
    /// Sub-commit index.
    SubCommit(usize),
    /// Commit index.
    Commit(usize),
    /// Reflog index.
    Reflog(usize),
    /// Stash index.
    Stash(usize),
    /// Commit file index.
    CommitFile(usize),
}

/// Runtime messages.
#[derive(Debug)]
pub enum Msg {
    /// Panel data loaded.
    Snapshot(u64, Box<Snapshot>),
    /// Main view loaded.
    Main(u64, Box<MainContent>),
    /// Commit files loaded.
    CommitFiles(u64, Result<crate::data::ChangedFiles, String>),
    /// Sub-commits loaded.
    SubCommits(u64, Result<Vec<Commit>, String>),
    /// More history loaded.
    MoreCommits(Result<Vec<Commit>, String>),
    /// Operation finished.
    OpDone(Box<OpResult>),
    /// The repository may have changed in ways nothing reported (polling without a watcher,
    /// or headless use): scan everything again.
    Watch,
    /// A commit message prefill source may have changed while the commit editor is open.
    CommitSourcesChanged,
    /// Prefill found after a change, for an editor that is still empty.
    CommitPrefill(Box<crate::commit_message::Prefill>),
    /// Periodic tick.
    Tick,
    /// Time for the next spinner frame (sent while [`App::animation`] says so).
    Frame,
    /// The forge answered about pull requests.
    PullRequests(
        u64,
        Result<bcode_forge_models::PullRequestList, bcode_forge_models::ForgeError>,
    ),
    /// Local branches were matched to pull requests.
    PullRequestMatches(
        u64,
        std::collections::BTreeMap<String, crate::pulls::Matched>,
    ),
    /// The repository's live state changed.
    Live(Box<bcode_git_live::LiveUpdate>),
    /// Reload the side panels' git metadata (refs, stashes, tags, worktrees, …).
    Metadata,
    /// Terminal input failed.
    InputFailed(String),
}

/// Completion of a queued operation.
#[derive(Debug)]
pub struct OpResult {
    /// Label.
    pub label: String,
    /// Output or error.
    pub result: Result<Option<String>, String>,
    /// `HEAD` before the operation.
    pub before: Option<HeadPosition>,
    /// `HEAD` after the operation.
    pub after: Option<HeadPosition>,
    /// Show output in a popup.
    pub show_output: bool,
    /// Whether the operation talked to a remote (so pull requests may have changed).
    pub network: bool,
    /// Undo-journal movement.
    pub journal: Option<ops::JournalMove>,
}

/// The commit or stash whose files are listed.
#[derive(Debug, Clone)]
pub struct CommitFiles {
    /// Diff target.
    pub target: DiffTarget,
    /// Title.
    pub title: String,
    /// Revision (commit id or stash selector).
    pub revision: String,
    /// Files.
    pub files: Vec<ChangedFile>,
    /// Lines changed by (new) path.
    pub counts: BTreeMap<String, LineStats>,
    /// Window hosting the view.
    pub window: Window,
    /// Paths included in the custom patch.
    pub patch: BTreeSet<String>,
}

/// Commits of a selected ref, shown in the window it was opened from.
#[derive(Debug, Clone)]
pub struct SubCommits {
    /// Window hosting the view.
    pub window: Window,
    /// Title.
    pub title: String,
    /// Revision.
    pub revision: String,
    /// Commits.
    pub commits: Vec<Commit>,
    /// Graph cells.
    pub graph: Vec<Vec<graph::Cell>>,
}

/// Line selection while staging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Staging {
    /// Range anchor row.
    pub anchor: Option<usize>,
    /// Select whole hunks.
    pub hunk_mode: bool,
}

/// Transient message.
#[derive(Debug, Clone)]
pub struct Toast {
    /// Text.
    pub text: String,
    /// Whether it reports a failure.
    pub error: bool,
    /// Expiry.
    pub until: Instant,
}

/// An entry of the undo journal.
#[derive(Debug, Clone)]
pub struct Journal {
    /// What was done.
    pub label: String,
    /// Position before.
    pub before: HeadPosition,
    /// Position after.
    pub after: HeadPosition,
}

/// Application state.
// Independent UI toggles (tree view, command log, watcher, quit, inline execution) are
// distinct facts rather than one state machine.
#[allow(clippy::struct_excessive_bools)]
pub struct App {
    pub(crate) repo: Repository,
    pub(crate) config: GitUiConfig,
    pub(crate) keymap: Keymap,
    pub(crate) theme: GitTheme,
    pub(crate) cx: PresentContext,
    pub(crate) host: Arc<dyn GitUiHost>,

    pub(crate) snapshot: Snapshot,
    /// Pull requests from the forge.
    pub(crate) pulls: crate::pulls::PullRequests,
    pub(crate) snapshot_requested: u64,
    pub(crate) snapshot_loaded: u64,
    pub(crate) loaded_once: bool,
    pub(crate) commit_graph: Vec<Vec<graph::Cell>>,
    pub(crate) file_nodes: Vec<FileNode>,
    pub(crate) collapsed: BTreeSet<String>,
    pub(crate) file_tree: bool,

    pub(crate) layout: Layout,
    pub(crate) panels: Panels,
    /// Area of the open popup in the last painted frame.
    pub(crate) popup_area: Cell<Option<bmux_tui::geometry::Rect>>,
    /// Scroll geometry of every list in the last painted frame, by list id.
    pub(crate) painted: std::cell::RefCell<BTreeMap<String, crate::scroll::Painted>>,
    /// Scroll position of the command log.
    pub(crate) log_scroll: Cell<bmux_tui_components::scroll_view::ScrollViewState>,
    /// Command-log entries shown, newest first.
    pub(crate) log_records: Vec<bcode_git::models::GitCommandRecord>,
    /// Their terminal output, by record id.
    pub(crate) log_terminals: crate::terminal::LogOutputs,
    /// Command-log revision last synced.
    pub(crate) log_revision: Option<u64>,
    /// Shell commands run from the `:` prompt.
    pub(crate) shell_history: crate::popup::InputRecall,
    /// Main view position waiting for its content after a restore.
    pub(crate) pending_main: Option<SavedMain>,
    /// Drilled-in views waiting for repository data after a restore.
    pub(crate) pending_drill: Option<(Option<SavedSubCommits>, Option<SavedCommitFiles>)>,
    pub(crate) clicks: ClickTracker,
    pub(crate) commit_sources: Vec<CommitMessageSource>,
    pub(crate) env_message: Option<String>,
    pub(crate) window: Window,
    pub(crate) last_side: Window,
    pub(crate) tabs: BTreeMap<Window, usize>,
    pub(crate) cursors: BTreeMap<View, usize>,
    pub(crate) keys: BTreeMap<View, String>,
    pub(crate) filters: BTreeMap<View, String>,
    pub(crate) lists: BTreeMap<View, Cell<SelectableListState>>,
    pub(crate) remote: Option<String>,
    pub(crate) sub_commits: Option<SubCommits>,
    pub(crate) sub_commits_generation: u64,
    pub(crate) commit_files: Option<CommitFiles>,
    pub(crate) commit_files_generation: u64,

    pub(crate) main: Option<MainContent>,
    pub(crate) main_requested: Option<MainRequest>,
    pub(crate) main_generation: u64,
    pub(crate) main_section: usize,
    pub(crate) main_cursor: [usize; 2],
    pub(crate) main_lists: [Cell<SelectableListState>; 2],
    pub(crate) staging: Option<Staging>,

    pub(crate) popups: Vec<Popup>,
    pub(crate) pending: Vec<KeyStroke>,
    pub(crate) pending_since: Option<Instant>,
    pub(crate) toast: Option<Toast>,
    pub(crate) queue: VecDeque<(Op, bool)>,
    /// Running, queued and just-finished work, shown where it lands.
    pub(crate) activity: crate::activity::Activity,
    /// Set while something animates, so the runtime sends spinner frames.
    pub(crate) animating: Arc<std::sync::atomic::AtomicBool>,
    /// Milliseconds between frames while animating.
    pub(crate) frame_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Rows flashing after changes, and what the next refresh is compared with.
    pub(crate) flashes: crate::flash::Flashes,
    /// Lines changed in each worktree (this one included), by root.
    pub(crate) worktree_totals: BTreeMap<PathBuf, LineStats>,
    /// Where the live status and line counts come from.
    pub(crate) live: Option<Live>,
    /// How to reach the shared daemon, when it may be used.
    pub(crate) daemon: Option<bcode_git_daemon::ClientOptions>,
    pub(crate) command_log: bool,
    pub(crate) screen_mode: ScreenMode,
    pub(crate) cherry: Vec<Commit>,
    pub(crate) undo: Vec<Journal>,
    pub(crate) redo: Vec<Journal>,
    pub(crate) diff_base: Option<String>,
    pub(crate) last_poll: Instant,
    pub(crate) last_fetch: Instant,
    pub(crate) watching: bool,

    pub(crate) size: Cell<(u16, u16)>,
    pub(crate) hits: RefCell<HitMap>,
    pub(crate) external: Option<External>,
    pub(crate) quit: bool,
    pub(crate) diagnostics: Vec<String>,
    pub(crate) inline: bool,
    pub(crate) inbox: VecDeque<Msg>,
}

/// Where live repository state comes from.
pub(crate) enum Live {
    /// A session reacting to filesystem changes (served by the daemon or in-process).
    Session(Box<dyn bcode_git_live::LiveSession>),
    /// Served synchronously, for inline (test and headless) use.
    Inline(Box<bcode_git_live::InlineSession>),
}

const PENDING_TIMEOUT: Duration = Duration::from_millis(1500);
/// Longest time between frames while a row flashes, so fades look smooth.
const FLASH_FRAME: Duration = Duration::from_millis(33);
/// Most other worktrees measured for branch flashes and counts.
const WORKTREES_MEASURED: usize = 16;

impl App {
    /// Create application state.
    // One initializer per state field; splitting it would only scatter the defaults.
    #[allow(clippy::too_many_lines)]
    #[must_use]
    pub fn new(
        repo: Repository,
        config: GitUiConfig,
        keymap: Keymap,
        theme: GitTheme,
        host: Arc<dyn GitUiHost>,
        diagnostics: Vec<String>,
    ) -> Self {
        let cx = PresentContext {
            theme: theme.clone(),
            context: config.diff_context,
            highlight: config.highlight.enabled,
            max_file_bytes: config.highlight.max_file_bytes,
            relative_dates: config.relative_dates,
            show_graph: config.show_graph,
            branches: crate::branches::BranchColors::default(),
            pull_status: None,
        };
        let file_tree = config.files.tree;
        let (layout, layout_diagnostics) = Layout::resolve(&config.layout);
        let (commit_sources, commit_diagnostics) = config.commit.resolve_sources();
        let (branch_colors, branch_diagnostics) =
            crate::branches::BranchColors::new(&config.branches, &theme);
        let mut cx = cx;
        cx.branches = branch_colors;
        let (activity, activity_diagnostics) = crate::activity::Activity::new(&config.activity);
        let mut diagnostics = diagnostics;
        diagnostics.extend(activity_diagnostics);
        diagnostics.extend(branch_diagnostics);
        diagnostics.extend(layout_diagnostics);
        diagnostics.extend(commit_diagnostics);
        let screen_mode = config.layout.screen_mode;
        let command_log = config.layout.command_log.show;
        let start = layout.start;
        let panels = Panels::new(&config.layout, &layout.windows, None);
        let mut app = Self {
            repo,
            config,
            keymap,
            theme,
            cx,
            host,
            pulls: crate::pulls::PullRequests::new(None, None),
            snapshot: Snapshot::default(),
            snapshot_requested: 0,
            snapshot_loaded: 0,
            loaded_once: false,
            commit_graph: Vec::new(),
            file_nodes: Vec::new(),
            collapsed: BTreeSet::new(),
            file_tree,
            layout,
            panels,
            popup_area: Cell::new(None),
            painted: std::cell::RefCell::default(),
            log_scroll: Cell::default(),
            log_records: Vec::new(),
            log_terminals: crate::terminal::LogOutputs::new(),
            log_revision: None,
            shell_history: crate::popup::InputRecall::default(),
            pending_main: None,
            pending_drill: None,
            clicks: ClickTracker::new(),
            commit_sources,
            env_message: None,
            window: Window::Side(0),
            last_side: Window::Side(0),
            tabs: BTreeMap::new(),
            cursors: BTreeMap::new(),
            keys: BTreeMap::new(),
            filters: BTreeMap::new(),
            lists: View::ALL
                .iter()
                .map(|view| (*view, Cell::new(SelectableListState::new(Some(0)))))
                .collect(),
            remote: None,
            sub_commits: None,
            sub_commits_generation: 0,
            commit_files: None,
            commit_files_generation: 0,
            main: None,
            main_requested: None,
            main_generation: 0,
            main_section: 0,
            main_cursor: [0, 0],
            main_lists: [
                Cell::new(SelectableListState::new(None)),
                Cell::new(SelectableListState::new(None)),
            ],
            staging: None,
            popups: Vec::new(),
            pending: Vec::new(),
            pending_since: None,
            toast: None,
            queue: VecDeque::new(),
            activity,
            animating: Arc::default(),
            frame_ms: Arc::default(),
            flashes: crate::flash::Flashes::default(),
            worktree_totals: BTreeMap::new(),
            live: None,
            daemon: None,
            command_log,
            screen_mode,
            cherry: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            diff_base: None,
            last_poll: Instant::now(),
            last_fetch: Instant::now(),
            watching: false,
            size: Cell::new((80, 24)),
            hits: RefCell::new(HitMap::new()),
            external: None,
            quit: false,
            diagnostics,
            inline: false,
            inbox: VecDeque::new(),
        };
        match start {
            Start::View(view) => {
                app.focus_view(view);
            }
            Start::Main => {
                if !app.focus_view(View::Files) {
                    app.focus(Window::Side(0));
                }
                app.window = Window::Main;
            }
        }
        app.sync_panels();
        app
    }

    /// A flag set while something animates, and how many milliseconds apart to send
    /// [`Msg::Frame`] then; both change as the UI runs.
    #[must_use]
    pub fn animation(
        &self,
    ) -> (
        Arc<std::sync::atomic::AtomicBool>,
        Arc<std::sync::atomic::AtomicU64>,
    ) {
        self.sync_animation();
        (self.animating.clone(), self.frame_ms.clone())
    }

    /// Publish whether anything animates and how often frames are needed for it.
    fn sync_animation(&self) {
        let flashing = self.flashes.active();
        self.animating.store(
            self.activity.animating() || flashing,
            std::sync::atomic::Ordering::Relaxed,
        );
        let spinner = self.activity.interval();
        let interval = if flashing {
            spinner.min(FLASH_FRAME)
        } else {
            spinner
        };
        self.frame_ms.store(
            u64::try_from(interval.as_millis())
                .unwrap_or(u64::MAX)
                .max(1),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// External program requested before the runtime exited, if any.
    pub const fn take_external(&mut self) -> Option<External> {
        self.external.take()
    }

    /// Whether the user asked to quit.
    #[must_use]
    pub const fn quitting(&self) -> bool {
        self.quit
    }

    /// Interactive regions of the last presented frame, as `(id, rect)`.
    #[must_use]
    pub fn hit_regions(&self) -> Vec<(String, bmux_tui::geometry::Rect)> {
        self.hits
            .borrow()
            .regions()
            .iter()
            .map(|region| (region.id.as_str().to_owned(), region.area))
            .collect()
    }

    /// Focused window.
    #[must_use]
    pub const fn focused_window(&self) -> Window {
        self.window
    }

    /// Whether a shell command started from the UI is still running (for tests and
    /// automation).
    #[must_use]
    pub fn terminal_running(&self) -> bool {
        self.popups
            .iter()
            .any(|popup| matches!(popup, Popup::Terminal(run) if run.process.is_some()))
    }

    /// Cursor index of a view.
    #[must_use]
    pub fn view_cursor(&self, view: View) -> usize {
        self.cursor(view)
    }

    /// Record whether filesystem watching is active.
    pub const fn set_watching(&mut self, watching: bool) {
        self.watching = watching;
    }

    // ----- views and rows -----

    /// View shown in a side window.
    #[must_use]
    pub fn view(&self, window: Window) -> View {
        if window == Window::Main {
            return self.view(self.last_side);
        }
        if let Some(files) = &self.commit_files
            && files.window == window
        {
            return View::CommitFiles;
        }
        if let Some(sub) = &self.sub_commits
            && sub.window == window
        {
            return View::SubCommits;
        }
        let tabs = self.tabs(window);
        let Some(last) = tabs.len().checked_sub(1) else {
            return View::Status;
        };
        match tabs[self.tabs.get(&window).copied().unwrap_or(0).min(last)] {
            View::Remotes if self.remote.is_some() => View::RemoteBranches,
            view => view,
        }
    }

    /// Side windows from top to bottom.
    pub fn side_windows(&self) -> impl Iterator<Item = Window> + use<> {
        (0..self.layout.windows.len()).map(Window::Side)
    }

    /// Tabs of a side window.
    #[must_use]
    pub fn tabs(&self, window: Window) -> &[View] {
        match window {
            Window::Side(index) => self.layout.windows.get(index).map_or(&[], Vec::as_slice),
            Window::Main => &[],
        }
    }

    /// Focus the window holding a view and switch to its tab. Returns `false` when the
    /// layout hides the view.
    pub(crate) fn focus_view(&mut self, view: View) -> bool {
        let Some((window, tab)) = self.layout.position(view) else {
            return false;
        };
        let remotes = self.layout.windows[window].contains(&View::Remotes);
        let window = Window::Side(window);
        if self.tabs.get(&window).copied().unwrap_or(0) != tab {
            self.tabs.insert(window, tab);
            if remotes {
                self.remote = None;
            }
        }
        self.focus(window);
        true
    }

    /// Focused side view (the last side view when the main view is focused).
    #[must_use]
    pub fn current_view(&self) -> View {
        self.view(if self.window == Window::Main {
            self.last_side
        } else {
            self.window
        })
    }

    /// Rows of a view after filtering.
    #[must_use]
    pub fn rows(&self, view: View) -> Vec<Row> {
        let all: Vec<Row> = match view {
            View::Status => vec![Row::Status],
            View::Files => (0..self.file_nodes.len()).map(Row::File).collect(),
            View::Worktrees => (0..self.snapshot.worktrees.len())
                .map(Row::Worktree)
                .collect(),
            View::Submodules => (0..self.snapshot.submodules.len())
                .map(Row::Submodule)
                .collect(),
            View::Branches => (0..self.snapshot.branches.len()).map(Row::Branch).collect(),
            View::Remotes => (0..self.snapshot.remotes.len()).map(Row::Remote).collect(),
            View::RemoteBranches => self
                .snapshot
                .remote_branches
                .iter()
                .enumerate()
                .filter(|(_, branch)| Some(&branch.remote) == self.remote.as_ref())
                .map(|(index, _)| Row::RemoteBranch(index))
                .collect(),
            View::Tags => (0..self.snapshot.tags.len()).map(Row::Tag).collect(),
            View::SubCommits => (0..self.sub_commits.as_ref().map_or(0, |sub| sub.commits.len()))
                .map(Row::SubCommit)
                .collect(),
            View::Commits => (0..self.snapshot.commits.len()).map(Row::Commit).collect(),
            View::Reflog => (0..self.snapshot.reflog.len()).map(Row::Reflog).collect(),
            View::Stash => (0..self.snapshot.stashes.len()).map(Row::Stash).collect(),
            View::CommitFiles => (0..self
                .commit_files
                .as_ref()
                .map_or(0, |files| files.files.len()))
                .map(Row::CommitFile)
                .collect(),
        };
        let Some(filter) = self.filters.get(&view).filter(|filter| !filter.is_empty()) else {
            return all;
        };
        let filter = filter.to_lowercase();
        all.into_iter()
            .filter(|row| self.row_text(*row).to_lowercase().contains(&filter))
            .collect()
    }

    /// Searchable text and stable key of a row.
    #[must_use]
    pub fn row_text(&self, row: Row) -> String {
        let snapshot = &self.snapshot;
        match row {
            Row::Status => String::new(),
            Row::File(index) => match &self.file_nodes[index] {
                FileNode::Dir { path, .. } => path.clone(),
                FileNode::File { entry, .. } => snapshot.status.entries[*entry].path.clone(),
            },
            Row::Worktree(index) => snapshot.worktrees[index].path.display().to_string(),
            Row::Submodule(index) => snapshot.submodules[index].path.clone(),
            Row::Branch(index) => snapshot.branches[index].name.clone(),
            Row::Remote(index) => snapshot.remotes[index].name.clone(),
            Row::RemoteBranch(index) => snapshot.remote_branches[index].full_name(),
            Row::Tag(index) => snapshot.tags[index].name.clone(),
            Row::SubCommit(index) => self.sub_commits.as_ref().map_or_else(String::new, |sub| {
                format!("{} {}", sub.commits[index].oid, sub.commits[index].summary)
            }),
            Row::Commit(index) => format!(
                "{} {} {}",
                snapshot.commits[index].oid,
                snapshot.commits[index].summary,
                snapshot.commits[index].author_name
            ),
            Row::Reflog(index) => format!(
                "{} {}",
                snapshot.reflog[index].oid, snapshot.reflog[index].message
            ),
            Row::Stash(index) => snapshot.stashes[index].message.clone(),
            Row::CommitFile(index) => self
                .commit_files
                .as_ref()
                .map_or_else(String::new, |files| files.files[index].path.clone()),
        }
    }

    /// Selected row of a view.
    #[must_use]
    pub fn selected(&self, view: View) -> Option<Row> {
        let rows = self.rows(view);
        rows.get(self.cursor(view).min(rows.len().saturating_sub(1)))
            .copied()
    }

    pub(crate) fn cursor(&self, view: View) -> usize {
        self.cursors.get(&view).copied().unwrap_or(0)
    }

    pub(crate) fn set_cursor(&mut self, view: View, index: usize) {
        let len = self.rows(view).len();
        let index = index.min(len.saturating_sub(1));
        self.cursors.insert(view, index);
        if let Some(row) = self.rows(view).get(index) {
            let key = self.row_text(*row);
            self.keys.insert(view, key);
        }
    }

    /// Restore cursors by stable key after data changes.
    fn restore_cursors(&mut self) {
        for view in View::ALL {
            let Some(key) = self.keys.get(&view).cloned() else {
                continue;
            };
            let rows = self.rows(view);
            if let Some(index) = rows.iter().position(|row| self.row_text(*row) == key) {
                self.cursors.insert(view, index);
            } else {
                let clamped = self.cursor(view).min(rows.len().saturating_sub(1));
                self.cursors.insert(view, clamped);
            }
        }
    }

    pub(crate) fn selected_entry(&self) -> Option<&StatusEntry> {
        match self.selected(View::Files)? {
            Row::File(index) => match &self.file_nodes[index] {
                FileNode::File { entry, .. } => self.snapshot.status.entries.get(*entry),
                FileNode::Dir { .. } => None,
            },
            _ => None,
        }
    }

    pub(crate) fn selected_entries(&self) -> Vec<&StatusEntry> {
        match self.selected(View::Files) {
            Some(Row::File(index)) => self.file_nodes[index]
                .entries()
                .into_iter()
                .filter_map(|entry| self.snapshot.status.entries.get(entry))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Commit selected in the focused commit-like view.
    pub(crate) fn selected_commit(&self) -> Option<Commit> {
        match self.selected(self.current_view())? {
            Row::Commit(index) => self.snapshot.commits.get(index).cloned(),
            Row::SubCommit(index) => self.sub_commits.as_ref()?.commits.get(index).cloned(),
            _ => None,
        }
    }

    pub(crate) fn current_branch(&self) -> Option<String> {
        self.snapshot.status.branch.head.clone()
    }

    // ----- loading -----

    pub(crate) fn spawn<T: Send + 'static>(
        &mut self,
        key: Option<&str>,
        work: impl FnOnce() -> T + Send + 'static,
        wrap: impl FnOnce(T) -> Msg + Send + 'static,
    ) -> Option<Command<Msg>> {
        if self.inline {
            self.inbox.push_back(wrap(work()));
            return None;
        }
        let future = async move { tokio::task::spawn_blocking(work).await.ok().map(wrap) };
        Some(match key {
            Some(key) => Command::queue_latest(CommandKey::new(key), future),
            None => Command::concurrent(future),
        })
    }

    /// Run background work synchronously, queueing results for [`Self::drain_inline`].
    /// Intended for tests and headless automation.
    pub const fn set_inline(&mut self, inline: bool) {
        self.inline = inline;
    }

    /// Apply queued inline results until none remain.
    ///
    /// # Errors
    /// Propagates update errors.
    pub fn drain_inline(&mut self) -> Result<(), String> {
        while let Some(message) = self.inbox.pop_front() {
            self.update(RuntimeEvent::Message(message))?;
        }
        Ok(())
    }

    /// Bring everything up to date after git ran: check the index and `HEAD` again and reload
    /// git metadata.
    pub(crate) fn refresh(&mut self) -> Option<Command<Msg>> {
        self.live_request(bcode_git_live::Request::Check);
        self.reload_metadata()
    }

    /// Scan everything again and reload git metadata.
    pub(crate) fn refresh_all(&mut self) -> Option<Command<Msg>> {
        self.live_request(bcode_git_live::Request::Full);
        self.reload_metadata()
    }

    /// Reload the side panels' git metadata; status and line counts come from live state.
    pub(crate) fn reload_metadata(&mut self) -> Option<Command<Msg>> {
        self.snapshot_requested += 1;
        let generation = self.snapshot_requested;
        let repo = self.repo.clone();
        let limit = self.snapshot.commits.len().max(self.config.log_page_size);
        let merged = self.config.branches.dim_merged;
        self.activity.begin(crate::activity::Load::Repository);
        self.spawn(
            Some("snapshot"),
            move || load_snapshot(&repo, limit, merged),
            move |snapshot| Msg::Snapshot(generation, Box::new(snapshot)),
        )
    }

    /// What live state keeps for this UI.
    #[must_use]
    pub fn live_config(&self) -> bcode_git_live::EngineConfig {
        use bcode_git_live::CountsMode;
        let counts = match self.config.files.line_counts.show {
            // Flashes and branch counts measure changes even when files show no counts.
            LineCountsShow::Off if self.counts_needed() => CountsMode::Total,
            LineCountsShow::Off => CountsMode::Off,
            LineCountsShow::Total => CountsMode::Total,
            LineCountsShow::Split => CountsMode::Split,
        };
        bcode_git_live::EngineConfig {
            counts,
            other_worktrees: self.config.flash.branches() || self.config.branches.line_counts,
            max_other_worktrees: WORKTREES_MEASURED,
            scanner: self.config.refresh.scanner.into(),
        }
    }

    /// Take live state from `session`, which reports through [`Msg::Live`].
    pub fn set_live_session(&mut self, session: Box<dyn bcode_git_live::LiveSession>) {
        self.live = Some(Live::Session(session));
    }

    /// Ask live state for work. Inline, it runs now and its result is queued.
    fn live_request(&mut self, request: bcode_git_live::Request) {
        if self.live.is_none() && self.inline {
            let session = bcode_git_live::InlineSession::new(&self.repo, self.live_config());
            self.live = Some(Live::Inline(Box::new(session)));
        }
        match &mut self.live {
            Some(Live::Session(session)) => session.request(request),
            Some(Live::Inline(session)) => {
                let update = session.request(request);
                if !update.is_empty() {
                    self.inbox.push_back(Msg::Live(Box::new(update)));
                }
            }
            None => {}
        }
    }

    /// Main request for the current selection.
    #[must_use]
    pub fn main_request(&self) -> MainRequest {
        let view = self.current_view();
        let row = self.selected(view);
        let commit_like = |oid: &str| {
            self.diff_base.as_ref().map_or_else(
                || MainRequest::Commit {
                    oid: oid.to_owned(),
                    path: None,
                },
                |base| MainRequest::Range {
                    from: base.clone(),
                    to: oid.to_owned(),
                    path: None,
                },
            )
        };
        let log_like = |revision: String| match &self.diff_base {
            Some(base) => MainRequest::Range {
                from: base.clone(),
                to: revision,
                path: None,
            },
            None => MainRequest::Log { revision },
        };
        match (view, row) {
            (View::Status, _) => MainRequest::Status,
            (View::Files, Some(Row::File(index))) => match &self.file_nodes[index] {
                FileNode::File { entry, .. } => {
                    MainRequest::File(self.snapshot.status.entries[*entry].clone())
                }
                FileNode::Dir { path, entries, .. } => {
                    MainRequest::Message(format!("{path}/ · {} changed files", entries.len()))
                }
            },
            (View::Files, _) => MainRequest::Message("Working tree clean ✨".into()),
            (View::Worktrees, Some(Row::Worktree(index))) => {
                let worktree = &self.snapshot.worktrees[index];
                MainRequest::Message(format!(
                    "{}\n{}",
                    worktree.path.display(),
                    worktree.branch.as_deref().unwrap_or("(detached)")
                ))
            }
            (View::Submodules, Some(Row::Submodule(index))) => MainRequest::Log {
                revision: self.snapshot.submodules[index].oid.clone(),
            },
            (View::Branches, Some(Row::Branch(index))) => {
                log_like(self.snapshot.branches[index].name.clone())
            }
            (View::Remotes, Some(Row::Remote(index))) => {
                let remote = &self.snapshot.remotes[index];
                MainRequest::Message(format!(
                    "{}\nfetch {}\npush  {}",
                    remote.name,
                    remote.fetch_url.as_deref().unwrap_or("-"),
                    remote.push_url.as_deref().unwrap_or("-")
                ))
            }
            (View::RemoteBranches, Some(Row::RemoteBranch(index))) => {
                log_like(self.snapshot.remote_branches[index].full_name())
            }
            (View::Tags, Some(Row::Tag(index))) => log_like(self.snapshot.tags[index].name.clone()),
            (View::SubCommits, Some(Row::SubCommit(index))) => self
                .sub_commits
                .as_ref()
                .map_or(MainRequest::Status, |sub| {
                    commit_like(&sub.commits[index].oid)
                }),
            (View::Commits, Some(Row::Commit(index))) => {
                commit_like(&self.snapshot.commits[index].oid)
            }
            (View::Reflog, Some(Row::Reflog(index))) => {
                commit_like(&self.snapshot.reflog[index].oid)
            }
            (View::Stash, Some(Row::Stash(index))) => MainRequest::Stash {
                selector: self.snapshot.stashes[index].selector(),
                path: None,
            },
            (View::CommitFiles, Some(Row::CommitFile(index))) => {
                let Some(files) = &self.commit_files else {
                    return MainRequest::Status;
                };
                let path = Some(files.files[index].path.clone());
                match &files.target {
                    DiffTarget::Stash(selector) => MainRequest::Stash {
                        selector: selector.clone(),
                        path,
                    },
                    _ => MainRequest::Commit {
                        oid: files.revision.clone(),
                        path,
                    },
                }
            }
            (View::Commits | View::Reflog | View::SubCommits, _) => {
                MainRequest::Message("No commits yet".into())
            }
            (View::Stash, _) => MainRequest::Message("No stashes".into()),
            _ => MainRequest::Message(String::new()),
        }
    }

    /// Request the main view for the selection when it changed (or unconditionally).
    pub(crate) fn sync_main(&mut self, force: bool) -> Option<Command<Msg>> {
        if !self.loaded_once {
            return None;
        }
        let request = self.main_request();
        if !force && self.main_requested.as_ref() == Some(&request) {
            return None;
        }
        if self.main_requested.as_ref() != Some(&request) {
            self.main_cursor = [0, 0];
            self.reset_main_scroll();
            self.main_section = 0;
            if self.window != Window::Main {
                self.staging = None;
            }
        }
        self.main_requested = Some(request.clone());
        self.main_generation += 1;
        let generation = self.main_generation;
        let repo = self.repo.clone();
        let cx = self.cx.clone();
        self.activity.begin(crate::activity::Load::Main);
        self.spawn(
            Some("main"),
            move || load_main(&repo, request, &cx),
            move |content| Msg::Main(generation, Box::new(content)),
        )
    }

    pub(crate) fn load_commit_files(
        &mut self,
        target: DiffTarget,
        revision: String,
        title: String,
    ) -> Option<Command<Msg>> {
        self.load_commit_files_in(self.window, target, revision, title)
    }

    /// List the files of a commit or stash in `window`.
    pub(crate) fn load_commit_files_in(
        &mut self,
        window: Window,
        target: DiffTarget,
        revision: String,
        title: String,
    ) -> Option<Command<Msg>> {
        self.commit_files_generation += 1;
        let generation = self.commit_files_generation;
        self.commit_files = Some(CommitFiles {
            target: target.clone(),
            title,
            revision,
            files: Vec::new(),
            counts: BTreeMap::new(),
            window,
            patch: BTreeSet::new(),
        });
        self.cursors.insert(View::CommitFiles, 0);
        let repo = self.repo.clone();
        let counts = self.config.files.line_counts.show != LineCountsShow::Off;
        self.activity.begin(crate::activity::Load::CommitFiles);
        self.spawn(
            None,
            move || crate::data::load_changed_files(&repo, &target, counts),
            move |files| Msg::CommitFiles(generation, files),
        )
    }

    pub(crate) fn load_sub_commits(
        &mut self,
        revision: String,
        title: String,
    ) -> Option<Command<Msg>> {
        let window = if self.window == Window::Main {
            self.last_side
        } else {
            self.window
        };
        self.load_sub_commits_in(window, revision, title)
    }

    /// List the commits of a ref in `window`.
    pub(crate) fn load_sub_commits_in(
        &mut self,
        window: Window,
        revision: String,
        title: String,
    ) -> Option<Command<Msg>> {
        self.sub_commits_generation += 1;
        let generation = self.sub_commits_generation;
        self.sub_commits = Some(SubCommits {
            window,
            title,
            revision: revision.clone(),
            commits: Vec::new(),
            graph: Vec::new(),
        });
        self.cursors.insert(View::SubCommits, 0);
        let repo = self.repo.clone();
        let limit = self.config.log_page_size;
        self.activity.begin(crate::activity::Load::SubCommits);
        self.spawn(
            None,
            move || {
                repo.log(&LogQuery {
                    revision: Some(revision),
                    limit,
                    ..LogQuery::default()
                })
                .map_err(|error| error.message())
            },
            move |commits| Msg::SubCommits(generation, commits),
        )
    }

    pub(crate) fn load_more_commits(&mut self) -> Option<Command<Msg>> {
        let repo = self.repo.clone();
        let skip = self.snapshot.commits.len();
        let limit = self.config.log_page_size;
        self.activity.begin(crate::activity::Load::MoreCommits);
        self.spawn(
            Some("more-commits"),
            move || {
                repo.log(&LogQuery {
                    skip,
                    limit,
                    ..LogQuery::default()
                })
                .map_err(|error| error.message())
            },
            Msg::MoreCommits,
        )
    }

    // ----- operations -----

    /// Queue operations; they run one at a time in order.
    pub(crate) fn run_ops(&mut self, ops: Vec<Op>) -> Update<Msg> {
        self.run_ops_with_output(ops, false)
    }

    pub(crate) fn run_ops_with_output(&mut self, ops: Vec<Op>, show_output: bool) -> Update<Msg> {
        self.queue_ops(ops, show_output, false)
    }

    /// Queue operations; `automatic` ones (auto-fetch) show only in the status bar.
    fn queue_ops(&mut self, ops: Vec<Op>, show_output: bool, automatic: bool) -> Update<Msg> {
        for op in &ops {
            self.activity.queue(op, &self.snapshot, automatic);
        }
        self.queue
            .extend(ops.into_iter().map(|op| (op, show_output)));
        let mut update = Update::redraw();
        if let Some(command) = self.start_next_op() {
            update = update.maybe(command);
        }
        update
    }

    fn start_next_op(&mut self) -> Option<Command<Msg>> {
        if self.activity.running() {
            return None;
        }
        let (op, show_output) = self.queue.pop_front()?;
        let label = op.label();
        let records_before = self
            .repo
            .runner()
            .log()
            .records()
            .last()
            .map(|record| record.id);
        self.activity.start(records_before);
        let repo = self.repo.clone();
        let record = op.moves_head();
        let journal = op.journal();
        let network = op.is_network();
        self.spawn(
            None,
            move || {
                let before = record.then(|| HeadPosition::capture(&repo)).flatten();
                let result = ops::run(&repo, &op);
                let after = record.then(|| HeadPosition::capture(&repo)).flatten();
                OpResult {
                    label,
                    result,
                    before,
                    after,
                    show_output,
                    network,
                    journal,
                }
            },
            |result| Msg::OpDone(Box::new(result)),
        )
    }

    pub(crate) fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toast = Some(Toast {
            text: text.into(),
            error,
            until: Instant::now()
                + if error {
                    Duration::from_secs(8)
                } else {
                    Duration::from_secs(3)
                },
        });
    }

    pub(crate) fn open(&mut self, popup: Popup) -> Update<Msg> {
        self.pending.clear();
        self.popups.push(popup);
        Update::redraw()
    }

    pub(crate) fn prompt(
        &mut self,
        title: impl Into<String>,
        initial: &str,
        kind: PromptKind,
    ) -> Update<Msg> {
        self.open(Popup::Prompt(Prompt::new(title, initial, kind)))
    }

    pub(crate) fn launch(&mut self, external: External) -> Update<Msg> {
        self.external = Some(external);
        Update::exit()
    }

    /// Apply a menu or confirmation effect.
    pub(crate) fn apply(&mut self, then: Then) -> Update<Msg> {
        match then {
            Then::Ops(ops) => self.run_ops(ops),
            Then::Action(action) => self.dispatch(action),
            Then::ActionId(id) => self.dispatch_id(&id),
            Then::Prompt(kind) => {
                let (title, initial) = crate::actions::prompt_defaults(self, &kind);
                self.prompt(title, &initial, kind)
            }
            Then::Confirm {
                title,
                message,
                then,
            } => self.open(Popup::Confirm(crate::popup::Confirm {
                title,
                message,
                then: *then,
            })),
            Then::External(external) => self.launch(external),
            Then::Terminal {
                command,
                close_on_success,
            } => self.run_in_terminal(command, close_on_success),
            Then::Layout(crate::panels::LayoutCommand::Reopen(index)) => {
                self.focus(Window::Side(index));
                Update::redraw().maybe(self.sync_main(false))
            }
            Then::Layout(crate::panels::LayoutCommand::Reset) => self.reset_layout(),
            Then::Nothing => Update::redraw(),
        }
    }

    /// Ask first when `ask` is set.
    pub(crate) fn confirm_if(
        &mut self,
        ask: bool,
        title: &str,
        message: String,
        then: Then,
    ) -> Update<Msg> {
        if ask {
            self.apply(Then::Confirm {
                title: title.to_owned(),
                message,
                then: Box::new(then),
            })
        } else {
            self.apply(then)
        }
    }

    // ----- input -----

    fn scopes(&self) -> Vec<Scope> {
        if self.window == Window::Main {
            if self.staging.is_some() {
                vec![Scope::Staging, Scope::Global]
            } else {
                vec![Scope::Main, Scope::Global]
            }
        } else {
            vec![self.current_view().scope(), Scope::List, Scope::Global]
        }
    }

    fn key(&mut self, stroke: KeyStroke, event: &Event) -> Update<Msg> {
        if !self.popups.is_empty() {
            return self.popup_key(stroke, event);
        }
        if self
            .pending_since
            .is_some_and(|since| since.elapsed() > PENDING_TIMEOUT)
        {
            self.pending.clear();
        }
        self.pending.push(stroke);
        let scopes = self.scopes();
        match self.keymap.resolve(&scopes, &self.pending) {
            Resolution::Action(id) => {
                self.pending.clear();
                self.pending_since = None;
                self.dispatch_id(&id)
            }
            Resolution::Pending => {
                self.pending_since = Some(Instant::now());
                Update::redraw()
            }
            Resolution::Unmatched => {
                let retry = self.pending.len() > 1;
                self.pending.clear();
                self.pending_since = None;
                if retry {
                    return self.key(stroke, event);
                }
                Update::redraw()
            }
        }
    }

    pub(crate) fn dispatch_id(&mut self, id: &str) -> Update<Msg> {
        if let Some(action) = Action::from_id(id) {
            return self.dispatch(action);
        }
        if let Some(index) = id
            .strip_prefix("bcode.git.custom.")
            .and_then(|index| index.parse::<usize>().ok())
        {
            return crate::actions::custom_command(self, index, Vec::new());
        }
        Update::none()
    }

    fn popup_key(&mut self, stroke: KeyStroke, event: &Event) -> Update<Msg> {
        let Some(popup) = self.popups.last() else {
            return Update::none();
        };
        let scope = match popup {
            Popup::Menu(_) | Popup::Text(_) | Popup::Terminal(_) => Scope::Menu,
            Popup::Confirm(_) => Scope::Confirm,
            Popup::Prompt(_) => Scope::Input,
        };
        let typed_filter = matches!(popup, Popup::Menu(menu) if menu.filter.is_some())
            && bcode_keymap::controls::LiteralText::from_stroke(stroke).is_some();
        let action = if typed_filter {
            None
        } else {
            match self.keymap.resolve(&[scope], &[stroke]) {
                Resolution::Action(id) => Action::from_id(&id),
                _ => None,
            }
        };
        match self.popups.pop() {
            Some(Popup::Menu(menu)) => crate::actions::menu_key(self, menu, action, stroke, event),
            Some(Popup::Text(view)) => crate::actions::text_key(self, view, action),
            Some(Popup::Terminal(run)) => self.terminal_key(run, action),
            Some(Popup::Confirm(confirm)) => match action {
                Some(Action::ConfirmYes) => self.apply(confirm.then),
                Some(Action::ConfirmNo) => Update::redraw(),
                _ => {
                    self.popups.push(Popup::Confirm(confirm));
                    Update::none()
                }
            },
            Some(Popup::Prompt(mut prompt)) => match action {
                // Enter adds a line in the description; it submits from the summary.
                Some(Action::InputSubmit) if prompt.body_focused => {
                    prompt_input(&prompt, event);
                    self.popups.push(Popup::Prompt(prompt));
                    Update::redraw()
                }
                Some(Action::InputSubmit | Action::InputSubmitFromAnyField) => {
                    crate::actions::submit_prompt(self, prompt)
                }
                Some(Action::InputSwitchField) if prompt.body.is_some() => {
                    prompt.body_focused = !prompt.body_focused;
                    self.popups.push(Popup::Prompt(prompt));
                    Update::redraw()
                }
                Some(Action::InputClear) if prompt.body.is_some() => {
                    prompt.set_text("");
                    prompt.body_focused = false;
                    self.popups.push(Popup::Prompt(prompt));
                    Update::redraw()
                }
                Some(Action::InputCancel) => {
                    self.save_unsent_commit(&prompt);
                    if matches!(prompt.kind, PromptKind::Filter) {
                        let view = self.current_view();
                        self.filters.remove(&view);
                    }
                    Update::redraw()
                }
                Some(Action::InputToggleAmend) => {
                    let amending = match &mut prompt.kind {
                        PromptKind::Commit { amend, .. } => {
                            *amend = !*amend;
                            Some(*amend)
                        }
                        _ => None,
                    };
                    if amending == Some(true) && prompt.text().trim().is_empty() {
                        let message = self
                            .snapshot
                            .commits
                            .first()
                            .and_then(|commit| self.repo.commit_message(&commit.oid).ok())
                            .unwrap_or_default();
                        prompt.set_text(&message);
                    }
                    self.popups.push(Popup::Prompt(prompt));
                    Update::redraw()
                }
                _ => {
                    let outcome = prompt_input(&prompt, event);
                    self.recall_input(&mut prompt, outcome);
                    let edited = outcome == TextInputOutcome::Edited;
                    self.popups.push(Popup::Prompt(prompt));
                    if edited {
                        return self.apply_live_filter();
                    }
                    Update::redraw()
                }
            },
            None => Update::none(),
        }
    }

    /// The open commit editor (not an amend), wherever it sits in the popup stack.
    fn commit_editor(&mut self) -> Option<&mut Prompt> {
        self.popups.iter_mut().find_map(|popup| match popup {
            Popup::Prompt(prompt) if matches!(prompt.kind, PromptKind::Commit { .. }) => {
                Some(prompt)
            }
            _ => None,
        })
    }

    /// Whether a source change may fill the open commit editor: it is not amending and both
    /// fields are empty, however they got that way.
    fn commit_editor_accepts_prefill(&mut self) -> bool {
        self.commit_editor().is_some_and(|prompt| {
            matches!(prompt.kind, PromptKind::Commit { amend: false, .. })
                && prompt.summary().trim().is_empty()
                && prompt.description().trim().is_empty()
        })
    }

    /// Look for a prefill off the input thread when the open commit editor can take one.
    fn check_commit_prefill(&mut self) -> Option<Command<Msg>> {
        if !self.commit_editor_accepts_prefill() {
            return None;
        }
        let repo = self.repo.clone();
        // The environment message is offered to the first editor only.
        let sources: Vec<CommitMessageSource> = self
            .commit_sources
            .iter()
            .copied()
            .filter(|source| *source != CommitMessageSource::Env)
            .collect();
        self.spawn(
            Some("commit-prefill"),
            move || crate::commit_message::find(&repo, &sources, None),
            |prefill| Msg::CommitPrefill(Box::new(prefill)),
        )
    }

    /// Fill the commit editor with a found prefill, if it is still empty.
    fn apply_commit_prefill(&mut self, prefill: crate::commit_message::Prefill) -> Update<Msg> {
        if !self.commit_editor_accepts_prefill() {
            return Update::none();
        }
        let title = self.commit_title(&prefill);
        let Some(prompt) = self.commit_editor() else {
            return Update::none();
        };
        if let Some((_, text)) = &prefill.chosen {
            prompt.set_text(text);
        }
        if let PromptKind::Commit {
            prefill: current, ..
        } = &mut prompt.kind
        {
            *current = prefill.chosen;
        }
        prompt.title = title;
        Update::redraw()
    }

    /// Up on the first row and down on the last browse the history of prompts that keep one.
    fn recall_input(&mut self, prompt: &mut Prompt, outcome: TextInputOutcome) {
        if !matches!(prompt.kind, PromptKind::Shell) {
            return;
        }
        let recalled = match outcome {
            TextInputOutcome::EdgeUp => self.shell_history.older(&prompt.text()).map(str::to_owned),
            TextInputOutcome::EdgeDown => self.shell_history.newer(),
            _ => None,
        };
        if let Some(text) = recalled {
            prompt.set_text(&text);
        }
    }

    /// Apply the text of an open filter prompt to the focused view.
    fn apply_live_filter(&mut self) -> Update<Msg> {
        let Some(Popup::Prompt(prompt)) = self.popups.last() else {
            return Update::redraw();
        };
        if !matches!(prompt.kind, PromptKind::Filter) {
            return Update::redraw();
        }
        let text = prompt.text();
        let view = self.current_view();
        self.filters.insert(view, text);
        self.set_cursor(view, 0);
        Update::redraw().maybe(self.sync_main(false))
    }

    fn mouse(&mut self, event: &Event) -> Update<Msg> {
        let Event::Mouse(mouse) = event else {
            return Update::none();
        };
        // Scrollbars first: a drag keeps the pointer wherever it goes.
        if let Some(update) = self.route_scrollbars(event) {
            return update;
        }
        let wheel = match mouse.kind {
            MouseEventKind::ScrollUp => Some(-crate::scroll::WHEEL_ROWS),
            MouseEventKind::ScrollDown => Some(crate::scroll::WHEEL_ROWS),
            _ => None,
        };
        // With a popup open, only the scroll wheel does anything: over the popup it scrolls the
        // popup, elsewhere it reaches what is behind.
        if self.popups.is_empty() {
            if let Some(update) = self.route_dividers(event) {
                return update;
            }
        } else {
            let Some(delta) = wheel else {
                return Update::none();
            };
            let over_popup = self
                .popup_area
                .get()
                .is_some_and(|area| area.contains(Point::new(mouse.position.x, mouse.position.y)));
            if over_popup {
                let list = match self.popups.last() {
                    Some(Popup::Text(_)) => crate::scroll::TEXT_LIST,
                    Some(Popup::Terminal(_)) => crate::scroll::TERMINAL_LIST,
                    _ => crate::scroll::MENU_LIST,
                };
                return if self.scroll_list(list, delta) {
                    Update::redraw()
                } else {
                    Update::none()
                };
            }
        }
        let id = {
            let hits = self.hits.borrow();
            hits.hit_test(Point::new(mouse.position.x, mouse.position.y))
                .map(|hit| hit.region.id.as_str().to_owned())
        };
        let Some(id) = id else {
            return Update::none();
        };
        // The wheel scrolls the list under the pointer like a page: no cursor or focus moves.
        if let Some(delta) = wheel {
            return self.wheel(&id, delta);
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some(update) = self.chrome_click(&id, mouse.position)
        {
            return update;
        }
        let (window, item) = parse_hit(&id);
        let Some(window) = window else {
            return Update::none();
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if window == Window::Main {
                    // Focus the page; while staging, the clicked line takes the cursor.
                    self.window = Window::Main;
                    if let Some((section, row)) = item {
                        self.main_section = section.min(1);
                        if self.staging.is_some() {
                            self.main_cursor[section.min(1)] = row;
                        }
                    }
                    return Update::redraw();
                }
                let already = self.window == window;
                self.focus(window);
                let view = self.view(window);
                if let Some((_, index)) = item {
                    let reclick = already && self.cursor(view) == index;
                    self.set_cursor(view, index);
                    if reclick {
                        return self.dispatch(crate::actions::primary_action(view));
                    }
                }
                let mut update = Update::redraw();
                if let Some(command) = self.sync_main(false) {
                    update = update.maybe(command);
                }
                update
            }
            _ => Update::none(),
        }
    }

    /// Bring the panel layout in line with windows, focus, screen mode and command log.
    pub(crate) fn sync_panels(&mut self) {
        let focused = match (self.window, self.last_side) {
            (Window::Side(index), _) | (Window::Main, Window::Side(index)) => Some(index),
            _ => None,
        };
        let view = PanelView {
            config: &self.config.layout,
            windows: &self.layout.windows,
            focused,
            screen_mode: self.screen_mode,
            command_log: self.command_log,
        };
        self.panels.sync(&view);
    }

    /// Save the panel layout for the launch directory.
    pub(crate) fn save_layout(&mut self) {
        self.persist();
    }

    /// Save the session (layout included) if it changed. Skipped while a divider is being
    /// dragged; the drag's end is saved.
    pub(crate) fn persist(&mut self) {
        let dragging = crate::panels::GROUPS
            .into_iter()
            .any(|id| self.panel_group(id).1.borrow().active_divider().is_some());
        if dragging {
            return;
        }
        let session = self.capture_session();
        self.panels.save(&self.layout.windows, &session);
    }

    /// Apply the session saved for this directory, then treat it as saved.
    pub(crate) fn resume_session(&mut self) {
        if let Some(session) = self.panels.restored.take() {
            self.restore_session(session);
        }
        self.sync_panels();
        let session = self.capture_session();
        self.panels.mark_saved(&self.layout.windows, &session);
    }

    fn window_named(&self, name: &str) -> Option<Window> {
        self.layout
            .windows
            .iter()
            .position(|tabs| window_key(tabs) == name)
            .map(Window::Side)
    }

    /// Main view position and staging, to reopen with.
    fn saved_main(&self) -> SavedMain {
        SavedMain {
            section: self.main_section,
            cursor: self.main_cursor,
            scroll: [0, 1].map(|section| self.main_lists[section].get().scroll.vertical_offset()),
            staging: self.staging.map(|staging| SavedStaging {
                anchor: staging.anchor,
                hunk_mode: staging.hunk_mode,
            }),
        }
    }

    /// The state to reopen with.
    #[must_use]
    pub fn capture_session(&self) -> Session {
        let name = |window: Window| match window {
            Window::Side(index) => self.layout.windows.get(index).map(|tabs| window_key(tabs)),
            Window::Main => None,
        };
        let journal = |entries: &[Journal]| {
            entries
                .iter()
                .map(|entry| SavedJournal {
                    label: entry.label.clone(),
                    before: (entry.before.branch.clone(), entry.before.oid.clone()),
                    after: (entry.after.branch.clone(), entry.after.oid.clone()),
                })
                .collect()
        };
        // Restored views not yet reloaded are kept as they were saved.
        let (pending_sub, pending_files) = self.pending_drill.clone().unwrap_or_default();
        Session {
            screen_mode: Some(self.screen_mode),
            side: name(self.last_side),
            main_focused: self.window == Window::Main,
            tabs: self
                .tabs
                .iter()
                .filter_map(|(window, index)| {
                    let view = self.tabs(*window).get(*index)?;
                    Some((name(*window)?, view_id(*view).to_owned()))
                })
                .collect(),
            selected: self
                .keys
                .iter()
                .map(|(view, key)| (view_id(*view).to_owned(), key.clone()))
                .collect(),
            cursors: self
                .cursors
                .iter()
                .map(|(view, index)| (view_id(*view).to_owned(), *index))
                .collect(),
            scroll: self
                .lists
                .iter()
                .map(|(view, list)| {
                    (
                        view_id(*view).to_owned(),
                        list.get().scroll.vertical_offset(),
                    )
                })
                .filter(|(_, offset)| *offset > 0)
                .collect(),
            filters: self
                .filters
                .iter()
                .filter(|(_, text)| !text.is_empty())
                .map(|(view, text)| (view_id(*view).to_owned(), text.clone()))
                .collect(),
            file_tree: Some(self.file_tree),
            collapsed_dirs: self.collapsed.iter().cloned().collect(),
            command_log: Some(self.command_log),
            diff_context: Some(self.cx.context),
            show_graph: Some(self.cx.show_graph),
            diff_base: self.diff_base.clone(),
            remote: self.remote.clone(),
            sub_commits: pending_sub.or_else(|| {
                let sub = self.sub_commits.as_ref()?;
                Some(SavedSubCommits {
                    window: name(sub.window)?,
                    revision: sub.revision.clone(),
                    title: sub.title.clone(),
                })
            }),
            commit_files: pending_files.or_else(|| {
                let files = self.commit_files.as_ref()?;
                let target = match &files.target {
                    DiffTarget::Commit(oid) => SavedTarget::Commit(oid.clone()),
                    DiffTarget::Stash(stash) => SavedTarget::Stash(stash.clone()),
                    DiffTarget::Range { from, to } => SavedTarget::Range {
                        from: from.clone(),
                        to: to.clone(),
                    },
                    _ => return None,
                };
                Some(SavedCommitFiles {
                    window: name(files.window)?,
                    target,
                    revision: files.revision.clone(),
                    title: files.title.clone(),
                })
            }),
            main: Some(self.pending_main.unwrap_or_else(|| self.saved_main())),
            cherry: self.cherry.clone(),
            undo: journal(&self.undo),
            redo: journal(&self.redo),
            shell_history: self.shell_history.entries().to_vec(),
        }
    }

    /// Reopen as the session was left. Lists, the main view and drilled-in views settle as
    /// their data arrives; anything that no longer exists is skipped.
    pub fn restore_session(&mut self, session: Session) {
        let view = |id: &String| view_from_id(id);
        for (name, id) in &session.tabs {
            if let (Some(window), Some(view)) = (self.window_named(name), view(id))
                && let Some(position) = self.tabs(window).iter().position(|tab| *tab == view)
            {
                self.tabs.insert(window, position);
            }
        }
        if let Some(window) = session
            .side
            .as_deref()
            .and_then(|name| self.window_named(name))
        {
            self.focus(window);
        }
        if session.main_focused {
            self.window = Window::Main;
        }
        if let Some(mode) = session.screen_mode {
            self.screen_mode = mode;
        }
        self.keys = session
            .selected
            .iter()
            .filter_map(|(id, key)| Some((view(id)?, key.clone())))
            .collect();
        self.cursors = session
            .cursors
            .iter()
            .filter_map(|(id, index)| Some((view(id)?, *index)))
            .collect();
        for (id, offset) in &session.scroll {
            if let Some(list) = view(id).and_then(|view| self.lists.get(&view)) {
                let mut state = list.get();
                state.scroll.set_vertical_offset(*offset);
                list.set(state);
            }
        }
        self.filters = session
            .filters
            .iter()
            .filter_map(|(id, text)| Some((view(id)?, text.clone())))
            .collect();
        if let Some(tree) = session.file_tree {
            self.file_tree = tree;
        }
        self.collapsed = session.collapsed_dirs.into_iter().collect();
        if let Some(shown) = session.command_log {
            self.command_log = shown;
        }
        if let Some(context) = session.diff_context {
            self.cx.context = context;
        }
        if let Some(graph) = session.show_graph {
            self.cx.show_graph = graph;
        }
        self.diff_base = session.diff_base;
        self.remote = session.remote;
        if session.sub_commits.is_some() || session.commit_files.is_some() {
            self.pending_drill = Some((session.sub_commits, session.commit_files));
        }
        self.pending_main = session.main;
        self.cherry = session.cherry;
        let journal = |entries: Vec<SavedJournal>| {
            entries
                .into_iter()
                .map(|entry| Journal {
                    label: entry.label,
                    before: HeadPosition {
                        branch: entry.before.0,
                        oid: entry.before.1,
                    },
                    after: HeadPosition {
                        branch: entry.after.0,
                        oid: entry.after.1,
                    },
                })
                .collect()
        };
        self.undo = journal(session.undo);
        self.redo = journal(session.redo);
        self.shell_history = crate::popup::InputRecall::from_entries(session.shell_history);
    }

    /// Reload drilled-in views of a restored session once repository data is in.
    fn resume_drilled(&mut self) -> Vec<Command<Msg>> {
        let Some((sub, files)) = self.pending_drill.take() else {
            return Vec::new();
        };
        let mut commands = Vec::new();
        if let Some(sub) = sub
            && let Some(window) = self.window_named(&sub.window)
        {
            commands.extend(self.load_sub_commits_in(window, sub.revision, sub.title));
        }
        if let Some(files) = files
            && let Some(window) = self.window_named(&files.window)
        {
            let target = match files.target {
                SavedTarget::Commit(oid) => DiffTarget::Commit(oid),
                SavedTarget::Stash(stash) => DiffTarget::Stash(stash),
                SavedTarget::Range { from, to } => DiffTarget::Range { from, to },
            };
            commands.extend(self.load_commit_files_in(window, target, files.revision, files.title));
        }
        commands
    }

    /// The group and state behind a panel group id.
    pub(crate) fn panel_group(
        &self,
        id: &str,
    ) -> (
        bmux_tui_components::panel_group::PanelGroup,
        &std::cell::RefCell<bmux_tui_components::panel_group::PanelGroupState>,
    ) {
        use bmux_tui_components::panel_group::PanelGroupAxis;
        let (axis, state) = match id {
            crate::panels::ROOT => (crate::view::root_axis(self), &self.panels.root),
            crate::panels::MAIN => (PanelGroupAxis::Vertical, &self.panels.main),
            crate::panels::SECTIONS => (crate::view::sections_axis(self), &self.panels.sections),
            _ => (PanelGroupAxis::Vertical, &self.panels.side),
        };
        (crate::panels::group(axis, &self.theme), state)
    }

    /// Offer pointer input to the panel dividers: an ongoing drag first, then each group from
    /// the innermost out. Returns the update when a divider took the event.
    fn route_dividers(&mut self, event: &Event) -> Option<Update<Msg>> {
        use bmux_tui_components::panel_group::PanelGroupOutcome;
        let areas = self.panels.areas.borrow().clone();
        let dragging = crate::panels::GROUPS
            .into_iter()
            .find(|id| self.panel_group(id).1.borrow().active_divider().is_some());
        let candidates: Vec<&'static str> =
            dragging.map_or_else(|| crate::panels::GROUPS.to_vec(), |id| vec![id]);
        let mut redraw = false;
        for id in candidates {
            let Some(area) = areas.get(id).copied() else {
                continue;
            };
            let (group, state) = self.panel_group(id);
            let outcome = group.handle_event(area, &mut state.borrow_mut(), event);
            match outcome {
                PanelGroupOutcome::Ignored => {}
                PanelGroupOutcome::Resized { .. } => {
                    self.panels.resized(id);
                    return Some(Update::redraw());
                }
                PanelGroupOutcome::DividerDragEnded { .. } => {
                    self.save_layout();
                    return Some(Update::redraw());
                }
                PanelGroupOutcome::DividerDragStarted { .. } => return Some(Update::redraw()),
                // Hover changes: every group updates its own hover state.
                PanelGroupOutcome::Redraw
                | PanelGroupOutcome::Handled
                | PanelGroupOutcome::Focused { .. } => redraw = true,
            }
        }
        redraw.then(Update::redraw)
    }

    /// Clicks on pane titles and their buttons. A double click on a title maximizes the pane.
    fn chrome_click(&mut self, id: &str, position: Point) -> Option<Update<Msg>> {
        // Running work in the status bar: show the command log, with its live output.
        if id == crate::view::STATUS_ACTIVITY {
            self.command_log = true;
            self.sync_panels();
            return Some(Update::redraw());
        }
        if let Some(rest) = id.strip_prefix("tabs-") {
            let (number, tab) = rest
                .split_once('.')
                .map_or((rest, None), |(n, t)| (n, Some(t)));
            let index = number.parse::<usize>().ok()?.checked_sub(1)?;
            return Some(self.select_tab(index, tab));
        }
        let (pane, part) = id.split_once('.')?;
        let window = if pane == "pane-main" || pane.starts_with("pane-main-") {
            if let Some(section) = pane.strip_prefix("pane-main-").and_then(|n| n.parse().ok()) {
                self.main_section = section;
            }
            Window::Main
        } else if pane == "pane-log" {
            return (part == "action.close").then(|| {
                self.command_log = false;
                Update::redraw()
            });
        } else {
            let number: usize = pane.strip_prefix("pane-")?.parse().ok()?;
            Window::Side(number.checked_sub(1)?)
        };
        match part {
            "title" => {
                let clicks = self.clicks.register(
                    position.x,
                    position.y,
                    Instant::now(),
                    &ClickPolicy::with_max_count(2),
                );
                self.focus(window);
                if clicks >= 2 {
                    self.clicks.reset();
                    return Some(self.toggle_maximized(window));
                }
                Some(Update::redraw().maybe(self.sync_main(false)))
            }
            "action.maximize" => Some(self.toggle_maximized(window)),
            "action.collapse" => Some(self.toggle_collapsed(window)),
            "action.close" => Some(self.close_window(window)),
            _ => None,
        }
    }

    /// Focus a side window and, when `tab` names one of its views, switch to that tab. A
    /// collapsed window is expanded so the chosen tab shows.
    pub(crate) fn select_tab(&mut self, index: usize, tab: Option<&str>) -> Update<Msg> {
        let window = Window::Side(index);
        self.focus(window);
        if let Some(position) = tab.and_then(|name| {
            self.tabs(window)
                .iter()
                .position(|view| crate::layout::view_name(*view) == name)
        }) {
            if self.tabs.get(&window).copied().unwrap_or(0) != position {
                self.remote = None;
                if self
                    .sub_commits
                    .as_ref()
                    .is_some_and(|sub| sub.window == window)
                {
                    self.sub_commits = None;
                }
                if self
                    .commit_files
                    .as_ref()
                    .is_some_and(|files| files.window == window)
                {
                    self.commit_files = None;
                }
                self.tabs.insert(window, position);
            }
            if self.panels.collapsed.remove(&index) {
                self.sync_panels();
                self.save_layout();
            }
        }
        Update::redraw().maybe(self.sync_main(false))
    }

    /// Fill the screen with a window, or return to the configured screen mode.
    pub(crate) fn toggle_maximized(&mut self, window: Window) -> Update<Msg> {
        self.focus(window);
        self.screen_mode = if self.screen_mode == ScreenMode::Full {
            self.config.layout.screen_mode
        } else {
            ScreenMode::Full
        };
        Update::redraw()
    }

    /// Collapse a side window to its title, or expand it again.
    pub(crate) fn toggle_collapsed(&mut self, window: Window) -> Update<Msg> {
        let Window::Side(index) = window else {
            return Update::none();
        };
        // In half mode, unfocused windows are collapsed by the mode: show the window instead.
        if self.screen_mode == ScreenMode::Half && !self.panels.collapsed.contains(&index) {
            self.focus(window);
            if self.window == window && self.panels.is_collapsed(index) {
                return Update::redraw();
            }
        }
        if !self.panels.collapsed.remove(&index) {
            self.panels.collapsed.insert(index);
        }
        self.sync_panels();
        self.save_layout();
        Update::redraw()
    }

    /// Close a side window until it is reopened from the layout menu or focused by number.
    pub(crate) fn close_window(&mut self, window: Window) -> Update<Msg> {
        let Window::Side(index) = window else {
            return Update::none();
        };
        let open = self.layout.windows.len() - self.panels.closed.len();
        if open <= 1 {
            self.toast("The last side window cannot be closed", true);
            return Update::redraw();
        }
        self.panels.closed.insert(index);
        if self.last_side == window
            && let Some(next) =
                (0..self.layout.windows.len()).find(|other| !self.panels.closed.contains(other))
        {
            self.focus(Window::Side(next));
        }
        self.sync_panels();
        self.save_layout();
        Update::redraw().maybe(self.sync_main(false))
    }

    /// Move a divider of a panel group by `delta` cells and remember the result.
    pub(crate) fn resize_panel(
        &mut self,
        id: &'static str,
        divider: usize,
        delta: i32,
    ) -> Update<Msg> {
        let Some(area) = self.panels.areas.borrow().get(id).copied() else {
            return Update::none();
        };
        let (group, state) = self.panel_group(id);
        let outcome = group.resize_divider(area, &mut state.borrow_mut(), divider, delta);
        if matches!(
            outcome,
            bmux_tui_components::panel_group::PanelGroupOutcome::Resized { .. }
        ) {
            self.panels.resized(id);
            self.save_layout();
        }
        Update::redraw()
    }

    /// Restore the configured layout and forget the remembered one.
    pub(crate) fn reset_layout(&mut self) -> Update<Msg> {
        self.panels.reset(&self.config.layout, &self.layout.windows);
        self.screen_mode = self.config.layout.screen_mode;
        self.sync_panels();
        self.toast("Layout reset", false);
        Update::redraw()
    }

    /// Show a closed side window again.
    pub(crate) fn reopen_window(&mut self, index: usize) {
        if self.panels.closed.remove(&index) {
            self.sync_panels();
            self.save_layout();
        }
    }

    pub(crate) fn focus(&mut self, window: Window) {
        if let Window::Side(index) = window {
            self.reopen_window(index);
        }
        if window != Window::Main {
            self.last_side = window;
            self.staging = None;
        }
        self.window = window;
    }

    pub(crate) fn move_cursor(&mut self, view: View, delta: isize) -> Update<Msg> {
        let len = self.rows(view).len();
        if len == 0 {
            return Update::redraw();
        }
        let current = self.cursor(view);
        let target = current.saturating_add_signed(delta).min(len - 1);
        self.set_cursor(view, target);
        let mut update = Update::redraw();
        if view == View::Commits && target + 1 == len && self.snapshot.more_commits {
            update = update.maybe(self.load_more_commits());
        }
        if let Some(command) = self.sync_main(false) {
            update = update.maybe(command);
        }
        update
    }

    /// Show the top of the main view's sections, as for content not seen before.
    pub(crate) fn reset_main_scroll(&self) {
        for list in &self.main_lists {
            let mut state = list.get();
            state.scroll.set_vertical_offset(0);
            list.set(state);
        }
    }

    pub(crate) fn main_rows(&self, section: usize) -> usize {
        self.main
            .as_ref()
            .and_then(|main| main.sections.get(section))
            .map_or(0, |section| section.rows.len())
    }

    /// Scroll the list under a hit region by `delta` rows. Scrolling the commits list to its end
    /// loads more commits, as moving the cursor there does.
    fn wheel(&mut self, hit: &str, delta: i64) -> Update<Msg> {
        let Some(list) = crate::scroll::list_under(hit) else {
            return Update::none();
        };
        if !self.scroll_list(&list, delta) {
            return Update::none();
        }
        let mut update = Update::redraw();
        if delta > 0
            && self.snapshot.more_commits
            && self.list_at_end(&list)
            && self.side_windows().any(|window| {
                crate::scroll::side_list(window) == list && self.view(window) == View::Commits
            })
        {
            update = update.maybe(self.load_more_commits());
        }
        update
    }

    pub(crate) fn move_main(&mut self, delta: isize) {
        let section = self.main_section.min(1);
        let len = self.main_rows(section);
        if len == 0 {
            return;
        }
        self.main_cursor[section] = self.main_cursor[section]
            .saturating_add_signed(delta)
            .min(len - 1);
    }

    /// Kind of the main section with keyboard focus.
    #[must_use]
    pub fn main_section_kind(&self) -> SectionKind {
        self.main
            .as_ref()
            .and_then(|main| main.sections.get(self.main_section))
            .map_or(SectionKind::Plain, |section| section.kind)
    }

    /// Diff line identities selected while staging.
    #[must_use]
    pub fn staging_selection(&self) -> BTreeSet<DiffLineId> {
        let Some(staging) = self.staging else {
            return BTreeSet::new();
        };
        let Some(section) = self
            .main
            .as_ref()
            .and_then(|main| main.sections.get(self.main_section))
        else {
            return BTreeSet::new();
        };
        let cursor = self.main_cursor[self.main_section.min(1)];
        let Some(row) = section.rows.get(cursor) else {
            return BTreeSet::new();
        };
        let rows: Vec<&crate::diffview::DiffRow> = if let Some((start, end)) = staging
            .anchor
            .map(|anchor| (anchor.min(cursor), anchor.max(cursor)))
        {
            section.rows[start..=end.min(section.rows.len() - 1)]
                .iter()
                .collect()
        } else if staging.hunk_mode || row.kind == RowKind::HunkHeader {
            section
                .rows
                .iter()
                .filter(|candidate| candidate.hunk == row.hunk && candidate.file == row.file)
                .collect()
        } else {
            vec![row]
        };
        rows.into_iter()
            .filter(|row| row.kind.is_change())
            .filter_map(|row| row.id)
            .collect()
    }

    fn tick(&mut self) -> Update<Msg> {
        self.persist();
        let toast_expired = self
            .toast
            .as_ref()
            .is_some_and(|toast| Instant::now() > toast.until);
        if toast_expired {
            self.toast = None;
        }
        let pending_expired = self
            .pending_since
            .is_some_and(|since| since.elapsed() > PENDING_TIMEOUT);
        if pending_expired {
            self.pending.clear();
            self.pending_since = None;
        }
        let flashes_expired = self
            .flashes
            .expire(Instant::now(), self.config.flash.duration());
        let mut update =
            if self.activity.expire() | toast_expired | pending_expired | flashes_expired {
                Update::redraw()
            } else {
                Update::none()
            };
        update = update.maybe(self.request_pull_requests(false));
        let poll = if self.watching {
            0
        } else {
            self.config.refresh.poll_seconds
        };
        if poll > 0
            && self.last_poll.elapsed() >= Duration::from_secs(poll)
            && !self.activity.busy()
        {
            self.last_poll = Instant::now();
            update = update
                .maybe(self.refresh_all())
                .maybe(self.check_commit_prefill());
        }
        let fetch = self.config.refresh.auto_fetch_seconds;
        if fetch > 0
            && !self.activity.busy()
            && self.queue.is_empty()
            && self.last_fetch.elapsed() >= Duration::from_secs(fetch)
            && !self.snapshot.remotes.is_empty()
        {
            // Any fetch of the repository counts (another client's, a terminal's), so its
            // clients together fetch once per interval.
            match self.fetched_ago() {
                Some(ago) if ago < Duration::from_secs(fetch) => {
                    self.last_fetch = Instant::now().checked_sub(ago).unwrap_or_else(Instant::now);
                }
                _ => {
                    self.last_fetch = Instant::now();
                    update = update.merge(self.queue_ops(vec![Op::Fetch(None)], false, true));
                }
            }
        }
        update
    }

    /// How long ago the repository was last fetched (by anything), from `FETCH_HEAD`.
    fn fetched_ago(&self) -> Option<Duration> {
        self.repo
            .paths()
            .common_dir
            .join("FETCH_HEAD")
            .metadata()
            .ok()?
            .modified()
            .ok()?
            .elapsed()
            .ok()
    }

    /// Whether line counts are needed beyond showing them on files.
    const fn counts_needed(&self) -> bool {
        self.config.flash.files()
            || self.config.flash.branches()
            || self.config.branches.line_counts
    }

    /// Note which branches and worktrees are listed, flashing new ones.
    fn observe_listed(&mut self) {
        let now = Instant::now();
        let snapshot = &self.snapshot;
        self.flashes.listed(
            snapshot
                .branches
                .iter()
                .map(|branch| branch.name.clone())
                .collect(),
            snapshot
                .worktrees
                .iter()
                .filter(|worktree| !worktree.bare)
                .map(|worktree| worktree.path.clone())
                .collect(),
            &self.config.flash,
            now,
        );
        let root = self.repo.root();
        self.worktree_totals.retain(|path, _| {
            path == root
                || snapshot
                    .worktrees
                    .iter()
                    .any(|worktree| worktree.path == *path)
        });
    }

    /// Compare this worktree's files and total, measured at `head`, with the last ones,
    /// flashing what changed.
    fn observe_files(&mut self, head: Option<&str>) {
        let Some(counts) = self.snapshot.line_counts.combined() else {
            return;
        };
        let now = Instant::now();
        let config = self.config.flash;
        let files: BTreeMap<String, LineStats> = self
            .snapshot
            .status
            .entries
            .iter()
            .map(|entry| {
                let stats = counts.get(&entry.path).copied().unwrap_or_default();
                (entry.path.clone(), stats)
            })
            .collect();
        let total: LineStats = files.values().copied().sum();
        let branch = self.snapshot.status.branch.head.clone();
        let root = self.repo.root().to_owned();
        self.flashes.files(head, files, &config, now);
        self.flashes
            .worktree(&root, head, total, branch.as_deref(), &config, now);
        self.worktree_totals.insert(root, total);
    }

    /// Redraw while something animates, ending finished flashes.
    fn frame(&mut self) -> Update<Msg> {
        let expired = self
            .flashes
            .expire(Instant::now(), self.config.flash.duration());
        if expired || self.flashes.active() || self.activity.animating() {
            Update::redraw()
        } else {
            Update::none()
        }
    }

    fn install_live(&mut self, live: bcode_git_live::LiveUpdate) -> Update<Msg> {
        if let Some(error) = live.errors.first() {
            self.toast(error.clone(), true);
        }
        let mut update = Update::redraw();
        if let Some(primary) = live.primary {
            self.snapshot.status = primary.status;
            self.snapshot.line_counts = primary.counts.into();
            self.file_nodes = file_rows(&self.snapshot.status, self.file_tree, &self.collapsed);
            self.observe_files(primary.head.as_deref());
            self.restore_cursors();
            update = update.maybe(self.sync_main(true));
        }
        for root in live.removed {
            self.worktree_totals.remove(&root);
        }
        let now = Instant::now();
        for (path, (head, measured)) in live.totals {
            let branch = self
                .snapshot
                .worktrees
                .iter()
                .find(|worktree| worktree.path == path)
                .and_then(|worktree| worktree.branch.clone());
            self.flashes.worktree(
                &path,
                head.as_deref(),
                measured.total,
                branch.as_deref(),
                &self.config.flash,
                now,
            );
            self.worktree_totals.insert(path, measured.total);
        }
        if live.metadata {
            update = update.maybe(self.reload_metadata());
        }
        if live.files || live.metadata {
            update = update.maybe(self.check_commit_prefill());
        }
        update
    }

    fn install_snapshot(&mut self, snapshot: Snapshot) {
        self.loaded_once = true;
        self.commit_graph = graph::layout(&snapshot.commits);
        if let Some(error) = snapshot.errors.first() {
            self.toast(error.clone(), true);
        }
        // Status and line counts stay: they come from live state.
        let status = std::mem::take(&mut self.snapshot.status);
        let line_counts = std::mem::take(&mut self.snapshot.line_counts);
        self.snapshot = snapshot;
        self.snapshot.status = status;
        self.snapshot.line_counts = line_counts;
        self.observe_listed();
        self.pulls
            .set_remote(&self.snapshot.remotes, &self.config.pull_requests.remote);
        self.refresh_branch_colors();
        if self.remote.as_ref().is_some_and(|remote| {
            !self
                .snapshot
                .remotes
                .iter()
                .any(|candidate| &candidate.name == remote)
        }) {
            self.remote = None;
        }
        self.restore_cursors();
    }

    pub(crate) fn rebuild_files(&mut self) {
        self.file_nodes = file_rows(&self.snapshot.status, self.file_tree, &self.collapsed);
        self.restore_cursors();
    }

    fn install_main(&mut self, content: MainContent) {
        let preserve = self.main.as_ref().map(|main| &main.request) == Some(&content.request);
        self.main = Some(content);
        if !preserve {
            self.main_cursor = [0, 0];
            self.reset_main_scroll();
            self.main_section = 0;
        }
        let sections = self.main.as_ref().map_or(0, |main| main.sections.len());
        if sections == 0 {
            self.main_section = 0;
        } else {
            self.main_section = self.main_section.min(sections - 1);
        }
        for section in 0..sections.min(2) {
            let len = self.main_rows(section);
            self.main_cursor[section] = self.main_cursor[section].min(len.saturating_sub(1));
        }
        if self.staging.is_some() {
            if self.main_section_kind() == SectionKind::Plain || sections == 0 {
                self.staging = None;
                if self.window == Window::Main {
                    self.window = self.last_side;
                }
            } else {
                self.snap_to_change(1);
            }
        }
        // A restored session's position applies to the first content shown for its data.
        if self.loaded_once
            && let Some(saved) = self.pending_main.take()
        {
            if sections > 0 {
                self.main_section = saved.section.min(sections - 1);
                for section in 0..sections.min(2) {
                    let len = self.main_rows(section);
                    self.main_cursor[section] = saved.cursor[section].min(len.saturating_sub(1));
                    let mut state = self.main_lists[section].get();
                    state.scroll.set_vertical_offset(saved.scroll[section]);
                    self.main_lists[section].set(state);
                }
            }
            match saved.staging {
                Some(staging) if self.main_section_kind() != SectionKind::Plain => {
                    self.staging = Some(Staging {
                        anchor: staging.anchor,
                        hunk_mode: staging.hunk_mode,
                    });
                }
                _ => {
                    if self.window == Window::Main
                        && self.main_section_kind() == SectionKind::Plain
                        && saved.staging.is_some()
                    {
                        self.window = self.last_side;
                    }
                }
            }
        }
    }

    /// Move the main cursor to the nearest change row in `direction` (or the other way).
    pub(crate) fn snap_to_change(&mut self, direction: isize) {
        let section = self.main_section.min(1);
        let Some(rows) = self
            .main
            .as_ref()
            .and_then(|main| main.sections.get(section))
            .map(|section| &section.rows)
        else {
            return;
        };
        let start = self.main_cursor[section].min(rows.len().saturating_sub(1));
        if rows.get(start).is_some_and(|row| row.kind.is_change()) {
            return;
        }
        let forward = (start..rows.len()).find(|index| rows[*index].kind.is_change());
        let backward = (0..start).rev().find(|index| rows[*index].kind.is_change());
        let found = if direction >= 0 {
            forward.or(backward)
        } else {
            backward.or(forward)
        };
        if let Some(index) = found {
            self.main_cursor[section] = index;
        }
    }

    fn finish_op(&mut self, result: &OpResult) -> Update<Msg> {
        self.activity.finish(result.result.is_ok());
        // Fetches, pulls and pushes may have changed pull requests.
        let pulls = (result.network && result.result.is_ok())
            .then(|| self.request_pull_requests(true))
            .flatten();
        match &result.result {
            Ok(output) => {
                if let Some(journal) = result.journal {
                    let (from, to) = match journal {
                        ops::JournalMove::Undo => (&mut self.undo, &mut self.redo),
                        ops::JournalMove::Redo => (&mut self.redo, &mut self.undo),
                    };
                    if let Some(entry) = from.pop() {
                        to.push(entry);
                    }
                } else if let (Some(before), Some(after)) = (&result.before, &result.after)
                    && before != after
                {
                    self.undo.push(Journal {
                        label: result.label.clone(),
                        before: before.clone(),
                        after: after.clone(),
                    });
                    self.redo.clear();
                }
                match output {
                    Some(text) if result.show_output || text.lines().count() > 1 => {
                        let lines = text
                            .lines()
                            .map(|line| bmux_tui::text::Line::raw(line.to_owned()))
                            .collect();
                        self.popups.push(Popup::Text(crate::popup::TextView::new(
                            result.label.clone(),
                            lines,
                        )));
                    }
                    Some(text) if !text.is_empty() => self.toast(text.clone(), false),
                    _ => self.toast(result.label.clone(), false),
                }
            }
            Err(error) => {
                self.queue.clear();
                self.activity.clear_queued();
                let conflict = self.repo.operation() != RepoOperation::None;
                if conflict {
                    self.toast(
                        format!(
                            "{} stopped: resolve conflicts, then press m to continue",
                            result.label
                        ),
                        true,
                    );
                    self.focus_view(View::Files);
                } else {
                    self.toast(error.clone(), true);
                }
                if error.lines().count() > 2 {
                    let lines = error
                        .lines()
                        .map(|line| bmux_tui::text::Line::raw(line.to_owned()))
                        .collect();
                    self.popups
                        .push(Popup::Text(crate::popup::TextView::new("Error", lines)));
                }
            }
        }
        let mut update = Update::redraw().maybe(self.refresh()).maybe(pulls);
        if let Some(command) = self.start_next_op() {
            update = update.maybe(command);
        }
        update
    }
}

/// Attach optional commands to an update.
pub(crate) trait UpdateExt {
    /// Attach a command when present.
    #[must_use]
    fn maybe(self, command: impl Into<Option<Command<Msg>>>) -> Self;
}

impl UpdateExt for Update<Msg> {
    fn maybe(self, command: impl Into<Option<Command<Msg>>>) -> Self {
        match command.into() {
            Some(command) => self.with_command(command),
            None => self,
        }
    }
}

/// Split a hit-region id into its window and optional `(section, row)`.
fn parse_hit(id: &str) -> (Option<Window>, Option<(usize, usize)>) {
    let mut parts = id.split('.');
    let window = match parts.next() {
        Some(main) if main.starts_with("main-") => {
            let section = main.trim_start_matches("main-").parse().unwrap_or(0);
            let row = parts.find_map(|part| part.parse::<usize>().ok());
            return (Some(Window::Main), row.map(|row| (section, row)));
        }
        Some(side) if side.starts_with("side-") => {
            match side.trim_start_matches("side-").parse::<usize>() {
                Ok(number) if number > 0 => Window::Side(number - 1),
                _ => return (None, None),
            }
        }
        _ => return (None, None),
    };
    let item = parts.find_map(|part| part.parse::<usize>().ok());
    (Some(window), item.map(|item| (0, item)))
}

impl App {
    /// End the load a current result completes.
    fn end_load(&mut self, message: &Msg) {
        use crate::activity::Load;
        let load = match message {
            Msg::Snapshot(generation, _) if *generation == self.snapshot_requested => {
                Load::Repository
            }
            Msg::Main(generation, _) if *generation == self.main_generation => Load::Main,
            Msg::CommitFiles(generation, _) if *generation == self.commit_files_generation => {
                Load::CommitFiles
            }
            Msg::SubCommits(generation, _) if *generation == self.sub_commits_generation => {
                Load::SubCommits
            }
            Msg::MoreCommits(_) => Load::MoreCommits,
            _ => return,
        };
        self.activity.end(load);
    }

    fn message(&mut self, message: Msg) -> Update<Msg> {
        self.end_load(&message);
        match message {
            Msg::Snapshot(generation, snapshot) => {
                if generation >= self.snapshot_loaded {
                    self.snapshot_loaded = generation;
                    self.install_snapshot(*snapshot);
                }
                let mut update = Update::redraw();
                for command in self.resume_drilled() {
                    update = update.with_command(command);
                }
                if let Some(command) = self.sync_main(true) {
                    update = update.maybe(command);
                }
                update
                    .maybe(self.request_pull_requests(false))
                    .maybe(self.match_pull_requests())
            }
            Msg::PullRequests(generation, result) => {
                self.pull_requests_answered(generation, result)
            }
            Msg::PullRequestMatches(generation, matched) => {
                self.pull_requests_matched(generation, matched)
            }
            Msg::Live(live) => self.install_live(*live),
            Msg::Metadata => Update::none().maybe(self.reload_metadata()),
            Msg::Main(generation, content) => {
                if generation == self.main_generation {
                    self.install_main(*content);
                }
                Update::redraw()
            }
            Msg::CommitFiles(generation, files) => {
                if generation == self.commit_files_generation
                    && let Some(view) = &mut self.commit_files
                {
                    match files {
                        Ok(listing) => {
                            view.files = listing.files;
                            view.counts = listing.counts;
                        }
                        Err(error) => self.toast(error, true),
                    }
                }
                let mut update = Update::redraw();
                if let Some(command) = self.sync_main(false) {
                    update = update.maybe(command);
                }
                update
            }
            Msg::SubCommits(generation, commits) => {
                if generation == self.sub_commits_generation
                    && let Some(sub) = &mut self.sub_commits
                {
                    match commits {
                        Ok(commits) => {
                            sub.graph = graph::layout(&commits);
                            sub.commits = commits;
                        }
                        Err(error) => self.toast(error, true),
                    }
                }
                let mut update = Update::redraw();
                if let Some(command) = self.sync_main(false) {
                    update = update.maybe(command);
                }
                update
            }
            Msg::MoreCommits(result) => {
                match result {
                    Ok(commits) => {
                        self.snapshot.more_commits = commits.len() >= self.config.log_page_size;
                        self.snapshot.commits.extend(commits);
                        self.commit_graph = graph::layout(&self.snapshot.commits);
                    }
                    Err(error) => self.toast(error, true),
                }
                Update::redraw()
            }
            Msg::OpDone(result) => self.finish_op(&result),
            Msg::Watch => {
                let update = Update::none().maybe(self.check_commit_prefill());
                if self.activity.running() {
                    update
                } else {
                    update.maybe(self.refresh_all())
                }
            }
            Msg::CommitSourcesChanged => Update::none().maybe(self.check_commit_prefill()),
            Msg::CommitPrefill(prefill) => self.apply_commit_prefill(*prefill),
            Msg::Tick => self.tick(),
            Msg::Frame => self.frame(),
            Msg::InputFailed(error) => {
                self.diagnostics.push(error);
                self.quit = true;
                Update::exit()
            }
        }
    }
}

impl Program for App {
    type Message = Msg;
    type Error = String;

    fn update(&mut self, event: RuntimeEvent<Msg>) -> Result<Update<Msg>, String> {
        let watching_sources = self.commit_editor().is_some();
        let mut update = self.handle(event);
        self.sync_panels();
        // Live output: a running shell command's terminal and the git command log.
        let (pumped, command) = self.pump_terminal();
        if pumped | self.sync_command_log() {
            update = update.merge(Update::redraw());
        }
        update = update.maybe(command);
        self.sync_animation();
        if self.quit {
            self.persist();
        }
        // Watch prefill sources exactly while a commit editor is open.
        let update = match (watching_sources, self.commit_editor().is_some()) {
            (false, true) => update.with_subscription(crate::commit_message::subscription(
                crate::commit_message::watched_files(&self.repo, &self.commit_sources),
            )),
            (true, false) => {
                update.cancel_subscription(SubscriptionKey::new(crate::commit_message::WATCH_KEY))
            }
            _ => update,
        };
        Ok(update)
    }
}

impl App {
    fn handle(&mut self, event: RuntimeEvent<Msg>) -> Update<Msg> {
        let update = match event {
            RuntimeEvent::Terminal(event) => match &event {
                Event::Key(stroke) => self.key(*stroke, &event),
                Event::Paste(_) => {
                    let policy = TextInputPolicy::chat_composer();
                    match self.popups.last_mut() {
                        Some(Popup::Prompt(prompt)) => {
                            paste_into_prompt(prompt, &event);
                            return self.apply_live_filter();
                        }
                        Some(Popup::Menu(menu)) => {
                            if let Some(filter) = &menu.filter {
                                TextInputControl::new(&policy)
                                    .handle_event(&mut filter.borrow_mut(), &event);
                                menu.selected = 0;
                            }
                        }
                        _ => {}
                    }
                    Update::redraw()
                }
                Event::Mouse(_) => self.mouse(&event),
                Event::Resize(_) => Update::redraw(),
                Event::Focus(focus) if format!("{focus:?}").contains("Gained") => {
                    Update::redraw().maybe(self.refresh())
                }
                _ => Update::none(),
            },
            RuntimeEvent::Message(message) => self.message(message),
            RuntimeEvent::Timer(_) => Update::none(),
        };
        if self.quit { Update::exit() } else { update }
    }
}

/// Editing behavior of a prompt field: the summary of a commit message is one line, its
/// description and multi-line prompts take newlines.
const fn field_policy(prompt: &Prompt) -> TextInputPolicy {
    let mut policy = TextInputPolicy::chat_composer();
    if prompt.body.is_some() && !prompt.body_focused {
        policy.keyboard.shift_enter = None;
    } else if prompt.body_focused {
        policy.keyboard.enter = EnterBehavior::InsertNewline;
    }
    policy
}

/// Deliver an event to the focused field of a prompt.
fn prompt_input(prompt: &Prompt, event: &Event) -> TextInputOutcome {
    let policy = field_policy(prompt);
    TextInputControl::new(&policy).handle_event(&mut prompt.focused_input().borrow_mut(), event)
}

/// Paste into a prompt. Multi-line text pasted into a summary keeps its first line there and
/// moves the rest into the description, as a commit message would be split.
fn paste_into_prompt(prompt: &mut Prompt, event: &Event) {
    if let (Event::Paste(text), Some(_), false) = (event, &prompt.body, prompt.body_focused)
        && text.contains('\n')
    {
        let (summary, rest) = crate::popup::split_message(text);
        let policy = field_policy(prompt);
        TextInputControl::new(&policy).handle_paste(&mut prompt.input.borrow_mut(), summary);
        prompt.body_focused = true;
        let policy = field_policy(prompt);
        TextInputControl::new(&policy).handle_paste(&mut prompt.focused_input().borrow_mut(), rest);
        return;
    }
    prompt_input(prompt, event);
}
