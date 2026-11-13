//! Review annotations: drafts, threads, AI exchanges, and suggestions, kept in the review
//! store (`draft.*`, `thread.*`, `review.ai_exchange*`,
//! `review.suggestions.list`).
//!
//! * Changes are optimistic: the store changes at once, and the matching service call is queued.
//!   Calls run one at a time in order; each completion is reconciled by its job identity (never
//!   by body or position), and a failed call rolls its own change back.
//! * A new draft has a local id until its save completes; local drafts cannot be edited or
//!   deleted yet (the service does not know them).
//! * Requests use the annotation scope stored reviews have always used for their workspace, so
//!   older reviews keep their threads.

use bcode_code_review::document::Annotations;
use bcode_code_review_models::{
    ClassifyThreadRequest, ClassifyThreadResponse, DeleteDraftRequest,
    DeleteReviewAiExchangeRequest, DraftAnchor, DraftComment, ExternalAnchorMappingStatus,
    ExternalReviewImportRecord, ExternalReviewThreadState, ListDraftsRequest, ListDraftsResponse,
    ListExternalReviewImportsRequest, ListExternalReviewImportsResponse, ListPublishRecordsRequest,
    ListPublishRecordsResponse, ListReviewAiExchangesRequest, ListReviewAiExchangesResponse,
    ListReviewSuggestionsRequest, ListReviewSuggestionsResponse, ResolveThreadRequest,
    ReviewAiExchange, ReviewPublishRecord, ReviewScope, ReviewSuggestion, ReviewTarget,
    ReviewThreadKind, ReviewThreadSeverity, ReviewWorkspace, SaveDraftRequest, SaveDraftResponse,
    SaveReviewSuggestionRequest, UpdateDraftRequest,
};
use std::collections::VecDeque;
use std::path::PathBuf;

use crate::services::ReviewServices;

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
    /// The scope of `workspace`: its annotation scope id ([`bcode_code_review::scope`]: the
    /// workspace id, or a per-target id for a transient review) with the first included
    /// source's target (the repository when it has none expressible as a target), the scope
    /// stored durable workspaces have always used.
    #[must_use]
    pub fn for_workspace(workspace: &ReviewWorkspace) -> Self {
        let (target, scope) = bcode_code_review::scope::annotation_scope(workspace);
        Self {
            repo_path: workspace.repo_root.clone(),
            target,
            scope: Some(scope),
        }
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
    /// External review snapshots (durable reviews).
    pub external: Vec<ExternalReviewImportRecord>,
    /// Publish records (durable reviews).
    pub published: Vec<ReviewPublishRecord>,
}

/// What an imported (external, read-only) thread carries besides its comments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalThread {
    /// Importer that produced it.
    pub importer_id: String,
    /// Provider of the thread.
    pub provider_id: String,
    /// Remote state.
    pub state: ExternalReviewThreadState,
    /// How its anchor mapped onto this review.
    pub mapping: ExternalAnchorMappingStatus,
    /// Mapping explanation.
    pub message: Option<String>,
    /// Browser URL.
    pub url: Option<String>,
    /// Author label per comment id.
    pub authors: std::collections::BTreeMap<String, String>,
}

/// Prefix of imported thread and comment ids.
pub const EXTERNAL_PREFIX: &str = "external:";

/// Prefix of the ids of pull request threads the review reads from their provider (followed by
/// the pull request's key, a `\u{1}`, and the provider's thread id).
pub const REMOTE_PREFIX: &str = "remote:";

/// The review's id of `thread_id` on the pull request `pull` (a [`crate::conversation::PullKey`]
/// id).
#[must_use]
pub fn remote_thread_id(pull: &str, thread_id: &str) -> String {
    format!("{REMOTE_PREFIX}{}", reply_target(pull, thread_id))
}

/// What a held reply's anchor names as the thread it replies to (`DraftAnchor::reply_to`).
#[must_use]
pub fn reply_target(pull: &str, thread_id: &str) -> String {
    format!("{pull}\u{1}{thread_id}")
}

/// The review's id of the pull request thread a held reply's `reply_to` names.
#[must_use]
pub fn replied_thread(reply_to: &str) -> String {
    format!("{REMOTE_PREFIX}{reply_to}")
}

/// A pull request thread as the review shows it, besides its comments.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct RemoteThreadInfo {
    /// The pull request ([`crate::conversation::PullKey`] id).
    pub pull: String,
    /// Provider's display name.
    pub provider_label: String,
    /// The provider's thread id.
    pub remote_id: String,
    /// Resolved.
    pub resolved: bool,
    /// The forge considers its code changed since.
    pub outdated: bool,
    /// How it was placed on the review.
    pub placement: bcode_code_review::conversation::Placement,
    /// The viewer may reply.
    pub can_reply: bool,
    /// The viewer may resolve or reopen it.
    pub can_resolve: bool,
    /// Someone else spoke last on something the viewer is part of.
    pub waiting: bool,
    /// Something in it arrived since the viewer last looked.
    pub new: bool,
    /// Web page.
    pub url: Option<String>,
    /// Author and time per comment id.
    pub authors: std::collections::BTreeMap<String, (String, Option<u64>)>,
    /// Comments the viewer wrote, by id.
    pub mine: std::collections::BTreeSet<String>,
    /// Provider repository.
    pub repository: String,
    /// Pull request number.
    pub number: u64,
    /// The thread as the forge reported it (where it was written, its comments and diff hunk).
    pub thread: bcode_code_review_models::RemoteThread,
}

/// One remote thread to show: its info and comments (already anchored).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteNote {
    /// The review's thread id ([`remote_thread_id`]).
    pub thread_id: String,
    /// Where it shows.
    pub anchor: DraftAnchor,
    /// Its info.
    pub info: RemoteThreadInfo,
    /// Comments: id, body, created.
    pub comments: Vec<(String, String, u64)>,
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
    /// Create or update a suggestion.
    SaveSuggestion {
        /// The suggestion as saved.
        suggestion: Box<ReviewSuggestion>,
        /// The suggestion before the change (`None` when it is new), restored on failure.
        previous: Option<Box<ReviewSuggestion>>,
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
    /// Imported threads as read-only comments (never persisted as drafts).
    external_drafts: Vec<DraftComment>,
    external: std::collections::BTreeMap<String, ExternalThread>,
    external_records: Vec<ExternalReviewImportRecord>,
    /// Pull request threads read from their providers (never persisted).
    remote_drafts: Vec<DraftComment>,
    remote: std::collections::BTreeMap<String, RemoteThreadInfo>,
    /// Anchors recorded as published (with revision fields cleared).
    published: Vec<DraftAnchor>,
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
        // Drafts whose deletion is still queued or running stay deleted.
        let deleting: std::collections::BTreeSet<String> = self
            .queue
            .iter()
            .chain(self.running.iter())
            .filter_map(|mutation| match mutation {
                Mutation::Delete { comment, .. } => Some(comment.comment_id.clone()),
                _ => None,
            })
            .collect();
        self.drafts = loaded.drafts;
        self.drafts
            .retain(|draft| !deleting.contains(&draft.comment_id));
        self.drafts.extend(local);
        self.exchanges = loaded.exchanges;
        self.suggestions = loaded.suggestions;
        self.set_external(loaded.external);
        self.set_published(&loaded.published);
        self.touch();
    }

    /// Replace the imported threads with these snapshots (one per importer).
    pub fn set_external(&mut self, records: Vec<ExternalReviewImportRecord>) {
        self.external_drafts.clear();
        self.external.clear();
        for record in &records {
            for thread in &record.threads {
                let thread_id = format!(
                    "{EXTERNAL_PREFIX}{}:{}",
                    record.importer_id, thread.identity.external_id
                );
                let anchor = thread
                    .mapping
                    .anchor
                    .clone()
                    .filter(|_| thread.mapping.status != ExternalAnchorMappingStatus::Unmappable)
                    .unwrap_or_else(review_anchor);
                let resolved = thread.state == ExternalReviewThreadState::Resolved;
                let mut authors = std::collections::BTreeMap::new();
                for (index, comment) in thread.comments.iter().enumerate() {
                    let comment_id = format!("{thread_id}:{}", comment.identity.external_id);
                    if let Some(author) = &comment.author_label {
                        authors.insert(comment_id.clone(), author.clone());
                    }
                    let created = comment
                        .created_at_ms
                        .unwrap_or_else(|| u64::try_from(index).unwrap_or(u64::MAX));
                    self.external_drafts.push(DraftComment {
                        comment_id,
                        thread_id: thread_id.clone(),
                        anchor: anchor.clone(),
                        body: comment.body.clone(),
                        created_at_ms: created,
                        updated_at_ms: comment.updated_at_ms.unwrap_or(created),
                        session_id: None,
                        resolved_at_ms: resolved.then_some(created),
                        thread_kind: ReviewThreadKind::Note,
                        severity: ReviewThreadSeverity::Info,
                    });
                }
                self.external.insert(
                    thread_id,
                    ExternalThread {
                        importer_id: record.importer_id.clone(),
                        provider_id: thread.identity.provider_id.clone(),
                        state: thread.state,
                        mapping: thread.mapping.status,
                        message: thread.mapping.message.clone(),
                        url: thread.identity.url.clone(),
                        authors,
                    },
                );
            }
        }
        self.external_records = records;
        self.touch();
    }

    /// Add (or replace) one importer's snapshot.
    pub fn put_external(&mut self, record: ExternalReviewImportRecord) {
        let mut records = std::mem::take(&mut self.external_records);
        records.retain(|existing| existing.importer_id != record.importer_id);
        records.push(record);
        self.set_external(records);
    }

    /// Replace the pull request threads read from providers with `notes`. Returns whether
    /// anything shown changed.
    pub fn set_remote(&mut self, notes: Vec<RemoteNote>) -> bool {
        let mut drafts = Vec::new();
        let mut remote = std::collections::BTreeMap::new();
        for note in notes {
            for (comment_id, body, created) in note.comments {
                drafts.push(DraftComment {
                    comment_id,
                    thread_id: note.thread_id.clone(),
                    anchor: note.anchor.clone(),
                    body,
                    created_at_ms: created,
                    updated_at_ms: created,
                    session_id: None,
                    resolved_at_ms: note.info.resolved.then_some(created),
                    thread_kind: ReviewThreadKind::Note,
                    severity: ReviewThreadSeverity::Info,
                });
            }
            remote.insert(note.thread_id, note.info);
        }
        if drafts == self.remote_drafts && remote == self.remote {
            return false;
        }
        self.remote_drafts = drafts;
        self.remote = remote;
        self.touch();
        true
    }

    /// The pull request thread `thread_id`, when it is one.
    #[must_use]
    pub fn remote(&self, thread_id: &str) -> Option<&RemoteThreadInfo> {
        self.remote.get(thread_id)
    }

    /// Every pull request thread shown, by id.
    #[must_use]
    pub const fn remote_threads(&self) -> &std::collections::BTreeMap<String, RemoteThreadInfo> {
        &self.remote
    }

    /// Mark the pull request thread `thread_id` resolved or open at once (until the next read).
    pub fn set_remote_resolved(&mut self, thread_id: &str, resolved: bool) {
        if let Some(info) = self.remote.get_mut(thread_id) {
            info.resolved = resolved;
            for draft in &mut self.remote_drafts {
                if draft.thread_id == thread_id {
                    draft.resolved_at_ms = resolved.then_some(draft.created_at_ms);
                }
            }
            self.touch();
        }
    }

    /// Drafts held as replies to pull request threads (sent with the next review).
    #[must_use]
    pub fn held_replies(&self) -> Vec<&DraftComment> {
        self.drafts
            .iter()
            .filter(|draft| draft.anchor.reply_to.is_some())
            .collect()
    }

    /// The imported thread `thread_id`, when it is one.
    #[must_use]
    pub fn external(&self, thread_id: &str) -> Option<&ExternalThread> {
        self.external.get(thread_id)
    }

    /// Imported comments.
    #[must_use]
    pub fn external_drafts(&self) -> &[DraftComment] {
        &self.external_drafts
    }

    /// Imported threads.
    #[must_use]
    pub fn external_count(&self) -> usize {
        self.external.len()
    }

    /// Replace the published anchors with those of `records` (submitted ones).
    pub fn set_published(&mut self, records: &[ReviewPublishRecord]) {
        self.published = records
            .iter()
            .filter(|record| record.submitted)
            .flat_map(|record| record.published_thread_anchors.iter().map(anchor_identity))
            .collect();
        self.touch();
    }

    /// Whether the thread `thread_id` was published.
    #[must_use]
    pub fn is_published(&self, thread_id: &str) -> bool {
        !self.published.is_empty()
            && self
                .drafts
                .iter()
                .find(|draft| draft.thread_id == thread_id)
                .is_some_and(|draft| self.published.contains(&anchor_identity(&draft.anchor)))
    }

    /// The annotations as the document shows them (imported threads included).
    #[must_use]
    pub fn annotations(&self) -> Annotations {
        Annotations {
            drafts: self
                .drafts
                .iter()
                // A held reply shows in its pull request thread, not as a thread of its own.
                .filter(|draft| draft.anchor.reply_to.is_none())
                .chain(&self.external_drafts)
                .chain(&self.remote_drafts)
                .cloned()
                .collect(),
            exchanges: self.exchanges.clone(),
            suggestions: self.suggestions.clone(),
        }
    }

    /// A draft by comment id.
    #[must_use]
    pub fn comment(&self, comment_id: &str) -> Option<&DraftComment> {
        self.drafts
            .iter()
            .chain(&self.external_drafts)
            .chain(&self.remote_drafts)
            .find(|draft| draft.comment_id == comment_id)
    }

    /// Comments of a thread, oldest first.
    #[must_use]
    pub fn thread(&self, thread_id: &str) -> Vec<&DraftComment> {
        let mut comments: Vec<&DraftComment> = if thread_id.starts_with(EXTERNAL_PREFIX) {
            self.external_drafts
                .iter()
                .filter(|draft| draft.thread_id == thread_id)
                .collect()
        } else if thread_id.starts_with(REMOTE_PREFIX) {
            // Its comments, then the replies held for the next review.
            self.remote_drafts
                .iter()
                .filter(|draft| draft.thread_id == thread_id)
                .chain(self.drafts.iter().filter(|draft| {
                    draft
                        .anchor
                        .reply_to
                        .as_deref()
                        .is_some_and(|reply_to| replied_thread(reply_to) == thread_id)
                }))
                .collect()
        } else {
            self.drafts
                .iter()
                .filter(|draft| draft.thread_id == thread_id)
                .collect()
        };
        comments.sort_by(|a, b| {
            (a.created_at_ms, &a.comment_id).cmp(&(b.created_at_ms, &b.comment_id))
        });
        comments
    }

    /// Every thread shown, by id: drafts, imported and pull request threads.
    #[must_use]
    pub fn thread_ids(&self) -> std::collections::BTreeSet<String> {
        self.drafts
            .iter()
            .filter(|draft| draft.anchor.reply_to.is_none())
            .chain(&self.external_drafts)
            .chain(&self.remote_drafts)
            .map(|draft| draft.thread_id.clone())
            .collect()
    }

    /// A thread's comments as viewed state counts them: id, and whether the viewer wrote it
    /// (drafts and held replies are; imported comments are not).
    #[must_use]
    pub fn comment_refs(&self, thread_id: &str) -> Vec<(String, bool)> {
        let remote = self.remote.get(thread_id);
        let external = thread_id.starts_with(EXTERNAL_PREFIX);
        self.thread(thread_id)
            .into_iter()
            .map(|comment| {
                let id = &comment.comment_id;
                let mine = remote.map_or(!external, |info| {
                    info.mine.contains(id) || !id.starts_with(REMOTE_PREFIX)
                });
                (id.clone(), mine)
            })
            .collect()
    }

    /// Whether a thread is resolved: its forge's state, or every comment resolved.
    #[must_use]
    pub fn thread_resolved(&self, thread_id: &str) -> bool {
        if let Some(info) = self.remote.get(thread_id) {
            return info.resolved;
        }
        let comments = self.thread(thread_id);
        !comments.is_empty()
            && comments
                .iter()
                .all(|comment| comment.resolved_at_ms.is_some())
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

    /// Every suggestion.
    #[must_use]
    pub fn suggestions(&self) -> &[ReviewSuggestion] {
        &self.suggestions
    }

    /// Insert or replace a suggestion at once and queue its save (its id is its identity).
    pub fn save_suggestion(&mut self, suggestion: ReviewSuggestion) {
        let previous = if let Some(existing) = self
            .suggestions
            .iter_mut()
            .find(|existing| existing.suggestion_id == suggestion.suggestion_id)
        {
            Some(Box::new(std::mem::replace(existing, suggestion.clone())))
        } else {
            self.suggestions.push(suggestion.clone());
            None
        };
        self.queue.push_back(Mutation::SaveSuggestion {
            suggestion: Box::new(suggestion),
            previous,
        });
        self.touch();
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

    /// Put back the suggestion `id` as it was before a failed save (`None`: it was new).
    fn restore_suggestion(&mut self, id: &str, previous: Option<ReviewSuggestion>) {
        match previous {
            Some(previous) => {
                if let Some(existing) = self
                    .suggestions
                    .iter_mut()
                    .find(|existing| existing.suggestion_id == id)
                {
                    *existing = previous;
                }
            }
            None => self
                .suggestions
                .retain(|existing| existing.suggestion_id != id),
        }
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
            (mutation, MutationResult::Failed(error)) => Some(self.roll_back(mutation, &error)),
            _ => None,
        }
    }

    /// Undo a failed mutation's change. Returns the message.
    fn roll_back(&mut self, mutation: Mutation, error: &str) -> String {
        match mutation {
            Mutation::Save { local, .. } => {
                self.drafts.retain(|draft| draft.comment_id != local);
                format!("could not save the comment: {error}")
            }
            Mutation::Update {
                comment_id,
                previous,
                ..
            } => {
                if let Some(draft) = self
                    .drafts
                    .iter_mut()
                    .find(|draft| draft.comment_id == comment_id)
                {
                    draft.body = previous;
                }
                format!("could not update the comment: {error}")
            }
            Mutation::Delete { comment, index } => {
                let index = index.min(self.drafts.len());
                self.drafts.insert(index, comment);
                format!("could not delete the comment: {error}")
            }
            Mutation::Resolve { previous, .. } => {
                for (comment_id, resolved) in previous {
                    if let Some(draft) = self
                        .drafts
                        .iter_mut()
                        .find(|draft| draft.comment_id == comment_id)
                    {
                        draft.resolved_at_ms = resolved;
                    }
                }
                format!("could not change the thread: {error}")
            }
            Mutation::Classify {
                thread_id,
                previous,
                ..
            } => {
                for draft in self
                    .drafts
                    .iter_mut()
                    .filter(|draft| draft.thread_id == thread_id)
                {
                    (draft.thread_kind, draft.severity) = previous;
                }
                format!("could not change the thread: {error}")
            }
            Mutation::DeleteExchange { exchange } => {
                self.exchanges.push(exchange);
                format!("could not delete the AI exchange: {error}")
            }
            Mutation::SaveSuggestion {
                suggestion,
                previous,
            } => {
                self.restore_suggestion(
                    &suggestion.suggestion_id,
                    previous.map(|previous| *previous),
                );
                format!("could not save the suggestion: {error}")
            }
        }
    }
}

/// A review-level anchor (imported threads that do not map onto the review).
fn review_anchor() -> DraftAnchor {
    DraftAnchor {
        kind: bcode_code_review_models::ReviewAnchorKind::Review,
        file_path: bcode_code_review::document::REVIEW_LEVEL_PATH.to_owned(),
        diff_row: 0,
        start_diff_row: None,
        end_diff_row: None,
        old_start: None,
        old_end: None,
        new_start: None,
        new_end: None,
        old_line: None,
        new_line: None,
        line_kind: bcode_code_review_models::ReviewLineKind::Context,
        is_file_anchor: false,
        surface_id: None,
        source_id: None,
        head_oid: None,
        blob_oid: None,
        reply_to: None,
    }
}

/// An anchor with its revision fields cleared: its identity for comparing anchors across
/// revisions (published anchors, follow-up questions, duplicate suggestions).
#[must_use]
pub fn anchor_identity(anchor: &DraftAnchor) -> DraftAnchor {
    let mut anchor = anchor.clone();
    anchor.head_oid = None;
    anchor.blob_oid = None;
    anchor
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
pub async fn load(services: ReviewServices, scope: NoteScope) -> Result<LoadedNotes, String> {
    let drafts: ListDraftsResponse = services
        .call(ListDraftsRequest {
            repo_path: scope.repo_path.clone(),
            target: scope.target.clone(),
            scope: scope.scope.clone(),
        })
        .await
        .map_err(|error| error.message)?;
    let exchanges: ListReviewAiExchangesResponse = services
        .call(ListReviewAiExchangesRequest {
            repo_path: scope.repo_path.clone(),
            target: scope.target.clone(),
            scope: scope.scope.clone(),
        })
        .await
        .map_err(|error| error.message)?;
    let suggestions: ListReviewSuggestionsResponse = services
        .call(ListReviewSuggestionsRequest {
            repo_path: scope.repo_path.clone(),
            target: scope.target.clone(),
            scope: scope.scope.clone(),
        })
        .await
        .map_err(|error| error.message)?;
    // External imports and publish records belong to durable workspaces.
    let (mut external, mut published) = (Vec::new(), Vec::new());
    if let Some(ReviewScope::Workspace { workspace_id, .. }) = &scope.scope
        && !bcode_code_review::scope::is_transient_scope_id(workspace_id)
    {
        let imports: Result<ListExternalReviewImportsResponse, _> = services
            .call(ListExternalReviewImportsRequest {
                repo_path: scope.repo_path.clone(),
                workspace_id: workspace_id.clone(),
            })
            .await;
        external = imports.map_err(|error| error.message)?.records;
        let records: Result<ListPublishRecordsResponse, _> = services
            .call(ListPublishRecordsRequest {
                repo_path: scope.repo_path.clone(),
                workspace_id: workspace_id.clone(),
            })
            .await;
        published = records.map_err(|error| error.message)?.records;
    }
    Ok(LoadedNotes {
        drafts: drafts.drafts,
        exchanges: exchanges.exchanges,
        suggestions: suggestions.suggestions,
        external,
        published,
    })
}

/// Run one mutation.
pub async fn run(services: ReviewServices, scope: NoteScope, mutation: Mutation) -> MutationResult {
    let NoteScope {
        repo_path,
        target,
        scope,
    } = scope;
    let done = match mutation {
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
            return save(&services, scope, anchor, body, classify)
                .await
                .unwrap_or_else(|error| MutationResult::Failed(error.message));
        }
        Mutation::Update {
            comment_id, body, ..
        } => services
            .call(UpdateDraftRequest {
                repo_path,
                target,
                scope,
                comment_id,
                body,
            })
            .await
            .map(drop),
        Mutation::Delete { comment, .. } => services
            .call(DeleteDraftRequest {
                repo_path,
                target,
                scope,
                comment_id: comment.comment_id,
            })
            .await
            .map(drop),
        Mutation::Resolve {
            anchor, resolved, ..
        } => services
            .call(ResolveThreadRequest {
                repo_path,
                target,
                scope,
                anchor,
                resolved,
            })
            .await
            .map(drop),
        Mutation::Classify {
            anchor,
            kind,
            severity,
            ..
        } => services
            .call(ClassifyThreadRequest {
                repo_path,
                target,
                scope,
                anchor,
                thread_kind: kind,
                severity,
            })
            .await
            .map(drop),
        Mutation::SaveSuggestion { suggestion, .. } => services
            .call(SaveReviewSuggestionRequest {
                repo_path,
                target,
                scope,
                suggestion: *suggestion,
            })
            .await
            .map(drop),
        Mutation::DeleteExchange { exchange } => services
            .call(DeleteReviewAiExchangeRequest {
                repo_path,
                target,
                scope,
                exchange_id: exchange.exchange_id,
            })
            .await
            .map(drop),
    };
    done.map_or_else(
        |error| MutationResult::Failed(error.message),
        |()| MutationResult::Done,
    )
}

/// Move persisted drafts from `from` to `to` (saving a transient review).
///
/// Each draft is saved in `to`, each thread gets its kind, severity, and resolution there, and
/// the draft is then deleted from `from`. Drafts with local ids (not saved yet) are skipped.
///
/// # Errors
///
/// Returns the first failing call's message; drafts moved before it stay moved.
pub async fn move_drafts(
    services: ReviewServices,
    from: NoteScope,
    to: NoteScope,
    mut drafts: Vec<DraftComment>,
) -> Result<(), String> {
    drafts.retain(|draft| !is_local(&draft.comment_id));
    drafts.sort_by_key(|draft| draft.created_at_ms);
    let mut threads = std::collections::BTreeSet::new();
    for draft in drafts {
        let _: SaveDraftResponse = services
            .call(SaveDraftRequest {
                repo_path: to.repo_path.clone(),
                target: to.target.clone(),
                scope: to.scope.clone(),
                anchor: draft.anchor.clone(),
                body: draft.body.clone(),
            })
            .await
            .map_err(|error| error.message)?;
        if threads.insert(draft.thread_id.clone()) {
            let _: ClassifyThreadResponse = services
                .call(ClassifyThreadRequest {
                    repo_path: to.repo_path.clone(),
                    target: to.target.clone(),
                    scope: to.scope.clone(),
                    anchor: draft.anchor.clone(),
                    thread_kind: draft.thread_kind,
                    severity: draft.severity,
                })
                .await
                .map_err(|error| error.message)?;
            if draft.resolved_at_ms.is_some() {
                services
                    .call(ResolveThreadRequest {
                        repo_path: to.repo_path.clone(),
                        target: to.target.clone(),
                        scope: to.scope.clone(),
                        anchor: draft.anchor.clone(),
                        resolved: true,
                    })
                    .await
                    .map_err(|error| error.message)?;
            }
        }
        services
            .call(DeleteDraftRequest {
                repo_path: from.repo_path.clone(),
                target: from.target.clone(),
                scope: from.scope.clone(),
                comment_id: draft.comment_id.clone(),
            })
            .await
            .map_err(|error| error.message)?;
    }
    Ok(())
}

/// Move AI exchanges and suggestions from `from` to `to` (saving a transient review).
///
/// Their ids are unique across scopes (a save of an existing id updates it in its own scope),
/// so each is deleted from `from` first and then saved in `to`; when that save fails it is
/// saved back in `from`.
///
/// # Errors
///
/// Returns the first failing call's message; annotations moved before it stay moved.
pub async fn move_agent_notes(
    services: ReviewServices,
    from: NoteScope,
    to: NoteScope,
    exchanges: Vec<ReviewAiExchange>,
    suggestions: Vec<ReviewSuggestion>,
) -> Result<(), String> {
    use bcode_code_review_models::{DeleteReviewSuggestionRequest, SaveReviewAiExchangeRequest};
    let services = &services;
    let save_exchange = |scope: &NoteScope, exchange: ReviewAiExchange| {
        let request = SaveReviewAiExchangeRequest {
            repo_path: scope.repo_path.clone(),
            target: scope.target.clone(),
            scope: scope.scope.clone(),
            exchange,
        };
        async move { services.call(request).await }
    };
    for exchange in exchanges {
        services
            .call(DeleteReviewAiExchangeRequest {
                repo_path: from.repo_path.clone(),
                target: from.target.clone(),
                scope: from.scope.clone(),
                exchange_id: exchange.exchange_id.clone(),
            })
            .await
            .map_err(|error| error.message)?;
        if let Err(error) = save_exchange(&to, exchange.clone()).await {
            let _ = save_exchange(&from, exchange).await;
            return Err(error.message);
        }
    }
    let save_suggestion = |scope: &NoteScope, suggestion: ReviewSuggestion| {
        let request = SaveReviewSuggestionRequest {
            repo_path: scope.repo_path.clone(),
            target: scope.target.clone(),
            scope: scope.scope.clone(),
            suggestion,
        };
        async move { services.call(request).await }
    };
    for suggestion in suggestions {
        services
            .call(DeleteReviewSuggestionRequest {
                repo_path: from.repo_path.clone(),
                target: from.target.clone(),
                scope: from.scope.clone(),
                suggestion_id: suggestion.suggestion_id.clone(),
            })
            .await
            .map_err(|error| error.message)?;
        if let Err(error) = save_suggestion(&to, suggestion.clone()).await {
            let _ = save_suggestion(&from, suggestion).await;
            return Err(error.message);
        }
    }
    Ok(())
}

/// Save a draft, then give a new thread its kind and severity.
async fn save(
    services: &ReviewServices,
    scope: NoteScope,
    anchor: DraftAnchor,
    body: String,
    classify: Option<(ReviewThreadKind, ReviewThreadSeverity)>,
) -> Result<MutationResult, bcode_code_review::provider::ProviderCallError> {
    let NoteScope {
        repo_path,
        target,
        scope,
    } = scope;
    let saved: SaveDraftResponse = services
        .call(SaveDraftRequest {
            repo_path: repo_path.clone(),
            target: target.clone(),
            scope: scope.clone(),
            anchor: anchor.clone(),
            body,
        })
        .await?;
    if let Some((kind, severity)) = classify {
        let _: ClassifyThreadResponse = services
            .call(ClassifyThreadRequest {
                repo_path,
                target,
                scope,
                anchor,
                thread_kind: kind,
                severity,
            })
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
            reply_to: None,
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
