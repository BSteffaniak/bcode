//! BMUX backend session picker state.

use bcode_session_models::{SessionId, SessionSummary};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

use super::filtered_list::FilteredListState;

/// Session picker mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionPickerMode {
    /// Filtering/selecting sessions.
    Filter,
    /// Editing the selected session name.
    Rename,
    /// Confirming deletion of the selected session.
    DeleteConfirm,
}

/// Session picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionPickerApp {
    sessions: Vec<SessionSummary>,
    filter: TextEditBuffer,
    rename: TextEditBuffer,
    list: FilteredListState,
    status: String,
    mode: SessionPickerMode,
}

impl SessionPickerApp {
    /// Create a picker from session summaries.
    #[must_use]
    pub(super) fn new(sessions: Vec<SessionSummary>) -> Self {
        let list = FilteredListState::new(sessions.len());
        Self {
            sessions,
            filter: TextEditBuffer::new(),
            rename: TextEditBuffer::new(),
            list,
            status: "Select a session or press Ctrl-N to create one".to_owned(),
            mode: SessionPickerMode::Filter,
        }
    }

    /// Return picker mode.
    #[must_use]
    pub(super) const fn mode(&self) -> SessionPickerMode {
        self.mode
    }

    /// Return the filter input.
    #[must_use]
    pub(super) const fn filter(&self) -> &TextEditBuffer {
        &self.filter
    }

    /// Return the filter input mutably.
    pub(super) const fn filter_mut(&mut self) -> &mut TextEditBuffer {
        &mut self.filter
    }

    /// Return the rename input.
    #[must_use]
    pub(super) const fn rename(&self) -> &TextEditBuffer {
        &self.rename
    }

    /// Return the rename input mutably.
    pub(super) const fn rename_mut(&mut self) -> &mut TextEditBuffer {
        &mut self.rename
    }

    /// Return list state.
    pub(super) const fn list_state_mut(&mut self) -> &mut ListState {
        self.list.list_state_mut()
    }

    /// Return picker status.
    #[must_use]
    pub(super) fn status(&self) -> &str {
        &self.status
    }

    /// Set picker status.
    pub(super) fn set_status(&mut self, status: String) {
        self.status = status;
    }

    /// Replace all sessions and refresh the filter.
    pub(super) fn replace_sessions(&mut self, sessions: Vec<SessionSummary>) {
        self.sessions = sessions;
        self.refresh_filter();
    }

    /// Return visible list items.
    #[must_use]
    pub(super) fn list_items(&self) -> Vec<ListItem> {
        if self.list.indices().is_empty() {
            return vec![empty_item(
                "No matching sessions. Press Ctrl-N to create a new session.",
            )];
        }

        self.list
            .indices()
            .iter()
            .map(|index| session_item(&self.sessions[*index]))
            .collect()
    }

    /// Return the selected session id.
    #[must_use]
    pub(super) fn selected_session_id(&self) -> Option<SessionId> {
        let index = self.list.selected_source_index()?;
        Some(self.sessions[index].id)
    }

    /// Return selected session name.
    #[must_use]
    pub(super) fn selected_session_name(&self) -> Option<&str> {
        let index = self.list.selected_source_index()?;
        self.sessions[index].name.as_deref()
    }

    /// Select a visible row by zero-based index.
    pub(super) const fn select_visible(&mut self, row: usize) -> bool {
        self.list.select_visible(row)
    }

    /// Enter rename mode for the selected session.
    pub(super) fn start_rename(&mut self) -> bool {
        let Some(name) = self.selected_session_name() else {
            "No session selected to rename".clone_into(&mut self.status);
            return false;
        };
        self.rename = TextEditBuffer::from_text(name);
        self.mode = SessionPickerMode::Rename;
        "Enter saves rename; Esc cancels".clone_into(&mut self.status);
        true
    }

    /// Exit rename mode without saving.
    pub(super) fn cancel_rename(&mut self) {
        self.mode = SessionPickerMode::Filter;
        "Rename canceled".clone_into(&mut self.status);
    }

    /// Enter delete confirmation mode for the selected session.
    pub(super) fn start_delete_confirmation(&mut self) -> bool {
        if self.selected_session_id().is_none() {
            "No session selected to delete".clone_into(&mut self.status);
            return false;
        }
        self.mode = SessionPickerMode::DeleteConfirm;
        "Delete selected session? y/N".clone_into(&mut self.status);
        true
    }

    /// Exit delete confirmation mode without deleting.
    pub(super) fn cancel_delete(&mut self) {
        self.mode = SessionPickerMode::Filter;
        "Delete canceled".clone_into(&mut self.status);
    }

    /// Return to filter mode after a mutation.
    pub(super) fn finish_mutation(&mut self, status: String) {
        self.mode = SessionPickerMode::Filter;
        self.status = status;
    }

    /// Recompute filtered sessions after filter edits.
    pub(super) fn refresh_filter(&mut self) {
        let query = self.filter.text().trim().to_ascii_lowercase();
        let filtered_indices = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| session_matches(session, &query).then_some(index))
            .collect();
        self.list.replace_indices(filtered_indices);
        let visible_count = self.list.indices().len();
        self.list
            .list_state_mut()
            .ensure_selected_visible(1, visible_count);
    }

    /// Move selection down.
    pub(super) fn select_next(&mut self) {
        self.list.select_next();
    }

    /// Move selection up.
    pub(super) fn select_previous(&mut self) {
        self.list.select_previous();
    }
}

fn session_item(session: &SessionSummary) -> ListItem {
    let name = session
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("untitled");
    ListItem::new(Line::from_spans(vec![
        Span::styled(name.to_owned(), Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(session.id.to_string(), Style::new().fg(Color::BrightBlack)),
    ]))
}

fn session_matches(session: &SessionSummary, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    session
        .name
        .as_deref()
        .is_some_and(|name| name.to_ascii_lowercase().contains(query))
        || session.id.to_string().to_ascii_lowercase().contains(query)
}

fn empty_item(message: &str) -> ListItem {
    ListItem::new(Line::from_spans(vec![Span::styled(
        message.to_owned(),
        Style::new().fg(Color::BrightBlack),
    )]))
}
