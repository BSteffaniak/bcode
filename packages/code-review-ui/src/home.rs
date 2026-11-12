//! Review home: suggestions derived from repository state, the reviews table
//! grouped by state, filtering, a preview panel, rename, archive, restore, and continue-latest.
//!
//! * **Loading.** [`Home::refresh`] starts four bounded tasks: the review listing, the
//!   repository summary (`review.repo.state`), the providers' pull requests, and (once both the
//!   branch and its pull request are known) the stack containing it. Per-review stats (file
//!   totals, and whether a commit head moved since the recorded pass) load for visible rows
//!   first, then for the most recent reviews, two at a time and at most [`MAX_STATS`] per
//!   refresh. Nothing blocks input; rows and suggestions update as answers arrive.
//! * **Screen.** A header, the suggestions strip (a BMUX `menu`), the filter row (a BMUX text
//!   input), the reviews table (a BMUX `table` whose group headings are rows), a preview panel
//!   (a BMUX `pane`) when wide enough, and the footer (key hints, status, toasts). Each region's
//!   rectangle is kept from the frame it was painted in; pointer events route through exactly
//!   those rectangles.
//! * **Narrow degradation.** Below 100 columns the preview hides; table columns drop from the
//!   right of their importance order (publish, sources, notes, viewed); suggestions need at least
//!   10 body rows, and the filter row 4 rows left for the table.

pub(crate) mod load;
pub mod model;

use super::chrome::{self, Status, Toasts};
use super::entry::Entry;
use super::palette::{Overlay, OverlayKind, OverlayOutcome};
use super::services::ReviewServices;
use super::theme::ReviewTheme;
use crate::context::UiContext;
use bcode_code_review_models::{
    ReviewRepoStateResponse, ReviewStack, ReviewWorkspace, ReviewWorkspaceListItem,
};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span, Text};
use bmux_tui_components::empty_state::{EmptyStateComponent, EmptyStatePolicy};
use bmux_tui_components::menu::{Menu, MenuItem, MenuOutcome, MenuPolicy, MenuState};
use bmux_tui_components::pane::{Pane, PaneComponent, PaneState, PaneStyles};
use bmux_tui_components::spinner::Spinner;
use bmux_tui_components::status_bar::StatusSeverity;
use bmux_tui_components::table::{
    Table, TableAlign, TableColumn, TableOutcome, TablePolicy, TableRow, TableState,
};
use bmux_tui_components::text_input::{
    EdgePolicy, TextInputComponent, TextInputControl, TextInputOutcome, TextInputPolicy,
    TextInputState,
};
use bmux_tui_components::toast_stack::ToastSeverity;
use load::HomeMessage;
use model::{
    Group, PullsAnswer, RowKey, RowRef, Suggestion, SuggestionAction, SuggestionInputs,
    SuggestionKind, WorkspaceStats, plural,
};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Keymap scope of home.
pub const HOME_SCOPE: &str = "code-review-home";
/// Most reviews whose stats load per refresh.
pub const MAX_STATS: usize = 40;
/// Stats requests in flight at once.
const STATS_IN_FLIGHT: usize = 2;
/// Most suggestions shown (the wizard entry included).
const MAX_SUGGESTIONS: usize = 7;
/// Narrowest screen showing the preview panel.
const PREVIEW_MIN_WIDTH: u16 = 100;

/// What an input on home asks the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeOutcome {
    /// Nothing happened.
    Ignored,
    /// Home changed.
    Redraw,
    /// Leave the review app.
    Exit,
    /// Open a review.
    Open(Box<ReviewWorkspace>),
    /// Open a new transient review.
    Start(Entry),
    /// Open the new-review wizard.
    New,
}

/// Keyboard focus on home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeFocus {
    /// The reviews table.
    Table,
    /// The suggestions strip.
    Suggestions,
    /// The filter input.
    Filter,
    /// The rename input.
    Rename,
}

/// Finished work, each tagged with the refresh generation it was started in.
#[derive(Debug, Clone, Default)]
struct Inbox(Arc<Mutex<Vec<(u64, HomeMessage)>>>);

impl Inbox {
    fn push(&self, generation: u64, message: HomeMessage) {
        if let Ok(mut queue) = self.0.lock() {
            queue.push((generation, message));
        }
    }

    fn drain(&self) -> Vec<(u64, HomeMessage)> {
        self.0
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default()
    }
}

/// Rectangles of the last frame (absolute).
#[derive(Debug, Clone, Copy, Default)]
struct Regions {
    suggestions: Option<Rect>,
    filter: Option<Rect>,
    table: Option<Rect>,
    preview: Option<Rect>,
}

/// Table columns, in importance order of degradation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Title,
    Sources,
    Viewed,
    Notes,
    Publish,
    Updated,
}

/// The columns shown at table width `width`: the title and update time always, then viewed
/// progress from 56 columns, notes from 70, sources from 80, and publish status from 96.
fn columns_for(width: u16) -> Vec<Column> {
    let mut columns = vec![Column::Title];
    if width >= 80 {
        columns.push(Column::Sources);
    }
    if width >= 56 {
        columns.push(Column::Viewed);
    }
    if width >= 70 {
        columns.push(Column::Notes);
    }
    if width >= 96 {
        columns.push(Column::Publish);
    }
    columns.push(Column::Updated);
    columns
}

const fn table_column(column: Column) -> TableColumn<'static> {
    match column {
        Column::Title => TableColumn::new("Review").flex(3),
        Column::Sources => TableColumn::new("Sources").flex(2),
        Column::Viewed => TableColumn::new("Viewed").fixed(15),
        Column::Notes => TableColumn::new("Notes").fixed(10),
        Column::Publish => TableColumn::new("Publish").fixed(14),
        Column::Updated => TableColumn::new("Updated")
            .fixed(8)
            .align(TableAlign::Right),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The review home.
// Independent flags of loading and presentation state, not a mode.
#[allow(clippy::struct_excessive_bools)]
pub struct Home {
    services: ReviewServices,
    repo: PathBuf,
    inbox: Inbox,
    /// Refresh generation: answers of an older refresh still running are not applied over a
    /// newer one's.
    generation: u64,
    needs_refresh: bool,
    loading: usize,
    loaded: bool,
    list_error: Option<String>,
    items: Vec<ReviewWorkspaceListItem>,
    state: Option<ReviewRepoStateResponse>,
    pulls: Option<PullsAnswer>,
    pulls_error: Option<String>,
    stack: Option<ReviewStack>,
    stack_requested: bool,
    stats: BTreeMap<String, WorkspaceStats>,
    stats_requested: BTreeSet<String>,
    stats_in_flight: usize,
    visible: RefCell<Vec<String>>,
    collapsed: BTreeSet<Group>,
    filter: RefCell<TextInputState>,
    rename: RefCell<TextInputState>,
    renaming: Option<String>,
    input_policy: TextInputPolicy,
    focus: HomeFocus,
    rows: Vec<RowRef>,
    table_rows: Vec<TableRow>,
    columns: Vec<Column>,
    rows_dirty: bool,
    rows_theme: Option<ReviewTheme>,
    selected: Option<RowKey>,
    table: RefCell<TableState>,
    suggestions: Vec<Suggestion>,
    suggestion_items: Vec<MenuItem>,
    suggestion_state: MenuState,
    preview: bool,
    busy: Option<String>,
    status: Option<Status>,
    toasts: Toasts,
    overlay: Option<Overlay>,
    regions: Regions,
    preview_pane: Cell<PaneState>,
    started: Instant,
    action_keys: BTreeMap<String, String>,
}

impl std::fmt::Debug for Home {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Home")
            .field("repo", &self.repo)
            .field("items", &self.items.len())
            .field("focus", &self.focus)
            .finish_non_exhaustive()
    }
}

/// Actions whose first key the footer hints show.
const HINT_ACTIONS: &[&str] = &[
    "home_open",
    "home_new",
    "home_continue",
    "home_filter",
    "home_rename",
    "home_archive",
    "home_restore",
    "home_toggle_group",
    "home_preview",
    "home_focus_next",
    "home_refresh",
    "home_help",
    "home_exit",
];

impl Home {
    /// Home of the repository at `repo`; the first poll loads it.
    #[must_use]
    pub fn new(services: ReviewServices, repo: PathBuf) -> Self {
        let mut input_policy = TextInputPolicy::chat_composer();
        input_policy.keyboard.shift_enter = None;
        input_policy.viewport.max_rows = Some(1);
        input_policy.edge = EdgePolicy::disabled();
        Self {
            services,
            repo,
            inbox: Inbox::default(),
            generation: 0,
            needs_refresh: true,
            loading: 0,
            loaded: false,
            list_error: None,
            items: Vec::new(),
            state: None,
            pulls: None,
            pulls_error: None,
            stack: None,
            stack_requested: false,
            stats: BTreeMap::new(),
            stats_requested: BTreeSet::new(),
            stats_in_flight: 0,
            visible: RefCell::new(Vec::new()),
            collapsed: BTreeSet::from([Group::Archived]),
            filter: RefCell::new(TextInputState::new(bmux_text_edit::TextEditBuffer::new())),
            rename: RefCell::new(TextInputState::new(bmux_text_edit::TextEditBuffer::new())),
            renaming: None,
            input_policy,
            focus: HomeFocus::Table,
            rows: Vec::new(),
            table_rows: Vec::new(),
            columns: Vec::new(),
            rows_dirty: true,
            rows_theme: None,
            selected: None,
            table: RefCell::new(TableState::new(None)),
            suggestions: Vec::new(),
            suggestion_items: Vec::new(),
            suggestion_state: MenuState::new(None),
            preview: true,
            busy: None,
            status: None,
            toasts: Toasts::default(),
            overlay: None,
            regions: Regions::default(),
            preview_pane: Cell::new(PaneState::new(Rect::default())),
            started: Instant::now(),
            action_keys: BTreeMap::new(),
        }
    }

    /// Reload everything on the next poll (after returning from a review).
    pub const fn refresh(&mut self) {
        self.needs_refresh = true;
    }

    /// Remember the effective keys of the actions the footer hints show.
    pub fn prepare_keymap(&mut self, host: &UiContext) {
        self.action_keys = HINT_ACTIONS
            .iter()
            .filter_map(|action| {
                host.keymap_keys(HOME_SCOPE, &format!("bcode.code_review.{action}"))
                    .into_iter()
                    .next()
                    .map(|key| ((*action).to_owned(), key))
            })
            .collect();
    }

    // ----------------------------------------------------------------------------------------
    // Observation (tests)
    // ----------------------------------------------------------------------------------------

    /// Whether the first listing arrived.
    #[must_use]
    pub const fn loaded(&self) -> bool {
        self.loaded
    }

    /// Tasks still loading (listing, repository state, pull requests, stack, stats).
    #[must_use]
    pub const fn loading(&self) -> usize {
        self.loading + self.stats_in_flight
    }

    /// Titles of the suggestions shown, in order.
    #[must_use]
    pub fn suggestion_titles(&self) -> Vec<&str> {
        self.suggestions
            .iter()
            .map(|suggestion| suggestion.title.as_str())
            .collect()
    }

    /// The suggestions shown.
    #[must_use]
    pub fn suggestions(&self) -> &[Suggestion] {
        &self.suggestions
    }

    /// Table rows as `(group or review title)` labels, in order.
    #[must_use]
    pub fn row_labels(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|row| match row {
                RowRef::Group {
                    group,
                    count,
                    collapsed,
                } => format!(
                    "{}{} ({count})",
                    if *collapsed { "+" } else { "" },
                    group.label()
                ),
                RowRef::Review(index) => self.items[*index].workspace.title.clone(),
            })
            .collect()
    }

    /// Key of the selected row.
    #[must_use]
    pub const fn selected(&self) -> Option<&RowKey> {
        self.selected.as_ref()
    }

    /// Keyboard focus.
    #[must_use]
    pub const fn focus(&self) -> HomeFocus {
        self.focus
    }

    /// Stats of a review, once loaded.
    #[must_use]
    pub fn stats(&self, id: &str) -> Option<&WorkspaceStats> {
        self.stats.get(id)
    }

    /// Status line text.
    #[must_use]
    pub fn status(&self) -> Option<&str> {
        self.status.as_ref().map(|status| status.text.as_str())
    }

    /// Toast titles shown.
    #[must_use]
    pub fn toasts(&self) -> Vec<&str> {
        self.toasts.titles()
    }

    /// The listed reviews.
    #[must_use]
    pub fn items(&self) -> &[ReviewWorkspaceListItem] {
        &self.items
    }

    /// Absolute area of the reviews table in the last frame.
    #[must_use]
    pub const fn table_area(&self) -> Option<Rect> {
        self.regions.table
    }

    /// Absolute area of the suggestions strip in the last frame.
    #[must_use]
    pub const fn suggestions_area(&self) -> Option<Rect> {
        self.regions.suggestions
    }

    /// The open help overlay.
    #[must_use]
    pub const fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    // ----------------------------------------------------------------------------------------
    // Loading
    // ----------------------------------------------------------------------------------------

    fn spawn(
        &self,
        host: &UiContext,
        task: impl std::future::Future<Output = HomeMessage> + Send + 'static,
    ) {
        let (inbox, generation) = (self.inbox.clone(), self.generation);
        host.spawn(Box::pin(async move {
            inbox.push(generation, task.await);
        }));
    }

    fn start_refresh(&mut self, host: &UiContext) {
        self.needs_refresh = false;
        self.generation += 1;
        self.stats_requested.clear();
        self.stack_requested = false;
        self.loading += 3;
        let (services, repo) = (self.services.clone(), self.repo.clone());
        self.spawn(host, async move {
            HomeMessage::Listed(load::list(services, repo).await)
        });
        let (services, repo) = (self.services.clone(), self.repo.clone());
        self.spawn(host, async move {
            HomeMessage::State(load::state(services, repo).await)
        });
        let (services, repo) = (self.services.clone(), self.repo.clone());
        self.spawn(host, async move {
            HomeMessage::Pulls(load::pulls(services, repo).await)
        });
    }

    /// Ask for the stack of the current branch's pull request once both are known.
    fn request_stack(&mut self, host: &UiContext) {
        if self.stack_requested {
            return;
        }
        let (Some(state), Some(pulls)) = (&self.state, &self.pulls) else {
            return;
        };
        if !pulls.route.manifest.capabilities.stack_resolve {
            return;
        }
        let Some(number) = state.branch.as_ref().and_then(|branch| {
            pulls
                .pulls
                .iter()
                .find(|pull| pull.head_branch == *branch)
                .map(|pull| pull.number)
        }) else {
            return;
        };
        self.stack_requested = true;
        self.loading += 1;
        let services = self.services.clone();
        let (route, repo, repository) = (
            pulls.route.clone(),
            self.repo.clone(),
            pulls.repository.clone(),
        );
        self.spawn(host, async move {
            HomeMessage::Stack(load::stack(services, route, repo, repository, number).await)
        });
    }

    /// Request stats for visible reviews first, then the most recent ones.
    fn schedule_stats(&mut self, host: &UiContext) {
        if !self.loaded
            || self.stats_in_flight >= STATS_IN_FLIGHT
            || self.stats_requested.len() >= MAX_STATS
        {
            return;
        }
        let visible = self.visible.borrow().clone();
        let mut recent: Vec<&ReviewWorkspaceListItem> = self
            .items
            .iter()
            .filter(|item| item.workspace.archived_at_ms.is_none())
            .collect();
        recent.sort_by_key(|item| std::cmp::Reverse(item.workspace.updated_at_ms.unwrap_or(0)));
        let candidates: Vec<String> = visible
            .into_iter()
            .chain(recent.into_iter().map(|item| item.workspace.id.clone()))
            .collect();
        for id in candidates {
            if self.stats_in_flight >= STATS_IN_FLIGHT || self.stats_requested.len() >= MAX_STATS {
                break;
            }
            if self.stats_requested.contains(&id) {
                continue;
            }
            let Some(item) = self.items.iter().find(|item| item.workspace.id == id) else {
                continue;
            };
            let workspace = item.workspace.clone();
            self.stats_requested.insert(id.clone());
            self.stats_in_flight += 1;
            let services = self.services.clone();
            self.spawn(host, async move {
                HomeMessage::Stats {
                    id,
                    result: load::stats(services, workspace).await,
                }
            });
        }
    }

    /// Apply finished work and start what is due. Returns whether home changed.
    pub fn poll(&mut self, host: &UiContext) -> bool {
        let refresh = self.needs_refresh && self.busy.is_none();
        if refresh {
            self.start_refresh(host);
        }
        let mut changed = refresh;
        for (generation, message) in self.inbox.drain() {
            changed = true;
            self.apply(generation, message);
        }
        self.request_stack(host);
        let before = self.stats_in_flight;
        self.schedule_stats(host);
        changed |= self.stats_in_flight != before;
        changed |= self.toasts.expire(Instant::now());
        changed |= self.loading() > 0;
        changed
    }

    fn apply(&mut self, generation: u64, message: HomeMessage) {
        let current = generation == self.generation;
        match message {
            // A newer refresh started meanwhile: its own answers replace these.
            HomeMessage::Listed(_)
            | HomeMessage::State(_)
            | HomeMessage::Pulls(_)
            | HomeMessage::Stack(_)
                if !current =>
            {
                self.loading = self.loading.saturating_sub(1);
                return;
            }
            HomeMessage::Listed(result) => {
                self.loading = self.loading.saturating_sub(1);
                match result {
                    Ok(items) => {
                        self.items = items;
                        self.loaded = true;
                        self.list_error = None;
                        let ids: BTreeSet<&str> = self
                            .items
                            .iter()
                            .map(|item| item.workspace.id.as_str())
                            .collect();
                        self.stats.retain(|id, _| ids.contains(id.as_str()));
                    }
                    Err(error) => {
                        self.loaded = true;
                        self.list_error = Some(error);
                    }
                }
            }
            HomeMessage::State(result) => {
                self.loading = self.loading.saturating_sub(1);
                match result {
                    Ok(state) => self.state = Some(state),
                    Err(error) => self.toasts.push(
                        ToastSeverity::Warning,
                        "Could not read the repository state",
                        Some(error),
                    ),
                }
            }
            HomeMessage::Pulls(result) => {
                self.loading = self.loading.saturating_sub(1);
                match result {
                    Ok(pulls) => {
                        self.pulls = pulls;
                        self.pulls_error = None;
                    }
                    Err(error) => self.pulls_error = Some(error),
                }
            }
            HomeMessage::Stack(result) => {
                self.loading = self.loading.saturating_sub(1);
                self.stack = result.ok();
            }
            HomeMessage::Stats { id, result } => {
                self.stats_in_flight = self.stats_in_flight.saturating_sub(1);
                if let Ok(stats) = result {
                    self.stats.insert(id, stats);
                }
            }
            HomeMessage::Changed { done, result } => {
                self.busy = None;
                match result {
                    Ok(()) => self.toasts.push(ToastSeverity::Success, done, None),
                    Err(error) => self.toasts.push(
                        ToastSeverity::Error,
                        format!("Could not {done}"),
                        Some(error),
                    ),
                }
                self.needs_refresh = true;
            }
        }
        self.rows_dirty = true;
        self.rebuild_suggestions();
    }

    fn rebuild_suggestions(&mut self) {
        let minus = self.rows_theme.map_or("−", |theme| theme.icons.minus);
        let mut suggestions = model::suggestions(
            &SuggestionInputs {
                state: self.state.as_ref(),
                pulls: self.pulls.as_ref(),
                stack: self.stack.as_ref(),
                items: &self.items,
                stats: Some(&self.stats),
            },
            minus,
        );
        if suggestions.len() > MAX_SUGGESTIONS {
            let new = suggestions.pop();
            suggestions.truncate(MAX_SUGGESTIONS - 1);
            suggestions.extend(new);
        }
        let focused = self
            .suggestion_state
            .focused()
            .and_then(|index| self.suggestions.get(index))
            .map(|suggestion| suggestion.id.clone());
        self.suggestions = suggestions;
        self.suggestion_items.clear();
        let index = focused
            .and_then(|id| self.suggestions.iter().position(|item| item.id == id))
            .or(Some(0));
        self.suggestion_state
            .set_focused(index.filter(|_| self.focus == HomeFocus::Suggestions));
        // The selection marker shows only while the strip has focus.
        self.suggestion_state
            .list
            .set_selected(index.filter(|_| self.focus == HomeFocus::Suggestions));
    }

    // ----------------------------------------------------------------------------------------
    // Rows and selection
    // ----------------------------------------------------------------------------------------

    fn filter_text(&self) -> String {
        self.filter.borrow().buffer().text().to_owned()
    }

    fn rebuild_rows(&mut self, theme: &ReviewTheme, width: u16) {
        let columns = columns_for(width);
        if !self.rows_dirty && self.rows_theme == Some(*theme) && self.columns == columns {
            return;
        }
        let theme_changed = self.rows_theme != Some(*theme);
        self.rows_theme = Some(*theme);
        if theme_changed {
            self.rebuild_suggestions();
        }
        self.rows_dirty = false;
        self.columns = columns;
        let words = model::filter_words(&self.filter_text());
        self.rows = model::build_rows(&self.items, &self.stats, &words, &self.collapsed);
        let now = now_ms();
        self.table_rows = self
            .rows
            .iter()
            .map(|row| match row {
                RowRef::Group {
                    group,
                    count,
                    collapsed,
                } => self.group_row(theme, *group, *count, *collapsed),
                RowRef::Review(index) => self.review_row(theme, &self.items[*index], now),
            })
            .collect();
        self.suggestion_items = self
            .suggestions
            .iter()
            .enumerate()
            .map(|(index, suggestion)| suggestion_item(theme, index, suggestion))
            .collect();
        self.sync_selection();
    }

    fn group_row(
        &self,
        theme: &ReviewTheme,
        group: Group,
        count: usize,
        collapsed: bool,
    ) -> TableRow {
        let marker = if collapsed {
            theme.icons.collapsed
        } else {
            theme.icons.menu
        };
        let style = match group {
            Group::NeedsAttention => theme.notice,
            Group::InProgress => theme.accent,
            Group::Published => theme.badge_resolved,
            Group::Archived => theme.muted,
        };
        let mut cells = vec![Line::from_spans(vec![
            Span::styled(format!("{marker} "), theme.muted),
            Span::styled(group.label(), style.patch(theme.title)),
            Span::styled(format!("  {count}"), theme.muted),
        ])];
        cells.extend((1..self.columns.len()).map(|_| Line::from("")));
        TableRow::rich(cells)
    }

    fn review_row(
        &self,
        theme: &ReviewTheme,
        item: &ReviewWorkspaceListItem,
        now: u64,
    ) -> TableRow {
        let workspace = &item.workspace;
        let stats = self.stats.get(&workspace.id);
        let archived = workspace.archived_at_ms.is_some();
        let cells = self
            .columns
            .iter()
            .map(|column| match column {
                Column::Title => Line::from_spans(vec![
                    Span::raw("  "),
                    Span::styled(
                        workspace.title.clone(),
                        if archived { theme.muted } else { theme.path },
                    ),
                ]),
                Column::Sources => sources_line(theme, workspace),
                Column::Viewed => viewed_line(theme, workspace, stats),
                Column::Notes => notes_line(theme, item),
                Column::Publish => publish_line(theme, item, now),
                Column::Updated => Line::from_spans(vec![Span::styled(
                    workspace
                        .updated_at_ms
                        .map_or_else(String::new, |updated| model::relative_time(now, updated)),
                    theme.muted,
                )]),
            })
            .collect::<Vec<_>>();
        TableRow::rich(cells)
    }

    fn row_key(&self, row: &RowRef) -> RowKey {
        match row {
            RowRef::Group { group, .. } => RowKey::Group(*group),
            RowRef::Review(index) => RowKey::Review(self.items[*index].workspace.id.clone()),
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.rows
            .iter()
            .position(|row| self.row_key(row) == *selected)
    }

    /// Keep the selection on the same review or group; else the first review row.
    fn sync_selection(&mut self) {
        let index = self.selected_index().or_else(|| {
            self.rows
                .iter()
                .position(|row| matches!(row, RowRef::Review(_)))
                .or_else(|| (!self.rows.is_empty()).then_some(0))
        });
        self.selected = index.map(|index| self.row_key(&self.rows[index]));
        self.table.borrow_mut().set_selected(index);
    }

    fn select(&mut self, index: usize) {
        if let Some(row) = self.rows.get(index) {
            self.selected = Some(self.row_key(row));
            self.table.borrow_mut().set_selected(Some(index));
        }
    }

    fn selected_item(&self) -> Option<&ReviewWorkspaceListItem> {
        match self.rows.get(self.selected_index()?)? {
            RowRef::Review(index) => self.items.get(*index),
            RowRef::Group { .. } => None,
        }
    }

    fn move_selection(&mut self, delta: i64) {
        if self.rows.is_empty() {
            return;
        }
        let current = self.selected_index().unwrap_or(0);
        let last = self.rows.len() - 1;
        let next = if delta < 0 {
            current.saturating_sub(usize::try_from(-delta).unwrap_or(usize::MAX))
        } else {
            current
                .saturating_add(usize::try_from(delta).unwrap_or(usize::MAX))
                .min(last)
        };
        self.select(next);
    }

    fn toggle_group(&mut self, group: Group) {
        if !self.collapsed.remove(&group) {
            self.collapsed.insert(group);
        }
        self.selected = Some(RowKey::Group(group));
        self.rows_dirty = true;
    }

    fn set_status(&mut self, text: impl Into<String>, severity: StatusSeverity) {
        self.status = Some(Status {
            text: text.into(),
            severity,
        });
    }

    // ----------------------------------------------------------------------------------------
    // Input
    // ----------------------------------------------------------------------------------------

    /// Handle a key.
    pub fn key(&mut self, stroke: KeyStroke, host: &UiContext, theme: &ReviewTheme) -> HomeOutcome {
        if let Some(overlay) = self.overlay.as_mut() {
            return match overlay.key(stroke, theme) {
                OverlayOutcome::Close => {
                    self.overlay = None;
                    HomeOutcome::Redraw
                }
                OverlayOutcome::Run(action) => {
                    self.overlay = None;
                    let action = action.strip_prefix("bcode.code_review.").unwrap_or(&action);
                    let outcome = self.action(action, host, theme);
                    if outcome == HomeOutcome::Ignored {
                        HomeOutcome::Redraw
                    } else {
                        outcome
                    }
                }
                OverlayOutcome::Redraw => HomeOutcome::Redraw,
                OverlayOutcome::Ignored => HomeOutcome::Ignored,
            };
        }
        let before = self.status.clone();
        let outcome = match self.focus {
            HomeFocus::Filter => self.filter_key(stroke),
            HomeFocus::Rename => self.rename_key(stroke, host),
            HomeFocus::Suggestions => self.suggestions_key(stroke, host, theme),
            HomeFocus::Table => self.table_key(stroke, host, theme),
        };
        // Any other handled key acknowledges an earlier status message.
        if before.is_some() && self.status == before && outcome != HomeOutcome::Ignored {
            self.status = None;
        }
        outcome
    }

    fn table_key(
        &mut self,
        stroke: KeyStroke,
        host: &UiContext,
        theme: &ReviewTheme,
    ) -> HomeOutcome {
        if let Some(digit) = digit(stroke)
            && let Some(index) = digit.checked_sub(1)
            && index < self.suggestions.len()
        {
            return self.start_suggestion(index);
        }
        if stroke.key == KeyCode::Escape
            && stroke.modifiers.is_empty()
            && !self.filter_text().is_empty()
        {
            self.clear_filter();
            return HomeOutcome::Redraw;
        }
        let Some(action) = host.keymap_action(HOME_SCOPE, stroke) else {
            return HomeOutcome::Ignored;
        };
        let action = action
            .strip_prefix("bcode.code_review.")
            .unwrap_or(&action)
            .to_owned();
        self.action(&action, host, theme)
    }

    /// Run one home action (from a key or the help overlay).
    fn action(&mut self, action: &str, host: &UiContext, theme: &ReviewTheme) -> HomeOutcome {
        match action {
            "home_down" => self.move_selection(1),
            "home_up" => self.move_selection(-1),
            "home_page_down" => self.move_selection(self.page_rows()),
            "home_page_up" => self.move_selection(-self.page_rows()),
            "home_first" => self.select(0),
            "home_last" => self.select(self.rows.len().saturating_sub(1)),
            "home_open" => return self.open_selected(),
            "home_toggle_group" => {
                let group = match self.selected_index().and_then(|index| self.rows.get(index)) {
                    Some(RowRef::Group { group, .. }) => Some(*group),
                    Some(RowRef::Review(index)) => Some(model::group_of(
                        &self.items[*index],
                        self.stats.get(&self.items[*index].workspace.id),
                    )),
                    None => None,
                };
                if let Some(group) = group {
                    self.toggle_group(group);
                }
            }
            "home_new" => return HomeOutcome::New,
            "home_continue" => return self.continue_latest(),
            "home_filter" => self.focus = HomeFocus::Filter,
            "home_rename" => self.start_rename(),
            "home_archive" => self.archive(host),
            "home_restore" => self.restore(host),
            "home_preview" => self.preview = !self.preview,
            "home_focus_next" => self.focus_suggestions(),
            "home_refresh" => {
                self.needs_refresh = true;
                self.set_status("refreshing…", StatusSeverity::Info);
            }
            "home_help" => {
                let entries = super::palette::entries(
                    &crate::keymap::actions_for(HOME_SCOPE),
                    |id| host.keymap_keys(HOME_SCOPE, id),
                    Some("Reviews"),
                );
                self.overlay = Some(Overlay::new(OverlayKind::Help, entries, theme));
            }
            "home_exit" => return HomeOutcome::Exit,
            _ => return HomeOutcome::Ignored,
        }
        HomeOutcome::Redraw
    }

    fn page_rows(&self) -> i64 {
        self.regions
            .table
            .map_or(10, |area| i64::from(area.height.saturating_sub(1).max(1)))
    }

    fn focus_suggestions(&mut self) {
        if self.regions.suggestions.is_none() || self.suggestions.is_empty() {
            return;
        }
        self.focus = HomeFocus::Suggestions;
        let index = self
            .suggestion_state
            .selected()
            .unwrap_or(0)
            .min(self.suggestions.len() - 1);
        self.suggestion_state.set_focused(Some(index));
        self.suggestion_state.list.set_selected(Some(index));
    }

    fn open_selected(&mut self) -> HomeOutcome {
        match self
            .selected_index()
            .and_then(|index| self.rows.get(index))
            .cloned()
        {
            Some(RowRef::Group { group, .. }) => {
                self.toggle_group(group);
                HomeOutcome::Redraw
            }
            Some(RowRef::Review(index)) => self.open_item(index),
            None => HomeOutcome::Ignored,
        }
    }

    fn open_item(&mut self, index: usize) -> HomeOutcome {
        let workspace = &self.items[index].workspace;
        if workspace.archived_at_ms.is_some() {
            let key = self
                .action_keys
                .get("home_restore")
                .map_or_else(String::new, |key| format!(" ({key})"));
            self.set_status(
                format!(
                    "“{}” is archived; restore it{key} before opening",
                    workspace.title
                ),
                StatusSeverity::Warning,
            );
            return HomeOutcome::Redraw;
        }
        HomeOutcome::Open(Box::new(workspace.clone()))
    }

    fn continue_latest(&mut self) -> HomeOutcome {
        let latest = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.workspace.archived_at_ms.is_none())
            .max_by_key(|(_, item)| item.workspace.updated_at_ms.unwrap_or(0))
            .map(|(index, _)| index);
        if let Some(index) = latest {
            return self.open_item(index);
        }
        self.set_status("no review to continue yet", StatusSeverity::Info);
        HomeOutcome::Redraw
    }

    fn start_suggestion(&self, index: usize) -> HomeOutcome {
        let Some(suggestion) = self.suggestions.get(index) else {
            return HomeOutcome::Ignored;
        };
        match &suggestion.action {
            SuggestionAction::Start(entry) => HomeOutcome::Start(entry.clone()),
            SuggestionAction::Open(workspace) => HomeOutcome::Open(workspace.clone()),
            SuggestionAction::New => HomeOutcome::New,
        }
    }

    fn suggestions_key(
        &mut self,
        stroke: KeyStroke,
        host: &UiContext,
        theme: &ReviewTheme,
    ) -> HomeOutcome {
        let plain = stroke.modifiers.is_empty();
        let count = self.suggestions.len();
        if count == 0 {
            self.focus = HomeFocus::Table;
            return HomeOutcome::Redraw;
        }
        let current = self.suggestion_state.focused().unwrap_or(0).min(count - 1);
        let next = match stroke.key {
            KeyCode::Down | KeyCode::Char('j') if plain => Some((current + 1).min(count - 1)),
            KeyCode::Up | KeyCode::Char('k') if plain => Some(current.saturating_sub(1)),
            KeyCode::Home if plain => Some(0),
            KeyCode::End if plain => Some(count - 1),
            KeyCode::Enter if plain => return self.start_suggestion(current),
            KeyCode::Escape if plain => {
                self.focus = HomeFocus::Table;
                self.suggestion_state.set_focused(None);
                self.suggestion_state.list.set_selected(None);
                return HomeOutcome::Redraw;
            }
            _ => None,
        };
        if let Some(next) = next {
            self.suggestion_state.set_focused(Some(next));
            self.suggestion_state.list.set_selected(Some(next));
            return HomeOutcome::Redraw;
        }
        match host
            .keymap_action(HOME_SCOPE, stroke)
            .as_deref()
            .map(|action| action.strip_prefix("bcode.code_review.").unwrap_or(action))
        {
            Some("home_focus_next") => {
                self.focus = HomeFocus::Table;
                self.suggestion_state.set_focused(None);
                self.suggestion_state.list.set_selected(None);
                HomeOutcome::Redraw
            }
            _ => self.table_key(stroke, host, theme),
        }
    }

    fn clear_filter(&mut self) {
        *self.filter.borrow_mut() = TextInputState::new(bmux_text_edit::TextEditBuffer::new());
        self.rows_dirty = true;
    }

    fn filter_key(&mut self, stroke: KeyStroke) -> HomeOutcome {
        let plain = stroke.modifiers.is_empty();
        match stroke.key {
            KeyCode::Escape if plain => {
                self.clear_filter();
                self.focus = HomeFocus::Table;
                return HomeOutcome::Redraw;
            }
            KeyCode::Enter | KeyCode::Down | KeyCode::Tab if plain => {
                self.focus = HomeFocus::Table;
                return HomeOutcome::Redraw;
            }
            _ => {}
        }
        let outcome = TextInputControl::new(&self.input_policy)
            .handle_key(&mut self.filter.borrow_mut(), stroke);
        if outcome == TextInputOutcome::Edited {
            self.rows_dirty = true;
            self.selected = None;
        }
        if outcome == TextInputOutcome::Ignored {
            HomeOutcome::Ignored
        } else {
            HomeOutcome::Redraw
        }
    }

    fn start_rename(&mut self) {
        let Some(item) = self.selected_item() else {
            self.set_status("select a review to rename", StatusSeverity::Info);
            return;
        };
        let (id, title) = (item.workspace.id.clone(), item.workspace.title.clone());
        *self.rename.borrow_mut() =
            TextInputState::new(bmux_text_edit::TextEditBuffer::from_text(title));
        self.renaming = Some(id);
        self.focus = HomeFocus::Rename;
    }

    fn rename_key(&mut self, stroke: KeyStroke, host: &UiContext) -> HomeOutcome {
        let plain = stroke.modifiers.is_empty();
        match stroke.key {
            KeyCode::Escape if plain => {
                self.renaming = None;
                self.focus = HomeFocus::Table;
                HomeOutcome::Redraw
            }
            KeyCode::Enter if plain => {
                let title = self.rename.borrow().buffer().text().trim().to_owned();
                if title.is_empty() {
                    self.set_status("a review needs a title", StatusSeverity::Warning);
                    return HomeOutcome::Redraw;
                }
                self.focus = HomeFocus::Table;
                let Some(id) = self.renaming.take() else {
                    return HomeOutcome::Redraw;
                };
                let Some(mut workspace) = self
                    .items
                    .iter()
                    .find(|item| item.workspace.id == id)
                    .map(|item| item.workspace.clone())
                else {
                    return HomeOutcome::Redraw;
                };
                if workspace.title == title {
                    return HomeOutcome::Redraw;
                }
                workspace.title.clone_from(&title);
                // Show the new title at once; the reload confirms it.
                if let Some(item) = self.items.iter_mut().find(|item| item.workspace.id == id) {
                    item.workspace.title.clone_from(&title);
                }
                self.rows_dirty = true;
                self.run_change(host, format!("renamed to “{title}”"), {
                    let services = self.services.clone();
                    async move { load::update(services, workspace).await }
                });
                HomeOutcome::Redraw
            }
            _ => {
                let outcome = TextInputControl::new(&self.input_policy)
                    .handle_key(&mut self.rename.borrow_mut(), stroke);
                if outcome == TextInputOutcome::Ignored {
                    HomeOutcome::Ignored
                } else {
                    HomeOutcome::Redraw
                }
            }
        }
    }

    /// Run one rename, archive, or restore (one at a time).
    fn run_change(
        &mut self,
        host: &UiContext,
        done: String,
        task: impl std::future::Future<Output = Result<(), String>> + Send + 'static,
    ) {
        self.busy = Some(done.clone());
        self.spawn(host, async move {
            HomeMessage::Changed {
                done,
                result: task.await,
            }
        });
    }

    fn archive(&mut self, host: &UiContext) {
        if let Some(busy) = &self.busy {
            let text = format!("still working: {busy}");
            self.set_status(text, StatusSeverity::Warning);
            return;
        }
        let Some(item) = self.selected_item() else {
            self.set_status("select a review to archive", StatusSeverity::Info);
            return;
        };
        if item.workspace.archived_at_ms.is_some() {
            self.set_status("the review is already archived", StatusSeverity::Info);
            return;
        }
        let (id, title) = (item.workspace.id.clone(), item.workspace.title.clone());
        let (services, repo) = (self.services.clone(), self.repo.clone());
        self.run_change(host, format!("archived “{title}”"), async move {
            load::archive(services, repo, id).await
        });
    }

    fn restore(&mut self, host: &UiContext) {
        if let Some(busy) = &self.busy {
            let text = format!("still working: {busy}");
            self.set_status(text, StatusSeverity::Warning);
            return;
        }
        let Some(item) = self.selected_item() else {
            self.set_status("select a review to restore", StatusSeverity::Info);
            return;
        };
        if item.workspace.archived_at_ms.is_none() {
            self.set_status("the selected review is not archived", StatusSeverity::Info);
            return;
        }
        let mut workspace = item.workspace.clone();
        workspace.archived_at_ms = None;
        let title = workspace.title.clone();
        let services = self.services.clone();
        self.run_change(host, format!("restored “{title}”"), async move {
            load::update(services, workspace).await
        });
    }

    /// Insert pasted text into the focused input.
    pub fn paste(&mut self, text: &str) -> HomeOutcome {
        let state = match self.focus {
            HomeFocus::Filter => &self.filter,
            HomeFocus::Rename => &self.rename,
            HomeFocus::Table | HomeFocus::Suggestions => return HomeOutcome::Ignored,
        };
        TextInputControl::new(&self.input_policy).handle_paste(&mut state.borrow_mut(), text);
        if self.focus == HomeFocus::Filter {
            self.rows_dirty = true;
            self.selected = None;
        }
        HomeOutcome::Redraw
    }

    /// Route a pointer event through the regions painted last.
    pub fn mouse(&mut self, event: &Event, theme: &ReviewTheme) -> HomeOutcome {
        let Event::Mouse(mouse) = event else {
            return HomeOutcome::Ignored;
        };
        if let Some(overlay) = self.overlay.as_mut() {
            return match overlay.mouse(event, theme) {
                OverlayOutcome::Close => {
                    self.overlay = None;
                    HomeOutcome::Redraw
                }
                OverlayOutcome::Redraw | OverlayOutcome::Run(_) => HomeOutcome::Redraw,
                OverlayOutcome::Ignored => HomeOutcome::Ignored,
            };
        }
        let at = |rect: Option<Rect>| rect.filter(|rect| rect.contains(mouse.position));
        if let Some(area) = at(self.regions.suggestions) {
            let menu = Menu::new(&self.suggestion_items)
                .policy(MenuPolicy::interactive())
                .styles(theme.component.selectable_list_styles());
            return match menu.handle_event(area, &mut self.suggestion_state, event) {
                MenuOutcome::Activated { index, .. } => self.start_suggestion(index),
                MenuOutcome::Ignored => HomeOutcome::Ignored,
                _ => HomeOutcome::Redraw,
            };
        }
        if at(self.regions.filter).is_some()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            self.focus = HomeFocus::Filter;
            return HomeOutcome::Redraw;
        }
        if let Some(area) = at(self.regions.table) {
            let before = self.selected_index();
            let columns: Vec<TableColumn<'static>> = self
                .columns
                .iter()
                .map(|column| table_column(*column))
                .collect();
            let table = Table::new(&columns, &self.table_rows).policy(TablePolicy::interactive());
            let outcome = table.handle_event(area, &mut self.table.borrow_mut(), event);
            let after = self.table.borrow().selected();
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.focus = HomeFocus::Table;
                self.suggestion_state.set_focused(None);
                self.suggestion_state.list.set_selected(None);
                if let Some(index) = after {
                    if after == before {
                        // A press on the selected row opens it (or toggles its group).
                        return self.open_selected();
                    }
                    self.select(index);
                }
                return HomeOutcome::Redraw;
            }
            return if outcome == TableOutcome::Ignored {
                HomeOutcome::Ignored
            } else {
                HomeOutcome::Redraw
            };
        }
        HomeOutcome::Ignored
    }

    // ----------------------------------------------------------------------------------------
    // Rendering
    // ----------------------------------------------------------------------------------------

    /// Paint home over `area`.
    pub fn render(&mut self, area: Rect, cx: &mut PaintCx<'_, '_>, theme: &ReviewTheme) {
        self.regions = Regions::default();
        cx.fill(
            LocalRect::new(area.x.into(), area.y.into(), area.width, area.height),
            " ",
            theme.canvas,
        );
        if area.width == 0 || area.height == 0 {
            return;
        }
        let header = area.height >= 3;
        let footer = area.height >= 2;
        let body = Rect::new(
            area.x,
            area.y + u16::from(header),
            area.width,
            area.height - u16::from(header) - u16::from(footer),
        );
        let preview = self.preview && body.width >= PREVIEW_MIN_WIDTH;
        let preview_width = if preview {
            (body.width * 7 / 20).clamp(32, 52)
        } else {
            0
        };
        let main = Rect::new(body.x, body.y, body.width - preview_width, body.height);
        self.rebuild_rows(theme, main.width);
        if header {
            self.paint_header(Rect::new(area.x, area.y, area.width, 1), cx, theme);
        }
        self.paint_main(main, cx, theme);
        if preview && body.height > 0 {
            let rect = Rect::new(main.right(), body.y, preview_width, body.height);
            self.paint_preview(rect, cx, theme);
            self.regions.preview = Some(rect);
        }
        if footer {
            self.paint_footer(
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
                cx,
                theme,
            );
        }
        self.toasts.paint(theme, body, cx);
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.paint(theme, area, cx);
        }
    }

    fn paint_header(&self, area: Rect, cx: &mut PaintCx<'_, '_>, theme: &ReviewTheme) {
        cx.fill(
            LocalRect::new(area.x.into(), area.y.into(), area.width, 1),
            " ",
            theme.header_bar,
        );
        let repo = self.repo.file_name().map_or_else(
            || self.repo.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let mut counts: BTreeMap<Group, usize> = BTreeMap::new();
        for item in &self.items {
            *counts
                .entry(model::group_of(item, self.stats.get(&item.workspace.id)))
                .or_default() += 1;
        }
        let right = if self.loading() > 0 || !self.loaded {
            let spinner = if theme.icons.ellipsis.is_ascii() {
                Spinner::ascii()
            } else {
                Spinner::braille()
            };
            Line::from_spans(vec![
                Span::styled(
                    spinner.frame(self.started.elapsed()).to_owned(),
                    theme.accent,
                ),
                Span::styled(" loading ", theme.muted),
            ])
        } else {
            let mut spans = Vec::new();
            for (group, style) in [
                (Group::NeedsAttention, theme.notice),
                (Group::InProgress, theme.accent),
                (Group::Published, theme.badge_resolved),
            ] {
                if let Some(count) = counts.get(&group).filter(|count| **count > 0) {
                    if !spans.is_empty() {
                        spans.push(Span::styled(theme.icons.separator, theme.muted));
                    }
                    spans.push(Span::styled(count.to_string(), style));
                    spans.push(Span::styled(
                        format!(" {}", group.label().to_lowercase()),
                        theme.muted,
                    ));
                }
            }
            spans.push(Span::raw(" "));
            Line::from_spans(spans)
        };
        let right = right.truncate(usize::from(area.width / 2));
        let right_width = u16::try_from(right.width()).unwrap_or(0);
        let left = Line::from_spans(vec![
            Span::raw(" "),
            Span::styled("Code reviews", theme.title),
            Span::styled(theme.icons.separator, theme.muted),
            Span::styled(repo, theme.muted),
        ])
        .truncate(usize::from(area.width.saturating_sub(right_width)));
        cx.write_line(
            LocalRect::new(area.x.into(), area.y.into(), area.width - right_width, 1),
            &left,
        );
        if right_width > 0 {
            cx.write_line(
                LocalRect::new(
                    i32::from(area.right() - right_width),
                    area.y.into(),
                    right_width,
                    1,
                ),
                &right,
            );
        }
    }

    fn paint_main(&mut self, area: Rect, cx: &mut PaintCx<'_, '_>, theme: &ReviewTheme) {
        let mut y = area.y;
        let bottom = area.bottom();
        // Suggestions: a label row and one row per suggestion, when the body has room.
        if area.height >= 10 && !self.suggestions.is_empty() {
            let rows = u16::try_from(self.suggestions.len())
                .unwrap_or(u16::MAX)
                .min(area.height / 3);
            let label = Line::from_spans(vec![
                Span::raw(" "),
                Span::styled("Start a review", theme.title),
                Span::styled("  1–9 or tab", theme.muted),
            ])
            .truncate(usize::from(area.width));
            cx.write_line(
                LocalRect::new(area.x.into(), y.into(), area.width, 1),
                &label,
            );
            y += 1;
            let rect = Rect::new(area.x + 1, y, area.width.saturating_sub(2), rows);
            Menu::new(&self.suggestion_items)
                .policy(MenuPolicy::interactive())
                .styles(theme.component.selectable_list_styles())
                .paint(rect, &self.suggestion_state, theme.text, cx);
            self.regions.suggestions = Some(rect);
            y += rows + 1;
        }
        // The filter row, when the table keeps at least three rows.
        if bottom.saturating_sub(y) >= 4 {
            let rect = Rect::new(area.x, y, area.width, 1);
            self.paint_filter(rect, cx, theme);
            self.regions.filter = Some(rect);
            y += 1;
        }
        let table_area = Rect::new(area.x, y, area.width, bottom.saturating_sub(y));
        if table_area.height == 0 {
            return;
        }
        self.regions.table = Some(table_area);
        self.paint_table(table_area, cx, theme);
    }

    fn paint_filter(&self, area: Rect, cx: &mut PaintCx<'_, '_>, theme: &ReviewTheme) {
        let total = self.items.len();
        let shown = self
            .rows
            .iter()
            .filter(|row| matches!(row, RowRef::Review(_)))
            .count();
        let count = Line::from_spans(vec![Span::styled(
            if self.filter_text().is_empty() {
                format!("{total} review{} ", if total == 1 { "" } else { "s" })
            } else {
                format!("{shown} of {total} ")
            },
            theme.muted,
        )])
        .truncate(usize::from(area.width / 3));
        let count_width = u16::try_from(count.width()).unwrap_or(0);
        let prompt = Line::from_spans(vec![
            Span::raw(" "),
            Span::styled(theme.icons.filter, theme.accent),
            Span::raw(" "),
        ]);
        let prompt_width = u16::try_from(prompt.width()).unwrap_or(0).min(area.width);
        cx.write_line(
            LocalRect::new(area.x.into(), area.y.into(), prompt_width, 1),
            &prompt,
        );
        if count_width > 0 {
            cx.write_line(
                LocalRect::new(
                    i32::from(area.right() - count_width),
                    area.y.into(),
                    count_width,
                    1,
                ),
                &count,
            );
        }
        let input_width = area.width.saturating_sub(prompt_width + count_width + 1);
        if input_width == 0 {
            return;
        }
        let placeholder = self.action_keys.get("home_filter").map_or_else(
            || "filter reviews".to_owned(),
            |key| format!("filter reviews ({key})"),
        );
        let input = TextInputComponent::new("home.filter", &self.filter, &self.input_policy)
            .style(theme.input)
            .placeholder(&placeholder, theme.placeholder)
            .focused(self.focus == HomeFocus::Filter);
        let node = input.layout(
            Constraints::tight(Size::new(input_width, 1)),
            &mut LayoutCx::new(),
        );
        cx.with_child(
            i32::from(area.x + prompt_width),
            i64::from(area.y),
            LocalRect::new(0, 0, input_width, 1),
            |cx| input.paint(&node, cx),
        );
    }

    fn paint_table(&self, area: Rect, cx: &mut PaintCx<'_, '_>, theme: &ReviewTheme) {
        if let Some(error) = &self.list_error {
            let body = [Line::from_spans(vec![Span::styled(
                error.clone(),
                theme.error,
            )])];
            let empty = EmptyStateComponent::new("home.error", "Could not list reviews")
                .icon("!")
                .body(&body)
                .policy(EmptyStatePolicy::centered())
                .styles(theme.component.empty_state_styles());
            cx.paint_component(&empty, area);
            self.visible.borrow_mut().clear();
            return;
        }
        if self.loaded && self.rows.is_empty() {
            let filtered = !self.filter_text().is_empty();
            let (title, text) = if filtered {
                (
                    "No matching reviews",
                    "Every word must match a title, source, or id.".to_owned(),
                )
            } else {
                let new = self
                    .action_keys
                    .get("home_new")
                    .cloned()
                    .unwrap_or_default();
                (
                    "Your next review starts here",
                    format!("Pick a suggestion above or press {new} for a new review."),
                )
            };
            let body = [Line::from_spans(vec![Span::styled(text, theme.muted)])];
            let empty = EmptyStateComponent::new("home.empty", title)
                .icon(theme.icons.draft)
                .body(&body)
                .policy(EmptyStatePolicy::centered())
                .styles(theme.component.empty_state_styles());
            cx.paint_component(&empty, area);
            self.visible.borrow_mut().clear();
            return;
        }
        let columns: Vec<TableColumn<'static>> = self
            .columns
            .iter()
            .map(|column| table_column(*column))
            .collect();
        let mut styles = theme.component.table_styles();
        if self.focus != HomeFocus::Table {
            styles.selected = theme.cursor;
        }
        let table = Table::new(&columns, &self.table_rows)
            .policy(TablePolicy::interactive())
            .styles(styles)
            .focus_style(Style::new())
            .empty(if self.loaded {
                "No reviews"
            } else {
                "Loading reviews…"
            });
        let mut state = self.table.borrow_mut();
        table.reconcile(area, &mut state);
        table.paint(area, &state, cx);
        // Rows on screen get their stats first.
        let first = usize::try_from(state.scroll()).unwrap_or(usize::MAX);
        let shown = usize::from(area.height.saturating_sub(1));
        *self.visible.borrow_mut() = self
            .rows
            .iter()
            .skip(first)
            .take(shown)
            .filter_map(|row| match row {
                RowRef::Review(index) => Some(self.items[*index].workspace.id.clone()),
                RowRef::Group { .. } => None,
            })
            .collect();
    }

    fn preview_lines(&self, theme: &ReviewTheme) -> Vec<Line> {
        // Fixed labels, padded to one column.
        let label = |text: &'static str| Span::styled(text, theme.muted);
        let Some(index) = self.selected_index() else {
            return vec![Line::from_spans(vec![Span::styled(
                "Select a review to see its details.",
                theme.muted,
            )])];
        };
        let item = match &self.rows[index] {
            RowRef::Group { group, count, .. } => return group_preview(theme, *group, *count),
            RowRef::Review(index) => &self.items[*index],
        };
        let workspace = &item.workspace;
        let stats = self.stats.get(&workspace.id);
        let now = now_ms();
        let group = model::group_of(item, stats);
        let mut lines = vec![
            Line::from_spans(vec![Span::styled(workspace.title.clone(), theme.title)]),
            Line::from_spans(vec![
                Span::styled(group.label(), theme.accent),
                Span::styled(
                    workspace.updated_at_ms.map_or_else(String::new, |updated| {
                        format!(
                            "{}updated {}",
                            theme.icons.separator,
                            model::relative_time(now, updated)
                        )
                    }),
                    theme.muted,
                ),
            ]),
            Line::from(""),
            Line::from_spans(vec![Span::styled("Sources", theme.title)]),
        ];
        lines.extend(source_lines(theme, workspace));
        lines.push(Line::from(""));
        let files = files_preview(theme, stats);
        lines.push(Line::from_spans(
            std::iter::once(label("Files   "))
                .chain(files)
                .collect::<Vec<_>>(),
        ));
        let viewed = workspace.viewed_files.len();
        let mut viewed_spans = vec![label("Viewed  ")];
        viewed_spans.extend(viewed_line(theme, workspace, stats).spans);
        if viewed == 0 {
            viewed_spans = vec![label("Viewed  "), Span::styled("nothing yet", theme.muted)];
        }
        lines.push(Line::from_spans(viewed_spans));
        lines.push(Line::from_spans(vec![
            label("Notes   "),
            Span::styled(
                format!(
                    "{}{}{}",
                    plural(
                        u64::try_from(item.draft_count).unwrap_or(u64::MAX),
                        "draft",
                        "drafts"
                    ),
                    theme.icons.separator,
                    plural(
                        u64::try_from(item.thread_count).unwrap_or(u64::MAX),
                        "thread",
                        "threads"
                    )
                ),
                theme.text,
            ),
        ]));
        let publish = publish_preview(theme, item, now);
        lines.push(Line::from_spans(
            std::iter::once(label("Publish "))
                .chain(publish)
                .collect::<Vec<_>>(),
        ));
        if let Some(publish) = &item.last_publish
            && !publish.message.is_empty()
        {
            lines.push(Line::from_spans(vec![
                label("        "),
                Span::styled(publish.message.clone(), theme.muted),
            ]));
        }
        if stats.is_some_and(|stats| stats.new_commits) {
            lines.push(Line::from(""));
            lines.push(Line::from_spans(vec![Span::styled(
                "New commits since your last pass.",
                theme.notice,
            )]));
        }
        if workspace.archived_at_ms.is_some() {
            lines.push(Line::from(""));
            lines.push(Line::from_spans(vec![Span::styled(
                "Archived. Restore it to open it again.",
                theme.muted,
            )]));
        }
        lines
    }

    fn paint_preview(&self, area: Rect, cx: &mut PaintCx<'_, '_>, theme: &ReviewTheme) {
        // One cell of inner margin on the left.
        let lines: Vec<Line> = self
            .preview_lines(theme)
            .into_iter()
            .map(|line| {
                let mut spans = vec![Span::raw(" ")];
                spans.extend(line.spans);
                Line::from_spans(spans)
            })
            .collect();
        let pane = Pane::new()
            .title(Line::from_spans(vec![Span::styled(
                " Preview ",
                theme.title,
            )]))
            .styles(PaneStyles {
                background: None,
                border: theme.border,
                focused_border: theme.border_focused,
            })
            .chrome(theme.component.chrome);
        let component = PaneComponent::new(
            "home.preview",
            pane,
            &self.preview_pane,
            TextBlock::new(Text::from_lines(lines)),
        );
        cx.paint_component(&component, area);
    }

    fn hints(&self) -> Vec<(String, String)> {
        let key = |action: &str| self.action_keys.get(action).cloned();
        let mut hints = Vec::new();
        let mut push = |key: Option<String>, label: &str| {
            if let Some(key) = key {
                hints.push((key, label.to_owned()));
            }
        };
        match self.focus {
            HomeFocus::Filter => {
                push(Some("enter".to_owned()), "done");
                push(Some("esc".to_owned()), "clear");
            }
            HomeFocus::Rename => {
                push(Some("enter".to_owned()), "save");
                push(Some("esc".to_owned()), "cancel");
            }
            HomeFocus::Suggestions => {
                push(Some("enter".to_owned()), "start");
                push(Some("↑↓".to_owned()), "move");
                push(key("home_focus_next"), "reviews");
                push(key("home_new"), "new");
                push(key("home_exit"), "quit");
            }
            HomeFocus::Table => {
                let on_group = matches!(
                    self.selected_index().and_then(|index| self.rows.get(index)),
                    Some(RowRef::Group { .. })
                );
                let archived = self
                    .selected_item()
                    .is_some_and(|item| item.workspace.archived_at_ms.is_some());
                if on_group {
                    push(key("home_open"), "fold");
                } else if archived {
                    push(key("home_restore"), "restore");
                } else {
                    push(key("home_open"), "open");
                }
                push(key("home_new"), "new");
                push(key("home_continue"), "continue");
                push(key("home_filter"), "filter");
                if !on_group {
                    push(key("home_rename"), "rename");
                    if !archived {
                        push(key("home_archive"), "archive");
                    }
                }
                push(key("home_focus_next"), "suggestions");
                push(key("home_preview"), "preview");
                push(key("home_help"), "help");
                push(key("home_exit"), "quit");
            }
        }
        hints
    }

    fn paint_footer(&self, area: Rect, cx: &mut PaintCx<'_, '_>, theme: &ReviewTheme) {
        if self.focus == HomeFocus::Rename {
            cx.fill(
                LocalRect::new(area.x.into(), area.y.into(), area.width, 1),
                " ",
                theme.footer,
            );
            let prompt = Line::from_spans(vec![Span::styled(" rename: ", theme.accent)])
                .truncate(usize::from(area.width));
            let prompt_width = u16::try_from(prompt.width()).unwrap_or(0);
            cx.write_line(
                LocalRect::new(area.x.into(), area.y.into(), prompt_width, 1),
                &prompt,
            );
            let width = area.width.saturating_sub(prompt_width + 1);
            if width > 0 {
                let input =
                    TextInputComponent::new("home.rename", &self.rename, &self.input_policy)
                        .style(theme.input)
                        .placeholder("review title", theme.placeholder)
                        .focused(true);
                let node = input.layout(
                    Constraints::tight(Size::new(width, 1)),
                    &mut LayoutCx::new(),
                );
                cx.with_child(
                    i32::from(area.x + prompt_width),
                    i64::from(area.y),
                    LocalRect::new(0, 0, width, 1),
                    |cx| input.paint(&node, cx),
                );
            }
            return;
        }
        let status = self.status.clone().or_else(|| {
            self.pulls_error.as_ref().map(|error| Status {
                text: format!("pull requests unavailable: {error}"),
                severity: StatusSeverity::Warning,
            })
        });
        chrome::paint_footer(&self.hints(), status.as_ref(), theme, area, cx);
    }
}

/// Source rows of the preview: included sources marked, excluded ones muted.
fn source_lines(theme: &ReviewTheme, workspace: &ReviewWorkspace) -> Vec<Line> {
    let mut lines = Vec::new();
    if workspace.sources.is_empty() {
        lines.push(Line::from_spans(vec![Span::styled(
            "  none yet",
            theme.muted,
        )]));
    }
    for source in &workspace.sources {
        lines.push(Line::from_spans(vec![
            Span::styled(
                format!(
                    "  {} ",
                    if source.included {
                        theme.icons.viewed
                    } else {
                        theme.icons.unviewed
                    }
                ),
                if source.included {
                    theme.accent
                } else {
                    theme.muted
                },
            ),
            Span::styled(
                source.label.clone(),
                if source.included {
                    theme.text
                } else {
                    theme.muted
                },
            ),
        ]));
    }
    lines
}

/// File totals of the preview.
fn files_preview(theme: &ReviewTheme, stats: Option<&WorkspaceStats>) -> Vec<Span> {
    stats.map_or_else(
        || vec![Span::styled("counting…", theme.muted)],
        |stats| {
            let mut spans = vec![
                Span::styled(
                    format!(
                        "{} file{} ",
                        stats.files,
                        if stats.files == 1 { "" } else { "s" }
                    ),
                    theme.text,
                ),
                Span::styled(format!("+{}", stats.additions), theme.additions),
                Span::raw(" "),
                Span::styled(
                    format!("{}{}", theme.icons.minus, stats.deletions),
                    theme.deletions,
                ),
            ];
            if stats.unresolved > 0 {
                spans.push(Span::styled(
                    format!(
                        "{}{} counted when opened",
                        theme.icons.separator, stats.unresolved
                    ),
                    theme.muted,
                ));
            }
            spans
        },
    )
}

/// Publish state spans of the preview.
fn publish_preview(theme: &ReviewTheme, item: &ReviewWorkspaceListItem, now: u64) -> Vec<Span> {
    item.last_publish.as_ref().map_or_else(
        || vec![Span::styled("never published", theme.muted)],
        |publish| {
            let mut spans = vec![
                Span::styled(
                    if publish.submitted {
                        "published "
                    } else {
                        "previewed "
                    },
                    theme.badge_resolved,
                ),
                Span::styled(
                    format!(
                        "{} via {}",
                        model::relative_time(now, publish.created_at_ms),
                        publish.publisher_id
                    ),
                    theme.muted,
                ),
            ];
            if model::changed_since_publish(item) {
                spans.push(Span::styled(
                    format!("{}changed since", theme.icons.separator),
                    theme.notice,
                ));
            }
            spans
        },
    )
}

/// Preview of a group heading: what the group means.
fn group_preview(theme: &ReviewTheme, group: Group, count: usize) -> Vec<Line> {
    let about = match group {
        Group::NeedsAttention => {
            "Reviews without sources, with drafts changed since their last publish, or with new commits since your last pass."
        }
        Group::InProgress => "Reviews being worked on, drafts not published yet.",
        Group::Published => "Reviews published with nothing new since.",
        Group::Archived => "Archived reviews. Restore one to open it again.",
    };
    vec![
        Line::from_spans(vec![Span::styled(group.label(), theme.title)]),
        Line::from_spans(vec![Span::styled(
            format!("{count} review{}", if count == 1 { "" } else { "s" }),
            theme.muted,
        )]),
        Line::from(""),
        Line::from_spans(vec![Span::styled(about, theme.text)]),
    ]
}

fn digit(stroke: KeyStroke) -> Option<usize> {
    match stroke.key {
        KeyCode::Char(character) if stroke.modifiers.is_empty() => character
            .to_digit(10)
            .filter(|digit| *digit > 0)
            .and_then(|digit| usize::try_from(digit).ok()),
        _ => None,
    }
}

fn suggestion_item(theme: &ReviewTheme, index: usize, suggestion: &Suggestion) -> MenuItem {
    let style = match suggestion.kind {
        SuggestionKind::Uncommitted => theme.status_modified,
        SuggestionKind::Branch => theme.accent,
        SuggestionKind::Stack => theme.status_renamed,
        SuggestionKind::ReviewRequested => theme.badge_open,
        SuggestionKind::NewCommits => theme.notice,
        SuggestionKind::New => theme.text,
    };
    let key = if index < 9 {
        format!("{} ", index + 1)
    } else {
        "  ".to_owned()
    };
    MenuItem::rich(
        suggestion.id.clone(),
        Line::from_spans(vec![
            Span::styled(key, theme.muted),
            Span::styled(suggestion.title.clone(), style.patch(theme.title)),
            Span::styled(format!("  {}", suggestion.detail), theme.muted),
        ]),
    )
}

fn sources_line(theme: &ReviewTheme, workspace: &ReviewWorkspace) -> Line {
    let mut spans = Vec::new();
    for (index, source) in workspace
        .sources
        .iter()
        .filter(|source| source.included)
        .enumerate()
    {
        if index > 0 {
            spans.push(Span::styled(theme.icons.separator, theme.muted));
        }
        spans.push(Span::styled(source.label.clone(), theme.accent));
    }
    if spans.is_empty() {
        spans.push(Span::styled("no sources", theme.notice));
    }
    Line::from_spans(spans)
}

/// Viewed progress: five progress cells and `viewed/total`, or the viewed count until the total
/// is known.
fn viewed_line(
    theme: &ReviewTheme,
    workspace: &ReviewWorkspace,
    stats: Option<&WorkspaceStats>,
) -> Line {
    let viewed = workspace.viewed_files.len() as u64;
    let Some(total) = stats.map(|stats| stats.files).filter(|total| *total > 0) else {
        return Line::from_spans(vec![Span::styled(
            if viewed == 0 {
                String::new()
            } else {
                format!("{} {viewed}", theme.icons.viewed)
            },
            theme.viewed,
        )]);
    };
    let viewed = viewed.min(total);
    let filled = usize::try_from((viewed * 5).div_ceil(total).min(5)).unwrap_or(5);
    let filled = if viewed == 0 { 0 } else { filled.max(1) };
    let done = viewed == total;
    Line::from_spans(vec![
        Span::styled(
            theme.icons.progress_filled.repeat(filled),
            if done { theme.viewed } else { theme.accent },
        ),
        Span::styled(
            theme.icons.progress_empty.repeat(5 - filled),
            theme.bar_empty,
        ),
        Span::styled(format!(" {viewed}/{total}"), theme.muted),
    ])
}

fn notes_line(theme: &ReviewTheme, item: &ReviewWorkspaceListItem) -> Line {
    let mut spans = Vec::new();
    if item.draft_count > 0 {
        spans.push(Span::styled(
            format!("{}{}", theme.icons.draft, item.draft_count),
            theme.badge_draft,
        ));
    }
    if item.thread_count > 0 {
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!("{}{}", theme.icons.thread, item.thread_count),
            theme.badge_open,
        ));
    }
    Line::from_spans(spans)
}

fn publish_line(theme: &ReviewTheme, item: &ReviewWorkspaceListItem, now: u64) -> Line {
    match &item.last_publish {
        None => Line::from_spans(vec![Span::styled("—", theme.muted)]),
        Some(_) if model::changed_since_publish(item) => {
            Line::from_spans(vec![Span::styled("changed", theme.notice)])
        }
        Some(publish) => Line::from_spans(vec![
            Span::styled("published ", theme.badge_resolved),
            Span::styled(
                model::relative_time(now, publish.created_at_ms)
                    .trim_end_matches(" ago")
                    .to_owned(),
                theme.muted,
            ),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::Home;
    use super::load::HomeMessage;
    use crate::testing::backend::TestBackend;
    use crate::testing::harness::ScriptedHost;
    use bcode_code_review_models::{ReviewTarget, ReviewWorkspace, ReviewWorkspaceListItem};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn item(id: &str) -> ReviewWorkspaceListItem {
        ReviewWorkspaceListItem {
            workspace: ReviewWorkspace {
                id: id.to_owned(),
                ..ReviewWorkspace::from_target(PathBuf::from("/repo"), ReviewTarget::LastCommit)
            },
            thread_count: 0,
            draft_count: 0,
            last_publish: None,
        }
    }

    #[test]
    fn answers_of_an_older_refresh_never_replace_a_newer_one() {
        let host = Arc::new(ScriptedHost::new()).context();
        let mut home = Home::new(TestBackend::default().services(), PathBuf::from("/repo"));
        // Two refreshes start before either answers (focus regained while loading).
        home.poll(&host);
        home.refresh();
        home.poll(&host);
        // The newer listing answers first; the older one arrives late.
        home.inbox
            .push(2, HomeMessage::Listed(Ok(vec![item("new"), item("other")])));
        home.inbox
            .push(1, HomeMessage::Listed(Ok(vec![item("old")])));
        home.poll(&host);
        let ids: Vec<&str> = home
            .items()
            .iter()
            .map(|item| item.workspace.id.as_str())
            .collect();
        assert_eq!(ids, ["new", "other"]);
    }
}
