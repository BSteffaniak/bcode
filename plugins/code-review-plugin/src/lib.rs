#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled local Git code review plugin for Bcode.

use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use switchy::database::query::{FilterableQuery as _, where_eq};
use switchy::database::schema::{Column, DataType, alter_table, create_table};
use switchy::database::{Database, DatabaseError, DatabaseValue, Row};
use switchy::schema::discovery::code::{CodeMigration, CodeMigrationSource};
use switchy::schema::runner::MigrationRunner;
use thiserror::Error;

/// Code review plugin service interface.
pub const CODE_REVIEW_SERVICE_INTERFACE_ID: &str = "bcode.code_review/v1";

/// Operation that creates an ephemeral local review from a Git target.
pub const OP_CREATE_REVIEW: &str = "create_review";
/// Operation that lists persisted draft comments for a review target.
pub const OP_DRAFT_LIST: &str = "draft.list";
/// Operation that saves a persisted draft comment.
pub const OP_DRAFT_SAVE: &str = "draft.save";
/// Operation that deletes a persisted draft comment.
pub const OP_DRAFT_DELETE: &str = "draft.delete";
/// Operation that updates a persisted draft comment.
pub const OP_DRAFT_UPDATE: &str = "draft.update";
/// Operation that links a review thread to a Bcode session.
pub const OP_THREAD_LINK_SESSION: &str = "thread.link_session";
/// Operation that returns review context metadata.
pub const OP_REVIEW_CONTEXT_GET: &str = "review.context.get";
/// Operation that lists review comments.
pub const OP_REVIEW_COMMENTS_LIST: &str = "review.comments.list";
/// Operation that returns one review thread.
pub const OP_REVIEW_THREAD_GET: &str = "review.thread.get";
/// Operation that returns file diff context.
pub const OP_REVIEW_DIFF_GET: &str = "review.diff.get";
/// Operation that returns a provider-neutral review bundle.
pub const OP_REVIEW_BUNDLE_GET: &str = "review.bundle.get";
/// Operation that lists review publishers.
pub const OP_REVIEW_PUBLISHERS_LIST: &str = "review.publishers.list";
/// Operation that previews a review publish operation.
pub const OP_REVIEW_PUBLISH_PREVIEW: &str = "review.publish.preview";
/// Operation that submits a review publish operation.
pub const OP_REVIEW_PUBLISH_SUBMIT: &str = "review.publish.submit";

const CODE_REVIEW_STATE_DIR_ENV: &str = "BCODE_CODE_REVIEW_STATE_DIR";
const DEFAULT_STATE_ROOT: &str = ".bcode/code-review";
const DATABASE_FILE_NAME: &str = "code-review.db";
const MIGRATIONS_TABLE: &str = "__bcode_code_review_migrations";
const DATABASE_OPEN_RETRY_ATTEMPTS: u32 = 7;
const DATABASE_OPEN_INITIAL_RETRY_DELAY: Duration = Duration::from_millis(25);
const DATABASE_OPEN_MAX_RETRY_DELAY: Duration = Duration::from_secs(2);
const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Bundled local code review plugin.
#[derive(Default)]
pub struct CodeReviewPlugin;

impl RustPlugin for CodeReviewPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != CODE_REVIEW_SERVICE_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported code review plugin service interface",
            );
        }

        match context.request.operation.as_str() {
            OP_CREATE_REVIEW => create_review(&context.request),
            OP_DRAFT_LIST => list_drafts(&context.request),
            OP_DRAFT_SAVE => save_draft(&context.request),
            OP_DRAFT_DELETE => delete_draft(&context.request),
            OP_DRAFT_UPDATE => update_draft(&context.request),
            OP_THREAD_LINK_SESSION => link_thread_session(&context.request),
            OP_REVIEW_CONTEXT_GET => review_context_get(&context.request),
            OP_REVIEW_COMMENTS_LIST => review_comments_list(&context.request),
            OP_REVIEW_THREAD_GET => review_thread_get(&context.request),
            OP_REVIEW_DIFF_GET => review_diff_get(&context.request),
            OP_REVIEW_BUNDLE_GET => review_bundle_get(&context.request),
            OP_REVIEW_PUBLISHERS_LIST => review_publishers_list(&context.request),
            OP_REVIEW_PUBLISH_PREVIEW => review_publish_preview(&context.request),
            OP_REVIEW_PUBLISH_SUBMIT => review_publish_submit(&context.request),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported code review service operation",
            ),
        }
    }
}

/// Request payload for `create_review`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateReviewRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Local Git target to review.
    pub target: ReviewTarget,
}

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
    /// Local Git target whose draft should be saved.
    pub target: ReviewTarget,
    /// Comment anchor.
    pub anchor: DraftAnchor,
    /// Draft body.
    pub body: String,
}

/// Request payload for `draft.delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteDraftRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Comment id to delete.
    pub comment_id: String,
}

/// Request payload for `draft.update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateDraftRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Comment id to update.
    pub comment_id: String,
    /// Updated draft body.
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

/// Request payload for review context operations scoped to a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewContextRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
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
    /// Old range start line, when present.
    #[serde(default)]
    pub old_start: Option<u32>,
    /// Old range end line, when present.
    #[serde(default)]
    pub old_end: Option<u32>,
    /// New range start line, when present.
    #[serde(default)]
    pub new_start: Option<u32>,
    /// New range end line, when present.
    #[serde(default)]
    pub new_end: Option<u32>,
    /// Old line number, when present.
    pub old_line: Option<u32>,
    /// New line number, when present.
    pub new_line: Option<u32>,
    /// Line kind.
    pub line_kind: ReviewLineKind,
}

/// Persisted draft comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftComment {
    /// Comment id.
    pub comment_id: String,
    /// Thread id.
    pub thread_id: String,
    /// Comment anchor.
    pub anchor: DraftAnchor,
    /// Draft body.
    pub body: String,
    /// Creation timestamp in milliseconds since Unix epoch.
    pub created_at_ms: u64,
    /// Last update timestamp in milliseconds since Unix epoch.
    pub updated_at_ms: u64,
    /// Linked Bcode session id, when present.
    #[serde(default)]
    pub session_id: Option<String>,
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

/// Response payload for `review.context.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewContextResponse {
    /// Human-readable target label.
    pub title: String,
    /// Repository root resolved by Git.
    pub repo_root: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Files in review order.
    pub files: Vec<ReviewFileSummary>,
    /// Total added lines.
    pub additions: u32,
    /// Total removed lines.
    pub deletions: u32,
    /// Total draft comments.
    pub draft_count: usize,
    /// Total draft threads.
    pub thread_count: usize,
}

/// A review thread with its draft comments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewThread {
    /// Thread id.
    pub thread_id: String,
    /// Thread anchor.
    pub anchor: DraftAnchor,
    /// Linked Bcode session id, when present.
    pub session_id: Option<String>,
    /// Draft comments in the thread.
    pub comments: Vec<DraftComment>,
}

/// Response payload for `review.comments.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCommentsResponse {
    /// Draft threads in review order.
    pub threads: Vec<ReviewThread>,
}

/// Response payload for `review.thread.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewThreadResponse {
    /// Requested thread, if found.
    pub thread: Option<ReviewThread>,
    /// Selected diff lines for the thread.
    pub selected_lines: Vec<String>,
    /// Full hunk context for the thread.
    pub hunk_context: Vec<String>,
}

/// File diff context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFileDiff {
    /// File path.
    pub path: String,
    /// File status.
    pub status: ReviewFileStatus,
    /// Rendered diff lines.
    pub lines: Vec<String>,
}

/// Response payload for `review.diff.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDiffResponse {
    /// Matching file diffs.
    pub files: Vec<ReviewFileDiff>,
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
}

/// Response payload for `review.publishers.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReviewPublishersResponse {
    /// Available publishers.
    pub publishers: Vec<ReviewPublisherManifest>,
}

/// Request payload for publish operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishReviewRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Publisher id.
    pub publisher_id: String,
    /// Publisher-specific options.
    #[serde(default)]
    pub options: serde_json::Value,
}

/// Response payload for publish preview operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishReviewPreviewResponse {
    /// Publisher id.
    pub publisher_id: String,
    /// Human-readable preview content.
    pub preview: String,
}

/// Response payload for publish submit operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishReviewResponse {
    /// Publisher id.
    pub publisher_id: String,
    /// Whether publish succeeded.
    pub submitted: bool,
    /// Output location or provider URL, when available.
    pub output: Option<String>,
    /// Human-readable message.
    pub message: String,
}

/// Structured review response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummary {
    /// Human-readable target label.
    pub title: String,
    /// Repository root resolved by Git.
    pub repo_root: PathBuf,
    /// Files in review order.
    pub files: Vec<ReviewFile>,
    /// Total added lines.
    pub additions: u32,
    /// Total removed lines.
    pub deletions: u32,
}

/// A changed file in a review.
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

#[derive(Debug, Error)]
enum ReviewError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("Git command failed: {0}")]
    Git(String),
    #[error("failed to parse diff: {0}")]
    Parse(String),
    #[error("database connection failed: {0}")]
    Connection(#[from] switchy::database_connection::InitTursoError),
    #[error("database operation failed: {0}")]
    Database(#[from] DatabaseError),
    #[error("database migration failed: {0}")]
    Migration(#[from] switchy::schema::MigrationError),
    #[error("serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("database row is missing column {0}")]
    MissingColumn(&'static str),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported review publisher: {0}")]
    UnsupportedPublisher(String),
}

fn create_review(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<CreateReviewRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };

    match create_review_summary(&request) {
        Ok(summary) => json_response(&summary),
        Err(error) => ServiceResponse::error("review_failed", error.to_string()),
    }
}

fn list_drafts(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ListDraftsRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };

    match list_drafts_for_request(&request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("draft_list_failed", error.to_string()),
    }
}

fn save_draft(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<SaveDraftRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };

    match save_draft_for_request(request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("draft_save_failed", error.to_string()),
    }
}

fn delete_draft(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<DeleteDraftRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };

    match delete_draft_for_request(request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("draft_delete_failed", error.to_string()),
    }
}

fn update_draft(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<UpdateDraftRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };

    match update_draft_for_request(request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("draft_update_failed", error.to_string()),
    }
}

fn link_thread_session(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<LinkThreadSessionRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };

    match link_thread_session_for_request(request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("thread_link_session_failed", error.to_string()),
    }
}

fn review_context_get(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ReviewContextRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match review_context_for_request(request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("review_context_failed", error.to_string()),
    }
}

fn review_comments_list(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ReviewContextRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match review_comments_for_request(request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("review_comments_failed", error.to_string()),
    }
}

fn review_thread_get(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<GetReviewThreadRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match review_thread_for_request(request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("review_thread_failed", error.to_string()),
    }
}

fn review_diff_get(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<GetReviewDiffRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match review_diff_for_request(request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("review_diff_failed", error.to_string()),
    }
}

fn review_bundle_get(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ReviewContextRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match review_bundle_for_request(request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("review_bundle_failed", error.to_string()),
    }
}

fn review_publishers_list(request: &ServiceRequest) -> ServiceResponse {
    if let Err(error) = request.payload_json::<serde_json::Value>() {
        return ServiceResponse::error("invalid_request", error.to_string());
    }
    json_response(&ListReviewPublishersResponse {
        publishers: builtin_publishers(),
    })
}

fn review_publish_preview(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<PublishReviewRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match publish_preview_for_request(request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("review_publish_preview_failed", error.to_string()),
    }
}

fn review_publish_submit(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<PublishReviewRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match publish_submit_for_request(request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("review_publish_submit_failed", error.to_string()),
    }
}

fn review_context_for_request(
    request: ReviewContextRequest,
) -> Result<ReviewContextResponse, ReviewError> {
    let summary = create_review_summary(&CreateReviewRequest {
        repo_path: request.repo_path.clone(),
        target: request.target.clone(),
    })?;
    let drafts = list_drafts_for_request(&ListDraftsRequest {
        repo_path: request.repo_path,
        target: request.target.clone(),
    })?
    .drafts;
    let thread_count = drafts
        .iter()
        .map(|draft| draft.thread_id.clone())
        .collect::<BTreeSet<_>>()
        .len();
    Ok(ReviewContextResponse {
        title: summary.title,
        repo_root: summary.repo_root,
        target: request.target,
        files: summary
            .files
            .iter()
            .map(|file| ReviewFileSummary {
                path: file.display_path().to_string(),
                status: file.status,
                additions: file.additions,
                deletions: file.deletions,
                hunks: file.hunks.len(),
                is_binary: file.is_binary,
            })
            .collect(),
        additions: summary.additions,
        deletions: summary.deletions,
        draft_count: drafts.len(),
        thread_count,
    })
}

fn review_comments_for_request(
    request: ReviewContextRequest,
) -> Result<ReviewCommentsResponse, ReviewError> {
    let drafts = list_drafts_for_request(&ListDraftsRequest {
        repo_path: request.repo_path,
        target: request.target,
    })?
    .drafts;
    Ok(ReviewCommentsResponse {
        threads: threads_from_drafts(drafts),
    })
}

fn review_thread_for_request(
    request: GetReviewThreadRequest,
) -> Result<ReviewThreadResponse, ReviewError> {
    let summary = create_review_summary(&CreateReviewRequest {
        repo_path: request.repo_path.clone(),
        target: request.target.clone(),
    })?;
    let drafts = list_drafts_for_request(&ListDraftsRequest {
        repo_path: request.repo_path,
        target: request.target,
    })?
    .drafts;
    let thread = threads_from_drafts(drafts).into_iter().find(|thread| {
        request
            .thread_id
            .as_ref()
            .is_some_and(|thread_id| thread.thread_id == *thread_id)
            || request
                .anchor
                .as_ref()
                .is_some_and(|anchor| anchors_match(&thread.anchor, anchor))
    });
    let (selected_lines, hunk_context) = thread.as_ref().map_or_else(
        || (Vec::new(), Vec::new()),
        |thread| diff_context_for_anchor(&summary, &thread.anchor),
    );
    Ok(ReviewThreadResponse {
        thread,
        selected_lines,
        hunk_context,
    })
}

fn review_diff_for_request(
    request: GetReviewDiffRequest,
) -> Result<ReviewDiffResponse, ReviewError> {
    let summary = create_review_summary(&CreateReviewRequest {
        repo_path: request.repo_path,
        target: request.target,
    })?;
    let files = summary
        .files
        .iter()
        .filter(|file| {
            request
                .file_path
                .as_ref()
                .is_none_or(|path| file.display_path() == path)
        })
        .map(|file| ReviewFileDiff {
            path: file.display_path().to_string(),
            status: file.status,
            lines: rendered_diff_lines(file),
        })
        .collect();
    Ok(ReviewDiffResponse { files })
}

fn review_bundle_for_request(request: ReviewContextRequest) -> Result<ReviewBundle, ReviewError> {
    let summary = create_review_summary(&CreateReviewRequest {
        repo_path: request.repo_path.clone(),
        target: request.target.clone(),
    })?;
    let review_key = review_key(&summary.repo_root, &request.target)?;
    let threads = threads_from_drafts(
        list_drafts_for_request(&ListDraftsRequest {
            repo_path: request.repo_path,
            target: request.target.clone(),
        })?
        .drafts,
    )
    .into_iter()
    .map(|thread| {
        let (selected_diff_lines, hunk_context) = diff_context_for_anchor(&summary, &thread.anchor);
        ReviewBundleThread {
            thread_id: thread.thread_id,
            anchor: thread.anchor,
            comments: thread.comments,
            session_id: thread.session_id,
            selected_diff_lines,
            hunk_context,
        }
    })
    .collect();
    Ok(ReviewBundle {
        review_id: review_key,
        title: summary.title,
        repo_root: summary.repo_root,
        target: request.target,
        files: summary
            .files
            .iter()
            .map(|file| ReviewFileSummary {
                path: file.display_path().to_string(),
                status: file.status,
                additions: file.additions,
                deletions: file.deletions,
                hunks: file.hunks.len(),
                is_binary: file.is_binary,
            })
            .collect(),
        threads,
        generated_at_ms: now_ms(),
    })
}

fn builtin_publishers() -> Vec<ReviewPublisherManifest> {
    vec![
        ReviewPublisherManifest {
            id: "markdown_file".to_string(),
            label: "Markdown file".to_string(),
            description: "Write a local Markdown review export".to_string(),
            capabilities: file_publisher_capabilities(),
            options_schema: file_publisher_options_schema(),
        },
        ReviewPublisherManifest {
            id: "json_file".to_string(),
            label: "JSON file".to_string(),
            description: "Write a local JSON review bundle".to_string(),
            capabilities: file_publisher_capabilities(),
            options_schema: file_publisher_options_schema(),
        },
    ]
}

const fn file_publisher_capabilities() -> ReviewPublisherCapabilities {
    ReviewPublisherCapabilities {
        preview: true,
        submit: true,
        update_existing: true,
        supports_threads: true,
        supports_ranges: true,
        supports_inline_comments: true,
        supports_summary_comment: true,
    }
}

fn file_publisher_options_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "output_path": { "type": "string", "description": "Optional output path" }
        }
    })
}

fn publish_preview_for_request(
    request: PublishReviewRequest,
) -> Result<PublishReviewPreviewResponse, ReviewError> {
    let publisher_id = request.publisher_id.clone();
    let bundle = review_bundle_for_request(ReviewContextRequest {
        repo_path: request.repo_path,
        target: request.target,
    })?;
    Ok(PublishReviewPreviewResponse {
        preview: render_publisher_output(&publisher_id, &bundle)?,
        publisher_id,
    })
}

fn publish_submit_for_request(
    request: PublishReviewRequest,
) -> Result<PublishReviewResponse, ReviewError> {
    let output_path = request
        .options
        .get("output_path")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from);
    let publisher_id = request.publisher_id;
    let bundle = review_bundle_for_request(ReviewContextRequest {
        repo_path: request.repo_path,
        target: request.target,
    })?;
    let rendered = render_publisher_output(&publisher_id, &bundle)?;
    let extension = publisher_file_extension(&publisher_id)?;
    let path = output_path.unwrap_or_else(|| {
        bundle
            .repo_root
            .join(".bcode")
            .join("reviews")
            .join(format!(
                "{}.{}",
                safe_review_id(&bundle.review_id),
                extension
            ))
    });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, rendered)?;
    Ok(PublishReviewResponse {
        publisher_id,
        submitted: true,
        output: Some(path.display().to_string()),
        message: format!("wrote review export to {}", path.display()),
    })
}

fn render_publisher_output(
    publisher_id: &str,
    bundle: &ReviewBundle,
) -> Result<String, ReviewError> {
    match publisher_id {
        "markdown_file" => Ok(publish_markdown(bundle)),
        "json_file" => serde_json::to_string_pretty(bundle).map_err(ReviewError::Json),
        _ => Err(ReviewError::UnsupportedPublisher(publisher_id.to_string())),
    }
}

fn publisher_file_extension(publisher_id: &str) -> Result<&'static str, ReviewError> {
    match publisher_id {
        "markdown_file" => Ok("md"),
        "json_file" => Ok("json"),
        _ => Err(ReviewError::UnsupportedPublisher(publisher_id.to_string())),
    }
}

fn publish_markdown(bundle: &ReviewBundle) -> String {
    let mut output = String::new();
    let _ = write!(output, "# {}\n\n", bundle.title);
    let _ = writeln!(output, "* Review id: `{}`", bundle.review_id);
    let _ = writeln!(output, "* Repository: `{}`", bundle.repo_root.display());
    let _ = writeln!(output, "* Generated: `{}`", bundle.generated_at_ms);
    let _ = write!(output, "* Threads: `{}`\n\n", bundle.threads.len());
    for thread in &bundle.threads {
        let _ = write!(
            output,
            "## {} @ {}\n\n",
            thread.anchor.file_path,
            anchor_label(&thread.anchor)
        );
        if let Some(session_id) = &thread.session_id {
            let _ = write!(output, "* Bcode session: `{session_id}`\n\n");
        }
        for comment in &thread.comments {
            let _ = write!(
                output,
                "### Comment `{}`\n\n{}\n\n",
                comment.comment_id, comment.body
            );
        }
        if !thread.selected_diff_lines.is_empty() {
            output.push_str("Selected diff lines:\n\n```diff\n");
            output.push_str(&thread.selected_diff_lines.join("\n"));
            output.push_str("\n```\n\n");
        }
    }
    output
}

fn anchor_label(anchor: &DraftAnchor) -> String {
    let start = anchor.start_diff_row.unwrap_or(anchor.diff_row);
    let end = anchor.end_diff_row.unwrap_or(anchor.diff_row);
    if start == end {
        format!("row {start}")
    } else {
        format!("rows {start}-{end}")
    }
}

fn safe_review_id(review_id: &str) -> String {
    review_id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn list_drafts_for_request(request: &ListDraftsRequest) -> Result<ListDraftsResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let review_key = review_key(&repo_root, &request.target)?;
    let drafts = with_database(&repo_root, move |database| {
        Box::pin(async move { CodeReviewDb::new(database).list_drafts(&review_key).await })
    })?;
    Ok(ListDraftsResponse { drafts })
}

fn save_draft_for_request(request: SaveDraftRequest) -> Result<SaveDraftResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let db_repo_root = repo_root.clone();
    let review_key = review_key(&repo_root, &request.target)?;
    let target_kind = target_kind(&request.target).to_string();
    let target_json = serde_json::to_string(&request.target)?;
    let draft = with_database(&repo_root, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .save_draft(
                    &review_key,
                    &db_repo_root,
                    &target_kind,
                    &target_json,
                    request.anchor,
                    &request.body,
                )
                .await
        })
    })?;
    Ok(SaveDraftResponse { draft })
}

fn delete_draft_for_request(
    request: DeleteDraftRequest,
) -> Result<DeleteDraftResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let deleted = with_database(&repo_root, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .delete_draft(&request.comment_id)
                .await
        })
    })?;
    Ok(DeleteDraftResponse { deleted })
}

fn update_draft_for_request(
    request: UpdateDraftRequest,
) -> Result<UpdateDraftResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let result = with_database(&repo_root, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .update_draft(&request.comment_id, &request.body)
                .await
        })
    })?;
    Ok(result)
}

fn link_thread_session_for_request(
    request: LinkThreadSessionRequest,
) -> Result<LinkThreadSessionResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let db_repo_root = repo_root.clone();
    let review_key = review_key(&repo_root, &request.target)?;
    let target_kind = target_kind(&request.target).to_string();
    let target_json = serde_json::to_string(&request.target)?;
    let response = with_database(&repo_root, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .link_thread_session(
                    &review_key,
                    &db_repo_root,
                    &target_kind,
                    &target_json,
                    &request.anchor,
                    &request.session_id,
                )
                .await
        })
    })?;
    Ok(response)
}

fn threads_from_drafts(drafts: Vec<DraftComment>) -> Vec<ReviewThread> {
    let mut threads: BTreeMap<String, ReviewThread> = BTreeMap::new();
    for draft in drafts {
        let entry = threads
            .entry(draft.thread_id.clone())
            .or_insert_with(|| ReviewThread {
                thread_id: draft.thread_id.clone(),
                anchor: draft.anchor.clone(),
                session_id: draft.session_id.clone(),
                comments: Vec::new(),
            });
        if entry.session_id.is_none() {
            entry.session_id.clone_from(&draft.session_id);
        }
        entry.comments.push(draft);
    }
    threads.into_values().collect()
}

fn anchors_match(left: &DraftAnchor, right: &DraftAnchor) -> bool {
    left.file_path == right.file_path
        && left.diff_row == right.diff_row
        && left.end_diff_row == right.end_diff_row
        && left.old_line == right.old_line
        && left.new_line == right.new_line
}

fn diff_context_for_anchor(
    summary: &ReviewSummary,
    anchor: &DraftAnchor,
) -> (Vec<String>, Vec<String>) {
    let Some(file) = summary
        .files
        .iter()
        .find(|file| file.display_path() == anchor.file_path)
    else {
        return (Vec::new(), Vec::new());
    };
    let lines = rendered_diff_lines(file);
    let start =
        usize::try_from(anchor.start_diff_row.unwrap_or(anchor.diff_row)).unwrap_or(usize::MAX);
    let end = usize::try_from(anchor.end_diff_row.unwrap_or(anchor.diff_row)).unwrap_or(start);
    let selected_lines = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (start..=end).contains(&index).then_some(line.clone()))
        .collect();
    let mut row = 0usize;
    for hunk in &file.hunks {
        let hunk_start = row;
        let hunk_lines = rendered_hunk_lines(hunk);
        let hunk_end = hunk_start
            .saturating_add(hunk_lines.len())
            .saturating_sub(1);
        if start <= hunk_end && end >= hunk_start {
            return (selected_lines, hunk_lines);
        }
        row = row.saturating_add(hunk_lines.len());
    }
    (selected_lines, Vec::new())
}

fn rendered_diff_lines(file: &ReviewFile) -> Vec<String> {
    file.hunks.iter().flat_map(rendered_hunk_lines).collect()
}

fn rendered_hunk_lines(hunk: &ReviewHunk) -> Vec<String> {
    let mut lines = Vec::with_capacity(hunk.lines.len().saturating_add(1));
    lines.push(format!(
        "@@ -{},{} +{},{} @@{}",
        hunk.old_start,
        hunk.old_count,
        hunk.new_start,
        hunk.new_count,
        hunk.heading
            .as_ref()
            .map_or(String::new(), |heading| format!(" {heading}")),
    ));
    lines.extend(hunk.lines.iter().map(|line| {
        let marker = match line.kind {
            ReviewLineKind::Context => ' ',
            ReviewLineKind::Added => '+',
            ReviewLineKind::Removed => '-',
        };
        format!("{marker}{}", line.content)
    }));
    lines
}

fn resolve_repo_root(repo_path: &Path) -> Result<PathBuf, ReviewError> {
    if !repo_path.is_dir() {
        return Err(ReviewError::InvalidRequest(format!(
            "repo_path is not a directory: {}",
            repo_path.display()
        )));
    }
    let repo_root = git_output(repo_path, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(repo_root.trim()))
}

fn create_review_summary(request: &CreateReviewRequest) -> Result<ReviewSummary, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let diff = diff_for_target(&repo_root, &request.target)?;
    let files = parse_unified_diff(&diff)?;
    let additions = files.iter().map(|file| file.additions).sum();
    let deletions = files.iter().map(|file| file.deletions).sum();

    Ok(ReviewSummary {
        title: target_title(&request.target),
        repo_root,
        files,
        additions,
        deletions,
    })
}

struct CodeReviewDb<'a> {
    db: &'a dyn Database,
}

impl<'a> CodeReviewDb<'a> {
    const fn new(db: &'a dyn Database) -> Self {
        Self { db }
    }

    async fn list_drafts(&self, review_key: &str) -> Result<Vec<DraftComment>, ReviewError> {
        let thread_rows = self
            .db
            .select("draft_threads")
            .columns(&[
                "thread_id",
                "session_id",
                "file_path",
                "diff_row",
                "start_diff_row",
                "end_diff_row",
                "old_start",
                "old_end",
                "new_start",
                "new_end",
                "old_line",
                "new_line",
                "line_kind",
            ])
            .filter(Box::new(where_eq("review_key", review_key)))
            .execute(self.db)
            .await?;
        let mut drafts = Vec::new();
        for thread in thread_rows {
            let thread_id = required_text(&thread, "thread_id")?;
            let session_id = optional_text(&thread, "session_id");
            let anchor = DraftAnchor {
                file_path: required_text(&thread, "file_path")?,
                diff_row: i64_to_u64(required_i64(&thread, "diff_row")?),
                start_diff_row: optional_i64(&thread, "start_diff_row").map(i64_to_u64),
                end_diff_row: optional_i64(&thread, "end_diff_row").map(i64_to_u64),
                old_start: optional_i64(&thread, "old_start").map(i64_to_u32),
                old_end: optional_i64(&thread, "old_end").map(i64_to_u32),
                new_start: optional_i64(&thread, "new_start").map(i64_to_u32),
                new_end: optional_i64(&thread, "new_end").map(i64_to_u32),
                old_line: optional_i64(&thread, "old_line").map(i64_to_u32),
                new_line: optional_i64(&thread, "new_line").map(i64_to_u32),
                line_kind: line_kind_from_str(&required_text(&thread, "line_kind")?)?,
            };
            let comment_rows = self
                .db
                .select("draft_comments")
                .columns(&["comment_id", "body", "created_at_ms", "updated_at_ms"])
                .filter(Box::new(where_eq("thread_id", thread_id.clone())))
                .execute(self.db)
                .await?;
            drafts.extend(
                comment_rows
                    .into_iter()
                    .map(|row| comment_from_row(&row, &thread_id, &anchor, session_id.clone()))
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        Ok(drafts)
    }

    async fn save_draft(
        &self,
        review_key: &str,
        repo_root: &Path,
        target_kind: &str,
        target_json: &str,
        anchor: DraftAnchor,
        body: &str,
    ) -> Result<DraftComment, ReviewError> {
        let now = now_ms();
        let thread_id = thread_id(review_key, &anchor)?;
        let comment_id = comment_id(&thread_id, body, now);
        self.ensure_review(review_key, repo_root, target_kind, target_json, now)
            .await?;
        self.ensure_thread(review_key, &thread_id, &anchor, now)
            .await?;
        self.db
            .insert("draft_comments")
            .value("comment_id", comment_id.clone())
            .value("thread_id", thread_id.clone())
            .value("body", body.to_string())
            .value("created_at_ms", u64_to_i64(now))
            .value("updated_at_ms", u64_to_i64(now))
            .execute(self.db)
            .await?;
        Ok(DraftComment {
            comment_id,
            thread_id,
            anchor,
            body: body.to_string(),
            created_at_ms: now,
            updated_at_ms: now,
            session_id: None,
        })
    }

    async fn delete_draft(&self, comment_id: &str) -> Result<bool, ReviewError> {
        let Some(row) = self
            .db
            .select("draft_comments")
            .columns(&["thread_id"])
            .filter(Box::new(where_eq("comment_id", comment_id)))
            .execute_first(self.db)
            .await?
        else {
            return Ok(false);
        };
        let thread_id = required_text(&row, "thread_id")?;
        self.db
            .delete("draft_comments")
            .filter(Box::new(where_eq("comment_id", comment_id)))
            .execute(self.db)
            .await?;
        let remaining = self
            .db
            .select("draft_comments")
            .columns(&["comment_id"])
            .filter(Box::new(where_eq("thread_id", thread_id.clone())))
            .execute_first(self.db)
            .await?;
        if remaining.is_none() {
            self.db
                .delete("draft_threads")
                .filter(Box::new(where_eq("thread_id", thread_id)))
                .execute(self.db)
                .await?;
        }
        Ok(true)
    }

    async fn link_thread_session(
        &self,
        review_key: &str,
        repo_root: &Path,
        target_kind: &str,
        target_json: &str,
        anchor: &DraftAnchor,
        session_id: &str,
    ) -> Result<LinkThreadSessionResponse, ReviewError> {
        let now = now_ms();
        let thread_id = thread_id(review_key, anchor)?;
        self.ensure_review(review_key, repo_root, target_kind, target_json, now)
            .await?;
        self.ensure_thread(review_key, &thread_id, anchor, now)
            .await?;
        self.db
            .update("draft_threads")
            .value("session_id", session_id.to_string())
            .value("updated_at_ms", u64_to_i64(now))
            .filter(Box::new(where_eq("thread_id", thread_id.clone())))
            .execute(self.db)
            .await?;
        Ok(LinkThreadSessionResponse { thread_id })
    }

    async fn update_draft(
        &self,
        comment_id: &str,
        body: &str,
    ) -> Result<UpdateDraftResponse, ReviewError> {
        let exists = self
            .db
            .select("draft_comments")
            .columns(&["comment_id"])
            .filter(Box::new(where_eq("comment_id", comment_id)))
            .execute_first(self.db)
            .await?
            .is_some();
        if !exists {
            return Ok(UpdateDraftResponse {
                updated: false,
                updated_at_ms: None,
            });
        }
        let now = now_ms();
        self.db
            .update("draft_comments")
            .value("body", body.to_string())
            .value("updated_at_ms", u64_to_i64(now))
            .filter(Box::new(where_eq("comment_id", comment_id)))
            .execute(self.db)
            .await?;
        Ok(UpdateDraftResponse {
            updated: true,
            updated_at_ms: Some(now),
        })
    }

    async fn ensure_review(
        &self,
        review_key: &str,
        repo_root: &Path,
        target_kind: &str,
        target_json: &str,
        now: u64,
    ) -> Result<(), ReviewError> {
        if self
            .db
            .select("reviews")
            .columns(&["review_key"])
            .filter(Box::new(where_eq("review_key", review_key)))
            .execute_first(self.db)
            .await?
            .is_some()
        {
            self.db
                .update("reviews")
                .value("updated_at_ms", u64_to_i64(now))
                .filter(Box::new(where_eq("review_key", review_key)))
                .execute(self.db)
                .await?;
            return Ok(());
        }
        self.db
            .insert("reviews")
            .value("review_key", review_key.to_string())
            .value("repo_root", repo_root.display().to_string())
            .value("target_kind", target_kind.to_string())
            .value("target_json", target_json.to_string())
            .value("created_at_ms", u64_to_i64(now))
            .value("updated_at_ms", u64_to_i64(now))
            .execute(self.db)
            .await?;
        Ok(())
    }

    async fn ensure_thread(
        &self,
        review_key: &str,
        thread_id: &str,
        anchor: &DraftAnchor,
        now: u64,
    ) -> Result<(), ReviewError> {
        if self
            .db
            .select("draft_threads")
            .columns(&["thread_id"])
            .filter(Box::new(where_eq("thread_id", thread_id)))
            .execute_first(self.db)
            .await?
            .is_some()
        {
            self.db
                .update("draft_threads")
                .value("updated_at_ms", u64_to_i64(now))
                .filter(Box::new(where_eq("thread_id", thread_id)))
                .execute(self.db)
                .await?;
            return Ok(());
        }
        self.db
            .insert("draft_threads")
            .value("thread_id", thread_id.to_string())
            .value("review_key", review_key.to_string())
            .value("file_path", anchor.file_path.clone())
            .value("diff_row", u64_to_i64(anchor.diff_row))
            .value("start_diff_row", optional_u64(anchor.start_diff_row))
            .value("end_diff_row", optional_u64(anchor.end_diff_row))
            .value("old_start", optional_u32(anchor.old_start))
            .value("old_end", optional_u32(anchor.old_end))
            .value("new_start", optional_u32(anchor.new_start))
            .value("new_end", optional_u32(anchor.new_end))
            .value("old_line", optional_u32(anchor.old_line))
            .value("new_line", optional_u32(anchor.new_line))
            .value("line_kind", line_kind_str(anchor.line_kind))
            .value("created_at_ms", u64_to_i64(now))
            .value("updated_at_ms", u64_to_i64(now))
            .execute(self.db)
            .await?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatePaths {
    state_root: PathBuf,
    database_path: PathBuf,
}

fn state_paths(repo_root: &Path) -> StatePaths {
    let state_root = env::var_os(CODE_REVIEW_STATE_DIR_ENV)
        .map_or_else(|| repo_root.join(DEFAULT_STATE_ROOT), PathBuf::from);
    let database_path = state_root.join(DATABASE_FILE_NAME);
    StatePaths {
        state_root,
        database_path,
    }
}

fn with_database<T>(
    repo_root: &Path,
    operation: impl for<'a> FnOnce(
        &'a dyn Database,
    ) -> Pin<Box<dyn Future<Output = Result<T, ReviewError>> + 'a>>
    + Send
    + 'static,
) -> Result<T, ReviewError>
where
    T: Send + 'static,
{
    let paths = state_paths(repo_root);
    std::fs::create_dir_all(&paths.state_root)?;
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread Tokio runtime should build");
        runtime.block_on(async {
            let database = open_database(&paths.database_path).await?;
            run_migrations(database.as_ref()).await?;
            operation(database.as_ref()).await
        })
    })
    .join()
    .map_err(|_| ReviewError::InvalidRequest("database worker panicked".to_string()))?
}

async fn open_database(path: &Path) -> Result<Box<dyn Database>, ReviewError> {
    let mut attempt = 0_u32;
    let mut delay = DATABASE_OPEN_INITIAL_RETRY_DELAY;
    loop {
        match switchy::database_connection::builder()
            .turso()
            .with_path(path)
            .with_busy_timeout(DATABASE_BUSY_TIMEOUT)
            .with_multiprocess_wal(true)
            .build()
            .await
        {
            Ok(db) => return Ok(db),
            Err(error)
                if is_database_lock_error(&error) && attempt < DATABASE_OPEN_RETRY_ATTEMPTS =>
            {
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(DATABASE_OPEN_MAX_RETRY_DELAY);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn is_database_lock_error(error: &switchy::database_connection::InitTursoError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("locking error")
        || message.contains("failed locking file")
        || message.contains("database is locked")
        || message.contains("busy")
}

async fn run_migrations(database: &dyn Database) -> Result<(), ReviewError> {
    let runner = MigrationRunner::new(Box::new(code_review_migrations()))
        .with_table_name(MIGRATIONS_TABLE.to_string());
    runner.run(database).await?;
    Ok(())
}

fn code_review_migrations() -> CodeMigrationSource<'static> {
    let mut source = CodeMigrationSource::new();
    source.add_migration(CodeMigration::new(
        "001_reviews_table".to_string(),
        Box::new(
            create_table("reviews")
                .if_not_exists(true)
                .column(text_column("review_key"))
                .column(text_column("repo_root"))
                .column(text_column("target_kind"))
                .column(text_column("target_json"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .primary_key("review_key"),
        ),
        None,
    ));
    source.add_migration(CodeMigration::new(
        "002_draft_threads_table".to_string(),
        Box::new(
            create_table("draft_threads")
                .if_not_exists(true)
                .column(text_column("thread_id"))
                .column(text_column("review_key"))
                .column(text_column("file_path"))
                .column(int_column("diff_row"))
                .column(nullable_int_column("start_diff_row"))
                .column(nullable_int_column("end_diff_row"))
                .column(nullable_int_column("old_start"))
                .column(nullable_int_column("old_end"))
                .column(nullable_int_column("new_start"))
                .column(nullable_int_column("new_end"))
                .column(nullable_int_column("old_line"))
                .column(nullable_int_column("new_line"))
                .column(text_column("line_kind"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .primary_key("thread_id"),
        ),
        None,
    ));
    source.add_migration(CodeMigration::new(
        "003_draft_comments_table".to_string(),
        Box::new(
            create_table("draft_comments")
                .if_not_exists(true)
                .column(text_column("comment_id"))
                .column(text_column("thread_id"))
                .column(text_column("body"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .primary_key("comment_id"),
        ),
        None,
    ));
    source.add_migration(CodeMigration::new(
        "004_thread_session_column".to_string(),
        Box::new(alter_table("draft_threads").add_column(
            "session_id".to_string(),
            DataType::Text,
            true,
            None,
        )),
        None,
    ));
    source.add_migration(CodeMigration::new(
        "005_thread_range_columns".to_string(),
        Box::new(
            alter_table("draft_threads")
                .add_column("start_diff_row".to_string(), DataType::BigInt, true, None)
                .add_column("end_diff_row".to_string(), DataType::BigInt, true, None)
                .add_column("old_start".to_string(), DataType::BigInt, true, None)
                .add_column("old_end".to_string(), DataType::BigInt, true, None)
                .add_column("new_start".to_string(), DataType::BigInt, true, None)
                .add_column("new_end".to_string(), DataType::BigInt, true, None),
        ),
        None,
    ));
    source
}

fn text_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::Text,
        default: None,
    }
}

fn int_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::BigInt,
        default: None,
    }
}

fn nullable_int_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: true,
        auto_increment: false,
        data_type: DataType::BigInt,
        default: None,
    }
}

fn comment_from_row(
    row: &Row,
    thread_id: &str,
    anchor: &DraftAnchor,
    session_id: Option<String>,
) -> Result<DraftComment, ReviewError> {
    Ok(DraftComment {
        comment_id: required_text(row, "comment_id")?,
        thread_id: thread_id.to_string(),
        anchor: anchor.clone(),
        body: required_text(row, "body")?,
        created_at_ms: i64_to_u64(required_i64(row, "created_at_ms")?),
        updated_at_ms: i64_to_u64(required_i64(row, "updated_at_ms")?),
        session_id,
    })
}

fn required_text(row: &Row, column: &'static str) -> Result<String, ReviewError> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToString::to_string))
        .ok_or(ReviewError::MissingColumn(column))
}

fn optional_text(row: &Row, column: &'static str) -> Option<String> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToString::to_string))
}

fn required_i64(row: &Row, column: &'static str) -> Result<i64, ReviewError> {
    row.get(column)
        .and_then(|value| value.as_i64())
        .ok_or(ReviewError::MissingColumn(column))
}

fn optional_i64(row: &Row, column: &'static str) -> Option<i64> {
    row.get(column).and_then(|value| value.as_i64())
}

fn review_key(repo_root: &Path, target: &ReviewTarget) -> Result<String, ReviewError> {
    let mut hasher = Sha256::new();
    hasher.update(repo_root.display().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(serde_json::to_string(target)?.as_bytes());
    Ok(format!("review-{:x}", hasher.finalize()))
}

fn thread_id(review_key: &str, anchor: &DraftAnchor) -> Result<String, ReviewError> {
    let mut hasher = Sha256::new();
    hasher.update(review_key.as_bytes());
    hasher.update(b"\0");
    hasher.update(serde_json::to_string(anchor)?.as_bytes());
    Ok(format!("thread-{:x}", hasher.finalize()))
}

fn comment_id(thread_id: &str, body: &str, now: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(thread_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(body.as_bytes());
    hasher.update(b"\0");
    hasher.update(now.to_string().as_bytes());
    format!("comment-{:x}", hasher.finalize())
}

const fn target_kind(target: &ReviewTarget) -> &'static str {
    match target {
        ReviewTarget::WorkingTreeUnstaged => "working_tree_unstaged",
        ReviewTarget::IndexStaged => "index_staged",
        ReviewTarget::WorkingTreeAndIndex => "working_tree_and_index",
        ReviewTarget::LastCommit => "last_commit",
        ReviewTarget::CommitRange { .. } => "commit_range",
        ReviewTarget::BranchCompare { .. } => "branch_compare",
    }
}

const fn line_kind_str(kind: ReviewLineKind) -> &'static str {
    match kind {
        ReviewLineKind::Context => "context",
        ReviewLineKind::Added => "added",
        ReviewLineKind::Removed => "removed",
    }
}

fn line_kind_from_str(value: &str) -> Result<ReviewLineKind, ReviewError> {
    match value {
        "context" => Ok(ReviewLineKind::Context),
        "added" => Ok(ReviewLineKind::Added),
        "removed" => Ok(ReviewLineKind::Removed),
        _ => Err(ReviewError::InvalidRequest(format!(
            "unknown line kind: {value}"
        ))),
    }
}

fn optional_u32(value: Option<u32>) -> DatabaseValue {
    DatabaseValue::Int64Opt(value.map(i64::from))
}

fn optional_u64(value: Option<u64>) -> DatabaseValue {
    DatabaseValue::Int64Opt(value.map(u64_to_i64))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn i64_to_u32(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

fn diff_for_target(repo_root: &Path, target: &ReviewTarget) -> Result<String, ReviewError> {
    match target {
        ReviewTarget::WorkingTreeUnstaged => git_output(repo_root, &["diff", "--find-renames"]),
        ReviewTarget::IndexStaged => git_output(repo_root, &["diff", "--cached", "--find-renames"]),
        ReviewTarget::WorkingTreeAndIndex => {
            git_output(repo_root, &["diff", "HEAD", "--find-renames"])
        }
        ReviewTarget::LastCommit => {
            git_output(repo_root, &["show", "--format=", "--find-renames", "HEAD"])
        }
        ReviewTarget::CommitRange {
            base,
            head,
            merge_base,
        } => git_output(
            repo_root,
            &[
                "diff",
                "--find-renames",
                &range_spec(base, head, *merge_base),
            ],
        ),
        ReviewTarget::BranchCompare {
            base_branch,
            head_branch,
            merge_base,
        } => git_output(
            repo_root,
            &[
                "diff",
                "--find-renames",
                &range_spec(base_branch, head_branch, *merge_base),
            ],
        ),
    }
}

fn target_title(target: &ReviewTarget) -> String {
    match target {
        ReviewTarget::WorkingTreeUnstaged => "Unstaged Changes".to_string(),
        ReviewTarget::IndexStaged => "Staged Changes".to_string(),
        ReviewTarget::WorkingTreeAndIndex => "Staged + Unstaged Changes".to_string(),
        ReviewTarget::LastCommit => "Last Commit".to_string(),
        ReviewTarget::CommitRange {
            base,
            head,
            merge_base,
        } => format!("{base}{}{head}", if *merge_base { "..." } else { ".." }),
        ReviewTarget::BranchCompare {
            base_branch,
            head_branch,
            merge_base,
        } => format!(
            "{base_branch}{}{head_branch}",
            if *merge_base { "..." } else { ".." }
        ),
    }
}

fn range_spec(base: &str, head: &str, merge_base: bool) -> String {
    format!("{base}{}{head}", if merge_base { "..." } else { ".." })
}

fn git_output(repo_root: &Path, args: &[&str]) -> Result<String, ReviewError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }

    Err(ReviewError::Git(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

fn parse_unified_diff(diff: &str) -> Result<Vec<ReviewFile>, ReviewError> {
    let mut files = Vec::new();
    let mut current_file: Option<ReviewFile> = None;
    let mut current_hunk: Option<ReviewHunk> = None;
    let mut old_line = 0u32;
    let mut new_line = 0u32;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            push_hunk(&mut current_file, current_hunk.take());
            push_file(&mut files, current_file.take());
            current_file = Some(file_from_diff_git_line(line));
            continue;
        }

        let Some(file) = current_file.as_mut() else {
            continue;
        };

        if line.starts_with("new file mode ") {
            file.status = ReviewFileStatus::Added;
            continue;
        }
        if line.starts_with("deleted file mode ") {
            file.status = ReviewFileStatus::Deleted;
            continue;
        }
        if line.starts_with("similarity index ") {
            file.status = ReviewFileStatus::Renamed;
            continue;
        }
        if let Some(path) = line.strip_prefix("rename from ") {
            file.old_path = Some(path.to_string());
            file.status = ReviewFileStatus::Renamed;
            continue;
        }
        if let Some(path) = line.strip_prefix("rename to ") {
            file.new_path = Some(path.to_string());
            file.status = ReviewFileStatus::Renamed;
            continue;
        }
        if line.starts_with("Binary files ") {
            file.is_binary = true;
            continue;
        }
        if let Some(path) = line.strip_prefix("--- ") {
            if path != "/dev/null" {
                file.old_path = Some(strip_diff_path_prefix(path).to_string());
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ ") {
            if path != "/dev/null" {
                file.new_path = Some(strip_diff_path_prefix(path).to_string());
            }
            continue;
        }
        if line.starts_with("@@ ") {
            push_hunk(&mut current_file, current_hunk.take());
            let hunk = parse_hunk_header(line)?;
            old_line = hunk.old_start;
            new_line = hunk.new_start;
            current_hunk = Some(hunk);
            continue;
        }

        let Some(hunk) = current_hunk.as_mut() else {
            continue;
        };

        if let Some(content) = line.strip_prefix('+') {
            hunk.lines.push(ReviewLine {
                kind: ReviewLineKind::Added,
                old_line: None,
                new_line: Some(new_line),
                content: content.to_string(),
            });
            file.additions = file.additions.saturating_add(1);
            new_line = new_line.saturating_add(1);
        } else if let Some(content) = line.strip_prefix('-') {
            hunk.lines.push(ReviewLine {
                kind: ReviewLineKind::Removed,
                old_line: Some(old_line),
                new_line: None,
                content: content.to_string(),
            });
            file.deletions = file.deletions.saturating_add(1);
            old_line = old_line.saturating_add(1);
        } else if let Some(content) = line.strip_prefix(' ') {
            hunk.lines.push(ReviewLine {
                kind: ReviewLineKind::Context,
                old_line: Some(old_line),
                new_line: Some(new_line),
                content: content.to_string(),
            });
            old_line = old_line.saturating_add(1);
            new_line = new_line.saturating_add(1);
        }
    }

    push_hunk(&mut current_file, current_hunk.take());
    push_file(&mut files, current_file.take());
    Ok(files)
}

fn file_from_diff_git_line(line: &str) -> ReviewFile {
    let rest = line.strip_prefix("diff --git ").unwrap_or_default();
    let mut parts = rest.split_whitespace();
    let old_path = parts.next().map(strip_diff_path_prefix).map(str::to_string);
    let new_path = parts.next().map(strip_diff_path_prefix).map(str::to_string);

    ReviewFile {
        old_path,
        new_path,
        status: ReviewFileStatus::Modified,
        additions: 0,
        deletions: 0,
        hunks: Vec::new(),
        is_binary: false,
    }
}

fn parse_hunk_header(line: &str) -> Result<ReviewHunk, ReviewError> {
    let Some(rest) = line.strip_prefix("@@ ") else {
        return Err(ReviewError::Parse(format!("invalid hunk header: {line}")));
    };
    let Some((ranges, heading)) = rest.split_once(" @@") else {
        return Err(ReviewError::Parse(format!("invalid hunk header: {line}")));
    };
    let mut ranges = ranges.split_whitespace();
    let old_range = ranges
        .next()
        .ok_or_else(|| ReviewError::Parse(format!("missing old range: {line}")))?;
    let new_range = ranges
        .next()
        .ok_or_else(|| ReviewError::Parse(format!("missing new range: {line}")))?;
    let (old_start, old_count) = parse_hunk_range(old_range, '-')?;
    let (new_start, new_count) = parse_hunk_range(new_range, '+')?;
    let heading = heading.trim();

    Ok(ReviewHunk {
        old_start,
        old_count,
        new_start,
        new_count,
        heading: (!heading.is_empty()).then(|| heading.to_string()),
        lines: Vec::new(),
    })
}

fn parse_hunk_range(range: &str, prefix: char) -> Result<(u32, u32), ReviewError> {
    let Some(range) = range.strip_prefix(prefix) else {
        return Err(ReviewError::Parse(format!(
            "hunk range missing '{prefix}' prefix: {range}"
        )));
    };
    let (start, count) = range.split_once(',').map_or((range, "1"), |parts| parts);
    let start = start
        .parse::<u32>()
        .map_err(|error| ReviewError::Parse(format!("invalid hunk start '{start}': {error}")))?;
    let count = count
        .parse::<u32>()
        .map_err(|error| ReviewError::Parse(format!("invalid hunk count '{count}': {error}")))?;
    Ok((start, count))
}

fn push_hunk(file: &mut Option<ReviewFile>, hunk: Option<ReviewHunk>) {
    if let (Some(file), Some(hunk)) = (file, hunk) {
        file.hunks.push(hunk);
    }
}

fn push_file(files: &mut Vec<ReviewFile>, file: Option<ReviewFile>) {
    if let Some(file) = file {
        files.push(file);
    }
}

fn strip_diff_path_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

const fn default_true() -> bool {
    true
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(CodeReviewPlugin, include_str!("../bcode-plugin.toml"))
}

bcode_plugin_sdk::export_plugin!(CodeReviewPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modified_file_diff() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\nindex 1111111..2222222 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,3 @@\n line one\n-old\n+new\n+extra\n";

        let files = parse_unified_diff(diff).expect("parse diff");

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].display_path(), "src/lib.rs");
        assert_eq!(files[0].status, ReviewFileStatus::Modified);
        assert_eq!(files[0].additions, 2);
        assert_eq!(files[0].deletions, 1);
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(files[0].hunks[0].lines.len(), 4);
        assert_eq!(files[0].hunks[0].lines[0].old_line, Some(1));
        assert_eq!(files[0].hunks[0].lines[0].new_line, Some(1));
    }

    #[test]
    fn parses_rename_diff() {
        let diff = "diff --git a/old.rs b/new.rs\nsimilarity index 90%\nrename from old.rs\nrename to new.rs\n--- a/old.rs\n+++ b/new.rs\n@@ -1 +1 @@\n-old\n+new\n";

        let files = parse_unified_diff(diff).expect("parse diff");

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path.as_deref(), Some("old.rs"));
        assert_eq!(files[0].new_path.as_deref(), Some("new.rs"));
        assert_eq!(files[0].status, ReviewFileStatus::Renamed);
        assert_eq!(files[0].additions, 1);
        assert_eq!(files[0].deletions, 1);
    }

    #[test]
    fn parses_single_line_hunk_range() {
        let hunk = parse_hunk_header("@@ -4 +4,2 @@ fn main()").expect("parse hunk");

        assert_eq!(hunk.old_start, 4);
        assert_eq!(hunk.old_count, 1);
        assert_eq!(hunk.new_start, 4);
        assert_eq!(hunk.new_count, 2);
        assert_eq!(hunk.heading.as_deref(), Some("fn main()"));
    }
}
