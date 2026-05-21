//! BMUX backend app state.

use std::time::Instant;

use bcode_session_models::{
    ModelTurnOutcome, SessionEvent, SessionEventKind, SessionHistoryCursor, SessionId,
    SessionInputHistoryEntry,
};
use bcode_skill_models::SkillSource;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::diff::{DiffFileSummary, DiffLine};

use super::IDLE_REDRAW_INTERVAL;
use super::activity::{ActivityState, model_turn_outcome_label};
use super::diff_extract::diff_from_tool_request;
use super::diff_panel::DiffPanel;
use super::input_history::InputHistory;
use super::older_history::OlderHistoryState;
use super::pending_submission::PendingSubmission;
use super::transcript::{
    TranscriptItem, merge_transcript_boundary, optional_u32, pretty_jsonish, tool_request_item,
    transcript_items_from_events, truncate_block,
};

/// State owned by the BMUX-native backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BmuxApp {
    session_id: Option<SessionId>,
    composer: TextEditBuffer,
    input_history: InputHistory,
    transcript: Vec<TranscriptItem>,
    diff_panel: DiffPanel,
    pending_submissions: Vec<PendingSubmission>,
    pending_submission: Option<String>,
    scroll_offset: usize,
    transcript_max_scroll_offset: usize,
    scroll_preserve_max_offset: Option<usize>,
    older_history: OlderHistoryState,
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
            input_history: InputHistory::from_entries(input_history),
            transcript: Vec::new(),
            diff_panel: DiffPanel::new(),
            pending_submissions: Vec::new(),
            pending_submission: None,
            scroll_offset: 0,
            transcript_max_scroll_offset: 0,
            scroll_preserve_max_offset: None,
            older_history: OlderHistoryState::new(history, has_older_history),
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
        self.diff_panel.changed_files()
    }

    /// Return whether the diff panel is visible.
    #[must_use]
    pub(super) fn diff_visible(&self) -> bool {
        self.diff_panel.visible()
    }

    /// Toggle diff panel visibility.
    pub(super) const fn toggle_diff_visible(&mut self) -> bool {
        self.diff_panel.toggle_visible()
    }

    /// Return detailed diff lines inferred from edit tool calls.
    #[must_use]
    pub(super) fn diff_lines(&self) -> &[DiffLine] {
        self.diff_panel.lines()
    }

    /// Return diff scroll offset.
    #[must_use]
    pub(super) const fn diff_scroll_offset(&self) -> usize {
        self.diff_panel.scroll_offset()
    }

    /// Scroll diff preview up.
    pub(super) fn scroll_diff_up(&mut self, rows: usize) -> bool {
        self.diff_panel.scroll_up(rows)
    }

    /// Scroll diff preview down.
    pub(super) const fn scroll_diff_down(&mut self, rows: usize) -> bool {
        self.diff_panel.scroll_down(rows)
    }

    /// Select a changed-file diff detail.
    pub(super) const fn select_diff_file(&mut self, index: usize) -> bool {
        self.diff_panel.select_file(index)
    }

    /// Select next changed file.
    pub(super) fn select_next_diff_file(&mut self) -> bool {
        self.diff_panel.select_next_file()
    }

    /// Select previous changed file.
    pub(super) fn select_previous_diff_file(&mut self) -> bool {
        self.diff_panel.select_previous_file()
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
        self.older_history.has_older_history()
    }

    /// Return whether an older-history request is in flight.
    #[must_use]
    pub(super) const fn loading_older_history(&self) -> bool {
        self.older_history.loading()
    }

    /// Mark older history as loading or idle.
    pub(super) const fn set_loading_older_history(&mut self, loading: bool) {
        self.older_history.set_loading(loading);
    }

    /// Return the cursor for loading older history.
    #[must_use]
    pub(super) const fn older_history_cursor(&self) -> Option<SessionHistoryCursor> {
        self.older_history.cursor()
    }

    /// Return whether an older-history request should be started.
    #[must_use]
    pub(super) const fn should_load_older_history(&self) -> bool {
        self.older_history.should_load()
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
        self.input_history.push_submission(text);
        self.composer.clear();
    }

    /// Return the currently pending submission.
    pub(super) fn take_pending_submission(&mut self) -> String {
        self.pending_submission.take().unwrap_or_default()
    }

    /// Remove a pending submission that was handled outside the session transcript.
    pub(super) fn clear_pending_submission(&mut self, text: &str) {
        if self.pending_submission.as_deref() == Some(text) {
            self.pending_submission = None;
        }
        self.remove_pending_submission(text);
    }

    /// Mark the oldest pending submission as queued by the server.
    pub(super) fn mark_pending_submission_queued(&mut self, queue_position: Option<u32>) {
        if let Some(pending) = self.pending_submissions.first_mut() {
            pending.mark_queued(queue_position);
        }
    }

    /// Mark the oldest pending submission as sent to the server.
    pub(super) fn mark_pending_submission_sent(&mut self) {
        if let Some(pending) = self.pending_submissions.first_mut() {
            pending.mark_sent();
        }
    }

    /// Remove a pending submission and restore it into the composer.
    pub(super) fn restore_pending_submission(&mut self, text: &str) {
        if self.pending_submission.as_deref() == Some(text) {
            self.pending_submission = None;
        }
        self.remove_pending_submission(text);
        self.composer.insert_str(text);
        self.wake_cursor();
    }

    /// Show the previous input-history entry, if available.
    pub(super) fn previous_input_history(&mut self) -> bool {
        let Some(text) = self.input_history.previous(self.composer.text()) else {
            return false;
        };
        self.replace_composer_with(&text);
        true
    }

    /// Show the next input-history entry, or restore the draft.
    pub(super) fn next_input_history(&mut self) -> bool {
        let Some(text) = self.input_history.next() else {
            return false;
        };
        self.replace_composer_with(&text);
        true
    }

    /// Scroll transcript up by rendered rows.
    pub(super) fn scroll_transcript_up(&mut self, rows: usize) -> bool {
        if rows == 0 {
            return false;
        }
        let previous = self.scroll_offset;
        let previous_request = self.older_history.reveal_request();
        let desired = self.scroll_offset.saturating_add(rows);
        self.scroll_offset = desired.min(self.transcript_max_scroll_offset);
        if desired > self.transcript_max_scroll_offset {
            self.request_older_history_load(
                desired.saturating_sub(self.transcript_max_scroll_offset),
            );
        }
        self.scroll_offset != previous || self.older_history.reveal_request() != previous_request
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
        self.older_history.clear_reveal_request();
        changed
    }

    /// Sync cached rendered transcript scroll bounds from the latest frame.
    pub(super) fn sync_transcript_scroll_max(&mut self, max_scroll_offset: usize) {
        let previous_max = self.transcript_max_scroll_offset;
        self.transcript_max_scroll_offset = max_scroll_offset;
        if let Some(requested_rows) = self.older_history.take_reveal_request() {
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
        if self.older_history.cursor().is_none() || self.older_history.loading() {
            return;
        }
        let reveal_rows = reveal_rows.max(1);
        self.older_history.request_load(reveal_rows);
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
            self.older_history.update_cursor(&[], false);
            self.older_history.set_loading(false);
            "start of session history".clone_into(&mut self.status);
            return;
        }

        let mut older = transcript_items_from_events(events);
        merge_transcript_boundary(&mut older, &mut self.transcript);
        older.append(&mut self.transcript);
        self.transcript = older;
        self.older_history.update_cursor(events, has_more);
        self.older_history.set_loading(false);
        if self.older_history.has_older_history() {
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
            .position(|pending| pending.text() == text)
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
        self.diff_panel.record(summary, lines);
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
