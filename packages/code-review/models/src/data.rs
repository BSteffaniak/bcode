//! Lazy, revision-pinned review data contracts (Code Review V2 data layer).
//!
//! These operations are additive to `bcode.code_review/v1`. Each one is bounded: none returns
//! the whole review. A client resolves a workspace's sources to [`ResolvedSource`] values once
//! (`review.sources.resolve`), lists each source's files (`review.index.get`), and then loads
//! hunks per file (`review.file.hunks.get`) and context lines per range
//! (`review.blob.lines.get`) on demand.
//!
//! Paged operations take an opaque `cursor` returned by the previous page and a `limit` that the
//! service clamps to the operation's maximum page size. A cursor is only valid for the same
//! request parameters; when the underlying listing changed between pages, the service rejects the
//! cursor with the `cursor_stale` error code and the client restarts from the first page.

use crate::{
    ResolvedReviewStack, ReviewFileStatus, ReviewHunk, ReviewSource, ReviewSourceDiagnostic,
    ReviewSourceKind, ReviewSourceProviderResolution,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Operation that resolves workspace sources to revision-pinned [`ResolvedSource`] values.
pub const OP_REVIEW_SOURCES_RESOLVE: &str = "review.sources.resolve";
/// Operation that returns a cheap change token for working-tree and index sources.
pub const OP_REVIEW_SOURCES_TOKEN: &str = "review.sources.token";
/// Operation that returns one page of a resolved source's file index.
pub const OP_REVIEW_INDEX_GET: &str = "review.index.get";
/// Operation that returns the parsed hunks of one file of one resolved source.
pub const OP_REVIEW_FILE_HUNKS_GET: &str = "review.file.hunks.get";
/// Operation that returns a line range of one blob.
pub const OP_REVIEW_BLOB_LINES_GET: &str = "review.blob.lines.get";
/// Operation that returns one page of branches and tags matching a query.
pub const OP_REVIEW_REFS_SEARCH: &str = "review.refs.search";
/// Operation that returns one page of the commit log.
pub const OP_REVIEW_COMMITS_PAGE: &str = "review.commits.page";
/// Operation that returns one page of repository paths matching a query.
pub const OP_REVIEW_FILES_SEARCH: &str = "review.files.search";
/// Operation summarizing repository state for review suggestions.
pub const OP_REVIEW_REPO_STATE: &str = "review.repo.state";

/// Default page size for `review.index.get`.
pub const REVIEW_INDEX_DEFAULT_PAGE_SIZE: u32 = 1_000;
/// Maximum page size for `review.index.get`.
pub const REVIEW_INDEX_MAX_PAGE_SIZE: u32 = 5_000;
/// Default page size for the search and log operations.
pub const REVIEW_SEARCH_DEFAULT_PAGE_SIZE: u32 = 50;
/// Maximum page size for the search and log operations.
pub const REVIEW_SEARCH_MAX_PAGE_SIZE: u32 = 500;
/// Default context lines around each hunk for `review.file.hunks.get`.
pub const REVIEW_HUNKS_DEFAULT_CONTEXT: u32 = 3;
/// Maximum context lines around each hunk for `review.file.hunks.get`.
pub const REVIEW_HUNKS_MAX_CONTEXT: u32 = 1_000;
/// Default diff-line budget for `review.file.hunks.get` before the response is truncated.
pub const REVIEW_HUNKS_DEFAULT_MAX_LINES: u32 = 20_000;
/// Maximum diff-line budget for `review.file.hunks.get`.
pub const REVIEW_HUNKS_MAX_LINES: u32 = 200_000;
/// Maximum line count for one `review.blob.lines.get` request.
pub const REVIEW_BLOB_LINES_MAX_COUNT: u32 = 5_000;

/// Error code: a paged cursor no longer matches the listing it was issued for.
pub const REVIEW_ERROR_CURSOR_STALE: &str = "cursor_stale";
/// Error code: a repository-relative path was rejected by path confinement.
pub const REVIEW_ERROR_PATH_REJECTED: &str = "path_rejected";
/// Error code: the requested object, revision, or path does not exist.
pub const REVIEW_ERROR_NOT_FOUND: &str = "not_found";
/// Error code: the requested content exceeds a service bound.
pub const REVIEW_ERROR_TOO_LARGE: &str = "too_large";
/// Error code: the request was malformed or used an unsupported combination.
pub const REVIEW_ERROR_INVALID_REQUEST: &str = "invalid_request";
/// Error code: a git command failed.
pub const REVIEW_ERROR_GIT_FAILED: &str = "git_failed";
/// Diagnostic code: the source needs a review-source provider answer that the request did not
/// carry (no provider plugin is enabled, or the client did not ask one).
pub const REVIEW_DIAGNOSTIC_PROVIDER_REQUIRED: &str = "provider_required";
/// Diagnostic code: the source kind is unknown to this version and was preserved unchanged.
pub const REVIEW_DIAGNOSTIC_UNSUPPORTED_SOURCE: &str = "unsupported_source_kind";
/// Diagnostic code: a source revision could not be resolved.
pub const REVIEW_DIAGNOSTIC_UNRESOLVED_REVISION: &str = "unresolved_revision";

/// One side of a review: a concrete commit, the index, or the working tree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RevisionRef {
    /// A commit, by full object id.
    Commit {
        /// Full commit object id.
        oid: String,
    },
    /// The empty tree: the base side of a root commit or of an unborn branch.
    EmptyTree,
    /// The repository index (staging area).
    Index,
    /// The working tree on disk.
    WorkingTree,
}

impl RevisionRef {
    /// Build a commit revision from a full object id.
    #[must_use]
    pub fn commit(oid: impl Into<String>) -> Self {
        Self::Commit { oid: oid.into() }
    }

    /// Return the commit object id, when this side is a commit.
    #[must_use]
    pub fn commit_oid(&self) -> Option<&str> {
        match self {
            Self::Commit { oid } => Some(oid),
            Self::EmptyTree | Self::Index | Self::WorkingTree => None,
        }
    }

    /// Whether this side's content can change without a new commit (index or working tree).
    #[must_use]
    pub const fn is_live(&self) -> bool {
        matches!(self, Self::Index | Self::WorkingTree)
    }
}

/// How a resolved source is reviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedSourceMode {
    /// Review the difference between `base` and `head`.
    #[default]
    Diff,
    /// Browse whole files at `head` (repository, file, and file-range sources).
    Browse,
}

/// Position of one part of a source that resolves to several revision pairs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSourcePart {
    /// Zero-based part index in source order.
    pub index: u32,
    /// Total parts of this source.
    pub count: u32,
    /// Human-readable part label, such as a short commit id and subject.
    pub label: String,
}

/// Inclusive line range in one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReviewLineInterval {
    /// First line (1-based, inclusive).
    pub start: u32,
    /// Last line (1-based, inclusive).
    pub end: u32,
}

/// A workspace source pinned to concrete revisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSource {
    /// Workspace source id this resolution belongs to.
    pub source_id: String,
    /// Base side.
    pub base: RevisionRef,
    /// Head side.
    pub head: RevisionRef,
    /// Diff or browse mode.
    #[serde(default)]
    pub mode: ResolvedSourceMode,
    /// Repository-relative paths the source is limited to; empty means the whole repository.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Line range a file-range source is limited to.
    #[serde(default)]
    pub line_range: Option<ReviewLineInterval>,
    /// Part position when one source resolves to several revision pairs, such as a
    /// non-contiguous commit selection.
    #[serde(default)]
    pub part: Option<ResolvedSourcePart>,
}

/// Request payload for `review.sources.resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveReviewSourcesRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Workspace sources; only included sources are resolved.
    pub sources: Vec<ReviewSource>,
    /// Provider answers for pull-request, stack-layer, and stack sources, gathered by the client
    /// over `bcode.review_source/v1`. A provider-backed source without one resolves to a
    /// `provider_required` diagnostic.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_resolutions: Vec<ReviewSourceProviderResolution>,
}

/// Response payload for `review.sources.resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveReviewSourcesResponse {
    /// Resolved sources in workspace order. A source may resolve to several parts.
    pub sources: Vec<ResolvedSource>,
    /// Per-source diagnostics, for example `provider_required` for pull-request kinds.
    #[serde(default)]
    pub diagnostics: Vec<ReviewSourceDiagnostic>,
    /// Layer structure of resolved stack and stack-layer sources, for layer labels and
    /// `review.stack.attribution`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stacks: Vec<ResolvedReviewStack>,
}

/// Request payload for `review.sources.token`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourcesTokenRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Whether the token must also change when unstaged working-tree content changes. This adds
    /// a `git status` call; set it only when a working-tree source is open.
    #[serde(default)]
    pub include_working_tree: bool,
}

/// Response payload for `review.sources.token`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourcesTokenResponse {
    /// Opaque change token. Equal tokens mean the head commit, the index, and (when requested)
    /// the working tree did not change observably.
    pub token: String,
    /// Current `HEAD` commit id, or `None` on an unborn branch.
    #[serde(default)]
    pub head_oid: Option<String>,
}

/// Upper bound on the commit count reported by [`ReviewRepoStateResponse::ahead`].
pub const REVIEW_REPO_STATE_MAX_AHEAD: u32 = 10_000;

/// Request payload for `review.repo.state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRepoStateRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
}

/// Changed files and lines of one working-tree state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReviewChangeCount {
    /// Changed files.
    pub files: u32,
    /// Added lines.
    pub additions: u64,
    /// Removed lines.
    pub deletions: u64,
}

impl ReviewChangeCount {
    /// Whether nothing changed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.files == 0
    }
}

/// Response payload for `review.repo.state`: what review suggestions are derived from.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReviewRepoStateResponse {
    /// Checked-out branch, or `None` when `HEAD` is detached.
    #[serde(default)]
    pub branch: Option<String>,
    /// `HEAD` commit, or `None` on an unborn branch.
    #[serde(default)]
    pub head_oid: Option<String>,
    /// Base the `CurrentBranch` source detects: the upstream, else `origin/HEAD`, else the first
    /// existing default-branch candidate other than the current branch.
    #[serde(default)]
    pub base: Option<String>,
    /// Merge base of `base` and `HEAD`.
    #[serde(default)]
    pub merge_base_oid: Option<String>,
    /// Commits on `HEAD` that are not on `base`, capped at [`REVIEW_REPO_STATE_MAX_AHEAD`].
    #[serde(default)]
    pub ahead: Option<u32>,
    /// Unstaged changes (index to working tree; untracked files excluded).
    #[serde(default)]
    pub unstaged: ReviewChangeCount,
    /// Staged changes (`HEAD` to index).
    #[serde(default)]
    pub staged: ReviewChangeCount,
}

/// Blob identity of one side of an index entry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewBlobRef {
    /// A blob object (or a gitlink commit id for submodules).
    Object {
        /// Full object id.
        oid: String,
    },
    /// Working-tree content, which has no object id until it is staged.
    WorkingTree,
    /// The side does not exist (added or deleted files).
    Absent,
}

/// Viewed-mark identity recorded for an absent side.
pub const VIEWED_ABSENT_SIDE: &str = "absent";

impl ReviewBlobRef {
    /// Return the object id, when this side is an object.
    #[must_use]
    pub fn oid(&self) -> Option<&str> {
        match self {
            Self::Object { oid } => Some(oid),
            Self::WorkingTree | Self::Absent => None,
        }
    }

    /// Return the identity recorded by a viewed mark: the object id, [`VIEWED_ABSENT_SIDE`] for
    /// an absent side, and `None` for working-tree content (whose identity is not pinned).
    #[must_use]
    pub fn viewed_identity(&self) -> Option<String> {
        match self {
            Self::Object { oid } => Some(oid.clone()),
            Self::Absent => Some(VIEWED_ABSENT_SIDE.to_string()),
            Self::WorkingTree => None,
        }
    }
}

/// Property of an index entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewIndexFlag {
    /// Git treats the content as binary.
    Binary,
    /// Both sides exist and their modes differ.
    ModeChanged,
    /// Either side is a symbolic link.
    Symlink,
    /// Either side is a submodule (gitlink).
    Submodule,
}

/// One file of a resolved source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewIndexEntry {
    /// Workspace source id.
    pub source_id: String,
    /// Display path: the new path, or the old path for deleted files.
    pub path: String,
    /// Old path for renames and copies.
    #[serde(default)]
    pub old_path: Option<String>,
    /// File status.
    pub status: ReviewFileStatus,
    /// Old-side blob.
    pub old_blob: ReviewBlobRef,
    /// New-side blob.
    pub new_blob: ReviewBlobRef,
    /// Old file mode (octal, such as `100644`), when the side exists.
    #[serde(default)]
    pub old_mode: Option<String>,
    /// New file mode (octal), when the side exists.
    #[serde(default)]
    pub new_mode: Option<String>,
    /// Rename or copy similarity percentage.
    #[serde(default)]
    pub similarity: Option<u8>,
    /// Added lines (zero for binary files).
    pub additions: u32,
    /// Removed lines (zero for binary files).
    pub deletions: u32,
    /// Content and object-type properties of the entry.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub flags: BTreeSet<ReviewIndexFlag>,
}

/// Request payload for `review.index.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewIndexRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Resolved source to list.
    pub source: ResolvedSource,
    /// Cursor from the previous page; `None` for the first page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Page size; clamped to [`REVIEW_INDEX_MAX_PAGE_SIZE`].
    #[serde(default)]
    pub limit: Option<u32>,
}

impl ReviewIndexEntry {
    /// Whether the entry carries `flag`.
    #[must_use]
    pub fn has(&self, flag: ReviewIndexFlag) -> bool {
        self.flags.contains(&flag)
    }
}

/// Response payload for `review.index.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewIndexResponse {
    /// Workspace source id.
    pub source_id: String,
    /// Entries in this page, in git diff order.
    pub entries: Vec<ReviewIndexEntry>,
    /// Cursor for the next page, when more entries remain.
    #[serde(default)]
    pub next_cursor: Option<String>,
    /// Total entries across all pages.
    pub total_entries: u32,
    /// Total added lines across all pages.
    pub total_additions: u64,
    /// Total removed lines across all pages.
    pub total_deletions: u64,
}

/// Request payload for `review.file.hunks.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFileHunksRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Resolved source the file belongs to.
    pub source: ResolvedSource,
    /// File path (new path, or old path for deleted files).
    pub path: String,
    /// Old path for renames and copies.
    #[serde(default)]
    pub old_path: Option<String>,
    /// Context lines around each hunk; defaults to [`REVIEW_HUNKS_DEFAULT_CONTEXT`].
    #[serde(default)]
    pub context_lines: Option<u32>,
    /// Ignore whitespace changes (`git diff -w`).
    #[serde(default)]
    pub ignore_whitespace: bool,
    /// Diff-line budget; defaults to [`REVIEW_HUNKS_DEFAULT_MAX_LINES`].
    #[serde(default)]
    pub max_lines: Option<u32>,
}

/// Truncation marker for a file whose diff exceeds the requested line budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewHunksTruncation {
    /// Diff lines returned.
    pub shown_lines: u32,
    /// Diff lines in the full file diff.
    pub total_lines: u32,
    /// Hunks returned (the last one may be cut short).
    pub shown_hunks: u32,
    /// Hunks in the full file diff.
    pub total_hunks: u32,
}

/// Response payload for `review.file.hunks.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFileHunksResponse {
    /// Workspace source id.
    pub source_id: String,
    /// File path.
    pub path: String,
    /// Parsed hunks with old and new line numbers. Line text excludes the diff marker and any
    /// trailing carriage return.
    pub hunks: Vec<ReviewHunk>,
    /// Whether git treats the content as binary (no hunks are returned).
    #[serde(default)]
    pub binary: bool,
    /// Present when the hunks were cut at the line budget.
    #[serde(default)]
    pub truncated: Option<ReviewHunksTruncation>,
    /// Stable patch identity: a SHA-256 over the file's changed lines in order (`+`/`-` marker
    /// plus text, without line numbers or context) and any mode change. It is computed over the
    /// whole diff even when truncated, and is `None` for binary files and for
    /// whitespace-ignoring requests.
    #[serde(default)]
    pub patch_id: Option<String>,
}

/// Which blob a `review.blob.lines.get` request reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewBlobSelector {
    /// A blob object by id.
    Object {
        /// Full or unambiguous object id.
        oid: String,
    },
    /// The file at `path` in `revision` (commit, index, or working tree).
    Revision {
        /// Revision to read.
        revision: RevisionRef,
        /// Repository-relative path.
        path: String,
    },
}

/// Request payload for `review.blob.lines.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewBlobLinesRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Blob to read.
    pub blob: ReviewBlobSelector,
    /// First line to return (1-based).
    pub start_line: u32,
    /// Lines to return; clamped to [`REVIEW_BLOB_LINES_MAX_COUNT`].
    pub count: u32,
}

/// Response payload for `review.blob.lines.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewBlobLinesResponse {
    /// First returned line (1-based).
    pub start_line: u32,
    /// Returned lines without line terminators or trailing carriage returns. Invalid UTF-8 is
    /// replaced.
    pub lines: Vec<String>,
    /// Total lines in the blob.
    pub total_lines: u32,
    /// Whether the content looks binary; no lines are returned for binary content.
    #[serde(default)]
    pub binary: bool,
}

/// Kind of a repository reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewRefKind {
    /// Local branch (`refs/heads`).
    Branch,
    /// Remote-tracking branch (`refs/remotes`).
    RemoteBranch,
    /// Tag (`refs/tags`).
    Tag,
}

/// One reference returned by `review.refs.search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRefEntry {
    /// Short name, such as `main` or `origin/main`.
    pub name: String,
    /// Full reference name, such as `refs/heads/main`.
    pub full_name: String,
    /// Reference kind.
    pub kind: ReviewRefKind,
    /// Object id the reference points at (the commit for annotated tags).
    pub oid: String,
    /// Subject of the referenced commit.
    #[serde(default)]
    pub subject: String,
    /// Committer timestamp in seconds since the Unix epoch, when known.
    #[serde(default)]
    pub committed_at_s: Option<i64>,
    /// Whether this is the checked-out branch.
    #[serde(default)]
    pub is_head: bool,
}

/// Request payload for `review.refs.search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRefsSearchRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Case-insensitive fuzzy query; empty matches every reference.
    #[serde(default)]
    pub query: String,
    /// Reference kinds to include; empty includes all kinds.
    #[serde(default)]
    pub kinds: Vec<ReviewRefKind>,
    /// Cursor from the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Page size; clamped to [`REVIEW_SEARCH_MAX_PAGE_SIZE`].
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Response payload for `review.refs.search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRefsSearchResponse {
    /// Matching references, best match first.
    pub refs: Vec<ReviewRefEntry>,
    /// Cursor for the next page.
    #[serde(default)]
    pub next_cursor: Option<String>,
    /// Total matches across all pages.
    pub total_matches: u32,
}

/// One commit returned by `review.commits.page`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCommitEntry {
    /// Full commit id.
    pub oid: String,
    /// Abbreviated commit id.
    pub short_oid: String,
    /// Subject line.
    pub subject: String,
    /// Author name.
    pub author_name: String,
    /// Author email.
    pub author_email: String,
    /// Author timestamp in seconds since the Unix epoch.
    pub authored_at_s: i64,
    /// Reference decorations, such as `HEAD -> main` or `tag: v1`.
    #[serde(default)]
    pub refs: Vec<String>,
    /// Parent commit ids.
    #[serde(default)]
    pub parents: Vec<String>,
}

/// Request payload for `review.commits.page`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCommitsPageRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Revision to walk from; defaults to `HEAD`. Ignored when `cursor` is set.
    #[serde(default)]
    pub rev: Option<String>,
    /// Case-insensitive fixed-string filter on commit messages.
    #[serde(default)]
    pub query: Option<String>,
    /// Cursor from the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Page size; clamped to [`REVIEW_SEARCH_MAX_PAGE_SIZE`].
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Response payload for `review.commits.page`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCommitsPageResponse {
    /// Commits, newest first.
    pub commits: Vec<ReviewCommitEntry>,
    /// Cursor for the next page. Pages stay anchored to the commit the first page started from.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// One path returned by `review.files.search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFileSearchEntry {
    /// Repository-relative path.
    pub path: String,
}

/// Request payload for `review.files.search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFilesSearchRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Case-insensitive fuzzy query; empty matches every path.
    #[serde(default)]
    pub query: String,
    /// Revision whose tree is searched; defaults to the index (tracked files).
    #[serde(default)]
    pub revision: Option<RevisionRef>,
    /// Cursor from the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Page size; clamped to [`REVIEW_SEARCH_MAX_PAGE_SIZE`].
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Response payload for `review.files.search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFilesSearchResponse {
    /// Matching paths, best match first.
    pub files: Vec<ReviewFileSearchEntry>,
    /// Cursor for the next page.
    #[serde(default)]
    pub next_cursor: Option<String>,
    /// Total matches across all pages.
    pub total_matches: u32,
}

/// Operation that compares a previously reviewed commit with the current one (re-review).
pub const OP_REVIEW_REVISIONS_COMPARE: &str = "review.revisions.compare";
/// Operation that returns the patch ids of several files of one revision pair (re-review).
pub const OP_REVIEW_PATCH_IDS_GET: &str = "review.patch_ids.get";
/// Maximum paths one `review.patch_ids.get` request may name.
pub const REVIEW_PATCH_IDS_MAX_PATHS: usize = 500;
/// Upper bound on [`ReviewRevisionsCompareResponse::commits_ahead`].
pub const REVIEW_REVISIONS_MAX_AHEAD: u32 = 10_000;

/// Request payload for `review.revisions.compare`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRevisionsCompareRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Commit reviewed last time (full object id).
    pub old_oid: String,
    /// Commit the source resolves to now (full object id).
    pub new_oid: String,
}

/// Response payload for `review.revisions.compare`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRevisionsCompareResponse {
    /// Whether the old commit still exists locally.
    pub old_exists: bool,
    /// Whether the old commit is an ancestor of the new one (a fast-forward update).
    pub fast_forward: bool,
    /// Commits reachable from the new commit but not the old one, capped at
    /// [`REVIEW_REVISIONS_MAX_AHEAD`]; `None` when the old commit is missing.
    #[serde(default)]
    pub commits_ahead: Option<u32>,
}

/// Request payload for `review.patch_ids.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPatchIdsRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Base side.
    pub base: RevisionRef,
    /// Head side.
    pub head: RevisionRef,
    /// Repository-relative paths (at most [`REVIEW_PATCH_IDS_MAX_PATHS`]); a renamed file is
    /// named by either path.
    pub paths: Vec<String>,
}

/// Response payload for `review.patch_ids.get`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReviewPatchIdsResponse {
    /// Diff fingerprint per requested path that has a textual difference (the same identity
    /// `review.file.hunks.get` reports). Paths without a difference, and binary files, are
    /// absent.
    pub patch_ids: std::collections::BTreeMap<String, String>,
    /// When the head is the working tree: the object id each requested regular file's working
    /// copy would have if added (`git hash-object`), so its content can be identified without
    /// reading hunks. Missing files and symbolic links are absent.
    #[serde(default)]
    pub content_ids: std::collections::BTreeMap<String, String>,
}

/// Whether a source kind is resolved by a review-source provider plugin.
#[must_use]
pub const fn source_kind_requires_provider(kind: &ReviewSourceKind) -> bool {
    matches!(
        kind,
        ReviewSourceKind::PullRequest { .. }
            | ReviewSourceKind::StackLayer { .. }
            | ReviewSourceKind::Stack { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_refs_round_trip_with_kind_tags() {
        let refs = [
            RevisionRef::commit("abc"),
            RevisionRef::EmptyTree,
            RevisionRef::Index,
            RevisionRef::WorkingTree,
        ];
        let json = serde_json::to_string(&refs).unwrap();
        assert_eq!(
            json,
            r#"[{"kind":"commit","oid":"abc"},{"kind":"empty_tree"},{"kind":"index"},{"kind":"working_tree"}]"#
        );
        let decoded: Vec<RevisionRef> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, refs);
        assert!(RevisionRef::Index.is_live() && !RevisionRef::commit("a").is_live());
    }

    #[test]
    fn resolved_source_defaults_optional_fields() {
        let decoded: ResolvedSource = serde_json::from_str(
            r#"{"source_id":"s","base":{"kind":"index"},"head":{"kind":"working_tree"}}"#,
        )
        .unwrap();
        assert_eq!(decoded.mode, ResolvedSourceMode::Diff);
        assert!(decoded.paths.is_empty() && decoded.part.is_none());
    }

    #[test]
    fn blob_viewed_identity_distinguishes_absent_from_unknown() {
        assert_eq!(
            ReviewBlobRef::Object { oid: "o".into() }.viewed_identity(),
            Some("o".to_string())
        );
        assert_eq!(
            ReviewBlobRef::Absent.viewed_identity(),
            Some(VIEWED_ABSENT_SIDE.to_string())
        );
        assert_eq!(ReviewBlobRef::WorkingTree.viewed_identity(), None);
    }
}
