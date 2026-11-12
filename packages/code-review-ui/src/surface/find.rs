//! Search, viewed marks, copying, the threads panel filter, and presentation persistence.
//!
//! * **Search** (`/`) is incremental over resident content (hunk lines and revealed context):
//!   every edit re-runs the document search and moves to the first match at or after the
//!   cursor. Files with changes whose content is not resident are reported as pending, and the
//!   search re-runs as content loads. `n`/`N` move between matches while a search is active.
//! * **Viewed** (`v`) records an identity-pinned mark for the cursor's file (both workspace
//!   fields, so home and older clients keep working) and moves to the next unviewed file; on a viewed file
//!   it removes the mark.
//! * **Presentation** is the scroll anchor by item identity, the layout toggle, the
//!   panels, and the revealed context. It is compared with the stored state at most once a
//!   second and saved when it changed, and always on exit; it is restored once when the review
//!   first opens.

use super::annotate::now_ms;
use super::{Focus, ReviewSurface, Selection};
use crate::action::Action;
use crate::context::UiContext;
use crate::rows::{RowKey, SearchHighlights};
use bcode_code_review::document::{
    Direction, ItemKey, LayoutMode, NavKind, ReviewItem, SearchQuery, SearchResults,
};
use bcode_code_review_models::{
    DraftComment, ReviewAiExchange, ReviewAiExchangeStatus, ReviewAnchorKind, ReviewLayoutMode,
    ReviewPresentationState, ReviewScrollAnchor, ReviewThreadKind, ReviewThreadSeverity,
};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui_components::status_bar::StatusSeverity;
use bmux_tui_components::text_input::{
    EdgePolicy, TextInputControl, TextInputOutcome, TextInputPolicy, TextInputState,
};
use bmux_tui_components::virtual_list::{AnchorPolicy, ListAnchor, ListPosition};
use std::cell::RefCell;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// How often presentation state is compared with the stored state.
const PRESENTATION_INTERVAL: Duration = Duration::from_secs(1);

/// Which threads the threads panel lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadsFilter {
    /// Every thread and AI answer.
    All,
    /// Unresolved threads and AI answers.
    Open,
    /// Unresolved threads that need a decision (warnings, blockers, questions, to-dos) and
    /// failed AI questions.
    Attention,
    /// Review-level threads.
    General,
    /// Resolved threads.
    Resolved,
}

impl ThreadsFilter {
    /// Next filter, cycling.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::All => Self::Open,
            Self::Open => Self::Attention,
            Self::Attention => Self::General,
            Self::General => Self::Resolved,
            Self::Resolved => Self::All,
        }
    }

    /// Previous filter, cycling.
    #[must_use]
    pub const fn previous(self) -> Self {
        match self {
            Self::All => Self::Resolved,
            Self::Resolved => Self::General,
            Self::Open => Self::All,
            Self::Attention => Self::Open,
            Self::General => Self::Attention,
        }
    }

    /// Display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Open => "open",
            Self::Attention => "needs attention",
            Self::General => "general",
            Self::Resolved => "resolved",
        }
    }

    /// Whether a thread (by its first comment) is listed.
    #[must_use]
    pub fn matches_thread(self, first: &DraftComment) -> bool {
        let open = first.resolved_at_ms.is_none();
        match self {
            Self::All => true,
            Self::Open => open,
            Self::Attention => {
                open && (matches!(
                    first.severity,
                    ReviewThreadSeverity::Warning | ReviewThreadSeverity::Blocker
                ) || matches!(
                    first.thread_kind,
                    ReviewThreadKind::Question | ReviewThreadKind::Todo
                ))
            }
            Self::General => first.anchor.kind == ReviewAnchorKind::Review,
            Self::Resolved => !open,
        }
    }

    /// Whether an AI exchange is listed.
    #[must_use]
    pub fn matches_exchange(self, exchange: &ReviewAiExchange) -> bool {
        match self {
            Self::All | Self::Open => true,
            Self::Attention => exchange.status == ReviewAiExchangeStatus::Failed,
            Self::General => exchange.anchor.kind == ReviewAnchorKind::Review,
            Self::Resolved => false,
        }
    }
}

/// An active search.
#[derive(Debug)]
pub struct SearchState {
    /// Query input.
    pub input: RefCell<TextInputState>,
    /// Editing policy of the input.
    pub policy: TextInputPolicy,
    /// Results of the last run.
    pub results: SearchResults,
    /// Matches to highlight.
    pub highlights: SearchHighlights,
    /// Document revision the last run saw.
    doc_revision: u64,
    /// Ordinal of the current match (1-based) and match count.
    pub position: Option<(usize, usize)>,
}

impl SearchState {
    fn new() -> Self {
        let mut policy = TextInputPolicy::chat_composer();
        policy.keyboard.shift_enter = None;
        policy.viewport.max_rows = Some(1);
        policy.edge = EdgePolicy::disabled();
        Self {
            input: RefCell::new(TextInputState::new(bmux_text_edit::TextEditBuffer::new())),
            policy,
            results: SearchResults::default(),
            highlights: SearchHighlights::default(),
            doc_revision: u64::MAX,
            position: None,
        }
    }

    /// The query text.
    #[must_use]
    pub fn query(&self) -> String {
        self.input.borrow().buffer().text().to_string()
    }

    /// Status text: match position and pending files.
    #[must_use]
    pub fn status(&self) -> String {
        let query = self.query();
        if query.is_empty() {
            return String::new();
        }
        let mut text = match self.position {
            Some((current, total)) => format!("{current}/{total}"),
            None if self.results.matches.is_empty() => "no matches".to_owned(),
            None => format!("{} matches", self.results.matches.len()),
        };
        let pending = self.results.unsearched.len();
        if pending > 0 {
            let _ = write!(
                text,
                " · {pending} unloaded file{} pending",
                if pending == 1 { "" } else { "s" }
            );
        }
        text
    }
}

impl ReviewSurface {
    // ----------------------------------------------------------------------------------------
    // Search
    // ----------------------------------------------------------------------------------------

    /// Start (or resume editing) a search.
    pub(super) fn start_search(&mut self) {
        if self.search.is_none() {
            self.search = Some(SearchState::new());
        }
        self.focus = Focus::Search;
    }

    /// Clear the search and its highlights.
    pub(super) fn clear_search(&mut self) {
        self.search = None;
        if self.focus == Focus::Search {
            self.focus = Focus::Review;
        }
    }

    /// Keys while the search input has focus.
    pub(super) fn search_key(&mut self, stroke: KeyStroke) -> Action {
        let plain = stroke.modifiers.is_empty();
        match stroke.key {
            KeyCode::Escape if plain => {
                self.clear_search();
                return Action::Redraw;
            }
            KeyCode::Enter if plain => {
                self.focus = Focus::Review;
                return Action::Redraw;
            }
            _ => {}
        }
        let Some(search) = self.search.as_mut() else {
            self.focus = Focus::Review;
            return Action::Redraw;
        };
        let outcome = TextInputControl::new(&search.policy)
            .handle_key(&mut search.input.borrow_mut(), stroke);
        if outcome == TextInputOutcome::Edited {
            self.run_search(true);
        }
        Action::Redraw
    }

    /// Insert pasted text into the search.
    pub(super) fn search_paste(&mut self, text: &str) {
        if let Some(search) = self.search.as_mut() {
            TextInputControl::new(&search.policy)
                .handle_paste(&mut search.input.borrow_mut(), text);
            self.run_search(true);
        }
    }

    /// Run the search; with `jump`, move to the first match at or after the cursor.
    pub(super) fn run_search(&mut self, jump: bool) {
        let (Some(search), Some(review)) = (self.search.as_mut(), self.review.as_mut()) else {
            return;
        };
        let query = search.query();
        search.doc_revision = review.doc.revision();
        let results = review
            .doc
            .search(&SearchQuery {
                case_sensitive: query.chars().any(char::is_uppercase),
                pattern: query,
                regex: false,
            })
            .unwrap_or_default();
        search.highlights.lines.clear();
        for found in &results.matches {
            search
                .highlights
                .lines
                .entry((found.file.clone(), found.side, found.line))
                .or_default()
                .extend(found.ranges.iter().copied());
        }
        search.results = results;
        search.highlights.revision += 1;
        if jump {
            let from = review.cursor_index.saturating_sub(1);
            self.search_step(from, Direction::Forward);
        } else {
            self.update_search_position();
        }
    }

    /// Move to the next or previous match from item `from`, wrapping around.
    pub(super) fn search_step(&mut self, from: usize, direction: Direction) {
        let (Some(search), Some(review)) = (self.search.as_mut(), self.review.as_mut()) else {
            return;
        };
        let target = review
            .doc
            .navigate_matches(&search.results, from, direction)
            .or_else(|| review.doc.wrapped_match(&search.results, direction));
        if let Some(target) = target {
            review.set_cursor(target.index, AnchorPolicy::Center);
        }
        self.update_search_position();
    }

    /// Next or previous match from the cursor (`n`/`N`). Returns whether a search is active.
    pub(super) fn search_next(&mut self, direction: Direction) -> bool {
        let active = self
            .search
            .as_ref()
            .is_some_and(|search| !search.query().is_empty());
        if !active {
            return false;
        }
        let from = self.review.as_ref().map_or(0, |review| review.cursor_index);
        self.search_step(from, direction);
        true
    }

    /// Recompute which match the cursor is on.
    fn update_search_position(&mut self) {
        let (Some(search), Some(review)) = (self.search.as_mut(), self.review.as_ref()) else {
            return;
        };
        let mut indices: Vec<(usize, usize)> = search
            .results
            .matches
            .iter()
            .enumerate()
            .filter_map(|(number, found)| Some((review.doc.index_of(&found.key())?, number)))
            .collect();
        indices.sort_unstable();
        indices.dedup_by_key(|(index, _)| *index);
        let current = indices
            .iter()
            .position(|(index, _)| *index == review.cursor_index);
        search.position = current.map(|position| (position + 1, indices.len()));
        let line = current
            .and_then(|position| search.results.matches.get(indices[position].1))
            .map(|found| (found.file.clone(), found.side, found.line));
        if search.highlights.current != line {
            search.highlights.current = line;
            search.highlights.revision += 1;
        }
    }

    /// Re-run an active search when the document changed (content loaded or evicted).
    pub(super) fn refresh_search(&mut self) {
        let stale = match (&self.search, &self.review) {
            (Some(search), Some(review)) => search.doc_revision != review.doc.revision(),
            _ => false,
        };
        if stale {
            self.run_search(false);
        }
    }

    // ----------------------------------------------------------------------------------------
    // Viewed
    // ----------------------------------------------------------------------------------------

    /// Toggle the cursor file's viewed mark; marking moves to the next unviewed file.
    pub(super) fn toggle_viewed(&mut self) {
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let Some(file) = review.cursor_file() else {
            self.set_status(
                "move into a file to mark it viewed",
                StatusSeverity::Warning,
            );
            return;
        };
        let Some((identity, patch)) = review.doc.viewed_identity(&file) else {
            return;
        };
        let viewed = review
            .doc
            .viewed_state(&file)
            .is_some_and(bcode_code_review::viewed::ViewedState::is_viewed);
        let workspace = self.workspace_mut();
        if viewed {
            bcode_code_review::viewed::unmark_viewed(
                workspace,
                &identity.with_patch(patch.as_deref()),
            );
        } else {
            bcode_code_review::viewed::mark_viewed(
                workspace,
                &identity.with_patch(patch.as_deref()),
            );
        }
        self.apply_viewed();
        let Some(review) = self.review.as_mut() else {
            return;
        };
        if viewed {
            self.set_status(
                format!("{} marked unviewed", file.path),
                StatusSeverity::Info,
            );
            return;
        }
        let origin = review
            .doc
            .index_of(&ItemKey::FileHeader(file.clone()))
            .and_then(|index| review.doc.item(index).map(ReviewItem::key));
        let moved = origin.is_some_and(|origin| {
            Self::run_motion(review, origin, NavKind::UnviewedFile, Direction::Forward, 0)
        });
        let message = if moved {
            format!("{} viewed", file.path)
        } else {
            format!("{} viewed · every file is viewed", file.path)
        };
        self.set_status(message, StatusSeverity::Success);
        // Finishing a stack layer says so, and how to go on.
        self.note_layer_done(&file);
    }

    /// Show the workspace's viewed marks and save them.
    pub(super) fn apply_viewed(&mut self) {
        let workspace = self.workspace_mut().clone();
        if let Some(review) = self.review.as_mut() {
            review.doc.set_viewed_marks(&workspace);
            review.note_splices();
        }
        self.request_save();
    }

    /// Re-record marks whose file identity became known or changed.
    pub(super) fn update_viewed_marks(&mut self) {
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let stale: Vec<_> = review
            .doc
            .viewed_marks_to_update()
            .into_iter()
            .filter_map(|file| review.doc.viewed_identity(&file))
            .collect();
        if stale.is_empty() {
            return;
        }
        let workspace = self.workspace_mut();
        for (identity, patch) in &stale {
            bcode_code_review::viewed::mark_viewed(
                workspace,
                &identity.with_patch(patch.as_deref()),
            );
        }
        self.apply_viewed();
    }

    // ----------------------------------------------------------------------------------------
    // Copy and range selection
    // ----------------------------------------------------------------------------------------

    /// Toggle keyboard range selection anchored at the cursor.
    pub(super) fn toggle_range(&mut self) {
        let Some(review) = self.review.as_mut() else {
            return;
        };
        if review.selection.take().is_some() {
            return;
        }
        let Some(cursor) = review.cursor.clone() else {
            return;
        };
        if !matches!(cursor, ItemKey::Line { .. } | ItemKey::LinePair { .. }) {
            self.set_status(
                "move to a diff line to select lines",
                StatusSeverity::Warning,
            );
            return;
        }
        review.selection = Some(Selection {
            anchor: cursor.clone(),
            end: cursor,
            follow: true,
        });
    }

    /// Copy the selected lines (else the cursor line) to the clipboard.
    pub(super) fn copy_lines(&mut self, host: &UiContext) {
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let (first, last) = match &review.selection {
            Some(selection) => {
                let end = if selection.follow {
                    review.cursor.clone()
                } else {
                    Some(selection.end.clone())
                };
                let (Some(a), Some(b)) = (
                    review.doc.index_of(&selection.anchor),
                    end.and_then(|end| review.doc.index_of(&end)),
                ) else {
                    return;
                };
                (a.min(b), a.max(b))
            }
            None => (review.cursor_index, review.cursor_index),
        };
        let mut lines = Vec::new();
        for index in first..=last.min(first + 10_000) {
            match review.doc.item(index) {
                Some(ReviewItem::Line(line)) => {
                    lines.push(line.content.text().unwrap_or_default().to_owned());
                }
                Some(ReviewItem::LinePair(pair)) => {
                    let side = pair.new.as_ref().or(pair.old.as_ref());
                    if let Some(side) = side {
                        lines.push(side.content.text().unwrap_or_default().to_owned());
                    }
                }
                _ => {}
            }
        }
        if lines.is_empty() {
            self.set_status("nothing to copy here", StatusSeverity::Warning);
            return;
        }
        let count = lines.len();
        match host.copy_text(&lines.join("\n")) {
            Ok(()) => self.set_status(
                format!("copied {count} line{}", if count == 1 { "" } else { "s" }),
                StatusSeverity::Success,
            ),
            Err(error) => self.set_status(format!("copy failed: {error}"), StatusSeverity::Error),
        }
    }

    // ----------------------------------------------------------------------------------------
    // Presentation state
    // ----------------------------------------------------------------------------------------

    /// The presentation state as it would be saved now.
    #[must_use]
    pub(super) fn current_presentation(&self) -> ReviewPresentationState {
        let mut state = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.presentation.clone())
            .unwrap_or_default();
        state.panels = self.panels.state();
        state.layer_positions.clone_from(&self.layer_positions);
        state.collapsed_threads.clone_from(&self.folds.collapsed);
        state.expanded_answers.clone_from(&self.folds.expanded);
        state.hide_resolved_threads = self.hide_resolved;
        state.hide_viewed_files = self.hide_viewed_files;
        state.all_stack_layers = self.stack_view == super::stack::StackView::AllLayers;
        state.layout_mode = self.layout_override.map(|mode| match mode {
            LayoutMode::Unified => ReviewLayoutMode::Unified,
            LayoutMode::SideBySide => ReviewLayoutMode::SideBySide,
        });
        if let Some(review) = &self.review {
            state.expansions = review.doc.expansions();
            let list = review.list.borrow();
            // The item on the viewport's first row and the rows of it scrolled above. Items
            // without a persisted form (gaps, AI answers, the composer) anchor at the next one
            // that has one.
            state.scroll_anchor = list.resolved().and_then(|resolved| {
                let top = resolved.top_offset;
                let first = list.index().item_at_offset(top)?;
                (first..first.saturating_add(64)).find_map(|index| {
                    let item = list.index().item(index)?;
                    let start = list.index().item_offset(index)?;
                    Some(ReviewScrollAnchor {
                        item: item.key.0.to_persisted()?,
                        row_offset: u32::try_from(top.saturating_sub(start)).unwrap_or(0),
                    })
                })
            });
        }
        state
    }

    /// Store the current presentation state on the workspace and save it when it changed
    /// (checked at most once per interval unless `now` is forced).
    pub(super) fn persist_presentation(&mut self, force: bool) {
        let now = Instant::now();
        if !force
            && self
                .presentation_checked
                .is_some_and(|checked| now.duration_since(checked) < PRESENTATION_INTERVAL)
        {
            return;
        }
        self.presentation_checked = Some(now);
        if !self.restored || self.review.is_none() {
            return;
        }
        let state = self.current_presentation();
        let workspace = self.workspace_mut();
        if workspace.presentation.as_ref() == Some(&state) {
            return;
        }
        workspace.presentation = Some(state);
        self.request_save();
    }

    /// Mark the workspace for saving: a durable review whenever it changed, an unsaved one when
    /// its viewed state did (the only state it keeps; see `save`).
    pub(super) fn request_save(&mut self) {
        let wanted = match &self.workspace {
            None => false,
            Some(workspace) if workspace.id == super::TRANSIENT_WORKSPACE_ID => {
                let viewed = (
                    workspace.viewed_files.clone(),
                    workspace.viewed_marks.clone(),
                );
                let changed = self.transient_viewed.as_ref() != Some(&viewed);
                self.transient_viewed = Some(viewed);
                changed
            }
            Some(_) => true,
        };
        if wanted {
            self.save = match self.save {
                super::SaveState::Idle | super::SaveState::Pending => super::SaveState::Pending,
                super::SaveState::Running | super::SaveState::RunningThenPending => {
                    super::SaveState::RunningThenPending
                }
            };
        }
        if let Some(workspace) = &mut self.workspace {
            workspace.updated_at_ms = Some(now_ms());
        }
    }

    /// Apply stored presentation state to a freshly opened review (once).
    pub(super) fn restore_presentation(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let Some(state) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.presentation.clone())
        else {
            return;
        };
        self.layout_override = match state.layout_mode {
            Some(ReviewLayoutMode::Unified) => Some(LayoutMode::Unified),
            Some(ReviewLayoutMode::SideBySide) => Some(LayoutMode::SideBySide),
            Some(ReviewLayoutMode::Auto) | None => None,
        };
        let Some(review) = self.review.as_mut() else {
            return;
        };
        if let Some(mode) = self.layout_override {
            review.doc.set_layout(mode);
        }
        review.doc.restore_expansions(&state.expansions);
        review.note_splices();
        let Some(anchor) = state.scroll_anchor else {
            return;
        };
        let Some(key) = review.doc.resolve_persisted(&anchor.item) else {
            return;
        };
        match review.doc.jump_to(&key) {
            Some(target) if target.needs_load.is_none() => {
                review
                    .list
                    .borrow_mut()
                    .set_position(ListPosition::Anchored(ListAnchor::row(
                        RowKey(target.key.clone()),
                        u64::from(anchor.row_offset),
                    )));
                review.cursor_index = target.index;
                review.cursor = Some(target.key);
            }
            Some(target) => {
                // The anchored line's file loads first; land on its header meanwhile.
                review
                    .list
                    .borrow_mut()
                    .set_position(ListPosition::Anchored(ListAnchor::top(RowKey(
                        target.key.clone(),
                    ))));
                review.cursor_index = target.index;
                review.cursor = Some(target.key);
                review.pending_restore = Some((key, u64::from(anchor.row_offset)));
            }
            None => {}
        }
    }
}
