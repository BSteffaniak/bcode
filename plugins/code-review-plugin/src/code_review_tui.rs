//! Full-screen local code review TUI mode.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::async_values::{AsyncValue, AsyncValueStore};
use bcode_client::BcodeClient;
use bcode_code_review_models::{
    CODE_REVIEW_SERVICE_INTERFACE_ID, MaterializeReviewWorkspaceRequest,
    MaterializeReviewWorkspaceResponse, OP_REVIEW_BUNDLE_GET, OP_REVIEW_PUBLISH_PREVIEW,
    OP_REVIEW_PUBLISH_RECORD_SAVE, OP_REVIEW_PUBLISH_SUBMIT, OP_REVIEW_PUBLISHER_MANIFEST,
    OP_REVIEW_PUBLISHER_PREVIEW, OP_REVIEW_PUBLISHER_SUBMIT, OP_REVIEW_PUBLISHERS_LIST,
    OP_REVIEW_REPO_FILE_GET, OP_REVIEW_WORKSPACE_MATERIALIZE, OP_REVIEW_WORKSPACE_UPDATE,
    REVIEW_PUBLISHER_INTERFACE_ID, ReviewBundle, ReviewRepositoryCommit,
    ReviewScope as ModelReviewScope, ReviewSource, ReviewSourceDiagnostic,
    ReviewSourceDiagnosticSeverity, ReviewSourceKind, ReviewSurface, ReviewSurfaceKind,
    ReviewTarget as ModelReviewTarget, ReviewTarget as ReviewOpenTarget, ReviewWorkspace,
    SavePublishRecordRequest, SavePublishRecordResponse, UpdateReviewWorkspaceRequest,
};
use bcode_ipc::PluginServiceResponse;
use bcode_plugin_sdk::tui::{PluginTuiAction, PluginTuiHost, PluginTuiSurface};
use bcode_session_models::SessionId;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::event::{Event, FocusEvent, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyOutcome};
use bmux_tui::terminal::Terminal;
use serde::{Deserialize, Serialize};

use crate::code_review_tui_render::materialized_file_surface_rows;
use crate::code_review_tui_view::{
    ReviewThreadAction, ReviewThreadAnchor, ReviewViewBlock, ReviewViewDocument, ReviewViewTarget,
};
use crate::tui_host_types::{TuiError, helpers};

const SERVICE_INTERFACE_ID: &str = CODE_REVIEW_SERVICE_INTERFACE_ID;
const CREATE_REVIEW_OPERATION: &str = "create_review";
const LIST_DRAFTS_OPERATION: &str = "draft.list";
const SAVE_DRAFT_OPERATION: &str = "draft.save";
const DELETE_DRAFT_OPERATION: &str = "draft.delete";
const UPDATE_DRAFT_OPERATION: &str = "draft.update";
const LINK_THREAD_SESSION_OPERATION: &str = "thread.link_session";
const THREAD_RESOLVE_OPERATION: &str = "thread.resolve";
const PUBLISH_SUBMIT_OPERATION: &str = OP_REVIEW_PUBLISH_SUBMIT;
const PUBLISH_RECORD_SAVE_OPERATION: &str = OP_REVIEW_PUBLISH_RECORD_SAVE;
const PUBLISHERS_LIST_OPERATION: &str = OP_REVIEW_PUBLISHERS_LIST;
const PUBLISH_PREVIEW_OPERATION: &str = OP_REVIEW_PUBLISH_PREVIEW;
const REVIEW_BUNDLE_GET_OPERATION: &str = OP_REVIEW_BUNDLE_GET;
const REVIEW_REPO_FILE_GET_OPERATION: &str = OP_REVIEW_REPO_FILE_GET;
const REVIEW_WORKSPACE_MATERIALIZE_OPERATION: &str = OP_REVIEW_WORKSPACE_MATERIALIZE;
const REVIEW_WORKSPACE_UPDATE_OPERATION: &str = OP_REVIEW_WORKSPACE_UPDATE;
const REVIEW_PUBLISHER_MANIFEST_OPERATION: &str = OP_REVIEW_PUBLISHER_MANIFEST;
const REVIEW_PUBLISHER_PREVIEW_OPERATION: &str = OP_REVIEW_PUBLISHER_PREVIEW;
const REVIEW_PUBLISHER_SUBMIT_OPERATION: &str = OP_REVIEW_PUBLISHER_SUBMIT;
const DEFAULT_PUBLISHER_ID: &str = "markdown_file";
const FILE_SIDEBAR_WIDTH: u16 = 34;

#[allow(dead_code)]
const fn review_open_target_id(target: &ReviewOpenTarget) -> &'static str {
    match target {
        ReviewOpenTarget::WorkingTreeUnstaged => "working-tree-unstaged",
        ReviewOpenTarget::IndexStaged => "index-staged",
        ReviewOpenTarget::WorkingTreeAndIndex => "working-tree-and-index",
        ReviewOpenTarget::LastCommit => "last-commit",
        ReviewOpenTarget::CommitRange { .. } => "commit-range",
        ReviewOpenTarget::BranchCompare { .. } => "branch-compare",
        ReviewOpenTarget::Repository => "repository",
    }
}

fn model_target_from_source_kind(
    kind: &ReviewSourceKind,
) -> bcode_code_review_models::ReviewTarget {
    match kind {
        ReviewSourceKind::WorkingTreeUnstaged => {
            bcode_code_review_models::ReviewTarget::WorkingTreeUnstaged
        }
        ReviewSourceKind::IndexStaged => bcode_code_review_models::ReviewTarget::IndexStaged,
        ReviewSourceKind::WorkingTreeAndIndex => {
            bcode_code_review_models::ReviewTarget::WorkingTreeAndIndex
        }
        ReviewSourceKind::LastCommit => bcode_code_review_models::ReviewTarget::LastCommit,
        ReviewSourceKind::Commit { rev } => bcode_code_review_models::ReviewTarget::CommitRange {
            base: format!("{rev}^"),
            head: rev.clone(),
            merge_base: false,
        },
        ReviewSourceKind::CommitRange {
            base,
            head,
            merge_base,
        } => bcode_code_review_models::ReviewTarget::CommitRange {
            base: base.clone(),
            head: head.clone(),
            merge_base: *merge_base,
        },
        ReviewSourceKind::BranchCompare {
            base_branch,
            head_branch,
            merge_base,
        } => bcode_code_review_models::ReviewTarget::BranchCompare {
            base_branch: base_branch.clone(),
            head_branch: head_branch.clone(),
            merge_base: *merge_base,
        },
        ReviewSourceKind::File { .. }
        | ReviewSourceKind::FileRange { .. }
        | ReviewSourceKind::Repository => bcode_code_review_models::ReviewTarget::Repository,
    }
}

/// Native TUI surface wrapper for the code review experience.
pub struct CodeReviewSurface {
    client: BcodeClient,
    repo_path: PathBuf,
    review_target: ReviewOpenTarget,
    app: ReviewApp,
    file_store: AsyncValueStore<String, CachedReviewFile>,
}

impl CodeReviewSurface {
    /// Load a code review surface from plugin-backed review data.
    ///
    /// # Errors
    ///
    /// Returns an error when initial review data cannot be loaded.
    pub async fn load(
        repo_path: PathBuf,
        target: ReviewOpenTarget,
        workspace: Option<ReviewWorkspace>,
        build_mode: bool,
    ) -> Result<Self, TuiError> {
        let client = BcodeClient::default_endpoint();
        let review_target = target;
        let mut app =
            load_review_app(&client, repo_path.clone(), review_target.clone(), workspace).await?;
        if build_mode {
            app.set_build_mode();
            app.status_message =
                Some("build mode: add sources with A, then m to review".to_string());
        }
        let mut surface = Self {
            client,
            repo_path,
            review_target,
            app,
            file_store: AsyncValueStore::new(),
        };
        surface.app.queue_selected_file_load();
        surface.ensure_selected_repository_file_load();
        Ok(surface)
    }

    /// Return whether the surface requested exit.
    #[must_use]
    pub const fn should_exit(&self) -> bool {
        self.app.should_exit
    }

    /// Take a session that should be opened after the surface exits.
    pub const fn take_session_to_open(&mut self) -> Option<SessionId> {
        self.app.take_session_to_open()
    }

    fn ensure_selected_repository_file_load(&mut self) -> bool {
        ensure_selected_repository_file_load(
            &self.client,
            &self.repo_path,
            &mut self.app,
            &mut self.file_store,
        )
    }

    /// Drain pending effectful work.
    pub async fn drain_inline_effects(&mut self) -> bool {
        let mut needs_redraw = false;
        match drain_pending_workspace_changes(
            &self.client,
            &self.repo_path,
            &self.review_target,
            &mut self.app,
        )
        .await
        {
            WorkspaceDrainOutcome::Idle => {}
            WorkspaceDrainOutcome::Applied | WorkspaceDrainOutcome::Failed => needs_redraw = true,
        }
        if handle_pending_draft_save(
            &self.client,
            &self.repo_path,
            &self.review_target,
            &mut self.app,
        )
        .await
        {
            needs_redraw = true;
        }
        if let Some(delete) = self.app.take_pending_draft_delete() {
            match delete_draft(&self.client, self.repo_path.clone(), delete.clone()).await {
                Ok(()) => {
                    self.app.status_message = Some("deleted draft comment".to_string());
                }
                Err(error) => {
                    self.app.restore_deleted_draft(delete);
                    self.app.status_message =
                        Some(format!("delete failed; restored local draft: {error}"));
                }
            }
            needs_redraw = true;
        }
        if let Some(update) = self.app.take_pending_draft_update() {
            match update_draft(&self.client, self.repo_path.clone(), update.clone()).await {
                Ok(()) => {
                    self.app.status_message = Some("updated draft comment".to_string());
                }
                Err(error) => {
                    self.app.restore_updated_draft(update);
                    self.app.status_message =
                        Some(format!("update failed; restored local draft: {error}"));
                }
            }
            needs_redraw = true;
        }
        if let Some(resolve) = self.app.take_pending_thread_resolve() {
            match resolve_thread(&self.client, self.repo_path.clone(), resolve.clone()).await {
                Ok(()) => {
                    self.app.status_message = Some(if resolve.resolved {
                        "resolved review thread".to_string()
                    } else {
                        "reopened review thread".to_string()
                    });
                }
                Err(error) => {
                    self.app.restore_thread_resolution(&resolve);
                    self.app.status_message = Some(format!(
                        "thread resolution failed; restored local state: {error}"
                    ));
                }
            }
            needs_redraw = true;
        }
        if let Some(ask) = self.app.take_pending_agent_session() {
            handle_pending_agent_session(
                &self.client,
                self.repo_path.clone(),
                self.review_target.clone(),
                &mut self.app,
                ask,
            )
            .await;
            needs_redraw = true;
        }
        if let Some(request) = self.app.take_publish_request() {
            handle_publish_request(
                &self.client,
                self.repo_path.clone(),
                self.review_target.clone(),
                &mut self.app,
                request,
            )
            .await;
            needs_redraw = true;
        }
        needs_redraw
    }
}

impl PluginTuiSurface for CodeReviewSurface {
    fn id(&self) -> &'static str {
        "bcode.code-review"
    }

    fn title(&self) -> &'static str {
        "Code Review"
    }

    fn render(&mut self, _area: Rect, frame: &mut Frame<'_>) {
        crate::code_review_tui_render::render(&mut self.app, frame);
    }

    fn handle_event(&mut self, event: &Event, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        let mut needs_redraw = handle_event_no_resize(&mut self.app, event);
        self.app.queue_selected_file_load();
        if self.ensure_selected_repository_file_load() {
            needs_redraw = true;
        }
        if self.app.should_exit {
            let outcome = self
                .app
                .take_session_to_open()
                .map(|session_id| serde_json::json!({ "open_session": session_id }));
            PluginTuiAction::Close { outcome }
        } else if needs_redraw {
            PluginTuiAction::Redraw
        } else {
            PluginTuiAction::None
        }
    }

    fn poll(&mut self, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        let mut needs_redraw = false;
        while let Ok(update) = self.file_store.try_recv() {
            self.file_store.apply(update);
            sync_repository_file_store(&mut self.app, &self.file_store);
            needs_redraw = true;
        }
        if needs_redraw {
            PluginTuiAction::Redraw
        } else {
            PluginTuiAction::None
        }
    }

    fn drain_effects<'a>(
        &'a mut self,
        _host: &'a dyn PluginTuiHost,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PluginTuiAction> + Send + 'a>> {
        Box::pin(async move {
            if self.drain_inline_effects().await {
                PluginTuiAction::Redraw
            } else {
                PluginTuiAction::None
            }
        })
    }
}

/// Code review TUI surface factory backed by the current in-crate implementation.
#[derive(Debug, Default)]
pub struct CodeReviewSurfaceFactory;

impl bcode_plugin_sdk::tui::PluginTuiSurfaceFactory for CodeReviewSurfaceFactory {
    fn surface_kind(&self) -> &'static str {
        "code-review"
    }

    fn open(
        &self,
        request: bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest,
    ) -> bcode_plugin_sdk::tui::PluginTuiSurfaceFuture {
        Box::pin(async move {
            let repo_path = request
                .repo_path
                .ok_or("code review surface requires repo_path")?;
            let target = match request.target.as_deref() {
                Some("repository") => ReviewOpenTarget::Repository,
                Some("working-tree-unstaged") => ReviewOpenTarget::WorkingTreeUnstaged,
                Some("index-staged") => ReviewOpenTarget::IndexStaged,
                Some("working-tree-and-index") => ReviewOpenTarget::WorkingTreeAndIndex,
                Some("last-commit") => ReviewOpenTarget::LastCommit,
                Some(target) => {
                    return Err(format!("unsupported code review target: {target}").into());
                }
                None => request
                    .options
                    .get("target")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or(ReviewOpenTarget::Repository),
            };
            let build_mode = request
                .options
                .get("build_mode")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let workspace = request
                .options
                .get("workspace")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok());
            let surface = CodeReviewSurface::load(repo_path, target, workspace, build_mode).await?;
            Ok(Box::new(surface) as Box<dyn bcode_plugin_sdk::tui::PluginTuiSurface>)
        })
    }
}

/// Registry for code review TUI surfaces backed by current TUI implementation.
#[must_use]
pub fn current_code_review_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    registry.register_factory(Box::new(CodeReviewSurfaceFactory));
    registry
}

/// Run a full-screen local Git review from a durable workspace.
///
/// # Errors
///
/// Returns an error when review data cannot be loaded or terminal I/O fails.
#[allow(clippy::future_not_send)]
pub async fn run_workspace<W: Write>(
    terminal: &mut Terminal<&mut W>,
    workspace: ReviewWorkspace,
    build_mode: bool,
) -> Result<Option<SessionId>, TuiError> {
    let target = target_from_workspace(&workspace);
    run_with_workspace(
        terminal,
        workspace.repo_root.clone(),
        target,
        Some(workspace),
        build_mode,
    )
    .await
}

/// Run a full-screen local Git review.
///
/// # Errors
///
/// Returns an error when review data cannot be loaded or terminal I/O fails.
#[allow(clippy::future_not_send)]
pub async fn run<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
    target: ReviewOpenTarget,
) -> Result<Option<SessionId>, TuiError> {
    run_with_workspace(terminal, repo_path, target, None, false).await
}

#[allow(clippy::future_not_send)]
#[allow(
    clippy::unused_async,
    clippy::needless_pass_by_ref_mut,
    unused_variables,
    dead_code
)]
async fn run_with_workspace<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
    target: ReviewOpenTarget,
    workspace: Option<ReviewWorkspace>,
    build_mode: bool,
) -> Result<Option<SessionId>, TuiError> {
    let options = serde_json::json!({ "build_mode": build_mode, "workspace": workspace });
    let _ = (terminal, repo_path, target, workspace, build_mode);
    Err(TuiError::PluginService {
        code: "unsupported_embedded_runner".to_string(),
        message: "code review plugin surfaces must be hosted by bcode_tui".to_string(),
    })
}

/// Result of draining pending workspace changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceDrainOutcome {
    /// No workspace work was pending.
    Idle,
    /// Workspace was saved or reloaded successfully.
    Applied,
    /// Workspace save/reload failed and remains pending.
    Failed,
}

async fn drain_pending_workspace_changes(
    client: &BcodeClient,
    repo_path: &Path,
    fallback_target: &ReviewOpenTarget,
    app: &mut ReviewApp,
) -> WorkspaceDrainOutcome {
    let mut changed = false;
    if app.take_pending_workspace_save() {
        match save_workspace(client, app.workspace.clone()).await {
            Ok(()) => {
                changed = true;
                app.status_message = Some("saved review workspace".to_string());
            }
            Err(error) => {
                app.pending_workspace_save = true;
                app.status_message = Some(format!("workspace save failed: {error}"));
                return WorkspaceDrainOutcome::Failed;
            }
        }
    }

    if app.take_pending_workspace_reload() {
        match load_workspace_review(
            client,
            repo_path.to_path_buf(),
            fallback_target.clone(),
            app.workspace.clone(),
        )
        .await
        {
            Ok(review) => {
                app.replace_review(review);
                changed = true;
                let diagnostics = app.review.diagnostics.len();
                app.status_message = Some(if diagnostics == 0 {
                    "updated review content".to_string()
                } else {
                    format!("updated review content with {diagnostics} diagnostic(s)")
                });
            }
            Err(error) => {
                app.pending_workspace_reload = true;
                app.status_message = Some(format!("review reload failed: {error}"));
                return WorkspaceDrainOutcome::Failed;
            }
        }
    }

    if changed {
        WorkspaceDrainOutcome::Applied
    } else {
        WorkspaceDrainOutcome::Idle
    }
}

async fn handle_pending_draft_save(
    client: &BcodeClient,
    repo_path: &Path,
    review_target: &ReviewOpenTarget,
    app: &mut ReviewApp,
) -> bool {
    let Some(save) = app.take_pending_draft_save() else {
        return false;
    };
    match save_draft(
        client,
        repo_path.to_path_buf(),
        review_target.clone(),
        Some(review_scope_for_workspace(&app.workspace)),
        save,
    )
    .await
    {
        Ok(()) => app.status_message = Some("saved draft comment".to_string()),
        Err(error) => {
            app.status_message = Some(format!("saved locally; draft persistence failed: {error}"));
        }
    }
    true
}

fn ensure_selected_repository_file_load(
    client: &BcodeClient,
    repo_path: &Path,
    app: &mut ReviewApp,
    file_store: &mut AsyncValueStore<String, CachedReviewFile>,
) -> bool {
    let Some(path) = app.take_pending_file_load() else {
        return false;
    };
    let client = client.clone();
    let repo_path = repo_path.to_path_buf();
    let started = file_store.ensure(path, move |path| async move {
        load_repository_file(&client, repo_path, path)
            .await
            .map_err(|error| error.to_string())
    });
    sync_repository_file_store(app, file_store);
    started
}

fn sync_repository_file_store(
    app: &mut ReviewApp,
    file_store: &AsyncValueStore<String, CachedReviewFile>,
) {
    let Some(path) = app.selected_file_path() else {
        return;
    };
    match file_store.get(&path) {
        AsyncValue::Ready(file) => app.store_loaded_file(file.clone()),
        AsyncValue::Error(error) => app.store_file_load_error(path, error),
        AsyncValue::Missing | AsyncValue::Loading => {}
    }
}

async fn load_review_app(
    client: &BcodeClient,
    repo_path: PathBuf,
    review_target: ReviewOpenTarget,
    workspace: Option<ReviewWorkspace>,
) -> Result<ReviewApp, TuiError> {
    let review = if let Some(workspace) = workspace.clone() {
        load_workspace_review(client, repo_path.clone(), review_target.clone(), workspace).await?
    } else {
        load_review(client, repo_path.clone(), review_target.clone()).await?
    };
    let drafts = load_drafts(
        client,
        repo_path,
        review_target,
        workspace.as_ref().map(review_scope_for_workspace),
    )
    .await;
    let mut app = ReviewApp::new(review);
    if app.workspace.sources.is_empty()
        && let Some(workspace) = workspace
    {
        app.workspace = workspace;
    }
    match drafts {
        Ok(drafts) => app.load_persisted_drafts(drafts),
        Err(error) => {
            app.status_message = Some(format!("failed to load persisted drafts: {error}"));
        }
    }
    Ok(app)
}

fn review_scope_for_workspace(workspace: &ReviewWorkspace) -> ModelReviewScope {
    ModelReviewScope::Workspace {
        workspace_id: workspace.id.clone(),
        target: workspace
            .sources
            .iter()
            .find(|source| source.included)
            .map_or(
                bcode_code_review_models::ReviewTarget::Repository,
                |source| model_target_from_source_kind(&source.kind),
            ),
    }
}

async fn save_workspace(client: &BcodeClient, workspace: ReviewWorkspace) -> Result<(), TuiError> {
    let payload = serde_json::to_vec(&UpdateReviewWorkspaceRequest {
        repo_path: workspace.repo_root.clone(),
        workspace,
    })
    .map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            REVIEW_WORKSPACE_UPDATE_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    Ok(())
}

async fn handle_pending_agent_session(
    client: &BcodeClient,
    repo_path: PathBuf,
    review_target: ReviewOpenTarget,
    app: &mut ReviewApp,
    ask: PendingAgentSession,
) {
    if let Some(session_id) = app.session_id_for_anchor(&ask.anchor) {
        if let Ok(session_id) = session_id.parse::<SessionId>() {
            let prompt = app.agent_session_prompt(&ask);
            match client
                .send_user_message(session_id, prompt, bcode_ipc::PromptPlacement::FollowUp)
                .await
            {
                Ok(_) => {
                    app.status_message = Some(format!(
                        "sent review follow-up to linked session {session_id}"
                    ));
                }
                Err(error) => {
                    app.status_message = Some(format!("failed to send review follow-up: {error}"));
                }
            }
        } else {
            app.status_message = Some("linked session id is invalid".to_string());
        }
    } else {
        match create_agent_session(client, repo_path, review_target, app, ask.clone()).await {
            Ok(session_id) => {
                app.mark_thread_session(&ask.anchor, session_id.to_string());
                app.status_message = Some(format!("created Bcode session {session_id}"));
            }
            Err(error) => {
                app.status_message = Some(format!("failed to create Bcode session: {error}"));
            }
        }
    }
}

async fn handle_publish_request(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewOpenTarget,
    app: &mut ReviewApp,
    request: PendingPublishRequest,
) {
    match request {
        PendingPublishRequest::ListPublishers => match list_publishers(client).await {
            Ok(mut publishers) => {
                match list_external_publishers(client).await {
                    Ok(external) => publishers.extend(external),
                    Err(error) => {
                        app.status_message =
                            Some(format!("external publisher discovery failed: {error}"));
                    }
                }
                app.show_publishers(publishers);
            }
            Err(error) => app.status_message = Some(format!("publisher list failed: {error}")),
        },
        PendingPublishRequest::Preview {
            publisher_id,
            options,
        } => match preview_review(
            client,
            repo_path,
            target,
            Some(app.workspace.clone()),
            app.publisher_for_id(&publisher_id)
                .and_then(|publisher| publisher.route.clone()),
            publisher_id,
            options,
        )
        .await
        {
            Ok(response) => app.show_publish_preview(response.publisher_id, response.preview),
            Err(error) => app.status_message = Some(format!("publish preview failed: {error}")),
        },
        PendingPublishRequest::Submit {
            publisher_id,
            options,
        } => match publish_review(
            client,
            repo_path,
            target,
            Some(app.workspace.clone()),
            app.publisher_for_id(&publisher_id)
                .and_then(|publisher| publisher.route.clone()),
            publisher_id,
            options,
        )
        .await
        {
            Ok(response) => app.finish_publish(response),
            Err(error) => app.status_message = Some(format!("publish failed: {error}")),
        },
    }
}

async fn list_publishers(client: &BcodeClient) -> Result<Vec<ReviewPublisherManifest>, TuiError> {
    let payload = serde_json::to_vec(&serde_json::json!({})).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            PUBLISHERS_LIST_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let response: ListReviewPublishersResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(response.publishers)
}

async fn preview_review(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewOpenTarget,
    workspace: Option<ReviewWorkspace>,
    route: Option<ReviewPublisherRoute>,
    publisher_id: String,
    options: Vec<ReviewPublishOption>,
) -> Result<PublishReviewPreviewResponse, TuiError> {
    let options = options_json(options);
    if let Some(route) = route {
        let bundle = load_review_bundle(client, repo_path, target, workspace.clone()).await?;
        let response = invoke_external_publisher(
            client,
            route,
            REVIEW_PUBLISHER_PREVIEW_OPERATION.to_string(),
            &bundle,
            options,
        )
        .await?;
        if let Some(error) = response.error {
            return Err(TuiError::PluginService {
                code: error.code,
                message: error.message,
            });
        }
        return serde_json::from_slice(&response.payload).map_err(TuiError::Json);
    }
    let request = PublishReviewRequest {
        repo_path,
        target,
        workspace,
        publisher_id,
        options,
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            PUBLISH_PREVIEW_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    serde_json::from_slice(&response.payload).map_err(TuiError::Json)
}

async fn publish_review(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewOpenTarget,
    workspace: Option<ReviewWorkspace>,
    route: Option<ReviewPublisherRoute>,
    publisher_id: String,
    options: Vec<ReviewPublishOption>,
) -> Result<PublishReviewResponse, TuiError> {
    let options = options_json(options);
    if let Some(route) = route {
        let bundle =
            load_review_bundle(client, repo_path.clone(), target, workspace.clone()).await?;
        let response = invoke_external_publisher(
            client,
            route,
            REVIEW_PUBLISHER_SUBMIT_OPERATION.to_string(),
            &bundle,
            options,
        )
        .await?;
        if let Some(error) = response.error {
            return Err(TuiError::PluginService {
                code: error.code,
                message: error.message,
            });
        }
        let publish_response: PublishReviewResponse =
            serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
        if let Some(workspace) = workspace {
            save_external_publish_record(client, workspace, bundle.review_id, &publish_response)
                .await?;
        }
        return Ok(publish_response);
    }
    let request = PublishReviewRequest {
        repo_path,
        target,
        workspace,
        publisher_id,
        options,
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            PUBLISH_SUBMIT_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    serde_json::from_slice(&response.payload).map_err(TuiError::Json)
}

async fn save_external_publish_record(
    client: &BcodeClient,
    workspace: ReviewWorkspace,
    review_id: String,
    response: &PublishReviewResponse,
) -> Result<(), TuiError> {
    let request = SavePublishRecordRequest {
        workspace,
        review_id,
        publisher_id: response.publisher_id.clone(),
        submitted: response.submitted,
        output: response.output.clone(),
        message: response.message.clone(),
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            PUBLISH_RECORD_SAVE_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let _: SavePublishRecordResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(())
}

fn options_from_schema(schema: &serde_json::Value) -> Vec<ReviewPublishOption> {
    let required = schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect::<BTreeSet<_>>();
    schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .map(|properties| {
            properties
                .iter()
                .filter_map(|(name, schema)| publish_option_from_schema(name, schema, &required))
                .collect()
        })
        .unwrap_or_default()
}

fn publish_option_from_schema(
    name: &str,
    schema: &serde_json::Value,
    required: &BTreeSet<&str>,
) -> Option<ReviewPublishOption> {
    let option_type = schema
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if option_type != "string" && option_type != "boolean" {
        return None;
    }
    Some(ReviewPublishOption {
        name: name.to_string(),
        label: publish_option_label(name, schema, required.contains(name)),
        value: publish_option_default(schema),
        choices: publish_option_choices(option_type, schema),
    })
}

fn publish_option_label(name: &str, schema: &serde_json::Value, required: bool) -> String {
    let mut label = schema
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(name)
        .to_string();
    if required {
        label.push_str(" [required]");
    }
    let choices = publish_option_choices(
        schema
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
        schema,
    )
    .join("|");
    if !choices.is_empty() {
        let _ = write!(label, " [{choices}]");
    }
    label
}

fn publish_option_default(schema: &serde_json::Value) -> String {
    schema
        .get("default")
        .and_then(|value| {
            value
                .as_str()
                .map(ToString::to_string)
                .or_else(|| value.as_bool().map(|value| value.to_string()))
        })
        .unwrap_or_default()
}

fn publish_option_choices(option_type: &str, schema: &serde_json::Value) -> Vec<String> {
    let mut choices = schema
        .get("enum")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| {
            value
                .as_str()
                .map(ToString::to_string)
                .or_else(|| value.as_bool().map(|value| value.to_string()))
        })
        .collect::<Vec<_>>();
    if option_type == "boolean" && choices.is_empty() {
        choices.extend(["false".to_string(), "true".to_string()]);
    }
    choices
}

fn options_json(options: Vec<ReviewPublishOption>) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    for option in options {
        if option.value.is_empty() {
            continue;
        }
        if option.choices == ["false".to_string(), "true".to_string()] {
            object.insert(
                option.name,
                serde_json::Value::Bool(option.value.eq_ignore_ascii_case("true")),
            );
        } else {
            object.insert(option.name, serde_json::Value::String(option.value));
        }
    }
    serde_json::Value::Object(object)
}

async fn list_external_publishers(
    client: &BcodeClient,
) -> Result<Vec<ReviewPublisherManifest>, TuiError> {
    let services = client.plugin_services().await?;
    let mut publishers = Vec::new();
    for service in services
        .into_iter()
        .filter(|service| service.interface_id == REVIEW_PUBLISHER_INTERFACE_ID)
    {
        let response = client
            .invoke_plugin_service(
                service.plugin_id.clone(),
                service.interface_id.clone(),
                REVIEW_PUBLISHER_MANIFEST_OPERATION.to_string(),
                serde_json::to_vec(&serde_json::json!({})).map_err(TuiError::Json)?,
            )
            .await?;
        if let Some(error) = response.error {
            return Err(TuiError::PluginService {
                code: error.code,
                message: error.message,
            });
        }
        let mut publisher: ReviewPublisherManifest =
            serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
        publisher.route = Some(ReviewPublisherRoute {
            plugin_id: service.plugin_id,
            interface_id: service.interface_id,
        });
        publishers.push(publisher);
    }
    Ok(publishers)
}

async fn load_review_bundle(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewOpenTarget,
    workspace: Option<ReviewWorkspace>,
) -> Result<ReviewBundle, TuiError> {
    let request = ReviewBundleRequest {
        repo_path,
        target,
        workspace,
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            REVIEW_BUNDLE_GET_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    serde_json::from_slice(&response.payload).map_err(TuiError::Json)
}

async fn invoke_external_publisher(
    client: &BcodeClient,
    route: ReviewPublisherRoute,
    operation: String,
    bundle: &ReviewBundle,
    options: serde_json::Value,
) -> Result<PluginServiceResponse, TuiError> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "bundle": bundle,
        "options": options,
    }))
    .map_err(TuiError::Json)?;
    client
        .invoke_plugin_service(route.plugin_id, route.interface_id, operation, payload)
        .await
        .map_err(TuiError::from)
}

async fn load_workspace_review(
    client: &BcodeClient,
    repo_path: PathBuf,
    _fallback_target: ReviewOpenTarget,
    workspace: ReviewWorkspace,
) -> Result<ReviewSummary, TuiError> {
    let payload = serde_json::to_vec(&MaterializeReviewWorkspaceRequest {
        repo_path: repo_path.clone(),
        workspace,
    })
    .map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            REVIEW_WORKSPACE_MATERIALIZE_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let response: MaterializeReviewWorkspaceResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    let materialization = response.materialization;
    let mut files = Vec::new();
    let mut surfaces = Vec::new();
    for surface in materialization.surfaces {
        if let Some(file) = surface.file.clone() {
            files.push(ReviewFile::from_model(file));
            surfaces.push(surface);
        }
    }
    let diagnostics = materialization.diagnostics;
    let repository_files = materialization.repository_files;
    let repository_branches = materialization.repository_branches;
    let repository_commits = materialization.repository_commits;
    if files.is_empty() {
        return Ok(ReviewSummary {
            title: materialization.workspace.title.clone(),
            repo_root: materialization.workspace.repo_root.clone(),
            files,
            additions: materialization.additions,
            deletions: materialization.deletions,
            workspace: Some(materialization.workspace),
            surfaces,
            diagnostics,
            repository_files,
            repository_branches,
            repository_commits,
        });
    }
    Ok(ReviewSummary {
        title: materialization.workspace.title.clone(),
        repo_root: materialization.workspace.repo_root.clone(),
        files,
        additions: materialization.additions,
        deletions: materialization.deletions,
        workspace: Some(materialization.workspace),
        surfaces,
        diagnostics,
        repository_files,
        repository_branches,
        repository_commits,
    })
}

async fn load_review(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewOpenTarget,
) -> Result<ReviewSummary, TuiError> {
    let request = CreateReviewRequest {
        repo_path,
        target: target.clone(),
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            CREATE_REVIEW_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let mut summary: ReviewSummary =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    summary.surfaces = summary.surfaces();
    Ok(summary)
}

async fn load_repository_file(
    client: &BcodeClient,
    repo_path: PathBuf,
    file_path: String,
) -> Result<CachedReviewFile, TuiError> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "repo_path": repo_path,
        "file_path": file_path,
    }))
    .map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            REVIEW_REPO_FILE_GET_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let response: RepositoryFileResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(CachedReviewFile::from_response(response))
}

async fn load_drafts(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewOpenTarget,
    scope: Option<ModelReviewScope>,
) -> Result<Vec<DraftComment>, TuiError> {
    let request = ListDraftsRequest {
        repo_path,
        target: target.clone(),
        scope,
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            LIST_DRAFTS_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let response: ListDraftsResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(response.drafts)
}

async fn save_draft(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewOpenTarget,
    scope: Option<ModelReviewScope>,
    save: PendingDraftSave,
) -> Result<(), TuiError> {
    let request = SaveDraftRequest {
        repo_path,
        target: target.clone(),
        scope,
        anchor: save.anchor.into(),
        body: save.body,
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            SAVE_DRAFT_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let _: SaveDraftResponse = serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(())
}

async fn delete_draft(
    client: &BcodeClient,
    repo_path: PathBuf,
    delete: PendingDraftDelete,
) -> Result<(), TuiError> {
    let Some(comment_id) = delete.comment.id else {
        return Ok(());
    };
    let request = DeleteDraftRequest {
        repo_path,
        target: delete.target,
        scope: delete.scope,
        comment_id,
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            DELETE_DRAFT_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let _: DeleteDraftResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(())
}

async fn update_draft(
    client: &BcodeClient,
    repo_path: PathBuf,
    update: PendingDraftUpdate,
) -> Result<(), TuiError> {
    let request = UpdateDraftRequest {
        repo_path,
        target: update.target,
        scope: update.scope,
        comment_id: update.comment_id,
        body: update.new_body,
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            UPDATE_DRAFT_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let _: UpdateDraftResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(())
}

async fn create_agent_session(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewOpenTarget,
    app: &ReviewApp,
    ask: PendingAgentSession,
) -> Result<SessionId, TuiError> {
    let session = client
        .create_session_in_working_directory(
            Some(format!("Review: {}", ask.anchor.path)),
            app.review.repo_root.clone(),
        )
        .await?;
    let prompt = app.agent_session_prompt(&ask);
    client
        .send_user_message(session.id, prompt, bcode_ipc::PromptPlacement::FollowUp)
        .await?;
    link_thread_session(client, repo_path, target, app, ask.anchor, session.id).await?;
    Ok(session.id)
}

async fn link_thread_session(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewOpenTarget,
    app: &ReviewApp,
    anchor: ReviewCommentAnchor,
    session_id: SessionId,
) -> Result<(), TuiError> {
    let request = LinkThreadSessionRequest {
        repo_path,
        target: target.clone(),
        scope: Some(review_scope_for_workspace(&app.workspace)),
        anchor: anchor.into(),
        session_id: session_id.to_string(),
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            LINK_THREAD_SESSION_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let _: LinkThreadSessionResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(())
}

async fn resolve_thread(
    client: &BcodeClient,
    repo_path: PathBuf,
    resolve: PendingThreadResolve,
) -> Result<(), TuiError> {
    let request = ResolveThreadRequest {
        repo_path,
        target: resolve.target,
        scope: resolve.scope,
        anchor: resolve.anchor.into(),
        resolved: resolve.resolved,
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            THREAD_RESOLVE_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let response: ResolveThreadResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    let _ = (response.thread_id, response.resolved_at_ms);
    Ok(())
}

fn handle_publish_event(app: &mut ReviewApp, event: &Event) -> bool {
    match event {
        Event::Key(stroke) => handle_publish_key(app, *stroke),
        Event::Paste(text) => app.insert_publish_option_text(text),
        Event::Resize(_) | Event::Focus(_) | Event::Tick => true,
        Event::Mouse(_) | Event::User(_) => false,
    }
}

fn handle_publish_key(app: &mut ReviewApp, stroke: KeyStroke) -> bool {
    if matches!(app.publish_state, Some(ReviewPublishState::Checklist))
        && stroke.modifiers.is_empty()
    {
        match stroke.key {
            KeyCode::Char('!') => {
                app.publish_state = None;
                return app.show_attention_sidebar();
            }
            KeyCode::Char('W') => {
                app.publish_state = None;
                return app.select_next_unviewed_file();
            }
            KeyCode::Char('P') => {
                app.publish_state = None;
                return app.select_next_open_thread_global();
            }
            _ => {}
        }
    }
    if app.publish_options_active() {
        if stroke.key == KeyCode::Escape && stroke.modifiers.is_empty() {
            app.publish_state = None;
            return true;
        }
        if stroke.key == KeyCode::Enter && stroke.modifiers.is_empty() {
            return app.confirm_publish_selection();
        }
        if stroke.key == KeyCode::Tab && stroke.modifiers.is_empty() {
            return app.publish_down(1);
        }
        if stroke.key == KeyCode::Tab && stroke.modifiers.shift {
            return app.publish_up(1);
        }
        if stroke.key == KeyCode::Right && stroke.modifiers.is_empty() {
            return app.cycle_selected_publish_option(1);
        }
        if stroke.key == KeyCode::Left && stroke.modifiers.is_empty() {
            return app.cycle_selected_publish_option(-1);
        }
        if stroke.key == KeyCode::Char(' ') && stroke.modifiers.is_empty() {
            return app.cycle_selected_publish_option(1);
        }
        return app.edit_publish_option(stroke);
    }
    if !stroke.modifiers.is_empty() {
        return false;
    }
    match stroke.key {
        KeyCode::Escape => {
            app.publish_state = None;
            true
        }
        KeyCode::Char('j') | KeyCode::Down => app.publish_down(1),
        KeyCode::Char('k') | KeyCode::Up => app.publish_up(1),
        KeyCode::Char('p') => app.back_to_publish_preview(),
        KeyCode::Enter => app.confirm_publish_selection(),
        _ => false,
    }
}

fn handle_event_no_resize(app: &mut ReviewApp, event: &Event) -> bool {
    if app.prompt_state.is_some() {
        return handle_prompt_event(app, event);
    }
    if app.publish_state.is_some() {
        return handle_publish_event(app, event);
    }
    if app.comment_editor.is_some() {
        return handle_comment_editor_event(app, event);
    }
    match event {
        Event::Resize(_) | Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick => {
            true
        }
        Event::Key(stroke) => handle_key(app, *stroke),
        Event::Mouse(mouse) => handle_mouse(app, *mouse),
        Event::Paste(_) | Event::User(_) => false,
    }
}

fn handle_prompt_event(app: &mut ReviewApp, event: &Event) -> bool {
    match event {
        Event::Key(stroke) => handle_prompt_key(app, *stroke),
        Event::Paste(text) => {
            if let Some(prompt) = &mut app.prompt_state {
                prompt.buffer.insert_str(text);
                prompt.selected = 0;
                return true;
            }
            false
        }
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::Resize(_) => {
            true
        }
        Event::Mouse(_) | Event::User(_) => false,
    }
}

fn handle_prompt_key(app: &mut ReviewApp, stroke: KeyStroke) -> bool {
    if stroke.key == KeyCode::Escape && stroke.modifiers.is_empty() {
        app.cancel_prompt();
        return true;
    }
    if stroke.key == KeyCode::Enter && stroke.modifiers.is_empty() {
        app.submit_prompt();
        return true;
    }
    if matches!(stroke.key, KeyCode::Down | KeyCode::Char('j')) && stroke.modifiers.is_empty() {
        return app.move_prompt_selection_down();
    }
    if matches!(stroke.key, KeyCode::Up | KeyCode::Char('k')) && stroke.modifiers.is_empty() {
        return app.move_prompt_selection_up();
    }
    if let Some(prompt) = &mut app.prompt_state {
        let outcome = helpers::handle_default_text_key(
            &mut prompt.buffer,
            stroke,
            TextInputEnterBehavior::Submit,
        );
        if matches!(outcome, TextInputKeyOutcome::Edited) {
            prompt.selected = 0;
        }
        return matches!(
            outcome,
            TextInputKeyOutcome::Edited | TextInputKeyOutcome::Submitted
        );
    }
    false
}

fn handle_comment_editor_event(app: &mut ReviewApp, event: &Event) -> bool {
    match event {
        Event::Key(stroke) => handle_comment_editor_key(app, *stroke),
        Event::Paste(text) => {
            if let Some(editor) = &mut app.comment_editor {
                editor.buffer.insert_str(text);
                return true;
            }
            false
        }
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::Resize(_) => {
            true
        }
        Event::Mouse(_) | Event::User(_) => false,
    }
}

fn handle_comment_editor_key(app: &mut ReviewApp, stroke: KeyStroke) -> bool {
    if stroke.key == KeyCode::Escape && stroke.modifiers.is_empty() {
        app.comment_editor = None;
        app.status_message = Some("comment draft canceled".to_string());
        return true;
    }
    if stroke.key == KeyCode::Char('s') && stroke.modifiers.ctrl {
        app.save_comment_editor();
        return true;
    }
    if stroke.key == KeyCode::Enter && !stroke.modifiers.ctrl && !stroke.modifiers.alt {
        app.save_comment_editor();
        return true;
    }
    if stroke.key == KeyCode::Tab
        && stroke.modifiers.is_empty()
        && let Some(editor) = &mut app.comment_editor
    {
        editor.preview = !editor.preview;
        app.status_message = Some(if editor.preview {
            "showing Markdown preview".to_string()
        } else {
            "editing Markdown comment".to_string()
        });
        return true;
    }
    if let Some(editor) = &mut app.comment_editor {
        if editor.preview {
            return true;
        }
        return matches!(
            helpers::handle_default_text_key(
                &mut editor.buffer,
                stroke,
                TextInputEnterBehavior::InsertNewline,
            ),
            TextInputKeyOutcome::Edited | TextInputKeyOutcome::Submitted
        );
    }
    false
}

fn handle_build_key(app: &mut ReviewApp, key: KeyCode) -> Option<bool> {
    if app.ux_mode != ReviewUxMode::Build {
        return None;
    }
    Some(match key {
        KeyCode::Char('R') => app.rematerialize_workspace(),
        KeyCode::Char('+') => app.open_add_file_source_picker(),
        KeyCode::Char('A' | 'a') => app.open_add_source_prompt(),
        KeyCode::Char('u') => app.add_quick_source(ReviewSourceKind::WorkingTreeUnstaged),
        KeyCode::Char('s') => app.add_quick_source(ReviewSourceKind::IndexStaged),
        KeyCode::Char('w') => app.add_quick_source(ReviewSourceKind::WorkingTreeAndIndex),
        KeyCode::Char('l') => app.add_quick_source(ReviewSourceKind::LastCommit),
        KeyCode::Char('g') => app.select_first_build_row(),
        KeyCode::Char('G') => app.select_last_build_row(),
        KeyCode::Char('I') => app.include_all_sources(),
        KeyCode::Char('E') => app.exclude_all_sources(),
        KeyCode::Char('V') => app.invert_source_inclusion(),
        KeyCode::Char('n') => app.select_next_empty_source(),
        KeyCode::Char('N') => app.select_previous_empty_source(),
        KeyCode::Char('z') => app.exclude_empty_sources(),
        KeyCode::Char('d') => app.select_next_diagnostic_source(),
        KeyCode::Char('Z') => app.exclude_sources_with_errors(),
        KeyCode::Char('C') => app.open_edit_source_spec_prompt(),
        KeyCode::Char('M') => app.toggle_selected_source_merge_base(),
        KeyCode::Char('X') => app.remove_excluded_sources(),
        KeyCode::Char('P') => app.remove_duplicate_sources(),
        KeyCode::Char(' ') => app.toggle_selected_build_source(),
        KeyCode::Char('T') => app.open_rename_workspace_prompt(),
        KeyCode::Char('r') => app.open_rename_source_prompt(),
        KeyCode::Char('[') => app.move_selected_source_up(),
        KeyCode::Char(']') => app.move_selected_source_down(),
        KeyCode::Char('-') => app.remove_selected_build_source(),
        KeyCode::Char('O') => app.open_selected_source_surface(),
        KeyCode::Char('Y') => app.select_source_for_current_surface(),
        KeyCode::Enter => app.activate_selected_build_row(),
        _ => return None,
    })
}

fn handle_key(app: &mut ReviewApp, stroke: KeyStroke) -> bool {
    if stroke.modifiers.ctrl {
        if stroke.key == KeyCode::Char('p') {
            return app.open_file_picker();
        }
        return false;
    }
    if stroke.modifiers.alt
        || stroke.modifiers.super_key
        || stroke.modifiers.hyper
        || stroke.modifiers.meta
    {
        return false;
    }
    let key = normalized_shortcut_key(stroke);
    if let Some(handled) = handle_build_key(app, key) {
        return handled;
    }
    match key {
        KeyCode::Char('q') => {
            app.should_exit = true;
            true
        }
        KeyCode::Escape => {
            let cleared = app.clear_range_selection()
                || app.clear_selected_view_target()
                || app.expand_all_inline_threads();
            if !cleared {
                app.should_exit = true;
            }
            true
        }
        KeyCode::Char('b') => {
            app.sidebar_visible = !app.sidebar_visible;
            true
        }
        KeyCode::Char('B') => app.set_build_mode(),
        KeyCode::Char('m') => app.toggle_ux_mode(),
        KeyCode::Char('M') => app.select_next_attention_item(),
        KeyCode::Char('+') => app.open_add_file_source_picker(),
        KeyCode::Char('A') => app.open_add_source_prompt(),
        KeyCode::Char('t') => app.toggle_sidebar_mode(),
        KeyCode::Char('f') => app.open_file_picker(),
        KeyCode::Char(':') => app.open_jump_to_line_prompt(),
        KeyCode::Char('/') => app.open_file_search_prompt(),
        KeyCode::Char('N') => app.search_previous_match(),
        KeyCode::Enter => {
            if app.activate_selected_review_target() {
                true
            } else if app.sidebar_mode == ReviewSidebarMode::Repository
                && app.review.is_repository_review()
            {
                app.activate_selected_tree_row()
            } else {
                app.jump_to_selected_thread()
            }
        }
        KeyCode::Char('j') | KeyCode::Down => app.move_down(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_up(1),
        KeyCode::Char('g') => app.scroll_to_top(),
        KeyCode::Char('G') => app.scroll_to_bottom(),
        KeyCode::Char('n') => {
            if app.has_active_search() {
                app.search_next_match()
            } else {
                app.select_next_file()
            }
        }
        KeyCode::Right => app.expand_selected_tree_row(),
        KeyCode::Char('p') => app.select_previous_file(),
        KeyCode::Left => app.collapse_selected_tree_row(),
        key => handle_review_navigation_key(app, key),
    }
}

fn handle_review_navigation_key(app: &mut ReviewApp, key: KeyCode) -> bool {
    match key {
        KeyCode::Char('}') => app.select_next_inline_draft(),
        KeyCode::Char('{') => app.select_previous_inline_draft(),
        KeyCode::Char(']') => app.select_next_inline_thread(),
        KeyCode::Char('[') => app.select_previous_inline_thread(),
        KeyCode::Char('!') => app.show_attention_sidebar(),
        KeyCode::Char('T') => app.cycle_thread_filter(),
        KeyCode::Char('E') => app.mark_all_files_unviewed(),
        KeyCode::Char('V') => app.mark_all_files_viewed(),
        KeyCode::Char('I') => app.select_previous_unviewed_file(),
        KeyCode::Char('W') => app.select_next_unviewed_file(),
        KeyCode::Char('w') => app.toggle_selected_file_viewed(),
        KeyCode::Char('u') => app.select_next_open_thread(),
        KeyCode::Char('i') => app.select_previous_open_thread(),
        KeyCode::Char('O') => app.select_previous_open_thread_global(),
        KeyCode::Char('P') => app.select_next_open_thread_global(),
        KeyCode::Char('R') => app.toggle_show_resolved_threads(),
        KeyCode::Char('r') => app.toggle_selected_thread_resolved(),
        KeyCode::Char('U') => app.expand_all_inline_threads(),
        KeyCode::Char('Z') => app.collapse_all_inline_threads(),
        KeyCode::Char('J') => app.select_next_hunk(),
        KeyCode::Char('K') => app.select_previous_hunk(),
        KeyCode::Char('v') => app.toggle_range_selection(),
        KeyCode::Char('c') => app.open_comment_editor(),
        KeyCode::Char('e') => app.open_latest_draft_editor(),
        KeyCode::Char('D') => app.delete_latest_draft_at_selection(),
        KeyCode::Char('x') => {
            if app.activate_selected_inline_action() {
                true
            } else {
                app.publish_review()
            }
        }
        KeyCode::Char('a') => app.ask_bcode_about_selection(),
        KeyCode::Char('o') => app.open_linked_session_at_selection(),
        KeyCode::Char('?') => {
            app.help_visible = !app.help_visible;
            true
        }
        _ => false,
    }
}

const fn normalized_shortcut_key(stroke: KeyStroke) -> KeyCode {
    if !stroke.modifiers.shift {
        return stroke.key;
    }
    match stroke.key {
        KeyCode::Char('=') => KeyCode::Char('+'),
        KeyCode::Char('/') => KeyCode::Char('?'),
        KeyCode::Char(ch) if ch.is_ascii_lowercase() => KeyCode::Char(ch.to_ascii_uppercase()),
        key => key,
    }
}

fn handle_mouse(app: &mut ReviewApp, mouse: MouseEvent) -> bool {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if app.file_area_contains(mouse.position.x, mouse.position.y) {
                if matches!(
                    app.sidebar_mode,
                    ReviewSidebarMode::Threads | ReviewSidebarMode::NeedsAttention
                ) {
                    app.select_previous_thread(3)
                } else {
                    app.scroll_files_up(3)
                }
            } else {
                app.scroll_up(3)
            }
        }
        MouseEventKind::ScrollDown => {
            if app.file_area_contains(mouse.position.x, mouse.position.y) {
                if matches!(
                    app.sidebar_mode,
                    ReviewSidebarMode::Threads | ReviewSidebarMode::NeedsAttention
                ) {
                    app.select_next_thread(3)
                } else {
                    app.scroll_files_down(3)
                }
            } else {
                app.scroll_down(3)
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if app.file_area_contains(mouse.position.x, mouse.position.y) {
                if matches!(
                    app.sidebar_mode,
                    ReviewSidebarMode::Threads | ReviewSidebarMode::NeedsAttention
                ) {
                    app.thread_index_at(mouse.position.x, mouse.position.y)
                        .is_some_and(|index| {
                            app.select_thread(index);
                            app.jump_to_selected_thread()
                        })
                } else if app.review.is_repository_review() {
                    match app.file_tree_row_at(mouse.position.x, mouse.position.y) {
                        Some(ReviewFileTreeRow::Directory { path, .. }) => {
                            app.toggle_file_tree_directory(&path)
                        }
                        Some(ReviewFileTreeRow::File { index, .. }) => app.select_file(index),
                        None => false,
                    }
                } else if let Some(index) = app.file_index_at(mouse.position.x, mouse.position.y) {
                    app.select_file(index)
                } else {
                    false
                }
            } else if let Some(visual_row) =
                app.view_visual_row_at(mouse.position.x, mouse.position.y)
            {
                app.handle_review_view_click(visual_row)
            } else {
                false
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => app
            .diff_line_index_at(mouse.position.x, mouse.position.y)
            .is_some_and(|index| app.update_mouse_range_selection(index)),
        MouseEventKind::Up(MouseButton::Left) => app.finish_mouse_range_selection(),
        MouseEventKind::Down(MouseButton::Right | MouseButton::Middle | MouseButton::Other(_))
        | MouseEventKind::Up(MouseButton::Right | MouseButton::Middle | MouseButton::Other(_))
        | MouseEventKind::Drag(MouseButton::Right | MouseButton::Middle | MouseButton::Other(_))
        | MouseEventKind::Move
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ListReviewPublishersResponse {
    publishers: Vec<ReviewPublisherManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewPublisherManifest {
    /// Publisher id.
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Human-readable description.
    pub description: String,
    /// Publisher capabilities.
    pub capabilities: ReviewPublisherCapabilities,
    /// Publisher option schema.
    pub options_schema: serde_json::Value,
    /// Optional external plugin route.
    #[serde(default)]
    pub route: Option<ReviewPublisherRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReviewPublisherRoute {
    /// Plugin id for external publisher.
    pub plugin_id: String,
    /// Service interface id.
    pub interface_id: String,
}

impl ReviewPublisherManifest {
    /// Return compact capability labels.
    #[must_use]
    pub fn capability_labels(&self) -> Vec<&'static str> {
        let mut labels = Vec::new();
        if self.capabilities.preview {
            labels.push("preview");
        }
        if self.capabilities.supports_threads {
            labels.push("threads");
        }
        if self.capabilities.supports_ranges {
            labels.push("ranges");
        }
        if self.capabilities.supports_inline_comments {
            labels.push("inline");
        }
        labels
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReviewPublisherCapabilities {
    preview: bool,
    submit: bool,
    update_existing: bool,
    supports_threads: bool,
    supports_ranges: bool,
    supports_inline_comments: bool,
    supports_summary_comment: bool,
}

fn relative_index(current_index: Option<usize>, len: usize, offset: isize) -> usize {
    let Some(max_index) = len.checked_sub(1) else {
        return 0;
    };
    let start = current_index.unwrap_or_else(|| if offset.is_negative() { len } else { 0 });
    if offset.is_negative() {
        start.saturating_sub(offset.unsigned_abs()).min(max_index)
    } else {
        start.saturating_add(offset.unsigned_abs()).min(max_index)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct PublishReviewPreviewResponse {
    publisher_id: String,
    preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ReviewBundleRequest {
    repo_path: PathBuf,
    target: ReviewOpenTarget,
    #[serde(default)]
    workspace: Option<ReviewWorkspace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PublishReviewRequest {
    repo_path: PathBuf,
    target: ReviewOpenTarget,
    #[serde(default)]
    workspace: Option<ReviewWorkspace>,
    publisher_id: String,
    #[serde(default)]
    options: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PublishReviewResponse {
    publisher_id: String,
    submitted: bool,
    output: Option<String>,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CreateReviewRequest {
    repo_path: PathBuf,
    target: bcode_code_review_models::ReviewTarget,
}

fn target_from_workspace(workspace: &ReviewWorkspace) -> ReviewOpenTarget {
    workspace
        .sources
        .iter()
        .find(|source| source.included)
        .map_or(ReviewOpenTarget::Repository, |source| {
            target_from_source_kind(&source.kind)
        })
}

fn target_from_source_kind(kind: &ReviewSourceKind) -> ReviewOpenTarget {
    match kind {
        ReviewSourceKind::WorkingTreeUnstaged => ReviewOpenTarget::WorkingTreeUnstaged,
        ReviewSourceKind::IndexStaged => ReviewOpenTarget::IndexStaged,
        ReviewSourceKind::WorkingTreeAndIndex => ReviewOpenTarget::WorkingTreeAndIndex,
        ReviewSourceKind::LastCommit => ReviewOpenTarget::LastCommit,
        ReviewSourceKind::CommitRange {
            base,
            head,
            merge_base,
        } => ReviewOpenTarget::CommitRange {
            base: base.clone(),
            head: head.clone(),
            merge_base: *merge_base,
        },
        ReviewSourceKind::BranchCompare {
            base_branch,
            head_branch,
            merge_base,
        } => ReviewOpenTarget::BranchCompare {
            base_branch: base_branch.clone(),
            head_branch: head_branch.clone(),
            merge_base: *merge_base,
        },
        ReviewSourceKind::Commit { rev } => ReviewOpenTarget::CommitRange {
            base: format!("{rev}^"),
            head: rev.clone(),
            merge_base: false,
        },
        ReviewSourceKind::File { .. }
        | ReviewSourceKind::FileRange { .. }
        | ReviewSourceKind::Repository => ReviewOpenTarget::Repository,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ListDraftsRequest {
    repo_path: PathBuf,
    target: ModelReviewTarget,
    #[serde(default)]
    scope: Option<ModelReviewScope>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SaveDraftRequest {
    repo_path: PathBuf,
    target: ModelReviewTarget,
    #[serde(default)]
    scope: Option<ModelReviewScope>,
    anchor: DraftAnchor,
    body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct DeleteDraftRequest {
    repo_path: PathBuf,
    target: ModelReviewTarget,
    #[serde(default)]
    scope: Option<ModelReviewScope>,
    comment_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct DeleteDraftResponse {
    deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct UpdateDraftRequest {
    repo_path: PathBuf,
    target: ModelReviewTarget,
    #[serde(default)]
    scope: Option<ModelReviewScope>,
    comment_id: String,
    body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct UpdateDraftResponse {
    updated: bool,
    updated_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct LinkThreadSessionRequest {
    repo_path: PathBuf,
    target: ModelReviewTarget,
    #[serde(default)]
    scope: Option<ModelReviewScope>,
    anchor: DraftAnchor,
    session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct LinkThreadSessionResponse {
    thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ResolveThreadRequest {
    repo_path: PathBuf,
    target: ModelReviewTarget,
    #[serde(default)]
    scope: Option<ModelReviewScope>,
    anchor: DraftAnchor,
    resolved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ResolveThreadResponse {
    thread_id: String,
    resolved_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DraftAnchor {
    file_path: String,
    diff_row: u64,
    old_line: Option<u32>,
    start_diff_row: Option<u64>,
    end_diff_row: Option<u64>,
    old_start: Option<u32>,
    old_end: Option<u32>,
    new_start: Option<u32>,
    new_end: Option<u32>,
    new_line: Option<u32>,
    line_kind: ReviewLineKind,
    #[serde(default)]
    is_file_anchor: bool,
    /// Surface id for normalized mixed-surface anchors.
    #[serde(default)]
    surface_id: Option<String>,
    /// Source id for normalized mixed-surface anchors.
    #[serde(default)]
    source_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct DraftComment {
    comment_id: String,
    thread_id: String,
    anchor: DraftAnchor,
    body: String,
    created_at_ms: u64,
    updated_at_ms: u64,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    resolved_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ListDraftsResponse {
    drafts: Vec<DraftComment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct SaveDraftResponse {
    draft: DraftComment,
}

impl From<ReviewCommentAnchor> for DraftAnchor {
    fn from(anchor: ReviewCommentAnchor) -> Self {
        Self {
            file_path: anchor.path.clone(),
            diff_row: u64::try_from(anchor.diff_row).unwrap_or(u64::MAX),
            start_diff_row: Some(u64::try_from(anchor.start_diff_row()).unwrap_or(u64::MAX)),
            end_diff_row: Some(u64::try_from(anchor.end_diff_row()).unwrap_or(u64::MAX)),
            old_start: anchor.old_start,
            old_end: anchor.old_end,
            new_start: anchor.new_start,
            new_end: anchor.new_end,
            old_line: anchor.old_line,
            new_line: anchor.new_line,
            line_kind: anchor.line_kind,
            is_file_anchor: anchor.is_file_anchor,
            surface_id: anchor.surface_id,
            source_id: anchor.source_id,
        }
    }
}

/// Review interaction mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewUxMode {
    /// Build the review by adding/removing sources.
    Build,
    /// Review/comment/publish included and context files.
    Review,
}

/// Full review summary displayed by the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewSummary {
    /// Human-readable review title.
    pub title: String,
    /// Git repository root.
    pub repo_root: PathBuf,
    /// Changed files.
    pub files: Vec<ReviewFile>,
    /// Total additions.
    pub additions: u32,
    /// Total deletions.
    pub deletions: u32,
    /// Workspace that owns this review session.
    #[serde(default)]
    pub workspace: Option<ReviewWorkspace>,
    /// Materialized review surfaces corresponding to files.
    #[serde(default)]
    pub surfaces: Vec<ReviewSurface>,
    /// Materialization diagnostics from workspace sources.
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
}

impl ReviewSummary {
    /// Return true when this review is browsing repository files instead of a diff.
    #[must_use]
    pub fn is_repository_review(&self) -> bool {
        self.title == "Repository Review"
            || self.workspace.as_ref().is_some_and(|workspace| {
                workspace.sources.iter().any(|source| {
                    source.included && matches!(source.kind, ReviewSourceKind::Repository)
                })
            })
    }

    /// Return workspace, creating a transient workspace for legacy single-target reviews.
    #[must_use]
    pub fn workspace(&self) -> ReviewWorkspace {
        self.workspace.clone().unwrap_or_else(|| {
            let target = if self.is_repository_review() {
                ReviewOpenTarget::Repository
            } else {
                ReviewOpenTarget::WorkingTreeAndIndex
            };
            ReviewWorkspace::from_target(self.repo_root.clone(), target)
        })
    }

    /// Return normalized surfaces visible for this review.
    #[must_use]
    pub fn surfaces(&self) -> Vec<ReviewSurface> {
        if !self.surfaces.is_empty() {
            return self.surfaces.clone();
        }
        self.files
            .iter()
            .enumerate()
            .map(|(index, file)| ReviewSurface {
                id: format!("surface-{index}"),
                source_id: "source-1".to_string(),
                path: file.display_path().to_string(),
                kind: if self.is_repository_review() {
                    ReviewSurfaceKind::File
                } else {
                    ReviewSurfaceKind::Diff
                },
                file: None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RepositoryFileResponse {
    file_path: String,
    content: Option<String>,
    size_bytes: u64,
    #[serde(default)]
    mtime_ms: Option<u64>,
    is_binary: bool,
    #[serde(default)]
    unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedReviewFile {
    pub path: String,
    pub content: String,
    pub line_spans: Vec<(usize, usize)>,
    pub size_bytes: u64,
    pub mtime_ms: Option<u64>,
    pub is_binary: bool,
    pub unavailable_reason: Option<String>,
}

impl CachedReviewFile {
    #[must_use]
    fn from_response(response: RepositoryFileResponse) -> Self {
        let content = response.content.unwrap_or_default();
        let line_spans = line_spans(&content);
        Self {
            path: response.file_path,
            content,
            line_spans,
            size_bytes: response.size_bytes,
            mtime_ms: response.mtime_ms,
            is_binary: response.is_binary,
            unavailable_reason: response.unavailable_reason,
        }
    }

    #[must_use]
    pub fn line(&self, index: usize) -> Option<&str> {
        let (start, end) = *self.line_spans.get(index)?;
        self.content.get(start..end)
    }
}

fn line_spans(content: &str) -> Vec<(usize, usize)> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut start = 0;
    for line in content.split_inclusive('\n') {
        let end = start + line.trim_end_matches('\n').trim_end_matches('\r').len();
        spans.push((start, end));
        start += line.len();
    }
    if !content.ends_with('\n') && spans.is_empty() {
        spans.push((0, content.len()));
    }
    spans
}

#[derive(Debug, Clone, Default)]
pub struct ReviewFileCache {
    entries: BTreeMap<String, CachedReviewFile>,
    lru: Vec<String>,
    total_bytes: usize,
}

impl ReviewFileCache {
    const MAX_FILES: usize = 128;
    const MAX_BYTES: usize = 32 * 1024 * 1024;

    #[must_use]
    pub fn get(&self, path: &str) -> Option<&CachedReviewFile> {
        self.entries.get(path)
    }

    pub fn insert(&mut self, file: CachedReviewFile) {
        let path = file.path.clone();
        if let Some(existing) = self.entries.remove(&path) {
            self.total_bytes = self.total_bytes.saturating_sub(existing.content.len());
        }
        self.total_bytes = self.total_bytes.saturating_add(file.content.len());
        self.entries.insert(path.clone(), file);
        self.lru.retain(|entry| entry != &path);
        self.lru.push(path);
        self.evict();
    }

    fn evict(&mut self) {
        while self.entries.len() > Self::MAX_FILES || self.total_bytes > Self::MAX_BYTES {
            let Some(path) = self.lru.first().cloned() else {
                break;
            };
            self.lru.remove(0);
            if let Some(existing) = self.entries.remove(&path) {
                self.total_bytes = self.total_bytes.saturating_sub(existing.content.len());
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchDirection {
    Next,
    Previous,
}

fn find_file_match(
    file: &CachedReviewFile,
    query: &str,
    selected_line: usize,
    direction: SearchDirection,
) -> Option<usize> {
    let len = file.line_spans.len();
    if len == 0 {
        return None;
    }
    match direction {
        SearchDirection::Next => {
            let start = selected_line.saturating_add(1).min(len);
            (start..len)
                .chain(0..start)
                .find(|index| file.line(*index).is_some_and(|line| line.contains(query)))
        }
        SearchDirection::Previous => {
            let start = selected_line.min(len.saturating_sub(1));
            (0..=start)
                .rev()
                .chain((start.saturating_add(1)..len).rev())
                .find(|index| file.line(*index).is_some_and(|line| line.contains(query)))
        }
    }
}

/// Sidebar file-tree row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewFileTreeRow {
    /// Directory row.
    Directory {
        /// Directory path.
        path: PathBuf,
        /// Nesting depth.
        depth: usize,
    },
    /// File row.
    File {
        /// File index in review files.
        index: usize,
        /// Nesting depth.
        depth: usize,
    },
}

/// Changed file displayed by the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewFile {
    /// Old path.
    pub old_path: Option<String>,
    /// New path.
    pub new_path: Option<String>,
    /// File status.
    pub status: ReviewFileStatus,
    /// Additions.
    pub additions: u32,
    /// Deletions.
    pub deletions: u32,
    /// Hunks.
    pub hunks: Vec<ReviewHunk>,
    /// Binary marker.
    pub is_binary: bool,
}

impl ReviewFile {
    fn from_model(file: bcode_code_review_models::ReviewFile) -> Self {
        Self {
            old_path: file.old_path,
            new_path: file.new_path,
            status: ReviewFileStatus::from_model(file.status),
            additions: file.additions,
            deletions: file.deletions,
            hunks: file.hunks.into_iter().map(ReviewHunk::from_model).collect(),
            is_binary: file.is_binary,
        }
    }

    /// Return the display path.
    #[must_use]
    pub fn display_path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("<unknown>")
    }
}

/// Review file status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
    /// Unknown status.
    Unknown,
}

impl ReviewFileStatus {
    const fn from_model(status: bcode_code_review_models::ReviewFileStatus) -> Self {
        match status {
            bcode_code_review_models::ReviewFileStatus::Modified => Self::Modified,
            bcode_code_review_models::ReviewFileStatus::Added => Self::Added,
            bcode_code_review_models::ReviewFileStatus::Deleted => Self::Deleted,
            bcode_code_review_models::ReviewFileStatus::Renamed => Self::Renamed,
            bcode_code_review_models::ReviewFileStatus::Copied => Self::Copied,
            bcode_code_review_models::ReviewFileStatus::Unknown => Self::Unknown,
        }
    }

    /// Return a compact status label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Copied => "C",
            Self::Unknown => "?",
        }
    }
}

/// Review hunk.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewHunk {
    /// Old start line.
    pub old_start: u32,
    /// Old line count.
    pub old_count: u32,
    /// New start line.
    pub new_start: u32,
    /// New line count.
    pub new_count: u32,
    /// Optional heading.
    pub heading: Option<String>,
    /// Lines.
    pub lines: Vec<ReviewLine>,
}

impl ReviewHunk {
    fn from_model(hunk: bcode_code_review_models::ReviewHunk) -> Self {
        Self {
            old_start: hunk.old_start,
            old_count: hunk.old_count,
            new_start: hunk.new_start,
            new_count: hunk.new_count,
            heading: hunk.heading,
            lines: hunk.lines.into_iter().map(ReviewLine::from_model).collect(),
        }
    }
}

/// Review diff line.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewLine {
    /// Line kind.
    pub kind: ReviewLineKind,
    /// Old line number.
    pub old_line: Option<u32>,
    /// New line number.
    pub new_line: Option<u32>,
    /// Content without diff marker.
    pub content: String,
}

impl ReviewLine {
    fn from_model(line: bcode_code_review_models::ReviewLine) -> Self {
        Self {
            kind: ReviewLineKind::from_model(line.kind),
            old_line: line.old_line,
            new_line: line.new_line,
            content: line.content,
        }
    }
}

/// Review diff line kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewLineKind {
    /// Context line.
    Context,
    /// Added line.
    Added,
    /// Removed line.
    Removed,
}

impl ReviewLineKind {
    const fn from_model(kind: bcode_code_review_models::ReviewLineKind) -> Self {
        match kind {
            bcode_code_review_models::ReviewLineKind::Context => Self::Context,
            bcode_code_review_models::ReviewLineKind::Added => Self::Added,
            bcode_code_review_models::ReviewLineKind::Removed => Self::Removed,
        }
    }
}

/// Draft comment line anchor.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReviewCommentAnchor {
    /// File index in the current review.
    pub file_index: usize,
    /// Display path for the commented file.
    pub path: String,
    /// Rendered diff row in the selected file.
    pub diff_row: usize,
    /// Rendered diff range end row in the selected file.
    pub end_diff_row: Option<usize>,
    /// Old line number, when present.
    pub old_line: Option<u32>,
    /// New line number, when present.
    pub new_line: Option<u32>,
    /// Old range start line, when present.
    pub old_start: Option<u32>,
    /// Old range end line, when present.
    pub old_end: Option<u32>,
    /// New range start line, when present.
    pub new_start: Option<u32>,
    /// New range end line, when present.
    pub new_end: Option<u32>,
    /// Anchored diff line kind.
    pub line_kind: ReviewLineKind,
    /// Whether this comment points at a file surface line rather than a diff row.
    pub is_file_anchor: bool,
    /// Surface id for normalized mixed-surface anchors.
    pub surface_id: Option<String>,
    /// Source id for normalized mixed-surface anchors.
    pub source_id: Option<String>,
}

impl ReviewCommentAnchor {
    /// Return the first rendered diff row for this anchor.
    #[must_use]
    pub const fn start_diff_row(&self) -> usize {
        self.diff_row
    }

    /// Return the final rendered diff row for this anchor.
    #[must_use]
    pub const fn end_diff_row(&self) -> usize {
        match self.end_diff_row {
            Some(row) => row,
            None => self.diff_row,
        }
    }
}

/// Review draft comment metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDraftComment {
    /// Persisted comment id, when saved in the plugin database.
    pub id: Option<String>,
    /// Draft body.
    pub body: String,
    /// Whether the draft is known to be persisted.
    pub persisted: bool,
    /// Creation timestamp in milliseconds since Unix epoch.
    pub created_at_ms: Option<u64>,
    /// Last update timestamp in milliseconds since Unix epoch.
    pub updated_at_ms: Option<u64>,
    /// Linked Bcode session id.
    pub session_id: Option<String>,
}

fn parse_range_spec(text: &str) -> Option<(&str, &str, bool)> {
    text.split_once("...")
        .map(|(base, head)| (base, head, true))
        .or_else(|| {
            text.split_once("..")
                .map(|(base, head)| (base, head, false))
        })
}

fn parse_file_range_spec(text: &str) -> Option<(String, u32, u32)> {
    let (path, range) = text.rsplit_once(':')?;
    let (start, end) = range.split_once('-')?;
    let (Ok(start), Ok(end)) = (start.parse::<u32>(), end.parse::<u32>()) else {
        return None;
    };
    let path = path.trim();
    if path.is_empty() || start == 0 || end == 0 || start > end {
        return None;
    }
    Some((path.to_string(), start, end))
}

fn edit_source_prompt_value(kind: &ReviewSourceKind) -> Option<(EditSourceTargetKind, String)> {
    match kind {
        ReviewSourceKind::Commit { rev } => Some((EditSourceTargetKind::Commit, rev.clone())),
        ReviewSourceKind::CommitRange {
            base,
            head,
            merge_base,
        } => {
            let separator = if *merge_base { "..." } else { ".." };
            Some((
                EditSourceTargetKind::CommitRange,
                format!("{base}{separator}{head}"),
            ))
        }
        ReviewSourceKind::BranchCompare {
            base_branch,
            head_branch,
            merge_base,
        } => {
            let separator = if *merge_base { "..." } else { ".." };
            Some((
                EditSourceTargetKind::BranchCompare,
                format!("{base_branch}{separator}{head_branch}"),
            ))
        }
        ReviewSourceKind::File { path } => Some((EditSourceTargetKind::File, path.clone())),
        ReviewSourceKind::FileRange { path, start, end } => Some((
            EditSourceTargetKind::FileRange,
            format!("{path}:{start}-{end}"),
        )),
        ReviewSourceKind::WorkingTreeUnstaged
        | ReviewSourceKind::IndexStaged
        | ReviewSourceKind::WorkingTreeAndIndex
        | ReviewSourceKind::LastCommit
        | ReviewSourceKind::Repository => None,
    }
}

/// Pending draft comment persistence request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDraftSave {
    /// Saved anchor.
    pub anchor: ReviewCommentAnchor,
    /// Saved body.
    pub body: String,
}

/// Pending draft comment delete request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDraftDelete {
    /// Review target for persistence scope.
    pub target: ModelReviewTarget,
    /// Review scope for persistence.
    pub scope: Option<ModelReviewScope>,
    /// Deleted anchor.
    pub anchor: ReviewCommentAnchor,
    /// Deleted comment.
    pub comment: ReviewDraftComment,
}

/// Pending draft comment update request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDraftUpdate {
    /// Review target for persistence scope.
    pub target: ModelReviewTarget,
    /// Review scope for persistence.
    pub scope: Option<ModelReviewScope>,
    /// Edited anchor.
    pub anchor: ReviewCommentAnchor,
    /// Persisted comment id.
    pub comment_id: String,
    /// Previous body for failure restore.
    pub previous_body: String,
    /// New body.
    pub new_body: String,
}

/// Pending thread resolution persistence request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingThreadResolve {
    /// Review target for persistence scope.
    pub target: ModelReviewTarget,
    /// Review scope for persistence.
    pub scope: Option<ModelReviewScope>,
    /// Thread anchor.
    pub anchor: ReviewCommentAnchor,
    /// Whether the thread should be resolved.
    pub resolved: bool,
}

/// Pending Bcode agent session request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAgentSession {
    /// Thread anchor.
    pub anchor: ReviewCommentAnchor,
    /// Optional selected draft body.
    pub draft_body: Option<String>,
}

/// Draft comment editor mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewCommentEditorMode {
    /// Creating a new draft.
    Create,
    /// Editing an existing persisted draft.
    Edit {
        /// Existing comment id.
        comment_id: String,
        /// Previous body for failure restore.
        previous_body: String,
    },
}

/// Active draft comment editor.
#[derive(Debug, Clone)]
pub struct ReviewCommentEditor {
    /// Anchor being commented on.
    pub anchor: ReviewCommentAnchor,
    /// Editable comment buffer.
    pub buffer: TextEditBuffer,
    /// Whether the editor is showing Markdown preview.
    pub preview: bool,
    /// Editor mode.
    pub mode: ReviewCommentEditorMode,
}

impl ReviewCommentEditor {
    /// Create an editor for an anchor.
    #[must_use]
    pub const fn new(anchor: ReviewCommentAnchor) -> Self {
        Self {
            anchor,
            buffer: TextEditBuffer::new(),
            preview: false,
            mode: ReviewCommentEditorMode::Create,
        }
    }

    /// Create an editor for updating an existing draft.
    #[must_use]
    pub fn edit(anchor: ReviewCommentAnchor, comment_id: String, body: String) -> Self {
        Self {
            anchor,
            buffer: TextEditBuffer::from_text(&body),
            preview: false,
            mode: ReviewCommentEditorMode::Edit {
                comment_id,
                previous_body: body,
            },
        }
    }
}

/// Review sidebar mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewSidebarMode {
    /// Included review workspace sources/sidebar.
    Included,
    /// Full repository file browser.
    Repository,
    /// Review thread list sidebar.
    Threads,
    /// Review source list sidebar.
    Sources,
    /// Files with local review work remaining.
    NeedsAttention,
}

impl ReviewSidebarMode {
    /// Return a user-facing sidebar label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Included => "included",
            Self::Repository => "repo",
            Self::Threads => "threads",
            Self::Sources => "sources",
            Self::NeedsAttention => "attention",
        }
    }
}

/// Review thread sidebar filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewThreadFilter {
    /// Show all threads.
    All,
    /// Show open threads only.
    Open,
    /// Show resolved threads only.
    Resolved,
}

impl ReviewThreadFilter {
    /// Return the next filter in cycle order.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::All => Self::Open,
            Self::Open => Self::Resolved,
            Self::Resolved => Self::All,
        }
    }

    /// Return a compact filter label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Open => "open",
            Self::Resolved => "resolved",
        }
    }
}

/// Review thread row summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewThreadSummary {
    /// Thread anchor.
    pub anchor: ReviewCommentAnchor,
    /// Number of draft comments.
    pub draft_count: usize,
    /// Latest draft body.
    pub latest_body: String,
    /// Linked Bcode session id, when present.
    pub session_id: Option<String>,
    /// Whether the thread is locally resolved.
    pub resolved: bool,
}

impl ReviewThreadSummary {
    /// Return a compact line label for the thread anchor.
    #[must_use]
    pub fn line_label(&self) -> String {
        self.anchor.new_start.or(self.anchor.old_start).map_or_else(
            || format!("@{}", self.anchor.diff_row),
            |line| format!("+{line}"),
        )
    }
}

/// Pending publish request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingPublishRequest {
    /// List available publishers.
    ListPublishers,
    /// Preview a publisher output.
    Preview {
        /// Publisher id.
        publisher_id: String,
        /// Publisher options.
        options: Vec<ReviewPublishOption>,
    },
    /// Submit publisher output.
    Submit {
        /// Publisher id.
        publisher_id: String,
        /// Publisher options.
        options: Vec<ReviewPublishOption>,
    },
}

/// Publish option field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPublishOption {
    /// Option name.
    pub name: String,
    /// Option label.
    pub label: String,
    /// Option value.
    pub value: String,
    /// Enumerated choices, when the publisher exposes them.
    pub choices: Vec<String>,
}

/// Source kind that should be edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditSourceTargetKind {
    /// Commit source.
    Commit,
    /// Commit range source.
    CommitRange,
    /// Branch compare source.
    BranchCompare,
    /// File source.
    File,
    /// File range source.
    FileRange,
}

/// Active review prompt kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewPromptKind {
    /// Fuzzy file picker.
    FilePicker,
    /// Jump to a line number.
    JumpToLine,
    /// Search within the current file.
    FileSearch,
    /// Add a source to the workspace.
    AddSourceKind,
    /// Pick a commit source from recent repository commits.
    AddCommitPicker,
    /// Add a commit source to the workspace.
    AddCommitSource,
    /// Pick a base commit for a commit range source.
    AddCommitRangeBasePicker,
    /// Pick a head commit for a commit range source.
    AddCommitRangeHeadPicker { base_rev: String },
    /// Add a commit range source to the workspace.
    AddCommitRangeSource,
    /// Pick a base branch for a branch compare source.
    AddBranchCompareBasePicker,
    /// Pick a head branch for a branch compare source.
    AddBranchCompareHeadPicker { base_branch: String },
    /// Add a branch compare source to the workspace.
    AddBranchCompareSource,
    /// Add a file source using the fuzzy file picker.
    AddFileSourcePicker,
    /// Pick a file before entering a file range source.
    AddFileRangePathPicker,
    /// Add a file range source to the workspace.
    AddFileRangeSource,
    /// Rename the review workspace.
    RenameWorkspace,
    /// Edit the selected source specification.
    EditSourceSpec { target_kind: EditSourceTargetKind },
    /// Rename the selected source.
    RenameSource,
}

/// Add-source menu item kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddSourceMenuKind {
    /// File source.
    File,
    /// File range source.
    FileRange,
    /// Commit source.
    Commit,
    /// Commit range source.
    CommitRange,
    /// Branch compare source.
    BranchCompare,
    /// Staged changes source.
    Staged,
    /// Unstaged changes source.
    Unstaged,
    /// Working tree source.
    WorkingTree,
    /// Last commit source.
    LastCommit,
    /// Repository source.
    Repository,
}

/// Add-source menu item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddSourceMenuItem {
    /// Kind submitted by this menu item.
    pub kind: AddSourceMenuKind,
    /// Short label.
    pub label: &'static str,
    /// Help text.
    pub help: &'static str,
}

/// Add-source menu items.
#[must_use]
pub const fn add_source_menu_items() -> &'static [AddSourceMenuItem] {
    &[
        AddSourceMenuItem {
            kind: AddSourceMenuKind::Unstaged,
            label: "unstaged changes",
            help: "changes in the working tree that are not staged",
        },
        AddSourceMenuItem {
            kind: AddSourceMenuKind::Staged,
            label: "staged changes",
            help: "index changes ready to commit",
        },
        AddSourceMenuItem {
            kind: AddSourceMenuKind::WorkingTree,
            label: "working tree",
            help: "all staged and unstaged changes",
        },
        AddSourceMenuItem {
            kind: AddSourceMenuKind::LastCommit,
            label: "last commit",
            help: "changes introduced by HEAD",
        },
        AddSourceMenuItem {
            kind: AddSourceMenuKind::BranchCompare,
            label: "branch compare",
            help: "compare base..head or base...head branches",
        },
        AddSourceMenuItem {
            kind: AddSourceMenuKind::File,
            label: "file",
            help: "whole file from repository tree",
        },
        AddSourceMenuItem {
            kind: AddSourceMenuKind::FileRange,
            label: "file range",
            help: "file slice path:start-end",
        },
        AddSourceMenuItem {
            kind: AddSourceMenuKind::Commit,
            label: "commit",
            help: "single commit revision",
        },
        AddSourceMenuItem {
            kind: AddSourceMenuKind::CommitRange,
            label: "commit range",
            help: "compare base..head or base...head revisions",
        },
        AddSourceMenuItem {
            kind: AddSourceMenuKind::Repository,
            label: "repository browser",
            help: "browse repository files as review context",
        },
    ]
}

/// Active one-line prompt state.
#[derive(Debug, Clone)]
pub struct ReviewPromptState {
    /// Prompt kind.
    pub kind: ReviewPromptKind,
    /// Editable prompt buffer.
    pub buffer: TextEditBuffer,
    /// Selected match index.
    pub selected: usize,
}

impl ReviewPromptState {
    /// Create a prompt.
    #[must_use]
    pub const fn new(kind: ReviewPromptKind) -> Self {
        Self {
            kind,
            buffer: TextEditBuffer::new(),
            selected: 0,
        }
    }
}

/// Publish modal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewPublishState {
    /// Publish readiness checklist.
    Checklist,
    /// Publisher picker.
    Picker,
    /// Options editor.
    Options {
        /// Publisher id.
        publisher_id: String,
        /// Options.
        options: Vec<ReviewPublishOption>,
        /// Selected option index.
        selected: usize,
    },
    /// Preview content.
    Preview {
        /// Publisher id.
        publisher_id: String,
        /// Publisher options.
        options: Vec<ReviewPublishOption>,
        /// Preview text.
        preview: String,
        /// Top visible preview line.
        scroll: usize,
    },
    /// Submit confirmation.
    ConfirmSubmit {
        /// Publisher id.
        publisher_id: String,
        /// Publisher options.
        options: Vec<ReviewPublishOption>,
        /// Preview text.
        preview: String,
        /// Top visible preview line.
        scroll: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ReviewViewportState {
    diff_scroll: usize,
    selected_diff_line: usize,
}

/// Stateful review app model.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReviewApp {
    /// Workspace that owns this review session.
    pub workspace: ReviewWorkspace,
    /// Review data.
    pub review: ReviewSummary,
    /// Current review UX mode.
    pub ux_mode: ReviewUxMode,
    /// Selected file index.
    pub selected_file: usize,
    /// Top visible file row.
    pub file_scroll: usize,
    /// Top visible repository tree row.
    pub tree_scroll: usize,
    /// Top visible rendered diff row.
    pub diff_scroll: usize,
    /// Selected rendered diff row.
    pub selected_diff_line: usize,
    /// Whether the file sidebar is visible.
    pub sidebar_visible: bool,
    /// Active sidebar mode.
    pub sidebar_mode: ReviewSidebarMode,
    /// Selected review thread index.
    pub selected_thread: usize,
    /// Active thread sidebar filter.
    pub thread_filter: ReviewThreadFilter,
    /// Top visible review thread row.
    pub thread_scroll: usize,
    /// Whether help is visible.
    pub help_visible: bool,
    /// Whether to exit.
    pub should_exit: bool,
    /// Last transient status message.
    pub status_message: Option<String>,
    /// Draft comments keyed by anchor.
    pub draft_comments: BTreeMap<ReviewCommentAnchor, Vec<ReviewDraftComment>>,
    /// Active draft editor, if open.
    pub comment_editor: Option<ReviewCommentEditor>,
    /// Draft comment awaiting persistence.
    pub pending_draft_save: Option<PendingDraftSave>,
    /// Draft comment awaiting deletion.
    pub pending_draft_delete: Option<PendingDraftDelete>,
    /// Draft comment awaiting update.
    pub pending_draft_update: Option<PendingDraftUpdate>,
    /// Thread resolution awaiting persistence.
    pub pending_thread_resolve: Option<PendingThreadResolve>,
    /// Pending publish request.
    pub pending_publish_request: Option<PendingPublishRequest>,
    /// Available publishers.
    pub publishers: Vec<ReviewPublisherManifest>,
    /// Files marked viewed by display path.
    pub viewed_files: BTreeSet<String>,
    /// Repository file cache for read-only file browsing.
    pub file_cache: ReviewFileCache,
    /// Per-file viewport state keyed by stable file/surface path.
    file_viewports: BTreeMap<String, ReviewViewportState>,
    /// Repository file path awaiting lazy load.
    pub pending_file_load: Option<String>,
    /// Selected publisher index.
    pub selected_publisher: usize,
    /// Active publish UI state.
    pub publish_state: Option<ReviewPublishState>,
    /// Whether incomplete-review publish warning has been acknowledged.
    pub publish_readiness_ack: bool,
    /// Active one-line prompt, if any.
    pub prompt_state: Option<ReviewPromptState>,
    /// Last current-file search query.
    pub last_search_query: Option<String>,
    /// Expanded repository sidebar directories.
    pub expanded_dirs: BTreeSet<PathBuf>,
    /// Selected repository file-tree row.
    pub selected_tree_row: usize,
    /// Active build-mode row.
    pub selected_build_row: usize,
    /// Top visible build selectable row.
    pub build_scroll: usize,
    /// Whether workspace changes should be persisted.
    pub pending_workspace_save: bool,
    /// Whether workspace content should be rematerialized.
    pub pending_workspace_reload: bool,
    /// Review thread awaiting Bcode session creation.
    pub pending_agent_session: Option<PendingAgentSession>,
    /// Active range selection start row, if any.
    pub range_selection_start: Option<usize>,
    mouse_range_selection_start: Option<usize>,
    mouse_range_selection_dragged: bool,
    /// Selected inline review target, if selection is on a non-source row.
    pub selected_view_target: Option<ReviewViewTarget>,
    /// Collapsed inline review thread keys.
    pub collapsed_review_threads: BTreeSet<String>,
    /// Locally resolved inline review thread keys.
    pub resolved_review_threads: BTreeSet<String>,
    /// Whether resolved inline review threads are visible.
    pub show_resolved_threads: bool,
    /// Session id to open after leaving review mode.
    pub session_to_open: Option<SessionId>,
    last_file_area: Option<Rect>,
    last_diff_area: Option<Rect>,
}

impl ReviewApp {
    /// Create a new review app.
    #[must_use]
    pub fn new(review: ReviewSummary) -> Self {
        let workspace = review.workspace();
        let viewed_files = workspace.viewed_files.clone();
        Self {
            workspace,
            review,
            ux_mode: ReviewUxMode::Review,
            selected_file: 0,
            file_scroll: 0,
            tree_scroll: 0,
            diff_scroll: 0,
            selected_diff_line: 0,
            sidebar_visible: true,
            sidebar_mode: ReviewSidebarMode::Included,
            selected_thread: 0,
            thread_filter: ReviewThreadFilter::All,
            thread_scroll: 0,
            help_visible: false,
            should_exit: false,
            status_message: None,
            draft_comments: BTreeMap::new(),
            comment_editor: None,
            pending_draft_save: None,
            pending_draft_delete: None,
            pending_draft_update: None,
            pending_thread_resolve: None,
            pending_publish_request: None,
            publishers: Vec::new(),
            viewed_files,
            file_cache: ReviewFileCache::default(),
            file_viewports: BTreeMap::new(),
            pending_file_load: None,
            selected_publisher: 0,
            publish_state: None,
            publish_readiness_ack: false,
            prompt_state: None,
            last_search_query: None,
            expanded_dirs: BTreeSet::new(),
            selected_tree_row: 0,
            selected_build_row: 0,
            build_scroll: 0,
            pending_workspace_save: false,
            pending_workspace_reload: false,
            pending_agent_session: None,
            range_selection_start: None,
            mouse_range_selection_start: None,
            mouse_range_selection_dragged: false,
            selected_view_target: None,
            collapsed_review_threads: BTreeSet::new(),
            resolved_review_threads: BTreeSet::new(),
            show_resolved_threads: true,
            session_to_open: None,
            last_file_area: None,
            last_diff_area: None,
        }
    }

    /// Switch directly to build mode.
    pub fn set_build_mode(&mut self) -> bool {
        self.ux_mode = ReviewUxMode::Build;
        self.sidebar_mode = ReviewSidebarMode::Sources;
        self.sidebar_visible = true;
        self.status_message = Some("build mode: assemble review sources".to_string());
        true
    }

    /// Toggle between build and review UX modes.
    pub fn toggle_ux_mode(&mut self) -> bool {
        if self.ux_mode == ReviewUxMode::Build && self.workspace.sources.is_empty() {
            self.status_message =
                Some("add at least one source before switching to review mode".to_string());
            return true;
        }
        if self.ux_mode == ReviewUxMode::Build
            && self.workspace.sources.iter().all(|source| !source.included)
        {
            self.status_message =
                Some("include at least one source before switching to review mode".to_string());
            return true;
        }
        if self.has_materialization_errors() {
            self.status_message = Some(
                "review has source errors; fix or exclude them before review mode".to_string(),
            );
            return true;
        }
        self.ux_mode = match self.ux_mode {
            ReviewUxMode::Build => ReviewUxMode::Review,
            ReviewUxMode::Review => ReviewUxMode::Build,
        };
        self.status_message = Some(match self.ux_mode {
            ReviewUxMode::Build => "build mode: assemble review sources".to_string(),
            ReviewUxMode::Review => "review mode: comment, ask, publish".to_string(),
        });
        true
    }

    /// Open add-source prompt appropriate for the selected build row.
    pub fn open_add_source_prompt(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            self.status_message = Some("switch to build mode with m to add sources".to_string());
            return true;
        }
        self.prompt_state = Some(ReviewPromptState::new(ReviewPromptKind::AddSourceKind));
        self.status_message = Some(
            "add source kind: file, file-range, commit, range, branch, staged, unstaged, working-tree"
                .to_string(),
        );
        true
    }

    /// Open rename-workspace prompt.
    pub fn open_rename_workspace_prompt(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        let mut prompt = ReviewPromptState::new(ReviewPromptKind::RenameWorkspace);
        prompt.buffer.insert_str(&self.workspace.title);
        self.prompt_state = Some(prompt);
        self.status_message = Some("rename review workspace".to_string());
        true
    }

    /// Open edit-source-spec prompt for the selected workspace source.
    pub fn open_edit_source_spec_prompt(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        let Some(index) = self.selected_build_source_index() else {
            self.status_message = Some("select an editable source to change".to_string());
            return true;
        };
        let Some(source) = self.workspace.sources.get(index) else {
            self.status_message = Some("select an editable source to change".to_string());
            return true;
        };
        let Some((target_kind, value)) = edit_source_prompt_value(&source.kind) else {
            self.status_message = Some("selected source has no editable spec".to_string());
            return true;
        };
        let mut prompt = ReviewPromptState::new(ReviewPromptKind::EditSourceSpec { target_kind });
        prompt.buffer.insert_str(&value);
        self.prompt_state = Some(prompt);
        self.status_message = Some("edit source spec".to_string());
        true
    }

    /// Open rename-source prompt for the selected workspace source.
    pub fn open_rename_source_prompt(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        let Some(index) = self.selected_build_source_index() else {
            self.status_message = Some("select an included source to rename".to_string());
            return true;
        };
        let Some(source) = self.workspace.sources.get(index) else {
            self.status_message = Some("select an included source to rename".to_string());
            return true;
        };
        let mut prompt = ReviewPromptState::new(ReviewPromptKind::RenameSource);
        prompt.buffer.insert_str(&source.label);
        self.prompt_state = Some(prompt);
        self.status_message = Some("rename source".to_string());
        true
    }

    /// Open fuzzy file picker prompt.
    pub fn open_file_picker(&mut self) -> bool {
        self.prompt_state = Some(ReviewPromptState::new(ReviewPromptKind::FilePicker));
        self.status_message = Some("file picker: type path, enter open, esc cancel".to_string());
        true
    }

    /// Open add-commit picker.
    pub fn open_add_commit_picker(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        if self.review.repository_commits.is_empty() {
            self.prompt_state = Some(ReviewPromptState::new(ReviewPromptKind::AddCommitSource));
            self.status_message = Some("add commit: enter revision".to_string());
            return true;
        }
        self.prompt_state = Some(ReviewPromptState::new(ReviewPromptKind::AddCommitPicker));
        self.status_message = Some("add commit: pick recent commit".to_string());
        true
    }

    /// Open add-commit-range base picker.
    pub fn open_add_commit_range_base_picker(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        if self.review.repository_commits.is_empty() {
            self.prompt_state = Some(ReviewPromptState::new(
                ReviewPromptKind::AddCommitRangeSource,
            ));
            self.status_message = Some("add commit range: base..head or base...head".to_string());
            return true;
        }
        self.prompt_state = Some(ReviewPromptState::new(
            ReviewPromptKind::AddCommitRangeBasePicker,
        ));
        self.status_message = Some("add commit range: pick base commit".to_string());
        true
    }

    /// Open add-branch-compare base picker.
    pub fn open_add_branch_compare_base_picker(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        if self.review.repository_branches.is_empty() {
            self.prompt_state = Some(ReviewPromptState::new(
                ReviewPromptKind::AddBranchCompareSource,
            ));
            self.status_message = Some("add branch compare: base...head or base..head".to_string());
            return true;
        }
        self.prompt_state = Some(ReviewPromptState::new(
            ReviewPromptKind::AddBranchCompareBasePicker,
        ));
        self.status_message = Some("add branch compare: pick base branch".to_string());
        true
    }

    /// Open add-file-source picker.
    pub fn open_add_file_source_picker(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        if self.review.repository_files.is_empty() {
            return self.add_selected_file_to_workspace();
        }
        self.prompt_state = Some(ReviewPromptState::new(
            ReviewPromptKind::AddFileSourcePicker,
        ));
        self.status_message = Some("add file source: type to filter, enter add".to_string());
        true
    }

    /// Open add-file-range path picker.
    pub fn open_add_file_range_path_picker(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        if self.review.repository_files.is_empty() {
            self.prompt_state = Some(ReviewPromptState::new(ReviewPromptKind::AddFileRangeSource));
            self.status_message = Some("add file range: path:start-end".to_string());
            return true;
        }
        self.prompt_state = Some(ReviewPromptState::new(
            ReviewPromptKind::AddFileRangePathPicker,
        ));
        self.status_message = Some("add file range: pick file, then enter start-end".to_string());
        true
    }

    /// Open jump-to-line prompt.
    pub fn open_jump_to_line_prompt(&mut self) -> bool {
        self.prompt_state = Some(ReviewPromptState::new(ReviewPromptKind::JumpToLine));
        self.status_message = Some("jump to line".to_string());
        true
    }

    /// Open current-file search prompt.
    pub fn open_file_search_prompt(&mut self) -> bool {
        self.prompt_state = Some(ReviewPromptState::new(ReviewPromptKind::FileSearch));
        self.status_message = Some("search current file".to_string());
        true
    }

    /// Cancel active prompt.
    pub fn cancel_prompt(&mut self) {
        self.prompt_state = None;
        self.status_message = Some("prompt canceled".to_string());
    }

    /// Submit active prompt.
    pub fn submit_prompt(&mut self) -> bool {
        let Some(prompt) = self.prompt_state.take() else {
            return false;
        };
        let text = prompt.buffer.text().trim().to_string();
        match prompt.kind.clone() {
            ReviewPromptKind::FilePicker => self.submit_file_picker(&text, prompt.selected),
            ReviewPromptKind::JumpToLine => self.submit_jump_to_line(&text),
            ReviewPromptKind::FileSearch => self.submit_file_search(&text),
            ReviewPromptKind::AddSourceKind => self.submit_add_source_kind(&text, prompt.selected),
            ReviewPromptKind::AddCommitPicker => {
                self.submit_add_commit_picker(&text, prompt.selected)
            }
            ReviewPromptKind::AddCommitSource => self.submit_add_commit_source(&text),
            ReviewPromptKind::AddCommitRangeBasePicker => {
                self.submit_add_commit_range_base_picker(&text, prompt.selected)
            }
            ReviewPromptKind::AddCommitRangeHeadPicker { base_rev } => {
                self.submit_add_commit_range_head_picker(&base_rev, &text, prompt.selected)
            }
            ReviewPromptKind::AddCommitRangeSource => self.submit_add_commit_range_source(&text),
            ReviewPromptKind::AddBranchCompareBasePicker => {
                self.submit_add_branch_compare_base_picker(&text, prompt.selected)
            }
            ReviewPromptKind::AddBranchCompareHeadPicker { base_branch } => {
                self.submit_add_branch_compare_head_picker(&base_branch, &text, prompt.selected)
            }
            ReviewPromptKind::AddBranchCompareSource => {
                self.submit_add_branch_compare_source(&text)
            }
            ReviewPromptKind::AddFileSourcePicker => {
                self.submit_add_file_source_picker(&text, prompt.selected)
            }
            ReviewPromptKind::AddFileRangePathPicker => {
                self.submit_add_file_range_path_picker(&text, prompt.selected)
            }
            ReviewPromptKind::AddFileRangeSource => self.submit_add_file_range_source(&text),
            ReviewPromptKind::EditSourceSpec { target_kind } => {
                self.submit_edit_source_spec(target_kind, &text)
            }
            ReviewPromptKind::RenameWorkspace => self.submit_rename_workspace(&text),
            ReviewPromptKind::RenameSource => self.submit_rename_source(&text),
        }
    }

    fn start_add_source_kind(&mut self, kind: AddSourceMenuKind) -> bool {
        match kind {
            AddSourceMenuKind::File => self.open_add_file_source_picker(),
            AddSourceMenuKind::FileRange => self.open_add_file_range_path_picker(),
            AddSourceMenuKind::Commit => self.open_add_commit_picker(),
            AddSourceMenuKind::CommitRange => self.open_add_commit_range_base_picker(),
            AddSourceMenuKind::BranchCompare => self.open_add_branch_compare_base_picker(),
            AddSourceMenuKind::Staged => self.push_workspace_source(ReviewSourceKind::IndexStaged),
            AddSourceMenuKind::Unstaged => {
                self.push_workspace_source(ReviewSourceKind::WorkingTreeUnstaged)
            }
            AddSourceMenuKind::WorkingTree => {
                self.push_workspace_source(ReviewSourceKind::WorkingTreeAndIndex)
            }
            AddSourceMenuKind::LastCommit => {
                self.push_workspace_source(ReviewSourceKind::LastCommit)
            }
            AddSourceMenuKind::Repository => {
                self.push_workspace_source(ReviewSourceKind::Repository)
            }
        }
    }

    fn submit_add_source_kind(&mut self, text: &str, selected: usize) -> bool {
        if text.trim().is_empty()
            && let Some(kind) = add_source_menu_items().get(selected).map(|item| item.kind)
        {
            return self.start_add_source_kind(kind);
        }
        match text.trim().to_ascii_lowercase().as_str() {
            "file" | "f" => return self.start_add_source_kind(AddSourceMenuKind::File),
            "file-range" | "range-file" | "fr" => {
                return self.start_add_source_kind(AddSourceMenuKind::FileRange);
            }
            "commit" | "c" => return self.start_add_source_kind(AddSourceMenuKind::Commit),
            "range" | "commit-range" | "r" => {
                return self.start_add_source_kind(AddSourceMenuKind::CommitRange);
            }
            "branch" | "branch-compare" | "compare" | "bc" => {
                return self.start_add_source_kind(AddSourceMenuKind::BranchCompare);
            }
            "staged" | "index" => return self.start_add_source_kind(AddSourceMenuKind::Staged),
            "unstaged" => return self.start_add_source_kind(AddSourceMenuKind::Unstaged),
            "working-tree" | "worktree" | "working" | "all" => {
                return self.start_add_source_kind(AddSourceMenuKind::WorkingTree);
            }
            "last" | "last-commit" => {
                return self.start_add_source_kind(AddSourceMenuKind::LastCommit);
            }
            "repo" | "repository" => {
                return self.start_add_source_kind(AddSourceMenuKind::Repository);
            }
            _ => {
                self.status_message = Some(
                    "unknown source kind; use file, file-range, commit, range, branch, staged, unstaged, working-tree".to_string(),
                );
            }
        }
        true
    }

    fn validate_non_empty_pair(&mut self, left: &str, right: &str, message: &str) -> bool {
        if left.trim().is_empty() || right.trim().is_empty() {
            self.status_message = Some(message.to_string());
            return false;
        }
        true
    }

    fn submit_add_commit_picker(&mut self, query: &str, selected: usize) -> bool {
        let matches = self.repository_commit_picker_matches(query);
        let Some(commit) = matches
            .get(selected)
            .cloned()
            .or_else(|| matches.first().cloned())
        else {
            self.status_message = Some(format!("no commit matches `{query}`"));
            return true;
        };
        self.push_workspace_source(ReviewSourceKind::Commit { rev: commit.rev })
    }

    fn submit_add_commit_source(&mut self, text: &str) -> bool {
        let rev = text.trim();
        if rev.is_empty() {
            self.status_message = Some("enter a commit revision".to_string());
            return true;
        }
        self.push_workspace_source(ReviewSourceKind::Commit {
            rev: rev.to_string(),
        })
    }

    fn submit_add_commit_range_base_picker(&mut self, query: &str, selected: usize) -> bool {
        let matches = self.repository_commit_picker_matches(query);
        let Some(commit) = matches
            .get(selected)
            .cloned()
            .or_else(|| matches.first().cloned())
        else {
            self.status_message = Some(format!("no commit matches `{query}`"));
            return true;
        };
        self.prompt_state = Some(ReviewPromptState::new(
            ReviewPromptKind::AddCommitRangeHeadPicker {
                base_rev: commit.rev,
            },
        ));
        self.status_message = Some("add commit range: pick head commit".to_string());
        true
    }

    fn submit_add_commit_range_head_picker(
        &mut self,
        base_rev: &str,
        query: &str,
        selected: usize,
    ) -> bool {
        let matches = self.repository_commit_picker_matches(query);
        let Some(commit) = matches
            .get(selected)
            .cloned()
            .or_else(|| matches.first().cloned())
        else {
            self.status_message = Some(format!("no commit matches `{query}`"));
            return true;
        };
        if base_rev == commit.rev {
            self.status_message = Some("base and head commit must differ".to_string());
            return true;
        }
        self.push_workspace_source(ReviewSourceKind::CommitRange {
            base: base_rev.to_string(),
            head: commit.rev,
            merge_base: true,
        })
    }

    fn submit_add_commit_range_source(&mut self, text: &str) -> bool {
        let Some((base, head, merge_base)) = parse_range_spec(text) else {
            self.status_message = Some("enter range as base..head or base...head".to_string());
            return true;
        };
        if !self.validate_non_empty_pair(base, head, "range endpoints cannot be empty") {
            return true;
        }
        self.push_workspace_source(ReviewSourceKind::CommitRange {
            base: base.trim().to_string(),
            head: head.trim().to_string(),
            merge_base,
        })
    }

    fn submit_add_branch_compare_source(&mut self, text: &str) -> bool {
        let Some((base_branch, head_branch, merge_base)) = parse_range_spec(text) else {
            self.status_message =
                Some("enter branch compare as base..head or base...head".to_string());
            return true;
        };
        if !self.validate_non_empty_pair(base_branch, head_branch, "branch names cannot be empty") {
            return true;
        }
        self.push_workspace_source(ReviewSourceKind::BranchCompare {
            base_branch: base_branch.trim().to_string(),
            head_branch: head_branch.trim().to_string(),
            merge_base,
        })
    }

    fn submit_add_branch_compare_base_picker(&mut self, query: &str, selected: usize) -> bool {
        let matches = self.repository_branch_picker_matches(query);
        let Some(base_branch) = matches
            .get(selected)
            .cloned()
            .or_else(|| matches.first().cloned())
        else {
            self.status_message = Some(format!("no branch matches `{query}`"));
            return true;
        };
        self.prompt_state = Some(ReviewPromptState::new(
            ReviewPromptKind::AddBranchCompareHeadPicker { base_branch },
        ));
        self.status_message = Some("add branch compare: pick head branch".to_string());
        true
    }

    fn submit_add_branch_compare_head_picker(
        &mut self,
        base_branch: &str,
        query: &str,
        selected: usize,
    ) -> bool {
        let matches = self.repository_branch_picker_matches(query);
        let Some(head_branch) = matches
            .get(selected)
            .cloned()
            .or_else(|| matches.first().cloned())
        else {
            self.status_message = Some(format!("no branch matches `{query}`"));
            return true;
        };
        if base_branch == head_branch {
            self.status_message = Some("base and head branch must differ".to_string());
            return true;
        }
        self.push_workspace_source(ReviewSourceKind::BranchCompare {
            base_branch: base_branch.to_string(),
            head_branch,
            merge_base: true,
        })
    }

    fn submit_add_file_source_picker(&mut self, query: &str, selected: usize) -> bool {
        let matches = self.repository_file_picker_matches(query);
        let Some(path) = matches
            .get(selected)
            .cloned()
            .or_else(|| matches.first().cloned())
        else {
            self.status_message = Some(format!("no file matches `{query}`"));
            return true;
        };
        self.push_workspace_source(ReviewSourceKind::File { path })
    }

    fn submit_add_file_range_path_picker(&mut self, query: &str, selected: usize) -> bool {
        let matches = self.repository_file_picker_matches(query);
        let Some(path) = matches
            .get(selected)
            .cloned()
            .or_else(|| matches.first().cloned())
        else {
            self.status_message = Some(format!("no file matches `{query}`"));
            return true;
        };
        let mut prompt = ReviewPromptState::new(ReviewPromptKind::AddFileRangeSource);
        prompt.buffer.insert_str(&format!("{path}:"));
        self.prompt_state = Some(prompt);
        self.status_message = Some("add file range: enter start-end".to_string());
        true
    }

    fn submit_add_file_range_source(&mut self, text: &str) -> bool {
        let Some((path, start, end)) = parse_file_range_spec(text) else {
            self.status_message = Some("enter file range as path:start-end".to_string());
            return true;
        };
        self.push_workspace_source(ReviewSourceKind::FileRange { path, start, end })
    }

    fn sync_review_workspace(&mut self) {
        self.review.workspace = Some(self.workspace.clone());
    }

    fn next_source_id(&self) -> String {
        let mut next = self.workspace.sources.len().saturating_add(1);
        loop {
            let source_id = format!("source-{next}");
            if self
                .workspace
                .sources
                .iter()
                .all(|source| source.id != source_id)
            {
                return source_id;
            }
            next = next.saturating_add(1);
        }
    }

    fn push_workspace_source(&mut self, kind: ReviewSourceKind) -> bool {
        if matches!(kind, ReviewSourceKind::Repository)
            && self
                .workspace
                .sources
                .iter()
                .any(|source| matches!(source.kind, ReviewSourceKind::Repository))
        {
            self.status_message = Some("repository source is already included".to_string());
            return true;
        }
        if self
            .workspace
            .sources
            .iter()
            .any(|source| source.kind == kind)
        {
            self.status_message = Some(format!("{} is already included", kind.label()));
            return true;
        }
        let label = kind.label();
        let source_id = self.next_source_id();
        self.workspace.sources.push(ReviewSource {
            id: source_id,
            kind,
            label: label.clone(),
            included: true,
        });
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        self.pending_workspace_reload = true;
        self.status_message = Some(format!("added {label}"));
        true
    }

    fn replace_selected_source_kind(&mut self, kind: ReviewSourceKind) -> bool {
        let Some(index) = self.selected_build_source_index() else {
            self.status_message = Some("select an editable source to change".to_string());
            return true;
        };
        if self
            .workspace
            .sources
            .iter()
            .enumerate()
            .any(|(source_index, source)| source_index != index && source.kind == kind)
        {
            self.status_message = Some(format!("{} is already included", kind.label()));
            return true;
        }
        let label = kind.label();
        let Some(source) = self.workspace.sources.get_mut(index) else {
            self.status_message = Some("select an editable source to change".to_string());
            return true;
        };
        source.kind = kind;
        source.label.clone_from(&label);
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        self.pending_workspace_reload = true;
        self.status_message = Some(format!("updated source to {label}"));
        true
    }

    fn submit_edit_source_spec(&mut self, target_kind: EditSourceTargetKind, text: &str) -> bool {
        match target_kind {
            EditSourceTargetKind::Commit => self.submit_edit_commit_source(text),
            EditSourceTargetKind::CommitRange => self.submit_edit_commit_range_source(text),
            EditSourceTargetKind::BranchCompare => self.submit_edit_branch_compare_source(text),
            EditSourceTargetKind::File => self.submit_edit_file_source(text),
            EditSourceTargetKind::FileRange => self.submit_edit_file_range_source(text),
        }
    }

    fn submit_edit_commit_source(&mut self, text: &str) -> bool {
        let rev = text.trim();
        if rev.is_empty() {
            self.status_message = Some("commit revision cannot be empty".to_string());
            return true;
        }
        self.replace_selected_source_kind(ReviewSourceKind::Commit {
            rev: rev.to_string(),
        })
    }

    fn submit_edit_commit_range_source(&mut self, text: &str) -> bool {
        let Some((base, head, merge_base)) = parse_range_spec(text) else {
            self.status_message = Some("enter range as base..head or base...head".to_string());
            return true;
        };
        if !self.validate_non_empty_pair(base, head, "range endpoints cannot be empty") {
            return true;
        }
        self.replace_selected_source_kind(ReviewSourceKind::CommitRange {
            base: base.trim().to_string(),
            head: head.trim().to_string(),
            merge_base,
        })
    }

    fn submit_edit_branch_compare_source(&mut self, text: &str) -> bool {
        let Some((base_branch, head_branch, merge_base)) = parse_range_spec(text) else {
            self.status_message =
                Some("enter branch compare as base..head or base...head".to_string());
            return true;
        };
        if !self.validate_non_empty_pair(
            base_branch,
            head_branch,
            "branch compare endpoints cannot be empty",
        ) {
            return true;
        }
        self.replace_selected_source_kind(ReviewSourceKind::BranchCompare {
            base_branch: base_branch.trim().to_string(),
            head_branch: head_branch.trim().to_string(),
            merge_base,
        })
    }

    fn submit_edit_file_source(&mut self, text: &str) -> bool {
        let path = text.trim();
        if path.is_empty() {
            self.status_message = Some("file path cannot be empty".to_string());
            return true;
        }
        self.replace_selected_source_kind(ReviewSourceKind::File {
            path: path.to_string(),
        })
    }

    fn submit_edit_file_range_source(&mut self, text: &str) -> bool {
        let Some((path, start, end)) = parse_file_range_spec(text) else {
            self.status_message = Some("enter file range as path:start-end".to_string());
            return true;
        };
        self.replace_selected_source_kind(ReviewSourceKind::FileRange { path, start, end })
    }

    fn submit_rename_workspace(&mut self, text: &str) -> bool {
        let title = text.trim();
        if title.is_empty() {
            self.status_message = Some("workspace title cannot be empty".to_string());
            return true;
        }
        self.workspace.title = title.to_string();
        self.review.workspace = Some(self.workspace.clone());
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        self.status_message = Some(format!("renamed workspace to {title}"));
        true
    }

    fn submit_rename_source(&mut self, text: &str) -> bool {
        let label = text.trim();
        if label.is_empty() {
            self.status_message = Some("source label cannot be empty".to_string());
            return true;
        }
        let Some(index) = self.selected_build_source_index() else {
            self.status_message = Some("select an included source to rename".to_string());
            return true;
        };
        let Some(source) = self.workspace.sources.get_mut(index) else {
            self.status_message = Some("select an included source to rename".to_string());
            return true;
        };
        source.label = label.to_string();
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        self.status_message = Some(format!("renamed source to {label}"));
        true
    }

    fn submit_file_picker(&mut self, query: &str, selected: usize) -> bool {
        let matches = self.file_picker_matches(query);
        let Some(index) = matches
            .get(selected)
            .copied()
            .or_else(|| matches.first().copied())
        else {
            self.status_message = Some(format!("no file matches `{query}`"));
            return true;
        };
        self.select_file(index);
        self.status_message = Some(format!(
            "opened {}",
            self.review.files[index].display_path()
        ));
        true
    }

    fn submit_jump_to_line(&mut self, text: &str) -> bool {
        let Ok(line) = text.parse::<usize>() else {
            self.status_message = Some(format!("invalid line number `{text}`"));
            return true;
        };
        self.select_diff_line(line.saturating_sub(1));
        self.status_message = Some(format!("jumped to line {line}"));
        true
    }

    fn submit_file_search(&mut self, query: &str) -> bool {
        if query.is_empty() {
            self.status_message = Some("empty search query".to_string());
            return true;
        }
        let Some(file) = self.selected_file_data() else {
            return false;
        };
        let Some(cached) = self.file_cache.get(file.display_path()) else {
            self.status_message = Some("current file is not loaded".to_string());
            return true;
        };
        let next = find_file_match(
            cached,
            query,
            self.selected_diff_line,
            SearchDirection::Next,
        );
        match next {
            Some(index) => {
                self.last_search_query = Some(query.to_string());
                self.select_diff_line(index);
                self.status_message = Some(format!("found `{query}`"));
            }
            None => self.status_message = Some(format!("no match for `{query}`")),
        }
        true
    }

    /// Return true when file search has an active query.
    #[must_use]
    pub const fn has_active_search(&self) -> bool {
        self.last_search_query.is_some()
    }

    /// Jump to next current-file search match.
    pub fn search_next_match(&mut self) -> bool {
        self.search_match(SearchDirection::Next)
    }

    /// Jump to previous current-file search match.
    pub fn search_previous_match(&mut self) -> bool {
        self.search_match(SearchDirection::Previous)
    }

    fn search_match(&mut self, direction: SearchDirection) -> bool {
        let Some(query) = self.last_search_query.clone() else {
            self.status_message = Some("no active search; press / first".to_string());
            return true;
        };
        let Some(file) = self.selected_file_data() else {
            return false;
        };
        let Some(cached) = self.file_cache.get(file.display_path()) else {
            self.status_message = Some("current file is not loaded".to_string());
            return true;
        };
        match find_file_match(cached, &query, self.selected_diff_line, direction) {
            Some(index) => {
                self.select_diff_line(index);
                self.status_message = Some(format!("found `{query}`"));
            }
            None => self.status_message = Some(format!("no match for `{query}`")),
        }
        true
    }

    /// Move prompt selected row down.
    pub fn move_prompt_selection_down(&mut self) -> bool {
        let Some(prompt) = &self.prompt_state else {
            return false;
        };
        if !matches!(
            prompt.kind,
            ReviewPromptKind::FilePicker
                | ReviewPromptKind::AddSourceKind
                | ReviewPromptKind::AddCommitPicker
                | ReviewPromptKind::AddCommitRangeBasePicker
                | ReviewPromptKind::AddCommitRangeHeadPicker { .. }
                | ReviewPromptKind::AddFileSourcePicker
                | ReviewPromptKind::AddFileRangePathPicker
                | ReviewPromptKind::AddBranchCompareBasePicker
                | ReviewPromptKind::AddBranchCompareHeadPicker { .. }
        ) {
            return false;
        }
        let max = match &prompt.kind {
            ReviewPromptKind::AddSourceKind => add_source_menu_items().len().saturating_sub(1),
            ReviewPromptKind::AddCommitPicker
            | ReviewPromptKind::AddCommitRangeBasePicker
            | ReviewPromptKind::AddCommitRangeHeadPicker { .. } => self
                .repository_commit_picker_matches(prompt.buffer.text())
                .len()
                .saturating_sub(1),
            ReviewPromptKind::AddFileSourcePicker | ReviewPromptKind::AddFileRangePathPicker => {
                self.repository_file_picker_matches(prompt.buffer.text())
                    .len()
                    .saturating_sub(1)
            }
            ReviewPromptKind::AddBranchCompareBasePicker
            | ReviewPromptKind::AddBranchCompareHeadPicker { .. } => self
                .repository_branch_picker_matches(prompt.buffer.text())
                .len()
                .saturating_sub(1),
            _ => self
                .file_picker_matches(prompt.buffer.text())
                .len()
                .saturating_sub(1),
        };
        let Some(prompt) = &mut self.prompt_state else {
            return false;
        };
        prompt.selected = prompt.selected.saturating_add(1).min(max);
        true
    }

    /// Move prompt selected row up.
    pub const fn move_prompt_selection_up(&mut self) -> bool {
        let Some(prompt) = &mut self.prompt_state else {
            return false;
        };
        if !matches!(
            prompt.kind,
            ReviewPromptKind::FilePicker
                | ReviewPromptKind::AddSourceKind
                | ReviewPromptKind::AddCommitPicker
                | ReviewPromptKind::AddCommitRangeBasePicker
                | ReviewPromptKind::AddCommitRangeHeadPicker { .. }
                | ReviewPromptKind::AddFileSourcePicker
                | ReviewPromptKind::AddFileRangePathPicker
                | ReviewPromptKind::AddBranchCompareBasePicker
                | ReviewPromptKind::AddBranchCompareHeadPicker { .. }
        ) {
            return false;
        }
        prompt.selected = prompt.selected.saturating_sub(1);
        true
    }

    /// Return file picker matches for a query.
    #[must_use]
    pub fn file_picker_matches(&self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();
        self.review
            .files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| {
                let path = file.display_path().to_lowercase();
                (query.is_empty() || path.contains(&query)).then_some(index)
            })
            .take(12)
            .collect()
    }

    /// Return repository file matches for source pickers.
    #[must_use]
    pub fn repository_file_picker_matches(&self, query: &str) -> Vec<String> {
        let query = query.to_lowercase();
        self.review
            .repository_files
            .iter()
            .filter(|path| query.is_empty() || path.to_lowercase().contains(&query))
            .take(12)
            .cloned()
            .collect()
    }

    /// Return repository commit matches for source pickers.
    #[must_use]
    pub fn repository_commit_picker_matches(&self, query: &str) -> Vec<ReviewRepositoryCommit> {
        let query = query.to_lowercase();
        self.review
            .repository_commits
            .iter()
            .filter(|commit| {
                query.is_empty()
                    || commit.rev.to_lowercase().contains(&query)
                    || commit.short_rev.to_lowercase().contains(&query)
                    || commit.subject.to_lowercase().contains(&query)
            })
            .take(12)
            .cloned()
            .collect()
    }

    /// Return repository branch matches for source pickers.
    #[must_use]
    pub fn repository_branch_picker_matches(&self, query: &str) -> Vec<String> {
        let query = query.to_lowercase();
        self.review
            .repository_branches
            .iter()
            .filter(|branch| query.is_empty() || branch.to_lowercase().contains(&query))
            .take(12)
            .cloned()
            .collect()
    }

    fn review_item_count(&self) -> usize {
        self.review.files.len().max(self.review.surfaces().len())
    }

    /// Return visible file-tree rows for repository review.
    #[must_use]
    pub fn file_tree_rows(&self) -> Vec<ReviewFileTreeRow> {
        let mut rows = Vec::new();
        self.push_tree_rows(Path::new(""), 0, &mut rows);
        rows
    }

    fn push_tree_rows(&self, prefix: &Path, depth: usize, rows: &mut Vec<ReviewFileTreeRow>) {
        let mut dirs = BTreeSet::new();
        let mut files = Vec::new();
        for index in 0..self.review_item_count() {
            let Some(path) = self.review_path_for_index(index) else {
                continue;
            };
            let path = Path::new(&path);
            let rest = if prefix.as_os_str().is_empty() {
                path
            } else if let Ok(rest) = path.strip_prefix(prefix) {
                rest
            } else {
                continue;
            };
            let mut components = rest.components();
            let Some(first) = components.next() else {
                continue;
            };
            if components.next().is_some() {
                dirs.insert(prefix.join(first.as_os_str()));
            } else {
                files.push(index);
            }
        }
        for dir in dirs {
            rows.push(ReviewFileTreeRow::Directory {
                path: dir.clone(),
                depth,
            });
            if self.expanded_dirs.contains(&dir) {
                self.push_tree_rows(&dir, depth.saturating_add(1), rows);
            }
        }
        for index in files {
            rows.push(ReviewFileTreeRow::File { index, depth });
        }
    }

    /// Activate the selected repository tree row.
    pub fn activate_selected_tree_row(&mut self) -> bool {
        let rows = self.file_tree_rows();
        match rows.get(self.selected_tree_row).cloned() {
            Some(ReviewFileTreeRow::Directory { path, .. }) => {
                self.toggle_file_tree_directory(&path)
            }
            Some(ReviewFileTreeRow::File { index, .. }) => self.select_file(index),
            None => false,
        }
    }

    /// Toggle a directory in the repository sidebar.
    pub fn toggle_file_tree_directory(&mut self, path: &Path) -> bool {
        if self.expanded_dirs.remove(path) {
            return true;
        }
        self.expanded_dirs.insert(path.to_path_buf());
        true
    }

    /// Store the current file hit area.
    pub const fn set_file_area(&mut self, area: Option<Rect>) {
        self.last_file_area = area;
    }

    /// Store the current diff hit area.
    pub const fn set_diff_area(&mut self, area: Rect) {
        self.last_diff_area = Some(area);
    }

    /// Return currently selected surface.
    #[must_use]
    pub fn selected_surface(&self) -> Option<ReviewSurface> {
        self.review.surfaces().get(self.selected_file).cloned()
    }

    fn selected_surface_ids(&self) -> (Option<String>, Option<String>) {
        self.selected_surface().map_or((None, None), |surface| {
            (Some(surface.id), Some(surface.source_id))
        })
    }

    /// Return currently selected file.
    #[must_use]
    pub fn selected_file_data(&self) -> Option<&ReviewFile> {
        self.review.files.get(self.selected_file)
    }

    /// Return review file/surface path for an index.
    #[must_use]
    pub fn review_path_for_index(&self, index: usize) -> Option<String> {
        self.review
            .files
            .get(index)
            .map(|file| file.display_path().to_string())
            .or_else(|| {
                self.review
                    .surfaces()
                    .get(index)
                    .map(|surface| surface.path.clone())
            })
    }

    /// Return currently selected file path.
    #[must_use]
    pub fn selected_file_path(&self) -> Option<String> {
        self.selected_file_data()
            .map(|file| file.display_path().to_string())
            .or_else(|| self.selected_surface().map(|surface| surface.path))
    }

    /// Replace review content after rematerialization.
    pub fn replace_review(&mut self, review: ReviewSummary) {
        self.save_current_file_viewport();
        let previous_path = self.selected_file_path();
        let workspace = review.workspace();
        self.review = review;
        self.workspace = workspace;
        self.sync_review_workspace();
        self.selected_file = previous_path
            .as_deref()
            .and_then(|path| self.review_file_index_for_path(path))
            .unwrap_or(self.selected_file)
            .min(self.review_item_count().saturating_sub(1));
        self.selected_build_row = self
            .selected_build_row
            .min(self.build_row_count().saturating_sub(1));
        self.build_scroll = self.build_scroll.min(self.selected_build_row);
        self.file_scroll = self
            .file_scroll
            .min(self.review.files.len().saturating_sub(1));
        self.tree_scroll = self
            .tree_scroll
            .min(self.file_tree_rows().len().saturating_sub(1));
        self.restore_current_file_viewport();
        self.queue_selected_file_load();
    }

    /// Focus the attention sidebar.
    pub fn show_attention_sidebar(&mut self) -> bool {
        self.sidebar_mode = ReviewSidebarMode::NeedsAttention;
        self.sidebar_visible = true;
        self.selected_thread = self
            .selected_thread
            .min(self.visible_thread_summaries().len().saturating_sub(1));
        self.thread_scroll = self.thread_scroll.min(self.selected_thread);
        self.status_message = Some("sidebar: attention".to_string());
        true
    }

    /// Toggle sidebar between included, repository, threads, sources, and attention.
    pub fn toggle_sidebar_mode(&mut self) -> bool {
        self.sidebar_mode = match self.sidebar_mode {
            ReviewSidebarMode::Included => ReviewSidebarMode::Repository,
            ReviewSidebarMode::Repository => ReviewSidebarMode::Threads,
            ReviewSidebarMode::Threads => ReviewSidebarMode::Sources,
            ReviewSidebarMode::Sources => ReviewSidebarMode::NeedsAttention,
            ReviewSidebarMode::NeedsAttention => ReviewSidebarMode::Included,
        };
        self.sidebar_visible = true;
        self.status_message = Some(format!("sidebar: {}", self.sidebar_mode.label()));
        true
    }

    /// Move the active selection down.
    pub fn move_down(&mut self, rows: usize) -> bool {
        if self.ux_mode == ReviewUxMode::Build {
            self.select_next_build_row(rows)
        } else if matches!(
            self.sidebar_mode,
            ReviewSidebarMode::Threads | ReviewSidebarMode::NeedsAttention
        ) && self.sidebar_visible
        {
            self.select_next_thread(rows)
        } else if self.review.is_repository_review()
            && self.sidebar_mode == ReviewSidebarMode::Repository
            && self.sidebar_visible
        {
            self.select_next_tree_row(rows)
        } else {
            self.select_next_view_row(rows)
        }
    }

    /// Move the active selection up.
    pub fn move_up(&mut self, rows: usize) -> bool {
        if self.ux_mode == ReviewUxMode::Build {
            self.select_previous_build_row(rows)
        } else if matches!(
            self.sidebar_mode,
            ReviewSidebarMode::Threads | ReviewSidebarMode::NeedsAttention
        ) && self.sidebar_visible
        {
            self.select_previous_thread(rows)
        } else if self.review.is_repository_review()
            && self.sidebar_mode == ReviewSidebarMode::Repository
            && self.sidebar_visible
        {
            self.select_previous_tree_row(rows)
        } else {
            self.select_previous_view_row(rows)
        }
    }

    #[must_use]
    pub fn source_diagnostics(&self, source_id: &str) -> Vec<&ReviewSourceDiagnostic> {
        self.review
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.source_id == source_id)
            .collect()
    }

    fn has_materialization_errors(&self) -> bool {
        self.review
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == ReviewSourceDiagnosticSeverity::Error)
    }

    /// Count included workspace sources.
    #[must_use]
    pub fn included_source_count(&self) -> usize {
        self.workspace
            .sources
            .iter()
            .filter(|source| source.included)
            .count()
    }

    /// Count workspace sources by diagnostic severity.
    #[must_use]
    pub fn diagnostic_source_counts(&self) -> (usize, usize, usize) {
        let mut info_sources = BTreeSet::new();
        let mut warning_sources = BTreeSet::new();
        let mut error_sources = BTreeSet::new();
        for diagnostic in &self.review.diagnostics {
            match diagnostic.severity {
                ReviewSourceDiagnosticSeverity::Info => {
                    info_sources.insert(diagnostic.source_id.clone());
                }
                ReviewSourceDiagnosticSeverity::Warning => {
                    warning_sources.insert(diagnostic.source_id.clone());
                }
                ReviewSourceDiagnosticSeverity::Error => {
                    error_sources.insert(diagnostic.source_id.clone());
                }
            }
        }
        (
            info_sources.len(),
            warning_sources.len(),
            error_sources.len(),
        )
    }

    /// Return number of rows in build mode.
    #[must_use]
    pub fn build_row_count(&self) -> usize {
        self.workspace
            .sources
            .len()
            .saturating_add(self.review.surfaces().len())
            .max(1)
    }

    /// Select first build row.
    pub fn select_first_build_row(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        self.set_selected_build_row(0);
        true
    }

    /// Select last build row.
    pub fn select_last_build_row(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        self.set_selected_build_row(self.build_row_count().saturating_sub(1));
        true
    }

    /// Select next build row.
    pub fn select_next_build_row(&mut self, rows: usize) -> bool {
        if self.workspace.sources.is_empty() {
            self.status_message = Some("no sources yet; press A or u/s/w/l to add one".to_string());
            return true;
        }
        let max = self.build_row_count().saturating_sub(1);
        let next = self.selected_build_row.saturating_add(rows).min(max);
        if next == self.selected_build_row {
            return false;
        }
        self.set_selected_build_row(next);
        true
    }

    /// Select previous build row.
    pub fn select_previous_build_row(&mut self, rows: usize) -> bool {
        if self.workspace.sources.is_empty() {
            self.status_message = Some("no sources yet; press A or u/s/w/l to add one".to_string());
            return true;
        }
        let next = self.selected_build_row.saturating_sub(rows);
        if next == self.selected_build_row {
            return false;
        }
        self.set_selected_build_row(next);
        true
    }

    fn set_selected_build_row(&mut self, row: usize) {
        self.selected_build_row = row.min(self.build_row_count().saturating_sub(1));
        if row == 0 {
            self.build_scroll = 0;
        } else {
            self.build_scroll = self.build_scroll.min(self.selected_build_row);
        }
    }

    fn selected_build_source_index(&self) -> Option<usize> {
        (self.selected_build_row < self.workspace.sources.len()).then_some(self.selected_build_row)
    }

    fn selected_build_surface_index(&self) -> Option<usize> {
        self.selected_build_row
            .checked_sub(self.workspace.sources.len())
            .filter(|index| *index < self.review.surfaces().len())
    }

    fn first_surface_index_for_source(&self, source_id: &str) -> Option<usize> {
        self.review
            .surfaces()
            .iter()
            .position(|surface| surface.source_id == source_id)
    }

    /// Activate review mode at the first surface for the selected source.
    pub fn open_selected_source_surface(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        let Some(source_index) = self.selected_build_source_index() else {
            self.status_message = Some("select a source to open its first surface".to_string());
            return true;
        };
        let Some(source_id) = self
            .workspace
            .sources
            .get(source_index)
            .map(|source| source.id.clone())
        else {
            self.status_message = Some("select a source to open its first surface".to_string());
            return true;
        };
        let Some(surface_index) = self.first_surface_index_for_source(&source_id) else {
            self.status_message = Some("selected source has no materialized surfaces".to_string());
            return true;
        };
        self.ux_mode = ReviewUxMode::Review;
        let _ = self.select_file(surface_index);
        self.status_message = Some("opened source surface".to_string());
        true
    }

    /// Select the build source for the current review surface.
    pub fn select_source_for_current_surface(&mut self) -> bool {
        let Some(source_id) = self.selected_surface().map(|surface| surface.source_id) else {
            self.status_message = Some("current surface has no source".to_string());
            return true;
        };
        let Some(source_index) = self
            .workspace
            .sources
            .iter()
            .position(|source| source.id == source_id)
        else {
            self.status_message =
                Some("current surface source is no longer in workspace".to_string());
            return true;
        };
        self.ux_mode = ReviewUxMode::Build;
        self.set_selected_build_row(source_index);
        self.status_message = Some("selected source for current surface".to_string());
        true
    }

    /// Activate selected build-mode row.
    pub fn activate_selected_build_row(&mut self) -> bool {
        if self.selected_build_source_index().is_some() {
            return self.toggle_selected_build_source();
        }
        let Some(surface_index) = self.selected_build_surface_index() else {
            return false;
        };
        self.ux_mode = ReviewUxMode::Review;
        let _ = self.select_file(surface_index);
        self.status_message = Some("opened review surface".to_string());
        true
    }

    /// Take pending workspace save flag.
    pub const fn take_pending_workspace_save(&mut self) -> bool {
        let pending = self.pending_workspace_save;
        self.pending_workspace_save = false;
        pending
    }

    /// Take pending workspace reload flag.
    pub const fn take_pending_workspace_reload(&mut self) -> bool {
        let pending = self.pending_workspace_reload;
        self.pending_workspace_reload = false;
        pending
    }

    /// Rematerialize workspace content.
    pub fn rematerialize_workspace(&mut self) -> bool {
        self.pending_workspace_reload = true;
        self.status_message = Some("refreshing review sources".to_string());
        true
    }

    /// Add a quick source while in build mode.
    pub fn add_quick_source(&mut self, kind: ReviewSourceKind) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        self.push_workspace_source(kind)
    }

    /// Add the selected file as an included workspace source.
    pub fn add_selected_file_to_workspace(&mut self) -> bool {
        let Some(path) = self
            .selected_file_data()
            .map(|file| file.display_path().to_string())
        else {
            self.status_message = Some("no selected file to add".to_string());
            return true;
        };
        if self
            .workspace
            .sources
            .iter()
            .any(|source| matches!(&source.kind, ReviewSourceKind::File { path: source_path } if source_path == &path))
        {
            self.status_message = Some(format!("{path} is already included"));
            return true;
        }
        self.push_workspace_source(ReviewSourceKind::File { path })
    }

    fn set_all_sources_included(&mut self, included: bool) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        if self.workspace.sources.is_empty() {
            self.status_message = Some("no sources yet; press A or u/s/w/l to add one".to_string());
            return true;
        }
        let mut changed = false;
        for source in &mut self.workspace.sources {
            if source.included != included {
                source.included = included;
                changed = true;
            }
        }
        if !changed {
            self.status_message = Some(if included {
                "all sources are already included".to_string()
            } else {
                "all sources are already excluded".to_string()
            });
            return true;
        }
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        self.pending_workspace_reload = true;
        self.status_message = Some(if included {
            "included all sources".to_string()
        } else {
            "excluded all sources".to_string()
        });
        true
    }

    /// Include all workspace sources.
    pub fn include_all_sources(&mut self) -> bool {
        self.set_all_sources_included(true)
    }

    /// Exclude all workspace sources.
    pub fn exclude_all_sources(&mut self) -> bool {
        self.set_all_sources_included(false)
    }

    /// Invert source inclusion for all workspace sources.
    pub fn invert_source_inclusion(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        if self.workspace.sources.is_empty() {
            self.status_message = Some("no sources yet; press A or u/s/w/l to add one".to_string());
            return true;
        }
        for source in &mut self.workspace.sources {
            source.included = !source.included;
        }
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        self.pending_workspace_reload = true;
        self.status_message = Some("inverted source inclusion".to_string());
        true
    }

    fn source_has_surfaces(&self, source_id: &str) -> bool {
        self.review
            .surfaces()
            .iter()
            .any(|surface| surface.source_id == source_id)
    }

    fn select_empty_source(&mut self, forward: bool) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        let source_count = self.workspace.sources.len();
        if source_count == 0 {
            self.status_message = Some("no sources yet; press A or u/s/w/l to add one".to_string());
            return true;
        }
        let step = if forward {
            1
        } else {
            source_count.saturating_sub(1)
        };
        let mut index = self.selected_build_row % source_count;
        for _ in 0..source_count {
            index = index.saturating_add(step) % source_count;
            let source = &self.workspace.sources[index];
            if source.included && !self.source_has_surfaces(&source.id) {
                self.set_selected_build_row(index);
                self.status_message = Some("selected included source with no surfaces".to_string());
                return true;
            }
        }
        self.status_message = Some("no included sources without surfaces".to_string());
        true
    }

    /// Select next included source that materialized no surfaces.
    pub fn select_next_empty_source(&mut self) -> bool {
        self.select_empty_source(true)
    }

    /// Select previous included source that materialized no surfaces.
    pub fn select_previous_empty_source(&mut self) -> bool {
        self.select_empty_source(false)
    }

    /// Exclude included sources that currently materialize no surfaces.
    pub fn exclude_empty_sources(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        let surface_source_ids: BTreeSet<String> = self
            .review
            .surfaces()
            .iter()
            .map(|surface| surface.source_id.clone())
            .collect();
        let mut excluded = 0usize;
        for source in &mut self.workspace.sources {
            if source.included && !surface_source_ids.contains(&source.id) {
                source.included = false;
                excluded = excluded.saturating_add(1);
            }
        }
        if excluded == 0 {
            self.status_message = Some("no empty included sources to exclude".to_string());
            return true;
        }
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        self.pending_workspace_reload = true;
        self.status_message = Some(format!("excluded {excluded} empty source(s)"));
        true
    }

    /// Select the next source with diagnostics.
    pub fn select_next_diagnostic_source(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        if self.review.diagnostics.is_empty() {
            self.status_message = Some("no source diagnostics".to_string());
            return true;
        }
        let source_count = self.workspace.sources.len();
        if source_count == 0 {
            self.status_message =
                Some("diagnostics have no matching workspace sources".to_string());
            return true;
        }
        let start = self.selected_build_row.saturating_add(1);
        for offset in 0..source_count {
            let index = start.saturating_add(offset) % source_count;
            let source_id = self.workspace.sources[index].id.clone();
            if self
                .review
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.source_id == source_id)
            {
                self.set_selected_build_row(index);
                self.status_message = Some("selected source with diagnostics".to_string());
                return true;
            }
        }
        self.status_message = Some("diagnostics have no matching workspace sources".to_string());
        true
    }

    /// Exclude all sources that have error diagnostics.
    pub fn exclude_sources_with_errors(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        let error_source_ids: BTreeSet<String> = self
            .review
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == ReviewSourceDiagnosticSeverity::Error)
            .map(|diagnostic| diagnostic.source_id.clone())
            .collect();
        if error_source_ids.is_empty() {
            self.status_message = Some("no source errors to exclude".to_string());
            return true;
        }
        let mut excluded = 0usize;
        for source in &mut self.workspace.sources {
            if source.included && error_source_ids.contains(&source.id) {
                source.included = false;
                excluded = excluded.saturating_add(1);
            }
        }
        if excluded == 0 {
            self.status_message = Some("error sources are already excluded".to_string());
            return true;
        }
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        self.pending_workspace_reload = true;
        self.status_message = Some(format!("excluded {excluded} source(s) with errors"));
        true
    }

    /// Toggle merge-base semantics for selected range/compare source.
    pub fn toggle_selected_source_merge_base(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        let Some(index) = self.selected_build_source_index() else {
            self.status_message =
                Some("select a commit range or branch compare source".to_string());
            return true;
        };
        let Some(source) = self.workspace.sources.get_mut(index) else {
            self.status_message =
                Some("select a commit range or branch compare source".to_string());
            return true;
        };
        let status = match &mut source.kind {
            ReviewSourceKind::CommitRange { merge_base, .. }
            | ReviewSourceKind::BranchCompare { merge_base, .. } => {
                *merge_base = !*merge_base;
                if *merge_base { "merge-base" } else { "direct" }
            }
            _ => {
                self.status_message =
                    Some("selected source does not support merge-base toggling".to_string());
                return true;
            }
        };
        source.label = source.kind.label();
        let label = source.label.clone();
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        self.pending_workspace_reload = true;
        self.status_message = Some(format!("set {label} comparison to {status}"));
        true
    }

    /// Remove all excluded sources from the workspace.
    pub fn remove_excluded_sources(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        let before = self.workspace.sources.len();
        self.workspace.sources.retain(|source| source.included);
        let removed = before.saturating_sub(self.workspace.sources.len());
        if removed == 0 {
            self.status_message = Some("no excluded sources to remove".to_string());
            return true;
        }
        self.set_selected_build_row(self.selected_build_row);
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        self.pending_workspace_reload = true;
        self.status_message = Some(format!("removed {removed} excluded source(s)"));
        true
    }

    /// Remove duplicate sources from the workspace, preserving the first occurrence.
    pub fn remove_duplicate_sources(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        let mut seen = Vec::new();
        let before = self.workspace.sources.len();
        self.workspace.sources.retain(|source| {
            if seen.contains(&source.kind) {
                false
            } else {
                seen.push(source.kind.clone());
                true
            }
        });
        let removed = before.saturating_sub(self.workspace.sources.len());
        if removed == 0 {
            self.status_message = Some("no duplicate sources to remove".to_string());
            return true;
        }
        self.set_selected_build_row(self.selected_build_row);
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        self.pending_workspace_reload = true;
        self.status_message = Some(format!("removed {removed} duplicate source(s)"));
        true
    }

    /// Toggle whether the selected workspace source is included.
    pub fn toggle_selected_build_source(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        let Some(index) = self.selected_build_source_index() else {
            self.status_message = Some("select an included source to toggle".to_string());
            return true;
        };
        let Some(source) = self.workspace.sources.get_mut(index) else {
            self.status_message = Some("select an included source to toggle".to_string());
            return true;
        };
        source.included = !source.included;
        let included = source.included;
        let label = source.label.clone();
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        self.pending_workspace_reload = true;
        self.status_message = Some(format!(
            "{} {}",
            if included { "included" } else { "excluded" },
            label
        ));
        true
    }

    /// Move the selected workspace source earlier.
    pub fn move_selected_source_up(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        let Some(index) = self.selected_build_source_index() else {
            return false;
        };
        if index == 0 {
            return false;
        }
        self.workspace.sources.swap(index, index - 1);
        self.set_selected_build_row(index - 1);
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        self.pending_workspace_reload = true;
        true
    }

    /// Move the selected workspace source later.
    pub fn move_selected_source_down(&mut self) -> bool {
        let Some(index) = self.selected_build_source_index() else {
            return false;
        };
        if index.saturating_add(1) >= self.workspace.sources.len() {
            return false;
        }
        self.workspace.sources.swap(index, index + 1);
        self.set_selected_build_row(index + 1);
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        self.pending_workspace_reload = true;
        true
    }

    /// Remove selected build source from the workspace.
    pub fn remove_selected_build_source(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        let Some(index) = self.selected_build_source_index() else {
            self.status_message = Some("select an included source to remove".to_string());
            return true;
        };
        let source = self.workspace.sources.remove(index);
        self.set_selected_build_row(self.selected_build_row);
        self.status_message = Some(format!("removed {}", source.label));
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        self.pending_workspace_reload = true;
        true
    }

    /// Select next repository tree row.
    pub fn select_next_tree_row(&mut self, rows: usize) -> bool {
        let max = self.file_tree_rows().len().saturating_sub(1);
        let next = self.selected_tree_row.saturating_add(rows).min(max);
        if next == self.selected_tree_row {
            return false;
        }
        self.selected_tree_row = next;
        true
    }

    /// Select previous repository tree row.
    pub const fn select_previous_tree_row(&mut self, rows: usize) -> bool {
        let next = self.selected_tree_row.saturating_sub(rows);
        if next == self.selected_tree_row {
            return false;
        }
        self.selected_tree_row = next;
        true
    }

    /// Expand selected directory row.
    pub fn expand_selected_tree_row(&mut self) -> bool {
        match self.file_tree_rows().get(self.selected_tree_row).cloned() {
            Some(ReviewFileTreeRow::Directory { path, .. }) => self.expanded_dirs.insert(path),
            _ => false,
        }
    }

    /// Collapse selected directory row or selected file parent.
    pub fn collapse_selected_tree_row(&mut self) -> bool {
        match self.file_tree_rows().get(self.selected_tree_row).cloned() {
            Some(ReviewFileTreeRow::Directory { path, .. }) => self.expanded_dirs.remove(&path),
            Some(ReviewFileTreeRow::File { index, .. }) => {
                let Some(path) = self.review_path_for_index(index) else {
                    return false;
                };
                let Some(parent) = Path::new(&path).parent().map(Path::to_path_buf) else {
                    return false;
                };
                self.expanded_dirs.remove(&parent)
            }
            None => false,
        }
    }

    /// Return open and resolved thread counts.
    #[must_use]
    pub fn thread_status_counts(&self) -> (usize, usize) {
        let summaries = self.thread_summaries();
        let total = summaries.len();
        let resolved = summaries.iter().filter(|thread| thread.resolved).count();
        (total.saturating_sub(resolved), resolved)
    }

    /// Return review thread summaries in deterministic order.
    #[must_use]
    pub fn thread_summaries(&self) -> Vec<ReviewThreadSummary> {
        self.draft_comments
            .iter()
            .filter_map(|(anchor, comments)| {
                let latest = comments.last()?;
                Some(ReviewThreadSummary {
                    anchor: anchor.clone(),
                    draft_count: comments.len(),
                    latest_body: latest.body.clone(),
                    session_id: latest.session_id.clone(),
                    resolved: self
                        .resolved_review_threads
                        .contains(&Self::thread_key_for_anchor(anchor)),
                })
            })
            .collect()
    }

    /// Return review thread summaries visible in the sidebar.
    #[must_use]
    pub fn visible_thread_summaries(&self) -> Vec<ReviewThreadSummary> {
        if self.sidebar_mode == ReviewSidebarMode::NeedsAttention {
            return self
                .thread_summaries()
                .into_iter()
                .filter(|thread| !thread.resolved)
                .collect();
        }
        self.thread_summaries()
            .into_iter()
            .filter(|thread| match self.thread_filter {
                ReviewThreadFilter::All => true,
                ReviewThreadFilter::Open => !thread.resolved,
                ReviewThreadFilter::Resolved => thread.resolved,
            })
            .collect()
    }

    /// Cycle thread sidebar filter.
    pub fn cycle_thread_filter(&mut self) -> bool {
        self.thread_filter = self.thread_filter.next();
        self.selected_thread = 0;
        self.thread_scroll = 0;
        self.status_message = Some(format!(
            "showing {} review threads",
            self.thread_filter.label()
        ));
        true
    }

    /// Select next thread.
    pub fn select_next_thread(&mut self, rows: usize) -> bool {
        let max = self.visible_thread_summaries().len().saturating_sub(1);
        let next = self.selected_thread.saturating_add(rows).min(max);
        if next == self.selected_thread {
            return false;
        }
        self.selected_thread = next;
        true
    }

    /// Select a thread by absolute index.
    pub fn select_thread(&mut self, index: usize) -> bool {
        if index >= self.visible_thread_summaries().len() || index == self.selected_thread {
            return false;
        }
        self.selected_thread = index;
        true
    }

    /// Select previous thread.
    pub const fn select_previous_thread(&mut self, rows: usize) -> bool {
        let next = self.selected_thread.saturating_sub(rows);
        if next == self.selected_thread {
            return false;
        }
        self.selected_thread = next;
        true
    }

    /// Clear selected inline review target.
    pub fn clear_selected_view_target(&mut self) -> bool {
        if self.selected_view_target.is_none() {
            return false;
        }
        self.selected_view_target = None;
        self.status_message = Some("cleared inline selection".to_string());
        true
    }

    /// Select next draft comment in the main pane.
    pub fn select_next_inline_draft(&mut self) -> bool {
        self.select_relative_inline_comment(1)
    }

    /// Select previous draft comment in the main pane.
    pub fn select_previous_inline_draft(&mut self) -> bool {
        self.select_relative_inline_comment(-1)
    }

    fn select_relative_inline_comment(&mut self, offset: isize) -> bool {
        let Some(document) = self.current_review_view_document() else {
            self.status_message = Some("no review comments".to_string());
            return true;
        };
        let comment_rows = document
            .rows
            .iter()
            .filter(|row| {
                matches!(
                    row.block,
                    ReviewViewBlock::InlineComment {
                        body_line_index: 0,
                        ..
                    }
                )
            })
            .collect::<Vec<_>>();
        if comment_rows.is_empty() {
            self.status_message = Some("no review comments".to_string());
            return true;
        }
        let current_visual = self.selected_diff_visual_row();
        let current_index = comment_rows
            .iter()
            .position(|row| row.visual_row >= current_visual)
            .unwrap_or_else(|| comment_rows.len().saturating_sub(1));
        let next_index = if offset.is_negative() {
            current_index.saturating_sub(offset.unsigned_abs())
        } else {
            let step = offset.unsigned_abs();
            let start_index = if comment_rows[current_index].visual_row == current_visual {
                current_index.saturating_add(step)
            } else {
                current_index
            };
            start_index.min(comment_rows.len().saturating_sub(1))
        };
        let Some(row) = comment_rows.get(next_index) else {
            return false;
        };
        self.select_view_visual_row(row.visual_row)
    }

    /// Select next unresolved review thread in the main pane.
    pub fn select_next_open_thread(&mut self) -> bool {
        self.select_relative_open_thread(1)
    }

    /// Select previous unresolved review thread in the main pane.
    pub fn select_previous_open_thread(&mut self) -> bool {
        self.select_relative_open_thread(-1)
    }

    fn select_relative_open_thread(&mut self, offset: isize) -> bool {
        let summaries = self.open_inline_thread_summaries();
        if summaries.is_empty() {
            self.status_message = Some("no unresolved review threads".to_string());
            return true;
        }
        let current_anchor = self.selected_comment_anchor();
        let current_index = current_anchor
            .as_ref()
            .and_then(|anchor| summaries.iter().position(|thread| &thread.anchor == anchor));
        let next_index = relative_index(current_index, summaries.len(), offset);
        self.jump_to_thread_summary(&summaries[next_index])
    }

    /// Select next unresolved review thread across all files.
    pub fn select_next_open_thread_global(&mut self) -> bool {
        self.select_relative_open_thread_global(1)
    }

    /// Select previous unresolved review thread across all files.
    pub fn select_previous_open_thread_global(&mut self) -> bool {
        self.select_relative_open_thread_global(-1)
    }

    fn select_relative_open_thread_global(&mut self, offset: isize) -> bool {
        let summaries = self.open_thread_summaries();
        if summaries.is_empty() {
            self.status_message = Some("no unresolved review threads".to_string());
            return true;
        }
        let current_anchor = self.selected_comment_anchor();
        let current_index = current_anchor
            .as_ref()
            .and_then(|anchor| summaries.iter().position(|thread| &thread.anchor == anchor));
        let next_index = relative_index(current_index, summaries.len(), offset);
        self.jump_to_thread_summary(&summaries[next_index])
    }

    /// Select next review thread in the main pane.
    pub fn select_next_inline_thread(&mut self) -> bool {
        let Some(current_anchor) = self.selected_comment_anchor() else {
            return self.jump_to_thread_index(0);
        };
        let summaries = self.inline_thread_summaries();
        let Some(current_index) = summaries
            .iter()
            .position(|thread| thread.anchor == current_anchor)
        else {
            return self.jump_to_thread_index(0);
        };
        self.jump_to_thread_summary_index(
            current_index
                .saturating_add(1)
                .min(summaries.len().saturating_sub(1)),
        )
    }

    /// Select previous review thread in the main pane.
    pub fn select_previous_inline_thread(&mut self) -> bool {
        let Some(current_anchor) = self.selected_comment_anchor() else {
            return self.jump_to_thread_index(0);
        };
        let summaries = self.inline_thread_summaries();
        let Some(current_index) = summaries
            .iter()
            .position(|thread| thread.anchor == current_anchor)
        else {
            return self.jump_to_thread_index(0);
        };
        self.jump_to_thread_summary_index(current_index.saturating_sub(1))
    }

    fn jump_to_thread_index(&mut self, index: usize) -> bool {
        let Some(thread) = self.visible_thread_summaries().get(index).cloned() else {
            self.status_message = Some("no review threads".to_string());
            return true;
        };
        self.selected_thread = index;
        self.focus_thread_summary(&thread);
        true
    }

    fn jump_to_thread_summary_index(&mut self, index: usize) -> bool {
        let Some(thread) = self.inline_thread_summaries().get(index).cloned() else {
            self.status_message = Some("no review threads".to_string());
            return true;
        };
        self.jump_to_thread_summary(&thread)
    }

    fn jump_to_thread_summary(&mut self, thread: &ReviewThreadSummary) -> bool {
        self.sync_selected_thread_to_specific_anchor(&thread.anchor);
        self.focus_thread_summary(thread);
        true
    }

    fn open_inline_thread_summaries(&self) -> Vec<ReviewThreadSummary> {
        self.inline_thread_summaries()
            .into_iter()
            .filter(|thread| !thread.resolved)
            .collect()
    }

    fn open_thread_summaries(&self) -> Vec<ReviewThreadSummary> {
        self.thread_summaries()
            .into_iter()
            .filter(|thread| !thread.resolved)
            .collect()
    }

    fn inline_thread_summaries(&self) -> Vec<ReviewThreadSummary> {
        let mut summaries = self
            .thread_summaries()
            .into_iter()
            .filter(|thread| thread.anchor.file_index == self.selected_file)
            .collect::<Vec<_>>();
        summaries.sort_by_key(|thread| {
            (
                thread.anchor.file_index,
                thread.anchor.start_diff_row(),
                thread.anchor.end_diff_row(),
                thread.anchor.path.clone(),
            )
        });
        summaries
    }

    fn focus_thread_summary(&mut self, thread: &ReviewThreadSummary) {
        self.select_anchor(&thread.anchor);
        self.selected_view_target = Some(ReviewViewTarget::Thread {
            thread_key: Self::thread_key_for_anchor(&thread.anchor),
        });
        self.ensure_selected_diff_line_visible();
        self.status_message = Some("selected review thread".to_string());
    }

    /// Jump to the selected thread in the diff.
    pub fn jump_to_selected_thread(&mut self) -> bool {
        if !matches!(
            self.sidebar_mode,
            ReviewSidebarMode::Threads | ReviewSidebarMode::NeedsAttention
        ) {
            return false;
        }
        let Some(thread) = self
            .visible_thread_summaries()
            .get(self.selected_thread)
            .cloned()
        else {
            self.status_message = Some("no review thread selected".to_string());
            return true;
        };
        self.select_anchor(&thread.anchor);
        self.selected_view_target = Some(ReviewViewTarget::Thread {
            thread_key: Self::thread_key_for_anchor(&thread.anchor),
        });
        self.ensure_selected_diff_line_visible();
        self.status_message = Some("jumped to review thread".to_string());
        true
    }

    /// Select an anchor in the diff.
    pub fn select_anchor(&mut self, anchor: &ReviewCommentAnchor) {
        self.selected_file = anchor.file_index;
        self.selected_diff_line = anchor.diff_row;
        self.ensure_selected_diff_line_visible();
    }

    fn review_file_index_for_path(&self, path: &str) -> Option<usize> {
        self.review
            .files
            .iter()
            .position(|file| file.display_path() == path)
            .or_else(|| {
                self.review
                    .surfaces()
                    .iter()
                    .position(|surface| surface.path == path)
            })
    }

    fn save_current_file_viewport(&mut self) {
        let Some(path) = self.selected_file_path() else {
            return;
        };
        self.file_viewports.insert(
            path,
            ReviewViewportState {
                diff_scroll: self.diff_scroll,
                selected_diff_line: self.selected_diff_line,
            },
        );
    }

    fn restore_current_file_viewport(&mut self) {
        let Some(path) = self.selected_file_path() else {
            self.diff_scroll = 0;
            self.selected_diff_line = 0;
            return;
        };
        if let Some(viewport) = self.file_viewports.get(&path).copied() {
            self.diff_scroll = viewport.diff_scroll.min(self.max_diff_scroll());
            self.selected_diff_line = viewport
                .selected_diff_line
                .min(self.source_rendered_diff_len().saturating_sub(1));
            self.ensure_selected_diff_line_visible();
        } else {
            self.diff_scroll = 0;
            self.selected_diff_line = 0;
        }
    }

    /// Return whether the file at index is marked viewed.
    #[must_use]
    pub fn file_viewed(&self, index: usize) -> bool {
        self.review
            .files
            .get(index)
            .is_some_and(|file| self.viewed_files.contains(file.display_path()))
    }

    /// Return unviewed file display paths.
    #[must_use]
    pub fn unviewed_file_paths(&self) -> Vec<String> {
        self.review
            .files
            .iter()
            .filter(|file| !self.viewed_files.contains(file.display_path()))
            .map(|file| file.display_path().to_string())
            .collect()
    }

    /// Return reviewed file progress.
    #[must_use]
    pub fn viewed_file_counts(&self) -> (usize, usize) {
        let total = self.review.files.len();
        let viewed = self
            .review
            .files
            .iter()
            .filter(|file| self.viewed_files.contains(file.display_path()))
            .count();
        (viewed, total)
    }

    /// Toggle viewed state for the selected review file.
    pub fn toggle_selected_file_viewed(&mut self) -> bool {
        let Some(path) = self.selected_file_path() else {
            self.status_message = Some("no review file selected".to_string());
            return true;
        };
        if self.viewed_files.remove(&path) {
            self.workspace.viewed_files.remove(&path);
            self.pending_workspace_save = true;
            self.status_message = Some(format!("marked {path} unviewed"));
        } else {
            self.viewed_files.insert(path.clone());
            self.workspace.viewed_files.insert(path.clone());
            self.pending_workspace_save = true;
            self.status_message = Some(format!("marked {path} viewed"));
        }
        true
    }

    /// Return whether the file at index has draft comments.
    #[must_use]
    pub fn file_has_drafts(&self, index: usize) -> bool {
        self.draft_comment_count_for_file(index) > 0
    }

    /// Select a file by index.
    pub fn select_file(&mut self, index: usize) -> bool {
        if index >= self.review_item_count() || index == self.selected_file {
            return false;
        }
        self.save_current_file_viewport();
        self.selected_file = index;
        self.range_selection_start = None;
        self.mouse_range_selection_start = None;
        self.mouse_range_selection_dragged = false;
        self.selected_view_target = None;
        self.restore_current_file_viewport();
        self.queue_selected_file_load();
        self.expand_selected_file_dirs();
        self.sync_tree_row_to_selected_file();
        true
    }

    /// Return true when a repository file load is pending.
    #[must_use]
    pub const fn has_pending_file_load(&self) -> bool {
        self.pending_file_load.is_some()
    }

    /// Take pending repository file load request.
    pub const fn take_pending_file_load(&mut self) -> Option<String> {
        self.pending_file_load.take()
    }

    /// Store a lazily loaded repository file.
    pub fn store_loaded_file(&mut self, file: CachedReviewFile) {
        self.file_cache.insert(file);
    }

    /// Store a repository file load failure.
    pub fn store_file_load_error(&mut self, path: String, error: String) {
        self.file_cache.insert(CachedReviewFile {
            path,
            content: String::new(),
            line_spans: Vec::new(),
            size_bytes: 0,
            mtime_ms: None,
            is_binary: false,
            unavailable_reason: Some(error),
        });
    }

    /// Sync selected tree row to selected file.
    pub fn sync_tree_row_to_selected_file(&mut self) {
        if let Some(row) = self.file_tree_rows().iter().position(|row| {
            matches!(row, ReviewFileTreeRow::File { index, .. } if *index == self.selected_file)
        }) {
            self.selected_tree_row = row;
        }
    }

    /// Expand ancestor directories for the selected file.
    pub fn expand_selected_file_dirs(&mut self) {
        let Some(path) = self.selected_file_path() else {
            return;
        };
        let path = Path::new(&path);
        let mut prefix = PathBuf::new();
        if let Some(parent) = path.parent() {
            for component in parent.components() {
                prefix.push(component.as_os_str());
                self.expanded_dirs.insert(prefix.clone());
            }
        }
    }

    /// Queue selected repository file for loading when needed.
    pub fn queue_selected_file_load(&mut self) {
        if !self.review.is_repository_review() {
            return;
        }
        let Some(path) = self.selected_file_path() else {
            return;
        };
        if self.file_cache.get(&path).is_none() && self.pending_file_load.as_deref() != Some(&path)
        {
            self.pending_file_load = Some(path);
        }
    }

    /// Select next file.
    pub fn select_next_file(&mut self) -> bool {
        self.select_file((self.selected_file + 1).min(self.review_item_count().saturating_sub(1)))
    }

    /// Scroll file sidebar down.
    pub fn scroll_files_down(&mut self, rows: usize) -> bool {
        if self.sidebar_mode == ReviewSidebarMode::Repository && self.review.is_repository_review()
        {
            return self.scroll_tree_down(rows);
        }
        let max = self.review.files.len().saturating_sub(
            self.last_file_area
                .map_or(1, |area| usize::from(area.height).max(1)),
        );
        let next = self.file_scroll.saturating_add(rows).min(max);
        if next == self.file_scroll {
            return false;
        }
        self.file_scroll = next;
        true
    }

    /// Scroll file sidebar up.
    pub fn scroll_files_up(&mut self, rows: usize) -> bool {
        if self.sidebar_mode == ReviewSidebarMode::Repository && self.review.is_repository_review()
        {
            return self.scroll_tree_up(rows);
        }
        let next = self.file_scroll.saturating_sub(rows);
        if next == self.file_scroll {
            return false;
        }
        self.file_scroll = next;
        true
    }

    /// Scroll repository tree sidebar down.
    pub fn scroll_tree_down(&mut self, rows: usize) -> bool {
        let max = self.file_tree_rows().len().saturating_sub(
            self.last_file_area
                .map_or(1, |area| usize::from(area.height).max(1)),
        );
        let next = self.tree_scroll.saturating_add(rows).min(max);
        if next == self.tree_scroll {
            return false;
        }
        self.tree_scroll = next;
        true
    }

    /// Scroll repository tree sidebar up.
    pub const fn scroll_tree_up(&mut self, rows: usize) -> bool {
        let next = self.tree_scroll.saturating_sub(rows);
        if next == self.tree_scroll {
            return false;
        }
        self.tree_scroll = next;
        true
    }

    /// Mark every review file unviewed.
    pub fn mark_all_files_unviewed(&mut self) -> bool {
        if self.viewed_files.is_empty() && self.workspace.viewed_files.is_empty() {
            self.status_message = Some("all files already unviewed".to_string());
            return true;
        }
        self.viewed_files.clear();
        self.workspace.viewed_files.clear();
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        let (_, total) = self.viewed_file_counts();
        self.status_message = Some(format!("marked 0/{total} files viewed"));
        true
    }

    /// Mark every review file viewed.
    pub fn mark_all_files_viewed(&mut self) -> bool {
        self.viewed_files = self
            .review
            .files
            .iter()
            .map(|file| file.display_path().to_string())
            .collect();
        self.workspace.viewed_files.clone_from(&self.viewed_files);
        self.sync_review_workspace();
        self.pending_workspace_save = true;
        let (viewed, total) = self.viewed_file_counts();
        self.status_message = Some(format!("marked {viewed}/{total} files viewed"));
        true
    }

    /// Select next item needing review attention.
    pub fn select_next_attention_item(&mut self) -> bool {
        if !self.unviewed_file_indices().is_empty() {
            return self.select_next_unviewed_file();
        }
        self.select_next_open_thread_global()
    }

    /// Select next file not yet marked viewed.
    pub fn select_next_unviewed_file(&mut self) -> bool {
        self.select_relative_unviewed_file(1)
    }

    /// Select previous file not yet marked viewed.
    pub fn select_previous_unviewed_file(&mut self) -> bool {
        self.select_relative_unviewed_file(-1)
    }

    fn select_relative_unviewed_file(&mut self, offset: isize) -> bool {
        let files = self.unviewed_file_indices();
        if files.is_empty() {
            self.status_message = Some("all files viewed".to_string());
            return true;
        }
        let current_index = files.iter().position(|index| *index == self.selected_file);
        let next_index = files[relative_index(current_index, files.len(), offset)];
        let _ = self.select_file(next_index);
        self.status_message = Some("selected unviewed file".to_string());
        true
    }

    fn unviewed_file_indices(&self) -> Vec<usize> {
        self.review
            .files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| {
                (!self.viewed_files.contains(file.display_path())).then_some(index)
            })
            .collect()
    }

    /// Select previous file.
    pub fn select_previous_file(&mut self) -> bool {
        self.select_file(self.selected_file.saturating_sub(1))
    }

    /// Move visual review selection down.
    pub fn select_next_view_row(&mut self, rows: usize) -> bool {
        let Some(document) = self.current_review_view_document() else {
            return self.scroll_down(rows);
        };
        let current = self.selected_diff_visual_row();
        let next = current
            .saturating_add(rows)
            .min(document.rows.len().saturating_sub(1));
        if next == current {
            return false;
        }
        self.select_view_visual_row(next)
    }

    /// Move visual review selection up.
    pub fn select_previous_view_row(&mut self, rows: usize) -> bool {
        let Some(_) = self.current_review_view_document() else {
            return self.scroll_up(rows);
        };
        let current = self.selected_diff_visual_row();
        let previous = current.saturating_sub(rows);
        if previous == current {
            return false;
        }
        self.select_view_visual_row(previous)
    }

    /// Scroll diff down.
    pub fn scroll_down(&mut self, rows: usize) -> bool {
        let max = self.max_diff_scroll();
        let next = self.diff_scroll.saturating_add(rows).min(max);
        if next == self.diff_scroll {
            return false;
        }
        self.diff_scroll = next;
        self.keep_selection_inside_visible_diff();
        true
    }

    /// Scroll diff up.
    pub fn scroll_up(&mut self, rows: usize) -> bool {
        let next = self.diff_scroll.saturating_sub(rows);
        if next == self.diff_scroll {
            return false;
        }
        self.diff_scroll = next;
        self.keep_selection_inside_visible_diff();
        true
    }

    fn keep_selection_inside_visible_diff(&mut self) {
        let height = self
            .last_diff_area
            .map_or(1, |area| usize::from(area.height).max(1));
        let selected_visual_row = self.selected_diff_visual_row();
        if selected_visual_row < self.diff_scroll {
            let _ = self.select_view_visual_row(self.diff_scroll);
        } else if selected_visual_row >= self.diff_scroll.saturating_add(height) {
            let bottom = self.diff_scroll.saturating_add(height.saturating_sub(1));
            let _ = self.select_view_visual_row(bottom);
        }
    }

    /// Scroll to top.
    pub fn scroll_to_top(&mut self) -> bool {
        if self.diff_scroll == 0 {
            return false;
        }
        self.selected_view_target = None;
        self.diff_scroll = 0;
        self.ensure_selected_diff_line_visible();
        true
    }

    /// Scroll to bottom.
    pub fn scroll_to_bottom(&mut self) -> bool {
        let max = self.max_diff_scroll();
        if self.diff_scroll == max {
            return false;
        }
        self.selected_view_target = None;
        self.diff_scroll = max;
        self.selected_diff_line = self.source_rendered_diff_len().saturating_sub(1);
        self.ensure_selected_diff_line_visible();
        true
    }

    /// Select next hunk.
    pub fn select_next_hunk(&mut self) -> bool {
        let Some(next) = self
            .hunk_render_rows()
            .into_iter()
            .find(|row| *row > self.selected_diff_line)
        else {
            return false;
        };
        self.selected_diff_line = next;
        self.ensure_selected_diff_line_visible();
        true
    }

    /// Select previous hunk.
    pub fn select_previous_hunk(&mut self) -> bool {
        let Some(previous) = self
            .hunk_render_rows()
            .into_iter()
            .rev()
            .find(|row| *row < self.selected_diff_line)
        else {
            return false;
        };
        self.selected_diff_line = previous;
        self.ensure_selected_diff_line_visible();
        true
    }

    /// Return whether a visual row target is an inline action.
    #[must_use]
    pub fn is_view_visual_row_action(&self, visual_row: usize) -> bool {
        self.current_review_view_document()
            .and_then(|document| document.row_for_visual_row(visual_row).cloned())
            .is_some_and(|row| matches!(row.target, ReviewViewTarget::ThreadAction { .. }))
    }

    /// Return whether a visual row target is an inline thread header.
    #[must_use]
    pub fn is_view_visual_row_thread(&self, visual_row: usize) -> bool {
        self.current_review_view_document()
            .and_then(|document| document.row_for_visual_row(visual_row).cloned())
            .is_some_and(|row| matches!(row.target, ReviewViewTarget::Thread { .. }))
    }

    /// Handle primary click on a review document visual row.
    pub fn handle_review_view_click(&mut self, visual_row: usize) -> bool {
        let Some(document) = self.current_review_view_document() else {
            return self.begin_mouse_range_selection(visual_row);
        };
        let Some(row) = document.row_for_visual_row(visual_row) else {
            return false;
        };
        let selected = self.select_view_visual_row(visual_row);
        match &row.target {
            ReviewViewTarget::SourceLine { source_row, .. }
            | ReviewViewTarget::HunkHeader { source_row, .. } => {
                self.begin_mouse_range_selection(*source_row) || selected
            }
            ReviewViewTarget::Thread { .. } => self.toggle_selected_inline_thread() || selected,
            ReviewViewTarget::ThreadAction { .. } => {
                self.activate_selected_inline_action() || selected
            }
            ReviewViewTarget::Comment { .. } => selected,
        }
    }

    /// Select a visible diff line by rendered row index.
    pub fn select_diff_line(&mut self, index: usize) -> bool {
        let clamped = index.min(self.source_rendered_diff_len().saturating_sub(1));
        self.selected_view_target = None;
        if clamped == self.selected_diff_line {
            return false;
        }
        self.selected_diff_line = clamped;
        self.ensure_selected_diff_line_visible();
        true
    }

    /// Select a semantic row in the current review view document.
    pub fn select_view_visual_row(&mut self, visual_row: usize) -> bool {
        let Some(document) = self.current_review_view_document() else {
            return self.select_diff_line(visual_row);
        };
        let Some(row) = document.row_for_visual_row(visual_row) else {
            return false;
        };
        match &row.target {
            ReviewViewTarget::SourceLine { source_row, .. }
            | ReviewViewTarget::HunkHeader { source_row, .. } => self.select_diff_line(*source_row),
            target => {
                if self.selected_view_target.as_ref() == Some(target) {
                    return false;
                }
                self.selected_view_target = Some(target.clone());
                self.sync_selected_thread_to_view_target(target);
                self.ensure_selected_diff_line_visible();
                true
            }
        }
    }

    /// Return the selected semantic target in the current review view.
    #[must_use]
    pub fn selected_review_view_target(&self) -> Option<ReviewViewTarget> {
        self.selected_view_target.clone().or_else(|| {
            self.current_review_view_document().and_then(|document| {
                document
                    .visual_row_for_source_row(self.selected_diff_line)
                    .and_then(|visual_row| document.target_for_visual_row(visual_row).cloned())
            })
        })
    }

    /// Return whether the provided view target is selected.
    #[must_use]
    pub fn is_view_target_selected(&self, target: &ReviewViewTarget) -> bool {
        self.selected_review_view_target().as_ref() == Some(target)
    }

    /// Start a mouse-driven range selection from a rendered diff row.
    pub fn begin_mouse_range_selection(&mut self, index: usize) -> bool {
        self.mouse_range_selection_start =
            Some(index.min(self.source_rendered_diff_len().saturating_sub(1)));
        self.mouse_range_selection_dragged = false;
        self.range_selection_start = None;
        self.select_diff_line(index)
    }

    /// Extend a mouse-driven range selection to a rendered diff row.
    pub fn update_mouse_range_selection(&mut self, index: usize) -> bool {
        let Some(start) = self.mouse_range_selection_start else {
            return false;
        };
        self.mouse_range_selection_dragged = true;
        self.range_selection_start = Some(start);
        let changed = self.select_diff_line(index);
        if changed {
            self.status_message = self.range_selection_label();
        }
        true
    }

    /// Complete a mouse-driven range selection.
    pub fn finish_mouse_range_selection(&mut self) -> bool {
        let Some(_) = self.mouse_range_selection_start.take() else {
            return false;
        };
        if self.mouse_range_selection_dragged {
            self.mouse_range_selection_dragged = false;
            self.status_message = self.range_selection_label();
            true
        } else {
            self.mouse_range_selection_dragged = false;
            self.range_selection_start = None;
            false
        }
    }

    /// Return whether file sidebar contains terminal coordinates.
    #[must_use]
    pub fn file_area_contains(&self, x: u16, y: u16) -> bool {
        self.last_file_area
            .is_some_and(|area| x >= area.x && x < area.right() && y >= area.y && y < area.bottom())
    }

    /// Return visible file tree row under terminal coordinates.
    #[must_use]
    pub fn file_tree_row_at(&self, x: u16, y: u16) -> Option<ReviewFileTreeRow> {
        let area = self.last_file_area?;
        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
            return None;
        }
        let index = self.tree_scroll + usize::from(y.saturating_sub(area.y));
        self.file_tree_rows().get(index).cloned()
    }

    /// Return visible file index under terminal coordinates.
    #[must_use]
    pub fn file_index_at(&self, x: u16, y: u16) -> Option<usize> {
        let area = self.last_file_area?;
        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
            return None;
        }
        let index = self.file_scroll + usize::from(y.saturating_sub(area.y));
        (index < self.review.files.len()).then_some(index)
    }

    /// Return visible thread index under terminal coordinates.
    #[must_use]
    pub fn thread_index_at(&self, x: u16, y: u16) -> Option<usize> {
        let area = self.last_file_area?;
        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
            return None;
        }
        let index = self.thread_scroll + usize::from(y.saturating_sub(area.y));
        (index < self.visible_thread_summaries().len()).then_some(index)
    }

    /// Return visible diff row index under terminal coordinates.
    #[must_use]
    pub fn diff_line_index_at(&self, x: u16, y: u16) -> Option<usize> {
        let area = self.last_diff_area?;
        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
            return None;
        }
        let visual_row = self.diff_scroll + usize::from(y.saturating_sub(area.y));
        if let Some(document) = self.current_review_view_document() {
            return document
                .row_for_visual_row(visual_row)
                .and_then(|row| row.source_row);
        }
        Some(visual_row)
    }

    /// Return visible semantic row index under terminal coordinates.
    #[must_use]
    pub fn view_visual_row_at(&self, x: u16, y: u16) -> Option<usize> {
        let area = self.last_diff_area?;
        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
            return None;
        }
        Some(self.diff_scroll + usize::from(y.saturating_sub(area.y)))
    }

    /// Return total draft comment count.
    #[must_use]
    pub fn draft_comment_count(&self) -> usize {
        self.draft_comments.values().map(Vec::len).sum()
    }

    /// Return draft comment count for a file.
    #[must_use]
    pub fn draft_comment_count_for_file(&self, file_index: usize) -> usize {
        self.draft_comments
            .iter()
            .filter(|(anchor, _)| anchor.file_index == file_index)
            .map(|(_, comments)| comments.len())
            .sum()
    }

    /// Return unresolved thread count for a file.
    #[must_use]
    pub fn open_thread_count_for_file(&self, file_index: usize) -> usize {
        self.draft_comments
            .keys()
            .filter(|anchor| {
                anchor.file_index == file_index
                    && !self
                        .resolved_review_threads
                        .contains(&Self::thread_key_for_anchor(anchor))
            })
            .count()
    }

    /// Clear active range selection.
    pub fn clear_range_selection(&mut self) -> bool {
        if self.range_selection_start.is_none() {
            return false;
        }
        self.range_selection_start = None;
        self.status_message = Some("cleared range selection".to_string());
        true
    }

    /// Toggle range selection from the selected diff line.
    pub fn toggle_range_selection(&mut self) -> bool {
        if self.range_selection_start.is_some() {
            self.range_selection_start = None;
            self.status_message = Some("cleared range selection".to_string());
            return true;
        }
        if self.selected_comment_anchor().is_none() {
            self.status_message = Some("select a diff line to start range selection".to_string());
            return true;
        }
        self.range_selection_start = Some(self.selected_diff_line);
        self.status_message =
            Some("range selection started; move then c comment or a ask Bcode".to_string());
        true
    }

    /// Return selected range bounds, if active.
    #[must_use]
    pub fn selected_range_bounds(&self) -> Option<(usize, usize)> {
        let start = self.range_selection_start?;
        Some(if start <= self.selected_diff_line {
            (start, self.selected_diff_line)
        } else {
            (self.selected_diff_line, start)
        })
    }

    /// Return true when a rendered row is within the active range selection.
    #[must_use]
    pub fn is_row_in_range_selection(&self, file_index: usize, diff_row: usize) -> bool {
        if file_index != self.selected_file {
            return false;
        }
        let Some((start, end)) = self.selected_range_bounds() else {
            return false;
        };
        (start..=end).contains(&diff_row)
    }

    /// Return a status label for an active range selection.
    #[must_use]
    pub fn range_selection_label(&self) -> Option<String> {
        let (start, end) = self.selected_range_bounds()?;
        Some(format!("range {start}-{end} selected"))
    }

    /// Return true when a diff row has draft comments.
    #[must_use]
    pub fn has_draft_comment_at(&self, file_index: usize, diff_row: usize) -> bool {
        self.draft_comments.keys().any(|anchor| {
            anchor.file_index == file_index
                && (anchor.start_diff_row()..=anchor.end_diff_row()).contains(&diff_row)
        })
    }

    /// Return the draft marker for a rendered diff row.
    #[must_use]
    pub fn draft_marker_at(&self, file_index: usize, diff_row: usize) -> Option<String> {
        let mut count = 0usize;
        let mut linked = false;
        for (anchor, comments) in &self.draft_comments {
            if anchor.file_index != file_index
                || !(anchor.start_diff_row()..=anchor.end_diff_row()).contains(&diff_row)
            {
                continue;
            }
            count = count.saturating_add(comments.len());
            linked |= comments.iter().any(|comment| comment.session_id.is_some());
        }
        if count == 0 {
            None
        } else if linked {
            Some(if count > 1 {
                format!("🤖💬{count}")
            } else {
                "🤖💬".to_string()
            })
        } else {
            Some(if count > 1 {
                format!("💬{count}")
            } else {
                "💬".to_string()
            })
        }
    }

    /// Open the draft comment editor for the selected diff line.
    pub fn open_comment_editor(&mut self) -> bool {
        let Some(anchor) = self.selected_comment_anchor() else {
            self.status_message =
                Some("select an added, removed, or context line to comment".to_string());
            return true;
        };
        self.comment_editor = Some(ReviewCommentEditor::new(anchor));
        self.sync_selected_thread_to_anchor();
        self.status_message =
            Some("editing draft comment; enter/ctrl+s saves, esc cancels".to_string());
        true
    }

    /// Open the latest persisted draft for editing.
    pub fn open_latest_draft_editor(&mut self) -> bool {
        let Some((anchor, selected_index)) = self
            .selected_draft_anchor_and_comment_index()
            .or_else(|| self.selected_comment_anchor().map(|anchor| (anchor, None)))
        else {
            self.status_message = Some("select a commented line to edit a draft".to_string());
            return true;
        };
        let Some(comments) = self.draft_comments.get(&anchor) else {
            self.status_message = Some("no draft comment at selected line".to_string());
            return true;
        };
        let index = selected_index.unwrap_or_else(|| comments.len().saturating_sub(1));
        let Some(comment) = comments.get(index) else {
            self.status_message = Some("no draft comment at selected line".to_string());
            return true;
        };
        let Some(comment_id) = comment.id.clone() else {
            self.status_message =
                Some("draft is not persisted yet; try again after save".to_string());
            return true;
        };
        self.comment_editor = Some(ReviewCommentEditor::edit(
            anchor,
            comment_id,
            comment.body.clone(),
        ));
        self.sync_selected_thread_to_anchor();
        self.status_message =
            Some("editing draft comment; enter/ctrl+s saves, esc cancels".to_string());
        true
    }

    /// Save the active draft comment editor.
    pub fn save_comment_editor(&mut self) -> bool {
        let Some(editor) = self.comment_editor.take() else {
            return false;
        };
        let text = editor.buffer.text().trim().to_string();
        if text.is_empty() {
            self.status_message = Some("empty comment discarded".to_string());
            return true;
        }
        let anchor = editor.anchor;
        match editor.mode {
            ReviewCommentEditorMode::Create => {
                self.draft_comments
                    .entry(anchor.clone())
                    .or_default()
                    .push(ReviewDraftComment {
                        id: None,
                        body: text.clone(),
                        persisted: false,
                        created_at_ms: None,
                        updated_at_ms: None,
                        session_id: None,
                    });
                self.pending_draft_save = Some(PendingDraftSave { anchor, body: text });
                self.sync_selected_thread_to_anchor();
                let count = self.draft_comment_count();
                self.status_message = Some(format!("saved draft comment ({count} total)"));
            }
            ReviewCommentEditorMode::Edit {
                comment_id,
                previous_body,
            } => {
                self.update_local_draft_body(&anchor, &comment_id, text.clone());
                self.pending_draft_update = Some(PendingDraftUpdate {
                    target: review_scope_for_workspace(&self.workspace).target().clone(),
                    scope: Some(review_scope_for_workspace(&self.workspace)),
                    anchor,
                    comment_id,
                    previous_body,
                    new_body: text,
                });
                self.status_message = Some("updated draft comment".to_string());
            }
        }
        true
    }

    /// Take the pending draft save request, if present.
    pub const fn take_pending_draft_save(&mut self) -> Option<PendingDraftSave> {
        self.pending_draft_save.take()
    }

    /// Take the pending draft delete request, if present.
    pub const fn take_pending_draft_delete(&mut self) -> Option<PendingDraftDelete> {
        self.pending_draft_delete.take()
    }

    /// Take the pending draft update request, if present.
    pub const fn take_pending_draft_update(&mut self) -> Option<PendingDraftUpdate> {
        self.pending_draft_update.take()
    }

    /// Take the pending thread resolution request, if present.
    pub const fn take_pending_thread_resolve(&mut self) -> Option<PendingThreadResolve> {
        self.pending_thread_resolve.take()
    }

    /// Take the pending Bcode agent session request, if present.
    pub const fn take_pending_agent_session(&mut self) -> Option<PendingAgentSession> {
        self.pending_agent_session.take()
    }

    /// Return linked session id for an anchor.
    #[must_use]
    pub fn session_id_for_anchor(&self, anchor: &ReviewCommentAnchor) -> Option<&str> {
        self.draft_comments
            .get(anchor)?
            .last()?
            .session_id
            .as_deref()
    }

    /// Mark the latest draft at an anchor as linked to a Bcode session.
    pub fn mark_thread_session(&mut self, anchor: &ReviewCommentAnchor, session_id: String) {
        if let Some(comment) = self
            .draft_comments
            .get_mut(anchor)
            .and_then(|comments| comments.last_mut())
        {
            comment.session_id = Some(session_id);
        } else {
            self.draft_comments
                .entry(anchor.clone())
                .or_default()
                .push(ReviewDraftComment {
                    id: None,
                    body: String::new(),
                    persisted: false,
                    created_at_ms: None,
                    updated_at_ms: None,
                    session_id: Some(session_id),
                });
        }
    }

    /// Restore a locally updated draft after persistence failure.
    pub fn restore_updated_draft(&mut self, update: PendingDraftUpdate) {
        self.update_local_draft_body(&update.anchor, &update.comment_id, update.previous_body);
        self.sync_selected_thread_to_anchor();
    }

    /// Restore a locally deleted draft after persistence failure.
    pub fn restore_deleted_draft(&mut self, delete: PendingDraftDelete) {
        self.draft_comments
            .entry(delete.anchor)
            .or_default()
            .push(delete.comment);
        self.sync_selected_thread_to_anchor();
    }

    fn update_local_draft_body(
        &mut self,
        anchor: &ReviewCommentAnchor,
        comment_id: &str,
        body: String,
    ) {
        let Some(comment) = self.draft_comments.get_mut(anchor).and_then(|comments| {
            comments
                .iter_mut()
                .rev()
                .find(|comment| comment.id.as_deref() == Some(comment_id))
        }) else {
            return;
        };
        comment.body = body;
        comment.persisted = false;
    }

    fn sync_selected_thread_to_anchor(&mut self) {
        let Some(anchor) = self.selected_comment_anchor() else {
            return;
        };
        self.sync_selected_thread_to_specific_anchor(&anchor);
    }

    fn sync_selected_thread_to_view_target(&mut self, target: &ReviewViewTarget) {
        let Some(anchor) = self.anchor_for_view_target(target) else {
            return;
        };
        self.selected_file = anchor.file_index;
        self.selected_diff_line = anchor.diff_row;
        self.sync_selected_thread_to_specific_anchor(&anchor);
    }

    fn sync_selected_thread_to_specific_anchor(&mut self, anchor: &ReviewCommentAnchor) {
        if let Some(index) = self
            .thread_summaries()
            .iter()
            .position(|thread| thread.anchor == *anchor)
        {
            self.selected_thread = index;
        }
    }

    fn anchor_for_view_target(&self, target: &ReviewViewTarget) -> Option<ReviewCommentAnchor> {
        match target {
            ReviewViewTarget::Thread { thread_key }
            | ReviewViewTarget::Comment { thread_key, .. }
            | ReviewViewTarget::ThreadAction { thread_key, .. } => {
                self.draft_anchor_for_thread_key(thread_key)
            }
            ReviewViewTarget::HunkHeader { .. } | ReviewViewTarget::SourceLine { .. } => None,
        }
    }

    /// Return a concise review readiness label.
    #[must_use]
    pub fn review_readiness_label(&self) -> String {
        let (viewed, total) = self.viewed_file_counts();
        let unviewed = total.saturating_sub(viewed);
        let (open_threads, _) = self.thread_status_counts();
        if unviewed == 0 && open_threads == 0 {
            return "ready".to_string();
        }
        let mut issues = Vec::new();
        if unviewed > 0 {
            issues.push(format!("{unviewed} unviewed"));
        }
        if open_threads > 0 {
            issues.push(format!("{open_threads} open"));
        }
        format!("incomplete: {}", issues.join(", "))
    }

    /// Return warning message for publishing before review is complete.
    #[must_use]
    pub fn publish_readiness_warning(&self) -> Option<String> {
        let (viewed, total) = self.viewed_file_counts();
        let (open_threads, _) = self.thread_status_counts();
        if viewed == total && open_threads == 0 {
            return None;
        }
        let mut warnings = Vec::new();
        if viewed < total {
            warnings.push(format!("{viewed}/{total} files viewed"));
        }
        if open_threads > 0 {
            warnings.push(format!("{open_threads} unresolved thread(s)"));
        }
        Some(format!(
            "review incomplete: {}; press x again to publish anyway",
            warnings.join(", ")
        ))
    }

    /// Queue generic review publish.
    pub fn publish_review(&mut self) -> bool {
        if !self.publish_readiness_ack
            && let Some(warning) = self.publish_readiness_warning()
        {
            self.publish_readiness_ack = true;
            self.status_message = Some(warning);
            return true;
        }
        self.publish_state = Some(ReviewPublishState::Checklist);
        self.status_message = None;
        true
    }

    /// Show loaded publishers.
    pub fn show_publishers(&mut self, publishers: Vec<ReviewPublisherManifest>) {
        self.publishers = publishers;
        self.selected_publisher = self
            .publishers
            .iter()
            .position(|publisher| publisher.id == DEFAULT_PUBLISHER_ID)
            .unwrap_or(0);
        self.publish_state = Some(ReviewPublishState::Picker);
        self.status_message = None;
    }

    /// Return publisher for id.
    #[must_use]
    pub fn publisher_for_id(&self, publisher_id: &str) -> Option<&ReviewPublisherManifest> {
        self.publishers
            .iter()
            .find(|publisher| publisher.id == publisher_id)
    }

    /// Show publisher preview.
    pub fn show_publish_preview(&mut self, publisher_id: String, preview: String) {
        self.publish_state = Some(ReviewPublishState::Preview {
            publisher_id,
            options: self.current_publish_options(),
            preview,
            scroll: 0,
        });
        self.status_message = None;
    }

    /// Finish publish flow.
    pub fn finish_publish(&mut self, response: PublishReviewResponse) {
        self.publish_state = None;
        self.publish_readiness_ack = false;
        self.status_message = Some(response.message);
    }

    /// Move publish UI selection down.
    pub fn publish_down(&mut self, rows: usize) -> bool {
        match &mut self.publish_state {
            Some(ReviewPublishState::Picker) => {
                let max = self.publishers.len().saturating_sub(1);
                let next = self.selected_publisher.saturating_add(rows).min(max);
                if next == self.selected_publisher {
                    return false;
                }
                self.selected_publisher = next;
                true
            }
            Some(ReviewPublishState::Options {
                selected, options, ..
            }) => {
                let max = options.len().saturating_sub(1);
                let next = selected.saturating_add(rows).min(max);
                if next == *selected {
                    return false;
                }
                *selected = next;
                true
            }
            Some(
                ReviewPublishState::Preview {
                    scroll, preview, ..
                }
                | ReviewPublishState::ConfirmSubmit {
                    scroll, preview, ..
                },
            ) => {
                let max = preview.lines().count().saturating_sub(1);
                let next = scroll.saturating_add(rows).min(max);
                if next == *scroll {
                    return false;
                }
                *scroll = next;
                true
            }
            Some(ReviewPublishState::Checklist) | None => false,
        }
    }

    /// Move publish UI selection up.
    pub const fn publish_up(&mut self, rows: usize) -> bool {
        match &mut self.publish_state {
            Some(ReviewPublishState::Picker) => {
                let next = self.selected_publisher.saturating_sub(rows);
                if next == self.selected_publisher {
                    return false;
                }
                self.selected_publisher = next;
                true
            }
            Some(ReviewPublishState::Options { selected, .. }) => {
                let next = selected.saturating_sub(rows);
                if next == *selected {
                    return false;
                }
                *selected = next;
                true
            }
            Some(
                ReviewPublishState::Preview { scroll, .. }
                | ReviewPublishState::ConfirmSubmit { scroll, .. },
            ) => {
                let next = scroll.saturating_sub(rows);
                if next == *scroll {
                    return false;
                }
                *scroll = next;
                true
            }
            Some(ReviewPublishState::Checklist) | None => false,
        }
    }

    /// Return true when publisher options are active.
    #[must_use]
    pub const fn publish_options_active(&self) -> bool {
        matches!(self.publish_state, Some(ReviewPublishState::Options { .. }))
    }

    /// Return selected publisher option index.
    #[must_use]
    pub const fn selected_publish_option_index(&self) -> usize {
        match &self.publish_state {
            Some(ReviewPublishState::Options { selected, .. }) => *selected,
            _ => 0,
        }
    }

    /// Insert text into the selected publisher option.
    pub fn insert_publish_option_text(&mut self, text: &str) -> bool {
        let Some(ReviewPublishState::Options {
            options, selected, ..
        }) = &mut self.publish_state
        else {
            return false;
        };
        let Some(option) = options.get_mut(*selected) else {
            return false;
        };
        option.value.push_str(text);
        true
    }

    /// Cycle the selected publish option when it has enumerated choices.
    pub fn cycle_selected_publish_option(&mut self, offset: isize) -> bool {
        let Some(ReviewPublishState::Options {
            options, selected, ..
        }) = &mut self.publish_state
        else {
            return false;
        };
        let Some(option) = options.get_mut(*selected) else {
            return false;
        };
        if option.choices.is_empty() {
            self.status_message = Some("selected publish option has no choices".to_string());
            return true;
        }
        let current = option
            .choices
            .iter()
            .position(|choice| choice == &option.value)
            .unwrap_or(0);
        let len = option.choices.len();
        let next = if offset.is_negative() {
            current
                .saturating_add(len)
                .saturating_sub(offset.unsigned_abs() % len)
                % len
        } else {
            current.saturating_add(offset.unsigned_abs()) % len
        };
        option.value.clone_from(&option.choices[next]);
        self.status_message = Some(format!("{} = {}", option.name, option.value));
        true
    }

    /// Edit selected publisher option.
    pub fn edit_publish_option(&mut self, stroke: KeyStroke) -> bool {
        let Some(ReviewPublishState::Options {
            options, selected, ..
        }) = &mut self.publish_state
        else {
            return false;
        };
        let Some(option) = options.get_mut(*selected) else {
            return false;
        };
        match stroke.key {
            KeyCode::Char(ch) if stroke.modifiers.is_empty() && option.choices.is_empty() => {
                option.value.push(ch);
                true
            }
            KeyCode::Backspace if stroke.modifiers.is_empty() && option.choices.is_empty() => {
                option.value.pop();
                true
            }
            _ => false,
        }
    }

    /// Return publish readiness checklist lines.
    #[must_use]
    pub fn publish_checklist_lines(&self) -> Vec<String> {
        let (viewed, total) = self.viewed_file_counts();
        let unviewed = total.saturating_sub(viewed);
        let (open_threads, resolved_threads) = self.thread_status_counts();
        let drafts = self.draft_comment_count();
        let readiness = if unviewed == 0 && open_threads == 0 {
            "✓ ready to publish".to_string()
        } else {
            "! review has remaining attention items".to_string()
        };
        let mut lines = vec![
            readiness,
            format!("files viewed: {viewed}/{total}"),
            format!("unviewed files: {unviewed}  (W jump)"),
        ];
        lines.extend(
            self.unviewed_file_paths()
                .into_iter()
                .take(3)
                .map(|path| format!("  • {path}")),
        );
        lines.push(format!("open threads: {open_threads}  (P jump)"));
        lines.extend(
            self.open_thread_summaries()
                .into_iter()
                .take(3)
                .map(|thread| format!("  • {} {}", thread.anchor.path, thread.line_label())),
        );
        lines.extend([
            format!("resolved threads: {resolved_threads}"),
            format!("draft comments: {drafts}"),
            "press ! for attention sidebar".to_string(),
        ]);
        lines
    }

    fn current_publish_options(&self) -> Vec<ReviewPublishOption> {
        match &self.publish_state {
            Some(
                ReviewPublishState::Options { options, .. }
                | ReviewPublishState::Preview { options, .. }
                | ReviewPublishState::ConfirmSubmit { options, .. },
            ) => options.clone(),
            Some(ReviewPublishState::Checklist | ReviewPublishState::Picker) | None => Vec::new(),
        }
    }

    /// Return from submit confirmation to preview.
    pub fn back_to_publish_preview(&mut self) -> bool {
        let Some(ReviewPublishState::ConfirmSubmit {
            publisher_id,
            options,
            preview,
            scroll,
        }) = self.publish_state.take()
        else {
            return false;
        };
        self.publish_state = Some(ReviewPublishState::Preview {
            publisher_id,
            options,
            preview,
            scroll,
        });
        self.status_message = Some("returned to review publish preview".to_string());
        true
    }

    /// Confirm current publish UI selection.
    pub fn confirm_publish_selection(&mut self) -> bool {
        match &self.publish_state {
            Some(ReviewPublishState::Checklist) => {
                self.pending_publish_request = Some(PendingPublishRequest::ListPublishers);
                self.publish_readiness_ack = false;
                self.status_message = Some("loading review publishers".to_string());
                true
            }
            Some(ReviewPublishState::Picker) => {
                let Some(publisher) = self.publishers.get(self.selected_publisher) else {
                    self.status_message = Some("no publisher selected".to_string());
                    return true;
                };
                let options = options_from_schema(&publisher.options_schema);
                if options.is_empty() {
                    self.pending_publish_request = Some(PendingPublishRequest::Preview {
                        publisher_id: publisher.id.clone(),
                        options,
                    });
                    self.status_message = Some(format!("previewing publisher {}", publisher.label));
                } else {
                    self.publish_state = Some(ReviewPublishState::Options {
                        publisher_id: publisher.id.clone(),
                        options,
                        selected: 0,
                    });
                    self.status_message = None;
                }
                true
            }
            Some(ReviewPublishState::Options {
                publisher_id,
                options,
                ..
            }) => {
                self.pending_publish_request = Some(PendingPublishRequest::Preview {
                    publisher_id: publisher_id.clone(),
                    options: options.clone(),
                });
                self.status_message = Some(format!("previewing publisher {publisher_id}"));
                true
            }
            Some(ReviewPublishState::Preview { .. }) => {
                let Some(ReviewPublishState::Preview {
                    publisher_id,
                    options,
                    preview,
                    scroll,
                }) = self.publish_state.take()
                else {
                    return false;
                };
                self.status_message = Some(format!("confirm submit via {publisher_id}"));
                self.publish_state = Some(ReviewPublishState::ConfirmSubmit {
                    publisher_id,
                    options,
                    preview,
                    scroll,
                });
                true
            }
            Some(ReviewPublishState::ConfirmSubmit {
                publisher_id,
                options,
                ..
            }) => {
                self.pending_publish_request = Some(PendingPublishRequest::Submit {
                    publisher_id: publisher_id.clone(),
                    options: options.clone(),
                });
                self.status_message = Some(format!("submitting review via {publisher_id}"));
                true
            }
            None => false,
        }
    }

    /// Take pending review publish request.
    pub const fn take_publish_request(&mut self) -> Option<PendingPublishRequest> {
        self.pending_publish_request.take()
    }

    /// Take pending linked session open request.
    pub const fn take_session_to_open(&mut self) -> Option<SessionId> {
        self.session_to_open.take()
    }

    /// Open linked session for the selected thread.
    pub fn open_linked_session_at_selection(&mut self) -> bool {
        let Some(anchor) = self.selected_comment_anchor() else {
            self.status_message = Some("select a linked review thread to open".to_string());
            return true;
        };
        let Some(session_id) = self.session_id_for_anchor(&anchor) else {
            self.status_message = Some("no linked session for selected thread".to_string());
            return true;
        };
        match session_id.parse::<SessionId>() {
            Ok(session_id) => {
                self.session_to_open = Some(session_id);
                self.should_exit = true;
            }
            Err(_) => {
                self.status_message = Some("linked session id is invalid".to_string());
            }
        }
        true
    }

    /// Ask Bcode about the selected review line/thread.
    pub fn ask_bcode_about_selection(&mut self) -> bool {
        let Some(anchor) = self.selected_comment_anchor() else {
            self.status_message = Some("select a diff line to ask Bcode".to_string());
            return true;
        };
        let existing_session = self.session_id_for_anchor(&anchor).map(ToString::to_string);
        let draft_body = self
            .draft_comments
            .get(&anchor)
            .and_then(|comments| comments.last())
            .map(|comment| comment.body.clone());
        self.pending_agent_session = Some(PendingAgentSession { anchor, draft_body });
        self.status_message = Some(existing_session.map_or_else(
            || "creating Bcode session for review thread".to_string(),
            |session_id| format!("sending review follow-up to linked session {session_id}"),
        ));
        true
    }

    /// Expand all inline review threads.
    pub fn expand_all_inline_threads(&mut self) -> bool {
        if self.collapsed_review_threads.is_empty() {
            return false;
        }
        self.collapsed_review_threads.clear();
        self.status_message = Some("expanded all review threads".to_string());
        self.ensure_selected_diff_line_visible();
        true
    }

    /// Collapse all inline review threads in the current view.
    pub fn collapse_all_inline_threads(&mut self) -> bool {
        let keys = self
            .inline_thread_summaries()
            .into_iter()
            .map(|thread| Self::thread_key_for_anchor(&thread.anchor))
            .collect::<BTreeSet<_>>();
        if keys.is_empty() || keys == self.collapsed_review_threads {
            return false;
        }
        self.collapsed_review_threads = keys;
        self.status_message = Some("collapsed all review threads".to_string());
        self.ensure_selected_diff_line_visible();
        true
    }

    /// Toggle selected inline thread collapsed state.
    pub fn toggle_selected_inline_thread(&mut self) -> bool {
        let Some(ReviewViewTarget::Thread { thread_key }) = self.selected_view_target.clone()
        else {
            return false;
        };
        if self.collapsed_review_threads.remove(&thread_key) {
            self.status_message = Some("expanded review thread".to_string());
        } else {
            self.collapsed_review_threads.insert(thread_key);
            self.status_message = Some("collapsed review thread".to_string());
        }
        self.ensure_selected_diff_line_visible();
        true
    }

    /// Activate the currently selected inline action, if any.
    pub fn activate_selected_inline_action(&mut self) -> bool {
        let Some(ReviewViewTarget::ThreadAction { action, .. }) = self.selected_view_target.clone()
        else {
            return false;
        };
        match ReviewThreadAction::from_id(&action) {
            Some(ReviewThreadAction::Reply) => self.open_comment_editor(),
            Some(ReviewThreadAction::Edit) => self.open_latest_draft_editor(),
            Some(ReviewThreadAction::Delete) => self.delete_latest_draft_at_selection(),
            Some(ReviewThreadAction::AskBcode) => self.ask_bcode_about_selection(),
            Some(ReviewThreadAction::Publish) => self.publish_review(),
            Some(ReviewThreadAction::Resolve) => self.resolve_selected_thread(),
            Some(ReviewThreadAction::Reopen) => self.reopen_selected_thread(),
            None => false,
        }
    }

    /// Activate selected review target, if it has a primary action.
    pub fn activate_selected_review_target(&mut self) -> bool {
        self.activate_selected_inline_action() || self.toggle_selected_inline_thread()
    }

    /// Toggle resolved inline review thread visibility.
    pub fn toggle_show_resolved_threads(&mut self) -> bool {
        self.show_resolved_threads = !self.show_resolved_threads;
        if self.show_resolved_threads {
            self.status_message = Some("showing resolved review threads".to_string());
        } else {
            self.status_message = Some("hiding resolved review threads".to_string());
            if self.selected_view_target.as_ref().is_some_and(|target| {
                self.anchor_for_view_target(target).is_some_and(|anchor| {
                    self.resolved_review_threads
                        .contains(&Self::thread_key_for_anchor(&anchor))
                })
            }) {
                self.selected_view_target = None;
            }
        }
        self.ensure_selected_diff_line_visible();
        true
    }

    /// Toggle selected thread resolved state locally.
    pub fn toggle_selected_thread_resolved(&mut self) -> bool {
        let Some(thread_key) = self.selected_thread_key() else {
            self.status_message = Some("select a review thread to resolve".to_string());
            return true;
        };
        if self.resolved_review_threads.contains(&thread_key) {
            self.reopen_thread_key(&thread_key)
        } else {
            self.resolve_thread_key(&thread_key)
        }
    }

    /// Resolve selected thread locally.
    pub fn resolve_selected_thread(&mut self) -> bool {
        let Some(thread_key) = self.selected_thread_key() else {
            self.status_message = Some("select a review thread to resolve".to_string());
            return true;
        };
        self.resolve_thread_key(&thread_key)
    }

    /// Reopen selected thread locally.
    pub fn reopen_selected_thread(&mut self) -> bool {
        let Some(thread_key) = self.selected_thread_key() else {
            self.status_message = Some("select a review thread to reopen".to_string());
            return true;
        };
        self.reopen_thread_key(&thread_key)
    }

    fn resolve_thread_key(&mut self, thread_key: &str) -> bool {
        let Some(anchor) = self.anchor_for_thread_key(thread_key) else {
            self.status_message = Some("select a review thread to resolve".to_string());
            return true;
        };
        self.resolved_review_threads.insert(thread_key.to_string());
        self.pending_thread_resolve = Some(PendingThreadResolve {
            target: review_scope_for_workspace(&self.workspace).target().clone(),
            scope: Some(review_scope_for_workspace(&self.workspace)),
            anchor,
            resolved: true,
        });
        self.status_message = Some("resolved review thread".to_string());
        true
    }

    fn reopen_thread_key(&mut self, thread_key: &str) -> bool {
        let Some(anchor) = self.anchor_for_thread_key(thread_key) else {
            self.status_message = Some("select a review thread to reopen".to_string());
            return true;
        };
        self.resolved_review_threads.remove(thread_key);
        self.pending_thread_resolve = Some(PendingThreadResolve {
            target: review_scope_for_workspace(&self.workspace).target().clone(),
            scope: Some(review_scope_for_workspace(&self.workspace)),
            anchor,
            resolved: false,
        });
        self.status_message = Some("reopened review thread".to_string());
        true
    }

    fn anchor_for_thread_key(&self, thread_key: &str) -> Option<ReviewCommentAnchor> {
        self.draft_comments
            .keys()
            .find(|anchor| Self::thread_key_for_anchor(anchor) == thread_key)
            .cloned()
    }

    fn selected_thread_key(&self) -> Option<String> {
        match self.selected_view_target.as_ref()? {
            ReviewViewTarget::Thread { thread_key }
            | ReviewViewTarget::Comment { thread_key, .. }
            | ReviewViewTarget::ThreadAction { thread_key, .. } => Some(thread_key.clone()),
            ReviewViewTarget::HunkHeader { .. } | ReviewViewTarget::SourceLine { .. } => self
                .selected_comment_anchor()
                .map(|anchor| Self::thread_key_for_anchor(&anchor)),
        }
    }

    /// Return a prompt for a pending Bcode agent session.
    #[must_use]
    pub fn agent_session_prompt(&self, ask: &PendingAgentSession) -> String {
        let hunk = self.hunk_context_for_anchor(&ask.anchor);
        let selected_lines = self.selected_lines_for_anchor(&ask.anchor);
        let other_comment_count = self.draft_comment_count().saturating_sub(usize::from(
            self.draft_comments
                .get(&ask.anchor)
                .is_some_and(|comments| !comments.is_empty()),
        ));
        format!(
            "You are helping with a local code review in Bcode.\n\nReview: {}\nRepository: {}\nFile: {}\nDiff rows: {}-{}\nOld range: {}-{}\nNew range: {}-{}\nLine kind: {:?}\nOther draft comment threads in this review: {}\n\nCurrent draft/comment:\n{}\n\nSelected diff lines:\n```diff\n{}\n```\n\nNearby diff hunk/context:\n```diff\n{}\n```\n\nReview context is also available through the bundled code-review plugin service. The relevant interface is `bcode.code_review/v1`; useful operations are `review.context.get`, `review.comments.list`, `review.thread.get`, and `review.diff.get`. Request payloads include `repo_path` plus the review `target`; `review.thread.get` accepts `thread_id` or `anchor`, and `review.diff.get` accepts optional `file_path`.\n\nPlease analyze this review thread. Keep the anchored file and line context in mind. If broader context is needed, inspect the repository from the session working directory.",
            self.review.title,
            self.review.repo_root.display(),
            ask.anchor.path,
            ask.anchor.start_diff_row(),
            ask.anchor.end_diff_row(),
            ask.anchor
                .old_start
                .map_or_else(|| "none".to_string(), |line| line.to_string()),
            ask.anchor
                .old_end
                .map_or_else(|| "none".to_string(), |line| line.to_string()),
            ask.anchor
                .new_start
                .map_or_else(|| "none".to_string(), |line| line.to_string()),
            ask.anchor
                .new_end
                .map_or_else(|| "none".to_string(), |line| line.to_string()),
            ask.anchor.line_kind,
            other_comment_count,
            ask.draft_body.as_deref().unwrap_or("(no draft body yet)"),
            selected_lines,
            hunk,
        )
    }

    fn selected_lines_for_anchor(&self, anchor: &ReviewCommentAnchor) -> String {
        let Some(file) = self.review.files.get(anchor.file_index) else {
            return String::new();
        };
        let rows = rendered_rows_for_prompt(file);
        let start = anchor.start_diff_row();
        let end = anchor.end_diff_row();
        rows.into_iter()
            .enumerate()
            .filter_map(|(index, row)| (start..=end).contains(&index).then_some(row))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn hunk_context_for_anchor(&self, anchor: &ReviewCommentAnchor) -> String {
        let Some(file) = self.review.files.get(anchor.file_index) else {
            return String::new();
        };
        let mut row = 0usize;
        for hunk in &file.hunks {
            let hunk_start_row = row;
            row = row.saturating_add(1);
            let hunk_end_row = row.saturating_add(hunk.lines.len());
            if anchor.diff_row < hunk_start_row || anchor.diff_row >= hunk_end_row {
                row = hunk_end_row;
                continue;
            }
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
            return lines.join("\n");
        }
        String::new()
    }

    /// Delete the latest draft comment at the selected line.
    pub fn delete_latest_draft_at_selection(&mut self) -> bool {
        let Some((anchor, selected_index)) = self
            .selected_draft_anchor_and_comment_index()
            .or_else(|| self.selected_comment_anchor().map(|anchor| (anchor, None)))
        else {
            self.status_message = Some("select a commented line to delete a draft".to_string());
            return true;
        };
        let Some(comments) = self.draft_comments.get_mut(&anchor) else {
            self.status_message = Some("no draft comment at selected line".to_string());
            return true;
        };
        if comments.is_empty() {
            self.status_message = Some("no draft comment at selected line".to_string());
            return true;
        }
        let index = selected_index.unwrap_or_else(|| comments.len().saturating_sub(1));
        if index >= comments.len() {
            self.status_message = Some("no draft comment at selected line".to_string());
            return true;
        }
        let comment = comments.remove(index);
        if comments.is_empty() {
            self.draft_comments.remove(&anchor);
            self.selected_view_target = None;
        }
        self.pending_draft_delete = Some(PendingDraftDelete {
            target: review_scope_for_workspace(&self.workspace).target().clone(),
            scope: Some(review_scope_for_workspace(&self.workspace)),
            anchor,
            comment,
        });
        self.status_message = Some("deleted draft comment".to_string());
        true
    }

    /// Restore local thread resolution state after failed persistence.
    pub fn restore_thread_resolution(&mut self, resolve: &PendingThreadResolve) {
        let thread_key = Self::thread_key_for_anchor(&resolve.anchor);
        if resolve.resolved {
            self.resolved_review_threads.remove(&thread_key);
        } else {
            self.resolved_review_threads.insert(thread_key);
        }
    }

    /// Return a footer preview for the selected thread.
    #[must_use]
    pub fn selected_thread_preview(&self) -> Option<String> {
        let thread = self
            .visible_thread_summaries()
            .get(self.selected_thread)?
            .clone();
        let range = if thread.anchor.start_diff_row() == thread.anchor.end_diff_row() {
            format!("@{}", thread.anchor.start_diff_row())
        } else {
            format!(
                "@{}-{}",
                thread.anchor.start_diff_row(),
                thread.anchor.end_diff_row()
            )
        };
        let linked = thread
            .session_id
            .as_deref()
            .map_or(String::new(), |session_id| format!("  🤖 {session_id}"));
        let status = if thread.resolved { "resolved" } else { "open" };
        Some(format!(
            " {status} thread {} {range} x{}:{linked} {}  Enter jump  r resolve/reopen  a ask/follow up  o open ",
            thread.anchor.path, thread.draft_count, thread.latest_body
        ))
    }

    /// Return a short preview for the selected line's latest draft comment.
    #[must_use]
    pub fn selected_draft_preview(&self) -> Option<String> {
        let (anchor, selected_index) = self
            .selected_draft_anchor_and_comment_index()
            .or_else(|| self.selected_comment_anchor().map(|anchor| (anchor, None)))?;
        let comments = self.draft_comments.get(&anchor)?;
        let index = selected_index.unwrap_or_else(|| comments.len().saturating_sub(1));
        let comment = comments.get(index)?;
        Some(format!("{} draft: {}", comments.len(), comment.body))
    }

    /// Return linked session id for the selected line's latest draft comment.
    #[must_use]
    pub fn selected_draft_session_id(&self) -> Option<&str> {
        let (anchor, selected_index) = self
            .selected_draft_anchor_and_comment_index()
            .or_else(|| self.selected_comment_anchor().map(|anchor| (anchor, None)))?;
        let comments = self.draft_comments.get(&anchor)?;
        let index = selected_index.unwrap_or_else(|| comments.len().saturating_sub(1));
        comments.get(index)?.session_id.as_deref()
    }

    /// Load persisted draft comments into local state.
    fn load_persisted_drafts(&mut self, drafts: Vec<DraftComment>) {
        for draft in drafts {
            if let Some(anchor) = self.anchor_from_persisted_draft(&draft) {
                self.draft_comments
                    .entry(anchor.clone())
                    .or_default()
                    .push(ReviewDraftComment {
                        id: Some(draft.comment_id),
                        body: draft.body,
                        persisted: true,
                        created_at_ms: Some(draft.created_at_ms),
                        updated_at_ms: Some(draft.updated_at_ms),
                        session_id: draft.session_id,
                    });
                if draft.resolved_at_ms.is_some() {
                    self.resolved_review_threads
                        .insert(Self::thread_key_for_anchor(&anchor));
                }
            }
        }
    }

    fn anchor_from_persisted_draft(&self, draft: &DraftComment) -> Option<ReviewCommentAnchor> {
        let diff_row = usize::try_from(draft.anchor.diff_row).ok()?;
        let end_diff_row = draft
            .anchor
            .end_diff_row
            .and_then(|row| usize::try_from(row).ok())
            .filter(|row| *row != diff_row);
        let file_index = self
            .review
            .files
            .iter()
            .position(|file| file.display_path() == draft.anchor.file_path)?;
        Some(ReviewCommentAnchor {
            file_index,
            path: draft.anchor.file_path.clone(),
            diff_row,
            end_diff_row,
            old_line: draft.anchor.old_line,
            new_line: draft.anchor.new_line,
            old_start: draft.anchor.old_start.or(draft.anchor.old_line),
            old_end: draft.anchor.old_end.or(draft.anchor.old_line),
            new_start: draft.anchor.new_start.or(draft.anchor.new_line),
            new_end: draft.anchor.new_end.or(draft.anchor.new_line),
            line_kind: draft.anchor.line_kind,
            is_file_anchor: draft.anchor.is_file_anchor,
            surface_id: draft.anchor.surface_id.clone(),
            source_id: draft.anchor.source_id.clone(),
        })
    }

    /// Return the selected diff line comment anchor, if the selected row is commentable.
    #[must_use]
    pub fn selected_comment_anchor(&self) -> Option<ReviewCommentAnchor> {
        self.selected_draft_anchor_and_comment_index()
            .map(|(anchor, _)| anchor)
            .or_else(|| self.comment_anchor_for_row(self.selected_diff_line))
    }

    fn selected_draft_anchor_and_comment_index(
        &self,
    ) -> Option<(ReviewCommentAnchor, Option<usize>)> {
        match self.selected_view_target.as_ref()? {
            ReviewViewTarget::Thread { thread_key }
            | ReviewViewTarget::ThreadAction { thread_key, .. } => self
                .draft_anchor_for_thread_key(thread_key)
                .map(|anchor| (anchor, None)),
            ReviewViewTarget::Comment {
                thread_key,
                comment_index,
            } => self
                .draft_anchor_for_thread_key(thread_key)
                .map(|anchor| (anchor, Some(*comment_index))),
            ReviewViewTarget::HunkHeader { .. } | ReviewViewTarget::SourceLine { .. } => None,
        }
    }

    fn draft_anchor_for_thread_key(&self, thread_key: &str) -> Option<ReviewCommentAnchor> {
        self.draft_comments
            .keys()
            .find(|anchor| Self::thread_key_for_anchor(anchor) == thread_key)
            .cloned()
    }

    fn thread_key_for_anchor(anchor: &ReviewCommentAnchor) -> String {
        ReviewThreadAnchor {
            file_index: anchor.file_index,
            path: anchor.path.clone(),
            source_row: anchor.diff_row,
            end_source_row: anchor.end_diff_row,
        }
        .thread_key()
    }

    /// Return a comment anchor for a rendered diff row.
    #[must_use]
    pub fn comment_anchor_for_row(&self, diff_row: usize) -> Option<ReviewCommentAnchor> {
        let file = self.selected_file_data()?;
        let (surface_id, source_id) = self.selected_surface_ids();
        if self.review.is_repository_review() {
            let (start_row, end_row) = self.selected_range_bounds().unwrap_or((diff_row, diff_row));
            let start_line = u32::try_from(start_row.saturating_add(1)).ok()?;
            let end_line = u32::try_from(end_row.saturating_add(1)).ok()?;
            return Some(ReviewCommentAnchor {
                file_index: self.selected_file,
                path: file.display_path().to_string(),
                diff_row: start_row,
                end_diff_row: (end_row != start_row).then_some(end_row),
                old_line: None,
                new_line: Some(start_line),
                old_start: None,
                old_end: None,
                new_start: Some(start_line),
                new_end: Some(end_line),
                line_kind: ReviewLineKind::Context,
                is_file_anchor: true,
                surface_id,
                source_id,
            });
        }
        if self
            .selected_surface()
            .is_some_and(|surface| surface.kind == ReviewSurfaceKind::File)
        {
            let (start_row, end_row) = self.selected_range_bounds().unwrap_or((diff_row, diff_row));
            let start_line = self.materialized_file_line_number(start_row)?;
            let end_line = self.materialized_file_line_number(end_row)?;
            return Some(ReviewCommentAnchor {
                file_index: self.selected_file,
                path: file.display_path().to_string(),
                diff_row: start_row,
                end_diff_row: (end_row != start_row).then_some(end_row),
                old_line: None,
                new_line: Some(start_line),
                old_start: None,
                old_end: None,
                new_start: Some(start_line),
                new_end: Some(end_line),
                line_kind: ReviewLineKind::Context,
                is_file_anchor: true,
                surface_id,
                source_id,
            });
        }
        let (start_row, end_row) = self.selected_range_bounds().unwrap_or((diff_row, diff_row));
        let start_line = self.diff_line_for_render_row(start_row)?;
        let end_line = self.diff_line_for_render_row(end_row)?;
        let (surface_id, source_id) = self.selected_surface_ids();
        Some(ReviewCommentAnchor {
            file_index: self.selected_file,
            path: file.display_path().to_string(),
            diff_row: start_row,
            end_diff_row: (end_row != start_row).then_some(end_row),
            old_line: start_line.old_line,
            new_line: start_line.new_line,
            old_start: start_line.old_line.or(end_line.old_line),
            old_end: end_line.old_line.or(start_line.old_line),
            new_start: start_line.new_line.or(end_line.new_line),
            new_end: end_line.new_line.or(start_line.new_line),
            line_kind: start_line.kind,
            is_file_anchor: false,
            surface_id,
            source_id,
        })
    }

    fn materialized_file_line_number(&self, diff_row: usize) -> Option<u32> {
        let file = self.selected_file_data()?;
        let rows = materialized_file_surface_rows(file);
        rows.get(diff_row).and_then(|(line, _)| *line)
    }

    fn diff_line_for_render_row(&self, diff_row: usize) -> Option<&ReviewLine> {
        let file = self.selected_file_data()?;
        if file.is_binary {
            return None;
        }
        let mut row = 0usize;
        for hunk in &file.hunks {
            if diff_row == row {
                return None;
            }
            row = row.saturating_add(1);
            let hunk_line_index = diff_row.checked_sub(row)?;
            if hunk_line_index < hunk.lines.len() {
                return hunk.lines.get(hunk_line_index);
            }
            row = row.saturating_add(hunk.lines.len());
        }
        None
    }

    /// Return current hunk position as one-based `(current, total)`.
    #[must_use]
    pub fn hunk_position(&self) -> (usize, usize) {
        let rows = self.hunk_render_rows();
        let total = rows.len();
        let current = rows
            .iter()
            .position(|row| *row > self.selected_diff_line)
            .unwrap_or(total)
            .max(1);
        (current, total)
    }

    fn ensure_selected_diff_line_visible(&mut self) {
        let height = self
            .last_diff_area
            .map_or(1, |area| usize::from(area.height).max(1));
        let selected_visual_row = self.selected_diff_visual_row();
        if selected_visual_row < self.diff_scroll {
            self.diff_scroll = selected_visual_row;
        } else if selected_visual_row >= self.diff_scroll.saturating_add(height) {
            self.diff_scroll = selected_visual_row.saturating_sub(height.saturating_sub(1));
        }
        self.diff_scroll = self.diff_scroll.min(self.max_diff_scroll());
    }

    fn selected_diff_visual_row(&self) -> usize {
        let document = self.current_review_view_document();
        if let Some(target) = &self.selected_view_target
            && let Some(visual_row) = document.as_ref().and_then(|document| {
                document
                    .rows
                    .iter()
                    .find(|row| &row.target == target)
                    .map(|row| row.visual_row)
            })
        {
            return visual_row;
        }
        document
            .as_ref()
            .and_then(|document| document.visual_row_for_source_row(self.selected_diff_line))
            .or_else(|| {
                document.as_ref().and_then(|document| {
                    document.rows.iter().find_map(|row| match &row.target {
                        ReviewViewTarget::Thread { thread_key }
                            if self.collapsed_review_threads.contains(thread_key) =>
                        {
                            Some(row.visual_row)
                        }
                        _ => None,
                    })
                })
            })
            .unwrap_or(self.selected_diff_line)
    }

    fn max_diff_scroll(&self) -> usize {
        self.rendered_diff_len().saturating_sub(
            self.last_diff_area
                .map_or(1, |area| usize::from(area.height).max(1)),
        )
    }

    fn rendered_diff_len(&self) -> usize {
        self.current_review_view_document()
            .map_or_else(
                || self.source_rendered_diff_len(),
                |document| document.rows.len(),
            )
            .max(1)
    }

    fn source_rendered_diff_len(&self) -> usize {
        if self.review.is_repository_review() {
            let Some(path) = self.selected_file_path() else {
                return 1;
            };
            return self
                .file_cache
                .get(&path)
                .map_or(1, |file| file.line_spans.len().max(1));
        }
        let Some(file) = self.selected_file_data() else {
            return 1;
        };
        if self
            .selected_surface()
            .is_some_and(|surface| surface.kind == ReviewSurfaceKind::File)
        {
            return materialized_file_surface_len(file).max(1);
        }
        if file.is_binary {
            return 1;
        }
        file.hunks
            .iter()
            .map(|hunk| hunk.lines.len().saturating_add(1))
            .sum::<usize>()
            .max(1)
    }

    /// Build the semantic view document for the current main review pane.
    #[must_use]
    pub fn current_review_view_document(&self) -> Option<ReviewViewDocument> {
        let mut document = if self.review.is_repository_review() {
            let path = self.selected_file_path()?;
            let cached = self.file_cache.get(&path)?;
            if cached.unavailable_reason.is_some() {
                return None;
            }
            ReviewViewDocument::build_repository_file(self.selected_file, cached)
        } else {
            let file = self.selected_file_data()?;
            if file.is_binary {
                return None;
            }
            if self
                .selected_surface()
                .is_some_and(|surface| surface.kind == ReviewSurfaceKind::File)
            {
                ReviewViewDocument::build_materialized_file_surface(self.selected_file, file)
            } else {
                ReviewViewDocument::build_diff_file(self.selected_file, file, true)
            }
        };
        document = document.with_inline_draft_threads(
            self.selected_file,
            self.draft_comments.iter().map(|(anchor, comments)| {
                (
                    ReviewThreadAnchor {
                        file_index: anchor.file_index,
                        path: anchor.path.clone(),
                        source_row: anchor.diff_row,
                        end_source_row: anchor.end_diff_row,
                    },
                    comments.clone(),
                )
            }),
            &self.collapsed_review_threads,
            &self.resolved_review_threads,
            self.show_resolved_threads,
        );
        Some(document)
    }

    fn hunk_render_rows(&self) -> Vec<usize> {
        let Some(file) = self.selected_file_data() else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        if self
            .selected_surface()
            .is_some_and(|surface| surface.kind == ReviewSurfaceKind::File)
        {
            let mut row = 0usize;
            for hunk in &file.hunks {
                if hunk.heading.is_some() {
                    rows.push(row);
                    row = row.saturating_add(1);
                }
                row = row.saturating_add(hunk.lines.len());
            }
            return rows;
        }
        let mut row = 0usize;
        for hunk in &file.hunks {
            rows.push(row);
            row = row.saturating_add(hunk.lines.len()).saturating_add(1);
        }
        rows
    }
}

fn materialized_file_surface_len(file: &ReviewFile) -> usize {
    materialized_file_surface_rows(file).len()
}

/// Return current sidebar width for an app and terminal width.
#[must_use]
pub fn sidebar_width(app: &ReviewApp, width: u16) -> u16 {
    if app.sidebar_visible && width >= 80 {
        FILE_SIDEBAR_WIDTH.min(width.saturating_sub(30))
    } else {
        0
    }
}

fn rendered_rows_for_prompt(file: &ReviewFile) -> Vec<String> {
    let mut rows = Vec::new();
    for hunk in &file.hunks {
        rows.push(format!(
            "@@ -{},{} +{},{} @@{}",
            hunk.old_start,
            hunk.old_count,
            hunk.new_start,
            hunk.new_count,
            hunk.heading
                .as_ref()
                .map_or(String::new(), |heading| format!(" {heading}")),
        ));
        rows.extend(hunk.lines.iter().map(|line| {
            let marker = match line.kind {
                ReviewLineKind::Context => ' ',
                ReviewLineKind::Added => '+',
                ReviewLineKind::Removed => '-',
            };
            format!("{marker}{}", line.content)
        }));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_app() -> ReviewApp {
        ReviewApp::new(ReviewSummary {
            title: "test".to_string(),
            repo_root: PathBuf::from("/repo"),
            additions: 2,
            deletions: 1,
            workspace: None,
            surfaces: Vec::new(),
            diagnostics: Vec::new(),
            repository_files: Vec::new(),
            repository_branches: Vec::new(),
            repository_commits: Vec::new(),
            files: vec![
                ReviewFile {
                    old_path: Some("a.rs".to_string()),
                    new_path: Some("a.rs".to_string()),
                    status: ReviewFileStatus::Modified,
                    additions: 2,
                    deletions: 1,
                    is_binary: false,
                    hunks: vec![
                        ReviewHunk {
                            old_start: 1,
                            old_count: 1,
                            new_start: 1,
                            new_count: 2,
                            heading: None,
                            lines: vec![
                                ReviewLine {
                                    kind: ReviewLineKind::Removed,
                                    old_line: Some(1),
                                    new_line: None,
                                    content: "old".to_string(),
                                },
                                ReviewLine {
                                    kind: ReviewLineKind::Added,
                                    old_line: None,
                                    new_line: Some(1),
                                    content: "new".to_string(),
                                },
                            ],
                        },
                        ReviewHunk {
                            old_start: 10,
                            old_count: 1,
                            new_start: 11,
                            new_count: 1,
                            heading: Some("next".to_string()),
                            lines: vec![ReviewLine {
                                kind: ReviewLineKind::Context,
                                old_line: Some(10),
                                new_line: Some(11),
                                content: "ctx".to_string(),
                            }],
                        },
                    ],
                },
                ReviewFile {
                    old_path: Some("b.rs".to_string()),
                    new_path: Some("b.rs".to_string()),
                    status: ReviewFileStatus::Modified,
                    additions: 0,
                    deletions: 0,
                    is_binary: false,
                    hunks: Vec::new(),
                },
            ],
        })
    }

    #[test]
    fn inline_draft_navigation_selects_comment_targets() {
        let mut app = sample_app();
        for (index, diff_row) in [1_usize, 2_usize].into_iter().enumerate() {
            let anchor = ReviewCommentAnchor {
                file_index: 0,
                path: "a.rs".to_string(),
                diff_row,
                end_diff_row: None,
                old_line: (diff_row == 1).then_some(1),
                new_line: (diff_row == 2).then_some(1),
                old_start: (diff_row == 1).then_some(1),
                old_end: (diff_row == 1).then_some(1),
                new_start: (diff_row == 2).then_some(1),
                new_end: (diff_row == 2).then_some(1),
                line_kind: if diff_row == 1 {
                    ReviewLineKind::Removed
                } else {
                    ReviewLineKind::Added
                },
                is_file_anchor: false,
                surface_id: None,
                source_id: None,
            };
            app.draft_comments.insert(
                anchor,
                vec![ReviewDraftComment {
                    id: Some(format!("comment-{index}")),
                    body: "note".to_string(),
                    persisted: true,
                    created_at_ms: None,
                    updated_at_ms: None,
                    session_id: None,
                }],
            );
        }

        assert!(app.select_next_inline_draft());
        assert!(matches!(
            app.selected_view_target,
            Some(ReviewViewTarget::Comment {
                comment_index: 0,
                ..
            })
        ));
        assert!(app.select_next_inline_draft());
        assert_eq!(app.selected_diff_line, 2);
    }

    #[test]
    fn scrolling_keeps_selection_on_visible_semantic_rows() {
        let mut app = sample_app();
        app.last_diff_area = Some(Rect::new(0, 0, 80, 2));
        let anchor = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 2,
            end_diff_row: None,
            old_line: None,
            new_line: Some(1),
            old_start: None,
            old_end: None,
            new_start: Some(1),
            new_end: Some(1),
            line_kind: ReviewLineKind::Added,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        app.draft_comments.insert(
            anchor,
            vec![ReviewDraftComment {
                id: Some("comment-1".to_string()),
                body: "note".to_string(),
                persisted: true,
                created_at_ms: None,
                updated_at_ms: None,
                session_id: None,
            }],
        );

        assert!(app.scroll_down(3));

        assert!(app.selected_diff_visual_row() >= app.diff_scroll);
        assert!(app.selected_diff_visual_row() < app.diff_scroll + 2);
    }

    #[test]
    fn clicking_inline_thread_header_toggles_collapse() {
        let mut app = sample_app();
        let anchor = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 2,
            end_diff_row: None,
            old_line: None,
            new_line: Some(1),
            old_start: None,
            old_end: None,
            new_start: Some(1),
            new_end: Some(1),
            line_kind: ReviewLineKind::Added,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        let thread_key = ReviewApp::thread_key_for_anchor(&anchor);
        app.draft_comments.insert(
            anchor,
            vec![ReviewDraftComment {
                id: Some("comment-1".to_string()),
                body: "note".to_string(),
                persisted: true,
                created_at_ms: None,
                updated_at_ms: None,
                session_id: None,
            }],
        );
        let document = app.current_review_view_document().expect("document");
        let thread_row = document
            .rows
            .iter()
            .find(|row| matches!(row.target, ReviewViewTarget::Thread { .. }))
            .expect("thread row")
            .visual_row;

        assert!(app.handle_review_view_click(thread_row));

        assert!(app.collapsed_review_threads.contains(&thread_key));
    }

    #[test]
    fn clicking_inline_action_activates_action() {
        let mut app = sample_app();
        let anchor = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 2,
            end_diff_row: None,
            old_line: None,
            new_line: Some(1),
            old_start: None,
            old_end: None,
            new_start: Some(1),
            new_end: Some(1),
            line_kind: ReviewLineKind::Added,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        app.draft_comments.insert(
            anchor,
            vec![ReviewDraftComment {
                id: Some("comment-1".to_string()),
                body: "note".to_string(),
                persisted: true,
                created_at_ms: None,
                updated_at_ms: None,
                session_id: None,
            }],
        );
        let document = app.current_review_view_document().expect("document");
        let resolve_row = document
            .rows
            .iter()
            .find(|row| {
                matches!(
                    &row.target,
                    ReviewViewTarget::ThreadAction { action, .. } if action == "resolve"
                )
            })
            .expect("resolve action")
            .visual_row;

        assert!(app.handle_review_view_click(resolve_row));

        assert_eq!(app.resolved_review_threads.len(), 1);
    }

    #[test]
    fn selected_inline_thread_can_toggle_resolved_state() {
        let mut app = sample_app();
        let anchor = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 2,
            end_diff_row: None,
            old_line: None,
            new_line: Some(1),
            old_start: None,
            old_end: None,
            new_start: Some(1),
            new_end: Some(1),
            line_kind: ReviewLineKind::Added,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        let thread_key = ReviewApp::thread_key_for_anchor(&anchor);
        app.draft_comments.insert(
            anchor,
            vec![ReviewDraftComment {
                id: Some("comment-1".to_string()),
                body: "note".to_string(),
                persisted: true,
                created_at_ms: None,
                updated_at_ms: None,
                session_id: None,
            }],
        );
        app.selected_view_target = Some(ReviewViewTarget::Thread {
            thread_key: thread_key.clone(),
        });

        assert!(app.toggle_selected_thread_resolved());
        assert!(app.resolved_review_threads.contains(&thread_key));
        let document = app.current_review_view_document().expect("document");
        assert!(document.rows.iter().any(|row| matches!(
            row.block,
            ReviewViewBlock::InlineThreadHeader { resolved: true, .. }
        )));
        assert!(app.toggle_selected_thread_resolved());
        assert!(!app.resolved_review_threads.contains(&thread_key));
    }

    #[test]
    fn hiding_resolved_threads_removes_them_from_document() {
        let mut app = sample_app();
        let anchor = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 2,
            end_diff_row: None,
            old_line: None,
            new_line: Some(1),
            old_start: None,
            old_end: None,
            new_start: Some(1),
            new_end: Some(1),
            line_kind: ReviewLineKind::Added,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        let thread_key = ReviewApp::thread_key_for_anchor(&anchor);
        app.draft_comments.insert(
            anchor,
            vec![ReviewDraftComment {
                id: Some("comment-1".to_string()),
                body: "note".to_string(),
                persisted: true,
                created_at_ms: None,
                updated_at_ms: None,
                session_id: None,
            }],
        );
        app.resolved_review_threads.insert(thread_key.clone());

        app.thread_filter = ReviewThreadFilter::Open;
        assert!(app.visible_thread_summaries().is_empty());
        app.thread_filter = ReviewThreadFilter::Resolved;
        assert_eq!(app.visible_thread_summaries().len(), 1);

        assert!(app.toggle_show_resolved_threads());

        let document = app.current_review_view_document().expect("document");
        assert!(!document.rows.iter().any(|row| {
            row.target
                == ReviewViewTarget::Thread {
                    thread_key: thread_key.clone(),
                }
        }));
    }

    #[test]
    fn selected_inline_thread_can_toggle_collapsed_state() {
        let mut app = sample_app();
        let anchor = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 2,
            end_diff_row: None,
            old_line: None,
            new_line: Some(1),
            old_start: None,
            old_end: None,
            new_start: Some(1),
            new_end: Some(1),
            line_kind: ReviewLineKind::Added,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        let thread_key = ReviewApp::thread_key_for_anchor(&anchor);
        app.draft_comments.insert(
            anchor,
            vec![ReviewDraftComment {
                id: Some("comment-1".to_string()),
                body: "note".to_string(),
                persisted: true,
                created_at_ms: None,
                updated_at_ms: None,
                session_id: None,
            }],
        );
        app.selected_view_target = Some(ReviewViewTarget::Thread {
            thread_key: thread_key.clone(),
        });

        assert!(app.toggle_selected_inline_thread());
        assert!(app.collapsed_review_threads.contains(&thread_key));
        assert!(app.toggle_selected_inline_thread());
        assert!(!app.collapsed_review_threads.contains(&thread_key));
    }

    #[test]
    fn inline_thread_navigation_selects_next_thread_target() {
        let mut app = sample_app();
        let first = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 1,
            end_diff_row: None,
            old_line: Some(1),
            new_line: None,
            old_start: Some(1),
            old_end: Some(1),
            new_start: None,
            new_end: None,
            line_kind: ReviewLineKind::Removed,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        let second = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 2,
            end_diff_row: None,
            old_line: None,
            new_line: Some(1),
            old_start: None,
            old_end: None,
            new_start: Some(1),
            new_end: Some(1),
            line_kind: ReviewLineKind::Added,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        for anchor in [first.clone(), second.clone()] {
            app.draft_comments.insert(
                anchor,
                vec![ReviewDraftComment {
                    id: Some("comment".to_string()),
                    body: "note".to_string(),
                    persisted: true,
                    created_at_ms: None,
                    updated_at_ms: None,
                    session_id: None,
                }],
            );
        }
        app.select_anchor(&first);
        app.selected_view_target = Some(ReviewViewTarget::Thread {
            thread_key: ReviewApp::thread_key_for_anchor(&first),
        });

        assert!(app.select_next_inline_thread());

        assert_eq!(app.selected_diff_line, second.diff_row);
        assert_eq!(
            app.selected_view_target,
            Some(ReviewViewTarget::Thread {
                thread_key: ReviewApp::thread_key_for_anchor(&second),
            })
        );
    }

    #[test]
    fn open_thread_navigation_skips_resolved_threads() {
        let mut app = sample_app();
        let first = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 1,
            end_diff_row: None,
            old_line: Some(1),
            new_line: None,
            old_start: Some(1),
            old_end: Some(1),
            new_start: None,
            new_end: None,
            line_kind: ReviewLineKind::Removed,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        let second = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 2,
            end_diff_row: None,
            old_line: None,
            new_line: Some(1),
            old_start: None,
            old_end: None,
            new_start: Some(1),
            new_end: Some(1),
            line_kind: ReviewLineKind::Added,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        for anchor in [first.clone(), second.clone()] {
            app.draft_comments.insert(
                anchor,
                vec![ReviewDraftComment {
                    id: Some("comment".to_string()),
                    body: "note".to_string(),
                    persisted: true,
                    created_at_ms: None,
                    updated_at_ms: None,
                    session_id: None,
                }],
            );
        }
        app.resolved_review_threads
            .insert(ReviewApp::thread_key_for_anchor(&first));
        app.select_anchor(&first);

        assert!(app.select_next_open_thread());

        assert_eq!(app.selected_diff_line, second.diff_row);
        assert_eq!(
            app.selected_view_target,
            Some(ReviewViewTarget::Thread {
                thread_key: ReviewApp::thread_key_for_anchor(&second),
            })
        );
    }

    #[test]
    fn open_thread_navigation_selects_previous_thread() {
        let mut app = sample_app();
        let first = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 1,
            end_diff_row: None,
            old_line: Some(1),
            new_line: None,
            old_start: Some(1),
            old_end: Some(1),
            new_start: None,
            new_end: None,
            line_kind: ReviewLineKind::Removed,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        let second = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 2,
            end_diff_row: None,
            old_line: None,
            new_line: Some(1),
            old_start: None,
            old_end: None,
            new_start: Some(1),
            new_end: Some(1),
            line_kind: ReviewLineKind::Added,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        for anchor in [first.clone(), second.clone()] {
            app.draft_comments.insert(
                anchor,
                vec![ReviewDraftComment {
                    id: Some("comment".to_string()),
                    body: "note".to_string(),
                    persisted: true,
                    created_at_ms: None,
                    updated_at_ms: None,
                    session_id: None,
                }],
            );
        }
        app.select_anchor(&second);
        app.selected_view_target = Some(ReviewViewTarget::Thread {
            thread_key: ReviewApp::thread_key_for_anchor(&second),
        });

        assert!(app.select_previous_open_thread());

        assert_eq!(app.selected_diff_line, first.diff_row);
    }

    #[test]
    fn show_attention_sidebar_focuses_attention_mode() {
        let mut app = sample_app();
        app.sidebar_visible = false;

        assert!(app.show_attention_sidebar());

        assert!(app.sidebar_visible);
        assert_eq!(app.sidebar_mode, ReviewSidebarMode::NeedsAttention);
        assert_eq!(app.status_message.as_deref(), Some("sidebar: attention"));
    }

    #[test]
    fn attention_sidebar_shows_only_open_threads() {
        let mut app = sample_app();
        let open = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 1,
            end_diff_row: None,
            old_line: Some(1),
            new_line: None,
            old_start: Some(1),
            old_end: Some(1),
            new_start: None,
            new_end: None,
            line_kind: ReviewLineKind::Removed,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        let resolved = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 2,
            end_diff_row: None,
            old_line: None,
            new_line: Some(1),
            old_start: None,
            old_end: None,
            new_start: Some(1),
            new_end: Some(1),
            line_kind: ReviewLineKind::Added,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        for anchor in [open.clone(), resolved.clone()] {
            app.draft_comments.insert(
                anchor,
                vec![ReviewDraftComment {
                    id: Some("comment".to_string()),
                    body: "note".to_string(),
                    persisted: true,
                    created_at_ms: None,
                    updated_at_ms: None,
                    session_id: None,
                }],
            );
        }
        app.resolved_review_threads
            .insert(ReviewApp::thread_key_for_anchor(&resolved));
        app.sidebar_mode = ReviewSidebarMode::NeedsAttention;

        let threads = app.visible_thread_summaries();

        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].anchor, open);
    }

    #[test]
    fn global_open_thread_navigation_crosses_files() {
        let mut app = sample_app();
        let first = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 1,
            end_diff_row: None,
            old_line: Some(1),
            new_line: None,
            old_start: Some(1),
            old_end: Some(1),
            new_start: None,
            new_end: None,
            line_kind: ReviewLineKind::Removed,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        let second = ReviewCommentAnchor {
            file_index: 1,
            path: "b.rs".to_string(),
            diff_row: 0,
            end_diff_row: None,
            old_line: None,
            new_line: None,
            old_start: None,
            old_end: None,
            new_start: None,
            new_end: None,
            line_kind: ReviewLineKind::Context,
            is_file_anchor: true,
            surface_id: None,
            source_id: None,
        };
        for anchor in [first.clone(), second.clone()] {
            app.draft_comments.insert(
                anchor,
                vec![ReviewDraftComment {
                    id: Some("comment".to_string()),
                    body: "note".to_string(),
                    persisted: true,
                    created_at_ms: None,
                    updated_at_ms: None,
                    session_id: None,
                }],
            );
        }
        app.select_anchor(&first);
        app.selected_view_target = Some(ReviewViewTarget::Thread {
            thread_key: ReviewApp::thread_key_for_anchor(&first),
        });

        assert!(app.select_next_open_thread_global());

        assert_eq!(app.selected_file, 1);
        assert_eq!(
            app.selected_view_target,
            Some(ReviewViewTarget::Thread {
                thread_key: ReviewApp::thread_key_for_anchor(&second),
            })
        );
    }

    #[test]
    fn semantic_view_document_includes_inline_thread_actions() {
        let mut app = sample_app();
        let anchor = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 2,
            end_diff_row: None,
            old_line: None,
            new_line: Some(1),
            old_start: None,
            old_end: None,
            new_start: Some(1),
            new_end: Some(1),
            line_kind: ReviewLineKind::Added,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        app.draft_comments.insert(
            anchor.clone(),
            vec![ReviewDraftComment {
                id: Some("comment-1".to_string()),
                body: "Looks risky".to_string(),
                persisted: true,
                created_at_ms: None,
                updated_at_ms: None,
                session_id: None,
            }],
        );

        let document = app.current_review_view_document().expect("document");
        let thread_key = ReviewApp::thread_key_for_anchor(&anchor);

        assert!(document.rows.iter().any(|row| {
            row.target
                == ReviewViewTarget::Thread {
                    thread_key: thread_key.clone(),
                }
        }));
        assert!(document.rows.iter().any(|row| {
            row.target
                == ReviewViewTarget::ThreadAction {
                    thread_key: thread_key.clone(),
                    action: "reply".to_string(),
                }
        }));
    }

    #[test]
    fn file_navigation_resets_diff_state() {
        let mut app = sample_app();
        app.diff_scroll = 2;
        app.selected_diff_line = 2;

        assert!(app.select_next_file());

        assert_eq!(app.selected_file, 1);
        assert_eq!(app.diff_scroll, 0);
        assert_eq!(app.selected_diff_line, 0);
    }

    #[test]
    fn hunk_navigation_tracks_selected_line() {
        let mut app = sample_app();
        app.set_diff_area(Rect::new(0, 0, 80, 2));

        assert!(app.select_next_hunk());

        assert_eq!(app.selected_diff_line, 3);
        assert_eq!(app.diff_scroll, 2);
        assert_eq!(app.hunk_position(), (2, 2));
    }

    #[test]
    fn creates_anchor_for_selected_diff_line() {
        let mut app = sample_app();
        app.selected_diff_line = 2;

        let anchor = app
            .selected_comment_anchor()
            .expect("added line should be commentable");

        assert_eq!(anchor.path, "a.rs");
        assert_eq!(anchor.diff_row, 2);
        assert_eq!(anchor.old_line, None);
        assert_eq!(anchor.new_line, Some(1));
        assert_eq!(anchor.line_kind, ReviewLineKind::Added);
    }

    #[test]
    fn hunk_header_is_not_commentable() {
        let app = sample_app();

        assert_eq!(app.comment_anchor_for_row(0), None);
    }

    #[test]
    fn saves_in_memory_draft_comment() {
        let mut app = sample_app();
        app.selected_diff_line = 2;

        assert!(app.open_comment_editor());
        app.comment_editor
            .as_mut()
            .expect("editor should open")
            .buffer
            .insert_str("Needs a test");
        assert!(app.save_comment_editor());

        assert_eq!(app.draft_comment_count(), 1);
        assert!(app.has_draft_comment_at(0, 2));
        assert_eq!(app.draft_comment_count_for_file(0), 1);
        assert_eq!(app.open_thread_count_for_file(0), 1);
        let pending = app
            .take_pending_draft_save()
            .expect("draft should be pending persistence");
        assert_eq!(pending.anchor.diff_row, 2);
        assert_eq!(pending.body, "Needs a test");
    }

    #[test]
    fn edits_persisted_draft_comment() {
        let mut app = sample_app();
        app.selected_diff_line = 2;
        app.load_persisted_drafts(vec![DraftComment {
            comment_id: "comment-1".to_string(),
            thread_id: "thread-1".to_string(),
            anchor: DraftAnchor {
                file_path: "a.rs".to_string(),
                diff_row: 2,
                start_diff_row: Some(2),
                end_diff_row: Some(2),
                old_start: None,
                old_end: None,
                new_start: Some(1),
                new_end: Some(1),
                old_line: None,
                new_line: Some(1),
                line_kind: ReviewLineKind::Added,
                is_file_anchor: false,
                surface_id: Some("surface-0".to_string()),
                source_id: Some("source-1".to_string()),
            },
            body: "Before".to_string(),
            created_at_ms: 1,
            updated_at_ms: 1,
            session_id: None,
            resolved_at_ms: None,
        }]);

        assert!(app.open_latest_draft_editor());
        let editor = app.comment_editor.as_mut().expect("editor should open");
        editor.buffer = TextEditBuffer::from_text("After");
        assert!(app.save_comment_editor());

        let pending = app
            .take_pending_draft_update()
            .expect("update should be pending persistence");
        assert_eq!(pending.comment_id, "comment-1");
        assert_eq!(pending.previous_body, "Before");
        assert_eq!(pending.new_body, "After");
        assert_eq!(
            app.selected_draft_preview().as_deref(),
            Some("1 draft: After")
        );
    }

    #[test]
    fn loads_persisted_drafts_into_local_state() {
        let mut app = sample_app();

        app.load_persisted_drafts(vec![DraftComment {
            comment_id: "comment-1".to_string(),
            thread_id: "thread-1".to_string(),
            anchor: DraftAnchor {
                file_path: "a.rs".to_string(),
                diff_row: 2,
                start_diff_row: Some(2),
                end_diff_row: Some(2),
                old_start: None,
                old_end: None,
                new_start: Some(1),
                new_end: Some(1),
                old_line: None,
                new_line: Some(1),
                line_kind: ReviewLineKind::Added,
                is_file_anchor: false,
                surface_id: Some("surface-0".to_string()),
                source_id: Some("source-1".to_string()),
            },
            body: "Persisted".to_string(),
            created_at_ms: 1,
            updated_at_ms: 1,
            session_id: None,
            resolved_at_ms: None,
        }]);

        assert_eq!(app.draft_comment_count(), 1);
        assert!(app.has_draft_comment_at(0, 2));
    }

    #[test]
    fn selecting_file_preserves_sidebar_mode() {
        let mut app = sample_app();
        app.review.files.push(ReviewFile {
            old_path: Some("b.rs".to_string()),
            new_path: Some("b.rs".to_string()),
            status: ReviewFileStatus::Modified,
            additions: 0,
            deletions: 0,
            is_binary: false,
            hunks: Vec::new(),
        });
        app.sidebar_mode = ReviewSidebarMode::Repository;

        assert!(app.select_file(1));

        assert_eq!(app.sidebar_mode, ReviewSidebarMode::Repository);
    }

    #[test]
    fn selecting_file_restores_previous_viewport() {
        let mut app = sample_app();
        app.review.files.push(ReviewFile {
            old_path: Some("b.rs".to_string()),
            new_path: Some("b.rs".to_string()),
            status: ReviewFileStatus::Modified,
            additions: 0,
            deletions: 0,
            is_binary: false,
            hunks: vec![ReviewHunk {
                old_start: 1,
                old_count: 0,
                new_start: 1,
                new_count: 1,
                heading: None,
                lines: vec![ReviewLine {
                    kind: ReviewLineKind::Added,
                    old_line: None,
                    new_line: Some(1),
                    content: "b".to_string(),
                }],
            }],
        });
        app.diff_scroll = 1;
        app.selected_diff_line = 2;
        app.set_diff_area(Rect::new(0, 0, 80, 3));

        assert!(app.select_file(1));
        assert_eq!(app.diff_scroll, 0);
        assert_eq!(app.selected_diff_line, 0);

        assert!(app.select_file(0));
        assert_eq!(app.diff_scroll, 1);
        assert_eq!(app.selected_diff_line, 2);
    }

    #[test]
    fn repository_tree_rows_use_surface_paths() {
        let app = build_workspace_app(
            vec![build_source("source-1", ReviewSourceKind::Repository, true)],
            vec![build_file_surface("surface-1", "source-1")],
            Vec::new(),
        );

        assert_eq!(
            app.file_tree_rows(),
            vec![ReviewFileTreeRow::File { index: 0, depth: 0 }]
        );
    }

    #[test]
    fn moving_repository_tree_focus_does_not_open_file() {
        let mut app = build_workspace_app(
            vec![build_source("source-1", ReviewSourceKind::Repository, true)],
            vec![
                ReviewSurface {
                    id: "surface-1".to_string(),
                    source_id: "source-1".to_string(),
                    path: "a.rs".to_string(),
                    kind: ReviewSurfaceKind::File,
                    file: None,
                },
                ReviewSurface {
                    id: "surface-2".to_string(),
                    source_id: "source-1".to_string(),
                    path: "b.rs".to_string(),
                    kind: ReviewSurfaceKind::File,
                    file: None,
                },
            ],
            Vec::new(),
        );

        assert!(app.select_next_tree_row(1));

        assert_eq!(app.selected_tree_row, 1);
        assert_eq!(app.selected_file, 0);
    }

    #[test]
    fn tree_scroll_is_independent_from_file_scroll() {
        let mut app = build_workspace_app(
            vec![build_source("source-1", ReviewSourceKind::Repository, true)],
            vec![
                ReviewSurface {
                    id: "surface-1".to_string(),
                    source_id: "source-1".to_string(),
                    path: "a.rs".to_string(),
                    kind: ReviewSurfaceKind::File,
                    file: None,
                },
                ReviewSurface {
                    id: "surface-2".to_string(),
                    source_id: "source-1".to_string(),
                    path: "b.rs".to_string(),
                    kind: ReviewSurfaceKind::File,
                    file: None,
                },
            ],
            Vec::new(),
        );
        app.file_scroll = 7;

        assert!(app.scroll_tree_down(1));

        assert_eq!(app.tree_scroll, 1);
        assert_eq!(app.file_scroll, 7);
    }

    #[test]
    fn activating_repository_tree_file_opens_it() {
        let mut app = build_workspace_app(
            vec![build_source("source-1", ReviewSourceKind::Repository, true)],
            vec![
                ReviewSurface {
                    id: "surface-1".to_string(),
                    source_id: "source-1".to_string(),
                    path: "a.rs".to_string(),
                    kind: ReviewSurfaceKind::File,
                    file: None,
                },
                ReviewSurface {
                    id: "surface-2".to_string(),
                    source_id: "source-1".to_string(),
                    path: "b.rs".to_string(),
                    kind: ReviewSurfaceKind::File,
                    file: None,
                },
            ],
            Vec::new(),
        );
        app.selected_tree_row = 1;

        assert!(app.activate_selected_tree_row());

        assert_eq!(app.selected_file, 1);
    }

    #[test]
    fn repository_workspace_is_repository_review() {
        let app = build_workspace_app(
            vec![build_source("source-1", ReviewSourceKind::Repository, true)],
            vec![build_file_surface("surface-1", "source-1")],
            Vec::new(),
        );

        assert!(app.review.is_repository_review());
    }

    #[test]
    fn repository_workspace_queues_selected_file_load() {
        let mut app = build_workspace_app(
            vec![build_source("source-1", ReviewSourceKind::Repository, true)],
            vec![build_file_surface("surface-1", "source-1")],
            Vec::new(),
        );

        app.queue_selected_file_load();

        assert_eq!(app.pending_file_load.as_deref(), Some("a.rs"));
    }

    #[test]
    fn thread_sidebar_toggle_and_jump() {
        let mut app = sample_app();
        app.selected_diff_line = 2;

        assert!(app.open_comment_editor());
        app.comment_editor
            .as_mut()
            .expect("editor should open")
            .buffer
            .insert_str("Needs a test");
        assert!(app.save_comment_editor());
        assert!(app.toggle_sidebar_mode());
        assert!(app.toggle_sidebar_mode());

        assert_eq!(app.sidebar_mode, ReviewSidebarMode::Threads);
        assert_eq!(app.thread_summaries().len(), 1);
        app.selected_diff_line = 0;
        assert!(app.jump_to_selected_thread());
        assert_eq!(app.selected_diff_line, 2);
    }

    fn build_source(id: &str, kind: ReviewSourceKind, included: bool) -> ReviewSource {
        ReviewSource {
            id: id.to_string(),
            label: kind.label(),
            kind,
            included,
        }
    }

    fn build_workspace_app(
        sources: Vec<ReviewSource>,
        surfaces: Vec<ReviewSurface>,
        diagnostics: Vec<ReviewSourceDiagnostic>,
    ) -> ReviewApp {
        ReviewApp::new(ReviewSummary {
            title: "workspace".to_string(),
            repo_root: PathBuf::from("/repo"),
            additions: 0,
            deletions: 0,
            workspace: Some(ReviewWorkspace {
                id: "workspace-1".to_string(),
                title: "Workspace".to_string(),
                repo_root: PathBuf::from("/repo"),
                sources,
                created_at_ms: None,
                updated_at_ms: None,
                viewed_files: BTreeSet::new(),
                archived_at_ms: None,
            }),
            surfaces,
            diagnostics,
            repository_files: Vec::new(),
            repository_branches: Vec::new(),
            repository_commits: Vec::new(),
            files: Vec::new(),
        })
    }

    fn build_surface(id: &str, source_id: &str) -> ReviewSurface {
        ReviewSurface {
            id: id.to_string(),
            source_id: source_id.to_string(),
            path: "a.rs".to_string(),
            kind: ReviewSurfaceKind::Diff,
            file: None,
        }
    }

    fn build_file_surface(id: &str, source_id: &str) -> ReviewSurface {
        ReviewSurface {
            id: id.to_string(),
            source_id: source_id.to_string(),
            path: "a.rs".to_string(),
            kind: ReviewSurfaceKind::File,
            file: None,
        }
    }

    fn build_diagnostic(
        source_id: &str,
        severity: ReviewSourceDiagnosticSeverity,
    ) -> ReviewSourceDiagnostic {
        ReviewSourceDiagnostic {
            source_id: source_id.to_string(),
            severity,
            code: "test".to_string(),
            message: "diagnostic".to_string(),
        }
    }

    #[test]
    fn removes_duplicate_sources_preserving_first() {
        let mut app = build_workspace_app(
            vec![
                build_source("source-1", ReviewSourceKind::LastCommit, true),
                build_source("source-2", ReviewSourceKind::WorkingTreeUnstaged, true),
                build_source("source-3", ReviewSourceKind::LastCommit, true),
            ],
            Vec::new(),
            Vec::new(),
        );
        app.set_build_mode();
        app.selected_build_row = 2;

        assert!(app.remove_duplicate_sources());

        assert_eq!(app.workspace.sources.len(), 2);
        assert_eq!(app.workspace.sources[0].id, "source-1");
        assert_eq!(app.workspace.sources[1].id, "source-2");
        assert!(app.pending_workspace_save);
        assert!(app.pending_workspace_reload);
        assert_eq!(app.review.workspace, Some(app.workspace.clone()));
    }

    #[test]
    fn excludes_empty_included_sources() {
        let mut app = build_workspace_app(
            vec![
                build_source("source-1", ReviewSourceKind::LastCommit, true),
                build_source("source-2", ReviewSourceKind::WorkingTreeUnstaged, true),
                build_source("source-3", ReviewSourceKind::IndexStaged, false),
            ],
            vec![build_surface("surface-1", "source-1")],
            Vec::new(),
        );
        app.set_build_mode();

        assert!(app.exclude_empty_sources());

        assert!(app.workspace.sources[0].included);
        assert!(!app.workspace.sources[1].included);
        assert!(!app.workspace.sources[2].included);
        assert!(app.pending_workspace_save);
        assert!(app.pending_workspace_reload);
    }

    #[test]
    fn selects_empty_sources_with_wraparound() {
        let mut app = build_workspace_app(
            vec![
                build_source("source-1", ReviewSourceKind::LastCommit, true),
                build_source("source-2", ReviewSourceKind::WorkingTreeUnstaged, true),
                build_source("source-3", ReviewSourceKind::IndexStaged, true),
            ],
            vec![build_surface("surface-1", "source-1")],
            Vec::new(),
        );
        app.set_build_mode();
        app.selected_build_row = 2;

        assert!(app.select_next_empty_source());
        assert_eq!(app.selected_build_row, 1);
        assert!(app.select_previous_empty_source());
        assert_eq!(app.selected_build_row, 2);
    }

    #[test]
    fn diagnostic_source_actions_target_errors() {
        let mut app = build_workspace_app(
            vec![
                build_source("source-1", ReviewSourceKind::LastCommit, true),
                build_source("source-2", ReviewSourceKind::WorkingTreeUnstaged, true),
                build_source("source-3", ReviewSourceKind::IndexStaged, true),
            ],
            Vec::new(),
            vec![
                build_diagnostic("source-2", ReviewSourceDiagnosticSeverity::Warning),
                build_diagnostic("source-3", ReviewSourceDiagnosticSeverity::Error),
            ],
        );
        app.set_build_mode();

        assert!(app.select_next_diagnostic_source());
        assert_eq!(app.selected_build_row, 1);
        assert!(app.exclude_sources_with_errors());
        assert!(app.workspace.sources[1].included);
        assert!(!app.workspace.sources[2].included);
        assert!(app.pending_workspace_save);
        assert!(app.pending_workspace_reload);
    }

    #[test]
    fn diagnostic_source_counts_are_grouped_by_source() {
        let app = build_workspace_app(
            vec![build_source("source-1", ReviewSourceKind::LastCommit, true)],
            Vec::new(),
            vec![
                build_diagnostic("source-1", ReviewSourceDiagnosticSeverity::Info),
                build_diagnostic("source-1", ReviewSourceDiagnosticSeverity::Info),
                build_diagnostic("source-2", ReviewSourceDiagnosticSeverity::Warning),
                build_diagnostic("source-3", ReviewSourceDiagnosticSeverity::Error),
            ],
        );

        assert_eq!(app.diagnostic_source_counts(), (1, 1, 1));
    }

    #[test]
    fn thread_preview_shows_linked_session() {
        let mut app = sample_app();
        app.selected_diff_line = 2;
        assert!(app.open_comment_editor());
        app.comment_editor
            .as_mut()
            .expect("editor should open")
            .buffer
            .insert_str("Needs a test");
        assert!(app.save_comment_editor());
        let anchor = app.selected_comment_anchor().expect("anchor");
        app.mark_thread_session(&anchor, SessionId::new().to_string());
        app.toggle_sidebar_mode();

        let preview = app.selected_thread_preview().expect("thread preview");
        assert!(preview.contains("🤖"));
        assert!(preview.contains("Needs a test"));
    }

    #[test]
    fn thread_summary_tracks_resolved_state() {
        let mut app = sample_app();
        app.selected_diff_line = 2;
        assert!(app.open_comment_editor());
        app.comment_editor
            .as_mut()
            .expect("editor should open")
            .buffer
            .insert_str("Needs a test");
        assert!(app.save_comment_editor());
        let anchor = app.selected_comment_anchor().expect("anchor");
        app.resolved_review_threads
            .insert(ReviewApp::thread_key_for_anchor(&anchor));

        let summary = app.thread_summaries().pop().expect("thread summary");

        assert!(summary.resolved);
        assert_eq!(app.thread_status_counts(), (0, 1));
        assert!(
            app.selected_thread_preview()
                .expect("thread preview")
                .contains("resolved thread")
        );
    }

    #[test]
    fn cycles_enumerated_publish_options() {
        let mut app = sample_app();
        app.publish_state = Some(ReviewPublishState::Options {
            publisher_id: "github".to_string(),
            options: vec![ReviewPublishOption {
                name: "submit_event".to_string(),
                label: "GitHub review event".to_string(),
                value: "COMMENT".to_string(),
                choices: vec![
                    "COMMENT".to_string(),
                    "REQUEST_CHANGES".to_string(),
                    "APPROVE".to_string(),
                ],
            }],
            selected: 0,
        });

        assert!(app.cycle_selected_publish_option(1));
        let Some(ReviewPublishState::Options { options, .. }) = &app.publish_state else {
            panic!("options state expected");
        };
        assert_eq!(options[0].value, "REQUEST_CHANGES");
    }

    #[test]
    fn toggles_selected_file_viewed() {
        let mut app = sample_app();
        assert_eq!(app.viewed_file_counts(), (0, app.review.files.len()));

        assert!(app.toggle_selected_file_viewed());
        assert!(app.file_viewed(0));
        assert_eq!(app.viewed_file_counts(), (1, app.review.files.len()));

        assert!(app.toggle_selected_file_viewed());
        assert!(!app.file_viewed(0));
    }

    #[test]
    fn selects_next_unviewed_file() {
        let mut app = sample_app();
        assert!(app.toggle_selected_file_viewed());

        assert!(app.select_next_unviewed_file());

        assert_eq!(app.selected_file, 1);
        assert_eq!(
            app.status_message.as_deref(),
            Some("selected unviewed file")
        );
    }

    #[test]
    fn selects_previous_unviewed_file() {
        let mut app = sample_app();
        app.selected_file = 1;
        app.viewed_files.insert("b.rs".to_string());

        assert!(app.select_previous_unviewed_file());

        assert_eq!(app.selected_file, 0);
        assert_eq!(
            app.status_message.as_deref(),
            Some("selected unviewed file")
        );
    }

    #[test]
    fn marks_all_files_viewed() {
        let mut app = sample_app();

        assert!(app.mark_all_files_viewed());

        assert_eq!(
            app.viewed_file_counts(),
            (app.review.files.len(), app.review.files.len())
        );
        assert_eq!(app.workspace.viewed_files, app.viewed_files);
        assert!(app.pending_workspace_save);
    }

    #[test]
    fn marks_all_files_unviewed() {
        let mut app = sample_app();
        assert!(app.mark_all_files_viewed());
        app.pending_workspace_save = false;

        assert!(app.mark_all_files_unviewed());

        assert_eq!(app.viewed_file_counts(), (0, app.review.files.len()));
        assert!(app.workspace.viewed_files.is_empty());
        assert!(app.viewed_files.is_empty());
        assert!(app.pending_workspace_save);
    }

    #[test]
    fn review_readiness_tracks_unviewed_and_open_threads() {
        let mut app = sample_app();
        assert_eq!(app.review_readiness_label(), "incomplete: 2 unviewed");

        assert!(app.mark_all_files_viewed());
        assert_eq!(app.review_readiness_label(), "ready");

        let anchor = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 2,
            end_diff_row: None,
            old_line: None,
            new_line: Some(1),
            old_start: None,
            old_end: None,
            new_start: Some(1),
            new_end: Some(1),
            line_kind: ReviewLineKind::Added,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        app.draft_comments.insert(
            anchor,
            vec![ReviewDraftComment {
                id: Some("comment".to_string()),
                body: "note".to_string(),
                persisted: true,
                created_at_ms: None,
                updated_at_ms: None,
                session_id: None,
            }],
        );
        assert_eq!(app.review_readiness_label(), "incomplete: 1 open");
    }

    #[test]
    fn publish_warns_when_review_incomplete() {
        let mut app = sample_app();

        assert!(app.publish_review());
        assert!(app.pending_publish_request.is_none());
        assert_eq!(
            app.status_message.as_deref(),
            Some("review incomplete: 0/2 files viewed; press x again to publish anyway")
        );

        assert!(app.publish_review());
        assert!(matches!(
            app.publish_state,
            Some(ReviewPublishState::Checklist)
        ));
        assert!(app.pending_publish_request.is_none());

        assert!(app.confirm_publish_selection());
        assert!(matches!(
            app.pending_publish_request,
            Some(PendingPublishRequest::ListPublishers)
        ));
    }

    #[test]
    fn publish_checklist_summarizes_readiness() {
        let mut app = sample_app();

        assert_eq!(
            app.publish_checklist_lines(),
            vec![
                "! review has remaining attention items".to_string(),
                "files viewed: 0/2".to_string(),
                "unviewed files: 2  (W jump)".to_string(),
                "  • a.rs".to_string(),
                "  • b.rs".to_string(),
                "open threads: 0  (P jump)".to_string(),
                "resolved threads: 0".to_string(),
                "draft comments: 0".to_string(),
                "press ! for attention sidebar".to_string(),
            ]
        );

        assert!(app.mark_all_files_viewed());
        assert_eq!(
            app.publish_checklist_lines().first().map(String::as_str),
            Some("✓ ready to publish")
        );
    }

    #[test]
    fn attention_navigation_prioritizes_unviewed_then_open_threads() {
        let mut app = sample_app();

        assert!(app.select_next_attention_item());
        assert_eq!(app.selected_file, 1);
        assert_eq!(
            app.status_message.as_deref(),
            Some("selected unviewed file")
        );

        assert!(app.mark_all_files_viewed());
        let anchor = ReviewCommentAnchor {
            file_index: 0,
            path: "a.rs".to_string(),
            diff_row: 2,
            end_diff_row: None,
            old_line: None,
            new_line: Some(1),
            old_start: None,
            old_end: None,
            new_start: Some(1),
            new_end: Some(1),
            line_kind: ReviewLineKind::Added,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        };
        app.draft_comments.insert(
            anchor.clone(),
            vec![ReviewDraftComment {
                id: Some("comment".to_string()),
                body: "note".to_string(),
                persisted: true,
                created_at_ms: None,
                updated_at_ms: None,
                session_id: None,
            }],
        );

        assert!(app.select_next_attention_item());
        assert_eq!(app.selected_file, anchor.file_index);
        assert_eq!(app.selected_diff_line, anchor.diff_row);
    }

    #[test]
    fn publish_checklist_shortcuts_jump_to_attention_items() {
        let mut app = sample_app();
        app.publish_state = Some(ReviewPublishState::Checklist);

        assert!(handle_publish_key(
            &mut app,
            KeyStroke {
                key: KeyCode::Char('W'),
                modifiers: bmux_keyboard::Modifiers::NONE,
            },
        ));

        assert!(app.publish_state.is_none());
        assert_eq!(app.selected_file, 1);
        assert_eq!(
            app.status_message.as_deref(),
            Some("selected unviewed file")
        );

        app.publish_state = Some(ReviewPublishState::Checklist);
        assert!(handle_publish_key(
            &mut app,
            KeyStroke {
                key: KeyCode::Char('!'),
                modifiers: bmux_keyboard::Modifiers::NONE,
            },
        ));
        assert!(app.publish_state.is_none());
        assert_eq!(app.sidebar_mode, ReviewSidebarMode::NeedsAttention);
    }

    #[test]
    fn publish_without_warning_when_review_complete() {
        let mut app = sample_app();
        assert!(app.mark_all_files_viewed());
        app.pending_workspace_save = false;

        assert!(app.publish_review());

        assert!(matches!(
            app.publish_state,
            Some(ReviewPublishState::Checklist)
        ));
        assert!(app.pending_publish_request.is_none());

        assert!(app.confirm_publish_selection());
        assert!(matches!(
            app.pending_publish_request,
            Some(PendingPublishRequest::ListPublishers)
        ));
    }
}
