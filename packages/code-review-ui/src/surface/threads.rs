//! Cards' viewed and folded states (see `docs/review-thread-cards.md`): comment threads, and
//! AI answers (keyed [`AI_PREFIX`] and the exchange id, Bcode's answer being the comment from
//! someone else, so a new answer makes a viewed card unviewed again).
//!
//! * Viewed marks live in the workspace (`viewed_threads`), saved with viewed files.
//! * Folds by hand live in the presentation state (`thread_folds`).
//! * What shows folded is computed from both, the thread's resolution, and the configuration,
//!   whenever any of them changes ([`ReviewSurface::refresh_threads`]).

use super::ReviewSurface;
use super::annotate::now_ms;
use crate::rows::ThreadLook;
use crate::rows::thread_card::{AI_PREFIX, ai_key};
use bcode_code_review::threads::{self, CommentRef, ThreadViewed};
use bcode_code_review_models::{ThreadFoldMark, ThreadViewMark};
use bmux_tui_components::status_bar::StatusSeverity;
use std::collections::{BTreeMap, BTreeSet};

impl ReviewSurface {
    /// Every card shown: threads, and AI answers with an exchange.
    pub(super) fn card_ids(&self) -> BTreeSet<String> {
        let mut ids = self.notes.thread_ids();
        ids.extend(
            self.notes
                .exchanges()
                .iter()
                .map(|exchange| ai_key(&exchange.exchange_id)),
        );
        ids
    }

    /// A card's comments as viewed state counts them (id, whether you wrote it). An AI answer
    /// has one, its latest update, from Bcode.
    fn card_comments(&self, key: &str) -> Vec<(String, bool)> {
        key.strip_prefix(AI_PREFIX).map_or_else(
            || self.notes.comment_refs(key),
            |exchange_id| {
                self.notes
                    .exchange(exchange_id)
                    .map(|exchange| {
                        vec![(format!("{exchange_id}@{}", exchange.updated_at_ms), false)]
                    })
                    .unwrap_or_default()
            },
        )
    }

    /// The card at the cursor: its thread, or its AI answer.
    pub(super) fn card_key_at_cursor(&self) -> Option<String> {
        let review = self.review.as_ref()?;
        match review.doc.item(review.cursor_index)? {
            bcode_code_review::document::ReviewItem::Thread(thread) => {
                Some(thread.thread_id.clone())
            }
            bcode_code_review::document::ReviewItem::AgentThread(agent) => Some(ai_key(&agent.id)),
            _ => None,
        }
    }

    /// The viewed mark of `thread_id`.
    fn thread_mark(&self, thread_id: &str) -> Option<&ThreadViewMark> {
        self.workspace
            .as_ref()?
            .viewed_threads
            .iter()
            .find(|mark| mark.thread_id == thread_id)
    }

    /// Recompute every thread's viewed state, and which show folded. Bumps the folds' revision
    /// when anything changed.
    pub(super) fn refresh_threads(&mut self) {
        let rules = self.services.conversation_config().fold_rules();
        let mut looks = BTreeMap::new();
        let mut collapsed = BTreeSet::new();
        for thread_id in self.card_ids() {
            let refs = self.card_comments(&thread_id);
            let comments: Vec<CommentRef> = refs
                .iter()
                .map(|(id, mine)| CommentRef { id, mine: *mine })
                .collect();
            let mark = self.thread_mark(&thread_id);
            let viewed = threads::viewed(mark, &comments);
            let new = comments
                .iter()
                .filter(|comment| threads::is_new(mark, **comment))
                .map(|comment| comment.id.to_owned())
                .collect();
            let resolved =
                !thread_id.starts_with(AI_PREFIX) && self.notes.thread_resolved(&thread_id);
            let hand = self.folds.hand.get(&thread_id).copied();
            if threads::folded(hand, viewed, resolved, comments.len(), rules) {
                collapsed.insert(thread_id.clone());
            }
            looks.insert(thread_id, ThreadLook { viewed, new });
        }
        if collapsed != self.folds.collapsed || looks != self.folds.threads {
            self.folds.collapsed = collapsed;
            self.folds.threads = looks;
            self.folds_changed();
        }
    }

    /// Whether `thread_id` shows as viewed.
    pub(super) fn thread_viewed(&self, thread_id: &str) -> ThreadViewed {
        self.folds
            .threads
            .get(thread_id)
            .map_or(ThreadViewed::Unviewed, |look| look.viewed)
    }

    /// Mark `thread_id` viewed (pinned to its comments now) or unviewed. Its fold goes back to
    /// following the mark.
    pub(super) fn set_thread_viewed(&mut self, thread_id: &str, viewed: bool) {
        let refs = self.card_comments(thread_id);
        let comments: Vec<CommentRef> = refs
            .iter()
            .map(|(id, mine)| CommentRef { id, mine: *mine })
            .collect();
        let mark = threads::mark(thread_id, &comments, now_ms());
        let workspace = self.workspace_mut();
        workspace
            .viewed_threads
            .retain(|mark| mark.thread_id != thread_id);
        if viewed {
            workspace.viewed_threads.push(mark);
        }
        self.folds.hand.remove(thread_id);
        self.request_save();
        self.persist_presentation(true);
        self.refresh_threads();
    }

    /// Toggle the cursor card's viewed mark (`v` on a thread or an AI answer). Returns whether
    /// the cursor is on one.
    pub(super) fn toggle_thread_viewed(&mut self) -> bool {
        let Some(key) = self.card_key_at_cursor() else {
            return false;
        };
        if key.starts_with(AI_PREFIX) && self.card_comments(&key).is_empty() {
            self.set_status("this answer has no question to view", StatusSeverity::Info);
            return true;
        }
        let viewed = !self.thread_viewed(&key).is_viewed();
        self.set_thread_viewed(&key, viewed);
        let what = if key.starts_with(AI_PREFIX) {
            "answer"
        } else {
            "thread"
        };
        self.set_status(
            if viewed {
                format!("{what} viewed")
            } else {
                format!("{what} marked unviewed")
            },
            StatusSeverity::Success,
        );
        true
    }

    /// Mark every thread and AI answer viewed (or unviewed).
    pub(super) fn view_all_threads(&mut self, viewed: bool) {
        let ids = self.card_ids();
        if ids.is_empty() {
            self.set_status("no threads", StatusSeverity::Info);
            return;
        }
        let now = now_ms();
        let marks: Vec<ThreadViewMark> = ids
            .iter()
            .map(|thread_id| {
                let refs = self.card_comments(thread_id);
                let comments: Vec<CommentRef> = refs
                    .iter()
                    .map(|(id, mine)| CommentRef { id, mine: *mine })
                    .collect();
                threads::mark(thread_id, &comments, now)
            })
            .collect();
        let workspace = self.workspace_mut();
        workspace
            .viewed_threads
            .retain(|mark| !ids.contains(&mark.thread_id));
        if viewed {
            workspace.viewed_threads.extend(marks);
        }
        for thread_id in &ids {
            self.folds.hand.remove(thread_id);
        }
        self.request_save();
        self.persist_presentation(true);
        self.refresh_threads();
        self.set_status(
            format!(
                "{} thread{} marked {}",
                ids.len(),
                if ids.len() == 1 { "" } else { "s" },
                if viewed { "viewed" } else { "unviewed" }
            ),
            StatusSeverity::Success,
        );
    }

    /// Fold or open `thread_id` by hand: it stays so until it is marked viewed or unviewed, or
    /// (by configuration) a new comment comes.
    pub(super) fn fold_thread(&mut self, thread_id: &str, folded: bool) {
        let comments = self.card_comments(thread_id).len();
        self.folds.hand.insert(
            thread_id.to_owned(),
            ThreadFoldMark {
                folded,
                comments: Some(comments),
            },
        );
        self.persist_presentation(true);
        self.refresh_threads();
    }

    /// Fold (or open) every thread and AI answer by hand.
    pub(super) fn fold_threads(&mut self, folded: bool) {
        for thread_id in self.card_ids() {
            let comments = self.card_comments(&thread_id).len();
            self.folds.hand.insert(
                thread_id,
                ThreadFoldMark {
                    folded,
                    comments: Some(comments),
                },
            );
        }
        self.persist_presentation(true);
        self.refresh_threads();
    }

    /// After you resolve `thread_id`: marks it viewed, when configured.
    pub(super) fn resolved_by_you(&mut self, thread_id: &str, resolved: bool) {
        if resolved
            && self
                .services
                .conversation_config()
                .threads
                .resolve_marks_viewed
        {
            self.set_thread_viewed(thread_id, true);
        } else {
            self.refresh_threads();
        }
    }

    /// Folding and viewing across the review: every file folded or opened, every thread viewed,
    /// unviewed, folded or opened, and the next card not viewed. Returns whether `action` was
    /// one.
    pub(super) fn fold_and_view_action(&mut self, action: &str) -> bool {
        match action {
            "fold_files" | "open_files" => self.fold_files(action == "fold_files"),
            "unviewed_thread_next" => self.next_unviewed_card(),
            "view_threads" | "unview_threads" => self.view_all_threads(action == "view_threads"),
            "fold_threads" | "open_threads" => self.fold_threads(action == "fold_threads"),
            _ => return false,
        }
        true
    }

    /// Cards (threads and AI answers) viewed, and cards in all.
    pub(super) fn threads_progress(&self) -> (usize, usize) {
        let viewed = self
            .folds
            .threads
            .values()
            .filter(|look| look.viewed.is_viewed())
            .count();
        (viewed, self.folds.threads.len())
    }
}
