//! BMUX backend app state.

use std::time::Instant;

use bcode_session_models::{
    ModelTurnOutcome, SessionEvent, SessionEventKind, SessionId, SessionInputHistoryEntry,
};
use bmux_text_edit::TextEditBuffer;

use super::IDLE_REDRAW_INTERVAL;

/// State owned by the BMUX-native backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BmuxApp {
    session_id: Option<SessionId>,
    composer: TextEditBuffer,
    input_history: Vec<String>,
    input_history_index: Option<usize>,
    input_history_draft: Option<String>,
    transcript: Vec<TranscriptItem>,
    pending_submissions: Vec<PendingSubmission>,
    pending_submission: Option<String>,
    scroll_offset: usize,
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
            pending_submissions: Vec::new(),
            pending_submission: None,
            scroll_offset: 0,
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
    pub(super) const fn scroll_transcript_up(&mut self, rows: usize) -> bool {
        if rows == 0 {
            return false;
        }
        self.scroll_offset = self.scroll_offset.saturating_add(rows);
        true
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
        changed
    }

    /// Absorb replayed history events.
    pub(super) fn absorb_history(&mut self, events: &[SessionEvent]) {
        for event in events {
            self.absorb_session_event(event);
        }
    }

    /// Absorb one live session event.
    pub(super) fn absorb_session_event(&mut self, event: &SessionEvent) {
        match &event.kind {
            SessionEventKind::UserMessage { text, .. } => {
                self.activity = ActivityState::Thinking;
                self.push_user_message(text);
            }
            SessionEventKind::AssistantDelta { text } => {
                self.activity = ActivityState::Streaming;
                self.push_streaming_item("Assistant", text);
            }
            SessionEventKind::AssistantMessage { text } => {
                self.finish_streaming_item("Assistant", text);
            }
            SessionEventKind::SystemMessage { text } => self.push_system_message(text),
            SessionEventKind::ToolCallRequested { tool_name, .. } => {
                self.push_tool_request(tool_name);
            }
            SessionEventKind::ToolCallFinished {
                result, is_error, ..
            } => {
                self.activity = ActivityState::Thinking;
                self.push_tool_result(result, *is_error);
            }
            SessionEventKind::PermissionRequested { tool_name, .. } => {
                self.push_permission_request(tool_name);
            }
            SessionEventKind::PermissionResolved { approved, .. } => {
                self.activity = ActivityState::Thinking;
                self.set_permission_status(*approved);
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
            SessionEventKind::ModelUsage { usage, .. } => {
                if let Some(tokens) = usage.metered_total_tokens() {
                    self.status = format!("tokens: {tokens}");
                }
            }
            SessionEventKind::ContextCompacted { summary, .. } => self.push_compaction(summary),
            SessionEventKind::SessionRenamed { name } => {
                self.set_session_name_status(name.as_deref());
            }
            SessionEventKind::SkillInvoked { skill_id, .. } => {
                self.transcript
                    .push(TranscriptItem::new("Skill", format!("invoked {skill_id}")));
            }
            SessionEventKind::SkillSuggested { skill_id, .. } => {
                self.status = format!("suggested skill: {skill_id}");
            }
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

    fn replace_composer_with(&mut self, text: &str) {
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

    fn push_tool_request(&mut self, tool_name: &str) {
        self.transcript
            .push(TranscriptItem::new("Tool", format!("running {tool_name}")));
        self.activity = ActivityState::RunningTool {
            name: tool_name.to_owned(),
        };
        self.status = format!("running tool {tool_name}");
    }

    fn push_tool_result(&mut self, result: &str, is_error: bool) {
        let label = if is_error { "Tool error" } else { "Tool" };
        self.transcript
            .push(TranscriptItem::new(label, result.to_owned()));
        if is_error {
            "tool failed".clone_into(&mut self.status);
        } else {
            "tool finished".clone_into(&mut self.status);
        }
    }

    fn push_permission_request(&mut self, tool_name: &str) {
        self.transcript.push(TranscriptItem::new(
            "Permission",
            format!("waiting for approval: {tool_name}"),
        ));
        self.activity = ActivityState::WaitingPermission {
            name: tool_name.to_owned(),
        };
        self.status = format!("waiting for permission: {tool_name}");
    }

    fn set_permission_status(&mut self, approved: bool) {
        if approved {
            "permission approved".clone_into(&mut self.status);
        } else {
            "permission denied".clone_into(&mut self.status);
        }
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

    fn push_skill_error(&mut self, skill_id: &impl std::fmt::Display, error: &str) {
        self.transcript.push(TranscriptItem::new(
            "Skill error",
            format!("{skill_id}: {error}"),
        ));
    }
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
