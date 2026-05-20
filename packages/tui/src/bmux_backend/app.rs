//! BMUX backend app state.

use std::time::Instant;

use bcode_session_models::{
    ModelTurnOutcome, SessionEvent, SessionEventKind, SessionHistoryCursor, SessionId,
    SessionInputHistoryEntry,
};
use bcode_skill_models::SkillSource;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::diff::DiffFileSummary;

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
    changed_files: Vec<DiffFileSummary>,
    pending_submissions: Vec<PendingSubmission>,
    pending_submission: Option<String>,
    scroll_offset: usize,
    older_history_cursor: Option<SessionHistoryCursor>,
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
            pending_submissions: Vec::new(),
            pending_submission: None,
            scroll_offset: 0,
            older_history_cursor: history.first().map(|event| SessionHistoryCursor {
                sequence: event.sequence,
            }),
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
    }

    /// Return the cursor for loading older history.
    #[must_use]
    pub(super) const fn older_history_cursor(&self) -> Option<SessionHistoryCursor> {
        self.older_history_cursor
    }

    /// Return whether the viewport is near the oldest loaded event.
    #[must_use]
    pub(super) const fn should_load_older_history(&self) -> bool {
        self.older_history_cursor.is_some() && !self.loading_older_history && self.scroll_offset > 0
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

    /// Prepend older history and preserve approximate viewport position.
    pub(super) fn prepend_older_history(&mut self, events: &[SessionEvent], has_more: bool) {
        let added = events.len();
        let mut older = Vec::with_capacity(self.transcript.len().saturating_add(added));
        for event in events {
            if let Some(item) = transcript_item_from_event(event) {
                older.push(item);
            }
        }
        older.append(&mut self.transcript);
        self.transcript = older;
        self.scroll_offset = self.scroll_offset.saturating_add(added);
        self.older_history_cursor = if has_more {
            events.first().map(|event| SessionHistoryCursor {
                sequence: event.sequence,
            })
        } else {
            None
        };
        self.loading_older_history = false;
    }

    /// Absorb one live session event.
    pub(super) fn absorb_session_event(&mut self, event: &SessionEvent) {
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
        let Some(summary) = diff_summary_from_tool_request(tool_name, arguments_json) else {
            return;
        };
        let path = summary.display_path();
        if let Some(existing) = self
            .changed_files
            .iter_mut()
            .find(|existing| existing.display_path() == path)
        {
            *existing = summary;
        } else {
            self.changed_files.push(summary);
        }
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

fn transcript_item_from_event(event: &SessionEvent) -> Option<TranscriptItem> {
    match &event.kind {
        SessionEventKind::UserMessage { text, .. } => {
            Some(TranscriptItem::new("You", text.clone()))
        }
        SessionEventKind::AssistantDelta { text } => {
            Some(TranscriptItem::new_streaming("Assistant", text.clone()))
        }
        SessionEventKind::AssistantMessage { text } => {
            Some(TranscriptItem::new("Assistant", text.clone()))
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
        SessionEventKind::AssistantReasoningDelta { text } => {
            Some(TranscriptItem::new_streaming("Reasoning", text.clone()))
        }
        SessionEventKind::AssistantReasoningMessage { text } => {
            Some(TranscriptItem::new("Reasoning", text.clone()))
        }
        SessionEventKind::PermissionResolved { .. }
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

fn optional_u32(value: Option<u32>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

fn tool_request_item(tool_call_id: &str, tool_name: &str, arguments_json: &str) -> TranscriptItem {
    let diff_note = diff_summary_from_tool_request(tool_name, arguments_json).map_or_else(
        String::new,
        |summary| {
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

fn diff_summary_from_tool_request(
    tool_name: &str,
    arguments_json: &str,
) -> Option<DiffFileSummary> {
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
    Some(DiffFileSummary::new(path, added, removed))
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
