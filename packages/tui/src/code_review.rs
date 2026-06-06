//! Full-screen local code review TUI mode.

use std::io::Write;
use std::path::PathBuf;

use bcode_client::BcodeClient;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, FocusEvent, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;
use serde::{Deserialize, Serialize};

use super::terminal_events::TuiInput;
use super::{TuiError, helpers};

const SERVICE_INTERFACE_ID: &str = "bcode.code_review/v1";
const CREATE_REVIEW_OPERATION: &str = "create_review";
const FILE_SIDEBAR_WIDTH: u16 = 34;

/// Local Git target to open in review mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewOpenTarget {
    /// Review unstaged working-tree changes.
    WorkingTreeUnstaged,
    /// Review staged index changes.
    IndexStaged,
    /// Review staged and unstaged changes together.
    WorkingTreeAndIndex,
    /// Review the last commit.
    LastCommit,
    /// Review a commit range.
    CommitRange {
        /// Base revision.
        base: String,
        /// Head revision.
        head: String,
        /// Whether to use merge-base semantics.
        merge_base: bool,
    },
    /// Review a branch comparison.
    BranchCompare {
        /// Base branch.
        base_branch: String,
        /// Head branch.
        head_branch: String,
        /// Whether to use merge-base semantics.
        merge_base: bool,
    },
}

/// Run a full-screen local Git review.
///
/// # Errors
///
/// Returns an error when review data cannot be loaded or terminal I/O fails.
pub async fn run<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
    target: ReviewOpenTarget,
) -> Result<(), TuiError> {
    let client = BcodeClient::default_endpoint();
    let review = load_review(&client, repo_path, target).await?;
    let mut input = TuiInput::start();
    let mut app = ReviewApp::new(review);
    let mut needs_redraw = true;

    while !app.should_exit {
        if helpers::resize_from_terminal(terminal)? {
            needs_redraw = true;
        }
        if needs_redraw {
            terminal.draw(|frame| super::code_review_render::render(&mut app, frame))?;
            needs_redraw = false;
        }
        let Some(event) = input.recv().await? else {
            continue;
        };
        if handle_event(&mut app, terminal, &event) {
            needs_redraw = true;
        }
    }

    Ok(())
}

async fn load_review(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewOpenTarget,
) -> Result<ReviewSummary, TuiError> {
    let request = CreateReviewRequest {
        repo_path,
        target: target.into(),
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            CREATE_REVIEW_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    serde_json::from_slice(&response.payload).map_err(TuiError::Json)
}

fn handle_event<W: Write>(
    app: &mut ReviewApp,
    terminal: &mut Terminal<&mut W>,
    event: &Event,
) -> bool {
    match event {
        Event::Resize(size) => {
            terminal.resize(Rect::new(0, 0, size.width, size.height));
            true
        }
        Event::Key(stroke) => handle_key(app, *stroke),
        Event::Mouse(mouse) => handle_mouse(app, *mouse),
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick => true,
        Event::Paste(_) | Event::User(_) => false,
    }
}

fn handle_key(app: &mut ReviewApp, stroke: KeyStroke) -> bool {
    if !stroke.modifiers.is_empty() {
        return false;
    }
    match stroke.key {
        KeyCode::Char('q') | KeyCode::Escape => {
            app.should_exit = true;
            true
        }
        KeyCode::Char('b') => {
            app.sidebar_visible = !app.sidebar_visible;
            true
        }
        KeyCode::Char('j') | KeyCode::Down => app.scroll_down(1),
        KeyCode::Char('k') | KeyCode::Up => app.scroll_up(1),
        KeyCode::Char('g') => app.scroll_to_top(),
        KeyCode::Char('G') => app.scroll_to_bottom(),
        KeyCode::Char('n') | KeyCode::Right => app.select_next_file(),
        KeyCode::Char('p') | KeyCode::Left => app.select_previous_file(),
        KeyCode::Char('J') => app.select_next_hunk(),
        KeyCode::Char('K') => app.select_previous_hunk(),
        KeyCode::Char('?') => {
            app.help_visible = !app.help_visible;
            true
        }
        _ => false,
    }
}

fn handle_mouse(app: &mut ReviewApp, mouse: MouseEvent) -> bool {
    match mouse.kind {
        MouseEventKind::ScrollUp => app.scroll_up(3),
        MouseEventKind::ScrollDown => app.scroll_down(3),
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(index) = app.file_index_at(mouse.position.x, mouse.position.y) {
                app.select_file(index)
            } else if let Some(index) = app.diff_line_index_at(mouse.position.x, mouse.position.y) {
                app.select_diff_line(index)
            } else {
                false
            }
        }
        MouseEventKind::Down(MouseButton::Right | MouseButton::Middle | MouseButton::Other(_))
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::Move
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CreateReviewRequest {
    repo_path: PathBuf,
    target: ReviewTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReviewTarget {
    WorkingTreeUnstaged,
    IndexStaged,
    WorkingTreeAndIndex,
    LastCommit,
    CommitRange {
        base: String,
        head: String,
        #[serde(default)]
        merge_base: bool,
    },
    BranchCompare {
        base_branch: String,
        head_branch: String,
        #[serde(default)]
        merge_base: bool,
    },
}

impl From<ReviewOpenTarget> for ReviewTarget {
    fn from(target: ReviewOpenTarget) -> Self {
        match target {
            ReviewOpenTarget::WorkingTreeUnstaged => Self::WorkingTreeUnstaged,
            ReviewOpenTarget::IndexStaged => Self::IndexStaged,
            ReviewOpenTarget::WorkingTreeAndIndex => Self::WorkingTreeAndIndex,
            ReviewOpenTarget::LastCommit => Self::LastCommit,
            ReviewOpenTarget::CommitRange {
                base,
                head,
                merge_base,
            } => Self::CommitRange {
                base,
                head,
                merge_base,
            },
            ReviewOpenTarget::BranchCompare {
                base_branch,
                head_branch,
                merge_base,
            } => Self::BranchCompare {
                base_branch,
                head_branch,
                merge_base,
            },
        }
    }
}

/// Full review summary displayed by the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewSummary {
    /// Human-readable review title.
    pub title: String,
    /// Git repository root.
    pub repo_root: PathBuf,
    /// Changed files.
    pub files: Vec<ReviewFile>,
    /// Total additions.
    pub additions: u32,
    /// Total deletions.
    pub deletions: u32,
}

/// Changed file displayed by the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewFile {
    /// Old path.
    pub old_path: Option<String>,
    /// New path.
    pub new_path: Option<String>,
    /// File status.
    pub status: ReviewFileStatus,
    /// Additions.
    pub additions: u32,
    /// Deletions.
    pub deletions: u32,
    /// Hunks.
    pub hunks: Vec<ReviewHunk>,
    /// Binary marker.
    pub is_binary: bool,
}

impl ReviewFile {
    /// Return the display path.
    #[must_use]
    pub fn display_path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("<unknown>")
    }
}

/// Review file status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFileStatus {
    /// Modified file.
    Modified,
    /// Added file.
    Added,
    /// Deleted file.
    Deleted,
    /// Renamed file.
    Renamed,
    /// Copied file.
    Copied,
    /// Unknown status.
    Unknown,
}

impl ReviewFileStatus {
    /// Return a compact status label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Copied => "C",
            Self::Unknown => "?",
        }
    }
}

/// Review hunk.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewHunk {
    /// Old start line.
    pub old_start: u32,
    /// Old line count.
    pub old_count: u32,
    /// New start line.
    pub new_start: u32,
    /// New line count.
    pub new_count: u32,
    /// Optional heading.
    pub heading: Option<String>,
    /// Lines.
    pub lines: Vec<ReviewLine>,
}

/// Review diff line.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewLine {
    /// Line kind.
    pub kind: ReviewLineKind,
    /// Old line number.
    pub old_line: Option<u32>,
    /// New line number.
    pub new_line: Option<u32>,
    /// Content without diff marker.
    pub content: String,
}

/// Review diff line kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewLineKind {
    /// Context line.
    Context,
    /// Added line.
    Added,
    /// Removed line.
    Removed,
}

/// Stateful review app model.
#[derive(Debug, Clone)]
pub struct ReviewApp {
    /// Review data.
    pub review: ReviewSummary,
    /// Selected file index.
    pub selected_file: usize,
    /// Top visible file row.
    pub file_scroll: usize,
    /// Top visible rendered diff row.
    pub diff_scroll: usize,
    /// Selected rendered diff row.
    pub selected_diff_line: usize,
    /// Whether the file sidebar is visible.
    pub sidebar_visible: bool,
    /// Whether help is visible.
    pub help_visible: bool,
    /// Whether to exit.
    pub should_exit: bool,
    last_file_area: Option<Rect>,
    last_diff_area: Option<Rect>,
}

impl ReviewApp {
    /// Create a new review app.
    #[must_use]
    pub const fn new(review: ReviewSummary) -> Self {
        Self {
            review,
            selected_file: 0,
            file_scroll: 0,
            diff_scroll: 0,
            selected_diff_line: 0,
            sidebar_visible: true,
            help_visible: false,
            should_exit: false,
            last_file_area: None,
            last_diff_area: None,
        }
    }

    /// Store the current file hit area.
    pub const fn set_file_area(&mut self, area: Option<Rect>) {
        self.last_file_area = area;
    }

    /// Store the current diff hit area.
    pub const fn set_diff_area(&mut self, area: Rect) {
        self.last_diff_area = Some(area);
    }

    /// Return currently selected file.
    #[must_use]
    pub fn selected_file_data(&self) -> Option<&ReviewFile> {
        self.review.files.get(self.selected_file)
    }

    /// Select a file by index.
    pub const fn select_file(&mut self, index: usize) -> bool {
        if index >= self.review.files.len() || index == self.selected_file {
            return false;
        }
        self.selected_file = index;
        self.diff_scroll = 0;
        self.selected_diff_line = 0;
        true
    }

    /// Select next file.
    pub fn select_next_file(&mut self) -> bool {
        self.select_file((self.selected_file + 1).min(self.review.files.len().saturating_sub(1)))
    }

    /// Select previous file.
    pub const fn select_previous_file(&mut self) -> bool {
        self.select_file(self.selected_file.saturating_sub(1))
    }

    /// Scroll diff down.
    pub fn scroll_down(&mut self, rows: usize) -> bool {
        let max = self.max_diff_scroll();
        let next = self.diff_scroll.saturating_add(rows).min(max);
        if next == self.diff_scroll {
            return false;
        }
        self.diff_scroll = next;
        true
    }

    /// Scroll diff up.
    pub const fn scroll_up(&mut self, rows: usize) -> bool {
        let next = self.diff_scroll.saturating_sub(rows);
        if next == self.diff_scroll {
            return false;
        }
        self.diff_scroll = next;
        true
    }

    /// Scroll to top.
    pub const fn scroll_to_top(&mut self) -> bool {
        if self.diff_scroll == 0 {
            return false;
        }
        self.diff_scroll = 0;
        true
    }

    /// Scroll to bottom.
    pub fn scroll_to_bottom(&mut self) -> bool {
        let max = self.max_diff_scroll();
        if self.diff_scroll == max {
            return false;
        }
        self.diff_scroll = max;
        true
    }

    /// Select next hunk.
    pub fn select_next_hunk(&mut self) -> bool {
        let Some(next) = self
            .hunk_render_rows()
            .into_iter()
            .find(|row| *row > self.diff_scroll)
        else {
            return false;
        };
        self.diff_scroll = next;
        true
    }

    /// Select previous hunk.
    pub fn select_previous_hunk(&mut self) -> bool {
        let Some(previous) = self
            .hunk_render_rows()
            .into_iter()
            .rev()
            .find(|row| *row < self.diff_scroll)
        else {
            return false;
        };
        self.diff_scroll = previous;
        true
    }

    /// Select a visible diff line by rendered row index.
    pub const fn select_diff_line(&mut self, index: usize) -> bool {
        self.selected_diff_line = index;
        true
    }

    /// Return visible file index under terminal coordinates.
    #[must_use]
    pub fn file_index_at(&self, x: u16, y: u16) -> Option<usize> {
        let area = self.last_file_area?;
        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
            return None;
        }
        let index = self.file_scroll + usize::from(y.saturating_sub(area.y));
        (index < self.review.files.len()).then_some(index)
    }

    /// Return visible diff row index under terminal coordinates.
    #[must_use]
    pub fn diff_line_index_at(&self, x: u16, y: u16) -> Option<usize> {
        let area = self.last_diff_area?;
        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
            return None;
        }
        Some(self.diff_scroll + usize::from(y.saturating_sub(area.y)))
    }

    fn max_diff_scroll(&self) -> usize {
        self.rendered_diff_len().saturating_sub(
            self.last_diff_area
                .map_or(1, |area| usize::from(area.height).max(1)),
        )
    }

    fn rendered_diff_len(&self) -> usize {
        let Some(file) = self.selected_file_data() else {
            return 1;
        };
        if file.is_binary {
            return 1;
        }
        file.hunks
            .iter()
            .map(|hunk| hunk.lines.len().saturating_add(1))
            .sum::<usize>()
            .max(1)
    }

    fn hunk_render_rows(&self) -> Vec<usize> {
        let Some(file) = self.selected_file_data() else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        let mut row = 0usize;
        for hunk in &file.hunks {
            rows.push(row);
            row = row.saturating_add(hunk.lines.len()).saturating_add(1);
        }
        rows
    }
}

/// Return current sidebar width for an app and terminal width.
#[must_use]
pub fn sidebar_width(app: &ReviewApp, width: u16) -> u16 {
    if app.sidebar_visible && width >= 80 {
        FILE_SIDEBAR_WIDTH.min(width.saturating_sub(30))
    } else {
        0
    }
}
