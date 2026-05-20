//! BMUX backend app state.

use std::time::Instant;

use bcode_session_models::{
    ModelTurnOutcome, SessionEvent, SessionEventKind, SessionHistoryCursor, SessionId,
    SessionInputHistoryEntry,
};
use bcode_skill_models::SkillSource;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::diff::{DiffFileSummary, DiffLine, DiffLineKind};

use super::IDLE_REDRAW_INTERVAL;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffPanelState {
    Hidden,
    Visible,
}

/// State owned by the BMUX-native backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BmuxApp {
    session_id: Option<SessionId>,
    composer: TextEditBuffer,
    input_history: Vec<String>,
    input_history_index: Option<usize>,
    input_history_draft: Option<String>,
    transcript: Vec<TranscriptItem>,
    changed_files: Vec<DiffFileSummary>,
    diff_details: Vec<Vec<DiffLine>>,
    selected_diff_file: Option<usize>,
    diff_panel: DiffPanelState,
    diff_scroll_offset: usize,
    diff_lines: Vec<DiffLine>,
    pending_submissions: Vec<PendingSubmission>,
    pending_submission: Option<String>,
    scroll_offset: usize,
    transcript_max_scroll_offset: usize,
    scroll_preserve_max_offset: Option<usize>,
    older_history_cursor: Option<SessionHistoryCursor>,
    older_history_reveal_request: Option<usize>,
    loading_older_history: bool,
    activity: ActivityState,
    status: String,
    should_exit: bool,
    cursor_visible: bool,
    last_cursor_toggle: Instant,
}

impl BmuxApp {
    /// Create BMUX backend state with replayed session data.
    #[must_use]
    pub(super) fn new_with_history(
        session_id: Option<SessionId>,
        history: &[SessionEvent],
        input_history: &[SessionInputHistoryEntry],
        has_older_history: bool,
    ) -> Self {
        let mut app = Self {
            session_id,
            composer: TextEditBuffer::new(),
            input_history: input_history
                .iter()
                .map(|entry| entry.text.clone())
                .collect(),
            input_history_index: None,
            input_history_draft: None,
            transcript: Vec::new(),
            changed_files: Vec::new(),
            diff_details: Vec::new(),
            selected_diff_file: None,
            diff_panel: DiffPanelState::Hidden,
            diff_scroll_offset: 0,
            diff_lines: Vec::new(),
            pending_submissions: Vec::new(),
            pending_submission: None,
            scroll_offset: 0,
            transcript_max_scroll_offset: 0,
            scroll_preserve_max_offset: None,
            older_history_cursor: oldest_history_cursor(history, has_older_history),
            older_history_reveal_request: None,
            loading_older_history: false,
            activity: ActivityState::Idle,
            status: String::from("BMUX backend connected. Enter submits; Esc/Ctrl-C exits."),
            should_exit: false,
            cursor_visible: true,
            last_cursor_toggle: Instant::now(),
        };
        app.absorb_history(history);
        app
    }

    /// Return the active session id, if one was provided.
    #[must_use]
    pub(super) const fn session_id(&self) -> Option<SessionId> {
        self.session_id
    }

    /// Return the composer buffer.
    #[must_use]
    pub(super) const fn composer(&self) -> &TextEditBuffer {
        &self.composer
    }

    /// Return the composer buffer mutably.
    pub(super) const fn composer_mut(&mut self) -> &mut TextEditBuffer {
        &mut self.composer
    }

    /// Return transcript items.
    #[must_use]
    pub(super) fn transcript(&self) -> &[TranscriptItem] {
        &self.transcript
    }

    /// Return changed-file summaries inferred from edit tool calls.
    #[must_use]
    pub(super) fn changed_files(&self) -> &[DiffFileSummary] {
        &self.changed_files
    }

    /// Return whether the diff panel is visible.
    #[must_use]
    pub(super) fn diff_visible(&self) -> bool {
        self.diff_panel == DiffPanelState::Visible
    }

    /// Toggle diff panel visibility.
    pub(super) const fn toggle_diff_visible(&mut self) -> bool {
        self.diff_panel = match self.diff_panel {
            DiffPanelState::Hidden => DiffPanelState::Visible,
            DiffPanelState::Visible => DiffPanelState::Hidden,
        };
        true
    }

    /// Return detailed diff lines inferred from edit tool calls.
    #[must_use]
    pub(super) fn diff_lines(&self) -> &[DiffLine] {
        self.selected_diff_file
            .and_then(|index| self.diff_details.get(index).map(Vec::as_slice))
            .unwrap_or(&self.diff_lines)
    }

    /// Return diff scroll offset.
    #[must_use]
    pub(super) const fn diff_scroll_offset(&self) -> usize {
        self.diff_scroll_offset
    }

    /// Scroll diff preview up.
    pub(super) fn scroll_diff_up(&mut self, rows: usize) -> bool {
        if rows == 0 || self.diff_lines.is_empty() {
            return false;
        }
        let previous = self.diff_scroll_offset;
        self.diff_scroll_offset = self
            .diff_scroll_offset
            .saturating_add(rows)
            .min(self.diff_lines.len());
        self.diff_scroll_offset != previous
    }

    /// Scroll diff preview down.
    pub(super) const fn scroll_diff_down(&mut self, rows: usize) -> bool {
        let previous = self.diff_scroll_offset;
        self.diff_scroll_offset = self.diff_scroll_offset.saturating_sub(rows);
        self.diff_scroll_offset != previous
    }

    /// Select a changed-file diff detail.
    pub(super) const fn select_diff_file(&mut self, index: usize) -> bool {
        if index >= self.changed_files.len() {
            return false;
        }
        self.selected_diff_file = Some(index);
        self.diff_scroll_offset = 0;
        true
    }

    /// Select next changed file.
    pub(super) fn select_next_diff_file(&mut self) -> bool {
        if self.changed_files.is_empty() {
            return false;
        }
        let next = self.selected_diff_file.map_or(0, |index| {
            index.saturating_add(1).min(self.changed_files.len() - 1)
        });
        self.select_diff_file(next)
    }

    /// Select previous changed file.
    pub(super) fn select_previous_diff_file(&mut self) -> bool {
        if self.changed_files.is_empty() {
            return false;
        }
        let previous = self
            .selected_diff_file
            .map_or(0, |index| index.saturating_sub(1));
        self.select_diff_file(previous)
    }

    /// Move composer cursor to soft-wrapped row and column.
    pub(super) fn move_composer_to_wrapped_position(
        &mut self,
        width: usize,
        row: usize,
        col: usize,
    ) {
        self.composer
            .move_cursor_to_wrapped_position(width, row, col);
        self.wake_cursor();
    }

    /// Return pending submissions that have not been committed by the session stream.
    #[must_use]
    pub(super) fn pending_submissions(&self) -> &[PendingSubmission] {
        &self.pending_submissions
    }

    /// Return the number of transcript rows hidden below the viewport.
    #[must_use]
    pub(super) const fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Return whether older history may be available.
    #[must_use]
    pub(super) const fn has_older_history(&self) -> bool {
        self.older_history_cursor.is_some()
    }

    /// Return whether an older-history request is in flight.
    #[must_use]
    pub(super) const fn loading_older_history(&self) -> bool {
        self.loading_older_history
    }

    /// Mark older history as loading or idle.
    pub(super) const fn set_loading_older_history(&mut self, loading: bool) {
        self.loading_older_history = loading;
        if !loading {
            self.older_history_reveal_request = None;
        }
    }

    /// Return the cursor for loading older history.
    #[must_use]
    pub(super) const fn older_history_cursor(&self) -> Option<SessionHistoryCursor> {
        self.older_history_cursor
    }

    /// Return whether an older-history request should be started.
    #[must_use]
    pub(super) const fn should_load_older_history(&self) -> bool {
        self.older_history_cursor.is_some()
            && !self.loading_older_history
            && self.older_history_reveal_request.is_some()
    }

    /// Return the current activity state.
    #[must_use]
    pub(super) const fn activity(&self) -> &ActivityState {
        &self.activity
    }

    /// Return the current status line.
    #[must_use]
    pub(super) fn status(&self) -> &str {
        &self.status
    }

    /// Append a system-style transcript note.
    pub(super) fn push_system_note(&mut self, text: String) {
        self.transcript.push(TranscriptItem::new("System", text));
    }

    /// Replace the current status line.
    pub(super) fn set_status(&mut self, status: String) {
        self.status = status;
    }

    /// Store the current composer text as a pending submission and clear input.
    pub(super) fn stage_submission(&mut self) {
        let text = self.composer.text().to_owned();
        self.pending_submission = Some(text.clone());
        self.pending_submissions
            .push(PendingSubmission::new(text.clone()));
        self.input_history.push(text);
        self.input_history_index = None;
        self.input_history_draft = None;
        self.composer.clear();
    }

    /// Return the currently pending submission.
    pub(super) fn take_pending_submission(&mut self) -> String {
        self.pending_submission.take().unwrap_or_default()
    }

    /// Remove the pending submission after an intercepted slash command.
    pub(super) fn clear_pending_submission(&mut self) {
        if let Some(text) = self.pending_submission.take() {
            self.remove_pending_submission(&text);
        }
    }

    /// Mark the oldest pending submission as queued by the server.
    pub(super) fn mark_pending_submission_queued(&mut self, queue_position: Option<u32>) {
        if let Some(pending) = self.pending_submissions.first_mut() {
            pending.state = PendingSubmissionState::Queued { queue_position };
        }
    }

    /// Mark the oldest pending submission as sent to the server.
    pub(super) fn mark_pending_submission_sent(&mut self) {
        if let Some(pending) = self.pending_submissions.first_mut() {
            pending.state = PendingSubmissionState::Sent;
        }
    }

    /// Remove the oldest pending submission and restore it into the composer.
    pub(super) fn restore_pending_submission(&mut self) {
        if let Some(text) = self.pending_submission.take() {
            self.remove_pending_submission(&text);
            self.composer.insert_str(&text);
        }
        self.wake_cursor();
    }

    /// Show the previous input-history entry, if available.
    pub(super) fn previous_input_history(&mut self) -> bool {
        if self.input_history.is_empty() {
            return false;
        }
        let next_index = self.input_history_index.map_or_else(
            || self.input_history.len().saturating_sub(1),
            |index| index.saturating_sub(1),
        );
        if self.input_history_index.is_none() {
            self.input_history_draft = Some(self.composer.text().to_owned());
        }
        self.input_history_index = Some(next_index);
        let text = self.input_history[next_index].clone();
        self.replace_composer_with(&text);
        true
    }

    /// Show the next input-history entry, or restore the draft.
    pub(super) fn next_input_history(&mut self) -> bool {
        let Some(index) = self.input_history_index else {
            return false;
        };
        if index + 1 < self.input_history.len() {
            let next_index = index + 1;
            self.input_history_index = Some(next_index);
            let text = self.input_history[next_index].clone();
            self.replace_composer_with(&text);
        } else {
            self.input_history_index = None;
            let draft = self.input_history_draft.take().unwrap_or_default();
            self.replace_composer_with(&draft);
        }
        true
    }

    /// Scroll transcript up by rendered rows.
    pub(super) fn scroll_transcript_up(&mut self, rows: usize) -> bool {
        if rows == 0 {
            return false;
        }
        let previous = self.scroll_offset;
        let previous_request = self.older_history_reveal_request;
        let desired = self.scroll_offset.saturating_add(rows);
        self.scroll_offset = desired.min(self.transcript_max_scroll_offset);
        if desired > self.transcript_max_scroll_offset {
            self.request_older_history_load(
                desired.saturating_sub(self.transcript_max_scroll_offset),
            );
        }
        self.scroll_offset != previous || self.older_history_reveal_request != previous_request
    }

    /// Scroll transcript down by rendered rows.
    pub(super) const fn scroll_transcript_down(&mut self, rows: usize) -> bool {
        let previous = self.scroll_offset;
        self.scroll_offset = self.scroll_offset.saturating_sub(rows);
        self.scroll_offset != previous
    }

    /// Pin transcript to the newest rows.
    pub(super) const fn scroll_transcript_to_bottom(&mut self) -> bool {
        let changed = self.scroll_offset != 0;
        self.scroll_offset = 0;
        self.older_history_reveal_request = None;
        changed
    }

    /// Sync cached rendered transcript scroll bounds from the latest frame.
    pub(super) fn sync_transcript_scroll_max(&mut self, max_scroll_offset: usize) {
        let previous_max = self.transcript_max_scroll_offset;
        self.transcript_max_scroll_offset = max_scroll_offset;
        if let Some(requested_rows) = self.older_history_reveal_request.take() {
            let inserted_rows = max_scroll_offset.saturating_sub(previous_max);
            let reveal_rows = requested_rows.min(inserted_rows);
            self.scroll_offset = self.scroll_offset.saturating_add(reveal_rows);
        }
        if let Some(preserve_max) = self.scroll_preserve_max_offset.take()
            && self.scroll_offset > 0
        {
            let appended_rows = max_scroll_offset.saturating_sub(preserve_max);
            self.scroll_offset = self.scroll_offset.saturating_add(appended_rows);
        }
        self.scroll_offset = self.scroll_offset.min(self.transcript_max_scroll_offset);
    }

    fn request_older_history_load(&mut self, reveal_rows: usize) {
        if self.older_history_cursor.is_none() || self.loading_older_history {
            return;
        }
        let reveal_rows = reveal_rows.max(1);
        self.older_history_reveal_request = Some(
            self.older_history_reveal_request
                .map_or(reveal_rows, |requested| requested.max(reveal_rows)),
        );
    }

    /// Absorb replayed history events.
    pub(super) fn absorb_history(&mut self, events: &[SessionEvent]) {
        for event in events {
            self.absorb_session_event(event);
        }
    }

    /// Prepend older history and preserve the current viewport.
    pub(super) fn prepend_older_history(&mut self, events: &[SessionEvent], has_more: bool) {
        if events.is_empty() {
            self.older_history_cursor = None;
            self.older_history_reveal_request = None;
            self.loading_older_history = false;
            "start of session history".clone_into(&mut self.status);
            return;
        }

        let mut older = transcript_items_from_events(events);
        merge_transcript_boundary(&mut older, &mut self.transcript);
        older.append(&mut self.transcript);
        self.transcript = older;
        self.older_history_cursor = oldest_history_cursor(events, has_more);
        self.loading_older_history = false;
        if self.older_history_cursor.is_some() {
            "loaded older history".clone_into(&mut self.status);
        } else {
            "start of session history".clone_into(&mut self.status);
        }
    }

    /// Absorb one live session event.
    pub(super) fn absorb_session_event(&mut self, event: &SessionEvent) {
        if self.scroll_offset > 0 && event_affects_transcript_rows(event) {
            self.scroll_preserve_max_offset = Some(self.transcript_max_scroll_offset);
        }
        match &event.kind {
            SessionEventKind::UserMessage { text, .. } => self.push_live_user_message(text),
            SessionEventKind::AssistantDelta { text } => self.push_live_assistant_delta(text),
            SessionEventKind::AssistantMessage { text } => {
                self.finish_streaming_item("Assistant", text);
            }
            SessionEventKind::SystemMessage { text } => self.push_system_message(text),
            SessionEventKind::ToolCallRequested {
                tool_call_id,
                tool_name,
                arguments_json,
            } => {
                self.record_diff_summary(tool_name, arguments_json);
                self.push_tool_request(tool_call_id, tool_name, arguments_json);
            }
            SessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
            } => {
                self.activity = ActivityState::Thinking;
                self.push_tool_result(tool_call_id, result, *is_error);
            }
            SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                tool_name,
                arguments_json,
            } => {
                self.push_permission_request(
                    permission_id,
                    tool_call_id,
                    tool_name,
                    arguments_json,
                );
            }
            SessionEventKind::PermissionResolved {
                permission_id,
                approved,
            } => {
                self.activity = ActivityState::Thinking;
                self.set_permission_status(permission_id, *approved);
            }
            SessionEventKind::ModelChanged { provider, model } => {
                self.status = format!("model: {provider}/{model}");
            }
            SessionEventKind::ModelTurnStarted { .. } => {
                self.activity = ActivityState::Thinking;
                "thinking".clone_into(&mut self.status);
            }
            SessionEventKind::ModelTurnFinished {
                outcome, message, ..
            } => self.finish_model_turn(*outcome, message.as_deref()),
            SessionEventKind::ModelUsage { turn_id, usage } => {
                self.push_model_usage(turn_id, usage);
            }
            SessionEventKind::ContextCompacted { summary, .. } => self.push_compaction(summary),
            SessionEventKind::SessionRenamed { name } => {
                self.set_session_name_status(name.as_deref());
            }
            SessionEventKind::SkillInvoked {
                skill_id,
                arguments,
                source,
                ..
            } => self.push_skill_invoked(skill_id, arguments, source.as_ref()),
            SessionEventKind::SkillSuggested {
                skill_id, reason, ..
            } => self.push_skill_suggested(skill_id, reason.as_deref()),
            SessionEventKind::SkillActivated { skill_id, .. } => {
                self.status = format!("activated skill: {skill_id}");
            }
            SessionEventKind::SkillDeactivated { skill_id, .. } => {
                self.status = format!("deactivated skill: {skill_id}");
            }
            SessionEventKind::SkillContextLoaded {
                skill_id,
                bytes_loaded,
                truncated,
                ..
            } => self.set_skill_context_status(skill_id, *bytes_loaded, *truncated),
            SessionEventKind::SkillInvocationFailed {
                skill_id, error, ..
            } => self.push_skill_error(skill_id, error),
            SessionEventKind::AssistantReasoningDelta { text } => {
                self.activity = ActivityState::Streaming;
                self.push_streaming_item("Reasoning", text);
            }
            SessionEventKind::AssistantReasoningMessage { text } => {
                self.finish_streaming_item("Reasoning", text);
            }
            SessionEventKind::TraceEvent { .. }
            | SessionEventKind::SessionCreated { .. }
            | SessionEventKind::ClientAttached { .. }
            | SessionEventKind::ClientDetached { .. }
            | SessionEventKind::AgentChanged { .. } => {}
        }
    }

    /// Return whether the composer cursor should be visible.
    #[must_use]
    pub(super) const fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Reset cursor blink state after input.
    pub(super) fn wake_cursor(&mut self) {
        self.cursor_visible = true;
        self.last_cursor_toggle = Instant::now();
    }

    /// Advance time-based UI state.
    pub(super) fn tick(&mut self) -> bool {
        if self.last_cursor_toggle.elapsed() < IDLE_REDRAW_INTERVAL {
            return false;
        }
        self.cursor_visible = !self.cursor_visible;
        self.last_cursor_toggle = Instant::now();
        true
    }

    /// Return whether the backend should exit.
    #[must_use]
    pub(super) const fn should_exit(&self) -> bool {
        self.should_exit
    }

    /// Request backend shutdown.
    pub(super) const fn request_exit(&mut self) {
        self.should_exit = true;
    }

    /// Replace composer contents.
    pub(super) fn replace_composer_with(&mut self, text: &str) {
        self.composer.clear();
        self.composer.insert_str(text);
        self.wake_cursor();
    }

    fn remove_pending_submission(&mut self, text: &str) {
        if let Some(index) = self
            .pending_submissions
            .iter()
            .position(|pending| pending.text == text)
        {
            self.pending_submissions.remove(index);
        }
    }

    fn push_live_user_message(&mut self, text: &str) {
        self.activity = ActivityState::Thinking;
        self.push_user_message(text);
    }

    fn push_live_assistant_delta(&mut self, text: &str) {
        self.activity = ActivityState::Streaming;
        self.push_streaming_item("Assistant", text);
    }

    fn push_user_message(&mut self, text: &str) {
        self.remove_pending_submission(text);
        self.transcript
            .push(TranscriptItem::new("You", text.to_owned()));
    }

    fn push_system_message(&mut self, text: &str) {
        self.transcript
            .push(TranscriptItem::new("System", text.to_owned()));
    }

    fn push_streaming_item(&mut self, role: &'static str, text: &str) {
        if let Some(last) = self.transcript.last_mut()
            && last.role == role
            && last.streaming
        {
            last.text.push_str(text);
            return;
        }
        self.transcript
            .push(TranscriptItem::new_streaming(role, text.to_owned()));
    }

    fn finish_streaming_item(&mut self, role: &'static str, text: &str) {
        if let Some(last) = self.transcript.last_mut()
            && last.role == role
            && last.streaming
        {
            last.text.clear();
            last.text.push_str(text);
            last.streaming = false;
            return;
        }
        self.transcript
            .push(TranscriptItem::new(role, text.to_owned()));
    }

    fn push_tool_request(&mut self, tool_call_id: &str, tool_name: &str, arguments_json: &str) {
        self.record_diff_summary(tool_name, arguments_json);
        self.transcript
            .push(tool_request_item(tool_call_id, tool_name, arguments_json));
        self.activity = ActivityState::RunningTool {
            name: tool_name.to_owned(),
        };
        self.status = format!("running tool {tool_name}");
    }

    fn record_diff_summary(&mut self, tool_name: &str, arguments_json: &str) {
        let Some((summary, lines)) = diff_from_tool_request(tool_name, arguments_json) else {
            return;
        };
        let path = summary.display_path();
        if let Some(existing_index) = self
            .changed_files
            .iter()
            .position(|existing| existing.display_path() == path)
        {
            self.changed_files[existing_index] = summary;
            if let Some(existing_lines) = self.diff_details.get_mut(existing_index) {
                *existing_lines = lines;
            }
            self.selected_diff_file = Some(existing_index);
        } else {
            self.changed_files.push(summary);
            self.diff_details.push(lines);
            self.selected_diff_file = Some(self.changed_files.len().saturating_sub(1));
        }
        self.diff_scroll_offset = 0;
        self.diff_lines = self
            .diff_details
            .iter()
            .flat_map(|detail| detail.iter().cloned())
            .collect();
    }

    fn push_tool_result(&mut self, tool_call_id: &str, result: &str, is_error: bool) {
        let label = if is_error { "Tool error" } else { "Tool" };
        self.transcript.push(TranscriptItem::new(
            label,
            format!(
                "result for {tool_call_id}\n{}",
                truncate_block(result, 4_000)
            ),
        ));
        if is_error {
            "tool failed".clone_into(&mut self.status);
        } else {
            "tool finished".clone_into(&mut self.status);
        }
    }

    fn push_permission_request(
        &mut self,
        permission_id: &str,
        tool_call_id: &str,
        tool_name: &str,
        arguments_json: &str,
    ) {
        self.transcript.push(TranscriptItem::new(
            "Permission",
            format!(
                "waiting for approval: {tool_name}\nPermission: {permission_id}\nCall: {tool_call_id}\nArguments:\n{}",
                pretty_jsonish(arguments_json)
            ),
        ));
        self.activity = ActivityState::WaitingPermission {
            name: tool_name.to_owned(),
        };
        self.status = format!("waiting for permission: {tool_name}");
    }

    fn set_permission_status(&mut self, permission_id: &str, approved: bool) {
        let status = if approved {
            "permission approved"
        } else {
            "permission denied"
        };
        status.clone_into(&mut self.status);
        self.transcript.push(TranscriptItem::new(
            "Permission",
            format!("{status}: {permission_id}"),
        ));
    }

    fn push_model_usage(&mut self, turn_id: &str, usage: &bcode_session_models::SessionTokenUsage) {
        if let Some(tokens) = usage.metered_total_tokens() {
            self.status = format!("tokens: {tokens}");
        }
        self.transcript.push(TranscriptItem::new(
            "Usage",
            format!(
                "turn {turn_id}\ninput: {}\noutput: {}\ntotal: {}\nreasoning: {}",
                optional_u32(usage.input_tokens),
                optional_u32(usage.output_tokens),
                optional_u32(usage.metered_total_tokens()),
                optional_u32(usage.reasoning_tokens),
            ),
        ));
    }

    fn finish_model_turn(&mut self, outcome: ModelTurnOutcome, message: Option<&str>) {
        self.status = message.map_or_else(
            || model_turn_outcome_label(outcome).to_owned(),
            ToOwned::to_owned,
        );
        if let Some(last) = self.transcript.last_mut()
            && last.role == "Assistant"
        {
            last.streaming = false;
        }
        self.activity = ActivityState::Idle;
    }

    fn push_compaction(&mut self, summary: &str) {
        self.transcript.push(TranscriptItem::new(
            "Compaction",
            format!("context compacted: {summary}"),
        ));
    }

    fn set_session_name_status(&mut self, name: Option<&str>) {
        self.status = name.map_or_else(
            || "session renamed".to_owned(),
            |name| format!("session: {name}"),
        );
    }

    fn set_skill_context_status(
        &mut self,
        skill_id: &impl std::fmt::Display,
        bytes_loaded: usize,
        truncated: bool,
    ) {
        let suffix = if truncated { " truncated" } else { "" };
        self.status = format!("loaded skill context: {skill_id} ({bytes_loaded} bytes{suffix})");
    }

    fn push_skill_invoked(
        &mut self,
        skill_id: &impl std::fmt::Display,
        arguments: &str,
        source: Option<&SkillSource>,
    ) {
        let source =
            source.map_or_else(String::new, |source| format!("\nSource: {}", source.label));
        self.transcript.push(TranscriptItem::new(
            "Skill",
            format!("invoked {skill_id}{source}\nArguments: {arguments}"),
        ));
    }

    fn push_skill_suggested(&mut self, skill_id: &impl std::fmt::Display, reason: Option<&str>) {
        self.status = format!("suggested skill: {skill_id}");
        if let Some(reason) = reason {
            self.transcript.push(TranscriptItem::new(
                "Skill",
                format!("suggested {skill_id}\nReason: {reason}"),
            ));
        }
    }

    fn push_skill_error(&mut self, skill_id: &impl std::fmt::Display, error: &str) {
        self.transcript.push(TranscriptItem::new(
            "Skill error",
            format!("{skill_id}: {error}"),
        ));
    }
}

const fn event_affects_transcript_rows(event: &SessionEvent) -> bool {
    match &event.kind {
        SessionEventKind::UserMessage { .. }
        | SessionEventKind::AssistantDelta { .. }
        | SessionEventKind::AssistantMessage { .. }
        | SessionEventKind::SystemMessage { .. }
        | SessionEventKind::ToolCallRequested { .. }
        | SessionEventKind::ToolCallFinished { .. }
        | SessionEventKind::PermissionRequested { .. }
        | SessionEventKind::PermissionResolved { .. }
        | SessionEventKind::ModelUsage { .. }
        | SessionEventKind::ContextCompacted { .. }
        | SessionEventKind::SkillInvoked { .. }
        | SessionEventKind::SkillInvocationFailed { .. }
        | SessionEventKind::AssistantReasoningDelta { .. }
        | SessionEventKind::AssistantReasoningMessage { .. } => true,
        SessionEventKind::SkillSuggested { reason, .. } => reason.is_some(),
        SessionEventKind::SessionCreated { .. }
        | SessionEventKind::ClientAttached { .. }
        | SessionEventKind::ClientDetached { .. }
        | SessionEventKind::ModelChanged { .. }
        | SessionEventKind::AgentChanged { .. }
        | SessionEventKind::ModelTurnStarted { .. }
        | SessionEventKind::ModelTurnFinished { .. }
        | SessionEventKind::SessionRenamed { .. }
        | SessionEventKind::SkillActivated { .. }
        | SessionEventKind::SkillDeactivated { .. }
        | SessionEventKind::SkillContextLoaded { .. }
        | SessionEventKind::TraceEvent { .. } => false,
    }
}

fn transcript_items_from_events(events: &[SessionEvent]) -> Vec<TranscriptItem> {
    let mut items = Vec::new();
    for event in events {
        push_transcript_item_from_event(&mut items, event);
    }
    items
}

fn push_transcript_item_from_event(items: &mut Vec<TranscriptItem>, event: &SessionEvent) {
    match &event.kind {
        SessionEventKind::AssistantDelta { text } => {
            push_streaming_transcript_item(items, "Assistant", text);
        }
        SessionEventKind::AssistantMessage { text } => {
            finish_streaming_transcript_item(items, "Assistant", text);
        }
        SessionEventKind::AssistantReasoningDelta { text } => {
            push_streaming_transcript_item(items, "Reasoning", text);
        }
        SessionEventKind::AssistantReasoningMessage { text } => {
            finish_streaming_transcript_item(items, "Reasoning", text);
        }
        _ => {
            if let Some(item) = non_streaming_transcript_item_from_event(event) {
                items.push(item);
            }
        }
    }
}

fn push_streaming_transcript_item(items: &mut Vec<TranscriptItem>, role: &'static str, text: &str) {
    if let Some(last) = items.last_mut()
        && last.role == role
        && last.streaming
    {
        last.text.push_str(text);
        return;
    }
    items.push(TranscriptItem::new_streaming(role, text.to_owned()));
}

fn finish_streaming_transcript_item(
    items: &mut Vec<TranscriptItem>,
    role: &'static str,
    text: &str,
) {
    if let Some(last) = items.last_mut()
        && last.role == role
        && last.streaming
    {
        last.text.clear();
        last.text.push_str(text);
        last.streaming = false;
        return;
    }
    items.push(TranscriptItem::new(role, text.to_owned()));
}

fn merge_transcript_boundary(older: &mut Vec<TranscriptItem>, current: &mut Vec<TranscriptItem>) {
    let (Some(last_older), Some(first_current)) = (older.last_mut(), current.first()) else {
        return;
    };
    if last_older.role != first_current.role || !last_older.streaming {
        return;
    }
    if first_current.streaming {
        last_older.text.push_str(&first_current.text);
        current.remove(0);
    } else {
        older.pop();
    }
}

fn non_streaming_transcript_item_from_event(event: &SessionEvent) -> Option<TranscriptItem> {
    match &event.kind {
        SessionEventKind::UserMessage { text, .. } => {
            Some(TranscriptItem::new("You", text.clone()))
        }
        SessionEventKind::SystemMessage { text } => {
            Some(TranscriptItem::new("System", text.clone()))
        }
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            arguments_json,
        } => Some(tool_request_item(tool_call_id, tool_name, arguments_json)),
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            is_error,
        } => Some(TranscriptItem::new(
            if *is_error { "Tool error" } else { "Tool" },
            format!(
                "result for {tool_call_id}\n{}",
                truncate_block(result, 4_000)
            ),
        )),
        SessionEventKind::PermissionRequested {
            permission_id,
            tool_call_id,
            tool_name,
            arguments_json,
        } => Some(TranscriptItem::new(
            "Permission",
            format!(
                "waiting for approval: {tool_name}\nPermission: {permission_id}\nCall: {tool_call_id}\nArguments:\n{}",
                pretty_jsonish(arguments_json)
            ),
        )),
        SessionEventKind::ContextCompacted { summary, .. } => Some(TranscriptItem::new(
            "Compaction",
            format!("context compacted: {summary}"),
        )),
        SessionEventKind::SkillInvoked {
            skill_id,
            arguments,
            source,
            ..
        } => Some(TranscriptItem::new(
            "Skill",
            format!(
                "invoked {skill_id}{}\nArguments: {arguments}",
                source
                    .as_ref()
                    .map_or_else(String::new, |source| format!("\nSource: {}", source.label))
            ),
        )),
        SessionEventKind::SkillInvocationFailed {
            skill_id, error, ..
        } => Some(TranscriptItem::new(
            "Skill error",
            format!("{skill_id}: {error}"),
        )),
        SessionEventKind::AssistantDelta { .. }
        | SessionEventKind::AssistantMessage { .. }
        | SessionEventKind::AssistantReasoningDelta { .. }
        | SessionEventKind::AssistantReasoningMessage { .. }
        | SessionEventKind::PermissionResolved { .. }
        | SessionEventKind::ModelChanged { .. }
        | SessionEventKind::ModelTurnStarted { .. }
        | SessionEventKind::ModelTurnFinished { .. }
        | SessionEventKind::ModelUsage { .. }
        | SessionEventKind::SessionRenamed { .. }
        | SessionEventKind::SkillSuggested { .. }
        | SessionEventKind::SkillActivated { .. }
        | SessionEventKind::SkillDeactivated { .. }
        | SessionEventKind::SkillContextLoaded { .. }
        | SessionEventKind::TraceEvent { .. }
        | SessionEventKind::SessionCreated { .. }
        | SessionEventKind::ClientAttached { .. }
        | SessionEventKind::ClientDetached { .. }
        | SessionEventKind::AgentChanged { .. } => None,
    }
}

fn oldest_history_cursor(
    events: &[SessionEvent],
    has_older_history: bool,
) -> Option<SessionHistoryCursor> {
    if !has_older_history {
        return None;
    }
    let oldest_sequence = events.first()?.sequence;
    if oldest_sequence == 0 {
        None
    } else {
        Some(SessionHistoryCursor {
            sequence: oldest_sequence.saturating_sub(1),
        })
    }
}

fn optional_u32(value: Option<u32>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

fn tool_request_item(tool_call_id: &str, tool_name: &str, arguments_json: &str) -> TranscriptItem {
    let diff_note = diff_from_tool_request(tool_name, arguments_json).map_or_else(
        String::new,
        |(summary, _lines)| {
            format!(
                "\nDiff: {} (+{} -{})",
                summary.display_path(),
                summary.added,
                summary.removed
            )
        },
    );
    TranscriptItem::new(
        "Tool",
        format!(
            "request {tool_name}\nCall: {tool_call_id}{diff_note}\nArguments:\n{}",
            pretty_jsonish(arguments_json)
        ),
    )
}

fn diff_from_tool_request(
    tool_name: &str,
    arguments_json: &str,
) -> Option<(DiffFileSummary, Vec<DiffLine>)> {
    let normalized_tool = tool_name.replace(['-', '.'], "_").to_ascii_lowercase();
    if !matches!(
        normalized_tool.as_str(),
        "filesystem_edit" | "filesystem_write"
    ) {
        return None;
    }
    let value = serde_json::from_str::<serde_json::Value>(arguments_json).ok()?;
    let path = value
        .get("path")
        .or_else(|| value.get("file_path"))
        .or_else(|| value.get("file"))?
        .as_str()?;
    let (added, removed) = count_edit_lines(&value);
    let summary = DiffFileSummary::new(path, added, removed);
    let lines = diff_lines_from_value(path, &value);
    Some((summary, lines))
}

fn diff_lines_from_value(path: &str, value: &serde_json::Value) -> Vec<DiffLine> {
    let old_text = value
        .get("old_text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let new_text = value
        .get("new_text")
        .or_else(|| value.get("contents"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let mut lines = vec![
        DiffLine::new(DiffLineKind::FileHeader, None, None, format!("--- {path}")),
        DiffLine::new(DiffLineKind::FileHeader, None, None, format!("+++ {path}")),
        DiffLine::new(DiffLineKind::HunkHeader, None, None, "@@ inferred edit @@"),
    ];
    let mut old_line = 1_u32;
    for line in old_text.lines().take(200) {
        lines.push(DiffLine::new(
            DiffLineKind::Removed,
            Some(old_line),
            None,
            line.to_owned(),
        ));
        old_line = old_line.saturating_add(1);
    }
    let mut new_line = 1_u32;
    for line in new_text.lines().take(200) {
        lines.push(DiffLine::new(
            DiffLineKind::Added,
            None,
            Some(new_line),
            line.to_owned(),
        ));
        new_line = new_line.saturating_add(1);
    }
    if old_text.lines().count() > 200 || new_text.lines().count() > 200 {
        lines.push(DiffLine::new(
            DiffLineKind::Context,
            None,
            None,
            "… diff preview truncated …",
        ));
    }
    lines
}

fn count_edit_lines(value: &serde_json::Value) -> (u32, u32) {
    let new_text = value
        .get("new_text")
        .or_else(|| value.get("contents"))
        .and_then(serde_json::Value::as_str);
    let old_text = value.get("old_text").and_then(serde_json::Value::as_str);
    match (new_text, old_text) {
        (Some(new_text), Some(old_text)) => (line_count(new_text), line_count(old_text)),
        (Some(new_text), None) => (line_count(new_text), 0),
        (None, Some(old_text)) => (0, line_count(old_text)),
        (None, None) => (0, 0),
    }
}

fn line_count(value: &str) -> u32 {
    u32::try_from(value.lines().count().max(1)).unwrap_or(u32::MAX)
}

fn pretty_jsonish(value: &str) -> String {
    truncate_block(value, 2_000)
}

fn truncate_block(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index >= max_chars {
            output.push_str("\n… truncated");
            return output;
        }
        output.push(ch);
    }
    output
}

/// Current high-level backend activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ActivityState {
    /// No active model/tool work.
    Idle,
    /// Waiting for a model response.
    Thinking,
    /// Receiving streamed model output.
    Streaming,
    /// Running a tool.
    RunningTool {
        /// Tool name.
        name: String,
    },
    /// Waiting for a permission decision.
    WaitingPermission {
        /// Tool name.
        name: String,
    },
}

/// Pending user message not yet confirmed by the session stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingSubmission {
    text: String,
    state: PendingSubmissionState,
}

impl PendingSubmission {
    const fn new(text: String) -> Self {
        Self {
            text,
            state: PendingSubmissionState::Sending,
        }
    }

    /// Return pending text.
    #[must_use]
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    /// Return pending state.
    #[must_use]
    pub(super) const fn state(&self) -> PendingSubmissionState {
        self.state
    }
}

/// Pending user message state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PendingSubmissionState {
    /// Client request is in flight.
    Sending,
    /// Server accepted the request immediately.
    Sent,
    /// Server queued the request.
    Queued {
        /// Server-reported queue position.
        queue_position: Option<u32>,
    },
}

/// Renderable transcript item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TranscriptItem {
    role: &'static str,
    text: String,
    streaming: bool,
}

impl TranscriptItem {
    const fn new(role: &'static str, text: String) -> Self {
        Self {
            role,
            text,
            streaming: false,
        }
    }

    const fn new_streaming(role: &'static str, text: String) -> Self {
        Self {
            role,
            text,
            streaming: true,
        }
    }

    /// Return display role.
    #[must_use]
    pub(super) const fn role(&self) -> &'static str {
        self.role
    }

    /// Return display text.
    #[must_use]
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    /// Return whether this item is currently streaming.
    #[must_use]
    pub(super) const fn streaming(&self) -> bool {
        self.streaming
    }
}

const fn model_turn_outcome_label(outcome: ModelTurnOutcome) -> &'static str {
    match outcome {
        ModelTurnOutcome::Completed => "done",
        ModelTurnOutcome::Cancelled => "cancelled",
        ModelTurnOutcome::Error => "error",
        ModelTurnOutcome::IdleTimeout => "idle timeout",
        ModelTurnOutcome::ToolRoundLimitReached => "tool round limit reached",
        ModelTurnOutcome::ProviderUnavailable => "provider unavailable",
    }
}
