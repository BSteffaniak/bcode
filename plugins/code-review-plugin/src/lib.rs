#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled local Git code review plugin for Bcode.

use bcode_code_review_models::{
    ArchiveReviewWorkspaceRequest, ArchiveReviewWorkspaceResponse, CreateReviewWorkspaceRequest,
    CreateReviewWorkspaceResponse, DeleteDraftRequest, DeleteDraftResponse, DraftAnchor,
    DraftComment, GetReviewDiffRequest, GetReviewThreadRequest, GetReviewWorkspaceRequest,
    GetReviewWorkspaceResponse, LinkThreadSessionRequest, LinkThreadSessionResponse,
    ListDraftsRequest, ListDraftsResponse, ListReviewPublishersResponse,
    ListReviewWorkspacesRequest, ListReviewWorkspacesResponse, MaterializeReviewWorkspaceRequest,
    MaterializeReviewWorkspaceResponse, OP_REVIEW_REPO_FILE_GET, OP_REVIEW_WORKSPACE_ARCHIVE,
    OP_REVIEW_WORKSPACE_CREATE, OP_REVIEW_WORKSPACE_GET, OP_REVIEW_WORKSPACE_LIST,
    OP_REVIEW_WORKSPACE_MATERIALIZE, OP_REVIEW_WORKSPACE_UPDATE, PublishReviewPreviewResponse,
    PublishReviewRequest, PublishReviewResponse, RepositoryFileRequest, RepositoryFileResponse,
    ReviewBundle, ReviewBundleLine, ReviewBundleThread, ReviewContextRequest, ReviewFile,
    ReviewFileStatus, ReviewFileSummary, ReviewHunk, ReviewLine, ReviewLineKind,
    ReviewPublisherCapabilities, ReviewPublisherManifest, ReviewSource, ReviewSourceKind,
    ReviewSurface, ReviewSurfaceKind, ReviewTarget, ReviewWorkspace,
    ReviewWorkspaceMaterialization, SaveDraftRequest, SaveDraftResponse, UpdateDraftRequest,
    UpdateDraftResponse, UpdateReviewWorkspaceRequest, UpdateReviewWorkspaceResponse,
};
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
const DEFAULT_REPO_STATE_ROOT: &str = ".bcode/code-review";
const DEFAULT_STATE_SUBDIR: &str = "code-review";
const DATABASE_FILE_NAME: &str = "code-review.db";
const MIGRATIONS_TABLE: &str = "__bcode_code_review_migrations";
const DATABASE_OPEN_RETRY_ATTEMPTS: u32 = 7;
const DATABASE_OPEN_INITIAL_RETRY_DELAY: Duration = Duration::from_millis(25);
const DATABASE_OPEN_MAX_RETRY_DELAY: Duration = Duration::from_secs(2);
const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REPOSITORY_FILE_BYTES: u64 = 1_000_000;

/// Code review state location preference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeReviewStateLocation {
    /// Store code review state under the user's Bcode state directory.
    #[default]
    User,
    /// Store code review state under the repository-local `.bcode/code-review` directory.
    Repo,
}

/// Resolved code review plugin configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeReviewPluginConfig {
    /// State location preference. Defaults to the user's Bcode state directory.
    #[serde(default)]
    pub state_location: CodeReviewStateLocation,
    /// Explicit state directory path. Relative paths are resolved against the repository root.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
}

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
            OP_DRAFT_LIST => list_drafts(&context),
            OP_DRAFT_SAVE => save_draft(&context),
            OP_DRAFT_DELETE => delete_draft(&context),
            OP_DRAFT_UPDATE => update_draft(&context),
            OP_THREAD_LINK_SESSION => link_thread_session(&context),
            OP_REVIEW_CONTEXT_GET => review_context_get(&context),
            OP_REVIEW_COMMENTS_LIST => review_comments_list(&context),
            OP_REVIEW_THREAD_GET => review_thread_get(&context),
            OP_REVIEW_DIFF_GET => review_diff_get(&context),
            OP_REVIEW_BUNDLE_GET => review_bundle_get(&context),
            OP_REVIEW_REPO_FILE_GET => review_repo_file_get(&context.request),
            OP_REVIEW_WORKSPACE_LIST => review_workspace_list(&context),
            OP_REVIEW_WORKSPACE_CREATE => review_workspace_create(&context),
            OP_REVIEW_WORKSPACE_GET => review_workspace_get(&context),
            OP_REVIEW_WORKSPACE_UPDATE => review_workspace_update(&context),
            OP_REVIEW_WORKSPACE_ARCHIVE => review_workspace_archive(&context),
            OP_REVIEW_WORKSPACE_MATERIALIZE => review_workspace_materialize(&context),
            OP_REVIEW_PUBLISHERS_LIST => review_publishers_list(&context.request),
            OP_REVIEW_PUBLISH_PREVIEW => review_publish_preview(&context),
            OP_REVIEW_PUBLISH_SUBMIT => review_publish_submit(&context),
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

/// Ephemeral review summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummary {
    /// Human-readable target label.
    pub title: String,
    /// Repository root resolved by Git.
    pub repo_root: PathBuf,
    /// Parsed review files.
    pub files: Vec<ReviewFile>,
    /// Total added lines.
    pub additions: u32,
    /// Total removed lines.
    pub deletions: u32,
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

fn review_repo_file_get(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<RepositoryFileRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match repository_file_get(&request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("repo_file_get_failed", error.to_string()),
    }
}

fn review_workspace_list(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context
        .request
        .payload_json::<ListReviewWorkspacesRequest>()
    {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };
    match list_review_workspaces_for_request(&request, &config) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("workspace_list_failed", error.to_string()),
    }
}

fn review_workspace_create(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context
        .request
        .payload_json::<CreateReviewWorkspaceRequest>()
    {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };
    match create_review_workspace_for_request(request, &config) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("workspace_create_failed", error.to_string()),
    }
}

fn review_workspace_get(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<GetReviewWorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };
    match get_review_workspace_for_request(request, &config) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("workspace_get_failed", error.to_string()),
    }
}

fn review_workspace_update(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context
        .request
        .payload_json::<UpdateReviewWorkspaceRequest>()
    {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };
    match update_review_workspace_for_request(request, &config) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("workspace_update_failed", error.to_string()),
    }
}

fn review_workspace_archive(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context
        .request
        .payload_json::<ArchiveReviewWorkspaceRequest>()
    {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };
    match archive_review_workspace_for_request(request, &config) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("workspace_archive_failed", error.to_string()),
    }
}

fn review_workspace_materialize(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context
        .request
        .payload_json::<MaterializeReviewWorkspaceRequest>()
    {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match materialize_review_workspace_for_request(request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("workspace_materialize_failed", error.to_string()),
    }
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

fn plugin_config(context: &NativeServiceContext) -> Result<CodeReviewPluginConfig, ReviewError> {
    context.config_or_default().map_err(ReviewError::Json)
}

fn list_drafts(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<ListDraftsRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };

    match list_drafts_for_request(&request, &config) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("draft_list_failed", error.to_string()),
    }
}

fn save_draft(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<SaveDraftRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };

    match save_draft_for_request(request, &config) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("draft_save_failed", error.to_string()),
    }
}

fn delete_draft(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<DeleteDraftRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };

    match delete_draft_for_request(request, &config) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("draft_delete_failed", error.to_string()),
    }
}

fn update_draft(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<UpdateDraftRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };

    match update_draft_for_request(request, &config) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("draft_update_failed", error.to_string()),
    }
}

fn link_thread_session(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<LinkThreadSessionRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };

    match link_thread_session_for_request(request, &config) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("thread_link_session_failed", error.to_string()),
    }
}

fn review_context_get(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<ReviewContextRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };
    match review_context_for_request(request, &config) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("review_context_failed", error.to_string()),
    }
}

fn review_comments_list(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<ReviewContextRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };
    match review_comments_for_request(request, &config) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("review_comments_failed", error.to_string()),
    }
}

fn review_thread_get(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<GetReviewThreadRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };
    match review_thread_for_request(request, &config) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("review_thread_failed", error.to_string()),
    }
}

fn review_diff_get(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<GetReviewDiffRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match review_diff_for_request(request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("review_diff_failed", error.to_string()),
    }
}

fn review_bundle_get(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<ReviewContextRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };
    match review_bundle_for_request(request, &config) {
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

fn review_publish_preview(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<PublishReviewRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };
    match publish_preview_for_request(request, &config) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("review_publish_preview_failed", error.to_string()),
    }
}

fn review_publish_submit(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<PublishReviewRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let config = match plugin_config(context) {
        Ok(config) => config,
        Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
    };
    match publish_submit_for_request(request, &config) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("review_publish_submit_failed", error.to_string()),
    }
}

fn review_context_for_request(
    request: ReviewContextRequest,
    config: &CodeReviewPluginConfig,
) -> Result<ReviewContextResponse, ReviewError> {
    let summary = create_review_summary(&CreateReviewRequest {
        repo_path: request.repo_path.clone(),
        target: request.target.clone(),
    })?;
    let drafts = list_drafts_for_request(
        &ListDraftsRequest {
            repo_path: request.repo_path,
            target: request.target.clone(),
        },
        config,
    )?
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
    config: &CodeReviewPluginConfig,
) -> Result<ReviewCommentsResponse, ReviewError> {
    let drafts = list_drafts_for_request(
        &ListDraftsRequest {
            repo_path: request.repo_path,
            target: request.target,
        },
        config,
    )?
    .drafts;
    Ok(ReviewCommentsResponse {
        threads: threads_from_drafts(drafts),
    })
}

fn review_thread_for_request(
    request: GetReviewThreadRequest,
    config: &CodeReviewPluginConfig,
) -> Result<ReviewThreadResponse, ReviewError> {
    let summary = create_review_summary(&CreateReviewRequest {
        repo_path: request.repo_path.clone(),
        target: request.target.clone(),
    })?;
    let drafts = list_drafts_for_request(
        &ListDraftsRequest {
            repo_path: request.repo_path,
            target: request.target,
        },
        config,
    )?
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
        |thread| {
            let (_, selected_lines, hunk_context) =
                diff_context_for_anchor(&summary, &thread.anchor);
            (selected_lines, hunk_context)
        },
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

fn review_bundle_for_request(
    request: ReviewContextRequest,
    config: &CodeReviewPluginConfig,
) -> Result<ReviewBundle, ReviewError> {
    let summary = create_review_summary(&CreateReviewRequest {
        repo_path: request.repo_path.clone(),
        target: request.target.clone(),
    })?;
    let review_key = review_key(&summary.repo_root, &request.target)?;
    let threads = threads_from_drafts(
        list_drafts_for_request(
            &ListDraftsRequest {
                repo_path: request.repo_path,
                target: request.target.clone(),
            },
            config,
        )?
        .drafts,
    )
    .into_iter()
    .map(|thread| {
        let (selected_lines, selected_diff_lines, hunk_context) =
            diff_context_for_anchor(&summary, &thread.anchor);
        ReviewBundleThread {
            thread_id: thread.thread_id,
            anchor: thread.anchor,
            comments: thread.comments,
            session_id: thread.session_id,
            selected_lines,
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

trait ReviewPublisher {
    fn manifest(&self) -> ReviewPublisherManifest;

    fn preview(
        &self,
        bundle: &ReviewBundle,
        options: &serde_json::Value,
    ) -> Result<String, ReviewError>;

    fn submit(
        &self,
        bundle: &ReviewBundle,
        options: &serde_json::Value,
    ) -> Result<PublishReviewResponse, ReviewError>;
}

struct MarkdownFilePublisher;

impl ReviewPublisher for MarkdownFilePublisher {
    fn manifest(&self) -> ReviewPublisherManifest {
        ReviewPublisherManifest {
            id: "markdown_file".to_string(),
            label: "Markdown file".to_string(),
            description: "Write a local Markdown review export".to_string(),
            capabilities: file_publisher_capabilities(),
            options_schema: file_publisher_options_schema(),
            route: None,
        }
    }

    fn preview(
        &self,
        bundle: &ReviewBundle,
        _options: &serde_json::Value,
    ) -> Result<String, ReviewError> {
        Ok(publish_markdown(bundle))
    }

    fn submit(
        &self,
        bundle: &ReviewBundle,
        options: &serde_json::Value,
    ) -> Result<PublishReviewResponse, ReviewError> {
        write_file_publish(
            self.manifest().id,
            bundle,
            options,
            "md",
            publish_markdown(bundle),
        )
    }
}

struct JsonFilePublisher;

impl ReviewPublisher for JsonFilePublisher {
    fn manifest(&self) -> ReviewPublisherManifest {
        ReviewPublisherManifest {
            id: "json_file".to_string(),
            label: "JSON file".to_string(),
            description: "Write a local JSON review bundle".to_string(),
            capabilities: file_publisher_capabilities(),
            options_schema: file_publisher_options_schema(),
            route: None,
        }
    }

    fn preview(
        &self,
        bundle: &ReviewBundle,
        _options: &serde_json::Value,
    ) -> Result<String, ReviewError> {
        serde_json::to_string_pretty(bundle).map_err(ReviewError::Json)
    }

    fn submit(
        &self,
        bundle: &ReviewBundle,
        options: &serde_json::Value,
    ) -> Result<PublishReviewResponse, ReviewError> {
        write_file_publish(
            self.manifest().id,
            bundle,
            options,
            "json",
            serde_json::to_string_pretty(bundle)?,
        )
    }
}

fn builtin_review_publishers() -> Vec<Box<dyn ReviewPublisher>> {
    vec![Box::new(MarkdownFilePublisher), Box::new(JsonFilePublisher)]
}

fn builtin_publishers() -> Vec<ReviewPublisherManifest> {
    builtin_review_publishers()
        .into_iter()
        .map(|publisher| publisher.manifest())
        .collect()
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
    config: &CodeReviewPluginConfig,
) -> Result<PublishReviewPreviewResponse, ReviewError> {
    let publisher_id = request.publisher_id.clone();
    let bundle = review_bundle_for_request(
        ReviewContextRequest {
            repo_path: request.repo_path,
            target: request.target,
        },
        config,
    )?;
    let preview = with_publisher(&publisher_id, |publisher| {
        publisher.preview(&bundle, &request.options)
    })?;
    Ok(PublishReviewPreviewResponse {
        publisher_id,
        preview,
    })
}

fn publish_submit_for_request(
    request: PublishReviewRequest,
    config: &CodeReviewPluginConfig,
) -> Result<PublishReviewResponse, ReviewError> {
    let publisher_id = request.publisher_id;
    let bundle = review_bundle_for_request(
        ReviewContextRequest {
            repo_path: request.repo_path,
            target: request.target,
        },
        config,
    )?;
    with_publisher(&publisher_id, |publisher| {
        publisher.submit(&bundle, &request.options)
    })
}

fn with_publisher<T>(
    publisher_id: &str,
    operation: impl FnOnce(&dyn ReviewPublisher) -> Result<T, ReviewError>,
) -> Result<T, ReviewError> {
    for publisher in builtin_review_publishers() {
        if publisher.manifest().id == publisher_id {
            return operation(publisher.as_ref());
        }
    }
    Err(ReviewError::UnsupportedPublisher(publisher_id.to_string()))
}

fn write_file_publish(
    publisher_id: String,
    bundle: &ReviewBundle,
    options: &serde_json::Value,
    extension: &str,
    contents: String,
) -> Result<PublishReviewResponse, ReviewError> {
    let path = publish_output_path(bundle, options, extension);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, contents)?;
    Ok(PublishReviewResponse {
        publisher_id,
        submitted: true,
        output: Some(path.display().to_string()),
        message: format!("wrote review export to {}", path.display()),
    })
}

fn publish_output_path(
    bundle: &ReviewBundle,
    options: &serde_json::Value,
    extension: &str,
) -> PathBuf {
    options
        .get("output_path")
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.is_empty())
        .map_or_else(
            || {
                bundle
                    .repo_root
                    .join(".bcode")
                    .join("reviews")
                    .join(format!(
                        "{}.{}",
                        safe_review_id(&bundle.review_id),
                        extension
                    ))
            },
            PathBuf::from,
        )
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

fn list_review_workspaces_for_request(
    request: &ListReviewWorkspacesRequest,
    config: &CodeReviewPluginConfig,
) -> Result<ListReviewWorkspacesResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let include_archived = request.include_archived;
    let db_repo_root = repo_root.clone();
    let workspaces = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .list_workspaces(&db_repo_root, include_archived)
                .await
        })
    })?;
    Ok(ListReviewWorkspacesResponse { workspaces })
}

fn create_review_workspace_for_request(
    request: CreateReviewWorkspaceRequest,
    config: &CodeReviewPluginConfig,
) -> Result<CreateReviewWorkspaceResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let now = now_ms();
    let title = request
        .title
        .unwrap_or_else(|| "Untitled review".to_string());
    let id = workspace_id(&repo_root, &title, now);
    let workspace = ReviewWorkspace {
        id,
        title,
        repo_root: repo_root.clone(),
        sources: request.sources,
        created_at_ms: Some(now),
        updated_at_ms: Some(now),
        archived_at_ms: None,
    };
    let saved = workspace.clone();
    with_database(&repo_root, config, move |database| {
        Box::pin(async move { CodeReviewDb::new(database).save_workspace(&saved).await })
    })?;
    Ok(CreateReviewWorkspaceResponse { workspace })
}

fn get_review_workspace_for_request(
    request: GetReviewWorkspaceRequest,
    config: &CodeReviewPluginConfig,
) -> Result<GetReviewWorkspaceResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let workspace_id = request.workspace_id;
    let workspace = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .get_workspace(&workspace_id)
                .await
        })
    })?;
    Ok(GetReviewWorkspaceResponse { workspace })
}

fn update_review_workspace_for_request(
    request: UpdateReviewWorkspaceRequest,
    config: &CodeReviewPluginConfig,
) -> Result<UpdateReviewWorkspaceResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let mut workspace = request.workspace;
    workspace.repo_root.clone_from(&repo_root);
    workspace.updated_at_ms = Some(now_ms());
    let saved = workspace.clone();
    with_database(&repo_root, config, move |database| {
        Box::pin(async move { CodeReviewDb::new(database).save_workspace(&saved).await })
    })?;
    Ok(UpdateReviewWorkspaceResponse { workspace })
}

fn archive_review_workspace_for_request(
    request: ArchiveReviewWorkspaceRequest,
    config: &CodeReviewPluginConfig,
) -> Result<ArchiveReviewWorkspaceResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let workspace_id = request.workspace_id;
    let archived = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .archive_workspace(&workspace_id, now_ms())
                .await
        })
    })?;
    Ok(ArchiveReviewWorkspaceResponse { archived })
}

fn materialize_review_workspace_for_request(
    request: MaterializeReviewWorkspaceRequest,
) -> Result<MaterializeReviewWorkspaceResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let mut surfaces = Vec::new();
    let mut additions = 0_u32;
    let mut deletions = 0_u32;
    for source in request
        .workspace
        .sources
        .iter()
        .filter(|source| source.included)
    {
        materialize_source(
            &repo_root,
            source,
            &mut surfaces,
            &mut additions,
            &mut deletions,
        )?;
    }
    Ok(MaterializeReviewWorkspaceResponse {
        materialization: ReviewWorkspaceMaterialization {
            workspace: request.workspace,
            surfaces,
            additions,
            deletions,
        },
    })
}

fn materialize_source(
    repo_root: &Path,
    source: &ReviewSource,
    surfaces: &mut Vec<ReviewSurface>,
    additions: &mut u32,
    deletions: &mut u32,
) -> Result<(), ReviewError> {
    match review_target_from_source_kind(&source.kind) {
        Some(target) => {
            let request = CreateReviewRequest {
                repo_path: repo_root.to_path_buf(),
                target,
            };
            let summary = create_review_summary(&request)?;
            for file in summary.files {
                *additions = additions.saturating_add(file.additions);
                *deletions = deletions.saturating_add(file.deletions);
                let path = file
                    .new_path
                    .clone()
                    .or_else(|| file.old_path.clone())
                    .unwrap_or_default();
                surfaces.push(ReviewSurface {
                    id: surface_id(&source.id, &path, ReviewSurfaceKind::Diff),
                    source_id: source.id.clone(),
                    path,
                    kind: ReviewSurfaceKind::Diff,
                    file: Some(file),
                });
            }
        }
        None => materialize_context_source(repo_root, source, surfaces)?,
    }
    Ok(())
}

fn materialize_context_source(
    repo_root: &Path,
    source: &ReviewSource,
    surfaces: &mut Vec<ReviewSurface>,
) -> Result<(), ReviewError> {
    match &source.kind {
        ReviewSourceKind::File { path } => {
            surfaces.push(file_surface_for_path(repo_root, source, path, None)?);
        }
        ReviewSourceKind::FileRange { path, start, end } => {
            surfaces.push(file_surface_for_path(
                repo_root,
                source,
                path,
                Some((*start, *end)),
            )?);
        }
        ReviewSourceKind::Repository => {
            for file in repository_review_files(repo_root)? {
                let path = file.display_path().to_string();
                surfaces.push(ReviewSurface {
                    id: surface_id(&source.id, &path, ReviewSurfaceKind::File),
                    source_id: source.id.clone(),
                    path,
                    kind: ReviewSurfaceKind::File,
                    file: Some(file),
                });
            }
        }
        ReviewSourceKind::Commit { rev } => surfaces.push(ReviewSurface {
            id: surface_id(&source.id, rev, ReviewSurfaceKind::File),
            source_id: source.id.clone(),
            path: rev.clone(),
            kind: ReviewSurfaceKind::File,
            file: None,
        }),
        ReviewSourceKind::WorkingTreeUnstaged
        | ReviewSourceKind::IndexStaged
        | ReviewSourceKind::WorkingTreeAndIndex
        | ReviewSourceKind::LastCommit
        | ReviewSourceKind::CommitRange { .. }
        | ReviewSourceKind::BranchCompare { .. } => {}
    }
    Ok(())
}

fn file_surface_for_path(
    repo_root: &Path,
    source: &ReviewSource,
    path: &str,
    range: Option<(u32, u32)>,
) -> Result<ReviewSurface, ReviewError> {
    let file = review_file_for_repository_path(repo_root, path, range)?;
    Ok(ReviewSurface {
        id: surface_id(&source.id, path, ReviewSurfaceKind::File),
        source_id: source.id.clone(),
        path: path.to_string(),
        kind: ReviewSurfaceKind::File,
        file: Some(file),
    })
}

fn review_file_for_repository_path(
    repo_root: &Path,
    path: &str,
    range: Option<(u32, u32)>,
) -> Result<ReviewFile, ReviewError> {
    let response = repository_file_get(&RepositoryFileRequest {
        repo_path: repo_root.to_path_buf(),
        file_path: path.to_string(),
    })?;
    let lines = response
        .content
        .as_deref()
        .map_or_else(Vec::new, |content| file_review_lines(content, range));
    Ok(ReviewFile {
        old_path: None,
        new_path: Some(path.to_string()),
        status: ReviewFileStatus::Unknown,
        additions: 0,
        deletions: 0,
        hunks: vec![ReviewHunk {
            old_start: range.map_or(1, |(start, _)| start),
            old_count: u32::try_from(lines.len()).unwrap_or(u32::MAX),
            new_start: range.map_or(1, |(start, _)| start),
            new_count: u32::try_from(lines.len()).unwrap_or(u32::MAX),
            heading: response.unavailable_reason,
            lines,
        }],
        is_binary: response.is_binary,
    })
}

fn file_review_lines(content: &str, range: Option<(u32, u32)>) -> Vec<ReviewLine> {
    content
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line_number = u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX);
            if let Some((start, end)) = range
                && (line_number < start || line_number > end)
            {
                return None;
            }
            Some(ReviewLine {
                kind: ReviewLineKind::Context,
                old_line: Some(line_number),
                new_line: Some(line_number),
                content: line.to_string(),
            })
        })
        .collect()
}

fn review_target_from_source_kind(kind: &ReviewSourceKind) -> Option<ReviewTarget> {
    match kind {
        ReviewSourceKind::WorkingTreeUnstaged => Some(ReviewTarget::WorkingTreeUnstaged),
        ReviewSourceKind::IndexStaged => Some(ReviewTarget::IndexStaged),
        ReviewSourceKind::WorkingTreeAndIndex => Some(ReviewTarget::WorkingTreeAndIndex),
        ReviewSourceKind::LastCommit => Some(ReviewTarget::LastCommit),
        ReviewSourceKind::CommitRange {
            base,
            head,
            merge_base,
        } => Some(ReviewTarget::CommitRange {
            base: base.clone(),
            head: head.clone(),
            merge_base: *merge_base,
        }),
        ReviewSourceKind::BranchCompare {
            base_branch,
            head_branch,
            merge_base,
        } => Some(ReviewTarget::BranchCompare {
            base_branch: base_branch.clone(),
            head_branch: head_branch.clone(),
            merge_base: *merge_base,
        }),
        ReviewSourceKind::Commit { rev } => Some(ReviewTarget::CommitRange {
            base: format!("{rev}^"),
            head: rev.clone(),
            merge_base: false,
        }),
        ReviewSourceKind::File { .. }
        | ReviewSourceKind::FileRange { .. }
        | ReviewSourceKind::Repository => None,
    }
}

fn list_drafts_for_request(
    request: &ListDraftsRequest,
    config: &CodeReviewPluginConfig,
) -> Result<ListDraftsResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let review_key = review_key(&repo_root, &request.target)?;
    let drafts = with_database(&repo_root, config, move |database| {
        Box::pin(async move { CodeReviewDb::new(database).list_drafts(&review_key).await })
    })?;
    Ok(ListDraftsResponse { drafts })
}

fn save_draft_for_request(
    request: SaveDraftRequest,
    config: &CodeReviewPluginConfig,
) -> Result<SaveDraftResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let db_repo_root = repo_root.clone();
    let review_key = review_key(&repo_root, &request.target)?;
    let target_kind = target_kind(&request.target).to_string();
    let target_json = serde_json::to_string(&request.target)?;
    let draft = with_database(&repo_root, config, move |database| {
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
    config: &CodeReviewPluginConfig,
) -> Result<DeleteDraftResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let deleted = with_database(&repo_root, config, move |database| {
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
    config: &CodeReviewPluginConfig,
) -> Result<UpdateDraftResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let result = with_database(&repo_root, config, move |database| {
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
    config: &CodeReviewPluginConfig,
) -> Result<LinkThreadSessionResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let db_repo_root = repo_root.clone();
    let review_key = review_key(&repo_root, &request.target)?;
    let target_kind = target_kind(&request.target).to_string();
    let target_json = serde_json::to_string(&request.target)?;
    let response = with_database(&repo_root, config, move |database| {
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
) -> (Vec<ReviewBundleLine>, Vec<String>, Vec<String>) {
    let Some(file) = summary
        .files
        .iter()
        .find(|file| file.display_path() == anchor.file_path)
    else {
        return (Vec::new(), Vec::new(), Vec::new());
    };
    let lines = bundle_diff_lines(file);
    let rendered_lines: Vec<String> = lines.iter().map(render_bundle_line).collect();
    let start =
        usize::try_from(anchor.start_diff_row.unwrap_or(anchor.diff_row)).unwrap_or(usize::MAX);
    let end = usize::try_from(anchor.end_diff_row.unwrap_or(anchor.diff_row)).unwrap_or(start);
    let selected_structured_lines = lines
        .iter()
        .filter(|line| {
            usize::try_from(line.diff_row).is_ok_and(|index| (start..=end).contains(&index))
        })
        .cloned()
        .collect();
    let selected_lines = rendered_lines
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
            return (selected_structured_lines, selected_lines, hunk_lines);
        }
        row = row.saturating_add(hunk_lines.len());
    }
    (selected_structured_lines, selected_lines, Vec::new())
}

fn bundle_diff_lines(file: &ReviewFile) -> Vec<ReviewBundleLine> {
    let mut diff_row = 0_u64;
    let mut lines = Vec::new();
    for hunk in &file.hunks {
        lines.push(ReviewBundleLine {
            file_path: file.display_path().to_string(),
            kind: ReviewLineKind::Context,
            old_line: None,
            new_line: None,
            diff_row,
            content: format!(
                "@@ -{},{} +{},{} @@{}",
                hunk.old_start,
                hunk.old_count,
                hunk.new_start,
                hunk.new_count,
                hunk.heading
                    .as_ref()
                    .map_or(String::new(), |heading| format!(" {heading}")),
            ),
        });
        diff_row = diff_row.saturating_add(1);
        for line in &hunk.lines {
            lines.push(ReviewBundleLine {
                file_path: file.display_path().to_string(),
                kind: line.kind,
                old_line: line.old_line,
                new_line: line.new_line,
                diff_row,
                content: line.content.clone(),
            });
            diff_row = diff_row.saturating_add(1);
        }
    }
    lines
}

fn render_bundle_line(line: &ReviewBundleLine) -> String {
    let marker = match line.kind {
        ReviewLineKind::Context => ' ',
        ReviewLineKind::Added => '+',
        ReviewLineKind::Removed => '-',
    };
    format!("{marker}{}", line.content)
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

fn repository_file_get(
    request: &RepositoryFileRequest,
) -> Result<RepositoryFileResponse, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let relative_path = Path::new(&request.file_path);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(ReviewError::InvalidRequest(
            "file_path must be repository-relative".to_string(),
        ));
    }
    let path = repo_root.join(relative_path);
    let metadata = std::fs::metadata(&path)?;
    let size_bytes = metadata.len();
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
    if size_bytes > MAX_REPOSITORY_FILE_BYTES {
        return Ok(RepositoryFileResponse {
            file_path: request.file_path.clone(),
            content: None,
            size_bytes,
            mtime_ms,
            is_binary: false,
            unavailable_reason: Some(format!(
                "file is larger than {MAX_REPOSITORY_FILE_BYTES} bytes"
            )),
        });
    }
    let bytes = std::fs::read(&path)?;
    let is_binary = bytes.contains(&0);
    if is_binary {
        return Ok(RepositoryFileResponse {
            file_path: request.file_path.clone(),
            content: None,
            size_bytes,
            mtime_ms,
            is_binary: true,
            unavailable_reason: Some("binary file".to_string()),
        });
    }
    let content = String::from_utf8(bytes).map_err(|error| {
        ReviewError::InvalidRequest(format!("file is not valid UTF-8: {error}"))
    })?;
    Ok(RepositoryFileResponse {
        file_path: request.file_path.clone(),
        content: Some(content),
        size_bytes,
        mtime_ms,
        is_binary: false,
        unavailable_reason: None,
    })
}

fn create_review_summary(request: &CreateReviewRequest) -> Result<ReviewSummary, ReviewError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let files = if matches!(request.target, ReviewTarget::Repository) {
        repository_review_files(&repo_root)?
    } else {
        let diff = diff_for_target(&repo_root, &request.target)?;
        parse_unified_diff(&diff)?
    };
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

    async fn list_workspaces(
        &self,
        repo_root: &Path,
        include_archived: bool,
    ) -> Result<Vec<ReviewWorkspace>, ReviewError> {
        let rows = self
            .db
            .select("review_workspaces")
            .columns(&[
                "workspace_id",
                "repo_root",
                "title",
                "sources_json",
                "created_at_ms",
                "updated_at_ms",
                "archived_at_ms",
            ])
            .filter(Box::new(where_eq(
                "repo_root",
                repo_root.display().to_string(),
            )))
            .execute(self.db)
            .await?;
        let mut workspaces = Vec::new();
        for row in rows {
            let archived_at_ms = optional_i64(&row, "archived_at_ms").map(i64_to_u64);
            if archived_at_ms.is_some() && !include_archived {
                continue;
            }
            workspaces.push(workspace_from_row(&row)?);
        }
        workspaces.sort_by(|left, right| right.updated_at_ms.cmp(&left.updated_at_ms));
        Ok(workspaces)
    }

    async fn get_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<Option<ReviewWorkspace>, ReviewError> {
        let Some(row) = self
            .db
            .select("review_workspaces")
            .columns(&[
                "workspace_id",
                "repo_root",
                "title",
                "sources_json",
                "created_at_ms",
                "updated_at_ms",
                "archived_at_ms",
            ])
            .filter(Box::new(where_eq("workspace_id", workspace_id)))
            .execute_first(self.db)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(workspace_from_row(&row)?))
    }

    async fn save_workspace(&self, workspace: &ReviewWorkspace) -> Result<(), ReviewError> {
        let sources_json = serde_json::to_string(&workspace.sources)?;
        if self.get_workspace(&workspace.id).await?.is_some() {
            self.db
                .update("review_workspaces")
                .value("repo_root", workspace.repo_root.display().to_string())
                .value("title", workspace.title.clone())
                .value("sources_json", sources_json)
                .value(
                    "updated_at_ms",
                    u64_to_i64(workspace.updated_at_ms.unwrap_or_else(now_ms)),
                )
                .value("archived_at_ms", optional_u64(workspace.archived_at_ms))
                .filter(Box::new(where_eq("workspace_id", workspace.id.clone())))
                .execute(self.db)
                .await?;
            return Ok(());
        }
        self.db
            .insert("review_workspaces")
            .value("workspace_id", workspace.id.clone())
            .value("repo_root", workspace.repo_root.display().to_string())
            .value("title", workspace.title.clone())
            .value("sources_json", sources_json)
            .value(
                "created_at_ms",
                u64_to_i64(workspace.created_at_ms.unwrap_or_else(now_ms)),
            )
            .value(
                "updated_at_ms",
                u64_to_i64(workspace.updated_at_ms.unwrap_or_else(now_ms)),
            )
            .value("archived_at_ms", optional_u64(workspace.archived_at_ms))
            .execute(self.db)
            .await?;
        Ok(())
    }

    async fn archive_workspace(&self, workspace_id: &str, now: u64) -> Result<bool, ReviewError> {
        if self.get_workspace(workspace_id).await?.is_none() {
            return Ok(false);
        }
        self.db
            .update("review_workspaces")
            .value("archived_at_ms", u64_to_i64(now))
            .value("updated_at_ms", u64_to_i64(now))
            .filter(Box::new(where_eq("workspace_id", workspace_id.to_string())))
            .execute(self.db)
            .await?;
        Ok(true)
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
                "is_file_anchor",
                "surface_id",
                "source_id",
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
                is_file_anchor: optional_bool(&thread, "is_file_anchor"),
                surface_id: optional_text(&thread, "surface_id"),
                source_id: optional_text(&thread, "source_id"),
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
            .value("is_file_anchor", anchor.is_file_anchor)
            .value("surface_id", anchor.surface_id.clone())
            .value("source_id", anchor.source_id.clone())
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

fn state_paths(repo_root: &Path, config: &CodeReviewPluginConfig) -> StatePaths {
    let state_root = env::var_os(CODE_REVIEW_STATE_DIR_ENV)
        .map_or_else(|| configured_state_root(repo_root, config), PathBuf::from);
    let database_path = state_root.join(DATABASE_FILE_NAME);
    StatePaths {
        state_root,
        database_path,
    }
}

fn configured_state_root(repo_root: &Path, config: &CodeReviewPluginConfig) -> PathBuf {
    if let Some(state_dir) = &config.state_dir {
        if state_dir.is_absolute() {
            return state_dir.clone();
        }
        return repo_root.join(state_dir);
    }
    match config.state_location {
        CodeReviewStateLocation::User => {
            bcode_config::default_state_dir().join(DEFAULT_STATE_SUBDIR)
        }
        CodeReviewStateLocation::Repo => repo_root.join(DEFAULT_REPO_STATE_ROOT),
    }
}

fn with_database<T>(
    repo_root: &Path,
    config: &CodeReviewPluginConfig,
    operation: impl for<'a> FnOnce(
        &'a dyn Database,
    ) -> Pin<Box<dyn Future<Output = Result<T, ReviewError>> + 'a>>
    + Send
    + 'static,
) -> Result<T, ReviewError>
where
    T: Send + 'static,
{
    let paths = state_paths(repo_root, config);
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

fn workspace_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "006_review_workspaces_table".to_string(),
        Box::new(
            create_table("review_workspaces")
                .if_not_exists(true)
                .column(text_column("workspace_id"))
                .column(text_column("repo_root"))
                .column(text_column("title"))
                .column(text_column("sources_json"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .column(nullable_int_column("archived_at_ms"))
                .primary_key("workspace_id"),
        ),
        None,
    )
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
                .column(bool_column("is_file_anchor"))
                .column(nullable_text_column("surface_id"))
                .column(nullable_text_column("source_id"))
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
    source.add_migration(CodeMigration::new(
        "006_thread_file_anchor_column".to_string(),
        Box::new(alter_table("draft_threads").add_column(
            "is_file_anchor".to_string(),
            DataType::Bool,
            false,
            Some(DatabaseValue::Bool(false)),
        )),
        None,
    ));
    source.add_migration(thread_surface_anchor_columns_migration());
    source.add_migration(workspace_table_migration());
    source
}

fn thread_surface_anchor_columns_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "007_thread_surface_anchor_columns".to_string(),
        Box::new(
            alter_table("draft_threads")
                .add_column("surface_id".to_string(), DataType::Text, true, None)
                .add_column("source_id".to_string(), DataType::Text, true, None),
        ),
        None,
    )
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

fn nullable_text_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: true,
        auto_increment: false,
        data_type: DataType::Text,
        default: None,
    }
}

fn bool_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::Bool,
        default: Some(DatabaseValue::Bool(false)),
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

fn workspace_from_row(row: &Row) -> Result<ReviewWorkspace, ReviewError> {
    let sources: Vec<ReviewSource> = serde_json::from_str(&required_text(row, "sources_json")?)?;
    Ok(ReviewWorkspace {
        id: required_text(row, "workspace_id")?,
        title: required_text(row, "title")?,
        repo_root: PathBuf::from(required_text(row, "repo_root")?),
        sources,
        created_at_ms: Some(i64_to_u64(required_i64(row, "created_at_ms")?)),
        updated_at_ms: Some(i64_to_u64(required_i64(row, "updated_at_ms")?)),
        archived_at_ms: optional_i64(row, "archived_at_ms").map(i64_to_u64),
    })
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

fn optional_bool(row: &Row, column: &'static str) -> bool {
    row.get(column)
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn surface_id(source_id: &str, path: &str, kind: ReviewSurfaceKind) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(path.as_bytes());
    hasher.update(b"\0");
    hasher.update(format!("{kind:?}").as_bytes());
    format!("surface-{:x}", hasher.finalize())
}

fn workspace_id(repo_root: &Path, title: &str, now: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_root.display().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(title.as_bytes());
    hasher.update(b"\0");
    hasher.update(now.to_string().as_bytes());
    format!("workspace-{:x}", hasher.finalize())
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
        ReviewTarget::Repository => "repository",
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

fn repository_review_files(repo_root: &Path) -> Result<Vec<ReviewFile>, ReviewError> {
    let output = git_output(repo_root, &["ls-files"])?;
    Ok(output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|path| ReviewFile {
            old_path: None,
            new_path: Some(path.to_string()),
            status: ReviewFileStatus::Unknown,
            additions: 0,
            deletions: 0,
            hunks: Vec::new(),
            is_binary: false,
        })
        .collect())
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
        ReviewTarget::Repository => Ok(String::new()),
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
        ReviewTarget::Repository => "Repository Review".to_string(),
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
    fn default_state_path_uses_user_bcode_state_dir() {
        let config = CodeReviewPluginConfig::default();
        let state_paths = state_paths(Path::new("/repo"), &config);

        assert_eq!(
            state_paths.state_root,
            bcode_config::default_state_dir().join(DEFAULT_STATE_SUBDIR)
        );
    }

    #[test]
    fn repo_state_location_uses_repo_local_directory() {
        let config = CodeReviewPluginConfig {
            state_location: CodeReviewStateLocation::Repo,
            state_dir: None,
        };
        let state_paths = state_paths(Path::new("/repo"), &config);

        assert_eq!(
            state_paths.state_root,
            PathBuf::from("/repo/.bcode/code-review")
        );
    }

    #[test]
    fn explicit_relative_state_dir_is_repo_relative() {
        let config = CodeReviewPluginConfig {
            state_location: CodeReviewStateLocation::User,
            state_dir: Some(PathBuf::from(".bcode/code-review")),
        };
        let state_paths = state_paths(Path::new("/repo"), &config);

        assert_eq!(
            state_paths.state_root,
            PathBuf::from("/repo/.bcode/code-review")
        );
    }

    #[test]
    fn explicit_absolute_state_dir_is_used() {
        let config = CodeReviewPluginConfig {
            state_location: CodeReviewStateLocation::User,
            state_dir: Some(PathBuf::from("/state/code-review")),
        };
        let state_paths = state_paths(Path::new("/repo"), &config);

        assert_eq!(state_paths.state_root, PathBuf::from("/state/code-review"));
    }

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

    #[test]
    fn publisher_registry_lists_builtin_publishers() {
        let publishers = builtin_publishers();

        assert_eq!(publishers.len(), 2);
        assert!(
            publishers
                .iter()
                .any(|publisher| publisher.id == "markdown_file")
        );
        assert!(
            publishers
                .iter()
                .any(|publisher| publisher.id == "json_file")
        );
    }

    #[test]
    fn json_publisher_preview_is_review_bundle_json() {
        let bundle = ReviewBundle {
            review_id: "review-1".to_string(),
            title: "Review".to_string(),
            repo_root: PathBuf::from("/repo"),
            target: ReviewTarget::WorkingTreeUnstaged,
            files: Vec::new(),
            threads: Vec::new(),
            generated_at_ms: 1,
        };

        let preview = with_publisher("json_file", |publisher| {
            publisher.preview(&bundle, &serde_json::json!({}))
        })
        .expect("json preview");
        let decoded: ReviewBundle = serde_json::from_str(&preview).expect("valid bundle json");

        assert_eq!(decoded.review_id, "review-1");
    }

    #[test]
    fn unsupported_publisher_returns_error() {
        let error = with_publisher("missing", |publisher| {
            publisher.preview(
                &ReviewBundle {
                    review_id: "review-1".to_string(),
                    title: "Review".to_string(),
                    repo_root: PathBuf::from("/repo"),
                    target: ReviewTarget::WorkingTreeUnstaged,
                    files: Vec::new(),
                    threads: Vec::new(),
                    generated_at_ms: 1,
                },
                &serde_json::json!({}),
            )
        })
        .expect_err("missing publisher should fail");

        assert!(matches!(error, ReviewError::UnsupportedPublisher(_)));
    }

    #[test]
    fn output_path_override_is_used() {
        let bundle = ReviewBundle {
            review_id: "review-1".to_string(),
            title: "Review".to_string(),
            repo_root: PathBuf::from("/repo"),
            target: ReviewTarget::WorkingTreeUnstaged,
            files: Vec::new(),
            threads: Vec::new(),
            generated_at_ms: 1,
        };
        let path = publish_output_path(
            &bundle,
            &serde_json::json!({ "output_path": "custom/review.json" }),
            "json",
        );

        assert_eq!(path, PathBuf::from("custom/review.json"));
    }
}
