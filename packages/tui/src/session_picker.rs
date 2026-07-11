//! TUI session picker state.

use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_session_models::{SessionId, SessionSummary};
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::text_input::TextInputState;

use super::filtered_list::FilteredListState;

/// Session picker mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPickerMode {
    /// Filtering/selecting sessions.
    Filter,
    /// Editing the selected session name.
    Rename,
    /// Confirming deletion of the selected session.
    DeleteConfirm,
}

/// Session picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPickerApp {
    sessions: Vec<SessionSummary>,
    filter: TextInputState,
    rename: TextInputState,
    list: FilteredListState,
    status: String,
    empty_message: String,
    last_import: Option<(SessionSummary, Vec<bcode_ipc::SessionImportWarning>)>,
    mode: SessionPickerMode,
}

impl SessionPickerApp {
    /// Create a picker from session summaries.
    #[must_use]
    pub fn new(sessions: Vec<SessionSummary>) -> Self {
        let list = FilteredListState::new(sessions.len());
        Self {
            sessions,
            filter: super::text_input_flow::empty_state(),
            rename: super::text_input_flow::empty_state(),
            list,
            status: "Select a session or press Ctrl-N to create one".to_owned(),
            empty_message: "No matching sessions. Press Ctrl-N to create a new session.".to_owned(),
            last_import: None,
            mode: SessionPickerMode::Filter,
        }
    }

    /// Return picker mode.
    #[must_use]
    pub const fn mode(&self) -> SessionPickerMode {
        self.mode
    }

    /// Return the filter input mutably.
    pub const fn filter_mut(&mut self) -> &mut TextInputState {
        &mut self.filter
    }

    /// Return the rename input.
    #[must_use]
    pub const fn rename(&self) -> &TextInputState {
        &self.rename
    }

    /// Return the rename input mutably.
    pub const fn rename_mut(&mut self) -> &mut TextInputState {
        &mut self.rename
    }

    /// Return active text input mutably.
    pub const fn active_input_mut(&mut self) -> &mut TextInputState {
        match self.mode {
            SessionPickerMode::Filter | SessionPickerMode::DeleteConfirm => &mut self.filter,
            SessionPickerMode::Rename => &mut self.rename,
        }
    }

    /// Return list state.
    pub const fn list_state_mut(&mut self) -> &mut ListState {
        self.list.list_state_mut()
    }

    /// Return picker status.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Set picker status.
    pub fn set_status(&mut self, status: String) {
        self.status = status;
    }

    /// Set both loading status and empty-list copy.
    pub fn set_loading_status(&mut self, status: String) {
        self.status.clone_from(&status);
        self.empty_message = status;
    }

    /// Set the default empty-list message for an idle picker.
    pub fn set_idle_empty_message(&mut self) {
        "No matching sessions. Press Ctrl-N to create a new session."
            .clone_into(&mut self.empty_message);
    }

    /// Record the most recent successful external import for the warning panel.
    pub fn set_last_import(
        &mut self,
        import: Option<(SessionSummary, Vec<bcode_ipc::SessionImportWarning>)>,
    ) {
        self.last_import = import;
    }

    /// Return the most recent successful external import, if any.
    #[must_use]
    pub const fn last_import(
        &self,
    ) -> Option<&(SessionSummary, Vec<bcode_ipc::SessionImportWarning>)> {
        self.last_import.as_ref()
    }

    /// Clear the most recent external import warning panel.
    pub fn clear_last_import(&mut self) {
        self.last_import = None;
    }

    /// Replace sessions.
    pub fn replace_sessions(&mut self, sessions: Vec<SessionSummary>) {
        self.sessions = sessions;
        self.refresh_filter();
    }

    /// Return visible list items.
    #[must_use]
    pub fn list_items(&self) -> Vec<ListItem> {
        if self.list.indices().is_empty() {
            return vec![empty_item(&self.empty_message)];
        }
        self.list
            .indices()
            .iter()
            .map(|index| session_item(&self.sessions[*index]))
            .collect()
    }

    /// Return selected session id.
    #[must_use]
    pub fn selected_session_id(&self) -> Option<SessionId> {
        let index = self.list.selected_source_index()?;
        Some(self.sessions[index].id)
    }

    /// Return selected import metadata.
    #[must_use]
    pub fn selected_import(&self) -> Option<&bcode_session_models::SessionImportSummary> {
        let index = self.list.selected_source_index()?;
        self.sessions[index].import.as_ref()
    }

    /// Return selected session name.
    #[must_use]
    pub fn selected_session_name(&self) -> Option<&str> {
        let index = self.list.selected_source_index()?;
        self.sessions[index].name.as_deref()
    }

    /// Select a visible row by zero-based index.
    pub const fn select_visible(&mut self, row: usize) -> bool {
        self.list.select_visible(row)
    }

    /// Enter rename mode for the selected session.
    pub fn start_rename(&mut self) -> bool {
        let Some(name) = self.selected_session_name() else {
            "No session selected to rename".clone_into(&mut self.status);
            return false;
        };
        self.rename = super::text_input_flow::state_with_text(name, true);
        self.mode = SessionPickerMode::Rename;
        "Enter saves rename; Esc cancels".clone_into(&mut self.status);
        true
    }

    /// Exit rename mode without saving.
    pub fn cancel_rename(&mut self) {
        self.mode = SessionPickerMode::Filter;
        "Rename canceled".clone_into(&mut self.status);
    }

    /// Enter delete confirmation mode for the selected session.
    pub fn start_delete_confirmation(&mut self) -> bool {
        if self.selected_session_id().is_none() {
            "No session selected to delete".clone_into(&mut self.status);
            return false;
        }
        self.mode = SessionPickerMode::DeleteConfirm;
        "Delete selected session? y/N".clone_into(&mut self.status);
        true
    }

    /// Exit delete confirmation mode without deleting.
    pub fn cancel_delete(&mut self) {
        self.mode = SessionPickerMode::Filter;
        "Delete canceled".clone_into(&mut self.status);
    }

    /// Return to filter mode after a mutation.
    pub fn finish_mutation(&mut self, status: String) {
        self.mode = SessionPickerMode::Filter;
        self.status = status;
    }

    /// Recompute filtered sessions after filter edits.
    pub fn refresh_filter(&mut self) {
        let query = self.filter.buffer().text().trim().to_ascii_lowercase();
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
    pub fn select_next(&mut self) {
        self.list.select_next();
    }

    /// Move selection up.
    pub fn select_previous(&mut self) {
        self.list.select_previous();
    }
}

fn session_item(session: &SessionSummary) -> ListItem {
    let name = session.display_title();
    let display_name = session.import.as_ref().map_or_else(
        || fork_display_name(session, name),
        |import| {
            if import.imported_at_ms == 0 {
                format!("[{} import] {name}", import.source_id)
            } else {
                format!("[{}] {name}", import.source_id)
            }
        },
    );
    let id = session.id.to_string();
    let cwd = display_from_current_dir(&session.working_directory).to_string();
    ListItem::new(Line::from_spans(vec![
        Span::styled(display_name, Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(id, Style::new().fg(Color::BrightBlack)),
        Span::raw("  "),
        Span::styled(cwd, Style::new().fg(Color::BrightBlack)),
    ]))
}

fn fork_display_name(session: &SessionSummary, name: &str) -> String {
    let Some(fork) = &session.fork else {
        return name.to_owned();
    };
    let label = match fork.kind {
        bcode_session_models::SessionForkKind::Fork => "fork",
        bcode_session_models::SessionForkKind::Clone => "clone",
    };
    match fork.source_title.as_deref() {
        Some(source_title) if !source_title.is_empty() => {
            format!("[{label} of {source_title}] {name}")
        }
        _ => format!("[{label}] {name}"),
    }
}

fn session_matches(session: &SessionSummary, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    session.display_title().to_ascii_lowercase().contains(query)
        || session.id.to_string().contains(query)
        || session
            .fork
            .as_ref()
            .is_some_and(|fork| fork_matches_query(fork, query))
        || display_from_current_dir(&session.working_directory)
            .to_string()
            .to_ascii_lowercase()
            .contains(query)
}

fn fork_matches_query(fork: &bcode_session_models::SessionForkSummary, query: &str) -> bool {
    let kind = match fork.kind {
        bcode_session_models::SessionForkKind::Fork => "fork",
        bcode_session_models::SessionForkKind::Clone => "clone",
    };
    kind.contains(query)
        || fork.source_session_id.to_string().contains(query)
        || fork
            .source_title
            .as_deref()
            .is_some_and(|title| title.to_ascii_lowercase().contains(query))
}

fn empty_item(message: &str) -> ListItem {
    ListItem::new(Line::from_spans(vec![Span::styled(
        message.to_owned(),
        Style::new().fg(Color::BrightBlack),
    )]))
}
