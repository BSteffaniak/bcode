//! The assistant's history picker: every conversation of the repository, searchable, with the
//! active one marked.
//!
//! * **Loading.** One bounded catalog page of the app's sessions, the repository's assistant
//!   bindings, the summaries of sessions known only from a binding (at most
//!   [`MAX_LEGACY_SUMMARIES`]), the saved reviews' titles and the pending permission requests
//!   are read off the UI thread and merged ([`super::history::merge`]). Turn counts are read for
//!   the first [`TURN_ROWS`] rows.
//! * **Rows.** `●` on the active conversation, the title, where it started, how long ago it was
//!   active, its turns and whether it needs the user (`needs you`, `working`); card sessions are
//!   labelled `card`. Columns drop from the right as the panel narrows (kind, turns, time, tag);
//!   the title is cut last. Widths are measured with BMUX's text measurement.
//! * **Actions.** Switch, rename, delete (marked by the first press, deleted by the second) and
//!   open in Bcode; the panel runs them ([`super::panel`]).

use super::history::{self, HistoryRow, HistorySources};
use crate::host::{AppSession, PendingPermission, TurnCount};
use crate::services::ReviewServices;
use crate::theme::ReviewTheme;
use bcode_code_review_models::{ListReviewAssistantsRequest, ListReviewWorkspacesRequest};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::text_input::{TextInputPolicy, TextInputState};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Summaries of binding-only sessions read per load at most.
pub const MAX_LEGACY_SUMMARIES: usize = 200;
/// Rows whose turns are counted after a load.
pub const TURN_ROWS: usize = 30;
/// Cells kept for the title before more columns are dropped.
const TITLE_KEEP: usize = 14;

/// Everything one history load read.
#[derive(Debug, Clone, Default)]
pub struct HistoryLoad {
    /// Merged rows, newest activity first.
    pub rows: Vec<HistoryRow>,
    /// Whether Bcode's catalog was still loading or incomplete.
    pub degraded: bool,
    /// Sessions waiting on a permission answer.
    pub needs_you: BTreeSet<String>,
    /// Cursor of Bcode's next catalog page, when the catalog has more sessions than listed.
    pub next_cursor: Option<String>,
}

/// Read and merge the history of the repository at `repo`.
///
/// # Errors
///
/// * Why Bcode or the review store could not be read.
pub async fn load(services: ReviewServices, repo: PathBuf) -> Result<HistoryLoad, String> {
    let host = services.host();
    let page = host
        .list_app_sessions(repo.clone(), None, crate::host::MAX_APP_SESSION_PAGE)
        .await
        .map_err(|error| error.message)?;
    let bindings = services
        .call(ListReviewAssistantsRequest {
            repo_path: repo.clone(),
            limit: None,
        })
        .await
        .map_err(|error| error.message)?
        .bindings;
    let reviews: BTreeMap<String, String> = services
        .call(ListReviewWorkspacesRequest {
            repo_path: repo,
            include_archived: true,
        })
        .await
        .map_err(|error| error.message)?
        .workspaces
        .into_iter()
        .map(|workspace| {
            (
                bcode_code_review::scope::annotation_workspace_id(&workspace),
                workspace.title,
            )
        })
        .collect();
    let mut legacy: BTreeMap<String, Option<AppSession>> = BTreeMap::new();
    let wanted: Vec<String> = history::legacy_sessions(&HistorySources {
        catalog: &page.sessions,
        bindings: &bindings,
        legacy: &legacy,
        reviews: &reviews,
    })
    .into_iter()
    .take(MAX_LEGACY_SUMMARIES)
    .collect();
    if !wanted.is_empty() {
        let summaries = host
            .session_summaries(wanted.clone())
            .await
            .map_err(|error| error.message)?;
        legacy.extend(wanted.into_iter().zip(summaries));
    }
    let rows = history::merge(&HistorySources {
        catalog: &page.sessions,
        bindings: &bindings,
        legacy: &legacy,
        reviews: &reviews,
    });
    // A failed permission listing only leaves the needs-you column empty.
    let needs_you = host
        .pending_permissions()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|permission: PendingPermission| permission.session_id)
        .collect();
    Ok(HistoryLoad {
        rows,
        degraded: page.degraded,
        needs_you,
        next_cursor: page.next_cursor,
    })
}

/// Conversations "delete every conversation" lists at most in one run.
pub const MAX_DELETE_ALL: usize = 1_000;

/// Every conversation the review app started (the history's rows, then Bcode's further catalog
/// pages), at most [`MAX_DELETE_ALL`]; and whether more remain unlisted.
///
/// # Errors
///
/// * Why Bcode or the review store could not be read.
pub async fn list_all(
    services: ReviewServices,
    repo: PathBuf,
) -> Result<(Vec<String>, bool), String> {
    let load = load(services.clone(), repo.clone()).await?;
    let mut sessions: Vec<String> = load.rows.into_iter().map(|row| row.session_id).collect();
    let mut known: BTreeSet<String> = sessions.iter().cloned().collect();
    let mut cursor = load.next_cursor;
    let host = services.host();
    while let Some(next) = cursor.take() {
        if sessions.len() >= MAX_DELETE_ALL {
            return Ok((sessions, true));
        }
        let page = host
            .list_app_sessions(
                repo.clone(),
                Some(next),
                crate::host::MAX_APP_SESSION_PAGE.min(MAX_DELETE_ALL - sessions.len()),
            )
            .await
            .map_err(|error| error.message)?;
        for session in page.sessions {
            if known.insert(session.id.clone()) {
                sessions.push(session.id);
            }
        }
        cursor = page.next_cursor;
    }
    Ok((sessions, false))
}

/// State of the open history picker.
#[derive(Debug)]
pub struct Picker {
    query: RefCell<TextInputState>,
    policy: TextInputPolicy,
    /// The load, once it finished; `None` while loading.
    pub load: Option<Result<HistoryLoad, String>>,
    /// Turns counted per session.
    pub turns: BTreeMap<String, TurnCount>,
    /// Index of the selected row among the matching rows.
    pub selected: usize,
    /// The session marked for deletion by a first press.
    pub marked: Option<String>,
    /// First matching row shown in the last frame.
    pub first: Cell<usize>,
}

impl Default for Picker {
    fn default() -> Self {
        let policy = super::panel::single_line_policy();
        Self {
            query: RefCell::new(TextInputState::new(bmux_text_edit::TextEditBuffer::new())),
            policy,
            load: None,
            turns: BTreeMap::new(),
            selected: 0,
            marked: None,
            first: Cell::new(0),
        }
    }
}

impl Picker {
    /// The search input.
    #[must_use]
    pub const fn query_input(&self) -> &RefCell<TextInputState> {
        &self.query
    }

    /// Editing policy of the search input.
    #[must_use]
    pub const fn policy(&self) -> &TextInputPolicy {
        &self.policy
    }

    /// The search text.
    #[must_use]
    pub fn query(&self) -> String {
        self.query.borrow().buffer().text().to_string()
    }

    /// The rows matching the search, in order.
    #[must_use]
    pub fn matching(&self) -> Vec<&HistoryRow> {
        let query = self.query();
        self.load
            .as_ref()
            .and_then(|load| load.as_ref().ok())
            .map(|load| load.rows.iter().filter(|row| row.matches(&query)).collect())
            .unwrap_or_default()
    }

    /// Every loaded row.
    #[must_use]
    pub fn rows(&self) -> &[HistoryRow] {
        self.load
            .as_ref()
            .and_then(|load| load.as_ref().ok())
            .map_or(&[], |load| load.rows.as_slice())
    }

    /// The selected row.
    #[must_use]
    pub fn selected_row(&self) -> Option<&HistoryRow> {
        self.matching().get(self.selected).copied()
    }

    /// Move the selection by `delta` rows, within the matching rows.
    pub fn move_by(&mut self, delta: isize) {
        let count = self.matching().len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.saturating_add_signed(delta).min(count - 1);
        self.marked = None;
    }

    /// Keep the selection on a matching row after the search or the rows changed.
    pub fn clamp(&mut self) {
        let count = self.matching().len();
        self.selected = self.selected.min(count.saturating_sub(1));
    }

    /// Forget a row (its session was deleted).
    pub fn remove(&mut self, session: &str) {
        if let Some(Ok(load)) = self.load.as_mut() {
            load.rows.retain(|row| row.session_id != session);
        }
        if self.marked.as_deref() == Some(session) {
            self.marked = None;
        }
        self.clamp();
    }

    /// Rename a row.
    pub fn rename(&mut self, session: &str, title: &str) {
        if let Some(Ok(load)) = self.load.as_mut() {
            for row in &mut load.rows {
                if row.session_id == session {
                    title.clone_into(&mut row.title);
                }
            }
        }
    }

    /// Whether `session` waits on a permission answer.
    #[must_use]
    pub fn needs_you(&self, session: &str) -> bool {
        self.load
            .as_ref()
            .and_then(|load| load.as_ref().ok())
            .is_some_and(|load| load.needs_you.contains(session))
    }
}

/// Live state of a row the picker cannot read from the history load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct RowState {
    /// The row is the active conversation.
    pub active: bool,
    /// Its turn runs.
    pub working: bool,
    /// It waits on the user.
    pub needs_you: bool,
    /// The row is selected.
    pub selected: bool,
    /// The row is marked for deletion.
    pub marked: bool,
}

/// One row of the picker at `width` cells, columns dropped from the right until the title
/// keeps [`TITLE_KEEP`] cells (or its whole width).
#[must_use]
pub fn row_line(
    row: &HistoryRow,
    state: RowState,
    turns: Option<TurnCount>,
    now_ms: u64,
    width: usize,
    theme: &ReviewTheme,
) -> Line {
    let active_mark = if state.active { "● " } else { "  " };
    let mut columns: Vec<Span> = Vec::new();
    let tag = Span::styled(row.tag.label(), theme.muted);
    let time = Span::styled(
        history::relative_time(row.updated_at_ms, now_ms),
        theme.muted,
    );
    let turns = turns.map(|turns| {
        Span::styled(
            if turns.more {
                format!("{}+", turns.turns)
            } else {
                turns.turns.to_string()
            },
            theme.muted,
        )
    });
    let status = if state.marked {
        Some(Span::styled("alt+d again to delete", theme.error))
    } else if state.needs_you {
        Some(Span::styled("needs you", theme.notice))
    } else if state.working {
        Some(Span::styled("working", theme.accent))
    } else {
        None
    };
    let kind = (!row.kind.label().is_empty()).then(|| Span::styled(row.kind.label(), theme.muted));
    // Dropped from the end first: kind, turns, time, tag. The status always stays.
    let mut optional: Vec<Span> = [Some(tag), Some(time), turns, kind]
        .into_iter()
        .flatten()
        .collect();
    let title_width = Line::from(row.title.as_str()).width();
    let marker_width = Line::from(active_mark).width();
    let fixed = |columns: &[Span], status: &Option<Span>| -> usize {
        columns
            .iter()
            .chain(status.iter())
            .map(|span| Line::from_spans(vec![span.clone()]).width() + 2)
            .sum()
    };
    while !optional.is_empty()
        && marker_width + title_width.min(TITLE_KEEP) + fixed(&optional, &status) > width
    {
        optional.pop();
    }
    columns.extend(optional);
    columns.extend(status);
    let right = fixed(&columns, &None);
    let title_room = width.saturating_sub(marker_width + right);
    let title_style = if state.selected {
        theme.title
    } else {
        theme.tiers.strong
    };
    let title =
        Line::from_spans(vec![Span::styled(row.title.clone(), title_style)]).truncate(title_room);
    let pad = title_room.saturating_sub(title.width());
    let mut spans = vec![Span::styled(active_mark, theme.accent)];
    spans.extend(title.spans);
    spans.push(Span::raw(" ".repeat(pad)));
    for column in columns {
        spans.push(Span::raw("  "));
        spans.push(column);
    }
    let line = Line::from_spans(spans).truncate(width);
    if state.selected {
        line.patch_style(theme.selection)
    } else {
        line
    }
}
