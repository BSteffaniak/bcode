//! Review operations for agents: what the code review plugin's tools read and change.
//!
//! An agent works on one review, found through the session's binding (a review app binds the
//! sessions it starts, see [`bcode_code_review_models::ReviewAssistantBinding`]) or named by a
//! saved review's id. [`ReviewTools::open`] loads it with the revisions its sources resolved to;
//! the other operations read it (summary, files, diffs, file content, threads) or change it
//! (draft comments, replies, resolution, viewed marks, suggestions) through the same store the
//! review app uses, so the app shows every change as if the reviewer had made it.
//!
//! Every read is bounded: file lists, diff lines, content lines, and thread bodies have limits,
//! thread listings and full threads fit [`MAX_THREADS_OUTPUT_BYTES`] (see [`fit`]), and the
//! result says when it was cut.
//!
//! Threads are the review's own (local drafts) and its pull requests' threads on their forge,
//! read from the snapshot the review app keeps of them (see [`pull_threads`]); the operations
//! that change threads change local ones only.

pub mod fit;
pub mod pull_threads;

pub use fit::MAX_THREADS_OUTPUT_BYTES;
pub use pull_threads::{PullThreadQuery, PullThreadView, PullThreadsView, ThreadOrigin};

use crate::data::DataError;
use crate::document::REVIEW_LEVEL_PATH;
use crate::store::StoreError;
use crate::viewed::{EntryIdentity, ViewedReset, ViewedState};
use crate::{ReviewData, ReviewStore};
use bcode_code_review_models::{
    DraftAnchor, DraftComment, GetReviewAssistantRequest, GetReviewConversationRequest,
    GetReviewWorkspaceRequest, ListDraftsRequest, ListReviewSuggestionsRequest,
    ListReviewWorkspacesRequest, PublishReviewRequest, ResolveReviewSourcesRequest,
    ResolveThreadRequest, ResolvedReviewStack, ResolvedSource, ResolvedSourceMode,
    ReviewAnchorKind, ReviewAssistantLookup, ReviewBlobLinesRequest, ReviewBlobRef,
    ReviewBlobSelector, ReviewConversationSnapshot, ReviewFileHunksRequest, ReviewFileStatus,
    ReviewIndexEntry, ReviewIndexRequest, ReviewLineKind, ReviewPatchIdsRequest, ReviewScope,
    ReviewSourceKind, ReviewSuggestion, ReviewSuggestionStatus, ReviewTarget, ReviewThreadKind,
    ReviewThreadSeverity, ReviewWorkspace, RevisionRef, SaveDraftRequest,
    SaveReviewSuggestionRequest, UpdateReviewWorkspaceRequest,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Diff fingerprint and working-tree content id per `(source index, path)`.
type Fingerprints = BTreeMap<(usize, String), (Option<String>, Option<String>)>;

/// Files listed at most.
pub const MAX_FILES: usize = 2_000;
/// Diff lines returned at most by one call.
pub const MAX_DIFF_LINES: u32 = 2_000;
/// Diff lines returned when the caller does not say.
pub const DEFAULT_DIFF_LINES: u32 = 600;
/// Content lines returned at most by one call.
pub const MAX_CONTENT_LINES: u32 = 2_000;
/// Content lines returned when the caller does not say.
pub const DEFAULT_CONTENT_LINES: u32 = 400;
/// Threads listed at most.
pub const MAX_THREADS: usize = 200;
/// Bytes of one comment body returned at most.
pub const MAX_BODY_BYTES: usize = 4 * 1024;
/// Bytes of a comment, reply, or suggestion an agent may write.
pub const MAX_WRITE_BYTES: usize = 64 * 1024;
/// Index entries read per source at most.
const MAX_INDEX_ENTRIES: usize = 20_000;

/// Error code: the session has no review and none was named.
pub const ERROR_NOT_BOUND: &str = "review_not_bound";
/// Error code: the named review, file, line, or thread does not exist.
pub const ERROR_NOT_FOUND: &str = "not_found";
/// Error code: the arguments cannot be used.
pub const ERROR_INVALID: &str = "invalid_arguments";

/// A failed review operation.
#[derive(Debug, thiserror::Error)]
pub enum AssistantError {
    /// The session is not bound to a review and no review was named.
    #[error("{0}")]
    NotBound(String),
    /// A named review, file, line, or thread does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The arguments cannot be used.
    #[error("{0}")]
    Invalid(String),
    /// Stored review state could not be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The repository could not be read.
    #[error(transparent)]
    Data(#[from] DataError),
}

impl AssistantError {
    /// Stable error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotBound(_) => ERROR_NOT_BOUND,
            Self::NotFound(_) => ERROR_NOT_FOUND,
            Self::Invalid(_) => ERROR_INVALID,
            Self::Store(_) => "store_failed",
            Self::Data(error) => error.code(),
        }
    }
}

/// Which review an operation acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewRef {
    /// The review a session is bound to.
    Session(String),
    /// A saved review, by id.
    Workspace(String),
}

/// A review opened for agent operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenReview {
    /// Repository root.
    pub repo_root: PathBuf,
    /// The review's workspace (with its current viewed state).
    pub workspace: ReviewWorkspace,
    /// Its resolved sources, in review order.
    pub resolved: Vec<ResolvedSource>,
    /// Stacks its sources resolved into.
    pub stacks: Vec<ResolvedReviewStack>,
    /// The session the review was opened for, when it came from a binding.
    pub session_id: Option<String>,
}

impl OpenReview {
    /// Whether the review is unsaved.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        crate::scope::is_transient(&self.workspace)
    }

    fn scope(&self) -> (ReviewTarget, ReviewScope) {
        crate::scope::annotation_scope(&self.workspace)
    }

    /// The record the viewed state is stored in: the workspace itself, or for an unsaved review
    /// its viewed files and marks under its annotation scope id.
    fn viewed_record(&self) -> ReviewWorkspace {
        if !self.is_transient() {
            return self.workspace.clone();
        }
        ReviewWorkspace {
            id: crate::scope::annotation_workspace_id(&self.workspace),
            title: self.workspace.title.clone(),
            repo_root: self.workspace.repo_root.clone(),
            sources: self.workspace.sources.clone(),
            created_at_ms: None,
            updated_at_ms: None,
            viewed_files: self.workspace.viewed_files.clone(),
            archived_at_ms: None,
            viewed_marks: self.workspace.viewed_marks.clone(),
            viewed_threads: self.workspace.viewed_threads.clone(),
            reviewed_revisions: BTreeMap::new(),
            presentation: None,
            stack_snapshots: Vec::new(),
        }
    }
}

/// A saved review of the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewListing {
    /// Review id (pass it as `review`).
    pub id: String,
    /// Title.
    pub title: String,
    /// Source labels.
    pub sources: Vec<String>,
    /// Viewed files.
    pub viewed_files: usize,
    /// Last update, in milliseconds since the Unix epoch.
    pub updated_at_ms: Option<u64>,
}

/// One source of a review summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSummary {
    /// Source id.
    pub id: String,
    /// Label.
    pub label: String,
    /// Base side (`index`, `working_tree`, `empty_tree`, or a commit id).
    pub base: String,
    /// Head side.
    pub head: String,
    /// Whether the source shows files rather than a diff.
    pub browse: bool,
}

/// One layer of a stack in a review summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackSummary {
    /// Source id of the stack.
    pub source_id: String,
    /// Trunk branch.
    pub trunk: String,
    /// Layer labels from the trunk up.
    pub layers: Vec<String>,
}

/// What a review is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummaryView {
    /// Review id (`None` while the review is unsaved).
    pub id: Option<String>,
    /// Title.
    pub title: String,
    /// Repository root.
    pub repo_root: PathBuf,
    /// Sources.
    pub sources: Vec<SourceSummary>,
    /// Stacks.
    pub stacks: Vec<StackSummary>,
    /// Changed files.
    pub files: usize,
    /// Added lines.
    pub additions: u64,
    /// Removed lines.
    pub deletions: u64,
    /// Files marked viewed.
    pub viewed: usize,
    /// Comment threads.
    pub threads: usize,
    /// Threads not resolved.
    pub open_threads: usize,
    /// Pull request threads (as the review app last read them).
    #[serde(default)]
    pub pull_request_threads: usize,
    /// Pull request threads not resolved.
    #[serde(default)]
    pub open_pull_request_threads: usize,
    /// Pending suggestions.
    pub suggestions: usize,
}

/// One file of a review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileView {
    /// Path.
    pub path: String,
    /// The path it was renamed from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// Status.
    pub status: ReviewFileStatus,
    /// Added lines.
    pub additions: u32,
    /// Removed lines.
    pub deletions: u32,
    /// Whether the reviewer marked it viewed (at its current content).
    pub viewed: bool,
    /// Source id, when the review has several sources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Files of a review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesView {
    /// Files in review order.
    pub files: Vec<FileView>,
    /// Files left out by the limit.
    pub omitted: usize,
}

/// Which side of a diff a line number is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The new side (added and unchanged lines).
    #[default]
    New,
    /// The old side (removed lines).
    Old,
}

/// A place in a review: the whole review, a file, or lines of a file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Location {
    /// File path; the whole review when absent.
    #[serde(default)]
    pub path: Option<String>,
    /// First line; the whole file when absent.
    #[serde(default)]
    pub line: Option<u32>,
    /// Last line of a range.
    #[serde(default)]
    pub end_line: Option<u32>,
    /// Side the lines are on.
    #[serde(default)]
    pub side: Side,
    /// Source id, for a file in several sources.
    #[serde(default)]
    pub source: Option<String>,
}

/// A thread of a review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadView {
    /// Thread id.
    pub thread_id: String,
    /// Always [`ThreadOrigin::Local`].
    #[serde(default)]
    pub origin: ThreadOrigin,
    /// Where it is (`path:line`, `path`, or `review`).
    pub location: String,
    /// Kind.
    pub kind: ReviewThreadKind,
    /// Severity.
    pub severity: ReviewThreadSeverity,
    /// Whether it is resolved.
    pub resolved: bool,
    /// Comment bodies, oldest first (the first and the latest when some are left out).
    pub comments: Vec<String>,
    /// Comments left out of `comments` (the oldest replies) to fit the output.
    #[serde(default, skip_serializing_if = "pull_threads::is_zero")]
    pub comments_omitted: usize,
}

/// A pending suggestion of a review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestionView {
    /// Suggestion id.
    pub suggestion_id: String,
    /// Where it is.
    pub location: String,
    /// Body.
    pub body: String,
}

/// Threads and pending suggestions of a review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadsView {
    /// Threads in creation order.
    pub threads: Vec<ThreadView>,
    /// Pending suggestions.
    pub suggestions: Vec<SuggestionView>,
    /// Threads left out (resolved ones when not asked for, and those past the limit or the
    /// output's size).
    pub omitted: usize,
    /// Pending suggestions left out (past the output's size).
    #[serde(default, skip_serializing_if = "pull_threads::is_zero")]
    pub suggestions_omitted: usize,
    /// The pull requests' threads on their forge.
    #[serde(default)]
    pub pull_request_threads: PullThreadsView,
}

/// One thread in full: a local one, or a pull request's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ThreadDetail {
    /// A pull request thread.
    PullRequest(Box<PullThreadView>),
    /// A local thread.
    Local(ThreadView),
}

/// A change an operation made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Written {
    /// What changed, in words.
    pub message: String,
    /// Thread id, for comments and resolutions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Comment or suggestion id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// Review operations over the repository reads and the stored state of one configuration.
pub struct ReviewTools<'a> {
    data: &'a ReviewData,
    store: &'a ReviewStore,
}

impl<'a> ReviewTools<'a> {
    /// Operations reading with `data` and storing with `store`.
    pub const fn new(data: &'a ReviewData, store: &'a ReviewStore) -> Self {
        Self { data, store }
    }

    /// Saved reviews of the repository at `repo_path`, newest first.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Store`] when stored state cannot be read.
    pub fn list_reviews(&self, repo_path: &Path) -> Result<Vec<ReviewListing>, AssistantError> {
        let response = self.store.list_workspaces(&ListReviewWorkspacesRequest {
            repo_path: repo_path.to_path_buf(),
            include_archived: false,
        })?;
        Ok(response
            .workspaces
            .into_iter()
            .map(|workspace| ReviewListing {
                id: workspace.id.clone(),
                title: workspace.title.clone(),
                sources: workspace
                    .sources
                    .iter()
                    .map(|source| source.label.clone())
                    .collect(),
                viewed_files: workspace.viewed_files.len(),
                updated_at_ms: workspace.updated_at_ms,
            })
            .collect())
    }

    /// Open `review` of the repository at `repo_path`.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::NotBound`] when a session has no binding.
    /// * [`AssistantError::NotFound`] when a named review does not exist.
    /// * [`AssistantError::Invalid`] when a review's sources need a review-source provider and
    ///   it was not opened through a binding.
    /// * [`AssistantError::Store`] / [`AssistantError::Data`] when state or the repository
    ///   cannot be read.
    pub fn open(
        &mut self,
        repo_path: &Path,
        review: &ReviewRef,
    ) -> Result<OpenReview, AssistantError> {
        let (mut workspace, resolved, stacks, session_id) = match review {
            ReviewRef::Session(session_id) => {
                let binding = self
                    .store
                    .assistant_binding(&GetReviewAssistantRequest {
                        repo_path: repo_path.to_path_buf(),
                        lookup: ReviewAssistantLookup::Session {
                            session_id: session_id.clone(),
                        },
                    })?
                    .binding
                    .ok_or_else(|| {
                        AssistantError::NotBound(
                            "this session is not attached to a review; pass `review` with a saved \
                             review's id (code_review.list_reviews lists them)"
                                .to_owned(),
                        )
                    })?;
                (
                    binding.workspace,
                    binding.resolved,
                    binding.stacks,
                    Some(session_id.clone()),
                )
            }
            ReviewRef::Workspace(id) => {
                let workspace = self.saved(repo_path, id)?.ok_or_else(|| {
                    AssistantError::NotFound(format!("no saved review has id {id}"))
                })?;
                (workspace, Vec::new(), Vec::new(), None)
            }
        };
        // Durable reviews are read again (their viewed state may have changed); unsaved ones
        // keep their viewed state under their annotation scope id.
        let stored_id = crate::scope::annotation_workspace_id(&workspace);
        if let Some(stored) = self.saved(repo_path, &stored_id)? {
            if crate::scope::is_transient(&workspace) {
                workspace.viewed_files = stored.viewed_files;
                workspace.viewed_marks = stored.viewed_marks;
            } else {
                workspace = stored;
            }
        }
        let (resolved, stacks) = if resolved.is_empty() {
            self.resolve_sources(&workspace)?
        } else {
            (resolved, stacks)
        };
        Ok(OpenReview {
            repo_root: workspace.repo_root.clone(),
            workspace,
            resolved,
            stacks,
            session_id,
        })
    }

    fn saved(&self, repo_path: &Path, id: &str) -> Result<Option<ReviewWorkspace>, AssistantError> {
        Ok(self
            .store
            .get_workspace(GetReviewWorkspaceRequest {
                repo_path: repo_path.to_path_buf(),
                workspace_id: id.to_owned(),
            })?
            .workspace)
    }

    fn resolve_sources(
        &self,
        workspace: &ReviewWorkspace,
    ) -> Result<(Vec<ResolvedSource>, Vec<ResolvedReviewStack>), AssistantError> {
        if workspace.sources.iter().any(|source| {
            source.included && bcode_code_review_models::source_kind_requires_provider(&source.kind)
        }) {
            return Err(AssistantError::Invalid(
                "this review has pull request or stack sources, which only the review app can \
                 resolve; ask from the review's assistant"
                    .to_owned(),
            ));
        }
        let response = self.data.resolve_sources(&ResolveReviewSourcesRequest {
            repo_path: workspace.repo_root.clone(),
            sources: workspace.sources.clone(),
            provider_resolutions: Vec::new(),
        })?;
        if response.sources.is_empty()
            && let Some(diagnostic) = response.diagnostics.first()
        {
            return Err(AssistantError::Invalid(format!(
                "the review's sources do not resolve: {}",
                diagnostic.message
            )));
        }
        Ok((response.sources, response.stacks))
    }

    /// Index entries of every source, in review order, with their source.
    fn entries(
        &self,
        review: &OpenReview,
    ) -> Result<Vec<(usize, ReviewIndexEntry)>, AssistantError> {
        let mut entries = Vec::new();
        for (index, source) in review.resolved.iter().enumerate() {
            let mut cursor = None;
            let mut read = 0;
            loop {
                let page = self.data.index_get(&ReviewIndexRequest {
                    repo_path: review.repo_root.clone(),
                    source: source.clone(),
                    cursor: cursor.take(),
                    limit: None,
                })?;
                read += page.entries.len();
                entries.extend(page.entries.into_iter().map(|entry| (index, entry)));
                match page.next_cursor {
                    Some(next) if read < MAX_INDEX_ENTRIES => cursor = Some(next),
                    _ => break,
                }
            }
        }
        Ok(entries)
    }

    /// What `review` is: its sources, size, and annotation counts.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Store`] / [`AssistantError::Data`] when state or the repository
    ///   cannot be read.
    pub fn summary(&mut self, review: &OpenReview) -> Result<ReviewSummaryView, AssistantError> {
        let entries = self.entries(review)?;
        let threads = self.threads(
            review,
            &PullThreadQuery {
                include_resolved: true,
                ..PullThreadQuery::default()
            },
        )?;
        let pull_threads = self.conversation(review)?;
        let pull_threads = pull_threads
            .as_ref()
            .map_or(&[][..], |snapshot| snapshot.threads.as_slice());
        let sources = review
            .resolved
            .iter()
            .map(|source| SourceSummary {
                id: source.source_id.clone(),
                label: review
                    .workspace
                    .sources
                    .iter()
                    .find(|candidate| candidate.id == source.source_id)
                    .map_or_else(
                        || source.source_id.clone(),
                        |candidate| candidate.label.clone(),
                    ),
                base: revision_label(&source.base),
                head: revision_label(&source.head),
                browse: source.mode == ResolvedSourceMode::Browse,
            })
            .collect();
        let stacks = review
            .stacks
            .iter()
            .map(|stack| StackSummary {
                source_id: stack.source_id.clone(),
                trunk: stack.trunk.clone(),
                layers: stack
                    .layers
                    .iter()
                    .map(|layer| format!("#{} {}", layer.number, layer.title))
                    .collect(),
            })
            .collect();
        Ok(ReviewSummaryView {
            id: (!review.is_transient()).then(|| review.workspace.id.clone()),
            title: review.workspace.title.clone(),
            repo_root: review.repo_root.clone(),
            sources,
            stacks,
            files: entries.len(),
            additions: entries
                .iter()
                .map(|(_, entry)| u64::from(entry.additions))
                .sum(),
            deletions: entries
                .iter()
                .map(|(_, entry)| u64::from(entry.deletions))
                .sum(),
            viewed: review.workspace.viewed_files.len(),
            open_threads: threads
                .threads
                .iter()
                .filter(|thread| !thread.resolved)
                .count(),
            threads: threads.threads.len() + threads.omitted,
            pull_request_threads: pull_threads.len(),
            open_pull_request_threads: pull_threads
                .iter()
                .filter(|record| !record.thread.resolved)
                .count(),
            suggestions: threads.suggestions.len(),
        })
    }

    /// Files of `review` with their status and viewed state.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Data`] when the repository cannot be read.
    pub fn files(
        &mut self,
        review: &OpenReview,
        reset: ViewedReset,
    ) -> Result<FilesView, AssistantError> {
        let entries = self.entries(review)?;
        let fingerprints = self.fingerprints(review, &entries);
        let several = review.resolved.len() > 1;
        let omitted = entries.len().saturating_sub(MAX_FILES);
        let files = entries
            .iter()
            .take(MAX_FILES)
            .map(|(source, entry)| {
                let (patch, content) = fingerprints
                    .get(&(*source, entry.path.clone()))
                    .cloned()
                    .unwrap_or_default();
                let identity = EntryIdentity::from_entry(entry).with_content(content.as_deref());
                let viewed = crate::viewed::viewed_state(
                    &review.workspace,
                    &identity.with_patch(patch.as_deref()),
                    reset,
                ) != ViewedState::Unviewed;
                FileView {
                    path: entry.path.clone(),
                    old_path: entry.old_path.clone().filter(|old| old != &entry.path),
                    status: entry.status,
                    additions: entry.additions,
                    deletions: entry.deletions,
                    viewed,
                    source: several.then(|| review.resolved[*source].source_id.clone()),
                }
            })
            .collect();
        Ok(FilesView { files, omitted })
    }

    /// Diff fingerprints and working-tree content ids of the files that have viewed marks. Files
    /// whose fingerprints cannot be computed are left out (blob identities decide for them).
    fn fingerprints(
        &self,
        review: &OpenReview,
        entries: &[(usize, ReviewIndexEntry)],
    ) -> Fingerprints {
        let marked = |entry: &ReviewIndexEntry| {
            review.workspace.viewed_files.contains(&entry.path)
                || entry
                    .old_path
                    .as_ref()
                    .is_some_and(|old| review.workspace.viewed_files.contains(old))
        };
        let mut out = Fingerprints::new();
        for (index, source) in review.resolved.iter().enumerate() {
            if source.mode == ResolvedSourceMode::Browse {
                continue;
            }
            let paths: Vec<String> = entries
                .iter()
                .filter(|(owner, entry)| *owner == index && marked(entry))
                .map(|(_, entry)| entry.path.clone())
                .collect();
            for chunk in paths.chunks(bcode_code_review_models::REVIEW_PATCH_IDS_MAX_PATHS) {
                let Ok(response) = self.data.patch_ids_get(&ReviewPatchIdsRequest {
                    repo_path: review.repo_root.clone(),
                    base: source.base.clone(),
                    head: source.head.clone(),
                    paths: chunk.to_vec(),
                }) else {
                    // Fingerprints refine viewed state; without them blob identities decide.
                    continue;
                };
                for path in chunk {
                    out.insert(
                        (index, path.clone()),
                        (
                            response.patch_ids.get(path).cloned(),
                            response.content_ids.get(path).cloned(),
                        ),
                    );
                }
            }
        }
        out
    }

    /// The entry of `path` (or a file renamed from it) and its source index.
    fn find_file(
        &self,
        review: &OpenReview,
        path: &str,
        source: Option<&str>,
    ) -> Result<(usize, ReviewIndexEntry), AssistantError> {
        let path = path.trim().trim_start_matches("./");
        let entries = self.entries(review)?;
        entries
            .into_iter()
            .find(|(index, entry)| {
                source.is_none_or(|source| review.resolved[*index].source_id == source)
                    && (entry.path == path || entry.old_path.as_deref() == Some(path))
            })
            .ok_or_else(|| {
                AssistantError::NotFound(format!(
                    "{path} is not a file of this review (code_review.list_files lists them)"
                ))
            })
    }

    /// The diff of one file as numbered lines: `old new marker content`, with hunk headers.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::NotFound`] when the file is not in the review.
    /// * [`AssistantError::Invalid`] for a source that shows files rather than a diff.
    /// * [`AssistantError::Data`] when the repository cannot be read.
    pub fn file_diff(
        &mut self,
        review: &OpenReview,
        path: &str,
        source: Option<&str>,
        context: Option<u32>,
        max_lines: Option<u32>,
    ) -> Result<String, AssistantError> {
        let (index, entry) = self.find_file(review, path, source)?;
        let resolved = &review.resolved[index];
        if resolved.mode == ResolvedSourceMode::Browse {
            return Err(AssistantError::Invalid(format!(
                "{} is browsed, not diffed; read it with code_review.file_content",
                entry.path
            )));
        }
        let max_lines = max_lines
            .unwrap_or(DEFAULT_DIFF_LINES)
            .clamp(1, MAX_DIFF_LINES);
        let hunks = self.data.file_hunks_get(&ReviewFileHunksRequest {
            repo_path: review.repo_root.clone(),
            source: resolved.clone(),
            path: entry.path.clone(),
            old_path: entry.old_path.clone(),
            context_lines: Some(context.unwrap_or(3).min(20)),
            ignore_whitespace: false,
            max_lines: Some(max_lines),
        })?;
        let mut out = format!(
            "{} ({}, +{} -{})\n",
            entry.path,
            status_label(entry.status),
            entry.additions,
            entry.deletions
        );
        if let Some(old) = entry.old_path.as_ref().filter(|old| **old != entry.path) {
            let _ = writeln!(out, "renamed from {old}");
        }
        if hunks.binary {
            out.push_str("binary file\n");
            return Ok(out);
        }
        out.push_str("columns: old line, new line, marker (+ added, - removed), text\n");
        for hunk in &hunks.hunks {
            let _ = writeln!(
                out,
                "@@ -{},{} +{},{} @@{}",
                hunk.old_start,
                hunk.old_count,
                hunk.new_start,
                hunk.new_count,
                hunk.heading
                    .as_deref()
                    .map(|heading| format!(" {heading}"))
                    .unwrap_or_default()
            );
            for line in &hunk.lines {
                let number =
                    |line: Option<u32>| line.map_or_else(String::new, |line| line.to_string());
                let marker = match line.kind {
                    ReviewLineKind::Added => '+',
                    ReviewLineKind::Removed => '-',
                    ReviewLineKind::Context => ' ',
                };
                let _ = writeln!(
                    out,
                    "{:>6} {:>6} {marker}{}",
                    number(line.old_line),
                    number(line.new_line),
                    line.content
                );
            }
        }
        if let Some(truncated) = hunks.truncated {
            let _ = writeln!(
                out,
                "… {} of {} diff lines shown; ask for more with max_lines",
                truncated.shown_lines, truncated.total_lines
            );
        }
        Ok(out)
    }

    /// Lines of one file at the review's base (`Side::Old`) or head (`Side::New`), numbered.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::NotFound`] when the file is not in the review or has no such side.
    /// * [`AssistantError::Data`] when the repository cannot be read.
    pub fn file_content(
        &mut self,
        review: &OpenReview,
        path: &str,
        source: Option<&str>,
        side: Side,
        start_line: Option<u32>,
        count: Option<u32>,
    ) -> Result<String, AssistantError> {
        let (index, entry) = self.find_file(review, path, source)?;
        let resolved = &review.resolved[index];
        let (blob, revision, file) = match side {
            Side::New => (&entry.new_blob, resolved.head.clone(), entry.path.clone()),
            Side::Old => (
                &entry.old_blob,
                resolved.base.clone(),
                entry.old_path.clone().unwrap_or_else(|| entry.path.clone()),
            ),
        };
        let selector = match blob {
            ReviewBlobRef::Absent => {
                return Err(AssistantError::NotFound(format!(
                    "{} has no {} side",
                    entry.path,
                    if side == Side::New { "new" } else { "old" }
                )));
            }
            ReviewBlobRef::Object { oid } => ReviewBlobSelector::Object { oid: oid.clone() },
            ReviewBlobRef::WorkingTree => ReviewBlobSelector::Revision {
                revision,
                path: file,
            },
        };
        let start = start_line.unwrap_or(1).max(1);
        let count = count
            .unwrap_or(DEFAULT_CONTENT_LINES)
            .clamp(1, MAX_CONTENT_LINES);
        let lines = self.data.blob_lines_get(&ReviewBlobLinesRequest {
            repo_path: review.repo_root.clone(),
            blob: selector,
            start_line: start,
            count,
        })?;
        if lines.binary {
            return Ok(format!("{} is binary\n", entry.path));
        }
        let mut out = format!(
            "{} ({} side), lines {}-{} of {}\n",
            entry.path,
            if side == Side::New { "new" } else { "old" },
            lines.start_line,
            lines.start_line
                + u32::try_from(lines.lines.len())
                    .unwrap_or(0)
                    .saturating_sub(1),
            lines.total_lines
        );
        for (offset, line) in lines.lines.iter().enumerate() {
            let number = lines.start_line + u32::try_from(offset).unwrap_or(u32::MAX);
            let _ = writeln!(out, "{number:>6} {line}");
        }
        Ok(out)
    }

    /// Threads (draft comments), pending suggestions, and a page of the pull request threads of
    /// `review`, as `query` selects them (its page applies to the pull request threads).
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Store`] when stored state cannot be read.
    pub fn threads(
        &self,
        review: &OpenReview,
        query: &PullThreadQuery,
    ) -> Result<ThreadsView, AssistantError> {
        let include_resolved = query.include_resolved;
        let drafts: Vec<DraftComment> = self
            .drafts(review)?
            .into_iter()
            .filter(|draft| {
                query
                    .path
                    .as_deref()
                    .is_none_or(|path| draft.anchor.file_path == path)
            })
            .collect();
        let mut order: Vec<String> = Vec::new();
        let mut grouped: BTreeMap<String, Vec<DraftComment>> = BTreeMap::new();
        for draft in drafts {
            if !grouped.contains_key(&draft.thread_id) {
                order.push(draft.thread_id.clone());
            }
            grouped
                .entry(draft.thread_id.clone())
                .or_default()
                .push(draft);
        }
        let mut threads = Vec::new();
        let mut omitted = 0;
        for thread_id in order {
            let Some(mut comments) = grouped.remove(&thread_id) else {
                continue;
            };
            comments.sort_by_key(|comment| (comment.created_at_ms, comment.comment_id.clone()));
            let resolved = comments
                .iter()
                .all(|comment| comment.resolved_at_ms.is_some());
            if (resolved && !include_resolved) || threads.len() >= MAX_THREADS {
                omitted += 1;
                continue;
            }
            let first = &comments[0];
            threads.push(ThreadView {
                thread_id,
                origin: ThreadOrigin::Local,
                location: location_label(&first.anchor),
                kind: first.thread_kind,
                severity: first.severity,
                resolved,
                comments: comments
                    .iter()
                    .map(|comment| bounded(&comment.body, MAX_BODY_BYTES))
                    .collect(),
                comments_omitted: 0,
            });
        }
        let (target, scope) = review.scope();
        let suggestions = self
            .store
            .list_suggestions(&ListReviewSuggestionsRequest {
                repo_path: review.repo_root.clone(),
                target,
                scope: Some(scope),
            })?
            .suggestions
            .into_iter()
            .filter(|suggestion| suggestion.status == ReviewSuggestionStatus::Suggested)
            .map(|suggestion| SuggestionView {
                suggestion_id: suggestion.suggestion_id,
                location: location_label(&suggestion.anchor),
                body: bounded(&suggestion.body, MAX_BODY_BYTES),
            })
            .collect();
        let snapshot = self.conversation(review)?;
        let mut pull_request_threads = pull_threads::page(snapshot.as_ref(), query);
        if snapshot.is_none() && has_pull_requests(review) {
            pull_request_threads.note = Some(pull_threads::UNREAD_NOTE.to_owned());
        }
        let mut view = ThreadsView {
            threads,
            suggestions,
            omitted,
            suggestions_omitted: 0,
            pull_request_threads,
        };
        fit::listing(&mut view, MAX_THREADS_OUTPUT_BYTES);
        Ok(view)
    }

    /// One thread of `review` in full: a local thread (by its id), or a pull request thread
    /// (by the id `code_review.list_threads` gives it) with the diff hunk it was written on.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::NotFound`] for an unknown thread, or a pull request thread while the
    ///   review app has not read the review's pull request threads.
    /// * [`AssistantError::Store`] when stored state cannot be read.
    pub fn thread(
        &self,
        review: &OpenReview,
        thread_id: &str,
    ) -> Result<ThreadDetail, AssistantError> {
        if bcode_code_review_models::is_pull_thread_ref(thread_id) {
            let snapshot = self
                .conversation(review)?
                .ok_or_else(|| AssistantError::NotFound(pull_threads::UNREAD_NOTE.to_owned()))?;
            return pull_threads::find(&snapshot, thread_id)
                .map(|mut view| {
                    fit::pull_thread(&mut view, MAX_THREADS_OUTPUT_BYTES);
                    ThreadDetail::PullRequest(Box::new(view))
                })
                .ok_or_else(|| {
                    AssistantError::NotFound(format!(
                        "no pull request thread {thread_id} in this review (code_review.list_threads lists them)"
                    ))
                });
        }
        let mut comments: Vec<DraftComment> = self
            .drafts(review)?
            .into_iter()
            .filter(|draft| draft.thread_id == thread_id)
            .collect();
        comments.sort_by_key(|comment| (comment.created_at_ms, comment.comment_id.clone()));
        let first = comments.first().ok_or_else(|| {
            AssistantError::NotFound(format!(
                "no thread {thread_id} in this review (code_review.list_threads lists them)"
            ))
        })?;
        let mut view = ThreadView {
            thread_id: thread_id.to_owned(),
            origin: ThreadOrigin::Local,
            location: location_label(&first.anchor),
            kind: first.thread_kind,
            severity: first.severity,
            resolved: comments
                .iter()
                .all(|comment| comment.resolved_at_ms.is_some()),
            comments: comments
                .iter()
                .map(|comment| bounded(&comment.body, pull_threads::MAX_THREAD_BODY_BYTES))
                .collect(),
            comments_omitted: 0,
        };
        fit::local_thread(&mut view, MAX_THREADS_OUTPUT_BYTES);
        Ok(ThreadDetail::Local(view))
    }

    /// The review's pull request threads as the review app last read them.
    fn conversation(
        &self,
        review: &OpenReview,
    ) -> Result<Option<ReviewConversationSnapshot>, AssistantError> {
        let (target, scope) = review.scope();
        Ok(self
            .store
            .conversation(&GetReviewConversationRequest {
                repo_path: review.repo_root.clone(),
                target,
                scope: Some(scope),
            })?
            .snapshot)
    }

    fn drafts(&self, review: &OpenReview) -> Result<Vec<DraftComment>, AssistantError> {
        let (target, scope) = review.scope();
        Ok(self
            .store
            .list_drafts(&ListDraftsRequest {
                repo_path: review.repo_root.clone(),
                target,
                scope: Some(scope),
            })?
            .drafts)
    }

    /// The stored anchor of a comment at `location`: the whole review, a file, or lines, with
    /// the kind of the last line and the source's head and blob, as the review app records them.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::NotFound`] when the file or a line is not in the review.
    /// * [`AssistantError::Invalid`] for an empty or reversed range.
    /// * [`AssistantError::Data`] when the repository cannot be read.
    pub fn anchor(
        &mut self,
        review: &OpenReview,
        location: &Location,
    ) -> Result<DraftAnchor, AssistantError> {
        let mut anchor = DraftAnchor {
            kind: ReviewAnchorKind::Review,
            file_path: REVIEW_LEVEL_PATH.to_owned(),
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
        };
        let Some(path) = location
            .path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
        else {
            return Ok(anchor);
        };
        let (index, entry) = self.find_file(review, path, location.source.as_deref())?;
        let resolved = &review.resolved[index];
        anchor.kind = ReviewAnchorKind::File;
        anchor.file_path.clone_from(&entry.path);
        anchor.source_id = Some(resolved.source_id.clone());
        anchor.head_oid = resolved.head.commit_oid().map(str::to_owned);
        let Some(start) = location.line else {
            return Ok(anchor);
        };
        let end = location.end_line.unwrap_or(start);
        if start == 0 || end < start {
            return Err(AssistantError::Invalid(format!(
                "lines {start}-{end} are not a range of lines (lines start at 1)"
            )));
        }
        let map = self.line_map(review, index, &entry)?;
        let mut lines = Vec::new();
        for line in start..=end.min(start.saturating_add(10_000)) {
            let mapped = map.find(location.side, line).ok_or_else(|| {
                AssistantError::NotFound(format!(
                    "{}: line {line} is not on the file's {} side",
                    entry.path,
                    if location.side == Side::New {
                        "new"
                    } else {
                        "old"
                    }
                ))
            })?;
            lines.push(mapped);
        }
        let range = |pick: fn(&Mapped) -> Option<u32>| {
            let values: Vec<u32> = lines.iter().filter_map(pick).collect();
            Some((*values.iter().min()?, *values.iter().max()?))
        };
        let last = lines.last().copied().unwrap_or_default();
        anchor.kind = ReviewAnchorKind::Range;
        anchor.line_kind = last.kind;
        if let Some((first, last_old)) = range(|line| line.old) {
            anchor.old_start = Some(first);
            anchor.old_end = Some(last_old);
            anchor.old_line = Some(last_old);
        }
        if let Some((first, last_new)) = range(|line| line.new) {
            anchor.new_start = Some(first);
            anchor.new_end = Some(last_new);
            anchor.new_line = Some(last_new);
        }
        anchor.diff_row = u64::from(end);
        let blob = if last.kind == ReviewLineKind::Removed {
            &entry.old_blob
        } else {
            &entry.new_blob
        };
        anchor.blob_oid = blob.oid().map(str::to_owned);
        Ok(anchor)
    }

    /// Line numbers of one file on both sides, from its hunks.
    fn line_map(
        &self,
        review: &OpenReview,
        index: usize,
        entry: &ReviewIndexEntry,
    ) -> Result<LineMap, AssistantError> {
        let resolved = &review.resolved[index];
        if resolved.mode == ResolvedSourceMode::Browse {
            return Ok(LineMap::default());
        }
        let hunks = self.data.file_hunks_get(&ReviewFileHunksRequest {
            repo_path: review.repo_root.clone(),
            source: resolved.clone(),
            path: entry.path.clone(),
            old_path: entry.old_path.clone(),
            context_lines: Some(0),
            ignore_whitespace: false,
            max_lines: None,
        })?;
        let mut map = LineMap {
            exists_old: entry.old_blob != ReviewBlobRef::Absent,
            exists_new: entry.new_blob != ReviewBlobRef::Absent,
            ..LineMap::default()
        };
        for hunk in hunks.hunks {
            map.hunks.push((
                hunk.old_start,
                hunk.old_count,
                hunk.new_start,
                hunk.new_count,
            ));
            for line in hunk.lines {
                map.changed.push(Mapped {
                    old: line.old_line,
                    new: line.new_line,
                    kind: line.kind,
                });
            }
        }
        Ok(map)
    }

    /// Save a draft comment at `location`.
    ///
    /// # Errors
    ///
    /// * See [`Self::anchor`]; [`AssistantError::Invalid`] for an empty or oversized body;
    ///   [`AssistantError::Store`] when it cannot be saved.
    pub fn add_comment(
        &mut self,
        review: &OpenReview,
        location: &Location,
        body: &str,
        classification: Option<(ReviewThreadKind, ReviewThreadSeverity)>,
    ) -> Result<Written, AssistantError> {
        let body = checked_body(body)?;
        let anchor = self.anchor(review, location)?;
        let label = location_label(&anchor);
        let saved = self.save_draft(review, anchor.clone(), body)?;
        if let Some((thread_kind, severity)) = classification {
            let (target, scope) = review.scope();
            self.store
                .classify_thread(bcode_code_review_models::ClassifyThreadRequest {
                    repo_path: review.repo_root.clone(),
                    target,
                    scope: Some(scope),
                    anchor,
                    thread_kind,
                    severity,
                })?;
        }
        Ok(Written {
            message: format!("drafted a comment at {label}"),
            thread_id: Some(saved.thread_id),
            id: Some(saved.comment_id),
        })
    }

    fn save_draft(
        &self,
        review: &OpenReview,
        anchor: DraftAnchor,
        body: String,
    ) -> Result<DraftComment, AssistantError> {
        let (target, scope) = review.scope();
        Ok(self
            .store
            .save_draft(SaveDraftRequest {
                repo_path: review.repo_root.clone(),
                target,
                scope: Some(scope),
                anchor,
                body,
            })?
            .draft)
    }

    fn thread_anchor(
        &self,
        review: &OpenReview,
        thread_id: &str,
    ) -> Result<DraftAnchor, AssistantError> {
        self.drafts(review)?
            .into_iter()
            .find(|draft| draft.thread_id == thread_id)
            .map(|draft| draft.anchor)
            .ok_or_else(|| {
                AssistantError::NotFound(format!(
                    "no thread {thread_id} in this review (code_review.list_threads lists them)"
                ))
            })
    }

    /// Reply to a thread with a draft comment.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::NotFound`] for an unknown thread; [`AssistantError::Invalid`] for an
    ///   empty or oversized body; [`AssistantError::Store`] when it cannot be saved.
    pub fn reply(
        &mut self,
        review: &OpenReview,
        thread_id: &str,
        body: &str,
    ) -> Result<Written, AssistantError> {
        refuse_pull_thread(thread_id, "reply to")?;
        let body = checked_body(body)?;
        let anchor = self.thread_anchor(review, thread_id)?;
        let label = location_label(&anchor);
        let saved = self.save_draft(review, anchor, body)?;
        Ok(Written {
            message: format!("replied in the thread at {label}"),
            thread_id: Some(saved.thread_id),
            id: Some(saved.comment_id),
        })
    }

    /// Resolve or reopen a thread.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::NotFound`] for an unknown thread; [`AssistantError::Store`] when it
    ///   cannot be saved.
    pub fn resolve(
        &mut self,
        review: &OpenReview,
        thread_id: &str,
        resolved: bool,
    ) -> Result<Written, AssistantError> {
        refuse_pull_thread(thread_id, if resolved { "resolve" } else { "reopen" })?;
        let anchor = self.thread_anchor(review, thread_id)?;
        let label = location_label(&anchor);
        let (target, scope) = review.scope();
        let response = self.store.resolve_thread(ResolveThreadRequest {
            repo_path: review.repo_root.clone(),
            target,
            scope: Some(scope),
            anchor,
            resolved,
        })?;
        Ok(Written {
            message: format!(
                "{} the thread at {label}",
                if resolved { "resolved" } else { "reopened" }
            ),
            thread_id: Some(response.thread_id),
            id: None,
        })
    }

    /// Mark files viewed (or not viewed) at their current content.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::NotFound`] when a file is not in the review;
    ///   [`AssistantError::Invalid`] without files; [`AssistantError::Store`] /
    ///   [`AssistantError::Data`] when state or the repository cannot be read or written.
    pub fn mark_viewed(
        &mut self,
        review: &OpenReview,
        paths: &[String],
        viewed: bool,
    ) -> Result<Written, AssistantError> {
        if paths.is_empty() {
            return Err(AssistantError::Invalid("name at least one file".to_owned()));
        }
        let mut files = Vec::new();
        for path in paths {
            files.push(self.find_file(review, path, None)?);
        }
        let fingerprints = self.fingerprints_of(review, &files)?;
        let mut record = review.viewed_record();
        for (index, entry) in &files {
            let (patch, content) = fingerprints
                .get(&(*index, entry.path.clone()))
                .cloned()
                .unwrap_or_default();
            let identity = EntryIdentity::from_entry(entry).with_content(content.as_deref());
            let identity = identity.with_patch(patch.as_deref());
            if viewed {
                crate::viewed::mark_viewed(&mut record, &identity);
            } else {
                crate::viewed::unmark_viewed(&mut record, &identity);
            }
        }
        self.store.update_workspace(UpdateReviewWorkspaceRequest {
            repo_path: review.repo_root.clone(),
            workspace: record,
        })?;
        let names: Vec<&str> = files.iter().map(|(_, entry)| entry.path.as_str()).collect();
        Ok(Written {
            message: format!(
                "marked {} {}",
                names.join(", "),
                if viewed { "viewed" } else { "not viewed" }
            ),
            thread_id: None,
            id: None,
        })
    }

    fn fingerprints_of(
        &self,
        review: &OpenReview,
        files: &[(usize, ReviewIndexEntry)],
    ) -> Result<Fingerprints, AssistantError> {
        let mut out = BTreeMap::new();
        for (index, entry) in files {
            let source = &review.resolved[*index];
            if source.mode == ResolvedSourceMode::Browse {
                continue;
            }
            let response = self.data.patch_ids_get(&ReviewPatchIdsRequest {
                repo_path: review.repo_root.clone(),
                base: source.base.clone(),
                head: source.head.clone(),
                paths: vec![entry.path.clone()],
            })?;
            out.insert(
                (*index, entry.path.clone()),
                (
                    response.patch_ids.get(&entry.path).cloned(),
                    response.content_ids.get(&entry.path).cloned(),
                ),
            );
        }
        Ok(out)
    }

    /// Suggest a change at `location`: a comment the reviewer can accept as a draft, with an
    /// optional replacement for the lines as a `suggestion` block.
    ///
    /// # Errors
    ///
    /// * See [`Self::anchor`]; [`AssistantError::Invalid`] for an empty or oversized suggestion;
    ///   [`AssistantError::Store`] when it cannot be saved.
    pub fn suggest(
        &mut self,
        review: &OpenReview,
        location: &Location,
        body: &str,
        replacement: Option<&str>,
        rationale: Option<&str>,
        now_ms: u64,
    ) -> Result<Written, AssistantError> {
        let mut text = body.trim().to_owned();
        if let Some(replacement) = replacement {
            if location.line.is_none() {
                return Err(AssistantError::Invalid(
                    "a replacement needs the lines it replaces (line, end_line)".to_owned(),
                ));
            }
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str("```suggestion\n");
            text.push_str(replacement.trim_end_matches('\n'));
            text.push_str("\n```");
        }
        let text = checked_body(&text)?;
        let anchor = self.anchor(review, location)?;
        let label = location_label(&anchor);
        let suggestion_id = format!(
            "suggestion-{}-{now_ms}",
            crate::scope::annotation_workspace_id(&review.workspace)
                .chars()
                .rev()
                .take(8)
                .collect::<String>()
        );
        let (target, scope) = review.scope();
        let saved = self
            .store
            .save_suggestion(SaveReviewSuggestionRequest {
                repo_path: review.repo_root.clone(),
                target,
                scope: Some(scope),
                suggestion: ReviewSuggestion {
                    suggestion_id,
                    anchor,
                    body: text,
                    session_id: review.session_id.clone(),
                    status: ReviewSuggestionStatus::Suggested,
                    rationale: rationale
                        .map(str::trim)
                        .filter(|rationale| !rationale.is_empty())
                        .map(|rationale| bounded(rationale, MAX_BODY_BYTES)),
                    created_at_ms: now_ms,
                    updated_at_ms: now_ms,
                },
            })?
            .suggestion;
        Ok(Written {
            message: format!("suggested a change at {label}; the reviewer accepts or rejects it"),
            thread_id: None,
            id: Some(saved.suggestion_id),
        })
    }

    /// Check that `location` exists in the review, for navigation. Returns its label.
    ///
    /// # Errors
    ///
    /// * See [`Self::anchor`].
    pub fn locate(
        &mut self,
        review: &OpenReview,
        location: &Location,
    ) -> Result<String, AssistantError> {
        let anchor = self.anchor(review, location)?;
        Ok(location_label(&anchor))
    }

    /// Preview or submit the review with a built-in publisher (local markdown, a markdown or JSON
    /// file). Returns the publisher's output.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Store`] for an unknown publisher, or when the bundle cannot be built
    ///   or written.
    pub fn publish(
        &mut self,
        review: &OpenReview,
        publisher_id: &str,
        submit: bool,
        options: serde_json::Value,
    ) -> Result<String, AssistantError> {
        let (target, _) = review.scope();
        let mut workspace = review.workspace.clone();
        // An unsaved review's comments are stored under its annotation scope id.
        workspace.id = crate::scope::annotation_workspace_id(&review.workspace);
        let request = PublishReviewRequest {
            repo_path: review.repo_root.clone(),
            target,
            workspace: Some(workspace),
            excluded_thread_anchors: Vec::new(),
            publisher_id: publisher_id.to_owned(),
            options,
        };
        if submit {
            let response = self.store.publish_submit(request)?;
            Ok(format!(
                "{}{}",
                response.message,
                response
                    .output
                    .map(|output| format!("\n{output}"))
                    .unwrap_or_default()
            ))
        } else {
            let response = self.store.publish_preview(request)?;
            Ok(response.preview)
        }
    }
}

/// A file line on both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mapped {
    old: Option<u32>,
    new: Option<u32>,
    kind: ReviewLineKind,
}

impl Default for Mapped {
    fn default() -> Self {
        Self {
            old: None,
            new: None,
            kind: ReviewLineKind::Context,
        }
    }
}

/// Line numbers of a file's sides: the changed lines from its hunks, and unchanged lines by
/// the offset the hunks before them leave.
#[derive(Debug, Default)]
struct LineMap {
    /// `(old_start, old_count, new_start, new_count)` of each hunk.
    hunks: Vec<(u32, u32, u32, u32)>,
    changed: Vec<Mapped>,
    exists_old: bool,
    exists_new: bool,
}

impl LineMap {
    fn find(&self, side: Side, line: u32) -> Option<Mapped> {
        let on_side = |mapped: &&Mapped| match side {
            Side::New => mapped.new == Some(line),
            Side::Old => mapped.old == Some(line),
        };
        if let Some(changed) = self.changed.iter().find(on_side) {
            return Some(*changed);
        }
        if !(match side {
            Side::New => self.exists_new,
            Side::Old => self.exists_old,
        }) {
            return None;
        }
        // An unchanged line: shift by the hunks that end before it on its side.
        let mut delta: i64 = 0;
        for (old_start, old_count, new_start, new_count) in &self.hunks {
            let (start, count) = match side {
                Side::New => (*new_start, *new_count),
                Side::Old => (*old_start, *old_count),
            };
            if count > 0 && (start..start + count).contains(&line) {
                return None;
            }
            // A hunk without lines on this side sits after its start line.
            let before = if count == 0 {
                start < line
            } else {
                start + count - 1 < line
            };
            if before {
                delta += i64::from(*new_count) - i64::from(*old_count);
            }
        }
        let (old, new) = match side {
            Side::New => (i64::from(line) - delta, i64::from(line)),
            Side::Old => (i64::from(line), i64::from(line) + delta),
        };
        let old = u32::try_from(old).ok().filter(|old| *old > 0)?;
        let new = u32::try_from(new).ok().filter(|new| *new > 0)?;
        Some(Mapped {
            old: Some(old),
            new: Some(new),
            kind: ReviewLineKind::Context,
        })
    }
}

/// `path:line`, `path:start-end`, `path`, or `review` for an anchor.
#[must_use]
pub fn location_label(anchor: &DraftAnchor) -> String {
    match anchor.kind {
        ReviewAnchorKind::Review => "the review".to_owned(),
        ReviewAnchorKind::File => anchor.file_path.clone(),
        ReviewAnchorKind::Range => {
            let (start, end, side) =
                if anchor.line_kind == ReviewLineKind::Removed || anchor.new_line.is_none() {
                    (
                        anchor.old_start.or(anchor.old_line),
                        anchor.old_end.or(anchor.old_line),
                        " (old)",
                    )
                } else {
                    (
                        anchor.new_start.or(anchor.new_line),
                        anchor.new_end.or(anchor.new_line),
                        "",
                    )
                };
            match (start, end) {
                (Some(start), Some(end)) if start != end => {
                    format!("{}:{start}-{end}{side}", anchor.file_path)
                }
                (_, Some(line)) | (Some(line), None) => {
                    format!("{}:{line}{side}", anchor.file_path)
                }
                (None, None) => anchor.file_path.clone(),
            }
        }
    }
}

fn revision_label(revision: &RevisionRef) -> String {
    match revision {
        RevisionRef::Commit { oid } => oid.clone(),
        RevisionRef::EmptyTree => "empty_tree".to_owned(),
        RevisionRef::Index => "index".to_owned(),
        RevisionRef::WorkingTree => "working_tree".to_owned(),
    }
}

const fn status_label(status: ReviewFileStatus) -> &'static str {
    match status {
        ReviewFileStatus::Modified => "modified",
        ReviewFileStatus::Added => "added",
        ReviewFileStatus::Deleted => "deleted",
        ReviewFileStatus::Renamed => "renamed",
        ReviewFileStatus::Copied => "copied",
        ReviewFileStatus::Unknown => "changed",
    }
}

/// Whether `review` shows pull requests (whose threads the review app reads).
fn has_pull_requests(review: &OpenReview) -> bool {
    review.workspace.sources.iter().any(|source| {
        matches!(
            source.kind,
            ReviewSourceKind::PullRequest { .. }
                | ReviewSourceKind::StackLayer { .. }
                | ReviewSourceKind::Stack { .. }
        )
    })
}

/// Refuse to `act` on the pull request thread `thread_id`: the tools change local threads only.
fn refuse_pull_thread(thread_id: &str, act: &str) -> Result<(), AssistantError> {
    if bcode_code_review_models::is_pull_thread_ref(thread_id) {
        return Err(AssistantError::Invalid(format!(
            "{thread_id} is a pull request thread on its forge; the review tools cannot {act} it \
             (they change the review's local threads only). The reviewer can do it in the review \
             app (Reply / Resolve on the thread) or on the forge; to note something for them, \
             draft a comment with code_review.add_comment."
        )));
    }
    Ok(())
}

fn checked_body(body: &str) -> Result<String, AssistantError> {
    let body = body.trim();
    if body.is_empty() {
        return Err(AssistantError::Invalid("the text is empty".to_owned()));
    }
    if body.len() > MAX_WRITE_BYTES {
        return Err(AssistantError::Invalid(format!(
            "the text is longer than {MAX_WRITE_BYTES} bytes"
        )));
    }
    Ok(body.to_owned())
}

/// `text` cut to at most `max` bytes on a character boundary, marked when cut.
fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
