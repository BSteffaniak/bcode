//! BMUX backend app state.

use bcode_session_models::{
    ModelTurnOutcome, ProviderStreamEvent, SessionEvent, SessionEventKind, SessionHistoryCursor,
    SessionId, SessionInputHistoryEntry, SessionTraceEvent, SessionTracePayload, SessionTracePhase,
};
use bcode_skill_models::SkillSource;
use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};
use bmux_tui::diff::{DiffFileSummary, DiffLine};
use bmux_tui::event::MouseEvent;
use bmux_tui::geometry::Rect;
use bmux_tui_components::text_input::{
    TextInputControl, TextInputOutcome, TextInputPolicy, TextInputState,
};

use super::activity::{ActivityState, model_turn_outcome_label};
use super::cursor_blink::CursorBlink;
use super::diff_extract::diff_from_tool_request;
use super::diff_panel::DiffPanel;
use super::exit_state::ExitState;
use super::input_history::{InputHistory, InputHistoryOutcome};
use super::older_history::OlderHistoryState;
use super::pending_submission::PendingSubmission;
use super::pending_submissions::PendingSubmissions;
use super::transcript::{
    TranscriptItem, merge_transcript_boundary, optional_u32, pretty_jsonish, tool_request_item,
    transcript_items_from_events, truncate_block,
};
use super::transcript_viewport::TranscriptViewport;

/// State owned by the BMUX-native backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BmuxApp {
    session_id: Option<SessionId>,
    session_title: Option<String>,
    selected_provider_plugin_id: Option<String>,
    selected_model_id: Option<String>,
    current_agent_id: String,
    thinking_label: String,
    token_usage: TokenUsageMeter,
    composer: TextInputState,
    input_history: InputHistory,
    transcript: Vec<TranscriptItem>,
    diff_panel: DiffPanel,
    pending_submissions: PendingSubmissions,
    viewport: TranscriptViewport,
    older_history: OlderHistoryState,
    activity: ActivityState,
    status: String,
    key_hints: String,
    exit: ExitState,
    cursor: CursorBlink,
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
            session_title: None,
            selected_provider_plugin_id: None,
            selected_model_id: None,
            current_agent_id: "build".to_owned(),
            thinking_label: "default".to_owned(),
            token_usage: TokenUsageMeter::default(),
            composer: TextInputState::new(TextEditBuffer::new()),
            input_history: InputHistory::from_entries(input_history),
            transcript: Vec::new(),
            diff_panel: DiffPanel::new(),
            pending_submissions: PendingSubmissions::default(),
            viewport: TranscriptViewport::default(),
            older_history: OlderHistoryState::new(history, has_older_history),
            activity: ActivityState::Idle,
            status: String::from("BMUX backend connected. Enter submits; Esc/Ctrl-C exits."),
            key_hints: String::from("enter send · escape interrupt · ctrl+d exit · ctrl+p palette"),
            exit: ExitState::default(),
            cursor: CursorBlink::new(),
        };
        app.absorb_history(history);
        app
    }

    /// Return the active session id, if one was provided.
    #[must_use]
    pub(super) const fn session_id(&self) -> Option<SessionId> {
        self.session_id
    }

    /// Return the current session title, if known.
    #[must_use]
    pub(super) fn session_title(&self) -> Option<&str> {
        self.session_title.as_deref()
    }

    /// Return the currently selected provider plugin id, if explicit.
    #[must_use]
    pub(super) fn selected_provider_plugin_id(&self) -> Option<&str> {
        self.selected_provider_plugin_id.as_deref()
    }

    /// Return the currently selected model id, if explicit.
    #[must_use]
    pub(super) fn selected_model_id(&self) -> Option<&str> {
        self.selected_model_id.as_deref()
    }

    /// Return the current agent id.
    #[must_use]
    pub(super) fn current_agent_id(&self) -> &str {
        &self.current_agent_id
    }

    /// Return the current thinking display label.
    #[must_use]
    pub(super) fn thinking_label(&self) -> &str {
        &self.thinking_label
    }

    /// Return the token/context footer summary.
    #[must_use]
    pub(super) fn token_summary(&self) -> String {
        self.token_usage.footer_summary()
    }

    /// Return the composer content area from the latest render.
    #[must_use]
    pub(super) const fn composer_content_area(&self) -> Rect {
        self.composer.content_area()
    }

    /// Store the composer content area from the latest render.
    pub(super) fn set_composer_content_area(&mut self, area: Rect) {
        self.composer.set_content_area(area, &composer_policy());
    }

    /// Return the current composer wrapped-row scroll offset.
    #[must_use]
    pub(super) const fn composer_scroll_offset(&self) -> usize {
        self.composer.vertical_scroll()
    }

    /// Return the composer buffer.
    #[must_use]
    pub(super) const fn composer(&self) -> &TextEditBuffer {
        self.composer.buffer()
    }

    /// Return the composer buffer mutably.
    pub(super) const fn composer_mut(&mut self) -> &mut TextEditBuffer {
        self.composer.buffer_mut()
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

    /// Extend composer selection with an editor motion.
    pub(super) fn extend_composer_selection(&mut self, motion: TextMotion) {
        self.input_history.reset_navigation();
        let width = usize::from(self.composer.content_area().width.max(1));
        match motion {
            TextMotion::VisualUp => self.extend_composer_selection_to_visual_delta(width, -1),
            TextMotion::VisualDown => self.extend_composer_selection_to_visual_delta(width, 1),
            motion => self
                .composer
                .buffer_mut()
                .move_cursor_with_selection(motion, SelectionMode::Extend),
        }
        self.wake_cursor();
    }

    /// Handle a composer mouse event through the reusable text-input component.
    pub(super) fn handle_composer_mouse(&mut self, mouse: MouseEvent) -> TextInputOutcome {
        let outcome =
            TextInputControl::new(&composer_policy()).handle_mouse(&mut self.composer, mouse);
        if matches!(outcome, TextInputOutcome::Edited | TextInputOutcome::Redraw) {
            self.input_history.reset_navigation();
            self.wake_cursor();
        }
        outcome
    }

    /// Return whether a composer mouse selection is active.
    #[must_use]
    pub(super) const fn composer_mouse_selection_active(&self) -> bool {
        self.composer.mouse_selection_active()
    }

    /// Move the composer cursor one rendered row up, if possible.
    pub(super) fn move_composer_visual_up(&mut self) -> bool {
        self.move_composer_visual_up_with_history_reset(true)
    }

    /// Move the composer cursor one rendered row up without leaving history navigation.
    pub(super) fn move_composer_visual_up_preserving_history(&mut self) -> bool {
        self.move_composer_visual_up_with_history_reset(false)
    }

    /// Move the composer cursor one rendered row down, if possible.
    pub(super) fn move_composer_visual_down(&mut self) -> bool {
        self.move_composer_visual_down_with_history_reset(true)
    }

    /// Move the composer cursor one rendered row down without leaving history navigation.
    pub(super) fn move_composer_visual_down_preserving_history(&mut self) -> bool {
        self.move_composer_visual_down_with_history_reset(false)
    }

    fn move_composer_visual_up_with_history_reset(&mut self, reset_history: bool) -> bool {
        let width = usize::from(self.composer.content_area().width.max(1));
        let layout = self.composer.buffer().wrapped_layout(width);
        if layout.cursor.row == 0 {
            return false;
        }
        if reset_history {
            self.input_history.reset_navigation();
        }
        self.composer.buffer_mut().move_cursor_to_wrapped_position(
            width,
            layout.cursor.row.saturating_sub(1),
            layout.cursor.col,
        );
        self.wake_cursor();
        true
    }

    fn move_composer_visual_down_with_history_reset(&mut self, reset_history: bool) -> bool {
        let width = usize::from(self.composer.content_area().width.max(1));
        let layout = self.composer.buffer().wrapped_layout(width);
        if layout.cursor.row.saturating_add(1) >= layout.lines.len() {
            return false;
        }
        if reset_history {
            self.input_history.reset_navigation();
        }
        self.composer.buffer_mut().move_cursor_to_wrapped_position(
            width,
            layout.cursor.row.saturating_add(1),
            layout.cursor.col,
        );
        self.wake_cursor();
        true
    }

    /// Apply hydrated model metadata to the app.
    pub(super) fn apply_model_status(&mut self, status: bcode_ipc::SessionModelStatus) {
        if status.provider_plugin_id.is_some() {
            self.selected_provider_plugin_id = status.provider_plugin_id;
        }
        if status.model_id.is_some() {
            self.selected_model_id = status.model_id;
        }
        self.token_usage.apply_model_info(status.model.as_ref());
    }

    /// Return pending submissions that have not been committed by the session stream.
    #[must_use]
    pub(super) fn pending_submissions(&self) -> &[PendingSubmission] {
        self.pending_submissions.items()
    }

    /// Return the number of transcript rows hidden below the viewport.
    #[must_use]
    pub(super) const fn scroll_offset(&self) -> usize {
        self.viewport.offset()
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

    /// Return configured key hints for the status line.
    #[must_use]
    pub(super) fn key_hints(&self) -> &str {
        &self.key_hints
    }

    /// Store configured key hints for the status line.
    pub(super) fn set_key_hints(&mut self, key_hints: String) {
        self.key_hints = key_hints;
    }

    /// Append a system-style transcript note.
    pub(super) fn push_system_note(&mut self, text: String) {
        self.transcript.push(TranscriptItem::new("System", text));
    }

    /// Replace the current status line.
    pub(super) fn set_status(&mut self, status: String) {
        self.status = status;
    }

    /// Mark the app as waiting for turn cancellation.
    pub(super) fn set_cancelling(&mut self) {
        self.set_activity(ActivityState::Cancelling);
    }

    /// Return the app to idle activity.
    pub(super) fn set_idle(&mut self) {
        self.set_activity(ActivityState::Idle);
    }

    /// Store the current composer text as a pending submission and clear input.
    pub(super) fn stage_submission(&mut self) {
        let text = self.composer.buffer().text().to_owned();
        self.pending_submissions.stage(text);
        self.input_history.reset_navigation();
        self.composer.buffer_mut().clear();
    }

    /// Return the currently pending submission.
    pub(super) fn take_pending_submission(&mut self) -> String {
        self.pending_submissions.take_staged()
    }

    /// Remove a pending submission that was handled outside the session transcript.
    pub(super) fn clear_pending_submission(&mut self, text: &str) {
        self.pending_submissions.clear_staged_if(text);
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
        self.pending_submissions.clear_staged_if(text);
        self.remove_pending_submission(text);
        self.composer.buffer_mut().insert_str(text);
        self.wake_cursor();
    }

    /// Show the previous input-history entry, if available.
    pub(super) fn previous_input_history(&mut self) -> bool {
        match self.input_history.previous(self.composer.buffer().text()) {
            InputHistoryOutcome::Entry { index, total, text } => {
                self.replace_composer_with(&text);
                self.status = format!("input history {index}/{total}");
            }
            InputHistoryOutcome::DraftRestored(text) => {
                self.replace_composer_with(&text);
                "draft restored".clone_into(&mut self.status);
            }
            InputHistoryOutcome::Empty => {
                "no input history in this session".clone_into(&mut self.status);
            }
            InputHistoryOutcome::NotBrowsing => {
                "not browsing input history".clone_into(&mut self.status);
            }
        }
        true
    }

    /// Show the next input-history entry, or restore the draft.
    pub(super) fn next_input_history(&mut self) -> bool {
        match self.input_history.next() {
            InputHistoryOutcome::Entry { index, total, text } => {
                self.replace_composer_with(&text);
                self.status = format!("input history {index}/{total}");
            }
            InputHistoryOutcome::DraftRestored(text) => {
                self.replace_composer_with(&text);
                "draft restored".clone_into(&mut self.status);
            }
            InputHistoryOutcome::Empty => {
                "no input history in this session".clone_into(&mut self.status);
            }
            InputHistoryOutcome::NotBrowsing => {
                "not browsing input history".clone_into(&mut self.status);
            }
        }
        true
    }

    /// Return whether input-history navigation is active.
    #[must_use]
    pub(super) const fn input_history_navigation_active(&self) -> bool {
        self.input_history.is_browsing()
    }

    /// Reset active input-history navigation after direct composer editing.
    pub(super) fn reset_input_history_navigation(&mut self) {
        self.input_history.reset_navigation();
    }

    /// Scroll transcript up by rendered rows.
    pub(super) fn scroll_transcript_up(&mut self, rows: usize) -> bool {
        self.viewport.scroll_up(rows, &mut self.older_history)
    }

    /// Scroll transcript down by rendered rows.
    pub(super) const fn scroll_transcript_down(&mut self, rows: usize) -> bool {
        self.viewport.scroll_down(rows)
    }

    /// Pin transcript to the newest rows.
    pub(super) const fn scroll_transcript_to_bottom(&mut self) -> bool {
        self.viewport.scroll_to_bottom(&mut self.older_history)
    }

    /// Sync cached rendered transcript scroll bounds from the latest frame.
    pub(super) fn sync_transcript_scroll_max(&mut self, max_scroll_offset: usize) {
        self.viewport
            .sync_max(max_scroll_offset, &mut self.older_history);
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

        let input_messages = events.iter().filter_map(|event| match &event.kind {
            SessionEventKind::UserMessage { text, .. } => Some((event.sequence, text.clone())),
            _ => None,
        });
        self.input_history.prepend_committed(input_messages);

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
        if event_affects_transcript_rows(event) {
            self.viewport.preserve_for_append();
        }
        match &event.kind {
            SessionEventKind::UserMessage { text, .. } => {
                self.push_committed_user_message(event.sequence, text);
            }
            SessionEventKind::AssistantDelta { text } => self.push_live_assistant_delta(text),
            SessionEventKind::AssistantMessage { text } => {
                self.finish_streaming_item("Assistant", text);
            }
            SessionEventKind::SystemMessage { text } => self.push_system_message(text),
            SessionEventKind::ToolCallRequested {
                tool_call_id,
                tool_name,
                arguments_json,
            } => self.push_tool_request(tool_call_id, tool_name, arguments_json),
            SessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
            } => {
                self.set_activity(ActivityState::Thinking);
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
                self.set_activity(ActivityState::Thinking);
                self.set_permission_status(permission_id, *approved);
            }
            SessionEventKind::ModelChanged { provider, model } => {
                self.apply_model_changed(provider, model);
            }
            SessionEventKind::ModelTurnStarted { .. } => {
                self.set_activity(ActivityState::Thinking);
                "thinking".clone_into(&mut self.status);
            }
            SessionEventKind::ModelTurnFinished {
                outcome, message, ..
            } => self.finish_model_turn(*outcome, message.as_deref()),
            SessionEventKind::ModelUsage { turn_id, usage } => {
                self.push_model_usage(turn_id, usage);
            }
            SessionEventKind::ContextCompacted { summary, .. } => self.push_compaction(summary),
            SessionEventKind::SessionRenamed { name } => self.rename_session(name.as_deref()),
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
                self.add_streaming_delta(text);
                self.push_streaming_item("Reasoning", text);
            }
            SessionEventKind::AssistantReasoningMessage { text } => {
                self.finish_streaming_item("Reasoning", text);
            }
            SessionEventKind::SessionCreated { name } => self.session_title.clone_from(name),
            SessionEventKind::AgentChanged { agent_id } => {
                self.current_agent_id.clone_from(agent_id);
            }
            SessionEventKind::TraceEvent { trace } => self.apply_trace_event(trace),
            SessionEventKind::ClientAttached { .. } | SessionEventKind::ClientDetached { .. } => {}
        }
    }

    /// Return whether the composer cursor should be visible.
    #[must_use]
    pub(super) const fn cursor_visible(&self) -> bool {
        self.cursor.visible()
    }

    /// Reset cursor blink state after input.
    pub(super) fn wake_cursor(&mut self) {
        self.cursor.wake();
    }

    /// Advance time-based UI state.
    pub(super) fn tick(&mut self) -> bool {
        self.cursor.tick()
    }

    /// Return whether the backend should exit.
    #[must_use]
    pub(super) const fn should_exit(&self) -> bool {
        self.exit.requested()
    }

    /// Request backend shutdown.
    pub(super) const fn request_exit(&mut self) {
        self.exit.request();
    }

    /// Replace composer contents.
    pub(super) fn replace_composer_with(&mut self, text: &str) {
        self.composer.buffer_mut().clear();
        self.composer.buffer_mut().insert_str(text);
        self.wake_cursor();
    }

    fn apply_model_changed(&mut self, provider: &str, model: &str) {
        self.selected_provider_plugin_id = provider_to_display_selection(provider);
        self.selected_model_id = model_to_display_selection(model);
        self.token_usage.clear_model_info();
        self.status = format!("model: {provider}/{model}");
    }

    fn extend_composer_selection_to_visual_delta(&mut self, width: usize, delta: isize) {
        let layout = self.composer.buffer().wrapped_layout(width);
        let target_row = if delta.is_negative() {
            layout.cursor.row.saturating_sub(delta.unsigned_abs())
        } else {
            layout
                .cursor
                .row
                .saturating_add(delta.unsigned_abs())
                .min(layout.lines.len().saturating_sub(1))
        };
        self.composer
            .buffer_mut()
            .select_to_wrapped_position(width, target_row, layout.cursor.col);
    }

    fn rename_session(&mut self, name: Option<&str>) {
        self.session_title = name.map(ToOwned::to_owned);
        self.set_session_name_status(name);
    }

    fn remove_pending_submission(&mut self, text: &str) {
        self.pending_submissions.remove(text);
    }

    fn push_committed_user_message(&mut self, sequence: u64, text: &str) {
        self.input_history.push_committed(sequence, text);
        self.push_live_user_message(text);
    }

    fn push_live_user_message(&mut self, text: &str) {
        self.set_activity(ActivityState::Thinking);
        self.push_user_message(text);
    }

    fn push_live_assistant_delta(&mut self, text: &str) {
        self.add_streaming_delta(text);
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
        self.set_activity(ActivityState::RunningTool {
            name: tool_name.to_owned(),
        });
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
        self.set_activity(ActivityState::WaitingPermission {
            name: tool_name.to_owned(),
        });
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
        self.token_usage.absorb(usage);
        if let Some(tokens) = usage.metered_total_tokens() {
            self.status = format!("tokens: {tokens}");
        }
        self.transcript.push(TranscriptItem::new(
            "Usage",
            format!(
                "turn {turn_id}\ninput: {}\noutput: {}\ntotal: {}\ncached: {}\ncache write: {}\nreasoning: {}",
                optional_u32(usage.input_tokens),
                optional_u32(usage.output_tokens),
                optional_u32(usage.metered_total_tokens()),
                optional_u32(usage.cached_input_tokens),
                optional_u32(usage.cache_write_input_tokens),
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
        self.set_activity(ActivityState::Idle);
    }

    fn push_compaction(&mut self, summary: &str) {
        self.transcript.push(TranscriptItem::new(
            "Compaction",
            format!("context compacted: {summary}"),
        ));
    }

    fn set_activity(&mut self, activity: ActivityState) {
        if self.activity != activity {
            self.activity = activity;
        }
    }

    fn add_streaming_delta(&mut self, text: &str) {
        let delta = text.chars().count();
        if let ActivityState::Streaming { chars } = &mut self.activity {
            *chars = chars.saturating_add(delta);
        } else {
            self.set_activity(ActivityState::Streaming { chars: delta });
        }
    }

    fn apply_trace_event(&mut self, trace: &SessionTraceEvent) {
        match &trace.payload {
            SessionTracePayload::ProviderStreamEvent(event) => {
                self.apply_provider_stream_event(event);
            }
            SessionTracePayload::ProviderEvent { event_type, detail } => {
                if matches!(event_type.as_str(), "tool_call_delta" | "warning" | "error") {
                    let detail = detail
                        .clone()
                        .unwrap_or_else(|| format!("provider event: {event_type}"));
                    self.set_activity(ActivityState::ProviderStream {
                        detail: detail.clone(),
                    });
                    self.status = detail;
                }
            }
            SessionTracePayload::ContextCompaction {
                reason,
                compacted,
                message,
                ..
            } => self.apply_compaction_trace(trace.phase, reason, *compacted, message.as_deref()),
            SessionTracePayload::ModelRequestBuilt { .. }
            | SessionTracePayload::ProviderRound { .. }
            | SessionTracePayload::ToolInvocationStarted { .. }
            | SessionTracePayload::ToolPolicyEvaluated { .. }
            | SessionTracePayload::ToolPermissionWait { .. }
            | SessionTracePayload::ToolInvocationFinished { .. } => {}
        }
    }

    fn apply_provider_stream_event(&mut self, event: &ProviderStreamEvent) {
        match event {
            ProviderStreamEvent::TurnStarted => {
                self.set_activity(ActivityState::ProviderStream {
                    detail: "provider stream started".to_owned(),
                });
                "provider stream started".clone_into(&mut self.status);
            }
            ProviderStreamEvent::ToolCallStarted { tool_name, .. } => {
                let detail = format!("provider stream tool started: {tool_name}");
                self.set_activity(ActivityState::ProviderStream {
                    detail: detail.clone(),
                });
                self.status = detail;
            }
            ProviderStreamEvent::ToolCallProgress {
                tool_name,
                argument_bytes,
                chunk_count,
                ..
            } => {
                let detail = format!(
                    "provider stream tool progress: {tool_name} ({argument_bytes} bytes, {chunk_count} chunks)"
                );
                self.set_activity(ActivityState::ProviderStream {
                    detail: detail.clone(),
                });
                self.status = detail;
            }
            ProviderStreamEvent::ToolCallFinished { tool_name, .. } => {
                self.status = format!("provider stream tool finished: {tool_name}");
                if matches!(self.activity, ActivityState::ProviderStream { .. }) {
                    self.set_activity(ActivityState::Thinking);
                }
            }
            ProviderStreamEvent::NoProgressWarning {
                idle_seconds,
                active_tool_call,
            } => {
                let detail = active_tool_call.as_ref().map_or_else(
                    || format!("provider stream idle for {idle_seconds}s"),
                    |tool| {
                        format!(
                            "provider stream idle for {idle_seconds}s while streaming {}",
                            tool.tool_name
                        )
                    },
                );
                self.set_activity(ActivityState::ProviderStream {
                    detail: detail.clone(),
                });
                self.status = detail;
            }
        }
    }

    fn apply_compaction_trace(
        &mut self,
        phase: SessionTracePhase,
        reason: &str,
        compacted: bool,
        message: Option<&str>,
    ) {
        match phase {
            SessionTracePhase::ContextCompactionStarted => {
                let detail = message.map_or_else(
                    || format!("context compaction · {reason}"),
                    ToOwned::to_owned,
                );
                self.set_activity(ActivityState::Compacting {
                    detail: detail.clone(),
                });
                self.status = detail;
            }
            SessionTracePhase::ContextCompactionFinished => {
                let detail = message.map_or_else(
                    || "context compaction finished".to_owned(),
                    ToOwned::to_owned,
                );
                self.status = detail;
                if compacted {
                    self.set_activity(ActivityState::Thinking);
                }
            }
            SessionTracePhase::ContextCompactionSkipped => {
                if matches!(self.activity, ActivityState::Compacting { .. }) {
                    self.set_activity(ActivityState::Thinking);
                }
                if let Some(message) = message {
                    message.clone_into(&mut self.status);
                }
            }
            SessionTracePhase::ModelRequestBuilt
            | SessionTracePhase::ModelProviderRoundStarted
            | SessionTracePhase::ModelProviderRoundFinished
            | SessionTracePhase::ModelProviderEvent
            | SessionTracePhase::ToolInvocationStarted
            | SessionTracePhase::ToolPolicyEvaluated
            | SessionTracePhase::ToolPermissionWaitStarted
            | SessionTracePhase::ToolPermissionWaitFinished
            | SessionTracePhase::ToolInvocationFinished
            | SessionTracePhase::SkillInvoked
            | SessionTracePhase::SkillSuggested
            | SessionTracePhase::SkillActivated
            | SessionTracePhase::SkillDeactivated
            | SessionTracePhase::SkillContextLoaded
            | SessionTracePhase::SkillInvocationFailed => {}
        }
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

const fn composer_policy() -> TextInputPolicy {
    TextInputPolicy::chat_composer()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TokenUsageMeter {
    session_tokens: u64,
    latest_context_input_tokens: Option<u32>,
    latest_cached_input_tokens: Option<u32>,
    latest_cache_write_input_tokens: Option<u32>,
    context_window: Option<u32>,
}

impl TokenUsageMeter {
    fn absorb(&mut self, usage: &bcode_session_models::SessionTokenUsage) {
        if let Some(tokens) = usage.metered_total_tokens() {
            self.session_tokens = self.session_tokens.saturating_add(u64::from(tokens));
        }
        if let Some(input_tokens) = usage.context_input_tokens() {
            self.latest_context_input_tokens = Some(input_tokens);
        }
        if usage.cached_input_tokens.is_some() {
            self.latest_cached_input_tokens = usage.cached_input_tokens;
        }
        if usage.cache_write_input_tokens.is_some() {
            self.latest_cache_write_input_tokens = usage.cache_write_input_tokens;
        }
    }

    const fn apply_model_info(&mut self, model: Option<&bcode_model::ModelInfo>) {
        if let Some(model) = model {
            self.context_window = model.context_window;
        }
    }

    const fn clear_model_info(&mut self) {
        self.context_window = None;
    }

    fn footer_summary(&self) -> String {
        let mut parts = vec![self.context_summary()];
        if let Some(cached) = self.latest_cached_input_tokens
            && cached > 0
        {
            parts.push(format!("cached {} tok", compact_u64(u64::from(cached))));
        }
        if let Some(written) = self.latest_cache_write_input_tokens
            && written > 0
        {
            parts.push(format!(
                "cache write {} tok",
                compact_u64(u64::from(written))
            ));
        }
        parts.push(format!("spent {} tok", compact_u64(self.session_tokens)));
        parts.join(" · ")
    }

    fn context_summary(&self) -> String {
        if let Some(window) = self.context_window
            && window > 0
        {
            let input = self.latest_context_input_tokens.unwrap_or_default();
            return format!(
                "ctx {}/{} {}%",
                compact_u64(u64::from(input)),
                compact_u64(u64::from(window)),
                context_window_percentage(input, window)
            );
        }
        "ctx unknown".to_owned()
    }
}

fn provider_to_display_selection(provider: &str) -> Option<String> {
    if provider == "<auto>" || provider.is_empty() {
        None
    } else {
        Some(provider.to_owned())
    }
}

fn model_to_display_selection(model: &str) -> Option<String> {
    if model == "<default>" || model.is_empty() {
        None
    } else {
        Some(model.to_owned())
    }
}

fn compact_u64(value: u64) -> String {
    if value >= 1_000_000 {
        let whole = value / 1_000_000;
        let decimal = (value % 1_000_000) / 100_000;
        format!("{whole}.{decimal}m")
    } else if value >= 1_000 {
        let whole = value / 1_000;
        let decimal = (value % 1_000) / 100;
        format!("{whole}.{decimal}k")
    } else {
        value.to_string()
    }
}

fn context_window_percentage(input_tokens: u32, context_window: u32) -> u32 {
    let numerator = u64::from(input_tokens).saturating_mul(100);
    let denominator = u64::from(context_window).max(1);
    u32::try_from(numerator / denominator).unwrap_or(u32::MAX)
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
