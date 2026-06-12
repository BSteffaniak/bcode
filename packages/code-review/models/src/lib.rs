#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared code review models for Bcode.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Code review plugin service interface.
pub const CODE_REVIEW_SERVICE_INTERFACE_ID: &str = "bcode.code_review/v1";
/// Review publisher service interface.
pub const REVIEW_PUBLISHER_INTERFACE_ID: &str = "bcode.review_publisher/v1";

/// Operation that returns a provider-neutral review bundle.
pub const OP_REVIEW_BUNDLE_GET: &str = "review.bundle.get";
/// Operation that lists review publishers.
pub const OP_REVIEW_PUBLISHERS_LIST: &str = "review.publishers.list";
/// Operation that previews a review publish operation.
pub const OP_REVIEW_PUBLISH_PREVIEW: &str = "review.publish.preview";
/// Operation that submits a review publish operation.
pub const OP_REVIEW_PUBLISH_SUBMIT: &str = "review.publish.submit";
/// Operation that saves publish history for an externally handled publisher.
pub const OP_REVIEW_PUBLISH_RECORD_SAVE: &str = "review.publish.record.save";
/// Operation that lists durable review workspaces.
pub const OP_REVIEW_WORKSPACE_LIST: &str = "review.workspace.list";
/// Operation that creates a durable review workspace.
pub const OP_REVIEW_WORKSPACE_CREATE: &str = "review.workspace.create";
/// Operation that fetches a durable review workspace.
pub const OP_REVIEW_WORKSPACE_GET: &str = "review.workspace.get";
/// Operation that updates a durable review workspace.
pub const OP_REVIEW_WORKSPACE_UPDATE: &str = "review.workspace.update";
/// Operation that archives a durable review workspace.
pub const OP_REVIEW_WORKSPACE_ARCHIVE: &str = "review.workspace.archive";
/// Operation that materializes review workspace sources into reviewable surfaces.
pub const OP_REVIEW_WORKSPACE_MATERIALIZE: &str = "review.workspace.materialize";
/// Operation that returns repository file content for review browsing.
pub const OP_REVIEW_REPO_FILE_GET: &str = "review.repo.file.get";
/// Operation that returns an external publisher manifest.
pub const OP_REVIEW_PUBLISHER_MANIFEST: &str = "review.publisher.manifest";
/// Operation that previews an external publisher request.
pub const OP_REVIEW_PUBLISHER_PREVIEW: &str = "review.publisher.preview";
/// Operation that submits an external publisher request.
pub const OP_REVIEW_PUBLISHER_SUBMIT: &str = "review.publisher.submit";

/// Supported local Git review target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewTarget {
    /// Review unstaged working-tree changes.
    WorkingTreeUnstaged,
    /// Review staged index changes.
    IndexStaged,
    /// Review both staged and unstaged changes.
    WorkingTreeAndIndex,
    /// Review the last commit.
    LastCommit,
    /// Review an explicit commit range.
    CommitRange {
        /// Base revision.
        base: String,
        /// Head revision.
        head: String,
        /// Whether to use merge-base `...` semantics.
        #[serde(default)]
        merge_base: bool,
    },
    /// Review a branch comparison.
    BranchCompare {
        /// Base branch.
        base_branch: String,
        /// Head branch.
        head_branch: String,
        /// Whether to use merge-base `...` semantics.
        #[serde(default = "default_true")]
        merge_base: bool,
    },
    /// Review the repository as browsable read-only files.
    Repository,
}

/// Durable review workspace assembled from one or more review sources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewWorkspace {
    /// Stable workspace id.
    pub id: String,
    /// Human-readable review title.
    pub title: String,
    /// Repository path for the workspace.
    pub repo_root: PathBuf,
    /// Sources intentionally included in the review.
    pub sources: Vec<ReviewSource>,
    /// Created timestamp in milliseconds since Unix epoch, when known.
    #[serde(default)]
    pub created_at_ms: Option<u64>,
    /// Updated timestamp in milliseconds since Unix epoch, when known.
    #[serde(default)]
    pub updated_at_ms: Option<u64>,
    /// Review file display paths marked viewed.
    #[serde(default)]
    pub viewed_files: BTreeSet<String>,
    /// Archived timestamp in milliseconds since Unix epoch, when archived.
    #[serde(default)]
    pub archived_at_ms: Option<u64>,
}

impl ReviewWorkspace {
    /// Create a transient workspace from an entry-point review target.
    #[must_use]
    pub fn from_target(repo_root: PathBuf, target: ReviewTarget) -> Self {
        let source_kind = ReviewSourceKind::from(target);
        let title = source_kind.label();
        Self {
            id: "transient-review-workspace".to_string(),
            title: title.clone(),
            repo_root,
            sources: vec![ReviewSource {
                id: "source-1".to_string(),
                kind: source_kind,
                label: title,
                included: true,
            }],
            created_at_ms: None,
            updated_at_ms: None,
            viewed_files: BTreeSet::new(),
            archived_at_ms: None,
        }
    }
}

/// One source of reviewable content in a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSource {
    /// Stable source id within the workspace.
    pub id: String,
    /// Source kind.
    pub kind: ReviewSourceKind,
    /// Human-readable source label.
    pub label: String,
    /// Whether this source is included in the review output.
    pub included: bool,
}

/// Supported source kinds for building a review workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewSourceKind {
    /// Unstaged working-tree changes.
    WorkingTreeUnstaged,
    /// Staged index changes.
    IndexStaged,
    /// Staged and unstaged working-tree changes together.
    WorkingTreeAndIndex,
    /// Last commit reachable from `HEAD`.
    LastCommit,
    /// Specific commit.
    Commit {
        /// Commit revision.
        rev: String,
    },
    /// Explicit commit range.
    CommitRange {
        /// Base revision.
        base: String,
        /// Head revision.
        head: String,
        /// Whether to use merge-base semantics.
        #[serde(default)]
        merge_base: bool,
    },
    /// Branch comparison.
    BranchCompare {
        /// Base branch.
        base_branch: String,
        /// Head branch.
        head_branch: String,
        /// Whether to use merge-base semantics.
        #[serde(default = "default_true")]
        merge_base: bool,
    },
    /// Specific repository file.
    File {
        /// Repository-relative file path.
        path: String,
    },
    /// Specific repository file range.
    FileRange {
        /// Repository-relative file path.
        path: String,
        /// Start line.
        start: u32,
        /// End line.
        end: u32,
    },
    /// Full repository browser context.
    Repository,
}

impl ReviewSourceKind {
    /// Return a human-readable source label.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::WorkingTreeUnstaged => "Unstaged changes".to_string(),
            Self::IndexStaged => "Staged changes".to_string(),
            Self::WorkingTreeAndIndex => "Working tree and index".to_string(),
            Self::LastCommit => "Last commit".to_string(),
            Self::Commit { rev } => format!("Commit {rev}"),
            Self::CommitRange {
                base,
                head,
                merge_base,
            } => {
                let separator = if *merge_base { "..." } else { ".." };
                format!("Range {base}{separator}{head}")
            }
            Self::BranchCompare {
                base_branch,
                head_branch,
                merge_base,
            } => {
                let separator = if *merge_base { "..." } else { ".." };
                format!("Compare {base_branch}{separator}{head_branch}")
            }
            Self::File { path } => format!("File {path}"),
            Self::FileRange { path, start, end } => format!("File {path}:{start}-{end}"),
            Self::Repository => "Repository browser".to_string(),
        }
    }
}

impl From<ReviewTarget> for ReviewSourceKind {
    fn from(target: ReviewTarget) -> Self {
        match target {
            ReviewTarget::WorkingTreeUnstaged => Self::WorkingTreeUnstaged,
            ReviewTarget::IndexStaged => Self::IndexStaged,
            ReviewTarget::WorkingTreeAndIndex => Self::WorkingTreeAndIndex,
            ReviewTarget::LastCommit => Self::LastCommit,
            ReviewTarget::CommitRange {
                base,
                head,
                merge_base,
            } => Self::CommitRange {
                base,
                head,
                merge_base,
            },
            ReviewTarget::BranchCompare {
                base_branch,
                head_branch,
                merge_base,
            } => Self::BranchCompare {
                base_branch,
                head_branch,
                merge_base,
            },
            ReviewTarget::Repository => Self::Repository,
        }
    }
}

/// Normalized review surface kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSurfaceKind {
    /// Diff surface.
    Diff,
    /// Full-file surface.
    File,
}

/// Normalized reviewable surface produced by workspace sources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSurface {
    /// Stable surface id.
    pub id: String,
    /// Source id that produced this surface.
    pub source_id: String,
    /// Repository-relative path.
    pub path: String,
    /// Surface kind.
    pub kind: ReviewSurfaceKind,
    /// Materialized review file, when this is a diff surface.
    #[serde(default)]
    pub file: Option<ReviewFile>,
}

/// Severity for a source materialization diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSourceDiagnosticSeverity {
    /// Informational diagnostic.
    Info,
    /// Warning diagnostic.
    Warning,
    /// Error diagnostic.
    Error,
}

/// Diagnostic produced while materializing one review source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourceDiagnostic {
    /// Source id that produced this diagnostic.
    pub source_id: String,
    /// Diagnostic severity.
    pub severity: ReviewSourceDiagnosticSeverity,
    /// Stable diagnostic code.
    pub code: String,
    /// Human-readable diagnostic message.
    pub message: String,
}

/// Repository commit available for source pickers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRepositoryCommit {
    /// Full commit revision.
    pub rev: String,
    /// Short commit revision.
    pub short_rev: String,
    /// Commit subject.
    pub subject: String,
}

/// Materialized workspace review data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewWorkspaceMaterialization {
    /// Source workspace.
    pub workspace: ReviewWorkspace,
    /// Reviewable surfaces produced from included sources.
    pub surfaces: Vec<ReviewSurface>,
    /// Diagnostics produced while materializing sources.
    #[serde(default)]
    pub diagnostics: Vec<ReviewSourceDiagnostic>,
    /// Repository file paths available for source/file pickers.
    #[serde(default)]
    pub repository_files: Vec<String>,
    /// Repository branch names available for source pickers.
    #[serde(default)]
    pub repository_branches: Vec<String>,
    /// Recent repository commits available for source pickers.
    #[serde(default)]
    pub repository_commits: Vec<ReviewRepositoryCommit>,
    /// Total added lines across diff surfaces.
    pub additions: u32,
    /// Total removed lines across diff surfaces.
    pub deletions: u32,
}

/// Request payload for `review.workspace.materialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializeReviewWorkspaceRequest {
    /// Repository path where the workspace lives.
    pub repo_path: PathBuf,
    /// Workspace to materialize.
    pub workspace: ReviewWorkspace,
}

/// Response payload for `review.workspace.materialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializeReviewWorkspaceResponse {
    /// Materialized workspace review data.
    pub materialization: ReviewWorkspaceMaterialization,
}

/// Request payload for `review.workspace.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReviewWorkspacesRequest {
    /// Repository path whose review workspaces should be listed.
    pub repo_path: PathBuf,
    /// Whether archived workspaces should be included.
    #[serde(default)]
    pub include_archived: bool,
}

/// Publish history metadata for a review workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPublishRecord {
    /// Publish record id.
    pub id: String,
    /// Workspace id, when the publish was tied to a durable workspace.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Review bundle id/key that was published.
    pub review_id: String,
    /// Publisher id.
    pub publisher_id: String,
    /// Whether submission happened.
    pub submitted: bool,
    /// Output location, when available.
    #[serde(default)]
    pub output: Option<String>,
    /// Human-readable result message.
    pub message: String,
    /// Record creation timestamp.
    pub created_at_ms: u64,
}

/// Summary metadata for a review workspace picker row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewWorkspaceListItem {
    /// Review workspace.
    pub workspace: ReviewWorkspace,
    /// Number of draft threads associated with this workspace.
    #[serde(default)]
    pub thread_count: usize,
    /// Number of draft comments associated with this workspace.
    #[serde(default)]
    pub draft_count: usize,
    /// Most recent publish record associated with this workspace.
    #[serde(default)]
    pub last_publish: Option<ReviewPublishRecord>,
}

/// Response payload for `review.workspace.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReviewWorkspacesResponse {
    /// Matching review workspaces.
    pub workspaces: Vec<ReviewWorkspace>,
    /// Matching review workspaces with picker metadata.
    #[serde(default)]
    pub items: Vec<ReviewWorkspaceListItem>,
}

/// Request payload for `review.workspace.create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateReviewWorkspaceRequest {
    /// Repository path where the review workspace should be created.
    pub repo_path: PathBuf,
    /// Optional workspace title.
    #[serde(default)]
    pub title: Option<String>,
    /// Initial workspace sources.
    #[serde(default)]
    pub sources: Vec<ReviewSource>,
}

/// Response payload for `review.workspace.create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateReviewWorkspaceResponse {
    /// Created review workspace.
    pub workspace: ReviewWorkspace,
}

/// Request payload for `review.workspace.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReviewWorkspaceRequest {
    /// Repository path where the workspace lives.
    pub repo_path: PathBuf,
    /// Workspace id.
    pub workspace_id: String,
}

/// Response payload for `review.workspace.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReviewWorkspaceResponse {
    /// Requested workspace, when found.
    pub workspace: Option<ReviewWorkspace>,
}

/// Request payload for `review.workspace.update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateReviewWorkspaceRequest {
    /// Repository path where the workspace lives.
    pub repo_path: PathBuf,
    /// Updated workspace.
    pub workspace: ReviewWorkspace,
}

/// Response payload for `review.workspace.update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateReviewWorkspaceResponse {
    /// Updated workspace.
    pub workspace: ReviewWorkspace,
}

/// Request payload for `review.workspace.archive`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveReviewWorkspaceRequest {
    /// Repository path where the workspace lives.
    pub repo_path: PathBuf,
    /// Workspace id.
    pub workspace_id: String,
}

/// Response payload for `review.workspace.archive`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveReviewWorkspaceResponse {
    /// Whether a workspace was archived.
    pub archived: bool,
}

/// Stable review scope used for draft persistence and publishing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewScope {
    /// Legacy target-scoped review.
    Target {
        /// Review target.
        target: ReviewTarget,
    },
    /// Durable workspace-scoped review.
    Workspace {
        /// Workspace id.
        workspace_id: String,
        /// Fallback target for provider operations.
        target: ReviewTarget,
    },
}

impl ReviewScope {
    /// Return the target associated with this scope.
    #[must_use]
    pub const fn target(&self) -> &ReviewTarget {
        match self {
            Self::Target { target } | Self::Workspace { target, .. } => target,
        }
    }
}

/// Request payload for `draft.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListDraftsRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Local Git target whose drafts should be listed.
    pub target: ReviewTarget,
    /// Review scope, when using durable workspace-scoped drafts.
    #[serde(default)]
    pub scope: Option<ReviewScope>,
}

/// Request payload for `draft.save`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveDraftRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Review scope, when using durable workspace-scoped drafts.
    #[serde(default)]
    pub scope: Option<ReviewScope>,
    /// Draft anchor.
    pub anchor: DraftAnchor,
    /// Markdown body.
    pub body: String,
}

/// Request payload for `draft.delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteDraftRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Review scope, when using durable workspace-scoped drafts.
    #[serde(default)]
    pub scope: Option<ReviewScope>,
    /// Comment id to delete.
    pub comment_id: String,
}

/// Request payload for `draft.update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateDraftRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Review scope, when using durable workspace-scoped drafts.
    #[serde(default)]
    pub scope: Option<ReviewScope>,
    /// Comment id to update.
    pub comment_id: String,
    /// Markdown body.
    pub body: String,
}

/// Request payload for `thread.link_session`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkThreadSessionRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target for the thread.
    pub target: ReviewTarget,
    /// Review scope, when using durable workspace-scoped drafts.
    #[serde(default)]
    pub scope: Option<ReviewScope>,
    /// Thread anchor.
    pub anchor: DraftAnchor,
    /// Bcode session id.
    pub session_id: String,
}

/// Request payload for `review.thread.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReviewThreadRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Review scope, when using durable workspace-scoped drafts.
    #[serde(default)]
    pub scope: Option<ReviewScope>,
    /// Thread id to fetch, if known.
    pub thread_id: Option<String>,
    /// Thread anchor to fetch, if thread id is not known.
    pub anchor: Option<DraftAnchor>,
}

/// Request payload for `review.diff.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReviewDiffRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// File path to fetch, or all files when absent.
    pub file_path: Option<String>,
}

/// Response payload for `draft.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListDraftsResponse {
    /// Draft comments.
    pub drafts: Vec<DraftComment>,
}

/// Response payload for `draft.save`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveDraftResponse {
    /// Saved draft comment.
    pub draft: DraftComment,
}

/// Response payload for `draft.delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteDraftResponse {
    /// Whether a persisted draft was deleted.
    pub deleted: bool,
}

/// Response payload for `draft.update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateDraftResponse {
    /// Whether a persisted draft was updated.
    pub updated: bool,
    /// Updated timestamp in milliseconds since Unix epoch, when updated.
    pub updated_at_ms: Option<u64>,
}

/// Response payload for `thread.link_session`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkThreadSessionResponse {
    /// Linked thread id.
    pub thread_id: String,
}

/// Request payload for `thread.resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveThreadRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Review scope, when using durable workspace-scoped threads.
    #[serde(default)]
    pub scope: Option<ReviewScope>,
    /// Thread anchor.
    pub anchor: DraftAnchor,
    /// Whether the thread should be resolved.
    pub resolved: bool,
}

/// Response payload for `thread.resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveThreadResponse {
    /// Updated thread id.
    pub thread_id: String,
    /// Resolution timestamp in milliseconds since Unix epoch, when resolved.
    pub resolved_at_ms: Option<u64>,
}

/// Request payload for review context operations scoped to a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewContextRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
}

/// Review thread anchor scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAnchorKind {
    /// Thread applies to the entire review.
    Review,
    /// Thread applies to a whole file.
    File,
    /// Thread applies to a rendered line or range.
    #[default]
    Range,
}

/// Persisted draft anchor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftAnchor {
    /// Anchor scope.
    #[serde(default)]
    pub kind: ReviewAnchorKind,
    /// File path in the review.
    pub file_path: String,
    /// Rendered diff row.
    pub diff_row: u64,
    /// Start rendered diff row for range comments.
    #[serde(default)]
    pub start_diff_row: Option<u64>,
    /// End rendered diff row for range comments.
    #[serde(default)]
    pub end_diff_row: Option<u64>,
    /// Start line on old side, when known.
    #[serde(default)]
    pub old_start: Option<u32>,
    /// End line on old side, when known.
    #[serde(default)]
    pub old_end: Option<u32>,
    /// Start line on new side, when known.
    #[serde(default)]
    pub new_start: Option<u32>,
    /// End line on new side, when known.
    #[serde(default)]
    pub new_end: Option<u32>,
    /// Old line for single-line anchors, when known.
    #[serde(default)]
    pub old_line: Option<u32>,
    /// New line for single-line anchors, when known.
    #[serde(default)]
    pub new_line: Option<u32>,
    /// Anchor line kind.
    pub line_kind: ReviewLineKind,
    /// Whether this anchor points at a file surface line rather than a diff row.
    #[serde(default)]
    pub is_file_anchor: bool,
    /// Surface id for normalized mixed-surface anchors.
    #[serde(default)]
    pub surface_id: Option<String>,
    /// Source id for normalized mixed-surface anchors.
    #[serde(default)]
    pub source_id: Option<String>,
}

/// Persisted draft comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftComment {
    /// Stable comment id.
    pub comment_id: String,
    /// Stable thread id for comments sharing an anchor.
    pub thread_id: String,
    /// Review anchor.
    pub anchor: DraftAnchor,
    /// Markdown comment body.
    pub body: String,
    /// Creation timestamp in milliseconds since Unix epoch.
    pub created_at_ms: u64,
    /// Last update timestamp in milliseconds since Unix epoch.
    pub updated_at_ms: u64,
    /// Linked Bcode session id, when present.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Resolution timestamp in milliseconds since Unix epoch, when resolved.
    #[serde(default)]
    pub resolved_at_ms: Option<u64>,
}

/// File status in a review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFileStatus {
    /// Modified file.
    Modified,
    /// Added file.
    Added,
    /// Deleted file.
    Deleted,
    /// Renamed file.
    Renamed,
    /// Copied file.
    Copied,
    /// Unknown or unsupported status.
    Unknown,
}

/// Compact file summary for review context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFileSummary {
    /// Display path.
    pub path: String,
    /// File status.
    pub status: ReviewFileStatus,
    /// Added lines.
    pub additions: u32,
    /// Removed lines.
    pub deletions: u32,
    /// Hunk count.
    pub hunks: usize,
    /// Whether Git reported a binary patch.
    pub is_binary: bool,
}

/// Diff line kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewLineKind {
    /// Context line.
    Context,
    /// Added line.
    Added,
    /// Removed line.
    Removed,
}

/// Unified diff hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewHunk {
    /// Old start line.
    pub old_start: u32,
    /// Old line count.
    pub old_count: u32,
    /// New start line.
    pub new_start: u32,
    /// New line count.
    pub new_count: u32,
    /// Optional hunk heading.
    pub heading: Option<String>,
    /// Diff lines in this hunk.
    pub lines: Vec<ReviewLine>,
}

/// A line in a unified diff hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewLine {
    /// Line kind.
    pub kind: ReviewLineKind,
    /// Old file line number, when present.
    pub old_line: Option<u32>,
    /// New file line number, when present.
    pub new_line: Option<u32>,
    /// Line content without the leading unified diff marker.
    pub content: String,
}

/// Request payload for repository file browsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryFileRequest {
    /// Repository path where the file should be read.
    pub repo_path: PathBuf,
    /// Repository-relative file path.
    pub file_path: String,
}

/// Response payload for repository file browsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryFileResponse {
    /// Repository-relative file path.
    pub file_path: String,
    /// UTF-8 file content when available.
    pub content: Option<String>,
    /// File size in bytes.
    pub size_bytes: u64,
    /// File modification timestamp in milliseconds since Unix epoch, when available.
    #[serde(default)]
    pub mtime_ms: Option<u64>,
    /// Whether the file appears binary.
    pub is_binary: bool,
    /// Optional unavailable reason.
    #[serde(default)]
    pub unavailable_reason: Option<String>,
}

/// Parsed review file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFile {
    /// Old path for deleted/renamed files.
    pub old_path: Option<String>,
    /// New path for added/modified/renamed files.
    pub new_path: Option<String>,
    /// File status.
    pub status: ReviewFileStatus,
    /// Added lines.
    pub additions: u32,
    /// Removed lines.
    pub deletions: u32,
    /// Parsed hunks.
    pub hunks: Vec<ReviewHunk>,
    /// Whether Git reported a binary patch.
    pub is_binary: bool,
}

impl ReviewFile {
    /// Return display path for the file.
    #[must_use]
    pub fn display_path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("<unknown>")
    }
}

/// A provider-neutral selected review line reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewBundleLine {
    /// File path.
    pub file_path: String,
    /// Line kind.
    pub kind: ReviewLineKind,
    /// Old file line number, when present.
    pub old_line: Option<u32>,
    /// New file line number, when present.
    pub new_line: Option<u32>,
    /// Rendered diff row.
    pub diff_row: u64,
    /// Line content without diff marker.
    pub content: String,
    /// Surface id that owns this selected line, when known.
    #[serde(default)]
    pub surface_id: Option<String>,
    /// Source id that owns this selected line, when known.
    #[serde(default)]
    pub source_id: Option<String>,
}

/// Provider-neutral review bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewBundle {
    /// Stable review id for this repository and target.
    pub review_id: String,
    /// Human-readable review title.
    pub title: String,
    /// Repository root.
    pub repo_root: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Files in review order.
    pub files: Vec<ReviewFileSummary>,
    /// Materialized review surfaces in review order.
    #[serde(default)]
    pub surfaces: Vec<ReviewSurface>,
    /// Review threads.
    pub threads: Vec<ReviewBundleThread>,
    /// Generated timestamp in milliseconds since Unix epoch.
    pub generated_at_ms: u64,
}

/// Provider-neutral review thread bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewBundleThread {
    /// Thread id.
    pub thread_id: String,
    /// Thread anchor.
    pub anchor: DraftAnchor,
    /// Draft comments.
    pub comments: Vec<DraftComment>,
    /// Linked Bcode session id, when present.
    pub session_id: Option<String>,
    /// Resolution timestamp in milliseconds since Unix epoch, when resolved.
    #[serde(default)]
    pub resolved_at_ms: Option<u64>,
    /// Structured selected diff lines.
    #[serde(default)]
    pub selected_lines: Vec<ReviewBundleLine>,
    /// Selected diff lines.
    pub selected_diff_lines: Vec<String>,
    /// Hunk context.
    pub hunk_context: Vec<String>,
}

/// Review publisher capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReviewPublisherCapabilities {
    /// Whether preview is supported.
    pub preview: bool,
    /// Whether submit is supported.
    pub submit: bool,
    /// Whether existing output can be updated.
    pub update_existing: bool,
    /// Whether threaded comments are supported.
    pub supports_threads: bool,
    /// Whether range anchors are supported.
    pub supports_ranges: bool,
    /// Whether inline comments are supported.
    pub supports_inline_comments: bool,
    /// Whether a summary comment is supported.
    pub supports_summary_comment: bool,
}

/// External publisher route metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPublisherRoute {
    /// Plugin id for external publisher.
    pub plugin_id: String,
    /// Service interface id.
    pub interface_id: String,
}

/// Review publisher manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPublisherManifest {
    /// Publisher id.
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Human-readable description.
    pub description: String,
    /// Publisher capabilities.
    pub capabilities: ReviewPublisherCapabilities,
    /// JSON-schema-like option description.
    pub options_schema: serde_json::Value,
    /// Optional external plugin route.
    #[serde(default)]
    pub route: Option<ReviewPublisherRoute>,
}

/// Response payload for publisher list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReviewPublishersResponse {
    /// Available publishers.
    pub publishers: Vec<ReviewPublisherManifest>,
}

/// Request payload for built-in publish operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishReviewRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Review workspace, when publishing a durable mixed-source workspace.
    #[serde(default)]
    pub workspace: Option<ReviewWorkspace>,
    /// Publisher id.
    pub publisher_id: String,
    /// Publisher options.
    #[serde(default)]
    pub options: serde_json::Value,
}

/// Request payload for recording an external publish result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavePublishRecordRequest {
    /// Review workspace that was published.
    pub workspace: ReviewWorkspace,
    /// Review bundle id/key that was published.
    pub review_id: String,
    /// Publisher id.
    pub publisher_id: String,
    /// Whether submission happened.
    pub submitted: bool,
    /// Output location, when available.
    #[serde(default)]
    pub output: Option<String>,
    /// Human-readable result message.
    pub message: String,
}

/// Response payload for recording an external publish result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavePublishRecordResponse {
    /// Saved publish record.
    pub record: ReviewPublishRecord,
}

/// Request payload for external publisher operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalPublishReviewRequest {
    /// Provider-neutral review bundle.
    pub bundle: ReviewBundle,
    /// Publisher options.
    #[serde(default)]
    pub options: serde_json::Value,
}

/// Response payload for publish preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishReviewPreviewResponse {
    /// Publisher id.
    pub publisher_id: String,
    /// Preview content.
    pub preview: String,
}

/// Response payload for publish submit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishReviewResponse {
    /// Publisher id.
    pub publisher_id: String,
    /// Whether submission happened.
    pub submitted: bool,
    /// Output location, when available.
    pub output: Option<String>,
    /// Human-readable result message.
    pub message: String,
}

const fn default_true() -> bool {
    true
}
