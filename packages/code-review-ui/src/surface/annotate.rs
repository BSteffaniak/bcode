//! Comments, threads, and AI questions on the surface.
//!
//! * Annotations load when the review opens (and re-open); every change goes through
//!   [`ReviewNotes`](crate::notes::ReviewNotes) and is applied to the document at once
//!   (`set_annotations`), so threads appear, change, and disappear before their calls finish.
//! * The composer is the document's composer item. Opening it moves the cursor and keyboard
//!   focus there; closing it returns both to the review.
//! * Asking Bcode records a pending exchange at once and puts the cursor on its card; the
//!   question is sent from the next poll and the card follows the linked session live, without
//!   leaving the review. Opening the session in Bcode (`Action::OpenSession`) remembers the
//!   exact cursor and scroll position to restore on return.

use super::{Focus, Review, ReviewSurface, TRANSIENT_WORKSPACE_ID, ThreadRows};
use crate::action::Action;
use crate::agent::{self, AskContext};
use crate::composer::{Composer, ComposerButton, ComposerInput, ComposerMode};
use crate::context::UiContext;
use crate::loader::Message;
use crate::notes::{self, MutationResult, NoteScope};
use crate::rows::{AgentAction, ThreadAction};
use bcode_code_review::document::{
    AnchorTarget, DocAnchor, ItemKey, LineKind, PairSide, ReviewItem,
};
use bcode_code_review_models::{
    REVIEW_AI_EXCHANGE_SCHEMA_VERSION, ReviewAiExchange, ReviewAiExchangeStatus, ReviewAnchorKind,
    ReviewLineSide, ReviewSuggestionStatus, ReviewWorkspace,
};
use bmux_keyboard::KeyStroke;
use bmux_tui::text::{Line, Span};
use bmux_tui_components::status_bar::StatusSeverity;
use bmux_tui_components::toast_stack::ToastSeverity;
use bmux_tui_components::tree_view::TreeViewItem;
use bmux_tui_components::virtual_list::{AnchorPolicy, Edge};

/// Footer status while a question is being sent to Bcode.
const ASKING_STATUS: &str = "asking Bcode…";

/// Keymap scope of the composer.
pub const COMPOSER_SCOPE: &str = "code-review-composer";

/// Milliseconds since the Unix epoch.
pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

impl ReviewSurface {
    /// The workspace annotations and viewed state belong to (a transient one when the surface
    /// was opened without one).
    pub(super) fn workspace_mut(&mut self) -> &mut ReviewWorkspace {
        let (repo, sources, title) = (&self.repo_path, &self.sources, &self.title);
        self.workspace.get_or_insert_with(|| ReviewWorkspace {
            id: TRANSIENT_WORKSPACE_ID.to_owned(),
            title: title.clone(),
            repo_root: repo.clone(),
            sources: sources.clone(),
            created_at_ms: None,
            updated_at_ms: None,
            viewed_files: std::collections::BTreeSet::new(),
            archived_at_ms: None,
            viewed_marks: Vec::new(),
            viewed_threads: Vec::new(),
            reviewed_revisions: std::collections::BTreeMap::new(),
            presentation: None,
            stack_snapshots: Vec::new(),
        })
    }

    pub(super) fn note_scope(&mut self) -> NoteScope {
        NoteScope::for_workspace(self.workspace_mut())
    }

    /// List the review's annotations for the current document.
    pub(super) fn load_notes(&mut self, host: &UiContext) {
        let scope = self.note_scope();
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        let generation = self.generation;
        host.spawn(Box::pin(async move {
            let result = notes::load(services, scope).await;
            inbox.push(Message::NotesLoaded { generation, result });
        }));
    }

    /// Start the next queued annotation call and the next queued question.
    pub(super) fn pump_notes(&mut self, host: &UiContext) {
        if let Some(mutation) = self.notes.start_next() {
            let scope = self.note_scope();
            let services = self.services.clone();
            let inbox = self.inbox.clone();
            host.spawn(Box::pin(async move {
                let result = notes::run(services, scope, mutation).await;
                inbox.push(Message::NoteDone(result));
            }));
        }
        while let Some((exchange, text)) = self.asks.pop_front() {
            if exchange.session_id.is_none() {
                self.fresh_asks.insert(exchange.exchange_id.clone());
            }
            let scope = self.note_scope();
            // A new session is bound to the review, so its review tools act on this review.
            let binding = self.assistant_binding(String::new());
            let services = self.services.clone();
            let inbox = self.inbox.clone();
            host.spawn(Box::pin(async move {
                let result = agent::ask(services, scope, exchange, text, binding, now_ms()).await;
                inbox.push(Message::Asked(Box::new(result)));
            }));
        }
        let sessions: Vec<String> = self
            .notes
            .exchanges()
            .iter()
            .filter_map(|exchange| exchange.session_id.clone())
            .chain(self.assistant_sessions())
            .collect();
        if self
            .agents
            .ensure(host, sessions.iter().map(String::as_str))
        {
            self.sync_notes();
        }
    }

    /// A finished annotation call.
    pub(super) fn finish_note(&mut self, result: MutationResult) {
        if let Some(error) = self.notes.finish(result) {
            self.toasts
                .push(ToastSeverity::Error, "Not saved", Some(error));
        }
        self.sync_notes();
    }

    /// A finished question: keep the exchange; its card follows the linked session.
    pub(super) fn finish_ask(
        &mut self,
        result: Result<ReviewAiExchange, Box<(ReviewAiExchange, String)>>,
    ) {
        // The footer said a question was on its way; it is no longer.
        if self
            .status
            .as_ref()
            .is_some_and(|status| status.text == ASKING_STATUS)
        {
            self.status = None;
        }
        match result {
            Ok(exchange) => {
                // A question that started its own session: follow every review tool call of
                // that session, including ones that finished before it was first observed.
                if self.fresh_asks.remove(&exchange.exchange_id)
                    && let Some(session) = &exchange.session_id
                {
                    self.dock.follow_new_session(session);
                }
                self.notes.put_exchange(exchange);
            }
            Err(failed) => {
                let (exchange, error) = *failed;
                self.fresh_asks.remove(&exchange.exchange_id);
                self.notes.put_exchange(exchange);
                self.set_status("could not ask Bcode", StatusSeverity::Error);
                self.toasts
                    .push(ToastSeverity::Error, "Could not ask Bcode", Some(error));
            }
        }
        self.sync_notes();
    }

    /// Show the current annotations in the document when they changed.
    pub(super) fn sync_notes(&mut self) {
        let revision = self.notes.revision().wrapping_mul(2) | u64::from(self.hide_resolved);
        let Some(review) = self.review.as_mut() else {
            return;
        };
        if review.notes_applied == Some(revision) {
            return;
        }
        review.notes_applied = Some(revision);
        let mut annotations = self.notes.annotations();
        if self.hide_resolved {
            // A thread is resolved when every comment is (as the document decides).
            let open: std::collections::BTreeSet<String> = annotations
                .drafts
                .iter()
                .filter(|draft| draft.resolved_at_ms.is_none())
                .map(|draft| draft.thread_id.clone())
                .collect();
            annotations
                .drafts
                .retain(|draft| open.contains(&draft.thread_id));
        }
        review.doc.set_annotations(&annotations);
        review.note_splices();
        review.threads.borrow_mut().revision = None;
        review.marks_revision = None;
        self.refresh_threads();
    }

    // ----------------------------------------------------------------------------------------
    // Composer
    // ----------------------------------------------------------------------------------------

    /// Open the composer at `anchor` and focus it.
    pub(super) fn open_composer(&mut self, anchor: DocAnchor, mode: ComposerMode, text: &str) {
        let Some(review) = self.review.as_mut() else {
            return;
        };
        review.doc.open_composer(anchor.clone());
        review.note_splices();
        if let Some(index) = review.doc.index_of(&ItemKey::Composer) {
            review.set_cursor(index, AnchorPolicy::EnsureVisibleFrom(Edge::Bottom));
        }
        self.composer = Some(Composer::new(anchor, mode, text));
        self.focus = Focus::Composer;
    }

    /// Close the composer; the cursor returns to the commented line.
    pub(super) fn close_composer(&mut self) {
        let Some(composer) = self.composer.take() else {
            return;
        };
        self.focus = Focus::Review;
        let Some(review) = self.review.as_mut() else {
            return;
        };
        review.doc.close_composer();
        review.note_splices();
        review.selection = None;
        let back = match &composer.mode {
            ComposerMode::Reply { thread_id } => Some(ItemKey::Thread {
                thread_id: thread_id.clone(),
            }),
            _ => anchor_key(&review.doc, &composer.anchor),
        };
        if let Some(index) = back.and_then(|key| review.doc.index_of(&key)) {
            review.set_cursor(index, AnchorPolicy::EnsureVisibleFrom(Edge::Bottom));
        }
    }

    /// Submit the composer the other way: a reply to a pull request thread sent now instead of
    /// held, or held instead of sent.
    pub(super) fn submit_composer_alternate(&mut self) {
        self.submit_with(true);
    }

    /// Submit the composer.
    pub(super) fn submit_composer(&mut self) {
        self.submit_with(false);
    }

    fn submit_with(&mut self, alternate: bool) {
        let Some(composer) = self.composer.as_ref() else {
            return;
        };
        let text = composer.text();
        if text.trim().is_empty() {
            self.close_composer();
            self.set_status("empty comment discarded", StatusSeverity::Info);
            return;
        }
        let anchor = composer.anchor.clone();
        let mode = composer.mode.clone();
        let classify = (composer.kind, composer.severity);
        let remote_reply = composer.remote_reply;
        self.close_composer();
        let now = now_ms();
        let result = match mode {
            ComposerMode::Comment => {
                self.notes
                    .add_draft(anchor.to_draft_anchor(), text, None, Some(classify), now);
                Ok("draft saved")
            }
            ComposerMode::Reply { thread_id } if remote_reply.is_some() => {
                let send_now = (remote_reply
                    == Some(crate::config::conversation::ReplyMode::SendNow))
                    != alternate;
                self.reply_remote(&thread_id, text, send_now)
            }
            ComposerMode::Conversation { pull, .. } => self.post_conversation_comment(&pull, text),
            ComposerMode::Reply { thread_id } => {
                match self
                    .notes
                    .thread(&thread_id)
                    .first()
                    .map(|first| first.anchor.clone())
                {
                    Some(anchor) => {
                        self.notes
                            .add_draft(anchor, text, Some(thread_id), None, now);
                        Ok("reply saved")
                    }
                    None => Err("the thread no longer exists".to_owned()),
                }
            }
            ComposerMode::Edit { comment_id } => self
                .notes
                .update_draft(&comment_id, text)
                .map(|()| "comment updated"),
            ComposerMode::Ask => {
                self.ask_question(&anchor, text, now);
                Ok(ASKING_STATUS)
            }
        };
        match result {
            Ok(message) => self.set_status(message, StatusSeverity::Success),
            Err(error) => self.set_status(error, StatusSeverity::Error),
        }
        self.sync_notes();
    }

    /// Keys while the composer has focus.
    pub(super) fn composer_key(&mut self, stroke: KeyStroke, host: &UiContext) -> Action {
        let action = host.keymap_action(COMPOSER_SCOPE, stroke);
        let action = action
            .as_deref()
            .map(|action| action.strip_prefix("bcode.code_review.").unwrap_or(action));
        let Some(composer) = self.composer.as_mut() else {
            self.focus = Focus::Review;
            return Action::Redraw;
        };
        // Open suggestions take their keys before the composer's bindings.
        if composer.completion_key(stroke).is_some() {
            return Action::Redraw;
        }
        match action {
            Some("composer_submit") => self.submit_composer(),
            Some("composer_cancel") => {
                self.close_composer();
                self.set_status("comment canceled", StatusSeverity::Info);
            }
            Some("composer_preview") => self.composer_button(ComposerButton::Preview),
            Some("composer_switch") => composer.switch(),
            Some("composer_kind") => {
                composer.kind = notes::next_kind(composer.kind);
                composer.touch();
            }
            Some("composer_severity") => {
                composer.severity = notes::next_severity(composer.severity);
                composer.touch();
            }
            _ => match composer.key(stroke) {
                ComposerInput::Submit => self.submit_composer(),
                ComposerInput::Edited => self.complete_paths(host),
                ComposerInput::Ignored => return Action::None,
            },
        }
        Action::Redraw
    }

    /// Search paths for the composer's `@word` after an edit, when it changed.
    pub(super) fn complete_paths(&mut self, host: &UiContext) {
        let Some(request) = self
            .composer
            .as_mut()
            .and_then(Composer::refresh_completion)
        else {
            return;
        };
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        let search = bcode_code_review_models::ReviewFilesSearchRequest {
            repo_path: self.repo_path.clone(),
            query: request.query,
            revision: Some(bcode_code_review_models::RevisionRef::WorkingTree),
            cursor: None,
            limit: Some(u32::from(crate::path_completion::MAX_SUGGESTIONS)),
        };
        host.spawn(Box::pin(async move {
            let result = services.call(search).await.map_err(|error| error.message);
            inbox.push(Message::PathCompletions {
                generation: request.generation,
                result,
            });
        }));
    }

    /// A clicked composer button.
    pub(super) fn composer_button(&mut self, button: ComposerButton) {
        let Some(composer) = self.composer.as_mut() else {
            return;
        };
        self.focus = Focus::Composer;
        match button {
            ComposerButton::Submit => self.submit_composer(),
            ComposerButton::Alternate => self.submit_composer_alternate(),
            ComposerButton::Switch => composer.switch(),
            ComposerButton::Preview => {
                composer.preview = !composer.preview;
                composer.touch();
            }
            ComposerButton::Cancel => self.close_composer(),
            ComposerButton::Suggestion(index) => composer.accept_suggestion(index),
        }
    }

    /// Anchor of a new comment from the cursor: the selected range, else the cursor's line or
    /// file header.
    pub(super) fn cursor_anchor(&self) -> Option<DocAnchor> {
        let review = self.review.as_ref()?;
        if let Some(selection) = &review.selection {
            let end = if selection.follow {
                review.cursor.as_ref()?
            } else {
                &selection.end
            };
            if let Some(anchor) = review.doc.range_anchor(&selection.anchor, end) {
                return Some(anchor);
            }
        }
        review.doc.anchor_for(review.cursor.as_ref()?)
    }

    /// Start a comment (or a question) at the cursor; on a thread, reply to it; asking on an AI
    /// answer asks a follow-up in its session.
    pub(super) fn start_comment(&mut self, ask: bool) {
        if ask && self.agent_at_cursor().is_some() {
            self.follow_up();
            return;
        }
        if self.thread_at_cursor().is_some() {
            let action = if ask {
                ThreadAction::Ask
            } else {
                ThreadAction::Reply
            };
            self.thread_action(action);
            return;
        }
        match self.cursor_anchor() {
            Some(anchor) => {
                let mode = if ask {
                    ComposerMode::Ask
                } else {
                    ComposerMode::Comment
                };
                self.open_composer(anchor, mode, "");
            }
            None => self.set_status(
                "move to a diff line or file header to comment",
                StatusSeverity::Warning,
            ),
        }
    }

    /// A file-level or review-level comment.
    pub(super) fn start_scoped_comment(&mut self, file: bool) {
        self.start_scoped(file, ComposerMode::Comment);
    }

    /// Ask Bcode about the cursor's file.
    pub(super) fn ask_about_file(&mut self) {
        self.start_scoped(true, ComposerMode::Ask);
    }

    /// Show only the cursor file's header, or its content again.
    pub(super) fn toggle_file_collapsed(&mut self) {
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let Some(file) = review.cursor_file() else {
            self.set_status("move into a file to collapse it", StatusSeverity::Warning);
            return;
        };
        let collapsed = !review.doc.is_collapsed(&file);
        review.doc.set_collapsed(&file, collapsed);
        review.note_splices();
        let header = ItemKey::FileHeader(file);
        if let Some(index) = review.doc.index_of(&header) {
            review.set_cursor(index, AnchorPolicy::EnsureVisibleFrom(Edge::Top));
        }
        self.persist_presentation(true);
    }

    /// Copy the cursor file's path.
    pub(super) fn copy_file_path(&mut self) {
        let Some(path) = self
            .review
            .as_ref()
            .and_then(super::Review::cursor_file)
            .map(|file| file.path.clone())
        else {
            self.set_status("move into a file to copy its path", StatusSeverity::Warning);
            return;
        };
        self.clipboard = Some((path, "path"));
    }

    fn start_scoped(&mut self, file: bool, mode: ComposerMode) {
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let anchor = if file {
            let Some(file) = review.cursor_file() else {
                self.set_status("move into a file to comment on it", StatusSeverity::Warning);
                return;
            };
            DocAnchor {
                head_oid: review.doc.head_oid(&file),
                blob_oid: None,
                file: Some(file),
                target: AnchorTarget::File,
            }
        } else {
            DocAnchor::review()
        };
        self.open_composer(anchor, mode, "");
    }

    // ----------------------------------------------------------------------------------------
    // Threads and AI answers
    // ----------------------------------------------------------------------------------------

    /// Thread id of the cursor item.
    pub(super) fn thread_at_cursor(&self) -> Option<String> {
        match self.review.as_ref()?.cursor.as_ref()? {
            ItemKey::Thread { thread_id } => Some(thread_id.clone()),
            _ => None,
        }
    }

    pub(super) fn agent_at_cursor(&self) -> Option<String> {
        let review = self.review.as_ref()?;
        match review.doc.item(review.cursor_index)? {
            ReviewItem::AgentThread(agent) => agent.exchange_id.clone(),
            _ => None,
        }
    }

    /// Run a thread action on the cursor's thread.
    pub(super) fn thread_action(&mut self, action: ThreadAction) -> Option<Action> {
        if action == ThreadAction::Fold {
            self.fold();
            return None;
        }
        if action == ThreadAction::Viewed {
            if !self.toggle_thread_viewed() {
                self.set_status("move to a thread first", StatusSeverity::Warning);
            }
            return None;
        }
        if let Some(thread_id) = self.thread_at_cursor()
            && self.remote_thread_action(&thread_id, action, None)
        {
            return None;
        }
        if self.refuse_external_thread() {
            return None;
        }
        let Some(thread_id) = self.thread_at_cursor() else {
            self.set_status("move to a thread first", StatusSeverity::Warning);
            return None;
        };
        let comments = self.notes.thread(&thread_id);
        let first = comments.first().map(|comment| (*comment).clone());
        let latest = comments.last().map(|comment| (*comment).clone());
        let first = first?;
        let anchor = self.doc_anchor(&first.anchor);
        let result = match action {
            ThreadAction::Reply => {
                self.open_composer(anchor, ComposerMode::Reply { thread_id }, "");
                Ok(None)
            }
            ThreadAction::Edit => match latest {
                Some(latest) if notes::is_local(&latest.comment_id) => {
                    Err("this comment is still being saved; try again after save".to_owned())
                }
                Some(latest) => {
                    self.open_composer(
                        anchor,
                        ComposerMode::Edit {
                            comment_id: latest.comment_id,
                        },
                        &latest.body,
                    );
                    Ok(None)
                }
                None => Ok(None),
            },
            ThreadAction::Delete => latest.map_or(Ok(None), |latest| {
                self.notes
                    .delete_draft(&latest.comment_id)
                    .map(|()| Some("comment deleted"))
            }),
            ThreadAction::Resolve => self.resolve_local(&thread_id, first.resolved_at_ms.is_none()),
            ThreadAction::Ask => {
                self.open_composer(anchor, ComposerMode::Ask, "");
                Ok(None)
            }
            ThreadAction::Fold => {
                self.fold();
                return None;
            }
            ThreadAction::Accept => {
                self.decide_suggestion(ReviewSuggestionStatus::Accepted);
                return None;
            }
            ThreadAction::Refine => {
                self.decide_suggestion(ReviewSuggestionStatus::Refining);
                return None;
            }
            ThreadAction::Reject => {
                self.decide_suggestion(ReviewSuggestionStatus::Rejected);
                return None;
            }
            ThreadAction::Open => {
                self.set_status("this thread has no web page", StatusSeverity::Info);
                return None;
            }
            ThreadAction::Viewed => return None,
        };
        match result {
            Ok(Some(message)) => self.set_status(message, StatusSeverity::Success),
            Ok(None) => {}
            Err(error) => self.set_status(error, StatusSeverity::Warning),
        }
        self.sync_notes();
        None
    }

    /// Resolve (or reopen) your own thread `thread_id`.
    fn resolve_local(
        &mut self,
        thread_id: &str,
        resolved: bool,
    ) -> Result<Option<&'static str>, String> {
        self.notes.resolve(thread_id, resolved, now_ms())?;
        self.sync_notes();
        self.resolved_by_you(thread_id, resolved);
        Ok(Some(if resolved {
            "thread resolved"
        } else {
            "thread reopened"
        }))
    }

    /// Cycle the cursor thread's kind or severity.
    pub(super) fn classify_thread(&mut self, severity: bool) {
        if self.refuse_external_thread() {
            return;
        }
        let Some(thread_id) = self.thread_at_cursor() else {
            self.set_status("move to a thread first", StatusSeverity::Warning);
            return;
        };
        let Some((kind, level)) = self
            .notes
            .thread(&thread_id)
            .first()
            .map(|first| (first.thread_kind, first.severity))
        else {
            return;
        };
        let (kind, level) = if severity {
            (kind, notes::next_severity(level))
        } else {
            (notes::next_kind(kind), level)
        };
        if let Err(error) = self.notes.classify(&thread_id, kind, level) {
            self.set_status(error, StatusSeverity::Warning);
        }
        self.sync_notes();
    }

    /// Run an AI answer action on the cursor's answer.
    pub(super) fn agent_action(&mut self, action: AgentAction) -> Option<Action> {
        match action {
            AgentAction::Accept => self.decide_suggestion(ReviewSuggestionStatus::Accepted),
            AgentAction::Refine => self.decide_suggestion(ReviewSuggestionStatus::Refining),
            AgentAction::Reject => self.decide_suggestion(ReviewSuggestionStatus::Rejected),
            AgentAction::FollowUp => self.follow_up(),
            AgentAction::Expand => self.fold(),
            AgentAction::Suggest => self.suggest_from_answer(),
            AgentAction::Draft => self.draft_from_answer(),
            AgentAction::Copy => self.copy_answer(),
            AgentAction::Open | AgentAction::Retry | AgentAction::Delete => {
                return self.exchange_action(action);
            }
            AgentAction::Allow
            | AgentAction::Always
            | AgentAction::Decline
            | AgentAction::Never
            | AgentAction::Stop
            | AgentAction::Toggle(_) => self.feed_action(action),
        }
        None
    }

    /// Open, retry, or delete the cursor's exchange.
    fn exchange_action(&mut self, action: AgentAction) -> Option<Action> {
        let Some(exchange_id) = self.agent_at_cursor() else {
            self.set_status("move to an AI answer first", StatusSeverity::Warning);
            return None;
        };
        let session = self
            .notes
            .exchange(&exchange_id)
            .and_then(|exchange| exchange.session_id.clone());
        match action {
            AgentAction::Open => {
                let Some(session) = session else {
                    self.set_status(
                        "this question has no linked session yet",
                        StatusSeverity::Warning,
                    );
                    return None;
                };
                return self.open_session(&session);
            }
            AgentAction::Retry => {
                if let Some(session) = session {
                    self.agents.retry(&session);
                }
            }
            AgentAction::Delete => {
                if let Err(error) = self.notes.delete_exchange(&exchange_id) {
                    self.set_status(error, StatusSeverity::Warning);
                }
            }
            _ => {}
        }
        self.sync_notes();
        None
    }

    /// Ask Bcode about `anchor`: record a pending exchange and queue the question. When the
    /// anchor already has a linked session (an earlier question, or a linked thread), the
    /// question is a follow-up in that session.
    pub(super) fn ask_question(&mut self, anchor: &DocAnchor, question: String, now: u64) {
        self.next_exchange += 1;
        let draft_anchor = anchor.to_draft_anchor();
        let identity = notes::anchor_identity(&draft_anchor);
        let session = self
            .notes
            .exchanges()
            .iter()
            .rev()
            .filter(|exchange| notes::anchor_identity(&exchange.anchor) == identity)
            .find_map(|exchange| exchange.session_id.clone())
            .or_else(|| {
                self.notes
                    .drafts()
                    .iter()
                    .filter(|draft| notes::anchor_identity(&draft.anchor) == identity)
                    .find_map(|draft| draft.session_id.clone())
            });
        let exchange_id = format!("exchange-{now}-{}", self.next_exchange);
        let exchange = ReviewAiExchange {
            schema_version: REVIEW_AI_EXCHANGE_SCHEMA_VERSION,
            exchange_id,
            anchor: draft_anchor,
            question: question.clone(),
            session_id: session,
            status: ReviewAiExchangeStatus::Pending,
            error: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let path = exchange.anchor.file_path.clone();
        let mut seen = std::collections::BTreeSet::new();
        let threads: Vec<String> = self
            .notes
            .drafts()
            .iter()
            .filter(|draft| draft.anchor.file_path == path && seen.insert(&draft.thread_id))
            .filter_map(|draft| self.notes.thread(&draft.thread_id).first().copied())
            .take(agent::MAX_PROMPT_THREADS)
            .map(|first| format!("{}: {}", location_label(&first.anchor), first.body.trim()))
            .collect();
        let context = AskContext {
            title: self.title.clone(),
            repo: self.repo_path.clone(),
            sources: self
                .sources
                .iter()
                .filter(|source| source.included)
                .map(|source| source.label.clone())
                .collect(),
            threads,
            path: exchange.anchor.file_path.clone(),
            old: anchor_range(anchor, ReviewLineSide::Old),
            new: anchor_range(anchor, ReviewLineSide::New),
            lines: self.anchor_lines(anchor),
            question,
        };
        self.notes.put_exchange(exchange.clone());
        let exchange_id = exchange.exchange_id.clone();
        self.asks.push_back((exchange, agent::prompt(&context)));
        self.sync_notes();
        self.focus_card(&exchange_id);
    }

    /// Diff lines of a line anchor, `+`/`-`/` ` prefixed (bounded).
    fn anchor_lines(&self, anchor: &DocAnchor) -> Vec<String> {
        let (Some(review), Some(file), AnchorTarget::Lines(lines)) =
            (self.review.as_ref(), anchor.file.as_ref(), &anchor.target)
        else {
            return Vec::new();
        };
        let within = |range: Option<(u32, u32)>, line: Option<u32>| matches!((range, line), (Some((start, end)), Some(line)) if (start..=end).contains(&line));
        let Some(start) = review.doc.index_of(&ItemKey::FileHeader(file.clone())) else {
            return Vec::new();
        };
        let marker = |kind: LineKind| match kind {
            LineKind::Added => '+',
            LineKind::Removed => '-',
            _ => ' ',
        };
        let mut out = Vec::new();
        // Side-by-side rows pair removed with added lines; the added ones are held back until
        // the change ends so the lines read in unified order whichever layout is shown.
        let mut added = Vec::new();
        for index in start..start.saturating_add(20_000) {
            let Some(item) = review.doc.item(index) else {
                break;
            };
            if item.file() != Some(file) {
                break;
            }
            match item {
                ReviewItem::Line(line)
                    if within(lines.old, line.old_line) || within(lines.new, line.new_line) =>
                {
                    out.append(&mut added);
                    out.push(format!(
                        "{}{}",
                        marker(line.kind),
                        line.content.text().unwrap_or_default()
                    ));
                }
                ReviewItem::LinePair(pair) => {
                    let old = pair
                        .old
                        .as_ref()
                        .filter(|side| within(lines.old, Some(side.line)));
                    let new = pair
                        .new
                        .as_ref()
                        .filter(|side| within(lines.new, Some(side.line)));
                    let changed = |side: Option<&PairSide>| {
                        side.is_some_and(|side| side.kind != LineKind::Context)
                    };
                    if !changed(pair.old.as_ref()) && !changed(pair.new.as_ref()) {
                        if let Some(side) = old.or(new) {
                            out.append(&mut added);
                            out.push(format!(" {}", side.content.text().unwrap_or_default()));
                        }
                    } else {
                        if let Some(side) = old {
                            out.push(format!(
                                "{}{}",
                                marker(side.kind),
                                side.content.text().unwrap_or_default()
                            ));
                        }
                        if let Some(side) = new {
                            added.push(format!(
                                "{}{}",
                                marker(side.kind),
                                side.content.text().unwrap_or_default()
                            ));
                        }
                    }
                }
                _ => {}
            }
            if out.len() >= agent::MAX_PROMPT_LINES {
                break;
            }
        }
        out.append(&mut added);
        out.truncate(agent::MAX_PROMPT_LINES);
        out
    }

    /// Open a linked session, remembering the exact position to restore on return.
    pub(super) fn open_session(&mut self, session: &str) -> Option<Action> {
        let Ok(session_id) = session.parse::<bcode_session_models::SessionId>() else {
            self.set_status("could not open linked session", StatusSeverity::Error);
            return None;
        };
        if let Some(review) = &self.review {
            self.return_to = Some((review.cursor.clone(), review.list.borrow().position()));
        }
        Some(Action::OpenSession { session_id })
    }

    /// Restore the position remembered when a linked session opened.
    pub(super) fn restore_return(&mut self) {
        let Some((cursor, position)) = self.return_to.take() else {
            return;
        };
        let Some(review) = self.review.as_mut() else {
            return;
        };
        review.list.borrow_mut().set_position(position);
        if let Some(cursor) = cursor
            && let Some(index) = review.doc.index_of(&cursor)
        {
            review.cursor_index = index;
            review.cursor = Some(cursor);
        }
        self.set_status("returned from linked Bcode session", StatusSeverity::Info);
    }

    /// The document anchor of a persisted anchor (at its file in this review).
    pub(super) fn doc_anchor(&self, anchor: &bcode_code_review_models::DraftAnchor) -> DocAnchor {
        let file = self.review.as_ref().and_then(|review| {
            if anchor.kind == ReviewAnchorKind::Review {
                return None;
            }
            review
                .doc
                .file_keys()
                .filter(|file| file.path == anchor.file_path)
                .find(|file| {
                    anchor
                        .source_id
                        .as_deref()
                        .is_none_or(|id| id == file.source_id)
                })
                .cloned()
        });
        DocAnchor {
            file,
            target: bcode_code_review::document::target_of(anchor),
            head_oid: anchor.head_oid.clone(),
            blob_oid: anchor.blob_oid.clone(),
        }
    }

    // ----------------------------------------------------------------------------------------
    // Threads panel
    // ----------------------------------------------------------------------------------------

    /// Rebuild the threads panel rows when annotations or the filter changed.
    pub(super) fn sync_thread_rows(&self) {
        let Some(review) = &self.review else {
            return;
        };
        let stamp = self
            .notes
            .revision()
            .wrapping_mul(16)
            .wrapping_add(self.threads_filter as u64)
            ^ self.folds.revision.rotate_left(29);
        let mut rows = review.threads.borrow_mut();
        if rows.revision == Some(stamp) {
            return;
        }
        *rows = ThreadRows {
            revision: Some(stamp),
            ..ThreadRows::default()
        };
        let theme = &self.theme;
        let mut entries: Vec<(String, u32, ItemKey, Line)> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for draft in self.notes.drafts() {
            // A held reply is listed with its pull request thread.
            if draft.anchor.reply_to.is_some() || !seen.insert(draft.thread_id.clone()) {
                continue;
            }
            let comments = self.notes.thread(&draft.thread_id);
            let Some(first) = comments.first() else {
                continue;
            };
            if !self.threads_filter.matches_thread(first) || !self.listed(&first.thread_id) {
                continue;
            }
            let resolved = first.resolved_at_ms.is_some();
            let line = first.anchor.new_line.or(first.anchor.old_line).unwrap_or(0);
            let location = location_label(&first.anchor);
            let body = first.body.lines().next().unwrap_or_default().to_owned();
            entries.push((
                first.anchor.file_path.clone(),
                line,
                ItemKey::Thread {
                    thread_id: first.thread_id.clone(),
                },
                Line::from_spans(vec![
                    self.viewed_mark(&first.thread_id),
                    Span::styled(theme.icons.thread, theme.thread),
                    Span::raw(" "),
                    if resolved {
                        Span::styled("resolved", theme.badge_resolved)
                    } else {
                        Span::styled("open", theme.badge_open)
                    },
                    Span::raw(" "),
                    Span::styled(notes::severity_label(first.severity), theme.muted),
                    Span::raw(" "),
                    Span::styled(location, theme.path),
                    Span::raw(" "),
                    Span::styled(body, theme.text),
                ]),
            ));
        }
        entries.extend(self.external_thread_entries());
        entries.extend(self.remote_thread_entries());
        for exchange in self.notes.exchanges() {
            if !self.threads_filter.matches_exchange(exchange) {
                continue;
            }
            let line = exchange
                .anchor
                .new_line
                .or(exchange.anchor.old_line)
                .unwrap_or(0);
            entries.push((
                exchange.anchor.file_path.clone(),
                line,
                ItemKey::AgentThread {
                    id: exchange.exchange_id.clone(),
                },
                Line::from_spans(vec![
                    Span::styled(theme.icons.ai, theme.badge_ai),
                    Span::raw(" "),
                    Span::styled(location_label(&exchange.anchor), theme.path),
                    Span::raw(" "),
                    Span::styled(
                        exchange
                            .question
                            .lines()
                            .next()
                            .unwrap_or_default()
                            .to_owned(),
                        theme.text,
                    ),
                ]),
            ));
        }
        entries.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        for (index, (_, _, key, line)) in entries.into_iter().take(1_024).enumerate() {
            rows.items
                .push(TreeViewItem::rich(format!("t{index}"), line, 0));
            rows.keys.push(key);
        }
    }

    /// Whether the threads filter lists `thread_id` by its viewed state.
    fn listed(&self, thread_id: &str) -> bool {
        self.threads_filter != super::find::ThreadsFilter::Unviewed
            || !self.thread_viewed(thread_id).is_viewed()
    }

    /// A threads panel row's viewed mark: `✓ `, `○ `, or `• N ` when comments came since.
    fn viewed_mark(&self, thread_id: &str) -> Span {
        let theme = &self.theme;
        let viewed = self.thread_viewed(thread_id);
        match viewed.new_count() {
            0 if viewed.is_viewed() => {
                Span::styled(format!("{} ", theme.icons.viewed), theme.viewed)
            }
            0 => Span::styled(format!("{} ", theme.icons.unviewed), theme.muted),
            new => Span::styled(format!("{} {new} ", theme.icons.new), theme.badge_draft),
        }
    }

    /// Threads panel entries of pull request threads: `(path, line, key, row)`.
    fn remote_thread_entries(&self) -> Vec<(String, u32, ItemKey, Line)> {
        let theme = &self.theme;
        let mut entries = Vec::new();
        for (thread_id, remote) in self.notes.remote_threads() {
            if !self.threads_filter.matches_remote(remote) || !self.listed(thread_id) {
                continue;
            }
            let comments = self.notes.thread(thread_id);
            let Some(first) = comments.first() else {
                continue;
            };
            let author = remote
                .authors
                .get(&first.comment_id)
                .map(|(author, _)| author.clone())
                .unwrap_or_default();
            let (state, style) = if remote.resolved {
                ("resolved", theme.badge_resolved)
            } else if remote.waiting {
                ("waiting on you", theme.pill_warning)
            } else if remote.new {
                ("new", theme.badge_draft)
            } else {
                ("open", theme.badge_open)
            };
            let line = first.anchor.new_line.or(first.anchor.old_line).unwrap_or(0);
            entries.push((
                first.anchor.file_path.clone(),
                line,
                ItemKey::Thread {
                    thread_id: thread_id.clone(),
                },
                Line::from_spans(vec![
                    self.viewed_mark(thread_id),
                    Span::styled(theme.icons.thread, theme.thread),
                    Span::raw(" "),
                    Span::styled(state, style),
                    Span::raw(" "),
                    Span::styled(author, theme.title),
                    Span::raw(" "),
                    Span::styled(location_label(&first.anchor), theme.path),
                    Span::raw(" "),
                    Span::styled(
                        comments
                            .last()
                            .map(|last| last.body.lines().next().unwrap_or_default().to_owned())
                            .unwrap_or_default(),
                        theme.text,
                    ),
                ]),
            ));
        }
        entries
    }

    /// Threads panel entries of imported threads: `(path, line, key, row)`.
    fn external_thread_entries(&self) -> Vec<(String, u32, ItemKey, Line)> {
        let theme = &self.theme;
        let mut entries = Vec::new();
        let mut seen_external = std::collections::BTreeSet::new();
        for draft in self.notes.external_drafts() {
            if !seen_external.insert(draft.thread_id.clone())
                || !self.threads_filter.matches_thread(draft)
                || !self.listed(&draft.thread_id)
            {
                continue;
            }
            let Some(external) = self.notes.external(&draft.thread_id) else {
                continue;
            };
            let line = draft.anchor.new_line.or(draft.anchor.old_line).unwrap_or(0);
            entries.push((
                draft.anchor.file_path.clone(),
                line,
                ItemKey::Thread {
                    thread_id: draft.thread_id.clone(),
                },
                Line::from_spans(vec![
                    self.viewed_mark(&draft.thread_id),
                    Span::styled(theme.icons.thread, theme.thread),
                    Span::raw(" "),
                    Span::styled("imported", theme.badge_ai),
                    Span::raw(" "),
                    Span::styled(external.provider_id.clone(), theme.muted),
                    Span::raw(" "),
                    Span::styled(location_label(&draft.anchor), theme.path),
                    Span::raw(" "),
                    Span::styled(
                        draft.body.lines().next().unwrap_or_default().to_owned(),
                        theme.text,
                    ),
                ]),
            ));
        }
        entries
    }
}

/// Move the cursor to an annotation; one in a file whose content is not resident moves to the
/// file's header and finishes once the file loads.
pub(super) fn jump_to_annotation(review: &mut Review, key: &ItemKey) {
    // An annotation in a folded file opens it.
    if let Some(file) = review.doc.jump_to(key).and_then(|target| target.opens) {
        review.doc.set_collapsed(&file, false);
        review.note_splices();
    }
    match review.doc.jump_to(key) {
        Some(target) => {
            review.set_cursor(target.index, AnchorPolicy::Center);
            review.pending_jump = target.needs_load.map(|_| key.clone());
        }
        None => review.pending_jump = None,
    }
}

/// The line key an anchor's composer returns to.
fn anchor_key(
    doc: &bcode_code_review::document::ReviewDocument,
    anchor: &DocAnchor,
) -> Option<ItemKey> {
    let file = anchor.file.clone()?;
    match &anchor.target {
        AnchorTarget::Lines(lines) => doc.map_key(&ItemKey::Line {
            file,
            side: lines.end_side,
            line: lines.end_line(),
        }),
        _ => Some(ItemKey::FileHeader(file)),
    }
}

const fn anchor_range(anchor: &DocAnchor, side: ReviewLineSide) -> Option<(u32, u32)> {
    match &anchor.target {
        AnchorTarget::Lines(lines) => match side {
            ReviewLineSide::Old => lines.old,
            ReviewLineSide::New => lines.new,
        },
        _ => None,
    }
}

pub(super) fn location_label(anchor: &bcode_code_review_models::DraftAnchor) -> String {
    if anchor.kind == ReviewAnchorKind::Review {
        return "review".to_owned();
    }
    anchor.new_line.or(anchor.old_line).map_or_else(
        || anchor.file_path.clone(),
        |line| format!("{}:{line}", anchor.file_path),
    )
}
