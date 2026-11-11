//! Comments, threads, and AI questions on the surface.
//!
//! * Annotations load when the review opens (and re-open); every change goes through
//!   [`ReviewNotes`](crate::v2::tui::notes::ReviewNotes) and is applied to the document at once
//!   (`set_annotations`), so threads appear, change, and disappear before their calls finish.
//! * The composer is the document's composer item. Opening it moves the cursor and keyboard
//!   focus there; closing it returns both to the review.
//! * Asking Bcode records a pending exchange at once; the question is sent from the next poll,
//!   and when the session is linked the surface opens it (`PluginTuiAction::OpenSession`),
//!   remembering the exact cursor and scroll position to restore on return.

use super::{Focus, Review, ReviewSurfaceV2, TRANSIENT_WORKSPACE_ID, ThreadRows};
use crate::v2::document::{AnchorTarget, DocAnchor, ItemKey, ReviewItem};
use crate::v2::tui::agent::{self, AskContext};
use crate::v2::tui::composer::{Composer, ComposerButton, ComposerInput, ComposerMode};
use crate::v2::tui::loader::Message;
use crate::v2::tui::notes::{self, MutationResult, NoteScope};
use crate::v2::tui::rows::{AgentAction, ThreadAction};
use bcode_code_review_models::{
    REVIEW_AI_EXCHANGE_SCHEMA_VERSION, ReviewAiExchange, ReviewAiExchangeStatus, ReviewAnchorKind,
    ReviewLineSide, ReviewWorkspace,
};
use bcode_plugin_sdk::tui::{PluginTuiAction, PluginTuiHost};
use bmux_keyboard::KeyStroke;
use bmux_tui::text::{Line, Span};
use bmux_tui_components::status_bar::StatusSeverity;
use bmux_tui_components::toast_stack::ToastSeverity;
use bmux_tui_components::tree_view::TreeViewItem;
use bmux_tui_components::virtual_list::{AnchorPolicy, Edge};
use std::sync::Arc;

/// Keymap scope of the composer.
pub const COMPOSER_SCOPE: &str = "code-review-v2-composer";

/// Milliseconds since the Unix epoch.
pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

impl ReviewSurfaceV2 {
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
            presentation_state: None,
            viewed_marks: Vec::new(),
            reviewed_revisions: std::collections::BTreeMap::new(),
            presentation_v2: None,
        })
    }

    pub(super) fn note_scope(&mut self) -> NoteScope {
        NoteScope::for_workspace(self.workspace_mut())
    }

    /// List the review's annotations for the current document.
    pub(super) fn load_notes(&mut self, host: &dyn PluginTuiHost) {
        let scope = self.note_scope();
        let backend = Arc::clone(&self.backend);
        let inbox = self.inbox.clone();
        let generation = self.generation;
        host.spawn(Box::pin(async move {
            let result = notes::load(backend, scope).await;
            inbox.push(Message::NotesLoaded { generation, result });
        }));
    }

    /// Start the next queued annotation call and the next queued question.
    pub(super) fn pump_notes(&mut self, host: &dyn PluginTuiHost) {
        if let Some(mutation) = self.notes.start_next() {
            let scope = self.note_scope();
            let backend = Arc::clone(&self.backend);
            let inbox = self.inbox.clone();
            host.spawn(Box::pin(async move {
                let result = notes::run(backend, scope, mutation).await;
                inbox.push(Message::NoteDone(result));
            }));
        }
        while let Some((exchange, text)) = self.asks.pop_front() {
            let scope = self.note_scope();
            let backend = Arc::clone(&self.backend);
            let inbox = self.inbox.clone();
            host.spawn(Box::pin(async move {
                let result = agent::ask(backend, scope, exchange, text, now_ms()).await;
                inbox.push(Message::Asked(Box::new(result)));
            }));
        }
        let sessions: Vec<String> = self
            .notes
            .exchanges()
            .iter()
            .filter_map(|exchange| exchange.session_id.clone())
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

    /// A finished question: keep the exchange and open its session.
    pub(super) fn finish_ask(
        &mut self,
        result: Result<ReviewAiExchange, Box<(ReviewAiExchange, String)>>,
    ) {
        match result {
            Ok(exchange) => {
                let session = exchange.session_id.clone();
                self.notes.put_exchange(exchange);
                if let Some(session) = session {
                    self.pending_action = self.open_session(&session);
                }
            }
            Err(failed) => {
                let (exchange, error) = *failed;
                self.notes.put_exchange(exchange);
                self.toasts
                    .push(ToastSeverity::Error, "Could not ask Bcode", Some(error));
            }
        }
        self.sync_notes();
    }

    /// Show the current annotations in the document when they changed.
    pub(super) fn sync_notes(&mut self) {
        let revision = self.notes.revision();
        let Some(review) = self.review.as_mut() else {
            return;
        };
        if review.notes_applied == Some(revision) {
            return;
        }
        review.notes_applied = Some(revision);
        review.doc.set_annotations(&self.notes.annotations());
        review.note_splices();
        review.threads.borrow_mut().revision = None;
        review.marks_revision = None;
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

    /// Submit the composer.
    pub(super) fn submit_composer(&mut self) {
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
        self.close_composer();
        let now = now_ms();
        let result = match mode {
            ComposerMode::Comment => {
                self.notes
                    .add_draft(anchor.to_draft_anchor(), text, None, Some(classify), now);
                Ok("draft saved")
            }
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
                self.ask(&anchor, text, now);
                Ok("asking Bcode…")
            }
        };
        match result {
            Ok(message) => self.set_status(message, StatusSeverity::Success),
            Err(error) => self.set_status(error, StatusSeverity::Error),
        }
        self.sync_notes();
    }

    /// Keys while the composer has focus.
    pub(super) fn composer_key(
        &mut self,
        stroke: KeyStroke,
        host: &dyn PluginTuiHost,
    ) -> PluginTuiAction {
        let action = host.keymap_action(COMPOSER_SCOPE, stroke);
        let action = action
            .as_deref()
            .map(|action| action.strip_prefix("bcode.code_review.").unwrap_or(action));
        let Some(composer) = self.composer.as_mut() else {
            self.focus = Focus::Review;
            return PluginTuiAction::Redraw;
        };
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
                ComposerInput::Edited => {}
                ComposerInput::Ignored => return PluginTuiAction::None,
            },
        }
        PluginTuiAction::Redraw
    }

    /// A clicked composer button.
    pub(super) fn composer_button(&mut self, button: ComposerButton) {
        let Some(composer) = self.composer.as_mut() else {
            return;
        };
        self.focus = Focus::Composer;
        match button {
            ComposerButton::Submit => self.submit_composer(),
            ComposerButton::Switch => composer.switch(),
            ComposerButton::Preview => {
                composer.preview = !composer.preview;
                composer.touch();
            }
            ComposerButton::Cancel => self.close_composer(),
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

    /// Start a comment (or a question) at the cursor; on a thread, reply to it.
    pub(super) fn start_comment(&mut self, ask: bool) {
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
        self.open_composer(anchor, ComposerMode::Comment, "");
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

    fn agent_at_cursor(&self) -> Option<String> {
        let review = self.review.as_ref()?;
        match review.doc.item(review.cursor_index)? {
            ReviewItem::AgentThread(agent) => agent.exchange_id.clone(),
            _ => None,
        }
    }

    /// Run a thread action on the cursor's thread.
    pub(super) fn thread_action(&mut self, action: ThreadAction) -> Option<PluginTuiAction> {
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
            ThreadAction::Resolve => {
                let resolved = first.resolved_at_ms.is_none();
                self.notes
                    .resolve(&thread_id, resolved, now_ms())
                    .map(|()| {
                        Some(if resolved {
                            "thread resolved"
                        } else {
                            "thread reopened"
                        })
                    })
            }
            ThreadAction::Ask => {
                self.open_composer(anchor, ComposerMode::Ask, "");
                Ok(None)
            }
        };
        match result {
            Ok(Some(message)) => self.set_status(message, StatusSeverity::Success),
            Ok(None) => {}
            Err(error) => self.set_status(error, StatusSeverity::Warning),
        }
        self.sync_notes();
        None
    }

    /// Cycle the cursor thread's kind or severity.
    pub(super) fn classify_thread(&mut self, severity: bool) {
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
    pub(super) fn agent_action(&mut self, action: AgentAction) -> Option<PluginTuiAction> {
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
        }
        self.sync_notes();
        None
    }

    /// Ask Bcode about `anchor`: record a pending exchange and queue the question.
    fn ask(&mut self, anchor: &DocAnchor, question: String, now: u64) {
        self.next_exchange += 1;
        let exchange = ReviewAiExchange {
            schema_version: REVIEW_AI_EXCHANGE_SCHEMA_VERSION,
            exchange_id: format!("exchange-{now}-{}", self.next_exchange),
            anchor: anchor.to_draft_anchor(),
            question: question.clone(),
            session_id: None,
            status: ReviewAiExchangeStatus::Pending,
            error: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let context = AskContext {
            title: self.title.clone(),
            repo: self.repo_path.clone(),
            path: exchange.anchor.file_path.clone(),
            old: anchor_range(anchor, ReviewLineSide::Old),
            new: anchor_range(anchor, ReviewLineSide::New),
            lines: self.anchor_lines(anchor),
            question,
        };
        self.notes.put_exchange(exchange.clone());
        self.asks.push_back((exchange, agent::prompt(&context)));
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
        let mut out = Vec::new();
        for index in start..start.saturating_add(20_000) {
            let Some(item) = review.doc.item(index) else {
                break;
            };
            if item.file() != Some(file) {
                break;
            }
            if let ReviewItem::Line(line) = item
                && (within(lines.old, line.old_line) || within(lines.new, line.new_line))
            {
                let marker = match line.kind {
                    crate::v2::document::LineKind::Added => '+',
                    crate::v2::document::LineKind::Removed => '-',
                    _ => ' ',
                };
                out.push(format!(
                    "{marker}{}",
                    line.content.text().unwrap_or_default()
                ));
                if out.len() >= agent::MAX_PROMPT_LINES {
                    break;
                }
            }
        }
        out
    }

    /// Open a linked session, remembering the exact position to restore on return.
    pub(super) fn open_session(&mut self, session: &str) -> Option<PluginTuiAction> {
        let Ok(session_id) = session.parse::<bcode_session_models::SessionId>() else {
            self.set_status("could not open linked session", StatusSeverity::Error);
            return None;
        };
        if let Some(review) = &self.review {
            self.return_to = Some((review.cursor.clone(), review.list.borrow().position()));
        }
        Some(PluginTuiAction::OpenSession { session_id })
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
    fn doc_anchor(&self, anchor: &bcode_code_review_models::DraftAnchor) -> DocAnchor {
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
            target: crate::v2::document::target_of(anchor),
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
            .wrapping_mul(8)
            .wrapping_add(self.threads_filter as u64);
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
            if !seen.insert(draft.thread_id.clone()) {
                continue;
            }
            let comments = self.notes.thread(&draft.thread_id);
            let Some(first) = comments.first() else {
                continue;
            };
            if !self.threads_filter.matches_thread(first) {
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
}

/// Move the cursor to an annotation; one in a file whose content is not resident moves to the
/// file's header and finishes once the file loads.
pub(super) fn jump_to_annotation(review: &mut Review, key: &ItemKey) {
    match review.doc.jump_to(key) {
        Some(target) => {
            review.set_cursor(target.index, AnchorPolicy::Center);
            review.pending_jump = target.needs_load.map(|_| key.clone());
        }
        None => review.pending_jump = None,
    }
}

/// The line key an anchor's composer returns to.
fn anchor_key(doc: &crate::v2::document::ReviewDocument, anchor: &DocAnchor) -> Option<ItemKey> {
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

fn location_label(anchor: &bcode_code_review_models::DraftAnchor) -> String {
    if anchor.kind == ReviewAnchorKind::Review {
        return "review".to_owned();
    }
    anchor.new_line.or(anchor.old_line).map_or_else(
        || anchor.file_path.clone(),
        |line| format!("{}:{line}", anchor.file_path),
    )
}
