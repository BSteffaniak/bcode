//! AI answers, suggestions, folding, bulk viewed marks, the review report, and item menus
//!.
//!
//! * **Folding.** A thread folds to its header (`fold`, or a click on the header); on an AI
//!   answer `fold` shows the whole answer or the preview again. `fold_all` collapses every
//!   thread (or expands them all when every one is collapsed). Resolved threads can be hidden
//!   from the document (`resolved_toggle`). Folds and the resolved filter are presentation
//!   state saved with the workspace.
//! * **AI answers.** A question on an anchor that already has a linked session is sent to that
//!   session as a follow-up. Structured commands ask a fixed instruction about the cursor's
//!   anchor. The latest answer can be copied, turned into a suggested comment, or turned into a
//!   draft. Suggestions are accepted (a draft at their anchor), refined (asked again in the
//!   linked session), or rejected; each change is persisted with `review.suggestion.save`.
//! * **Menus.** `enter` on a thread or an AI answer lists the actions that apply to it;
//!   `ai_menu` lists the structured AI commands. Both are generated from the manifest keymap,
//!   so they show each action's effective keys.
//! * **Report.** `copy_report` copies a Markdown review report (viewed progress, unviewed
//!   files, threads with kind, severity, and location, and AI questions) to the clipboard.

use super::{ReviewSurface, annotate::now_ms};
use crate::action::Action;
use crate::agent::{self, AiCommand};
use crate::composer::ComposerMode;
use crate::context::UiContext;
use crate::notes;
use crate::palette::{Overlay, OverlayEntry, OverlayKind};
use bcode_code_review::document::{ItemKey, NavKind, ReviewItem};
use bcode_code_review_models::{ReviewSuggestion, ReviewSuggestionStatus};
use bmux_tui_components::status_bar::StatusSeverity;
use std::fmt::Write as _;

/// Files listed by name in the report's unviewed section.
const REPORT_UNVIEWED_FILES: usize = 50;
/// Threads in the report.
const REPORT_THREADS: usize = 500;

/// Thread actions offered by a thread's menu, in order.
const THREAD_MENU: &[&str] = &[
    "comment",
    "edit",
    "resolve",
    "thread_kind",
    "thread_severity",
    "ask",
    "fold",
    "accept_suggestion",
    "refine_suggestion",
    "reject_suggestion",
    "delete",
];

/// AI answer actions offered by an answer's menu, in order.
const AGENT_MENU: &[&str] = &[
    "open_session",
    "ask",
    "fold",
    "copy_answer",
    "suggest_comment",
    "draft_answer",
    "accept_suggestion",
    "refine_suggestion",
    "reject_suggestion",
    "retry",
    "delete",
];

/// What the cursor item is, for item actions.
#[derive(Debug, Clone)]
enum Target {
    Thread {
        thread_id: String,
        suggestions: Vec<String>,
    },
    Agent {
        exchange_id: Option<String>,
        suggestions: Vec<String>,
    },
}

impl ReviewSurface {
    fn target(&self) -> Option<Target> {
        let review = self.review.as_ref()?;
        match review.doc.item(review.cursor_index)? {
            ReviewItem::Thread(thread) => Some(Target::Thread {
                thread_id: thread.thread_id.clone(),
                suggestions: thread.suggestion_ids.clone(),
            }),
            ReviewItem::AgentThread(agent) => Some(Target::Agent {
                exchange_id: agent.exchange_id.clone(),
                suggestions: agent.suggestion_ids.clone(),
            }),
            _ => None,
        }
    }

    // ----------------------------------------------------------------------------------------
    // Folding and the resolved filter
    // ----------------------------------------------------------------------------------------

    /// Fold the cursor's thread, or show the whole (or the preview of the) cursor's AI answer.
    pub(super) fn fold(&mut self) {
        // On a file's header, details, or end: the file folds to its header.
        let on_file = self.review.as_ref().is_some_and(|review| {
            matches!(
                review.cursor,
                Some(
                    bcode_code_review::document::ItemKey::FileHeader(_)
                        | bcode_code_review::document::ItemKey::FileDetails(_)
                        | bcode_code_review::document::ItemKey::FileEnd(_)
                )
            )
        });
        if on_file {
            self.toggle_file_collapsed();
            return;
        }
        match self.target() {
            Some(Target::Thread { thread_id, .. }) => {
                let collapsed = !self.folds.collapsed.remove(&thread_id);
                if collapsed {
                    self.folds.collapsed.insert(thread_id);
                }
                self.folds_changed();
                self.set_status(
                    if collapsed {
                        "thread collapsed"
                    } else {
                        "thread expanded"
                    },
                    StatusSeverity::Info,
                );
            }
            Some(Target::Agent {
                exchange_id: Some(exchange_id),
                ..
            }) => {
                if self.answer_text(&exchange_id).is_none() {
                    self.set_status("this question has no answer yet", StatusSeverity::Warning);
                    return;
                }
                let expanded = !self.folds.expanded.remove(&exchange_id);
                if expanded {
                    self.folds.expanded.insert(exchange_id);
                }
                self.folds_changed();
                self.set_status(
                    if expanded {
                        "showing the whole answer"
                    } else {
                        "showing the answer preview"
                    },
                    StatusSeverity::Info,
                );
            }
            _ => self.set_status(
                "move to a thread or an AI answer to fold it",
                StatusSeverity::Warning,
            ),
        }
    }

    /// Collapse every thread, or expand them all when every one is collapsed.
    pub(super) fn fold_all(&mut self) {
        let threads: std::collections::BTreeSet<String> = self
            .notes
            .drafts()
            .iter()
            .chain(self.notes.external_drafts())
            .map(|draft| draft.thread_id.clone())
            .collect();
        if threads.is_empty() {
            self.set_status("no threads to fold", StatusSeverity::Info);
            return;
        }
        let all_collapsed = threads
            .iter()
            .all(|thread| self.folds.collapsed.contains(thread));
        if all_collapsed {
            self.folds.collapsed.clear();
            self.set_status("expanded every thread", StatusSeverity::Info);
        } else {
            self.folds.collapsed = threads;
            self.set_status("collapsed every thread", StatusSeverity::Info);
        }
        self.folds_changed();
    }

    /// Show or hide resolved threads in the document.
    pub(super) fn toggle_resolved(&mut self) {
        self.hide_resolved = !self.hide_resolved;
        self.sync_notes();
        self.set_status(
            if self.hide_resolved {
                "resolved threads hidden"
            } else {
                "resolved threads shown"
            },
            StatusSeverity::Info,
        );
    }

    /// Note a fold change: rows re-measure and the presentation is saved.
    pub(super) fn folds_changed(&mut self) {
        self.folds.revision = self.folds.revision.wrapping_add(1);
        if let Some(review) = self.review.as_mut() {
            review.marks_revision = None;
        }
    }

    /// Show or hide viewed files in the file panel.
    pub(super) fn toggle_viewed_files(&mut self) {
        self.hide_viewed_files = !self.hide_viewed_files;
        let theme = self.theme;
        let hide = self.hide_viewed_files;
        if let Some(review) = self.review.as_mut()
            && review.files.set_hide_viewed(hide, &theme)
        {
            self.tree_reveal = true;
        }
        self.set_status(
            if hide {
                "viewed files hidden from the file panel"
            } else {
                "viewed files shown in the file panel"
            },
            StatusSeverity::Info,
        );
    }

    // ----------------------------------------------------------------------------------------
    // AI answers and suggestions
    // ----------------------------------------------------------------------------------------

    /// The finished answer of an exchange's turn in its linked session, trimmed; `None` without
    /// one. When the turn is no longer in the observed window, the newest exchange of the
    /// session takes the session's latest answer.
    fn answer_text(&self, exchange_id: &str) -> Option<String> {
        let exchange = self.notes.exchange(exchange_id)?;
        let state = self.agents.state(exchange.session_id.as_deref()?)?;
        let newest = crate::rows::is_newest(exchange, &self.notes);
        let answer = match crate::rows::exchange_turn(exchange, state, newest) {
            Some((turn, _)) => turn.answer(),
            None if newest => state.answer.trim(),
            None => "",
        };
        (!answer.is_empty()).then(|| answer.to_owned())
    }

    /// The cursor's exchange and its answer, or a status message saying what is missing.
    fn answer_at_cursor(&mut self) -> Option<(bcode_code_review_models::ReviewAiExchange, String)> {
        let Some(Target::Agent {
            exchange_id: Some(exchange_id),
            ..
        }) = self.target()
        else {
            self.set_status("move to an AI answer first", StatusSeverity::Warning);
            return None;
        };
        let exchange = self.notes.exchange(&exchange_id)?.clone();
        let Some(answer) = self.answer_text(&exchange_id) else {
            self.set_status("this question has no answer yet", StatusSeverity::Warning);
            return None;
        };
        Some((exchange, answer))
    }

    /// Copy the cursor's AI answer to the clipboard (written once the host is at hand, after
    /// the event or on the next poll).
    pub(super) fn copy_answer(&mut self) {
        let Some((_, answer)) = self.answer_at_cursor() else {
            return;
        };
        self.clipboard = Some((answer, "Bcode answer"));
    }

    /// Write queued clipboard text through the host.
    pub(super) fn flush_clipboard(&mut self, host: &UiContext) -> bool {
        let Some((text, what)) = self.clipboard.take() else {
            return false;
        };
        match host.copy_text(&text) {
            Ok(()) => self.set_status(
                format!("copied the {what} to the clipboard"),
                StatusSeverity::Success,
            ),
            Err(error) => self.set_status(
                format!("failed to copy the {what}: {error}"),
                StatusSeverity::Error,
            ),
        }
        true
    }

    /// Turn the cursor's AI answer into a suggested comment.
    pub(super) fn suggest_from_answer(&mut self) {
        let Some((exchange, answer)) = self.answer_at_cursor() else {
            return;
        };
        let duplicate = self.notes.suggestions().iter().any(|suggestion| {
            suggestion.status == ReviewSuggestionStatus::Suggested
                && suggestion.body.trim() == answer
                && notes::anchor_identity(&suggestion.anchor)
                    == notes::anchor_identity(&exchange.anchor)
        });
        if duplicate {
            self.set_status("this answer is already suggested", StatusSeverity::Info);
            return;
        }
        let now = now_ms();
        let count = self
            .notes
            .suggestions()
            .iter()
            .filter(|suggestion| suggestion.suggestion_id.contains(&exchange.exchange_id))
            .count();
        self.notes.save_suggestion(ReviewSuggestion {
            suggestion_id: format!("suggestion-{}-{}", exchange.exchange_id, count + 1),
            anchor: exchange.anchor,
            body: answer,
            session_id: exchange.session_id,
            status: ReviewSuggestionStatus::Suggested,
            rationale: Some("from linked Bcode answer".to_owned()),
            created_at_ms: now,
            updated_at_ms: now,
        });
        self.sync_notes();
        self.set_status("added a suggested review comment", StatusSeverity::Success);
    }

    /// Turn the cursor's AI answer into a draft comment at its anchor.
    pub(super) fn draft_from_answer(&mut self) {
        let Some((exchange, answer)) = self.answer_at_cursor() else {
            return;
        };
        let identity = notes::anchor_identity(&exchange.anchor);
        let duplicate = self.notes.drafts().iter().any(|draft| {
            draft.body.trim() == answer && notes::anchor_identity(&draft.anchor) == identity
        });
        if duplicate {
            self.set_status(
                "this answer is already a draft comment",
                StatusSeverity::Info,
            );
            return;
        }
        self.notes
            .add_draft(exchange.anchor, answer, None, None, now_ms());
        self.sync_notes();
        self.set_status("drafted the Bcode answer", StatusSeverity::Success);
    }

    /// The latest pending suggestion of the cursor item.
    fn pending_suggestion(&mut self) -> Option<ReviewSuggestion> {
        let ids = match self.target() {
            Some(Target::Thread { suggestions, .. } | Target::Agent { suggestions, .. }) => {
                suggestions
            }
            None => Vec::new(),
        };
        let found = ids
            .iter()
            .rev()
            .filter_map(|id| self.notes.suggestion(id))
            .find(|suggestion| suggestion.status == ReviewSuggestionStatus::Suggested)
            .cloned();
        if found.is_none() {
            self.set_status(
                "no pending suggestion here; move to an AI answer with one",
                StatusSeverity::Warning,
            );
        }
        found
    }

    /// Accept, refine, or reject the cursor item's latest pending suggestion.
    pub(super) fn decide_suggestion(&mut self, status: ReviewSuggestionStatus) {
        let Some(mut suggestion) = self.pending_suggestion() else {
            return;
        };
        let now = now_ms();
        suggestion.status = status;
        suggestion.updated_at_ms = now;
        let body = suggestion.body.clone();
        let anchor = suggestion.anchor.clone();
        self.notes.save_suggestion(suggestion);
        let message = match status {
            ReviewSuggestionStatus::Accepted => {
                self.notes.add_draft(anchor, body, None, None, now);
                "accepted the suggestion as a draft comment"
            }
            ReviewSuggestionStatus::Refining => {
                let anchor = self.doc_anchor(&anchor);
                self.ask_question(&anchor, agent::refine_question(&body), now);
                "asked Bcode to refine the suggestion"
            }
            _ => "rejected the suggestion",
        };
        self.sync_notes();
        self.set_status(message, StatusSeverity::Success);
    }

    /// Ask a structured AI command about the cursor's anchor (a thread's or answer's anchor on
    /// those items).
    pub(super) fn ai_command(&mut self, command: AiCommand) {
        let anchor = self.question_anchor();
        let Some(anchor) = anchor else {
            self.set_status(
                format!(
                    "move to a line, range, file, or thread to {}",
                    command.label()
                ),
                StatusSeverity::Warning,
            );
            return;
        };
        self.ask_question(&anchor, command.instruction().to_owned(), now_ms());
        self.sync_notes();
        self.set_status(
            format!("asked Bcode to {}", command.label()),
            StatusSeverity::Success,
        );
    }

    /// Anchor a question at the cursor is about.
    fn question_anchor(&self) -> Option<bcode_code_review::document::DocAnchor> {
        match self.target() {
            Some(Target::Thread { thread_id, .. }) => {
                let first = self
                    .notes
                    .thread(&thread_id)
                    .first()
                    .map(|first| first.anchor.clone())?;
                Some(self.doc_anchor(&first))
            }
            Some(Target::Agent {
                exchange_id: Some(exchange_id),
                ..
            }) => {
                let anchor = self.notes.exchange(&exchange_id)?.anchor.clone();
                Some(self.doc_anchor(&anchor))
            }
            _ => self.cursor_anchor(),
        }
    }

    /// Ask a follow-up question in the cursor answer's linked session.
    pub(super) fn follow_up(&mut self) {
        match self.question_anchor() {
            Some(anchor) => self.open_composer(anchor, ComposerMode::Ask, ""),
            None => self.set_status("move to an AI answer first", StatusSeverity::Warning),
        }
    }

    // ----------------------------------------------------------------------------------------
    // Bulk viewed marks
    // ----------------------------------------------------------------------------------------

    /// Mark every file of the review viewed, or unviewed.
    pub(super) fn view_all(&mut self, viewed: bool) {
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let identities: Vec<_> = review
            .doc
            .file_keys()
            .filter_map(|file| review.doc.viewed_identity(file))
            .collect();
        let count = identities.len();
        let workspace = self.workspace_mut();
        if viewed {
            let files: Vec<_> = identities
                .iter()
                .map(|(identity, patch)| identity.with_patch(patch.as_deref()))
                .collect();
            bcode_code_review::viewed::mark_all_viewed(workspace, &files);
        } else {
            for (identity, patch) in &identities {
                bcode_code_review::viewed::unmark_viewed(
                    workspace,
                    &identity.with_patch(patch.as_deref()),
                );
            }
        }
        self.apply_viewed();
        self.set_status(
            format!(
                "{count} file{} marked {}",
                if count == 1 { "" } else { "s" },
                if viewed { "viewed" } else { "unviewed" }
            ),
            StatusSeverity::Success,
        );
    }

    /// Next unviewed file, else the next open thread.
    pub(super) fn attention_next(&mut self) {
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let Some(origin) = review.cursor.clone() else {
            return;
        };
        if Self::run_motion(
            review,
            origin.clone(),
            NavKind::UnviewedFile,
            bcode_code_review::document::Direction::Forward,
            0,
        ) || Self::run_motion(
            review,
            origin,
            NavKind::OpenThread,
            bcode_code_review::document::Direction::Forward,
            0,
        ) {
            return;
        }
        self.set_status(
            "nothing needs attention: every file is viewed and no thread is open below",
            StatusSeverity::Info,
        );
    }

    /// Move the cursor to line `line` of the cursor's file (the new side, else the old side),
    /// revealing it when it is hidden context.
    pub(super) fn goto_line(&mut self, line: u32) {
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let Some(file) = review.cursor_file() else {
            self.set_status("move into a file first", StatusSeverity::Warning);
            return;
        };
        let key = |side| ItemKey::Line {
            file: file.clone(),
            side,
            line,
        };
        let found = |review: &super::Review| {
            [
                bcode_code_review_models::ReviewLineSide::New,
                bcode_code_review_models::ReviewLineSide::Old,
            ]
            .into_iter()
            .map(key)
            .find(|key| review.doc.index_of(key).is_some())
        };
        let mut target = found(review);
        if target.is_none() && review.doc.reveal_lines(&file, line, line) {
            review.note_splices();
            target = found(review);
        }
        if let Some(target) = target {
            super::annotate::jump_to_annotation(review, &target);
            self.set_status(format!("{}:{line}", file.path), StatusSeverity::Info);
            return;
        }
        let loaded = review
            .doc
            .load_state(&file)
            .is_some_and(|state| matches!(state, bcode_code_review::document::FileLoad::Loaded));
        if loaded {
            self.set_status(
                format!("{} has no line {line}", file.path),
                StatusSeverity::Warning,
            );
        } else {
            super::annotate::jump_to_annotation(
                review,
                &key(bcode_code_review_models::ReviewLineSide::New),
            );
            self.set_status("loading the file…", StatusSeverity::Info);
        }
    }

    // ----------------------------------------------------------------------------------------
    // Menus
    // ----------------------------------------------------------------------------------------

    fn menu_entries(actions: &[&str], host: &UiContext) -> Vec<OverlayEntry> {
        let declared = crate::keymap::actions_for(super::SCOPE);
        actions
            .iter()
            .filter_map(|action| {
                let id = format!("bcode.code_review.{action}");
                let declared = declared.iter().find(|declared| declared.id == id)?;
                Some(OverlayEntry {
                    action: Some((*action).to_owned()),
                    label: declared.description.clone(),
                    keys: host.keymap_keys(super::SCOPE, &id).join(" "),
                })
            })
            .collect()
    }

    /// Open the actions of the cursor's thread or AI answer in a menu; on a gap, reveal it.
    pub(super) fn activate(&mut self, host: &UiContext) -> Action {
        let on_gap = self
            .review
            .as_ref()
            .is_some_and(|review| matches!(review.cursor, Some(ItemKey::Gap { .. })));
        if on_gap {
            return self.document_action("expand_down", None);
        }
        let actions: Vec<&str> = match self.target() {
            Some(Target::Thread { thread_id, .. }) if self.notes.external(&thread_id).is_some() => {
                vec!["fold"]
            }
            Some(Target::Thread { suggestions, .. }) => THREAD_MENU
                .iter()
                .copied()
                .filter(|action| !action.ends_with("_suggestion") || !suggestions.is_empty())
                .collect(),
            Some(Target::Agent {
                exchange_id,
                suggestions,
            }) => {
                let answered = exchange_id
                    .as_deref()
                    .and_then(|id| self.answer_text(id))
                    .is_some();
                AGENT_MENU
                    .iter()
                    .copied()
                    .filter(|action| {
                        (!action.ends_with("_suggestion") || !suggestions.is_empty())
                            && (answered
                                || !matches!(
                                    *action,
                                    "fold" | "copy_answer" | "suggest_comment" | "draft_answer"
                                ))
                    })
                    .collect()
            }
            None => {
                self.set_status(
                    "enter opens the actions of a thread or an AI answer",
                    StatusSeverity::Info,
                );
                return Action::Redraw;
            }
        };
        let entries = Self::menu_entries(&actions, host);
        self.overlay = Some(Overlay::new(OverlayKind::Menu, entries, &self.theme));
        Action::Redraw
    }

    /// Open the structured AI commands in a menu.
    pub(super) fn open_ai_menu(&mut self, host: &UiContext) {
        let mut actions: Vec<&str> = AiCommand::ALL
            .iter()
            .map(|command| command.action())
            .collect();
        actions.push("ask");
        let entries = Self::menu_entries(&actions, host);
        self.overlay = Some(Overlay::new(OverlayKind::Menu, entries, &self.theme));
    }

    // ----------------------------------------------------------------------------------------
    // Report
    // ----------------------------------------------------------------------------------------

    /// The review report as Markdown.
    #[must_use]
    pub fn report_markdown(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "# {}\n", self.title);
        let _ = writeln!(out, "Repository: `{}`", self.repo_path.display());
        let sources: Vec<&str> = self
            .sources
            .iter()
            .filter(|source| source.included)
            .map(|source| source.label.as_str())
            .collect();
        let _ = writeln!(out, "Sources: {}", sources.join(", "));
        self.report_files(&mut out);
        self.report_threads(&mut out);
        out
    }

    /// The report's viewed progress and unviewed files.
    fn report_files(&self, out: &mut String) {
        if let Some(review) = &self.review {
            let progress = review.doc.viewed_progress();
            let _ = writeln!(out, "Viewed: {}/{} files", progress.viewed, progress.total);
            let unviewed: Vec<String> = review
                .doc
                .file_keys()
                .filter(|file| {
                    !review
                        .doc
                        .viewed_state(file)
                        .is_some_and(bcode_code_review::viewed::ViewedState::is_viewed)
                })
                .map(|file| file.path.clone())
                .collect();
            if !unviewed.is_empty() {
                let _ = writeln!(out, "\n## Unviewed files ({})\n", unviewed.len());
                for path in unviewed.iter().take(REPORT_UNVIEWED_FILES) {
                    let _ = writeln!(out, "* `{path}`");
                }
                if unviewed.len() > REPORT_UNVIEWED_FILES {
                    let _ = writeln!(
                        out,
                        "* … and {} more",
                        unviewed.len() - REPORT_UNVIEWED_FILES
                    );
                }
            }
        }
    }

    /// The report's threads and AI questions.
    fn report_threads(&self, out: &mut String) {
        let mut seen = std::collections::BTreeSet::new();
        let threads: Vec<&str> = self
            .notes
            .drafts()
            .iter()
            .map(|draft| draft.thread_id.as_str())
            .filter(|thread| seen.insert(*thread))
            .collect();
        let open = threads
            .iter()
            .filter(|thread| {
                self.notes
                    .thread(thread)
                    .first()
                    .is_some_and(|first| first.resolved_at_ms.is_none())
            })
            .count();
        let _ = writeln!(
            out,
            "\n## Threads ({} open, {} resolved)\n",
            open,
            threads.len() - open
        );
        if threads.is_empty() {
            let _ = writeln!(out, "No threads.");
        }
        for thread in threads.iter().take(REPORT_THREADS) {
            let comments = self.notes.thread(thread);
            let Some(first) = comments.first() else {
                continue;
            };
            let _ = writeln!(
                out,
                "### {} · {} · {} · {}",
                super::annotate::location_label(&first.anchor),
                notes::kind_label(first.thread_kind),
                notes::severity_label(first.severity),
                if first.resolved_at_ms.is_some() {
                    "resolved"
                } else {
                    "open"
                }
            );
            if let Some(session) = comments
                .iter()
                .find_map(|comment| comment.session_id.as_ref())
            {
                let _ = writeln!(out, "Linked session: `{session}`");
            }
            for comment in comments {
                let _ = writeln!(out, "\n{}\n", comment.body.trim());
            }
        }
        let exchanges = self.notes.exchanges();
        if !exchanges.is_empty() {
            let _ = writeln!(out, "\n## Bcode questions ({})\n", exchanges.len());
            for exchange in exchanges {
                let _ = writeln!(
                    out,
                    "* {}: {}{}",
                    super::annotate::location_label(&exchange.anchor),
                    exchange.question.lines().next().unwrap_or_default(),
                    exchange
                        .session_id
                        .as_ref()
                        .map(|session| format!(" (session `{session}`)"))
                        .unwrap_or_default()
                );
            }
        }
    }

    /// Copy the review report to the clipboard.
    pub(super) fn copy_report(&mut self) {
        self.clipboard = Some((self.report_markdown(), "review report"));
    }
}
