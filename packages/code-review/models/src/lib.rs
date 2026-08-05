#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared code review models for Bcode.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Code review plugin service interface.
pub const CODE_REVIEW_SERVICE_INTERFACE_ID: &str = "bcode.code_review/v1";
/// Review publisher service interface.
pub const REVIEW_PUBLISHER_INTERFACE_ID: &str = "bcode.review_publisher/v1";
/// External review importer service interface.
pub const REVIEW_IMPORTER_INTERFACE_ID: &str = "bcode.review_importer/v1";

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
/// Operation that returns publish records for a durable workspace.
pub const OP_REVIEW_PUBLISH_RECORDS_LIST: &str = "review.publish.records.list";
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
/// Operation that lists durable AI-suggested review comments.
pub const OP_REVIEW_SUGGESTIONS_LIST: &str = "review.suggestions.list";
/// Operation that creates or updates a durable AI-suggested review comment.
pub const OP_REVIEW_SUGGESTION_SAVE: &str = "review.suggestion.save";
/// Operation that lists durable, non-publishable review AI exchanges.
pub const OP_REVIEW_AI_EXCHANGES_LIST: &str = "review.ai_exchanges.list";
/// Operation that creates or updates a durable, non-publishable review AI exchange.
pub const OP_REVIEW_AI_EXCHANGE_SAVE: &str = "review.ai_exchange.save";
/// Operation that explicitly deletes a durable review AI exchange.
pub const OP_REVIEW_AI_EXCHANGE_DELETE: &str = "review.ai_exchange.delete";
/// Current persisted schema version for review AI exchanges.
pub const REVIEW_AI_EXCHANGE_SCHEMA_VERSION: u16 = 1;
/// Operation that saves a provider-neutral external review import snapshot.
pub const OP_REVIEW_EXTERNAL_IMPORT_SAVE: &str = "review.external_import.save";
/// Operation that lists saved external review import snapshots.
pub const OP_REVIEW_EXTERNAL_IMPORTS_LIST: &str = "review.external_imports.list";
/// Operation that returns an external publisher manifest.
pub const OP_REVIEW_PUBLISHER_MANIFEST: &str = "review.publisher.manifest";
/// Operation that previews an external publisher request.
pub const OP_REVIEW_PUBLISHER_PREVIEW: &str = "review.publisher.preview";
/// Operation that submits an external publisher request.
pub const OP_REVIEW_PUBLISHER_SUBMIT: &str = "review.publisher.submit";
/// Operation that returns an external review importer manifest.
pub const OP_REVIEW_IMPORTER_MANIFEST: &str = "review.importer.manifest";
/// Operation that imports external review state without mutating it.
pub const OP_REVIEW_IMPORTER_IMPORT: &str = "review.importer.import";

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

pub const REVIEW_WORKSPACE_PRESENTATION_SCHEMA_VERSION: u16 = 1;

/// Durable Code Review-owned semantic presentation state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewWorkspacePresentationState {
    /// Presentation schema version.
    pub schema_version: u16,
    /// Stable selected source or file path, when known.
    #[serde(default)]
    pub selected_path: Option<String>,
    /// Stable selected thread identity, when known.
    #[serde(default)]
    pub selected_thread_key: Option<String>,
    /// Semantic selected line within the source, when known.
    #[serde(default)]
    pub selected_line: Option<u32>,
    /// Plugin-owned sidebar mode identifier.
    #[serde(default)]
    pub sidebar_mode: String,
    /// Whether the sidebar is visible.
    #[serde(default = "default_true")]
    pub sidebar_visible: bool,
    /// Plugin-owned thread filter identifier.
    #[serde(default)]
    pub thread_filter: String,
    /// Whether resolved threads are shown.
    #[serde(default = "default_true")]
    pub show_resolved_threads: bool,
    /// Whether viewed files are hidden.
    #[serde(default)]
    pub hide_viewed_files: bool,
    /// Stable collapsed thread identities.
    #[serde(default)]
    pub collapsed_thread_keys: BTreeSet<String>,
    /// Stable expanded linked-session answer identities.
    #[serde(default)]
    pub expanded_agent_answer_keys: BTreeSet<String>,
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
    /// Stable semantic presentation state for useful workspace reopen.
    #[serde(default)]
    pub presentation_state: Option<ReviewWorkspacePresentationState>,
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
            presentation_state: None,
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

/// Request payload for listing publish records for a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPublishRecordsRequest {
    /// Repository path where the workspace lives.
    pub repo_path: PathBuf,
    /// Durable workspace id.
    pub workspace_id: String,
}

/// Response payload for listing publish records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPublishRecordsResponse {
    /// Publish records in creation order.
    pub records: Vec<ReviewPublishRecord>,
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
    /// Provider-neutral review thread anchors included in this publish.
    #[serde(default)]
    pub published_thread_anchors: Vec<DraftAnchor>,
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

/// Request payload for `review.ai_exchanges.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReviewAiExchangesRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Review scope, when using durable workspace-scoped exchanges.
    #[serde(default)]
    pub scope: Option<ReviewScope>,
}

/// Request payload for `review.ai_exchange.delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteReviewAiExchangeRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Review scope, when using durable workspace-scoped exchanges.
    #[serde(default)]
    pub scope: Option<ReviewScope>,
    /// Stable exchange identity to delete.
    pub exchange_id: String,
}

/// Response payload for `review.ai_exchange.delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteReviewAiExchangeResponse {
    /// Whether a persisted exchange was deleted.
    pub deleted: bool,
}

/// Request payload for `review.ai_exchange.save`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveReviewAiExchangeRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Review scope, when using durable workspace-scoped exchanges.
    #[serde(default)]
    pub scope: Option<ReviewScope>,
    /// Exchange to create or update.
    pub exchange: ReviewAiExchange,
}

/// Response payload for `review.ai_exchanges.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReviewAiExchangesResponse {
    /// Persisted, non-publishable AI exchanges.
    pub exchanges: Vec<ReviewAiExchange>,
}

/// Response payload for `review.ai_exchange.save`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveReviewAiExchangeResponse {
    /// Persisted exchange.
    pub exchange: ReviewAiExchange,
}

/// Durable lifecycle for one reviewer-authored AI exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAiExchangeStatus {
    /// The question is durable but no linked session has been established yet.
    Pending,
    /// The exchange is linked to a Bcode session.
    Linked,
    /// The latest attempt to establish or continue the linked session failed.
    Failed,
}

/// Provider-neutral durable metadata for a non-publishable review AI exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAiExchange {
    /// Persisted schema version.
    pub schema_version: u16,
    /// Stable exchange identity.
    pub exchange_id: String,
    /// Review anchor that owns the exchange.
    pub anchor: DraftAnchor,
    /// Reviewer-authored question shown in the review conversation.
    pub question: String,
    /// Linked Bcode session id, when established.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Durable exchange lifecycle.
    pub status: ReviewAiExchangeStatus,
    /// Normalized failure message for the latest failed attempt.
    #[serde(default)]
    pub error: Option<String>,
    /// Creation timestamp in milliseconds since Unix epoch.
    pub created_at_ms: u64,
    /// Last update timestamp in milliseconds since Unix epoch.
    pub updated_at_ms: u64,
}

/// Request payload for `review.suggestions.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReviewSuggestionsRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Review scope, when using durable workspace-scoped suggestions.
    #[serde(default)]
    pub scope: Option<ReviewScope>,
}

/// Request payload for `review.suggestion.save`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveReviewSuggestionRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Review scope, when using durable workspace-scoped suggestions.
    #[serde(default)]
    pub scope: Option<ReviewScope>,
    /// Suggestion to create or update.
    pub suggestion: ReviewSuggestion,
}

/// Response payload for `review.suggestions.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReviewSuggestionsResponse {
    /// Persisted suggestions.
    pub suggestions: Vec<ReviewSuggestion>,
}

/// Response payload for `review.suggestion.save`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveReviewSuggestionResponse {
    /// Persisted suggestion.
    pub suggestion: ReviewSuggestion,
}

/// Durable AI-suggested review comment lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSuggestionStatus {
    /// Suggested but not yet accepted or rejected.
    Suggested,
    /// A refinement request is in progress.
    Refining,
    /// Accepted into a normal draft comment.
    Accepted,
    /// Rejected by the reviewer and retained for audit/undo visibility.
    Rejected,
}

/// Provider-neutral durable AI-suggested review comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSuggestion {
    /// Stable suggestion id.
    pub suggestion_id: String,
    /// Review anchor.
    pub anchor: DraftAnchor,
    /// Suggested Markdown body.
    pub body: String,
    /// Linked Bcode session id that produced the suggestion.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Suggestion lifecycle state.
    pub status: ReviewSuggestionStatus,
    /// Optional short rationale or provenance.
    #[serde(default)]
    pub rationale: Option<String>,
    /// Creation timestamp in milliseconds since Unix epoch.
    pub created_at_ms: u64,
    /// Last update timestamp in milliseconds since Unix epoch.
    pub updated_at_ms: u64,
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

/// Local review thread kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReviewThreadKind {
    /// General note.
    #[default]
    Note,
    /// Question to answer before finishing review.
    Question,
    /// Follow-up task.
    Todo,
    /// Review finding.
    Finding,
    /// Review summary.
    Summary,
}

/// Local review thread severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReviewThreadSeverity {
    /// Informational thread.
    #[default]
    Info,
    /// Minor nit.
    Nit,
    /// Warning that should be considered before publishing.
    Warning,
    /// Blocking issue.
    Blocker,
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
    /// Thread kind.
    #[serde(default)]
    pub thread_kind: ReviewThreadKind,
    /// Thread severity.
    #[serde(default)]
    pub severity: ReviewThreadSeverity,
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
    /// Review thread anchors excluded by the reviewer from this publish.
    #[serde(default)]
    pub excluded_thread_anchors: Vec<DraftAnchor>,
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
    /// Provider-neutral review thread anchors included in this publish.
    #[serde(default)]
    pub published_thread_anchors: Vec<DraftAnchor>,
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

/// Provider-neutral state of an externally hosted review thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalReviewThreadState {
    /// Thread is active remotely.
    Open,
    /// Thread was resolved remotely.
    Resolved,
    /// Provider reports that the thread's diff context is outdated.
    Outdated,
}

/// Confidence of mapping an external comment to the current local review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalAnchorMappingStatus {
    /// The provider supplied enough current information for an exact mapping.
    Exact,
    /// The anchor was mapped heuristically and should be reviewed.
    Approximate,
    /// The external anchor cannot be mapped to the current local review.
    Unmappable,
}

/// Provider-neutral identity for an external review object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalReviewIdentity {
    /// Provider id, such as `github`.
    pub provider_id: String,
    /// Provider-owned opaque identifier.
    pub external_id: String,
    /// Browser URL, when supplied by the provider.
    #[serde(default)]
    pub url: Option<String>,
}

/// Provider-neutral mapping from external diff coordinates to a local anchor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalReviewAnchorMapping {
    /// Mapping result.
    pub status: ExternalAnchorMappingStatus,
    /// Local anchor when mapping succeeded.
    #[serde(default)]
    pub anchor: Option<DraftAnchor>,
    /// Human-readable mapping explanation.
    #[serde(default)]
    pub message: Option<String>,
}

/// A provider-neutral externally hosted review comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalReviewComment {
    /// Stable provider identity for the comment.
    pub identity: ExternalReviewIdentity,
    /// Stable provider identity for the author, when available.
    #[serde(default)]
    pub author: Option<ExternalReviewIdentity>,
    /// Author display label.
    #[serde(default)]
    pub author_label: Option<String>,
    /// Markdown comment body.
    pub body: String,
    /// Creation timestamp in milliseconds since Unix epoch, when available.
    #[serde(default)]
    pub created_at_ms: Option<u64>,
    /// Last update timestamp in milliseconds since Unix epoch, when available.
    #[serde(default)]
    pub updated_at_ms: Option<u64>,
}

/// A provider-neutral externally hosted review thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalReviewThread {
    /// Stable provider identity for the thread.
    pub identity: ExternalReviewIdentity,
    /// Current remote state.
    pub state: ExternalReviewThreadState,
    /// Mapping to the current local review.
    pub mapping: ExternalReviewAnchorMapping,
    /// Comments ordered from oldest to newest.
    pub comments: Vec<ExternalReviewComment>,
}

/// External review importer manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewImporterManifest {
    /// Stable importer id.
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Human-readable description.
    pub description: String,
    /// JSON-schema-like option description.
    pub options_schema: serde_json::Value,
}

/// Read-only external review import request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalReviewImportRequest {
    /// Repository root used for provider discovery.
    pub repo_root: PathBuf,
    /// Current local review files used for anchor mapping.
    #[serde(default)]
    pub files: Vec<ReviewFile>,
    /// Provider options such as repository and pull request.
    #[serde(default)]
    pub options: serde_json::Value,
}

/// Read-only external review import response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalReviewImportResponse {
    /// Importer that produced this response.
    pub importer_id: String,
    /// Imported provider-neutral threads.
    pub threads: Vec<ExternalReviewThread>,
    /// Non-fatal discovery or mapping warnings.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Durable provider-neutral external review import snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalReviewImportRecord {
    /// Durable workspace id.
    pub workspace_id: String,
    /// Importer that produced this snapshot.
    pub importer_id: String,
    /// Imported provider-neutral threads.
    pub threads: Vec<ExternalReviewThread>,
    /// Non-fatal discovery or mapping warnings.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Snapshot update time in milliseconds since Unix epoch.
    pub updated_at_ms: u64,
}

/// Request to atomically replace an external import snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveExternalReviewImportRequest {
    /// Repository containing the durable workspace.
    pub repo_path: PathBuf,
    /// Durable workspace id.
    pub workspace_id: String,
    /// Read-only importer response to persist.
    pub import: ExternalReviewImportResponse,
}

/// Response after saving an external import snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveExternalReviewImportResponse {
    /// Persisted snapshot.
    pub record: ExternalReviewImportRecord,
}

/// Request to list durable external import snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListExternalReviewImportsRequest {
    /// Repository containing the durable workspace.
    pub repo_path: PathBuf,
    /// Durable workspace id.
    pub workspace_id: String,
}

/// Response containing durable external import snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListExternalReviewImportsResponse {
    /// Snapshots ordered by importer id.
    pub records: Vec<ExternalReviewImportRecord>,
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

/// Current portable adversarial review contract version.
pub const ADVERSARIAL_REVIEW_CONTRACT_VERSION: u32 = 1;
/// Maximum reviewers retained in one panel result.
pub const MAX_ADVERSARIAL_REVIEWERS: usize = 32;
/// Maximum findings retained in one report or consensus.
pub const MAX_ADVERSARIAL_REVIEW_FINDINGS: usize = 512;
/// Maximum UTF-8 bytes retained in one evidence/remediation field.
pub const MAX_ADVERSARIAL_REVIEW_TEXT_BYTES: usize = 16_384;
/// Maximum model/tool fact entries retained for one reviewer.
pub const MAX_ADVERSARIAL_REVIEW_FACTS: usize = 64;

/// Portable validation error for adversarial review contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdversarialReviewValidationError {
    /// Stable contract field path.
    pub path: String,
    /// Actionable validation message.
    pub message: String,
}

impl fmt::Display for AdversarialReviewValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid adversarial review at '{}': {}",
            self.path, self.message
        )
    }
}

impl std::error::Error for AdversarialReviewValidationError {}

fn adversarial_review_error(
    path: impl Into<String>,
    message: impl Into<String>,
) -> AdversarialReviewValidationError {
    AdversarialReviewValidationError {
        path: path.into(),
        message: message.into(),
    }
}

fn validate_review_id(path: &str, value: &str) -> Result<(), AdversarialReviewValidationError> {
    if value.is_empty()
        || value.len() > 256
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'/' | b'_' | b'-' | b':')
        })
    {
        return Err(adversarial_review_error(
            path,
            "identity must contain 1..=256 safe ASCII bytes",
        ));
    }
    Ok(())
}

fn validate_sha256(path: &str, value: &str) -> Result<(), AdversarialReviewValidationError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(adversarial_review_error(
            path,
            "digest must be 64 lowercase hexadecimal bytes",
        ));
    }
    Ok(())
}

fn validate_review_path(path: &str, value: &str) -> Result<(), AdversarialReviewValidationError> {
    let candidate = Path::new(value);
    if value.is_empty()
        || value.len() > 4_096
        || candidate.is_absolute()
        || candidate
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(adversarial_review_error(
            path,
            "path must be a bounded confined repository-relative path",
        ));
    }
    Ok(())
}

fn validate_review_text(path: &str, value: &str) -> Result<(), AdversarialReviewValidationError> {
    if value.trim().is_empty() || value.len() > MAX_ADVERSARIAL_REVIEW_TEXT_BYTES {
        return Err(adversarial_review_error(
            path,
            format!("text must contain 1..={MAX_ADVERSARIAL_REVIEW_TEXT_BYTES} UTF-8 bytes"),
        ));
    }
    Ok(())
}

/// Exact immutable repository state reviewed by every member of one panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRepositorySnapshot {
    /// Must equal [`ADVERSARIAL_REVIEW_CONTRACT_VERSION`].
    pub version: u32,
    /// Git-owner supplied stable repository identity digest.
    pub repository_identity_sha256: String,
    /// Exact HEAD object identity at assignment time.
    pub head_object_id: String,
    /// Git-owner supplied aggregate repository snapshot digest.
    pub aggregate_sha256: String,
    /// Exact project-instruction fingerprint included in the snapshot.
    pub project_instruction_fingerprint_sha256: String,
}

impl ReviewRepositorySnapshot {
    /// Validate exact repository snapshot facts.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions or malformed digest/object identities.
    pub fn validate(&self) -> Result<(), AdversarialReviewValidationError> {
        if self.version != ADVERSARIAL_REVIEW_CONTRACT_VERSION {
            return Err(adversarial_review_error(
                "snapshot.version",
                "unsupported version",
            ));
        }
        validate_sha256(
            "snapshot.repository_identity_sha256",
            &self.repository_identity_sha256,
        )?;
        validate_review_id("snapshot.head_object_id", &self.head_object_id)?;
        validate_sha256("snapshot.aggregate_sha256", &self.aggregate_sha256)?;
        validate_sha256(
            "snapshot.project_instruction_fingerprint_sha256",
            &self.project_instruction_fingerprint_sha256,
        )
    }
}

/// One independent reviewer assignment bound to one exact repository snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewAssignment {
    pub version: u32,
    pub assignment_id: String,
    pub reviewer_id: String,
    pub snapshot: ReviewRepositorySnapshot,
    /// Bounded confined repository-relative paths selected for review; empty means the full snapshot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include_paths: Vec<String>,
    /// Stable review policy identity.
    pub policy_id: String,
}

/// Deterministic request to materialize one independent assignment per reviewer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewPanelAssignmentRequest {
    pub version: u32,
    /// Stable identity of this review round.
    pub round_id: String,
    pub snapshot: ReviewRepositorySnapshot,
    /// Reviewer identities in deterministic panel order.
    pub reviewer_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include_paths: Vec<String>,
    pub policy_id: String,
}

/// Ordered exact assignments for one review round and repository snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewPanelAssignments {
    pub version: u32,
    pub round_id: String,
    pub snapshot_sha256: String,
    pub assignments: Vec<ReviewAssignment>,
}

impl ReviewPanelAssignmentRequest {
    /// Materialize stable ordered assignments without runtime or persistence behavior.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed identities/paths/snapshot, fewer than
    /// two reviewers, excessive reviewers, or reviewer identities not in strict unique order.
    pub fn materialize(&self) -> Result<ReviewPanelAssignments, AdversarialReviewValidationError> {
        if self.version != ADVERSARIAL_REVIEW_CONTRACT_VERSION {
            return Err(adversarial_review_error(
                "panel_assignment.version",
                "unsupported version",
            ));
        }
        validate_review_id("panel_assignment.round_id", &self.round_id)?;
        validate_review_id("panel_assignment.policy_id", &self.policy_id)?;
        self.snapshot.validate()?;
        if self.reviewer_ids.len() < 2 || self.reviewer_ids.len() > MAX_ADVERSARIAL_REVIEWERS {
            return Err(adversarial_review_error(
                "panel_assignment.reviewer_ids",
                format!("panel requires 2..={MAX_ADVERSARIAL_REVIEWERS} reviewers"),
            ));
        }
        if self.include_paths.len() > MAX_ADVERSARIAL_REVIEW_FINDINGS {
            return Err(adversarial_review_error(
                "panel_assignment.include_paths",
                "selected paths exceed the review bound",
            ));
        }
        let mut previous = None;
        for reviewer_id in &self.reviewer_ids {
            validate_review_id("panel_assignment.reviewer_ids", reviewer_id)?;
            if previous.is_some_and(|identity: &str| identity >= reviewer_id.as_str()) {
                return Err(adversarial_review_error(
                    "panel_assignment.reviewer_ids",
                    "reviewer identities must be strictly ordered and unique",
                ));
            }
            previous = Some(reviewer_id.as_str());
        }
        let mut paths = BTreeSet::new();
        for path in &self.include_paths {
            validate_review_path("panel_assignment.include_paths", path)?;
            if !paths.insert(path) {
                return Err(adversarial_review_error(
                    "panel_assignment.include_paths",
                    "selected paths must be unique",
                ));
            }
        }
        let assignments = self
            .reviewer_ids
            .iter()
            .map(|reviewer_id| ReviewAssignment {
                version: ADVERSARIAL_REVIEW_CONTRACT_VERSION,
                assignment_id: format!("{}:{reviewer_id}", self.round_id),
                reviewer_id: reviewer_id.clone(),
                snapshot: self.snapshot.clone(),
                include_paths: self.include_paths.clone(),
                policy_id: self.policy_id.clone(),
            })
            .collect::<Vec<_>>();
        for assignment in &assignments {
            assignment.validate()?;
        }
        Ok(ReviewPanelAssignments {
            version: ADVERSARIAL_REVIEW_CONTRACT_VERSION,
            round_id: self.round_id.clone(),
            snapshot_sha256: self.snapshot.aggregate_sha256.clone(),
            assignments,
        })
    }
}

impl ReviewAssignment {
    /// Validate assignment identity, exact snapshot binding, and selected paths.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed identities, duplicate paths, or an
    /// invalid repository snapshot.
    pub fn validate(&self) -> Result<(), AdversarialReviewValidationError> {
        if self.version != ADVERSARIAL_REVIEW_CONTRACT_VERSION {
            return Err(adversarial_review_error(
                "assignment.version",
                "unsupported version",
            ));
        }
        validate_review_id("assignment.assignment_id", &self.assignment_id)?;
        validate_review_id("assignment.reviewer_id", &self.reviewer_id)?;
        validate_review_id("assignment.policy_id", &self.policy_id)?;
        self.snapshot.validate()?;
        if self.include_paths.len() > MAX_ADVERSARIAL_REVIEW_FINDINGS {
            return Err(adversarial_review_error(
                "assignment.include_paths",
                "selected paths exceed the review bound",
            ));
        }
        let mut paths = BTreeSet::new();
        for path in &self.include_paths {
            validate_review_path("assignment.include_paths", path)?;
            if !paths.insert(path) {
                return Err(adversarial_review_error(
                    "assignment.include_paths",
                    "selected paths must be unique",
                ));
            }
        }
        Ok(())
    }
}

/// Severity of one normalized review finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingSeverity {
    Advisory,
    Minor,
    Major,
    Critical,
}

/// Reviewer's requested treatment before consensus disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingDisposition {
    Required,
    Advisory,
}

/// Exact optional repository source range for one finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSourceRange {
    pub path: String,
    pub start_line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_column: Option<u32>,
    pub end_line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
}

impl ReviewSourceRange {
    fn validate(&self) -> Result<(), AdversarialReviewValidationError> {
        validate_review_path("finding.location.path", &self.path)?;
        if self.start_line == 0
            || self.end_line < self.start_line
            || self.start_column == Some(0)
            || self.end_column == Some(0)
            || (self.start_line == self.end_line
                && self
                    .start_column
                    .zip(self.end_column)
                    .is_some_and(|(start, end)| end < start))
        {
            return Err(adversarial_review_error(
                "finding.location",
                "source range is malformed or reversed",
            ));
        }
        Ok(())
    }
}

/// Stable snapshot-bound normalized review finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFinding {
    pub version: u32,
    pub finding_id: String,
    pub snapshot_sha256: String,
    pub severity: ReviewFindingSeverity,
    pub disposition: ReviewFindingDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<ReviewSourceRange>,
    pub title: String,
    pub evidence: String,
    pub remediation: String,
}

impl ReviewFinding {
    /// Validate one stable bounded snapshot-bound finding.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity/snapshot/range or oversized text.
    pub fn validate(&self) -> Result<(), AdversarialReviewValidationError> {
        if self.version != ADVERSARIAL_REVIEW_CONTRACT_VERSION {
            return Err(adversarial_review_error(
                "finding.version",
                "unsupported version",
            ));
        }
        validate_review_id("finding.finding_id", &self.finding_id)?;
        validate_sha256("finding.snapshot_sha256", &self.snapshot_sha256)?;
        if let Some(location) = &self.location {
            location.validate()?;
        }
        validate_review_text("finding.title", &self.title)?;
        validate_review_text("finding.evidence", &self.evidence)?;
        validate_review_text("finding.remediation", &self.remediation)
    }
}

/// Exact normalized model and tool facts recorded for one reviewer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewExecutionFacts {
    pub provider_id: String,
    pub model_id: String,
    pub agent_profile: String,
    /// Exact read-only tool identities available to this reviewer.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub tools: BTreeSet<String>,
}

impl ReviewExecutionFacts {
    fn validate(&self) -> Result<(), AdversarialReviewValidationError> {
        validate_review_id("report.execution.provider_id", &self.provider_id)?;
        validate_review_id("report.execution.model_id", &self.model_id)?;
        validate_review_id("report.execution.agent_profile", &self.agent_profile)?;
        if self.tools.len() > MAX_ADVERSARIAL_REVIEW_FACTS {
            return Err(adversarial_review_error(
                "report.execution.tools",
                "tool facts exceed the review bound",
            ));
        }
        for tool in &self.tools {
            validate_review_id("report.execution.tools", tool)?;
        }
        Ok(())
    }
}

/// One independent typed reviewer report for one exact assignment and snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewReport {
    pub version: u32,
    pub assignment_id: String,
    pub reviewer_id: String,
    pub snapshot_sha256: String,
    pub execution: ReviewExecutionFacts,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<ReviewFinding>,
}

impl ReviewReport {
    /// Validate report identity, bounds, finding uniqueness, and exact snapshot binding.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed facts, duplicate/conflicting findings,
    /// or mixed snapshot identities.
    pub fn validate(&self) -> Result<(), AdversarialReviewValidationError> {
        if self.version != ADVERSARIAL_REVIEW_CONTRACT_VERSION {
            return Err(adversarial_review_error(
                "report.version",
                "unsupported version",
            ));
        }
        validate_review_id("report.assignment_id", &self.assignment_id)?;
        validate_review_id("report.reviewer_id", &self.reviewer_id)?;
        validate_sha256("report.snapshot_sha256", &self.snapshot_sha256)?;
        self.execution.validate()?;
        if self.findings.len() > MAX_ADVERSARIAL_REVIEW_FINDINGS {
            return Err(adversarial_review_error(
                "report.findings",
                "findings exceed the review bound",
            ));
        }
        let mut findings = BTreeMap::new();
        for finding in &self.findings {
            finding.validate()?;
            if finding.snapshot_sha256 != self.snapshot_sha256 {
                return Err(adversarial_review_error(
                    "report.findings.snapshot_sha256",
                    "every finding must bind to the report snapshot",
                ));
            }
            if findings.insert(&finding.finding_id, finding).is_some() {
                return Err(adversarial_review_error(
                    "report.findings.finding_id",
                    "finding identities must be unique",
                ));
            }
        }
        Ok(())
    }

    /// Validate this report against the exact assignment it answers.
    ///
    /// # Errors
    ///
    /// Returns an error when assignment/reviewer/snapshot facts differ or either contract is
    /// malformed.
    pub fn validate_against_assignment(
        &self,
        assignment: &ReviewAssignment,
    ) -> Result<(), AdversarialReviewValidationError> {
        self.validate()?;
        assignment.validate()?;
        if self.assignment_id != assignment.assignment_id
            || self.reviewer_id != assignment.reviewer_id
            || self.snapshot_sha256 != assignment.snapshot.aggregate_sha256
        {
            return Err(adversarial_review_error(
                "report.assignment",
                "report must exactly match its assignment, reviewer, and repository snapshot",
            ));
        }
        Ok(())
    }
}

/// Ordered independent reports for one exact repository snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewPanelResult {
    pub version: u32,
    pub snapshot_sha256: String,
    /// Reports ordered by deterministic panel member identity.
    pub reports: Vec<ReviewReport>,
}

impl ReviewPanelResult {
    /// Validate bounded ordered unique reports and exact common snapshot binding.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed reports, ordering/identity conflicts,
    /// or mixed snapshots.
    pub fn validate(&self) -> Result<(), AdversarialReviewValidationError> {
        if self.version != ADVERSARIAL_REVIEW_CONTRACT_VERSION {
            return Err(adversarial_review_error(
                "panel.version",
                "unsupported version",
            ));
        }
        validate_sha256("panel.snapshot_sha256", &self.snapshot_sha256)?;
        if self.reports.len() < 2 || self.reports.len() > MAX_ADVERSARIAL_REVIEWERS {
            return Err(adversarial_review_error(
                "panel.reports",
                format!("panel requires 2..={MAX_ADVERSARIAL_REVIEWERS} reports"),
            ));
        }
        let mut previous = None;
        let mut findings = BTreeMap::<&str, &ReviewFinding>::new();
        for report in &self.reports {
            report.validate()?;
            if report.snapshot_sha256 != self.snapshot_sha256 {
                return Err(adversarial_review_error(
                    "panel.reports.snapshot_sha256",
                    "every report must bind to the panel snapshot",
                ));
            }
            if previous.is_some_and(|identity: &str| identity >= report.reviewer_id.as_str()) {
                return Err(adversarial_review_error(
                    "panel.reports.reviewer_id",
                    "reports must be strictly ordered by unique reviewer identity",
                ));
            }
            for finding in &report.findings {
                if findings
                    .insert(&finding.finding_id, finding)
                    .is_some_and(|existing| existing != finding)
                {
                    return Err(adversarial_review_error(
                        "panel.reports.findings.finding_id",
                        "duplicate finding identities must have identical normalized payloads",
                    ));
                }
            }
            previous = Some(report.reviewer_id.as_str());
        }
        Ok(())
    }

    fn findings_by_id(&self) -> BTreeMap<&str, &ReviewFinding> {
        self.reports
            .iter()
            .flat_map(|report| &report.findings)
            .map(|finding| (finding.finding_id.as_str(), finding))
            .collect()
    }
}

/// Consensus treatment of one submitted finding identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewConsensusDisposition {
    Required,
    Advisory,
    Invalid,
    Duplicate,
}

/// One auditable consensus decision for a submitted finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewConsensusDecision {
    pub finding_id: String,
    pub disposition: ReviewConsensusDisposition,
    /// Canonical retained finding for required/advisory decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finding: Option<ReviewFinding>,
    /// Existing canonical finding identity for duplicate decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<String>,
    pub rationale: String,
}

/// Complete bounded consensus over one exact panel result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewConsensus {
    pub version: u32,
    pub snapshot_sha256: String,
    /// Decisions ordered by submitted finding identity.
    pub decisions: Vec<ReviewConsensusDecision>,
}

impl ReviewConsensus {
    /// Validate ordered decisions, disposition payloads, and snapshot binding.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed/duplicate decisions, inconsistent
    /// disposition payloads, or mixed snapshot identities.
    pub fn validate(&self) -> Result<(), AdversarialReviewValidationError> {
        if self.version != ADVERSARIAL_REVIEW_CONTRACT_VERSION {
            return Err(adversarial_review_error(
                "consensus.version",
                "unsupported version",
            ));
        }
        validate_sha256("consensus.snapshot_sha256", &self.snapshot_sha256)?;
        if self.decisions.len() > MAX_ADVERSARIAL_REVIEW_FINDINGS {
            return Err(adversarial_review_error(
                "consensus.decisions",
                "decisions exceed the review bound",
            ));
        }
        let mut previous = None;
        for decision in &self.decisions {
            validate_review_id("consensus.decisions.finding_id", &decision.finding_id)?;
            validate_review_text("consensus.decisions.rationale", &decision.rationale)?;
            if previous.is_some_and(|identity: &str| identity >= decision.finding_id.as_str()) {
                return Err(adversarial_review_error(
                    "consensus.decisions.finding_id",
                    "decisions must be strictly ordered by unique finding identity",
                ));
            }
            match decision.disposition {
                ReviewConsensusDisposition::Required | ReviewConsensusDisposition::Advisory => {
                    let finding = decision.finding.as_ref().ok_or_else(|| {
                        adversarial_review_error(
                            "consensus.decisions.finding",
                            "retained decision requires the canonical finding",
                        )
                    })?;
                    finding.validate()?;
                    if finding.finding_id != decision.finding_id
                        || finding.snapshot_sha256 != self.snapshot_sha256
                        || decision.duplicate_of.is_some()
                    {
                        return Err(adversarial_review_error(
                            "consensus.decisions",
                            "retained decision payload is inconsistent",
                        ));
                    }
                }
                ReviewConsensusDisposition::Duplicate => {
                    if decision.finding.is_some()
                        || decision.duplicate_of.as_deref().is_none_or(|identity| {
                            identity == decision.finding_id
                                || validate_review_id("duplicate_of", identity).is_err()
                        })
                    {
                        return Err(adversarial_review_error(
                            "consensus.decisions.duplicate_of",
                            "duplicate decision requires one different canonical finding identity",
                        ));
                    }
                }
                ReviewConsensusDisposition::Invalid => {
                    if decision.finding.is_some() || decision.duplicate_of.is_some() {
                        return Err(adversarial_review_error(
                            "consensus.decisions",
                            "invalid decision cannot retain a finding or duplicate target",
                        ));
                    }
                }
            }
            previous = Some(decision.finding_id.as_str());
        }
        Ok(())
    }

    /// Validate consensus as a complete disposition of the exact panel findings.
    ///
    /// # Errors
    ///
    /// Returns an error when a panel finding is omitted, an unknown finding is introduced, a
    /// retained payload differs, a duplicate target is absent, or required reviewer findings are
    /// silently downgraded or invalidated.
    pub fn validate_against_panel(
        &self,
        panel: &ReviewPanelResult,
    ) -> Result<(), AdversarialReviewValidationError> {
        self.validate()?;
        panel.validate()?;
        if self.snapshot_sha256 != panel.snapshot_sha256 {
            return Err(adversarial_review_error(
                "consensus.snapshot_sha256",
                "consensus must bind to the panel snapshot",
            ));
        }
        let findings = panel.findings_by_id();
        if self.decisions.len() != findings.len() {
            return Err(adversarial_review_error(
                "consensus.decisions",
                "consensus must disposition every distinct panel finding exactly once",
            ));
        }
        let decisions = self
            .decisions
            .iter()
            .map(|decision| (decision.finding_id.as_str(), decision))
            .collect::<BTreeMap<_, _>>();
        for (finding_id, finding) in findings {
            let decision = decisions.get(finding_id).ok_or_else(|| {
                adversarial_review_error(
                    "consensus.decisions",
                    "consensus omitted a submitted finding",
                )
            })?;
            if finding.disposition == ReviewFindingDisposition::Required
                && !matches!(decision.disposition, ReviewConsensusDisposition::Required)
            {
                return Err(adversarial_review_error(
                    "consensus.decisions.disposition",
                    "required reviewer findings must remain required unless separately adjudicated with an explicit supported proof contract",
                ));
            }
            if matches!(
                decision.disposition,
                ReviewConsensusDisposition::Required | ReviewConsensusDisposition::Advisory
            ) && decision.finding.as_ref() != Some(finding)
            {
                return Err(adversarial_review_error(
                    "consensus.decisions.finding",
                    "retained consensus finding must equal the normalized panel finding",
                ));
            }
            if let Some(duplicate_of) = &decision.duplicate_of {
                let Some(target) = decisions.get(duplicate_of.as_str()) else {
                    return Err(adversarial_review_error(
                        "consensus.decisions.duplicate_of",
                        "duplicate target must be another submitted finding",
                    ));
                };
                if matches!(target.disposition, ReviewConsensusDisposition::Duplicate) {
                    return Err(adversarial_review_error(
                        "consensus.decisions.duplicate_of",
                        "duplicate chains are prohibited",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Terminal result of one bounded adversarial review round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewRoundOutcome {
    Clean {
        version: u32,
        snapshot_sha256: String,
    },
    RemediationRequired {
        version: u32,
        snapshot_sha256: String,
        consensus: ReviewConsensus,
    },
    Exhausted {
        version: u32,
        snapshot_sha256: String,
        rounds: u32,
        consensus: ReviewConsensus,
    },
    Failed {
        version: u32,
        snapshot_sha256: String,
        message: String,
    },
    Cancelled {
        version: u32,
        snapshot_sha256: String,
    },
}

impl ReviewRoundOutcome {
    /// Validate one terminal review outcome and its exact snapshot binding.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed snapshots/messages, zero exhaustion
    /// rounds, or consensus bound to another snapshot.
    pub fn validate(&self) -> Result<(), AdversarialReviewValidationError> {
        let (version, snapshot) = match self {
            Self::Clean {
                version,
                snapshot_sha256,
            }
            | Self::Cancelled {
                version,
                snapshot_sha256,
            } => (*version, snapshot_sha256),
            Self::RemediationRequired {
                version,
                snapshot_sha256,
                consensus,
            } => {
                consensus.validate()?;
                if consensus.snapshot_sha256 != *snapshot_sha256 {
                    return Err(adversarial_review_error(
                        "outcome.consensus.snapshot_sha256",
                        "consensus must bind to the outcome snapshot",
                    ));
                }
                (*version, snapshot_sha256)
            }
            Self::Exhausted {
                version,
                snapshot_sha256,
                rounds,
                consensus,
            } => {
                if *rounds == 0 {
                    return Err(adversarial_review_error(
                        "outcome.rounds",
                        "exhausted outcome requires at least one round",
                    ));
                }
                consensus.validate()?;
                if consensus.snapshot_sha256 != *snapshot_sha256 {
                    return Err(adversarial_review_error(
                        "outcome.consensus.snapshot_sha256",
                        "consensus must bind to the outcome snapshot",
                    ));
                }
                (*version, snapshot_sha256)
            }
            Self::Failed {
                version,
                snapshot_sha256,
                message,
            } => {
                validate_review_text("outcome.message", message)?;
                (*version, snapshot_sha256)
            }
        };
        if version != ADVERSARIAL_REVIEW_CONTRACT_VERSION {
            return Err(adversarial_review_error(
                "outcome.version",
                "unsupported version",
            ));
        }
        validate_sha256("outcome.snapshot_sha256", snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review_snapshot() -> ReviewRepositorySnapshot {
        ReviewRepositorySnapshot {
            version: ADVERSARIAL_REVIEW_CONTRACT_VERSION,
            repository_identity_sha256: "a".repeat(64),
            head_object_id: "0123456789abcdef".to_string(),
            aggregate_sha256: "b".repeat(64),
            project_instruction_fingerprint_sha256: "c".repeat(64),
        }
    }

    fn review_finding(id: &str) -> ReviewFinding {
        ReviewFinding {
            version: ADVERSARIAL_REVIEW_CONTRACT_VERSION,
            finding_id: id.to_string(),
            snapshot_sha256: "b".repeat(64),
            severity: ReviewFindingSeverity::Major,
            disposition: ReviewFindingDisposition::Required,
            location: Some(ReviewSourceRange {
                path: "src/lib.rs".to_string(),
                start_line: 10,
                start_column: Some(2),
                end_line: 12,
                end_column: Some(8),
            }),
            title: "Terminal state can reopen".to_string(),
            evidence: "A stale update overwrites the persisted terminal state.".to_string(),
            remediation: "Reject updates after the authoritative terminal transition.".to_string(),
        }
    }

    fn review_report(reviewer: &str, finding_id: &str) -> ReviewReport {
        ReviewReport {
            version: ADVERSARIAL_REVIEW_CONTRACT_VERSION,
            assignment_id: format!("assignment-{reviewer}"),
            reviewer_id: reviewer.to_string(),
            snapshot_sha256: "b".repeat(64),
            execution: ReviewExecutionFacts {
                provider_id: "provider".to_string(),
                model_id: "model".to_string(),
                agent_profile: "reviewer".to_string(),
                tools: BTreeSet::from(["filesystem.read".to_string()]),
            },
            findings: vec![review_finding(finding_id)],
        }
    }

    #[test]
    fn adversarial_review_contracts_validate_and_round_trip() {
        let assignment_request = ReviewPanelAssignmentRequest {
            version: ADVERSARIAL_REVIEW_CONTRACT_VERSION,
            round_id: "round-1".to_string(),
            snapshot: review_snapshot(),
            reviewer_ids: vec!["reviewer-a".to_string(), "reviewer-b".to_string()],
            include_paths: vec!["src/lib.rs".to_string()],
            policy_id: "strict".to_string(),
        };
        let assignments = assignment_request.materialize().expect("assignments");
        assert_eq!(assignments.assignments.len(), 2);
        assert_eq!(
            assignments.assignments[0].assignment_id,
            "round-1:reviewer-a"
        );
        assert_eq!(
            assignments.assignments[1].assignment_id,
            "round-1:reviewer-b"
        );

        let assignment = ReviewAssignment {
            version: ADVERSARIAL_REVIEW_CONTRACT_VERSION,
            assignment_id: "assignment-a".to_string(),
            reviewer_id: "reviewer-a".to_string(),
            snapshot: review_snapshot(),
            include_paths: vec!["src/lib.rs".to_string()],
            policy_id: "strict".to_string(),
        };
        assignment.validate().expect("assignment");

        let panel = ReviewPanelResult {
            version: ADVERSARIAL_REVIEW_CONTRACT_VERSION,
            snapshot_sha256: "b".repeat(64),
            reports: vec![
                review_report("reviewer-a", "finding-a"),
                review_report("reviewer-b", "finding-b"),
            ],
        };
        panel.validate().expect("panel");
        let encoded = serde_json::to_string(&panel).expect("serialize panel");
        assert_eq!(
            serde_json::from_str::<ReviewPanelResult>(&encoded).expect("deserialize panel"),
            panel
        );

        let findings = [review_finding("finding-a"), review_finding("finding-b")];
        let consensus = ReviewConsensus {
            version: ADVERSARIAL_REVIEW_CONTRACT_VERSION,
            snapshot_sha256: "b".repeat(64),
            decisions: findings
                .into_iter()
                .map(|finding| ReviewConsensusDecision {
                    finding_id: finding.finding_id.clone(),
                    disposition: ReviewConsensusDisposition::Required,
                    finding: Some(finding),
                    duplicate_of: None,
                    rationale: "The evidence demonstrates a correctness defect.".to_string(),
                })
                .collect(),
        };
        consensus.validate().expect("consensus");
        consensus
            .validate_against_panel(&panel)
            .expect("panel consensus");
        ReviewRoundOutcome::RemediationRequired {
            version: ADVERSARIAL_REVIEW_CONTRACT_VERSION,
            snapshot_sha256: "b".repeat(64),
            consensus,
        }
        .validate()
        .expect("outcome");
    }

    #[test]
    fn adversarial_review_contracts_reject_mixed_snapshot_and_conflicting_identity() {
        let mut report = review_report("reviewer-a", "finding-a");
        report.findings[0].snapshot_sha256 = "d".repeat(64);
        assert!(report.validate().is_err());

        let unordered = ReviewPanelAssignmentRequest {
            version: ADVERSARIAL_REVIEW_CONTRACT_VERSION,
            round_id: "round-1".to_string(),
            snapshot: review_snapshot(),
            reviewer_ids: vec!["reviewer-b".to_string(), "reviewer-a".to_string()],
            include_paths: Vec::new(),
            policy_id: "strict".to_string(),
        };
        assert!(unordered.materialize().is_err());

        let duplicate_report = review_report("reviewer-a", "finding-b");
        let panel = ReviewPanelResult {
            version: ADVERSARIAL_REVIEW_CONTRACT_VERSION,
            snapshot_sha256: "b".repeat(64),
            reports: vec![review_report("reviewer-a", "finding-a"), duplicate_report],
        };
        assert!(panel.validate().is_err());

        let decision = ReviewConsensusDecision {
            finding_id: "finding-a".to_string(),
            disposition: ReviewConsensusDisposition::Duplicate,
            finding: None,
            duplicate_of: Some("finding-a".to_string()),
            rationale: "duplicate".to_string(),
        };
        assert!(
            ReviewConsensus {
                version: ADVERSARIAL_REVIEW_CONTRACT_VERSION,
                snapshot_sha256: "b".repeat(64),
                decisions: vec![decision],
            }
            .validate()
            .is_err()
        );

        let panel = ReviewPanelResult {
            version: ADVERSARIAL_REVIEW_CONTRACT_VERSION,
            snapshot_sha256: "b".repeat(64),
            reports: vec![
                review_report("reviewer-a", "finding-a"),
                review_report("reviewer-b", "finding-b"),
            ],
        };
        let finding = review_finding("finding-a");
        let incomplete = ReviewConsensus {
            version: ADVERSARIAL_REVIEW_CONTRACT_VERSION,
            snapshot_sha256: "b".repeat(64),
            decisions: vec![ReviewConsensusDecision {
                finding_id: finding.finding_id.clone(),
                disposition: ReviewConsensusDisposition::Advisory,
                finding: Some(finding),
                duplicate_of: None,
                rationale: "downgraded".to_string(),
            }],
        };
        assert!(incomplete.validate_against_panel(&panel).is_err());
    }

    #[test]
    fn external_review_import_models_round_trip_json() {
        let response = ExternalReviewImportResponse {
            importer_id: "github_pr_review".to_string(),
            threads: vec![ExternalReviewThread {
                identity: ExternalReviewIdentity {
                    provider_id: "github".to_string(),
                    external_id: "thread-1".to_string(),
                    url: Some("https://github.com/owner/repo/pull/1#discussion_r1".to_string()),
                },
                state: ExternalReviewThreadState::Outdated,
                mapping: ExternalReviewAnchorMapping {
                    status: ExternalAnchorMappingStatus::Unmappable,
                    anchor: None,
                    message: Some("diff context is outdated".to_string()),
                },
                comments: vec![ExternalReviewComment {
                    identity: ExternalReviewIdentity {
                        provider_id: "github".to_string(),
                        external_id: "comment-1".to_string(),
                        url: None,
                    },
                    author: None,
                    author_label: Some("reviewer".to_string()),
                    body: "Please handle this case.".to_string(),
                    created_at_ms: Some(10),
                    updated_at_ms: Some(20),
                }],
            }],
            warnings: vec!["one thread was unmappable".to_string()],
        };

        let json = serde_json::to_string(&response).expect("serialize import response");
        let decoded: ExternalReviewImportResponse =
            serde_json::from_str(&json).expect("deserialize import response");

        assert_eq!(decoded, response);
    }

    #[test]
    fn review_ai_exchange_models_round_trip_json() {
        let exchange = ReviewAiExchange {
            schema_version: REVIEW_AI_EXCHANGE_SCHEMA_VERSION,
            exchange_id: "exchange-1".to_string(),
            anchor: DraftAnchor {
                kind: ReviewAnchorKind::Range,
                file_path: "src/lib.rs".to_string(),
                diff_row: 3,
                start_diff_row: Some(3),
                end_diff_row: Some(4),
                old_start: Some(2),
                old_end: Some(2),
                new_start: Some(3),
                new_end: Some(4),
                old_line: None,
                new_line: None,
                line_kind: ReviewLineKind::Added,
                is_file_anchor: false,
                surface_id: Some("surface-1".to_string()),
                source_id: Some("source-1".to_string()),
            },
            question: "Why is this safe?".to_string(),
            session_id: Some("session-1".to_string()),
            status: ReviewAiExchangeStatus::Linked,
            error: None,
            created_at_ms: 10,
            updated_at_ms: 20,
        };

        let json = serde_json::to_string(&exchange).expect("serialize exchange");
        let round_trip: ReviewAiExchange =
            serde_json::from_str(&json).expect("deserialize exchange");

        assert_eq!(round_trip, exchange);
        assert_eq!(
            serde_json::to_value(ReviewAiExchangeStatus::Failed).expect("status json"),
            serde_json::json!("failed")
        );
    }

    #[test]
    fn review_suggestion_models_round_trip_json() {
        let suggestion = ReviewSuggestion {
            suggestion_id: "suggestion-1".to_string(),
            anchor: DraftAnchor {
                kind: ReviewAnchorKind::Range,
                file_path: "src/lib.rs".to_string(),
                diff_row: 3,
                start_diff_row: Some(3),
                end_diff_row: Some(4),
                old_start: Some(2),
                old_end: Some(2),
                new_start: Some(3),
                new_end: Some(4),
                old_line: None,
                new_line: None,
                line_kind: ReviewLineKind::Added,
                is_file_anchor: false,
                surface_id: Some("surface-1".to_string()),
                source_id: Some("source-1".to_string()),
            },
            body: "Handle the error explicitly.".to_string(),
            session_id: Some("session-1".to_string()),
            status: ReviewSuggestionStatus::Rejected,
            rationale: Some("AI review".to_string()),
            created_at_ms: 10,
            updated_at_ms: 20,
        };

        let json = serde_json::to_string(&suggestion).expect("serialize suggestion");
        let decoded: ReviewSuggestion =
            serde_json::from_str(&json).expect("deserialize suggestion");

        assert_eq!(decoded, suggestion);
    }

    #[test]
    fn review_suggestion_status_uses_provider_neutral_wire_values() {
        assert_eq!(
            serde_json::to_string(&ReviewSuggestionStatus::Refining)
                .expect("serialize suggestion status"),
            "\"refining\""
        );
        assert_eq!(
            serde_json::from_str::<ReviewSuggestionStatus>("\"accepted\"")
                .expect("deserialize suggestion status"),
            ReviewSuggestionStatus::Accepted
        );
    }
}
