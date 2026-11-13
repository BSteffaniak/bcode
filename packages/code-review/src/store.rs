//! Persisted review state and the reads and publishers built on it.
//!
//! The store keeps workspaces, drafts and threads, AI exchanges, suggestions, publish records,
//! and external imports, and builds whole-review context, bundles, and diffs from them.
//!
//! State lives in one Turso database per state directory (see [`CodeReviewConfig`]). Every
//! call opens the database on a worker thread, runs pending migrations, and closes it again, so
//! the store holds no connection and the daemon plugin and a standalone app can use the same
//! database concurrently.

mod conversation;
mod db;
mod history;

use conversation::{conversation_for_request, save_conversation_for_request};
use history::{list_layer_versions_for_request, record_layer_versions_for_request};

pub(crate) use db::with_database;
use db::{
    CodeReviewDb, now_ms, review_key, review_key_for_scope, surface_id, target_kind, workspace_id,
    workspace_review_key,
};

use bcode_code_review_models::{
    ArchiveReviewWorkspaceRequest, ArchiveReviewWorkspaceResponse, BindReviewAssistantRequest,
    BindReviewAssistantResponse, CreateReviewWorkspaceRequest, CreateReviewWorkspaceResponse,
    DeleteDraftRequest, DeleteDraftResponse, DeleteReviewAiExchangeRequest,
    DeleteReviewAiExchangeResponse, DraftAnchor, DraftComment, ExternalReviewImportRecord,
    GetReviewAssistantRequest, GetReviewAssistantResponse, GetReviewDiffRequest,
    GetReviewThreadRequest, GetReviewWorkspaceRequest, GetReviewWorkspaceResponse,
    LinkThreadSessionRequest, LinkThreadSessionResponse, ListDraftsRequest, ListDraftsResponse,
    ListExternalReviewImportsRequest, ListExternalReviewImportsResponse, ListPublishRecordsRequest,
    ListPublishRecordsResponse, ListReviewAiExchangesRequest, ListReviewAiExchangesResponse,
    ListReviewSuggestionsRequest, ListReviewSuggestionsResponse, ListReviewWorkspacesRequest,
    ListReviewWorkspacesResponse, PublishReviewPreviewResponse, PublishReviewRequest,
    PublishReviewResponse, RepositoryFileRequest, RepositoryFileResponse, ResolveThreadRequest,
    ResolveThreadResponse, ReviewAiExchange, ReviewAiExchangeStatus, ReviewAnchorKind,
    ReviewAssistantBinding, ReviewAssistantLookup, ReviewBundle, ReviewBundleLine,
    ReviewBundleThread, ReviewContextRequest, ReviewFile, ReviewFileStatus, ReviewFileSummary,
    ReviewHunk, ReviewLine, ReviewLineKind, ReviewPublishRecord, ReviewPublisherCapabilities,
    ReviewPublisherManifest, ReviewScope, ReviewSource, ReviewSourceKind, ReviewSuggestion,
    ReviewSuggestionStatus, ReviewSurface, ReviewSurfaceKind, ReviewTarget, ReviewThreadKind,
    ReviewThreadSeverity, ReviewWorkspace, ReviewWorkspaceListItem, SaveDraftRequest,
    SaveDraftResponse, SaveExternalReviewImportRequest, SaveExternalReviewImportResponse,
    SavePublishRecordRequest, SavePublishRecordResponse, SaveReviewAiExchangeRequest,
    SaveReviewAiExchangeResponse, SaveReviewSuggestionRequest, SaveReviewSuggestionResponse,
    UpdateDraftRequest, UpdateDraftResponse, UpdateReviewWorkspaceRequest,
    UpdateReviewWorkspaceResponse,
};
use bcode_path_display::display_from_current_dir;
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
use switchy::schema::version::VersionTracker;
use thiserror::Error;

const CODE_REVIEW_STATE_DIR_ENV: &str = "BCODE_CODE_REVIEW_STATE_DIR";
const DEFAULT_REPO_STATE_ROOT: &str = ".bcode/code-review";
const DEFAULT_STATE_SUBDIR: &str = "code-review";
const DATABASE_FILE_NAME: &str = "code-review.db";
/// Lock file that serializes schema migrations of the state database.
const MIGRATION_LOCK_FILE_NAME: &str = "code-review.db.migrate.lock";
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

/// Where code review state is stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeReviewConfig {
    /// State location preference. Defaults to the user's Bcode state directory.
    #[serde(default)]
    pub state_location: CodeReviewStateLocation,
    /// Explicit state directory path. Relative paths are resolved against the repository root.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
}

/// Typed access to persisted review state under one [`CodeReviewConfig`].
///
/// Each method is one blocking call: it resolves the repository root, opens the state database
/// on a worker thread, runs pending migrations, and performs the operation. Callers on an async
/// runtime should call it from a blocking task.
///
/// # Errors
///
/// Every method returns a [`StoreError`]:
///
/// * [`StoreError::InvalidRequest`] when the repository path is relative, not a directory, or
///   not inside a git worktree, or when a request names records that cannot be combined.
/// * [`StoreError::Connection`], [`StoreError::Database`], [`StoreError::Migration`],
///   [`StoreError::Json`], or [`StoreError::MissingColumn`] when the state database cannot be
///   opened, migrated, queried, or decoded.
/// * [`StoreError::Io`] when the state directory cannot be created.
/// * [`StoreError::Git`] or [`StoreError::Parse`] for operations that read the repository.
/// * [`StoreError::UnsupportedPublisher`] for an unknown publisher id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewStore {
    config: CodeReviewConfig,
}

/// Generates [`ReviewStore`] methods that forward to the request functions.
macro_rules! store_operations {
    ($($(#[$doc:meta])* $name:ident($request:ty) -> $response:ty => $function:ident $(by $by_ref:tt)?;)*) => {
        impl ReviewStore {
            $(
                $(#[$doc])*
                ///
                /// # Errors
                ///
                /// * See [`ReviewStore`].
                pub fn $name(&self, request: $request) -> Result<$response, StoreError> {
                    $function(request, &self.config)
                }
            )*
        }
    };
}

impl ReviewStore {
    /// Store for `config`.
    #[must_use]
    pub const fn new(config: CodeReviewConfig) -> Self {
        Self { config }
    }

    /// The state configuration this store reads and writes.
    #[must_use]
    pub const fn config(&self) -> &CodeReviewConfig {
        &self.config
    }

    /// File diffs of a review target (no stored state is read).
    ///
    /// # Errors
    ///
    /// * See [`ReviewStore`].
    pub fn review_diff(
        &self,
        request: GetReviewDiffRequest,
    ) -> Result<ReviewDiffResponse, StoreError> {
        review_diff_for_request(request)
    }
}

store_operations! {
    /// Record versions of pull requests in the version log.
    record_layer_versions(bcode_code_review_models::RecordLayerVersionsRequest)
        -> bcode_code_review_models::RecordLayerVersionsResponse
        => record_layer_versions_for_request;
    /// Versions of pull requests from the version log.
    list_layer_versions(&bcode_code_review_models::ListLayerVersionsRequest)
        -> bcode_code_review_models::ListLayerVersionsResponse
        => list_layer_versions_for_request;
    /// Workspaces of a repository, newest first.
    list_workspaces(&ListReviewWorkspacesRequest) -> ListReviewWorkspacesResponse
        => list_review_workspaces_for_request;
    /// Create and persist a workspace.
    create_workspace(CreateReviewWorkspaceRequest) -> CreateReviewWorkspaceResponse
        => create_review_workspace_for_request;
    /// One workspace by id.
    get_workspace(GetReviewWorkspaceRequest) -> GetReviewWorkspaceResponse
        => get_review_workspace_for_request;
    /// Replace a stored workspace.
    update_workspace(UpdateReviewWorkspaceRequest) -> UpdateReviewWorkspaceResponse
        => update_review_workspace_for_request;
    /// Archive a workspace.
    archive_workspace(ArchiveReviewWorkspaceRequest) -> ArchiveReviewWorkspaceResponse
        => archive_review_workspace_for_request;
    /// Draft comments of a review scope.
    list_drafts(&ListDraftsRequest) -> ListDraftsResponse => list_drafts_for_request;
    /// Save a draft comment, creating its thread when needed.
    save_draft(SaveDraftRequest) -> SaveDraftResponse => save_draft_for_request;
    /// Delete a draft comment.
    delete_draft(DeleteDraftRequest) -> DeleteDraftResponse => delete_draft_for_request;
    /// Replace a draft comment's body.
    update_draft(UpdateDraftRequest) -> UpdateDraftResponse => update_draft_for_request;
    /// Link a thread to a Bcode session.
    link_thread_session(LinkThreadSessionRequest) -> LinkThreadSessionResponse
        => link_thread_session_for_request;
    /// Resolve or reopen a thread.
    resolve_thread(ResolveThreadRequest) -> ResolveThreadResponse => resolve_thread_for_request;
    /// Set a thread's kind and severity.
    classify_thread(bcode_code_review_models::ClassifyThreadRequest)
        -> bcode_code_review_models::ClassifyThreadResponse => classify_thread_for_request;
    /// AI exchanges of a review scope.
    list_ai_exchanges(&ListReviewAiExchangesRequest) -> ListReviewAiExchangesResponse
        => list_ai_exchanges_for_request;
    /// Save an AI exchange.
    save_ai_exchange(SaveReviewAiExchangeRequest) -> SaveReviewAiExchangeResponse
        => save_ai_exchange_for_request;
    /// Delete an AI exchange; deleting a missing one succeeds.
    delete_ai_exchange(DeleteReviewAiExchangeRequest) -> DeleteReviewAiExchangeResponse
        => delete_ai_exchange_for_request;
    /// Suggestions of a review scope.
    list_suggestions(&ListReviewSuggestionsRequest) -> ListReviewSuggestionsResponse
        => list_suggestions_for_request;
    /// Save a suggestion.
    save_suggestion(SaveReviewSuggestionRequest) -> SaveReviewSuggestionResponse
        => save_suggestion_for_request;
    /// Delete a suggestion.
    delete_suggestion(bcode_code_review_models::DeleteReviewSuggestionRequest)
        -> bcode_code_review_models::DeleteReviewSuggestionResponse
        => delete_suggestion_for_request;
    /// Publish records of a workspace.
    list_publish_records(&ListPublishRecordsRequest) -> ListPublishRecordsResponse
        => list_publish_records_for_request;
    /// Record one publish of a workspace.
    save_publish_record(SavePublishRecordRequest) -> SavePublishRecordResponse
        => save_publish_record_for_request;
    /// External review imports of a workspace.
    list_external_imports(&ListExternalReviewImportsRequest) -> ListExternalReviewImportsResponse
        => list_external_imports_for_request;
    /// Replace a workspace's snapshot from one importer.
    save_external_import(SaveExternalReviewImportRequest) -> SaveExternalReviewImportResponse
        => save_external_import_for_request;
    /// Replace a review's snapshot of its pull request threads.
    save_conversation(bcode_code_review_models::SaveReviewConversationRequest)
        -> bcode_code_review_models::SaveReviewConversationResponse
        => save_conversation_for_request;
    /// A review's snapshot of its pull request threads.
    conversation(&bcode_code_review_models::GetReviewConversationRequest)
        -> bcode_code_review_models::GetReviewConversationResponse
        => conversation_for_request;
    /// Bind a session to the review it assists (or refresh its binding).
    bind_assistant(BindReviewAssistantRequest) -> BindReviewAssistantResponse
        => bind_assistant_for_request;
    /// The binding of a session, or the newest binding of a review.
    assistant_binding(&GetReviewAssistantRequest) -> GetReviewAssistantResponse
        => assistant_binding_for_request;
    /// Summary of a review target with its draft counts.
    review_context(ReviewContextRequest) -> ReviewContextResponse => review_context_for_request;
    /// Draft threads of a review target.
    review_comments(ReviewContextRequest) -> ReviewCommentsResponse
        => review_comments_for_request;
    /// One thread with its selected lines and hunk context.
    review_thread(GetReviewThreadRequest) -> ReviewThreadResponse => review_thread_for_request;
    /// The provider-neutral bundle of a review.
    review_bundle(PublishReviewRequest) -> ReviewBundle => review_bundle_for_request;
    /// Preview publishing a review with a built-in publisher.
    publish_preview(PublishReviewRequest) -> PublishReviewPreviewResponse
        => publish_preview_for_request;
    /// Publish a review with a built-in publisher and record it for its workspace.
    publish_submit(PublishReviewRequest) -> PublishReviewResponse => publish_submit_for_request;
}

/// Summarize the files and line counts of a git review target (no stored state is read).
///
/// # Errors
///
/// * [`StoreError::InvalidRequest`] for an invalid repository path.
/// * [`StoreError::Git`] or [`StoreError::Parse`] when the diff cannot be produced or parsed.
pub fn create_review(request: &CreateReviewRequest) -> Result<ReviewSummary, StoreError> {
    create_review_summary(request)
}

/// Read a bounded range of one repository file.
///
/// # Errors
///
/// * [`StoreError::InvalidRequest`] for an invalid repository path, an unconfined path, or a
///   file above the size bound.
/// * [`StoreError::Io`] when the file cannot be read.
pub fn repository_file(
    request: &RepositoryFileRequest,
) -> Result<RepositoryFileResponse, StoreError> {
    repository_file_get(request)
}

/// Manifests of the built-in publishers (local markdown, markdown file, JSON file).
#[must_use]
pub fn publishers() -> Vec<ReviewPublisherManifest> {
    builtin_publishers()
}

/// Preview a client-built bundle with a built-in publisher. No publish record is saved.
///
/// # Errors
///
/// * [`StoreError::UnsupportedPublisher`] for an unknown publisher id.
/// * [`StoreError::Json`] when the bundle cannot be encoded.
pub fn preview_bundle(
    request: &bcode_code_review_models::PublishBundleRequest,
) -> Result<PublishReviewPreviewResponse, StoreError> {
    let preview = with_publisher(&request.publisher_id, |publisher| {
        publisher.preview(&request.bundle, &request.options)
    })?;
    Ok(PublishReviewPreviewResponse {
        publisher_id: request.publisher_id.clone(),
        preview,
    })
}

/// Submit a client-built bundle with a built-in publisher. No publish record is saved; the
/// caller records each published bundle with [`ReviewStore::save_publish_record`].
///
/// # Errors
///
/// * [`StoreError::UnsupportedPublisher`] for an unknown publisher id.
/// * [`StoreError::Io`] or [`StoreError::Json`] when the export cannot be written.
pub fn submit_bundle(
    request: &bcode_code_review_models::PublishBundleRequest,
) -> Result<PublishReviewResponse, StoreError> {
    with_publisher(&request.publisher_id, |publisher| {
        publisher.submit(&request.bundle, &request.options)
    })
}

/// Request payload for [`create_review`].
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
    /// Resolution timestamp in milliseconds since Unix epoch, when resolved.
    #[serde(default)]
    pub resolved_at_ms: Option<u64>,
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

/// Errors returned by [`ReviewStore`] operations and the review reads built on the store.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The request was malformed (for example a relative repository path).
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// A git command failed.
    #[error("Git command failed: {0}")]
    Git(String),
    /// A unified diff could not be parsed.
    #[error("failed to parse diff: {0}")]
    Parse(String),
    /// The state database could not be opened.
    #[error("database connection failed: {0}")]
    Connection(#[from] switchy::database_connection::InitTursoError),
    /// A database query failed.
    #[error("database operation failed: {0}")]
    Database(#[from] DatabaseError),
    /// A schema migration failed.
    #[error("database migration failed: {0}")]
    Migration(#[from] switchy::schema::MigrationError),
    /// A stored or requested JSON value could not be encoded or decoded.
    #[error("serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    /// A database row lacks a required column.
    #[error("database row is missing column {0}")]
    MissingColumn(&'static str),
    /// A file or directory operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// No built-in publisher has the requested id.
    #[error("unsupported review publisher: {0}")]
    UnsupportedPublisher(String),
}

fn review_context_for_request(
    request: ReviewContextRequest,
    config: &CodeReviewConfig,
) -> Result<ReviewContextResponse, StoreError> {
    let summary = create_review_summary(&CreateReviewRequest {
        repo_path: request.repo_path.clone(),
        target: request.target.clone(),
    })?;
    let drafts = list_drafts_for_request(
        &ListDraftsRequest {
            repo_path: request.repo_path,
            target: request.target.clone(),
            scope: None,
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
    config: &CodeReviewConfig,
) -> Result<ReviewCommentsResponse, StoreError> {
    let drafts = list_drafts_for_request(
        &ListDraftsRequest {
            repo_path: request.repo_path,
            target: request.target,
            scope: None,
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
    config: &CodeReviewConfig,
) -> Result<ReviewThreadResponse, StoreError> {
    let summary = create_review_summary(&CreateReviewRequest {
        repo_path: request.repo_path.clone(),
        target: request.target.clone(),
    })?;
    let drafts = list_drafts_for_request(
        &ListDraftsRequest {
            repo_path: request.repo_path,
            target: request.target,
            scope: None,
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
) -> Result<ReviewDiffResponse, StoreError> {
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
    request: PublishReviewRequest,
    config: &CodeReviewConfig,
) -> Result<ReviewBundle, StoreError> {
    let workspace = request.workspace.clone();
    let (summary, surfaces) = if let Some(workspace) = workspace.clone() {
        let materialization = materialize_review_workspace(&request.repo_path, workspace)?;
        (
            review_summary_from_materialization(&materialization),
            materialization.surfaces,
        )
    } else {
        (
            create_review_summary(&CreateReviewRequest {
                repo_path: request.repo_path.clone(),
                target: request.target.clone(),
            })?,
            Vec::new(),
        )
    };
    let review_key = workspace.as_ref().map_or_else(
        || review_key(&summary.repo_root, &request.target),
        |workspace| Ok(workspace_review_key(workspace)),
    )?;
    let scope = workspace.as_ref().map(|workspace| ReviewScope::Workspace {
        workspace_id: crate::scope::annotation_workspace_id(workspace),
        target: request.target.clone(),
    });
    let excluded_thread_anchors = request.excluded_thread_anchors;
    let threads = threads_from_drafts(
        list_drafts_for_request(
            &ListDraftsRequest {
                repo_path: request.repo_path.clone(),
                target: request.target.clone(),
                scope,
            },
            config,
        )?
        .drafts,
    )
    .into_iter()
    .filter(|thread| !excluded_thread_anchors.contains(&thread.anchor))
    .map(|thread| {
        let (mut selected_lines, selected_diff_lines, hunk_context) =
            context_for_anchor(&summary, &thread.anchor);
        hydrate_bundle_line_anchor_metadata(&mut selected_lines, &thread.anchor);
        ReviewBundleThread {
            summary_only: false,
            thread_id: thread.thread_id,
            anchor: thread.anchor,
            comments: thread.comments,
            session_id: thread.session_id,
            resolved_at_ms: thread.resolved_at_ms,
            selected_lines,
            selected_diff_lines,
            hunk_context,
        }
    })
    .collect();
    Ok(ReviewBundle {
        pull_request: None,
        review_id: review_key,
        title: summary.title,
        repo_root: summary.repo_root,
        target: request.target,
        surfaces,
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

fn review_summary_from_materialization(
    materialization: &WorkspaceMaterialization,
) -> ReviewSummary {
    ReviewSummary {
        title: materialization.workspace.title.clone(),
        repo_root: materialization.workspace.repo_root.clone(),
        files: materialization
            .surfaces
            .iter()
            .filter_map(|surface| surface.file.clone())
            .collect(),
        additions: materialization.additions,
        deletions: materialization.deletions,
    }
}

trait ReviewPublisher {
    fn manifest(&self) -> ReviewPublisherManifest;

    fn preview(
        &self,
        bundle: &ReviewBundle,
        options: &serde_json::Value,
    ) -> Result<String, StoreError>;

    fn submit(
        &self,
        bundle: &ReviewBundle,
        options: &serde_json::Value,
    ) -> Result<PublishReviewResponse, StoreError>;
}

struct LocalMarkdownPublisher;

impl ReviewPublisher for LocalMarkdownPublisher {
    fn manifest(&self) -> ReviewPublisherManifest {
        ReviewPublisherManifest {
            pull_request_provider: None,
            id: "local_markdown".to_string(),
            label: "Local review markdown".to_string(),
            description: "Write a local-first Markdown review notebook export".to_string(),
            capabilities: file_publisher_capabilities(),
            options_schema: file_publisher_options_schema(),
            route: None,
        }
    }

    fn preview(
        &self,
        bundle: &ReviewBundle,
        _options: &serde_json::Value,
    ) -> Result<String, StoreError> {
        Ok(publish_local_markdown(bundle))
    }

    fn submit(
        &self,
        bundle: &ReviewBundle,
        options: &serde_json::Value,
    ) -> Result<PublishReviewResponse, StoreError> {
        write_file_publish(
            self.manifest().id,
            bundle,
            options,
            "md",
            publish_local_markdown(bundle),
        )
    }
}

struct MarkdownFilePublisher;

impl ReviewPublisher for MarkdownFilePublisher {
    fn manifest(&self) -> ReviewPublisherManifest {
        ReviewPublisherManifest {
            pull_request_provider: None,
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
    ) -> Result<String, StoreError> {
        Ok(publish_markdown(bundle))
    }

    fn submit(
        &self,
        bundle: &ReviewBundle,
        options: &serde_json::Value,
    ) -> Result<PublishReviewResponse, StoreError> {
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
            pull_request_provider: None,
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
    ) -> Result<String, StoreError> {
        serde_json::to_string_pretty(bundle).map_err(StoreError::Json)
    }

    fn submit(
        &self,
        bundle: &ReviewBundle,
        options: &serde_json::Value,
    ) -> Result<PublishReviewResponse, StoreError> {
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
    vec![
        Box::new(LocalMarkdownPublisher),
        Box::new(MarkdownFilePublisher),
        Box::new(JsonFilePublisher),
    ]
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
    config: &CodeReviewConfig,
) -> Result<PublishReviewPreviewResponse, StoreError> {
    let publisher_id = request.publisher_id.clone();
    let options = request.options.clone();
    let bundle = review_bundle_for_request(request, config)?;
    let preview = with_publisher(&publisher_id, |publisher| {
        publisher.preview(&bundle, &options)
    })?;
    Ok(PublishReviewPreviewResponse {
        publisher_id,
        preview,
    })
}

fn list_external_imports_for_request(
    request: &ListExternalReviewImportsRequest,
    config: &CodeReviewConfig,
) -> Result<ListExternalReviewImportsResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let workspace_id = request.workspace_id.clone();
    let records = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .list_external_imports(&workspace_id)
                .await
        })
    })?;
    Ok(ListExternalReviewImportsResponse { records })
}

fn save_external_import_for_request(
    request: SaveExternalReviewImportRequest,
    config: &CodeReviewConfig,
) -> Result<SaveExternalReviewImportResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let workspace_id = request.workspace_id;
    let importer_id = request.import.importer_id;
    let record = ExternalReviewImportRecord {
        workspace_id,
        importer_id,
        threads: request.import.threads,
        warnings: request.import.warnings,
        updated_at_ms: now_ms(),
    };
    let record_to_save = record.clone();
    with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .save_external_import(&record_to_save)
                .await
        })
    })?;
    Ok(SaveExternalReviewImportResponse { record })
}

fn list_publish_records_for_request(
    request: &ListPublishRecordsRequest,
    config: &CodeReviewConfig,
) -> Result<ListPublishRecordsResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let workspace_id = request.workspace_id.clone();
    let records = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .list_publish_records(&workspace_id)
                .await
        })
    })?;
    Ok(ListPublishRecordsResponse { records })
}

fn publish_submit_for_request(
    request: PublishReviewRequest,
    config: &CodeReviewConfig,
) -> Result<PublishReviewResponse, StoreError> {
    let publisher_id = request.publisher_id.clone();
    let options = request.options.clone();
    let bundle = review_bundle_for_request(request.clone(), config)?;
    let response = with_publisher(&publisher_id, |publisher| {
        publisher.submit(&bundle, &options)
    })?;
    if let Some(workspace) = request.workspace {
        let record = ReviewPublishRecord {
            id: publish_record_id(&workspace.id, &publisher_id, response.submitted),
            workspace_id: Some(workspace.id.clone()),
            review_id: bundle.review_id,
            publisher_id: response.publisher_id.clone(),
            submitted: response.submitted,
            published_thread_anchors: bundle
                .threads
                .iter()
                .map(|thread| thread.anchor.clone())
                .collect(),
            output: response.output.clone(),
            message: response.message.clone(),
            created_at_ms: now_ms(),
        };
        with_database(&workspace.repo_root, config, move |database| {
            Box::pin(async move {
                CodeReviewDb::new(database)
                    .save_publish_record(&record)
                    .await
            })
        })?;
    }
    Ok(response)
}

fn save_publish_record_for_request(
    request: SavePublishRecordRequest,
    config: &CodeReviewConfig,
) -> Result<SavePublishRecordResponse, StoreError> {
    let record = ReviewPublishRecord {
        id: publish_record_id(
            &request.workspace.id,
            &request.publisher_id,
            request.submitted,
        ),
        workspace_id: Some(request.workspace.id.clone()),
        review_id: request.review_id,
        publisher_id: request.publisher_id,
        submitted: request.submitted,
        published_thread_anchors: request.published_thread_anchors,
        output: request.output,
        message: request.message,
        created_at_ms: now_ms(),
    };
    let record_to_save = record.clone();
    with_database(&request.workspace.repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .save_publish_record(&record_to_save)
                .await
        })
    })?;
    Ok(SavePublishRecordResponse { record })
}

fn publish_record_id(workspace_id: &str, publisher_id: &str, submitted: bool) -> String {
    let status = if submitted { "submitted" } else { "skipped" };
    format!(
        "publish-{workspace_id}-{publisher_id}-{status}-{}",
        now_ms()
    )
}

fn with_publisher<T>(
    publisher_id: &str,
    operation: impl FnOnce(&dyn ReviewPublisher) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    for publisher in builtin_review_publishers() {
        if publisher.manifest().id == publisher_id {
            return operation(publisher.as_ref());
        }
    }
    Err(StoreError::UnsupportedPublisher(publisher_id.to_string()))
}

fn write_file_publish(
    publisher_id: String,
    bundle: &ReviewBundle,
    options: &serde_json::Value,
    extension: &str,
    contents: String,
) -> Result<PublishReviewResponse, StoreError> {
    let path = publish_output_path(bundle, options, extension);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, contents)?;
    Ok(PublishReviewResponse {
        publisher_id,
        submitted: true,
        output: Some(path.display().to_string()),
        message: format!("wrote review export to {}", display_from_current_dir(&path)),
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

fn publish_local_markdown(bundle: &ReviewBundle) -> String {
    let mut output = String::new();
    let _ = write!(output, "# Local review: {}\n\n", bundle.title);
    let _ = writeln!(output, "* Review id: `{}`", bundle.review_id);
    let _ = writeln!(
        output,
        "* Repository: `{}`",
        display_from_current_dir(&bundle.repo_root)
    );
    let _ = writeln!(output, "* Generated: `{}`", bundle.generated_at_ms);
    let _ = writeln!(output, "* Files: `{}`", bundle.files.len());
    let open_threads = bundle
        .threads
        .iter()
        .filter(|thread| thread.resolved_at_ms.is_none())
        .count();
    let resolved_threads = bundle.threads.len().saturating_sub(open_threads);
    let comment_count = bundle
        .threads
        .iter()
        .map(|thread| thread.comments.len())
        .sum::<usize>();
    let _ = writeln!(output, "* Open threads: `{open_threads}`");
    let _ = writeln!(output, "* Resolved threads: `{resolved_threads}`");
    let _ = write!(output, "* Local comments: `{comment_count}`\n\n");

    if !bundle.files.is_empty() {
        output.push_str("## Files\n\n");
        for file in &bundle.files {
            let _ = writeln!(
                output,
                "* `{}` {:?} +{} -{}",
                file.path, file.status, file.additions, file.deletions
            );
        }
        output.push('\n');
    }

    if bundle.threads.is_empty() {
        output.push_str("## Local review threads\n\nNo local review threads.\n");
        return output;
    }

    let review_threads = bundle
        .threads
        .iter()
        .filter(|thread| thread.anchor.kind == ReviewAnchorKind::Review)
        .collect::<Vec<_>>();
    if !review_threads.is_empty() {
        output.push_str("## Review-level threads\n\n");
        for thread in review_threads {
            push_local_markdown_thread(&mut output, thread);
        }
    }

    let file_threads = bundle
        .threads
        .iter()
        .filter(|thread| thread.anchor.kind != ReviewAnchorKind::Review)
        .collect::<Vec<_>>();
    if file_threads.is_empty() {
        return output;
    }

    output.push_str("## File threads\n\n");
    for thread in file_threads {
        push_local_markdown_thread(&mut output, thread);
    }
    output
}

fn push_local_markdown_thread(output: &mut String, thread: &ReviewBundleThread) {
    let status = if thread.resolved_at_ms.is_some() {
        "resolved"
    } else {
        "open"
    };
    let heading = if thread.anchor.kind == ReviewAnchorKind::Review {
        "Review".to_string()
    } else {
        format!(
            "`{}` {}",
            thread.anchor.file_path,
            anchor_label(&thread.anchor)
        )
    };
    let _ = write!(output, "### {heading} ({status})\n\n");
    let _ = writeln!(output, "* Thread id: `{}`", thread.thread_id);
    if let Some(session_id) = &thread.session_id {
        let _ = writeln!(output, "* Bcode session: `{session_id}`");
    }
    if let Some(resolved_at_ms) = thread.resolved_at_ms {
        let _ = writeln!(output, "* Resolved at: `{resolved_at_ms}`");
    }
    output.push('\n');
    for comment in &thread.comments {
        let _ = writeln!(output, "#### Comment `{}`\n", comment.comment_id);
        output.push_str(&comment.body);
        output.push_str("\n\n");
    }
    if !thread.selected_diff_lines.is_empty() {
        output.push_str("Context:\n\n```diff\n");
        output.push_str(&thread.selected_diff_lines.join("\n"));
        output.push_str("\n```\n\n");
    }
}

fn publish_markdown(bundle: &ReviewBundle) -> String {
    let mut output = String::new();
    let _ = write!(output, "# {}\n\n", bundle.title);
    let _ = writeln!(output, "* Review id: `{}`", bundle.review_id);
    let _ = writeln!(
        output,
        "* Repository: `{}`",
        display_from_current_dir(&bundle.repo_root)
    );
    let _ = writeln!(output, "* Generated: `{}`", bundle.generated_at_ms);
    if !bundle.surfaces.is_empty() {
        let _ = writeln!(output, "* Surfaces: `{}`", bundle.surfaces.len());
    }
    let _ = write!(output, "* Threads: `{}`\n\n", bundle.threads.len());
    if !bundle.surfaces.is_empty() {
        output.push_str("## Review surfaces\n\n");
        for surface in &bundle.surfaces {
            let _ = writeln!(
                output,
                "* `{}` `{}` from source `{}`",
                surface_kind_label(surface.kind),
                surface.path,
                surface.source_id
            );
        }
        output.push('\n');
    }
    for thread in &bundle.threads {
        let _ = write!(
            output,
            "## {} @ {}\n\n",
            thread.anchor.file_path,
            anchor_label(&thread.anchor)
        );
        if let Some(surface_id) = &thread.anchor.surface_id {
            let _ = writeln!(output, "* Surface: `{surface_id}`");
        }
        if let Some(source_id) = &thread.anchor.source_id {
            let _ = writeln!(output, "* Source: `{source_id}`");
        }
        if thread.anchor.is_file_anchor {
            output.push_str("* Anchor kind: `file`\n");
        }
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

const fn surface_kind_label(kind: ReviewSurfaceKind) -> &'static str {
    match kind {
        ReviewSurfaceKind::Diff => "diff",
        ReviewSurfaceKind::File => "file",
    }
}

fn anchor_label(anchor: &DraftAnchor) -> String {
    match anchor.kind {
        ReviewAnchorKind::Review => "review".to_string(),
        ReviewAnchorKind::File => "file".to_string(),
        ReviewAnchorKind::Range => {
            let start = anchor.start_diff_row.unwrap_or(anchor.diff_row);
            let end = anchor.end_diff_row.unwrap_or(anchor.diff_row);
            if start == end {
                format!("row {start}")
            } else {
                format!("rows {start}-{end}")
            }
        }
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
    config: &CodeReviewConfig,
) -> Result<ListReviewWorkspacesResponse, StoreError> {
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
    let items = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            let db = CodeReviewDb::new(database);
            let mut items = Vec::new();
            for mut workspace in workspaces {
                let review_key = workspace_review_key(&workspace);
                let mut counts = db.draft_counts(&review_key).await?;
                if let Some(legacy_review_key) = workspace_legacy_review_key(&workspace)? {
                    let legacy = db.draft_counts(&legacy_review_key).await?;
                    if counts.1 == 0 && legacy.1 > 0 {
                        counts = legacy;
                    }
                }
                let (thread_count, draft_count, drafts_updated) = counts;
                // A review is updated when its comments change, not only its own record.
                workspace.updated_at_ms = workspace.updated_at_ms.max(drafts_updated);
                let last_publish = db.last_publish_record(Some(&workspace.id)).await?;
                items.push(ReviewWorkspaceListItem {
                    workspace,
                    thread_count,
                    draft_count,
                    last_publish,
                });
            }
            items.sort_by_key(|item| std::cmp::Reverse(item.workspace.updated_at_ms));
            Ok(items)
        })
    })?;
    let workspaces = items.iter().map(|item| item.workspace.clone()).collect();
    Ok(ListReviewWorkspacesResponse { workspaces, items })
}

fn create_review_workspace_for_request(
    request: CreateReviewWorkspaceRequest,
    config: &CodeReviewConfig,
) -> Result<CreateReviewWorkspaceResponse, StoreError> {
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
        viewed_files: BTreeSet::new(),
        archived_at_ms: None,
        viewed_marks: Vec::new(),
        viewed_threads: Vec::new(),
        reviewed_revisions: std::collections::BTreeMap::new(),
        presentation: None,
        stack_snapshots: Vec::new(),
    };
    let saved = workspace.clone();
    with_database(&repo_root, config, move |database| {
        Box::pin(async move { CodeReviewDb::new(database).save_workspace(&saved).await })
    })?;
    Ok(CreateReviewWorkspaceResponse { workspace })
}

fn get_review_workspace_for_request(
    request: GetReviewWorkspaceRequest,
    config: &CodeReviewConfig,
) -> Result<GetReviewWorkspaceResponse, StoreError> {
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
    config: &CodeReviewConfig,
) -> Result<UpdateReviewWorkspaceResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let mut workspace = request.workspace;
    workspace.repo_root.clone_from(&repo_root);
    let saved = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            let db = CodeReviewDb::new(database);
            // Presentation state (scroll position, panels) records how the review was last
            // looked at; only changes to the review itself make it updated.
            let stored = db.get_workspace(&workspace.id).await?;
            workspace.updated_at_ms = match stored {
                Some(stored) if same_review(&stored, &workspace) => stored.updated_at_ms,
                _ => Some(now_ms()),
            };
            db.save_workspace(&workspace).await?;
            Ok(workspace)
        })
    })?;
    Ok(UpdateReviewWorkspaceResponse { workspace: saved })
}

/// Whether `a` and `b` are the same review apart from timestamps and presentation state.
fn same_review(a: &ReviewWorkspace, b: &ReviewWorkspace) -> bool {
    a.title == b.title
        && a.sources == b.sources
        && a.viewed_files == b.viewed_files
        && a.viewed_marks == b.viewed_marks
        && a.reviewed_revisions == b.reviewed_revisions
        && a.archived_at_ms == b.archived_at_ms
}

fn archive_review_workspace_for_request(
    request: ArchiveReviewWorkspaceRequest,
    config: &CodeReviewConfig,
) -> Result<ArchiveReviewWorkspaceResponse, StoreError> {
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

/// A workspace's included sources materialized into reviewable surfaces (for review bundles).
#[derive(Debug)]
struct WorkspaceMaterialization {
    /// Source workspace.
    workspace: ReviewWorkspace,
    /// Reviewable surfaces produced from included sources.
    surfaces: Vec<ReviewSurface>,
    /// Total added lines across diff surfaces.
    additions: u32,
    /// Total removed lines across diff surfaces.
    deletions: u32,
}

fn materialize_review_workspace(
    repo_path: &Path,
    workspace: ReviewWorkspace,
) -> Result<WorkspaceMaterialization, StoreError> {
    let repo_root = resolve_repo_root(repo_path)?;
    let mut surfaces = Vec::new();
    let mut additions = 0_u32;
    let mut deletions = 0_u32;
    for source in workspace.sources.iter().filter(|source| source.included) {
        // A source that fails contributes no surfaces; the bundle keeps the others.
        let _ = materialize_source(
            &repo_root,
            source,
            &mut surfaces,
            &mut additions,
            &mut deletions,
        );
    }
    Ok(WorkspaceMaterialization {
        workspace,
        surfaces,
        additions,
        deletions,
    })
}

fn materialize_source(
    repo_root: &Path,
    source: &ReviewSource,
    surfaces: &mut Vec<ReviewSurface>,
    additions: &mut u32,
    deletions: &mut u32,
) -> Result<(), StoreError> {
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
) -> Result<(), StoreError> {
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
        ReviewSourceKind::Commits { .. }
        | ReviewSourceKind::PullRequest { .. }
        | ReviewSourceKind::StackLayer { .. }
        | ReviewSourceKind::Stack { .. }
        | ReviewSourceKind::CurrentBranch { .. }
        | ReviewSourceKind::Unsupported { .. } => {
            return Err(StoreError::InvalidRequest(format!(
                "source kind '{}' is not supported by this review view",
                source.kind.label()
            )));
        }
    }
    Ok(())
}

fn file_surface_for_path(
    repo_root: &Path,
    source: &ReviewSource,
    path: &str,
    range: Option<(u32, u32)>,
) -> Result<ReviewSurface, StoreError> {
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
) -> Result<ReviewFile, StoreError> {
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
        | ReviewSourceKind::Repository
        | ReviewSourceKind::Commits { .. }
        | ReviewSourceKind::PullRequest { .. }
        | ReviewSourceKind::StackLayer { .. }
        | ReviewSourceKind::Stack { .. }
        | ReviewSourceKind::CurrentBranch { .. }
        | ReviewSourceKind::Unsupported { .. } => None,
    }
}

fn workspace_legacy_review_key(workspace: &ReviewWorkspace) -> Result<Option<String>, StoreError> {
    let Some(target) = workspace
        .sources
        .iter()
        .find(|source| source.included)
        .and_then(|source| review_target_from_source_kind(&source.kind))
    else {
        return Ok(None);
    };
    review_key(&workspace.repo_root, &target).map(Some)
}

fn list_drafts_for_request(
    request: &ListDraftsRequest,
    config: &CodeReviewConfig,
) -> Result<ListDraftsResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let review_key = review_key_for_scope(&repo_root, &request.target, request.scope.as_ref())?;
    let drafts = with_database(&repo_root, config, move |database| {
        Box::pin(async move { CodeReviewDb::new(database).list_drafts(&review_key).await })
    })?;
    Ok(ListDraftsResponse { drafts })
}

fn bind_assistant_for_request(
    request: BindReviewAssistantRequest,
    config: &CodeReviewConfig,
) -> Result<BindReviewAssistantResponse, StoreError> {
    let binding = request.binding;
    if binding.schema_version != bcode_code_review_models::REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION {
        return Err(StoreError::InvalidRequest(format!(
            "unsupported review assistant binding schema version: {}",
            binding.schema_version
        )));
    }
    if binding.session_id.trim().is_empty() {
        return Err(StoreError::InvalidRequest(
            "a review assistant binding needs a session id".to_owned(),
        ));
    }
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let db_repo_root = repo_root.clone();
    let workspace_id = crate::scope::annotation_workspace_id(&binding.workspace);
    let binding = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .save_assistant_binding(&workspace_id, &db_repo_root, binding)
                .await
        })
    })?;
    Ok(BindReviewAssistantResponse { binding })
}

fn assistant_binding_for_request(
    request: &GetReviewAssistantRequest,
    config: &CodeReviewConfig,
) -> Result<GetReviewAssistantResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let lookup = request.lookup.clone();
    let binding = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            let db = CodeReviewDb::new(database);
            match lookup {
                ReviewAssistantLookup::Session { session_id } => {
                    db.assistant_binding_for_session(&session_id).await
                }
                ReviewAssistantLookup::Review { workspace_id } => {
                    db.assistant_binding_for_review(&workspace_id).await
                }
            }
        })
    })?;
    Ok(GetReviewAssistantResponse { binding })
}

fn list_ai_exchanges_for_request(
    request: &ListReviewAiExchangesRequest,
    config: &CodeReviewConfig,
) -> Result<ListReviewAiExchangesResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let review_key = review_key_for_scope(&repo_root, &request.target, request.scope.as_ref())?;
    let exchanges = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .list_ai_exchanges(&review_key)
                .await
        })
    })?;
    Ok(ListReviewAiExchangesResponse { exchanges })
}

fn save_ai_exchange_for_request(
    request: SaveReviewAiExchangeRequest,
    config: &CodeReviewConfig,
) -> Result<SaveReviewAiExchangeResponse, StoreError> {
    if request.exchange.schema_version
        != bcode_code_review_models::REVIEW_AI_EXCHANGE_SCHEMA_VERSION
    {
        return Err(StoreError::InvalidRequest(format!(
            "unsupported review AI exchange schema version: {}",
            request.exchange.schema_version
        )));
    }
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let db_repo_root = repo_root.clone();
    let review_key = review_key_for_scope(&repo_root, &request.target, request.scope.as_ref())?;
    let target_kind = target_kind(&request.target).to_string();
    let target_json = serde_json::to_string(&request.target)?;
    let exchange = request.exchange;
    let exchange = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .save_ai_exchange(
                    &review_key,
                    &db_repo_root,
                    &target_kind,
                    &target_json,
                    exchange,
                )
                .await
        })
    })?;
    Ok(SaveReviewAiExchangeResponse { exchange })
}

fn delete_ai_exchange_for_request(
    request: DeleteReviewAiExchangeRequest,
    config: &CodeReviewConfig,
) -> Result<DeleteReviewAiExchangeResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let review_key = review_key_for_scope(&repo_root, &request.target, request.scope.as_ref())?;
    let exchange_id = request.exchange_id;
    let deleted = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .delete_ai_exchange(&review_key, &exchange_id)
                .await
        })
    })?;
    Ok(DeleteReviewAiExchangeResponse { deleted })
}

fn delete_suggestion_for_request(
    request: bcode_code_review_models::DeleteReviewSuggestionRequest,
    config: &CodeReviewConfig,
) -> Result<bcode_code_review_models::DeleteReviewSuggestionResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let review_key = review_key_for_scope(&repo_root, &request.target, request.scope.as_ref())?;
    let suggestion_id = request.suggestion_id;
    let deleted = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .delete_suggestion(&review_key, &suggestion_id)
                .await
        })
    })?;
    Ok(bcode_code_review_models::DeleteReviewSuggestionResponse { deleted })
}

fn list_suggestions_for_request(
    request: &ListReviewSuggestionsRequest,
    config: &CodeReviewConfig,
) -> Result<ListReviewSuggestionsResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let review_key = review_key_for_scope(&repo_root, &request.target, request.scope.as_ref())?;
    let suggestions = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .list_suggestions(&review_key)
                .await
        })
    })?;
    Ok(ListReviewSuggestionsResponse { suggestions })
}

fn save_suggestion_for_request(
    request: SaveReviewSuggestionRequest,
    config: &CodeReviewConfig,
) -> Result<SaveReviewSuggestionResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let db_repo_root = repo_root.clone();
    let review_key = review_key_for_scope(&repo_root, &request.target, request.scope.as_ref())?;
    let target_kind = target_kind(&request.target).to_string();
    let target_json = serde_json::to_string(&request.target)?;
    let suggestion = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .save_suggestion(
                    &review_key,
                    &db_repo_root,
                    &target_kind,
                    &target_json,
                    request.suggestion,
                )
                .await
        })
    })?;
    Ok(SaveReviewSuggestionResponse { suggestion })
}

fn save_draft_for_request(
    request: SaveDraftRequest,
    config: &CodeReviewConfig,
) -> Result<SaveDraftResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let db_repo_root = repo_root.clone();
    let review_key = review_key_for_scope(&repo_root, &request.target, request.scope.as_ref())?;
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
    config: &CodeReviewConfig,
) -> Result<DeleteDraftResponse, StoreError> {
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
    config: &CodeReviewConfig,
) -> Result<UpdateDraftResponse, StoreError> {
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
    config: &CodeReviewConfig,
) -> Result<LinkThreadSessionResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let db_repo_root = repo_root.clone();
    let review_key = review_key_for_scope(&repo_root, &request.target, request.scope.as_ref())?;
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

fn resolve_thread_for_request(
    request: ResolveThreadRequest,
    config: &CodeReviewConfig,
) -> Result<ResolveThreadResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let db_repo_root = repo_root.clone();
    let review_key = review_key_for_scope(&repo_root, &request.target, request.scope.as_ref())?;
    let target_kind = target_kind(&request.target).to_string();
    let target_json = serde_json::to_string(&request.target)?;
    let response = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .resolve_thread(
                    &review_key,
                    &db_repo_root,
                    &target_kind,
                    &target_json,
                    &request.anchor,
                    request.resolved,
                )
                .await
        })
    })?;
    Ok(response)
}

fn classify_thread_for_request(
    request: bcode_code_review_models::ClassifyThreadRequest,
    config: &CodeReviewConfig,
) -> Result<bcode_code_review_models::ClassifyThreadResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let db_repo_root = repo_root.clone();
    let review_key = review_key_for_scope(&repo_root, &request.target, request.scope.as_ref())?;
    let target_kind = target_kind(&request.target).to_string();
    let target_json = serde_json::to_string(&request.target)?;
    with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .classify_thread(
                    &review_key,
                    &db_repo_root,
                    &target_kind,
                    &target_json,
                    &request.anchor,
                    request.thread_kind,
                    request.severity,
                )
                .await
        })
    })
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
                resolved_at_ms: draft.resolved_at_ms,
                comments: Vec::new(),
            });
        if entry.session_id.is_none() {
            entry.session_id.clone_from(&draft.session_id);
        }
        if entry.resolved_at_ms.is_none() {
            entry.resolved_at_ms = draft.resolved_at_ms;
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

fn context_for_anchor(
    summary: &ReviewSummary,
    anchor: &DraftAnchor,
) -> (Vec<ReviewBundleLine>, Vec<String>, Vec<String>) {
    match anchor.kind {
        ReviewAnchorKind::Review => (Vec::new(), Vec::new(), Vec::new()),
        ReviewAnchorKind::File => file_context_for_anchor(summary, anchor),
        ReviewAnchorKind::Range => {
            if anchor.is_file_anchor {
                file_context_for_anchor(summary, anchor)
            } else {
                diff_context_for_anchor(summary, anchor)
            }
        }
    }
}

fn file_context_for_anchor(
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
    let lines = bundle_file_lines(file);
    let rendered_lines: Vec<String> = lines.iter().map(render_bundle_file_line).collect();
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
    (selected_structured_lines, selected_lines, rendered_lines)
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

fn hydrate_bundle_line_anchor_metadata(lines: &mut [ReviewBundleLine], anchor: &DraftAnchor) {
    for line in lines {
        line.surface_id.clone_from(&anchor.surface_id);
        line.source_id.clone_from(&anchor.source_id);
    }
}

fn bundle_file_lines(file: &ReviewFile) -> Vec<ReviewBundleLine> {
    let mut diff_row = 0_u64;
    let mut lines = Vec::new();
    for hunk in &file.hunks {
        if let Some(heading) = &hunk.heading {
            lines.push(ReviewBundleLine {
                file_path: file.display_path().to_string(),
                kind: ReviewLineKind::Context,
                old_line: None,
                new_line: None,
                diff_row,
                content: format!("# {heading}"),
                surface_id: None,
                source_id: None,
            });
            diff_row = diff_row.saturating_add(1);
        }
        for line in &hunk.lines {
            lines.push(ReviewBundleLine {
                file_path: file.display_path().to_string(),
                kind: ReviewLineKind::Context,
                old_line: None,
                new_line: line.new_line.or(line.old_line),
                diff_row,
                content: line.content.clone(),
                surface_id: None,
                source_id: None,
            });
            diff_row = diff_row.saturating_add(1);
        }
    }
    lines
}

fn render_bundle_file_line(line: &ReviewBundleLine) -> String {
    let line_number = line
        .new_line
        .or(line.old_line)
        .map_or_else(|| "     ".to_string(), |line| format!("{line:>5}"));
    format!("{line_number} {}", line.content)
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
            surface_id: None,
            source_id: None,
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
                surface_id: None,
                source_id: None,
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

fn resolve_repo_root(repo_path: &Path) -> Result<PathBuf, StoreError> {
    if !repo_path.is_absolute() {
        return Err(StoreError::InvalidRequest(format!(
            "repo_path must be absolute, but received: {}",
            repo_path.display()
        )));
    }
    if !repo_path.is_dir() {
        return Err(StoreError::InvalidRequest(format!(
            "repo_path is not a directory: {}",
            display_from_current_dir(repo_path)
        )));
    }
    let repo_root = git_output(repo_path, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(repo_root.trim()))
}

fn repository_file_get(
    request: &RepositoryFileRequest,
) -> Result<RepositoryFileResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let relative_path = Path::new(&request.file_path);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(StoreError::InvalidRequest(
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
    let content = String::from_utf8(bytes)
        .map_err(|error| StoreError::InvalidRequest(format!("file is not valid UTF-8: {error}")))?;
    Ok(RepositoryFileResponse {
        file_path: request.file_path.clone(),
        content: Some(content),
        size_bytes,
        mtime_ms,
        is_binary: false,
        unavailable_reason: None,
    })
}

fn create_review_summary(request: &CreateReviewRequest) -> Result<ReviewSummary, StoreError> {
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

fn repository_review_files(repo_root: &Path) -> Result<Vec<ReviewFile>, StoreError> {
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

fn diff_for_target(repo_root: &Path, target: &ReviewTarget) -> Result<String, StoreError> {
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

fn git_output(repo_root: &Path, args: &[&str]) -> Result<String, StoreError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }

    Err(StoreError::Git(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

fn parse_unified_diff(diff: &str) -> Result<Vec<ReviewFile>, StoreError> {
    let files = bcode_git::diff::parse_unified_diff(diff)
        .map_err(|error| StoreError::Parse(error.to_string()))?;
    Ok(files.into_iter().map(review_file).collect())
}

fn review_file(file: bcode_git::models::DiffFile) -> ReviewFile {
    use bcode_git::models::{DiffFileStatus, DiffLineKind};
    ReviewFile {
        old_path: file.old_path,
        new_path: file.new_path,
        status: match file.status {
            DiffFileStatus::Modified => ReviewFileStatus::Modified,
            DiffFileStatus::Added => ReviewFileStatus::Added,
            DiffFileStatus::Deleted => ReviewFileStatus::Deleted,
            DiffFileStatus::Renamed => ReviewFileStatus::Renamed,
            DiffFileStatus::Copied => ReviewFileStatus::Copied,
        },
        additions: file.additions,
        deletions: file.deletions,
        is_binary: file.is_binary,
        hunks: file
            .hunks
            .into_iter()
            .map(|hunk| ReviewHunk {
                old_start: hunk.old_start,
                old_count: hunk.old_count,
                new_start: hunk.new_start,
                new_count: hunk.new_count,
                heading: hunk.heading,
                lines: hunk
                    .lines
                    .into_iter()
                    .map(|line| ReviewLine {
                        kind: match line.kind {
                            DiffLineKind::Context => ReviewLineKind::Context,
                            DiffLineKind::Added => ReviewLineKind::Added,
                            DiffLineKind::Removed => ReviewLineKind::Removed,
                        },
                        old_line: line.old_line,
                        new_line: line.new_line,
                        // Review presentation is line-oriented; drop CRLF carriage returns.
                        content: line
                            .content
                            .strip_suffix('\r')
                            .map_or_else(|| line.content.clone(), str::to_owned),
                    })
                    .collect(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests;
