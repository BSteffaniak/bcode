//! TUI session picker state.

use bcode_session_models::{SessionId, SessionSummary};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

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
    filter: TextEditBuffer,
    rename: TextEditBuffer,
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
            filter: TextEditBuffer::new(),
            rename: TextEditBuffer::new(),
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

    /// Return the filter input.
    #[must_use]
    pub const fn filter(&self) -> &TextEditBuffer {
        &self.filter
    }

    /// Return the filter input mutably.
    pub const fn filter_mut(&mut self) -> &mut TextEditBuffer {
        &mut self.filter
    }

    /// Return the rename input.
    #[must_use]
    pub const fn rename(&self) -> &TextEditBuffer {
        &self.rename
    }

    /// Return the rename input mutably.
    pub const fn rename_mut(&mut self) -> &mut TextEditBuffer {
        &mut self.rename
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

    /// Replace all sessions and refresh the filter.
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

    /// Return the selected session id.
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
        self.rename = TextEditBuffer::from_text(name);
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
    pub fn select_next(&mut self) {
        self.list.select_next();
    }

    /// Move selection up.
    pub fn select_previous(&mut self) {
        self.list.select_previous();
    }
}

fn session_item(session: &SessionSummary) -> ListItem {
    let name = session
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("untitled");
    let display_name = session.import.as_ref().map_or_else(
        || name.to_owned(),
        |import| {
            if import.imported_at_ms == 0 {
                format!("[{} import] {name}", import.source_id)
            } else {
                format!("[{}] {name}", import.source_id)
            }
        },
    );
    let id = session.import.as_ref().map_or_else(
        || session.id.to_string(),
        |import| {
            if import.imported_at_ms == 0 {
                import.external_session_id.clone()
            } else {
                session.id.to_string()
            }
        },
    );
    ListItem::new(Line::from_spans(vec![
        Span::styled(display_name, Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(id, Style::new().fg(Color::BrightBlack)),
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
        || session
            .import
            .as_ref()
            .is_some_and(|import| import.source_id.to_ascii_lowercase().contains(query))
        || session.id.to_string().to_ascii_lowercase().contains(query)
}

fn empty_item(message: &str) -> ListItem {
    ListItem::new(Line::from_spans(vec![Span::styled(
        message.to_owned(),
        Style::new().fg(Color::BrightBlack),
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::SessionImportSummary;

    fn summary(imported_at_ms: u64) -> SessionSummary {
        SessionSummary {
            id: SessionId::new(),
            name: Some("Imported title".to_owned()),
            client_count: 0,
            created_at_ms: 1,
            updated_at_ms: 2,
            working_directory: std::path::PathBuf::from("/tmp/project"),
            import: Some(SessionImportSummary {
                source_id: "pi".to_owned(),
                source_display_name: "Pi".to_owned(),
                external_session_id: "external-1".to_owned(),
                imported_at_ms,
            }),
        }
    }

    #[test]
    fn importable_session_item_uses_external_id() {
        let item = session_item(&summary(0));
        let rendered = format!("{item:?}");

        assert!(rendered.contains("[pi import] Imported title"));
        assert!(rendered.contains("external-1"));
    }

    #[test]
    fn imported_session_item_uses_native_id() {
        let session = summary(42);
        let native_id = session.id.to_string();
        let item = session_item(&session);
        let rendered = format!("{item:?}");

        assert!(rendered.contains("[pi] Imported title"));
        assert!(rendered.contains(&native_id));
    }
}
