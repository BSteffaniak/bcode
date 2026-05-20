//! BMUX backend session picker state.

use bcode_session_models::{SessionId, SessionSummary};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

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
    list_state: ListState,
    filtered_indices: Vec<usize>,
    status: String,
    mode: SessionPickerMode,
}

impl SessionPickerApp {
    /// Create a picker from session summaries.
    #[must_use]
    pub(super) fn new(sessions: Vec<SessionSummary>) -> Self {
        let filtered_indices = (0..sessions.len()).collect::<Vec<_>>();
        let mut list_state = ListState::new();
        if !filtered_indices.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            sessions,
            filter: TextEditBuffer::new(),
            rename: TextEditBuffer::new(),
            list_state,
            filtered_indices,
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
        &mut self.list_state
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
        if self.filtered_indices.is_empty() {
            return vec![ListItem::new(Line::from_spans(vec![Span::styled(
                "No matching sessions. Press Ctrl-N to create a new session.",
                Style::new().fg(Color::BrightBlack),
            )]))];
        }

        self.filtered_indices
            .iter()
            .map(|index| session_item(&self.sessions[*index]))
            .collect()
    }

    /// Return the selected session id.
    #[must_use]
    pub(super) fn selected_session_id(&self) -> Option<SessionId> {
        let selected = self.list_state.selected?;
        let index = *self.filtered_indices.get(selected)?;
        Some(self.sessions[index].id)
    }

    /// Return selected session name.
    #[must_use]
    pub(super) fn selected_session_name(&self) -> Option<&str> {
        let selected = self.list_state.selected?;
        let index = *self.filtered_indices.get(selected)?;
        self.sessions[index].name.as_deref()
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
        self.filtered_indices = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| session_matches(session, &query).then_some(index))
            .collect();
        if self.filtered_indices.is_empty() {
            self.list_state.select(None);
            self.list_state.offset = 0;
        } else {
            self.list_state.select(Some(
                self.list_state
                    .selected
                    .unwrap_or(0)
                    .min(self.filtered_indices.len() - 1),
            ));
            self.list_state
                .ensure_selected_visible(1, self.filtered_indices.len());
        }
    }

    /// Move selection down.
    pub(super) fn select_next(&mut self) {
        self.list_state.select_next(self.filtered_indices.len());
    }

    /// Move selection up.
    pub(super) fn select_previous(&mut self) {
        self.list_state.select_previous(self.filtered_indices.len());
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
