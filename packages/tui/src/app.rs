//! TUI app state.

use std::collections::BTreeMap;

use bcode_config::{TuiConfig, TuiInlineDiffConfig, TuiThinkingConfig};
use bcode_session_models::{
    ModelTurnOutcome, ProviderStreamEvent, SessionEvent, SessionEventKind, SessionHistoryCursor,
    SessionId, SessionInputHistoryEntry, SessionTraceEvent, SessionTracePayload, SessionTracePhase,
    ToolInvocationStreamEvent, ToolOutputStream,
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
use super::runtime_work_view::RuntimeWorkViewState;
use super::tool_present::{
    ShellResultPresentation, ToolResultPresentation, tool_result_presentation,
};
use super::transcript::{
    FileEditPhase, TranscriptItem, TranscriptItemKind, finish_streaming_transcript_item,
    merge_transcript_boundary, model_usage_item, permission_request_item, permission_result_item,
    push_streaming_transcript_item, streaming_terminal_output_item, streaming_tool_output_item,
    tool_request_item, tool_result_item, transcript_items_from_events_with_reasoning,
};
use super::transcript_layout::TranscriptLayoutCache;
use super::transcript_viewport::TranscriptViewport;

/// State owned by the terminal user interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BmuxApp {
    session_id: Option<SessionId>,
    session_title: Option<String>,
    working_directory: Option<std::path::PathBuf>,
    selected_provider_plugin_id: Option<String>,
    selected_model_id: Option<String>,
    current_agent_id: String,
    reasoning_visible: bool,
    thinking_label: String,
    reasoning_effort: Option<String>,
    reasoning_summary: Option<String>,
    reasoning_default_effort: Option<String>,
    reasoning_default_summary: Option<String>,
    token_usage: TokenUsageMeter,
    composer: TextInputState,
    input_history: InputHistory,
    transcript: Vec<TranscriptItem>,
    tool_call_contexts: BTreeMap<String, ToolCallContext>,
    streamed_tool_results: BTreeMap<String, StreamedToolResultContext>,
    runtime_work: RuntimeWorkViewState,
    diff_panel: DiffPanel,
    pending_submissions: PendingSubmissions,
    transcript_layout: TranscriptLayoutCache,
    viewport: TranscriptViewport,
    older_history: OlderHistoryState,
    activity: ActivityState,
    status: String,
    key_hints: String,
    tui_config: TuiConfig,
    exit: ExitState,
    cursor: CursorBlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolCallContext {
    tool_name: String,
    arguments_json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamedToolResultContext {
    index: Option<usize>,
    columns: u16,
    rows: u16,
    saw_output: bool,
}

impl BmuxApp {
    /// Create TUI state with replayed session data.
    #[must_use]
    pub fn new_with_history(
        session_id: Option<SessionId>,
        history: &[SessionEvent],
        input_history: &[SessionInputHistoryEntry],
        has_older_history: bool,
    ) -> Self {
        let mut app = Self {
            session_id,
            session_title: None,
            working_directory: None,
            selected_provider_plugin_id: None,
            selected_model_id: None,
            current_agent_id: "build".to_owned(),
            reasoning_visible: true,
            thinking_label: "shown · effort: provider default · summary: provider default"
                .to_owned(),
            reasoning_effort: None,
            reasoning_summary: None,
            reasoning_default_effort: None,
            reasoning_default_summary: None,
            token_usage: TokenUsageMeter::default(),
            composer: TextInputState::new(TextEditBuffer::new()),
            input_history: InputHistory::from_entries(input_history),
            transcript: Vec::new(),
            tool_call_contexts: BTreeMap::new(),
            streamed_tool_results: BTreeMap::new(),
            runtime_work: RuntimeWorkViewState::default(),
            diff_panel: DiffPanel::new(),
            pending_submissions: PendingSubmissions::default(),
            transcript_layout: TranscriptLayoutCache::default(),
            viewport: TranscriptViewport::default(),
            older_history: OlderHistoryState::new(history, has_older_history),
            activity: ActivityState::Idle,
            status: String::from("TUI connected. Enter submits; Esc/Ctrl-C exits."),
            key_hints: String::from("enter send · escape interrupt · ctrl+d exit · ctrl+p palette"),
            tui_config: TuiConfig::default(),
            exit: ExitState::default(),
            cursor: CursorBlink::new(),
        };
        app.absorb_history(history);
        app
    }

    /// Return the active session id, if one was provided.
    #[must_use]
    pub const fn session_id(&self) -> Option<SessionId> {
        self.session_id
    }

    /// Return the current session title, if known.
    #[must_use]
    pub fn session_title(&self) -> Option<&str> {
        self.session_title.as_deref()
    }

    /// Return the current working directory, if known.
    #[must_use]
    pub fn working_directory(&self) -> Option<&std::path::Path> {
        self.working_directory.as_deref()
    }

    /// Apply canonical session metadata from an attach/list response.
    pub fn apply_session_summary(&mut self, summary: &bcode_session_models::SessionSummary) {
        self.session_id = Some(summary.id);
        self.session_title.clone_from(&summary.name);
        self.working_directory = Some(summary.working_directory.clone());
    }

    /// Apply terminal UI configuration.
    pub fn apply_tui_config(&mut self, config: TuiConfig) {
        self.apply_thinking_config(config.thinking);
        self.tui_config = config;
    }

    /// Return inline diff preview rendering configuration.
    #[must_use]
    pub const fn inline_diff_config(&self) -> TuiInlineDiffConfig {
        self.tui_config.inline_diff
    }

    /// Return the currently selected provider plugin id, if explicit.
    #[must_use]
    pub fn selected_provider_plugin_id(&self) -> Option<&str> {
        self.selected_provider_plugin_id.as_deref()
    }

    /// Return the currently selected model id, if explicit.
    #[must_use]
    pub fn selected_model_id(&self) -> Option<&str> {
        self.selected_model_id.as_deref()
    }

    /// Return the current agent id.
    #[must_use]
    pub fn current_agent_id(&self) -> &str {
        &self.current_agent_id
    }

    /// Return the current thinking display label.
    #[must_use]
    pub fn thinking_label(&self) -> &str {
        &self.thinking_label
    }

    /// Return the token/context footer summary.
    #[must_use]
    pub fn token_summary(&self) -> String {
        self.token_usage.footer_summary()
    }

    /// Return the composer content area from the latest render.
    #[must_use]
    pub const fn composer_content_area(&self) -> Rect {
        self.composer.content_area()
    }

    /// Store the composer content area from the latest render.
    pub fn set_composer_content_area(&mut self, area: Rect) {
        self.composer.set_content_area(area, &composer_policy());
    }

    /// Return the composer scroll offset that should be used for the latest content area.
    pub fn composer_scroll_offset_for_render(&self) -> usize {
        if self.composer.vertical_scroll() == usize::MAX {
            self.composer
                .cursor_scroll_offset(&composer_policy())
                .unwrap_or(0)
        } else {
            self.composer.vertical_scroll()
        }
    }

    /// Return the composer text input state.
    #[must_use]
    pub const fn composer_state(&self) -> &TextInputState {
        &self.composer
    }

    /// Return the composer buffer.
    #[must_use]
    pub const fn composer(&self) -> &TextEditBuffer {
        self.composer.buffer()
    }

    /// Return the composer buffer mutably.
    pub const fn composer_mut(&mut self) -> &mut TextEditBuffer {
        self.composer.buffer_mut()
    }

    /// Insert pasted text into the composer.
    pub fn paste_composer_text(&mut self, text: &str) {
        TextInputControl::new(&composer_policy()).handle_paste(&mut self.composer, text);
    }

    /// Return transcript items.
    #[must_use]
    pub fn transcript(&self) -> &[TranscriptItem] {
        &self.transcript
    }

    /// Return changed-file summaries inferred from edit tool calls.
    #[must_use]
    pub fn changed_files(&self) -> &[DiffFileSummary] {
        self.diff_panel.changed_files()
    }

    /// Return whether the diff panel is visible.
    #[must_use]
    pub fn diff_visible(&self) -> bool {
        self.diff_panel.visible()
    }

    /// Toggle diff panel visibility.
    pub const fn toggle_diff_visible(&mut self) -> bool {
        self.diff_panel.toggle_visible()
    }

    /// Return detailed diff lines inferred from edit tool calls.
    #[must_use]
    pub fn diff_lines(&self) -> &[DiffLine] {
        self.diff_panel.lines()
    }

    /// Return diff scroll offset.
    #[must_use]
    pub const fn diff_scroll_offset(&self) -> usize {
        self.diff_panel.scroll_offset()
    }

    /// Scroll diff preview up.
    pub fn scroll_diff_up(&mut self, rows: usize) -> bool {
        self.diff_panel.scroll_up(rows)
    }

    /// Scroll diff preview down.
    pub const fn scroll_diff_down(&mut self, rows: usize) -> bool {
        self.diff_panel.scroll_down(rows)
    }

    /// Select a changed-file diff detail.
    pub const fn select_diff_file(&mut self, index: usize) -> bool {
        self.diff_panel.select_file(index)
    }

    /// Select next changed file.
    pub fn select_next_diff_file(&mut self) -> bool {
        self.diff_panel.select_next_file()
    }

    /// Select previous changed file.
    pub fn select_previous_diff_file(&mut self) -> bool {
        self.diff_panel.select_previous_file()
    }

    /// Extend composer selection with an editor motion.
    pub fn extend_composer_selection(&mut self, motion: TextMotion) {
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
    pub fn handle_composer_mouse(&mut self, mouse: MouseEvent) -> TextInputOutcome {
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
    pub const fn composer_mouse_selection_active(&self) -> bool {
        self.composer.mouse_selection_active()
    }

    /// Move the composer cursor one rendered row up, if possible.
    pub fn move_composer_visual_up(&mut self) -> bool {
        self.move_composer_visual_up_with_history_reset(true)
    }

    /// Move the composer cursor one rendered row up without leaving history navigation.
    pub fn move_composer_visual_up_preserving_history(&mut self) -> bool {
        self.move_composer_visual_up_with_history_reset(false)
    }

    /// Move the composer cursor one rendered row down, if possible.
    pub fn move_composer_visual_down(&mut self) -> bool {
        self.move_composer_visual_down_with_history_reset(true)
    }

    /// Move the composer cursor one rendered row down without leaving history navigation.
    pub fn move_composer_visual_down_preserving_history(&mut self) -> bool {
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
    pub fn apply_model_status(&mut self, status: bcode_ipc::SessionModelStatus) {
        if status.provider_plugin_id.is_some() {
            self.selected_provider_plugin_id = status.provider_plugin_id;
        }
        if status.model_id.is_some() {
            self.selected_model_id = status.model_id;
        }
        self.reasoning_effort = status.reasoning_effort.clone();
        self.reasoning_summary = status.reasoning_summary.clone();
        self.reasoning_default_effort = status
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.default_effort.clone());
        self.reasoning_default_summary = status
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.default_summary.clone());
        self.refresh_thinking_label();
        let model = status
            .context_window
            .map(|context_window| bcode_model::ModelInfo {
                model_id: self.selected_model_id.clone().unwrap_or_default(),
                display_name: self.selected_model_id.clone().unwrap_or_default(),
                is_default: false,
                context_window: Some(context_window),
                max_output_tokens: status.max_output_tokens,
                capabilities: std::collections::BTreeSet::new(),
                reasoning: status.reasoning.clone(),
            });
        self.token_usage.apply_model_info(model.as_ref());
    }

    /// Return pending submissions that have not been committed by the session stream.
    #[must_use]
    pub fn pending_submissions(&self) -> &[PendingSubmission] {
        self.pending_submissions.items()
    }

    /// Return cached transcript layout.
    #[must_use]
    pub const fn transcript_layout(&self) -> &TranscriptLayoutCache {
        &self.transcript_layout
    }

    /// Return mutable cached transcript layout.
    #[must_use]
    pub const fn transcript_layout_mut(&mut self) -> &mut TranscriptLayoutCache {
        &mut self.transcript_layout
    }

    /// Return the number of transcript rows hidden below the viewport.
    #[must_use]
    pub const fn scroll_offset(&self) -> usize {
        self.viewport.offset()
    }

    /// Return whether older history may be available.
    #[must_use]
    pub const fn has_older_history(&self) -> bool {
        self.older_history.has_older_history()
    }

    /// Return whether an older-history request is in flight.
    #[must_use]
    pub const fn loading_older_history(&self) -> bool {
        self.older_history.loading()
    }

    /// Mark older history as loading or idle.
    pub const fn set_loading_older_history(&mut self, loading: bool) {
        self.older_history.set_loading(loading);
    }

    /// Return the cursor for loading older history.
    #[must_use]
    pub const fn older_history_cursor(&self) -> Option<SessionHistoryCursor> {
        self.older_history.cursor()
    }

    /// Return whether an older-history request should be started.
    #[must_use]
    pub const fn should_load_older_history(&self) -> bool {
        self.older_history.should_load()
    }

    /// Return the current activity state.
    #[must_use]
    pub const fn activity(&self) -> &ActivityState {
        &self.activity
    }

    /// Return the current status line.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Return configured key hints for the status line.
    #[must_use]
    pub fn key_hints(&self) -> &str {
        &self.key_hints
    }

    /// Store configured key hints for the status line.
    pub fn set_key_hints(&mut self, key_hints: String) {
        self.key_hints = key_hints;
    }

    /// Append a system-style transcript note.
    pub fn push_system_note(&mut self, text: String) {
        self.transcript.push(TranscriptItem::new("System", text));
    }

    /// Replace the current status line.
    pub fn set_status(&mut self, status: String) {
        self.status = status;
    }

    /// Return whether reasoning transcript items are visible.
    #[must_use]
    pub const fn reasoning_visible(&self) -> bool {
        self.reasoning_visible
    }

    /// Set whether reasoning transcript items are visible.
    pub fn set_reasoning_visible(&mut self, visible: bool) {
        self.reasoning_visible = visible;
        self.refresh_thinking_label();
    }

    /// Apply configured thinking display visibility.
    pub fn apply_thinking_config(&mut self, config: TuiThinkingConfig) {
        self.set_reasoning_visible(config.show);
    }

    fn refresh_thinking_label(&mut self) {
        let display = if self.reasoning_visible {
            "shown"
        } else {
            "hidden"
        };
        let effort = self
            .reasoning_effort
            .as_deref()
            .or(self.reasoning_default_effort.as_deref())
            .unwrap_or("provider default");
        let summary = self
            .reasoning_summary
            .as_deref()
            .or(self.reasoning_default_summary.as_deref())
            .unwrap_or("provider default");
        self.thinking_label = format!("{display} · effort: {effort} · summary: {summary}");
    }

    /// Mark the app as waiting for turn cancellation.
    pub fn set_cancelling(&mut self) {
        self.set_activity(ActivityState::Cancelling);
    }

    /// Return the app to idle activity.
    pub fn set_idle(&mut self) {
        self.set_activity(ActivityState::Idle);
    }

    /// Store the current composer text as a pending submission and clear input.
    pub fn stage_submission(&mut self) {
        let text = self.composer.buffer().text().to_owned();
        self.pending_submissions.stage(text);
        self.input_history.reset_navigation();
        self.composer.buffer_mut().clear();
    }

    /// Return the currently pending submission.
    pub fn take_pending_submission(&mut self) -> String {
        self.pending_submissions.take_staged()
    }

    /// Remove a pending submission that was handled outside the session transcript.
    pub fn clear_pending_submission(&mut self, text: &str) {
        self.pending_submissions.clear_staged_if(text);
        self.remove_pending_submission(text);
    }

    /// Mark the oldest pending submission as queued by the server.
    pub fn mark_pending_submission_queued(&mut self, queue_position: Option<u32>) {
        if let Some(pending) = self.pending_submissions.first_mut() {
            pending.mark_queued(queue_position);
        }
    }

    /// Mark the oldest pending submission as sent to the server.
    pub fn mark_pending_submission_sent(&mut self) {
        if let Some(pending) = self.pending_submissions.first_mut() {
            pending.mark_sent();
        }
    }

    /// Remove a pending submission and restore it into the composer.
    pub fn restore_pending_submission(&mut self, text: &str) {
        self.pending_submissions.clear_staged_if(text);
        self.remove_pending_submission(text);
        self.composer.buffer_mut().insert_str(text);
        self.wake_cursor();
    }

    /// Show the previous input-history entry, if available.
    pub fn previous_input_history(&mut self) -> bool {
        match self.input_history.previous(self.composer.buffer().text()) {
            InputHistoryOutcome::Entry { index, total, text } => {
                self.replace_composer_from_history(&text);
                self.status = format!("input history {index}/{total}");
            }
            InputHistoryOutcome::DraftRestored(text) => {
                self.replace_composer_from_history(&text);
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
    pub fn next_input_history(&mut self) -> bool {
        match self.input_history.next() {
            InputHistoryOutcome::Entry { index, total, text } => {
                self.replace_composer_from_history(&text);
                self.status = format!("input history {index}/{total}");
            }
            InputHistoryOutcome::DraftRestored(text) => {
                self.replace_composer_from_history(&text);
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
    pub const fn input_history_navigation_active(&self) -> bool {
        self.input_history.is_browsing()
    }

    /// Reset active input-history navigation after direct composer editing.
    pub fn reset_input_history_navigation(&mut self) {
        self.input_history.reset_navigation();
    }

    /// Scroll transcript up by rendered rows.
    pub fn scroll_transcript_up(&mut self, rows: usize) -> bool {
        self.viewport.scroll_up(rows, &mut self.older_history)
    }

    /// Scroll transcript down by rendered rows.
    pub const fn scroll_transcript_down(&mut self, rows: usize) -> bool {
        self.viewport.scroll_down(rows)
    }

    /// Pin transcript to the newest rows.
    pub const fn scroll_transcript_to_bottom(&mut self) -> bool {
        self.viewport.scroll_to_bottom(&mut self.older_history)
    }

    /// Sync cached rendered transcript scroll bounds from the latest frame.
    pub fn sync_transcript_scroll_max(&mut self, max_scroll_offset: usize) {
        self.viewport
            .sync_max(max_scroll_offset, &mut self.older_history);
    }

    /// Absorb replayed history events.
    pub fn absorb_history(&mut self, events: &[SessionEvent]) {
        for event in events {
            self.absorb_session_event(event);
        }
    }

    /// Prepend older history and preserve the current viewport.
    pub fn prepend_older_history(&mut self, events: &[SessionEvent], has_more: bool) {
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

        let mut older =
            transcript_items_from_events_with_reasoning(events, self.reasoning_visible());
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
    #[allow(clippy::too_many_lines)]
    pub fn absorb_session_event(&mut self, event: &SessionEvent) {
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
                ..
            } => {
                self.set_activity(ActivityState::Thinking);
                self.push_tool_result(tool_call_id, result, *is_error);
            }
            SessionEventKind::ToolInvocationStream { event } => {
                self.apply_tool_stream_event(event);
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
            SessionEventKind::ModelTurnCancelRequested { .. } => {
                self.set_cancelling();
                "cancellation requested".clone_into(&mut self.status);
            }
            SessionEventKind::ModelTurnFinished {
                outcome, message, ..
            } => self.finish_model_turn(*outcome, message.as_deref()),
            SessionEventKind::ModelUsage { turn_id, usage } => {
                self.push_model_usage(turn_id, usage);
            }
            SessionEventKind::ContextCompacted { summary, .. } => self.push_compaction(summary),
            SessionEventKind::WorkingDirectoryChanged {
                old_working_directory,
                new_working_directory,
            } => self.apply_working_directory_changed(old_working_directory, new_working_directory),
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
                if self.reasoning_visible() {
                    self.push_streaming_item("Reasoning", text);
                }
            }
            SessionEventKind::AssistantReasoningMessage { text } => {
                if self.reasoning_visible() {
                    self.finish_streaming_item("Reasoning", text);
                }
            }
            SessionEventKind::SessionCreated { name, .. } => self.session_title.clone_from(name),
            SessionEventKind::AgentChanged { agent_id } => {
                self.current_agent_id.clone_from(agent_id);
            }
            SessionEventKind::TraceEvent { trace } => self.apply_trace_event(trace),
            SessionEventKind::RuntimeWorkStarted { .. }
            | SessionEventKind::RuntimeWorkCancelRequested { .. }
            | SessionEventKind::RuntimeWorkProgress { .. }
            | SessionEventKind::RuntimeWorkFinished { .. } => self.apply_runtime_work_event(event),
            _ => {}
        }
    }

    pub fn apply_runtime_work_snapshots(&mut self, snapshots: &[bcode_ipc::RuntimeWorkSnapshot]) {
        self.runtime_work.apply_snapshots(snapshots);
        if let Some(status) = self.runtime_work.status_label() {
            self.status = status;
        }
        if self.runtime_work.is_cancelling() {
            self.set_cancelling();
        } else if self.runtime_work.is_busy() {
            self.set_activity(ActivityState::Thinking);
        }
    }

    /// Return whether the composer cursor should be visible.
    #[must_use]
    pub const fn cursor_visible(&self) -> bool {
        self.cursor.visible()
    }

    /// Reset cursor blink state after input.
    pub fn wake_cursor(&mut self) {
        self.cursor.wake();
    }

    /// Advance time-based UI state.
    pub fn tick(&mut self) -> bool {
        self.cursor.tick()
    }

    /// Return whether the TUI should exit.
    #[must_use]
    pub const fn should_exit(&self) -> bool {
        self.exit.requested()
    }

    /// Request TUI shutdown.
    pub const fn request_exit(&mut self) {
        self.exit.request();
    }

    /// Replace composer contents.
    pub fn replace_composer_with(&mut self, text: &str) {
        self.replace_composer_with_policy(text, true);
    }

    fn replace_composer_from_history(&mut self, text: &str) {
        self.replace_composer_with_policy(text, false);
    }

    fn replace_composer_with_policy(&mut self, text: &str, reset_history: bool) {
        if reset_history {
            self.input_history.reset_navigation();
        }
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

    fn apply_working_directory_changed(
        &mut self,
        old_working_directory: &std::path::Path,
        new_working_directory: &std::path::Path,
    ) {
        self.working_directory = Some(new_working_directory.to_path_buf());
        let message =
            working_directory_changed_message(old_working_directory, new_working_directory);
        self.transcript.push(TranscriptItem::new("System", message));
        self.status = format!("working directory: {}", new_working_directory.display());
    }

    fn push_streaming_item(&mut self, role: &'static str, text: &str) {
        push_streaming_transcript_item(&mut self.transcript, role, text);
    }

    fn finish_streaming_item(&mut self, role: &'static str, text: &str) {
        finish_streaming_transcript_item(&mut self.transcript, role, text);
    }

    fn push_tool_request(&mut self, tool_call_id: &str, tool_name: &str, arguments_json: &str) {
        let edit_summary = self.record_diff_summary(tool_name, arguments_json);
        self.tool_call_contexts.insert(
            tool_call_id.to_owned(),
            ToolCallContext {
                tool_name: tool_name.to_owned(),
                arguments_json: arguments_json.to_owned(),
            },
        );
        self.transcript
            .push(tool_request_item(tool_call_id, tool_name, arguments_json));
        if let Some(status) = edit_summary {
            self.set_file_activity(tool_name);
            self.set_tool_request_file_phase(tool_call_id, FileEditPhase::Pending);
            self.status = status;
        } else {
            self.set_activity(ActivityState::RunningTool {
                name: tool_name.to_owned(),
            });
            self.status = tool_request_status(tool_name, arguments_json)
                .unwrap_or_else(|| "started".to_owned());
        }
    }

    fn record_diff_summary(&mut self, tool_name: &str, arguments_json: &str) -> Option<String> {
        let (summary, lines) = diff_from_tool_request(tool_name, arguments_json)?;
        let status = format!(
            "{} · +{} -{}",
            summary.display_path(),
            summary.added,
            summary.removed
        );
        self.diff_panel.record(summary, lines);
        Some(status)
    }

    fn finish_live_tool_output(
        &mut self,
        tool_call_id: &str,
        is_error: Option<bool>,
        result: Option<&str>,
    ) -> bool {
        if let Some(context) = self.streamed_tool_results.get_mut(tool_call_id) {
            if let Some(index) = context.index
                && let Some(item) = self.transcript.get_mut(index)
            {
                if let Some(ShellResultPresentation::Terminal {
                    exit_code,
                    timed_out,
                    ..
                }) = result.and_then(terminal_shell_presentation)
                {
                    item.finish_terminal(exit_code, timed_out, is_error.unwrap_or(false));
                } else {
                    if let Some(is_error) = is_error {
                        item.set_terminal_error(is_error);
                    }
                    item.finish_streaming();
                }
            }
            return context.saw_output;
        }
        false
    }

    fn push_tool_result(&mut self, tool_call_id: &str, result: &str, is_error: bool) {
        if self.finish_live_tool_output(tool_call_id, Some(is_error), Some(result)) {
            if is_error {
                "failed".clone_into(&mut self.status);
                self.set_tool_request_file_phase(tool_call_id, FileEditPhase::Failed);
            } else if let Some(status) = self.tool_call_file_status(tool_call_id) {
                self.set_tool_request_file_phase(tool_call_id, FileEditPhase::Applied);
                self.status = format!("applied · {status}");
            } else {
                self.set_tool_request_file_phase(tool_call_id, FileEditPhase::Applied);
                "finished".clone_into(&mut self.status);
            }
            self.finish_tool_request_preview(tool_call_id);
            return;
        }
        let context = self.tool_call_contexts.get(tool_call_id);
        self.transcript.push(tool_result_item(
            tool_call_id,
            context.map(|context| context.tool_name.as_str()),
            context.map(|context| context.arguments_json.as_str()),
            result,
            is_error,
        ));
        if is_error {
            "failed".clone_into(&mut self.status);
            self.set_tool_request_file_phase(tool_call_id, FileEditPhase::Failed);
        } else if let Some(status) = self.tool_call_file_status(tool_call_id) {
            self.set_tool_request_file_phase(tool_call_id, FileEditPhase::Applied);
            self.status = format!("applied · {status}");
        } else {
            self.set_tool_request_file_phase(tool_call_id, FileEditPhase::Applied);
            "finished".clone_into(&mut self.status);
        }
        self.finish_tool_request_preview(tool_call_id);
    }

    fn apply_tool_stream_event(&mut self, event: &ToolInvocationStreamEvent) {
        match event {
            ToolInvocationStreamEvent::OutputDelta {
                tool_call_id,
                stream,
                text,
                ..
            } => self.push_tool_output_delta(tool_call_id, *stream, text),
            ToolInvocationStreamEvent::Status { message, .. } => {
                message.clone_into(&mut self.status);
            }
            ToolInvocationStreamEvent::Started {
                tool_call_id,
                tool_name,
                terminal,
                columns,
                rows,
            } => {
                if *terminal {
                    self.streamed_tool_results.insert(
                        tool_call_id.clone(),
                        StreamedToolResultContext {
                            index: None,
                            columns: columns.unwrap_or(120).max(1),
                            rows: rows.unwrap_or(24).max(1),
                            saw_output: false,
                        },
                    );
                }
                self.set_activity_for_tool_call(tool_call_id, tool_name);
                self.set_tool_request_file_phase(tool_call_id, FileEditPhase::Applying);
                if let Some(status) = self.tool_call_file_status(tool_call_id) {
                    self.status = status;
                } else {
                    tool_name.clone_into(&mut self.status);
                }
            }
            ToolInvocationStreamEvent::Finished {
                tool_call_id,
                is_error,
                ..
            } => {
                if let Some(context) = self.streamed_tool_results.get_mut(tool_call_id)
                    && let Some(index) = context.index
                    && let Some(item) = self.transcript.get_mut(index)
                {
                    item.set_terminal_error(*is_error);
                }
                self.finish_tool_request_preview(tool_call_id);
                if *is_error {
                    "failed".clone_into(&mut self.status);
                    self.set_tool_request_file_phase(tool_call_id, FileEditPhase::Failed);
                } else if let Some(status) = self.tool_call_file_status(tool_call_id) {
                    self.set_tool_request_file_phase(tool_call_id, FileEditPhase::Applied);
                    self.status = format!("applied · {status}");
                } else {
                    self.set_tool_request_file_phase(tool_call_id, FileEditPhase::Applied);
                    "finished".clone_into(&mut self.status);
                }
            }
        }
    }

    fn push_tool_output_delta(&mut self, tool_call_id: &str, stream: ToolOutputStream, text: &str) {
        if text.is_empty() {
            return;
        }
        if stream == ToolOutputStream::Pty {
            self.push_terminal_output_delta(tool_call_id, text);
            return;
        }
        if let Some(context) = self.streamed_tool_results.get(tool_call_id)
            && let Some(index) = context.index
            && let Some(item) = self.transcript.get_mut(index)
        {
            item.append_text(text);
            return;
        }
        let context = self.tool_call_contexts.get(tool_call_id);
        self.transcript.push(streaming_tool_output_item(
            tool_call_id,
            context.map(|context| context.tool_name.as_str()),
            context.map(|context| context.arguments_json.as_str()),
            text,
        ));
        self.streamed_tool_results.insert(
            tool_call_id.to_owned(),
            StreamedToolResultContext {
                index: Some(self.transcript.len().saturating_sub(1)),
                columns: 0,
                rows: 0,
                saw_output: true,
            },
        );
    }

    fn push_terminal_output_delta(&mut self, tool_call_id: &str, text: &str) {
        if let Some(context) = self.streamed_tool_results.get_mut(tool_call_id) {
            context.saw_output = true;
            if let Some(index) = context.index {
                if let Some(item) = self.transcript.get_mut(index) {
                    item.append_text(text);
                }
                return;
            }
            let tool_context = self.tool_call_contexts.get(tool_call_id);
            self.transcript.push(streaming_terminal_output_item(
                tool_call_id,
                tool_context.map(|context| context.tool_name.as_str()),
                text,
                context.columns,
                context.rows,
            ));
            context.index = Some(self.transcript.len().saturating_sub(1));
            return;
        }
        let tool_context = self.tool_call_contexts.get(tool_call_id);
        self.transcript.push(streaming_terminal_output_item(
            tool_call_id,
            tool_context.map(|context| context.tool_name.as_str()),
            text,
            120,
            24,
        ));
        self.streamed_tool_results.insert(
            tool_call_id.to_owned(),
            StreamedToolResultContext {
                index: Some(self.transcript.len().saturating_sub(1)),
                columns: 120,
                rows: 24,
                saw_output: true,
            },
        );
    }

    fn push_permission_request(
        &mut self,
        permission_id: &str,
        tool_call_id: &str,
        tool_name: &str,
        arguments_json: &str,
    ) {
        self.transcript.push(permission_request_item(
            permission_id,
            tool_call_id,
            tool_name,
            arguments_json,
        ));
        self.set_activity(ActivityState::WaitingPermission {
            name: tool_name.to_owned(),
        });
        self.set_tool_request_file_phase(tool_call_id, FileEditPhase::WaitingPermission);
        self.status = self.tool_call_file_status(tool_call_id).map_or_else(
            || {
                tool_request_status(tool_name, arguments_json)
                    .unwrap_or_else(|| tool_name.to_owned())
            },
            |status| format!("waiting permission · {status}"),
        );
    }

    fn set_permission_status(&mut self, permission_id: &str, approved: bool) {
        let status = if approved {
            "permission approved"
        } else {
            "permission denied"
        };
        if !approved && let Some(tool_call_id) = self.permission_tool_call_id(permission_id) {
            self.set_tool_request_file_phase(&tool_call_id, FileEditPhase::Failed);
            self.finish_tool_request_preview(&tool_call_id);
        }
        status.clone_into(&mut self.status);
        self.transcript
            .push(permission_result_item(permission_id, approved));
    }

    fn set_file_activity(&mut self, tool_name: &str) {
        let normalized = normalized_tool_name(tool_name);
        if matches!(normalized.as_str(), "filesystem_write" | "write") {
            self.set_activity(ActivityState::WritingFile);
        } else if matches!(normalized.as_str(), "filesystem_edit" | "edit") {
            self.set_activity(ActivityState::EditingFile);
        } else {
            self.set_activity(ActivityState::RunningTool {
                name: tool_name.to_owned(),
            });
        }
    }

    fn set_activity_for_tool_call(&mut self, tool_call_id: &str, fallback_tool_name: &str) {
        if let Some(context) = self.tool_call_contexts.get(tool_call_id) {
            let tool_name = context.tool_name.clone();
            self.set_file_activity(&tool_name);
        } else {
            self.set_file_activity(fallback_tool_name);
        }
    }

    fn tool_call_file_status(&self, tool_call_id: &str) -> Option<String> {
        let context = self.tool_call_contexts.get(tool_call_id)?;
        let (summary, _) = diff_from_tool_request(&context.tool_name, &context.arguments_json)?;
        Some(format!(
            "{} · +{} -{}",
            summary.display_path(),
            summary.added,
            summary.removed
        ))
    }

    fn set_tool_request_file_phase(&mut self, tool_call_id: &str, phase: FileEditPhase) {
        for item in self.transcript.iter_mut().rev() {
            let TranscriptItemKind::ToolRequest {
                tool_call_id: item_tool_call_id,
                file_edit_phase,
                ..
            } = item.kind_mut()
            else {
                continue;
            };
            if item_tool_call_id == tool_call_id {
                if file_edit_phase.is_some() {
                    *file_edit_phase = Some(phase);
                }
                break;
            }
        }
    }

    fn permission_tool_call_id(&self, permission_id: &str) -> Option<String> {
        self.transcript.iter().rev().find_map(|item| {
            let TranscriptItemKind::PermissionRequest {
                permission_id: item_permission_id,
                tool_call_id,
                ..
            } = item.kind()
            else {
                return None;
            };
            (item_permission_id == permission_id).then(|| tool_call_id.clone())
        })
    }

    fn finish_tool_request_preview(&mut self, tool_call_id: &str) {
        for item in self.transcript.iter_mut().rev() {
            let TranscriptItemKind::ToolRequest {
                tool_call_id: item_tool_call_id,
                ..
            } = item.kind_mut()
            else {
                continue;
            };
            if item_tool_call_id == tool_call_id {
                item.finish_streaming();
                break;
            }
        }
    }

    fn push_model_usage(&mut self, turn_id: &str, usage: &bcode_session_models::SessionTokenUsage) {
        self.token_usage.absorb(usage);
        if let Some(tokens) = usage.metered_total_tokens() {
            self.status = format!("tokens: {tokens}");
        }
        self.transcript.push(model_usage_item(turn_id, usage));
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

    fn apply_runtime_work_event(&mut self, event: &SessionEvent) {
        self.runtime_work.apply_event(event);
        if let Some(status) = self.runtime_work.status_label() {
            self.status = status;
        }
        if self.runtime_work.is_cancelling() {
            self.set_cancelling();
        } else if self.runtime_work.is_busy() {
            self.set_activity(ActivityState::Thinking);
        } else {
            self.set_activity(ActivityState::Idle);
        }
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
            | SessionTracePayload::ToolInvocationFinished { .. }
            | SessionTracePayload::ToolInvocationStreamEvent(_) => {}
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
                ..
            } => {
                let detail =
                    format!("provider stream tool assembled: {tool_name} ({argument_bytes} bytes)");
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
                            "provider stream idle for {idle_seconds}s while assembling {}",
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
            | SessionTracePhase::ToolInvocationOutput
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

pub const fn composer_policy() -> TextInputPolicy {
    TextInputPolicy::chat_composer()
}

fn normalized_tool_name(tool_name: &str) -> String {
    tool_name.replace(['-', '.'], "_").to_ascii_lowercase()
}

fn is_shell_tool_name(tool_name: &str) -> bool {
    matches!(
        normalized_tool_name(tool_name).as_str(),
        "shell" | "shell_run" | "filesystem_shell_run" | "bash"
    )
}

fn tool_request_status(tool_name: &str, arguments_json: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(arguments_json).ok()?;
    let normalized = normalized_tool_name(tool_name);
    if is_shell_tool_name(tool_name) {
        return value
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .map(|cwd| format!("cwd {cwd}"));
    }
    match normalized.as_str() {
        "filesystem_read" | "read" | "filesystem_exists" | "exists" | "filesystem_stat"
        | "stat" => value
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        "filesystem_list" | "list" | "filesystem_find" | "find" | "filesystem_grep" | "grep" => {
            value
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        }
        _ => None,
    }
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

fn terminal_shell_presentation(result: &str) -> Option<ShellResultPresentation> {
    match tool_result_presentation(Some("shell.run"), result)? {
        ToolResultPresentation::Shell(shell @ ShellResultPresentation::Terminal { .. }) => {
            Some(shell)
        }
        _ => None,
    }
}

fn working_directory_changed_message(
    old_working_directory: &std::path::Path,
    new_working_directory: &std::path::Path,
) -> String {
    format!(
        "Working directory changed from `{}` to `{}`. Treat prior file/path assumptions as possibly stale unless reconfirmed.",
        old_working_directory.display(),
        new_working_directory.display()
    )
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
        | SessionEventKind::WorkingDirectoryChanged { .. }
        | SessionEventKind::SkillInvoked { .. }
        | SessionEventKind::SkillInvocationFailed { .. }
        | SessionEventKind::RuntimeWorkStarted { .. }
        | SessionEventKind::RuntimeWorkCancelRequested { .. }
        | SessionEventKind::RuntimeWorkProgress { .. }
        | SessionEventKind::RuntimeWorkFinished { .. }
        | SessionEventKind::ToolInvocationStream { .. }
        | SessionEventKind::AssistantReasoningDelta { .. }
        | SessionEventKind::AssistantReasoningMessage { .. } => true,
        SessionEventKind::SkillSuggested { reason, .. } => reason.is_some(),
        SessionEventKind::SessionCreated { .. }
        | SessionEventKind::ClientAttached { .. }
        | SessionEventKind::ClientDetached { .. }
        | SessionEventKind::ModelChanged { .. }
        | SessionEventKind::AgentChanged { .. }
        | SessionEventKind::ModelTurnStarted { .. }
        | SessionEventKind::ModelTurnCancelRequested { .. }
        | SessionEventKind::ModelTurnFinished { .. }
        | SessionEventKind::SessionRenamed { .. }
        | SessionEventKind::SessionImported { .. }
        | SessionEventKind::SkillActivated { .. }
        | SessionEventKind::SkillDeactivated { .. }
        | SessionEventKind::SkillContextLoaded { .. }
        | SessionEventKind::TraceEvent { .. } => false,
    }
}
