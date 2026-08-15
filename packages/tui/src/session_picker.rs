//! TUI session picker state.

use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_session_models::{SessionId, SessionSummary};
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;
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
    /// Selecting from portable canonically hydrated transcript-search results.
    TranscriptSearch,
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
    search_results: Vec<bcode_session_search::HydratedSessionSearchHit>,
    search_query_complete: bool,
    search_coverage_complete: bool,
    search_provider_reports: usize,
    search_failures: usize,
    search_controls: super::session_search::SessionSearchControls,
    search_cursor: Option<bcode_session_search::SearchCursor>,
    search_preview: String,
    migration_confirmation_armed: bool,
    bulk_migration_operation_id: Option<String>,
    search_backfill_operation_id: Option<String>,
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
            search_results: Vec::new(),
            search_query_complete: true,
            search_coverage_complete: true,
            search_provider_reports: 0,
            search_failures: 0,
            search_controls: super::session_search::SessionSearchControls::default(),
            search_cursor: None,
            search_preview: String::new(),
            migration_confirmation_armed: false,
            bulk_migration_operation_id: None,
            search_backfill_operation_id: None,
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
    pub const fn filter(&self) -> &TextInputState {
        &self.filter
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
            SessionPickerMode::Filter
            | SessionPickerMode::DeleteConfirm
            | SessionPickerMode::TranscriptSearch => &mut self.filter,
            SessionPickerMode::Rename => &mut self.rename,
        }
    }

    /// Synchronize list visibility before rendering and return its render state.
    pub fn list_render_state(&mut self, viewport_height: u16) -> &mut ListState {
        self.list.render_state(viewport_height)
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

    /// Replace sessions.
    pub fn replace_sessions(&mut self, sessions: Vec<SessionSummary>) {
        self.sessions = sessions;
        self.refresh_filter();
    }

    /// Enter transcript-search query mode.
    pub fn start_transcript_search(&mut self) {
        self.search_results.clear();
        self.search_cursor = None;
        self.search_preview.clear();
        self.mode = SessionPickerMode::TranscriptSearch;
        self.list = FilteredListState::new(0);
        "Type a transcript query, then press Enter".clone_into(&mut self.status);
        "No transcript search results".clone_into(&mut self.empty_message);
    }

    /// Install one bounded terminal transcript-search result set.
    pub fn set_search_results(
        &mut self,
        response: &bcode_session_search::FederatedSessionSearchResponse,
        hydrated: Vec<bcode_session_search::HydratedSessionSearchHit>,
    ) {
        self.search_results = hydrated;
        self.search_query_complete = response.query_complete;
        self.search_coverage_complete = response.coverage_complete;
        self.search_provider_reports = response.providers.len();
        self.search_failures = response.failures.len();
        self.search_cursor = response
            .providers
            .iter()
            .find_map(|provider| provider.next_cursor.clone());
        self.list = FilteredListState::new(self.search_results.len());
        self.mode = SessionPickerMode::TranscriptSearch;
        self.refresh_search_preview();
        let query = if self.search_query_complete {
            "query complete"
        } else {
            "query incomplete"
        };
        let coverage = if self.search_coverage_complete {
            "searchable coverage complete"
        } else {
            "searchable coverage incomplete"
        };
        let failures = if self.search_failures == 0 {
            "no provider failures".to_owned()
        } else {
            format!("{} provider failures", self.search_failures)
        };
        self.status = format!(
            "{} results from {} providers; {query}; {coverage}; {failures}; {}. Press ? for details",
            self.search_results.len(),
            self.search_provider_reports,
            self.search_controls.depth.explanation()
        );
    }

    /// Return the selected transcript-search result.
    #[must_use]
    pub fn selected_search_result(
        &self,
    ) -> Option<&bcode_session_search::HydratedSessionSearchHit> {
        let index = self.list.selected_source_index()?;
        self.search_results.get(index)
    }

    /// Cycle discoverable match mode.
    pub fn cycle_search_match_mode(&mut self) {
        self.search_controls.cycle_match_mode();
        self.status = format!("Match mode: {:?}", self.search_controls.match_mode);
    }

    /// Toggle explicit ordinary/deep search intent.
    pub fn toggle_search_depth(&mut self) {
        self.search_controls.toggle_depth();
        self.search_controls
            .depth
            .explanation()
            .clone_into(&mut self.status);
    }

    /// Cycle deterministic search result ordering.
    pub fn cycle_search_sort(&mut self) {
        self.search_controls.cycle_sort();
        self.status = format!("Sort: {:?}", self.search_controls.sort);
    }

    /// Build the current bounded search request and explicit provider policy.
    pub fn search_request(
        &self,
        query: &str,
        continue_page: bool,
    ) -> Result<
        (
            bcode_session_search::SessionSearchRequest,
            bcode_session_search::SessionSearchPlanPolicy,
        ),
        String,
    > {
        let parsed =
            super::session_search::parse_search_input(query, self.search_controls.clone())?;
        let cursor = continue_page.then(|| self.search_cursor.clone()).flatten();
        Ok((
            parsed.controls.request(parsed.query, cursor),
            parsed.controls.depth.policy(),
        ))
    }

    /// Return whether another provider cursor is available.
    #[must_use]
    pub const fn has_next_search_page(&self) -> bool {
        self.search_cursor.is_some()
    }

    /// Return bounded preview for the selected result.
    #[must_use]
    pub fn search_preview(&self) -> &str {
        &self.search_preview
    }

    fn refresh_search_preview(&mut self) {
        self.search_preview = self
            .selected_search_result()
            .and_then(|result| result.hit.preview.as_deref())
            .unwrap_or("No canonical preview available")
            .chars()
            .take(512)
            .collect();
    }

    /// Arm or consume the explicit two-step canonical migration confirmation.
    pub const fn confirm_canonical_migration(&mut self) -> bool {
        if self.migration_confirmation_armed {
            self.migration_confirmation_armed = false;
            true
        } else {
            self.migration_confirmation_armed = true;
            false
        }
    }

    /// Record the latest transient aggregate canonical migration identity.
    pub fn set_bulk_migration_operation_id(&mut self, operation_id: String) {
        self.bulk_migration_operation_id = Some(operation_id);
    }

    /// Take the latest transient aggregate canonical migration identity for cancellation.
    pub const fn take_bulk_migration_operation_id(&mut self) -> Option<String> {
        self.bulk_migration_operation_id.take()
    }

    /// Record the latest transient aggregate derived backfill identity.
    pub fn set_search_backfill_operation_id(&mut self, operation_id: String) {
        self.search_backfill_operation_id = Some(operation_id);
    }

    /// Take the latest transient aggregate derived backfill identity for cancellation.
    pub const fn take_search_backfill_operation_id(&mut self) -> Option<String> {
        self.search_backfill_operation_id.take()
    }

    /// Leave transcript-search results and rebuild local catalog filtering.
    pub fn close_search_results(&mut self) {
        self.search_results.clear();
        self.search_cursor = None;
        self.search_preview.clear();
        self.migration_confirmation_armed = false;
        self.mode = SessionPickerMode::Filter;
        self.list = FilteredListState::new(self.sessions.len());
        self.refresh_filter();
        "Transcript search closed".clone_into(&mut self.status);
    }

    /// Return visible list items.
    #[must_use]
    pub fn list_items(&self, muted: Style) -> Vec<ListItem> {
        if self.mode == SessionPickerMode::TranscriptSearch {
            if self.search_results.is_empty() {
                return vec![empty_item("No transcript matches", muted)];
            }
            return self
                .search_results
                .iter()
                .map(|result| {
                    let title = self
                        .sessions
                        .iter()
                        .find(|session| session.id == result.hit.locator.session_id)
                        .map(SessionSummary::display_title);
                    search_result_item(result, title, muted)
                })
                .collect();
        }
        if self.list.indices().is_empty() {
            return vec![empty_item(&self.empty_message, muted)];
        }
        self.list
            .indices()
            .iter()
            .map(|index| session_item(&self.sessions[*index], muted))
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
    pub fn select_visible(&mut self, row: usize) -> bool {
        let selected = self.list.select_visible(row);
        if selected && self.mode == SessionPickerMode::TranscriptSearch {
            self.refresh_search_preview();
        }
        selected
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
    }

    /// Move selection down.
    pub fn select_next(&mut self) {
        self.list.select_next();
        if self.mode == SessionPickerMode::TranscriptSearch {
            self.refresh_search_preview();
        }
    }

    /// Move selection up.
    pub fn select_previous(&mut self) {
        self.list.select_previous();
        if self.mode == SessionPickerMode::TranscriptSearch {
            self.refresh_search_preview();
        }
    }
}

fn search_result_item(
    result: &bcode_session_search::HydratedSessionSearchHit,
    canonical_title: Option<&str>,
    muted: Style,
) -> ListItem {
    let preview = result.hit.preview.as_deref().unwrap_or("<no preview>");
    let timestamp = result
        .event
        .as_deref()
        .map(|event| format!(" @{}", event.timestamp_ms))
        .unwrap_or_default();
    let degraded = (result.outcome != bcode_session_search::SearchHitHydrationOutcome::Hydrated)
        .then_some(format!(" [{:?}]", result.outcome))
        .unwrap_or_default();
    let title = canonical_title.unwrap_or("<canonical title unavailable>");
    ListItem::new(Line::from_spans(vec![
        Span::styled(
            format!(
                "{title}  {} #{} {:?}{timestamp}",
                result.hit.locator.session_id, result.hit.locator.sequence, result.hit.content_kind
            ),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!(
                "{} rank {}{degraded}",
                result.hit.provider_id, result.hit.provider_rank
            ),
            muted,
        ),
        Span::raw("  "),
        Span::raw(preview),
        Span::raw(if result.hit.preview_truncated {
            " [truncated]"
        } else {
            ""
        }),
    ]))
}

fn session_item(session: &SessionSummary, muted: Style) -> ListItem {
    let name = session.display_title();
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
    let id = session.id.to_string();
    let cwd = display_from_current_dir(&session.working_directory).to_string();
    ListItem::new(Line::from_spans(vec![
        Span::styled(display_name, Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(id, muted),
        Span::raw("  "),
        Span::styled(cwd, muted),
    ]))
}

fn session_matches(session: &SessionSummary, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    session.display_title().to_ascii_lowercase().contains(query)
        || session.id.to_string().contains(query)
        || session.import.as_ref().is_some_and(|import| {
            import.source_id.to_ascii_lowercase().contains(query)
                || import
                    .source_display_name
                    .to_ascii_lowercase()
                    .contains(query)
        })
        || display_from_current_dir(&session.working_directory)
            .to_string()
            .to_ascii_lowercase()
            .contains(query)
}

fn empty_item(message: &str, muted: Style) -> ListItem {
    ListItem::new(Line::from_spans(vec![Span::styled(
        message.to_owned(),
        muted,
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(title: &str, working_directory: &str) -> SessionSummary {
        SessionSummary {
            id: SessionId::new(),
            name: Some(title.to_owned()),
            explicit_name: None,
            derived_title: None,
            title_source: bcode_session_models::SessionTitleSource::Explicit,
            client_count: 0,
            created_at_ms: 1,
            updated_at_ms: 1,
            working_directory: working_directory.into(),
            import: None,
            execution: None,
        }
    }

    fn sample_search_result(
        outcome: bcode_session_search::SearchHitHydrationOutcome,
    ) -> bcode_session_search::HydratedSessionSearchHit {
        let session_id = SessionId::new();
        bcode_session_search::HydratedSessionSearchHit {
            hit: bcode_session_search::SessionSearchHit {
                locator: bcode_session_search::SessionSearchLocator {
                    session_id,
                    sequence: 7,
                    record_id: Some("result".to_owned()),
                },
                content_kind: bcode_session_search::SearchContentKind::AssistantMessage,
                matched_field: bcode_session_search::SearchField::Text,
                provider_id: "provider".to_owned(),
                provider_rank: 1,
                provider_score: None,
                preview: Some("bounded preview".to_owned()),
                preview_truncated: false,
            },
            outcome,
            event: (outcome == bcode_session_search::SearchHitHydrationOutcome::Hydrated).then(
                || {
                    Box::new(bcode_session_models::SessionEvent {
                        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                        sequence: 7,
                        timestamp_ms: 99,
                        session_id,
                        provenance: None,
                        kind: bcode_session_models::SessionEventKind::AssistantMessage {
                            text: "canonical".to_owned(),
                        },
                    })
                },
            ),
            message: None,
        }
    }

    use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::geometry::Point;

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent::new(kind, Point::new(x, y))
    }

    #[test]
    fn transcript_search_mouse_row_selects_result_and_refreshes_preview() {
        let mut app = SessionPickerApp::new(Vec::new());
        let first = sample_search_result(bcode_session_search::SearchHitHydrationOutcome::Hydrated);
        let mut second =
            sample_search_result(bcode_session_search::SearchHitHydrationOutcome::Hydrated);
        second.hit.provider_rank = 2;
        second.hit.preview = Some("second preview".to_owned());
        let response = bcode_session_search::FederatedSessionSearchResponse {
            hits: vec![first.hit.clone(), second.hit.clone()],
            query_complete: true,
            coverage_complete: true,
            providers: Vec::new(),
            failures: Vec::new(),
        };
        app.set_search_results(&response, vec![first, second]);
        let row = super::super::picker_mouse::picker_row_from_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            4,
            6,
        ))
        .expect("second visible row");

        assert!(app.select_visible(row));
        assert_eq!(
            app.selected_search_result()
                .map(|result| result.hit.provider_rank),
            Some(2)
        );
        assert!(app.search_preview().contains("second preview"));
    }

    #[test]
    fn transcript_search_mode_accepts_query_before_results_exist() {
        let mut app = SessionPickerApp::new(Vec::new());
        app.start_transcript_search();

        assert_eq!(app.mode(), SessionPickerMode::TranscriptSearch);
        assert!(app.selected_search_result().is_none());
        assert!(app.status().contains("press Enter"));
    }

    #[test]
    fn transcript_results_preserve_all_rows_coverage_and_selection() {
        let mut app = SessionPickerApp::new(Vec::new());
        let first = sample_search_result(bcode_session_search::SearchHitHydrationOutcome::Hydrated);
        let mut canonical = summary("Canonical title", "/workspace");
        canonical.id = first.hit.locator.session_id;
        app.replace_sessions(vec![canonical]);
        let mut second =
            sample_search_result(bcode_session_search::SearchHitHydrationOutcome::SessionMissing);
        second.hit.provider_rank = 2;
        let response = bcode_session_search::FederatedSessionSearchResponse {
            hits: vec![first.hit.clone(), second.hit.clone()],
            query_complete: false,
            coverage_complete: false,
            providers: Vec::new(),
            failures: Vec::new(),
        };
        app.set_search_results(&response, vec![first.clone(), second]);

        assert_eq!(app.mode(), SessionPickerMode::TranscriptSearch);
        let items = app.list_items(Style::new());
        assert_eq!(items.len(), 2);
        let first_text = items[0]
            .line()
            .spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(first_text.contains("Canonical title"));
        assert!(first_text.contains("@99"));
        assert!(app.status().contains("query incomplete"));
        assert_eq!(app.selected_search_result(), Some(&first));
        app.select_next();
        assert_eq!(
            app.selected_search_result()
                .map(|result| result.hit.provider_rank),
            Some(2)
        );
        app.close_search_results();
        assert_eq!(app.mode(), SessionPickerMode::Filter);
    }

    #[test]
    fn transcript_status_keeps_query_coverage_and_failures_independent() {
        let cases = [
            (
                true,
                true,
                0,
                [
                    "query complete",
                    "searchable coverage complete",
                    "no provider failures",
                ],
            ),
            (
                false,
                true,
                0,
                [
                    "query incomplete",
                    "searchable coverage complete",
                    "no provider failures",
                ],
            ),
            (
                true,
                false,
                0,
                [
                    "query complete",
                    "searchable coverage incomplete",
                    "no provider failures",
                ],
            ),
            (
                true,
                true,
                1,
                [
                    "query complete",
                    "searchable coverage complete",
                    "1 provider failures",
                ],
            ),
        ];
        for (query_complete, coverage_complete, failures, expected) in cases {
            let mut app = SessionPickerApp::new(Vec::new());
            let response = bcode_session_search::FederatedSessionSearchResponse {
                hits: Vec::new(),
                query_complete,
                coverage_complete,
                providers: Vec::new(),
                failures: (0..failures)
                    .map(|_| bcode_session_search::SessionSearchProviderFailure {
                        plugin_id: "provider".to_owned(),
                        error: bcode_session_search::SessionSearchServiceError {
                            code: bcode_session_search::SearchErrorCode::ProviderUnavailable,
                            message: "unavailable".to_owned(),
                            retryable: true,
                        },
                        stage: bcode_session_search::SessionSearchProviderStage::Execution,
                        elapsed_ms: 0,
                        content: Vec::new(),
                    })
                    .collect(),
            };

            app.set_search_results(&response, Vec::new());

            for phrase in expected {
                assert!(
                    app.status().contains(phrase),
                    "missing {phrase}: {}",
                    app.status()
                );
            }
        }
    }

    #[test]
    fn dedicated_search_state_controls_request_preview_and_human_status() {
        let mut app = SessionPickerApp::new(vec![summary("one", "/one")]);
        app.start_transcript_search();
        app.cycle_search_match_mode();
        app.toggle_search_depth();
        app.cycle_search_sort();
        let (request, policy) = app.search_request("needle", false).expect("search request");
        assert!(matches!(
            request.query,
            bcode_session_search::SessionSearchQuery::Text {
                mode: bcode_session_search::TextMatchMode::Phrase,
                ..
            }
        ));
        assert_eq!(
            request.sort,
            bcode_session_search::SessionSearchSort::NewestFirst
        );
        assert_eq!(
            policy.execution_class,
            bcode_session_search::SessionSearchExecutionClass::Deep
        );

        let hit = sample_search_result(bcode_session_search::SearchHitHydrationOutcome::Hydrated);
        let response = bcode_session_search::FederatedSessionSearchResponse {
            hits: vec![hit.hit.clone()],
            query_complete: true,
            coverage_complete: true,
            providers: Vec::new(),
            failures: Vec::new(),
        };
        app.set_search_results(&response, vec![hit]);
        assert!(app.status().contains("searchable coverage complete"));
        assert!(!app.status().contains("query_complete="));
        assert!(!app.search_preview().is_empty());
    }

    #[test]
    fn maintenance_controls_require_confirmation_and_retain_cancellable_operations() {
        let mut app = SessionPickerApp::new(Vec::new());
        assert!(!app.confirm_canonical_migration());
        assert!(app.confirm_canonical_migration());
        assert!(!app.confirm_canonical_migration());

        app.set_bulk_migration_operation_id("migration-1".to_owned());
        app.set_search_backfill_operation_id("backfill-1".to_owned());
        assert_eq!(
            app.take_bulk_migration_operation_id().as_deref(),
            Some("migration-1")
        );
        assert!(app.take_bulk_migration_operation_id().is_none());
        assert_eq!(
            app.take_search_backfill_operation_id().as_deref(),
            Some("backfill-1")
        );
    }

    #[test]
    fn filter_matches_only_portable_summary_metadata() {
        let source_session_id = SessionId::new();
        let mut session = summary("Visible title", "/workspace/project");
        session.import = Some(bcode_session_models::SessionImportSummary {
            source_id: "opencode".to_owned(),
            source_display_name: "OpenCode".to_owned(),
            external_session_id: "external".to_owned(),
            imported_at_ms: 1,
        });
        assert!(session_matches(&session, "visible"));
        assert!(session_matches(&session, &session.id.to_string()));
        assert!(session_matches(&session, "workspace/project"));
        assert!(session_matches(&session, "opencode"));
        assert!(session_matches(&session, "parent title"));
        assert!(session_matches(&session, &source_session_id.to_string()));
        assert!(!session_matches(&session, "transcript-only-term"));
        assert!(!session_matches(&session, "external"));
    }
}
