#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared code review models for Bcode.

use serde::{Deserialize, Serialize};
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
}

/// Request payload for `draft.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListDraftsRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Local Git target whose drafts should be listed.
    pub target: ReviewTarget,
}

/// Request payload for `draft.save`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveDraftRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
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

/// Request payload for review context operations scoped to a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewContextRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
}

/// Persisted draft anchor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftAnchor {
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
    /// Publisher id.
    pub publisher_id: String,
    /// Publisher options.
    #[serde(default)]
    pub options: serde_json::Value,
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
