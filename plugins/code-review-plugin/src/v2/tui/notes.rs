//! Review annotations of the V2 surface: drafts, threads, AI exchanges, and suggestions, kept in
//! the same persisted store V1 uses (`draft.*`, `thread.*`, `review.ai_exchange*`,
//! `review.suggestions.list`).
//!
//! * Changes are optimistic: the store changes at once, and the matching service call is queued.
//!   Calls run one at a time in order; each completion is reconciled by its job identity (never
//!   by body or position), and a failed call rolls its own change back.
//! * A new draft has a local id until its save completes; local drafts cannot be edited or
//!   deleted yet (the service does not know them).
//! * Requests use the same review scope V1 derives from the workspace, so V1 and V2 read and
//!   write the same threads.

use crate::v2::document::Annotations;
use bcode_code_review_models::{
    ClassifyThreadRequest, ClassifyThreadResponse, DeleteDraftRequest,
    DeleteReviewAiExchangeRequest, DraftAnchor, DraftComment, ListDraftsRequest,
    ListDraftsResponse, ListReviewAiExchangesRequest, ListReviewAiExchangesResponse,
    ListReviewSuggestionsRequest, ListReviewSuggestionsResponse, OP_REVIEW_AI_EXCHANGE_DELETE,
    OP_REVIEW_AI_EXCHANGES_LIST, OP_REVIEW_SUGGESTIONS_LIST, OP_THREAD_CLASSIFY,
    ResolveThreadRequest, ReviewAiExchange, ReviewScope, ReviewSourceKind, ReviewSuggestion,
    ReviewTarget, ReviewThreadKind, ReviewThreadSeverity, ReviewWorkspace, SaveDraftRequest,
    SaveDraftResponse, UpdateDraftRequest,
};
use std::collections::VecDeque;
use std::path::PathBuf;

use super::backend::{SharedBackend, call_json};

/// `draft.list`.
pub const OP_DRAFT_LIST: &str = "draft.list";
/// `draft.save`.
pub const OP_DRAFT_SAVE: &str = "draft.save";
/// `draft.update`.
pub const OP_DRAFT_UPDATE: &str = "draft.update";
/// `draft.delete`.
pub const OP_DRAFT_DELETE: &str = "draft.delete";
/// `thread.resolve`.
pub const OP_THREAD_RESOLVE: &str = "thread.resolve";
/// `thread.link_session`.
pub const OP_THREAD_LINK_SESSION: &str = "thread.link_session";
/// Prefix of comment ids not persisted yet.
const LOCAL_PREFIX: &str = "local-";

/// Where annotation requests are addressed: the repository, the workspace's fallback target, and
/// its scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteScope {
    /// Repository path.
    pub repo_path: PathBuf,
    /// Fallback target.
    pub target: ReviewTarget,
    /// Workspace scope.
    pub scope: Option<ReviewScope>,
}

impl NoteScope {
    /// The scope V1 uses for `workspace`: the workspace id with the first included source's
    /// target (the repository when it has none expressible as a target).
    #[must_use]
    pub fn for_workspace(workspace: &ReviewWorkspace) -> Self {
        let target = workspace
            .sources
            .iter()
            .find(|source| source.included)
            .map_or(ReviewTarget::Repository, |source| {
                target_of_kind(&source.kind)
            });
        Self {
            repo_path: workspace.repo_root.clone(),
            target: target.clone(),
            scope: Some(ReviewScope::Workspace {
                workspace_id: workspace.id.clone(),
                target,
            }),
        }
    }
}

/// The V1 target of a source kind (the repository for kinds V1 has no target for).
fn target_of_kind(kind: &ReviewSourceKind) -> ReviewTarget {
    match kind {
        ReviewSourceKind::WorkingTreeUnstaged => ReviewTarget::WorkingTreeUnstaged,
        ReviewSourceKind::IndexStaged => ReviewTarget::IndexStaged,
        ReviewSourceKind::WorkingTreeAndIndex => ReviewTarget::WorkingTreeAndIndex,
        ReviewSourceKind::LastCommit => ReviewTarget::LastCommit,
        ReviewSourceKind::Commit { rev } => ReviewTarget::CommitRange {
            base: format!("{rev}^"),
            head: rev.clone(),
            merge_base: false,
        },
        ReviewSourceKind::CommitRange {
            base,
            head,
            merge_base,
        } => ReviewTarget::CommitRange {
            base: base.clone(),
            head: head.clone(),
            merge_base: *merge_base,
        },
        ReviewSourceKind::BranchCompare {
            base_branch,
            head_branch,
            merge_base,
        } => ReviewTarget::BranchCompare {
            base_branch: base_branch.clone(),
            head_branch: head_branch.clone(),
            merge_base: *merge_base,
        },
        _ => ReviewTarget::Repository,
    }
}

/// Every persisted annotation of a review.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadedNotes {
    /// Drafts.
    pub drafts: Vec<DraftComment>,
    /// AI exchanges.
    pub exchanges: Vec<ReviewAiExchange>,
    /// Suggestions.
    pub suggestions: Vec<ReviewSuggestion>,
}

/// One queued service call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    /// Save a new draft (a new thread, or a reply when `thread_id` names an existing thread).
    Save {
        /// Local comment id until the save completes.
        local: String,
        /// Anchor.
        anchor: DraftAnchor,
        /// Body.
        body: String,
        /// Kind and severity to give a new thread.
        classify: Option<(ReviewThreadKind, ReviewThreadSeverity)>,
    },
    /// Replace a draft's body.
    Update {
        /// Comment id.
        comment_id: String,
        /// New body.
        body: String,
        /// Body before the change, restored on failure.
        previous: String,
    },
    /// Delete a draft.
    Delete {
        /// The deleted draft, restored on failure.
        comment: DraftComment,
        /// Its position, for restoring.
        index: usize,
    },
    /// Resolve or reopen a thread.
    Resolve {
        /// Thread id.
        thread_id: String,
        /// Thread anchor.
        anchor: DraftAnchor,
        /// Resolve (or reopen).
        resolved: bool,
        /// Resolution times before the change, per comment id.
        previous: Vec<(String, Option<u64>)>,
    },
    /// Set a thread's kind and severity.
    Classify {
        /// Thread id.
        thread_id: String,
        /// Thread anchor.
        anchor: DraftAnchor,
        /// Kind.
        kind: ReviewThreadKind,
        /// Severity.
        severity: ReviewThreadSeverity,
        /// Kind and severity before the change.
        previous: (ReviewThreadKind, ReviewThreadSeverity),
    },
    /// Delete an AI exchange.
    DeleteExchange {
        /// The deleted exchange, restored on failure.
        exchange: ReviewAiExchange,
    },
}

/// Result of one finished mutation.
#[derive(Debug)]
pub enum MutationResult {
    /// The saved draft.
    Saved(Box<DraftComment>),
    /// Done without a payload the store needs.
    Done,
    /// The call failed.
    Failed(String),
}

/// The annotations of the open review and the mutations not yet persisted.
#[derive(Debug, Default)]
pub struct ReviewNotes {
    drafts: Vec<DraftComment>,
    exchanges: Vec<ReviewAiExchange>,
    suggestions: Vec<ReviewSuggestion>,
    queue: VecDeque<Mutation>,
    running: Option<Mutation>,
    next_local: u64,
    revision: u64,
}

impl ReviewNotes {
    /// Changes whenever any annotation changed (rows showing annotations key on it).
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    const fn touch(&mut self) {
        self.revision += 1;
    }

    /// Replace the persisted annotations, keeping drafts whose save has not completed.
    pub fn load(&mut self, loaded: LoadedNotes) {
        let local: Vec<DraftComment> = self
            .drafts
            .iter()
            .filter(|draft| is_local(&draft.comment_id))
            .cloned()
            .collect();
        self.drafts = loaded.drafts;
        self.drafts.extend(local);
        self.exchanges = loaded.exchanges;
        self.suggestions = loaded.suggestions;
        self.touch();
    }

    /// The annotations as the document shows them.
    #[must_use]
    pub fn annotations(&self) -> Annotations {
        Annotations {
            drafts: self.drafts.clone(),
            exchanges: self.exchanges.clone(),
            suggestions: self.suggestions.clone(),
        }
    }

    /// A draft by comment id.
    #[must_use]
    pub fn comment(&self, comment_id: &str) -> Option<&DraftComment> {
        self.drafts
            .iter()
            .find(|draft| draft.comment_id == comment_id)
    }

    /// Comments of a thread, oldest first.
    #[must_use]
    pub fn thread(&self, thread_id: &str) -> Vec<&DraftComment> {
        let mut comments: Vec<&DraftComment> = self
            .drafts
            .iter()
            .filter(|draft| draft.thread_id == thread_id)
            .collect();
        comments.sort_by(|a, b| {
            (a.created_at_ms, &a.comment_id).cmp(&(b.created_at_ms, &b.comment_id))
        });
        comments
    }

    /// Every draft.
    #[must_use]
    pub fn drafts(&self) -> &[DraftComment] {
        &self.drafts
    }

    /// An AI exchange by id.
    #[must_use]
    pub fn exchange(&self, exchange_id: &str) -> Option<&ReviewAiExchange> {
        self.exchanges
            .iter()
            .find(|exchange| exchange.exchange_id == exchange_id)
    }

    /// Every AI exchange.
    #[must_use]
    pub fn exchanges(&self) -> &[ReviewAiExchange] {
        &self.exchanges
    }

    /// A suggestion by id.
    #[must_use]
    pub fn suggestion(&self, suggestion_id: &str) -> Option<&ReviewSuggestion> {
        self.suggestions
            .iter()
            .find(|suggestion| suggestion.suggestion_id == suggestion_id)
    }

    /// Draft comments (persisted or not).
    #[must_use]
    pub const fn draft_count(&self) -> usize {
        self.drafts.len()
    }

    /// Mutations queued or running.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.queue.len() + usize::from(self.running.is_some())
    }

    /// Insert or replace an exchange (its id is its identity).
    pub fn put_exchange(&mut self, exchange: ReviewAiExchange) {
        match self
            .exchanges
            .iter_mut()
            .find(|existing| existing.exchange_id == exchange.exchange_id)
        {
            Some(existing) => *existing = exchange,
            None => self.exchanges.push(exchange),
        }
        self.touch();
    }

    /// Add a draft at once and queue its save. A reply joins `thread_id`; a new thread gets a
    /// local thread id until saved. Returns the local comment id.
    pub fn add_draft(
        &mut self,
        anchor: DraftAnchor,
        body: String,
        thread_id: Option<String>,
        classify: Option<(ReviewThreadKind, ReviewThreadSeverity)>,
        now_ms: u64,
    ) -> String {
        self.next_local += 1;
        let local = format!("{LOCAL_PREFIX}{}", self.next_local);
        let (kind, severity) = thread_id
            .as_deref()
            .and_then(|thread| {
                self.thread(thread)
                    .first()
                    .map(|first| (first.thread_kind, first.severity))
            })
            .or(classify)
            .unwrap_or_default();
        self.drafts.push(DraftComment {
            comment_id: local.clone(),
            thread_id: thread_id
                .unwrap_or_else(|| format!("{LOCAL_PREFIX}thread-{}", self.next_local)),
            anchor: anchor.clone(),
            body: body.clone(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            session_id: None,
            resolved_at_ms: None,
            thread_kind: kind,
            severity,
        });
        self.queue.push_back(Mutation::Save {
            local: local.clone(),
            anchor,
            body,
            classify: classify.filter(|value| *value != Default::default()),
        });
        self.touch();
        local
    }

    /// Change a persisted draft's body at once and queue the update.
    ///
    /// # Errors
    ///
    /// * When the draft does not exist or is not persisted yet.
    pub fn update_draft(&mut self, comment_id: &str, body: String) -> Result<(), String> {
        let draft = self.persisted_mut(comment_id)?;
        let previous = std::mem::replace(&mut draft.body, body.clone());
        self.queue.push_back(Mutation::Update {
            comment_id: comment_id.to_owned(),
            body,
            previous,
        });
        self.touch();
        Ok(())
    }

    /// Remove a persisted draft at once and queue the delete.
    ///
    /// # Errors
    ///
    /// * When the draft does not exist or is not persisted yet.
    pub fn delete_draft(&mut self, comment_id: &str) -> Result<(), String> {
        self.persisted_mut(comment_id)?;
        let index = self
            .drafts
            .iter()
            .position(|draft| draft.comment_id == comment_id)
            .ok_or_else(|| "comment not found".to_owned())?;
        let comment = self.drafts.remove(index);
        self.queue.push_back(Mutation::Delete { comment, index });
        self.touch();
        Ok(())
    }

    /// Resolve or reopen a thread at once and queue the change.
    ///
    /// # Errors
    ///
    /// * When the thread has no comments or is not persisted yet.
    pub fn resolve(&mut self, thread_id: &str, resolved: bool, now_ms: u64) -> Result<(), String> {
        let anchor = self.thread_anchor(thread_id)?;
        let mut previous = Vec::new();
        for draft in self
            .drafts
            .iter_mut()
            .filter(|draft| draft.thread_id == thread_id)
        {
            previous.push((draft.comment_id.clone(), draft.resolved_at_ms));
            draft.resolved_at_ms = resolved.then_some(now_ms);
        }
        self.queue.push_back(Mutation::Resolve {
            thread_id: thread_id.to_owned(),
            anchor,
            resolved,
            previous,
        });
        self.touch();
        Ok(())
    }

    /// Set a thread's kind and severity at once and queue the change.
    ///
    /// # Errors
    ///
    /// * When the thread has no comments or is not persisted yet.
    pub fn classify(
        &mut self,
        thread_id: &str,
        kind: ReviewThreadKind,
        severity: ReviewThreadSeverity,
    ) -> Result<(), String> {
        let anchor = self.thread_anchor(thread_id)?;
        let mut previous = None;
        for draft in self
            .drafts
            .iter_mut()
            .filter(|draft| draft.thread_id == thread_id)
        {
            previous.get_or_insert((draft.thread_kind, draft.severity));
            draft.thread_kind = kind;
            draft.severity = severity;
        }
        self.queue.push_back(Mutation::Classify {
            thread_id: thread_id.to_owned(),
            anchor,
            kind,
            severity,
            previous: previous.unwrap_or_default(),
        });
        self.touch();
        Ok(())
    }

    /// Remove an AI exchange at once and queue the delete.
    ///
    /// # Errors
    ///
    /// * When the exchange does not exist.
    pub fn delete_exchange(&mut self, exchange_id: &str) -> Result<(), String> {
        let index = self
            .exchanges
            .iter()
            .position(|exchange| exchange.exchange_id == exchange_id)
            .ok_or_else(|| "AI exchange not found".to_owned())?;
        let exchange = self.exchanges.remove(index);
        self.queue.push_back(Mutation::DeleteExchange { exchange });
        self.touch();
        Ok(())
    }

    fn persisted_mut(&mut self, comment_id: &str) -> Result<&mut DraftComment, String> {
        if is_local(comment_id) {
            return Err("this comment is still being saved; try again after save".to_owned());
        }
        self.drafts
            .iter_mut()
            .find(|draft| draft.comment_id == comment_id)
            .ok_or_else(|| "comment not found".to_owned())
    }

    fn thread_anchor(&self, thread_id: &str) -> Result<DraftAnchor, String> {
        if is_local(thread_id) {
            return Err("this thread is still being saved; try again after save".to_owned());
        }
        self.drafts
            .iter()
            .find(|draft| draft.thread_id == thread_id)
            .map(|draft| draft.anchor.clone())
            .ok_or_else(|| "thread not found".to_owned())
    }

    /// The next mutation to run, when none is running.
    pub fn start_next(&mut self) -> Option<Mutation> {
        if self.running.is_some() {
            return None;
        }
        let next = self.queue.pop_front()?;
        self.running = Some(next.clone());
        Some(next)
    }

    /// Apply the result of the running mutation. Returns an error message when it failed (its
    /// change was rolled back).
    pub fn finish(&mut self, result: MutationResult) -> Option<String> {
        let mutation = self.running.take()?;
        self.touch();
        match (mutation, result) {
            (Mutation::Save { local, .. }, MutationResult::Saved(saved)) => {
                let saved = *saved;
                let local_thread = self
                    .drafts
                    .iter()
                    .find(|draft| draft.comment_id == local)
                    .map(|draft| draft.thread_id.clone());
                if let Some(draft) = self
                    .drafts
                    .iter_mut()
                    .find(|draft| draft.comment_id == local)
                {
                    let (kind, severity) = (draft.thread_kind, draft.severity);
                    *draft = saved.clone();
                    draft.thread_kind = kind;
                    draft.severity = severity;
                }
                // Queued changes of the thread address it by its persisted id from now on.
                if let Some(local_thread) = local_thread.filter(|thread| is_local(thread)) {
                    for draft in &mut self.drafts {
                        if draft.thread_id == local_thread {
                            draft.thread_id.clone_from(&saved.thread_id);
                        }
                    }
                }
                None
            }
            (Mutation::Save { local, .. }, MutationResult::Failed(error)) => {
                self.drafts.retain(|draft| draft.comment_id != local);
                Some(format!("could not save the comment: {error}"))
            }
            (
                Mutation::Update {
                    comment_id,
                    previous,
                    ..
                },
                MutationResult::Failed(error),
            ) => {
                if let Some(draft) = self
                    .drafts
                    .iter_mut()
                    .find(|draft| draft.comment_id == comment_id)
                {
                    draft.body = previous;
                }
                Some(format!("could not update the comment: {error}"))
            }
            (Mutation::Delete { comment, index }, MutationResult::Failed(error)) => {
                let index = index.min(self.drafts.len());
                self.drafts.insert(index, comment);
                Some(format!("could not delete the comment: {error}"))
            }
            (Mutation::Resolve { previous, .. }, MutationResult::Failed(error)) => {
                for (comment_id, resolved) in previous {
                    if let Some(draft) = self
                        .drafts
                        .iter_mut()
                        .find(|draft| draft.comment_id == comment_id)
                    {
                        draft.resolved_at_ms = resolved;
                    }
                }
                Some(format!("could not change the thread: {error}"))
            }
            (
                Mutation::Classify {
                    thread_id,
                    previous,
                    ..
                },
                MutationResult::Failed(error),
            ) => {
                for draft in self
                    .drafts
                    .iter_mut()
                    .filter(|draft| draft.thread_id == thread_id)
                {
                    (draft.thread_kind, draft.severity) = previous;
                }
                Some(format!("could not change the thread: {error}"))
            }
            (Mutation::DeleteExchange { exchange }, MutationResult::Failed(error)) => {
                self.exchanges.push(exchange);
                Some(format!("could not delete the AI exchange: {error}"))
            }
            _ => None,
        }
    }
}

/// Whether `id` is a local (unsaved) comment or thread id.
#[must_use]
pub fn is_local(id: &str) -> bool {
    id.starts_with(LOCAL_PREFIX)
}

/// Load every annotation of the review.
///
/// # Errors
///
/// Returns the first failing call's message.
pub async fn load(backend: SharedBackend, scope: NoteScope) -> Result<LoadedNotes, String> {
    let backend = backend.as_ref();
    let drafts: ListDraftsResponse = call_json(
        backend,
        OP_DRAFT_LIST,
        &ListDraftsRequest {
            repo_path: scope.repo_path.clone(),
            target: scope.target.clone(),
            scope: scope.scope.clone(),
        },
    )
    .await
    .map_err(|error| error.message)?;
    let exchanges: ListReviewAiExchangesResponse = call_json(
        backend,
        OP_REVIEW_AI_EXCHANGES_LIST,
        &ListReviewAiExchangesRequest {
            repo_path: scope.repo_path.clone(),
            target: scope.target.clone(),
            scope: scope.scope.clone(),
        },
    )
    .await
    .map_err(|error| error.message)?;
    let suggestions: ListReviewSuggestionsResponse = call_json(
        backend,
        OP_REVIEW_SUGGESTIONS_LIST,
        &ListReviewSuggestionsRequest {
            repo_path: scope.repo_path.clone(),
            target: scope.target.clone(),
            scope: scope.scope.clone(),
        },
    )
    .await
    .map_err(|error| error.message)?;
    Ok(LoadedNotes {
        drafts: drafts.drafts,
        exchanges: exchanges.exchanges,
        suggestions: suggestions.suggestions,
    })
}

/// Run one mutation.
pub async fn run(backend: SharedBackend, scope: NoteScope, mutation: Mutation) -> MutationResult {
    let backend = backend.as_ref();
    let NoteScope {
        repo_path,
        target,
        scope,
    } = scope;
    let json = |request: Result<serde_json::Value, serde_json::Error>| request.unwrap_or_default();
    let (operation, request) = match mutation {
        Mutation::Save {
            anchor,
            body,
            classify,
            ..
        } => {
            let scope = NoteScope {
                repo_path,
                target,
                scope,
            };
            return save(backend, scope, anchor, body, classify)
                .await
                .unwrap_or_else(|error| MutationResult::Failed(error.message));
        }
        Mutation::Update {
            comment_id, body, ..
        } => (
            OP_DRAFT_UPDATE,
            json(serde_json::to_value(UpdateDraftRequest {
                repo_path,
                target,
                scope,
                comment_id,
                body,
            })),
        ),
        Mutation::Delete { comment, .. } => (
            OP_DRAFT_DELETE,
            json(serde_json::to_value(DeleteDraftRequest {
                repo_path,
                target,
                scope,
                comment_id: comment.comment_id,
            })),
        ),
        Mutation::Resolve {
            anchor, resolved, ..
        } => (
            OP_THREAD_RESOLVE,
            json(serde_json::to_value(ResolveThreadRequest {
                repo_path,
                target,
                scope,
                anchor,
                resolved,
            })),
        ),
        Mutation::Classify {
            anchor,
            kind,
            severity,
            ..
        } => (
            OP_THREAD_CLASSIFY,
            json(serde_json::to_value(ClassifyThreadRequest {
                repo_path,
                target,
                scope,
                anchor,
                thread_kind: kind,
                severity,
            })),
        ),
        Mutation::DeleteExchange { exchange } => (
            OP_REVIEW_AI_EXCHANGE_DELETE,
            json(serde_json::to_value(DeleteReviewAiExchangeRequest {
                repo_path,
                target,
                scope,
                exchange_id: exchange.exchange_id,
            })),
        ),
    };
    call_json::<_, serde_json::Value>(backend, operation, &request)
        .await
        .map_or_else(
            |error| MutationResult::Failed(error.message),
            |_| MutationResult::Done,
        )
}

/// Save a draft, then give a new thread its kind and severity.
async fn save(
    backend: &dyn super::backend::ReviewBackend,
    scope: NoteScope,
    anchor: DraftAnchor,
    body: String,
    classify: Option<(ReviewThreadKind, ReviewThreadSeverity)>,
) -> Result<MutationResult, crate::v2::provider::ProviderCallError> {
    let NoteScope {
        repo_path,
        target,
        scope,
    } = scope;
    let saved: SaveDraftResponse = call_json(
        backend,
        OP_DRAFT_SAVE,
        &SaveDraftRequest {
            repo_path: repo_path.clone(),
            target: target.clone(),
            scope: scope.clone(),
            anchor: anchor.clone(),
            body,
        },
    )
    .await?;
    if let Some((kind, severity)) = classify {
        let _: ClassifyThreadResponse = call_json(
            backend,
            OP_THREAD_CLASSIFY,
            &ClassifyThreadRequest {
                repo_path,
                target,
                scope,
                anchor,
                thread_kind: kind,
                severity,
            },
        )
        .await?;
    }
    Ok(MutationResult::Saved(Box::new(saved.draft)))
}

/// The next value of a thread kind, cycling.
#[must_use]
pub const fn next_kind(kind: ReviewThreadKind) -> ReviewThreadKind {
    match kind {
        ReviewThreadKind::Note => ReviewThreadKind::Question,
        ReviewThreadKind::Question => ReviewThreadKind::Todo,
        ReviewThreadKind::Todo => ReviewThreadKind::Finding,
        ReviewThreadKind::Finding => ReviewThreadKind::Summary,
        ReviewThreadKind::Summary => ReviewThreadKind::Note,
    }
}

/// The next value of a thread severity, cycling.
#[must_use]
pub const fn next_severity(severity: ReviewThreadSeverity) -> ReviewThreadSeverity {
    match severity {
        ReviewThreadSeverity::Info => ReviewThreadSeverity::Nit,
        ReviewThreadSeverity::Nit => ReviewThreadSeverity::Warning,
        ReviewThreadSeverity::Warning => ReviewThreadSeverity::Blocker,
        ReviewThreadSeverity::Blocker => ReviewThreadSeverity::Info,
    }
}

/// Display label of a thread kind.
#[must_use]
pub const fn kind_label(kind: ReviewThreadKind) -> &'static str {
    match kind {
        ReviewThreadKind::Note => "note",
        ReviewThreadKind::Question => "question",
        ReviewThreadKind::Todo => "todo",
        ReviewThreadKind::Finding => "finding",
        ReviewThreadKind::Summary => "summary",
    }
}

/// Display label of a thread severity.
#[must_use]
pub const fn severity_label(severity: ReviewThreadSeverity) -> &'static str {
    match severity {
        ReviewThreadSeverity::Info => "info",
        ReviewThreadSeverity::Nit => "nit",
        ReviewThreadSeverity::Warning => "warning",
        ReviewThreadSeverity::Blocker => "blocker",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::{ReviewAnchorKind, ReviewLineKind};

    fn anchor() -> DraftAnchor {
        DraftAnchor {
            kind: ReviewAnchorKind::File,
            file_path: "a.rs".to_owned(),
            diff_row: 0,
            start_diff_row: None,
            end_diff_row: None,
            old_start: None,
            old_end: None,
            new_start: None,
            new_end: None,
            old_line: None,
            new_line: None,
            line_kind: ReviewLineKind::Context,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
            head_oid: None,
            blob_oid: None,
        }
    }

    fn persisted(local: &DraftComment, id: &str) -> DraftComment {
        DraftComment {
            comment_id: id.to_owned(),
            thread_id: "thread-1".to_owned(),
            ..local.clone()
        }
    }

    #[test]
    fn saves_reconcile_by_job_and_rename_local_threads() {
        let mut notes = ReviewNotes::default();
        let local = notes.add_draft(anchor(), "first".to_owned(), None, None, 1);
        assert!(is_local(&local));
        assert!(
            notes.update_draft(&local, "x".to_owned()).is_err(),
            "local drafts are not editable"
        );
        let job = notes.start_next().expect("save queued");
        assert!(notes.start_next().is_none(), "one job at a time");
        assert!(matches!(job, Mutation::Save { .. }));
        let draft = notes.comment(&local).cloned().expect("local draft");
        assert_eq!(
            notes.finish(MutationResult::Saved(Box::new(persisted(&draft, "c1")))),
            None
        );
        assert!(notes.comment(&local).is_none());
        assert_eq!(notes.thread("thread-1").len(), 1);
        notes
            .update_draft("c1", "edited".to_owned())
            .expect("persisted drafts are editable");
        assert_eq!(
            notes.comment("c1").map(|draft| draft.body.as_str()),
            Some("edited")
        );
    }

    #[test]
    fn failed_mutations_roll_back_only_their_own_change() {
        let mut notes = ReviewNotes::default();
        let local = notes.add_draft(anchor(), "body".to_owned(), None, None, 1);
        let draft = notes.comment(&local).cloned().expect("local");
        notes.start_next();
        notes.finish(MutationResult::Saved(Box::new(persisted(&draft, "c1"))));
        notes.resolve("thread-1", true, 5).expect("resolve");
        notes.delete_draft("c1").expect("delete");
        assert!(notes.comment("c1").is_none());
        notes.start_next();
        assert_eq!(
            notes.finish(MutationResult::Done),
            None,
            "resolve succeeded"
        );
        notes.start_next();
        let error = notes.finish(MutationResult::Failed("offline".to_owned()));
        assert!(error.is_some_and(|error| error.contains("offline")));
        let restored = notes.comment("c1").expect("delete rolled back");
        assert_eq!(
            restored.resolved_at_ms,
            Some(5),
            "the resolve stays applied"
        );
    }
}
