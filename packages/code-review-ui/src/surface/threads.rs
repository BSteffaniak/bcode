//! Threads' viewed and folded states (see `docs/review-thread-cards.md`).
//!
//! * Viewed marks live in the workspace (`viewed_threads`), saved with viewed files.
//! * Folds by hand live in the presentation state (`thread_folds`).
//! * What shows folded is computed from both, the thread's resolution, and the configuration,
//!   whenever any of them changes ([`ReviewSurface::refresh_threads`]).

use super::ReviewSurface;
use super::annotate::now_ms;
use crate::rows::ThreadLook;
use bcode_code_review::threads::{self, CommentRef, ThreadViewed};
use bcode_code_review_models::{ThreadFoldMark, ThreadViewMark};
use bmux_tui_components::status_bar::StatusSeverity;
use std::collections::{BTreeMap, BTreeSet};

impl ReviewSurface {
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
        for thread_id in self.notes.thread_ids() {
            let refs = self.notes.comment_refs(&thread_id);
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
            let resolved = self.notes.thread_resolved(&thread_id);
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
        let refs = self.notes.comment_refs(thread_id);
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

    /// Toggle the cursor thread's viewed mark (`v` on a thread). Returns whether the cursor is
    /// on one.
    pub(super) fn toggle_thread_viewed(&mut self) -> bool {
        let Some(thread_id) = self.thread_at_cursor() else {
            return false;
        };
        let viewed = !self.thread_viewed(&thread_id).is_viewed();
        self.set_thread_viewed(&thread_id, viewed);
        self.set_status(
            if viewed {
                "thread viewed"
            } else {
                "thread marked unviewed"
            },
            StatusSeverity::Success,
        );
        true
    }

    /// Mark every thread viewed (or unviewed).
    pub(super) fn view_all_threads(&mut self, viewed: bool) {
        let ids = self.notes.thread_ids();
        if ids.is_empty() {
            self.set_status("no threads", StatusSeverity::Info);
            return;
        }
        let now = now_ms();
        let marks: Vec<ThreadViewMark> = ids
            .iter()
            .map(|thread_id| {
                let refs = self.notes.comment_refs(thread_id);
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
        let comments = self.notes.thread(thread_id).len();
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

    /// Fold (or open) every thread by hand.
    pub(super) fn fold_threads(&mut self, folded: bool) {
        for thread_id in self.notes.thread_ids() {
            let comments = self.notes.thread(&thread_id).len();
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

    /// Every-thread actions: `view_threads`, `unview_threads`, `fold_threads`, `open_threads`.
    pub(super) fn threads_action(&mut self, action: &str) {
        match action {
            "view_threads" | "unview_threads" => self.view_all_threads(action == "view_threads"),
            _ => self.fold_threads(action == "fold_threads"),
        }
    }

    /// Threads viewed, and threads in all.
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
